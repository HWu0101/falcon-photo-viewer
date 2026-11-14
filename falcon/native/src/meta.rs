//! v1.0.5: file_gamuts is worker-shared; its generation-fenced facts and frame gamuts
//! are carried with retained pixels during photo-first promotion. cs_built stores the
//! optional source VALUE, not only whether some source probe has landed.
//! v0.9.4 (P4a, PLAN §66): the PER-SHOT METADATA subsystem — the transition series' apply_scan
//! folder-swap fold. ONE struct owning the scattered per-shot metadata stores so their folder-swap
//! clearing + reseeding happens BY CONSTRUCTION — a single [`PerShotMeta::on_folder_swap`] — instead of
//! the seven hand-written lines `apply_scan` used to run in three separate clusters (the same fragility
//! the §64 tier rounds fixed for the decode caches; this round extends the pattern to the metadata).
//! `main()` used to clone NINE separate handles into the tick + worker + handler closures and thread
//! them through the `step_*` signatures; they now live here and one `Rc<PerShotMeta>` clones in (with
//! the worker-shared `orient_cache` Arc cloned out at each spawn, exactly as its loose handle was).
//!
//! BEHAVIOR-IDENTICAL: this round MOVES state and changes NO logic. The stores keep the exact
//! interior-mutability discipline they had as loose handles (`orient_cache` stays `Arc<Mutex>` — the
//! decode POOL WORKERS read/fill it off-thread; the other ten are UI-thread-only `RefCell`/`Cell`).
//! The clearing + reseeding semantics below match the pre-fold `apply_scan` lines byte-for-byte (proven
//! by `cargo test` + a boot-metric parity spike on the testkit).
//!
//! MEMBERSHIP (the conservative boundary — only unambiguously per-shot-metadata, swap-cleared/reseeded state):
//!   • `orient_cache` — F2b: the worker-visible per-shot JPG/finished-side EXIF base-orientation cache
//!                      ([`OrientCache`], `gen`-keyed). RESEEDED (resized) on swap under the new
//!                      generation + shot count. Worker-shared → `Arc<Mutex>`.
//!   • `shot_gamut`   — V2: the per-shot source-gamut memo (idx → [`Gamut`]). CLEARED on swap.
//!   • `wb_cache` / `wb_requested` — the per-shot white-balance (colour-temperature) cache + its
//!                      in-flight request latch. CLEARED on swap.
//!   • `exif_cache` / `exif_requested` — the parsed EXIF-rows cache (idx → key/value pairs) + its
//!                      in-flight request latch. CLEARED on swap.
//!   • `exif_retry`   — v0.8.89: the bounded empty-parse retry state (idx → attempts + next-retry
//!                      Instant) for a mid-copy / locked file. CLEARED on swap.
//!   • `exif_built`   — the info-panel build signature (shot idx, wb-known, K) — `None` forces a rebuild.
//!                      CLEARED (to `None`) on swap.
//!   • `cs_built`     — v0.8.93: the RAW/JPG colour-chip build signature (shot idx, output gamut,
//!                    …+ v0.8.119 (design-sweep Y68): a fifth term, whether the SETTLED frame is a
//!                    RAW develop — that frame is sRGB whatever the JPG sibling's ICC says, so the
//!                    chip must re-build on the fast→detail flip instead of memoizing the proxy's
//!                    answer,
//!                      source-gamut probe landed). Split out of `exif_built` when the EXIF pipeline
//!                      moved behind the panel's mount gate — the chip is not info-panel state.
//!                      v0.8.95: the third term separates the honest blank (probe out) from the real
//!                      reading (probe landed). v0.8.96: a fourth, `unreadable` — a permanently
//!                      failed / unsupported shot, whose blank says "unavailable" rather than
//!                      promising a read that can never land. CLEARED (to `None`) on swap.
//!   • `dims_requested` — the source-dimension-probe in-flight request latch. CLEARED on swap.
//!   • `cloud_tagged` — v0.8.24 (D3): the LIVE set of shot indices believed to be cloud (OneDrive)
//!                      placeholders. RESEEDED from the new folder's scan tags on swap (the retry sweep
//!                      shrinks it as files hydrate). UI-thread-only.
//!
//! DELIBERATELY OUT: the decode CHANNELS (`exif_req/res`, `dims_req/res` — plumbing, parity with the tier
//! rounds' channels) and the develop-config atomics (`output_gamut`, `raw_mode`, `det_epoch` — cross-
//! consumer config signals, not per-shot swap-cleared caches). Those stay loose handles.
use crate::*;

