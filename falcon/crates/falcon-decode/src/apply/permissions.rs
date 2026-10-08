//! Preflight the destinations Apply will actually write, without creating probe files.
use super::*;
use std::{fs::{self, File, OpenOptions}, io};

pub(super) enum JpegRoute { Patch, AlreadyTarget, Sidecar(PatchErr) }

fn at(path: &Path, error: io::Error) -> RotationFailure {
    RotationFailure::io(path, error)
}
fn denied(path: &Path) -> RotationFailure {
    RotationFailure::new(path, RotationFailureReason::ReadOnly)
}
fn writable_file(path: &Path) -> Result<(), RotationFailure> {
    let metadata = fs::metadata(path).map_err(|e| at(path, e))?;
    if !metadata.is_file() { return Err(RotationFailure::new(path, RotationFailureReason::NotFile)); }
    if metadata.permissions().readonly() { return Err(denied(path)); }
    OpenOptions::new().read(true).write(true).open(path).map_err(|e| at(path, e))?;
    Ok(())
}
fn writable_folder(path: &Path) -> Result<(), RotationFailure> {
    if !fs::metadata(path).map_err(|e| at(path, e))?.is_dir() {
        return Err(RotationFailure::new(path, RotationFailureReason::NotDirectory));
    }
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        unsafe extern "C" { fn access(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> std::ffi::c_int; }
        if fs::metadata(path).map_err(|e| at(path, e))?.permissions().readonly() { return Err(denied(path)); }
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| RotationFailure::new(path, RotationFailureReason::InvalidPath))?;
        if unsafe { access(name.as_ptr(), 2 | 1) } != 0 { return Err(at(path, io::Error::last_os_error())); }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_ADD_FILE on an existing directory, FILE_FLAG_BACKUP_SEMANTICS.
        // This checks the ACL without creating OneDrive/network sync activity.
        OpenOptions::new().access_mode(0x0002).custom_flags(0x0200_0000)
            .open(path).map_err(|e| at(path, e))?;
    }
    Ok(())
}
fn writable_sidecar(path: &Path) -> Result<(), RotationFailure> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    writable_folder(parent)?;
    match fs::symlink_metadata(path) {
        Ok(_) => writable_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(at(path, e)),
    }
}
fn jpeg_route(path: &Path, expected: u8, target: u8) -> Result<JpegRoute, RotationFailure> {
    let mut file = File::open(path).map_err(|e| at(path, e))?;
    let cap = JPEG_LOCATE_CAP.min(file.metadata().map_err(|e| at(path, e))?.len() as usize);
    let mut bytes = vec![0; cap];
    read_full(&mut file, &mut bytes).map_err(|e| at(path, e))?;
    let loc = match locate_jpeg_orientation(&bytes) {
        Ok(loc) => loc,
        Err(error) => return Ok(JpegRoute::Sidecar(PatchErr::Locate(error))),
    };
    file.seek(SeekFrom::Start(loc.value_offset)).map_err(|e| at(path, e))?;
    let mut value = [0; 2];
    read_full(&mut file, &mut value).map_err(|e| at(path, e))?;
    let found = loc.endian.u16(&value);
    Ok(if found == target as u16 { JpegRoute::AlreadyTarget }
        else if found == expected as u16 { JpegRoute::Patch }
        else { JpegRoute::Sidecar(PatchErr::Cas { expected, target, found }) })
}
pub(super) fn check_rotation_write_access(plan: &RotApplyPlan) -> Result<Option<JpegRoute>, RotationFailure> {
    let mut route = None;
    for (path, raw) in [(plan.finished.as_deref(), false), (plan.raw.as_deref(), true)] {
        let Some(path) = path else { continue };
        if !fs::metadata(path).map_err(|e| at(path, e))?.is_file() {
            return Err(RotationFailure::new(path, RotationFailureReason::NotFile));
        }
        if !raw && plan.finished_is_jpeg {
            let expected = turns_to_orientation(plan.base_turns);
            let target = compose_exif_orientation(expected, plan.delta & 3);
            let jpeg = jpeg_route(path, expected, target)?;
            match &jpeg {
                JpegRoute::Patch => writable_file(path)?,
                JpegRoute::Sidecar(_) => writable_sidecar(&sidecar_fullname(path))?,
                JpegRoute::AlreadyTarget => {},
            }
            route = Some(jpeg);
        } else {
            // Do not open RAW/non-JPEG originals during preflight. An existing XMP
            // can supply orientation without reading a locked or cloud-only original.
            // The apply path reads embedded metadata lazily only when needed.
            writable_sidecar(&sidecar_path_for(path, raw))?;
        }
    }
    Ok(route)
}
