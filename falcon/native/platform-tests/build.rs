//! Builds the Windows icon helper and its test-only twin for `windows_shell_icons.rs`, with the
//! same code and flags as the app's own build script. Nothing to do on other platforms.

#[cfg(windows)]
#[path = "../build_shell_icons.rs"]
mod build_shell_icons;

fn main() {
    #[cfg(windows)]
    {
        let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let native = manifest.parent().expect("platform-tests sits in falcon/native");
        build_shell_icons::build(native, true);
    }
}
