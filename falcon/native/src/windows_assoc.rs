//! Per-user Windows registration. Tests use a disposable root, never real Classes.
use crate::{file_icons, support::ASSOC_FAMILIES, windows_icon_bridge as bridge};
use winreg::{enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE}, RegKey};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

const LEGACY: &str = file_icons::LEGACY_PROGID;
const CAPS: &str = r"Software\Falcon\Capabilities";

fn canonical_exe(path: &str) -> Option<Vec<u16>> {
    let path = Path::new(path);
    if !path.is_absolute() { return None; }
    let path = path.canonicalize().ok()?;
    if !path.is_file() { return None; }
    Some(path.as_os_str().encode_wide().collect())
}

/// Falcon writes a quoted absolute executable followed by its arguments. Anything ambiguous or
/// unresolvable waits for the explicit Update action; never guess the owner of a registration.
fn owned_command_path<'a>(command: &'a str, running: &[u16]) -> Option<&'a str> {
    let (path, tail) = command.trim_start().strip_prefix('"')?.split_once('"')?;
    if !tail.is_empty() && !tail.starts_with(char::is_whitespace) { return None; }
    let registered = canonical_exe(path)?;
    #[link(name = "kernel32")]
    extern "system" {
        fn CompareStringOrdinal(a: *const u16, a_len: i32, b: *const u16, b_len: i32, ignore_case: i32) -> i32;
    }
    // Windows' ordinal uppercase table, not locale-sensitive or ASCII-only case conversion.
    let equal = unsafe { CompareStringOrdinal(registered.as_ptr(), registered.len().try_into().ok()?,
        running.as_ptr(), running.len().try_into().ok()?, 1) } == 2;
    equal.then_some(path) // Keep the registered spelling; don't redirect to a temporary alias.
}

