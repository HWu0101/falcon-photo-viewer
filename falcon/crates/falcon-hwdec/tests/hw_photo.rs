//! v0.8.147 (E3 milestone 2) — **THE GATE**: whole real photos, real driver, real GPU, measured
//! against the path that ships.
//!
//! # What is a gate here and what is a report
//!
//! The plan's non-negotiable #4 makes one thing binary and one thing not. The DIMS are binary:
//! "dims byte-identical to `finish_source` per `scale_to`", and the only honest way to test that is
//! to ask the shipping WIC lane what it answers for the same file at the same tier and require the
//! same pair — which is what [`the_dims_are_the_shipping_paths_on_every_file_and_tier`] does, on
//! every corpus HEIC × every `scale_to` the browse lanes actually request. The PIXELS are not
//! binary: HEVC decode is bit-exact by spec, so the residual against WIC is entirely the two
//! converters' chroma reconstruction disagreeing, and pinning a tolerance on somebody else's
//! filter would be pinning their build. What IS tested about the pixels is the thing a tolerance
//! would hide: that the residual is FLAT ACROSS THE GRID. A compositor that places a tile wrong
//! produces a picture whose per-tile error is enormous in one cell and normal everywhere else, and
//! `the_grid_has_no_seam` is that statistic — with the falsifier beside it proving it bites.
//!
//! # The skip discipline
//!
//! These rows need a D3D11 video device, a GPU adapter, AND the corpus. Any of the three missing is
//! a SKIP WITH A NAMED REASON printed on the row, never a `cfg`-out — a test that cannot run is not
//! a gate. On the box this round was executed on (RTX 5080, driver 610.88) every row RAN.

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

use falcon_decode::{browse_frame_rgba, scan_folder, Lane, Shot};
use falcon_gpu::heic::FinishOut; // v0.8.167: the managed output contract, for the `decode_with_out` row
use falcon_hwdec::{
    tile_source, AssemblyFault, DecodeDevice, PhotoDecoder, PhotoRun, Submission, TileSource,
};

fn corpus_dirs() -> [&'static std::path::Path; 2] {
    [fixture_paths::heic(), fixture_paths::photos()]
}

/// Every corpus HEIC, deduplicated by NAME (the two directories overlap).
fn corpus() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for d in corpus_dirs() {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("heic"))
                && !out.iter().any(|q| q.file_name() == p.file_name())
            {
                out.push(p);
            }
        }
    }
    out.sort_by_key(|p| p.file_name().map(|n| n.to_owned()));
    out
}

/// The shipping lane's `Shot` for a path, built by the shipping SCANNER — so the baseline is the
/// real lane and not a re-implementation of it.
fn shot_for(path: &std::path::Path) -> Option<Shot> {
    let name = path.file_name()?;
    for d in corpus_dirs() {
        let Ok(shots) = scan_folder(std::path::Path::new(d)) else { continue };
        if let Some(s) =
            shots.into_iter().find(|s| s.jpg.as_deref().and_then(|p| p.file_name()) == Some(name))
        {
            return Some(s);
        }
    }
    None
}

fn hardware() -> bool {
    match DecodeDevice::new() {
        Ok(d) => d.supports_hevc_main_nv12(),
        Err(_) => false,
    }
}

/// **One photo pipeline at a time.**
///
/// `cargo test` runs a binary's rows on as many threads as the box has cores, and every row here
/// wants a wgpu device, a D3D11 decoder session, ~600 MB of VRAM buffers AND a 48 MP WIC decode
/// (146 MB of RGB, then 195 MB of RGBA) for its baseline. Eight of those at once is not a faster
/// suite; the first attempt at this file ran for twenty-two minutes and had burned 21 CPU-seconds
/// doing it — the box was thrashing, not computing. Serialised the whole file runs in ~50 s.
///
/// The guard is taken AFTER the skip checks so a machine without the hardware still reports its
/// reason on every row rather than queueing behind one.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A failed row poisons the lock; the rows that follow are still worth running, and their own
    // assertions are what judge them.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 28 October (round 2b review P2): **each hardware row's first line.** The row runs in its own
/// process with a time limit (`hw_child`), still one at a time: the parent holds the lock above
/// around each child, so no two photo pipelines overlap. The skips a child printed count toward
/// this file's audit (`SKIPPED` lives in the parent).
fn run_here(row: &str) -> bool {
    if hw_child::in_child() {
        return true;
    }
    let err = {
        let _serial = one_at_a_time();
        hw_child::run_in_child(module_path!(), row)
    };
    SKIPPED.fetch_add(err.matches("SKIP #").count(), std::sync::atomic::Ordering::Relaxed);
    false
}

/// The `scale_to` values the browse lanes really ask for, via `fast_decode_target(max_dim,
/// supersample)`: the detail tier's cap, the fast tier at both sampling modes, the thumb tier, and
/// `None` for the ROI-source full decode. Not invented — read off `browse_frame_rgba`'s callers.
const TIERS: &[(&str, Option<u32>)] = &[
    ("native", None),
    ("detail 8192", Some(8192)),
    ("fast supersample 2880", Some(2880)),
    ("fast subsample 1440", Some(1440)),
    ("thumb 256", Some(256)),
];

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; rgba.len() / 4 * 3];
    for (d, s) in out.chunks_exact_mut(3).zip(rgba.chunks_exact(4)) {
        d.copy_from_slice(&s[..3]);
    }
    out
}

/// **THE DIMS GATE.** Every corpus file × every tier the shipping path requests.
#[test]
fn the_dims_are_the_shipping_paths_on_every_file_and_tier() {
    if !run_here("the_dims_are_the_shipping_paths_on_every_file_and_tier") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let files = corpus();
    if files.is_empty() {
        skip!("no HEIC corpus on this machine");
        return;
    }
    let mut checked = 0usize;
    let mut bad = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Ok(src) = tile_source(path) else {
            skip!("{name}: E1 declined the container");
            continue;
        };
        let Some(shot) = shot_for(path) else {
            skip!("{name}: the shipping scanner did not classify it");
            continue;
        };
        let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
        for (label, scale) in TIERS {
            let hw = dec.decode(&src, *scale).unwrap_or_else(|e| panic!("{name} {label}: {e}"));
            // `None` is the full-res ROI source; the shipping lane expresses that as an ask no
            // real photo can exceed, which is what `TEX_SAFE_LONG` is.
            let ask = scale.unwrap_or(falcon_decode::TEX_SAFE_LONG);
            let wic = browse_frame_rgba(&shot, ask, true, Lane::Native)
                .unwrap_or_else(|e| panic!("{name} {label}: the WIC baseline failed: {e}"));
            // Two statements, because they can fail separately: the answer must match the SHIPPING
            // path, and the answer this crate promises WITHOUT decoding must match what it then
            // produces. A router that sized a cache entry off the second would otherwise be able
            // to disagree with the pixels it filed there.
            let promised = falcon_hwdec::photo::output_dims(&src, *scale).expect("promised dims");
            if (hw.w, hw.h) != (wic.w, wic.h) || promised != (hw.w, hw.h) {
                bad.push(format!(
                    "  {name} {label}: hw {}x{} / promised {}x{} / wic {}x{}",
                    hw.w, hw.h, promised.0, promised.1, wic.w, wic.h
                ));
            }
            assert_eq!(hw.rgb.len(), (hw.w as usize) * (hw.h as usize) * 3, "{name} {label}: the contract is 24 bpp at stride w*3");
            checked += 1;
        }
        eprintln!("{name}: {} tiers checked", TIERS.len());
    }
    assert!(bad.is_empty(), "dims disagree on {} row(s):\n{}", bad.len(), bad.join("\n"));
    assert!(checked >= TIERS.len(), "the gate ran on nothing");
    eprintln!("dims gate: {checked} file x tier rows, all identical to the shipping path");
}

