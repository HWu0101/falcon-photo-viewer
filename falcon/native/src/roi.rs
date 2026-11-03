//! v0.8.31 (architecture round 3, PLAN §64): the ROI / ZOOM-TIER subsystem. ONE struct owning the
//! scattered adaptive-hi-res (zoomed-region) decode-tier state so its clearing happens BY CONSTRUCTION —
//! the same two-path pattern [`crate::DetailTier`] established in round 2 — instead of the hand-maintained
//! checklists in `apply_scan` + `drop_developed_caches` + the develop-config handlers that history keeps
//! proving fragile (the §61 `frost_map` miss; the v0.8.13→15 delete-identity episode). `main()` used to
//! clone FOUR separate ROI handles into the tick closure and thread them through the `step_roi_*`
//! signatures; they now live here and one `Rc<RoiZoom>` clones in.
//!
//! BEHAVIOR-IDENTICAL: this round MOVES state and changes NO logic. The four stores keep the exact
//! interior-mutability discipline they had as loose handles (`failed` stays `Arc<Mutex>` for the ROI pool
//! worker that inserts into it off-thread; the other three are UI-thread-only `Rc<RefCell>`). The clearing
//! semantics below match the pre-extraction lines byte-for-byte (proven by `cargo test` + a boot-metric
//! parity spike on the testkit + a 43-shot folder).
//!
//! MEMBERSHIP (the conservative boundary — only unambiguously ROI-tier-OWNED, swap/develop-cleared state):
//!   • `tiles`  — the decoded hi-res zoom tiles (current viewport + pre-loaded neighbours), each an
//!                output-gamut-converted SOURCE crop, LRU by bytes. The [`RoiTile`] vector. This is the
//!                ONE ROI store `drop_developed_caches` drops (colour-managed → output-gamut-dependent).
//!                UI-thread-only (was `roi_tiles`).
//!   • `region` — the overlay descriptor ([`RegionState`]) for the tiles currently shown (which shot +
//!                normalised rect they cover, so the overlay texture isn't re-set every frame). Cleared in
//!                LOCKSTEP with `tiles` at every site (the overlay is meaningless without its tiles) but it
//!                is NOT a colour cache, so it was never inside `drop_developed_caches` — it is a
//!                swap-EXTRA here + a direct field at the develop-config handlers. UI-thread-only (was
//!                `roi_region`).
//!   • `failed` — the (gen, idx) ROI-decode failure latch (mirrors `fast_failed`/`thumb_failed`): a source
//!                whose full decode fails (over-cap panorama, corrupt file) stops being re-requested +
//!                re-read for the folder (the retry-storm guard, ROI edition). The POOL WORKER inserts
//!                off-thread → it stays `Arc<Mutex<…>>` (was `roi_failed`). Cleared on swap only.
//!   • `src`    — the per-shot UNORIENTED source-dimension memo (a cheap header probe), read by the focus
//!                / 1:1 zoom math + `step_zoom_pct` so the true source long-side is known without
//!                re-opening the file on the UI thread. UI-thread-only (was `roi_src`). Cleared on swap
//!                only.
//!
//! DELIBERATELY OUT (shared with other consumers, not folder-swap-cleared, channel/atomic plumbing, or not
//! a UI-thread handle at all — see the round-3 report):
//!   `roi_busy` (the one-region-decode-in-flight `AtomicBool` tick↔worker handshake — the WORKER itself
//!   resets it on a stale-gen request; NOT swap-cleared — direct parity with round-2's `det_busy`),
//!   `roi_max` / `roi_ss` / `roi_base` (the zoom-tile cap / super-sample factor / decode floor — VRAM-
//!   budget TIER state co-managed with `fast_budget`/`detail_budget`/`detail_cap` inside the `VramTier`
//!   value-struct + rewritten by the OOM-recovery / dev sim-VRAM control — parity with the budgets both
//!   prior rounds parked), the ROI request/result CHANNELS (`roi_req_tx`, `roi_res_rx`, `roi_yuv_res_rx` —
//!   plumbing, parity with the `Detail`/`Decoded` channels FastTier/DetailTier excluded), the zoom/pan
//!   GESTURE + motion state (`last_view` motion-detect, `roi_settle` settle timer, `pan_carry` zoom-
//!   persistence, `pan_at_press` press anchor — DISPLAY/INPUT state read by non-ROI-tier steps, e.g.
//!   `step_display` consumes `pan_carry`; none is a swap/develop-cleared cache), and the ROI WORKER's own
//!   thread-LOCAL CPU source caches (`sources` / `sources_yuv` / `src_dims`) with their
//!   `custom_profile_gen` guard — those live inside the worker closure, are never UI-thread handles, and
//!   are untouched by this extraction (the guard's read/write sites stay byte-identical). Folding any of
//!   these in is a mechanical follow-up.
use crate::*;

