//! v0.8.26 (architecture round 1, PLAN §64): the FAST-TIER subsystem. ONE struct owning the
//! scattered fast-decode-tier state so its folder-swap clearing happens BY CONSTRUCTION — a single
//! [`FastTier::on_folder_swap`] — instead of the hand-maintained checklist in `apply_scan` that
//! history keeps proving fragile (the §61 `frost_map` miss; the v0.8.13→15 delete-identity episode).
//! `main()` used to clone ~4 separate fast handles into the tick closure and thread them through the
//! `step_*` signatures; they now live here and one `Rc<FastTier>` clones in.
//!
//! BEHAVIOR-IDENTICAL: this round MOVES state and changes NO logic. The four stores keep the exact
//! interior-mutability discipline they had as loose handles; the clearing semantics below match the
//! pre-extraction `apply_scan` lines byte-for-byte (proven by `cargo test` + a boot-metric parity
//! spike on the testkit + the 66-shot folder).
//!
//! MEMBERSHIP (the conservative boundary — only unambiguously fast-tier-OWNED, swap-cleared state):
//!   • `cache`     — the fast scrub-tier VRAM textures (the [`tick::FastCache`] map: id → decoded
//!                   frame texture + kept blur mip). UI-thread-only (was `Rc<RefCell<…>>`).
//!   • `uploading` — ids currently in flight on the upload thread (was `uploading_fast`); prefetch +
//!                   readiness treat them as in-progress, the drain/Failed-reply clears them (gen-
//!                   checked, so a stale-folder result never resurrects one). UI-thread-only.
//!   • `failed`    — the (gen, idx) fast-decode failure latch (was `fast_failed`): a source whose
//!                   fast decode Err'd (corrupt file, or an HEIC with no OS HEVC extension) is asked
//!                   ONCE per folder instead of re-queued every tick (the retry-storm guard). The
//!                   POOL WORKERS insert off-thread → it stays `Arc<Mutex<…>>`.
//!   • `pump`      — the nearest-first prefetch request queue + the set currently decoding
//!                   ([`Pump`]). The POOL WORKERS pop from it off-thread → `Arc<(Mutex, Condvar)>`.
//!
//! DELIBERATELY OUT (shared with other tiers, or not swap-cleared — see the round-1 report):
//!   `fast_super` (sampling-mode atomic, also read by bench + the gamut handler), `fast_decodes` /
//!   `frost_misses` (perf-line counters, reset per perf-window not per-swap), `fast_budget` (co-
//!   managed with `detail_budget` inside the `VramTier`), `scrub_dim_atomic` (retargeted by
//!   step_adaptive_res), the `Decoded` result channel, and the RAM `L2` (its own module — explicitly
//!   out of scope this round). Folding any of these in is a mechanical follow-up.
use crate::*;

/// The fast-decode tier's owned state (see the module doc for the membership boundary). Wrapped in an
/// `Rc<FastTier>` on the UI thread; the two worker-shared stores (`failed`, `pump`) are individually
/// `Arc`-cloned into each decode-pool worker at spawn, exactly as the loose handles were.
pub(crate) struct FastTier {
    pub(crate) cache: RefCell<tick::FastCache>,
    /// v1.0.0-rc (queue item 31, §3.C.2) — **THE ONE INDEX THE EVICTION MAY NOT TAKE WHILE THE
    /// POINTER IS ON IT.**
    ///
    /// The fast eviction ranks by how far OUTSIDE the prefetch window a frame sits, so a frame
    /// fetched for a Review tile two hundred shots from `current` is the FIRST candidate on the very
    /// next over-budget insert — a fetch/evict ping-pong of exactly the class the v0.3.56 window-aware
    /// key was written to kill. This pin joins `support::displayed_shot`'s term at that ONE eviction
    /// site (never a second spelling of the exemption), and `step_hover_preview` is its only writer:
    /// it holds the SHOWN index and is cleared to `None` the moment nothing is shown, so the frame
    /// becomes ordinary again as soon as the pointer leaves. Row:
    /// `a_hovered_frame_is_not_evicted_under_the_pointer`.
    pub(crate) hover_pin: Cell<Option<usize>>,
    /// v1.0.0-rc (FIX 2026-09-07) — **THE INDEX THE HOVER PREVIEW IS ASKING FOR, WHERE THE
    /// DECODE POOL'S WORKERS CAN READ IT**; `-1` is no ask.
    ///
    /// `tick::HoverTier::requested` is a `Cell` on the UI thread and the pool's workers are
    /// other threads, so the ask itself is invisible to them — which is why the fast pool
    /// popped a far hover ask and dropped it as a stale window job, every tick, for as long as
    /// the pointer rested (the owner's 2026-09-07 field report). This atomic is that cell's
    /// cross-thread mirror: `step_hover_preview` writes it in the same statement group that
    /// sets `requested`, from the same value, and is the only writer that puts an INDEX in it
    /// (L28 — one derivation, one line); [`FastTier::on_folder_swap`] below clears it to `-1`
    /// beside the pin.
    ///
    /// THERE ARE **TWO** PRODUCTION READING PREDICATES, one per filter the ask must survive, at
    /// FOUR load sites between them (`main.rs`'s pre-decode test and its supersession watch, and
    /// one per tick step below — counted at the tree) [09-07 (b), D-V3:
    /// this said "The one production reader is `support::fast_job_stale`", and the drain's was
    /// added the same day, after the owner measured the second half of the same bug]:
    /// `support::fast_job_stale`, which the decode-pool WORKERS reach through the clone in
    /// `FastWorkerCtx::hover_ask`, and `support::fast_frame_exempt`, which the UPLOAD DRAIN and the
    /// prefetch's backpressure count read straight off this field on the UI thread
    /// (`tick::step_upload_fast`, `tick::step_prefetch_fast`). The first keeps the far ask from
    /// being dropped undecoded; the second keeps its finished frame from being parked as
    /// speculation for the gesture hold's 3 000 ms tail. Rows:
    /// `the_hover_ask_is_mirrored_where_the_fast_workers_can_read_it` and
    /// `a_parked_gate_publishes_the_hover_previews_frame`.
    pub(crate) hover_ask: Arc<AtomicI64>,
    pub(crate) uploading: RefCell<HashSet<usize>>,
    pub(crate) failed: Arc<Mutex<HashSet<(u64, usize)>>>,
    pub(crate) pump: Arc<(Mutex<Pump>, Condvar)>,
}

