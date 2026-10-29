//! Worker-level RAW export contracts. Real encoders/files/manifest, with an injectable pixel source
//! for deterministic cancellation and depth/orientation checks independent of camera fixtures.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use crate::raw_export::*;
use crate::support::{self, WebCollide, WebRun};
use falcon_decode::{Keep, Pixels, Shot, WebFormat};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "falcon-raw-export-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn jpg(&self, name: &str) {
        let bytes = falcon_decode::encode_jpeg_rgb(&[80; 6 * 4 * 3], 6, 4, 90).unwrap();
        std::fs::write(self.0.join(name), bytes).unwrap();
    }
    fn raw(&self, name: &str) {
        std::fs::write(self.0.join(name), b"opaque synthetic RAW input").unwrap();
    }
    fn shots(&self) -> Vec<Shot> {
        let mut shots = falcon_decode::scan_folder(&self.0).unwrap();
        shots.sort_by(|a, b| a.name.cmp(&b.name));
        shots
    }
    fn output(&self, name: &str) -> PathBuf {
        self.0.join("export").join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn recipe<'a>(
    dir: &'a Path,
    shots: &'a [Shot],
    marks: &'a [u8],
    deltas: &'a HashMap<String, u8>,
    policy: RawExportPolicy,
) -> WebRun<'a> {
    WebRun {
        dir,
        shots,
        marks,
        filt: 0,
        want: 1,
        long: 2048,
        quality: 90,
        wm: None,
        auto_orient: true,
        deltas,
        suffix: None,
        fmt: WebFormat::Png,
        raw_policy: policy,
        collide: WebCollide::Overwrite,
        cloud_ok: HashSet::new(),
        is_cloud: |_| false,
    }
}

fn synthetic_pixels(keep: Keep) -> WebDecodedPixels {
    Ok((
        if keep.depth {
            Pixels::Rgb16(vec![12345; 6 * 4 * 3])
        } else {
            Pixels::Rgb8(vec![48; 6 * 4 * 3])
        },
        6,
        4,
    ))
}

fn no_parts(fixture: &Fixture) {
    assert!(std::fs::read_dir(fixture.0.join("export")).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".part")));
}

/// 2026-10-28 (the cloud-only round, owner answer 11) — **A FILE MADE ONLINE-ONLY AFTER THE ANSWER IS
/// SKIPPED, NOT READ, AND THE RESULT SAYS SO.** The user agreed to download B (it was counted when
/// Export was pressed); A became cloud-only after that; C is on this device. The run reads B and C,
/// never A, and its sentence ends with the owner's clause. The probe stands in for placeholders.
/// R2 (Codex's review, 29 October): a RAW the run develops is never agreed (RAW export never
/// downloads); one freed up during the run is the same skip and the same clause, never handed to the
/// developer (whose own refusal would report a failed export).
///
/// FALSIFIER (L28): drop the run's last-look check and A is decoded; drop the `cloud_ok` term and B
/// is skipped too; leave the developed RAW out of the last look and D is handed to the developer.
#[test]
fn a_file_freed_up_after_the_answer_is_skipped_and_reported() {
    let f = Fixture::new();
    f.jpg("A_CLOUD.JPG");
    f.jpg("B_CLOUD.JPG");
    f.jpg("C.JPG");
    let shots = f.shots();
    assert_eq!(shots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["A_CLOUD", "B_CLOUD", "C"]);
    let marks = vec![1; shots.len()];
    let deltas = HashMap::new();
    let mut run = recipe(&f.0, &shots, &marks, &deltas, RawExportPolicy::Skip);
    run.is_cloud = |p| p.to_string_lossy().contains("_CLOUD");
    run.cloud_ok = [1usize].into_iter().collect();
    let seen = RefCell::new(Vec::new());
    let progress = ExportProgress::default();
    let (message, failed) = support::export_web_run_with_decoder(run, &progress, |shot, keep, source, progress| {
        seen.borrow_mut().push(shot.name.clone());
        decode_web_pixels(shot, keep, source, progress)
    });
    assert!(!failed, "{message}");
    assert_eq!(*seen.borrow(), ["B_CLOUD", "C"], "the agreed file and the local one are read; the freed one is not");
    assert!(!f.output("A_CLOUD.png").exists());
    assert!(message.ends_with(" · 1 file was skipped because it is only in the cloud now."), "{message}");

    // R2: RAW export (developed RAWs). D is freed up during the run; E is on this device.
    let g = Fixture::new();
    g.raw("D_CLOUD.CR3");
    g.raw("E.CR3");
    let shots = g.shots();
    assert_eq!(shots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["D_CLOUD", "E"]);
    let marks = vec![1; shots.len()];
    let mut run = recipe(&g.0, &shots, &marks, &deltas, RawExportPolicy::Develop);
    run.is_cloud = |p| p.to_string_lossy().contains("_CLOUD");
    let seen = RefCell::new(Vec::new());
    let progress = ExportProgress::default();
    let (message, failed) = support::export_web_run_with_decoder(run, &progress, |shot, keep, source, _| {
        assert_eq!(source, WebPixelSource::DevelopedRaw);
        seen.borrow_mut().push(shot.name.clone());
        synthetic_pixels(keep)
    });
    assert!(!failed, "a freed-up RAW is a skip, not a failed export: {message}");
    assert_eq!(*seen.borrow(), ["E"], "the freed RAW never reaches the developer");
    assert!(!g.output("D_CLOUD.png").exists() && g.output("E.png").exists());
    assert!(message.ends_with(" · 1 file was skipped because it is only in the cloud now."), "{message}");
}

