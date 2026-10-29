//! v0.8.101 (S1 + S2): the HEIC browse-lane strategy, tested against REAL iPhone files.
//!
//! # The skip-when-codec-absent pattern (read this before adding a row)
//!
//! Everything here needs the Windows Store HEVC/HEIF Image Extension — an OS component Falcon
//! deliberately does not vendor. On a machine without it (CI, a fresh laptop, macOS) these tests
//! must SKIP WITH A NOTE, never fail: a red test would be reporting the absence of a codec, not a
//! defect in Falcon, and a suite that cries wolf about the environment stops being read. So every
//! codec-dependent test opens with [`heic_shots_or_skip`], which prints exactly why it skipped.
//!
//! The corollary: a green run on a codec-less box proves NOTHING about S1/S2. The pure-arithmetic
//! rows below (`scaled_dims_matches_resize_to_long`, `classic_switch_reads_only_the_exact_flag`)
//! are the part that runs everywhere, and they are written to carry the invariants that CAN be
//! stated without pixels.

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

#[path = "../../test-support/fixture_paths.rs"]
mod fixture_paths;

use falcon_decode::*;

/// Optional real HEIC corpus, selected at runtime; never shipped in public source.
fn heic_dir() -> std::path::PathBuf { fixture_paths::heic().to_path_buf() }

/// v0.8.103 (V2): why `wic_heif_codec_present()` said no, stated PER PLATFORM.
///
/// On Windows it is a genuine environment fact — the Store HEVC/HEIF Image Extension is an OS
/// component Falcon deliberately does not vendor. Off Windows the function is a `#[cfg]`'d-off
/// `false` (lib.rs's non-Windows twin), and a Mac emphatically DOES have a HEIF decoder and does
/// decode HEIC through Image I/O. Telling a Mac tester "no HEIF decoder installed" is a false
/// statement about his machine, and a skip note that lies is worse than no note.
fn no_wic_reason() -> &'static str {
    if cfg!(windows) {
        "no WIC HEIF decoder installed on this machine — the HEVC/HEIF Image Extension is an OS \
         component, so its absence is an ENVIRONMENT fact, not a Falcon defect"
    } else {
        "this is not Windows, so `wic_heif_codec_present()` is a cfg'd-off `false` — the WIC ladder \
         these rows measure does not exist on this target at all (the platform's own HEIF decoder is \
         a different question, and irrelevant to it)"
    }
}

/// The shots, or `None` with a printed reason: no folder, no files, or no WIC HEIF ladder.
fn heic_shots_or_skip(what: &str) -> Option<Vec<Shot>> {
    let dir = heic_dir();
    if !dir.exists() {
        eprintln!("skip ({what}): HEIC testkit missing ({})", dir.display());
        return None;
    }
    if !wic_heif_codec_present() {
        eprintln!("skip ({what}): {}. Nothing about S1/S2 is proven by this run.", no_wic_reason());
        return None;
    }
    let shots: Vec<Shot> = scan_folder(&dir)
        .expect("scan the HEIC testkit")
        .into_iter()
        .filter(|s| s.kind == SrcKind::Heic)
        .collect();
    if !fixture_paths::require_available(!shots.is_empty(), "no HEIC shots classified in required corpus") {
        eprintln!("skip ({what}): no HEIC shots classified in {}", dir.display());
        return None;
    }
    Some(shots)
}

/// Mean absolute per-BYTE difference between two equally-sized RGBA buffers, as an f64 in 0..255.
/// The perceptual yardstick this round quotes: two renderings of the same photograph at the same
/// size differ by resampler choice and codec rounding, and this is how much.
///
/// v0.8.102: the ALPHA byte is excluded. `rgb_to_rgba` writes a constant 255 into every 4th byte, so
/// a quarter of the samples were guaranteed-zero differences that did nothing but divide the number
/// by 4/3 — a metric that flatters itself by 25% is a metric whose tolerance means 25% less than it
/// says. Every measured figure quoted in [`PERCEPTUAL_TOLERANCE`] was re-taken under this rule.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "buffers must be the same size to compare");
    let mut sum = 0u64;
    let mut n = 0u64;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if i % 4 == 3 {
            continue; // the constant alpha channel — see the note above
        }
        sum += x.abs_diff(*y) as u64;
        n += 1;
    }
    sum as f64 / n.max(1) as f64
}

/// The tolerance the round adopts for "this is the same picture", in mean absolute 0–255 byte
/// difference over the RGB bytes (alpha excluded — see [`mean_abs_diff`]). MEASURED on the 5-file
/// iPhone testkit (the tests print their own numbers, so this is checkable, not asserted from
/// memory). v0.8.102 re-took every figure: dropping the constant alpha channel scales the old
/// v0.8.101 numbers by 4/3, and the S2 sweep gained a row that engages the 1/8 stop.
///
/// * **S1**, embedded preview vs a full decode downscaled to the same 256 px tile: 1.151, 2.678,
///   3.213, 3.761, **5.255** — the JPEG-ish preview the camera wrote against Falcon's Lanczos
///   reduction of the master.
/// * **S2**, stop decode + Lanczos vs full decode + Lanczos: **0.000** wherever no stop qualifies
///   (bit-identical, because it IS the same code path); 2.362 / 2.465 / **3.398** at the 1/4 stop;
///   1.632 / 2.032 / 2.159 / 2.186 / **3.193** at the 1/8 stop (the 256/sup row, added by v0.8.102
///   so the coarsest stop the thumb tier can take is not the one rung in the ladder with zero pixel
///   coverage anywhere in the suite).
///
/// 8.0 sits ~1.5× above the worst observed (5.255), the same headroom v0.8.101's 6.0 held against
/// its own alpha-diluted worst of 3.942 — enough that a Windows codec revision moving a rounding
/// rule does not turn this red, and far below what a real defect produces: a channel-swapped,
/// mirrored, rotated or gamut-shifted frame lands in the tens.
const PERCEPTUAL_TOLERANCE: f64 = 8.0;

/// v0.8.115: the tolerance for comparing a frame the codec SUBSAMPLED against the same frame
/// produced by a full Lanczos reduction of the master — the one comparison
/// [`a_derived_fast_frame_is_the_same_picture_as_a_decoded_one`] cannot hold to
/// [`PERCEPTUAL_TOLERANCE`], and the reason is recorded in the S2 row above: asked at the stop's own
/// size, the codec's subsample measures 7.578 (IMG_1826) and 12.216 (IMG_1828) against a proper
/// reduction, because the subsample is genuinely more ALIASED. That is a fact about the rung, not a
/// defect — and in this comparison the DERIVED frame is the better of the two, so a bound that
/// failed it would be rejecting the higher-fidelity result.
///
/// 14.0 sits just above the 12.216 already measured on this rung. Where the codec does NOT scale —
/// the 48 MP × 2176 px case this round exists for — the row demands **bit-identity** instead, which
/// is a far stronger statement than any tolerance.
const DERIVE_STOP_TOLERANCE: f64 = 14.0;

