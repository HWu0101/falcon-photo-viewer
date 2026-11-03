//! v0.7.7 RAM L2 keep-alive cache (PLAN §57, stages 1–3). A plain, UI-thread-owned store of
//! decoded fast-tier frames (source-gamut RGBA + the frosted-blur mip) kept ALIVE in RAM after
//! their VRAM texture is evicted, so a frame the user already visited never re-decodes: a backward
//! pass / direction flip re-uploads FROM RAM (create_texture only) instead of paying the
//! ~200 ms Huffman-bound JPEG decode again.
//!
//! Zero-copy: the bytes arrive wrapped in `Arc<[u8]>` straight off the decode pool (the upload
//! thread wraps `d.rgba` BEFORE create_texture), so a deposit is a pointer move and a hit is a
//! pointer clone — never a memcpy of the multi-MB frame.
//!
//! Stage 2 adds the RAM-PRESSURE CONTROLLER: the store keeps a NOMINAL budget (fixed at boot,
//! min(25% RAM, 8 GiB)) and a live EFFECTIVE budget the controller steps down under system memory
//! pressure and back up when calm — mirroring the VRAM-OOM degrade/restore pattern, but pure and
//! self-contained here so it unit-tests without a live probe. See [`L2Store::pressure_sample`].
//!
//! Pure logic only (no wgpu/Slint) — unit-tested below. The `ram_l2` flag is consulted at the
//! DEPOSIT + HIT call sites (mirroring how `async_blur` gates in the worker), so these methods stay
//! flag-free and directly testable; when the flag is OFF nothing is deposited and the Arcs simply
//! drop (byte-identical to the pre-L2 path). The controller's runtime-disable state is INSIDE the
//! store (deposits/hits no-op while disabled) since it must gate every call site identically.
use crate::*;

/// Stage 3: does a cached frame decoded FOR bucket `entry_dim` satisfy the CURRENT want bucket?
/// A frame decoded for an equal-or-larger bucket downsamples for free (B3's shrink-keep rule);
/// a smaller one would have to upscale — it may keep DISPLAYING as a stale stand-in, but it stays
/// "wanted" so the pool re-decodes it. The one dim-satisfaction predicate shared by the L2 hit
/// rule ([`L2Store::get`]), the prefetch wanted-filter, and the upload-drain dedupe (tick.rs) —
/// if these diverge, a grow transition either starves (never re-decodes) or churns (re-decodes
/// forever). NOTE the convention: `entry_dim` is the WANT the frame was decoded FOR (stamped from
/// `scrub_dim_atomic` at decode start), never the pixel long side — a 3840-want frame that B2
/// kept at its 4096 DCT stop is still tagged 3840, so it satisfies want=3840 and does NOT
/// falsely satisfy a later want=4096.
///
/// v0.8.101 (S1 caveat): [`falcon_decode::PREVIEW_CACHE_DIM`] is the one value OUTSIDE that
/// bucket alphabet — the tag a frame lifted from a file's EMBEDDED PREVIEW carries (a 576 px
/// iPhone HEIC preview: fine for a 256 px filmstrip tile, never a scrub frame). It is refused
/// here rather than left to `>=`, so the quarantine holds for `want == 0` too and does not
/// depend on every future caller happening to pass a non-zero want. Preview frames do not reach
/// this cache today (thumbs live in `Film`, which has no dim bucket); this is the guard that
/// keeps that true when someone changes their mind.
///
/// v0.8.102 (F9): this predicate is a READ guard — it gates want-satisfaction and [`L2Store::get`],
/// and it never saw a WRITE. So on its own it did not make the quarantine claim true: a preview
/// lander would still have been inserted into the fast cache (`step_display` filters that cache on
/// `turns` alone, so it would have PRESENTED) and still deposited into L2 as an entry `get` can
/// never return. The write half now lives in the tick drain, which drops a `PREVIEW_CACHE_DIM`
/// lander beside the gen/gamut/tier/turns rejects; the two together are what "can never be served
/// as a scrub frame" rests on.
#[inline]
pub(crate) fn dim_satisfies(entry_dim: u32, want: u32) -> bool {
    entry_dim != falcon_decode::PREVIEW_CACHE_DIM && entry_dim >= want
}

/// v0.8.170 (ONE DECODE PER SHOT) — **may this RAM resident be served to the FULL-RES tier?**
///
/// The scrub tier's own question is [`dim_satisfies`] alone, and that is right for it: the bucket is
/// the WANT the bytes were decoded for, both sampling tiers share one bucket, and `Decoded::sup`
/// discriminates them. A SECOND tier reading the same bucket cannot lean on that. On the SUBSAMPLE
/// tier an ordinary fast frame is filed under want 2880 while its pixels are ~1440 — honest inside
/// the fast tier, and a soft picture presented as the sharp one if the full-res tier took the bucket
/// at its word. So this asks the bucket AND the pixels, and the pixel term is the one that makes it
/// safe: a frame whose long side does not reach the full-res target is refused and the detail worker
/// decodes as it always did.
///
/// The cost of the pixel term is that a source SMALLER than the full-res target is never served from
/// RAM even though its bytes would be exactly what a decode returns — the store has no way to know
/// the source was the limit rather than the ask. That is a decode this arm does not save, on files
/// (under ~2560 px) that were never the expensive class; the phone masters this round is aimed at
/// are 6048×8064.
///
/// # v0.8.172 (G1) — THE TWO TERMS THE SIZE TEST COULD NOT CARRY, AND WHY SIZE ALONE WAS A DEFECT
///
/// v0.8.170 asked the bucket and the pixels and NOTHING ELSE, and gated the whole consult on the
/// lever alone (`ram_l2 && heic_one_decode()`). Every one of the following then qualified as "the
/// sharp frame", because every one of them is an ordinary L2 resident whose bucket and pixels reach
/// the full-res target:
///
///   * **A RAW+JPG pair in RAW mode.** The fast tier is ALWAYS the finished JPG; the full-res tier
///     develops the RAW. Its L2 entry is the JPG, and it was published as the RAW's sharp frame.
///   * **A plain scrub frame on a maximized laptop window**, where `scrub want == ddim == 2560`, i.e.
///     in exactly the band the boot line calls "inert" — the routing gate declines (`detail > scrub`
///     is false) so no master is ever banked, and the ordinary fast frame served in its place.
///   * **A WIC decode-at-scale HEIC frame** on a folder the lane DECLINES: never hardware, never a
///     master, same size, served.
///   * **A resident parked under a different `res_limit`.** The consult read `det_epoch` LIVE at
///     enqueue and stamped it onto the job, so the drain's `epoch != det_epoch_now` guard compared
///     the live value against itself — structurally a no-op for a stale deposit.
///
/// So the resident now carries what it always needed to: **`master`**, set by the drain's master
/// deposit ALONE — which by construction exists only for a hardware-lane HEIC that passed
/// [`crate::support::hw_one_decode_want`] — and **`epoch`**, the develop-config token that was live
/// WHEN THE BYTES WERE PARKED. Every toggle that changes what "the full-res frame" means (RAW mode,
/// output gamut, res limit, adaptive hi-res, Simulate VRAM) bumps `det_epoch`, so a stale resident
/// stops being servable to this tier the moment it is stale — without clearing L2, so it keeps
/// serving the SCRUB tier, which is unaffected by every one of those toggles but the gamut (which
/// clears the store outright).
///
/// FALSIFIER (L28, RED-FIRST): drop the `pixel_long >= want` term and
/// `a_subsampled_scrub_frame_is_never_served_as_the_sharp_one` reddens; drop the `master` term and
/// `a_non_master_resident_is_never_served_as_the_sharp_frame` reddens on the RAW+JPG row; drop the
/// epoch term and the same row's res-limit half reddens.
#[inline]
pub(crate) fn full_res_serves(
    // Were these bytes parked by the one-decode arm's MASTER deposit? Nothing else may answer here.
    master: bool,
    // The develop-config token that was live when they were parked, against `det_epoch_now`.
    epoch: u64,
    entry_dim: u32,
    pixel_long: u32,
    want: u32,
    det_epoch_now: u64,
) -> bool {
    master && epoch == det_epoch_now && dim_satisfies(entry_dim, want) && pixel_long >= want
}