/// The ROI/zoom-decode tier's owned state (see the module doc for the membership boundary). Wrapped in an
/// `Rc<RoiZoom>` on the UI thread; only the worker-shared `failed` latch is `Arc`-cloned into the ROI
/// decode worker at spawn, exactly as the loose handle was (the other three are UI-thread-only and the
/// worker never touches them — it replies on the `roi_res` / `roi_yuv_res` channels).
pub(crate) struct RoiZoom {
    pub(crate) tiles: RefCell<Vec<RoiTile>>,
    pub(crate) region: RefCell<Option<RegionState>>,
    pub(crate) failed: Arc<Mutex<HashSet<(u64, usize)>>>,
    pub(crate) src: RefCell<HashMap<usize, (u32, u32)>>,
    /// Keeping the view on a sort change: the tiles of a kept view, waiting for the next tick's
    /// `step_bookkeeping` to finish its clear (a swap resets `last_seen`, so that tick clears `tiles`).
    /// The tick moves them into `tiles` right after it.
    pub(crate) kept_tiles: RefCell<Vec<RoiTile>>,
}

impl RoiZoom {
    pub(crate) fn new() -> RoiZoom {
        RoiZoom {
            tiles: RefCell::new(Vec::new()),
            region: RefCell::new(None),
            failed: Arc::new(Mutex::new(HashSet::new())),
            src: RefCell::new(HashMap::new()),
            kept_tiles: RefCell::new(Vec::new()),
        }
    }

    /// Drop the DEVELOPED ROI tier — the colour-managed zoom tiles (`tiles`). This is the ONE store
    /// `drop_developed_caches` drops for the ROI tier (its ROI responsibility was ALWAYS exactly
    /// `roi_tiles` — `region`/`failed`/`src` were never inside it), and the ONE definition every
    /// develop-config path funnels through (map old-line → method in the round-3 report):
    ///   • `drop_developed_caches` — the LV5 gamut/ICC chokepoint (§6.0).
    ///   • the ROI-hi-res toggle, the res-limit handler, the dev sim-VRAM control, and the RAW-develop
    ///     toggle — each re-crops every tile under the new cap/mode/gamut and each cleared exactly
    ///     `roi_tiles` before.
    ///   • [`on_folder_swap`](Self::on_folder_swap) — a swap re-keys every index (plus the swap-EXTRAs).
    ///
    /// DELIBERATELY does NOT touch `region`: the overlay descriptor is cleared in LOCKSTEP with `tiles` at
    /// every one of those sites, but as a DIRECT field (it is not a colour cache and was never inside
    /// `drop_developed_caches` — the call sites clear it on the adjacent line; `on_folder_swap` folds it in
    /// as a swap-extra). A skeptic should check the two-path invariants:
    ///   1. Each develop-config site cleared exactly `roi_tiles` of the ROI DEVELOPED set — and since that
    ///      set is a SINGLE store, unlike round-2's detail tier there is NO subset-clearing handler here
    ///      (every site clears it in full → all five route through this method).
    ///   2. This method must NOT clear `failed`/`src`: `drop_developed_caches` calls it, and a gamut/ICC
    ///      switch must NOT drop the ROI failure latch or the source-dim memo (they are folder-scoped, not
    ///      gamut-scoped) — only a folder SWAP clears those (see below). The unit test pins this.
    pub(crate) fn clear_developed(&self) {
        self.tiles.borrow_mut().clear();
    }