/// S2 — WIC decode-at-scale, against the path it replaces.
///
/// The two claims, both on real files:
///   1. **Same frame size.** `Lane::Fast` and `Lane::Native` must produce IDENTICAL dims for every
///      (file, target, supersample) combination. S2 is a cost change; a layout change would ripple
///      into the FastCache dim buckets, the upload path and the texture atlas.
///   2. **Same picture.** Mean absolute difference under [`PERCEPTUAL_TOLERANCE`]. This is what
///      catches a wrong colour conversion, a mirrored/rotated frame, or a codec handing back a
///      different image entirely — all of which are cheap mistakes to make inside a COM ladder.
///
///   3. **S2 is actually running** (v0.8.102, F8). Twelve of v0.8.101's fifteen rows compared rung 3
///      against rung 3 — structurally guaranteed 0.000, carrying no information — and nothing in the
///      suite would have gone red if the stop ladder had stopped engaging entirely. This test now
///      counts the rows whose `heic_stop_divisor` is > 1 and refuses to pass with none, mirroring
///      S1's `previews > 0` guard, and its sweep carries a target that engages the **1/8** stop as
///      well as the 1/4 (the coarsest stop the thumb tier can take had zero pixel coverage in the
///      whole round).
///
/// v0.8.103 (V7): the per-row half of (3) now measures the LADDER, not the arithmetic.
/// `heic_stop_divisor` is pure arithmetic on the master's header dims — it picks a stop on every
/// machine on earth, saying nothing about whether THIS codec can decode at one. The v0.8.99 probe
/// found a real configuration that cannot (`GetClosestSize` answers with the native size), and
/// v0.8.102's own F4/F5 fix is what correctly routes that codec to rung 3, where the scaled frame IS
/// the full decode and `d == 0.000`. So the v0.8.102 assert red-failed the suite for a documented,
/// correct environment — reporting the absence of a codec CAPABILITY as a Falcon defect, exactly what
/// this file's opening doctrine forbids. The guard now asks
/// [`heic_decode_at_scale_engages`] — rung 1's own acceptance test, run without decoding — and
/// asserts `d > 0.0` only on the rows where the codec really did approve a stop, plus a suite-level
/// row that on a box where it approved ANY stop at least one row came back with different pixels.
/// On a non-scaling codec every one of those is vacuously satisfied and the suite stays green.
///
/// FALSIFIER (L28): drop the exact-dims trim in `wic_decode_rgb24_scaled` (let the long-side clamp
/// stand alone) and the dims row fails on any file whose codec size rounds differently; feed the
/// transform's raw pixels through WITHOUT the format conversion (assume the codec hands back RGB24)
/// and the diff row explodes on a BGRA codec — the picture comes back channel-swapped, which is
/// precisely the bug a dims-only test would wave through. Raise `HEIC_MIN_STOP_DIV` past 8 and the
/// arithmetic engagement guard fires — the failure mode that used to be twelve green 0.000 rows over
/// a dead strategy. Break the rung-1 `frame.cast::<IWICBitmapSourceTransform>()` in
/// `wic_decode_rgb24_scaled` ONLY (leaving `heic_decode_at_scale_engages`'s own cast intact) and the
/// per-row `d > 0.0` guard fires: the probe says this codec approved a stop, so a bit-identical frame
/// means the ladder did not take the door it was handed.
#[test]
fn s2_scaled_decode_matches_the_full_decode_in_size_and_picture() {
    if !hw_child::run_here(module_path!(), "s2_scaled_decode_matches_the_full_decode_in_size_and_picture") { return; }
    let Some(shots) = heic_shots_or_skip("S2 scaled decode") else { return };
    let mut worst = 0.0f64;
    let mut engaged = 0usize;
    // v0.8.103 (V7): the LADDER's own counters, kept apart from the arithmetic's. `approved` counts
    // rows where the codec accepted a stop (rung 1 or 2 will run); `with_pixels` counts how many of
    // those actually came back different from the full decode.
    let mut approved = 0usize;
    let mut with_pixels = 0usize;
    let mut divisors_seen: Vec<u32> = Vec::new();
    for s in &shots {
        let (sw, sh) = source_dimensions(s).expect("HEIC header dims");
        let long = sw.max(sh);
        let path = s.jpg.as_deref().expect("a HEIC shot carries its finished path");
        // 2880/sup → decode target 2880 (no stop on any iPhone master), 2880/sub → 1440 (the 1/4
        // stop on the 8064 files), 2048/sup → 2048 (none), and v0.8.102's 256/sup → 256, where the
        // 1/8 stop qualifies on ALL FIVE files. 256 is the shape production actually reaches the
        // coarsest stop in — the thumb tier when S1's preview door declines — which matters, because
        // the stop is compared here against the SAME ask, i.e. after the 4:1 Lanczos that follows it.
        // MEASURED, and worth recording because it is the first pixel data anyone has on this rung:
        // asked at the stop's OWN size instead (a 1008 px ask on an 8064 master, so the codec's
        // subsample lands raw with no reduction after it) the gap against a Lanczos reduction of the
        // full frame is 7.578 (IMG_1826) and 12.216 (IMG_1828) — the 1/8 stop is genuinely more
        // ALIASED than a proper 8:1 reduction. It is only ever reached with a 256 px ask, where the
        // 4:1 Lanczos that follows washes that back down to the 1.6–3.2 the rows below print, so
        // this is a fact about the rung rather than a defect in what any tier serves.
        for (target, sup) in [(2880u32, true), (2880, false), (2048, true), (256, true)] {
            let decode_target = if sup { target } else { (target / 2).max(1) };
            let div = heic_stop_divisor(long, decode_target);
            // Does the CODEC agree with the arithmetic? Rung 1's own acceptance test, asked at the
            // same target the decode below uses, without decoding anything.
            let codec_approves = heic_decode_at_scale_engages(path, decode_target);
            if div > 1 {
                engaged += 1;
                if codec_approves {
                    approved += 1;
                }
                if !divisors_seen.contains(&div) {
                    divisors_seen.push(div);
                }
            }
            let old = browse_frame_rgba(s, target, sup, Lane::Native).expect("classic HEIC decode");
            let new = browse_frame_rgba(s, target, sup, Lane::Fast).expect("S2 HEIC decode");
            assert_eq!(
                new.source,
                FrameSource::MainImage,
                "{}: the FAST lane must NEVER return an embedded preview — that door is \
                 Lane::Thumb's alone",
                s.name
            );
            assert_eq!(
                (new.w, new.h),
                (old.w, old.h),
                "{} @ {target}/sup={sup}: S2 changed the frame SIZE ({}x{} vs {}x{})",
                s.name,
                new.w,
                new.h,
                old.w,
                old.h
            );
            let d = mean_abs_diff(&old.rgba, &new.rgba);
            worst = worst.max(d);
            eprintln!(
                "S2 {:<20} {target}/sup={sup:<5} 1/{div} approved={codec_approves} {}x{}  \
                 mean|Δ|={d:.3}  old {} ms / new {} ms",
                s.name, new.w, new.h, old.dec_ms, new.dec_ms
            );
            assert!(
                d <= PERCEPTUAL_TOLERANCE,
                "{} @ {target}/sup={sup}: mean|Δ| {d:.3} exceeds the {PERCEPTUAL_TOLERANCE} \
                 tolerance — the scaled decode is not showing the same picture",
                s.name
            );
            // v0.8.103 (V7): a row the CODEC approved a stop for and which STILL comes back
            // bit-identical is not a pass — the ladder was handed a usable door and did not walk
            // through it (two different resample chains cannot agree to the byte). Gated on the probe
            // and not on `div`, because `div > 1` alone is satisfied on a codec with no real
            // decode-at-scale, for which rung 3 (and `d == 0.000`) is the DOCUMENTED right answer.
            if div > 1 && codec_approves {
                assert!(
                    d > 0.0,
                    "{} @ {target}/sup={sup}: rung 1's own acceptance test approved the 1/{div} stop \
                     for this file, but the scaled frame is BIT-IDENTICAL to the full decode — the \
                     ladder fell to rung 3 with a usable stop in hand (a broken transform cast, or \
                     both CopyPixels and the scaler failing)",
                    s.name
                );
            }
            // v0.8.104 (rider Y4): counted INDEPENDENTLY of the assert branch above. Incrementing it
            // inside `if div > 1 && codec_approves` made the suite-level guard a restatement of the
            // per-row one — it could only be 0 when `approved` was 0, i.e. exactly when the guard is
            // skipped — and the summary line reported the approval count a second time rather than
            // pixel evidence. The condition here is the OBSERVATION ("a stop engaged and the pixels
            // really differ"), so the two checks now come from different facts.
            if div > 1 && d > 0.0 {
                with_pixels += 1;
            }
        }
    }
    divisors_seen.sort_unstable();
    eprintln!(
        "S2 worst mean|Δ| across the testkit: {worst:.3} (tolerance {PERCEPTUAL_TOLERANCE}); \
         {engaged}/{} rows engaged a stop arithmetically, {approved} of those approved by the codec, \
         {with_pixels} came back with different pixels; divisors {divisors_seen:?}",
        shots.len() * 4
    );
    // The engagement guard S1 has had since v0.8.101 and S2 did not (F8). Arithmetic-only, so it is
    // codec-capability-independent — an assertion about the STOP LADDER's code, not about this box.
    assert!(
        engaged > 0,
        "no row engaged S2's decode-at-scale — S2 is not actually running, and every green row \
         above is rung 3 compared against rung 3"
    );
    // An assertion about THIS testkit (48 MP + 12 MP iPhone masters), not about the world: both real
    // stops must carry pixels somewhere, or one of them is shipping with no coverage at all.
    assert!(
        divisors_seen.contains(&4) && divisors_seen.contains(&8),
        "the sweep must exercise BOTH the 1/4 and the 1/8 stop on real files (saw {divisors_seen:?})"
    );
    // v0.8.103 (V7): the suite-level half. On a box whose codec approved a stop ANYWHERE, at least one
    // row must have produced pixel evidence — otherwise S2 is dead on a machine that could run it,
    // and the per-row guards above were all vacuous. On a codec with no real decode-at-scale
    // `approved` is 0 and this is silently satisfied, which is the whole point.
    if approved > 0 {
        assert!(
            with_pixels > 0,
            "this codec approved {approved} stop(s), so S2 is reachable here — yet no row differed \
             from the full decode, which means nothing on the scaled path actually ran"
        );
    } else {
        eprintln!(
            "S2 NOTE: this machine's HEIF codec approved no stop at any tested target (it has no \
             real decode-at-scale — the v0.8.99 probe's documented case). Every engaged row above \
             correctly took rung 3, so this run proves nothing about rungs 1 and 2."
        );
    }
}

/// v0.8.116 (R3) — the derive when the detail tier does NOT decode at native.
///
/// The v0.8.115 round measured, and recorded, "bit-identical to the decoded fast frame wherever the
/// codec did not subsample". True — and true of ONE configuration: the detail tier decoding the master
/// at full size. Falcon ships a **res limit** that caps the detail decode (`ddim`), and the derive's
/// own guard only requires the master to be at least as large as the fast tier's ask — so under a cap
/// the derive is a reduction of an ALREADY-reduced master, and a two-step Lanczos is not bit-identical
/// to a one-step one. The claim needed narrowing (it is narrowed in logic.md), and the regime it
/// excludes needed a row of its own, because it is a shipping configuration and nothing measured it.
///
/// The claim here is the perceptual one: a res-limited derive is the SAME PICTURE the fast tier would
/// have decoded — [`PERCEPTUAL_TOLERANCE`], the yardstick S1 and S2 are held to — at the same size.
/// That is what makes the res limit safe to combine with the derive, and it is a strictly weaker
/// statement than the native row's bit-identity, which is the point.
///
/// The cap is 4096 on the long side, comfortably above the 2176 px fast ask for every file here — so
/// no row lands on the floor-refusal path (the `master_long >= ask` assertion states that).
///
/// v0.8.117 (audit correction): the cap does NOT sit below every master here, and this doc used to
/// say it did. The 48 MP master (6048×8064) IS res-limited, so its rows measure the two-step
/// reduction this rider exists for. The 12 MP master is 3024×4032 — long side 4032, UNDER the cap —
/// so the detail tier decodes it at NATIVE and its rows measure the native regime, the one the
/// v0.8.115 bit-identity claim already covered. That is not a fault in the row (a native-master
/// derive is the same picture too, and the tolerance holds either way) — it is a fault in the
/// description, and the master dims printed per row below say which regime each one was in.
///
/// FALSIFIER (L28): derive from the res-limited master but compare against a NATIVE-master derive and
/// the row still passes — the two are close; compare against the fast tier's own decode (what this row
/// does) with the tolerance dropped to 0.0, i.e. re-assert the v0.8.115 bit-identity claim under a cap,
/// and every row fails, which is exactly the narrowing this rider exists to record. Remove the size
/// floor in the detail worker and the cap can fall below the ask, where `derive_fast_rgba` would UPSCALE
/// the master into the tier's bucket — the `master_long >= ask` assertion below is what states that the
/// rows measured here are the derivable regime.
#[test]
fn a_derive_from_a_res_limited_master_is_the_same_picture() {
    if !hw_child::run_here(module_path!(), "a_derive_from_a_res_limited_master_is_the_same_picture") { return; }
    let Some(shots) = heic_shots_or_skip("res-limited derive") else { return };
    const RES_LIMIT: u32 = 4096; // a shipping res-limit value, above the fast ask and below the masters
    let mut worst = 0.0f64;
    let mut rows = 0usize;
    for s in &shots {
        // The master the detail tier would hand the derive worker UNDER THE CAP — `Lane::Native` at
        // the res limit rather than at the full-res cap.
        let master = browse_frame_rgba(s, RES_LIMIT, true, Lane::Native).expect("res-limited decode");
        for (target, sup) in [(2176u32, true), (2176, false)] {
            let ask = fast_decode_target(target, sup);
            // The derive's own production guard: a master smaller than the ask is never derived from.
            assert!(
                master.w.max(master.h) >= ask,
                "{}: the {RES_LIMIT} px cap must stay above the {ask} px ask for this row to measure \
                 the derivable regime",
                s.name
            );
            let dec = browse_frame_rgba(s, target, sup, Lane::Fast).expect("fast-lane HEIC decode");
            let (der, rw, rh) = derive_fast_rgba(&master.rgba, master.w, master.h, target, sup);
            if (dec.w, dec.h) != (rw, rh) {
                // The decoded frame kept a covering stop inside the B2 near-stop slack (the size
                // divergence the native row documents). Nothing to compare pixel-for-pixel.
                eprintln!(
                    "res-limited derive {:<20} {target}/sup={sup:<5} SIZE DIVERGENCE: decoded {}x{} \
                     vs derived {rw}x{rh} (same dim bucket)",
                    s.name, dec.w, dec.h
                );
                continue;
            }
            let d = mean_abs_diff(&dec.rgba, &der);
            worst = worst.max(d);
            rows += 1;
            eprintln!(
                "res-limited derive {:<20} {target}/sup={sup:<5} {rw}x{rh} master={}x{} mean|Δ|={d:.3}",
                s.name, master.w, master.h
            );
            assert!(
                d <= PERCEPTUAL_TOLERANCE,
                "{} @ {target}/sup={sup}: mean|Δ| {d:.3} exceeds {PERCEPTUAL_TOLERANCE} — a derive \
                 from a {RES_LIMIT} px master is no longer the picture the fast tier would decode",
                s.name
            );
        }
    }
    eprintln!(
        "res-limited derive worst mean|Δ|: {worst:.3} over {rows} compared rows (bound \
         {PERCEPTUAL_TOLERANCE}; the NATIVE-master rows demand bit-identity instead)"
    );
    assert!(rows > 0, "no row compared pixels — the res-limited regime has no evidence in this run");
    // …and the narrowing itself: under a cap the frames are NOT bit-identical, which is precisely why
    // the v0.8.115 claim needed its "when the detail tier decodes at native" condition. A run where
    // every row came back exact would mean the cap never engaged.
    assert!(
        worst > 0.0,
        "every res-limited row was bit-identical — the {RES_LIMIT} px cap cannot have engaged, so this \
         run says nothing about the regime the row exists to bound"
    );
}

