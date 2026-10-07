//! File synchronization and sibling-temp replacement shared by review JSON and XMP writes.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    write_with_sync(path, data, sync_file)
}

pub fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // Rust's Darwin sync_all uses F_FULLFSYNC, which SMB may reject even
        // when ordinary fsync, writes and rename work. Only ENOTSUP permits
        // this weaker filesystem flush; disk-full/I/O/permission errors remain fatal.
        sync_with_fallback(file.sync_all(), || {
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        })
    }
    #[cfg(not(target_os = "macos"))]
    file.sync_all()
}

#[cfg(target_os = "macos")]
fn sync_with_fallback(
    full: io::Result<()>,
    ordinary: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match full {
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => ordinary(),
        result => result,
    }
}

fn write_with_sync(
    path: &Path,
    data: &[u8],
    sync: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    static TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let n = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("falcon");
    let temp = dir.join(format!("{base}.{}.{n}.falcontmp", std::process::id()));
    // create_new never truncates another writer's temporary file.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| stage("create temporary file", e))?;
    let result = (|| {
        file.write_all(data)
            .map_err(|e| stage("write temporary file", e))?;
        sync(&file).map_err(|e| stage("sync temporary file", e))?;
        drop(file);
        fs::rename(&temp, path).map_err(|e| stage("replace target file", e))
    })();
    // The closure closes the handle on both success and error (including Windows).
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn stage(name: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{name}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "falcon-atomic-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn count(&self) -> usize {
            fs::read_dir(&self.0).unwrap().count()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replace_and_sync_failure_preserve_previous_bytes_and_clean_temp() {
        let t = Temp::new();
        let p = t.0.join("review.json");
        write_atomic(&p, b"old").unwrap();
        write_atomic(&p, b"new").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        let e = write_with_sync(&p, b"lost", |_| {
            Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"))
        })
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        assert!(e.to_string().contains("sync temporary file"));
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn rename_failure_cleans_temp_and_keeps_target() {
        let t = Temp::new();
        let p = t.0.join("directory");
        fs::create_dir(&p).unwrap();
        assert!(write_atomic(&p, b"bytes")
            .unwrap_err()
            .to_string()
            .contains("replace target file"));
        assert!(p.is_dir());
        assert_eq!(t.count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unsupported_full_sync_falls_back_but_other_errors_do_not() {
        let calls = std::cell::Cell::new(0);
        let fallback = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        sync_with_fallback(Err(io::Error::from_raw_os_error(libc::ENOTSUP)), fallback).unwrap();
        assert_eq!(calls.get(), 1);
        for errno in [libc::ENOSPC, libc::EIO, libc::EACCES, libc::EINVAL] {
            assert_eq!(
                sync_with_fallback(Err(io::Error::from_raw_os_error(errno)), fallback)
                    .unwrap_err()
                    .raw_os_error(),
                Some(errno)
            );
        }
        sync_with_fallback(Ok(()), fallback).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            sync_with_fallback(Err(io::Error::from_raw_os_error(libc::ENOTSUP)), || Err(
                io::Error::from_raw_os_error(libc::EIO)
            ))
            .unwrap_err()
            .raw_os_error(),
            Some(libc::EIO)
        );
    }
}