/// Shot, output gamut, resolved source value, unreadable state and RAW-detail state.
pub(crate) type ColorChipSignature = (usize, u32, Option<Gamut>, bool, bool);

/// File-space facts from the detail worker, independent of optional fast uploads.
/// Newer generations win; an abandoned worker can never erase a new folder's facts.
#[derive(Default)]
pub(crate) struct FileGamuts(Mutex<(u64, HashMap<usize, Gamut>)>);
impl FileGamuts {
    pub(crate) fn reset(&self, gen: u64) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if gen > s.0 {
            *s = (gen, HashMap::new());
        }
    }
    pub(crate) fn note(&self, gen: u64, id: usize, gamut: Gamut) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if gen < s.0 {
            return;
        }
        if gen > s.0 {
            *s = (gen, HashMap::new());
        }
        s.1.insert(id, gamut);
    }
    pub(crate) fn get(&self, gen: u64, id: usize) -> Option<Gamut> {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 == gen {
            s.1.get(&id).copied()
        } else {
            None
        }
    }
    pub(crate) fn snapshot(&self, gen: u64) -> HashMap<usize, Gamut> {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 == gen {
            s.1.clone()
        } else {
            HashMap::new()
        }
    }
}

/// The per-shot-metadata subsystem's owned state (see the module doc for the membership boundary). Wrapped
/// in an `Rc<PerShotMeta>` on the UI thread; only the worker-shared `orient_cache` is `Arc`-cloned into the
/// decode-pool workers at spawn, exactly as the loose handle was (the other ten are UI-thread-only and the
/// workers never touch them).
/// (v0.9.25 / v0.8.93-audit C15: "eight" → "ten" at both sites — the count had drifted twice, at
/// v0.8.89's `exif_retry` and again at v0.8.93's `cs_built`, both of which joined the struct and the
/// exhaustive swap destructure without joining the module doc's enumeration.)
pub(crate) struct PerShotMeta {
    pub(crate) file_gamuts: Arc<FileGamuts>,
    pub(crate) orient_cache: Arc<Mutex<OrientCache>>,
    pub(crate) shot_gamut: RefCell<HashMap<usize, Gamut>>,
    pub(crate) wb_cache: RefCell<HashMap<usize, Option<u32>>>,
    pub(crate) wb_requested: RefCell<HashSet<usize>>,
    pub(crate) exif_cache: RefCell<HashMap<usize, Vec<(String, String)>>>,
    pub(crate) exif_requested: RefCell<HashSet<usize>>,
    // v0.8.89: bounded EXIF empty-parse retry state (idx → attempts + next-retry Instant). An empty parse
    // (mid-copy / 0-byte / locked file) is no longer cached permanently; drain_exif records the attempt +
    // schedules a spaced re-request here, step_settle_exif honors the backoff, and the rotate / auto-orient
    // / manual-retry handlers clear a shot's entry so it re-parses fresh. Per-folder — CLEARED on swap.
    pub(crate) exif_retry: RefCell<HashMap<usize, crate::tick::ExifRetry>>,
    pub(crate) exif_built: Cell<Option<(usize, bool, u32)>>,
    /// v0.8.93 (ruling 5): the RAW/JPG panel's colour-chip build signature — (shot, output gamut,
    /// source-gamut probe landed). Split out of `exif_built` when the EXIF pipeline moved behind the
    /// panel's mount gate: the chip is NOT info-panel state (its panel stays visible while the info
    /// panel is minimized/off) and never depended on the EXIF rows, so it needs its own memo.
    /// v0.8.95 (v0.8.94-audit V3): the third term is load-bearing — it separates the honest BLANK
    /// written while the probe is out from the real reading written when it lands. Per-folder —
    /// CLEARED on swap (indices are re-keyed), so the new folder's landing shot blanks and rebuilds.
    /// v0.8.96: a FOURTH term, `unreadable` — the shot is in the stage's failure/unsupported state
    /// with no probe landed, so the blank's note says "unavailable" instead of "reading…". It is a
    /// signature term (not just a branch) so the pending → failed transition re-fires the note once.
    pub(crate) cs_built: Cell<Option<crate::meta::ColorChipSignature>>,
    pub(crate) dims_requested: RefCell<HashSet<usize>>,
    pub(crate) cloud_tagged: RefCell<HashSet<usize>>,
}

