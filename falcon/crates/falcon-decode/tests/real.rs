//! Integration tests on the real Canon CR3 + JPG test set. They self-skip if the
//! folder is absent so CI without the assets still passes.

#[path = "../../test-support/fixture_paths.rs"]
mod fixture_paths;

use std::io::Cursor;
use std::path::Path;

use falcon_decode::*;


fn shots_or_skip() -> Option<Vec<Shot>> {
    let dir = Path::new(fixture_paths::photos());
    if !dir.exists() {
        eprintln!("SKIP: set FALCON_PHOTO_TEST_DIR; corpus missing ({})", fixture_paths::photos().display());
        return None;
    }
    Some(scan_folder(dir).expect("scan folder"))
}

fn jpeg_dims(bytes: &[u8]) -> (u32, u32) {
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    d.read_info().expect("jpeg info");
    let i = d.info().unwrap();
    (i.width as u32, i.height as u32)
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 1000 && bytes[..3] == [0xFF, 0xD8, 0xFF]
}

#[test]
fn scans_and_pairs() {
    let Some(shots) = shots_or_skip() else { return };
    assert!(!shots.is_empty(), "expected shots");
    // This mutable corpus also contains RAW-only highlight samples. Require pairing
    // when a real JPG sibling exists; do not invent a JPG for HWU_0114.CR3.
    assert!(shots.iter().any(|s| s.has_raw && s.has_jpg), "expected at least one RAW+JPG pair");
    for shot in &shots {
        if let Some(raw)=&shot.raw {
            if raw.with_extension("JPG").is_file() {
                assert!(shot.has_jpg, "existing sibling must be paired: {}",shot.name);
            }
        }
    }
    if let Some(solo)=shots.iter().find(|s|s.name=="HWU_0114") {
        if !Path::new(fixture_paths::photos()).join("HWU_0114.JPG").exists() {
            assert!(solo.has_raw && !solo.has_jpg,"RAW-only sample must remain RAW-only");
        }
    }
    // ids are stable and sequential
    for (i, s) in shots.iter().enumerate() {
        assert_eq!(s.id, i);
        assert!(!s.name.is_empty());
    }
}

#[test]
fn fast_frame_is_downscaled_jpeg() {
    let Some(shots) = shots_or_skip() else { return };
    let f = fast_frame(&shots[0], 4096).expect("fast frame");
    assert_eq!(f.mime, "image/jpeg");
    assert!(is_jpeg(&f.bytes));
    let (w, h) = jpeg_dims(&f.bytes);
    assert!(w.max(h) <= 4096, "fast frame should fit 4096, got {w}x{h}");
    assert!(w.max(h) >= 2048, "fast frame unexpectedly small {w}x{h}");
}

#[test]
fn reference_frame_is_valid() {
    let Some(shots) = shots_or_skip() else { return };
    let r = reference_frame(&shots[0], 4096).expect("reference frame");
    assert!(is_jpeg(&r.bytes));
    let (w, h) = jpeg_dims(&r.bytes);
    assert!(w.max(h) <= 4096);
}

#[test]
fn thumbnail_is_small() {
    let Some(shots) = shots_or_skip() else { return };
    let t = thumbnail(&shots[0], 320).expect("thumbnail");
    assert!(is_jpeg(&t.bytes));
    let (w, h) = jpeg_dims(&t.bytes);
    assert!(w.max(h) <= 320, "thumb should fit 320, got {w}x{h}");
}

#[test]
fn develop_raw_produces_image() {
    let Some(shots) = shots_or_skip() else { return };
    let raw = shots.iter().find(|s| s.has_raw).expect("a raw shot");
    let d = develop_raw(raw, 4096).expect("develop raw");
    assert!(is_jpeg(&d.bytes));
    let (w, h) = jpeg_dims(&d.bytes);
    assert!(w.max(h) <= 4096 && w.max(h) >= 2048);
}