#[test]
fn raw_export_modes_share_admission_and_keep_finished_pairs_byte_identical() {
    let f = Fixture::new();
    f.jpg("FIN.JPG");
    f.jpg("PAIR.JPG");
    f.raw("PAIR.CR3");
    f.raw("RAW.CR3");
    f.raw("PASS.CR3");
    // Unsupported sibling: this is a RAW picture with a passenger, not a finished pair.
    let avif = b"\0\0\0\x18ftypavif\0\0\0\0mif1miaf";
    std::fs::write(f.0.join("PASS.JPG"), avif).unwrap();
    std::fs::write(f.0.join("BAD.JPG"), avif).unwrap();
    let shots = f.shots();
    assert_eq!(shots.len(), 5);
    let marks = vec![1; shots.len()];
    let deltas = HashMap::new();
    let mut finished = HashMap::new();
    for (policy, expected) in [
        (RawExportPolicy::Skip, 2),
        (RawExportPolicy::CameraPreview, 4),
        (RawExportPolicy::Develop, 4),
    ] {
        let seen = RefCell::new(Vec::new());
        let progress = ExportProgress::default();
        let (_, counted) =
            support::web_collision_scan(&f.0, &shots, &marks, 1, None, WebFormat::Png, policy);
        assert_eq!(counted, expected);
        let (message, failed) = support::export_web_run_with_decoder(
            recipe(&f.0, &shots, &marks, &deltas, policy),
            &progress,
            |shot, keep, source, progress| {
                seen.borrow_mut().push((shot.name.clone(), source));
                if source == WebPixelSource::Finished {
                    decode_web_pixels(shot, keep, source, progress)
                } else {
                    synthetic_pixels(keep)
                }
            },
        );
        assert!(!failed, "{message}");
        assert_eq!(seen.borrow().len(), counted);
        assert_eq!(
            progress.done.load(Ordering::Relaxed),
            shots.len(),
            "skips count only after processing"
        );
        for name in ["FIN.png", "PAIR.png"] {
            let bytes = std::fs::read(f.output(name)).unwrap();
            if policy == RawExportPolicy::Skip {
                finished.insert(name, bytes);
            } else {
                assert_eq!(
                    bytes, finished[name],
                    "RAW policy must not change a finished pair"
                );
            }
        }
        assert!(seen
            .borrow()
            .iter()
            .filter(|(name, _)| name == "FIN" || name == "PAIR")
            .all(|(_, source)| *source == WebPixelSource::Finished));
        match policy {
            RawExportPolicy::CameraPreview => assert!(
                message.contains("2 from camera previews")
                    && !message.contains("developed from RAW")
            ),
            RawExportPolicy::Develop => assert!(
                message.contains("2 developed from RAW")
                    && !message.contains("from camera previews")
            ),
            RawExportPolicy::Skip => assert!(message.contains("2 RAW-only skipped")),
        }
    }
    for index in [-1, 0, 3, i32::MAX] {
        assert_eq!(RawExportPolicy::from_index(index), RawExportPolicy::Skip);
    }
    assert_eq!(
        RawExportPolicy::from_index(1),
        RawExportPolicy::CameraPreview
    );
    assert_eq!(RawExportPolicy::from_index(2), RawExportPolicy::Develop);
}

