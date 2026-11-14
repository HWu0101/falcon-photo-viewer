// Built only for Windows targets on the Windows build host, using the same MSVC
// toolchain as the existing native dependencies. No runtime redistributable.
// Shared by the app's build script (the production helper, embedded in the exe) and
// `platform-tests/build.rs` (both helpers, for the Shell icon tests). `native` is the
// `falcon/native` folder, which holds `shell-icons/handler.cpp`.
pub fn build(native: &std::path::Path, with_test_helper: bool) {
    use std::{hash::{Hash, Hasher}, path::PathBuf};
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") { return; }
    let source = native.join("shell-icons/handler.cpp");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed={}", native.join("build_shell_icons.rs").display());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let compiler = cc::Build::new().cpp(true).static_crt(true).get_compiler();
    assert!(compiler.is_like_msvc(), "Falcon's Windows icon helper requires MSVC");
    let variants: &[(&str, bool)] = if with_test_helper {
        &[("falcon-shell-icons", false), ("falcon-shell-icons-test", true)]
    } else {
        &[("falcon-shell-icons", false)]
    };
    for &(stem, test_class) in variants {
        let mut command = compiler.to_command();
        if test_class { command.arg("/DFALCON_ICON_TEST_CLASS"); }
        let status = command
            .args(["/nologo", "/LD", "/MT", "/O2", "/W4", "/WX", "/GS", "/guard:cf", "/GR-", "/std:c++17"])
            .arg(&source)
            .arg(format!("/Fo{}", out.join(format!("{stem}.obj")).display()))
            .arg(format!("/Fe{}", out.join(format!("{stem}.dll")).display()))
            .args(["/link", "/INCREMENTAL:NO", "/DYNAMICBASE", "/NXCOMPAT", "/guard:cf", "/Brepro", "/EXPORT:DllGetClassObject", "/EXPORT:DllCanUnloadNow", "advapi32.lib", "shlwapi.lib", "uuid.lib"])
            .current_dir(&out).status().expect("compile Windows icon helper");
        assert!(status.success(), "Windows icon helper must build before packaging");
    }
    let bytes = std::fs::read(out.join("falcon-shell-icons.dll")).unwrap();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hash);
    // This is a cache name, not a trust digest. Runtime checks EVERY byte before
    // registration; a collision/mismatching file fails rather than being loaded.
    println!("cargo:rustc-env=FALCON_ICON_HELPER_NAME=falcon-icons-{:016x}.dll", hash.finish());
}