    /// Folder-swap clearing, BY CONSTRUCTION. Absorbs the ROI-tier entries `apply_scan` used to run as
    /// scattered hand-written lines: the developed tiles via [`clear_developed`](Self::clear_developed)
    /// PLUS the three swap-EXTRAs — the overlay descriptor (`region`), the (gen,idx) failure latch
    /// (`failed`), and the source-dim memo (`src`) — which the gamut/develop-config paths deliberately
    /// leave alone. Called from the ONE `apply_scan` swap chokepoint.
    ///
    /// SEMANTICS preserved EXACTLY (the round-3 contract — a skeptic should check these):
    ///   • `tiles` — on the SWAP path this is cleared TWICE: once by `drop_developed_caches` (which the
    ///     swap still routes through, so the GAMUT path also drops it) and once here via `clear_developed`.
    ///     That double-clear is a DELIBERATE, harmless no-op: the second `clear()` runs on an already-empty
    ///     Vec, and NO reader/writer runs between the two on the UI thread (the intervening `apply_scan`
    ///     lines — fast/detail `on_folder_swap`, then this — touch only OTHER per-tier stores; the ROI
    ///     worker replies on channels and never writes `tiles`; no tick step runs mid-`apply_scan`). It
    ///     STAYS in `drop_developed_caches` because a GAMUT/ICC change — a different call site that never
    ///     reaches this method — must also drop these colour-managed crops (LV5 §6.0). Mirrors the
    ///     FastTier/DetailTier `cache` double-clear exactly.
    ///   • `region` / `failed` / `src` — cleared to empty here (was the standalone `roi_region.take()`,
    ///     `roi_failed…clear()`, `roi_src…clear()` lines in `apply_scan`). All three are folder-scoped:
    ///     the old overlay/latch/dims are meaningless once every index is re-keyed. `clear_developed` does
    ///     NOT touch them, so on the swap path each is cleared exactly ONCE (only `tiles` double-clears).
    ///     This ABSORBS the three scattered `apply_scan` lines — provably inert to move up, nothing between
    ///     reads/writes them and no tick step runs mid-`apply_scan` (same argument as round-2's
    ///     `det_uploading.set(None)`).
    ///   • ORDER among the four is irrelevant: they are independent stores and no reader runs between them
    ///     on the UI thread at the swap chokepoint.
    pub(crate) fn on_folder_swap(&self) {
        self.clear_developed();
        self.region.borrow_mut().take();
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.src.borrow_mut().clear();
        // Two landings can share a tick, and a kept tile's id is a position in the list before this swap.
        self.kept_tiles.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 placeholder frame — the same no-backend `slint::Image` construction the
    /// `rotate_invalidate_tests` (and fast.rs / detail.rs) use, so a `RoiTile` can be built under
    /// `cargo test` (no event loop).
    fn img() -> slint::Image {
        slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(1, 1))
    }

    /// Populate EVERY owned store to a known non-empty state (used by both clearing tests).
    fn populate(r: &RoiZoom) {
        r.tiles.borrow_mut().push(RoiTile {
            id: 7,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            src_long: 8192,
            img: img(),
            bytes: 4,
            turns: 0,
        });
        *r.region.borrow_mut() =
            Some(RegionState { id: 7, u0: 0.0, v0: 0.0, u1: 1.0, v1: 1.0, src_long: 8192 });
        r.failed.lock().unwrap().insert((3, 7));
        r.src.borrow_mut().insert(7, (8192, 5464));
    }

