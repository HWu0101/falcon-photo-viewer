//! v0.8.146 (E3-M1) — **THE GATE**: real tiles, real driver, byte-compared against two independent
//! reference decoders.
//!
//! # What "three-way" means here, and why the pins are digests
//!
//! For each pinned row the NV12 came back byte-identical from THREE decoders:
//!
//!   a. **NVDEC** (`cuvid`), via Stage 0's SP1 probe with `--dump`;
//!   b. **libavcodec's software HEVC decoder**, `ffmpeg -hwaccel none … -f rawvideo -pix_fmt
//!      yuv420p`, with the U and V planes interleaved to NV12 — provenance VERIFIED rather than
//!      assumed: `nvidia-smi` reported decoder-engine utilisation 0 % across eight samples during a
//!      sustained 60-iteration decode loop, which is the check Stage 0's ffmpeg-mirage lesson
//!      demands (its `-hwaccel d3d11va` enumerated zero GUIDs and silently fell back to software
//!      while reporting a plausible frame count);
//!   c. **this crate**, through `ID3D11VideoDevice` with the marshalling in `falcon_hwdec::dxva`.
//!
//! (a) and (b) agreed on 0 of 1,376,256 bytes differing for all eight rows before (c) existed, so
//! the digests below are a fact about the FILES, not about Falcon. (c) is then asserted against
//! them here. What is pinned is the SHA-256 of the packed NV12 rather than the 1.3 MB buffer
//! itself: the digest is reproducible by anyone with the fixture, `ffmpeg` and `sha256sum` with no
//! Falcon code in the loop, and it keeps 11 MB of decoded pixels out of the repository.
//!
//! # The skip discipline
//!
//! These rows need a hardware video device. Where there is none they SKIP WITH A NAMED REASON, as
//! `tests/heic.rs` and `heif_grid_parity.rs` already do for the OS HEVC codec — and they are NOT
//! `cfg`'d out, because a test that cannot run is not a gate. On the box this round was executed on
//! (RTX 5080, driver 610.88) every row RAN.

#![cfg(windows)]

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

#[path = "../../test-support/fixture_paths.rs"]
mod fixture_paths;

// ── v0.8.149 (F9/B11): THE SKIP AUDIT ────────────────────────────────────────────────────────────
//
// Every row in this file can decline to run, and the E6 audit's point is that a suite which prints
// `test result: ok` while a third of it declined is not reporting what it did. The reason was
// already named at every site; what was missing is that nothing COUNTED them, so a green run on a
// box without the corpus and a green run that actually gated the epic were indistinguishable at a
// glance. `skip!` numbers each one and points at the inventory in TESTING.md §8h, and
// `the_skip_audit_states_what_this_machine_could_not_run` prints the total.
static SKIPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

macro_rules! skip {
    ($($t:tt)*) => {{
        let n = SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        eprintln!(
            "SKIP #{n} (this row needs hardware and/or the HEIC corpus — TESTING.md \u{a7}8h): {}",
            format!($($t)*)
        );
    }};
}

use std::path::PathBuf;

use falcon_decode::yuv_kernel::sha256_hex;
use falcon_hwdec::dxva::QmatrixPolicy;
use falcon_hwdec::{tile_source, DecodeDevice, DecodeSession, HwDecError, TileSource};

fn corpus_dirs() -> [&'static std::path::Path; 2] {
    [fixture_paths::heic(), fixture_paths::photos()]
}

fn find(name: &str) -> Option<PathBuf> {
    let found = corpus_dirs().iter().map(|d| PathBuf::from(d).join(name)).find(|p| fixture_paths::local_file(p));
    fixture_paths::require_available(found.is_some(), name);
    found
}

/// True when this box has a D3D11 video device that decodes HEVC Main to NV12. Anything else is a
/// named skip, printed once per row so a green suite on a codec-less machine still SAYS so.
fn hardware() -> bool {
    match DecodeDevice::new() {
        Ok(d) => d.supports_hevc_main_nv12(),
        Err(_) => false,
    }
}