impl FastTier {
    pub(crate) fn new() -> FastTier {
        FastTier {
            cache: RefCell::new(HashMap::new()),
            hover_pin: Cell::new(None), // v1.0.0-rc (item 31): nothing hovered at birth
            // v1.0.0-rc (FIX 2026-09-07): …and nothing asked for either.
            hover_ask: Arc::new(AtomicI64::new(-1)),
            uploading: RefCell::new(HashSet::new()),
            failed: Arc::new(Mutex::new(HashSet::new())),
            pump: Arc::new((
                Mutex::new(Pump {
                    queue: VecDeque::new(),
                    inflight: HashSet::new(),
                    costly_q: HashSet::new(),
                    costly_inflight: HashSet::new(),
                    local_only_q: HashSet::new(),
                }),
                Condvar::new(),
            )),
        }
    }

    /// Folder-swap clearing, BY CONSTRUCTION. Absorbs the four fast-tier entries `apply_scan` used to
    /// run as scattered hand-written lines (fast VRAM cache, the (gen,idx) failure latch, the in-flight
    /// upload markers, and the pump's pending queue). Called from the ONE `apply_scan` swap chokepoint.
    ///
    /// SEMANTICS preserved EXACTLY (the round-1 contract — a skeptic should check these three):
    ///   • `cache` — cleared here AND (on the swap path only) by `drop_developed_caches` in the line
    ///     immediately above the call. That double-clear is a DELIBERATE, harmless no-op: the second
    ///     `HashMap::clear()` runs on an already-empty map. `cache` STAYS in `drop_developed_caches`
    ///     because a GAMUT/ICC change — a different call site that never reaches this method — must
    ///     also drop these colour-managed textures (LV5 §6.0). So the two chokepoints intentionally
    ///     overlap on this one store, and both remain correct.
    ///   • `pump` — the QUEUE only, NOT `inflight`. This mirrors `apply_scan`'s original
    ///     `m.lock()…queue.clear()` exactly. A worker has already popped the `inflight` ids; it will
    ///     self-clear them (an unsent stale-skip / decode-fail) or the tick drain will (a sent frame),
    ///     both under the NEW generation, and the gen check drops any stale-folder decode before it is
    ///     ever displayed. Clearing `inflight` here would instead let those ids be immediately re-
    ///     queued for a redundant decode.
    ///     v0.8.112: the two cost-class stores follow their partners EXACTLY — `costly_q` is a
    ///     property of the queue (cleared with it; the tick rebuilds both together), `costly_inflight`
    ///     is a property of `inflight` (preserved; the same worker/drain that clears an id's
    ///     `inflight` entry clears its costly entry). That is what makes the cap's accounting survive
    ///     a folder swap: the surviving HEVC decodes keep being counted as the costly work they are,
    ///     instead of being re-derived from a shot list that no longer contains them (P5/P12).
    ///   • ORDER among the four is irrelevant: they are independent stores and no reader runs between
    ///     them on the UI thread at the swap chokepoint (verified — the intervening `apply_scan` lines
    ///     touch only OTHER per-folder stores).
    pub(crate) fn on_folder_swap(&self) {
        self.cache.borrow_mut().clear();
        // v1.0.0-rc (item 31): the hover pin is an INDEX into the outgoing folder, so it joins the
        // four stores this chokepoint clears. `step_hover_preview` lets go of it on the next tick
        // anyway — but NOT because the swap makes its gate false [09-07, skeptic A Y2: this
        // parenthetical said "the gen check makes its gate false for one tick" and there is no such
        // check and no such effect]. `hover-preview-live` is `hover-to-preview && sel-open &&
        // !immersive && !popup-open && !compare`, five terms with NO generation in them, and a scan
        // or a swap touches none of the five. What lets go is the PIN, which follows the PUBLISHED
        // card: the new folder publishes its own record or the default, and the pin is written from
        // that. A stale index living even one tick inside an exemption predicate is still the L43
        // shape, and this is the one place that cannot be forgotten.
        self.hover_pin.set(None);
        // v1.0.0-rc (FIX 2026-09-07): the ask is an INDEX into the outgoing folder too, and it
        // feeds an exemption that WORKER THREADS read — the same L43 shape as the pin above, one
        // thread further out. Row: the swap leg of
        // `the_hover_ask_is_mirrored_where_the_fast_workers_can_read_it`.
        self.hover_ask.store(-1, Ordering::Relaxed);
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.uploading.borrow_mut().clear();
        // v0.8.112: the queue's cost marks go with the queue. v0.8.114: via `Pump::clear_queue`, the
        // one method every clear site now calls (backpressure, this swap, the VRAM-OOM recovery) —
        // hand-writing the pair is what let the OOM site drift.
        self.pump.0.lock().unwrap_or_else(|e| e.into_inner()).clear_queue();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 placeholder frame — the same no-backend `slint::Image` construction the
    /// `rotate_invalidate_tests` use, so a `FastEntry` can be built under `cargo test` (no event loop).
    fn img() -> slint::Image {
        slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(1, 1))
    }