#[test]
fn raw_export_developed_png_keeps_16bit_srgb_and_composes_raw_rotation() {
    let f = Fixture::new();
    f.raw("RAW.CR3");
    let sidecar = f.0.join("RAW.xmp");
    let original = b"<rdf:Description tiff:Orientation=\"6\"/>";
    std::fs::write(&sidecar, original).unwrap();
    let shots = f.shots();
    let deltas = HashMap::from([("RAW".to_owned(), 1)]);
    let input = Pixels::Rgb16((0..18).map(|i| 1000 + i * 1234).collect());
    let expected = falcon_decode::rotate_pixels(input.clone(), 2, 3, 2);
    let progress = ExportProgress::default();
    let (message, failed) = support::export_web_run_with_decoder(
        recipe(&f.0, &shots, &[1], &deltas, RawExportPolicy::Develop),
        &progress,
        |_, keep, source, progress| {
            assert!(keep.depth);
            assert_eq!(source, WebPixelSource::DevelopedRaw);
            assert_eq!(progress.status().phase, "Developing RAW…");
            assert_eq!(progress.done.load(Ordering::Relaxed), 0);
            Ok((input.clone(), 2, 3))
        },
    );
    assert!(!failed, "{message}");
    let exported = falcon_decode::scan_folder(&f.0.join("export")).unwrap();
    let actual = falcon_decode::decode_full_pixels(&exported[0], Keep::ALL).unwrap();
    assert_eq!(
        actual, expected,
        "16-bit samples and composed RAW orientation survive without a second gamut transform"
    );
    assert_eq!(
        falcon_decode::shot_source_gamut(&exported[0]),
        falcon_color::Gamut::Srgb
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), original);
    assert_eq!(
        std::fs::read(f.0.join("RAW.CR3")).unwrap(),
        b"opaque synthetic RAW input"
    );
    assert_eq!(progress.done.load(Ordering::Relaxed), 1);
}

#[test]
fn raw_export_cancel_after_develop_keeps_completed_files_and_prior_manifest() {
    let f = Fixture::new();
    f.jpg("A_DONE.JPG");
    f.raw("B_RAW.CR3");
    std::fs::create_dir(f.0.join("export")).unwrap();
    std::fs::write(f.output("B_RAW.png"), b"user edited surviving prior output").unwrap();
    let prior = support::stat_rec(&f.output("B_RAW.png")).unwrap();
    support::write_export_manifest(
        &f.0.join("export"),
        &[(
            "B_RAW.png".into(),
            support::ManifestOutcome::Ok,
            Some(prior),
        )],
    );
    let shots = f.shots();
    let progress = ExportProgress::default();
    let (message, failed) = support::export_web_run_with_decoder(
        recipe(
            &f.0,
            &shots,
            &[1, 1],
            &HashMap::new(),
            RawExportPolicy::Develop,
        ),
        &progress,
        |shot, keep, source, progress| {
            if source == WebPixelSource::Finished {
                return decode_web_pixels(shot, keep, source, progress);
            }
            assert_eq!(progress.done.load(Ordering::Relaxed), 1);
            assert_eq!(progress.status().filename, "B_RAW.CR3");
            progress.cancel.store(true, Ordering::Relaxed);
            synthetic_pixels(keep)
        },
    );
    assert!(!failed, "cancellation is not a codec failure: {message}");
    assert!(
        message.contains("cancelled") && message.contains("1 written"),
        "{message}"
    );
    assert_eq!(progress.done.load(Ordering::Relaxed), 1);
    assert_eq!(progress.total.load(Ordering::Relaxed), 2);
    assert_eq!(
        std::fs::read(f.output("B_RAW.png")).unwrap(),
        b"user edited surviving prior output"
    );
    let manifest = support::read_export_manifest(&f.0.join("export")).unwrap();
    assert_eq!(manifest.files["B_RAW.png"], prior);
    assert!(manifest.files.contains_key("A_DONE.png"));
    no_parts(&f);
}