fn delete_value_if_present(key: &RegKey, name: &str) -> io::Result<bool> {
    match key.delete_value(name) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn changed_value(key: &RegKey, name: &str, value: &str) -> io::Result<bool> {
    if key.get_value::<String, _>(name).ok().as_deref() == Some(value) { return Ok(false); }
    key.set_value(name, &value)?;
    Ok(true)
}

struct Registration { root: RegKey, icon_dir: Option<std::path::PathBuf> }
impl Registration {
    fn current_user() -> Self {
        Self { root: RegKey::predef(HKEY_CURRENT_USER), icon_dir: std::env::var_os("LOCALAPPDATA")
            .map(|p| std::path::PathBuf::from(p).join("Falcon").join("shell-icons")) }
    }
    fn open(&self, path: &str) -> io::Result<RegKey> { self.root.open_subkey_with_flags(path, KEY_READ | KEY_WRITE) }
    fn create(&self, path: &str) -> io::Result<RegKey> { Ok(self.root.create_subkey(path)?.0) }
    fn class(id: &str) -> String { format!(r"Software\Classes\{id}") }
    fn claims(ext: &str) -> String { format!(r"Software\Classes\.{ext}\OpenWithProgids") }
    fn read_command(&self, id: &str) -> io::Result<Option<String>> {
        match self.root.open_subkey_with_flags(format!(r"{}\shell\open\command", Self::class(id)), KEY_READ)
            .and_then(|k| k.get_value("")) {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn command(&self, id: &str) -> Option<String> {
        self.read_command(id).ok().flatten()
    }
    fn has_claim(&self, ext: &str, id: &str) -> bool {
        self.open(&Self::claims(ext)).is_ok_and(|k| k.get_raw_value(id).is_ok())
    }
    /// `has_claim` with a read-only open, for checks that must never ask for write access.
    fn has_claim_read(&self, ext: &str, id: &str) -> bool {
        self.root.open_subkey_with_flags(Self::claims(ext), KEY_READ).is_ok_and(|k| k.get_raw_value(id).is_ok())
    }
    fn registered(&self) -> bool { self.command(LEGACY).is_some() }
    // 1.1.0-rc.1 (issue #11): a family is on when ANY of its extensions is claimed. Reading exts[0]
    // only made Update delete a sibling's claim (a lone .heif, or .jpeg without .jpg).
    fn family_states(&self) -> Vec<bool> {
        ASSOC_FAMILIES.iter().map(|(_, exts)| exts.iter().any(|e| self.has_claim(e, LEGACY)
            || self.has_claim(e, &file_icons::progid(e)))).collect()
    }
    /// 1.1.0-rc.1 (issue #11): READ-ONLY — do photos double-clicked in Explorer start THIS exe? The
    /// legacy command and every claimed format's own command must name it (`owned_command_path`). A
    /// missing, unreadable or foreign command means no, which is what the update prompt asks about.
    /// One file probe per distinct command string: Update writes the same command everywhere.
    fn opens_here(&self, exe: &str) -> bool {
        let Some(running) = canonical_exe(exe) else { return false };
        let mut owned = std::collections::HashMap::new();
        let mut here = |command: &str| *owned.entry(command.to_owned())
            .or_insert_with(|| owned_command_path(command, &running).is_some());
        let Ok(Some(command)) = self.read_command(LEGACY) else { return false };
        if !here(&command) { return false; }
        for (_, exts) in ASSOC_FAMILIES {
            for ext in *exts {
                let id = file_icons::progid(ext);
                if !self.has_claim_read(ext, LEGACY) && !self.has_claim_read(ext, &id) { continue; }
                match self.read_command(&id) {
                    Ok(Some(command)) => if !here(&command) { return false; },
                    Ok(None) => {} // the claim falls back to the legacy class, checked above
                    Err(_) => return false,
                }
            }
        }
        true
    }
    fn write_class(&self, id: &str, command: &str, icon: &str) -> io::Result<()> {
        let key = self.create(&Self::class(id))?;
        let _ = key.delete_value(""); // Preserve per-format Type text in Explorer.
        self.create(&format!(r"{}\shell\open\command", Self::class(id)))?.set_value("", &command)?;
        self.create(&format!(r"{}\DefaultIcon", Self::class(id)))?.set_value("", &icon)?;
        Ok(())
    }
    fn apply(&self, on: &[bool], exe: &str, distinct: bool) -> io::Result<()> {
        if on.len() != ASSOC_FAMILIES.len() { return Err(io::Error::other("invalid association family count")); }
        let command = format!("\"{exe}\" \"%1\"");
        self.write_class(LEGACY, &command, &file_icons::reference(exe, file_icons::GENERIC_DOCUMENT_ID))?;
        let caps = self.create(CAPS)?;
        caps.set_value("ApplicationName", &"Falcon Photo Viewer")?;
        caps.set_value("ApplicationDescription", &"Fast photo culling and review")?;
        let fa = self.create(&format!(r"{CAPS}\FileAssociations"))?;
        self.create(r"Software\RegisteredApplications")?.set_value("Falcon", &CAPS)?;
        for ((_, exts), enabled) in ASSOC_FAMILIES.iter().zip(on) {
            for ext in *exts {
                let id = file_icons::progid(ext);
                if *enabled {
                    self.write_class(&id, &command, &file_icons::reference(exe, file_icons::resource(ext, distinct)))?;
                    let claims = self.create(&Self::claims(ext))?;
                    claims.set_value(&id, &"")?;
                    let _ = claims.delete_value(LEGACY);
                    fa.set_value(format!(".{ext}"), &id)?;
                } else {
                    if let Ok(claims) = self.open(&Self::claims(ext)) {
                        let _ = claims.delete_value(LEGACY);
                        let _ = claims.delete_value(&id);
                    }
                    let _ = fa.delete_value(format!(".{ext}"));
                    // Retain handlers so an existing UserChoice never points at a deleted command.
                    // 1.1.0-rc.1 (issue #11): …and re-point them to this exe, or a UserChoice that
                    // names one keeps launching the old (possibly deleted) copy. Only an EXISTING
                    // class; no claim or capability is added, so the family still reads as off. A
                    // mistyped command here must not fail an Update that succeeds today.
                    if self.read_command(&id).ok().flatten().is_some() {
                        self.write_class(&id, &command, &file_icons::reference(exe, file_icons::resource(ext, distinct)))?;
                    }
                }
            }
        }
        // Opening photos is the core operation. Icons are optional: a blocked DLL,
        // unsupported architecture or partial helper registration cannot undo it.
        let helper_result = (|| {
            let dir = self.icon_dir.as_deref().ok_or_else(|| io::Error::other("LOCALAPPDATA is unavailable"))?;
            let helper = bridge::deploy(dir)?;
            self.install_bridge(exe, &helper)
        })();
        if let Err(e) = helper_result {
            crate::support::log_event(&format!("assoc: optional icon helper unavailable: {e}"));
            // Do not keep a known damaged DLL just because its file still exists,
            // or revive partially changed helper metadata. A later Update retries.
            if let Err(e) = self.use_static_legacy_icon(exe) {
                crate::support::log_event(&format!("assoc: icon fallback cleanup failed: {e}"));
            }
            // 1.1.0-rc.1 (issue #11): an earlier helper's FallbackIcon must follow this Update too,
            // or it keeps naming the old exe. Only an existing key; never create it here.
            if let Ok(key) = self.open(bridge::CLASS_KEY) {
                if let Err(e) = changed_value(&key, "FallbackIcon", &file_icons::reference(exe, file_icons::GENERIC_DOCUMENT_ID)) {
                    crate::support::log_event(&format!("assoc: icon fallback re-point failed: {e}"));
                }
            }
        }
        Ok(())
    }
    fn install_bridge(&self, exe: &str, dll: &Path) -> io::Result<()> {
        // No extension-level DefaultIcon: that would override another chosen app.
        // Only the old Falcon class uses this helper; UserChoice stays exactly as it is.
        let key = self.create(bridge::CLASS_KEY)?;
        key.set_value("FallbackIcon", &file_icons::reference(exe, file_icons::GENERIC_DOCUMENT_ID))?;
        let server = self.create(&format!(r"{}\InprocServer32", bridge::CLASS_KEY))?;
        server.set_value("", &dll.to_string_lossy().as_ref())?;
        server.set_value("ThreadingModel", &"Apartment")?;
        self.create(&format!(r"{}\shellex\IconHandler", Self::class(LEGACY)))?.set_value("", &bridge::CLSID)?;
        self.create(&format!(r"{}\DefaultIcon", Self::class(LEGACY)))?.set_value("", &"%1")?;
        Ok(())
    }
    fn has_bridge(&self) -> bool {
        self.open(&format!(r"{}\shellex\IconHandler", Self::class(LEGACY)))
            .and_then(|k| k.get_value::<String, _>("")).ok().as_deref() == Some(bridge::CLSID)
    }
    fn bridge_file_present(&self) -> bool {
        self.has_bridge() && self.root.open_subkey(format!(r"{}\InprocServer32", bridge::CLASS_KEY))
            .and_then(|k| k.get_value::<String, _>("")).is_ok_and(|p| {
                let path = Path::new(&p);
                path.is_absolute() && path.is_file()
            })
    }
    fn use_static_legacy_icon(&self, exe: &str) -> io::Result<bool> {
        // Commit the static fallback first, then detach only our own handler value.
        // COM metadata can stay for retry/Remove, but no file type loads it now.
        let mut changed = changed_value(&self.create(&format!(r"{}\DefaultIcon", Self::class(LEGACY)))?, "",
            &file_icons::reference(exe, file_icons::GENERIC_DOCUMENT_ID))?;
        if self.has_bridge() {
            changed |= delete_value_if_present(&self.open(&format!(r"{}\shellex\IconHandler", Self::class(LEGACY)))?, "")?;
        }
        Ok(changed)
    }
    fn upgrade_icons(&self, exe: &str, distinct: bool) -> io::Result<bool> {
        let Some(running) = canonical_exe(exe) else { return Ok(false) };
        let Some(command) = self.command(LEGACY) else { return Ok(false) };
        let Some(registered_exe) = owned_command_path(&command, &running) else { return Ok(false) };
        // Preserve the explicit Update's helper across boot. Only Update deploys native
        // Shell code; older registrations keep the static fallback until that action.
        let mut changed = if self.bridge_file_present() {
            changed_value(&self.create(&format!(r"{}\DefaultIcon", Self::class(LEGACY)))?, "", "%1")?
        } else {
            self.use_static_legacy_icon(registered_exe)?
        };
        // Migrate actual per-extension claims, not family checkbox approximations. Preserve defaults
        // and existing commands, including a moved executable until the user presses Update.
        for (_, exts) in ASSOC_FAMILIES {
            for ext in *exts {
                let id = file_icons::progid(ext);
                let old_claim = self.has_claim(ext, LEGACY);
                let new_claim = self.has_claim(ext, &id);
                if !old_claim && !new_claim { continue; }
                let existing = self.read_command(&id)?;
                let target_command = existing.as_deref().unwrap_or(&command);
                let Some(icon_exe) = owned_command_path(target_command, &running) else { continue };
                if existing.is_none() {
                    self.write_class(&id, &command, &file_icons::reference(icon_exe, file_icons::resource(ext, distinct)))?;
                    changed = true;
                }
                changed |= changed_value(&self.create(&format!(r"{}\DefaultIcon", Self::class(&id)))?, "",
                    &file_icons::reference(icon_exe, file_icons::resource(ext, distinct)))?;
                // Complete a partially interrupted upgrade even after the old claim was removed.
                // Only replace an existing Falcon mapping; never add a disabled capability.
                match self.open(&format!(r"{CAPS}\FileAssociations")) {
                    Ok(fa) => {
                        if fa.get_value::<String, _>(format!(".{ext}")).ok().as_deref() == Some(LEGACY) {
                            fa.set_value(format!(".{ext}"), &id)?;
                            changed = true;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                if old_claim {
                    let claims = self.open(&Self::claims(ext))?;
                    claims.set_value(&id, &"")?;
                    claims.delete_value(LEGACY)?;
                    changed = true;
                }
            }
        }
        Ok(changed)
    }
    fn raw_icons(&self, exe: &str, distinct: bool) -> io::Result<bool> {
        let Some(running) = canonical_exe(exe) else { return Ok(false) };
        let Some(command) = self.command(LEGACY) else { return Ok(false) };
        if owned_command_path(&command, &running).is_none() { return Ok(false); }
        let mut changed = false;
        for ext in falcon_decode::RAW_EXTS {
            let id = file_icons::progid(ext);
            if !self.has_claim(ext, &id) { continue; }
            let Some(command) = self.read_command(&id)? else { continue };
            let Some(icon_exe) = owned_command_path(&command, &running) else { continue };
            // Icon-only: no class creation, commands, claims, capabilities or defaults.
            let key = self.open(&format!(r"{}\DefaultIcon", Self::class(&id)))?;
            changed |= changed_value(&key, "", &file_icons::reference(icon_exe, file_icons::resource(ext, distinct)))?;
        }
        Ok(changed)
    }
    fn cleanup_heic(&self, exe: &str) -> io::Result<bool> {
        let Some(running) = canonical_exe(exe) else { return Ok(false) };
        let Some(command) = self.command(LEGACY) else { return Ok(false) };
        if owned_command_path(&command, &running).is_none() { return Ok(false); }
        let mut changed = false;
        // Both extensions independently: an old partial registration may contain only .heif.
        for ext in ASSOC_FAMILIES[crate::support::HEIC_FAMILY_IDX].1 {
            let id = file_icons::progid(ext);
            let existing = self.read_command(&id)?;
            let format_owned = existing.as_deref().is_none_or(|c| owned_command_path(c, &running).is_some());
            match self.open(&Self::claims(ext)) {
                Ok(k) => {
                    // Legacy and per-format claims can have different owners. The gate above
                    // proved legacy ownership; a foreign per-format handler doesn't override it.
                    changed |= delete_value_if_present(&k, LEGACY)?;
                    if format_owned { changed |= delete_value_if_present(&k, &id)?; }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            match self.open(&format!(r"{CAPS}\FileAssociations")) {
                Ok(k) => {
                    if k.get_value::<String, _>(format!(".{ext}")).ok().is_some_and(|v| v == LEGACY || (format_owned && v == id)) {
                        changed |= delete_value_if_present(&k, &format!(".{ext}"))?;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(changed)
    }
    fn repair_type_name(&self, exe: &str) -> bool {
        let Some(running) = canonical_exe(exe) else { return false };
        let Some(command) = self.command(LEGACY) else { return false };
        if owned_command_path(&command, &running).is_none() { return false; }
        self.open(&Self::class(LEGACY)).is_ok_and(|k| k.delete_value("").is_ok())
    }
    fn remove(&self) {
        for (_, exts) in ASSOC_FAMILIES {
            for ext in *exts {
                let id = file_icons::progid(ext);
                if let Ok(k) = self.open(&Self::claims(ext)) {
                    let _ = k.delete_value(LEGACY); let _ = k.delete_value(&id);
                }
                let _ = self.root.delete_subkey_all(Self::class(&id));
            }
        }
        let _ = self.root.delete_subkey_all(Self::class(LEGACY));
        let _ = self.root.delete_subkey_all(bridge::CLASS_KEY);
        let _ = self.root.delete_subkey_all(CAPS);
        let _ = self.root.delete_subkey(r"Software\Falcon");
        if let Ok(ra) = self.open(r"Software\RegisteredApplications") { let _ = ra.delete_value("Falcon"); }
    }
}

fn shell_changed() {
    #[link(name = "shell32")]
    extern "system" { fn SHChangeNotify(event: i32, flags: u32, item1: *const core::ffi::c_void, item2: *const core::ffi::c_void); }
    unsafe { SHChangeNotify(0x0800_0000, 0, std::ptr::null(), std::ptr::null()) };
}
pub(crate) fn assoc_registered() -> bool { Registration::current_user().registered() }
pub(crate) fn assoc_family_states() -> Vec<bool> { Registration::current_user().family_states() }
/// Read-only (the update prompt's "already open in this version?"); never notifies the shell.
pub(crate) fn assoc_opens_here() -> bool {
    std::env::current_exe().is_ok_and(|exe| Registration::current_user().opens_here(&exe.display().to_string()))
}
pub(crate) fn assoc_apply(on: &[bool], distinct: bool) -> io::Result<()> {
    let result = Registration::current_user().apply(on, &std::env::current_exe()?.display().to_string(), distinct);
    shell_changed(); result
}
pub(crate) fn assoc_upgrade_icons(distinct: bool) -> io::Result<()> {
    let result = Registration::current_user().upgrade_icons(&std::env::current_exe()?.display().to_string(), distinct);
    if !matches!(result, Ok(false)) { shell_changed(); }
    result.map(|_| ())
}
pub(crate) fn assoc_raw_icons(distinct: bool) -> io::Result<()> {
    let result = Registration::current_user().raw_icons(&std::env::current_exe()?.display().to_string(), distinct);
    // Even partial writes after a failure must be visible/retryable.
    if !matches!(result, Ok(false)) { shell_changed(); } result.map(|_| ())
}
pub(crate) fn assoc_cleanup_heic() -> io::Result<bool> {
    let result = Registration::current_user().cleanup_heic(&std::env::current_exe()?.display().to_string());
    if !matches!(result, Ok(false)) { shell_changed(); } result
}
pub(crate) fn assoc_repair_type_name() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let changed = Registration::current_user().repair_type_name(&exe.display().to_string());
    if changed { shell_changed(); } changed
}
pub(crate) fn assoc_remove() { Registration::current_user().remove(); shell_changed(); }
pub(crate) fn open_default_apps() {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("cmd").args(["/c", "start", "", "ms-settings:defaultapps"])
        .creation_flags(0x0800_0000).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    struct Scratch { path: String, dir: std::path::PathBuf }
    impl Scratch {
        fn exe(&self, other: bool) -> String {
            self.dir.join(if other { "Falcon Other.exe" } else { "Falcon Owner É.exe" }).display().to_string()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.path);
            let _ = std::fs::remove_file(self.dir.join("shell-icons").join(bridge::NAME));
            let _ = std::fs::remove_dir(self.dir.join("shell-icons"));
            for name in ["Falcon Other.exe", "Falcon Owner É.exe"] { let _ = std::fs::remove_file(self.dir.join(name)); }
            let _ = std::fs::remove_dir(self.dir.join("alias"));
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
    fn scratch() -> (Registration, Scratch) {
        let path = format!(r"Software\FalconIconTests\{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let root = RegKey::predef(HKEY_CURRENT_USER).create_subkey(&path).unwrap().0;
        let dir = std::env::temp_dir().join(format!("falcon-icon-owner-{}", path.rsplit('\\').next().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::create_dir(dir.join("alias")).unwrap();
        for name in ["Falcon Other.exe", "Falcon Owner É.exe"] { std::fs::write(dir.join(name), []).unwrap(); }
        (Registration { root, icon_dir: Some(dir.join("shell-icons")) }, Scratch { path, dir })
    }
    fn snapshot(key: &RegKey) -> BTreeMap<String, String> {
        fn walk(key: &RegKey, prefix: &str, out: &mut BTreeMap<String, String>) {
            for v in key.enum_values() { let (name, value) = v.unwrap(); out.insert(format!("{prefix}@{name}"), format!("{value:?}")); }
            for name in key.enum_keys() { let name = name.unwrap(); walk(&key.open_subkey(&name).unwrap(), &format!("{prefix}\\{name}"), out); }
        }
        let mut out = BTreeMap::new(); walk(key, "", &mut out); out
    }
    fn without_raw_icons(map: BTreeMap<String, String>) -> BTreeMap<String, String> {
        map.into_iter().filter(|(k, _)| !falcon_decode::RAW_EXTS.iter()
            .any(|ext| k == &format!(r"\Software\Classes\Falcon.Image.{ext}\DefaultIcon@"))).collect()
    }
    /// O1 falsifier: propagate deploy/install errors instead of completing core Update.
    #[test]
    fn helper_failure_does_not_block_file_opening_registration() {
        for failure in ["no directory", "blocked directory", "mismatching helper"] {
            let (mut r, fixture) = scratch();
            let exe = fixture.exe(false);
            let choice = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.png\UserChoiceLatest\ProgId";
            r.create(choice).unwrap().set_value("ProgId", &LEGACY).unwrap();
            r.create(choice).unwrap().set_value("Hash", &"protected-default").unwrap();
            let foreign = r"Software\Classes\.png\OpenWithProgids";
            r.create(foreign).unwrap().set_value("OtherApp", &"").unwrap();
            match failure {
                "no directory" => r.icon_dir = None,
                "blocked directory" => {
                    let blocked = fixture.dir.join("blocked");
                    std::fs::write(&blocked, []).unwrap();
                    r.icon_dir = Some(blocked);
                }
                _ => {
                    // A previously installed copy may have been damaged. A file that
                    // merely exists is not enough reason to retain this known bad DLL.
                    r.apply(&[true; 9], &exe, false).unwrap();
                    std::fs::write(r.icon_dir.as_ref().unwrap().join(bridge::NAME), b"damaged").unwrap();
                }
            }
            let selected = [true, true, false, false, false, false, false, false, false];
            r.apply(&selected, &exe, false).unwrap();
            assert_eq!(r.command(LEGACY).unwrap(), format!("\"{exe}\" \"%1\""));
            assert_eq!(r.family_states(), selected);
            assert_eq!(r.open(&format!(r"{CAPS}\FileAssociations")).unwrap()
                .get_value::<String,_>(".png").unwrap(), "Falcon.Image.png");
            assert_eq!(r.open(r"Software\RegisteredApplications").unwrap().get_value::<String,_>("Falcon").unwrap(), CAPS);
            assert_eq!(r.open(r"Software\Classes\Falcon.Image\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(),
                file_icons::reference(&exe, file_icons::GENERIC_DOCUMENT_ID));
            assert!(!r.has_bridge(), "do not load the failed helper: {failure}");
            assert!(r.has_claim("png", "OtherApp"));
            assert_eq!(r.open(choice).unwrap().get_value::<String,_>("Hash").unwrap(), "protected-default");
            let _ = std::fs::remove_file(fixture.dir.join("blocked"));
        }
    }

    /// Y1 falsifier: trust the IconHandler value without checking its registered DLL file.
    #[test]
    fn missing_helper_on_boot_restores_static_icon_only_for_registered_copy() {
        let (r, fixture) = scratch(); let exe = fixture.exe(false);
        r.apply(&[true; 9], &exe, false).unwrap();
        std::fs::remove_file(r.icon_dir.as_ref().unwrap().join(bridge::NAME)).unwrap();
        let before = snapshot(&r.root);
        assert!(!r.upgrade_icons(&fixture.exe(true), false).unwrap());
        assert_eq!(snapshot(&r.root), before, "other copy cannot repair this registration");
        assert!(r.upgrade_icons(&exe, false).unwrap());
        assert_eq!(r.open(r"Software\Classes\Falcon.Image\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(),
            file_icons::reference(&exe, file_icons::GENERIC_DOCUMENT_ID));
        assert!(!r.has_bridge());
        assert!(!r.icon_dir.as_ref().unwrap().join(bridge::NAME).exists(), "boot does not deploy code");
        assert!(!r.upgrade_icons(&exe, false).unwrap(), "the fallback is stable on the next boot");
        r.apply(&[true; 9], &exe, false).unwrap();
        assert!(r.has_bridge(), "a later explicit Update can repair it");
    }
    /// Falsifier: omit install_bridge from Update, reset it on boot, touch protected
    /// choices/extension icons, or leave the COM registration after Remove.
    #[test]
    fn update_repairs_legacy_icons_without_changing_default_choices() {
        let (r, fixture) = scratch(); let exe = fixture.exe(false);
        let choice = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.png\UserChoiceLatest\ProgId";
        r.create(choice).unwrap().set_value("ProgId", &LEGACY).unwrap();
        r.create(choice).unwrap().set_value("Hash", &"keep-protected-value").unwrap();
        r.create(r"Software\Classes\.png").unwrap().set_value("", &"OtherApp").unwrap();
        r.apply(&[true; 9], &exe, false).unwrap();
        let icon = r.open(r"Software\Classes\Falcon.Image\DefaultIcon").unwrap();
        assert_eq!(icon.get_value::<String,_>("").unwrap(), "%1");
        assert!(r.has_bridge());
        let helper: String = r.open(&format!(r"{}\InprocServer32", bridge::CLASS_KEY)).unwrap().get_value("").unwrap();
        assert!(Path::new(&helper).is_file());
        for ext in ["png", "PNG", "jpeg", "JPEG"] {
            let key = format!(r"Software\Classes\Falcon.Image.{ext}\DefaultIcon");
            let id = if ext.eq_ignore_ascii_case("png") { 11 } else { 10 };
            assert_eq!(r.open(&key).unwrap().get_value::<String,_>("").unwrap(), file_icons::reference(&exe, id));
        }
        let before = snapshot(&r.root);
        assert!(!r.upgrade_icons(&exe, false).unwrap());
        assert_eq!(before, snapshot(&r.root), "boot preserves Update's helper");
        assert_eq!(r.open(choice).unwrap().get_value::<String,_>("ProgId").unwrap(), LEGACY);
        assert_eq!(r.open(r"Software\Classes\.png").unwrap().get_value::<String,_>("").unwrap(), "OtherApp");
        assert!(r.open(r"Software\Classes\.png\DefaultIcon").is_err());
        r.remove();
        assert!(r.open(bridge::CLASS_KEY).is_err());
        assert_eq!(r.open(choice).unwrap().get_value::<String,_>("Hash").unwrap(), "keep-protected-value");
    }
    /// Falsifiers: remove the legacy command; migrate by all families instead of actual claims;
    /// call apply from raw_icons; leave our new ProgIDs on removal; erase another app's values.
    #[test]
    fn isolated_upgrade_toggle_and_removal_preserve_defaults_commands_and_disabled_claims() {
        let (r, fixture) = scratch();
        let owned_exe = fixture.exe(false);
        let exe = owned_exe.as_str();
        let original = format!("\"{exe}\" \"%1\"");
        let original = original.as_str();
        r.write_class(LEGACY, original, "old,0").unwrap();
        r.open(&Registration::class(LEGACY)).unwrap().set_value("", &"Falcon photo").unwrap();
        assert!(r.repair_type_name(exe)); assert!(!r.repair_type_name(exe));
        for ext in ["jpg", "jpeg", "cr3", "cr2"] {
            r.create(&Registration::claims(ext)).unwrap().set_value(LEGACY, &"").unwrap();
            r.create(&format!(r"{CAPS}\FileAssociations")).unwrap().set_value(format!(".{ext}"), &LEGACY).unwrap();
        }
        // Simulate defaults and other programs in the same private hive. None may be rewritten.
        let user_choice = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.cr3\UserChoice";
        r.create(user_choice).unwrap().set_value("ProgId", &LEGACY).unwrap();
        r.create(user_choice).unwrap().set_value("Hash", &"protected-by-Windows").unwrap();
        r.create(r"Software\Classes\.cr3").unwrap().set_value("", &"Another.Raw").unwrap();
        r.create(&Registration::claims("cr3")).unwrap().set_value("Another.Raw", &"").unwrap();
        assert!(r.upgrade_icons(exe, false).unwrap());
        assert!(!r.upgrade_icons(exe, false).unwrap(), "next boot is read-only");
        assert_eq!(r.command(LEGACY).as_deref(), Some(original));
        assert_eq!(r.command(&file_icons::progid("cr3")).as_deref(), Some(original));
        assert_eq!(r.family_states(), [true, false, false, false, false, false, false, false, true]);
        assert!(!r.has_claim("nef", &file_icons::progid("nef")), "partially disabled RAW stays disabled");
        assert!(!r.has_claim("cr3", LEGACY));
        assert_eq!(r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().get_value::<String,_>(".cr3").unwrap(), "Falcon.Image.cr3");
        // Falsifier: repair capabilities only inside old_claim. A prior interrupted upgrade
        // can leave only the new claim and the old default-app mapping; the next launch heals it.
        r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().set_value(".cr3", &LEGACY).unwrap();
        assert!(r.upgrade_icons(exe, false).unwrap());
        assert_eq!(r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().get_value::<String,_>(".cr3").unwrap(), "Falcon.Image.cr3");
        assert_eq!(r.open(user_choice).unwrap().get_value::<String,_>("ProgId").unwrap(), LEGACY);
        let before = snapshot(&r.root);
        assert!(r.raw_icons(exe, true).unwrap());
        let icon = r.open(r"Software\Classes\Falcon.Image.cr3\DefaultIcon").unwrap();
        assert_eq!(icon.get_value::<String,_>("").unwrap(), file_icons::reference(exe, file_icons::resource("cr3", true)));
        assert_eq!(without_raw_icons(before.clone()), without_raw_icons(snapshot(&r.root)), "only RAW icon values can change");
        r.raw_icons(exe, false).unwrap();
        assert_eq!(before, snapshot(&r.root), "Off restores the complete registry exactly");
        // Later registration must respect saved On and all existing codec/checkbox gates.
        r.apply(&[true, true, false, true, false, false, false, false, true], exe, true).unwrap();
        assert_ne!(r.open(r"Software\Classes\Falcon.Image.png\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(),
            r.open(r"Software\Classes\Falcon.Image.apng\DefaultIcon").unwrap().get_value::<String,_>("").unwrap());
        assert_eq!(icon.get_value::<String,_>("").unwrap(), file_icons::reference(exe, file_icons::resource("cr3", true)));
        let mut state = r.family_states(); crate::support::gate_heic(&mut state, false);
        r.apply(&state, exe, true).unwrap();
        assert!(!r.family_states()[crate::support::HEIC_FAMILY_IDX]);
        assert!(r.family_states()[0]);
        assert!(r.open(&Registration::class(LEGACY)).unwrap().get_value::<String,_>("").is_err());
        r.remove();
        assert!(!r.registered()); assert_eq!(r.family_states(), [false; 9]);
        for (_, exts) in ASSOC_FAMILIES { for ext in *exts {
            assert!(r.open(&Registration::class(&file_icons::progid(ext))).is_err());
        }}
        assert!(r.has_claim("cr3", "Another.Raw"));
        assert_eq!(r.open(user_choice).unwrap().get_value::<String,_>("Hash").unwrap(), "protected-by-Windows");
        assert_eq!(r.open(r"Software\Classes\.cr3").unwrap().get_value::<String,_>("").unwrap(), "Another.Raw");
    }
    /// O1 falsifier: remove upgrade_icons' registered-copy gate. Second-copy launches must
    /// leave the entire registry unchanged, including the legacy/default-app migration.
    #[test]
    fn second_copy_does_not_migrate_registered_icons() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        r.write_class(LEGACY, &format!("\"{owner}\" \"%1\""), "old,0").unwrap();
        r.open(&Registration::class(LEGACY)).unwrap().set_value("", &"Falcon photo").unwrap();
        for ext in ["jpg", "cr3", "heif"] {
            r.create(&Registration::claims(ext)).unwrap().set_value(LEGACY, &"").unwrap();
            r.create(&format!(r"{CAPS}\FileAssociations")).unwrap().set_value(format!(".{ext}"), &LEGACY).unwrap();
        }
        let before = snapshot(&r.root);
        assert!(!r.upgrade_icons(&other, true).unwrap());
        assert!(!r.cleanup_heic(&other).unwrap());
        assert!(!r.repair_type_name(&other));
        assert_eq!(before, snapshot(&r.root));
        assert!(r.upgrade_icons(&owner, true).unwrap(), "registered copy can migrate");
    }

    /// O1 falsifier: drop raw_icons' owner gate; a later explicit Update must still apply
    /// the saved choice and deliberately register the new copy for opening AND icons.
    #[test]
    fn second_copy_raw_preference_waits_for_explicit_update() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        let on = [true, false, false, false, false, false, false, false, true];
        r.apply(&on, &owner, false).unwrap();
        let before = snapshot(&r.root);
        assert!(!r.raw_icons(&other, true).unwrap());
        assert_eq!(before, snapshot(&r.root));
        r.apply(&on, &other, true).unwrap();
        assert_eq!(r.command(LEGACY).unwrap(), format!("\"{other}\" \"%1\""));
        assert_eq!(r.open(r"Software\Classes\Falcon.Image.cr3\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(),
            file_icons::reference(&other, file_icons::resource("cr3", true)));
        let after = snapshot(&r.root);
        assert!(!r.upgrade_icons(&owner, false).unwrap());
        assert!(!r.raw_icons(&owner, false).unwrap());
        assert_eq!(after, snapshot(&r.root), "old copy cannot reclaim icons");
    }

    /// Falsifier: compare raw spellings, accept unresolved paths, or use the running alias
    /// rather than the registered path when writing icons. Nothing launches these fixture files.
    #[test]
    fn registered_path_aliases_keep_the_registered_icon_reference() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false);
        let alias = fixture.dir.join("alias").join("..").join("FALCON OWNER É.EXE").display().to_string();
        let command = format!("\"{owner}\" \"%1\"");
        let canonical = canonical_exe(&alias).unwrap();
        assert_eq!(owned_command_path(&command, &canonical), Some(owner.as_str()));
        for malformed in [owner.clone(), "\"relative.exe\" \"%1\"".into(),
            format!("\"{owner}\"junk"), format!("\"{owner}.missing\" \"%1\""), String::new()] {
            assert!(owned_command_path(&malformed, &canonical).is_none());
            r.write_class(LEGACY, &malformed, "untouched,0").unwrap();
            let before = snapshot(&r.root);
            assert!(!r.upgrade_icons(&alias, true).unwrap());
            assert!(!r.raw_icons(&alias, true).unwrap());
            assert!(!r.cleanup_heic(&alias).unwrap());
            assert_eq!(before, snapshot(&r.root));
        }
        r.write_class(LEGACY, &command, "old,0").unwrap();
        r.create(&Registration::claims("cr3")).unwrap().set_value(LEGACY, &"").unwrap();
        assert!(r.upgrade_icons(&alias, false).unwrap());
        assert!(r.raw_icons(&alias, true).unwrap());
        assert_eq!(r.open(r"Software\Classes\Falcon.Image.cr3\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(),
            file_icons::reference(&owner, file_icons::resource("cr3", true)));
        assert_eq!(r.command(LEGACY).unwrap(), command);
    }

    /// Falsifier: use only the legacy gate; mixed or malformed per-format commands must
    /// not have their icons, claims or capabilities reassigned to the legacy owner's copy.
    #[test]
    fn mixed_or_invalid_format_owners_are_not_reassigned() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        r.apply(&[true; 9], &owner, false).unwrap();
        let classes = [("cr3", format!("\"{other}\" \"%1\"")),
            ("cr2", "\"missing-relative.exe\" \"%1\"".into()),
            ("heic", format!("\"{other}\" \"%1\""))];
        for (ext, command) in &classes {
            r.write_class(&file_icons::progid(ext), command, "preserve,7").unwrap();
            r.create(&Registration::claims(ext)).unwrap().set_value(LEGACY, &"").unwrap();
            r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().set_value(format!(".{ext}"), &LEGACY).unwrap();
        }
        assert!(!r.upgrade_icons(&owner, false).unwrap());
        r.raw_icons(&owner, true).unwrap();
        r.cleanup_heic(&owner).unwrap();
        for (ext, command) in &classes {
            assert_eq!(r.command(&file_icons::progid(ext)).as_ref(), Some(command));
            assert_eq!(r.open(&format!(r"{}\DefaultIcon", Registration::class(&file_icons::progid(ext)))).unwrap()
                .get_value::<String,_>("").unwrap(), "preserve,7");
            assert!(r.has_claim(ext, &file_icons::progid(ext)), "foreign per-format claims stay");
            let cap = r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().get_value::<String,_>(format!(".{ext}"));
            if *ext == "heic" {
                assert!(!r.has_claim(ext, LEGACY), "independently owned legacy HEIC claim is cleaned");
                assert!(cap.is_err());
            } else {
                assert!(r.has_claim(ext, LEGACY)); assert_eq!(cap.unwrap(), LEGACY);
            }
        }
        // An unreadable/mistyped existing value is an error, not permission to synthesize it.
        let key = r.open(r"Software\Classes\Falcon.Image.nef\shell\open\command").unwrap();
        key.set_value("", &7_u32).unwrap();
        let before = snapshot(&r.root);
        assert!(r.upgrade_icons(&owner, true).is_err());
        assert_eq!(before, snapshot(&r.root));
    }

    /// Falsifier: implement boot cleanup with full Apply, miss a lone HEIF entry, or delete
    /// other apps' claims/capabilities. Check the actual registry, including unchanged handlers.
    #[test]
    fn boot_heic_cleanup_is_owned_removal_only_including_lone_heif() {
        let (r, fixture) = scratch(); let owner = fixture.exe(false); let other = fixture.exe(true);
        r.apply(&[true, false, false, true, false, false, false, false, true], &owner, false).unwrap();
        let heic = r.open(&Registration::claims("heic")).unwrap();
        heic.delete_value("Falcon.Image.heic").unwrap(); // only the .heif claim is left
        assert!(r.family_states()[crate::support::HEIC_FAMILY_IDX], "a lone .heif claim keeps the HEIC family on (any-extension rule)");
        let heif = r.open(&Registration::claims("heif")).unwrap();
        heif.set_value(LEGACY, &"").unwrap(); heif.set_value("Another.Heif", &"").unwrap();
        let caps = r.open(&format!(r"{CAPS}\FileAssociations")).unwrap();
        caps.set_value(".heic", &"Another.Heic").unwrap();
        let before = snapshot(&r.root);
        assert!(!r.cleanup_heic(&other).unwrap());
        assert_eq!(before, snapshot(&r.root));
        assert!(r.cleanup_heic(&owner).unwrap());
        let mut expected = before;
        for key in [r"\Software\Classes\.heif\OpenWithProgids@Falcon.Image",
            r"\Software\Classes\.heif\OpenWithProgids@Falcon.Image.heif",
            r"\Software\Falcon\Capabilities\FileAssociations@.heif"] { assert!(expected.remove(key).is_some()); }
        assert_eq!(expected, snapshot(&r.root));
        assert!(!r.cleanup_heic(&owner).unwrap());
    }

    /// Falsifier: skip the entire extension for a foreign format handler, or remove its
    /// capability along with our legacy claim. Each registry value has its own owner.
    #[test]
    fn heic_cleanup_preserves_foreign_capability_while_removing_owned_legacy_claim() {
        let (r, fixture) = scratch(); let owner = fixture.exe(false); let other = fixture.exe(true);
        r.apply(&[false, false, false, true, false, false, false, false, false], &owner, false).unwrap();
        let id = file_icons::progid("heif");
        r.write_class(&id, &format!("\"{other}\" \"%1\""), "other,0").unwrap();
        r.create(&Registration::claims("heif")).unwrap().set_value(LEGACY, &"").unwrap();
        assert!(r.cleanup_heic(&owner).unwrap());
        assert!(!r.has_claim("heif", LEGACY));
        assert!(r.has_claim("heif", &id));
        assert_eq!(r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().get_value::<String,_>(".heif").unwrap(), id);
        assert_eq!(r.open(r"Software\Classes\Falcon.Image.heif\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(), "other,0");
        assert!(!r.cleanup_heic(&owner).unwrap());
    }

    /// Falsifier: create registration or claims while changing the preference before registration,
    /// for a real absolute owner exe (1.1.0-rc.1, issue #11 batch Q1: the relative `"app.exe"` returned
    /// early in `canonical_exe`, so the unregistered path was never reached), in every boot repair or
    /// the read-only "photos open here" check.
    #[test]
    fn unregistered_preferences_do_not_create_any_registry_entries() {
        let (r, fixture) = scratch();
        let exe = fixture.exe(false);
        assert!(!r.upgrade_icons(&exe, true).unwrap());
        assert!(!r.upgrade_icons(&exe, false).unwrap());
        assert!(!r.raw_icons(&exe, true).unwrap());
        assert!(!r.cleanup_heic(&exe).unwrap());
        assert!(!r.repair_type_name(&exe));
        assert!(!r.opens_here(&exe), "nothing is registered, so photos don't open here");
        assert!(!r.upgrade_icons("app.exe", true).unwrap());
        assert!(!r.raw_icons("app.exe", true).unwrap());
        assert!(snapshot(&r.root).is_empty());
    }

    fn command_for(exe: &str) -> String { format!("\"{exe}\" \"%1\"") }

    /// Falsifier: check only the legacy command, count an unclaimed retained class, accept an
    /// unreadable format command, compare raw spellings, or write anything while checking.
    #[test]
    fn opens_here_only_for_the_copy_every_claimed_command_names() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        assert!(!r.opens_here(&owner), "nothing registered");
        assert!(snapshot(&r.root).is_empty(), "the check creates nothing");
        r.apply(&[true; 9], &owner, false).unwrap();
        let before = snapshot(&r.root);
        let alias = fixture.dir.join("alias").join("..").join("FALCON OWNER É.EXE").display().to_string();
        assert!(r.opens_here(&owner));
        assert!(r.opens_here(&alias), "an equivalent spelling of the registered exe");
        assert!(!r.opens_here(&other), "photos open in the owner copy");
        assert_eq!(before, snapshot(&r.root), "read-only");
        // One claimed format still opening another copy: that format's double-click opens it.
        r.write_class(&file_icons::progid("cr3"), &command_for(&other), "other,0").unwrap();
        let before = snapshot(&r.root);
        assert!(!r.opens_here(&owner));
        assert_eq!(before, snapshot(&r.root));
        r.write_class(&file_icons::progid("cr3"), &command_for(&owner), "owner,0").unwrap();
        assert!(r.opens_here(&owner));
        // An unreadable format command is not "open in this version".
        let nef = r.open(r"Software\Classes\Falcon.Image.nef\shell\open\command").unwrap();
        nef.set_value("", &7_u32).unwrap();
        assert!(!r.opens_here(&owner));
        nef.set_value("", &command_for(&owner)).unwrap();
        // A switched-off family's retained class is not a claim, whatever it names.
        r.apply(&[true, true, true, true, true, true, true, true, false], &owner, false).unwrap();
        r.write_class(&file_icons::progid("cr3"), &command_for(&other), "other,0").unwrap();
        assert!(r.opens_here(&owner), "an unclaimed class does not open photos");
        // The registered exe was deleted: Explorer can't start this copy either.
        std::fs::remove_file(&owner).unwrap();
        assert!(!r.opens_here(&owner));
    }

    /// Falsifier: skip re-pointing per-format commands or icons, or rewrite UserChoice, claims,
    /// capabilities or another app's values while Update moves the registration to a new copy.
    #[test]
    fn update_from_new_copy_after_owner_deleted_repoints_only_exe_references() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        let choice = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.jpg\UserChoice";
        r.create(choice).unwrap().set_value("ProgId", &"Falcon.Image.jpg").unwrap();
        r.create(choice).unwrap().set_value("Hash", &"protected").unwrap();
        r.create(r"Software\Classes\.jpg").unwrap().set_value("", &"OtherApp").unwrap();
        r.create(&Registration::claims("jpg")).unwrap().set_value("OtherApp", &"").unwrap();
        r.apply(&[true; 9], &owner, false).unwrap();
        std::fs::remove_file(&owner).unwrap();
        let before = snapshot(&r.root);
        let st = r.family_states(); assert_eq!(st, [true; 9]);
        r.apply(&st, &other, false).unwrap();
        assert_eq!(r.command(LEGACY), Some(command_for(&other)));
        for (_, exts) in ASSOC_FAMILIES { for ext in *exts {
            let id = file_icons::progid(ext);
            assert_eq!(r.command(&id), Some(command_for(&other)), "{ext}");
            assert_eq!(r.open(&format!(r"{}\DefaultIcon", Registration::class(&id))).unwrap().get_value::<String,_>("").unwrap(),
                file_icons::reference(&other, file_icons::resource(ext, false)), "{ext}");
        }}
        let after = snapshot(&r.root);
        assert!(after.values().all(|v| !v.contains(&owner)), "no value names the deleted copy");
        let exe_refs = |m: &BTreeMap<String, String>| -> BTreeMap<String, String> { m.iter()
            .filter(|(k, _)| !k.ends_with(r"\shell\open\command@") && !k.ends_with(r"\DefaultIcon@") && !k.ends_with("@FallbackIcon"))
            .map(|(k, v)| (k.clone(), v.clone())).collect() };
        assert_eq!(exe_refs(&before), exe_refs(&after), "only exe references change");
        assert!(r.opens_here(&other));
        assert!(!r.upgrade_icons(&other, false).unwrap(), "nothing left for the next boot");
    }

    /// Falsifier: leave the CLSID FallbackIcon on the old exe when the icon helper is unavailable.
    #[test]
    fn update_from_new_copy_with_helper_unavailable_repoints_fallback_icon() {
        let (mut r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        r.apply(&[true; 9], &owner, false).unwrap();
        assert!(r.has_bridge());
        r.icon_dir = None;
        std::fs::remove_file(&owner).unwrap();
        r.apply(&r.family_states(), &other, false).unwrap();
        assert_eq!(r.open(r"Software\Classes\Falcon.Image\DefaultIcon").unwrap().get_value::<String,_>("").unwrap(),
            file_icons::reference(&other, file_icons::GENERIC_DOCUMENT_ID));
        assert_eq!(r.open(bridge::CLASS_KEY).unwrap().get_value::<String,_>("FallbackIcon").unwrap(),
            file_icons::reference(&other, file_icons::GENERIC_DOCUMENT_ID));
        assert!(!r.has_bridge());
        assert!(snapshot(&r.root).values().all(|v| !v.contains(&owner)));
        // Never created when it isn't there.
        let (mut fresh, fixture) = scratch();
        fresh.icon_dir = None;
        fresh.apply(&[true; 9], &fixture.exe(false), false).unwrap();
        assert!(fresh.open(bridge::CLASS_KEY).is_err());
    }

    /// Falsifier: skip the disabled branch's re-point, or re-add its claim or capability.
    #[test]
    fn update_repoints_retained_handler_of_disabled_family() {
        let (r, fixture) = scratch();
        let owner = fixture.exe(false); let other = fixture.exe(true);
        r.apply(&[true; 9], &owner, false).unwrap();
        let choice = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.cr3\UserChoice";
        r.create(choice).unwrap().set_value("ProgId", &"Falcon.Image.cr3").unwrap();
        let raw_off = [true, true, true, true, true, true, true, true, false];
        r.apply(&raw_off, &owner, false).unwrap();
        std::fs::remove_file(&owner).unwrap();
        r.apply(&r.family_states(), &other, false).unwrap();
        assert_eq!(r.command("Falcon.Image.cr3"), Some(command_for(&other)));
        assert!(!r.has_claim("cr3", "Falcon.Image.cr3"));
        assert!(r.open(&format!(r"{CAPS}\FileAssociations")).unwrap().get_value::<String,_>(".cr3").is_err());
        assert!(!r.family_states()[8], "the family stays off");
        assert_eq!(r.open(choice).unwrap().get_value::<String,_>("ProgId").unwrap(), "Falcon.Image.cr3");
    }

    /// Falsifier: read a family's state from its first extension only.
    #[test]
    fn family_state_counts_any_claimed_extension() {
        let (r, fixture) = scratch(); let owner = fixture.exe(false);
        r.write_class(LEGACY, &command_for(&owner), "old,0").unwrap();
        r.create(&Registration::claims("jpeg")).unwrap().set_value("Falcon.Image.jpeg", &"").unwrap();
        r.create(&Registration::claims("heif")).unwrap().set_value(LEGACY, &"").unwrap();
        let st = r.family_states();
        assert!(st[0], "a lone .jpeg claim keeps JPEG on");
        assert!(st[crate::support::HEIC_FAMILY_IDX], "a lone .heif claim keeps HEIC on");
        r.apply(&st, &owner, false).unwrap();
        assert!(r.has_claim("jpeg", "Falcon.Image.jpeg"), "Update keeps the sibling's claim");
        assert!(r.has_claim("heif", "Falcon.Image.heif"));
    }

    /// Falsifier: treat an unknown codec answer as "not installed" at boot or in Update.
    #[test]
    fn unknown_heic_codec_state_never_removes_heic_claims() {
        use crate::support::{gate_heic, heic_assoc_available};
        for state in [Some(true), None, Some(false)] {
            let (r, fixture) = scratch(); let owner = fixture.exe(false);
            r.apply(&[true; 9], &owner, false).unwrap();
            // The boot repair and Update as main.rs composes them.
            if !heic_assoc_available(state) { r.cleanup_heic(&owner).unwrap(); }
            let mut on = r.family_states(); gate_heic(&mut on, heic_assoc_available(state));
            r.apply(&on, &owner, false).unwrap();
            for ext in ["heic", "heif"] {
                assert_eq!(r.has_claim(ext, &file_icons::progid(ext)), state != Some(false), "{ext} with {state:?}");
            }
        }
    }
}