    /// The direct swap-clear test (the round-1 review's "cheap first target", ROI edition): populate every
    /// owned store, call `on_folder_swap`, assert each is empty — INCLUDING the three swap-EXTRAs
    /// (`region`, `failed`, `src`) that `clear_developed` alone leaves alone. Before this round the only
    /// proof that a folder swap clears the ROI tier was a live boot; now it is a unit test that can't rot.
    #[test]
    fn roi_zoom_swap_clears_everything() {
        let r = RoiZoom::new();
        populate(&r);

        r.on_folder_swap();

        assert!(r.tiles.borrow().is_empty(), "ROI zoom tiles cleared on swap");
        assert!(r.region.borrow().is_none(), "ROI overlay descriptor cleared on swap");
        assert!(r.failed.lock().unwrap().is_empty(), "(gen,idx) ROI failure latch cleared on swap");
        assert!(r.src.borrow().is_empty(), "ROI source-dim memo cleared on swap");
    }

    /// The TWO-PATH distinction, pinned: `clear_developed` (the gamut/ICC + develop-config path) drops
    /// EXACTLY the developed set {`tiles`} and must LEAVE `region`, `failed`, and `src` untouched. This is
    /// the load-bearing invariant — `drop_developed_caches` calls `clear_developed`, so if a future edit
    /// folded `failed`/`src` into it, every gamut/ICC switch would wrongly drop the ROI failure latch and
    /// the source-dim memo (folder-scoped state re-probed for no reason, and a doomed source retried), and
    /// this test would fail. (`region` is cleared alongside `tiles` at the develop-config CALL SITES, but
    /// NOT by this method — so a caller that wants the tiles gone without disturbing the overlay could,
    /// though today none does; the test documents the method's own contract.)
    ///
    /// ── v0.8.187 (W3): THE ROW THAT SHAPED THIS ROUND'S FIX ──────────────────────────────────────
    /// The audit found `roi.failed` missing from BOTH user-facing re-arm hands — `on_set_raw` (whose
    /// sibling clear of `detail.failed` carries the rationale "a shot that failed as JPG may develop
    /// fine as RAW", which is about the SOURCE and applies identically here) and `on_retry_detail`
    /// ("try this photo again", which cleared every sibling latch but this one). The obvious fix is
    /// to fold `failed` into `clear_developed`, and it is WRONG: `drop_developed_caches` calls this
    /// method, so a gamut/ICC switch — which changes no source and can prove nothing about a decode
    /// failure — would drop a folder-scoped latch on every colour change. This row is what stops
    /// that, so the two clears landed at the CALL SITES instead and this row stayed green.
    #[test]
    fn clear_developed_clears_only_the_tiles() {
        let r = RoiZoom::new();
        populate(&r);

        r.clear_developed();

        assert!(r.tiles.borrow().is_empty(), "clear_developed drops the ROI zoom tiles");
        assert!(
            r.region.borrow().is_some(),
            "clear_developed MUST preserve the overlay descriptor (a swap-extra, cleared at call sites)"
        );
        assert!(
            !r.failed.lock().unwrap().is_empty(),
            "clear_developed MUST preserve the ROI failure latch (folder-scoped; only a swap clears it)"
        );
        assert!(
            !r.src.borrow().is_empty(),
            "clear_developed MUST preserve the source-dim memo (folder-scoped; only a swap clears it)"
        );
    }

    /// Idempotence: the `tiles` double-clear on the swap path (drop_developed_caches THEN on_folder_swap)
    /// is a no-op on an already-empty Vec — this pins that so a future reader can't mistake the overlap for
    /// a bug. Also proves a second swap on a fresh tier stays clean.
    #[test]
    fn on_folder_swap_is_idempotent() {
        let r = RoiZoom::new();
        r.on_folder_swap();
        r.on_folder_swap();
        assert!(r.tiles.borrow().is_empty());
        assert!(r.region.borrow().is_none());
        assert!(r.failed.lock().unwrap().is_empty());
        assert!(r.src.borrow().is_empty());
    }
}