/// One pinned reference tile: `(file, tile index in E1's dimg raster order, SHA-256 of the packed
/// NV12)`. Measured 2026-08-05 on the owner's box; see the module header for the provenance of
/// each digest.
const REFERENCE_TILES: &[(&str, usize, &str)] = &[
    // IMG_1826 — iOS 26.3, 34-byte SPS, DEFAULT scaling lists.
    ("IMG_1826.HEIC", 0, "552b0189856789caf3b91d69cd538798a6a53d65750eca382e841384d436160e"),
    ("IMG_1826.HEIC", 22, "65328e144d14a309f10ca94d8e7e26d1fbb2522b49318348ed230404d004419c"),
    ("IMG_1826.HEIC", 40, "d10e094ca1886d7a34cb19860add5543f76b631791770ed8c896edc1de683191"),
    ("IMG_1826.HEIC", 53, "ca8db8f9c72c9d77dd4446fc2c05f9c55f28057eb882c4d3fdca22e1f41177ef"),
    // IMG_3258 — iOS 27.0, 501-byte SPS, 20 EXPLICIT scaling-list matrices.
    ("IMG_3258.HEIC", 0, "b5e202b77a8f0e893c83643e62df7370cf079bd84e6c40ca33afba987155299a"),
    ("IMG_3258.HEIC", 22, "293d4efec943e2e7aa69e721e83cd1f92326bad62bc114c7cd03eb17ce9fb260"),
    ("IMG_3258.HEIC", 40, "e26efe5e180ab5ad06ac25b7bf52ee20b14c43c0bf9a28d6f29eed6a89c7720b"),
    ("IMG_3258.HEIC", 53, "d41c142c3bcddd2ba40979f15e0778281ac6599c61cde5eaf5a6195808fc1fc6"),
];

fn session_for(src: &TileSource) -> Result<DecodeSession, HwDecError> {
    DecodeSession::new(src.tile_w, src.tile_h, 8)
}

#[test]
fn the_marshalling_reproduces_two_independent_decoders_byte_for_byte() {
    if !hw_child::run_here(module_path!(), "the_marshalling_reproduces_two_independent_decoders_byte_for_byte") { return; }
    // Public source contains no private camera corpus. An existing corpus still has to
    // satisfy every original pixel/hash/count assertion below.
    if corpus_dirs().iter().all(|dir| !dir.is_dir()) {
        skip!("no private HEIC corpus directories; set FALCON_HEIC_TESTKIT or FALCON_PHOTO_TEST_DIR");
        return;
    }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let mut checked = 0;
    for file in ["IMG_1826.HEIC", "IMG_3258.HEIC"] {
        let Some(path) = find(file) else {
            skip!("{file}: not in the corpus on this machine");
            continue;
        };
        let src = tile_source(&path).expect("E1 grid + hvcC");
        let mut s = session_for(&src).expect("a decode session");
        for (f, tile, want) in REFERENCE_TILES.iter().filter(|(f, _, _)| *f == file) {
            let img = s
                .decode_tile(&src.params, &src.tiles[*tile])
                .unwrap_or_else(|e| panic!("{f} tile {tile}: {e}"));
            assert_eq!(img.width, src.tile_w);
            assert_eq!(img.height, src.tile_h);
            let packed = img.packed();
            assert_eq!(
                packed.len(),
                (src.tile_w * src.tile_h + src.tile_w * src.tile_h.div_ceil(2)) as usize
            );
            let got = sha256_hex(&packed);
            assert_eq!(&got, want, "{f} tile {tile}: D3D11VA disagrees with NVDEC and libavcodec");
            checked += 1;
        }
    }
    assert_eq!(checked, REFERENCE_TILES.len(), "every pinned row must have run");
}