/// **THE FALSIFIER for the dims gate.** One perturbed scale computation must redden it.
#[test]
fn the_dims_gate_bites() {
    if !run_here("the_dims_gate_bites") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let Some(path) = corpus().into_iter().find(|p| p.file_name().unwrap() == "IMG_1826.HEIC")
    else {
        skip!("IMG_1826.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(&path).expect("E1 grid");
    let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
    for scale in [None, Some(2880u32)] {
        let good = dec.decode(&src, scale).expect("the honest answer");
        let bad = dec
            .decode_with(&src, scale, Submission::Pipelined, AssemblyFault::ScaleOffByOne)
            .expect("a one-pixel-narrower answer still decodes");
        assert_ne!(
            (good.w, good.h),
            (bad.w, bad.h),
            "a perturbed scale produced the SAME dims — the gate is comparing nothing"
        );
        assert_eq!(bad.w, good.w - 1);
        // …and restoring gives the reference dims back, so the falsifier left no residue.
        let restored = dec.decode(&src, scale).expect("restored");
        assert_eq!((restored.w, restored.h), (good.w, good.h));
        assert_eq!(restored.rgb, good.rgb, "the falsifier left residue in the pixels");
    }
}

/// Mean |delta| per GRID CELL, in display space: the seam statistic.
///
/// Sampled every third pixel on both axes. Three rather than two because two is the chroma lattice
/// and a stride of two would only ever look at one chroma phase; three visits all of them. A whole
/// tile out of place is ~90 000 sampled pixels of a 48 MP photo's cell either way — the subsample
/// costs the statistic nothing and buys the suite a 9× shorter row.
const SAMPLE: u32 = 3;

fn cell_means(src: &TileSource, a: &[u8], b: &[u8], w: u32, h: u32) -> Vec<f64> {
    let map = falcon_hwdec::photo::DisplayMap::new(src).expect("the display map");
    let cells = (src.rows * src.cols) as usize;
    let mut sum = vec![0u64; cells];
    let mut n = vec![0u64; cells];
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let k = map.cell_at(x, y);
            let o = ((y as usize) * (w as usize) + x as usize) * 3;
            let d = (0..3)
                .map(|i| (a[o + i] as i32 - b[o + i] as i32).unsigned_abs() as u64)
                .max()
                .unwrap_or(0);
            if k < cells {
                sum[k] += d;
                n[k] += 1;
            }
            x += SAMPLE;
        }
        y += SAMPLE;
    }
    sum.iter().zip(&n).map(|(s, c)| *s as f64 / (*c).max(1) as f64).collect()
}

fn spread(v: &[f64]) -> (f64, f64, f64) {
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[0], s[s.len() / 2], s[s.len() - 1])
}

/// **THE SEAM HUNT.** The residual against WIC, per grid cell, must be FLAT.
///
/// The whole-image mean cannot see a misplaced tile: one bad cell in 54 moves it by 2%. The
/// per-cell spread can, and this asserts the spread rather than the level — the level is somebody
/// else's chroma filter and is reported, not gated.
#[test]
fn the_grid_has_no_seam() {
    if !run_here("the_grid_has_no_seam") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let files = corpus();
    if files.is_empty() {
        skip!("no HEIC corpus on this machine");
        return;
    }
    let mut ran = 0usize;
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Ok(src) = tile_source(path) else { continue };
        let Some(shot) = shot_for(path) else { continue };
        let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
        let hw = dec.decode(&src, None).expect("the native photo");
        let w = browse_frame_rgba(&shot, falcon_decode::TEX_SAFE_LONG, true, Lane::Native)
            .expect("the WIC baseline");
        assert_eq!((hw.w, hw.h), (w.w, w.h));
        let wic = rgba_to_rgb(&w.rgba);
        let cells = cell_means(&src, &hw.rgb, &wic, hw.w, hw.h);
        let (lo, med, hi) = spread(&cells);
        eprintln!(
            "{name:<22} {} cells — per-cell mean |d| min {lo:.4} median {med:.4} max {hi:.4}  (max/median {:.2}x)",
            cells.len(),
            hi / med.max(1e-9)
        );
        // 3× is generous next to what a placement error does — the falsifier below moves ONE cell
        // to 16.6 against a median of 0.0 — and tight next to what the corpus actually produces:
        // the widest spread measured on 2026-08-05 is 2.13× ([12MP]IMG_2707, whose grid mixes a
        // flat sky cell at 0.000 with detailed ones at 1.937). The absolute floor is the second
        // arm because a ratio against a near-zero median says nothing: a cell whose whole residual
        // is under half an LSB is not a seam whatever it divides into.
        assert!(
            hi <= 0.5 || hi <= 3.0 * med.max(1e-9),
            "{name}: one grid cell's residual is {hi:.4} against a median of {med:.4} ({:.2}x) — that is a seam, not a filter",
            hi / med.max(1e-9)
        );
        ran += 1;
    }
    assert!(ran > 0, "the seam hunt ran on nothing");
}

