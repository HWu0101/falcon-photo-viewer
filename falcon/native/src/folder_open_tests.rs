//! Regression contracts for the shared argv/drop scan path and metadata-backed sort.
use super::*;

#[test]
fn partial_photo_edits_do_not_enable_whole_folder_or_file_operations() {
    let source = include_str!("main.rs");
    for callback in ["discard_rotations", "apply_rotations", "copy_picks", "move_rejects",
        "delete_recycle", "export_web", "open_export", "toggle_xmp_sync", "xmp_backfill_confirm"] {
        // Assert the production callback gate, so native menus and shortcut calls
        // cannot bypass a disabled Slint control. This is the owner-approved scope.
        let marker = format!("app.on_{callback}(move");
        let body = source.split_once(&marker).unwrap().1.split_once("{ return; }").unwrap().0;
        assert!(body.contains("a.get_opening_photo()"), "{callback} must wait for the folder");
        assert!(!body.contains("get_photo_open_edits"), "{callback} is not a per-photo edit");
    }
}

#[test]
fn latest_explicit_open_cancels_both_queued_and_running_old_results() {
    let request = AtomicU64::new(0);
    let pending = Mutex::new(None::<&str>);
    let b = support::begin_explicit_scan(&request, &pending);
    *pending.lock().unwrap() = Some("B");
    let a = support::begin_explicit_scan(&request, &pending);
    assert!(
        pending.lock().unwrap().is_none(),
        "reopen A must retire B even if B already finished"
    );
    // Exercise the actual worker publication helper, including reverse completion.
    for (id, name) in [(b, "B"), (a, "A"), (b, "late B")] {
        assert_eq!(
            support::publish_latest(&request, &pending, id, name),
            id == a
        );
    }
    assert_eq!(*pending.lock().unwrap(), Some("A"));
    // Internal post-op reloads do not call begin_explicit_scan and leave a pending
    // different-folder request eligible. This distinction was the F2 regression.
    assert_eq!(request.load(Ordering::Relaxed), a);
}

#[test]
fn startup_has_one_async_open_without_filesystem_probes() {
    // Integration wiring is part of the regression: the correct request helper is
    // useless if argv still scans inline, or the same-folder branch skips it.
    let source = include_str!("main.rs");
    let boot = source
        .split("    let launch_target =")
        .nth(1)
        .unwrap()
        .split("    // v0.8.85: the FOLDER-AWARE")
        .next()
        .unwrap();
    assert!(!boot.contains("scan_folder"));
    assert!(!boot.contains("sort_shots"));
    assert!(source.contains("slint::Timer::single_shot(Duration::ZERO, move || open(path))"));
    let open = source
        .split("    let begin_reload: Rc<dyn Fn(PathBuf)>")
        .nth(1)
        .unwrap();
    let open = open
        .split("    // ── v0.8.49: the position-chip SORT menu picks")
        .next()
        .unwrap();
    assert!(!open.contains("resolve_target("));
    assert!(
        open.find("support::begin_explicit_scan").unwrap() < open.find(".spawn(move ||").unwrap()
    );
    assert!(
        open.find(".spawn(move ||").unwrap()
            < open.find("support::resolve_existing_target").unwrap()
    );
}

