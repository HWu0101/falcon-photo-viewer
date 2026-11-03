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

/// Keeping prepared work (2 November): where each photo of `old` sits in `new`, `None` if it left.
/// Exact `same_photo`, never a name alone, so a half-deleted RAW+JPEG pair does not match. One map
/// by primary path, because a Delete asks this for every photo on the interface thread.
pub(crate) fn identity_map(old: &[Shot], new: &[Shot]) -> Vec<Option<usize>> {
    let destinations: HashMap<_, _> = new.iter().enumerate()
        .filter_map(|(i, s)| s.jpg.as_ref().or(s.raw.as_ref()).map(|p| (p, i))).collect();
    old.iter().map(|s| {
        let next = *destinations.get(s.jpg.as_ref().or(s.raw.as_ref())?)?;
        same_photo(s, &new[next]).then_some(next)
    }).collect()
}

pub(crate) fn reindex_history(history: &mut UndoRedo, old: &[Shot], new: &[Shot]) {
    remap_history(history, &identity_map(old, new));
}

/// Keeping prepared work (2 November): `identity`, kept only for a photo whose every file has the
/// same stamp in the previous full listing and this one. A stamp missing on either side, or both,
/// counts as changed. A frame decoded before or after a save differs from one of the two listings
/// either way, so the worst case is one needless decode, and no frame outlives its file.
pub(crate) fn unchanged_map(old: &[Shot], identity: &[Option<usize>],
    previous: &support::ListingStamps, new: &support::ListingStamps) -> Vec<Option<usize>> {
    let same = |p: &PathBuf| matches!((previous.get(p), new.get(p)), (Some(a), Some(b)) if a == b);
    old.iter().zip(identity).map(|(s, &next)| {
        next.filter(|_| s.jpg.iter().chain(s.raw.iter()).all(same))
    }).collect()
}

