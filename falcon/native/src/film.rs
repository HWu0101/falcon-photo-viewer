//! v0.8.32 (architecture round 4, PLAN §64): the FILM / thumbnail / frost subsystem — the FINAL
//! extraction of the §64 chain. ONE struct owning the scattered filmstrip/Selection thumbnail + frost-
//! mip state so its folder-swap clearing happens BY CONSTRUCTION — a single [`Film::on_folder_swap`] —
//! instead of the hand-maintained checklist in `apply_scan` that history keeps proving fragile (the §61
//! `frost_map` stale-read bug — THIS very map; the v0.8.13→15 delete-identity episode). `main()` used to
//! clone FOUR separate film handles into the tick closure and thread them through the `step_*`
//! signatures; they now live here and one `Rc<Film>` clones in.
//!
//! BEHAVIOR-IDENTICAL: this round MOVES state and changes NO logic. The four stores keep the exact
//! interior-mutability discipline they had as loose handles (`thumbs`/`pending` are UI-thread-only
//! `Rc<RefCell>`; `frost`/`failed` stay `Arc<Mutex>` — the thumb POOL WORKERS insert into BOTH off-
//! thread, and the FAST decode pool READS `frost`). The clearing semantics below match the pre-
//! extraction `apply_scan` lines byte-for-byte (proven by `cargo test` + a boot-metric parity spike on
//! the testkit + the 66-shot folder).
//!
//! GAMUT PATH (v0.8.104, Round-A C2 — this paragraph used to read "SINGLE-PATH: the film tier has NO
//! develop/gamut clearing path"). Thumbnails were the last CM-BYPASS surface showing SHARP photo pixels:
//! the filmstrip and the Selection grid blitted SOURCE-gamut bytes beside a correctly-converted stage.
//! (v0.8.105 / W1 corrects the v0.8.104 wording, which claimed they were the last CM-bypass surface
//! FULL STOP — the frosted backdrop was still one, and is colour-managed at its composite now.) They are
//! baked into the output gamut by the thumb worker and stamped with `support::cm_bake_key`, so `thumbs`
//! IS a developed cache — one that carries its bake key per entry (`tick::ThumbEntry`).
//! v0.8.105 (W2/W4): `tick::regamut_invalidate_thumbs` therefore clears only `pending` and bumps
//! `thumb_gen`; it does NOT empty the map. Emptying it blanked every tile in the strip and the grid to
//! the "…" placeholder (their only fallback, the fast cache, is cleared on the same line) for the
//! 208–434 ms × N a re-decode of the visible window costs, AND it orphaned frost mips — `drain_thumbs`
//! evicts those by iterating THIS map's keys, so a mip whose index left it became unevictable (~40 MB
//! per switch). Instead `lookup_thumb` shows a stale-key tile while re-requesting it, the drain
//! overwrites it when the correct bake lands, and `drain_thumbs` also carries a cap check over
//! `frost`'s own keys so its bound no longer depends on this map at all. `frost` stays
//! gamut-INDEPENDENT on purpose — the blur mip is derived from the UNTRANSFORMED
//! decode, matching the mip the fast tier builds from its own source-gamut frame — so it is not cleared
//! by the gamut sweep. `drop_developed_caches` still touches NONE of these four stores (its signature is
//! fast/detail/roi/l2 only); the film arm is its named sibling. The rotation invalidation
//! POKES `thumbs`/`pending` per-index (`rotate_invalidate`) or folder-wide (`rotate_invalidate_all`), but
//! that is a targeted EVICTION, not a folder clear, and stays a cross-tier free fn reading `&film.<field>`
//! (byte-identical call sites). So `on_folder_swap` is the ONE clearing method — simpler than the two-path
//! DetailTier/RoiZoom, matching the round-1 FastTier shape — and there is NO double-clear to reason about.
//!
//! MEMBERSHIP (the conservative boundary — only unambiguously film-tier-OWNED, swap-cleared state):
//!   • `thumbs`  — the filmstrip/Selection thumbnail image cache (idx → (decoded `slint::Image`, the
//!                 CM bake key its pixels were converted for — `tick::ThumbEntry`, v0.8.105/W2); small
//!                 CPU images in RAM, independent of the VRAM-budgeted fast cache so the strip fills on
//!                 any machine). UI-thread-only (was `thumb_cache`).
//!   • `pending` — ids with a thumb decode in flight; dedups so a thumb is decoded at most once. The
//!                 thumb worker's ALWAYS-answer rule frees the slot on every reply (decoded OR failure
//!                 marker). UI-thread-only (was `thumb_pending`).
//!   • `frost`   — A3 (§61): the shared thumb-derived frosted-blur mip map (idx → (mip bytes, w, h, the
//!                 SOURCE gamut those pixels were downscaled from — `tick::FrostMap`, v0.8.106)). The
//!                 thumb pool workers INSERT (after each 256 px thumb decode); the FAST decode pool READS
//!                 it to skip the ~20 % per-frame `downscale_rgba` tax; the UI tick EVICTS it in LOCKSTEP
//!                 with `thumbs` (drain_thumbs, bounded to the same keys) + CLEARS on swap. Worker-shared
//!                 → `Arc<Mutex>` (the [`tick::FrostMap`] alias). Was `frost_map`. The fast pool is a
//!                 READER, not a co-owner — like `failed`'s cross-tier reader (step_cloud_retry), that
//!                 doesn't make the store fast-owned: it is produced-by-thumbs, evicted-with-thumbs,
//!                 swap-cleared-with-thumbs.
//!   • `failed`  — the (gen, idx) THUMBNAIL-decode failure latch (mirrors fast/detail/roi `failed`): a
//!                 file whose thumb decode Err'd is asked ONCE per folder instead of leaking a `pending`
//!                 slot forever + a permanently-black tile (the stuck-TIFF-thumbnail report). Keyed
//!                 (gen, idx) so a stale-folder failure can't blacklist the same index in the next folder
//!                 — the race-free keying discipline is SACRED (matches fast/roi `failed`). The thumb POOL
//!                 WORKER inserts off-thread → `Arc<Mutex>`. Was `thumb_failed`.
//!   • `pinned`  — v0.8.42 (Selection demand-loading): the shot indices currently VISIBLE in the open
//!                 Selection panel's grid viewport (+ a small row margin). `drain_thumbs` EXEMPTS these
//!                 from the farthest-from-anchor eviction (which used to empty exactly the tiles the
//!                 user was looking at once they browsed far from their picks on a >THUMB_CACHE_MAX
//!                 folder). Fed wholesale each tick by `step_selection` from the grid's live viewport;
//!                 EMPTY whenever the panel is closed (zero pinning by contract) and cleared on swap
//!                 like every owned store (the indices are folder-scoped). UI-thread-only `RefCell`.
//!
//! DELIBERATELY OUT (monotonic/not-swap-cleared signal, display-input state, or channel/atomic plumbing —
//! see the round-4 report):
//!   `thumb_gen` (the thumbnail-ARRIVAL generation `Rc<Cell<u64>>` — MONOTONIC, never reset per-folder
//!   [verified: `apply_scan` never touches it]; bumped by `drain_thumbs` on every current-folder landing
//!   + by `rotate_invalidate_all`, read by the filmstrip/Selection rebuild KEYS. It is the film-domain
//!   analogue of `det_epoch`/`fast_super`/`roi_busy` — a generation SIGNAL, not a swap-cleared cache — so
//!   it stays a loose handle exactly as those three did; folding it in would give `Film` a field
//!   `on_folder_swap` must NOT clear, breaking the clean "swap clears every owned field" invariant the
//!   template is built on). The thumb request/result CHANNELS (`thumb_req_tx` / `thumb_res_rx` — plumbing,
//!   parity with every prior round's channels). The `frost_thumb` gate + `frost_misses` counter (an
//!   `AtomicBool` config flag the pool reads off-thread + a per-perf-window counter reset per window, not
//!   per swap — cross-consumer/not-swap-cleared, parity with `fast_super`). And the filmstrip SCROLL /
//!   gesture DISPLAY state (`film_pos`, `film_base`, `film_follow`, `film_target`, `film_dragging`,
//!   `last_film_key` — input/display state consumed by non-film steps; RESET to a start-centred default on
//!   swap, not cleared, exactly like the `pan_carry` display-state precedent both prior rounds parked).
//!   Folding any of these in is a mechanical follow-up.
use crate::*;