/// **THE FALSIFIER for the seam hunt.** A two-pixel tile displacement must break the flatness.
#[test]
fn the_seam_hunt_bites() {
    if !run_here("the_seam_hunt_bites") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let Some(path) = corpus().into_iter().find(|p| p.file_name().unwrap() == "IMG_1826.HEIC")
    else {
        skip!("IMG_1826.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(&path).expect("E1 grid");
    let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
    let good = dec.decode(&src, None).expect("the honest photo");
    let bad = dec
        .decode_with(&src, None, Submission::Pipelined, AssemblyFault::TileOffset)
        .expect("a displaced tile still assembles");
    assert_eq!((good.w, good.h), (bad.w, bad.h), "the falsifier must move pixels, not dims");
    let cells = cell_means(&src, &good.rgb, &bad.rgb, good.w, good.h);
    let (lo, med, hi) = spread(&cells);
    eprintln!(
        "seam falsifier: per-cell mean |d| min {lo:.4} median {med:.4} max {hi:.4} over {} cells",
        cells.len()
    );
    // Exactly ONE cell moved, so the median is 0 and the max is large. Both halves are asserted:
    // "one cell is loud" AND "the others are silent" — a falsifier that reddened every cell would
    // prove the statistic reacts to something, not that it localises.
    assert!(hi > 1.0, "displacing a tile by two pixels moved the worst cell by only {hi:.4}");
    assert!(med < 1e-9, "the displacement leaked outside its own tile (median {med:.6})");
    // …and restoring gives the reference bytes back.
    let restored = dec.decode(&src, None).expect("restored");
    assert_eq!(restored.rgb, good.rgb, "the seam falsifier left residue");
}

/// **ROTATION**, on the one corpus file that has any: portrait dims AND the right content.
///
/// The dims half is cheap and the content half is the one that matters — a picture turned the
/// wrong way has exactly the right dims. So the reference is WIC's own rotated output, and the
/// falsifier turns it the other way and requires the comparison to collapse.
#[test]
fn the_rotated_file_lands_upright_and_the_reverse_turn_does_not() {
    if !run_here("the_rotated_file_lands_upright_and_the_reverse_turn_does_not") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let Some(path) = corpus().into_iter().find(|p| p.file_name().unwrap() == "IMG_2814.HEIC")
    else {
        skip!("IMG_2814.HEIC (the irot=270 file) is not in the corpus on this machine");
        return;
    };
    let src = tile_source(&path).expect("E1 grid");
    assert_eq!(src.irot, 270, "this row exists for the rotated file");
    assert_eq!(src.crop.w, 8064);
    assert_eq!(src.crop.h, 6048);
    assert_eq!(src.display, (6048, 8064), "E1 already says the display is portrait");
    let shot = shot_for(&path).expect("the shipping scanner classifies IMG_2814");
    let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");

    // A tier size, so the comparison is a few MB rather than 146.
    let good = dec.decode(&src, Some(2880)).expect("the rotated photo");
    assert!(good.h > good.w, "the rotated photo must be PORTRAIT, got {}x{}", good.w, good.h);
    let w = browse_frame_rgba(&shot, 2880, true, Lane::Native).expect("the WIC baseline");
    assert_eq!((good.w, good.h), (w.w, w.h));
    let wic = rgba_to_rgb(&w.rgba);
    let mean = |a: &[u8], b: &[u8]| {
        a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum::<u64>()
            as f64
            / a.len() as f64
    };
    let upright = mean(&good.rgb, &wic);

    let reversed = dec
        .decode_with(&src, Some(2880), Submission::Pipelined, AssemblyFault::ReverseRotation)
        .expect("the wrong-way turn still assembles");
    assert_eq!(
        (reversed.w, reversed.h),
        (good.w, good.h),
        "the reverse turn must keep the dims — that is why the dims gate cannot see it"
    );
    let wrong = mean(&reversed.rgb, &wic);
    eprintln!(
        "IMG_2814 rotation: mean |d| vs WIC — 270 CCW as the container says {upright:.3}, turned the other way {wrong:.3} ({:.0}x)",
        wrong / upright.max(1e-9)
    );
    assert!(
        wrong > 10.0 * upright,
        "turning the picture the wrong way changed the residual by only {:.2}x — the content check is not checking content",
        wrong / upright.max(1e-9)
    );
    // ── v0.8.167 (audit): THE SAME FALSIFIER ON THE MANAGED CONTRACT ────────────────────────
    //
    // `decode_with_out` — the door that lets a falsifier meet the WAVE 1 output contract — shipped
    // in v0.8.165 with NO caller, so the `AssemblyFault` x `ManagedRgba` combination was never
    // executed by anything. That is the door's whole reason to exist, and an unexercised door is
    // either a bug or dead weight; this row is the ruling's "wire it".
    //
    // It matters beyond bookkeeping: the managed finish writes through `out_word`, a DIFFERENT
    // branch of `FINISH_WGSL` from the 24 bpp one every rotation row above drives. The two share
    // `rot_uv` by construction today — and "by construction" is exactly the kind of claim that
    // stops being true silently.
    //
    // FALSIFIER (L28): make the managed arm of `FINISH_WGSL` read its source uv without the
    // rotation (or hand `decode_with_out` `AssemblyFault::None` here) and the ratio assert reddens,
    // because the reversed managed frame would then be the same picture as the correct one.
    let good_managed = dec
        .decode_managed(&src, Some(2880), falcon_color::Gamut::DisplayP3, falcon_color::Gamut::Srgb)
        .expect("the rotated photo, managed");
    let rev_managed = dec
        .decode_with_out(
            &src,
            Some(2880),
            Submission::Pipelined,
            AssemblyFault::ReverseRotation,
            FinishOut::ManagedRgba {
                src: falcon_color::Gamut::DisplayP3,
                dst: falcon_color::Gamut::Srgb,
            },
        )
        .expect("the wrong-way turn still assembles on the managed contract");
    assert_eq!(
        (rev_managed.w, rev_managed.h),
        (good_managed.w, good_managed.h),
        "the reverse turn keeps the dims on the managed contract too"
    );
    // `decode_with_out` answers `PhotoRgb` whatever the contract, so on the managed contract its
    // `rgb` field carries RGBA — the same aliasing `decode_managed` unwraps. Compare like with
    // like: both buffers are 4 bpp here.
    //
    // WHAT THIS MEASURES, EXACTLY (v0.8.168, F6 — the v0.8.167 prose over-claimed). It is a
    // DIFFERENCE, not an uprightness: it proves the managed finish's `rot_uv` is reached and
    // applied, because turning the picture the other way changes the pixels. It cannot prove the
    // managed frame lands the RIGHT way up, because there is no WIC reference on this contract
    // (WIC hands back source-gamut RGB, and `good_managed` is RGBA already converted to sRGB).
    //
    // UPRIGHTNESS ON THE MANAGED CONTRACT IS COVERED, transitively and deliberately, by the
    // cross-path colour gate `the_gpu_colour_arm_lands_where_the_cpu_arm_lands`: it compares
    // `decode_managed`'s output against `transform_rgba(decode(...))` — the SOURCE contract's own
    // pixels — pixel for pixel, on this very file (it tracks `rotated_rows` and refuses to stop
    // before a rotated file has run), and the source contract's uprightness is what the `upright`
    // vs `wrong` comparison ABOVE pins against WIC. So: this row pins "the managed rotation
    // happens", that row pins "it is the same rotation the source contract performs", and the
    // WIC comparison pins "that rotation is correct".
    let m_upright_vs_reversed = mean(&good_managed.rgba, &rev_managed.rgb);
    eprintln!(
        "IMG_2814 rotation, MANAGED contract: mean |d| between the correct turn and the reverse \
         one = {m_upright_vs_reversed:.3}"
    );
    assert!(
        m_upright_vs_reversed > 8.0,
        "the managed finish produced nearly the same picture turned both ways (mean |d| \
         {m_upright_vs_reversed:.3}) — its rotation is not being applied, or this falsifier is not \
         reaching it"
    );

    // The honest level, reported not gated: this is two chroma reconstructions disagreeing.
    assert!(upright < 8.0, "the rotated photo's residual against WIC is {upright:.3}, far past a filter difference");
}

/// **DETERMINISM**, both axes: the same photo twice, and pipelined against serialised.
#[test]
fn the_same_photo_twice_is_byte_identical_and_pipelining_changes_nothing() {
    if !run_here("the_same_photo_twice_is_byte_identical_and_pipelining_changes_nothing") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let files = corpus();
    let Some(path) = files.iter().find(|p| p.file_name().unwrap() == "IMG_3258.HEIC") else {
        skip!("IMG_3258.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(path).expect("E1 grid");
    let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
    for scale in [None, Some(2880u32), Some(256u32)] {
        let a = dec.decode(&src, scale).expect("first");
        let b = dec.decode(&src, scale).expect("second");
        assert_eq!(a, b, "the same photo decoded twice differed at scale {scale:?}");
        let s = dec
            .decode_with(&src, scale, Submission::Serialised, AssemblyFault::None)
            .expect("serialised");
        assert_eq!(
            a, s,
            "pipelining changed the picture at scale {scale:?} — a surface is being read while it is still being written"
        );
    }
    eprintln!("determinism: 3 scales, twice each, plus a serialised run — all byte-identical");
}

/// **HOSTILE**: a tile that cannot decode must fail the PHOTO, not produce a half-composited one,
/// and must leave the session and the device usable.
#[test]
fn a_bad_tile_fails_the_photo_closed_and_leaves_the_session_usable() {
    if !run_here("a_bad_tile_fails_the_photo_closed_and_leaves_the_session_usable") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let files = corpus();
    let Some(path) = files.iter().find(|p| p.file_name().unwrap() == "IMG_1826.HEIC") else {
        skip!("IMG_1826.HEIC is not in the corpus on this machine");
        return;
    };
    let src = tile_source(path).expect("E1 grid");
    let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
    let good = dec.decode(&src, Some(2880)).expect("the clean photo");

    // A truncated item is the injection with a GUARANTEED refusal: `split_item_nals` rejects it
    // before the driver ever sees it, which is the failure direction a real corrupt file takes and
    // the one that must not leave a surface leased. Each position in the grid is tried, because
    // "tile 0 fails" and "tile 30 fails, 8 pictures into a pipelined chunk" are different bugs —
    // the second is the one that could strand leases mid-chunk.
    for victim in [0usize, 1, 7, 8, 30, src.tiles.len() - 1] {
        let mut hurt = src.clone();
        hurt.tiles[victim].truncate(4);
        let err = dec
            .decode_with(&hurt, Some(2880), Submission::Pipelined, AssemblyFault::None)
            .expect_err("a photo with an undecodable tile must NOT come back Ok");
        eprintln!("tile {victim} injected: the photo failed closed with `{err}`");
        // …and the very next honest photo must be byte-identical to the reference. This is the
        // assertion the lease-drain fix in `decode_tiles_streaming` exists for: a stranded lease
        // would surface here as `SurfacesExhausted` a few photos later, not at the injection.
        let after = dec.decode(&src, Some(2880)).expect("the session survived");
        assert_eq!(after, good, "the session stopped producing the right bytes after tile {victim}");
    }

    // The serialised path must fail closed too — it takes a different route through the session.
    let mut hurt = src.clone();
    hurt.tiles[20].truncate(4);
    assert!(dec
        .decode_with(&hurt, Some(2880), Submission::Serialised, AssemblyFault::None)
        .is_err());
    assert_eq!(dec.decode(&src, Some(2880)).expect("still alive"), good);
}

/// The CPU reference for the whole assembly: composite the tiles into one NV12 mosaic in system
/// memory, run E2's CPU kernel over it, then crop/rotate/mirror by the map in `DisplayMap`.
///
/// Deliberately the SLOW, OBVIOUS implementation — a `Vec` per plane, a copy per row, one pixel at
/// a time — because its only job is to be transparently right.
fn cpu_reference(src: &TileSource, tiles: &[falcon_hwdec::Nv12Image]) -> (Vec<u8>, u32, u32) {
    use falcon_decode::yuv_kernel::{nv12_to_rgb8, Nv12Frame};

    let (mw, mh) = (src.mosaic.0 as usize, src.mosaic.1 as usize);
    let mut y = vec![0u8; mw * mh];
    let mut uv = vec![0u8; mw * (mh / 2)];
    for (k, img) in tiles.iter().enumerate() {
        let (ox, oy) = src.tile_origin(k);
        let (ox, oy) = (ox as usize, oy as usize);
        let (tw, th, s) = (img.width as usize, img.height as usize, img.stride as usize);
        for r in 0..th {
            let d = (oy + r) * mw + ox;
            y[d..d + tw].copy_from_slice(&img.data[r * s..r * s + tw]);
        }
        // The chroma plane is HALF resolution in both axes and INTERLEAVED, so one chroma row is
        // `tw` BYTES wide (tw/2 Cb,Cr pairs) and starts at chroma row `oy/2`, byte column `ox`.
        let base = s * th;
        for r in 0..th / 2 {
            let d = (oy / 2 + r) * mw + ox;
            uv[d..d + tw].copy_from_slice(&img.data[base + r * s..base + r * s + tw]);
        }
    }
    let frame = Nv12Frame::packed(&y, &uv, src.mosaic.0, src.mosaic.1);
    let params = src.yuv_params().expect("the file's VUI");
    let mosaic = nv12_to_rgb8(&frame, params).expect("the E2 CPU reference must convert");

    let map = falcon_hwdec::photo::DisplayMap::new(src).expect("the display map");
    let (dw, dh) = map.display_dims();
    let mut out = vec![0u8; (dw as usize) * (dh as usize) * 3];
    for v in 0..dh {
        for u in 0..dw {
            let (mx, my) = map.mosaic_at(u, v);
            let s = ((my as usize) * mw + mx as usize) * 3;
            let d = ((v as usize) * (dw as usize) + u as usize) * 3;
            out[d..d + 3].copy_from_slice(&mosaic[s..s + 3]);
        }
    }
    (out, dw, dh)
}

/// **THE INDEPENDENT PROOF.** The GPU assembly's native output must be BYTE-IDENTICAL to the same
/// picture assembled on the CPU from the same decoded tiles.
///
/// This is the row that decides what the residual against WIC means. Everything measured against
/// WIC is a comparison with somebody else's converter and somebody else's rounding, so a 1-LSB
/// bias there is uninterpretable on its own — it could be theirs or ours. Here there is no third
/// party: the tiles are the same bytes, the colour arithmetic is E2's CPU reference (which
/// `yuv_kernel_twin` already pins the GPU kernel against, byte for byte), and the geometry is the
/// same map written twice — once in WGSL, once in Rust. Identical output means the compositing, the
/// crop, the rotation and the conversion are all exactly right and the entire WIC residual is on
/// the other side of the comparison.
///
/// Both grid shapes run: the 12 MP file (48 tiles of 512×512, `irot` 0) and the 48 MP rotated one
/// (54 tiles of 896×1024, `irot` 270), so the rotation arm of the map is proven too.
#[test]
fn the_gpu_assembly_is_byte_identical_to_the_cpu_reference() {
    if !run_here("the_gpu_assembly_is_byte_identical_to_the_cpu_reference") { return; }
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
    let _serial = one_at_a_time();
    let files = corpus();
    let mut ran = 0;
    for want in ["[12MP]IMG_2707.HEIC", "IMG_2814.HEIC"] {
        let Some(path) = files.iter().find(|p| p.file_name().unwrap() == want) else {
            skip!("{want}: not in the corpus on this machine");
            continue;
        };
        let src = tile_source(path).expect("E1 grid");
        let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
        let gpu = dec.decode(&src, None).expect("the native photo");

        let tiles = dec
            .session()
            .decode_tiles(&src.params, &src.tiles)
            .expect("the same tiles, straight out of M1");
        let (cpu, cw, ch) = cpu_reference(&src, &tiles);
        assert_eq!((gpu.w, gpu.h), (cw, ch), "{want}: the two references disagree about the size");
        if gpu.rgb != cpu {
            let n = gpu.rgb.iter().zip(&cpu).filter(|(a, b)| a != b).count();
            let i = gpu.rgb.iter().zip(&cpu).position(|(a, b)| a != b).unwrap();
            let max = gpu
                .rgb
                .iter()
                .zip(&cpu)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .max()
                .unwrap_or(0);
            panic!(
                "{want}: the GPU assembly and the CPU reference differ on {n} of {} bytes \
                 (first at pixel ({}, {}) channel {}, GPU {} vs CPU {}; max |delta| {max})",
                cpu.len(),
                (i / 3) as u32 % cw,
                (i / 3) as u32 / cw,
                ["R", "G", "B"][i % 3],
                gpu.rgb[i],
                cpu[i],
            );
        }
        eprintln!(
            "{want:<22} {cw}x{ch} — GPU assembly IDENTICAL to the CPU reference ({} bytes)",
            cpu.len()
        );
        ran += 1;
    }
    assert!(ran > 0, "the independent proof ran on nothing");
}

/// The contract's shape, on every corpus file at the tier a scrub actually serves: 24 bpp, stride
/// `w*3`, no padding, no alpha. Cheap, and it is the clause a refactor silently breaks.
#[test]
fn the_output_is_packed_rgb8_at_stride_w3() {
    if !run_here("the_output_is_packed_rgb8_at_stride_w3") { return; }
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
    let _serial = one_at_a_time();
    let mut ran = 0;
    for path in corpus() {
        let Ok(src) = tile_source(&path) else { continue };
        let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
        let p = dec.decode(&src, Some(1440)).expect("the fast subsample tier");
        assert_eq!(p.rgb.len(), (p.w as usize) * (p.h as usize) * 3);
        assert!(p.rgb.iter().any(|b| *b != 0), "an all-black photo is not a decode");
        ran += 1;
    }
    assert!(ran > 0, "the contract row ran on nothing");
}

/// v0.8.148 (E5) — **THE CM CHAIN, PROVED ON PIXELS AT A NON-sRGB OUTPUT GAMUT.**
///
/// The E6 trap the plan names is "every new present path must ride the managed chain". The
/// STRUCTURAL argument that this one does is short and strong: rung 0 returns from
/// `decode_heic_lane` at the same place the WIC rungs do, the frame travels the same
/// `browse_frame_rgba` → pool-worker → upload path, and the stage transform is keyed on
/// `shot_source_gamut(shot)` — a probe of the FILE, which cannot know which decoder ran. There is no
/// second present route to bypass anything with.
///
/// A structural argument is not a measurement, and the failure it would miss is the one that matters:
/// a hardware path that quietly returned pixels ALREADY IN THE OUTPUT GAMUT would satisfy every dims
/// gate and every seam statistic and then be transformed a second time. So this row measures.
///
/// THE METHOD. One Display-P3 corpus file, decoded both ways at a real browse tier, then run through
/// the SHIPPING transform (`falcon_color::transform_rgb`) into a NON-sRGB output gamut — Adobe RGB,
/// chosen because it is neither the source nor the identity, so a missing or doubled conversion has
/// nowhere to hide. Three numbers come out and they are read together:
///
///   * `raw`     — mean |delta| between the two decoders BEFORE any transform. E3-M2 measured this
///                 class at 0.37–1.40 per channel and named it: two converters' chroma reconstruction
///                 disagreeing by a fraction of an LSB, with the tail confined to R and B.
///   * `managed` — the same statistic AFTER both have been transformed. The claim is that the
///                 transform does not AMPLIFY: if both buffers are in the same space, the same matrix
///                 maps them the same way and `managed` stays in `raw`'s class.
///   * `bypass`  — the CONTROL, and the reason the row can be believed: transform ONE of them only.
///                 That is exactly what a CM-bypassed hardware path would look like, and it must be
///                 enormous next to `managed` or the statistic is not measuring anything.
///
/// FALSIFIER (L28): it is `bypass`. It is computed from the same buffers by the same function with
/// one call removed, so if the gate below could not tell a bypass from a match, `bypass` would sit
/// beside `managed` and the row would say so.
#[test]
fn the_hardware_pixels_ride_the_same_colour_managed_chain_as_wic() {
    if !run_here("the_hardware_pixels_ride_the_same_colour_managed_chain_as_wic") { return; }
    // Public source contains no private camera corpus. An existing corpus still has to
    // satisfy every original pixel/hash/count assertion below.
    if corpus_dirs().iter().all(|dir| !dir.is_dir()) {
        skip!("no private HEIC corpus directories; set FALCON_HEIC_TESTKIT or FALCON_PHOTO_TEST_DIR");
        return;
    }
    use falcon_color::Gamut;

    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    // A real browse tier, not `None`: the fast supersample ask is what the pool actually requests,
    // and it exercises the GPU resampler as well as the kernel.
    const TIER: u32 = 2880;

    let mut ran = 0usize;
    // How many (file × output gamut) pairs moved the picture far enough for the control to be able
    // to tell a CM bypass from a match. At least one is required; see the loop.
    let mut discriminating = 0usize;
    for path in corpus() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Some(shot) = shot_for(&path) else { continue };
        // THE SOURCE GAMUT IS READ BY THE SHIPPING ORACLE, not asserted from the filename. E1
        // delegates the container's colour answer to exactly this function, which is what makes
        // "the transform is keyed on the file" true rather than hoped.
        let src_gamut = falcon_decode::shot_source_gamut(&shot);
        if src_gamut == Gamut::Srgb {
            continue; // an sRGB source at an Adobe RGB output would still transform, but the
                      // round's claim is about a WIDE-gamut file, so hold out for one
        }
        let Ok(src) = tile_source(&path) else { continue };
        let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
        let hw = dec.decode(&src, Some(TIER)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let wic = browse_frame_rgba(&shot, TIER, true, Lane::Native)
            .unwrap_or_else(|e| panic!("{name}: the WIC baseline failed: {e}"));
        assert_eq!(
            (hw.w, hw.h),
            (wic.w, wic.h),
            "{name}: the two routes must produce the same frame before colour is even discussed"
        );
        let wic_rgb = rgba_to_rgb(&wic.rgba);
        let raw = mean_abs(&hw.rgb, &wic_rgb);

        // The stage's own transform, at BOTH non-sRGB output gamuts a user can select. Two rather
        // than one because the size of a gamut conversion depends on the pair AND on the picture's
        // saturation, and a claim that held only at the gentlest pair would not be a claim.
        for dst in [Gamut::AdobeRgb, Gamut::Rec2020] {
            assert_ne!(dst, src_gamut, "{name}: the control needs a real conversion to measure");
            let mut hw_m = hw.rgb.clone();
            let mut wic_m = wic_rgb.clone();
            falcon_color::transform_rgb(&mut hw_m, src_gamut, dst);
            falcon_color::transform_rgb(&mut wic_m, src_gamut, dst);
            let managed = mean_abs(&hw_m, &wic_m);
            // HOW BIG A GAMUT TRANSFORM IS, on this picture, in the same units — the scale the
            // charter's "must NOT diverge by a gamut transform" is measured against.
            let moved = mean_abs(&wic_rgb, &wic_m);
            // THE CONTROL: what a CM-bypassed hardware path would measure — the WIC side managed,
            // the hardware side left in the file's gamut.
            let bypass = mean_abs(&hw.rgb, &wic_m);

            eprintln!(
                "{name} @{TIER}: src={src_gamut:?} dst={dst:?}  raw={raw:.4}  managed={managed:.4} \
                 moved(transform size)={moved:.4}  bypass(control)={bypass:.4}"
            );
            // THE GATE, at every pair: the transform does not AMPLIFY. If the two buffers are in
            // the same space, the same matrix and the same transfer curves map them the same way and
            // the residual between them comes out where it went in. The bound is TIGHT — measured
            // 0.7002 → 0.6835 (Adobe RGB) and 0.6812 (Rec.2020), i.e. it does not move at all — and
            // a doubled or missing conversion could not sit inside it.
            assert!(
                managed <= raw * 1.5 + 0.2,
                "{name} → {dst:?}: the colour transform AMPLIFIED the two routes' difference \
                 (raw {raw:.4} → managed {managed:.4}) — they are not in the same space"
            );
            // THE CONTROL is a different question and it is NOT gated at every pair, because how far
            // a gamut conversion moves a picture depends on the pair AND on the picture: Display P3 →
            // Rec.2020 on this photograph moves it by 0.87 mean, which is simply not far enough for
            // a MEAN to tell a bypass from a match, and pretending otherwise would be a gate that
            // passes for the wrong reason. So each pair is asked whether it discriminates, and the
            // row requires that at least one did — see the assertion after the loop.
            if moved > managed * 2.5 && bypass > managed * 2.5 {
                discriminating += 1;
            }
            ran += 1;
        }
    }
    assert!(ran > 0, "no wide-gamut corpus file — the CM row proved nothing on this machine");
    assert!(
        discriminating > 0,
        "{ran} (file × gamut) pairs ran and NOT ONE of them moved the picture far enough for the \
         control to separate a bypass from a match — every no-amplification gate above passed for \
         a reason this row cannot distinguish from doing nothing"
    );
    eprintln!(
        "CM chain: {ran} (file x output-gamut) rows, {discriminating} of them with a control that \
         separates a bypass from a match"
    );
}

/// Mean absolute per-byte difference between two equally sized packed-RGB buffers.
fn mean_abs(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "the two buffers must be the same size to be compared");
    let sum: u64 = a.iter().zip(b).map(|(x, y)| u64::from(x.abs_diff(*y))).sum();
    sum as f64 / a.len() as f64
}

/// Per-channel worst case, mean, and the share of channels off by more than one — the three
/// numbers the cross-path gate reports and judges on.
fn delta_stats(a: &[u8], b: &[u8]) -> (u8, f64, f64) {
    assert_eq!(a.len(), b.len(), "the two buffers must be the same size to be compared");
    let mut max = 0u8;
    let mut sum = 0u64;
    let mut over1 = 0u64;
    for (x, y) in a.iter().zip(b) {
        let d = x.abs_diff(*y);
        max = max.max(d);
        sum += u64::from(d);
        if d > 1 {
            over1 += 1;
        }
    }
    (max, sum as f64 / a.len() as f64, over1 as f64 / a.len() as f64)
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// v0.8.165 (WAVE 1) — THE CROSS-PATH COLOUR GATE
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// **THE GATE FOR MOVING HEIC'S COLOUR OFF THE CPU.** Same file, same tiles, same canvas, the same
/// crop/`irot`/resample — finished BOTH WAYS and compared, on the real corpus, on real hardware.
///
/// ```text
///   OLD (v0.8.164):  finish -> packed RGB8 in the file's gamut
///                           -> falcon_decode::rgb_to_rgba          (browse_frame_rgba's `exp`)
///                           -> falcon_color::transform_rgba(src,dst) (the detail tier's `xform`)
///   NEW (v0.8.165):  finish_out(ManagedRgba { src, dst }) -> RGBA8 already in `dst`
/// ```
///
/// # What is being claimed, and what is not
///
/// Byte-identity is NOT presumed and would be the wrong thing to demand: the old arm is
/// `falcon-color`'s `f32` CPU transform and the new one is the same arithmetic in a WGSL `f32`
/// compute pass, so `pow` differs in the last ulp and the two land within a rounding step of each
/// other. What IS claimed is the acceptance criterion the round set: the new output is the colour
/// THE JPEG CHAIN ALREADY PRODUCES, because it is spliced from the same shader source
/// (`rot_uv_cm_core!`, asserted in `falcon-gpu`'s `the_heic_finish_pass_embeds_the_same_shared_core`)
/// and quantised by the same round-half-up rule (`pack`'s `floor(c + 0.5)` against
/// `apply_pixel`'s `(v * 255.0 + 0.5)`). The residual against the CPU arm is therefore reported as
/// three numbers per (file × tier × output gamut), not hidden behind a tolerance.
///
/// # The four output gamuts, and why each is there
///
///   * the SOURCE gamut itself — the pass-through arm. `transform_rgba` is a no-op when
///     `src == dst` and the shader's bit-1 arm refuses to round-trip through `linearize`/`encode`
///     for the same reason, so this row demands EXACT equality. It is what proves the new path
///     does not quietly re-encode a picture nobody asked it to touch.
///   * `AdobeRgb` and `Rec2020` — two real conversions of different sizes.
///   * `Custom` — **the owner's own case**, and the hard one: the destination TRC is not analytic
///     but a per-channel inverse LUT, fetched on the CPU by `Trc::Lut` and on the GPU by
///     `textureLoad` from the `CustomLutTex` built out of the SAME `custom_encode_lut` u16 table.
///     A profile is installed here so the row exercises that branch rather than the fallback.
///
/// # FALSIFIER (L28) — the mis-plumbed source gamut
///
/// The gate could pass vacuously if the source gamut never reached the shader at all (a uniform
/// packed wrong, a matrix left identity): both arms would then be doing "something with sRGB" and
/// might agree. So the row ALSO finishes the same canvas with `src` deliberately mis-declared as
/// `Gamut::Srgb` while the CPU arm converts from the file's real Display-P3, and asserts that the
/// delta EXCEEDS the gate by a wide margin. Delete the `c0/c1/c2` matrix rows from
/// `FinishUniforms`, or stop passing `src` through `decode_managed`, and the mis-plumbed and the
/// correct results become the same buffer — this assert is what reddens.
#[test]
fn the_gpu_colour_arm_lands_where_the_cpu_arm_lands() {
    if !run_here("the_gpu_colour_arm_lands_where_the_cpu_arm_lands") { return; }
    use falcon_color::{CustomProfile, Gamut};

    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();

    // A Custom profile with a REAL tone curve — sRGB primaries under a pure gamma 2.4 — so the
    // kind-2 inverse-LUT branch is genuinely exercised on BOTH sides (CPU `Trc::Lut`, GPU
    // `textureLoad` from `CustomLutTex`) instead of degrading to the gamma-2.2 stand-in
    // `build_custom_lut` installs when no profile is loaded. The colorant matrix is recovered
    // through falcon-color's own public API by `cm_parity`'s recipe, so it can never go stale
    // against a change to the crate's private matrices: install an IDENTITY profile, ask for
    // `Custom -> Srgb` (which is then `inv(sRGB->XYZ)`), and invert it back.
    const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    falcon_color::set_custom_profile(CustomProfile::from_gamma(IDENTITY, 2.2, "gate-probe"));
    let srgb_to_xyz = falcon_color::mat3_inv(falcon_color::src_to_dst_matrix(
        Gamut::Custom,
        Gamut::Srgb,
    ));
    falcon_color::set_custom_profile(CustomProfile::from_gamma(srgb_to_xyz, 2.4, "wave1-gate"));

    // ── THE GATE, BY SHAPE, with the measurement it was set from ────────────────────────────────
    //
    // The two finish shapes have different arithmetic and it would be dishonest to bound them with
    // one number:
    //
    //  * NO RESAMPLE (the output IS the display size) — the DETAIL tier's own shape, and the one
    //    this wave actually changes: `ddim` is 8192 against an 8064 px photo, so both finish passes
    //    take their identity-copy arm. Both arms then see the IDENTICAL u8 mosaic pixel and the
    //    only thing that can differ is `powf` against WGSL `pow`. MEASURED on the RTX 5080:
    //    max 1, mean 0.0000, over1 0.00000% — on every output gamut INCLUDING the owner's Custom.
    //
    //  * RESAMPLED (the 2880 px fast ask) — here the two arms are not doing the same arithmetic,
    //    and the GPU arm is the MORE accurate of the two: it colour-manages the f32 Lanczos
    //    accumulator, while the CPU arm manages the u8 that accumulator was rounded to first. So
    //    the residual is a missing 8-bit quantisation step, amplified near black by a pure-power
    //    destination TRC (the effect `YUV_WGSL` names in its own chroma-offset comment: "~6 LSB
    //    near black through Adobe RGB's pure-power TRC"). It is the SAME accuracy argument U2's
    //    fused YUV path already ships on — "one fewer 8-bit quantisation than the RGBI path".
    //    MEASURED: max 1 (Rec.2020), 6-7 (Adobe RGB), 13-15 (Custom); mean 0.147-0.179; over1
    //    0.00000-0.09994%.
    //
    // THE BOUNDS, AND WHAT THEY ACTUALLY ARE (v0.8.167: this paragraph is corrected — it used to
    // describe numbers the constants below do not carry, which is the ChevronGlyph class: a
    // doc-comment naming a quantity it does not measure).
    //
    //   MAX_LSB  = 24    against a measured worst of 15 (Custom, resampled) — 1.6x headroom.
    //   MAX_MEAN = 0.35  against a measured worst of 0.179                   — 2.0x headroom.
    //   MAX_OVER1= 0.005 (i.e. 0.5% of channels) against a measured worst of 0.09994% — 5x.
    //
    // `over1` is the term that actually bites: a systematically wrong colour moves EVERY saturated
    // pixel, not a rounding-boundary tail. The falsifier measures 15.0-17.0% there against this
    // gate's 0.5% ceiling — a 30x separation, which is what makes these bounds a gate rather than a
    // shrug. (The old text claimed a 0.10% ceiling and "150x"; both were arithmetic against a
    // constant that was never shipped.)
    const FLAT_MAX_LSB: u8 = 1;
    const FLAT_MAX_MEAN: f64 = 0.002;
    const MAX_LSB: u8 = 24;
    const MAX_MEAN: f64 = 0.35;
    const MAX_OVER1: f64 = 0.005;

    let mut rows = 0usize;
    let mut exact_rows = 0usize;
    let mut flat_rows = 0usize;
    let mut rotated_rows = 0usize;
    let mut falsified = 0usize;
    for path in corpus() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Some(shot) = shot_for(&path) else { continue };
        let src_gamut = falcon_decode::shot_source_gamut(&shot);
        if src_gamut == Gamut::Srgb {
            continue; // the round's claim is about the WIDE-gamut phone files
        }
        let Ok(src) = tile_source(&path) else { continue };
        let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");
        // The fast supersample ask (both resample passes + the crop/irot map) on every file, and
        // the NATIVE tier — the identity-copy arm, which is the shape the DETAIL tier actually
        // takes and the one WAVE 1 changed — UNTIL THAT SHAPE HAS ACTUALLY RUN.
        //
        // v0.8.167: this was `fi < 2`, an index into the corpus — which is fragile in the exact way
        // that matters, because the loop `continue`s past every sRGB file BEFORE reaching here. A
        // corpus whose first two wide-gamut files happened to sit at index 2 and 5 would run the
        // native tier on neither, and the `flat_rows > 0` assert at the bottom would then fail as
        // a CORPUS-ORDER accident rather than as a defect. Gating on the counter the tail assert
        // reads makes the two agree by construction: the expensive native decode runs until the
        // flat shape has been exercised, and then stops.
        let tiers: &[Option<u32>] =
            if flat_rows == 0 { &[Some(2880), None] } else { &[Some(2880)] };
        for &tier in tiers {
            let cpu = dec.decode(&src, tier).unwrap_or_else(|e| panic!("{name}: {e}"));
            // The OLD arm's expansion, through the SHIPPING function `browse_frame_rgba` calls —
            // not a re-implementation of it.
            let base = falcon_decode::rgb_to_rgba(&cpu.rgb);
            assert_eq!(base.len(), (cpu.w as usize) * (cpu.h as usize) * 4);
            // Which of the two shapes is this row? The finish chain resamples only when the asked
            // size differs from the photo's own display size — read off the SAME `TileSource` the
            // decode used, never guessed from the tier label.
            let resampled = (cpu.w, cpu.h) != src.display;
            if src.irot != 0 {
                rotated_rows += 1;
            }
            for dst in [src_gamut, Gamut::AdobeRgb, Gamut::Rec2020, Gamut::Custom] {
                let mut want = base.clone();
                falcon_color::transform_rgba(&mut want, src_gamut, dst);
                let got = dec
                    .decode_managed(&src, tier, src_gamut, dst)
                    .unwrap_or_else(|e| panic!("{name}: the managed finish failed: {e}"));
                assert_eq!(
                    (got.w, got.h),
                    (cpu.w, cpu.h),
                    "{name}: the managed finish must not change the frame size"
                );
                assert_eq!(
                    got.rgba.len(),
                    (got.w as usize) * (got.h as usize) * 4,
                    "{name}: RGBA8 at stride w*4 is the managed contract"
                );
                assert!(
                    got.rgba.chunks_exact(4).all(|p| p[3] == 255),
                    "{name}: every managed pixel must be opaque — the renderer blits it as-is"
                );
                let (max, mean, over1) = delta_stats(&got.rgba, &want);
                eprintln!(
                    "{name} tier={tier:?} src={src_gamut:?} dst={dst:?}  \
                     max={max} mean={mean:.4} over1={:.5}%",
                    over1 * 100.0
                );
                if dst == src_gamut {
                    // THE PASS-THROUGH ROW: both arms must decline to touch the picture.
                    assert_eq!(
                        max, 0,
                        "{name}: src == dst must be EXACT — the GPU arm re-encoded a picture the \
                         CPU arm left alone"
                    );
                    exact_rows += 1;
                } else if resampled {
                    assert!(
                        max <= MAX_LSB && mean <= MAX_MEAN && over1 <= MAX_OVER1,
                        "{name} -> {dst:?} (resampled): the GPU colour arm does NOT land where the \
                         CPU arm lands (max {max}, mean {mean:.4}, over1 {over1:.5}) — quantify and \
                         report to the architect; do not widen this bound to make it green"
                    );
                } else {
                    assert!(
                        max <= FLAT_MAX_LSB && mean <= FLAT_MAX_MEAN,
                        "{name} -> {dst:?} (no resample — THE DETAIL TIER'S OWN SHAPE): the two \
                         arms see the identical u8 mosaic here, so anything past a last-ulp `pow` \
                         difference is a real divergence (max {max}, mean {mean:.4})"
                    );
                    flat_rows += 1;
                }
                rows += 1;
            }

            // ── THE FALSIFIER, on this file's first tier only (it costs a whole extra decode) ──
            if tier == Some(2880) && src_gamut != Gamut::Srgb {
                let dst = Gamut::Rec2020;
                let mut want = base.clone();
                falcon_color::transform_rgba(&mut want, src_gamut, dst);
                let bad = dec
                    .decode_managed(&src, tier, Gamut::Srgb, dst)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                let (bmax, bmean, bover) = delta_stats(&bad.rgba, &want);
                eprintln!("{name} FALSIFIER (src mis-plumbed as sRGB): max={bmax} mean={bmean:.4} over1={:.5}%", bover * 100.0);
                // `over1` is the discriminator, not `max`: the legitimate residual is a
                // rounding-boundary tail (MEASURED 0.00000-0.09994% of channels, gated at 0.5%)
                // while a wrong source matrix moves every saturated pixel (MEASURED 15.0-17.0%).
                // The 5% bound here sits 10x above the gate's own ceiling and a third below the
                // measured falsifier — so neither a lucky legitimate row nor a marginal driver can
                // satisfy it. `bmean > MAX_MEAN` is the second half deliberately: the falsifier's
                // mean is 4-6x the gate's ceiling, so a falsifier that somehow tripped only `over1`
                // would still have to move the whole picture to pass.
                assert!(
                    bover > 0.05 && bmean > MAX_MEAN,
                    "{name}: declaring the WRONG source gamut produced a frame the gate would have \
                     accepted (max {bmax}, mean {bmean:.4}, over1 {bover:.5}) — the gate is not \
                     measuring the source gamut's plumbing at all"
                );
                falsified += 1;
            }
        }
        // Bounded — each row is a full 48 MP decode plus two 195 MB comparisons — but never
        // before a ROTATED file has been through it. `irot` is the one geometry the finish pass
        // treats specially and the one an upside-down photo would announce, so a gate that stopped
        // at the first two upright files would be gating half the shape.
        if rows >= 12 && rotated_rows > 0 {
            break;
        }
    }
    if rows == 0 {
        skip!("no wide-gamut HEIC in the corpus — the cross-path colour gate had nothing to run on");
        return;
    }
    assert!(exact_rows > 0, "the src == dst pass-through row never ran");
    assert!(flat_rows > 0, "the NO-RESAMPLE shape — the detail tier's own — never ran");
    if rotated_rows == 0 {
        skip!("no ROTATED (irot) wide-gamut HEIC in the corpus — the gate ran upright files only");
    }
    assert!(falsified > 0, "the mis-plumbed-source falsifier never ran — the gate is unfalsified");
    eprintln!(
        "cross-path colour gate: {rows} row(s), {exact_rows} exact, {flat_rows} flat, {rotated_rows} rotated, \
         {falsified} falsifier(s)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// v0.8.149 (F7) — THE BIT-DEPTH GUARD
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// **THE SYNTHETIC Main10 ROW — the failure that would not have announced itself.**
///
/// Every other decline in this lane is a fall-soft: the file is refused, WIC takes it, the picture
/// is right. A 10-bit primary is different in kind. The session is created for
/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN` — Main, 8-bit — with `NV12` surfaces, which are 8-bit by
/// definition. The driver is under no obligation to refuse a Main10 bitstream fed to that, and if it
/// does not, what comes back is an `NV12` surface of exactly the right dimensions: E3-M2's dims gate
/// passes it, the seam statistic passes it, the CM chain passes it, and the user is shown a quietly
/// wrong photograph. E5 shipped nine ways to decline and not one of them was this.
///
/// The corpus has no Main10 primary — E1's own header records that the base image is 8-bit on every
/// file measured, in a corpus whose AUX chain is 10-bit, which is exactly why the depth is READ
/// rather than assumed. So the row SYNTHESISES one, twice, from a real file: once through the
/// container's own `pixi` (the muxer's declaration, checked before a single NAL is parsed) and once
/// through the `hvcC` record's `bit_depth_luma_minus8` (the configuration's, checked before the
/// tiles are marshalled). Both are single-byte edits that leave every box size intact, so the file
/// still parses all the way to the gate — which is the only way to prove the gate is what stopped
/// it.
///
/// Needs the corpus. Needs NO hardware: refusing to build a session is the whole point.
///
/// FALSIFIER (L28): delete either arm of `bit_depth_gate` and the matching row reddens with the
/// file parsing happily as 8-bit; make the gate warn instead of refusing and both redden.
#[test]
fn a_main10_primary_is_refused_before_it_can_enter_an_8_bit_session() {
    let files = corpus();
    let Some(real) = files.first() else {
        skip!("no HEIC corpus on this machine");
        return;
    };
    let bytes = std::fs::read(real).expect("a corpus file");
    // The unpatched file must PASS, or the row proves nothing about the patch.
    let base = tile_source(real).expect("a corpus HEIC is 8-bit and parses");
    assert_eq!(base.params.record_bit_depth_luma, 8, "the fixture is 8-bit to begin with");
    assert_eq!(base.params.sps.bit_depth_luma_minus8, 0);

    let dir = std::env::temp_dir().join("falcon_f7_bitdepth");
    std::fs::create_dir_all(&dir).expect("a scratch dir");

    // EVERY occurrence of each box is patched, not the first. A grid HEIC carries several `pixi`
    // and `hvcC` properties in `ipco` — one set for the tiles, one for the 10-bit AUX chain — and
    // the primary's is not reliably the first in byte order. Patching the first alone was tried and
    // it FAILED SILENTLY: the file parsed as 8-bit and the row would have proved nothing about the
    // gate. (Only the ones currently declaring 8 are touched, so the AUX chain's real 10-bit
    // declaration is left exactly as it is.)

    // (a) the CONTAINER's declaration: `pixi` = version/flags(4) + num_channels(1) + one byte per
    // channel. Patch the first channel from 8 to 10.
    let mut patched = bytes.clone();
    // …in the BODY: FullBox version/flags (4) + num_channels (1), then channel 0's depth.
    let n = patch_boxes(&mut patched, b"pixi", 4 + 1, |b| (*b == 8).then(|| *b = 10).is_some());
    if n > 0 {
        let f = dir.join("pixi10.heic");
        std::fs::write(&f, &patched).expect("write the synthetic file");
        let err = tile_source(&f).expect_err("a 10-bit pixi must be refused");
        assert!(
            matches!(err, falcon_hwdec::HwDecError::BitDepth { bits: 10, .. }),
            "and refused BY NAME, not swallowed as a generic container error: {err}"
        );
        assert!(err.to_string().contains("pixi"), "the sentence names which declaration said so: {err}");
        eprintln!("F7 (a) pixi 10-bit ({n} box(es) patched): {err}");
    } else {
        skip!("the fixture carries no 8-bit pixi box — the container arm could not be synthesised");
    }

    // (b) the CONFIGURATION's declaration: `hvcC` body byte 17, low three bits =
    // bit_depth_luma_minus8. This is the one that matters most, because it is what the DECODER
    // SESSION is built from.
    let mut patched = bytes.clone();
    let n = patch_boxes(&mut patched, b"hvcC", 17, |b| {
        (*b & 0x07 == 0).then(|| *b |= 2).is_some() // minus8 = 2 -> 10-bit
    });
    assert!(n > 0, "every HEIC grid carries an 8-bit hvcC to patch");
    let f = dir.join("hvcc10.heic");
    std::fs::write(&f, &patched).expect("write the synthetic file");
    let err = tile_source(&f).expect_err("a 10-bit hvcC record must be refused");
    assert!(
        matches!(err, falcon_hwdec::HwDecError::BitDepth { bits: 10, .. }),
        "refused by name: {err}"
    );
    assert!(err.to_string().contains("hvcC"), "and it names the record: {err}");
    assert!(
        err.to_string().contains("8-bit HEVC Main only"),
        "and says what this lane can actually do: {err}"
    );
    eprintln!("F7 (b) hvcC 10-bit ({n} record(s) patched): {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate's own truth table — pure, so it runs everywhere including a box with no corpus at all.
///
/// The three witnesses have to AGREE as well as be 8: a file whose container and whose bitstream
/// describe different pictures is not one this lane should be guessing about.
#[test]
fn the_bit_depth_gate_refuses_every_witness_that_is_not_eight() {
    use falcon_hwdec::{bit_depth_gate, HwDecError};
    assert!(bit_depth_gate(Some(8), 8, 8, 8, 8).is_ok(), "every corpus file, and the only pass");
    assert!(bit_depth_gate(None, 8, 8, 8, 8).is_ok(), "an absent pixi is one fewer witness, not a veto");
    for (i, args) in [
        (Some(10u8), 8u8, 8u8, 8u8, 8u8),
        (Some(8), 10, 8, 8, 8),
        (Some(8), 8, 10, 8, 8),
        (Some(8), 8, 8, 10, 8),
        (Some(8), 8, 8, 8, 10),
    ]
    .into_iter()
    .enumerate()
    {
        let e = bit_depth_gate(args.0, args.1, args.2, args.3, args.4)
            .expect_err("one 10-bit witness is enough to refuse");
        assert!(matches!(e, HwDecError::BitDepth { bits: 10, .. }), "row {i}: {e}");
        assert!(!e.to_string().is_empty());
    }
    // 12-bit, and the DISAGREEMENT case: a container claiming 8 over a bitstream that is not.
    assert!(matches!(
        bit_depth_gate(Some(8), 8, 8, 12, 12),
        Err(HwDecError::BitDepth { bits: 12, .. })
    ));
}

/// Apply `f` to the byte at `off_in_body` of EVERY box whose four-byte TYPE is `want`, and report
/// how many `f` actually changed. A byte search rather than a box walk on purpose: the synthetic
/// files must differ from the original in named single bytes, so the helper that finds them must not
/// depend on the parser the row is testing.
fn patch_boxes(bytes: &mut [u8], want: &[u8; 4], off_in_body: usize, mut f: impl FnMut(&mut u8) -> bool) -> usize {
    let at: Vec<usize> = (0..bytes.len().saturating_sub(4))
        .filter(|&i| &bytes[i..i + 4] == want)
        .map(|i| i + 4 + off_in_body)
        .filter(|&i| i < bytes.len())
        .collect();
    at.into_iter().filter(|&i| f(&mut bytes[i])).count()
}

/// v0.8.149 (F9/B11) — **WHAT THIS MACHINE COULD NOT RUN, as a number.**
///
/// Not a gate and deliberately not an assertion: on a box with no video device and no corpus the
/// honest outcome is "everything skipped", and failing there would only teach people to ignore it.
/// What it does is make the count IMPOSSIBLE TO MISS at the end of the binary's output, so a green
/// run that gated nothing cannot be quoted as a green run that gated the epic. TESTING.md §8h lists
/// which rows need which.
///
/// Named `zz_` so it sorts last: `cargo test` runs a binary's rows alphabetically, and this number
/// is only meaningful after the rows that produce it.
///
/// 28 October (round 2b review P2): the device question is a driver call, so it is asked in this
/// row's own process, under the time limit; the count stays here in the parent, where the other
/// rows' children's skips were added (`run_here`).
#[test]
fn zz_the_skip_audit_states_what_this_machine_could_not_run() {
    if hw_child::in_child() {
        eprintln!("hardware present: {}", hardware());
        return;
    }
    let err = hw_child::run_in_child(module_path!(), "zz_the_skip_audit_states_what_this_machine_could_not_run");
    let present = err.lines().find_map(|l| l.strip_prefix("hardware present: ")).unwrap_or("unknown").to_string();
    let n = SKIPPED.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "SKIP AUDIT (hw_photo.rs): {n} row(s) declined on this machine — hardware present: {present}, \
         corpus files: {}",
        corpus().len()
    );
}

/// v0.8.171 (HEIC SPEED PRIORITY) — **A SUPERSEDED DECODE STOPS MID-GRID, AND LEAVES THE SESSION
/// EXACTLY AS IT FOUND IT.**
///
/// The mechanism is worth one sentence: a 48 MP HEIC is ~190 tiles through ONE video engine, and
/// the 08-06 laptop log caught such a run finishing for a photograph the user had already browsed
/// past (`full-res #91 skipped (post-decode: current is 95)`, after 780 ms of engine time) while the
/// shot ON SCREEN queued behind it. This row proves the run can stop, that stopping is not a
/// failure, and — the part that actually matters — that the session it stopped inside is still a
/// working session afterwards.
///
/// THE LEAK GATE IS TEN ABORTS AND THEN A DECODE. `SurfaceLease` is a plain `Copy` index with no
/// `Drop`: a lease abandoned mid-run leaves `in_flight[i]` true for the session's whole life, and a
/// session owns EIGHT surfaces — so one stranded per abort exhausts the pool at round eight and
/// every later photo meets `SurfacesExhausted` with nothing to say why (the v0.8.146 bug
/// `decode_tiles_streaming`'s release drain was written for). Ten aborts through the SAME
/// `PhotoDecoder`, then a full decode byte-compared against the unaborted one.
///
/// FALSIFIER (L28, RED-FIRST — RUN): delete the `if superseded()` block from
/// `DecodeSession::decode_tiles_streaming` and the first assertion reddens (the run completes).
/// Move the check INSIDE the submit loop — i.e. abort with leases outstanding and past the
/// `for l in rest { self.release(l) }` drain — and the run reddens: first on the between-tiles
/// assertion (nothing had been delivered), and with that relaxed, on the decode after the tenth
/// round with `SurfacesExhausted`. That is the whole argument for why the check sits at the chunk
/// boundary and nowhere else.
#[test]
fn a_superseded_decode_stops_mid_grid_and_leaves_the_session_usable() {
    if !run_here("a_superseded_decode_stops_mid_grid_and_leaves_the_session_usable") { return; }
    if !hardware() {
        skip!("no D3D11 video device decoding HEVC Main to NV12 on this machine");
        return;
    }
    let _serial = one_at_a_time();
    let files = corpus();
    let Some(path) = files.iter().find(|p| p.file_name().unwrap() == "IMG_2814.HEIC") else {
        skip!("IMG_2814.HEIC: not in the corpus on this machine");
        return;
    };
    let src = tile_source(path).expect("E1 grid");
    let total_tiles = src.tiles.len();
    assert!(total_tiles > 8, "this row needs a real multi-chunk grid, not a single-tile file");
    let mut dec = PhotoDecoder::new(&src, 8).expect("a photo decoder");

    // (1) THE REFERENCE: the same photo, decoded with the signal armed but never true. This is the
    // "toggle OFF is byte-identical to today" half — an armed-but-quiet predicate must change
    // nothing at all about the picture.
    let quiet = match dec.decode_watched(&src, None, &mut || false).expect("the native photo") {
        PhotoRun::Done(p) => p,
        PhotoRun::Superseded { .. } => panic!("a predicate that never fires must never abort"),
    };
    let plain = dec.decode(&src, None).expect("the same photo through the un-watched door");
    assert_eq!(quiet.rgb, plain.rgb, "an armed-but-quiet signal changes no pixel");
    assert_eq!((quiet.w, quiet.h), (plain.w, plain.h));

    // (2) THE ABORT, TEN TIMES OVER — and the count is the leak gate, not decoration. A session
    // owns EIGHT surfaces; strand one per abort and the ninth decode meets `SurfacesExhausted`. Ten
    // consecutive aborts through the same decoder therefore PROVE the accounting rather than
    // suggesting it, and they are what makes the "abort somewhere other than the chunk boundary"
    // falsifier bite: from inside the submit loop there are up to eight leases outstanding and the
    // release drain below it never runs.
    for round in 0..10 {
        let mut asked = 0usize;
        let run = dec
            .decode_watched(&src, None, &mut || {
                asked += 1;
                asked > 1 // let one chunk through, then stand down
            })
            .unwrap_or_else(|e| panic!("round {round}: an abort is not an error ({e})"));
        let (done, total) = match run {
            PhotoRun::Superseded { done, total } => (done, total),
            PhotoRun::Done(_) => panic!(
                "THE CLAIM: a supersession signal that goes true mid-grid must stop the run — \
                 round {round} decoded all {total_tiles} tiles instead"
            ),
        };
        assert_eq!(total, total_tiles, "the abort reports the grid it was decoding");
        assert!(
            done > 0 && done < total,
            "round {round} stopped BETWEEN tiles: {done} of {total} had been delivered"
        );
    }

    // (3) THE LEAK GATE, CASHED IN: after ten aborts the session is still a working session and the
    // photo it produces is the same photograph. One stranded surface per abort would have exhausted
    // the pool at round eight, and this call is where that shows up.
    let after = dec.decode(&src, None).expect("the session survived ten aborts");
    assert_eq!(
        after.rgb, plain.rgb,
        "the photo decoded AFTER an abort is byte-identical to the one decoded before it"
    );
    assert_eq!((after.w, after.h), (plain.w, plain.h));
}