/// Keeping prepared work (2 November): one identity vector moves both stacks. A departed single
/// entry drops, and a group drops whole if any member left (never trimmed). `.get`, so an index
/// out of range drops instead of panicking on the interface thread.
pub(crate) fn remap_history(history: &mut UndoRedo, identity: &[Option<usize>]) {
    let index = |i: usize| identity.get(i).copied().flatten();
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

/// Keeping prepared work (2 November): the one list of what counts as prepared work, shared by the
/// photo-first [`Promotion`] and the same-folder [`RescanCarry`], so a store can't join one and miss
/// the other. `rekey` says where a photo's work goes, `None` when it stays behind.
struct PreparedWork {
    source_dims: Vec<(usize, (u32, u32))>,
    fast: Vec<(usize, tick::FastEntry)>,
    detail: Vec<(usize, (slint::Image, u32, u32))>,
    thumbs: Vec<(usize, tick::ThumbEntry)>,
    frost: Vec<(usize, crate::backdrop::Mip)>,
    frame_gamuts: Vec<(usize,Gamut)>,
    file_gamuts: Vec<(usize,Gamut)>,
    base: Vec<(usize, u8)>,
    base_raw: Vec<(usize, u8)>,
}

impl PreparedWork {
    fn take(rekey: &dyn Fn(usize) -> Option<usize>, caches: (&FastTier, &DetailTier, &Film, &RoiZoom),
        rot: &RotState, metadata: (&crate::meta::PerShotMeta,u64)) -> Self {
        let (fast, detail, film, roi) = caches;
        let (meta,gen)=metadata;
        Self {
            source_dims: roi.src.borrow().iter().filter_map(|(&i,&dims)| rekey(i).map(|n| (n,dims))).collect(),
            fast: fast.cache.borrow().iter().filter_map(|(&i,e)| rekey(i).map(|n| (n,e.clone()))).collect(),
            detail: detail.cache.borrow().iter().filter_map(|(&i,e)| rekey(i).map(|n| (n,e.clone()))).collect(),
            thumbs: film.thumbs.borrow().iter().filter_map(|(&i,e)| rekey(i).map(|n| (n,e.clone()))).collect(),
            frost: film.frost.lock().unwrap_or_else(|e|e.into_inner()).iter().filter_map(|(&i,e)|rekey(i).map(|n|(n,e.clone()))).collect(),
            frame_gamuts: meta.shot_gamut.borrow().iter().filter_map(|(&i,&g)|rekey(i).map(|n|(n,g))).collect(),
            file_gamuts: meta.file_gamuts.snapshot(gen).into_iter().filter_map(|(i,g)|rekey(i).map(|n|(n,g))).collect(),
            base: rot.base.borrow().iter().filter_map(|(&i,&b)| rekey(i).map(|n| (n,b))).collect(),
            base_raw: rot.base_raw.borrow().iter().filter_map(|(&i,&b)| rekey(i).map(|n| (n,b))).collect(),
        }
    }

    /// Fast previews, sharp frames, thumbnails: what the log lines count.
    fn kept(&self) -> (usize, usize, usize) {
        (self.fast.len(), self.detail.len(), self.thumbs.len())
    }

    /// `anchor` is the photo landed on: sharp frames are ordered farthest from it first, as the
    /// eviction reads them.
    fn restore(self, anchor: usize, caches: (&FastTier, &DetailTier, &Film, &RoiZoom),
        rot: &RotState, metadata: (&crate::meta::PerShotMeta,u64)) {
        let (fast, detail, film, roi) = caches;
        let (meta,gen)=metadata;
        roi.src.borrow_mut().extend(self.source_dims);
        fast.cache.borrow_mut().extend(self.fast);
        let mut order: Vec<_> = self.detail.iter().map(|(i,_)| *i).collect();
        order.sort_by_key(|i| std::cmp::Reverse(i.abs_diff(anchor)));
        detail.order.borrow_mut().extend(order);
        detail.cache.borrow_mut().extend(self.detail);
        film.thumbs.borrow_mut().extend(self.thumbs);
        film.frost.lock().unwrap_or_else(|e|e.into_inner()).extend(self.frost);
        meta.shot_gamut.borrow_mut().extend(self.frame_gamuts);
        for (id,gamut) in self.file_gamuts {meta.file_gamuts.note(gen,id,gamut);}
        meta.cs_built.set(None);
        rot.base.borrow_mut().extend(self.base);
        rot.base_raw.borrow_mut().extend(self.base_raw);
    }
}

pub(crate) struct Promotion {
    index: usize,
    work: PreparedWork,
    zoom_tiles: Vec<RoiTile>,
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
        let roi = caches.3;
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
        let identity = identity_map(old, &done.shots);
        let rekey = |i: usize| {
            let s = old.get(i)?;
            if s.jpg.iter().chain(s.raw.iter()).any(|p| done.changed_sources.contains(p)) { return None; }
            identity.get(i).copied().flatten()
        };
        Some(Self {
            index, wait_dir,
            work: PreparedWork::take(&rekey, caches, rot, metadata),
            zoom_tiles: roi.tiles.borrow().iter().filter_map(|tile| rekey(tile.id).map(|id| {
                let mut tile = tile.clone(); tile.id = id; tile
            })).collect(),
            history, zoom: app.get_zoom(), pan: (app.get_pan_x(), app.get_pan_y()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore(self, app: &MainWindow, fast: &FastTier, detail: &DetailTier, film: &Film,
        rot: &RotState, undo: &RefCell<UndoRedo>, roi: &RoiZoom, metadata: (&crate::meta::PerShotMeta,u64)) {
        let kept = self.work.kept();
        self.work.restore(self.index, (fast, detail, film, roi), rot, metadata);
        roi.tiles.borrow_mut().extend(self.zoom_tiles);
        log_event(&format!("scan: promotion retained fast={} detail={} thumbnails={} by file identity", kept.0,kept.1,kept.2));
        *undo.borrow_mut() = self.history;
        app.set_opening_wait_dir(self.wait_dir);
        app.set_zoom(self.zoom);
        app.set_pan_x(self.pan.0);
        app.set_pan_y(self.pan.1);
    }
}

/// Keeping prepared work (2 November): which landings keep what is ready. `settle_reopen` sets `id`
/// to 0, so a reopen is asked for separately: it stays the manual "read it all again". `applying`
/// guards one route only, Recover during Apply rotations: Apply holds `sel_busy`, so Delete, Move
/// rejected and Undo move refuse, and sorting is greyed. A RAW written by side file keeps its own
/// stamp while the swap orphans what Apply published, so that rare case keeps today's wipe.
pub(crate) fn rescan_carries(id: u64, reopening: bool, same_folder: bool, applying: bool) -> bool {
    id == 0 && !reopening && same_folder && !applying
}

/// Keeping prepared work (2 November): a Delete, Move rejected, Undo move, Recover or sort change
/// still rescans on the interface thread and lands through every reset a folder change runs. This
/// takes what stays true out before the list swap and puts it back at the end of the same
/// `apply_scan` call, with no tick between, so a refused carry is exactly today's wipe. It is not
/// [`Promotion`], which refuses when the current photo left and sets the landing, zoom and pan: this
/// takes no window and only reads the landing. Pixels and pixel facts move through `unchanged_map`,
/// undo steps through `identity_map`, so a file another program saved reloads alone and keeps its
/// undo steps. Zoom tiles are carried only with a kept view ([`RescanCarry::keep_view`]): otherwise
/// `view.reset_for_swap` makes the same tick's `step_bookkeeping` clear them.
pub(crate) struct RescanCarry {
    work: PreparedWork,
    ram: Vec<(usize, L2Entry)>,
    mipless: Vec<usize>,
    history: UndoRedo,
    start: usize,
    changed: usize,
    spent: Duration,
    identity: Vec<Option<usize>>,
    unchanged: Vec<Option<usize>>,
}

impl RescanCarry {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn take(
        lists: (&[Shot], &[Shot]), stamps: (&support::ListingStamps, &support::ListingStamps), start: usize,
        caches: (&FastTier, &DetailTier, &Film, &RoiZoom), l2: &RefCell<L2Store>, rot: &RotState,
        undo: &RefCell<UndoRedo>, metadata: (&crate::meta::PerShotMeta,u64),
    ) -> Self {
        let at = std::time::Instant::now();
        let (old, new) = lists;
        let identity = identity_map(old, new);
        let unchanged = unchanged_map(old, &identity, stamps.0, stamps.1);
        let mut history = std::mem::take(&mut *undo.borrow_mut());
        remap_history(&mut history, &identity);
        let rekey = |i: usize| unchanged.get(i).copied().flatten();
        let film = caches.2;
        Self {
            work: PreparedWork::take(&rekey, caches, rot, metadata),
            ram: l2.borrow().carried(rekey),
            mipless: film.mipless.borrow().iter().filter_map(|&i| rekey(i)).collect(),
            history, start,
            changed: identity.iter().zip(&unchanged).filter(|(i, u)| i.is_some() && u.is_none()).count(),
            spent: at.elapsed(),
            identity,
            unchanged,
        }
    }

    /// Keeping the view on a sort change (measured in `2026-11-02_keep_view_cost`): `Some` only when
    /// the photo on screen, and both Compare photos, are the same photos after this reread, by exact
    /// `same_photo` identity, never a name alone. A file saved elsewhere keeps the zoom but loses its
    /// tiles (`unchanged`), like its pixels. `None` leaves today's reset.
    pub(crate) fn keep_view(&self, zoom: f32, pan: (f32, f32), current: usize, compare: Option<(usize, usize)>, tiles: &[RoiTile]) -> Option<KeptView> {
        let follow = |i: usize| self.identity.get(i).copied().flatten();
        if follow(current)? != self.start { return None; }
        let halves = match compare { Some((a, b)) => Some((follow(a)?, follow(b)?)), None => None };
        let tiles = tiles.iter().filter_map(|t| {
            let id = self.unchanged.get(t.id).copied().flatten()?;
            let mut t = t.clone();
            t.id = id;
            Some(t)
        }).collect();
        Some(KeptView { zoom, pan, halves, tiles })
    }

    pub(crate) fn restore(self, caches: (&FastTier, &DetailTier, &Film, &RoiZoom), l2: &RefCell<L2Store>,
        rot: &RotState, undo: &RefCell<UndoRedo>, metadata: (&crate::meta::PerShotMeta,u64)) {
        let at = std::time::Instant::now();
        let kept = self.work.kept();
        let (ram, undos, redos) = (self.ram.len(), self.history.undo.len(), self.history.redo.len());
        self.work.restore(self.start, caches, rot, metadata);
        l2.borrow_mut().restore_carried(self.ram);
        caches.2.mipless.borrow_mut().extend(self.mipless);
        *undo.borrow_mut() = self.history;
        log_event(&format!("scan: rescan kept fast={} detail={} thumbnails={} ram={ram} undo={undos} redo={redos} changed={} in {} ms",
            kept.0, kept.1, kept.2, self.changed, (self.spent + at.elapsed()).as_millis()));
    }
}

/// The view a same-folder sort reread keeps, read before the list swap and put back after every reset
/// `apply_scan` runs, with no tick between. What needs no code here: the focused half, the pins and
/// the Compare pan fraction are never reset by a swap, and the Compare halves re-present from the
/// prepared work [`RescanCarry`] keeps.
pub(crate) struct KeptView {
    zoom: f32,
    pan: (f32, f32),
    halves: Option<(usize, usize)>,
    tiles: Vec<RoiTile>,
}

impl KeptView {
    pub(crate) fn restore(self, app: &MainWindow, compare: &Cell<bool>, cmp: (&Cell<usize>, &Cell<usize>), roi: &RoiZoom) {
        app.set_zoom(self.zoom);
        app.set_pan_x(self.pan.0);
        app.set_pan_y(self.pan.1);
        if let Some((a, b)) = self.halves {
            compare.set(true);
            app.set_compare(true);
            cmp.0.set(a);
            cmp.1.set(b);
        }
        *roi.kept_tiles.borrow_mut() = self.tiles;
    }

    #[cfg(test)]
    pub(crate) fn view_for_test(&self) -> (f32, (f32, f32), Option<(usize, usize)>) {
        (self.zoom, self.pan, self.halves)
    }

    #[cfg(test)]
    pub(crate) fn restore_tiles_for_test(self, roi: &RoiZoom) {
        *roi.kept_tiles.borrow_mut() = self.tiles;
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

/// R3 (1 November safety round): an explicit reopen of the folder on screen is scanned off the
/// interface thread (`support::reopen_scan`), so ratings changed during the scan were carried at
/// acceptance (`carry_live_review_on_rescan`) onto an order sorted by older ones. Sort a Rating
/// order again by the carried values (stable over the scan's own order, as the synchronous rescan
/// sorts), land on the requested photo, and accept it as that synchronous rescan (`id` 0: no
/// promotion, no resume offer, no second open-folder registration, the number-badge plan kept).
pub(crate) fn settle_reopen(done: &mut ScanDone, requested: &Option<String>) {
    refresh_rating_sort(done);
    done.start = select_match(&done.shots, requested).unwrap_or(0);
    done.id = 0;
}

/// Rank a metadata-only catalogue in the approved folder order. Date taken needs
/// other photos' bytes, and Kind waits for the complete list; Rating can move under an early edit. Those
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
        let mut done = ScanDone { changed_sources: HashSet::new(), stamps: None, partial: false, pending_edges: (false, false), source_order: vec!["A".into(), "B".into(), "C".into()],
            id: 1, dir: PathBuf::from("folder"), shots: vec![shot("C"), shot("A"), shot("B")],
            ratings: vec![5, 4, 0], marks: vec![0; 3], start: 2, land: None,
            extra: BTreeMap::new(), rotations: BTreeMap::new(), attrs: FolderAttrs::default(),
            had_file: true, from_ledger: false, identity: None, manifest: None, sort: (SortMethod::Rating, true), scan_err: false, scan_denied: false, unlisted_target: None, review_load: crate::support::ReviewLoad::Loaded, taken_pass: None };
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

    fn pair(name: &str) -> Shot {
        Shot { raw: Some(PathBuf::from(format!("{name}.CR3"))), has_raw: true, ..shot(name) }
    }

    /// Keeping prepared work (2 November): the identity every carried store and undo entry moves by.
    ///
    /// FALSIFIER (L28): map by position, map by `name` alone, or remove the `same_photo` check.
    #[test]
    fn identity_follows_the_file_through_a_delete_and_a_resort() {
        let old = vec![shot("A"), shot("B"), shot("C"), shot("D")];
        // A delete in the middle: everything above it shifts down one.
        let deleted = vec![shot("A"), shot("C"), shot("D")];
        assert_eq!(identity_map(&old, &deleted), [Some(0), None, Some(1), Some(2)]);
        // A whole new order.
        let sorted = vec![shot("D"), shot("C"), shot("B"), shot("A")];
        assert_eq!(identity_map(&old, &sorted), [Some(3), Some(2), Some(1), Some(0)]);
        // One half of a RAW+JPEG pair recycled: its finished file is still listed, but it is no
        // longer the same photo, so nothing follows it.
        let pairs = vec![pair("A"), pair("B")];
        let half_gone = vec![shot("A"), pair("B")];
        assert_eq!(identity_map(&pairs, &half_gone), [None, Some(1)]);
        // Another file under the same name is another photo.
        let mut elsewhere = shot("A");
        elsewhere.jpg = Some(PathBuf::from("other/A.png"));
        assert_eq!(identity_map(&[shot("A")], &[elsewhere]), [None]);
    }

    /// Keeping prepared work (2 November): a file another program saved reloads alone, and its undo
    /// steps stay, because they follow `identity`, not this map.
    ///
    /// FALSIFIER (L28): drop the date term or the size term, check only the primary file, or compare
    /// `previous.get(p) == new.get(p)` so missing-in-both reads as unchanged.
    #[test]
    fn a_changed_file_loses_its_pixels_but_keeps_its_undo() {
        use std::time::{Duration, UNIX_EPOCH};
        let (t0, t1) = (UNIX_EPOCH + Duration::from_secs(1_000), UNIX_EPOCH + Duration::from_secs(2_000));
        let names = ["TOUCHED", "RESIZED", "RAWHALF", "NOTINOLD", "NOTINNEW", "NOWHERE", "PLAIN"];
        let old: Vec<Shot> = names.iter().map(|n| pair(n)).collect();
        let identity = identity_map(&old, &old);
        let files = |n: &str| [PathBuf::from(format!("{n}.png")), PathBuf::from(format!("{n}.CR3"))];
        let mut previous = support::ListingStamps::new();
        let mut new = support::ListingStamps::new();
        for n in names {
            for f in files(n) {
                previous.insert(f.clone(), (10, t0));
                new.insert(f, (10, t0));
            }
        }
        new.insert(files("TOUCHED")[0].clone(), (10, t1)); // same size, new date
        new.insert(files("RESIZED")[0].clone(), (11, t0)); // new size, same date
        new.insert(files("RAWHALF")[1].clone(), (10, t1)); // only the RAW half was saved
        previous.remove(&files("NOTINOLD")[0]);
        new.remove(&files("NOTINNEW")[0]);
        previous.remove(&files("NOWHERE")[0]);
        new.remove(&files("NOWHERE")[0]);
        let unchanged = unchanged_map(&old, &identity, &previous, &new);
        assert_eq!(unchanged, [None, None, None, None, None, None, Some(6)]);
        assert!(identity.iter().all(Option::is_some), "the undo steps still follow every one of them");
        // A photo that left stays out whatever its stamps say.
        let gone = [None; 7];
        assert!(unchanged_map(&old, &gone, &previous, &new).iter().all(Option::is_none));
    }

    /// Keeping prepared work (2 November): the table of who keeps what is ready.
    ///
    /// FALSIFIER (L28): remove any one term of the predicate.
    #[test]
    fn only_a_settled_same_folder_rescan_carries() {
        assert!(rescan_carries(0, false, true, false), "a Delete, a move, Recover or a sort change");
        assert!(!rescan_carries(7, false, true, false), "photo-first's final scan has Promotion");
        assert!(!rescan_carries(0, true, true, false), "a reopen reads it all again");
        assert!(!rescan_carries(0, false, false, false), "another folder");
        assert!(!rescan_carries(0, false, true, true), "Recover during Apply rotations");
    }

    /// Keeping prepared work (2 November): Ctrl+Z after a Delete or a sort change acts on the same
    /// photo, for every kind of undo entry on both stacks, in the order the acts were made.
    ///
    /// FALSIFIER (L28): trim a group instead of dropping it, reset `seq`, leave an index unchanged,
    /// or remap only `undo` and not `redo`.
    #[test]
    fn every_undo_kind_points_at_the_same_photo_after_a_delete_and_a_resort() {
        let old = vec![shot("A"), shot("B"), shot("C"), shot("D")];
        let history = || UndoRedo {
            undo: vec![
                Act { seq: 1, entry: UndoEntry::Cull { idx: 2, rating: 3, mark: 0 } },
                Act { seq: 2, entry: UndoEntry::Rot { name: "D".into(), idx: 3, delta: 1 } },
                Act { seq: 3, entry: UndoEntry::Bulk { edits: vec![(0, 1, 0), (2, 2, 0)], verb: "Rating" } },
                Act { seq: 4, entry: UndoEntry::Cull { idx: 1, rating: 5, mark: 0 } },
                Act { seq: 5, entry: UndoEntry::Bulk { edits: vec![(1, 1, 0), (3, 2, 0)], verb: "Rating" } },
            ],
            redo: vec![
                Act { seq: 6, entry: UndoEntry::Cull { idx: 3, rating: 4, mark: 0 } },
                Act { seq: 7, entry: UndoEntry::BulkRot { turns: vec![("A".into(), 0, 1), ("C".into(), 2, 1)] } },
                Act { seq: 8, entry: UndoEntry::BulkRot { turns: vec![("B".into(), 1, 1), ("D".into(), 3, 1)] } },
            ],
        };
        let seqs = |acts: &[Act]| acts.iter().map(|a| a.seq).collect::<Vec<_>>();
        // A real delete of B: its single entry and both groups holding it drop whole, the rest follow.
        let mut after_delete = history();
        remap_history(&mut after_delete, &identity_map(&old, &[shot("A"), shot("C"), shot("D")]));
        assert_eq!(seqs(&after_delete.undo), [1, 2, 3], "the stamps stay, in order");
        assert_eq!(seqs(&after_delete.redo), [6, 7]);
        assert!(matches!(after_delete.undo[0].entry, UndoEntry::Cull { idx: 1, rating: 3, .. }));
        assert!(matches!(&after_delete.undo[1].entry, UndoEntry::Rot { name, idx: 2, delta: 1 } if name == "D"));
        assert!(matches!(&after_delete.undo[2].entry, UndoEntry::Bulk { edits, .. } if edits[..] == [(0, 1, 0), (1, 2, 0)]));
        assert!(matches!(after_delete.redo[0].entry, UndoEntry::Cull { idx: 2, rating: 4, .. }));
        assert!(matches!(&after_delete.redo[1].entry, UndoEntry::BulkRot { turns }
            if turns[0] == ("A".to_string(), 0, 1) && turns[1] == ("C".to_string(), 1, 1)));
        // A re-sort: nothing leaves, and every index follows its file.
        let mut after_sort = history();
        remap_history(&mut after_sort, &identity_map(&old, &[shot("D"), shot("C"), shot("B"), shot("A")]));
        assert_eq!(seqs(&after_sort.undo), [1, 2, 3, 4, 5]);
        assert_eq!(seqs(&after_sort.redo), [6, 7, 8]);
        assert!(matches!(after_sort.undo[0].entry, UndoEntry::Cull { idx: 1, .. }));
        assert!(matches!(&after_sort.undo[1].entry, UndoEntry::Rot { idx: 0, .. }));
        assert!(matches!(&after_sort.undo[2].entry, UndoEntry::Bulk { edits, .. } if edits[..] == [(3, 1, 0), (1, 2, 0)]));
        assert!(matches!(after_sort.redo[0].entry, UndoEntry::Cull { idx: 0, .. }));
        // An index no list holds drops instead of panicking.
        let mut stray = UndoRedo { undo: vec![Act { seq: 9, entry: UndoEntry::Cull { idx: 99, rating: 0, mark: 0 } }], redo: Vec::new() };
        remap_history(&mut stray, &identity_map(&old, &old));
        assert!(stray.undo.is_empty());
        // The race the stamps exist for: a surviving star, then a newer delete, recovers first; and
        // rate, move, rate takes the stack, then the move, then the older stack entry.
        let dir = "same-folder";
        let top = after_delete.undo.last().map(|a| a.seq);
        assert_eq!(support::live_undo_arm(top, Some((dir, 10)), dir, None), Some(support::UndoArm::Delete(10)));
        assert_eq!(support::live_undo_arm(top, None, dir, None), Some(support::UndoArm::Stack(3)));
        let moved = Some((dir, 1, 11));
        assert_eq!(support::live_undo_arm(Some(12), None, dir, moved), Some(support::UndoArm::Stack(12)));
        assert_eq!(support::live_undo_arm(Some(10), None, dir, moved), Some(support::UndoArm::Move(11)));
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