    /// The FIRST direct test of swap-clearing in the codebase (the review's "cheap first target"):
    /// populate EVERY owned store, call `on_folder_swap`, assert each is empty — AND that `pump.inflight`
    /// SURVIVES (the deliberate queue-only semantic). Before this round the only proof that a folder
    /// swap clears the fast tier was a live boot; now it is a unit test that can't rot.
    #[test]
    fn fast_tier_swap_clears_everything() {
        let fast = FastTier::new();
        // Populate every store the swap is responsible for.
        let blur: Arc<[u8]> = Arc::from(vec![0u8; 4]);
        fast.cache.borrow_mut().insert(
            7,
            FastEntry { img: img(), w: 2, h: 1, blur, bw: 1, bh: 1, dim: 2048, turns: 0 },
        );
        fast.uploading.borrow_mut().insert(7);
        fast.failed.lock().unwrap().insert((3, 7));
        {
            let (m, _) = &*fast.pump;
            let mut p = m.lock().unwrap();
            p.queue.push_back(7);
            p.costly_q.insert(7); // v0.8.112: 7 was admitted as costly work — a queue property
            p.inflight.insert(9); // a worker is mid-decode on 9 — this MUST survive the swap
            p.costly_inflight.insert(9); // …and so must the fact that 9 is a COSTLY decode
        }

        fast.on_folder_swap();

        assert!(fast.cache.borrow().is_empty(), "fast VRAM cache cleared on swap");
        assert!(fast.uploading.borrow().is_empty(), "in-flight upload markers cleared on swap");
        assert!(fast.failed.lock().unwrap().is_empty(), "(gen,idx) failure latch cleared on swap");
        let (m, _) = &*fast.pump;
        let p = m.lock().unwrap();
        assert!(p.queue.is_empty(), "prefetch queue cleared on swap");
        assert!(p.costly_q.is_empty(), "…and its cost marks with it (v0.8.112)");
        assert!(
            p.inflight.contains(&9),
            "pump INFLIGHT survives swap — the worker/drain owns it, gen-guarded (queue-only clear)"
        );
        assert!(
            p.costly_inflight.contains(&9),
            "v0.8.112: …and so does its COST CLASS. This is the audit's P5/P12 fix — the surviving \
             HEVC decode must keep spending the cap across the swap, which it cannot do if the class \
             is re-derived from a shot list that no longer holds that index"
        );
    }

    /// Idempotence: the `cache` double-clear on the swap path (drop_developed_caches THEN
    /// on_folder_swap) is a no-op on an already-empty map — this pins that so a future reader can't
    /// mistake the overlap for a bug.
    #[test]
    fn on_folder_swap_is_idempotent() {
        let fast = FastTier::new();
        fast.on_folder_swap();
        fast.on_folder_swap();
        assert!(fast.cache.borrow().is_empty());
        assert!(fast.uploading.borrow().is_empty());
        assert!(fast.failed.lock().unwrap().is_empty());
        assert!(fast.pump.0.lock().unwrap().queue.is_empty());
    }
}