#[test]
fn the_quantisation_matrices_are_load_bearing_on_both_files() {
    if !hw_child::run_here(module_path!(), "the_quantisation_matrices_are_load_bearing_on_both_files") { return; }
    // THE FALSIFIER the milestone asked for, both halves of the matched pair. Zeroing the matrices
    // must change the picture — on IMG_3258 because its 20 explicit matrices go missing, and on
    // IMG_1826 because "no explicit lists" still means the H.265 DEFAULTS, not flat 16s. If either
    // row ever comes back IDENTICAL, the Qmatrix buffer is not reaching the driver at all and the
    // correctness above is an accident of this driver's fallbacks.
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    for file in ["IMG_1826.HEIC", "IMG_3258.HEIC"] {
        let Some(path) = find(file) else {
            skip!("{file}: not in the corpus on this machine");
            continue;
        };
        let src = tile_source(&path).expect("E1 grid + hvcC");
        let mut s = session_for(&src).expect("a decode session");
        for tile in [0usize, 22, 40] {
            let good = s.decode_tile(&src.params, &src.tiles[tile]).expect("the file's matrices");
            let zeroed = s
                .decode_tile_with(&src.params, &src.tiles[tile], QmatrixPolicy::Zeroed)
                .expect("zeroed matrices still decode SOMETHING");
            let (a, b) = (good.packed(), zeroed.packed());
            assert_ne!(a, b, "{file} tile {tile}: zeroing the matrices changed nothing");
            let diff = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            eprintln!(
                "{file} tile {tile}: zeroed matrices move {diff} of {} bytes ({:.1}%)",
                a.len(),
                100.0 * diff as f64 / a.len() as f64
            );
            // …and restoring the real matrices restores the reference bytes exactly.
            let restored = s.decode_tile(&src.params, &src.tiles[tile]).expect("restored");
            assert_eq!(restored.packed(), a, "{file} tile {tile}: the falsifier left residue");
        }
    }
}

#[test]
fn one_session_decodes_every_tile_of_a_real_photo() {
    if !hw_child::run_here(module_path!(), "one_session_decodes_every_tile_of_a_real_photo") { return; }
    // Plan non-negotiable #1 in miniature: all 54 tiles through ONE decoder, surfaces recycled. The
    // interesting failure is not "a tile is wrong" but "tile 9 fails once the 8 surfaces have gone
    // round once" — Stage 0's obstacle #4, and the reason this crate owns surface lifetime.
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let Some(path) = find("IMG_1826.HEIC") else {
        skip!("IMG_1826.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(&path).expect("E1 grid + hvcC");
    assert_eq!(src.tiles.len(), 54);
    let mut s = DecodeSession::new(src.tile_w, src.tile_h, 4).expect("a 4-surface session");
    let mut digests = Vec::with_capacity(src.tiles.len());
    for (k, item) in src.tiles.iter().enumerate() {
        let img = s.decode_tile(&src.params, item).unwrap_or_else(|e| panic!("tile {k}: {e}"));
        digests.push(sha256_hex(&img.packed()));
    }
    // 54 distinct pictures: a surface that was still being written when the next decode began, or a
    // stale readback, would show up as a repeat.
    let mut sorted = digests.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), digests.len(), "two tiles decoded to the same bytes");
    // The pinned rows must still hold when they arrive as part of a long run rather than alone.
    for (f, tile, want) in REFERENCE_TILES.iter().filter(|(f, _, _)| *f == "IMG_1826.HEIC") {
        assert_eq!(&digests[*tile], want, "{f} tile {tile} inside the full-photo run");
    }
}

