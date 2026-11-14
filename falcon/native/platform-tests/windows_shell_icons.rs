//! Native DLL/COM checks in a separate process. Normal tests redirect the registry;
//! the real-Shell test is opt-in because it temporarily registers synthetic types.
#[cfg(windows)]
mod native {
    use std::{ffi::c_void, ptr};
    use windows::{core::{GUID, HRESULT, Interface, PCWSTR, PWSTR}, Win32::{
        System::Com::{IClassFactory, IPersistFile, STGM_READ}, UI::Shell::IExtractIconW}};
    use winreg::{RegKey, enums::{HKEY_CURRENT_USER, HKEY_CLASSES_ROOT}};
    // --include-ignored may select both tests. Never redirect process-wide HKCU
    // while the real-Shell check is creating its deliberately temporary keys.
    static REGISTRY_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
    const CLSID: GUID = GUID::from_u128(0xbbaba60e_8fe6_4b64_8f05_aa306f973b4a);
    const CLASS: &str = r"Software\Classes\CLSID\{BBABA60E-8FE6-4B64-8F05-AA306F973B4A}";
    #[link(name = "advapi32")]
    extern "system" { fn RegOverridePredefKey(key: *mut c_void, replacement: *mut c_void) -> i32; }
    struct Hive { original: RegKey, path: String, root: RegKey }
    impl Hive {
        fn new() -> Self {
            let original = RegKey::predef(HKEY_CURRENT_USER);
            let path = format!(r"Software\FalconIconTests\dll-{}-{}", std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
            let root = original.create_subkey(&path).unwrap().0;
            assert_eq!(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, root.raw_handle()) }, 0);
            Self { original, path, root }
        }
        fn set(&self, key: &str, value: &str, data: &str) {
            self.root.create_subkey(key).unwrap().0.set_value(value, &data).unwrap();
        }
    }
    impl Drop for Hive {
        fn drop(&mut self) {
            unsafe {
                RegOverridePredefKey(HKEY_CLASSES_ROOT, ptr::null_mut());
                RegOverridePredefKey(HKEY_CURRENT_USER, ptr::null_mut());
            }
            let _ = self.original.delete_subkey_all(&self.path);
        }
    }
    fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
    fn location(icon: &IExtractIconW) -> (HRESULT, String, i32, u32) {
        let mut path = [0; 32768]; let mut index = 0; let mut flags = 0;
        let result = unsafe { (icon.vtable().GetIconLocation)(icon.as_raw(), 0,
            PWSTR(path.as_mut_ptr()), path.len() as u32, &mut index, &mut flags) };
        (result, String::from_utf16_lossy(&path[..path.iter().position(|v| *v == 0).unwrap()]), index, flags)
    }
    /// Falsifiers: route by shared class/case-sensitive suffix, cache the first icon
    /// for the class, open the source, skip fallback/bounds, or ignore COM lifetimes.
    #[test]
    fn native_dll_routes_existing_defaults_without_opening_photos() {
        let _serial = REGISTRY_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let hive = Hive::new();
        let exe = r"C:\not-present\Falcon, ü.exe";
        let reference = |id| format!("\"{exe}\",-{id}");
        hive.set(CLASS, "FallbackIcon", &reference(2));
        for (ext, id) in [("png", 11), ("jpg", 10), ("jpeg", 10), ("heic", 13),
            ("heif", 14), ("tif", 12), ("tiff", 12), ("apng", 19), ("webp", 15), ("cr3", 3)] {
            hive.set(&format!(r"Software\Classes\Falcon.Image.{ext}\DefaultIcon"), "", &reference(id));
        }
        // All source names and even the icon source need not exist: Load/GetIconLocation
        // must not open either. Explorer itself will later load packaged icon resources.
        let dll = unsafe { libloading::Library::new(concat!(env!("OUT_DIR"), "/falcon-shell-icons.dll")) }.unwrap();
        type GetClass = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
        let get: libloading::Symbol<GetClass> = unsafe { dll.get(b"DllGetClassObject") }.unwrap();
        let unload: libloading::Symbol<unsafe extern "system" fn() -> HRESULT> = unsafe { dll.get(b"DllCanUnloadNow") }.unwrap();
        assert_eq!(unsafe { unload() }, HRESULT(0));
        let mut raw = ptr::null_mut();
        assert!(unsafe { get(&GUID::zeroed(), &IClassFactory::IID, &mut raw) }.is_err());
        assert!(raw.is_null());
        unsafe { get(&CLSID, &IClassFactory::IID, &mut raw) }.unwrap();
        let factory = unsafe { IClassFactory::from_raw(raw) };
        assert_eq!(unsafe { unload() }, HRESULT(1));
        let file: IPersistFile = unsafe { factory.CreateInstance(None) }.unwrap();
        let icon: IExtractIconW = file.cast().unwrap();
        assert_eq!(unsafe { file.GetClassID() }.unwrap(), CLSID);
        for (suffix, id) in [("png", 11), ("PNG", 11), ("PnG", 11), ("jpg", 10), ("JPEG", 10),
            ("jpeg", 10), ("HEIC", 13), ("heif", 14), ("TIF", 12), ("tiff", 12),
            ("APNG", 19), ("webp", 15), ("cr3", 3), ("unknown", 2), ("pn-g", 2), ("png:stream", 2)] {
            let source = wide(&format!(r"Z:\offline-not-present\folder.jpg\test.{suffix}"));
            unsafe { file.Load(PCWSTR(source.as_ptr()), STGM_READ) }.unwrap();
            assert_eq!(location(&icon), (HRESULT(0), exe.into(), -id, 0), "{suffix}");
        }
        unsafe { file.Load(PCWSTR(wide(r"Z:\folder.png\no-extension").as_ptr()), STGM_READ) }.unwrap();
        assert_eq!(location(&icon).2, -2);
        // The same live object sees the RAW toggle without a stale per-class cache.
        unsafe { file.Load(PCWSTR(wide("missing.CR3").as_ptr()), STGM_READ) }.unwrap();
        assert_eq!(location(&icon).2, -3);
        hive.set(r"Software\Classes\Falcon.Image.cr3\DefaultIcon", "", &reference(100));
        assert_eq!(location(&icon).2, -100);
        let mut tiny = [9_u16; 2]; let mut index = 5; let mut flags = 5;
        assert!(unsafe { icon.GetIconLocation(0, &mut tiny[..1], &mut index, &mut flags) }.is_err());
        assert_eq!(tiny, [0, 9], "undersized output is terminated, not overrun");
        assert!(unsafe { icon.GetIconLocation(0, &mut [], &mut index, &mut flags) }.is_err());
        assert!(unsafe { file.Load(PCWSTR::null(), STGM_READ) }.is_err());
        assert_eq!(location(&icon).2, -2, "failed Load does not retain previous format");
        assert!(unsafe { file.Load(PCWSTR(wide(&"x".repeat(32769)).as_ptr()), STGM_READ) }.is_err());
        let extract = unsafe { (icon.vtable().Extract)(icon.as_raw(), PCWSTR::null(), 0,
            ptr::null_mut(), ptr::null_mut(), 0) };
        assert_eq!(extract, HRESULT(1), "Shell performs ordinary cached resource extraction");
        unsafe { factory.LockServer(true) }.unwrap();
        drop(icon); drop(file);
        unsafe { factory.LockServer(false) }.unwrap();
        drop(factory);
        assert_eq!(unsafe { unload() }, HRESULT(0));
    }

    /// Explicit native acceptance: temporarily creates synthetic file types and a
    /// fixed test-only COM class. Run alone with --ignored; never in ordinary CI.
    #[test]
    #[ignore = "temporarily registers synthetic Windows file types; run explicitly in isolation"]
    fn native_shell_locations() {
        let _serial = REGISTRY_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        const TEST_CLASS: &str = r"Software\Classes\CLSID\{D69608A2-5041-4666-9FA6-1A0C326F587E}";
        const TEST_ID: &str = "{D69608A2-5041-4666-9FA6-1A0C326F587E}";
        #[repr(C)]
        struct Info { icon: *mut c_void, index: i32, attributes: u32, display: [u16; 260], type_name: [u16; 80] }
        #[link(name = "shell32")]
        extern "system" { fn SHGetFileInfoW(path: *const u16, attributes: u32, info: *mut Info, size: u32, flags: u32) -> usize; }
        // Shell services do not share process-local registry redirection. Use ONLY
        // new, uniquely named scratch types/classes, never production extensions.
        struct Scratch { keys: Vec<String>, dir: std::path::PathBuf }
        impl Drop for Scratch {
            fn drop(&mut self) {
                for key in &self.keys { let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(key); }
                if let Ok(files) = std::fs::read_dir(&self.dir) {
                    for file in files.flatten() { let _ = std::fs::remove_file(file.path()); }
                }
                let _ = std::fs::remove_dir(&self.dir);
                unsafe { CoUninitialize(); }
            }
        }
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.unwrap();
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() % 100_000_000_000;
        let id = format!("Falcon.IconTest.{nonce}");
        let mut scratch = Scratch { keys: vec![], dir: std::env::temp_dir().join(&id) };
        std::fs::create_dir(&scratch.dir).unwrap();
        let root = RegKey::predef(HKEY_CURRENT_USER);
        let mut create = |path: String| {
            assert!(root.open_subkey(&path).is_err(), "scratch key must be new");
            let key = root.create_subkey(&path).unwrap().0;
            scratch.keys.push(path);
            key
        };
        let class = create(format!(r"Software\Classes\{id}"));
        // Real activation, but with a test-only identity compiled from the same
        // source. No production COM class or file association is changed.
        let com = create(TEST_CLASS.to_owned());
        let server = com.create_subkey("InprocServer32").unwrap().0;
        server.set_value("", &concat!(env!("OUT_DIR"), "/falcon-shell-icons-test.dll")).unwrap();
        server.set_value("ThreadingModel", &"Apartment").unwrap();
        let executable = std::env::current_exe().unwrap().display().to_string();
        class.create_subkey("DefaultIcon").unwrap().0.set_value("", &"%1").unwrap();
        class.create_subkey(r"shell\open\command").unwrap().0.set_value("", &format!("\"{executable}\" \"%1\"")).unwrap();
        class.create_subkey(r"shellex\IconHandler").unwrap().0.set_value("", &TEST_ID).unwrap();
        let extensions = [(format!("fp{nonce}"), -11), (format!("fj{nonce}"), -10)];
        for (ext, resource) in &extensions {
            create(format!(r"Software\Classes\.{ext}")).set_value("", &id).unwrap();
            create(format!(r"Software\Classes\Falcon.Image.{ext}")).create_subkey("DefaultIcon").unwrap().0
                .set_value("", &format!("\"{executable}\",{resource}")).unwrap();
        }
        for (extension, expected) in &extensions {
            for ext in [extension.clone(), extension.to_uppercase()] {
                let path = scratch.dir.join(format!("sample.{ext}"));
                std::fs::write(&path, []).unwrap();
                let mut info = Info { icon: ptr::null_mut(), index: 0, attributes: 0, display: [0;260], type_name: [0;80] };
                let result = unsafe { SHGetFileInfoW(wide(&path.display().to_string()).as_ptr(), 0, &mut info,
                    std::mem::size_of::<Info>() as u32, 0x1000 /* SHGFI_ICONLOCATION */) };
                std::fs::remove_file(path).unwrap();
                assert_ne!(result, 0, "native Shell icon query {ext}");
                let icon_path = String::from_utf16_lossy(&info.display[..info.display.iter().position(|c| *c == 0).unwrap()]);
                assert_eq!((icon_path, info.index), (executable.clone(), *expected), "native Shell {ext}");
            }
        }
        drop(scratch);
    }
}