/// v1.0.0-rc (BYTES OVER NAMES, same-commit fix): this row asks whether EXIF is read from a
/// CAMERA PHOTOGRAPH, so it must pick one — it used to take `shots[0]`, i.e. whatever happens to
/// sort first in a folder the owner adds files to by hand. On 2026-09-02 that became
/// `53d879f01a3f481c.JPG`, the field report's rasterised page (a PNG with a `.JPG` name and, the
/// investigation measured, no ancillary chunks at all — no `eXIf`, nothing), and `'5' < 'A' < 'H'`
/// put it first. The row went red on a file that HAS no camera, which is not the question it asks
/// and not a fact about this round: `read_exif` reaches the EXIF through kamadak-exif's
/// `read_from_container`, which sniffs the container itself and never consults `Shot::kind`.
/// A RAW sibling is the definition of "came out of a camera", so that is the pick.
#[test]
fn exif_reads_core_fields() {
    let Some(shots) = shots_or_skip() else { return };
    let cam = shots.iter().find(|s| s.has_raw && s.has_jpg).expect("a camera shot (RAW + finished)");
    let ex = read_exif(cam);
    assert!(ex.camera.is_some(), "camera should be present");
    assert!(ex.dimensions.is_some(), "dimensions should be present");
    assert!(ex.files.is_some(), "file sizes should be present");
}

/// F8 regression (CODEBASE_REVIEW_2026-07 §5 defect #3): the crafted-RAW scan-work cap added to
/// `largest_embedded_jpeg` must NOT change real-file behavior — HWU_0141.CR3's embedded JPEG preview
/// must still be found and decode. Exercised through the same public path the fast tier uses for a
/// RAW-only shot (`fast_frame_rgba` → `jpeg_source` → `largest_embedded_jpeg`), by viewing the pair
/// as RAW-only so the JPG sibling can't satisfy the decode instead.
#[test]
fn cr3_embedded_jpeg_still_found_after_scan_cap() {
    let Some(shots) = shots_or_skip() else { return };
    let Some(pair) = shots.iter().find(|s| s.name == "HWU_0141") else {
        eprintln!("skip: HWU_0141 not in the test set");
        return;
    };
    assert!(pair.has_raw, "probe shot should carry its CR3");
    let raw_only = Shot {
        id: pair.id,
        name: pair.name.clone(),
        has_raw: true,
        has_jpg: false,
        raw: pair.raw.clone(),
        jpg: None,
        kind: pair.kind,
        cloud_placeholder: false,
        sniffed: None,
    };
    let (rgba, w, h) = fast_frame_rgba(&raw_only, 2048, false)
        .expect("embedded CR3 preview must still be found and decode (F8 cap regression)");
    assert!(w.max(h) > 0 && rgba.len() == (w as usize * h as usize * 4), "sane RGBA frame from the embedded preview");
}

/// v0.8.0 rotation: the round's probe pair — the R5 II writes LANDSCAPE pixels (8192×5464) with
/// EXIF Orientation = 6 (→ 1 clockwise quarter-turn for upright display). Pins BOTH read paths:
/// kamadak-exif for the JPG side and rawler `raw_metadata().exif.orientation` for the CR3 side
/// (NOT `RawImage.orientation`, which is a hard-coded stub — probe-proven). Self-skips without
/// the asset folder. Landscape-shot bodies write Orientation = 1 → 0 turns (HWU_7781).
#[test]
fn orientation_from_probe_pair() {
    let Some(shots) = shots_or_skip() else { return };
    let Some(portrait) = shots.iter().find(|s| s.name == "HWU_0141") else {
        eprintln!("skip: HWU_0141 not in the test set");
        return;
    };
    // JPG side (kamadak): the finished image the fast/detail/thumb tiers decode.
    assert_eq!(read_orientation(portrait, false), Some(6), "HWU_0141.JPG carries Orientation=6");
    // RAW side (rawler metadata): what the RAW-develop tier displays under.
    assert!(portrait.has_raw, "probe shot should be a RAW+JPG pair");
    assert_eq!(read_orientation(portrait, true), Some(6), "HWU_0141.CR3 metadata carries Orientation=6");
    // Both map to 1 clockwise quarter-turn — and the pair AGREES (the per-tier bases coincide).
    assert_eq!(orientation_to_turns(6), 1);
    // A landscape shot from the same body reads upright on both paths.
    land_guard(&shots);
}