#[test]
fn raw_export_cancel_before_publish_discards_encoded_part_without_replacing() {
    let f = Fixture::new();
    f.raw("RAW.CR3");
    std::fs::create_dir(f.0.join("export")).unwrap();
    std::fs::write(f.output("RAW.png"), b"prior output").unwrap();
    let prior = support::stat_rec(&f.output("RAW.png")).unwrap();
    support::write_export_manifest(
        &f.0.join("export"),
        &[("RAW.png".into(), support::ManifestOutcome::Ok, Some(prior))],
    );
    let shots = f.shots();
    let progress = ExportProgress::default();
    let directory = f.0.join("export");
    *progress.before_publish.lock().unwrap() = Some(Box::new(move |progress| {
        assert!(
            std::fs::read_dir(&directory).unwrap().any(|e| {
                let e = e.unwrap();
                e.file_name().to_string_lossy().ends_with(".part")
                    && e.metadata().unwrap().len() > 0
            }),
            "cancellation arrives after real encoding and before final publication"
        );
        progress.cancel.store(true, Ordering::Relaxed);
    }));
    let (message, failed) = support::export_web_run_with_decoder(
        recipe(
            &f.0,
            &shots,
            &[1],
            &HashMap::new(),
            RawExportPolicy::Develop,
        ),
        &progress,
        |_, keep, _, _| synthetic_pixels(keep),
    );
    assert!(!failed && message.contains("cancelled"), "{message}");
    assert_eq!(progress.done.load(Ordering::Relaxed), 0);
    assert_eq!(std::fs::read(f.output("RAW.png")).unwrap(), b"prior output");
    assert_eq!(
        support::read_export_manifest(&f.0.join("export"))
            .unwrap()
            .files["RAW.png"],
        prior
    );
    no_parts(&f);
}

#[test]
fn raw_export_shutdown_flushes_completed_output_while_raw_is_still_decoding() {
    let f = Fixture::new();
    f.jpg("A_DONE.JPG");
    f.raw("B_RAW.CR3");
    let shots = f.shots();
    let progress = Arc::new(ExportProgress::default());
    let worker_progress = progress.clone();
    let directory = f.0.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        support::export_web_run_with_decoder(
            recipe(
                &directory,
                &shots,
                &[1, 1],
                &HashMap::new(),
                RawExportPolicy::Develop,
            ),
            &worker_progress,
            |shot, keep, source, progress| {
                if source == WebPixelSource::Finished {
                    return decode_web_pixels(shot, keep, source, progress);
                }
                started_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                synthetic_pixels(keep)
            },
        )
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(f.output("A_DONE.png").exists());
    assert!(support::read_export_manifest(&f.0.join("export")).is_none());
    // No wait for the decoder: the publication/ledger lock excludes it entirely.
    progress.stop_and_flush_completed();
    let at_shutdown = std::fs::read(f.0.join("export/falcon_export.json")).unwrap();
    assert!(support::read_export_manifest(&f.0.join("export"))
        .unwrap()
        .files
        .contains_key("A_DONE.png"));
    resume_tx.send(()).unwrap();
    let (message, failed) = worker.join().unwrap();
    assert!(!failed && message.contains("cancelled"), "{message}");
    assert!(!f.output("B_RAW.png").exists());
    assert_eq!(
        std::fs::read(f.0.join("export/falcon_export.json")).unwrap(),
        at_shutdown
    );
    no_parts(&f);
}

#[test]
fn raw_export_failure_does_not_fall_back_or_erase_surviving_provenance() {
    let f = Fixture::new();
    f.raw("RAW.CR3");
    std::fs::create_dir(f.0.join("export")).unwrap();
    std::fs::write(f.output("RAW.png"), b"old complete deliverable").unwrap();
    let prior = support::stat_rec(&f.output("RAW.png")).unwrap();
    support::write_export_manifest(
        &f.0.join("export"),
        &[("RAW.png".into(), support::ManifestOutcome::Ok, Some(prior))],
    );
    let shots = f.shots();
    let calls = RefCell::new(Vec::new());
    let (message, failed) = support::export_web_run_with_decoder(
        recipe(
            &f.0,
            &shots,
            &[1],
            &HashMap::new(),
            RawExportPolicy::Develop,
        ),
        &ExportProgress::default(),
        |_, _, source, _| {
            calls.borrow_mut().push(source);
            Err(WebPixelError::Failed("unsupported RAW".into()))
        },
    );
    assert!(failed && message.contains("1 FAILED"));
    assert_eq!(*calls.borrow(), vec![WebPixelSource::DevelopedRaw]);
    assert_eq!(
        std::fs::read(f.output("RAW.png")).unwrap(),
        b"old complete deliverable"
    );
    assert_eq!(
        support::read_export_manifest(&f.0.join("export"))
            .unwrap()
            .files["RAW.png"],
        prior
    );
    no_parts(&f);
}