/// v0.8.115 (DERIVE-DON'T-DECODE) — the DERIVED fast frame, on the real 48 MP Display-P3 files.
///
/// The round's premise is that one decode can serve both browse tiers on this lane: the detail tier
/// decodes the master anyway, so the fast/scrub frame is a Lanczos reduction of pixels that already
/// exist rather than a second full HEVC decode ("heic S2: no power-of-two stop covers a 2176 px ask
/// from a 6048x8064 master — full decode + Lanczos"). That premise is only worth anything if the
/// derived frame is the SAME PICTURE the fast tier would have decoded, at the same size, on the real
/// files — the synthetic row in `lib.rs` cannot speak for an HEVC master in Display P3.
///
/// The claims, per file and per scrub target:
///   1. **Same frame size** whenever the fast lane's own decode did not take a covering DCT/subsample
///      stop — which is the 48 MP × 2176 px case this round is measured on. Where a stop IS covered
///      and the B2 near-stop rule keeps it, the derived frame lands on exactly the ask instead; the
///      row states that relationship rather than asserting a false equality, and both frames are
///      filed under the same `dim` WANT bucket either way.
///   2. **Same picture** — mean absolute difference under [`PERCEPTUAL_TOLERANCE`], the same
///      yardstick S1 and S2 are held to. The pixels are Display P3 (non-sRGB) and are compared
///      BEFORE any colour transform, exactly as the production derive sees them.
///
/// The row prints every measurement, so the numbers in the round report are checkable.
///
/// FALSIFIER (L28): make `derive_fast_rgba` skip the near-stop branch and the equal-size rows still
/// pass while the covered-stop row's size relationship inverts; give `downscale_rgba` a different
/// filter and the mean|Δ| rows go from ~0 to well past the tolerance on every file.
#[test]
fn a_derived_fast_frame_is_the_same_picture_as_a_decoded_one() {
    if !hw_child::run_here(module_path!(), "a_derived_fast_frame_is_the_same_picture_as_a_decoded_one") { return; }
    let Some(shots) = heic_shots_or_skip("derived fast frame") else { return };
    let mut worst = 0.0f64;
    let mut same_size_rows = 0usize;
    let mut exact_rows = 0usize;
    for s in &shots {
        // The NATIVE master, exactly what the detail tier hands the derive worker (Lane::Native at
        // the full-res cap: no reduction, so these are the decoded master's own pixels).
        let native = browse_frame_rgba(s, 16384, true, Lane::Native).expect("native HEIC decode");
        let path = s.jpg.as_deref().expect("a HEIC shot carries its finished path");
        for (target, sup) in [(2176u32, true), (2176, false), (2880, true), (1440, false)] {
            let dec = browse_frame_rgba(s, target, sup, Lane::Fast).expect("fast-lane HEIC decode");
            let (der, rw, rh) =
                derive_fast_rgba(&native.rgba, native.w, native.h, target, sup);
            // Did the CODEC scale this decode? Rung 1's own acceptance test, asked at the same
            // target the decode used — the S2 row's probe, reused. It is what tells the two regimes
            // apart: an unscaled decode IS the master, so the derive must match it to the BYTE; a
            // scaled one is a different rendering of the same picture and gets the perceptual bound.
            let scaled = heic_decode_at_scale_engages(path, fast_decode_target(target, sup));
            if (dec.w, dec.h) == (rw, rh) {
                same_size_rows += 1;
                let d = mean_abs_diff(&dec.rgba, &der);
                worst = worst.max(d);
                eprintln!(
                    "derive {:<20} {target}/sup={sup:<5} {rw}x{rh} scaled={scaled:<5} \
                     mean|Δ|={d:.3}  (decoded {} ms vs one decode already paid)",
                    s.name, dec.dec_ms
                );
                if !scaled {
                    exact_rows += 1;
                    // THE ROUND'S OWN CASE: no stop covers a 2176 px ask from a 48 MP master, so
                    // the fast decode is the master + Lanczos and the derive is the same master
                    // through the same Lanczos. Not "close" — identical. If this ever stops being
                    // exact, the derive has silently become a different rendering.
                    assert_eq!(
                        d, 0.0,
                        "{} @ {target}/sup={sup}: the codec did NOT scale this decode, so the fast \
                         frame and the derived frame are the same master through the same Lanczos \
                         — they must be BIT-IDENTICAL, and this run measured mean|Δ| {d:.3}",
                        s.name
                    );
                } else {
                    // A scaled decode is the codec's own subsample; the derive is a full Lanczos
                    // reduction of the master. Different renderings of one picture — and the
                    // DERIVED one is the better of the two (S2's own row records the same rung at
                    // 7.578 / 12.216 when compared at the stop's raw size, noting the subsample is
                    // "genuinely more ALIASED than a proper 8:1 reduction"). The bound is set above
                    // that measured worst so a codec revision does not turn this red, and far below
                    // what a channel swap, a mirror or a gamut error produces (the tens).
                    assert!(
                        d <= DERIVE_STOP_TOLERANCE,
                        "{} @ {target}/sup={sup}: mean|Δ| {d:.3} exceeds the \
                         {DERIVE_STOP_TOLERANCE} stop-vs-master tolerance — the derived frame is \
                         not the picture the fast tier would have decoded",
                        s.name
                    );
                }
            } else {
                // The fast decode kept a covering stop inside the B2 near-stop slack; the derive
                // lands on the ask itself. Bounded, and the derived frame is never the LARGER one.
                let ask = fast_decode_target(target, sup);
                assert_eq!(
                    (rw, rh),
                    scaled_dims(native.w, native.h, ask),
                    "{}: the derive always lands exactly on the tier's own ask",
                    s.name
                );
                assert!(
                    dec.w.max(dec.h) <= target + target / 8,
                    "{} @ {target}/sup={sup}: the decoded frame's kept stop ({}x{}) is outside the \
                     near-stop slack, so this is not the documented size divergence",
                    s.name, dec.w, dec.h
                );
                eprintln!(
                    "derive {:<20} {target}/sup={sup:<5} SIZE DIVERGENCE: decoded {}x{} kept its \
                     covering stop, derived {rw}x{rh} lands on the ask (same dim bucket)",
                    s.name, dec.w, dec.h
                );
            }
        }
    }
    eprintln!(
        "derive worst mean|Δ| across the testkit: {worst:.3} (unscaled rows must be 0.000; scaled \
         rows tolerate {DERIVE_STOP_TOLERANCE}); {same_size_rows} of {} rows compared \
         pixel-for-pixel, {exact_rows} of them BIT-IDENTICAL",
        shots.len() * 4
    );
    // The row must actually compare pixels somewhere, or it is asserting nothing. The 48 MP × 2176
    // case — the one the round is measured on — has no covering stop, so this cannot be vacuous on
    // the owner's testkit.
    assert!(
        same_size_rows > 0,
        "no row compared a derived frame against a decoded one at the same size — the derive's \
         fidelity claim has no pixel evidence in this run"
    );
    // …and at least one of those must be an UNSCALED row. That is the case the round's premise
    // rests on (one decode serving both tiers with no fidelity change at all); a run in which every
    // row went through a codec subsample would only have proven the weaker perceptual claim.
    assert!(
        exact_rows > 0,
        "no row proved bit-identity — every comparison went through a codec subsample, so this run \
         says nothing about the derive on the lane the round is measured on"
    );
}

/// S1 — the embedded-preview thumb lane.
///
/// The claims:
///   1. The 256 px tile is the SAME SIZE it was before, whichever door served it.
///   2. When the preview door IS used, the tile shows the same picture as a full decode would have
///      — the guard that catches a preview item the codec hands back rotated or in the wrong shape
///      (the `irot`-on-the-master-only hazard the aspect check exists for).
///   3. The lane is honest about which door it used, and it is allowed to decline: a file with no
///      usable preview must still produce a correct tile via the main image.
///
/// FALSIFIER (L28): remove the `min_long` coverage guard in `wic_thumbnail_rgb24` and a camera that
/// writes a 160 px preview would serve a BLURRY 160 px tile where a 256 px one is expected — the
/// dims row catches it. Remove the aspect guard and a portrait file whose preview came back
/// landscape passes the dims row but blows the mean|Δ| row apart.
#[test]
fn s1_embedded_preview_thumb_matches_the_full_decode() {
    if !hw_child::run_here(module_path!(), "s1_embedded_preview_thumb_matches_the_full_decode") { return; }
    let Some(shots) = heic_shots_or_skip("S1 preview thumb") else { return };
    let mut previews = 0usize;
    for s in &shots {
        let old = browse_frame_rgba(s, 256, true, Lane::Native).expect("classic HEIC thumb");
        let new = browse_frame_rgba(s, 256, true, Lane::Thumb).expect("S1 HEIC thumb");
        assert_eq!(
            (new.w, new.h),
            (old.w, old.h),
            "{}: the thumb tile changed SIZE ({}x{} vs {}x{})",
            s.name,
            new.w,
            new.h,
            old.w,
            old.h
        );
        let d = mean_abs_diff(&old.rgba, &new.rgba);
        eprintln!(
            "S1 {:<20} {}x{}  source={:?}  mean|Δ|={d:.3}  old {} ms / new {} ms",
            s.name, new.w, new.h, new.source, old.dec_ms, new.dec_ms
        );
        match new.source {
            FrameSource::EmbeddedPreview => {
                previews += 1;
                assert!(
                    d <= PERCEPTUAL_TOLERANCE,
                    "{}: the preview-sourced tile differs from the decoded one by {d:.3} — wrong \
                     item, wrong orientation, or wrong colour",
                    s.name
                );
            }
            // A decline is a legitimate answer (see the fail-soft contract). v0.8.102 (F6/F10):
            // what it falls to is S2's STOP LADDER, not the classic decode — `decode_heic_lane`'s
            // very next arm catches `Lane::Thumb` too, and at a 256 px ask the 1/8 stop qualifies on
            // any master ≥ 2048 px (the sibling test pins `heic_stop_divisor(8064, 256) == 8`). So
            // the tile is a Lanczos reduction of a 1008×756 stop while `Lane::Native`'s is a Lanczos
            // reduction of 8064×6048: the same picture, provably NOT the same bytes. The old
            // `assert_eq!(d, 0.0)` encoded a ladder Falcon does not have, and the first Android /
            // Canon / edited HEIC to decline would have turned this row red for correct behaviour.
            FrameSource::MainImage => assert!(
                d <= PERCEPTUAL_TOLERANCE,
                "{}: the preview lane declined, so the tile came through S2's stop ladder — it must \
                 still be the same picture as the classic decode, but mean|Δ| is {d:.3}",
                s.name
            ),
        }
    }
    eprintln!("S1: {previews}/{} testkit files served from an embedded preview", shots.len());
    // Not an assertion about the world — an assertion about THIS testkit, which is 5 iPhone files
    // that all carry previews. If this ever fires, the S1 door stopped opening and the round's
    // headline number is gone; the log note will say which decline reason it hit.
    assert!(previews > 0, "no testkit file used the S1 preview door — S1 is not actually running");
}