#[test]
fn the_pipelined_path_gives_the_same_bytes_as_the_serialised_one() {
    if !hw_child::run_here(module_path!(), "the_pipelined_path_gives_the_same_bytes_as_the_serialised_one") { return; }
    // The product shape (plan non-negotiable #1) submits a run of pictures before reading any back.
    // That changes WHEN surfaces are recycled and nothing else, so the bytes must be identical —
    // and if they are not, some surface is being read while it is still being written.
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let Some(path) = find("IMG_3258.HEIC") else {
        skip!("IMG_3258.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(&path).expect("E1 grid + hvcC");
    let mut s = session_for(&src).expect("a decode session");
    let one_at_a_time: Vec<String> = src
        .tiles
        .iter()
        .map(|t| sha256_hex(&s.decode_tile(&src.params, t).expect("serialised").packed()))
        .collect();
    let batched: Vec<String> = s
        .decode_tiles(&src.params, &src.tiles)
        .expect("pipelined")
        .iter()
        .map(|i| sha256_hex(&i.packed()))
        .collect();
    assert_eq!(batched.len(), src.tiles.len());
    assert_eq!(one_at_a_time, batched, "pipelining changed the pixels");
    for (f, tile, want) in REFERENCE_TILES.iter().filter(|(f, _, _)| *f == "IMG_3258.HEIC") {
        assert_eq!(&batched[*tile], want, "{f} tile {tile} through the pipelined path");
    }
}

#[test]
fn garbled_bitstreams_fail_closed_and_leave_the_device_usable() {
    if !hw_child::run_here(module_path!(), "garbled_bitstreams_fail_closed_and_leave_the_device_usable") { return; }
    // 500 single-bit flips into a real tile's payload plus 100 truncations, all submitted to the
    // driver. The bar is not that they fail — most flips decode SOMETHING, which is the honest
    // behaviour of an entropy decoder handed slightly wrong bits — but that the PROCESS survives,
    // no call panics, and the device still produces the right bytes afterwards. A hardware path one
    // corrupt file can wedge would take the whole app with it.
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let Some(path) = find("IMG_1826.HEIC") else {
        skip!("IMG_1826.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(&path).expect("E1 grid + hvcC");
    let mut s = session_for(&src).expect("a decode session");
    let base = &src.tiles[0];
    let before = sha256_hex(&s.decode_tile(&src.params, base).expect("the clean tile").packed());

    let mut state = 0x2026_0805u64;
    let (mut decoded, mut declined) = (0usize, 0usize);
    for _ in 0..500 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut m = base.clone();
        let at = (state >> 33) as usize % m.len();
        m[at] ^= 1 << ((state >> 17) & 7);
        match s.decode_tile(&src.params, &m) {
            Ok(_) => decoded += 1,
            Err(_) => declined += 1,
        }
    }
    eprintln!("hostile run A — 500 byte flips: {decoded} decoded, {declined} declined, 0 panicked");
    assert_eq!(decoded + declined, 500);

    // Truncation is the other shape a damaged file takes, and it exercises the length checks rather
    // than the entropy decoder. Every one of these should be refused BEFORE the driver sees it.
    let (mut t_ok, mut t_err) = (0usize, 0usize);
    for _ in 0..100 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut m = base.clone();
        m.truncate(((state >> 33) as usize % m.len()).max(1));
        match s.decode_tile(&src.params, &m) {
            Ok(_) => t_ok += 1,
            Err(_) => t_err += 1,
        }
    }
    eprintln!("hostile run B — 100 truncations: {t_ok} decoded, {t_err} declined, 0 panicked");
    assert_eq!(t_err, 100, "a truncated item must never reach the driver");

    let after = sha256_hex(&s.decode_tile(&src.params, base).expect("the device survived").packed());
    assert_eq!(before, after, "the device stopped producing the right bytes after the hostile run");
}

#[test]
fn a_session_refuses_geometry_that_is_not_its_own() {
    if !hw_child::run_here(module_path!(), "a_session_refuses_geometry_that_is_not_its_own") { return; }
    // The 24 MP file's tiles are 640x896; a session built for 896x1024 must decline them rather
    // than decode a picture into a surface of the wrong shape.
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let (Some(big), Some(small)) = (find("IMG_1826.HEIC"), find("IMG_1827.HEIC")) else {
        skip!("the two-geometry pair is not both present on this machine");
        return;
    };
    let a = tile_source(&big).expect("48 MP grid");
    let b = tile_source(&small).expect("24 MP grid");
    assert_ne!((a.tile_w, a.tile_h), (b.tile_w, b.tile_h));
    let mut s = session_for(&a).expect("a decode session");
    assert!(matches!(
        s.decode_tile(&b.params, &b.tiles[0]),
        Err(HwDecError::Unsupported("tile geometry differs from the session's"))
    ));
    // …and the session still works for what it WAS built for.
    assert!(s.decode_tile(&a.params, &a.tiles[0]).is_ok());
}

#[test]
fn the_driver_answers_the_capability_question_directly() {
    if !hw_child::run_here(module_path!(), "the_driver_answers_the_capability_question_directly") { return; }
    // Stage 0's methodology lesson, kept as a test: ask `ID3D11VideoDevice`, never ffmpeg. On a box
    // with no video device this row reports that and returns — it does not fail.
    match DecodeDevice::new() {
        Ok(d) => {
            let n = d.profile_count();
            eprintln!(
                "decoder profiles advertised: {n};  HEVC_VLD_MAIN + NV12: {}",
                d.supports_hevc_main_nv12()
            );
            assert!(n > 0, "a video device that advertises no profiles at all");
        }
        Err(e) => skip!("no video device ({e})"),
    }
}