#[test]
fn raw_export_cancel_before_decode_and_at_error_checkpoint_is_not_a_failure() {
    for before_decode in [true, false] {
        let f = Fixture::new();
        f.raw("RAW.CR3");
        let shots = f.shots();
        let progress = ExportProgress::default();
        progress.cancel.store(before_decode, Ordering::Relaxed);
        let (message, failed) = support::export_web_run_with_decoder(
            recipe(
                &f.0,
                &shots,
                &[1],
                &HashMap::new(),
                RawExportPolicy::Develop,
            ),
            &progress,
            |_, _, _, progress| {
                assert!(
                    !before_decode,
                    "a pre-cancelled run never calls its decoder"
                );
                progress.cancel.store(true, Ordering::Relaxed);
                Err(WebPixelError::Failed(
                    "decode returned during cancellation".into(),
                ))
            },
        );
        assert!(!failed && message.contains("cancelled"), "{message}");
        assert_eq!(progress.done.load(Ordering::Relaxed), 0);
        assert!(!f.output("RAW.png").exists());
        no_parts(&f);
    }
}

#[test]
fn raw_export_panic_guard_preserves_completed_records_and_clears_running() {
    let f = Fixture::new();
    f.jpg("A_DONE.JPG");
    f.raw("B_RAW.CR3");
    let shots = f.shots();
    let progress = Arc::new(ExportProgress::default());
    progress.running.store(true, Ordering::Relaxed);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = support::RunGuard(progress.clone());
        support::export_web_run_with_decoder(
            recipe(
                &f.0,
                &shots,
                &[1, 1],
                &HashMap::new(),
                RawExportPolicy::Develop,
            ),
            &progress,
            |shot, keep, source, progress| {
                assert_ne!(
                    source,
                    WebPixelSource::DevelopedRaw,
                    "synthetic codec panic"
                );
                decode_web_pixels(shot, keep, source, progress)
            },
        )
    }));
    assert!(result.is_err());
    assert!(!progress.running.load(Ordering::Relaxed));
    assert_eq!(progress.done.load(Ordering::Relaxed), 1);
    assert!(support::read_export_manifest(&f.0.join("export"))
        .unwrap()
        .files
        .contains_key("A_DONE.png"));
    assert!(!f.output("B_RAW.png").exists());
    no_parts(&f);
}

#[test]
fn raw_export_failed_manifest_retries_its_original_folder_after_another_run() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.jpg("A.JPG");
    b.jpg("B.JPG");
    std::fs::create_dir(a.0.join("export")).unwrap();
    std::fs::write(a.output("OLD.png"), b"prior edited output").unwrap();
    let old_record = support::stat_rec(&a.output("OLD.png")).unwrap();
    assert!(support::write_export_manifest(
        &a.0.join("export"),
        &[(
            "OLD.png".into(),
            support::ManifestOutcome::Ok,
            Some(old_record)
        )]
    ));
    let manifest_path = a.0.join("export/falcon_export.json");
    let old_manifest = std::fs::read(&manifest_path).unwrap();
    // Portable failure of the actual atomic replace: a directory occupies the manifest path.
    std::fs::remove_file(&manifest_path).unwrap();
    std::fs::create_dir(&manifest_path).unwrap();
    let progress = ExportProgress::default();
    for fixture in [&a, &b] {
        let shots = fixture.shots();
        let (message, failed) = support::export_web_run_with_decoder(
            recipe(
                &fixture.0,
                &shots,
                &[1],
                &HashMap::new(),
                RawExportPolicy::Skip,
            ),
            &progress,
            decode_web_pixels,
        );
        assert!(
            !failed,
            "the images completed; the separate manifest write may need retry: {message}"
        );
    }
    assert!(a.output("A.png").is_file());
    assert!(
        manifest_path.is_dir(),
        "the failed A manifest was not silently replaced"
    );
    let b_manifest = support::read_export_manifest(&b.0.join("export")).unwrap();
    assert!(b_manifest.files.contains_key("B.png") && !b_manifest.files.contains_key("A.png"));
    std::fs::remove_dir(&manifest_path).unwrap();
    std::fs::write(&manifest_path, &old_manifest).unwrap();
    progress.stop_and_flush_completed();
    let restored = support::read_export_manifest(&a.0.join("export")).unwrap();
    assert!(restored.files.contains_key("A.png"));
    assert_eq!(restored.files["OLD.png"], old_record);
    assert!(
        !restored.files.contains_key("B.png"),
        "later B must not retarget pending A records"
    );
    assert_eq!(
        std::fs::read(a.output("OLD.png")).unwrap(),
        b"prior edited output"
    );
}