/// The COLOUR regression guard. The scaled decode must not disturb the source-gamut plumbing: the
/// tier reads the gamut from the FILE (`shot_source_gamut` → `heic_color_tag` → the container's
/// `colr` box), never from the decoded pixels, and WIC's format converter does no colour management
/// — so a P3 HEIC stays P3-tagged and its pixels stay in P3 whichever rung served them.
///
/// v0.8.102 (F7): the comparison now happens at a target that actually TAKES a stop. v0.8.101 asked
/// for `(2048, supersample=true)`, i.e. a decode target of 2048 — and engaging the 1/4 stop at 2048
/// needs an 8192 px master, which no iPhone file is (the suite pins `heic_stop_divisor(8064, 2048)
/// == 1` itself). So the "…through the scaled path" test compared a full decode against a full
/// decode on 100% of its rows: a colour guard containing no evidence about colour under scaling.
/// `(2880, supersample=false)` gives a decode target of 1440, which is the 1/4 stop on the 48 MP
/// files, and the loop now COUNTS the rows that engaged and refuses to pass with none.
///
/// FALSIFIER (L28): initialise the format converter with a colour-managing destination (or route
/// the scaled path through a different pixel-format door that transcodes primaries) and the
/// mean|Δ| between the scaled and the full decode jumps well past the tolerance while the gamut
/// tag stays P3 — the exact silent-wrong-colour failure this row exists to catch. Revert the target
/// to the old 2048/supersample pair and the engagement assertion fires, because that pair cannot
/// reach the scaled path at all.
#[test]
fn p3_heic_keeps_its_source_gamut_through_the_scaled_path() {
    if !hw_child::run_here(module_path!(), "p3_heic_keeps_its_source_gamut_through_the_scaled_path") { return; }
    let Some(shots) = heic_shots_or_skip("P3 colour guard") else { return };
    let mut p3 = 0usize;
    let mut scaled_rows = 0usize;
    // sup=false halves the ask: 2880 → a 1440 px decode target, where 8064/4 = 2016 covers.
    let (target, sup) = (2880u32, false);
    let decode_target = (target / 2).max(1);
    for s in &shots {
        let g = shot_source_gamut(s);
        let (sw, sh) = source_dimensions(s).expect("HEIC header dims");
        let div = heic_stop_divisor(sw.max(sh), decode_target);
        eprintln!("colour {:<20} source gamut = {g:?}  {sw}x{sh} → 1/{div} at {decode_target}", s.name);
        if g == falcon_color::Gamut::DisplayP3 {
            p3 += 1;
        }
        if div > 1 {
            scaled_rows += 1;
        }
        // The gamut is a property of the FILE, so it cannot depend on which lane decoded it — but
        // the tier reads it per decode, so pin that it is stable across repeated reads.
        assert_eq!(shot_source_gamut(s), g, "{}: the source gamut must be deterministic", s.name);
        let old = browse_frame_rgba(s, target, sup, Lane::Native).expect("classic decode");
        let new = browse_frame_rgba(s, target, sup, Lane::Fast).expect("scaled decode");
        assert!(
            mean_abs_diff(&old.rgba, &new.rgba) <= PERCEPTUAL_TOLERANCE,
            "{}: the scaled path shifted the pixels — a colour-managing rung would look exactly \
             like this",
            s.name
        );
    }
    assert!(p3 > 0, "the iPhone testkit should carry at least one Display P3 file");
    assert!(
        scaled_rows > 0,
        "no testkit file engaged a stop at a {decode_target} px decode target — this test would be \
         comparing the full decode against itself and proving nothing about colour under scaling"
    );
}

/// v0.8.105 (W5): the S1 preview door's COLOUR assumption, pinned on the real files.
///
/// v0.8.104 (C2) made the thumb worker colour-manage its tile — and `Lane::Thumb` is the one lane
/// allowed to serve those pixels from the file's EMBEDDED PREVIEW, while the gamut it converted FROM
/// came from the MASTER's `colr`. A preview declaring a different space would be converted with the
/// wrong source and ship visibly desaturated beside a correct stage, **on the shipped sRGB default**.
/// The tree now reads the preview item's OWN declaration (`frame_source_gamut` →
/// `heic_preview_color_desc`); this row is the evidence that on the installed files the two agree,
/// so C2's transform was right about these photographs and stays right.
///
/// The container makes the row non-trivial: each of these HDR HEICs carries 3–5 `colr` boxes (the
/// gain-map and tone-map auxiliary items declare "sRGB IEC61966-2.1 Linear" / "Display P3 Linear" /
/// "Display P3 Primaries; PQ …"), so "the file declares Display P3" is a statement about WHICH item
/// is being read, not about the file.
///
/// FALSIFIER (L28): a file whose preview item associates its own, different `colr` fails the
/// agreement row naming both gamuts. Make `heic_preview_color_desc` return the FIRST `colr` in the
/// container instead of the thumbnail item's and this row still passes — which is exactly why the
/// attribution itself is falsified separately, on a synthetic container, by
/// `heic_preview_colour_comes_from_the_thumbnail_item` (lib.rs).
///
/// v0.8.141 (R13/B10) — THE ROW NOW ASSERTS THE SHIPPING PATH. It used to compare
/// `Gamut::from_description(preview_desc)` against the master's gamut, which is the RETIRED name
/// matcher: after v0.8.140 the preview door resolves its own `colr` by COLORIMETRY through
/// `frame_source_gamut`, so the old assertion could pass while the real door disagreed with the
/// master (a name and its colorants can differ — that is the whole round), and it could fail on a
/// file whose preview profile is correctly named something the matcher does not know. The claim
/// this row exists to make is "the preview-sourced tile is converted from the same gamut as the
/// master", and that is now literally what it asks, of the two functions the app actually calls.
#[test]
fn the_embedded_preview_declares_the_masters_colour_space() {
    if !hw_child::run_here(module_path!(), "the_embedded_preview_declares_the_masters_colour_space") { return; }
    let Some(shots) = heic_shots_or_skip("preview/master colour agreement") else { return };
    let mut attributed = 0usize;
    let mut multi = 0usize;
    for s in &shots {
        let path = s.jpg.as_deref().expect("a HEIC shot has a finished-image path");
        let all = heic_color_descs(path);
        let preview = heic_preview_color_desc(path);
        let master = shot_source_gamut(s);
        let served = frame_source_gamut(s, FrameSource::EmbeddedPreview);
        eprintln!(
            "colr {:<20} preview declares {preview:?} → serves {served:?}  master gamut {master:?}  \
             (container carries {} colr boxes: {all:?})",
            s.name,
            all.len()
        );
        if all.len() > 1 {
            multi += 1;
        }
        assert_eq!(
            served, master,
            "{}: a tile served from the embedded preview would be converted FROM {served:?} while \
             the master is {master:?} — visibly wrong colour beside a correct stage",
            s.name
        );
        if preview.is_some() {
            attributed += 1;
        }
    }
    // ANTI-VACUITY: the rows above are only worth something if the walk actually resolved a preview
    // item, and the "which item" question is only interesting because the containers are plural.
    assert!(attributed > 0, "no testkit HEIC resolved a preview colour — the agreement proved nothing");
    assert!(multi > 0, "no testkit HEIC carried more than one colr box — attribution is untested here");
}

/// The DETAIL tier is byte-unchanged: `Lane::Native` must give the same answer it gave in
/// v0.8.100, i.e. a FULL native decode then Lanczos. Proven behaviourally — at the detail cap the
/// source fits, so the frame comes back at its native size, and the scaled rungs (which would
/// produce the same dims but different pixels) are demonstrably not in the path because `Lane::Fast`
/// at the SAME target is bit-identical only when no scaling happened at all.
///
/// FALSIFIER (L28): let `decode_heic_lane` take the S2 rung for `Lane::Native` and the equality
/// below fails on any file larger than the target — which is the whole reason the detail tier has
/// its own lane rather than sharing the fast tier's.
#[test]
fn the_detail_lane_still_decodes_natively() {
    if !hw_child::run_here(module_path!(), "the_detail_lane_still_decodes_natively") { return; }
    let Some(shots) = heic_shots_or_skip("detail lane") else { return };
    for s in &shots {
        let (nw, nh) = source_dimensions(s).expect("HEIC header dims");
        // The real detail cap on the owner's card. Every iPhone HEIC (12 MP and 48 MP) fits it.
        let f = browse_frame_rgba(s, 8192, true, Lane::Native).expect("detail decode");
        assert_eq!(
            (f.w, f.h),
            (nw, nh),
            "{}: the detail tier must hand back the NATIVE frame, not a scaled one",
            s.name
        );
        assert_eq!(f.source, FrameSource::MainImage, "{}: never a preview", s.name);
        eprintln!("detail {:<20} {}x{} in {} ms (+{} ms expand)", s.name, f.w, f.h, f.dec_ms, f.exp_ms);
    }
}

