//! v0.9.4 (P4a, PLAN §66): the VIEW / NAVIGATION TRANSIENTS bundle — the last of the apply_scan
//! folder-swap fold. ONE struct grouping the thirteen loose view/nav transient handles so their
//! folder-swap reset happens BY CONSTRUCTION — a single [`ViewReset::reset_for_swap`] — instead of the
//! two scattered clusters `apply_scan` used to run (the `nav_dir.set(1)` in the payload block + the
//! twelve-line compare/pan/shown/blur/filmstrip reset lower down). The film.rs round-4 module doc
//! flagged exactly these filmstrip-gesture handles (`film_pos`/`film_base`/`film_follow`/`film_target`/
//! `film_dragging`/`last_film_key`) as "RESET to a start-centred default on swap, not cleared … folding
//! them in is a mechanical follow-up" — this is that follow-up, widened to the sibling view transients.
//!
//! SWAP-OWNER BUNDLE (a deliberate deviation from the §64 tier structs + this round's PerShotMeta/RotState,
//! which are EXCLUSIVE owners). `ViewReset` holds SHARED handles — `Rc` clones of the very `Cell`/`RefCell`s
//! the tick's `step_*` fns + ~20 UI-event handlers already read and write — rather than owning the cells
//! outright. WHY: these thirteen are the app's most pervasive DISPLAY/INPUT state (`compare`, `shown`,
//! `film_follow`, `scrubbing`, … are threaded through dozens of step signatures and handler closures, e.g.
//! ~15 distinct `film_follow` writers alone). Exclusive ownership would repoint every one of ~100 call
//! sites to `view.<field>` — a large, delicate churn on hot-path display state for ZERO functional gain,
//! against this round's hard "zero behaviour change" bar + the project's fragile-UI history. The bundle
//! delivers what P4a is after — the swap-reset CHOKEPOINT (`apply_scan` calls one `reset_for_swap`), the
//! COMPILE-TIME guard (the exhaustive destructure below), and the CONTRACT test — without that risk.
//! Promoting to exclusive ownership (drop the `Rc` wrappers, repoint the call sites) is a mechanical
//! follow-up if the tier symmetry is later wanted.
//!
//! BEHAVIOR-IDENTICAL: `reset_for_swap` sets each handle to the EXACT value `apply_scan` set it to today
//! (proven by `cargo test` + the boot parity spike). No logic changes.
use crate::*;

/// One acceptance point for both RAW selectors. Rejected requests must never leave
/// the UI ahead of the worker state; repeat clicks do not invalidate valid frames.
pub(crate) fn accept_raw_mode(
    app: &MainWindow,
    on: bool,
    raw: &AtomicBool,
    rot: &RotState,
    epoch: &AtomicU64,
) -> bool {
    let current = raw.load(Ordering::Relaxed);
    if app.get_opening_photo() || on == current {
        app.set_raw_mode(current);
        return false;
    }
    raw.store(on, Ordering::Relaxed);
    rot.set_raw_mode(on);
    app.set_raw_mode(on);
    // Publish configuration before its stamp; old in-flight results retain the old epoch.
    epoch.fetch_add(1, Ordering::Release);
    true
}

/// The view/navigation transient handles a folder swap resets (see the module doc for the bundle rationale).
/// Every field is an `Rc` clone of the live handle `main()` also hands to the tick + the UI handlers; the
/// swap is the ONE writer that touches all thirteen at once.
pub(crate) struct ViewReset {
    pub(crate) compare: Rc<Cell<bool>>,
    pub(crate) pan_carry: Rc<Cell<Option<PanCarry>>>, // v0.8.91 merge: the pan-snap fix's carry record (was the max-pan fraction tuple)
    // v1.0 MERGE: the shown record is a THREE-slot `support::ShownRec` on this trunk — (idx, raw,
    // present epoch) since v0.8.128 (SEL-LAG) — not the two-slot tuple the branch bundled. Spelled
    // through the alias so the next widening is one edit, in one place, and this bundle follows it.
    pub(crate) shown: Rc<RefCell<Option<crate::support::ShownRec>>>,
    pub(crate) last_seen: Rc<RefCell<usize>>,
    // v0.9.63 (merge of main v0.8.128, SEL-LAG P1): the backdrop key is a two-slot `BackdropKey`
    // (shown + in-flight), not a bare `Option<BlurKey>`. The bundle holds the same handle the tick
    // does and clears it through the ONE verb tick.rs exposes.
    pub(crate) blur_key: Rc<RefCell<tick::BackdropKey>>,
    // v0.9.63 (merge of main v0.8.129, W1-1): + a seventh slot, `select_gen`. This literal and the
    // one in main() are deliberately the same shape — a mismatch is a compile error, not a bug.
    // v1.0 MERGE: and it WAS one. Trunk's v0.8.187 (Y1) re-typed slot 1 from the fast cache's
    // `len()` to the fast tier's monotonic ARRIVAL COUNTER (`fast_gen`, a u64), and this bundle —
    // a mac-only file — merged clean carrying the pre-Y1 shape. The compile error the comment
    // promised is exactly what happened, which is the only reason this seam was not silent.
    pub(crate) last_film_key: Rc<RefCell<tick::FilmKey>>,
    pub(crate) film_pos: Rc<Cell<f32>>,
    pub(crate) film_base: Rc<Cell<i64>>,
    pub(crate) film_follow: Rc<Cell<bool>>,
    pub(crate) film_target: Rc<Cell<f32>>,
    pub(crate) film_dragging: Rc<Cell<bool>>,
    pub(crate) scrubbing: Rc<RefCell<bool>>,
    pub(crate) nav_dir: Rc<Cell<i32>>,
}

