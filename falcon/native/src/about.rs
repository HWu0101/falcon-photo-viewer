//! Owner-approved identity card. Links are fixed targets, never photo-supplied commands.
use crate::MainWindow;
use slint::ComponentHandle;

pub(crate) const REPOSITORY: &str = "https://github.com/HWu0101/falcon-photo-viewer";
pub(crate) const INSTAGRAM: &str = "https://www.instagram.com/hwuphoto/";
pub(crate) const LINKEDIN: &str = "https://www.linkedin.com/in/hancheng-wu-325794321";
/// 1.1.0-rc.1: "Check for updates" opens the latest release in the browser; Falcon itself makes no
/// network request.
pub(crate) const RELEASES: &str = "https://github.com/HWu0101/falcon-photo-viewer/releases/latest";

fn target(kind: i32) -> Option<&'static str> {
    match kind { 0 => Some(REPOSITORY), 1 => Some(INSTAGRAM), 2 => Some(LINKEDIN), 3 => Some(RELEASES), _ => None }
}

pub(crate) fn wire(app: &MainWindow, open: impl Fn(&str) -> std::io::Result<()> + 'static) {
    app.set_about_repository(REPOSITORY.into());
    let weak = app.as_weak();
    app.on_about_link_requested(move |kind| {
        let Some(app) = weak.upgrade() else { return };
        if !app.get_about_open() { return; }
        let Some(url) = target(kind) else { return };
        match open(url) {
            Ok(()) => app.set_about_error("".into()),
            Err(e) => app.set_about_error(tr_format!("Couldn't open the link: {error}", error = e).into()),
        }
    });
}

pub(crate) fn open_external(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(window: *mut core::ffi::c_void, operation: *const u16,
                file: *const u16, parameters: *const u16, directory: *const u16, show: i32) -> *mut core::ffi::c_void;
        }
        let target: Vec<u16> = std::ffi::OsStr::new(url).encode_wide().chain(Some(0)).collect();
        let result = unsafe { ShellExecuteW(std::ptr::null_mut(), std::ptr::null(), target.as_ptr(),
            std::ptr::null(), std::ptr::null(), 1) } as usize;
        if result <= 32 { return Err(std::io::Error::other(format!("Windows link handler returned {result}"))); }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let program = if cfg!(target_os = "macos") { "/usr/bin/open" } else { "xdg-open" };
        // `open`/`xdg-open` return after dispatch (no -W); reap the helper and report failure.
        let status = std::process::Command::new(program).arg(url).status()?;
        if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("Link handler exited with {status}"))) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Falsifier: point Check for updates anywhere but the fixed latest-release page of this repository.
    #[test]
    fn about_releases_link_is_the_fixed_latest_release_url() {
        assert_eq!(target(3), Some(RELEASES));
        assert!(RELEASES.starts_with(REPOSITORY));
        assert!(RELEASES.ends_with("/releases/latest"));
        assert_eq!(target(4), None);
    }
}