/// S4 — ORIENTATION. The owner's report: "vertical HEICs are shown as horizontal like the sample
/// IMG_2814.HEIC". That file stores LANDSCAPE pixels (`ispe` 8064×6048) plus the container's `irot`
/// = one quarter-turn, and Apple restates the same turn in EXIF `Orientation` = 6. The Windows HEIF
/// decoder honours `irot` and hands back 6048×8064 — already upright — and Falcon then applied the
/// EXIF tag on top, turning the upright photo onto its side.
///
/// This test asserts the whole chain on the real file:
///   1. The DECODED frame is portrait on every tier — and the three tiers AGREE, which is the bar
///      the round holds above everything else (tiers disagreeing about rotation is strictly worse
///      than all tiers being consistently wrong).
///   2. The base orientation the render pipeline is handed is UPRIGHT — no second rotation.
///   3. The info panel's Dimensions row reports what is on screen, not the pre-rotation size.
///
/// FALSIFIER (L28): revert `read_orientation` to the plain `exif_orientation` read and row 2 fails
/// with `Some(6)` — the exact double-rotation the owner saw. Make S1's preview door skip the aspect
/// guard, or ask the S2 transform for a rotation other than `Rotate0`, and row 1's cross-tier
/// agreement fails on this file while every other test in this file still passes.
#[test]
fn a_portrait_heic_is_portrait_on_every_tier() {
    if !hw_child::run_here(module_path!(), "a_portrait_heic_is_portrait_on_every_tier") { return; }
    let Some(shots) = heic_shots_or_skip("S4 orientation") else { return };
    let mut portraits = 0usize;
    for s in &shots {
        let (dw, dh) = source_dimensions(s).expect("HEIC header dims");
        let t = browse_frame_rgba(s, 256, true, Lane::Thumb).expect("thumb");
        let f = browse_frame_rgba(s, 2880, true, Lane::Fast).expect("fast");
        let d = browse_frame_rgba(s, 8192, true, Lane::Native).expect("detail");
        let base = read_orientation(s, false);
        eprintln!(
            "S4 {:<20} decoded={dw}x{dh} base_orientation={base:?} thumb={}x{} fast={}x{} detail={}x{}",
            s.name, t.w, t.h, f.w, f.h, d.w, d.h
        );
        // (1) CROSS-TIER AGREEMENT — every tier must be the same shape as the decoded frame.
        let portrait = dh > dw;
        for (tier, w, h) in [("thumb", t.w, t.h), ("fast", f.w, f.h), ("detail", d.w, d.h)] {
            assert_eq!(
                h > w,
                portrait,
                "{}: the {tier} tier is {}, the source is {} ({w}x{h} vs {dw}x{dh}) — the tiers \
                 disagree about rotation",
                s.name,
                if h > w { "portrait" } else { "landscape" },
                if portrait { "portrait" } else { "landscape" }
            );
        }
        // (2) NO SECOND ROTATION. The decoder already applied the container's turn, so what the
        //     render pipeline still has to apply is nothing.
        assert_eq!(
            base.map(orientation_to_turns),
            Some(0),
            "{}: base turns must be 0 — the WIC frame is already upright, so any non-zero base is \
             the double-rotation that put the owner's portrait shots on their side",
            s.name
        );
        // (3) The panel agrees with the screen.
        let dims_row = exif_rows(s, 0).into_iter().find(|(k, _)| k == "Dimensions");
        assert_eq!(
            dims_row.map(|(_, v)| v),
            Some(format!("{dw} × {dh}")),
            "{}: the Dimensions row must report the DECODED frame, not EXIF's pre-rotation pair",
            s.name
        );
        if portrait {
            portraits += 1;
        }
    }
    assert!(portraits > 0, "IMG_2814.HEIC is portrait — if none are, the testkit changed");
}

/// S4, the pure half: the rule that decides whether the EXIF tag still needs applying. Runs on
/// EVERY machine — no codec, no file — which matters because it is also the rule the untestable
/// macOS arm will take.
///
/// FALSIFIER (L28): named in `orientation_after_decoder_transform`'s own doc — drop the
/// square-source term and a square image silently loses its rotation; drop the transpose
/// comparison and every rotated JPEG stops rotating.
#[test]
fn the_orientation_rule_only_drops_a_turn_the_decoder_already_made() {
    // The IMG_2814 case: stored landscape, tag says quarter-turn, decoder handed back portrait.
    assert_eq!(orientation_after_decoder_transform(6, (8064, 6048), (6048, 8064)), 1);
    assert_eq!(orientation_after_decoder_transform(8, (8064, 6048), (6048, 8064)), 1);
    // A decoder that did NOT rotate (every JPEG, and macOS Image I/O's CreateImageAtIndex): the
    // tag is returned untouched and the render pipeline rotates exactly as it always has.
    assert_eq!(orientation_after_decoder_transform(6, (8064, 6048), (8064, 6048)), 6);
    assert_eq!(orientation_after_decoder_transform(8, (3024, 4032), (3024, 4032)), 8);
    // Upright and 180° tags are pass-through — no dimension changes, so nothing is detectable.
    assert_eq!(orientation_after_decoder_transform(1, (8064, 6048), (6048, 8064)), 1);
    assert_eq!(orientation_after_decoder_transform(3, (8064, 6048), (8064, 6048)), 3);
    // A SQUARE source is its own transpose, so "the decoder already turned it" is unprovable and
    // must not be assumed — the tag stands.
    assert_eq!(orientation_after_decoder_transform(6, (4000, 4000), (4000, 4000)), 6);
    // Mirrored-and-rotated (5/7) — v0.8.102 (F14). The turn is consumed, so what is LEFT is the
    // MIRROR at zero turns: 2, not a bare 1. All a dims comparison can prove is a transpose; whether
    // the decoder also applied the container's `imir` is not observable here, and collapsing to 1
    // asserted it. The rotation answer is unchanged (`orientation_to_turns(2) == 0`), but
    // `orientation_is_mirrored(2)` is still true, so `note_orientation`'s once-per-session
    // "mirroring dropped, rotation honored" line can still fire for the files it describes.
    assert_eq!(orientation_after_decoder_transform(5, (8064, 6048), (6048, 8064)), 2);
    assert_eq!(orientation_after_decoder_transform(7, (8064, 6048), (6048, 8064)), 2);
    assert_eq!(orientation_to_turns(2), 0, "the residual still says 'no more rotation'");
    assert!(orientation_is_mirrored(2), "…and still says 'a mirror was dropped'");
    assert!(!orientation_is_mirrored(1), "the non-mirrored residual must not claim one was");
    // An unrelated decoded size (a scaled frame handed in by mistake) is not a transpose → no drop.
    assert_eq!(orientation_after_decoder_transform(6, (8064, 6048), (2880, 2160)), 6);
}

/// v0.8.103 (V2/V3/V12): `decoder_consumed_turns`'s arithmetic, on EVERY machine.
///
/// Two claims the tree made in prose and never pinned:
///   1. **It is FORMAT-BLIND.** v0.8.102's doc said the value is "0 for every JPEG, every PNG / TIFF
///      / WebP" as though the format decided it. Nothing in the code reads `shot.kind`: the rule
///      compares EXIF's stored dims against the CONTAINER HEADER dims, two unrelated metadata
///      sources. A PNG re-encoded from a rotated JPEG while carrying that JPEG's `eXIf` block
///      verbatim — a common converter behaviour — has transposed dims and a quarter-turn tag, and
///      gets 1. The precondition, not the format, is what makes the JPEG arms byte-unchanged.
///   2. **The macOS answer is DERIVED, not assumed.** The Mac write path rests on
///      `imageio_dimensions` reporting the PRE-`irot` `kCGImagePropertyPixelWidth/Height`, i.e. on
///      the decoded size EQUALLING the stored size — `CreateImageAtIndex` with
///      `kCGImageSourceCreateThumbnailWithTransform = false`. Feed that shape in and the answer is 0;
///      feed the transposed shape in and it is 1 on a Mac exactly as on Windows. The rule holds no
///      belief about either platform, which is the property that makes the Mac arm falsifiable by a
///      tester holding one portrait HEIC rather than by an argument.
///
/// FALSIFIER (L28): re-state the doc's "0 for every PNG/TIFF/WebP" as code (short-circuit on
/// `shot.kind != Heic`) and the PNG-shaped row below returns 0 while the read arm and the apply
/// report still disagree about that file by one quarter-turn. Make `imageio_dimensions` return the
/// POST-`irot` size on macOS (or Windows `wic_dimensions` the pre-`irot` one) and the platform rows
/// swap answers — which is the measurement, stated as a test rather than as a sentence.
#[test]
fn the_consumed_turns_rule_is_format_blind_and_platform_derived() {
    // Windows / WIC on IMG_2814: the decoder honoured `irot`, so one quarter-turn is already spent.
    assert_eq!(decoder_consumed_turns_from_dims(6, (8064, 6048), (6048, 8064)), 1);
    // macOS / Image I/O on the SAME file: `WithTransform = false`, decoded size == stored size, so
    // NOTHING was consumed and the render pipeline still owes the whole turn. This is the value the
    // Mac write path depends on, and it is computed here rather than asserted in prose.
    assert_eq!(decoder_consumed_turns_from_dims(6, (8064, 6048), (8064, 6048)), 0);
    // Every JPEG in a normal folder takes that same second row — and so does any decoder anywhere
    // that leaves the rotation to its caller.
    assert_eq!(decoder_consumed_turns_from_dims(8, (3024, 4032), (3024, 4032)), 0);
    // FORMAT-BLIND: a PNG whose EXIF block was copied from a differently-shaped JPEG. Same
    // arithmetic, same answer as the HEIC — the rule never asked what the file was.
    assert_eq!(
        decoder_consumed_turns_from_dims(6, (4032, 3024), (3024, 4032)),
        1,
        "the rule compares dims, not formats — 'always 0 for a PNG' is a claim about typical FILES"
    );
    // A transposing decoder consumes the WHOLE rotation, not one quarter of it: what is left is the
    // same mirror class at zero turns, so `consumed == turns_of(tag)`. 8 (270° CW) → residual 1 → 3.
    assert_eq!(decoder_consumed_turns_from_dims(8, (8064, 6048), (6048, 8064)), 3);
    // Mirrored-and-rotated: the TURN is consumed, the mirror is not (5 → residual 2, 7 → residual 2),
    // so the mirror class rides through untouched while the turn count still comes out right.
    assert_eq!(decoder_consumed_turns_from_dims(5, (8064, 6048), (6048, 8064)), 1);
    assert_eq!(decoder_consumed_turns_from_dims(7, (8064, 6048), (6048, 8064)), 3);
    // Nothing detectable: upright/180° tags, and a SQUARE master (its own transpose).
    assert_eq!(decoder_consumed_turns_from_dims(1, (8064, 6048), (6048, 8064)), 0);
    assert_eq!(decoder_consumed_turns_from_dims(3, (8064, 6048), (8064, 6048)), 0);
    assert_eq!(decoder_consumed_turns_from_dims(6, (4000, 4000), (4000, 4000)), 0);
    // And the bridge the whole absolute↔residual fork rests on: subtracting the consumed turns from
    // the file's own tag IS the residual `read_orientation` hands the render pipeline.
    for (o, stored, decoded) in [
        (6u32, (8064u32, 6048u32), (6048u32, 8064u32)),
        (8, (8064, 6048), (6048, 8064)),
        (5, (8064, 6048), (6048, 8064)),
        (7, (8064, 6048), (6048, 8064)),
        (6, (8064, 6048), (8064, 6048)),
        (3, (8064, 6048), (8064, 6048)),
    ] {
        let c = decoder_consumed_turns_from_dims(o, stored, decoded);
        assert_eq!(
            u32::from(orientation_minus_turns(o as u8, c)),
            orientation_after_decoder_transform(o, stored, decoded),
            "consumed must be exactly the turns S4's residual removed (o {o}, decoded {decoded:?})"
        );
    }
}

