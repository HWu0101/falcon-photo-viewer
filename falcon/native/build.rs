//! Build script: embed the Windows app icon + version metadata into the .exe so it
//! shows a proper icon in Explorer / the taskbar / Alt-Tab and a version under
//! Properties → Details. The whole thing is `#[cfg(windows)]` (host), so on macOS/Linux
//! it compiles to an empty `main` with no dependency on `winresource`. Embedding
//! failures are fatal: registered icon IDs must exist in every Windows executable.
//!
//! The interface's own build settings (element debug info for the rig, the bundled translations,
//! the `SLINT_SCALE_FACTOR` guard) live in `ui/build.rs`, beside the markup they apply to. This
//! script writes only the Rust side of the language packs (`packs.rs`).

#[cfg(windows)]
mod build_shell_icons;
#[allow(dead_code)] // the `.po` half is the interface crate's (`ui/build.rs`)
mod build_translations;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    build_translations::build_pack_list(&manifest.join("translations"));
    #[cfg(windows)]
    embed_windows_resources();
    // The production icon helper only; its test-only twin is built for `platform-tests`.
    #[cfg(windows)]
    build_shell_icons::build(&manifest, false);
}

#[cfg(windows)]
fn embed_windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") { return; }
    println!("cargo:rerun-if-changed=icon.ico");
    println!("cargo:rerun-if-changed=assets/icons");
    #[allow(dead_code)]
    mod icons { include!("assets/icons/catalog.rs"); }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("icon.ico"); // Resource 1 remains the app/window icon.
    for (id, file) in icons::RESOURCES.iter().filter(|(id, _)| *id != 1) {
        res.set_icon_with_id(&format!("assets/icons/{file}"), &id.to_string());
    }
    // FileVersion / ProductVersion default to CARGO_PKG_VERSION; set the display strings.
    res.set("ProductName", "Falcon Photo Viewer");
    res.set("FileDescription", "Falcon Photo Viewer");
    res.compile().expect("Windows icon/version resources must be embedded");
}
