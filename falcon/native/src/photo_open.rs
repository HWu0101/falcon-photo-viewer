//! A photo-first open is a real, paired shot with the COMPLETE review journal.
//! Only the directory's collection is incomplete. Promotion carries live edits
//! and rekeys the displayed caches/undo records by exact file identity.
use crate::*;

/// An unfinished edge is presentation state, never an extra shot or decode request.
pub(crate) fn pending_direction(index: i64, len: usize, edges: (bool, bool)) -> i32 {
    if len == 0 { 0 } else if index < 0 && edges.0 { -1 }
    else if index >= len as i64 && edges.1 { 1 } else { 0 }
}

pub(crate) fn pending_edges(app: &MainWindow) -> (bool, bool) {
    (app.get_opening_photo() && app.get_opening_before(),
     app.get_opening_photo() && app.get_opening_after())
}

/// Retain one request only. A real-photo navigation cancels an earlier edge wait.
pub(crate) fn hold_boundary(app: &MainWindow, requested: i64, len: usize) -> bool {
    let direction = pending_direction(requested, len, pending_edges(app));
    app.set_opening_wait_dir(direction);
    direction != 0
}

/// Grid keys keep their original photo and row width, not the temporary batch edge.
pub(crate) fn grid_row_navigation(app: &MainWindow, current: usize, len: usize, direction: i32) -> Option<usize> {
    if len == 0 || direction == 0 { return None; }
    let step = direction.signum() * app.get_grid_cols().max(1);
    let waiting = app.get_opening_wait_dir();
    if app.get_opening_photo() && waiting != 0 && waiting.signum() == step.signum() {
        return None; // repeats/resizing do not accumulate or reinterpret the queued request
    }
    let requested = current as i64 + i64::from(step);
    if pending_direction(requested, len, pending_edges(app)) != 0 {
        app.set_opening_wait_dir(step);
        None
    } else {
        app.set_opening_wait_dir(0);
        Some(requested.clamp(0, len.saturating_sub(1) as i64) as usize)
    }
}

/// Called after identity matching, so a batch index can never select a different file.
pub(crate) fn promoted_navigation(index: usize, len: usize, direction: i32,
    partial: bool, edges: (bool, bool)) -> (usize, i32) {
    let next = index as i64 + i64::from(direction);
    if partial && pending_direction(next, len, edges) != 0 { (index, direction) }
    else { (next.clamp(0, len.saturating_sub(1) as i64) as usize, 0) }
}

pub(crate) fn pending_tile(index: i64, len: usize, edges: (bool, bool)) -> FilmItem {
    let direction = pending_direction(index, len, edges);
    if direction == 0 { return FilmItem::default(); }
    FilmItem { pending: true, idx: if direction < 0 { -1 } else { len as i32 },
        ..Default::default() }
}

/// Do not restart the initial preview while it is still becoming visible. Once
/// that photo is visible, collection promotion waits for neither detail nor neighbors.
pub(crate) fn defer_promotion(partial_id: u64, request_id: u64, ready: bool, failed: bool, elapsed: Duration) -> bool {
    partial_id != 0 && partial_id == request_id && !ready && !failed && elapsed < Duration::from_secs(10)
}

/// Pause new thumbnails only during the clicked photo's bounded priming window.
/// GPU detail and CPU thumbnails can overlap after the first preview is ready.
pub(crate) fn defer_thumbnails(priming: bool, preview_ready: bool, cpu_detail: bool, age: Option<Duration>) -> bool {
    priming && (!preview_ready || cpu_detail) && age.is_some_and(|age| age < Duration::from_millis(750))
}

/// The longest the nearby stage waits for the UI to take the clicked photo's request.
pub(crate) const REQUEST_HANDOFF_LIMIT: Duration = Duration::from_millis(250);