/// The film / thumbnail tier's owned state (see the module doc for the membership boundary). Wrapped in an
/// `Rc<Film>` on the UI thread; the worker-shared stores (`frost`, `failed`, and since round 2b
/// `cloud_skipped`) are individually `Arc`-cloned into the pool workers at spawn — `frost` into BOTH the
/// thumb pool (writes it) and the fast decode pool (reads it), `failed` and `cloud_skipped` into the thumb
/// pool (writes them) — exactly as the loose handles were.
pub(crate) struct Film {
    pub(crate) defer_requests: Cell<bool>,
    pub(crate) thumbs: RefCell<HashMap<usize, crate::tick::ThumbEntry>>,
    pub(crate) pending: RefCell<HashSet<usize>>,
    pub(crate) frost: tick::FrostMap,
    pub(crate) failed: Arc<Mutex<HashSet<(u64, usize)>>>,
    pub(crate) pinned: RefCell<HashSet<usize>>,
    /// Pending requests that have been inside a visible window (filmstrip, Review panel or grid
    /// dock) while pending. Only these are retired when they leave every window; requests the
    /// blur feeder made for shots nobody has looked at are never retired. Pruned to `pending`.
    pub(crate) visible_pending: RefCell<HashSet<usize>>,
    /// The shot at the middle of the grid dock's viewport while the dock is open. `drain_thumbs`
    /// keeps thumbnails near it as well as near the current photo and the filmstrip, so rows the
    /// user scrolls back to are still in RAM when the dock is far from the current photo.
    pub(crate) grid_anchor: Cell<Option<usize>>,
    /// 2026-10-27 (round 2b review C-01): cached thumbnails whose frost mip `drain_thumbs`' belt
    /// trimmed. The frost map keeps the `THUMB_CACHE_MAX` nearest while the thumbnail cache may keep
    /// the whole folder, and only a thumbnail decode makes a mip, so a preview window whose thumbnails
    /// were cached never got its mips back. `step_frost_feed` asks for these again inside the window.
    /// An index leaves the set when its thumbnail lands again or is evicted (a stale entry for a
    /// thumbnail removed another way costs nothing: a missing thumbnail is a candidate anyway).
    pub(crate) mipless: RefCell<HashSet<usize>>,
    /// …and the preview window the frost feeder last asked within, `(lo, hi)` inclusive (empty while
    /// `lo > hi`). The belt never trims a mip inside it, so a mip the feeder has just made again is not
    /// trimmed again at once when the anchors are far apart (round 2b re-check T1).
    pub(crate) frost_window: Cell<(usize, usize)>,
    /// …and how many times the cache has lost thumbnails (an eviction, or a request retired or
    /// dropped), so the whole-folder feeder's cursor scans again from the current photo (review C-02).
    pub(crate) losses: Cell<u64>,
    /// 2026-10-27 (round 2b, owner: "do the check") → 2026-10-28 (the cloud-only round): the files
    /// the workers found only in the cloud just before reading them, `(gen, idx, file)`
    /// ([`support::CloudNotes`]). Written by all three workers (thumbnails `main.rs::run_thumb_job_with`,
    /// previews `run_fast_job_with`, full resolution the detail worker); read by every producer that
    /// would ask for that file again (the tiles, the backdrop, the whole folder, the previews and
    /// frames ahead, the hover), which ask for it no more this folder, and by the tiles, which show
    /// the cloud mark. Not a failure. A file on this device again leaves it
    /// (`tick::step_cloud_landed_with`, `tick::step_cloud_retry_with`); cleared on a folder swap.
    pub(crate) cloud_skipped: Arc<support::CloudNotes>,
    /// The cloud-only round (design 6): the photos on screen whose frame genuinely landed this tick
    /// (`tick::step_upload_drain`, beside the fast insert and the full-resolution landing), for
    /// `tick::step_cloud_landed_with` to re-check their notes and scan tag. Drained every tick.
    pub(crate) cloud_landed: RefCell<Vec<usize>>,
    pub(crate) visible_signal: Arc<(Mutex<(u64, usize)>, std::sync::Condvar)>,
    startup_at: Cell<Option<Instant>>,
    startup_request: Cell<(u64, usize)>,
    models_ready: Cell<bool>,
    any_thumb: Cell<bool>,
    first_presented: Cell<bool>,
    visible_presented: Cell<bool>,
}