impl PerShotMeta {
    /// Construct with the initial folder's shot count `len0` — `orient_cache` is seeded (`reset(0, len0)`)
    /// exactly as the loose `orient_cache` handle was at boot. `cloud_tagged` starts EMPTY; `main()` seeds
    /// it from the initial scan's placeholder tags right after (mirroring `on_folder_swap`'s reseed).
    pub(crate) fn new(len0: usize) -> PerShotMeta {
        let mut oc = OrientCache::new();
        oc.reset(0, len0);
        PerShotMeta {
            file_gamuts: Arc::new(FileGamuts::default()),
            orient_cache: Arc::new(Mutex::new(oc)),
            shot_gamut: RefCell::new(HashMap::new()),
            wb_cache: RefCell::new(HashMap::new()),
            wb_requested: RefCell::new(HashSet::new()),
            exif_cache: RefCell::new(HashMap::new()),
            exif_requested: RefCell::new(HashSet::new()),
            exif_retry: RefCell::new(HashMap::new()), // v0.8.89
            exif_built: Cell::new(None),
            cs_built: Cell::new(None), // v0.8.93
            dims_requested: RefCell::new(HashSet::new()),
            cloud_tagged: RefCell::new(HashSet::new()),
        }
    }

    /// Folder-swap clearing + reseeding, BY CONSTRUCTION. Absorbs the SEVEN per-shot-metadata lines
    /// `apply_scan` used to run as three separate clusters — the `orient_cache.reset` (early, right after
    /// the generation bump), the `shot_gamut`/`wb`/`exif`/`dims` clears, and the `cloud_tagged` reseed +
    /// `exif_built.set(None)` — into ONE call at the swap chokepoint. `gen`/`len` = the just-bumped folder
    /// generation + new shot count (for `orient_cache`); `shots` = the new folder's shot list (for the
    /// `cloud_tagged` reseed).
    ///
    /// COMPILE-TIME GUARD: opens with an EXHAUSTIVE destructure of `self` WITHOUT `..`, so any per-shot
    /// store added to `PerShotMeta` later fails to compile until this swap handles it (the by-construction
    /// discipline the §64 tier rounds established, made explicit here).
    ///
    /// SEMANTICS preserved EXACTLY (a skeptic should check these):
    ///   • `orient_cache` — RESEEDED, not cleared: `reset(gen, len)` re-sizes the slot vector to the new
    ///     folder + stamps the new generation, so every stale-folder worker's read/write becomes a no-op
    ///     (the gen check fails) and a new-folder miss fills correctly. Byte-identical to the old
    ///     `orient_cache.lock()…reset(generation.load(Relaxed), new_len)` line.
    ///   • `shot_gamut` / `wb_cache` / `wb_requested` / `exif_cache` / `exif_requested` / `dims_requested`
    ///     — CLEARED: the old-folder indices are re-keyed on a swap, so every per-shot memo/latch is stale.
    ///   • `exif_built` — set to `None`: forces the info panel to rebuild for the new folder's landing shot.
    ///   • `cloud_tagged` — RESEEDED from the new `shots`' `cloud_placeholder` tags (the retry sweep then
    ///     shrinks it as files hydrate). Logged when non-empty (a cloud folder), silent for a local folder
    ///     — byte-identical to the old reseed block.
    ///   • ORDER is irrelevant: the nine stores are independent (no reseed reads a value another row
    ///     clears), and no reader runs between them — `apply_scan` is atomic on the UI thread (no tick step
    ///     runs mid-swap; the intervening non-metadata lines touch only OTHER per-folder stores). The order
    ///     below mirrors the old scattered positions (orient early, then the clears, then cloud + exif_built).
    pub(crate) fn on_folder_swap(&self, gen: u64, len: usize, shots: &[Shot]) {
        let Self {
            file_gamuts,
            orient_cache,
            shot_gamut,
            wb_cache,
            wb_requested,
            exif_cache,
            exif_requested,
            exif_retry,
            exif_built,
            cs_built,
            dims_requested,
            cloud_tagged,
        } = self;
        // F2b: re-seed the per-shot orientation cache for the NEW folder (new gen + shot count).
        orient_cache.lock().unwrap_or_else(|e| e.into_inner()).reset(gen, len);
        file_gamuts.reset(gen);
        shot_gamut.borrow_mut().clear(); // V2: per-shot gamut cache is per-folder
        wb_cache.borrow_mut().clear();
        wb_requested.borrow_mut().clear();
        exif_cache.borrow_mut().clear();
        exif_requested.borrow_mut().clear();
        exif_retry.borrow_mut().clear(); // v0.8.89: the empty-parse retry state is per-folder
        dims_requested.borrow_mut().clear();
        // v0.8.24 (D3): rebuild the live cloud-placeholder set from the NEW folder's scan tags (the retry
        // sweep will shrink it as files hydrate). Logged when non-empty (a cloud folder) for parity with the
        // initial-scan tag line; silent for a local folder to avoid drag-drop noise.
        {
            let mut ct = cloud_tagged.borrow_mut();
            ct.clear();
            for s in shots.iter() {
                if s.cloud_placeholder {
                    ct.insert(s.id);
                }
            }
            if !ct.is_empty() {
                log_event(&format!("scan: {} cloud placeholder(s) tagged", ct.len()));
            }
        }
        exif_built.set(None);
        // v0.8.93: the colour chip's own memo is per-shot-index too — a swap re-keys every index, so a
        // surviving signature could match the NEW folder's landing shot and skip its first chip build.
        cs_built.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_gamuts_reject_late_workers_and_preserve_early_new_generation_facts() {
        let memo=FileGamuts::default();
        memo.note(4,0,Gamut::AdobeRgb);
        memo.note(5,0,Gamut::DisplayP3);
        memo.reset(5); // worker publication can precede the UI's reset for that generation
        memo.note(4,0,Gamut::Srgb);
        assert_eq!(memo.get(5,0),Some(Gamut::DisplayP3));
        assert_eq!(memo.get(4,0),None);
        memo.reset(6);
        assert_eq!(memo.get(6,0),None);
        memo.note(5,0,Gamut::Rec2020);
        assert!(memo.snapshot(6).is_empty());
    }

    #[test]
    fn legacy_raw_preference_is_not_part_of_saved_settings() {
        // Queue entry 6: `photo_open_edits` ("While the folder is loading") is retired the same way. A
        // file that still has it loads, and the save drops it.
        let settings:Settings=serde_json::from_str(r#"{"raw_mode":true,"photo_open_edits":true}"#).unwrap();
        let saved=serde_json::to_value(settings).unwrap();
        assert!(saved.get("raw_mode").is_none());
        assert!(saved.get("photo_open_edits").is_none());
    }

    /// Build a `Shot` with a chosen id + cloud flag (the only two fields the swap reseed reads).
    fn shot(id: usize, cloud: bool) -> Shot {
        Shot {
            id,
            name: format!("s{id}"),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: None,
            kind: SrcKind::Jpeg,
            cloud_placeholder: cloud,
            sniffed: None,
        }
    }

    /// Populate EVERY owned store to a known non-empty / stale state (used by the swap tests).
    fn populate(m: &PerShotMeta) {
        // orient_cache seeded for an OLD folder (gen 3, 5 slots) — the reseed must re-stamp both.
        m.orient_cache.lock().unwrap().reset(3, 5);
        m.shot_gamut.borrow_mut().insert(7, Gamut::Srgb);
        m.wb_cache.borrow_mut().insert(7, Some(5500));
        m.wb_requested.borrow_mut().insert(7);
        m.exif_cache.borrow_mut().insert(7, vec![("ISO".to_string(), "100".to_string())]);
        m.exif_requested.borrow_mut().insert(7);
        m.exif_retry.borrow_mut().insert(7, crate::tick::ExifRetry { attempts: 2, next_at: std::time::Instant::now() });
        m.dims_requested.borrow_mut().insert(7);
        m.exif_built.set(Some((7, true, 5500)));
        m.cs_built.set(Some((7, 2, Some(Gamut::Srgb), false, false))); // v0.8.93/v0.8.96/v0.8.119: the colour-chip signature is per-shot-index too
        m.cloud_tagged.borrow_mut().insert(99); // a STALE old-folder placeholder id
    }

    /// The direct swap test (the §64 tier pattern, metadata edition): populate every owned store to a stale
    /// state, `on_folder_swap` to a NEW folder (gen 4, 3 shots, shot #1 a cloud placeholder), then assert
    /// every CLEARED store is empty, `orient_cache` is RESEEDED to the new gen/len (all slots unknown), and
    /// `cloud_tagged` holds EXACTLY the new folder's placeholder ids (not the stale one). Before this round
    /// the only proof a swap cleared the metadata was a live boot; now it is a unit test that can't rot.
    #[test]
    fn per_shot_meta_swap_clears_and_reseeds() {
        let m = PerShotMeta::new(5);
        populate(&m);
        let shots = [shot(0, false), shot(1, true), shot(2, false)];

        m.on_folder_swap(4, 3, &shots);

        // the six clears
        assert!(m.shot_gamut.borrow().is_empty(), "per-shot gamut cleared on swap");
        assert!(m.wb_cache.borrow().is_empty(), "wb cache cleared on swap");
        assert!(m.wb_requested.borrow().is_empty(), "wb request latch cleared on swap");
        assert!(m.exif_cache.borrow().is_empty(), "exif cache cleared on swap");
        assert!(m.exif_requested.borrow().is_empty(), "exif request latch cleared on swap");
        assert!(m.exif_retry.borrow().is_empty(), "v0.8.89: exif empty-parse retry state cleared on swap");
        assert!(m.dims_requested.borrow().is_empty(), "dims request latch cleared on swap");
        assert_eq!(m.exif_built.get(), None, "exif panel-build signature reset on swap");
        assert_eq!(m.cs_built.get(), None, "v0.8.93: colour-chip signature reset on swap");
        // orient_cache reseeded: new gen, new len, every slot unknown (a hit under the OLD gen is now a miss).
        {
            let oc = m.orient_cache.lock().unwrap();
            assert_eq!(oc.gen_for_test(), 4, "orient_cache stamped with the new generation");
            assert_eq!(oc.len_for_test(), 3, "orient_cache resized to the new folder");
            assert!(oc.all_unknown_for_test(), "every orient slot reset to unknown on swap");
        }
        // cloud_tagged reseeded from the NEW shots: exactly {1} (the stale 99 is gone).
        let ct = m.cloud_tagged.borrow();
        assert_eq!(ct.len(), 1, "cloud set holds exactly the new folder's placeholders");
        assert!(ct.contains(&1), "the new folder's cloud placeholder id is tagged");
        assert!(!ct.contains(&99), "the stale old-folder placeholder id is dropped");
    }

    /// Idempotence: a swap twice-in-a-row (the realistic "swap, then an immediate second swap before
    /// anything repopulates") equals once — clean, no panic, same reseeded state. Matches the tier
    /// idempotence tests.
    #[test]
    fn on_folder_swap_is_idempotent() {
        let m = PerShotMeta::new(5);
        populate(&m);
        let shots = [shot(0, false), shot(1, true)];
        m.on_folder_swap(4, 2, &shots);
        m.on_folder_swap(4, 2, &shots);
        assert!(m.shot_gamut.borrow().is_empty());
        assert!(m.wb_cache.borrow().is_empty());
        assert!(m.wb_requested.borrow().is_empty());
        assert!(m.exif_cache.borrow().is_empty());
        assert!(m.exif_requested.borrow().is_empty());
        assert!(m.exif_retry.borrow().is_empty());
        assert!(m.dims_requested.borrow().is_empty());
        assert_eq!(m.exif_built.get(), None);
        assert_eq!(m.cs_built.get(), None);
        {
            let oc = m.orient_cache.lock().unwrap();
            assert_eq!(oc.gen_for_test(), 4);
            assert_eq!(oc.len_for_test(), 2);
            assert!(oc.all_unknown_for_test());
        }
        assert_eq!(m.cloud_tagged.borrow().len(), 1, "reseed is deterministic across repeat swaps");
    }

    /// 1.0.7 — **A CONSUMER HOLDS THE `Rc<PerShotMeta>`, NEVER A CLONE OF ONE OF ITS CELLS.**
    ///
    /// The UI-thread stores here are plain `RefCell`/`Cell` fields, and both implement `Clone` by
    /// COPYING the value. A closure that clones a field therefore compiles and runs, but reads a
    /// private snapshot taken when the closure was built. That happened at the v1.0.0-rc merge: the
    /// floating EXIF builder cloned the EXIF-rows cache at boot, so View EXIF showed an empty body
    /// for every photograph through 1.0.6 while the docked panel filled in from the live map.
    ///
    /// The field list is read from this struct's own declaration, so a store added later is covered
    /// without editing this row. The `Arc` members are shared handles and are not matched.
    ///
    /// FALSIFIER (L28): restore the field clone in the floating EXIF builder and this row names it.
    #[test]
    fn no_consumer_copies_a_per_shot_metadata_cell() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let own = std::fs::read_to_string(format!("{dir}/meta.rs")).expect("meta.rs");
        let decl_at = own.find("pub(crate) struct PerShotMeta {").expect("the struct declaration");
        let decl = &own[decl_at..decl_at + own[decl_at..].find("\n}").expect("the struct's end")];
        let cells: Vec<&str> = decl
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub(crate) "))
            .filter_map(|l| l.split_once(':'))
            .filter(|(_, ty)| {
                let ty = ty.trim_start();
                ty.starts_with("RefCell<") || ty.starts_with("Cell<")
            })
            .map(|(name, _)| name.trim())
            .collect();
        assert!(cells.contains(&"exif_cache") && cells.len() >= 9, "the parse must find the stores: {cells:?}");

        let mut copies = Vec::new();
        for entry in std::fs::read_dir(dir).expect("src") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source file");
            for (n, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for cell in &cells {
                    if line.contains(&format!(".{cell}.clone()")) {
                        copies.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            copies.is_empty(),
            "clone the Rc<PerShotMeta> and borrow the field inside the closure; cloning the field \
             copies the store:\n{}",
            copies.join("\n")
        );
    }
}