#[test]
fn raw_export_completion_and_panic_show_the_captured_folder_after_a_switch() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.jpg("A.JPG");
    let shots = a.shots();
    let (message, failed) = support::export_web_run_with_decoder(
        recipe(&a.0, &shots, &[1], &HashMap::new(), RawExportPolicy::Skip),
        &ExportProgress::default(),
        decode_web_pixels,
    );
    let captured = a.0.display().to_string();
    let current = b.0.display().to_string();
    let event = crate::tick::op_result_notification(
        (
            message,
            support::OpSeverity::from_err(failed),
            2,
            Some(captured.clone()),
        ),
        &current,
        false,
        None,
        std::time::Instant::now(),
    );
    assert_eq!(event.reveal_path, a.0.join("export").display().to_string());
    assert_eq!(event.dir, captured);
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let slot = Arc::new(std::sync::Mutex::new(None));
    drop(support::OpGuard {
        busy: busy.clone(),
        slot: slot.clone(),
        kind: 2,
        origin: Some(captured.clone()),
    });
    let panic_event = crate::tick::op_result_notification(
        slot.lock().unwrap().take().unwrap(),
        &current,
        false,
        None,
        std::time::Instant::now(),
    );
    assert_eq!(panic_event.reveal_path, event.reveal_path);
    assert_eq!(panic_event.dir, captured);
    assert!(!busy.load(Ordering::Relaxed));
}