impl Film {
    pub(crate) fn new() -> Film {
        Film {
            defer_requests: Cell::new(false),
            thumbs: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashSet::new()),
            frost: Arc::new(Mutex::new(HashMap::new())),
            failed: Arc::new(Mutex::new(HashSet::new())),
            pinned: RefCell::new(HashSet::new()),
            visible_pending: RefCell::new(HashSet::new()),
            grid_anchor: Cell::new(None),
            mipless: RefCell::new(HashSet::new()),
            frost_window: Cell::new((1, 0)),
            losses: Cell::new(0),
            cloud_skipped: Arc::new(Mutex::new(HashSet::new())),
            cloud_landed: RefCell::new(Vec::new()),
            visible_signal: Arc::new((Mutex::new((0, 0)), std::sync::Condvar::new())),
            startup_at: Cell::new(None), startup_request: Cell::new((0, 0)),
            models_ready: Cell::new(false), any_thumb: Cell::new(false),
            first_presented: Cell::new(false), visible_presented: Cell::new(false),
        }
    }

    pub(crate) fn arm_open(&self, request: u64, count: usize) {
        self.startup_at.set(Some(Instant::now()));
        self.startup_request.set((request, count));
        self.models_ready.set(false); self.any_thumb.set(false);
        self.first_presented.set(false); self.visible_presented.set(false);
    }

    /// Bound the exceptional hidden/failed-window case. The current photo remains
    /// exempt; this holds only optional work until actual visible pixels are presented.
    pub(crate) fn startup_hold(&self, now: Instant) -> bool {
        self.startup_request.get().1 > 0 && !self.visible_presented.get()
            && self.startup_at.get().is_some_and(|at| now.duration_since(at) < Duration::from_secs(3))
    }

    pub(crate) fn observe_models(&self, strip: &slint::VecModel<FilmItem>, grid: &slint::VecModel<FilmItem>, grid_open: bool) {
        use slint::Model;
        if self.startup_at.get().is_none() || self.visible_presented.get() { return; }
        let mut total = 0;
        let mut terminal = 0;
        let mut any = false;
        for row in strip.iter().chain(grid.iter().take(if grid_open { usize::MAX } else { 0 })) {
            if row.name.is_empty() { continue; }
            total += 1;
            any |= row.valid;
            // The cloud-only round: a cloud tile is settled too (it never gets a picture by itself),
            // or every cloud folder's open would hold the background work for the full 3 s.
            terminal += usize::from(row.valid || row.failed || row.unsupported || row.cloud);
        }
        self.any_thumb.set(any);
        self.models_ready.set(total > 0 && terminal == total);
    }

    /// Called only after Slint presented a frame, never just because workers finished.
    pub(crate) fn note_presented(&self) {
        let Some(at) = self.startup_at.get() else { return; };
        let (id, count) = self.startup_request.get();
        if self.any_thumb.get() && !self.first_presented.replace(true) {
            log_event(&format!("scan: request {id} stage={count} first visible thumbnail presented after {} ms", at.elapsed().as_millis()));
        }
        if self.models_ready.get() && !self.visible_presented.replace(true) {
            log_event(&format!("scan: request {id} stage={count} visible thumbnail window presented after {} ms", at.elapsed().as_millis()));
            *self.visible_signal.0.lock().unwrap_or_else(|e| e.into_inner()) = (id, count);
            self.visible_signal.1.notify_all();
        }
    }

    /// Folder-swap clearing, BY CONSTRUCTION. Absorbs the FOUR film-tier entries `apply_scan` used to run
    /// as scattered hand-written lines — the thumbnail cache, the in-flight-request dedup set, the thumb-
    /// derived frost-mip map, and the (gen,idx) failure latch. Called from the ONE `apply_scan` swap
    /// chokepoint. This is the SOLE clear of all four (no develop/gamut path reaches any of them — see the
    /// module doc), so unlike FastTier/DetailTier/RoiZoom there is NO double-clear here.
    ///
    /// SEMANTICS preserved EXACTLY (the round-4 contract — a skeptic should check these):
    ///   • `thumbs` / `pending` — cleared to empty (was `thumb_cache.borrow_mut().clear()` /
    ///     `thumb_pending.borrow_mut().clear()`). The old-folder indices are re-keyed on a swap, so both are
    ///     meaningless; a lingering `pending` id would also wrongly suppress the new folder's re-request.
    ///   • `frost` — cleared under its lock (was `frost_map.lock()…clear()`). A3 (§61): the old folder's
    ///     mips are meaningless (indices re-keyed) — clearing STOPS a new-folder fast decode reading a stale
    ///     same-index mip as its frost backdrop (the exact §61 bug this extraction hardens by construction).
    ///     A thumb worker's straggler mip for the OLD gen is refused under this lock
    ///     (`tick::park_frost_mip`, round 2b review P1), so this clear must stay AFTER `apply_scan`'s
    ///     generation bump, as the `cloud_skipped` clear already requires.
    ///   • `failed` — cleared under its lock (was `thumb_failed.lock()…clear()`). The (gen, idx) keys can't
    ///     collide across folders (the gen differs), so this is not needed for correctness — it keeps the
    ///     set BOUNDED across a long session of swaps. This line used to sit ~14 lines BELOW the other three
    ///     in `apply_scan` (separated by the rot/apply_slot/rot_acked clears); folding it up here is provably
    ///     inert — nothing between reads or writes `failed`, and no tick step runs mid-`apply_scan` (same
    ///     argument as round-2's `det_uploading.set(None)` move-up).
    ///   • ORDER among the four is irrelevant: they are independent stores and no reader runs between them on
    ///     the UI thread at the swap chokepoint (the thumb/fast workers touch `frost`/`failed` off-thread;
    ///     the next gen guards any straggler).
    ///   • `pinned` — v0.8.42: cleared to empty. The pins are ABSOLUTE indices of the OLD folder (re-keyed
    ///     on swap, meaningless — and a stale pin would wrongly shield a new-folder thumb from eviction).
    ///     `apply_scan` runs at the TOP of the tick, before `drain_thumbs`/`step_selection`, so no drain
    ///     ever sees a stale pin: the panel (if open) re-pins from the rebuilt grid the same tick.
    pub(crate) fn on_folder_swap(&self) {
        self.defer_requests.set(false);
        self.startup_at.set(None);
        self.thumbs.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        self.frost.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.pinned.borrow_mut().clear();
        self.visible_pending.borrow_mut().clear();
        self.grid_anchor.set(None);
        self.mipless.borrow_mut().clear();
        self.frost_window.set((1, 0));
        self.losses.set(0);
        self.cloud_skipped.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.cloud_landed.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_budget_releases_only_after_visible_pixels_are_presented_or_bounded_timeout() {
        let film = Film::new();
        let strip = slint::VecModel::from(vec![FilmItem { name: "photo".into(), valid: true, ..Default::default() }]);
        let grid = slint::VecModel::from(vec![FilmItem { name: "neighbor".into(), valid: false, ..Default::default() }]);
        film.arm_open(7, 21);
        let now = Instant::now();
        assert!(film.startup_hold(now));
        film.observe_models(&strip,&grid,true);
        film.note_presented();
        assert!(film.startup_hold(now), "a visible but unready grid must still get priority");
        assert_ne!(*film.visible_signal.0.lock().unwrap(),(7,21));
        film.observe_models(&strip,&grid,false);
        assert!(film.startup_hold(now), "model data alone is not a presented frame");
        film.note_presented();
        assert!(!film.startup_hold(now));
        assert_eq!(*film.visible_signal.0.lock().unwrap(),(7,21));
        film.arm_open(8,21);
        assert!(!film.startup_hold(Instant::now()+Duration::from_secs(4)), "hidden/failed windows cannot hold background discovery forever");
    }

    /// The cloud-only round (plan "Tests", Part 1): a cloud tile is a settled tile. It never gets a
    /// picture by itself, so a window of them must not hold the folder's background work for the full
    /// 3 s start-up budget.
    ///
    /// FALSIFIER (L28): drop `|| row.cloud` from `observe_models` and the window never reads ready.
    #[test]
    fn a_cloud_tile_counts_as_settled_for_the_open() {
        let film = Film::new();
        let strip = slint::VecModel::from(vec![
            FilmItem { name: "here".into(), valid: true, ..Default::default() },
            FilmItem { name: "cloud".into(), valid: false, cloud: true, ..Default::default() },
        ]);
        let grid = slint::VecModel::from(Vec::<FilmItem>::new());
        film.arm_open(3, 2);
        let now = Instant::now();
        film.observe_models(&strip, &grid, false);
        film.note_presented();
        assert!(!film.startup_hold(now), "the cloud tile is as settled as it will get");
    }

    /// A 1×1 placeholder frame — the same no-backend `slint::Image` construction the
    /// `rotate_invalidate_tests` (and fast.rs / detail.rs / roi.rs) use, so a thumbnail entry can be built
    /// under `cargo test` (no event loop).
    fn img() -> slint::Image {
        slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(1, 1))
    }

    /// Populate EVERY owned store to a known non-empty state (used by both clearing tests).
    fn populate(f: &Film) {
        f.thumbs.borrow_mut().insert(7, (img(), 1));
        f.pending.borrow_mut().insert(9);
        // frost value = (mip bytes, w, h, the mip's own source gamut — v0.8.106); a 1×1 mip is
        // enough to prove the map non-empty.
        f.frost.lock().unwrap().insert(7, (Arc::from(vec![0u8; 4]), 1, 1, falcon_color::Gamut::Srgb));
        f.failed.lock().unwrap().insert((3, 7));
        f.pinned.borrow_mut().insert(7); // v0.8.42: the Selection-viewport eviction exemption
        f.visible_pending.borrow_mut().insert(9);
        f.grid_anchor.set(Some(7));
        f.mipless.borrow_mut().insert(7);
        f.frost_window.set((5, 9));
        f.losses.set(3);
        f.cloud_skipped.lock().unwrap().insert((3, 7, support::CloudFile::Finished));
        f.cloud_landed.borrow_mut().push(7);
    }

    /// The direct swap-clear test (the review's "cheap first target", film edition): populate every owned
    /// store, call `on_folder_swap`, assert each is empty — INCLUDING the two `Arc<Mutex>` worker-shared
    /// stores (`frost`, `failed`). Before this round the only proof that a folder swap clears the film tier
    /// was a live boot; now it is a unit test that can't rot. This is the FIRST direct swap-clear test of
    /// the film/thumbnail state (the §61 frost-map miss lived precisely in this checklist).
    #[test]
    fn film_swap_clears_everything() {
        let f = Film::new();
        populate(&f);

        f.on_folder_swap();

        assert!(f.thumbs.borrow().is_empty(), "thumbnail cache cleared on swap");
        assert!(f.pending.borrow().is_empty(), "in-flight thumb-request set cleared on swap");
        assert!(f.frost.lock().unwrap().is_empty(), "thumb-derived frost-mip map cleared on swap");
        assert!(f.failed.lock().unwrap().is_empty(), "(gen,idx) thumb failure latch cleared on swap");
        assert!(f.pinned.borrow().is_empty(), "Selection-viewport pin set cleared on swap (v0.8.42)");
        assert!(f.visible_pending.borrow().is_empty(), "the retirement marks are folder-scoped too");
        assert_eq!(f.grid_anchor.get(), None, "so is the grid dock's eviction anchor");
        assert!(f.mipless.borrow().is_empty(), "the trimmed-mip notes are folder-scoped (round 2b)");
        assert_eq!(f.frost_window.get(), (1, 0), "…and so is the frost feeder's window");
        assert_eq!(f.losses.get(), 0, "…and so is the loss count");
        assert!(f.cloud_skipped.lock().unwrap().is_empty(), "…and so are the photos found online-only");
        assert!(f.cloud_landed.borrow().is_empty(), "…and the landings waiting for their re-check");
    }

    /// Idempotence: `on_folder_swap` on an already-clean tier is a no-op — this pins that a second swap in a
    /// row (or a swap on a fresh tier) stays clean, matching the FastTier/DetailTier/RoiZoom idempotence
    /// tests. (Unlike those, the film tier has no develop path, so there is no `clear_developed` overlap to
    /// exercise — the single `on_folder_swap` is the whole surface.)
    #[test]
    fn on_folder_swap_is_idempotent() {
        let f = Film::new();
        f.on_folder_swap();
        f.on_folder_swap();
        assert!(f.thumbs.borrow().is_empty());
        assert!(f.pending.borrow().is_empty());
        assert!(f.frost.lock().unwrap().is_empty());
        assert!(f.failed.lock().unwrap().is_empty());
        assert!(f.pinned.borrow().is_empty());
    }

    /// Populated double-clear (v0.8.34, from the review's skeptic notes): the idempotence test above starts
    /// EMPTY; this one POPULATES every store, then calls `on_folder_swap` TWICE in a row — the realistic
    /// "swap, then an immediate second swap before anything repopulates" case — and asserts a clean tier with
    /// no panic (the second clear on already-empty `Arc<Mutex>` stores must not wedge a lock or double-free).
    #[test]
    fn on_folder_swap_populated_double_clear() {
        let f = Film::new();
        populate(&f);
        f.on_folder_swap();
        f.on_folder_swap();
        assert!(f.thumbs.borrow().is_empty());
        assert!(f.pending.borrow().is_empty());
        assert!(f.frost.lock().unwrap().is_empty());
        assert!(f.failed.lock().unwrap().is_empty());
        assert!(f.pinned.borrow().is_empty());
    }
}