/// v0.8.102 (F4/F5): rung 1's acceptance test, on its own. It decides two things at once — whether
/// rung 1 may use the codec's stop, and (since this round) whether rung 2 is reachable at all — so
/// it is the one predicate in the S2 ladder worth pinning without a codec.
///
/// FALSIFIER (L28): drop the `< native` term and the "codec answers with the native size" row below
/// flips to accepted, which is the no-decode-at-scale case whose arbitrary-size ask this round's own
/// measurement records as a 2.6× LOSS; drop the `>= target_long` term and the undershoot row flips,
/// which is a scrub frame softer than the one v0.8.100 served.
#[test]
fn the_stop_acceptance_test_refuses_a_codec_that_cannot_really_shrink() {
    // The owner's codec on a 48 MP master at the sub tier: 1/4 = 2016×1512 for a 1440 ask.
    assert!(heic_stop_is_acceptable((2016, 1512), (8064, 6048), 1440));
    // Exactly covering is still covering.
    assert!(heic_stop_is_acceptable((2016, 1512), (8064, 6048), 2016));
    // The v0.8.99 probe's "the interface exists but the codec cannot decode at scale" answer: it
    // hands back the NATIVE size. Accepting it means decoding 1:1 and then resampling.
    assert!(!heic_stop_is_acceptable((8064, 6048), (8064, 6048), 1440));
    // A stop BELOW the ask undershoots — the frame would come back softer than v0.8.100's.
    assert!(!heic_stop_is_acceptable((2016, 1512), (8064, 6048), 2017));
    // Degenerate answers are refused, never divided by.
    assert!(!heic_stop_is_acceptable((0, 1512), (8064, 6048), 1440));
    assert!(!heic_stop_is_acceptable((2016, 0), (8064, 6048), 1440));
    // A portrait frame: the LONG side is what both tests are stated on, either way up.
    assert!(heic_stop_is_acceptable((1512, 2016), (6048, 8064), 1440));
    assert!(!heic_stop_is_acceptable((1512, 2016), (6048, 8064), 2017));
}

/// A unique, freshly-created temp dir. No `tempfile` crate in the tree — mirror the apply battery's
/// own idiom. Every HEIC the tests below mutate is a COPY that lives here; the testkit originals are
/// read-only to this whole file and the round-trip test asserts that at the end.
fn tmp_dir(what: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("falcon_{what}_{}_{nanos}_{n}", std::process::id()));
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

/// Copy `IMG_2814.HEIC` into a fresh temp dir and scan it into a real `Shot` — the only way a test
/// can drive the apply path without ever touching the owner's testkit.
fn heic_copy_shot(what: &str) -> Option<(std::path::PathBuf, std::path::PathBuf, Shot)> {
    let src = heic_dir().join("IMG_2814.HEIC");
    if !src.exists() {
        eprintln!("skip ({what}): {} missing", src.display());
        return None;
    }
    // v0.8.104 (rider Y2): the gate is the SCAN-SIDE predicate, not the WIC ladder's. These rows
    // drive orientation + `apply_rotation`, which need a real DECODED frame size to compare EXIF's
    // stored dims against — and macOS gets one from Image I/O, which has nothing to do with WIC.
    // Gating them on `wic_heif_codec_present()` (a `#[cfg]`'d-off `false` off Windows) meant a Mac
    // CI with FALCON_HEIC_TESTKIT set skipped every one of them and still reported green. The PURE
    // half of the same arithmetic runs everywhere either way, in
    // `the_consumed_turns_rule_is_format_blind_and_platform_derived`.
    let decodable = cfg!(target_os = "macos") || (cfg!(windows) && wic_heif_codec_present());
    if !decodable {
        if cfg!(windows) {
            eprintln!("skip ({what}): {}.", no_wic_reason());
        } else {
            eprintln!(
                "skip ({what}): this target has no HEIC decoder Falcon can drive (not Windows/WIC, \
                 not macOS/Image I/O) — these rows need a real decoded frame size."
            );
        }
        return None;
    }
    let dir = tmp_dir(what);
    let copy = dir.join("IMG_2814.HEIC");
    std::fs::copy(&src, &copy).expect("copy the testkit file into a temp dir");
    let shot = scan_folder(&dir)
        .expect("scan the temp dir")
        .into_iter()
        .find(|s| s.kind == SrcKind::Heic)?;
    Some((dir, copy, shot))
}