/// One kept-alive fast frame. `dim` is the scrub-dim BUCKET (the `want` long side the frame was
/// decoded FOR — the same quantity B3 keys its shrink-keep on), NOT the pixel long
/// side, so the sub/supersample tier choice never skews the comparison. `gamut` is the frame's
/// SOURCE gamut, kept so a re-upload from RAM runs the identical GPU colour-manage to whatever the
/// live output gamut is (the CPU only ever holds source-gamut pixels — the CM happens on the GPU).
#[derive(Clone)]
pub(crate) struct L2Entry {
    pub(crate) rgba: Arc<[u8]>,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) blur: Arc<[u8]>,
    pub(crate) bw: u32,
    pub(crate) bh: u32,
    pub(crate) dim: u32,
    pub(crate) gamut: Gamut,
    /// v0.8.172 (G1): were these bytes parked by the ONE-DECODE arm's master deposit? Written by the
    /// upload drain and by nothing else, from [`crate::support::l2_deposit_is_master`] — which is
    /// true only for a frame the fast pool routed through [`crate::support::hw_one_decode_want`],
    /// i.e. a hardware-lane HEIC on a folder the lane is SERVING. It is the term that separates "a
    /// resident whose bucket and pixels happen to reach the full-res target" from "the full-res
    /// tier's own frame", and without it the second tier was reading the first tier's cupboard. See
    /// [`full_res_serves`].
    pub(crate) master: bool,
    /// v0.8.172 (G1): the develop-config token (`det_epoch`) that was live AT DEPOSIT. Compared
    /// against the live value by [`full_res_serves`], so a res-limit / hi-res / RAW / Simulate-VRAM
    /// change invalidates RAM serves to the FULL-RES tier without clearing the store — the scrub
    /// tier keeps every one of these residents, because none of those toggles changes what a scrub
    /// frame is. Stamped for every deposit (a plain fast frame's value is simply never consulted).
    pub(crate) epoch: u64,
}
impl L2Entry {
    fn bytes(&self) -> usize {
        self.rgba.len() + self.blur.len()
    }
}

// ── Stage 2 pressure-controller constants ───────────────────────────────────────────────
// Sampling cadence is ~1/s (step_l2_pressure's throttle), so every count below is ≈ seconds.

/// Boot disable floor: a nominal budget under 1 GiB (total RAM < 4 GiB) is permanently off for
/// the session — the cache would hold so few frames that hits are luck, and a machine that small
/// needs every byte for the decode pools + OS file cache. Compressed-L2 (LZ4 the RGBA at deposit)
/// is the documented <16 GB fallback (PLAN §57 records the decision) — NOT implemented; re-decode
/// on big-RAM machines costs about the same as decompress, so it only ever pays on small ones.
pub(crate) const L2_BOOT_FLOOR: usize = GIB as usize;
/// Runtime disable floor: pressure that would push the effective budget below 512 MiB clears and
/// disables the store instead — below ~13 4K-proxy frames (~39 MB each) the cache can't even hold
/// one prefetch window, so it's churn with no hit-rate. Re-enable starts back AT this floor.
pub(crate) const L2_RUNTIME_FLOOR: usize = 512 * 1024 * 1024;
/// Consecutive CALM samples (avail ≥ restore threshold) required before EACH restore step — and
/// before re-enabling after a runtime disable. At the ~1/s cadence: a restore step every ≥4 s, a
/// full floor→nominal climb ≥ ~28 s, and a re-enable only after 4 s of demonstrated headroom.
pub(crate) const L2_CALM_SAMPLES: u32 = 4;
const GIB: u64 = 1024 * 1024 * 1024;
/// LOW_WATER floor: 1.5 GiB. 6% of a 16 GB machine is only ~0.96 GiB — too close to where Windows
/// starts hard-trimming working sets and thrashing the compressed store; 1.5 GiB leaves room for
/// one full 45 MP decode burst (~0.7 GiB across the pool) to land WITHOUT dipping into that zone.
const L2_LOW_WATER_MIN: u64 = 3 * GIB / 2;
/// Hysteresis band: restore threshold = LOW_WATER + 1.5 GiB. Must exceed the LARGEST degrade step
/// (nominal/8 ≤ 1 GiB at the 8 GiB cap) so that refilling one restored step's worth of cache can
/// never, by itself, push avail back under LOW_WATER — the degrade↔restore limit cycle is
/// structurally impossible, not just unlikely. Combined with L2_CALM_SAMPLES this is the
/// anti-flap design: hysteresis in LEVEL (the band) and in TIME (the streak).
const L2_RESTORE_BAND: u64 = 3 * GIB / 2;

/// DEGRADE trigger: system available RAM below max(1.5 GiB, 6% of total). The 6% term scales the
/// threshold up on big machines (5.76 GiB on the 96 GB dev box) where the OS file cache is doing
/// real work well above any fixed floor; the 1.5 GiB floor covers 16 GB machines (see
/// L2_LOW_WATER_MIN). Below this the OS is already under pressure — our cache is the most
/// shed-able consumer in the process, so it goes first.
fn l2_low_water(total_phys: u64) -> u64 {
    (total_phys / 100 * 6).max(L2_LOW_WATER_MIN)
}

/// v0.9.23 (one-pool round): the valve's three avail-RAM zones as a PURE stateless classification —
/// the SAME `l2_low_water` + `L2_RESTORE_BAND` constants [`L2Store::pressure_sample`] steps on, so
/// the macOS elastic-pool governor's shrink input is BY CONSTRUCTION the valve's degrade signal
/// (never a hand-mirrored threshold that can drift — the 07-19 cross-crate-constants lesson).
/// `Low` = the valve's degrade zone, `Dead` = the level-hysteresis band, `Calm` = the restore zone.
/// The agreement test below drives BOTH this and a live controller over the boundaries.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // constructed by tests + the macOS governor only
pub(crate) enum PressureZone {
    Low,
    Dead,
    Calm,
}
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pressure_zone(avail: u64, total_phys: u64) -> PressureZone {
    let low = l2_low_water(total_phys);
    if avail < low {
        PressureZone::Low
    } else if avail < low + L2_RESTORE_BAND {
        PressureZone::Dead
    } else {
        PressureZone::Calm
    }
}

/// A pressure-controller transition for the caller to LOG (one line per transition, nothing in
/// steady state — the no-spam rule is by construction: `None` in every non-transition sample).
/// Budgets in bytes; the tick formats MB.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum PressureAction {
    None,
    /// Effective budget stepped DOWN (entries already evicted to fit).
    Degrade { from: usize, to: usize },
    /// Pressure pushed the budget under the runtime floor: store cleared, deposits/hits OFF.
    Disable { from: usize },
    /// Effective budget stepped back UP toward nominal after a calm streak.
    Restore { from: usize, to: usize },
    /// Re-enabled after a runtime disable (empty, at the runtime floor; climbs from there).
    Reenable { to: usize },
}