impl ViewReset {
    /// Folder-swap reset, BY CONSTRUCTION. Sets every view/nav transient to the start-centred default a
    /// swap lands on — byte-identical to the two scattered clusters `apply_scan` used to run. `start` = the
    /// new folder's landing index (the filmstrip recentres on it).
    ///
    /// COMPILE-TIME GUARD: opens with an EXHAUSTIVE destructure of `self` WITHOUT `..`, so any handle added
    /// to `ViewReset` later fails to compile until this reset gives it a swap value.
    ///
    /// The values (a skeptic should check these against the pre-fold `apply_scan` lines):
    ///   • `nav_dir` → 1 (forward); `compare` → false (exit compare); `pan_carry` → None (no zoom-carry leak).
    ///   • `shown` → None; `last_seen` → usize::MAX; `blur_key` → invalidated (shown AND pending);
    ///     `last_film_key` → the all-MAX sentinel
    ///     — force the display/backdrop/filmstrip rebuild for the new folder.
    ///   • the filmstrip gesture state (audit ORANGE): `film_pos`/`film_target` → `start` (recentre),
    ///     `film_base` → 0, `film_follow` → true (follow photo-nav again), `film_dragging` → false,
    ///     `scrubbing` → false — so a scroll/drag/scrub started on the OLD strip (live during an async scan)
    ///     can't strand the new strip off its start shot or freeze the frosted backdrop.
    pub(crate) fn reset_for_swap(&self, start: usize) {
        let Self {
            compare,
            pan_carry,
            shown,
            last_seen,
            blur_key,
            last_film_key,
            film_pos,
            film_base,
            film_follow,
            film_target,
            film_dragging,
            scrubbing,
            nav_dir,
        } = self;
        nav_dir.set(1);
        compare.set(false);
        pan_carry.set(None);
        *shown.borrow_mut() = None;
        *last_seen.borrow_mut() = usize::MAX;
        blur_key.borrow_mut().invalidate(); // v0.9.63: BOTH halves — shown AND in-flight
        *last_film_key.borrow_mut() =
            (i64::MIN, u64::MAX, usize::MAX, usize::MAX, u64::MAX, u64::MAX, u64::MAX);
        film_pos.set(start as f32);
        film_base.set(0);
        film_follow.set(true);
        film_target.set(start as f32);
        film_dragging.set(false);
        *scrubbing.borrow_mut() = false;
    }

    /// R3: the strip's free-scroll place `(film_pos, film_target)`, while the person has one.
    pub(crate) fn free_scroll(&self) -> Option<(f32, f32)> {
        (!self.film_follow.get()).then(|| (self.film_pos.get(), self.film_target.get()))
    }

    /// R3: puts it back after `reset_for_swap`, on a landing that keeps the photo on screen and the
    /// order. The strip's step clamps it to the new list on its next pass.
    pub(crate) fn restore_free_scroll(&self, (pos, target): (f32, f32)) {
        self.film_follow.set(false);
        self.film_pos.set(pos);
        self.film_target.set(target);
    }
}

