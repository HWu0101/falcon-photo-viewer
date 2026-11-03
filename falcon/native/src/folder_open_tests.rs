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

/// R1 alias (1 November safety round): a generated temp folder holding `shown`, removed on drop.
/// Links are removed first, and only the link itself, never its target.
struct AliasFolder {
    root: PathBuf,
    links: Vec<PathBuf>,
}
impl Drop for AliasFolder {
    fn drop(&mut self) {
        for link in &self.links {
            let _ = std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link));
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn alias_folder(tag: &str) -> AliasFolder {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("falcon-alias-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(root.join("shown")).unwrap();
    std::fs::create_dir(root.join("other")).unwrap();
    AliasFolder { root, links: Vec::new() }
}
/// A second spelling of `dir`: a junction on Windows (no admin rights needed), a symbolic link on Unix.
fn link_to(folder: &mut AliasFolder, dir: &Path) -> PathBuf {
    let link = folder.root.join("link");
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(&link).arg(dir)
            .stdout(std::process::Stdio::null()).status().unwrap();
        assert!(made.success(), "mklink /J {}", link.display());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir, &link).unwrap();
    folder.links.push(link.clone());
    link
}

/// R1 alias (1 November safety round): the physical identity follows the folder, not its spelling.
/// FALSIFIER: an identity that reads the spelling (or the link itself), so a trailing separator, a
/// junction or link, or (Windows) a case variant looks like another folder.
#[test]
fn two_spellings_of_one_folder_share_one_physical_identity() {
    let mut folder = alias_folder("identity");
    let shown = folder.root.join("shown");
    std::fs::write(shown.join("a.png"), []).unwrap();
    let id = support::folder_identity(&shown);
    assert!(id.is_some(), "a local folder has an identity");
    let link = link_to(&mut folder, &shown);
    let upper = PathBuf::from(shown.display().to_string().to_uppercase()); // NTFS ignores case
    let spellings = [PathBuf::from(format!("{}{}", shown.display(), std::path::MAIN_SEPARATOR)), link.clone()]
        .into_iter().chain(cfg!(windows).then_some(upper)).collect::<Vec<_>>();
    for spelling in &spellings {
        assert_eq!(support::folder_identity(spelling), id, "{}", spelling.display());
    }
    // The open worker's route for a file: the identity of its resolved folder.
    let (dir, sel) = support::resolve_existing_target(&link.join("a.png")).unwrap();
    assert_eq!((support::folder_identity(&dir), sel.as_deref()), (id, Some("a.png")));
    assert_ne!(support::folder_identity(&folder.root.join("other")), id, "a different folder");
    assert_eq!(support::folder_identity(&folder.root.join("missing")), None);
}

/// R1 alias (1 November safety round): `apply_scan` records each landing; the worker's question.
/// FALSIFIER: a landing without an identity (the synchronous rescan) forgets the folder, or one
/// elsewhere keeps the old folder; an unknown identity matches.
#[test]
fn the_shown_folder_identity_follows_each_landing() {
    let folder = alias_folder("record");
    let (a, b) = (folder.root.join("shown"), folder.root.join("other"));
    let (sa, sb) = (a.display().to_string(), b.display().to_string());
    let (ia, ib) = (support::folder_identity(&a), support::folder_identity(&b));
    let mut shown = support::ShownFolder::default();
    assert_eq!(shown.alias_of(ia, support::folder_identity), None, "nothing has landed");
    shown.record(&sa, ia);
    assert_eq!(shown.alias_of(ia, support::folder_identity), Some(sa.clone()));
    shown.record(&sa, None);
    assert_eq!(shown.alias_of(ia, support::folder_identity), Some(sa.clone()), "a same-folder rescan keeps it");
    assert_eq!(shown.alias_of(ib, support::folder_identity), None, "another folder");
    assert_eq!(shown.alias_of(None, support::folder_identity), None, "an unknown identity matches nothing");
    shown.record(&sb, None);
    assert_eq!(shown.alias_of(ia, support::folder_identity), None, "a landing elsewhere forgets A");
    shown.record(&sb, ib);
    assert_eq!(shown.alias_of(ib, support::folder_identity), Some(sb));
}

/// R1 alias (1 November safety round): an explicit open of the folder on screen under another
/// spelling rescans it AS SHOWN, through the real pieces: the open worker (resolve, identity, alias),
/// `begin_resolved`'s test and the folder and file it hands the rescan (`reopen_scan`, R3), then the
/// scan domain's same-folder test that `apply_scan` asks (so the live edits are carried).
/// FALSIFIER: hand the rescan the requested spelling (the 1.1.0 `reload(path, None)`), ignore the
/// worker's alias answer, drop the `Path` fallback, or skip the probe of the on-screen spelling.
#[test]
fn an_open_of_the_shown_folder_by_another_spelling_rescans_it_as_shown() {
    let mut folder = alias_folder("route");
    let shown_dir = folder.root.join("shown");
    std::fs::write(shown_dir.join("a.png"), []).unwrap();
    let shown = shown_dir.display().to_string();
    let mut on_screen = support::ShownFolder::default();
    on_screen.record(&shown, support::folder_identity(&shown_dir)); // apply_scan's tail
    let scanned = support::LastScannedDir::at_boot(&shown, true);
    let route = |request: &Path, has_photos: bool| {
        let (dir, sel) = support::resolve_existing_target(request).unwrap();
        let alias = on_screen.alias_of(support::folder_identity(&dir), support::folder_identity);
        support::reopen_on_screen(has_photos, &shown, &dir, alias.as_deref(), sel.as_deref())
    };
    let trailing = format!("{shown}{}", std::path::MAIN_SEPARATOR);
    let link = link_to(&mut folder, &shown_dir);
    let requests = [
        (shown_dir.clone(), None),
        (PathBuf::from(&trailing), None),
        (link.clone(), None),
        (link.join("a.png"), Some("a.png")),
    ];
    let upper = (PathBuf::from(shown.to_uppercase()), None); // Windows only: NTFS ignores case
    for (request, sel) in requests.into_iter().chain(cfg!(windows).then_some(upper)) {
        let (dir, rescan_sel) = route(&request, true).unwrap_or_else(|| panic!("{} is the folder on screen", request.display()));
        assert!(!scanned.opens_new_folder(&dir.display().to_string()), "{}: rescanned as {}", request.display(), dir.display());
        assert_eq!(rescan_sel.as_deref(), sel, "{}", request.display());
    }
    // The `Path` fallback alone (no identity): a trailing separator is the same `Path` but another
    // string, and it too rescans as shown.
    assert!(Path::new(&trailing) == shown_dir && trailing != shown);
    assert_eq!(support::reopen_on_screen(true, &shown, Path::new(&trailing), None, None), Some((shown_dir.clone(), None)));
    // A plain launch seeds `cur_dir` without loading it: the first open stays a fresh (resume) open.
    assert_eq!(route(&shown_dir, false), None);
    assert_eq!(route(&link, false), None);
    assert_eq!(route(&folder.root.join("other"), true), None, "a different folder");
    // A rename keeps the identity, but the on-screen spelling no longer names the folder: the new
    // name opens as a different folder; the old spelling is never rescanned (that would read its parent).
    std::fs::remove_dir(&link).or_else(|_| std::fs::remove_file(&link)).unwrap();
    folder.links.clear();
    let renamed = folder.root.join("renamed");
    std::fs::rename(&shown_dir, &renamed).unwrap();
    assert_eq!(route(&renamed, true), None);
}

/// R1 alias (1 November safety round), the wiring the tests above can't see: the open worker
/// reads both answers, `begin_resolved` rescans the on-screen spelling and does no file-system work,
/// every async scan carries the identity, and `apply_scan` records it beside `scanned_dir`.
/// FALSIFIER: rescan the requested path, a worker that never asks, a scan that drops the identity,
/// or no record at acceptance.
#[test]
fn explicit_opens_compare_folder_identities_off_the_interface_thread() {
    let source = include_str!("main.rs").lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let section = |from: &str, to: &str| source.split_once(from).unwrap().1.split_once(to).unwrap().0.to_owned();
    let open = section("    let begin_reload: Rc<dyn Fn(PathBuf)> = {", "\n    };");
    let spawn = open.find(".spawn(move ||").unwrap();
    assert!(open.find("let shown = shown_folder.borrow().clone();").unwrap() < spawn, "snapshotted before the worker starts");
    for wired in [
        "let identity = target.as_ref().ok().and_then(|(dir, _)| support::folder_identity(dir));",
        "let shown_alias = shown.alias_of(identity, support::folder_identity);",
        "support::ResolvedOpen { id, path, requested_at, target, identity, shown_alias }",
    ] {
        assert!(open[spawn..].contains(wired), "the open worker: {wired}");
    }
    let resolved = section("    let begin_resolved: Rc<dyn Fn(support::ResolvedOpen)> = {", "\n    };");
    assert!(resolved.contains("let again = support::reopen_on_screen(\n                len_atomic.load(Ordering::Relaxed) > 0, &cur_dir.borrow(), &dir, shown_alias.as_deref(), sel.as_deref());"));
    // R3: the reopen hands its worker the on-screen folder from `reopen_on_screen`, never the request.
    assert!(resolved.contains("if let Some((dir, sel)) = again {") && !resolved.contains("reload(path, None)"));
    assert!(!resolved.contains("folder_identity"), "no file-system identity read on the interface thread");
    assert_eq!(resolved.matches(" had_file, from_ledger, identity, manifest").count(), 3, "first photo, nearby, final");
    let reload = section("    let reload: Rc<dyn Fn(PathBuf, Option<usize>)> = {", "\n    };");
    assert!(reload.contains(" had_file, from_ledger, identity: None, manifest,"), "the synchronous rescan reads none");
    assert!(source.contains("*scanned_dir.borrow_mut() = dir.display().to_string(); // v0.8.121 (F20): same rule, same line\n            shown_folder.borrow_mut().record(&dir.display().to_string(), identity);"));
}

/// R3 (1 November safety round): an explicit reopen of the folder on screen (a drop, either picker,
/// the Mac's Open Recent or a Finder open) scans on a worker. With its reader held, the interface
/// (this thread) still runs: a rating and a rotation changed meanwhile survive acceptance, in the
/// Rating order the synchronous rescan would give, landing on the requested photo. A folder that
/// can't be read lands nothing, so the list on screen stays.
/// FALSIFIER: no Rating re-sort or requested landing at acceptance (`settle_reopen`), the reopen
/// sorts by the file's sort instead of the live one, or its landing isn't accepted as id 0.
#[test]
fn a_reopen_scans_off_the_interface_thread_and_keeps_edits_made_meanwhile() {
    let folder = alias_folder("reopen");
    let dir = folder.root.join("shown");
    for name in ["A.png", "B.png", "C.png"] {
        std::fs::write(dir.join(name), []).unwrap();
    }
    // The review file as last written: A 5★, B 4★, C 1★, sorted by name.
    std::fs::write(dir.join(support::SELECTION_FILE),
        r#"{"photos":{"A":{"rating":5},"B":{"rating":4},"C":{"rating":1}},"sort":"name.asc"}"#).unwrap();
    // On screen, newer than the file: Rating, highest first (a sort pick), and A lowered to 2★.
    let shown = dir.display().to_string();
    let names = |shots: &[Shot]| shots.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
    let star = |rating| Sel { rating, ..Sel::default() };
    let live_map = BTreeMap::from([("A".to_string(), star(2)), ("B".to_string(), star(4)), ("C".to_string(), star(1))]);
    let mut old = falcon_decode::scan_folder_with_metadata(&dir).unwrap().shots;
    sort_scanned_shots(&mut old, SortMethod::Rating, true, &live_map, &BTreeMap::new());
    assert_eq!(names(&old), ["B", "A", "C"]);
    let (mut ratings, marks) = apply_selection(&live_map, &old);
    let rot = RotState::new(true);
    rot.load_rotations(&BTreeMap::new(), &old);
    // Drop A.png: begin_resolved's route, then the worker with its reader held.
    let (rescan_dir, sel) = support::reopen_on_screen(true, &shown, &dir, None, Some("A.png")).expect("the folder on screen");
    let id = 7;
    let mut pending = support::PendingReopen::default();
    pending.start(id, sel.clone());
    let live_sort = Some(support::encode_sort_spec(SortMethod::Rating, true));
    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel::<()>();
    let worker = std::thread::spawn(move || {
        let warn = Mutex::new(None);
        support::reopen_scan(id, rescan_dir, sel, live_sort, &[], 10, None, &warn, |dir: &Path| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            falcon_decode::scan_folder_with_metadata(dir)
        })
    });
    entered_rx.recv().unwrap(); // the reader has started and is held
    // The interface still runs: C rated 5★, B turned a quarter clockwise.
    let at = |name: &str| old.iter().position(|s| s.name == name).unwrap();
    ratings[at("C")] = 5;
    rot.rotate("B", at("B"), 1);
    assert!(!worker.is_finished(), "the scan is still waiting on its reader");
    release_tx.send(()).unwrap();
    let mut done = worker.join().unwrap().expect("the folder reads");
    // What the worker read: the file's stars, sorted by the LIVE sort, landing on A.
    assert_eq!(done.sort, (SortMethod::Rating, true), "the live sort, not the file's name sort");
    assert_eq!((names(&done.shots), done.start, done.id), (vec!["A".to_string(), "B".into(), "C".into()], 0, id));
    // Acceptance, as `apply_scan` runs it: a same-folder landing that is this reopen's own.
    assert!(!support::LastScannedDir::at_boot(&shown, true).opens_new_folder(&done.dir.display().to_string()));
    assert_eq!(pending.take_landing(id + 1), None, "another request's landing");
    let requested = pending.take_landing(id).expect("the reopen's own landing");
    support::carry_live_review_on_rescan(&mut done, &old, &ratings, &marks, &BTreeMap::new(), rot.rotations_snapshot());
    photo_open::settle_reopen(&mut done, &requested);
    assert_eq!(names(&done.shots), ["C", "B", "A"], "sorted by the live stars");
    assert_eq!(done.ratings, [5, 4, 2]);
    assert_eq!(done.rotations, BTreeMap::from([("B".to_string(), 1)]));
    assert_eq!(done.shots[done.start].name, "A", "lands on the requested photo");
    assert_eq!(done.id, 0, "accepted as the synchronous rescan it replaces");
    assert_eq!(pending.take_landing(id), None, "taken once");
    // The order the synchronous rescan gives: the scan's own order sorted by the live map.
    let mut sync = falcon_decode::scan_folder_with_metadata(&dir).unwrap().shots;
    sort_scanned_shots(&mut sync, SortMethod::Rating, true, &selection_map(&old, &ratings, &marks), &BTreeMap::new());
    assert_eq!(names(&done.shots), names(&sync));
    // A folder that can't be read lands nothing: the list on screen stays.
    let warn = Mutex::new(None);
    let unread = support::reopen_scan(8, dir.clone(), None, None, &[], 10, None, &warn, |_: &Path| Err(std::io::Error::other("offline")));
    assert!(unread.is_err());
}

/// R3 (1 November safety round): the rescan after a file operation or a sort pick reads the folder
/// after that change, so a reopen still scanning (an older listing) is retired, even if its result
/// is already waiting. A pending open of another folder is left alone.
/// FALSIFIER: `reload` never retires the reopen, or retires whatever request is pending.
#[test]
fn a_file_operation_rescan_retires_a_reopen_still_scanning() {
    let request = AtomicU64::new(0);
    let slot = Mutex::new(None::<&str>);
    let mut reopen = support::PendingReopen::default();
    let id = support::begin_explicit_scan(&request, &slot);
    reopen.start(id, Some("A.png".into()));
    assert!(support::publish_latest(&request, &slot, id, "older listing"));
    // `reload`'s first step:
    assert!(reopen.superseded_by_rescan(request.load(Ordering::Relaxed)));
    support::begin_explicit_scan(&request, &slot);
    assert!(slot.lock().unwrap().is_none(), "the older listing never lands");
    assert!(!support::publish_latest(&request, &slot, id, "late"), "nor a late one");
    assert_eq!(reopen.take_landing(id), None);
    // A newer open of another folder is pending: it stays, and the reopen record is forgotten.
    let id = support::begin_explicit_scan(&request, &slot);
    reopen.start(id, None);
    let other = support::begin_explicit_scan(&request, &slot);
    assert!(!reopen.superseded_by_rescan(request.load(Ordering::Relaxed)));
    assert!(support::publish_latest(&request, &slot, other, "B"));
    assert_eq!(reopen.take_landing(id), None);
    assert!(!support::PendingReopen::default().superseded_by_rescan(request.load(Ordering::Relaxed)), "nothing pending");
}

/// R3 (1 November safety round), the wiring the tests above can't see: `begin_resolved`'s reopen
/// branch records the reopen and scans on a worker (never `reload`, no "Scanning…"), sorted by the
/// live attr; `apply_scan` takes the landing and settles it right after the live carry, before
/// promotion; `reload` retires a reopen still scanning before it flushes and rescans.
/// FALSIFIER: the reopen calls `reload` again (the 1.1.0 freeze), shows the scanning overlay, never
/// records itself, sorts by the file's attr, or acceptance/retirement is unwired.
#[test]
fn explicit_reopens_scan_on_a_worker_and_settle_at_acceptance() {
    let source = include_str!("main.rs").lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let section = |from: &str, to: &str| source.split_once(from).unwrap().1.split_once(to).unwrap().0.to_owned();
    let resolved = section("    let begin_resolved: Rc<dyn Fn(support::ResolvedOpen)> = {", "\n    };");
    let branch = resolved.split_once("if let Some((dir, sel)) = again {").expect("the reopen branch").1
        .split_once("\n                return;\n").expect("its return").0.to_owned();
    let spawn = branch.find(".spawn(move ||").expect("a worker");
    assert!(!branch.contains("reload(") && !branch.contains("set_scanning(true)") && !branch[..spawn].contains("scan_folder"),
        "no rescan or overlay on the interface thread");
    assert!(branch.find("reopen_pending.borrow_mut().start(id, sel.clone());").unwrap() < spawn, "recorded before the worker starts");
    assert!(branch.contains("let live_sort = folder_attrs.borrow().sort.clone();"));
    for wired in [
        "let read = |dir: &Path| falcon_decode::scan_folder_with_metadata_cancellable(dir, cancelled);",
        "support::reopen_scan(id, dir.clone(), sel, live_sort, &sort_lru_snap, sort_cap, date_read, &local_warn, read)",
        "if support::publish_latest(&scan_id, &scan_done, id, done) {",
    ] {
        assert!(branch[spawn..].contains(wired), "the reopen worker: {wired}");
    }
    let apply = section("    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
    let take = apply.find("let reopen = reopen_pending.borrow_mut().take_landing(done.id);").expect("the landing is taken");
    let carry = apply.find("support::carry_live_review_on_rescan(").unwrap();
    let settle = apply.find("photo_open::settle_reopen(&mut done, requested);").expect("settled");
    assert!(take < carry && carry < settle && settle < apply.find("photo_open::Promotion::take(").unwrap());
    // Open-time check (2 November): the live list and the incoming list take what readers learned
    // from one snapshot, before anything compares them; and the photo on screen is corrected in the
    // tick before that tick's own snapshot of the list.
    // FALSIFIER: the block moves below the carry or the take, or the tick step is removed or moved
    // below `let (shots_now, gen_now) = {`.
    let learned = apply.find("falcon_decode::BytesLearned::snapshot()").expect("the overlay");
    assert!(apply.find("let mut done = done;").unwrap() < learned && learned < carry, "before the carry reads `old`");
    assert!(learned < apply.find("photo_open::Promotion::take(").unwrap(), "and before promotion compares");
    assert!(apply[learned..].contains("tick::correct_list_from(&learned, &shots);"), "the live list");
    assert!(apply[learned..].contains("learned.corrected(s)"), "the incoming list");
    let snapshot = source.find("let (shots_now, gen_now) = {").expect("the per-tick snapshot");
    let on_screen = source.find("tick::step_checked_kind(*cur_t.borrow(), &shots_t, &thumb_gen_t);").expect("the tick step");
    assert!(on_screen < snapshot, "the label is corrected before this tick reads the list");
    let compare_halves = &source[on_screen..snapshot];
    for half in ["tick::step_checked_kind(cmp_a_idx_t.get(), &shots_t, &thumb_gen_t);", "tick::step_checked_kind(cmp_b_idx_t.get(), &shots_t, &thumb_gen_t);"] {
        assert!(compare_halves.contains(half), "both Compare halves are looked at: {half}");
    }
    let reload = section("    let reload: Rc<dyn Fn(PathBuf, Option<usize>)> = {", "\n    };");
    let retire = reload.find("if reopen_pending.borrow_mut().superseded_by_rescan(scan_id.load(Ordering::Relaxed)) {\n                support::begin_explicit_scan(&scan_id, &scan_done);\n            }")
        .expect("a rescan retires a reopen still scanning");
    assert!(retire < reload.find("flush_old();").unwrap());
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

/// Date taken sort (L54): the landing file is worked out when the reread runs, from the live list. A
/// person who browsed from P3 to P9 during the read stays on P9 in the new order, and `keep_view`'s
/// same-photo check then keeps their zoom and Compare. Named by photo, never by position (L52).
/// FALSIFIER: restore `land_file` captured at the pick in `apply_pick`, or post it with the request.
#[test]
fn the_landing_is_the_photo_on_screen_now() {
    let names = ["P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9"];
    let file = |n: &str| format!("{n}.JPG");
    let mut shots: Vec<Shot> = names.iter().map(|n| opened(n, Some(&file(n)), None)).collect();
    let (at_the_pick, on_screen_now) = (2, 8);
    assert_eq!(shots[at_the_pick].name, "P3");
    let landing = support::landing_file(&shots, on_screen_now);
    assert_eq!(landing.as_deref(), Some("P9.JPG"));
    shots.reverse();
    let landed = support::select_match(&shots, &landing).expect("still listed");
    assert_eq!(shots[landed].name, "P9");
    assert_eq!(support::landing_file(&shots, 99), None, "no photo there");
    // A RAW+JPEG pair lands by its primary file, a pathless shot by its name.
    assert_eq!(support::landing_file(&[opened("HWU_1", Some("HWU_1.JPG"), Some("HWU_1.CR3"))], 0).as_deref(), Some("HWU_1.JPG"));
    assert_eq!(support::landing_file(&[opened("HWU_2", None, Some("HWU_2.CR3"))], 0).as_deref(), Some("HWU_2.CR3"));
    assert_eq!(support::landing_file(&[opened("HWU_3", None, None)], 0).as_deref(), Some("HWU_3"));
    // The wiring: the pick posts only the folder, and the drain asks the live list.
    let source = include_str!("main.rs").lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let section = |from: &str, to: &str| source.split_once(from).unwrap().1.split_once(to).unwrap().0.to_owned();
    let pick = section("let apply_pick = Rc::new(move |method: SortMethod, desc: bool| {", "\n        });");
    assert!(!pick.contains("land_file") && !pick.contains("landing_file(") && !pick.contains("file_name()"), "the pick captures no landing");
    assert!(source.contains("let sort_reload_req = Arc::new(Mutex::new(None::<PathBuf>));"), "the request carries only the folder");
    let drain = section("if let Some(p) = sort_reload_t.try_lock().ok().and_then(|mut g| g.take()) {", "mk!(\"reload_req\");");
    let order = ["let snap = shots_t.lock()", "support::landing_file(&snap, *cur_t.borrow())", "keep_view_req_t.set(true);", "(&*reload_t)(path, None);", "keep_view_req_t.set(false);"];
    let spots: Vec<usize> = order.iter().map(|s| drain.find(s).unwrap_or_else(|| panic!("the drain: {s}"))).collect();
    assert!(spots.windows(2).all(|w| w[0] < w[1]), "the drain's order: {spots:?}");
}

/// Keeping the view on a sort change: `view.reset_for_swap` makes the next tick's `step_bookkeeping`
/// clear the tiles, so a kept view's tiles come back right after that clear, and `apply_scan` restores
/// the view last, after every reset, reading it before the swap. Pinned by source: the real-window
/// check is the main session's.
/// FALSIFIER: move the append above `step_bookkeeping`, remove it, or restore before the resets.
#[test]
fn kept_tiles_return_after_the_clear() {
    let source = include_str!("main.rs").lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let book = source.find("let c = step_bookkeeping(").expect("the one bookkeeping call");
    let append = source.find("roi_t.tiles.borrow_mut().append(&mut roi_t.kept_tiles.borrow_mut());").expect("the tiles return");
    let publish = source.find("prep_priority::PREP.publish(support::ExplicitSet::new(c, compare_t.get(), cmp_a_idx_t.get(),").expect("the publish");
    assert!(book < append && append < publish, "right after the clear, before any step asks for tiles");
    let apply = source.split_once("    let apply_scan: Rc<dyn Fn(ScanDone)> = {").unwrap().1.split_once("\n    };").unwrap().0;
    let order = [
        "let kept_view = if keep_view_req.take() {",
        "*listing_stamps.borrow_mut() = new_stamps;",
        "*s = new_arc.clone();",
        "reset_transient_ui(&a, &del_pending, id != 0);",
        "carry.restore(",
        "kept.restore(&a, &view.compare, (&cmp_a_idx, &cmp_b_idx), &roi);",
    ];
    let spots: Vec<usize> = order.iter().map(|s| apply.find(s).unwrap_or_else(|| panic!("apply_scan: {s}"))).collect();
    assert!(spots.windows(2).all(|w| w[0] < w[1]), "read before the swap, restored after every reset: {spots:?}");
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

/// Keeping prepared work (2 November), the glue the rows on `RescanCarry` can't see, because it only
/// runs in the app: taken after the list is corrected and before the swap, put back after every reset
/// (a refused carry is then exactly today's wipe), a memory replaced at every landing, every full
/// listing passing its stamps, and neither partial stage. The Mac menu's Undo row asks `on_undo`'s
/// question; that block only compiles on a Mac, so the source is the only thing Windows can check.
///
/// FALSIFIER (L28): move take after the swap; restore before the Undo wipe (Undo silently lost); a
/// full-scan builder passes `None` (the gain silently lost); a partial builder passes stamps; the
/// memory is written only when carrying; the cost gate or an unconditional line changes.
#[test]
fn the_rescan_carry_is_wired_around_the_list_swap() {
    let code = |src: &str| src.split_once("\nmod tests {").map_or(src, |(p, _)| p).replace("\r\n", "\n")
        .lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let main = code(include_str!("main.rs"));
    let at = main.find("    let apply_scan: Rc<dyn Fn(ScanDone)> = {").expect("apply_scan");
    let apply = &main[at..at + main[at..].find("\n    };").expect("its end")];
    let order = [
        "falcon_decode::BytesLearned::snapshot()",
        "photo_open::Promotion::take(",
        "let new_stamps = done.stamps.take();",
        "photo_open::rescan_carries(done.id, reopen.is_some(), scan_same_folder, applying_at_swap.load(Ordering::SeqCst))",
        "photo_open::RescanCarry::take((&old, &done.shots), (previous, listed), done.start,",
        "*listing_stamps.borrow_mut() = new_stamps;",
        "*s = new_arc.clone();",
        "drop_developed_caches(&fast.cache, &detail, &roi, &l2);",
        "if !promoted && rescan_carry.is_none() { support::reset_fast_cost(); }",
        "*undo_stack.borrow_mut() = UndoRedo::default();",
        "promotion.restore(",
        "carry.restore((&fast, &detail, &film, &roi), &l2, &rot, &undo_stack, (&meta, generation.load(Ordering::Relaxed)));",
    ];
    let spots: Vec<usize> = order.iter().map(|s| apply.find(s).unwrap_or_else(|| panic!("apply_scan: {s}"))).collect();
    assert!(spots.windows(2).all(|w| w[0] < w[1]), "apply_scan's order: {spots:?}");
    // At the closure's own indent: nothing makes these conditional.
    for line in ["*listing_stamps.borrow_mut() = new_stamps;", "support::reset_fast_latency(generation.load(Ordering::Relaxed));",
        "hwheic::note_folder_swap();", "film.arm_open(id, new_len);", "*undo_stack.borrow_mut() = UndoRedo::default();"] {
        assert!(apply.contains(&format!("\n            {line}\n")), "unconditional: {line}");
    }
    assert_eq!(apply.matches("*listing_stamps.borrow_mut()").count(), 1, "one writer, the landing");
    assert!(apply.contains("let ScanDone { changed_sources: _, stamps: _, partial,"), "the destructure names the field");
    assert!(main.contains("let listing_stamps = Rc::new(RefCell::new(None::<support::ListingStamps>));"), "one cell, with one writer above");
    // Every builder decides.
    let builders: Vec<&str> = main.match_indices("ScanDone {").map(|(at, _)| &main[at..])
        .filter(|s| !s.starts_with("ScanDone { changed_sources: _")).collect();
    assert_eq!(builders.len(), 4, "the sync reload, the first photo, the nearby batch, the final scan");
    let stamps_of = |s: &str| {
        let body = &s[..s.find('}').expect("closed")];
        body.split_once("stamps: ").expect("a builder names its stamps").1.split_once(", partial").expect("beside `partial`").0.to_owned()
    };
    assert_eq!(stamps_of(builders[0]), "Some(stamps)", "the sync reload");
    assert_eq!(stamps_of(builders[1]), "None", "the first photo");
    assert_eq!(stamps_of(builders[2]), "None", "the nearby batch");
    assert_eq!(stamps_of(builders[3]), "(!scan_err).then(|| support::listing_stamps(metadata))", "the final scan");
    assert!(main.contains("sort_scanned_shots(&mut new_vec, method, desc, &sel_map, &scan.metadata);"), "the reload's sort reads it");
    assert!(main.contains("let stamps = support::listing_stamps(scan.metadata);"), "and then the reload keeps it");
    let support = code(include_str!("support.rs"));
    assert!(support.contains("stamps: Some(listing_stamps(scan.metadata)), partial: false,"), "a reopen's listing is the baseline too");
    // The Mac Edit menu.
    assert!(main.contains("mm::undo_kind_for(\n                        support::live_undo_arm(\n                            u.top_seq(),"), "the Mac row asks the same question");
    assert!(main.contains("undo: undo_kind,"), "and publishes the answer");
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