/// Poll until `taken` holds, the open is `cancelled`, or `limit` passes. This waits for the
/// request's hand-off to the UI thread (normally one tick), never for a decode or a frame.
pub(crate) fn request_taken(taken: impl Fn() -> bool, cancelled: impl Fn() -> bool, limit: Duration) -> bool {
    let until = std::time::Instant::now() + limit;
    loop {
        if cancelled() { return false; }
        if taken() { return true; }
        if std::time::Instant::now() >= until { return false; }
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(crate) fn same_photo(a: &Shot, b: &Shot) -> bool {
    a.name == b.name && a.raw == b.raw && a.jpg == b.jpg
        && a.kind == b.kind && a.has_jpg == b.has_jpg
        && a.cloud_placeholder == b.cloud_placeholder
}

pub(crate) fn reindex_history(history: &mut UndoRedo, old: &[Shot], new: &[Shot]) {
    let index = |i: usize| old.get(i).and_then(|s| new.iter().position(|n| same_photo(s, n)));
    for stack in [&mut history.undo, &mut history.redo] {
        stack.retain_mut(|act| match &mut act.entry {
            UndoEntry::Cull { idx, .. } | UndoEntry::Rot { idx, .. } => {
                if let Some(i) = index(*idx) { *idx = i; true } else { false }
            }
            UndoEntry::Bulk { edits, .. } => {
                let Some(indices) = edits.iter().map(|e| index(e.0)).collect::<Option<Vec<_>>>() else { return false };
                for (e, i) in edits.iter_mut().zip(indices) { e.0 = i; }
                true
            }
            UndoEntry::BulkRot { turns } => {
                let Some(indices) = turns.iter().map(|e| index(e.1)).collect::<Option<Vec<_>>>() else { return false };
                for (e, i) in turns.iter_mut().zip(indices) { e.1 = i; }
                true
            }
        });
    }
}

pub(crate) struct Promotion {
    index: usize,
    fast: Vec<(usize, tick::FastEntry)>,
    detail: Vec<(usize, (slint::Image, u32, u32))>,
    thumbs: Vec<(usize, tick::ThumbEntry)>,
    source_dims: Vec<(usize, (u32, u32))>,
    zoom_tiles: Vec<RoiTile>,
    frost: Vec<(usize, crate::backdrop::Mip)>,
    frame_gamuts: Vec<(usize,Gamut)>,
    file_gamuts: Vec<(usize,Gamut)>,
    base: Vec<(usize, u8)>,
    base_raw: Vec<(usize, u8)>,
    history: UndoRedo,
    zoom: f32,
    pan: (f32, f32),
    wait_dir: i32,
}

impl Promotion {
    pub(crate) fn take(
        app: &MainWindow, done: &mut ScanDone, position: (&[Shot], usize),
        caches: (&FastTier, &DetailTier, &Film, &RoiZoom), rot: &RotState, undo: &RefCell<UndoRedo>,
        metadata: (&crate::meta::PerShotMeta,u64),
    ) -> Option<Self> {
        let (fast, detail, film, roi) = caches;
        let (meta,gen)=metadata;
        if !app.get_opening_photo() || done.id == 0 || done.scan_err { return None; }
        let (old, current) = position;
        refresh_rating_sort(done);
        let shot = old.get(current)?;
        let index = done.shots.iter().position(|s| same_photo(shot, s))?;
        let (start, wait_dir) = promoted_navigation(index, done.shots.len(),
            app.get_opening_wait_dir(), done.partial, done.pending_edges);
        done.start = start;
        let mut history = std::mem::take(&mut *undo.borrow_mut());
        reindex_history(&mut history, old, &done.shots);
        let destinations: HashMap<_, _> = done.shots.iter().enumerate()
            .filter_map(|(i, s)| s.jpg.as_ref().or(s.raw.as_ref()).map(|p| (p, i))).collect();
        let rekey = |i: usize| {
            let s = old.get(i)?;
            if s.jpg.iter().chain(s.raw.iter()).any(|p| done.changed_sources.contains(p)) { return None; }
            let p = s.jpg.as_ref().or(s.raw.as_ref())?;
            let next = *destinations.get(p)?;
            same_photo(s, &done.shots[next]).then_some(next)
        };
        Some(Self {
            index, wait_dir,
            source_dims: roi.src.borrow().iter().filter_map(|(&i,&dims)| rekey(i).map(|n| (n,dims))).collect(),
            zoom_tiles: roi.tiles.borrow().iter().filter_map(|tile| rekey(tile.id).map(|id| {
                let mut tile = tile.clone(); tile.id = id; tile
            })).collect(),
            fast: fast.cache.borrow().iter().filter_map(|(&i,e)| rekey(i).map(|n| (n,e.clone()))).collect(),
            detail: detail.cache.borrow().iter().filter_map(|(&i,e)| rekey(i).map(|n| (n,e.clone()))).collect(),
            thumbs: film.thumbs.borrow().iter().filter_map(|(&i,e)| rekey(i).map(|n| (n,e.clone()))).collect(),
            frost: film.frost.lock().unwrap_or_else(|e|e.into_inner()).iter().filter_map(|(&i,e)|rekey(i).map(|n|(n,e.clone()))).collect(),
            frame_gamuts: meta.shot_gamut.borrow().iter().filter_map(|(&i,&g)|rekey(i).map(|n|(n,g))).collect(),
            file_gamuts: meta.file_gamuts.snapshot(gen).into_iter().filter_map(|(i,g)|rekey(i).map(|n|(n,g))).collect(),
            base: rot.base.borrow().iter().filter_map(|(&i,&b)| rekey(i).map(|n| (n,b))).collect(),
            base_raw: rot.base_raw.borrow().iter().filter_map(|(&i,&b)| rekey(i).map(|n| (n,b))).collect(),
            history, zoom: app.get_zoom(), pan: (app.get_pan_x(), app.get_pan_y()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore(self, app: &MainWindow, fast: &FastTier, detail: &DetailTier, film: &Film,
        rot: &RotState, undo: &RefCell<UndoRedo>, roi: &RoiZoom, metadata: (&crate::meta::PerShotMeta,u64)) {
        let (meta,gen)=metadata;
        let kept = (self.fast.len(), self.detail.len(), self.thumbs.len());
        roi.src.borrow_mut().extend(self.source_dims);
        roi.tiles.borrow_mut().extend(self.zoom_tiles);
        fast.cache.borrow_mut().extend(self.fast);
        let mut order: Vec<_> = self.detail.iter().map(|(i,_)| *i).collect();
        order.sort_by_key(|i| std::cmp::Reverse(i.abs_diff(self.index)));
        detail.order.borrow_mut().extend(order);
        detail.cache.borrow_mut().extend(self.detail);
        film.thumbs.borrow_mut().extend(self.thumbs);
        film.frost.lock().unwrap_or_else(|e|e.into_inner()).extend(self.frost);
        meta.shot_gamut.borrow_mut().extend(self.frame_gamuts);
        for (id,gamut) in self.file_gamuts {meta.file_gamuts.note(gen,id,gamut);}
        meta.cs_built.set(None);
        rot.base.borrow_mut().extend(self.base);
        rot.base_raw.borrow_mut().extend(self.base_raw);
        log_event(&format!("scan: promotion retained fast={} detail={} thumbnails={} by file identity", kept.0,kept.1,kept.2));
        *undo.borrow_mut() = self.history;
        app.set_opening_wait_dir(self.wait_dir);
        app.set_zoom(self.zoom);
        app.set_pan_x(self.pan.0);
        app.set_pan_y(self.pan.1);
    }
}

pub(crate) fn refresh_rating_sort(done: &mut ScanDone) {
    if done.sort.0 != SortMethod::Rating || done.source_order.is_empty() { return; }
    let map = selection_map(&done.shots, &done.ratings, &done.marks);
    let rank: HashMap<_, _> = done.source_order.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    done.shots.sort_by_key(|s| rank.get(s.name.as_str()).copied().unwrap_or(usize::MAX));
    sort_scanned_shots(&mut done.shots, done.sort.0, done.sort.1, &map, &BTreeMap::new());
    (done.ratings, done.marks) = apply_selection(&map, &done.shots);
}

/// Rank a metadata-only catalogue in the approved folder order. Date taken and
/// Kind need other photos' bytes; Rating can move under an early edit. Those
/// sorts deliberately wait for the complete scan instead of guessing neighbors.
#[cfg(test)]
pub(crate) fn nearby_paths(
    candidates: Vec<falcon_decode::SinglePhotoCandidate>, target: &Path,
    method: SortMethod, desc: bool,
) -> Option<Vec<PathBuf>> {
    nearby_paths_count(candidates, target, method, desc, 21)
}

pub(crate) fn visible_batch_len(app: &MainWindow) -> usize {
    let strip = (app.get_strip_w() / 128.0).ceil().max(0.0) as usize + 5;
    let grid = if app.get_grid_open() {
        let height = app.get_grid_vp_h().max(app.window().size().height as f32 / app.window().scale_factor());
        let rows = (height / tick::GRID_ROW_PITCH).ceil().max(0.0) as usize + 6;
        rows * tick::grid_cols_for_width(app.get_grid_dock_w()).max(1)
    } else { 0 };
    (strip.max(grid).max(21) | 1).min(129)
}

#[cfg(test)]
pub(crate) fn nearby_paths_count(
    candidates: Vec<falcon_decode::SinglePhotoCandidate>, target: &Path,
    method: SortMethod, desc: bool, requested: usize,
) -> Option<Vec<PathBuf>> {
    nearby_window(candidates, target, method, desc, requested).map(|window| window.paths)
}

pub(crate) struct NearbyWindow {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) pending_edges: (bool, bool),
}

pub(crate) fn nearby_window(
    mut candidates: Vec<falcon_decode::SinglePhotoCandidate>, target: &Path,
    method: SortMethod, desc: bool, requested: usize,
) -> Option<NearbyWindow> {
    if !supports_nearby_sort(method) { return None; }
    candidates.sort_by(|a, b| a.name.cmp(&b.name)); // canonical stable ties, exactly as the full scanner
    let key = |c: &falcon_decode::SinglePhotoCandidate| match method {
        SortMethod::Modified => c.metadata.as_ref().and_then(|m| m.modified().ok()).map(systemtime_epoch).unwrap_or(0),
        SortMethod::Created => c.metadata.as_ref().and_then(|m| m.created().or_else(|_| m.modified()).ok()).map(systemtime_epoch).unwrap_or(0),
        SortMethod::Size => c.metadata.as_ref().map_or(0, |m| m.len() as i64),
        _ => 0,
    };
    candidates.sort_by(|a, b| {
        let order = if method == SortMethod::Name { natural_name_cmp(&a.name, &b.name) } else { key(a).cmp(&key(b)) };
        if desc { order.reverse() } else { order }
    });
    let current = candidates.iter().position(|c| c.path == target)?;
    let count = candidates.len().min(requested.max(1));
    let start = current.saturating_sub(count / 2).min(candidates.len() - count);
    let pending_edges = (start > 0, start + count < candidates.len());
    Some(NearbyWindow { paths: candidates.into_iter().skip(start).take(count).map(|c| c.path).collect(), pending_edges })
}

pub(crate) fn supports_nearby_sort(method: SortMethod) -> bool {
    matches!(method, SortMethod::Name | SortMethod::Modified | SortMethod::Created | SortMethod::Size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_boundary_grid_row_distance_survives_promotion() {
        // Four columns: retain the original column, not the batch's last photo.
        assert_eq!(promoted_navigation(38, 100, 4, false, (false, false)), (42, 0));
        assert_eq!(promoted_navigation(38, 100, -4, false, (false, false)), (34, 0));
        assert_eq!(promoted_navigation(18, 21, 4, true, (true, true)), (18, 4));
        assert_eq!(promoted_navigation(2, 21, -4, true, (true, true)), (2, -4));
        assert_eq!(promoted_navigation(18, 23, 4, true, (true, true)), (22, 0));
        // When discovery proves a real end, use normal grid-arrow clamping.
        assert_eq!(promoted_navigation(18, 21, 4, false, (false, false)), (20, 0));
        assert_eq!(promoted_navigation(2, 21, -4, true, (false, true)), (0, 0));
        assert_eq!(promoted_navigation(38, 100, 1, false, (false, false)), (39, 0));
        assert_eq!(promoted_navigation(38, 100, 0, false, (false, false)), (38, 0));
    }

    #[test]
    fn batch_boundary_wait_keeps_one_step_and_true_ends_stop() {
        assert_eq!(pending_direction(21, 21, (true, true)), 1);
        assert_eq!(pending_direction(-1, 21, (true, true)), -1);
        assert_eq!(pending_direction(20, 21, (true, true)), 0);
        assert_eq!(pending_direction(-1, 21, (false, true)), 0);
        assert_eq!(pending_direction(21, 21, (true, false)), 0);
        assert_eq!(pending_direction(0, 0, (true, true)), 0);
        assert_eq!(promoted_navigation(40, 100, 1, false, (false, false)), (41, 0));
        assert_eq!(promoted_navigation(40, 100, -1, false, (false, false)), (39, 0));
        assert_eq!(promoted_navigation(20, 21, 1, true, (true, true)), (20, 1));
        assert_eq!(promoted_navigation(20, 21, 1, false, (false, false)), (20, 0));
        assert_eq!(promoted_navigation(0, 21, -1, true, (false, true)), (0, 0));
        assert_eq!(promoted_navigation(40, 100, 0, false, (false, false)), (40, 0));
        let pending = pending_tile(21, 21, (false, true));
        assert!(pending.pending && !pending.valid && !pending.selected);
        assert!(pending.name.is_empty() && pending.number.is_empty() && pending.badge.is_empty());
        assert_eq!(pending.idx, 21);
        assert_eq!(pending_tile(-1, 21, (true, false)).idx, -1);
        assert!(!pending_tile(21, 21, (false, false)).pending);
    }

    #[test]
    fn batch_boundary_flags_follow_sorted_window_not_temporary_count() {
        let candidates = || (1..=50).map(|i| falcon_decode::SinglePhotoCandidate {
            name: format!("P{i}"), path: PathBuf::from(format!("P{i}.png")), metadata: None,
        }).collect();
        for (name, desc, edges) in [("P1.png", false, (false, true)),
            ("P50.png", false, (true, false)), ("P25.png", false, (true, true)),
            ("P1.png", true, (true, false)), ("P50.png", true, (false, true))] {
            let window = nearby_window(candidates(), Path::new(name), SortMethod::Name, desc, 21).unwrap();
            assert_eq!(window.paths.len(), 21);
            assert_eq!(window.pending_edges, edges, "{name} descending={desc}");
        }
        assert_eq!(nearby_window(candidates(), Path::new("P25.png"), SortMethod::Name, false, 129)
            .unwrap().pending_edges, (false, false));
    }
    fn shot(name: &str) -> Shot {
        Shot { id: 0, name: name.into(), raw: None, jpg: Some(PathBuf::from(format!("{name}.png"))),
            has_raw: false, has_jpg: true, kind: SrcKind::Png, cloud_placeholder: false, sniffed: None }
    }

    #[test]
    fn the_nearby_stage_waits_for_the_request_handoff_not_for_pixels() {
        let limit = Duration::from_millis(40);
        assert!(request_taken(|| true, || false, limit), "a taken request lets neighbours follow at once");
        let started = std::time::Instant::now();
        assert!(!request_taken(|| false, || false, limit), "an untaken request releases discovery");
        let waited = started.elapsed();
        assert!(waited >= limit && waited < Duration::from_secs(2), "bounded wait, got {waited:?}");
        assert!(!request_taken(|| true, || true, limit), "a superseded open never publishes neighbours");
        let polls = std::cell::Cell::new(0);
        assert!(request_taken(|| { polls.set(polls.get() + 1); polls.get() > 3 }, || false, Duration::from_secs(5)),
            "a request taken on a later tick still lets the neighbours follow");
    }

    #[test]
    fn thumbnail_priority_is_bounded_and_preserves_gpu_overlap() {
        let age = Some(Duration::from_millis(100));
        assert!(defer_thumbnails(true, false, false, age));
        assert!(defer_thumbnails(true, true, true, age));
        assert!(!defer_thumbnails(true, true, false, age));
        assert!(!defer_thumbnails(false, false, true, age));
        assert!(!defer_thumbnails(true, false, true, Some(Duration::from_millis(750))));
        assert!(!defer_thumbnails(true, false, true, None));
    }

    #[test]
    fn early_edits_and_unseen_review_records_survive_rating_sort_promotion() {
        let old = vec![shot("B")];
        let mut done = ScanDone { changed_sources: HashSet::new(), partial: false, pending_edges: (false, false), source_order: vec!["A".into(), "B".into(), "C".into()],
            id: 1, dir: PathBuf::from("folder"), shots: vec![shot("C"), shot("A"), shot("B")],
            ratings: vec![5, 4, 0], marks: vec![0; 3], start: 2, land: None,
            extra: BTreeMap::new(), rotations: BTreeMap::new(), attrs: FolderAttrs::default(),
            had_file: true, manifest: None, sort: (SortMethod::Rating, true), scan_err: false, scan_denied: false, unlisted_target: None };
        let extra = BTreeMap::from([
            ("A".into(), Sel { rating: 4, ..Sel::default() }),
            ("C".into(), Sel { rating: 5, ..Sel::default() }),
            ("MOVED".into(), Sel { rating: 3, ..Sel::default() }),
        ]);
        support::carry_live_review_on_rescan(&mut done, &old, &[5], &[MARK_FLAG], &extra,
            BTreeMap::from([("B".into(), 1)]));
        refresh_rating_sort(&mut done);
        assert_eq!(done.shots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["B", "C", "A"]);
        assert_eq!(done.ratings, [5, 5, 4]);
        assert_eq!(done.marks, [MARK_FLAG, 0, 0]);
        assert_eq!(done.extra["MOVED"].rating, 3);
        assert_eq!(done.rotations["B"], 1);
        assert_eq!(done.shots.iter().map(|s| s.id).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn promotion_rekeys_undo_and_redo_and_drops_missing_batches_whole() {
        let old = vec![shot("B"), shot("MISSING")];
        let new = vec![shot("A"), shot("C"), shot("B")];
        let mut history = UndoRedo {
            undo: vec![Act { seq: 1, entry: UndoEntry::Cull { idx: 0, rating: 2, mark: 0 } },
                Act { seq: 2, entry: UndoEntry::Bulk { edits: vec![(0, 1, 0), (1, 2, 0)], verb: "Rating" } }],
            redo: vec![Act { seq: 3, entry: UndoEntry::Rot { name: "B".into(), idx: 0, delta: 1 } }],
        };
        reindex_history(&mut history, &old, &new);
        assert_eq!(history.undo.len(), 1);
        assert!(matches!(history.undo[0].entry, UndoEntry::Cull { idx: 2, rating: 2, .. }));
        assert!(matches!(history.redo[0].entry, UndoEntry::Rot { idx: 2, delta: 1, .. }));
        let mut changed_pair = new.clone();
        changed_pair[2].raw = Some(PathBuf::from("B.CR3"));
        reindex_history(&mut history, &new, &changed_pair);
        assert!(history.undo.is_empty() && history.redo.is_empty());
    }

    #[test]
    fn photo_open_setting_defaults_to_view_only_and_round_trips() {
        let mut settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(!settings.photo_open_edits);
        settings.photo_open_edits = true;
        let back: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(back.photo_open_edits);
    }

    #[test]
    fn slow_initial_decode_survives_discovery_but_does_not_block_replacements_or_failures() {
        assert!(defer_promotion(7, 7, false, false, Duration::from_secs(2)));
        assert!(!defer_promotion(7, 7, true, false, Duration::ZERO));
        assert!(!defer_promotion(7, 7, false, true, Duration::ZERO));
        assert!(!defer_promotion(7, 8, false, false, Duration::ZERO));
        assert!(!defer_promotion(0, 7, false, false, Duration::ZERO));
        assert!(!defer_promotion(7, 7, false, false, Duration::from_secs(10)));
    }

    #[test]
    fn nearby_window_keeps_natural_sort_stable_ties_and_folder_edges() {
        let catalogue = || (1..=50).rev().map(|i| falcon_decode::SinglePhotoCandidate {
            name: format!("P{i}"), path: PathBuf::from(format!("P{i}.png")), metadata: None,
        }).collect();
        let names = |paths: Vec<PathBuf>| paths.into_iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let ascending = nearby_paths(catalogue(), Path::new("P25.png"), SortMethod::Name, false).unwrap();
        assert_eq!(names(ascending), (15..=35).map(|i| format!("P{i}.png")).collect::<Vec<_>>());
        let descending = nearby_paths(catalogue(), Path::new("P25.png"), SortMethod::Name, true).unwrap();
        assert_eq!(names(descending), (15..=35).rev().map(|i| format!("P{i}.png")).collect::<Vec<_>>());
        let first = nearby_paths(catalogue(), Path::new("P1.png"), SortMethod::Name, false).unwrap();
        assert_eq!(names(first), (1..=21).map(|i| format!("P{i}.png")).collect::<Vec<_>>());
        for method in [SortMethod::Modified, SortMethod::Created, SortMethod::Size] {
            // All missing metadata tie at zero, retaining canonical byte order in both directions.
            let a = nearby_paths(catalogue(), Path::new("P25.png"), method, false).unwrap();
            let b = nearby_paths(catalogue(), Path::new("P25.png"), method, true).unwrap();
            assert_eq!(a, b);
        }
        for method in [SortMethod::Taken, SortMethod::Kind, SortMethod::Rating] {
            assert!(nearby_paths(catalogue(), Path::new("P25.png"), method, false).is_none());
        }
    }

    #[test]
    fn nearby_metadata_order_matches_the_full_scanner_for_every_supported_sort() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let dir = temp.join(format!("falcon-nearby-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        assert_eq!(dir.parent(), Some(temp.as_path()));
        std::fs::create_dir(&dir).unwrap();
        struct Remove(PathBuf);
        impl Drop for Remove { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _remove = Remove(dir.clone());
        for i in 1..=40 {
            let path = dir.join(format!("P{i}.png"));
            std::fs::write(&path, vec![0; i % 7]).unwrap();
            let time = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000 + (i / 3) as u64);
            std::fs::File::options().write(true).open(path).unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(time)).unwrap();
        }
        let target = dir.join("P21.png");
        for method in [SortMethod::Name, SortMethod::Modified, SortMethod::Created, SortMethod::Size] {
            for desc in [false, true] {
                let mut full = falcon_decode::scan_folder_with_metadata(&dir).unwrap();
                sort_scanned_shots(&mut full.shots, method, desc, &BTreeMap::new(), &full.metadata);
                let pos = full.shots.iter().position(|s| s.jpg.as_ref() == Some(&target)).unwrap();
                let start = pos.saturating_sub(10).min(full.shots.len() - 21);
                let expected: Vec<_> = full.shots[start..start + 21].iter().map(|s| s.jpg.clone().unwrap()).collect();
                let catalogue = falcon_decode::single_photo_candidates(&dir, || false).unwrap().unwrap();
                let actual = nearby_paths(catalogue, &target, method, desc).unwrap();
                assert_eq!(actual, expected, "{} descending={desc}", method.as_i32());
            }
        }
    }
}