#[cfg(test)]
impl ViewReset {
    /// Fresh, independent handles, for a rig that lands a list the way `apply_scan` does.
    pub(crate) fn for_test() -> Self {
        Self {
            compare: Rc::new(Cell::new(false)),
            pan_carry: Rc::new(Cell::new(None)),
            shown: Rc::new(RefCell::new(None)),
            last_seen: Rc::new(RefCell::new(usize::MAX)),
            blur_key: Rc::new(RefCell::new(tick::BackdropKey { shown: None, pending: None })),
            last_film_key: Rc::new(RefCell::new((0, 0, 0, 0, 0, 0, 0))),
            film_pos: Rc::new(Cell::new(0.0)),
            film_base: Rc::new(Cell::new(0)),
            film_follow: Rc::new(Cell::new(true)),
            film_target: Rc::new(Cell::new(0.0)),
            film_dragging: Rc::new(Cell::new(false)),
            scrubbing: Rc::new(RefCell::new(false)),
            nav_dir: Rc::new(Cell::new(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.9.63: a `BlurKey` standing for "a backdrop built for the OUTGOING folder". Its values are
    /// arbitrary; what matters is that it is `Some` in both slots before the reset runs.
    const STALE_BLUR_KEY: BlurKey = BlurKey { scene: 3, cm: 0 };

    /// A `ViewReset` with fresh (independent) handles, all populated to NON-default / stale values so the
    /// reset is observable. Standalone (no window) — the bundle is pure `Cell`/`RefCell` state.
    fn stale() -> ViewReset {
        let v = ViewReset {
            compare: Rc::new(Cell::new(true)),
            pan_carry: Rc::new(Cell::new(Some(PanCarry {
                px: 3.0,
                py: 4.0,
                fx: 0.01,
                fy: 0.02,
                aspect: 1.5,
            }))),
            shown: Rc::new(RefCell::new(Some((5, true, 7)))),
            last_seen: Rc::new(RefCell::new(9)),
            // v0.9.63: `BackdropKey` has two slots and the swap reset must clear BOTH — so seed a
            // stale key into each and let the assertions below prove neither survives.
            blur_key: Rc::new(RefCell::new(tick::BackdropKey {
                shown: Some(STALE_BLUR_KEY),
                pending: Some(STALE_BLUR_KEY),
            })),
            last_film_key: Rc::new(RefCell::new((1, 2, 3, 4, 5, 6, 7))),
            film_pos: Rc::new(Cell::new(99.0)),
            film_base: Rc::new(Cell::new(42)),
            film_follow: Rc::new(Cell::new(false)),
            film_target: Rc::new(Cell::new(99.0)),
            film_dragging: Rc::new(Cell::new(true)),
            scrubbing: Rc::new(RefCell::new(true)),
            nav_dir: Rc::new(Cell::new(-1)),
        };
        v
    }

    /// The swap-reset contract: populate every handle stale, `reset_for_swap(7)`, assert each lands on the
    /// exact value `apply_scan` set — the filmstrip recentres on `start`, the gesture state clears, the
    /// display/backdrop keys reset. Before this round the only proof a swap reset these was a live boot.
    #[test]
    fn reset_for_swap_sets_every_transient() {
        let v = stale();
        v.reset_for_swap(7);
        assert_eq!(v.nav_dir.get(), 1, "nav direction forward on swap");
        assert!(!v.compare.get(), "compare exited on swap");
        assert!(v.pan_carry.get().is_none(), "zoom-carry cleared on swap");
        assert_eq!(*v.shown.borrow(), None, "shown frame reset on swap");
        assert_eq!(*v.last_seen.borrow(), usize::MAX, "last-seen reset on swap");
        // v0.9.63: BOTH halves of the backdrop key. A reset that cleared only `shown` would let a
        // canvas already in flight for the OLD folder land and promote itself into the new one.
        assert!(v.blur_key.borrow().shown.is_none(), "shown blur key reset on swap");
        assert!(v.blur_key.borrow().pending.is_none(), "in-flight blur key reset on swap");
        assert_eq!(
            *v.last_film_key.borrow(),
            (i64::MIN, u64::MAX, usize::MAX, usize::MAX, u64::MAX, u64::MAX, u64::MAX),
            "last film key reset to the all-MAX sentinel on swap"
        );
        assert_eq!(v.film_pos.get(), 7.0, "filmstrip recentred on start");
        assert_eq!(v.film_base.get(), 0, "film base reset on swap");
        assert!(v.film_follow.get(), "filmstrip follows photo-nav again on swap");
        assert_eq!(v.film_target.get(), 7.0, "film target recentred on start");
        assert!(!v.film_dragging.get(), "film drag gesture cleared on swap");
        assert!(!*v.scrubbing.borrow(), "scrub gesture cleared on swap");
    }

    /// Idempotence: a second reset in a row equals one (same start-centred state), no panic.
    #[test]
    fn reset_for_swap_is_idempotent() {
        let v = stale();
        v.reset_for_swap(3);
        v.reset_for_swap(3);
        assert_eq!(v.nav_dir.get(), 1);
        assert!(!v.compare.get());
        assert_eq!(v.film_pos.get(), 3.0);
        assert_eq!(v.film_target.get(), 3.0);
        assert!(v.film_follow.get());
        assert!(!*v.scrubbing.borrow());
    }
}