/// T-k (stage-2 apply): the REAL-FILE write guard. COPY HWU_0141.JPG to a temp dir, patch its
/// Orientation 6 → compose(6,1)=3 in place, verify via the normal reader, assert exactly the 2 value
/// bytes changed (rest-of-file hash invariant), then re-apply once (idempotent no-op) and delete the
/// copy. The original in the optional mixed-photo corpus is NEVER opened for writing.
#[test]
fn apply_patches_a_copy_of_hwu_0141_in_place() {
    let Some(shots) = shots_or_skip() else { return };
    let Some(pair) = shots.iter().find(|s| s.name == "HWU_0141") else {
        eprintln!("skip: HWU_0141 not in the test set");
        return;
    };
    let Some(src) = pair.jpg.as_ref() else {
        eprintln!("skip: HWU_0141 has no JPG side");
        return;
    };
    let dir = std::env::temp_dir().join(format!("falcon_tk_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("HWU_0141.JPG");
    std::fs::copy(src, &copy).expect("copy the real JPG aside");

    let before = std::fs::read(&copy).unwrap();
    let target = compose_exif_orientation(6, 1);
    assert_eq!(target, 3);
    assert_eq!(patch_jpeg_orientation(&copy, 6, target).unwrap(), JpegPatch::Patched);
    let after = std::fs::read(&copy).unwrap();
    assert_eq!(before.len(), after.len(), "in-place patch must not change file length");
    let diffs: Vec<usize> = (0..before.len()).filter(|&i| before[i] != after[i]).collect();
    // The patch touches ONLY the 2-byte Orientation SHORT value field. 6→3 in little-endian flips just
    // the low byte (06→03; the high byte stays 00), so 1 or 2 changed bytes, all within that 2-byte span.
    assert!(!diffs.is_empty() && diffs.len() <= 2, "at most the 2 orientation bytes changed, got {}", diffs.len());
    assert!(diffs[diffs.len() - 1] - diffs[0] <= 1, "all changed bytes lie within the 2-byte value field");

    // read back through the normal reader (build a Shot pointing at the copy).
    let copy_shot = Shot {
        id: 0,
        name: "HWU_0141".into(),
        has_raw: false,
        has_jpg: true,
        raw: None,
        jpg: Some(copy.clone()),
        kind: pair.kind,
        cloud_placeholder: false,
        sniffed: None,
    };
    assert_eq!(read_orientation(&copy_shot, false), Some(3), "read-back sees the patched orientation");

    // idempotent re-apply: same (expected, target) → no write, bytes identical.
    assert_eq!(patch_jpeg_orientation(&copy, 6, target).unwrap(), JpegPatch::AlreadyTarget);
    assert_eq!(std::fs::read(&copy).unwrap(), after, "re-apply wrote nothing");

    std::fs::remove_dir_all(&dir).ok();
}

/// Formats batch (v0.8.55): if the test folder happens to contain any JXL/BMP/GIF (the formats added
/// this round), decode each one's fast frame end-to-end. Self-skips when none are present — no committed
/// binary fixtures (the unit tests cover BMP/GIF via generated fixtures; JXL has no in-tree encoder, so
/// this gated real-file test is its only real-decode coverage — drop a `.jxl` into the folder to exercise
/// it). GIF also gets an animation-probe + streaming sanity check when present.
#[test]
fn new_formats_decode_when_present() {
    use falcon_decode::{decode_gif_animation, GifPlayback, GifStream, SrcKind};
    let Some(shots) = shots_or_skip() else { return };
    let mut tested = 0usize;
    for s in shots.iter().filter(|s| matches!(s.kind, SrcKind::Jxl | SrcKind::Bmp | SrcKind::Gif)) {
        let f = fast_frame(s, 2048).unwrap_or_else(|e| panic!("decode {} ({:?}): {e}", s.name, s.kind));
        assert!(is_jpeg(&f.bytes), "{} fast frame is a JPEG", s.name);
        let (w, h) = jpeg_dims(&f.bytes);
        assert!(w.max(h) <= 2048 && w.max(h) > 0, "{} sane dims {w}x{h}", s.name);
        // For a GIF, also exercise the animation probe + one streamed frame.
        if s.kind == SrcKind::Gif {
            if let Some(p) = s.jpg.as_ref() {
                let anim = decode_gif_animation(p).expect("gif animation probe");
                let (aw, ah) = match &anim {
                    GifPlayback::InMemory(a) => {
                        assert!(!a.frames.is_empty(), "{} has frames", s.name);
                        (a.width, a.height)
                    }
                    GifPlayback::Streamed { width, height, frames } => {
                        assert!(*frames > 0);
                        (*width, *height)
                    }
                };
                let mut stream = GifStream::open(p).expect("gif stream open");
                assert_eq!(stream.dimensions(), (aw, ah));
                let fr = stream.next_frame().expect("first streamed frame");
                assert_eq!(fr.rgba.len(), (aw as usize * ah as usize * 4), "full-canvas RGBA");
            }
        }
        tested += 1;
    }
    eprintln!("new_formats_decode_when_present: exercised {tested} JXL/BMP/GIF shot(s)");
}

/// Helper so `orientation_from_probe_pair` keeps its landscape assertion (kept as a fn to avoid a giant
/// single test body).
fn land_guard(shots: &[Shot]) {
    if let Some(land) = shots.iter().find(|s| s.name == "HWU_7781") {
        // F7: assert the reader actually RETURNED an orientation (Some) before mapping — the old
        // `unwrap_or(1)` made a reader-returns-None regression invisible (None → 1 → 0 turns is the
        // same result a genuine upright read produces, so a broken reader passed silently).
        let jo = read_orientation(land, false);
        assert!(jo.is_some(), "landscape probe shot JPG must READ Some(orientation), not None (reader regression)");
        let jt = orientation_to_turns(jo.unwrap());
        // RAW side only when the landscape shot is itself a RAW+JPG pair (else there's no RAW to read).
        let rt = if land.has_raw {
            let ro = read_orientation(land, true);
            assert!(ro.is_some(), "landscape probe shot RAW must READ Some(orientation), not None (reader regression)");
            orientation_to_turns(ro.unwrap())
        } else {
            0
        };
        assert_eq!((jt, rt), (0, 0), "landscape probe shot displays unrotated on both paths");
    }
}

/// v1.0.0-rc TAIL 3 (verifier R1) — **A PORTRAIT RAW WITH A PASSENGER MUST NOT DISPLAY SIDEWAYS.**
///
/// The verifier's blocking finding, on the owner's own photograph. `read_orientation` matched
/// `shot.jpg.as_deref()`, so a RAW carrying an undecodable same-stem sibling took the FINISHED arm,
/// found no EXIF in an AVIF, and returned `None` — which the renderer reads as "upright". The frame
/// displayed rotated 90°, and `export_web_run` WROTE it that way, because the export takes the same
/// reader. The fix is one term: the finished arm is for the picture, and a passenger is not the
/// picture, so `.filter(|_| shot.has_jpg)` sends it to the RAW arm the fn's own doc reserves for
/// exactly this shot.
///
/// This row is the VALUE half of the rule; `a_passenger_shot_is_a_raw_only_shot_for_every_picture_surface`
/// in the lib binary is the AGREEMENT half. It needs a genuinely portrait RAW, so it uses the
/// owner's `HWU_0141.CR3` (Orientation 6) and self-skips like every other row in this file.
///
/// RED-FIRST at `e0dc974`: `the passenger must not steal the RAW's orientation: left None right
/// Some(6)`.
#[test]
fn passenger_does_not_rotate_the_owners_portrait_raw() {
    let src = Path::new(fixture_paths::photos()).join("HWU_0141.CR3");
    if !fixture_paths::require_file(&src) {
        eprintln!("skip: portrait RAW missing ({})", src.display());
        return;
    }
    let dir = std::env::temp_dir().join("falcon_t3_portrait_passenger");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&src, dir.join("HWU_0141.CR3")).unwrap();

    // The RAW alone: the orientation the photograph actually has.
    let alone = scan_folder(&dir).expect("scan");
    let alone = alone.first().expect("one shot").clone();
    let want = read_orientation(&alone, false);
    assert_eq!(want, Some(6), "the owner's HWU_0141 is a portrait frame (EXIF Orientation 6)");

    // …and now with an undecodable same-stem sibling riding along.
    let mut avif = vec![0u8, 0, 0, 24];
    avif.extend_from_slice(b"ftypavif");
    avif.extend_from_slice(&[0, 0, 0, 0]);
    avif.extend_from_slice(b"mif1miaf");
    std::fs::write(dir.join("HWU_0141.jpg"), &avif).unwrap();
    let with = scan_folder(&dir).expect("scan");
    let with = with.iter().find(|s| s.has_raw).expect("the RAW's shot").clone();
    assert!(with.jpg.is_some(), "the passenger is still on the shot (visible, deletable)");
    // Open-time check (2 November): the scan goes by the name; the check below is what the readers run.
    assert!(!falcon_decode::shot_by_bytes(&with).has_jpg, "…and it is not the picture");
    assert_eq!(
        read_orientation(&with, false),
        want,
        "the passenger must not steal the RAW's orientation"
    );
    // The RAW-mode reader was always right; pinned so the fix cannot be "fixed" by breaking it.
    assert_eq!(read_orientation(&with, true), read_orientation(&alone, true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fix round 2 (11 November 2026, `CS-2`): **A RAW-ONLY CR3 SHOWS THE SAME INFO ROWS AS ITS JPEG.**
/// For each real CR3+JPEG pair in the approved photo corpus (both files on this device; a cloud-only
/// file is skipped, never downloaded), the CR3 seen alone gives exactly the JPEG's rows, keys and
/// values character for character. Only `Files` may differ (it names the files present).
///
/// FALSIFIER (L28): drop the `raw_exif_from_bytes` fallback in `exif_rows` and the RAW-only rows are
/// empty, so every pair fails; drop the CMT2 re-tagging and every exposure row goes missing.
#[test]
fn a_raw_only_cr3_shows_the_same_info_rows_as_its_jpeg() {
    let Some(shots) = shots_or_skip() else { return };
    let is_cr3 = |p: &Option<std::path::PathBuf>| {
        p.as_deref().and_then(|p| p.extension()).is_some_and(|e| e.eq_ignore_ascii_case("cr3"))
    };
    let pairs: Vec<&Shot> = shots
        .iter()
        .filter(|s| s.has_raw && s.has_jpg && is_cr3(&s.raw))
        .filter(|s| fixture_paths::local_file(s.raw.as_deref().unwrap()) && fixture_paths::local_file(s.jpg.as_deref().unwrap()))
        .take(12)
        .collect();
    if !fixture_paths::require_available(!pairs.is_empty(), "a local CR3+JPEG pair in the photo corpus") {
        return;
    }
    for s in &pairs {
        let jpeg_rows: Vec<(String, String)> = exif_rows(s, 0).into_iter().filter(|(k, _)| k != "Files").collect();
        let alone = Shot { has_jpg: false, jpg: None, ..(*s).clone() };
        let raw_rows: Vec<(String, String)> = exif_rows(&alone, 0).into_iter().filter(|(k, _)| k != "Files").collect();
        // Keys only: values such as Artist and GPS are personal and stay out of logs.
        let keys: Vec<&str> = raw_rows.iter().map(|(k, _)| k.as_str()).collect();
        println!("{}: {} rows from the JPEG, {} from the CR3 alone: {keys:?}", s.name, jpeg_rows.len(), raw_rows.len());
        assert!(jpeg_rows.len() >= 8, "{}: premise, the JPEG has a full panel", s.name);
        assert_eq!(raw_rows, jpeg_rows, "{}: the CR3 alone must show the JPEG's rows", s.name);
        assert!(exif_rows(&alone, 0).iter().any(|(k, _)| k == "Files"), "{}: and its Files row", s.name);
    }
    println!("{} CR3 pair(s) compared", pairs.len());
}

/// Fix round 2 (`CS-2`): a real Fujifilm RAF from the approved other-brands corpus, seen alone,
/// shows the camera, exposure and date rows, read from the JPEG the RAF carries.
#[test]
fn a_raw_only_raf_shows_its_camera_and_exposure_rows() {
    let path = fixture_paths::other_raw().join("DSCF1276.RAF");
    if !fixture_paths::require_file(&path) {
        eprintln!("SKIP real RAF fixture: {} is absent or not on this device", path.display());
        return;
    }
    let shot = Shot { id: 0, name: "DSCF1276".into(), has_raw: true, has_jpg: false, raw: Some(path),
        jpg: None, kind: SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
    let rows = exif_rows(&shot, 0);
    let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
    println!("RAF rows: {keys:?}"); // keys only: values such as Artist and GPS are personal
    for key in ["Camera", "Shutter", "Aperture", "ISO", "Date", "Files"] {
        assert!(rows.iter().any(|(k, v)| k == key && !v.is_empty()), "the RAF's {key} row");
    }
    let camera = rows.iter().find(|(k, _)| k == "Camera").map(|(_, v)| v.as_str()).unwrap_or("");
    assert!(camera.to_ascii_uppercase().contains("FUJIFILM"), "the camera is the Fujifilm's own make: {camera}");
}