/// **The RED's pin (v0.8.102, F1/F2/F13).** The HEIC rotate → Apply ROUND TRIP, driven through the
/// real `apply_rotation` on a COPY of IMG_2814.HEIC — the test v0.8.101 did not have, and whose
/// absence is why the defect shipped. Its predecessor hand-fed the corrected base `1` into
/// `compose_exif_orientation` and asserted the arithmetic; the shipped path fed it the file's own
/// tag `6` instead, so the green test and the broken app were describing different programs.
///
/// The four things that must all be true at once, and the reason each one is separately asserted:
///   1. **What lands on disk is FILE-ABSOLUTE.** `compose(6, 1) = 3`. That is the convention
///      (`apply_rotation`'s doc): a Falcon sidecar states the same thing the embedded EXIF tag
///      states, so pre-v0.8.102 sidecars and third-party restatements need no migration.
///   2. **What comes back in the report is RESIDUAL.** `new_base_turns == base + delta`, because
///      that value is fed straight into `rot.note_base` and compared against the live display turns.
///   3. **A fresh read round-trips.** `read_orientation` after the write must equal exactly the
///      turns the user was looking at — this is the half that a sidecar bypassing S4 broke, and it
///      is what makes the fix survive a reload.
///
/// v0.8.103 (V5): there used to be a fourth row here, `rep.new_base_turns & 3 == eff_before`, credited
/// with "Apply is a visual NO-OP under auto-orient". It was a TAUTOLOGY of row 2 — `eff_before` is
/// defined as `(base + delta) & 3`, which is exactly what row 2 asserts — so it could not fail unless
/// row 2 did, and nothing here touched `compose_turns`, `RotState::effective` or the drain. The
/// invariant is real and it now has a real home on the native side, where the algebra that decides
/// eviction actually lives: `support.rs::apply_re_bases_without_moving_the_display` runs the drain's
/// own `set_delta` + `note_base` pair and asserts the composed turns are unchanged across it (and
/// that with auto-orient OFF they are NOT, which is why main.rs evicts there). The derivation this
/// test's report rests on is pinned in `apply/tests.rs::t_t_finished_report_is_derived_not_probed`.
///
/// Plus the second Apply (it must ADVANCE, not re-compose on the original) and the FOREIGN-sidecar
/// case: a `.HEIC.xmp` carrying the absolute value 6 — what an old Falcon session, Lightroom, or any
/// EXIF-honouring tool would write for this file — must read back as UPRIGHT, not as a quarter-turn.
/// That one is the pre-v0.8.101 double rotation coming back through a door the S4 fix did not cover.
///
/// v0.8.105 (W16): the two probe rows and the residuals downstream are PLATFORM-DERIVED now. Rider
/// Y2 let this test run on macOS but left the Windows/WIC answers hard-coded (`base == 0`,
/// `consumed == 1`), which are FALSE there by the tree's own documented rule — so the Mac CI the
/// rider exists to enable went red, blaming WIC on a machine with no WIC. See the derivation beside
/// the assertions; the ABSOLUTE sidecar values (3, then 8) are platform-independent and stay literal.
///
/// FALSIFIER (L28): make `apply_rotation` report the FILE-ABSOLUTE turns (the pre-v0.8.102 shape, or
/// equivalently the v0.8.102 shape with a `consumed` that degraded to 0) and row 2 fails with
/// `new_base_turns == 2` where 1 is required on Windows (3 where 2 is required on macOS) — the exact
/// "user asked for 90°, got 180°, and every
/// tier re-rendered it" the owner would have hit on the first rotated HEIC. Remove the
/// `decoder_consumed_turns` subtraction from `read_orientation`'s sidecar arm and row 3 and the
/// foreign-sidecar row fail with a quarter-turn where upright is required — the shipped-and-persisted
/// half of the same defect.
#[test]
fn a_heic_rotate_then_apply_round_trips_to_exactly_what_the_user_saw() {
    if !hw_child::run_here(module_path!(), "a_heic_rotate_then_apply_round_trips_to_exactly_what_the_user_saw") { return; }
    let src = heic_dir().join("IMG_2814.HEIC");
    let untouched = std::fs::metadata(&src).ok().map(|m| (m.len(), m.modified().ok()));
    let Some((dir, copy, shot)) = heic_copy_shot("heic_apply_roundtrip") else { return };

    // The state the user is in — and it is PLATFORM-DERIVED (v0.8.105 / W16). IMG_2814 stores
    // 8064×6048 with `irot` 3 and EXIF `Orientation` 6:
    //   * Windows/WIC HONOURS `irot`, so the decoded frame is the transpose (6048×8064),
    //     `orientation_after_decoder_transform` returns upright, `consumed == 1` and the app's
    //     residual base is 0 — one R press is one quarter-turn from what is on screen.
    //   * macOS/Image I/O does NOT (`WithTransform = false` / `CreateImageAtIndex`), so decoded ==
    //     stored, the tag comes back verbatim (6), `consumed == 0` and the residual base is the
    //     FULL EXIF turn (1).
    // Rider Y2 un-gated these rows for macOS but left the Windows answers hard-coded, so the exact
    // configuration it exists to enable went red with a message blaming WIC on a machine that has
    // none. Everything downstream is re-expressed in terms of `base`; the ABSOLUTE values written to
    // the sidecar (compose(6,1)=3, then 8) are platform-independent and stay literal.
    let (want_base, want_consumed) = if cfg!(target_os = "macos") { (1u8, 0u8) } else { (0u8, 1u8) };
    let base = read_orientation(&shot, false).map(orientation_to_turns).expect("a base orientation");
    let consumed = decoder_consumed_turns(&shot);
    eprintln!("apply: base_turns={base} decoder_consumed_turns={consumed} (want {want_base}/{want_consumed})");
    assert_eq!(
        base, want_base,
        "the residual base is the EXIF turn MINUS whatever this platform's decoder already spent"
    );
    assert_eq!(
        consumed, want_consumed,
        "Windows/WIC honours the container's irot (1); macOS/Image I/O does not (0)"
    );

    let delta = 1u8;
    let eff_before = (base + delta) & 3; // what the user is looking at when they hit Apply

    // v0.8.103 (V1): the plan carries NO `decoder_consumed_turns`. `apply_rotation` derives it from
    // `base_turns` and the absolute value it reads, so the `consumed` printed above is measured here
    // only to pin what WIC did — it is not an input, and no failure of that probe can reach the write
    // path any more. (v0.8.102 fed this exact value in, which is why its own pin could not see the
    // degraded-probe branch: the test always supplied a successful one.)
    let rep = apply_rotation(&RotApplyPlan {
        finished: Some(copy.clone()),
        finished_is_jpeg: false, // `is_jpeg_source()` for a HEIC — the non-JPEG sidecar arm
        raw: None,
        base_turns: base,
        delta,
    });
    assert!(rep.ok, "the sidecar write must succeed: {:?}", rep.finished_action);
    // (1) FILE-ABSOLUTE on disk.
    // 3 is `compose_exif_orientation(6, 1)` and nothing else composes to it from a single clockwise
    // turn, so this row also pins that the write side read the file's own raw tag (6), not the app's
    // residual base (0/upright) — the FILE-ABSOLUTE half of the convention, asserted on real bytes.
    assert_eq!(
        sidecar_orientation(&copy, false),
        Some(3),
        "the sidecar must carry compose(6, 1) = 3 — the same terms the embedded tag speaks"
    );
    // (2) RESIDUAL in the report.
    assert_eq!(
        rep.new_base_turns,
        (base + delta) & 3,
        "new_base_turns is what `rot.note_base` re-bases on; it must be in the app's own residual \
         terms, not the written value's absolute turns"
    );
    // (3) A fresh read round-trips to exactly the turns the user saw.
    assert_eq!(
        read_orientation(&shot, false).map(orientation_to_turns),
        Some(eff_before),
        "reloading the folder must show the photo where the user left it — the sidecar arm has to \
         subtract the same consumed turns S4 subtracts from the embedded tag"
    );
    // A SECOND Apply advances on what the first wrote (item 9) — in both coordinate systems.
    let rep2 = apply_rotation(&RotApplyPlan {
        finished: Some(copy.clone()),
        finished_is_jpeg: false,
        raw: None,
        base_turns: rep.new_base_turns,
        delta,
    });
    assert!(rep2.ok);
    assert_eq!(sidecar_orientation(&copy, false), Some(8), "compose(3, 1) = 8, absolute");
    // Residual again: turns(8) = 3, minus this platform's consumed. Windows 2, macOS 3 — i.e.
    // `base + 2` either way, which is what "two presses" MEANS.
    assert_eq!(
        rep2.new_base_turns,
        (base + 2) & 3,
        "…and two quarter-turns outstanding on top of the base the decoder left, residual"
    );
    assert_eq!(
        read_orientation(&shot, false).map(orientation_to_turns),
        Some((base + 2) & 3),
        "two presses, two quarter-turns — the second Apply must not re-compose on the original tag"
    );

    // The FOREIGN / PRE-v0.8.102 sidecar: absolute 6 (what any EXIF-honouring tool writes for this
    // file) must read back UPRIGHT. Before v0.8.102 this branch returned 6 verbatim and put the
    // owner's original sideways-HEIC bug straight back on screen for any file that had a sidecar.
    if let Some((dir2, copy2, shot2)) = heic_copy_shot("heic_foreign_sidecar") {
        write_xmp_sidecar(&sidecar_path_for(&copy2, false), 6).expect("plant a foreign sidecar");
        assert_eq!(sidecar_orientation(&copy2, false), Some(6), "planted, absolute");
        // 6 is this file's OWN embedded tag, so a sidecar restating it must read back exactly like
        // no sidecar at all — `base`. On Windows that is 0 (the decoder already turned it, and
        // reading the 6 verbatim is the pre-v0.8.101 double rotation, re-armed); on macOS it is 1,
        // because there the quarter-turn really is still outstanding.
        assert_eq!(
            read_orientation(&shot2, false).map(orientation_to_turns),
            Some(base),
            "a sidecar restating the file's own tag must subtract the same turns the embedded arm \
             subtracts — anything else is the double rotation coming back through the sidecar door"
        );
        let _ = std::fs::remove_dir_all(&dir2);
    }

    // The testkit ORIGINAL is untouched: same length, same mtime, and no sidecar was written beside
    // it (the whole test worked on copies in the temp dir).
    let now = std::fs::metadata(&src).ok().map(|m| (m.len(), m.modified().ok()));
    assert_eq!(untouched, now, "the testkit original must be byte- and mtime-identical");
    assert!(
        !sidecar_path_for(&src, false).exists(),
        "no sidecar may ever be written next to a testkit file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The composition ARITHMETIC the keys drive, kept as a pure row (it runs on a codec-less box, which
/// the round-trip test above cannot). v0.8.101 called this "what the APPLY path would write"; it is
/// not — the apply path is pinned above, on a real file. This is the display-side algebra only.
///
/// FALSIFIER (L28): swap `NONMIRR_BY_TURNS` to the anticlockwise cycle (1→8→3→6) and every
/// `compose_exif_orientation` row here inverts, so R and Shift+R would write each other's value.
#[test]
fn manual_rotation_composes_from_the_corrected_heic_base() {
    // The corrected base for IMG_2814: upright (the decoder already turned it).
    let base_turns = orientation_to_turns(1);
    assert_eq!(base_turns, 0);
    // R (+1) and Shift+R (-1 ≡ +3), as the tick composes them: (base + delta) & 3.
    assert_eq!((base_turns + 1) & 3, 1, "R turns the on-screen portrait one quarter clockwise");
    assert_eq!((base_turns + 3) & 3, 3, "Shift+R turns it the other way");
    assert_eq!((base_turns + 4) & 3, 0, "four presses return to the decoded orientation");
    assert_eq!(compose_exif_orientation(1, 1), 6, "one clockwise turn from upright");
    assert_eq!(compose_exif_orientation(1, 3), 8, "one anticlockwise turn from upright");
    assert_eq!(compose_exif_orientation(1, 4), 1, "back to where it started");
    // `orientation_minus_turns` is `compose`'s inverse in the turn argument, and it is the whole
    // absolute↔residual bridge — so pin the two against each other, mirror class included.
    for o in 1u8..=8 {
        for d in 0u8..4 {
            assert_eq!(
                orientation_minus_turns(compose_exif_orientation(o, d), d),
                compose_exif_orientation(o, 0),
                "minus must undo compose for orientation {o}, delta {d}"
            );
        }
    }
    // The two residuals S4 hands back, spelled out: absolute 6 with one turn consumed is upright,
    // absolute 5 (mirrored) with one turn consumed keeps the mirror.
    assert_eq!(orientation_minus_turns(6, 1), 1);
    assert_eq!(orientation_minus_turns(5, 1), 2);
    // And the RED's arithmetic end to end: the sidecar the apply path writes (absolute 3), read back
    // with the decoder's consumed turn removed, is the ONE quarter-turn the user asked for.
    assert_eq!(orientation_to_turns(orientation_minus_turns(3, 1).into()), 1);
}

/// The REPRODUCER for the numbers quoted in `heic_stop_divisor`'s documentation — the ones that
/// set [`HEIC_MIN_STOP_DIV`] to 4 rather than the obvious 2. `#[ignore]`d: it takes ~3 minutes and
/// it measures, it does not assert. Run it when a Windows codec update lands, when the testkit
/// gains a different camera, or when a later round wants to revisit the stop floor:
///
/// ```text
/// cargo test -p falcon-decode --test heic --release -- --ignored --nocapture --test-threads=1
/// ```
///
/// A cited measurement nobody can re-derive is a rumour with a number attached; this is the
/// difference. The pool half is the one that decided it — the single-thread table called the 1/2
/// stop a 12% loss, and the 18-worker run called it a 46% one.
#[test]
#[ignore]
fn heic_stop_ladder_cost_report() {
    use rayon::prelude::*;
    let Some(shots) = heic_shots_or_skip("stop-ladder cost report") else { return };
    fn med(mut v: Vec<u32>) -> u32 {
        v.sort_unstable();
        v[v.len() / 2]
    }
    eprintln!("── single thread: every stop, old (full+Lanczos) vs new (stop+Lanczos) ──");
    for s in &shots {
        let (w, h) = source_dimensions(s).expect("header dims");
        let long = w.max(h);
        eprintln!("--- {} {w}x{h} ---", s.name);
        for target in [long / 2 - 8, long / 4 - 8, long / 8 - 8, 2880, 2048, 1440] {
            if target < 64 {
                continue;
            }
            let div = heic_stop_divisor(long, target);
            let mut o = Vec::new();
            let mut n = Vec::new();
            for _ in 0..3 {
                o.push(browse_frame_rgba(s, target, true, Lane::Native).unwrap().dec_ms);
                n.push(browse_frame_rgba(s, target, true, Lane::Fast).unwrap().dec_ms);
            }
            eprintln!("  target {target:>5}  1/{div}  old {:>5} ms   new {:>5} ms", med(o), med(n));
        }
    }
    eprintln!("── 18 workers, 36 frames: the shape the real decode pool has ──");
    let pool = rayon::ThreadPoolBuilder::new().num_threads(18).build().unwrap();
    for target in [2880u32, 2048, 1440] {
        for (label, lane) in [("old", Lane::Native), ("new", Lane::Fast)] {
            let jobs: Vec<&Shot> = (0..36).map(|i| &shots[i % shots.len()]).collect();
            let t = std::time::Instant::now();
            let mut ms: Vec<u32> = pool.install(|| {
                jobs.par_iter()
                    .map(|s| browse_frame_rgba(s, target, true, lane).unwrap().dec_ms)
                    .collect()
            });
            let wall = t.elapsed().as_millis();
            ms.sort_unstable();
            eprintln!(
                "  target {target:>5} {label}: wall {wall:>6} ms   med {:>5} ms   max {:>5} ms",
                ms[ms.len() / 2],
                ms[ms.len() - 1]
            );
        }
    }
}

/// The stop ladder's arithmetic — pure, so it runs on EVERY machine, codec or not. This is where
/// the "never softer than v0.8.100" and "never a stop the codec charges full price for" rules
/// actually live.
///
/// FALSIFIER (L28): lower [`HEIC_MIN_STOP_DIV`] to 2 and the `8064 @ 2880 → 1` rows fail — which
/// is the whole point, because that configuration measured 46% SLOWER under the real pool. Change
/// the loop to accept a stop BELOW the target (`long / d >= target` → `> 0`) and the coverage rows
/// fail: the scrub tier would start serving frames softer than the ones it replaced.
#[test]
fn the_stop_ladder_only_takes_stops_that_cover_and_pay() {
    // 48 MP iPhone master. 2880 and 2048 have no qualifying stop (1/4 = 2016 is below both), so
    // they take the classic full decode — measured as the right answer, not a shortfall.
    assert_eq!(heic_stop_divisor(8064, 2880), 1);
    assert_eq!(heic_stop_divisor(8064, 2048), 1);
    assert_eq!(heic_stop_divisor(8064, 2016), 4, "exactly the 1/4 stop still COVERS the ask");
    assert_eq!(heic_stop_divisor(8064, 2017), 1, "one pixel over and it no longer covers");
    assert_eq!(heic_stop_divisor(8064, 1440), 4);
    assert_eq!(heic_stop_divisor(8064, 1008), 8, "the coarsest stop wins when it still covers");
    assert_eq!(heic_stop_divisor(8064, 256), 8, "the thumb tier, when S1's preview declines");
    // 12 MP iPhone master: the scrub tier is above every usable stop.
    assert_eq!(heic_stop_divisor(4032, 2880), 1);
    assert_eq!(heic_stop_divisor(4032, 1440), 1, "1/2 = 2016 is barred by HEIC_MIN_STOP_DIV");
    assert_eq!(heic_stop_divisor(4032, 1008), 4);
    // Degenerate inputs answer "full decode", never a divide-by-zero or a stop of 0.
    assert_eq!(heic_stop_divisor(4032, 0), 1);
    assert_eq!(heic_stop_divisor(0, 256), 1);
    // A floor of 1 would mean "scale to native", i.e. nothing — the ladder must start at a real stop.
    const _: () = assert!(HEIC_MIN_STOP_DIV >= 2);
}

/// Pure arithmetic — runs on EVERY machine, codec or not. `scaled_dims` is the size S2 asks the
/// codec for, and `resize_to_long` is the size the old path landed on: if those two formulas ever
/// diverge, "S2 never changes the frame size" quietly stops being true.
///
/// FALSIFIER (L28): round `scaled_dims` up instead of truncating (the natural "don't undershoot"
/// instinct) and half these rows fail by one pixel.
#[test]
fn scaled_dims_matches_resize_to_long() {
    // (w, h) pairs: iPhone 12 MP + 48 MP both ways up, a square, and awkward odd sizes.
    for (w, h) in [
        (4032u32, 3024u32),
        (3024, 4032),
        (8064, 6048),
        (6048, 8064),
        (1000, 1000),
        (4031, 3023),
        (7, 3),
    ] {
        for long in [256u32, 1440, 2048, 2880, 3840] {
            // Both formulas only apply when there is something to shrink; `resize_to_long` (and
            // therefore `downscale_rgb`) short-circuits an already-fitting frame, and so does the
            // S2 arm (`want_scale`), so the comparison is only meaningful in the shrink case.
            if w.max(h) <= long {
                continue;
            }
            let (nw, nh) = scaled_dims(w, h, long);
            // What `resize_to_long` produces, observed through the public API that uses it.
            let rgb = vec![0u8; (w as usize) * (h as usize) * 3];
            let (_, rw, rh) = resize_rgb_to_long_for_test(rgb, w, h, long);
            assert_eq!(
                (nw, nh),
                (rw, rh),
                "scaled_dims({w},{h},{long}) = {nw}x{nh} but the resize lands on {rw}x{rh}"
            );
        }
    }
}

/// `resize_to_long` is private; `downscale_rgb` is its public sibling with the identical formula
/// (both call the same `w >= h ? (long, long*h/w) : (long*w/h, long)` arithmetic). Using it here
/// keeps the test honest without widening the crate's API for a test's convenience.
fn resize_rgb_to_long_for_test(rgb: Vec<u8>, w: u32, h: u32, long: u32) -> (Vec<u8>, u32, u32) {
    downscale_rgb(rgb, w, h, long)
}

/// The one-switch revert's PARSE, tested without touching the process environment (the live reader
/// is a `OnceLock`, so a test that set the variable would poison every later test in the binary).
///
/// FALSIFIER (L28): accept any non-empty value (`"0"`, `"false"`, `"no"`) and the negative rows
/// fail — a user who wrote `FALCON_CLASSIC_HEIC=0` meaning "off" would silently get the old slow
/// paths and conclude the round did nothing.
#[test]
fn classic_switch_reads_only_the_exact_flag() {
    assert!(classic_heic_from_env(Some("1")), "the documented ON value");
    for off in [None, Some(""), Some("0"), Some("true"), Some("yes"), Some("2"), Some(" 1")] {
        assert!(!classic_heic_from_env(off), "{off:?} must not arm the revert");
    }
}

// ─────────────────────────── v0.8.148 (E5): THE HARDWARE-LANE ROUTER ───────────────────────────
//
// These rows are about the ROUTER, not the decoder. The decoder is E3's and is gated by
// `falcon-hwdec/tests/hw_photo.rs` against real files on real hardware; what has never been tested
// is the thing E5 adds — WHICH TIERS ask the hardware lane, and what happens when it declines. That
// is answerable with no GPU, no D3D11 and no HEVC anywhere, by installing a hook of the test's own
// and watching the shipping `browse_frame_rgba` through it.
//
// THE ONE-PROCESS HAZARD, and how it is closed. `install_hw_heic_hook` is a `OnceLock`: there is one
// hook per process and `cargo test` runs this binary's rows in parallel. A hook that answered for
// any file would corrupt every other row in this file that decodes a real HEIC. So the hook answers
// ONLY for a sentinel path that exists nowhere on disk and that no other row names — which also
// makes these rows free of file I/O entirely, and makes "the lane was asked" and "the lane was not
// asked" distinguishable without a codec: a fall-through on a nonexistent file FAILS, loudly, and
// for a reason the row can name.

/// A path no file will ever occupy. The hook answers for this and nothing else.
const HW_SENTINEL: &str = r"C:\__falcon_e5_hw_router_probe__.heic";

/// What the test hook returns for [`HW_SENTINEL`]: a tiny frame, small enough that `finish_source`
/// and the near-stop resize are both no-ops at every tier asked below.
const HW_PROBE_W: u32 = 8;
const HW_PROBE_H: u32 = 6;

// Every reader scopes entries to its calling thread: the hook runs synchronously in decode_heic_lane.
static HW_HOOK_LANES: std::sync::Mutex<Vec<(std::thread::ThreadId, &'static str)>> =
    std::sync::Mutex::new(Vec::new());

fn lane_name(l: Lane) -> &'static str {
    match l {
        Lane::Thumb => "thumb",
        Lane::Fast => "fast",
        Lane::Native => "native",
    }
}

fn hw_test_hook(
    path: &std::path::Path,
    _scale_to: Option<u32>,
    lane: Lane,
) -> HwHeicAnswer {
    HW_HOOK_LANES.lock().unwrap_or_else(|e| e.into_inner())
        .push((std::thread::current().id(), lane_name(lane)));
    if path == std::path::Path::new(HW_SENTINEL) {
        // A distinctive fill, so a frame that came from here cannot be mistaken for a decode.
        HwHeicAnswer::Served {
            rgb: vec![0x5Au8; (HW_PROBE_W * HW_PROBE_H * 3) as usize],
            w: HW_PROBE_W,
            h: HW_PROBE_H,
        }
    } else {
        // Every real file DECLINES — the fall-soft contract, and what keeps other rows safe.
        // v0.8.171: and this hook never answers , which is what keeps these rows
        // measuring the LADDER rather than the speed-priority abort.
        HwHeicAnswer::Declined
    }
}

/// Install once for this binary; rows call it and then read [`HW_HOOK_LANES`].
fn arm_test_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        assert!(install_hw_heic_hook(hw_test_hook), "the hook slot was free in this test binary");
    });
}

