//! Bounded header I/O for folder scans. Classification and pairing still run in
//! directory order on the caller; only the independent 32-byte reads overlap.

use std::path::PathBuf;

pub(crate) const MAX_SCAN_READERS: usize = 8;
// Open-time check (2 November): a scan reads only names shared by two or more finished
// files and the clicked photo. A folder of exported pairs (X.jpg + X.png) can still be
// large, so overlap those; the clicked photo and tiny scans stay serial, same cap.
const PARALLEL_SCAN_MIN_FILES: usize = 16;

pub(crate) fn reader_count(files: usize) -> usize {
    let requested = std::env::var("FALCON_SCAN_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    reader_count_for(files, requested)
}

fn reader_count_for(files: usize, requested: usize) -> usize {
    if files < PARALLEL_SCAN_MIN_FILES { 1 } else { requested.clamp(1, MAX_SCAN_READERS) }
}

/// The placeholder gate is inside the read operation, so even a worker receiving
/// a cloud path cannot open it. At most eight files are open simultaneously.
pub(crate) fn read_heads(
    files: &[(PathBuf, crate::SrcKind, bool)],
    readers: usize,
    is_cancelled: &(impl Fn() -> bool + Sync),
) -> std::io::Result<Vec<Option<Vec<u8>>>> {
    read_heads_with(files, readers, &crate::read_head, is_cancelled)
}

pub(crate) fn check_cancelled(is_cancelled: &impl Fn() -> bool) -> std::io::Result<()> {
    if is_cancelled() {
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "folder scan superseded",
        ))
    } else {
        Ok(())
    }
}

fn read_heads_with(
    files: &[(PathBuf, crate::SrcKind, bool)],
    readers: usize,
    read: &(impl Fn(&std::path::Path) -> Option<Vec<u8>> + Sync),
    is_cancelled: &(impl Fn() -> bool + Sync),
) -> std::io::Result<Vec<Option<Vec<u8>>>> {
    read_heads_with_start(files, readers, read, is_cancelled, &|| Ok(()))
}

