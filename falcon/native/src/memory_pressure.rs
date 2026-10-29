//! Mac shared-memory pressure: keep requested photos, release work done ahead of the user.
//! Decisions and cache operations also run in Windows unit tests. [`shed_work`] is shared with the
//! background release on every platform (`background_memory`); the pressure hold stays Mac-only.
#![cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]

use crate::*;

#[derive(Default)]
pub(crate) struct Pressure {
    pub(crate) active: bool,
    calm: u32,
}

impl Pressure {
    /// Called only for a successful, once-per-second memory sample. A failed sample never
    /// releases the hold. Use the RAM cache's existing low-water/recovery band and streak.
    pub(crate) fn sample(&mut self, zone: l2::PressureZone) -> bool {
        let before = self.active;
        match zone {
            l2::PressureZone::Low => { self.active = true; self.calm = 0; }
            l2::PressureZone::Dead => self.calm = 0,
            l2::PressureZone::Calm => {
                self.calm = self.calm.saturating_add(1);
                if self.calm >= l2::L2_CALM_SAMPLES { self.active = false; }
            }
        }
        self.active != before
    }

    pub(crate) fn publish_gate(&self, otherwise: support::PublishGate) -> support::PublishGate {
        if self.active { support::PublishGate::Park } else { otherwise }
    }
}

/// Run before the upload/dispatch steps on every held tick, not just on entry: work already
/// in flight can land after the initial shed. Never remove a running job's accounting marks.
/// Returns the previews removed: the tick hands it to `background_memory::release_after_trim`, so the
/// panels that are not refreshing let go of the stand-ins their rows still hold (round 2c review P1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn shed(
    explicit: &support::ExplicitSet,
    fast: &FastTier,
    detail: &DetailTier,
    fast_gen: &Cell<u64>,
    drain: &RefCell<VecDeque<Decoded>>,
    parked_full: &RefCell<Option<Detail>>,
    sent: &RefCell<HashSet<usize>>,
    parked_done: &RefCell<Vec<support::UploadDone>>,
) -> usize {
    let mut cache = fast.cache.borrow_mut();
    let before = cache.len();
    cache.retain(|&id, _| explicit.holds(id));
    let shed_previews = before - cache.len();
    if shed_previews > 0 { fast_gen.set(fast_gen.get().wrapping_add(1)); }
    drop(cache);
    detail.cache.borrow_mut().retain(|&id, _| explicit.holds(id));
    detail.order.borrow_mut().retain(|&id| explicit.holds(id));
    shed_work(explicit, fast, detail, drain, parked_full, sent, parked_done);
    shed_previews
}

/// The queued, parked and drained half of [`shed`]: speculative work that has not reached a cache.
/// The background release (`background_memory`) runs this every held tick and shrinks the caches
/// gradually itself. Same rule as above: a running job keeps its accounting marks.
pub(crate) fn shed_work(
    explicit: &support::ExplicitSet,
    fast: &FastTier,
    detail: &DetailTier,
    drain: &RefCell<VecDeque<Decoded>>,
    parked_full: &RefCell<Option<Detail>>,
    sent: &RefCell<HashSet<usize>>,
    parked_done: &RefCell<Vec<support::UploadDone>>,
) {
    drain.borrow_mut().retain(|frame| explicit.holds(frame.id));
    let mut parked = parked_full.borrow_mut();
    if parked.as_ref().is_some_and(|frame| !explicit.holds(frame.id)) {
        sent.borrow_mut().remove(&parked.take().unwrap().id);
    }
    parked_done.borrow_mut().retain(|done| match done {
        support::UploadDone::Fast { id, .. } if !explicit.holds(*id) => {
            fast.uploading.borrow_mut().remove(id);
            false
        }
        support::UploadDone::Detail { id, .. } if !explicit.holds(*id) => {
            if detail.uploading.get() == Some(*id) { detail.uploading.set(None); }
            false
        }
        _ => true,
    });
    let mut pump = fast.pump.0.lock().unwrap_or_else(|e| e.into_inner());
    pump.queue.retain(|&id| explicit.holds(id));
    pump.costly_q.retain(|&id| explicit.holds(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_pauses_speculation_immediately_and_requires_sustained_recovery() {
        use l2::PressureZone::*;
        let mut p = Pressure::default();
        assert!(p.sample(Low));
        assert_eq!(p.publish_gate(support::PublishGate::Full), support::PublishGate::Park);
        assert!(!support::full_res_publish_held(p.publish_gate(support::PublishGate::Full), true, None),
            "current and awaited detail remain admissible");
        assert!(support::full_res_publish_held(p.publish_gate(support::PublishGate::Full), false, None),
            "speculative detail must not refill the cache");
        for _ in 0..3 { assert!(!p.sample(Calm)); }
        assert!(!p.sample(Dead)); // break the calm streak
        for _ in 0..3 { assert!(!p.sample(Calm)); }
        assert!(p.active);
        assert!(p.sample(Calm));
        assert!(!p.active);
        assert_eq!(p.publish_gate(support::PublishGate::Full), support::PublishGate::Full);
    }

    /// Removing the production shed call/body leaves speculative textures/queued work live.
    #[test]
    fn pressure_shed_keeps_requested_photos_and_releases_speculative_caches_and_jobs() {
        let fast = FastTier::new();
        let detail = DetailTier::new();
        let image = slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(1, 1));
        for id in 0..8 {
            fast.cache.borrow_mut().insert(id, tick::FastEntry { img: image.clone(), w: 1, h: 1,
                blur: Arc::from([0u8; 4]), bw: 1, bh: 1, dim: 1, turns: 0 });
            detail.cache.borrow_mut().insert(id, (image.clone(), 1, 1));
            detail.order.borrow_mut().push_back(id);
            let mut pump = fast.pump.0.lock().unwrap();
            pump.queue.push_back(id);
            pump.costly_q.insert(id);
        }
        fast.pump.0.lock().unwrap().inflight.insert(7);
        fast.pump.0.lock().unwrap().costly_inflight.insert(7);
        // Current=1, compare=2/3, hovered=4, awaited=5. All protect the existing explicit set.
        let explicit = support::ExplicitSet::new(1, true, 2, 3, 8, Some(4), 5);
        let generation = Cell::new(10);
        shed(&explicit, &fast, &detail, &generation, &RefCell::new(VecDeque::new()),
            &RefCell::new(None), &RefCell::new(HashSet::new()), &RefCell::new(Vec::new()));
        for id in 0..8 {
            assert_eq!(fast.cache.borrow().contains_key(&id), explicit.holds(id));
            assert_eq!(detail.cache.borrow().contains_key(&id), explicit.holds(id));
            assert_eq!(detail.order.borrow().contains(&id), explicit.holds(id));
            let pump = fast.pump.0.lock().unwrap();
            assert_eq!(pump.queue.contains(&id), explicit.holds(id));
            assert_eq!(pump.costly_q.contains(&id), explicit.holds(id));
        }
        assert_eq!(generation.get(), 11, "thumbnail bindings must release evicted texture references");
        assert!(fast.pump.0.lock().unwrap().inflight.contains(&7), "running job still owns its marker");
        assert!(fast.pump.0.lock().unwrap().costly_inflight.contains(&7));
    }
}