/// The RAM keep-alive cache: shot-index → entry, with a running byte total and a two-level budget
/// (nominal = boot value; effective = what pressure currently allows). Eviction is SYMMETRIC
/// distance-LRU around the current shot (VRAM keeps a directional 2:1 window; RAM keeps BOTH
/// sides so a flip is free) — computed at eviction time from `|i - c|`, with NO per-hit recency
/// bookkeeping.
pub(crate) struct L2Store {
    map: HashMap<usize, L2Entry>,
    bytes: usize,
    /// The boot budget (min(25% RAM, 8 GiB)) — the ceiling restores climb back to; never changes.
    nominal: usize,
    /// The live budget inserts/evictions respect; pressure steps it down, calm steps it back up.
    effective: usize,
    /// Nominal < L2_BOOT_FLOOR at construction — permanently off this session (no restore path).
    boot_disabled: bool,
    /// Runtime disable (pressure floor tripped): deposits/hits no-op until a calm streak re-enables.
    disabled: bool,
    /// Consecutive calm samples (avail ≥ low_water + band); reset by ANY non-calm sample.
    calm: u32,
    /// max(1.5 GiB, 6% of total) — captured at construction from the boot RAM probe.
    low_water: u64,
}
impl L2Store {
    pub(crate) fn new(nominal: usize, total_phys: u64) -> L2Store {
        L2Store {
            map: HashMap::new(),
            bytes: 0,
            nominal,
            effective: nominal,
            boot_disabled: nominal < L2_BOOT_FLOOR,
            disabled: false,
            calm: 0,
            low_water: l2_low_water(total_phys),
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
    /// Deposits/hits allowed? False while boot- or runtime-disabled — every call site (deposit
    /// gate, hit partition) routes through [`get`]/[`insert`]/[`deposit_wanted`], which all check
    /// this, so a disabled store is inert without the tick knowing why.
    fn enabled(&self) -> bool {
        !self.boot_disabled && !self.disabled
    }
    /// Total RAM < 4 GiB at boot → the controller has nothing to manage (skip the ~1/s probe).
    pub(crate) fn is_boot_disabled(&self) -> bool {
        self.boot_disabled
    }

    /// Stage 3: should the drain deposit a frame decoded for bucket `dim`? Yes when the store can
    /// accept (enabled) AND the slot is empty OR holds a SMALLER-bucket frame (the grow re-decode
    /// case — the new frame replaces the stale one via `insert`'s replace path, bytes adjust).
    /// An equal-dim arrival (the FromRam re-upload echo) or a larger-dim resident (a shrink kept
    /// the better frame) deposits nothing — no byte churn, and the best frame always wins.
    pub(crate) fn deposit_wanted(&self, idx: usize, dim: u32) -> bool {
        self.enabled() && self.map.get(&idx).map_or(true, |e| e.dim < dim)
    }

    /// Deposit `entry` for shot `idx`, then evict the farthest-from-`anchor_c` UNPROTECTED entries
    /// until the effective budget holds. No-op while disabled, or if the single entry alone exceeds
    /// the WHOLE budget (never worth evicting everything for one frame). Replacing an existing idx
    /// adjusts the running byte total.
    ///
    /// v1.0.0-rc (FIX 2026-09-07, round X1 / F2): `protected` is
    /// `support::ExplicitSet::holds` — the SAME predicate the VRAM cache's eviction asks
    /// (both reach it through `support::ExplicitSet`; the VRAM site is `tick.rs`'s drain step —
    /// named, not counted: a line distance rots, verifier Y2 09-07). Without it the deposit that made an explicit far frame
    /// evicted that frame, because it is the farthest entry from `anchor_c` by construction.
    pub(crate) fn insert(
        &mut self,
        idx: usize,
        entry: L2Entry,
        anchor_c: usize,
        protected: &dyn Fn(usize) -> bool,
    ) {
        if !self.enabled() {
            return; // runtime-disabled (pressure) or boot-disabled — deposits are off
        }
        let ebytes = entry.bytes();
        if ebytes > self.effective {
            return; // a lone frame bigger than the whole budget — keep what we have instead
        }
        if let Some(old) = self.map.insert(idx, entry) {
            self.bytes -= old.bytes(); // replace: drop the old bytes before adding the new
        }
        self.bytes += ebytes;
        self.evict_to_budget(anchor_c, protected);
    }

    /// A usable kept-alive frame for `idx`: only when its bucket satisfies the CURRENT bucket
    /// ([`dim_satisfies`] — a larger-than-needed frame downsamples fine; a smaller one must NOT be
    /// served, it stays pool-wanted so the grow re-decode isn't starved). None while disabled.
    pub(crate) fn get(&self, idx: usize, current_dim_bucket: u32) -> Option<&L2Entry> {
        if !self.enabled() {
            return None;
        }
        self.map.get(&idx).filter(|e| dim_satisfies(e.dim, current_dim_bucket))
    }

    /// Wipe the whole cache (folder swap / output-gamut change / tier flip — see the call sites).
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    /// Keeping prepared work (2 November): the residents a same-folder rescan keeps, at their new
    /// positions. The bytes are `Arc`s in source gamut and unrotated, so a clone is a count and the
    /// later [`clear`](Self::clear) frees only what was left behind. Every field stays as it is,
    /// including a stale tier's `dim` of 0.
    pub(crate) fn carried(&self, rekey: impl Fn(usize) -> Option<usize>) -> Vec<(usize, L2Entry)> {
        self.map.iter().filter_map(|(&i, e)| rekey(i).map(|n| (n, e.clone()))).collect()
    }

    /// …and back, after the clear. No eviction: this is a subset of what already fit.
    pub(crate) fn restore_carried(&mut self, kept: Vec<(usize, L2Entry)>) {
        for (i, e) in kept {
            self.bytes += e.bytes();
            if let Some(old) = self.map.insert(i, e) {
                self.bytes -= old.bytes();
            }
        }
    }

    /// v0.8.196 fix tail (skeptic B, R6 — CONFIRMED): **every resident is now the WRONG SAMPLING
    /// TIER, but its bytes are still worth keeping.**
    ///
    /// This store carries no tier tag — the design has always leaned on a tier flip CLEARING it
    /// ("L2 holds no tier tag — a flip clears L2, so a served entry is live-tier now"). The
    /// efficiency posture flips the tier on its own edges, and a `clear()` there would throw away
    /// hundreds of megabytes of correctly-decoded pixels and force a full re-decode of the window on
    /// a machine that is on battery, at the exact moment it is trying to spend less.
    ///
    /// So: keep the bytes, invalidate the CLAIM. `dim = 0` satisfies no want ([`dim_satisfies`]), so
    /// [`get`] refuses every resident and the pool re-decodes as it would after a grow; and because
    /// `0 < dim` for every real bucket, [`deposit_wanted`] admits the replacement, which lands
    /// through `insert`'s replace path and corrects the byte total. Nothing is stranded: an entry
    /// that is never re-decoded is evicted by distance exactly as before.
    pub(crate) fn mark_stale_tier(&mut self) {
        for e in self.map.values_mut() {
            e.dim = 0;
        }
    }

    /// Stage 2: lower (or raise) the LIVE budget, evicting farthest-from-`anchor_c` immediately
    /// when shrinking below the current contents. Clamped to nominal — restores never overshoot.
    /// `protected` is [`insert`]'s, and for the same reason: a pressure shed is still an eviction
    /// over a set that has an explicit subset.
    pub(crate) fn set_effective_budget(
        &mut self,
        bytes: usize,
        anchor_c: usize,
        protected: &dyn Fn(usize) -> bool,
    ) {
        self.effective = bytes.min(self.nominal);
        self.evict_to_budget(anchor_c, protected);
    }
    /// Test-only: production reads the budget through the PressureAction from/to fields (the log
    /// lines), never directly — the perf line reports contents (`l2=<n>/<MB>MB`), not the ceiling.
    #[cfg(test)]
    pub(crate) fn effective_budget(&self) -> usize {
        self.effective
    }

    /// Evict the FARTHEST-from-anchor UNPROTECTED entry first (symmetric |i - c|, ties: either)
    /// until within the effective budget. Distance-at-eviction-time only — no recency to maintain.
    ///
    /// v1.0.0-rc (FIX 2026-09-07, round X1 / F2): **AND IF ONLY PROTECTED ENTRIES REMAIN WHILE OVER
    /// BUDGET, IT STOPS.** That is a real overshoot and it is stated rather than hidden: the
    /// explicit set is at most FIVE indices — the displayed shot, both on-screen compare halves,
    /// the one Review tile under the pointer, and (round X2) the one frame a blocked always-sharp
    /// browse is waiting for: `support::ExplicitSet`'s FIVE index-bearing terms (`len` is a clamp,
    /// not an index), of which `c` and `awaited` are always distinct while a browse is blocked —
    /// `awaited` is `c ± 1` — and the rest may coincide. So the store can sit at most five
    /// residents over its effective budget, and every one of them is a frame the user is looking at
    /// or waiting for. The alternative, evicting a photograph on the screen to honour a budget, is
    /// what this round exists to stop. The overshoot drains itself: `hover_pin` clears the moment
    /// the pointer moves, `awaited` clears the tick the browse advances or stops, the halves change
    /// with the compare, and the next deposit or pressure sample re-runs this loop against the
    /// smaller set.
    fn evict_to_budget(&mut self, anchor_c: usize, protected: &dyn Fn(usize) -> bool) {
        while self.bytes > self.effective {
            let victim = self
                .map
                .keys()
                .copied()
                .filter(|&i| !protected(i))
                .max_by_key(|&i| dist(i, anchor_c));
            let Some(victim) = victim else {
                break; // empty, or nothing left that may be taken — see the doc above
            };
            if let Some(e) = self.map.remove(&victim) {
                self.bytes -= e.bytes();
            }
        }
    }

    /// Stage 2: one ~1/s pressure sample — the WHOLE two-way controller, pure (drive it with any
    /// (avail) sequence in tests; `low_water`/nominal were fixed at construction). Three zones:
    ///
    ///   avail < low_water              → PRESSURE: step effective DOWN by nominal/8 (evicting
    ///                                    immediately); a step that would land under the 512 MiB
    ///                                    runtime floor CLEARS + DISABLES instead. calm := 0.
    ///   low_water ≤ avail < +band      → DEAD BAND: do nothing, calm := 0. This is the level
    ///                                    hysteresis — oscillating right at low_water can only
    ///                                    ever ratchet DOWN, never flap down-up-down.
    ///   avail ≥ low_water + 1.5 GiB    → CALM: after L2_CALM_SAMPLES consecutive calm samples,
    ///                                    take ONE step back up (or re-enable, empty, at the
    ///                                    runtime floor), then require a fresh streak. Never
    ///                                    overshoots nominal.
    ///
    /// Every non-None return is a transition the caller logs — steady state returns None, so the
    /// no-log-spam rule holds by construction. Boot-disabled stores always return None.
    ///
    /// ── v0.8.181 (pre-merge review): `hold` — THE FAST TIER'S GESTURE HOLD, AND WHY IT REACHES
    /// THIS CONTROLLER AT ALL. ───────────────────────────────────────────────────────────────────
    /// The v0.8.179 wave-2 hold parks decoded fast frames in `drain_buf` for the length of a
    /// gesture rather than publishing them. That is a deliberate, BOUNDED, transient reservation of
    /// system RAM — and it is invisible to this controller, which sees only `avail_ram_bytes()`
    /// falling. Under a long zoom-drag on a big folder the shed could therefore walk the effective
    /// budget all the way to the runtime floor and take the one step that is not a step:
    /// `Disable`, which CLEARS the whole cache and needs `L2_CALM_SAMPLES` of demonstrated headroom
    /// to come back EMPTY at 512 MiB. A gesture would then have cost the photographer his entire
    /// keep-alive cache, and the bytes that triggered it were released the moment he let go.
    ///
    /// So while the hold is engaged, a REACHED `Disable` downgrades to a `Degrade` that stops at
    /// the floor: the shed still happens (a genuinely low box gives back every step it has), the
    /// STICKY part does not. The hold is not a ceiling and it is not a veto — release it and the
    /// very next sample at the same pressure reaches `Disable` exactly as before.
    pub(crate) fn pressure_sample(
        &mut self,
        avail: u64,
        anchor_c: usize,
        hold: bool,
        // v1.0.0-rc (FIX 2026-09-07, round X1 / F2): every shed this controller takes runs through
        // `set_effective_budget`, i.e. through the same eviction the deposit path uses — so it
        // carries the same explicit-set exemption. `Disable` still CLEARS unconditionally: that arm
        // is not an eviction by distance, it is the store giving up, and a machine under real
        // memory pressure keeping four frames alive would be the wrong answer to the wrong problem.
        protected: &dyn Fn(usize) -> bool,
    ) -> PressureAction {
        if self.boot_disabled {
            return PressureAction::None;
        }
        let step = (self.nominal / 8).max(1);
        if avail < self.low_water {
            self.calm = 0;
            if self.disabled {
                return PressureAction::None; // already fully shed — nothing left to give back
            }
            let from = self.effective;
            let target = from.saturating_sub(step);
            if target < L2_RUNTIME_FLOOR {
                // v0.8.181: THE ONE ARM THE GESTURE HOLD REACHES (see the doc above). Everything
                // else in this controller is reversible on the next sample; this is not — it clears
                // the store and puts a four-sample calm streak between the photographer and an
                // EMPTY 512 MiB cache. So under a hold it downgrades to the floor and stops there:
                // still a shed, still logged (`Degrade`), nothing thrown away. At the floor already
                // there is no step left that is not the sticky one, so the honest answer is
                // silence — which is also this controller's no-log-spam rule, unchanged.
                if hold {
                    let to = L2_RUNTIME_FLOOR.min(self.nominal);
                    if to < from {
                        self.set_effective_budget(to, anchor_c, protected);
                        return PressureAction::Degrade { from, to };
                    }
                    return PressureAction::None;
                }
                self.clear();
                self.disabled = true;
                self.effective = 0;
                return PressureAction::Disable { from };
            }
            self.set_effective_budget(target, anchor_c, protected);
            return PressureAction::Degrade { from, to: target };
        }
        if avail < self.low_water + L2_RESTORE_BAND {
            self.calm = 0; // dead band: not calm enough to climb, not low enough to shed
            return PressureAction::None;
        }
        // Calm zone. Count a streak toward the next single restore step / re-enable.
        if self.disabled {
            self.calm += 1;
            if self.calm >= L2_CALM_SAMPLES {
                self.calm = 0;
                self.disabled = false;
                self.effective = L2_RUNTIME_FLOOR.min(self.nominal);
                return PressureAction::Reenable { to: self.effective };
            }
            return PressureAction::None;
        }
        if self.effective < self.nominal {
            self.calm += 1;
            if self.calm >= L2_CALM_SAMPLES {
                self.calm = 0;
                let from = self.effective;
                self.set_effective_budget(from.saturating_add(step), anchor_c, protected); // clamps to nominal
                return PressureAction::Restore { from, to: self.effective };
            }
            return PressureAction::None;
        }
        self.calm = 0; // at nominal and healthy — a future episode starts its streak fresh
        PressureAction::None
    }
}

#[inline]
fn dist(i: usize, c: usize) -> usize {
    if i >= c { i - c } else { c - i }
}

/// The nominal RAM budget: 25% of physical RAM, capped at 8 GiB. u64 math (the probe is u64),
/// clamped into usize. Designed for 16–32 GB machines (the 96 GB dev box is the outlier) so the OS
/// file cache still has room to breathe. Split out (pure) so it's unit-testable without a live probe.
pub(crate) fn l2_budget_bytes(total_phys: u64) -> usize {
    const CAP: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB
    (total_phys / 4).min(CAP).min(usize::MAX as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;
    /// A store on a "32 GiB machine": nominal 8 GiB, low_water = max(1.5, 1.92) = 1.92 GiB,
    /// restore threshold 1.92 + 1.5 = 3.42 GiB, step 1 GiB. Values asserted in `water_marks`.
    fn store32() -> L2Store {
        let total = 32 * GIB;
        L2Store::new(l2_budget_bytes(total), total)
    }
    const LOW_32: u64 = 32 * GIB / 100 * 6; // 1.92 GiB
    const CALM_32: u64 = LOW_32 + L2_RESTORE_BAND; // 3.42 GiB — first avail that counts calm

    /// v1.0.0-rc (FIX 2026-09-07, round X1 / F2): the rows below are about the store's DISTANCE
    /// rule, which is not about the fast tier's explicit set, so they protect nothing — and every
    /// one of them is therefore a byte-pin of the pre-exemption behaviour. The row that does
    /// protect is `an_explicit_far_frame_survives_its_own_deposit`.
    fn unprotected(_: usize) -> bool {
        false
    }

    /// An entry whose total bytes == `bytes` (rgba carries them, blur empty) tagged at bucket `dim`.
    fn entry(dim: u32, bytes: usize) -> L2Entry {
        let rgba: Arc<[u8]> = vec![0u8; bytes].into();
        let blur: Arc<[u8]> = Vec::new().into();
        L2Entry {
            rgba, w: dim, h: dim, blur, bw: 0, bh: 0, dim, gamut: Gamut::Srgb,
            // v0.8.172 (G1): an ORDINARY fast-tier resident — the store's own tests are about the
            // scrub tier's bucket rules, which these two fields do not touch.
            master: false,
            epoch: 0,
        }
    }
    fn has(s: &L2Store, idx: usize) -> bool {
        s.get(idx, 0).is_some() // want 0: any dim satisfies — presence only
    }

    /// Keeping prepared work (2 November): frames beyond graphics memory survive a Delete at their
    /// new positions, the running total stays true, and nothing about a frame's claim changes.
    ///
    /// FALSIFIER (L28): `restore_carried` doesn't add to `bytes`, or inserts through a path that
    /// resets `dim` or `epoch`; `carried` ignores the rekey.
    #[test]
    fn a_rescan_carries_ram_frames_and_the_byte_total_follows() {
        let mut s = store32();
        // A stale tier's resident (`dim` 0, the `mark_stale_tier` state) must stay stale.
        s.insert(3, L2Entry { master: true, epoch: 7, gamut: Gamut::DisplayP3, ..entry(0, MIB) }, 3, &unprotected);
        s.insert(5, entry(1024, 2 * MIB), 3, &unprotected);
        s.insert(8, entry(2048, 4 * MIB), 3, &unprotected);
        // 3 moves to 1, 5 moves to 9, 8 left the list.
        let kept = s.carried(|i| match i { 3 => Some(1), 5 => Some(9), _ => None });
        s.clear();
        assert_eq!((s.len(), s.bytes()), (0, 0));
        s.restore_carried(kept);
        assert_eq!(s.len(), 2);
        assert_eq!(s.bytes(), 3 * MIB, "the total is the survivors' sum");
        for gone in [3, 5, 8] {
            assert!(!s.map.contains_key(&gone), "{gone}: the old positions hold nothing");
        }
        let a = s.map.get(&1).expect("the frame follows its file");
        assert_eq!((a.rgba.len(), a.dim, a.epoch, a.master, a.gamut), (MIB, 0, 7, true, Gamut::DisplayP3));
        let b = s.map.get(&9).expect("and the other one");
        assert_eq!((b.rgba.len(), b.dim, b.epoch, b.master), (2 * MIB, 1024, 0, false));
        // Replacing later still corrects the total.
        s.insert(1, entry(2048, MIB / 2), 1, &unprotected);
        assert_eq!(s.bytes(), 2 * MIB + MIB / 2);
    }
    /// A small store with a tiny nominal for the eviction-shape tests (pressure paths untested here).
    fn small(budget: usize) -> L2Store {
        let mut s = L2Store::new(L2_BOOT_FLOOR, 16 * GIB); // enabled (nominal == floor)
        s.set_effective_budget(budget, 0, &unprotected);
        s
    }

    #[test]
    fn evicts_farthest_symmetric_first() {
        // Budget holds exactly two 100-byte entries; anchor at shot 10.
        let mut s = small(250);
        s.insert(20, entry(2048, 100), 10, &unprotected); // far (|20-10| = 10)
        s.insert(11, entry(2048, 100), 10, &unprotected); // near (|11-10| = 1)
        assert_eq!(s.len(), 2);
        s.insert(10, entry(2048, 100), 10, &unprotected); // anchor itself → overflow → evict the farthest (20)
        assert_eq!(s.len(), 2);
        assert!(!has(&s, 20)); // the far entry went
        assert!(has(&s, 10) && has(&s, 11));
        assert_eq!(s.bytes(), 200);
    }

    /// v1.0.0-rc (FIX 2026-09-07, round X1 / F2) — **AN EXPLICIT FAR FRAME SURVIVES ITS OWN
    /// DEPOSIT.**
    ///
    /// The hover preview's frame, and a far compare half's, are BY DEFINITION the farthest entries
    /// from `c` — that is why they were fetched at all — so on a store at budget the deposit that
    /// made one evicted it, on the very insert that made it. The exemption is the SAME
    /// `support::fast_evict_protected` the VRAM cache's eviction already asked, reached here
    /// through `support::ExplicitSet`.
    ///
    /// RED-FIRST (run against the unexempted eviction): `the frame the user asked for is not what
    /// an over-budget deposit takes`.
    ///
    /// FALSIFIER (L28), ALL THREE RUN, one per leg: drop the `filter(|&i| !protected(i))` from
    /// `evict_to_budget` and the first assert reddens with that same line; give that filter an
    /// `.or_else()` fallback to the UNFILTERED maximum — the only-protected case evicting anyway —
    /// and the second leg's `nothing may be taken, so nothing is` reddens; drop the `hover_pin`
    /// term from `support::fast_evict_protected` and the THIRD leg's `the hovered tile's RAM
    /// resident survives the deposit that made it` reddens, which is what binds this store to the
    /// predicate the VRAM cache's eviction asks rather than to any closure a row invents.
    #[test]
    fn an_explicit_far_frame_survives_its_own_deposit() {
        // Budget holds exactly two 100-byte entries; the anchor is shot 0.
        let mut s = small(250);
        // The explicit set: the hovered Review tile at 400 and nothing else.
        let hovered = |i: usize| i == 400;
        s.insert(1, entry(2048, 100), 0, &hovered); // speculation, one step ahead
        s.insert(2, entry(2048, 100), 0, &hovered); // speculation, two steps ahead
        s.insert(400, entry(2048, 100), 0, &hovered); // THE EXPLICIT FAR FRAME — the hovered tile

        assert!(
            has(&s, 400),
            "the frame the user asked for is not what an over-budget deposit takes"
        );
        assert!(!has(&s, 2), "…the farthest UNPROTECTED entry goes instead");
        assert!(has(&s, 1), "…and the nearest speculation stays");
        assert_eq!(s.len(), 2);

        // …AND IF ONLY PROTECTED ENTRIES REMAIN WHILE OVER BUDGET, THE LOOP STOPS. The overshoot
        // is bounded by the explicit set's own size (at most five frames since round X2) and is
        // stated in `evict_to_budget`'s doc; what it may never do is spin, panic, or take a
        // photograph off the screen.
        let all = |_: usize| true;
        s.insert(3, entry(2048, 100), 0, &all);
        assert_eq!(s.len(), 3, "nothing may be taken, so nothing is");
        assert_eq!(s.bytes(), 300, "…and the store is honestly over its 250-byte budget");
        s.set_effective_budget(100, 0, &all);
        assert_eq!(s.len(), 3, "a pressure shed over an all-protected store also stops");

        // The moment ONE of them is ordinary again — the pointer moved — the shed completes.
        s.set_effective_budget(100, 0, &|i: usize| i != 400);
        assert!(!has(&s, 400), "the un-pinned far frame is the farthest, and it goes first");

        // …AND THE PREDICATE THE APP ACTUALLY HANDS IT is `support::ExplicitSet::holds`, which is
        // `support::fast_evict_protected` — the SAME answer the VRAM cache's eviction takes
        // earlier in the same match arm. Driven here rather than described, so a change to
        // that predicate reaches this store's row and not only the VRAM one's.
        let mut s2 = small(250);
        let ex = crate::support::ExplicitSet::new(0, false, 0, 0, 500, Some(400), -1);
        let held = |i: usize| ex.holds(i);
        s2.insert(1, entry(2048, 100), 0, &held);
        s2.insert(2, entry(2048, 100), 0, &held);
        s2.insert(400, entry(2048, 100), 0, &held);
        assert!(has(&s2, 400), "the hovered tile's RAM resident survives the deposit that made it");
        assert!(!has(&s2, 2), "…and the farthest speculation goes in its place");
        assert!(has(&s2, 1));
    }

    #[test]
    fn symmetric_keeps_both_sides() {
        // A near-symmetric pair on OPPOSITE sides of the anchor both survive; the far one falls.
        let mut s = small(250);
        s.insert(9, entry(2048, 100), 10, &unprotected); // behind, d1
        s.insert(11, entry(2048, 100), 10, &unprotected); // ahead, d1
        s.insert(30, entry(2048, 100), 10, &unprotected); // far ahead, d20 → overflow → evicts 30 (itself, farthest)
        assert!(has(&s, 9) && has(&s, 11));
        assert!(!has(&s, 30));
    }

    #[test]
    fn oversize_single_entry_skipped() {
        let mut s = small(100);
        s.insert(5, entry(2048, 200), 0, &unprotected); // one frame bigger than the whole budget
        assert_eq!(s.len(), 0);
        assert_eq!(s.bytes(), 0);
        assert!(!has(&s, 5));
    }

    #[test]
    fn replace_adjusts_bytes() {
        let mut s = small(1000);
        s.insert(3, entry(2048, 100), 0, &unprotected);
        assert_eq!(s.bytes(), 100);
        s.insert(3, entry(2048, 300), 0, &unprotected); // same idx, bigger payload
        assert_eq!(s.len(), 1);
        assert_eq!(s.bytes(), 300); // old 100 dropped, new 300 counted
    }

    #[test]
    fn get_dim_rule() {
        let mut s = small(1000);
        s.insert(7, entry(2048, 100), 0, &unprotected);
        assert!(s.get(7, 2048).is_some()); // equal → serve
        assert!(s.get(7, 1024).is_some()); // stored larger than wanted → downsamples, serve
        assert!(s.get(7, 4096).is_none()); // stored smaller than wanted → must NOT serve (re-decode)
        assert!(s.get(99, 1024).is_none()); // absent
    }

    #[test]
    fn dim_satisfies_boundary() {
        assert!(dim_satisfies(3840, 3840)); // equal satisfies — the B2 near-stop case: a frame
        // B2 kept at the 4096 DCT stop is TAGGED with its want (3840, the stage-1 deposit-dim
        // convention: the want, never the pixel long side), so it satisfies want=3840 …
        assert!(!dim_satisfies(3840, 4096)); // … and does NOT falsely satisfy want=4096.
        assert!(dim_satisfies(4096, 3840)); // larger-bucket frame downsamples fine
        assert!(!dim_satisfies(2048, 3840)); // stale (pre-grow) frame stays wanted
    }

    /// v0.8.101 (S1 caveat): a frame lifted from a file's EMBEDDED PREVIEW is quarantined from the
    /// dim-bucket world STRUCTURALLY. `FrameSource::cache_dim` collapses it to
    /// `PREVIEW_CACHE_DIM`, and `dim_satisfies` refuses that value against EVERY want — including
    /// `0`, the one a plain `entry_dim >= want` would wave through. The other half of the
    /// guarantee is type-level and lives in falcon-decode: only `Lane::Thumb` can produce an
    /// `EmbeddedPreview` at all, so the fast tier's `browse_frame_rgba(.., Lane::Fast)` cannot
    /// return one to be stamped. v0.8.102 (F9) added the WRITE half this predicate cannot cover
    /// on its own — the tick drain rejects a `PREVIEW_CACHE_DIM` lander before the fast-cache
    /// insert and the L2 deposit, so the frame is neither presented nor parked.
    ///
    /// FALSIFIER (L28): drop the `entry_dim != PREVIEW_CACHE_DIM` term and the `want == 0` row
    /// fails immediately; make `cache_dim` return `want` for `EmbeddedPreview` (the "it's only a
    /// tag, the thumb cache has no buckets anyway" refactor) and EVERY row below fails — a 576 px
    /// iPhone preview would then satisfy a 2880 px scrub request the moment anything routed one
    /// into the fast cache, which is the exact bug the caveat names.
    #[test]
    fn an_embedded_preview_frame_can_never_satisfy_a_fast_tier_bucket() {
        use falcon_decode::{FrameSource, PREVIEW_CACHE_DIM};
        // The tag is the sentinel, whatever want the tier happened to be asking for.
        for want in [0u32, 1, 256, 2048, 2880, 3840, 8192] {
            assert_eq!(FrameSource::EmbeddedPreview.cache_dim(want), PREVIEW_CACHE_DIM);
            assert!(
                !dim_satisfies(FrameSource::EmbeddedPreview.cache_dim(want), want),
                "a preview-sourced frame must never satisfy want={want}"
            );
        }
        // …and a main-image frame is UNCHANGED: the stamp is byte-identical to the old `dim`.
        for want in [256u32, 2048, 2880, 3840] {
            assert_eq!(FrameSource::MainImage.cache_dim(want), want, "no behaviour change today");
            assert!(dim_satisfies(FrameSource::MainImage.cache_dim(want), want));
        }
    }

    #[test]
    fn deposit_wanted_dim_rule() {
        let mut s = small(1000);
        assert!(s.deposit_wanted(7, 2048)); // absent → deposit
        s.insert(7, entry(2048, 100), 0, &unprotected);
        assert!(!s.deposit_wanted(7, 2048)); // equal (FromRam echo) → no churn
        assert!(!s.deposit_wanted(7, 1024)); // resident is BETTER (shrink kept it) → keep it
        assert!(s.deposit_wanted(7, 3840)); // grow re-decode arrived → replace the stale one
    }

    #[test]
    fn deposit_replace_after_grow() {
        // The stage-3 L2 mirror: a stale 2048 entry is refused at bucket 3840, the grow re-decode
        // deposits over it (bytes adjust), and the store then serves the new bucket.
        let mut s = small(1000);
        s.insert(7, entry(2048, 100), 0, &unprotected);
        assert!(s.get(7, 3840).is_none()); // stale — the hit partition falls through to the pool
        assert!(s.deposit_wanted(7, 3840));
        s.insert(7, entry(3840, 300), 0, &unprotected); // the re-decoded frame lands
        assert_eq!(s.len(), 1);
        assert_eq!(s.bytes(), 300); // replace path adjusted (not 400)
        assert!(s.get(7, 3840).is_some()); // now serves
    }

    #[test]
    fn clear_empties() {
        let mut s = small(1000);
        s.insert(1, entry(2048, 100), 0, &unprotected);
        s.insert(2, entry(2048, 100), 0, &unprotected);
        s.clear();
        assert_eq!(s.len(), 0);
        assert_eq!(s.bytes(), 0);
    }

    #[test]
    fn budget_math() {
        // 25% of 32 GiB = 8 GiB → hits the cap exactly.
        assert_eq!(l2_budget_bytes(32 * GIB), 8 * GIB as usize);
        // 25% of 16 GiB = 4 GiB → under the cap.
        assert_eq!(l2_budget_bytes(16 * GIB), 4 * GIB as usize);
        // 25% of 96 GiB caps at 8 GiB.
        assert_eq!(l2_budget_bytes(96 * GIB), 8 * GIB as usize);
    }

    // ── Stage 2: the pressure controller ────────────────────────────────────────────────

    #[test]
    fn water_marks() {
        // Pin the constants the controller tests below rely on (32 GiB machine).
        assert_eq!(l2_low_water(32 * GIB), LOW_32); // 6% > the 1.5 GiB floor
        assert_eq!(l2_low_water(16 * GIB), L2_LOW_WATER_MIN); // 0.96 GiB → floored at 1.5 GiB
        assert_eq!(store32().effective_budget(), 8 * GIB as usize);
    }

    #[test]
    fn degrades_stepwise_under_sustained_pressure() {
        let mut s = store32(); // nominal 8 GiB, step 1 GiB
        let low = LOW_32 - 1; // just under LOW_WATER
        assert_eq!(
            s.pressure_sample(low, 0, false, &unprotected),
            PressureAction::Degrade { from: 8 * GIB as usize, to: 7 * GIB as usize }
        );
        assert_eq!(
            s.pressure_sample(low, 0, false, &unprotected),
            PressureAction::Degrade { from: 7 * GIB as usize, to: 6 * GIB as usize }
        );
        for _ in 0..3 {
            s.pressure_sample(low, 0, false, &unprotected); // 6 → 5 → 4 GiB
        }
        assert_eq!(s.effective_budget(), 3 * GIB as usize);
        // (Immediate eviction on each shrink is the set_effective_budget path, asserted in
        // `set_effective_budget_evicts_farthest_and_clamps_to_nominal` — Degrade calls it.)
    }

    #[test]
    fn hysteresis_dead_band_never_flaps() {
        let mut s = store32();
        // Oscillate avail across LOW_WATER but always UNDER the restore threshold: the budget may
        // only ever ratchet DOWN — an above-low sample must neither degrade nor restore.
        let mut budgets = vec![s.effective_budget()];
        for k in 0..10 {
            let avail = if k % 2 == 0 { LOW_32 - 1 } else { LOW_32 + 1 };
            let act = s.pressure_sample(avail, 0, false, &unprotected);
            if k % 2 == 1 {
                assert_eq!(act, PressureAction::None); // dead band: no action, no calm credit
            }
            budgets.push(s.effective_budget());
        }
        assert!(budgets.windows(2).all(|w| w[1] <= w[0]), "budget must be monotone under flap");
        // And the dead-band samples reset the calm streak: after this flapping, even repeated calm
        // needs the FULL streak again before the first restore.
        for _ in 0..(L2_CALM_SAMPLES - 1) {
            assert_eq!(s.pressure_sample(CALM_32, 0, false, &unprotected), PressureAction::None);
        }
        assert!(matches!(s.pressure_sample(CALM_32, 0, false, &unprotected), PressureAction::Restore { .. }));
    }

    #[test]
    fn restore_needs_full_calm_streak_and_a_dip_resets_it() {
        let mut s = store32();
        s.pressure_sample(LOW_32 - 1, 0, false, &unprotected); // one degrade: 8 → 7 GiB
        assert_eq!(s.effective_budget(), 7 * GIB as usize);
        // 3 calm samples — not enough (N = 4).
        for _ in 0..(L2_CALM_SAMPLES - 1) {
            assert_eq!(s.pressure_sample(CALM_32, 0, false, &unprotected), PressureAction::None);
        }
        // A dip resets the streak…
        assert!(matches!(s.pressure_sample(LOW_32 - 1, 0, false, &unprotected), PressureAction::Degrade { .. })); // 7→6
        for _ in 0..(L2_CALM_SAMPLES - 1) {
            assert_eq!(s.pressure_sample(CALM_32, 0, false, &unprotected), PressureAction::None);
        }
        // …so the 4th calm sample of the NEW streak is the first restore.
        assert_eq!(
            s.pressure_sample(CALM_32, 0, false, &unprotected),
            PressureAction::Restore { from: 6 * GIB as usize, to: 7 * GIB as usize }
        );
    }

    #[test]
    fn restore_never_overshoots_nominal() {
        let mut s = store32();
        s.pressure_sample(LOW_32 - 1, 0, false, &unprotected); // 8 → 7 GiB
        for _ in 0..L2_CALM_SAMPLES {
            s.pressure_sample(CALM_32, 0, false, &unprotected); // → Restore to 8 GiB (nominal)
        }
        assert_eq!(s.effective_budget(), 8 * GIB as usize);
        // Further calm samples at nominal: silent, forever.
        for _ in 0..10 {
            assert_eq!(s.pressure_sample(CALM_32, 0, false, &unprotected), PressureAction::None);
        }
        assert_eq!(s.effective_budget(), 8 * GIB as usize);
    }

    #[test]
    fn runtime_disable_clears_holds_then_reenables_at_floor() {
        // A 16 GiB machine: nominal 4 GiB, step 512 MiB, low_water 1.5 GiB (floored).
        let total = 16 * GIB;
        let mut s = L2Store::new(l2_budget_bytes(total), total);
        s.insert(1, entry(2048, MIB), 0, &unprotected); // some content so Disable provably clears
        let low = L2_LOW_WATER_MIN - 1;
        // 4096 → … → 512 MiB is 7 steps; the 8th would land at 0 < floor → Disable.
        for k in 0..7 {
            let act = s.pressure_sample(low, 0, false, &unprotected);
            assert!(matches!(act, PressureAction::Degrade { .. }), "step {k}: {act:?}");
        }
        assert_eq!(s.effective_budget(), 512 * MIB);
        assert_eq!(s.pressure_sample(low, 0, false, &unprotected), PressureAction::Disable { from: 512 * MIB });
        assert_eq!(s.len(), 0); // cleared
        // Held OFF: deposits and hits no-op, further pressure samples stay silent.
        assert!(!s.deposit_wanted(2, 2048));
        s.insert(2, entry(2048, MIB), 0, &unprotected);
        assert_eq!(s.len(), 0);
        assert!(s.get(2, 0).is_none());
        assert_eq!(s.pressure_sample(low, 0, false, &unprotected), PressureAction::None);
        // Re-enable needs the SAME full calm streak; a dead-band sample resets it.
        let calm = L2_LOW_WATER_MIN + L2_RESTORE_BAND;
        for _ in 0..(L2_CALM_SAMPLES - 1) {
            assert_eq!(s.pressure_sample(calm, 0, false, &unprotected), PressureAction::None);
        }
        assert_eq!(s.pressure_sample(calm - 1, 0, false, &unprotected), PressureAction::None); // dead band → reset
        for _ in 0..(L2_CALM_SAMPLES - 1) {
            assert_eq!(s.pressure_sample(calm, 0, false, &unprotected), PressureAction::None);
        }
        assert_eq!(s.pressure_sample(calm, 0, false, &unprotected), PressureAction::Reenable { to: 512 * MIB });
        // Working again, at the floor, and climbing from there on the next calm streak.
        assert!(s.deposit_wanted(2, 2048));
        s.insert(2, entry(2048, MIB), 0, &unprotected);
        assert_eq!(s.len(), 1);
        for _ in 0..L2_CALM_SAMPLES {
            s.pressure_sample(calm, 0, false, &unprotected);
        }
        assert_eq!(s.effective_budget(), 1024 * MIB); // floor + one 512 MiB step
    }

    /// **v0.8.181 (pre-merge review) — A GESTURE'S TRANSIENT RESERVATION MUST NOT COST THE WHOLE
    /// CACHE.** The wave-2 fast-tier hold parks decoded frames in `drain_buf` for the length of a
    /// drag; this controller sees only the RAM going missing. The shed is right and stays; the one
    /// step that is not a step — `Disable`, which clears the store and needs a four-sample calm
    /// streak to come back EMPTY at the floor — must not be bought by bytes that come back the
    /// moment the hand lifts.
    ///
    /// FALSIFIER (L28, RED-FIRST — RUN): delete the `hold` arm from `pressure_sample`'s
    /// `target < L2_RUNTIME_FLOOR` branch and block (2) reads
    /// `left: Disable { from: 536870912 } right: Degrade { .. }`; make the hold suppress `Degrade`
    /// as well and block (1) reddens; make the hold STICKY (ignore its release) and block (4)
    /// reddens.
    #[test]
    fn a_held_fast_tier_sheds_but_never_takes_the_sticky_disable() {
        // The same 16 GiB machine as the row above: nominal 4 GiB, step 512 MiB.
        let total = 16 * GIB;
        let mut s = L2Store::new(l2_budget_bytes(total), total);
        s.insert(1, entry(2048, MIB), 0, &unprotected);
        let low = L2_LOW_WATER_MIN - 1;

        // ── (1) THE SHED STILL HAPPENS. A hold is not a veto: a genuinely low box gives back every
        //        ordinary step it has, hand on the mouse or not.
        for k in 0..6 {
            let act = s.pressure_sample(low, 0, true, &unprotected);
            assert!(matches!(act, PressureAction::Degrade { .. }), "held step {k}: {act:?}");
        }
        assert_eq!(s.effective_budget(), 1024 * MIB, "six held steps: 4096 → 1024 MiB");

        // ── (2) THE STEP THAT WOULD DISABLE, HELD. 1024 − 512 = 512, which is the floor, so this is
        //        the last ordinary step; the NEXT one is the sticky one.
        assert_eq!(
            s.pressure_sample(low, 0, true, &unprotected),
            PressureAction::Degrade { from: 1024 * MIB, to: 512 * MIB }
        );
        let act = s.pressure_sample(low, 0, true, &unprotected);
        assert_eq!(
            act,
            PressureAction::None,
            "THE FIX: at the floor under a hold there is nothing left to give WITHOUT the sticky \
             clear, so the controller says nothing rather than clearing (got {act:?})"
        );
        assert_eq!(s.effective_budget(), 512 * MIB, "…and the floor is where it stopped");
        assert_eq!(s.len(), 1, "…and the photographer's cache is still there");

        // ── (3) IT DOES NOT SPAM. The hold can last a whole gesture; every sample inside it after
        //        the floor is a `None`, which is the no-log-spam rule this controller ships with.
        for _ in 0..20 {
            assert_eq!(s.pressure_sample(low, 0, true, &unprotected), PressureAction::None);
        }

        // ── (4) RELEASE, SAME PRESSURE ⇒ `Disable` IS REACHABLE AGAIN, IMMEDIATELY. The hold
        //        suppressed a transient; it never changed what a genuinely low box is owed.
        assert_eq!(s.pressure_sample(low, 0, false, &unprotected), PressureAction::Disable { from: 512 * MIB });
        assert_eq!(s.len(), 0, "the real shed clears, exactly as it always did");
    }

    /// …and the hold has NO effect anywhere else in the controller: it is one arm of one branch.
    ///
    /// FALSIFIER (L28): gate the calm zone or the dead band on `hold` and these rows redden.
    #[test]
    fn the_hold_touches_only_the_disable_step() {
        let mut s = store32();
        // Degrade under a hold is byte-identical to Degrade without one.
        assert_eq!(
            s.pressure_sample(LOW_32 - 1, 0, true, &unprotected),
            PressureAction::Degrade { from: 8 * GIB as usize, to: 7 * GIB as usize }
        );
        // The dead band is still silent, and still resets the streak.
        assert_eq!(s.pressure_sample(LOW_32 + 1, 0, true, &unprotected), PressureAction::None);
        // …and a calm streak restores THROUGH a hold — a held gesture is not a reason to keep a
        // budget shed after the machine has demonstrated headroom.
        for _ in 0..(L2_CALM_SAMPLES - 1) {
            assert_eq!(s.pressure_sample(CALM_32, 0, true, &unprotected), PressureAction::None);
        }
        assert_eq!(
            s.pressure_sample(CALM_32, 0, true, &unprotected),
            PressureAction::Restore { from: 7 * GIB as usize, to: 8 * GIB as usize }
        );
    }

    #[test]
    fn boot_floor_disables_for_the_session() {
        // Total 2 GiB → nominal 512 MiB < the 1 GiB boot floor → permanently off.
        let total = 2 * GIB;
        let mut s = L2Store::new(l2_budget_bytes(total), total);
        assert!(s.is_boot_disabled());
        assert!(!s.deposit_wanted(1, 2048));
        s.insert(1, entry(2048, MIB), 0, &unprotected);
        assert_eq!(s.len(), 0);
        assert!(s.get(1, 0).is_none());
        // The controller never acts — not even under calm (no restore path out of boot-disable).
        assert_eq!(s.pressure_sample(1, 0, false, &unprotected), PressureAction::None);
        assert_eq!(s.pressure_sample(64 * GIB, 0, false, &unprotected), PressureAction::None);
    }

    // ── v0.9.23 (one-pool round): the pure zone classifier vs the live controller ────────────

    #[test]
    fn pressure_zone_boundaries() {
        let total = 32 * GIB; // low_water = 1.92 GiB (6% wins), calm from 3.42 GiB
        assert_eq!(pressure_zone(LOW_32 - 1, total), PressureZone::Low);
        assert_eq!(pressure_zone(LOW_32, total), PressureZone::Dead); // boundary: NOT low
        assert_eq!(pressure_zone(CALM_32 - 1, total), PressureZone::Dead);
        assert_eq!(pressure_zone(CALM_32, total), PressureZone::Calm); // boundary: first calm
        // 16 GiB machine: the 1.5 GiB floor wins over 6% (0.96 GiB).
        let total16 = 16 * GIB;
        assert_eq!(pressure_zone(L2_LOW_WATER_MIN - 1, total16), PressureZone::Low);
        assert_eq!(pressure_zone(L2_LOW_WATER_MIN, total16), PressureZone::Dead);
        assert_eq!(pressure_zone(L2_LOW_WATER_MIN + L2_RESTORE_BAND, total16), PressureZone::Calm);
    }

    /// The anti-drift gate: for a sweep of avail values, the classifier's zone must agree with what
    /// a live controller DOES — `Low` ⟺ it degrades (from a healthy full budget), `Calm` ⟺ a full
    /// calm streak restores (from a degraded budget), `Dead` ⟺ neither. If someone ever forks the
    /// governor's thresholds from the valve's, this fails.
    #[test]
    fn pressure_zone_agrees_with_controller() {
        let total = 32 * GIB;
        for avail in [1, LOW_32 / 2, LOW_32 - 1, LOW_32, LOW_32 + 1, CALM_32 - 1, CALM_32, CALM_32 + GIB] {
            let zone = pressure_zone(avail, total);
            // Degrade probe: a fresh store at nominal.
            let mut s = store32();
            // v1.0 MERGE: `hold: false` at all three probes — this row is about the ZONE
            // CLASSIFIER agreeing with the controller, and the trunk's v0.8.179 fast-tier hold is a
            // separate mechanism with its own rows (it downgrades a reached `Disable` to a
            // floor-stopping `Degrade`; it does not move the Low/Calm/Dead boundaries this pins).
            let degraded =
                matches!(s.pressure_sample(avail, 0, false, &unprotected), PressureAction::Degrade { .. });
            assert_eq!(degraded, zone == PressureZone::Low, "degrade⟺Low at avail={avail}");
            // Restore probe: a store one step down, fed a full calm streak of this sample.
            let mut s = store32();
            s.pressure_sample(LOW_32 - 1, 0, false, &unprotected); // one degrade
            let mut restored = false;
            for _ in 0..L2_CALM_SAMPLES {
                restored |=
                    matches!(s.pressure_sample(avail, 0, false, &unprotected), PressureAction::Restore { .. });
            }
            assert_eq!(restored, zone == PressureZone::Calm, "restore⟺Calm at avail={avail}");
        }
    }

    #[test]
    fn set_effective_budget_evicts_farthest_and_clamps_to_nominal() {
        let mut s = small(300);
        s.insert(5, entry(2048, 100), 10, &unprotected); // d5
        s.insert(10, entry(2048, 100), 10, &unprotected); // d0 (anchor)
        s.insert(12, entry(2048, 100), 10, &unprotected); // d2
        s.set_effective_budget(150, 10, &unprotected); // room for one → drop 5 (d5) then 12 (d2)
        assert_eq!(s.len(), 1);
        assert!(has(&s, 10)); // the anchor-nearest survives
        s.set_effective_budget(usize::MAX, 10, &unprotected);
        assert_eq!(s.effective_budget(), L2_BOOT_FLOOR); // clamped to nominal, no overshoot
    }
}