// The pre-start hook lets tests inject resource exhaustion into the same Result
// handled for Builder::spawn_scoped failures; production always attempts the spawn.
fn read_heads_with_start(
    files: &[(PathBuf, crate::SrcKind, bool)],
    readers: usize,
    read: &(impl Fn(&std::path::Path) -> Option<Vec<u8>> + Sync),
    is_cancelled: &(impl Fn() -> bool + Sync),
    before_spawn: &impl Fn() -> std::io::Result<()>,
) -> std::io::Result<Vec<Option<Vec<u8>>>> {
    check_cancelled(is_cancelled)?;
    let width = readers.clamp(1, MAX_SCAN_READERS);
    let read_chunk = |files: &[(PathBuf, crate::SrcKind, bool)]| {
        let mut out = Vec::with_capacity(files.len());
        for (path, _, placeholder) in files {
            check_cancelled(is_cancelled)?;
            out.push(if *placeholder { None } else { read(path) });
        }
        check_cancelled(is_cancelled)?;
        Ok::<_, std::io::Error>(out)
    };
    if width == 1 || files.is_empty() {
        return read_chunk(files);
    }
    // Claim individual files so a slow directory region does not strand one
    // fixed chunk while other workers idle. Indexed results preserve exact order.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let read_next = || {
        let mut out = Vec::new();
        loop {
            check_cancelled(is_cancelled)?;
            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some((path, _, placeholder)) = files.get(index) else { break; };
            out.push((index, if *placeholder { None } else { read(path) }));
        }
        Ok::<_, std::io::Error>(out)
    };
    std::thread::scope(|scope| {
        let mut result = vec![None; files.len()];
        let mut workers = Vec::new();
        for _ in 0..width.min(files.len()) {
            check_cancelled(is_cancelled)?;
            let launch = before_spawn().and_then(|()| {
                std::thread::Builder::new().spawn_scoped(scope, read_next)
            });
            match launch {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    for (index, head) in read_next()? { result[index] = head; }
                }
            }
        }
        for worker in workers {
            let heads = worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
            for (index, head) in heads { result[index] = head; }
        }
        check_cancelled(is_cancelled)?;
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Falsifier: restore the 64-file cutoff. The real minimum nearby batch of
    // 21 verified shots then pays every cold open sequentially.
    #[test]
    fn nearby_batches_overlap_header_latency_without_parallelizing_the_clicked_file() {
        assert_eq!(reader_count_for(1, 8), 1);
        assert_eq!(reader_count_for(21, 4), 4);
        let files: Vec<_> = (0..21).map(|i|(PathBuf::from(i.to_string()),crate::SrcKind::Png,false)).collect();
        let live=AtomicUsize::new(0); let peak=AtomicUsize::new(0);
        let read=|p: &std::path::Path| {
            let n=live.fetch_add(1,Ordering::SeqCst)+1; peak.fetch_max(n,Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            live.fetch_sub(1,Ordering::SeqCst);
            Some(p.to_string_lossy().as_bytes().to_vec())
        };
        let started=std::time::Instant::now();
        let serial=read_heads_with(&files,1,&read,&||false).unwrap();
        let serial_ms=started.elapsed().as_millis(); peak.store(0,Ordering::SeqCst);
        let started=std::time::Instant::now();
        let parallel=read_heads_with(&files,reader_count_for(files.len(),4),&read,&||false).unwrap();
        let parallel_ms=started.elapsed().as_millis();
        assert_eq!(serial,parallel,"same bytes and ordering");
        assert!(peak.load(Ordering::SeqCst)>1 && peak.load(Ordering::SeqCst)<=4);
        println!("controlled 21-file latency: serial={serial_ms}ms parallel={parallel_ms}ms; 5ms synthetic wait per read");
    }

    #[test]
    fn parallel_scan_reads_preserve_identity_and_never_open_placeholders() {
        let files: Vec<_> = (0..96)
            .map(|i| {
                (
                    PathBuf::from(i.to_string()),
                    crate::SrcKind::Png,
                    i % 7 == 0,
                )
            })
            .collect();
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let calls = AtomicUsize::new(0);
        let read = |p: &std::path::Path| {
            let i: usize = p.to_str().unwrap().parse().unwrap();
            assert_ne!(i % 7, 0, "a placeholder reached file I/O");
            calls.fetch_add(1, Ordering::SeqCst);
            let n = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(n, Ordering::SeqCst);
            std::thread::yield_now();
            live.fetch_sub(1, Ordering::SeqCst);
            if i.is_multiple_of(11) {
                None
            } else {
                Some(vec![i as u8])
            }
        };
        let heads = read_heads_with(&files, 1000, &read, &|| false).unwrap();
        assert!(peak.load(Ordering::SeqCst) <= MAX_SCAN_READERS);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            files.iter().filter(|f| !f.2).count()
        );
        for (i, head) in heads.iter().enumerate() {
            assert_eq!(
                *head,
                if i % 7 == 0 || i % 11 == 0 {
                    None
                } else {
                    Some(vec![i as u8])
                }
            );
        }
        assert_eq!(heads, read_heads_with(&files, 1, &read, &|| false).unwrap());
    }

    #[test]
    fn thread_start_failures_fall_back_without_losing_or_reordering_files() {
        let files: Vec<_> = (0..96)
            .map(|i| {
                (
                    PathBuf::from(i.to_string()),
                    crate::SrcKind::Png,
                    i % 7 == 0,
                )
            })
            .collect();
        let read = |p: &std::path::Path| Some(p.to_string_lossy().as_bytes().to_vec());
        let expected = read_heads_with(&files, 1, &read, &|| false).unwrap();
        for fail_every_start in [false, true] {
            let attempted = AtomicUsize::new(0);
            let before_spawn = || {
                let i = attempted.fetch_add(1, Ordering::Relaxed);
                if fail_every_start || !i.is_multiple_of(2) {
                    Err(std::io::Error::other("injected thread resource exhaustion"))
                } else {
                    Ok(())
                }
            };
            let actual = read_heads_with_start(&files, 8, &read, &|| false, &before_spawn).unwrap();
            assert_eq!(
                actual, expected,
                "a failed launch must still read its complete chunk"
            );
            assert_eq!(attempted.load(Ordering::Relaxed), MAX_SCAN_READERS);
        }
    }

    #[test]
    fn cancelled_header_reads_stop_and_return_no_partial_result() {
        use std::sync::atomic::AtomicBool;
        let files: Vec<_> = (0..96)
            .map(|i| (PathBuf::from(i.to_string()), crate::SrcKind::Png, false))
            .collect();
        for readers in [1, MAX_SCAN_READERS] {
            let cancelled = AtomicBool::new(false);
            let reads = AtomicUsize::new(0);
            let read = |_: &std::path::Path| {
                reads.fetch_add(1, Ordering::Relaxed);
                cancelled.store(true, Ordering::Relaxed);
                Some(vec![1])
            };
            let result = read_heads_with(&files, readers, &read, &|| {
                cancelled.load(Ordering::Relaxed)
            });
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
            assert!(
                (1..=readers).contains(&reads.load(Ordering::Relaxed)),
                "only in-flight reads may finish"
            );
        }
        let result = read_heads_with(
            &files,
            MAX_SCAN_READERS,
            &|_| panic!("cancelled scan opened a file"),
            &|| true,
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().canonicalize().unwrap();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = root.join(format!(
                "falcon_scan_io_{}_{}_{}",
                std::process::id(),
                nonce,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            assert_eq!(path.parent(), Some(root.as_path()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn requested_shot_matches_full_scan_pairing_and_reads_only_its_group() {
        let scratch = Scratch::new();
        let dir = &scratch.0;
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        let jpeg = include_bytes!("../tests/fixtures/mismatch/jpeg_named_png.png");
        for i in 0..600 {
            std::fs::write(dir.join(format!("other_{i:04}.png")), png).unwrap();
        }
        for (name, bytes) in [("chosen.CR3", &b"raw"[..]), ("chosen.DNG", &b"raw"[..]),
            ("chosen.jpg", png.as_slice()), ("chosen.png", jpeg.as_slice())] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let group = crate::scan_folder_filtered(dir, Some(1), &|| false, Some("chosen")).unwrap();
        assert_eq!(group.header_reads, 2);
        assert_eq!(group.metadata.len(), 4);
        let full = crate::scan_folder(dir).unwrap();
        for file in ["chosen.CR3", "chosen.DNG", "chosen.jpg", "chosen.png"] {
            let path = dir.join(file);
            let early = crate::scan_requested_shot(&path, || false).unwrap().unwrap();
            let final_shot = full.iter().find(|s| s.raw.as_ref() == Some(&path) || s.jpg.as_ref() == Some(&path)).unwrap();
            assert_eq!((&early.name, &early.raw, &early.jpg, early.kind, early.has_jpg),
                (&final_shot.name, &final_shot.raw, &final_shot.jpg, final_shot.kind, final_shot.has_jpg));
        }
    }

    #[test]
    fn catalogue_prioritizes_complete_raw_pairs_and_reuses_each_header() {
        let scratch = Scratch::new();
        let jpeg = include_bytes!("../tests/fixtures/mismatch/jpeg_named_png.png");
        for i in 0..80 {
            std::fs::write(scratch.0.join(format!("P{i:04}.JPG")), jpeg).unwrap();
            std::fs::write(scratch.0.join(format!("P{i:04}.CR3")), b"RAW group partner").unwrap();
        }
        let mut catalogue = crate::FolderCatalogue::read(&scratch.0, || false).unwrap();
        assert!(catalogue.heads.is_empty(), "listing must not read any image header");
        assert_eq!(catalogue.simple_candidates().unwrap().len(), 80);
        let current = catalogue.requested_shot(&scratch.0.join("P0040.CR3"), || false).unwrap().unwrap();
        assert!(current.has_jpg && current.has_raw);
        assert_eq!(catalogue.heads.len(), 1, "unrelated cold files have not been opened");
        let paths: Vec<_> = (30..51).map(|i| scratch.0.join(format!("P{i:04}.JPG"))).collect();
        let batch = catalogue.scan_batch(&paths, || false).unwrap().unwrap();
        assert_eq!((batch.shots.len(), batch.header_reads), (21, 0));
        assert!(batch.shots.iter().all(|s| s.has_raw && s.has_jpg));
        assert_eq!(catalogue.heads.len(), 1, "the folder is ready once listed: only the opened photo was read");
        let full = catalogue.finish(|| false).unwrap();
        assert_eq!((full.shots.len(), full.header_reads), (80, 0));
        let fresh = crate::scan_folder(&scratch.0).unwrap();
        for (a,b) in full.shots.iter().zip(fresh.iter()) {
            assert_eq!((&a.name,&a.jpg,&a.raw,a.kind),(&b.name,&b.jpg,&b.raw,b.kind));
        }
    }

    #[test]
    fn catalogue_handles_mixed_formats_and_invalidates_changed_preview_sources() {
        let scratch = Scratch::new();
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        let jpeg = include_bytes!("../tests/fixtures/mismatch/jpeg_named_png.png");
        for (name,bytes) in [("a.jpg",png.as_slice()),("b.png",png.as_slice()),
            ("c.heic",&b"\0\0\0\x18ftypheic\0\0\0\0heicmif1"[..]),
            ("d.webp",&b"RIFF\x18\0\0\0WEBPVP8 "[..]),("e.tif",&b"II\x2a\0\x08\0\0\0"[..])] {
            std::fs::write(scratch.0.join(name),bytes).unwrap();
        }
        std::fs::write(scratch.0.join("c.RAF"), b"raw partner").unwrap();
        let mut catalogue = crate::FolderCatalogue::read(&scratch.0, || false).unwrap();
        let candidates = catalogue.simple_candidates().unwrap();
        assert_eq!(candidates.len(),5);
        let paths: Vec<_> = candidates.iter().map(|c|c.path.clone()).collect();
        let early = catalogue.scan_batch(&paths, || false).unwrap().unwrap();
        let fresh = crate::scan_folder(&scratch.0).unwrap();
        for (a,b) in early.shots.iter().zip(fresh.iter()) {
            assert_eq!((&a.name,&a.jpg,&a.raw,a.kind,a.has_jpg),(&b.name,&b.jpg,&b.raw,b.kind,b.has_jpg));
        }
        // Open-time check (2 November): the scan goes by the name, and the bytes decide at the reader.
        assert_eq!(early.shots[0].kind, crate::SrcKind::Jpeg, "the scan takes the JPG name");
        assert_eq!(crate::shot_by_bytes(&early.shots[0]).kind, crate::SrcKind::Png, "bytes still override it");
        std::fs::write(scratch.0.join("a.jpg"),jpeg).unwrap();
        std::fs::remove_file(scratch.0.join("b.png")).unwrap();
        let full = catalogue.finish(|| false).unwrap();
        assert!(full.changed_sources.contains(&scratch.0.join("a.jpg")));
        assert!(full.changed_sources.contains(&scratch.0.join("b.png")));
        assert_eq!(full.shots.len(),4);
        assert_eq!(full.shots[0].kind,crate::SrcKind::Jpeg);
        assert_eq!(full.header_reads,0);
        assert_eq!(catalogue.finish(|| true).err().unwrap().kind(),std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn catalogue_declines_ambiguous_neighborhoods_without_guessing_pairing() {
        let scratch = Scratch::new();
        for name in ["a.jpg", "a.CR3", "a.DNG"] { std::fs::write(scratch.0.join(name),b"x").unwrap(); }
        let c = crate::FolderCatalogue::read(&scratch.0, || false).unwrap();
        assert!(c.simple_candidates().is_none());
        std::fs::remove_file(scratch.0.join("a.DNG")).unwrap();
        std::fs::write(scratch.0.join("a.tif"),b"x").unwrap();
        std::fs::write(scratch.0.join("a.tif.png"),b"x").unwrap();
        let mut c = crate::FolderCatalogue::read(&scratch.0, || false).unwrap();
        assert!(c.simple_candidates().is_none());
        assert!(c.requested_shot(&scratch.0.join("a.jpg"), || false).unwrap().is_none());
    }

    #[test]
    fn simple_catalogue_proves_dotted_and_parenthesized_names_safe() {
        let scratch = Scratch::new();
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        for name in ["a.png", "a.edit.png", "a (2).png"] { std::fs::write(scratch.0.join(name),png).unwrap(); }
        std::fs::write(scratch.0.join("a.edit.CR3"), b"RAW partner").unwrap();
        let mut c = crate::FolderCatalogue::read(&scratch.0, || false).unwrap();
        let candidates = c.simple_candidates().unwrap();
        assert_eq!(candidates.len(),3);
        let path = scratch.0.join("a.edit.CR3");
        assert_eq!(c.requested_shot(&path, || false).unwrap().unwrap().name,"a.edit");
        let paths: Vec<_> = candidates.iter().map(|c|c.path.clone()).collect();
        let early = c.scan_batch(&paths, || false).unwrap().unwrap();
        let full = c.finish(|| false).unwrap();
        assert_eq!(early.shots.iter().map(|s|(&s.name,&s.raw,&s.jpg)).collect::<Vec<_>>(),
            full.shots.iter().map(|s|(&s.name,&s.raw,&s.jpg)).collect::<Vec<_>>());
    }

    #[test]
    fn requested_shot_defers_ambiguous_names_and_cancellation() {
        let scratch = Scratch::new();
        for name in ["A.jpg", "A.tif", "A.tif.png", "._hidden.jpg"] {
            std::fs::write(scratch.0.join(name), []).unwrap();
        }
        for name in ["A.jpg", "A.tif.png", "._hidden.jpg", "missing.png"] {
            assert!(crate::scan_requested_shot(&scratch.0.join(name), || false).unwrap().is_none());
        }
        assert_eq!(crate::scan_requested_shot(&scratch.0.join("A.jpg"), || true).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn nearby_batch_reads_only_selected_files_and_refuses_partial_or_paired_catalogues() {
        let scratch = Scratch::new();
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        for i in 0..80 { std::fs::write(scratch.0.join(format!("P{i:04}.png")), png).unwrap(); }
        let mut catalogue = crate::single_photo_candidates(&scratch.0, || false).unwrap().unwrap();
        catalogue.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(catalogue.len(), 80);
        let paths: Vec<_> = catalogue.iter().skip(30).take(21).map(|c| c.path.clone()).collect();
        let batch = crate::scan_requested_batch(&paths, || false).unwrap().unwrap();
        assert_eq!((batch.shots.len(), batch.header_reads, batch.metadata.len()), (21, 0, 21));
        let full = crate::scan_folder(&scratch.0).unwrap();
        assert_eq!(batch.shots.iter().map(|s| &s.jpg).collect::<Vec<_>>(),
            full[30..51].iter().map(|s| &s.jpg).collect::<Vec<_>>());
        std::fs::remove_file(&paths[10]).unwrap();
        assert!(crate::scan_requested_batch(&paths, || false).unwrap().is_none());
        std::fs::write(scratch.0.join("P0000.jpg"), png).unwrap();
        assert!(crate::single_photo_candidates(&scratch.0, || false).unwrap().is_none());
        std::fs::remove_file(scratch.0.join("P0000.jpg")).unwrap();
        std::fs::write(scratch.0.join("raw.CR3"), []).unwrap();
        assert!(crate::single_photo_candidates(&scratch.0, || false).unwrap().is_none());
        assert_eq!(crate::scan_requested_batch(&paths, || true).err().unwrap().kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn actual_serial_and_parallel_scans_agree_on_pairing_classification_and_metadata() {
        let scratch = Scratch::new();
        let dir = &scratch.0;
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        let jpeg = include_bytes!("../tests/fixtures/mismatch/jpeg_named_png.png");
        let write = |name: &str, bytes: &[u8]| std::fs::write(dir.join(name), bytes).unwrap();
        for i in 0..80 {
            write(&format!("bulk_{i:03}.png"), png);
        }
        write("pair.CR3", b"raw files are never sniffed");
        write("pair.DNG", b"every same-stem raw remains visible");
        write("pair.JPG", png);
        write("pair.jpeg", jpeg);
        write(
            "passenger.CR3",
            b"raw with an unsupported finished passenger",
        );
        write("passenger.avif", b"\0\0\0\x18ftypavif\0\0\0\0avifmif1");
        write("unknown.jpg", b"unknown magic keeps extension routing");
        write("misnamed.png", jpeg);
        write("twin.jpg", jpeg);
        write("twin.tif", b"II\x2a\0\x08\0\0\0");
        write("twin.tif.png", png);
        write("._hidden.jpg", jpeg);
        write("unrelated.txt", png);
        std::fs::create_dir(dir.join("nested.png")).unwrap();
        #[cfg(windows)]
        {
            use std::io::Write;
            use std::os::windows::fs::OpenOptionsExt;
            // A disposable OFFLINE marker exercises the actual metadata gate without
            // needing a cloud provider or ever modifying the owner's cloud files.
            let mut offline = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .attributes(crate::FILE_ATTRIBUTE_OFFLINE)
                .open(dir.join("offline.jpg"))
                .unwrap();
            offline.write_all(png).unwrap();
        }
        let serial = crate::scan_folder_with_readers(dir, Some(1), &|| false).unwrap();
        let parallel =
            crate::scan_folder_with_readers(dir, Some(MAX_SCAN_READERS), &|| false).unwrap();
        let identities = |scan: &crate::FolderScan| {
            scan.shots
                .iter()
                .map(|s| {
                    (
                        s.id,
                        s.name.clone(),
                        s.raw.clone(),
                        s.jpg.clone(),
                        s.has_raw,
                        s.has_jpg,
                        s.kind,
                        s.sniffed,
                        s.cloud_placeholder,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(identities(&parallel), identities(&serial));
        assert_eq!(parallel.appledouble, 1);
        assert_eq!(parallel.appledouble, serial.appledouble);
        assert_eq!(parallel.header_reads, serial.header_reads);
        let metadata = |scan: &crate::FolderScan| {
            scan.metadata
                .iter()
                .map(|(p, m)| (p.clone(), m.len(), m.modified().ok()))
                .collect::<Vec<_>>()
        };
        assert_eq!(metadata(&parallel), metadata(&serial));
        let paired = parallel
            .shots
            .iter()
            .find(|s| s.raw.as_deref() == Some(dir.join("pair.CR3").as_path()))
            .unwrap();
        assert_eq!(
            paired.jpg.as_deref(),
            Some(dir.join("pair.jpeg").as_path()),
            "real JPEG wins over PNG named JPG"
        );
        assert_eq!(paired.kind, crate::SrcKind::Jpeg);
        let impostor = parallel
            .shots
            .iter()
            .find(|s| s.jpg.as_deref() == Some(dir.join("pair.JPG").as_path()))
            .unwrap();
        assert_eq!(
            (impostor.kind, impostor.sniffed),
            (crate::SrcKind::Png, Some(crate::SrcKind::Png))
        );
        let passenger = parallel
            .shots
            .iter()
            .find(|s| s.name == "passenger")
            .unwrap();
        assert!(passenger.has_raw && !passenger.has_jpg && passenger.jpg.is_some());
        assert_eq!(
            passenger.kind,
            crate::SrcKind::Jpeg,
            "unsupported finished passenger must not hide RAW picture"
        );
        let names: std::collections::HashSet<_> = parallel.shots.iter().map(|s| &s.name).collect();
        assert_eq!(
            names.len(),
            parallel.shots.len(),
            "dotted-stem and split-shot names stay unique"
        );
        #[cfg(windows)]
        {
            let offline = parallel.shots.iter().find(|s| s.name == "offline").unwrap();
            assert!(offline.cloud_placeholder);
            assert_eq!(
                (offline.kind, offline.sniffed),
                (crate::SrcKind::Jpeg, None),
                "placeholder bytes cannot reclassify it"
            );
        }
    }

    /// Open-time check (2 November): **a scan reads only names shared by two finished files.** That
    /// is the owner's speed change, and its one exception keeps pairing right.
    ///
    /// L28 — REDDENED BY: removing the stem-count term from the scan's read filter (the lone files
    /// are not read: `header_reads` 0 and the RAW pairs with the impostor by path order), or making
    /// every candidate read again (`header_reads` 42 and the lone files become `Png`).
    #[test]
    fn only_names_shared_by_two_finished_files_are_read_at_scan() {
        let scratch = Scratch::new();
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        let jpeg = include_bytes!("../tests/fixtures/mismatch/jpeg_named_png.png");
        for i in 0..40 {
            std::fs::write(scratch.0.join(format!("lone_{i:02}.JPG")), png).unwrap();
        }
        std::fs::write(scratch.0.join("IMG_1.CR3"), b"never sniffed").unwrap();
        std::fs::write(scratch.0.join("IMG_1.JPG"), png).unwrap();
        std::fs::write(scratch.0.join("IMG_1.jpeg"), jpeg).unwrap();

        let scan = crate::scan_folder_with_metadata(&scratch.0).unwrap();
        assert_eq!(scan.header_reads, 2, "the two files that share a name, nothing else");
        for s in scan.shots.iter().filter(|s| s.name.starts_with("lone_")) {
            assert_eq!((s.kind, s.sniffed), (crate::SrcKind::Jpeg, None), "{} takes its name", s.name);
        }
        let paired = scan.shots.iter().find(|s| s.has_raw).unwrap();
        assert_eq!(paired.jpg.as_deref(), Some(scratch.0.join("IMG_1.jpeg").as_path()), "the real JPEG pairs");
        let impostor = scan.shots.iter().find(|s| s.jpg.as_deref() == Some(scratch.0.join("IMG_1.JPG").as_path())).unwrap();
        assert_eq!((impostor.kind, impostor.sniffed), (crate::SrcKind::Png, Some(crate::SrcKind::Png)));
    }

    /// Open-time check (2 November): **the opened photo is read when it is opened, and stays the same
    /// at every opening stage**, so promotion's identity check holds. An AVIF named `.jpg` must stay
    /// out of the early publish (`main.rs`, which filters on `is_unsupported`), or it would hold the
    /// folder for ten seconds.
    ///
    /// L28 — REDDENED BY: dropping `wanted` from the scan's read filter (the early shot is `Jpeg`),
    /// or `finish` no longer reusing the opening's cached head (`header_reads` 1).
    #[test]
    fn the_opened_misnamed_photo_is_checked_at_open_and_keeps_that_answer() {
        let scratch = Scratch::new();
        let png = include_bytes!("../tests/fixtures/mismatch/png_named_jpg.jpg");
        std::fs::write(scratch.0.join("lone.JPG"), png).unwrap();
        for i in 0..20 {
            std::fs::write(scratch.0.join(format!("single_{i:02}.png")), png).unwrap();
        }
        let path = scratch.0.join("lone.JPG");
        let mut catalogue = crate::FolderCatalogue::read(&scratch.0, || false).unwrap();
        let early = catalogue.requested_shot(&path, || false).unwrap().unwrap();
        assert_eq!((early.kind, early.sniffed), (crate::SrcKind::Png, Some(crate::SrcKind::Png)));
        let same = |s: &crate::Shot| (s.name.clone(), s.raw.clone(), s.jpg.clone(), s.kind, s.has_jpg, s.cloud_placeholder);
        let paths: Vec<_> = catalogue.simple_candidates().unwrap().into_iter().map(|c| c.path).collect();
        let batch = catalogue.scan_batch(&paths, || false).unwrap().unwrap();
        let in_batch = batch.shots.iter().find(|s| s.jpg.as_deref() == Some(path.as_path())).unwrap();
        assert_eq!(same(in_batch), same(&early));
        let full = catalogue.finish(|| false).unwrap();
        assert_eq!(full.header_reads, 0, "the opened photo's head is reused");
        let in_full = full.shots.iter().find(|s| s.jpg.as_deref() == Some(path.as_path())).unwrap();
        assert_eq!(same(in_full), same(&early));

        // An AVIF named `.jpg`, opened: unsupported from the first stage, as the early publish needs.
        let other = Scratch::new();
        std::fs::write(other.0.join("holiday.jpg"), b"\0\0\0\x18ftypavif\0\0\0\0mif1miaf").unwrap();
        let mut catalogue = crate::FolderCatalogue::read(&other.0, || false).unwrap();
        let early = catalogue.requested_shot(&other.0.join("holiday.jpg"), || false).unwrap().unwrap();
        assert!(early.is_unsupported());
    }

    #[test]
    fn cancelled_directory_scan_never_returns_a_partial_folder() {
        let scratch = Scratch::new();
        for i in 0..5 {
            std::fs::write(scratch.0.join(format!("{i}.png")), b"\x89PNG\r\n\x1a\n").unwrap();
        }
        let calls = AtomicUsize::new(0);
        let result = crate::scan_folder_with_metadata_cancellable(&scratch.0, || {
            calls.fetch_add(1, Ordering::Relaxed) >= 2
        });
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::Interrupted
        );
        let result =
            crate::scan_folder_with_metadata_cancellable(&scratch.0.join("not-present"), || true);
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::Interrupted,
            "already superseded requests do no directory I/O"
        );
    }
}