#[test]
fn explicit_target_resolution_rejects_missing_files_and_keeps_exact_filename() {
    let dir = std::env::temp_dir().join(format!(
        "falcon-open-target-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("photo.name.PNG");
    std::fs::write(&path, []).unwrap();
    assert_eq!(
        support::resolve_existing_target(&dir).unwrap(),
        (dir.clone(), None)
    );
    assert_eq!(
        support::resolve_existing_target(&path).unwrap(),
        (dir.clone(), Some("photo.name.PNG".into()))
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        support::resolve_existing_target(&path).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    std::fs::remove_dir(&dir).unwrap();
}

#[test]
fn scan_metadata_sorts_without_reopening_disappeared_files() {
    let dir = std::env::temp_dir().join(format!(
        "falcon-sort-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    for (name, size) in [("a.png", 32), ("b.png", 96), ("c.png", 64)] {
        let mut bytes = vec![0; size];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let scan = falcon_decode::scan_folder_with_metadata(&dir).unwrap();
    assert_eq!(scan.shots.len(), 3);
    for name in ["a.png", "b.png", "c.png"] {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    let mut shots = scan.shots;
    support::sort_scanned_shots(
        &mut shots,
        SortMethod::Size,
        true,
        &BTreeMap::new(),
        &scan.metadata,
    );
    assert_eq!(
        shots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["b", "c", "a"]
    );
    assert!(shots.iter().enumerate().all(|(i, s)| s.id == i));
    support::sort_scanned_shots(
        &mut shots,
        SortMethod::Name,
        false,
        &BTreeMap::new(),
        &scan.metadata,
    );
    assert_eq!(
        shots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
}

/// Falsifier (1.1.0-rc.1, issue #11 batch O1): keep a relative command-line path, so its parent
/// is `""` and the folder read fails, or `cur_dir` stays relative. Bin tests run in `falcon/native`.
#[test]
fn a_relative_command_line_target_is_made_absolute() {
    assert_eq!(
        support::resolve_existing_target(Path::new("Cargo.toml")).unwrap(),
        (std::env::current_dir().unwrap(), Some("Cargo.toml".into()))
    );
    assert!(support::resolve_existing_target(Path::new(".")).unwrap().0.is_absolute());
}

fn opened(name: &str, jpg: Option<&str>, raw: Option<&str>) -> Shot {
    Shot {
        id: 0,
        name: name.into(),
        has_raw: raw.is_some(),
        has_jpg: jpg.is_some(),
        raw: raw.map(|f| PathBuf::from(format!("C:/photos/{f}"))),
        jpg: jpg.map(|f| PathBuf::from(format!("C:/photos/{f}"))),
        kind: SrcKind::Jpeg,
        cloud_placeholder: false,
        sniffed: None,
    }
}

/// Falsifier (1.1.0-rc.1, issue #11 batch O2): match an explicitly opened file by the shot NAME
/// or a bare stem (a `.jfif` beside `photo.jpg`, or `photo.jpg` landing on `photo.jpg.png`), let a
/// case-folded match beat the exact one, accept an ambiguous case-folded match, count a pair's two
/// files as two shots, fold case where the file system doesn't, or show the notice for a match, a
/// folder open or an unreadable folder.
#[test]
fn an_explicit_open_matches_actual_files_and_names_an_unlisted_one() {
    let folds = cfg!(any(windows, target_os = "macos"));
    let pick = |shots: &[Shot], f: &str| support::select_match(shots, &Some(f.to_string()));
    // An unsupported sibling never borrows the listed photo's review identity.
    let listed = vec![opened("photo", Some("photo.jpg"), None)];
    assert_eq!(pick(&listed, "photo.jfif"), None);
    assert_eq!(support::unlisted_target(false, &Some("photo.jfif".into()), None), Some("photo.jfif".into()));
    // `photo.jpg.png`'s split-shot name is `photo.jpg`: never evidence for the file `photo.jpg`.
    let dotted = vec![opened("photo.jpg", Some("photo.jpg.png"), None), opened("photo", Some("photo.JPG"), None)];
    assert_eq!(pick(&dotted, "photo.jpg"), folds.then_some(1), "the case-folded actual file, never the PNG");
    // The exact filename wins over a case-folded candidate.
    let both = vec![opened("a", Some("IMG_0040.jpg"), None), opened("b", Some("IMG_0040.JPG"), None)];
    assert_eq!(pick(&both, "IMG_0040.JPG"), Some(1));
    assert_eq!(pick(&both, "img_0040.Jpg"), None, "two case-folded shots are ambiguous");
    // A RAW+JPG pair is one shot, matched through either file; both files folding is still one shot.
    let pair = vec![opened("HWU_1", Some("HWU_1.JPG"), Some("HWU_1.CR3")), opened("HWU_2", Some("HWU_2.JPG"), Some("HWU_2.CR3"))];
    assert_eq!(pick(&pair, "HWU_2.CR3"), Some(1));
    assert_eq!(pick(&pair, "hwu_2.cr3"), folds.then_some(1));
    let folded_pair = vec![opened("x", Some("X.JPG"), Some("x.Jpg"))];
    assert_eq!(pick(&folded_pair, "x.jpg"), folds.then_some(0));
    assert_eq!(pair[1].name, "HWU_2", "review identity is untouched");
    // The plan's platform split, and the old landing fallbacks.
    let shots = vec![opened("IMG_0039", Some("IMG_0039.JPG"), None), opened("IMG_0040", Some("IMG_0040.JPG"), None)];
    assert_eq!(pick(&shots, "IMG_0040.JPG"), Some(1));
    #[cfg(any(windows, target_os = "macos"))]
    assert_eq!(pick(&shots, "img_0040.jpg"), Some(1));
    #[cfg(not(any(windows, target_os = "macos")))]
    assert_eq!(pick(&shots, "img_0040.jpg"), None);
    assert_eq!(support::select_start(&shots, &Some("stranger.jpg".into())), 0);
    assert_eq!(support::select_start(&shots, &None), 0);
    // The notice: only an unmatched explicit file in a folder that was read.
    let sel = Some("stranger.jpg".to_string());
    assert_eq!(support::unlisted_target(true, &sel, None), None, "an unreadable folder shows its own empty state");
    assert_eq!(support::unlisted_target(false, &sel, Some(0)), None);
    assert_eq!(support::unlisted_target(false, &None, None), None, "a folder open names no file");
    assert_eq!(support::unlisted_target(false, &sel, None), sel);
}

/// Falsifier (1.1.0-rc.1, issue #11 batch M5): show the permission hint off the Mac, for an
/// ordinary read error, or with other wording than the owner's (27 October).
#[test]
fn a_denied_folder_gets_the_mac_permission_hint() {
    assert_eq!(
        support::folder_denied_notice(true, true),
        Some("Falcon couldn't read this folder because access was denied. Check the folder's permissions and System Settings → Privacy & Security → Files and Folders, then try again.")
    );
    assert_eq!(support::folder_denied_notice(true, false), None);
    assert_eq!(support::folder_denied_notice(false, true), None);
    assert_eq!(support::folder_denied_notice(false, false), None);
}

/// Falsifier (1.1.0-rc.1, issue #11 batch M5 + O2): the worker drops the denied kind, a scan path
/// skips the unlisted check, the hint or the notice isn't pushed, the notice shows on a partial
/// stage, or a resume offer competes with it. Pinned by source: the glue runs only in the app.
#[test]
fn every_scan_path_carries_the_denied_and_unlisted_answers() {
    let src = include_str!("main.rs");
    let main = src.split_once("\nmod tests {").map_or(src, |(p, _)| p);
    assert!(main.contains("let scan_denied = matches!(scan_res, Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied);"));
    assert!(main.contains("let unlisted_target = support::unlisted_target(scan_err, &sel, matched);"), "the final async scan");
    assert!(main.contains("(matched.unwrap_or(0), support::unlisted_target(false, &sel, matched))"), "the sync re-open");
    assert!(main.contains("support::folder_denied_notice(scan_denied, cfg!(target_os = \"macos\"))"));
    let notice = main.find("tr_format!(\"Couldn't show {name} — opened its folder instead.\"").expect("the notice");
    let (_, between) = main[..notice].rsplit_once("if !partial {").expect("a partial guard before the notice");
    assert!(!between.contains('}'), "only the final scan says it: the notice sits inside `if !partial`");
    assert!(main.contains("support::resume_offer_allowed(id != 0, land.is_some()) && new_len > 0 && unlisted_target.is_none()"));
}

/// Falsifier (1.1.0-rc.1, issue #11 batch M5): a refused listing reports another kind, so the
/// worker can't tell the Mac's permission hint apart. Skips by behaviour where the mode is ignored.
#[cfg(unix)]
#[test]
fn a_refused_folder_listing_reports_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let d = std::env::temp_dir().join(format!(
        "falcon-denied-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir(&d).unwrap();
    let _restore = Restore(d.clone());
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o300)).unwrap();
    if std::fs::read_dir(&d).is_ok() {
        eprintln!("skip: this user can list a mode-0300 folder (root or CAP_DAC_OVERRIDE)");
        return;
    }
    assert_eq!(
        falcon_decode::FolderCatalogue::read(&d, || false).err().map(|e| e.kind()),
        Some(std::io::ErrorKind::PermissionDenied)
    );
}