#[test]
fn raw_export_real_owner_fuji_and_portrait_canon_through_the_native_worker() {
    use std::hash::Hasher;
    use std::io::Read;

    // Streaming checksum plus metadata: no source copy/allocation and no new hashing dependency.
    let fingerprint = |path: &Path| {
        let metadata = std::fs::metadata(path).unwrap();
        let mut file = std::fs::File::open(path).unwrap();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            hash.write(&buffer[..read]);
        }
        (metadata.len(), metadata.modified().ok(), hash.finish())
    };
    for (path, orientation, expected_dims) in [
        (
            crate::fixture_paths::other_raw().join("DSCF1276.RAF"),
            1,
            (2560, 1706),
        ),
        (crate::fixture_paths::photos().join("HWU_0141.CR3"), 6, (1707, 2560)),
    ] {
        if !crate::fixture_paths::require_file(&path) {
            eprintln!("SKIP real native RAW fixture: {} is absent or not locally hydrated; portable worker tests remain active", path.display());
            continue;
        }
        let relative = path.file_name().unwrap().to_string_lossy().into_owned();
        let path = path.canonicalize().unwrap();
        let before = fingerprint(&path);
        let fixture = Fixture::new();
        assert!(
            !path.starts_with(&fixture.0),
            "the worker reads the external source and writes only its private output"
        );
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let shots = [Shot {
            id: 0,
            name: name.clone(),
            has_raw: true,
            has_jpg: false,
            raw: Some(path.clone()),
            jpg: None,
            kind: falcon_decode::SrcKind::Jpeg,
            sniffed: None,
            cloud_placeholder: false,
        }];
        assert_eq!(
            falcon_decode::read_orientation(&shots[0], true),
            Some(orientation)
        );
        let progress = ExportProgress::default();
        let (message, failed) = support::export_web_run(
            &fixture.0,
            &shots,
            &[1],
            0,
            1,
            2560,
            90,
            None,
            true,
            &HashMap::new(),
            None,
            WebFormat::Png,
            RawExportPolicy::Develop,
            WebCollide::Overwrite,
            &progress,
        );
        assert!(
            !failed && message.contains("1 developed from RAW"),
            "{relative}: {message}"
        );
        let filename = format!("{name}.png");
        let output = fixture.output(&filename);
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            (bytes[24], bytes[25]),
            (16, 2),
            "PNG IHDR names RGB16, not an 8-bit preview"
        );
        let mut at = 8usize;
        let mut has_srgb = false;
        while at + 12 <= bytes.len() {
            let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let end = at
                .checked_add(12)
                .and_then(|start| start.checked_add(len))
                .unwrap();
            assert!(end <= bytes.len(), "PNG chunk fits the generated file");
            has_srgb |= &bytes[at + 4..at + 8] == b"sRGB";
            at = end;
        }
        assert!(has_srgb, "the real output explicitly tags sRGB");
        let exported = falcon_decode::scan_folder(&fixture.0.join("export")).unwrap();
        assert_eq!(exported.len(), 1);
        let (pixels, width, height) =
            falcon_decode::decode_full_pixels(&exported[0], Keep::ALL).unwrap();
        assert_eq!(
            (width, height),
            expected_dims,
            "RAW orientation and default crop reach the native worker once"
        );
        let Pixels::Rgb16(samples) = pixels else {
            panic!("real RAW export did not retain RGB16")
        };
        assert!(
            samples.iter().any(|sample| sample % 257 != 0),
            "16-bit samples carry precision beyond widened RGB8"
        );
        assert!(
            matches!(
                falcon_decode::read_orientation(&exported[0], false),
                None | Some(1)
            ),
            "the rotated output does not retain the RAW's orientation tag"
        );
        let manifest = support::read_export_manifest(&fixture.0.join("export")).unwrap();
        assert_eq!(
            manifest.files[&filename],
            support::stat_rec(&output).unwrap()
        );
        assert_eq!(progress.done.load(Ordering::Relaxed), 1);
        assert_eq!(
            fingerprint(&path),
            before,
            "the original RAW's bytes/size/date must not change"
        );
        no_parts(&fixture);
        eprintln!("real native RAW verified: {relative}; PNG16 {width}x{height}; sRGB; orientation={orientation}; manifest recorded; source checksum/metadata unchanged");
    }
}

// Falsifier: keep Web in the real writer/collision/reveal path. Old deliveries stay untouched.
#[test]
fn public_release_export_directory_is_lowercase_and_old_web_is_preserved() {
    let f=Fixture::new(); f.jpg("sample.jpg");
    let old=f.0.join("Web"); std::fs::create_dir(&old).unwrap();
    std::fs::write(old.join("sample.png"),b"earlier delivery").unwrap();
    let shots=f.shots(); let marks=vec![1;shots.len()]; let deltas=HashMap::new();
    assert_eq!(support::web_collision_scan(&f.0,&shots,&marks,1,None,WebFormat::Png,RawExportPolicy::Skip),(0,1),"old Web files are not collisions in export");
    let (message,error)=support::export_web_run_with_decoder(recipe(&f.0,&shots,&marks,&deltas,RawExportPolicy::Skip),&ExportProgress::default(),decode_web_pixels);
    assert!(!error,"{message}");
    assert!(f.0.join("export/sample.png").is_file(),"new exports use export");
    assert_eq!(std::fs::read(old.join("sample.png")).unwrap(),b"earlier delivery");
    assert!(support::read_export_manifest(&f.0.join("export")).is_some());
    assert_eq!(support::web_collision_scan(&f.0,&shots,&marks,1,None,WebFormat::Png,RawExportPolicy::Skip),(1,1));
    let folders:Vec<_>=std::fs::read_dir(&f.0).unwrap().map(|e|e.unwrap().file_name()).collect();
    assert!(folders.iter().any(|n|n=="export"),"actual folder spelling is lowercase, even on a case-insensitive volume");
    assert_eq!(crate::tick::result_reveal_path(2,&f.0.display().to_string()),f.0.join("export").display().to_string());
}