/// Built by hand rather than scanned: the sentinel does not exist, so no scanner can produce it.
fn sentinel_shot() -> Shot {
    Shot {
        id: 0,
        name: "hw-router-probe".into(),
        has_raw: false,
        has_jpg: true,
        raw: None,
        jpg: Some(std::path::PathBuf::from(HW_SENTINEL)),
        kind: SrcKind::Heic,
        cloud_placeholder: false,
        sniffed: None,
    }
}

/// v0.8.148 (E5) — **THE TIER POLICY, through the shipping call.**
///
/// `hw_heic_lane_applies` is the predicate; this is the proof that `decode_heic_lane` actually
/// honours it, which is a different claim. The fast and native lanes reach the hardware lane and are
/// served by it; the THUMB lane never reaches it and therefore fails on a file that does not exist —
/// and that failure is the evidence, because a thumb lane that HAD been routed would have been
/// handed a frame without ever touching the disk.
///
/// Why the thumb tier is excluded at all: S1's embedded-preview door costs no HEVC frame, while
/// E3-M2's timing split measured the hardware lane's floor as the DECODE (40–55 ms on a 48 MP photo,
/// tier-independent). A hardware thumbnail would be ~58 ms against a preview read's ~3.
///
/// FALSIFIER (L28): widen `hw_heic_lane_applies` to `Lane::Thumb` and the thumb row reddens; delete
/// rung 0 from `decode_heic_lane` and both served rows redden.
#[test]
fn the_hardware_lane_serves_fast_and_native_and_never_the_thumb_tier() {
    let thread = std::thread::current().id();
    arm_test_hook();
    let shot = sentinel_shot();

    for (lane, want) in [(Lane::Fast, true), (Lane::Native, true), (Lane::Thumb, false)] {
        HW_HOOK_LANES.lock().unwrap_or_else(|e| e.into_inner()).retain(|(owner, _)| *owner != thread);
        let got = browse_frame_rgba(&shot, 2880, true, lane);
        let asked = HW_HOOK_LANES.lock().unwrap_or_else(|e| e.into_inner())
            .iter().any(|(owner, _)| *owner == thread);
        assert_eq!(asked, want, "{lane:?}: the hardware lane was asked = {asked}, expected {want}");
        if want {
            let f = got.expect("a lane the hardware serves needs no file on disk");
            assert_eq!((f.w, f.h), (HW_PROBE_W, HW_PROBE_H), "the hook's frame came back whole");
            assert_eq!(f.route, DecodeRoute::Hardware, "…and it is REPORTED as the hardware route");
            assert_eq!(f.source, FrameSource::MainImage, "a hardware frame is the main image");
            assert_eq!(f.rgba[0], 0x5A, "the pixels are the hook's, not a decoder's");
        } else {
            assert!(
                got.is_err(),
                "the thumb lane must fall to the WIC ladder, which cannot open a file that does \
                 not exist — a frame here would mean the thumb tier had been routed to hardware"
            );
        }
    }
}

/// v0.8.148 (E5) — **THE FALL-SOFT CONTRACT.** A decline is not a failure.
///
/// The hook above returns `None` for every path but the sentinel, which is exactly the shape of a
/// real decline (no capability for this file, a mosaic past the device's limits, a VUI the kernel
/// will not guess at, a saturated session pool…). The claim is that the frame produced afterwards is
/// the one v0.8.147 produced — and that it is REPORTED as the CPU route, so no log can claim a
/// hardware decode that did not happen.
///
/// FALSIFIER (L28): make rung 0 propagate a decline as an error instead of falling through and every
/// row here reddens with a decode failure; report `DecodeRoute::Hardware` unconditionally and the
/// route rows redden while the pixels still arrive, which is why the two are asserted apart.
#[test]
fn a_declined_file_still_decodes_through_wic_and_says_so() {
    if !hw_child::run_here(module_path!(), "a_declined_file_still_decodes_through_wic_and_says_so") { return; }
    arm_test_hook();
    let Some(shots) = heic_shots_or_skip("hardware-lane decline") else { return };
    let shot = &shots[0];
    for (lane, dim) in [(Lane::Fast, 1440u32), (Lane::Native, 2880), (Lane::Thumb, 256)] {
        let f = browse_frame_rgba(shot, dim, true, lane).expect("a decline still decodes");
        assert_eq!(
            f.route,
            DecodeRoute::Cpu,
            "{lane:?}: a declined file decoded on the CPU lane and the frame must say so"
        );
        assert!(f.w > 0 && f.h > 0 && !f.rgba.is_empty(), "{lane:?}: a real frame came back");
    }
}

/// v0.8.148 (E5): the tier predicate itself, stated once where it lives. The rows above prove the
/// ladder honours it; this proves it says what the plan's E5 charter requires.
#[test]
fn the_thumb_tier_is_not_a_hardware_lane_tier() {
    assert!(hw_heic_lane_applies(Lane::Fast));
    assert!(hw_heic_lane_applies(Lane::Native));
    assert!(
        !hw_heic_lane_applies(Lane::Thumb),
        "thumbs keep the S1 embedded-preview door — 58 ms of hardware decode is not a thumb budget"
    );
}
