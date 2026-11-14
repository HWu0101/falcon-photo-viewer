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
/// entry drops, and a group keeps its other members (Delete round, R8). `.get`, so an index out of
/// range drops instead of panicking on the interface thread.
///
/// A photo deleted or moved with a way back reaches here with its steps already out of the stack, in
/// its record (`support::park_steps`), so this is what's left: a delete with no way back, a file another
/// program removed.
pub(crate) fn remap_history(history: &mut UndoRedo, identity: &[Option<usize>]) {
    let index = |i: usize| identity.get(i).copied().flatten();
    for stack in [&mut history.undo, &mut history.redo] {
        stack.retain_mut(|act| match &mut act.entry {
            UndoEntry::Cull { idx, .. } | UndoEntry::Rot { idx, .. } => {
                if let Some(i) = index(*idx) { *idx = i; true } else { false }
            }
            UndoEntry::Bulk { edits, .. } => {
                edits.retain_mut(|e| index(e.0).map(|i| e.0 = i).is_some());
                !edits.is_empty()
            }
            UndoEntry::BulkRot { turns } => {
                turns.retain_mut(|e| index(e.1).map(|i| e.1 = i).is_some());
                !turns.is_empty()
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
/// still lands through every reset a folder change runs, a delete at its press (`support::take_out_scan`)
/// and each of them at a reread, on the interface thread, if the folder changed (`file_queue::Reread`). This
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
    emptied: bool,
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
            emptied: new.is_empty(),
        }
    }

    /// The view rule (measured in `2026-11-02_keep_view_cost`; owner: "Keep", "Both", "Yes to both").
    /// Every carrying reread keeps the view, in one of two forms, followed by exact `same_photo`
    /// identity, never a name alone:
    /// - The landing is the photo on screen, or Compare stays: zoom, pan and tiles. A file saved
    ///   elsewhere keeps the zoom but loses its tiles (`unchanged`), like its pixels.
    /// - The landing is another photo (Delete, a Move of your photo, Recover): zoom and pan only, no
    ///   tiles. `view.reset_for_swap` leaves `last_seen` and `shown` as an arrow's step does, so the
    ///   next tick runs the arrow's own path: `step_bookkeeping` captures a `PanCarry` with the old
    ///   photo's aspect (a swap doesn't reset `img-aspect`) and `step_display` applies it. At fit, that
    ///   path stays at fit.
    ///
    /// In Compare the first form holds whichever photos the halves show now (R5): `compare` says which,
    /// and a tile goes with its own photo.
    ///
    /// `None` leaves today's reset: Compare with fewer than two photos left closes on the survivor, and a
    /// folder with no photos left has nothing to keep a view on.
    pub(crate) fn keep_view(&self, zoom: f32, pan: (f32, f32), current: usize, compare: CompareLanding, tiles: &[RoiTile]) -> Option<KeptView> {
        if self.emptied { return None; }
        let halves = match compare {
            CompareLanding::Off => None,
            CompareLanding::Closes => return None,
            CompareLanding::Stays(a, b) => Some((a, b)),
        };
        if halves.is_none() && self.identity.get(current).copied().flatten() != Some(self.start) {
            return Some(KeptView { zoom, pan, halves, tiles: Vec::new() });
        }
        let tiles = tiles.iter().filter_map(|t| {
            let id = self.unchanged.get(t.id).copied().flatten()?;
            let mut t = t.clone();
            t.id = id;
            Some(t)
        }).collect();
        Some(KeptView { zoom, pan, halves, tiles })
    }

    /// R3: what this landing keeps of the person's scroll (see [`KeptScroll`]). `None` when the photos
    /// that stayed changed order: a re-sort keeps its recentre. `free_strip` is the strip's free-scroll
    /// place, if it has one; it stays only while the photo on screen does.
    pub(crate) fn keep_scroll(&self, current: usize, gen_before: u64, leading_before: bool, free_strip: Option<(f32, f32)>) -> Option<KeptScroll> {
        if !self.identity.iter().flatten().is_sorted() { return None; }
        let same_photo = self.identity.get(current).copied().flatten() == Some(self.start);
        Some(KeptScroll {
            identity: self.identity.clone(),
            anchor: current, landing: self.start, same_photo, gen_before, leading_before,
            strip: free_strip.filter(|_| same_photo),
        })
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

/// What a landing does to Compare, worked out once, at acceptance ([`settle_by_photo`]), for the photo it
/// lands on and the view it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompareLanding {
    Off,
    Closes,
    Stays(usize, usize),
}

/// R5: Compare's answer for a landing from `on_screen` to `new`. See [`support::halves_after`].
pub(crate) fn compare_landing(on_screen: &[Shot], new: &[Shot], compare: Option<support::ComparePair>, seats: &[support::CompareSeat]) -> CompareLanding {
    let Some(pair) = compare else { return CompareLanding::Off };
    match support::halves_after(on_screen, new, (pair.a, pair.b), seats) {
        Some((a, b)) => CompareLanding::Stays(a, b),
        None => CompareLanding::Closes,
    }
}

/// The view a same-folder reread keeps ([`RescanCarry::keep_view`]), read before the list swap and put
/// back after every reset `apply_scan` runs, with no tick between. What needs no code here: the focused
/// half, the pins and the Compare pan fraction are never reset by a swap, and the Compare halves
/// re-present from the prepared work [`RescanCarry`] keeps.
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

/// R3 (Delete round, 3 November): a same-folder landing keeps where the person has scrolled. Every
/// generation change recentred the grid and the Review panel on the photo on screen, and the strip on
/// its tile. That stayed hidden while a menu delete moved the person to the deleted spot. Now that it
/// doesn't, someone tidying tiles far from their photo would be thrown back to it after every delete.
///
/// Read before the swap, like [`KeptView`], and only while the order holds. `apply_scan` puts the strip
/// back and leaves the rest in the one slot the tick takes beside the two steps that own those scrolls,
/// because their rows and their scroll memos live there (`tick::hold_grid_on_landing`,
/// `tick::sel_hold_start`). A delete's take-out and a job's reread can land in the same tick, and
/// [`KeptScroll::after`] makes them one landing for those steps.
pub(crate) struct KeptScroll {
    identity: Vec<Option<usize>>,
    pub(crate) anchor: usize,
    pub(crate) landing: usize,
    pub(crate) same_photo: bool,
    pub(crate) gen_before: u64,
    pub(crate) leading_before: bool,
    pub(crate) strip: Option<(f32, f32)>,
}

impl KeptScroll {
    /// What the slot holds after this landing, `earlier` being what's still there from a landing in the
    /// same tick. The grid and the Review panel only saw the list before that first landing: its anchor
    /// and `gen_before` stay, and a tile goes through both maps. If this landing keeps no scroll (a
    /// re-sort), neither does the tick. With nothing earlier it stands alone, and if a landing before it
    /// refused, the steps find it not settled on its `gen_before`, and recentre.
    pub(crate) fn after(earlier: Option<Self>, landed: Option<Self>) -> Option<Self> {
        let landed = landed?;
        let Some(first) = earlier else { return Some(landed) };
        Some(Self {
            identity: first.identity.iter().map(|at| at.and_then(|i| landed.stayed(i))).collect(),
            anchor: first.anchor,
            landing: landed.landing,
            same_photo: first.same_photo && landed.same_photo,
            gen_before: first.gen_before,
            leading_before: first.leading_before,
            strip: landed.strip,
        })
    }

    /// Where each photo of the list before the landing is now, `None` if it left: the slide reads it
    /// (`tile_slide::SlideFrom::read`).
    pub(crate) fn identity(&self) -> &[Option<usize>] {
        &self.identity
    }

    /// Where a tile of the list before the landing is now, if it stayed.
    pub(crate) fn stayed(&self, tile: usize) -> Option<usize> {
        self.identity.get(tile).copied().flatten()
    }

    /// Where that tile is now, or the first after it that stayed, or the nearest before it: the landing
    /// rule's walk, for a tile. Exact identity is enough here, since one tile off moves nothing far.
    pub(crate) fn nearest_stayed(&self, tile: usize) -> usize {
        let len = self.identity.len();
        (tile.min(len)..len).find_map(|i| self.stayed(i))
            .or_else(|| (0..tile.min(len)).rev().find_map(|i| self.stayed(i)))
            .unwrap_or(0)
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

/// Landing by photo (Delete round, 3 November): a reread's landing, settled at acceptance like a
/// reopen's. `on_screen`, `anchor` and `nav_now` are read at that moment, so whatever the person has
/// done since the press counts (L54). A result carries no position, so a reread arriving on its own
/// finds the person's photo present and leaves them on it. See `support::land_by_photo`.
///
/// The view rule: while Compare stays (R5), a photo that comes back isn't landed on as such.
/// `step_compare_retarget` would put it in a half, or refuse it with a sentence about pins. It goes back in
/// the half it left, if that half still shows what took its place, and otherwise just returns to the list.
/// The person stays on the focused half, wherever that half's photo now is, because `current` mirrors it
/// and anything else would read as a navigation at the top of the next tick.
pub(crate) fn settle_by_photo(done: &mut ScanDone, on_screen: &[Shot], anchor: usize, nav_now: u64, compare: Option<support::ComparePair>) -> CompareLanding {
    let named = done.land_by_photo.take();
    if named.is_some() {
        refresh_rating_sort(done);
    }
    let seats = named.as_ref().map_or(&[][..], |named| &named.seats);
    let landing = compare_landing(on_screen, &done.shots, compare, seats);
    let Some(named) = named else { return landing };
    done.start = match (landing, compare) {
        (CompareLanding::Stays(a, b), Some(pair)) if anchor == pair.focused() => crate::tick::compare_cull_target(pair.focus, a, b),
        (CompareLanding::Stays(..), _) => support::land_by_photo(on_screen, anchor, &done.shots, &[]),
        _ => support::land_by_photo(on_screen, anchor, &done.shots, named.files_if_not_moved(nav_now)),
    };
    landing
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
            ratings: vec![5, 4, 0], marks: vec![0; 3], start: 2, land: None, land_by_photo: None,
            extra: BTreeMap::new(), rotations: BTreeMap::new(), attrs: FolderAttrs::default(),
            had_file: true, from_ledger: false, identity: None, manifest: None, sort: (SortMethod::Rating, true), scan_err: false, scan_denied: false, unlisted_target: None, review_load: crate::support::ReviewLoad::Loaded, taken_pass: None, delete_asks: None };
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
    fn promotion_rekeys_undo_and_redo_and_a_group_keeps_its_other_members() {
        let old = vec![shot("B"), shot("MISSING")];
        let new = vec![shot("A"), shot("C"), shot("B")];
        let mut history = UndoRedo {
            undo: vec![Act { seq: 1, entry: UndoEntry::Cull { idx: 0, rating: 2, mark: 0 } },
                Act { seq: 2, entry: UndoEntry::Bulk { edits: vec![(0, 1, 0), (1, 2, 0)], verb: "Rating" } }],
            redo: vec![Act { seq: 3, entry: UndoEntry::Rot { name: "B".into(), idx: 0, delta: 1 } }],
        };
        reindex_history(&mut history, &old, &new);
        assert_eq!(history.undo.len(), 2);
        assert!(matches!(history.undo[0].entry, UndoEntry::Cull { idx: 2, rating: 2, .. }));
        assert!(matches!(&history.undo[1].entry, UndoEntry::Bulk { edits, .. } if edits[..] == [(2, 1, 0)]), "MISSING left, B stays");
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
    /// FALSIFIER (L28): drop a group whole when one member left, reset `seq`, leave an index unchanged,
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
        // B leaves the list with its steps still on the stack: a move, or a delete with no way back.
        // Its single entry drops, both groups holding it keep their other member, the rest follow.
        let mut after_delete = history();
        remap_history(&mut after_delete, &identity_map(&old, &[shot("A"), shot("C"), shot("D")]));
        assert_eq!(seqs(&after_delete.undo), [1, 2, 3, 5], "the stamps stay, in order");
        assert_eq!(seqs(&after_delete.redo), [6, 7, 8]);
        assert!(matches!(after_delete.undo[0].entry, UndoEntry::Cull { idx: 1, rating: 3, .. }));
        assert!(matches!(&after_delete.undo[1].entry, UndoEntry::Rot { name, idx: 2, delta: 1 } if name == "D"));
        assert!(matches!(&after_delete.undo[2].entry, UndoEntry::Bulk { edits, .. } if edits[..] == [(0, 1, 0), (1, 2, 0)]));
        assert!(matches!(after_delete.redo[0].entry, UndoEntry::Cull { idx: 2, rating: 4, .. }));
        assert!(matches!(&after_delete.redo[1].entry, UndoEntry::BulkRot { turns }
            if turns[0] == ("A".to_string(), 0, 1) && turns[1] == ("C".to_string(), 1, 1)));
        assert!(matches!(&after_delete.undo[3].entry, UndoEntry::Bulk { edits, .. } if edits[..] == [(2, 2, 0)]), "D stays in the group B left");
        assert!(matches!(&after_delete.redo[2].entry, UndoEntry::BulkRot { turns } if turns[..] == [("D".to_string(), 2, 1)]));
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
        assert_eq!(support::live_undo_arm(top, None, dir, None), Some(support::UndoArm::Stack(5)));
        let moved = Some((dir, 1, 11));
        assert_eq!(support::live_undo_arm(Some(12), None, dir, moved), Some(support::UndoArm::Stack(12)));
        assert_eq!(support::live_undo_arm(Some(10), None, dir, moved), Some(support::UndoArm::Move(11)));
    }

    // ── Landing by photo (Delete round, 3 November). Rows name photos, never positions, and repeat
    //    the action (L52, L53). The glue only runs in the app, so its wiring is pinned by source. ──

    fn list(names: &[&str]) -> Vec<Shot> {
        names.iter().map(|n| shot(n)).collect()
    }

    /// The photo someone on `on` lands on.
    fn landed<'a>(on_screen: &[Shot], on: &str, new: &'a [Shot]) -> &'a str {
        let anchor = on_screen.iter().position(|s| s.name == on).expect("the person's photo is on screen");
        &new[support::land_by_photo(on_screen, anchor, new, &[])].name
    }

    /// The reread's result as `reload` hands it to `apply_scan`.
    fn reread(shots: Vec<Shot>, named: Named) -> ScanDone {
        ScanDone { changed_sources: HashSet::new(), stamps: None, partial: false, pending_edges: (false, false), source_order: Vec::new(),
            id: 0, dir: PathBuf::from("folder"), ratings: vec![0; shots.len()], marks: vec![0; shots.len()], shots, start: 0, land: None, land_by_photo: Some(named),
            extra: BTreeMap::new(), rotations: BTreeMap::new(), attrs: FolderAttrs::default(),
            had_file: true, from_ledger: false, identity: None, manifest: None, sort: (SortMethod::Name, false), scan_err: false, scan_denied: false, unlisted_target: None, review_load: crate::support::ReviewLoad::Loaded, taken_pass: None, delete_asks: None }
    }

    /// `Named` for a Recover of one photo.
    fn recovering(photo: &str, nav_at_press: u64) -> Named {
        Named { files: shot(photo).jpg.into_iter().collect(), nav_at_press, seats: Vec::new(), clean: None }
    }

    /// The carry `apply_scan` takes for a landing from `old` to `new`.
    fn carry_over(old: &[Shot], new: &[Shot], start: usize) -> RescanCarry {
        let t0 = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
        let stamps = |list: &[Shot]| -> support::ListingStamps {
            list.iter().map(|s| (s.jpg.clone().unwrap(), (4u64, t0))).collect()
        };
        let (fast, detail, film, roi) = (FastTier::new(), DetailTier::new(), Film::new(), RoiZoom::new());
        let (rot, meta) = (RotState::new(true), crate::meta::PerShotMeta::new(old.len()));
        let (l2, undo) = (RefCell::new(L2Store::new(1 << 30, 16 << 30)), RefCell::new(UndoRedo::default()));
        RescanCarry::take((old, new), (&stamps(old), &stamps(new)), start, (&fast, &detail, &film, &roi), &l2, &rot, &undo, (&meta, 1))
    }

    /// `main.rs`'s product code with the comment lines out, and the part of it from `from` to `to`.
    fn main_code() -> String {
        let src = include_str!("main.rs");
        src.split_once("\nmod tests {").map_or(src, |(p, _)| p).replace("\r\n", "\n")
            .lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
    }
    /// `file_queue.rs`'s product code the same way.
    fn queue_code() -> String {
        let src = include_str!("file_queue.rs");
        src.split_once("\nmod tests {").map_or(src, |(p, _)| p).replace("\r\n", "\n")
            .lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
    }
    fn section<'a>(code: &'a str, from: &str, to: &str) -> &'a str {
        code.split_once(from).unwrap_or_else(|| panic!("`{from}` is in main.rs")).1
            .split_once(to).unwrap_or_else(|| panic!("`{to}` follows `{from}`")).0
    }

    /// Item 11 of the round's decisions, found clicking the Date taken round: a photo saved from another
    /// program comes into the list at the Delete's reread, before the deleted one. The old number landed
    /// on the photo BEFORE the deleted one. Pressed again and again, as culling is. The ends of the list
    /// and an emptied folder are here too, from the retired row on the old number.
    ///
    /// FALSIFIER (L28): in `land_by_photo`, put the number back in place of the walk,
    /// `anchor.min(new.len().saturating_sub(1))`: the first press gives P2.
    #[test]
    fn delete_landing_new_photo_arrives_before_deleted() {
        let on_screen = list(&["P1", "P2", "P3", "P4", "P5", "P6"]);
        let after = list(&["NEW", "P1", "P2", "P4", "P5", "P6"]);
        assert_eq!(landed(&on_screen, "P3", &after), "P4", "the next photo, not the one before");
        let again = list(&["NEW", "NEWER", "P1", "P2", "P5", "P6"]);
        assert_eq!(landed(&after, "P4", &again), "P5", "pressed again, with one more arriving");
        let third = list(&["NEW", "NEWER", "P1", "P2", "P6"]);
        assert_eq!(landed(&again, "P5", &third), "P6", "and again");
        let fourth = list(&["NEW", "NEWER", "P1", "P2"]);
        assert_eq!(landed(&third, "P6", &fourth), "P2", "nothing after it: the photo before it that was on screen");
        // A new photo directly after the deleted one was never on screen, so it isn't "the next one".
        let abcd = list(&["A", "B", "C", "D"]);
        assert_eq!(landed(&abcd, "C", &list(&["A", "B", "N", "D"])), "D");
        // The ends.
        assert_eq!(landed(&abcd, "A", &list(&["B", "C", "D"])), "B", "the first photo");
        assert_eq!(landed(&abcd, "D", &list(&["A", "B", "C"])), "C", "the last photo");
        assert_eq!(support::land_by_photo(&list(&["ONLY"]), 0, &[], &[]), 0, "an emptied folder");
        // A delete that failed, or a menu delete of another photo: the person's photo stays, so do they.
        assert_eq!(landed(&abcd, "B", &abcd), "B", "nothing left");
        assert_eq!(landed(&abcd, "D", &list(&["B", "C", "D"])), "D", "another photo left");
        // No delete names a photo or keeps a position any more, in either arm.
        let queue = queue_code();
        let fold = section(&queue, "FileDone::Delete { seq, dir, msg, fail, can_recover, left, confirmed, for_good, fold } => {", "FileDone::Recover {");
        assert!(fold.contains("to.ask_for_reread(&dir, fail > 0 || !can_recover || for_good);") && !fold.contains("landing_name"), "its report asks for the folder and nothing else");
        let ask = section(&queue, "fn ask_for_reread(&self, dir: &str, at_once: bool) {", "\n    }");
        assert!(ask.contains("self.reread.borrow_mut().want(PathBuf::from(dir), at_once);"), "and the request is the folder");
    }

    /// A person's photo is their place: Move rejected keeps them on it, and when it is one of those
    /// moved, on the next photo that stayed. The old number moved them whenever photos before theirs
    /// left. Twice over, because Move rejected is pressed more than once (L53).
    ///
    /// FALSIFIER (L28): in `land_by_photo`, walk forward only from the anchor itself (no photo after
    /// it): the second row gives P3. Walk no way back: the third row gives P1. Post a position in
    /// `on_move_rejects`: the wiring row.
    #[test]
    fn move_rejected_stays_on_photo_and_goes_to_next_that_stayed() {
        let on_screen = list(&["P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9"]);
        assert_eq!(landed(&on_screen, "P9", &list(&["P1", "P4", "P5", "P6", "P7", "P8", "P9"])), "P9", "photos before yours left");
        let moved = list(&["P1", "P2", "P3", "P6", "P7", "P8", "P9"]);
        assert_eq!(landed(&on_screen, "P4", &moved), "P6", "yours and the next left");
        let last = list(&["P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8"]);
        assert_eq!(landed(&on_screen, "P9", &last), "P8", "yours left, with nothing after it");
        // A second Move rejected, from where the first left them.
        let twice = list(&["P1", "P2", "P3", "P8", "P9"]);
        assert_eq!(landed(&moved, "P6", &twice), "P8", "pressed again: P6 and P7 left");
        assert_eq!(landed(&twice, "P8", &list(&["P1", "P9"])), "P9", "and again: P2, P3 and P8 left");
        let main = main_code();
        let handler = section(&main, "app.on_move_rejects(move || {", "app.on_undo_move(move |seq| {");
        assert!(handler.contains("*rl.lock().unwrap_or_else(|e| e.into_inner()) = Some(PathBuf::from(&dir));"), "the folder and nothing else");
        assert!(!handler.contains("current") && !handler.contains("cur_"), "no position is captured:\n{handler}");
    }

    /// A half-deleted RAW+JPEG pair, or a cloud flag the listing has only now noticed, is still the
    /// photo the person is on. Undo steps and prepared work stay on exact identity.
    ///
    /// FALSIFIER (L28): in `land_by_photo`, drop the "any file in common" try: the first row gives IMG_8.
    #[test]
    fn landing_half_a_pair_left_stays_on_that_photo() {
        let on_screen = vec![pair("IMG_6"), pair("IMG_7"), pair("IMG_8")];
        let raw_gone = vec![pair("IMG_6"), shot("IMG_7"), pair("IMG_8")];
        assert_eq!(landed(&on_screen, "IMG_7", &raw_gone), "IMG_7", "the RAW left, the JPEG stayed");
        let jpeg_gone = Shot { jpg: None, has_jpg: false, ..pair("IMG_7") };
        assert_eq!(landed(&on_screen, "IMG_7", &[pair("IMG_6"), jpeg_gone, pair("IMG_8")]), "IMG_7", "the JPEG left, the RAW stayed");
        let flipped = Shot { cloud_placeholder: true, ..pair("IMG_7") };
        assert_eq!(landed(&on_screen, "IMG_7", &[pair("IMG_6"), flipped, pair("IMG_8")]), "IMG_7", "a cloud flag changed");
        // The same when it is the next photo: Del on IMG_6, and IMG_7 has lost its RAW by then.
        assert_eq!(landed(&on_screen, "IMG_6", &[shot("IMG_7"), pair("IMG_8")]), "IMG_7", "the next photo is still there");
        assert_eq!(identity_map(&on_screen, &raw_gone), [Some(0), None, Some(2)], "edits still follow exact identity");
    }

    /// R1, the owner: "do not move". Undo move leaves the person on their photo, wherever they are by
    /// now: on 41 after moving 40, and browsed on to 60. It names nothing, in every case. Twice over.
    ///
    /// FALSIFIER (L28): in `land_by_photo`, land on the anchor's old number instead of following it:
    /// the first row gives 40. Name the moved photos in `on_undo_move`: the wiring row.
    #[test]
    fn undo_move_stays_where_you_are() {
        let on_screen = list(&["39", "41", "43", "60"]);
        let back = list(&["39", "40", "41", "42", "43", "60"]);
        assert_eq!(landed(&on_screen, "41", &back), "41", "on 41 after moving 40");
        assert_eq!(landed(&on_screen, "60", &back), "60", "browsed on to 60");
        let again = list(&["38", "39", "40", "41", "42", "43", "50", "60"]);
        assert_eq!(landed(&back, "41", &again), "41", "a second Undo move");
        assert_eq!(landed(&back, "60", &again), "60", "a second Undo move, browsed on");
        let main = main_code();
        let handler = section(&main, "app.on_undo_move(move |seq| {", "app.on_empty_rejected(move || {");
        assert!(!handler.contains("current") && !handler.contains("cur_"), "no position is captured:\n{handler}");
        let queue = queue_code();
        let fold = section(&queue, "FileDone::UndoMove { seq, dir, msg, severity, back } => {", "FileDone::Crashed {");
        assert!(fold.contains("to.ask_for_reread(&dir, true);") && !fold.contains("landing_name"), "its report asks for the folder and nothing else");
    }

    /// R2: three photos deleted one at a time, then Ctrl+Z three times: each shows the photo that came
    /// back, so the person ends on the oldest of the three, at any pace. The presses are the handlers'
    /// (`file_queue::press_recover`), the jobs run on the queue, and a landing can fall between any two.
    /// Results fold in press order and the newest wins the one slot. Each press takes the newest delete
    /// left in the history. Falcon's own landings don't count as navigating, and a step the person took
    /// before a press doesn't either, because the press reads the count. Each landing keeps the zoom and
    /// the spot, by the view rule.
    ///
    /// FALSIFIER (L28): `settle_by_photo` ignores the name: it ends on DOG. A landing notes a navigation in
    /// `apply_scan`, or the handler stops reading the count at the press: the wiring rows. `fold_file_done`
    /// names nothing, or the drain drops the name: the landing row. `keep_view` refuses a landing on another
    /// photo: the zoom row. `push_delete` keeps one record: the press row.
    #[test]
    fn every_interleaving_of_three_recovers_ends_on_the_oldest() {
        // Whether a landing falls after CAT's press, after BEE's press, and whether the person steps
        // before ANT's press.
        for (land_after_cat, land_after_bee, step_before_ant) in [
            (false, false, false), (false, true, false), (true, false, false), (true, true, false),
            (true, true, true), (false, false, true),
        ] {
            let case = format!("land after CAT {land_after_cat}, after BEE {land_after_bee}, a step before ANT {step_before_ant}");
            let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
            for photo in ["ANT", "BEE", "CAT"] {
                cull.delete(&[photo]);
            }
            assert_eq!(cull.on, "DOG", "{case}: on DOG");
            let mut held = Some(held_lock(&cull));
            for (photo, lands) in [("CAT", land_after_cat), ("BEE", land_after_bee), ("ANT", true)] {
                if photo == "ANT" && step_before_ant {
                    cull.arrow_to(&cull.shots[0].name.clone()); // an arrow back to the first photo
                }
                let Some(support::UndoArm::Delete(seq)) = cull.arm() else { panic!("{case}: a delete is left") };
                assert_eq!(cull.record_name(seq), photo, "{case}: each press takes the newest delete left");
                assert_eq!(cull.press_z(), "job", "{case}: the press queues its job");
                if lands {
                    drop(held.take());
                    cull.settle();
                    let kept = carry_over(&cull.shots, &cull.shots, cull.place(&cull.on.clone())).keep_view(4.0, (30.0, -20.0), cull.place(&cull.on.clone()), CompareLanding::Off, &[]);
                    assert_eq!(kept.map(|k| k.view_for_test()), Some((4.0, (30.0, -20.0), None)), "{case}: same zoom and spot");
                    assert_eq!(cull.on, photo, "{case}: the photo that came back shows");
                    held = Some(held_lock(&cull));
                }
            }
            drop(held);
            assert!(cull.landing_name.borrow().is_none());
            assert_eq!(cull.on, "ANT", "{case}: the oldest of the three");
        }
        let main = main_code();
        let recover = section(&main, "app.on_recover_delete(move |seq| {", "let shots_fr = shots.clone();");
        let press = recover.find("let nav_at_press = nav_rc.get();").expect("the press reads the count");
        assert!(press < recover.find("file_queue::press_recover(&history_rc, &queue_rc, seq, nav_at_press, Instant::now())").expect("and hands it to the job"), "before the job");
        let queue = queue_code();
        let fold = section(&queue, "FileDone::Recover { seq, dir, files, recovered, failed, msg, nav_at_press, seats, waited } => {", "FileDone::UndoMove {");
        assert!(fold.contains("*named = Some(Named { files, nav_at_press, seats: all, clean });"), "the report names its files, from the press's count");
        let landing = section(&main, "file_queue::Acceptance::Land(mut done, warn) => {", "apply_scan_t(*done);");
        assert!(landing.contains("let named = landing_name_t.borrow_mut().take();") && landing.contains("done.land_by_photo = Some(named.clone().unwrap_or_default());"), "the landing takes the name, at acceptance");
        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        assert!(!apply.contains("note_navigation"), "a landing is Falcon's move, never the person's");
    }

    /// R2 and L54: results carry no position. A reread arriving while the person browses leaves them
    /// where they are, and only a Recover names a photo, while they haven't navigated since the press. The
    /// queue's half is at the end, through real presses and jobs: the person moves while a job runs.
    ///
    /// FALSIFIER (L28): `Named::files_if_not_moved` ignores the count: the second row lands on ANT.
    /// Read the anchor from anything but the live photo at acceptance, leave a route's
    /// `note_navigation` out, or settle before the carry or after the carry's take: the wiring rows.
    /// `press_recover` or the fold reads the count later than the press, or a delete's report names a
    /// photo: the queue's rows.
    #[test]
    fn worker_results_never_move_the_person() {
        let on_screen = list(&["BEE", "CAT", "DOG"]);
        let with_ant = || list(&["ANT", "BEE", "CAT", "DOG"]);
        let landed_after = |named: Named, on: &str, nav_now: u64| {
            let mut done = reread(with_ant(), named);
            let anchor = on_screen.iter().position(|s| s.name == on).unwrap();
            settle_by_photo(&mut done, &on_screen, anchor, nav_now, None);
            assert!(done.land_by_photo.is_none(), "settled once");
            done.shots[done.start].name.clone()
        };
        assert_eq!(landed_after(Named::default(), "CAT", 4), "CAT", "a delete of ANT failed while on BEE, arrow to CAT: ANT returns, still on CAT");
        assert_eq!(landed_after(recovering("ANT", 4), "DOG", 5), "DOG", "Ctrl+Z on CAT, arrow to DOG before it lands: stays on DOG");
        assert_eq!(landed_after(recovering("ANT", 4), "CAT", 4), "ANT", "without the arrow: the photo that came back");
        assert_eq!(landed_after(recovering("GONE", 4), "CAT", 4), "CAT", "a photo that didn't come back: where they are");
        assert_eq!(landed_after(recovering("ANT", 4), "BEE", 5), "BEE", "the other way: an arrow back to BEE before it lands");
        // A route that settled its own landing is left alone: an open, a reopen, a named file.
        let mut opened = reread(with_ant(), Named::default());
        opened.land_by_photo = None;
        opened.start = 3;
        settle_by_photo(&mut opened, &on_screen, 0, 0, None);
        assert_eq!(opened.start, 3);

        let main = main_code();
        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        let at = |s: &str| apply.find(s).unwrap_or_else(|| panic!("apply_scan: {s}"));
        let settle = at("let landing = photo_open::settle_by_photo(&mut done, &on_screen, *current.borrow(), nav_count.get(), compare);");
        assert!(at("support::carry_live_review_on_rescan(") < settle, "after the live carry");
        assert!(at("photo_open::settle_reopen(&mut done, requested);") < settle, "in the place a reopen settles");
        assert!(settle < at("photo_open::RescanCarry::take(") && settle < at("*s = new_arc.clone();"), "before the carry reads the landing, and before the swap");
        assert!(section(&main, "let reload: Rc<dyn Fn(PathBuf)> = {", "\n    };").contains("let land_by_photo = matched.is_none().then(support::Named::default);"),
            "a reread that didn't find a file it was asked for lands by photo, not on the first photo");
        // Every route of the person's own navigation counts. The Compare helper's writes follow one of these.
        let tap = section(&main, "app.on_navigate(move |dir| {", "app.on_nav_released(move");
        assert!(tap.split_once("if *held == 0 {").unwrap().1.split_once("} else {").unwrap().0.contains("support::note_navigation(&nav_n);"), "an arrow");
        assert!(section(&main, "let prime_jump: Rc<dyn Fn(usize)> = {", "\n    };").contains("support::note_navigation(&nav_pj);"), "the strip, the grid, a jump");
        assert!(section(&main, "ShowShot::Focus(h) => {", "ShowShot::Jump => {").contains("support::note_navigation(&nav_sos);"), "an undo showing the other half");
        assert!(section(&main, "let back = if foc_b {", "ff_tc.set(true);").contains("support::note_navigation(&nav_tc);"), "leaving Compare");
        assert!(section(&main, "app.on_compare_set_active(move |half| {", "ff_sa.set(true);").contains("support::note_navigation(&nav_sa);"), "the other half");
        assert!(section(&main, "app.on_sel_to_compare(move |idx, half| {", "ff_sc.set(true);").contains("support::note_navigation(&nav_sc);"), "a photo sent to Compare");
        let tick = section(&main, "let browse_from = *cur_t.borrow();", "det_awaited_t.store(");
        for step in ["step_compare_browse_advance(", "step_scrub_advance(", "step_wheel_advance("] {
            assert!(tick.contains(step), "the tick's count spans {step}");
        }
        assert!(tick.ends_with("if *cur_t.borrow() != browse_from {\n            support::note_navigation(&nav_count_t);\n        }\n        "), "and comes after them:\n{}", &tick[tick.len().saturating_sub(120)..]);

        // The queue's half. Del ANT fails while on BEE, arrow to CAT: ANT is still there, still on CAT.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.arrow_to("BEE");
        cull.lock_file("ANT.png");
        let held = held_lock(&cull);
        cull.press_del(&["ANT"]);
        cull.arrow_to("CAT");
        drop(held);
        cull.settle();
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["ANT", "BEE", "CAT", "DOG"], "CAT"), "a failed delete leaves them where they are");
        // Twice, as culling is: BEE fails on CAT, and they stay on CAT without an arrow.
        cull.lock_file("BEE.png");
        cull.delete(&["BEE"]);
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["ANT", "BEE", "CAT", "DOG"], "CAT"));

        // Del ANT from CAT, Ctrl+Z on CAT, arrow to DOG before it lands: stays on DOG.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.arrow_to("CAT");
        cull.delete(&["ANT"]);
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["BEE", "CAT", "DOG"], "CAT"), "another photo left, and they stay");
        let guard = held_lock(&cull);
        assert_eq!(cull.press_z(), "job");
        cull.arrow_to("DOG");
        drop(guard);
        assert_eq!(cull.settle(), "back ANT");
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["ANT", "BEE", "CAT", "DOG"], "DOG"), "the photo reappears in the strip, and they stay");
        // Without the arrow: the photo that came back, also when the press came before the delete's job ran.
        cull.delete(&["ANT"]);
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.on, "ANT", "the photo that came back");
        cull.arrow_to("CAT");
        let guard = held_lock(&cull);
        cull.press_del(&["ANT"]);
        assert_eq!(cull.press_z(), "job", "its delete is still queued");
        drop(guard);
        assert_eq!(cull.settle(), "back ANT");
        assert_eq!(cull.on, "ANT");
        // The newest press found nothing in the bin: the one before it still shows what it brought back.
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        cull.io.0.lock().unwrap().gone_from_bin.insert("ANT.png".into());
        let guard = held_lock(&cull);
        assert_eq!(cull.press_z(), "job", "BEE");
        assert_eq!(cull.press_z(), "job", "ANT");
        drop(guard);
        assert_eq!(cull.settle(), "not back");
        assert_eq!(cull.on, "BEE");
    }

    /// The view rule: while Compare stays, a photo coming back isn't landed on as such.
    /// `step_compare_retarget` would put it in the focused half, or refuse with a sentence about pins. A
    /// photo with no half of its own to go back to returns to the list. Pressed twice, naming photos.
    ///
    /// FALSIFIER (L28): in `settle_by_photo`, use the name while Compare stays: the first row lands on CAT.
    /// Treat a Compare that closes as staying, or give it no name: the last row.
    #[test]
    fn a_photo_coming_back_in_compare_keeps_the_pair() {
        let landed = |on_screen: &[&str], back: &[&str], named: &str, on: usize, compare: Option<(usize, usize)>| {
            let on_screen = list(on_screen);
            let mut done = reread(list(back), recovering(named, 4));
            settle_by_photo(&mut done, &on_screen, on, 4, compare.map(|(a, b)| support::ComparePair { a, b, focus: 1 }));
            done.shots[done.start].name.clone()
        };
        // In Compare on ANT and BEE, focused on BEE. CAT and then DOG come back.
        assert_eq!(landed(&["ANT", "BEE", "EEL"], &["ANT", "BEE", "CAT", "EEL"], "CAT", 1, Some((0, 1))), "BEE");
        assert_eq!(landed(&["ANT", "BEE", "CAT", "EEL"], &["ANT", "BEE", "CAT", "DOG", "EEL"], "DOG", 1, Some((0, 1))), "BEE");
        // A photo coming back before the pair doesn't move it.
        assert_eq!(landed(&["BEE", "CAT"], &["ANT", "BEE", "CAT"], "ANT", 1, Some((0, 1))), "CAT");
        // Out of Compare it lands, as Ctrl+Z shows what it brought back.
        assert_eq!(landed(&["ANT", "BEE", "EEL"], &["ANT", "BEE", "CAT", "EEL"], "CAT", 1, None), "CAT");
        // A half left in the same listing: it takes the next that stayed, and the name doesn't land.
        assert_eq!(landed(&["ANT", "BEE", "EEL"], &["ANT", "CAT", "EEL"], "CAT", 1, Some((0, 1))), "EEL");
        // Fewer than two left: Compare closes, and it's the single view's rule again.
        assert_eq!(landed(&["ANT", "BEE"], &["CAT"], "CAT", 1, Some((0, 1))), "CAT");
        assert_eq!(landed(&["ANT", "BEE"], &["BEE", "CAT"], "CAT", 1, Some((0, 1))), "CAT", "BEE stayed, and with no other that did, nothing can take ANT's half");
    }

    /// A rating sort is settled again by the carried ratings at a landing by photo, as a reopen does. A route
    /// that settled its own landing is left as it is, though `settle_by_photo` still reads Compare's answer for it.
    ///
    /// FALSIFIER (L28): in `settle_by_photo`, never refresh: the first row keeps the folder's order. Always
    /// refresh: the second row sorts a list its own route has settled.
    #[test]
    fn a_rating_sort_settles_again_only_at_a_landing_by_photo() {
        let rated = |named: Option<Named>| {
            let mut done = reread(list(&["ANT", "BEE", "CAT"]), Named::default());
            done.sort = (SortMethod::Rating, false);
            done.source_order = vec!["ANT".into(), "BEE".into(), "CAT".into()];
            done.ratings = vec![3, 1, 2];
            done.land_by_photo = named;
            settle_by_photo(&mut done, &list(&["ANT", "BEE", "CAT"]), 0, 4, None);
            let order: Vec<String> = done.shots.iter().map(|s| s.name.clone()).collect();
            (order, done.shots[done.start].name.clone())
        };
        let by_name = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<String>>();
        assert_eq!(rated(Some(Named::default())), (by_name(&["BEE", "CAT", "ANT"]), "ANT".to_string()), "fewest stars first, and still on ANT");
        assert_eq!(rated(None), (by_name(&["ANT", "BEE", "CAT"]), "ANT".to_string()), "its own route's order");
    }

    /// R5 (owner chose B, 3 November): "You compare two frames of a burst at 100%, B is soft, and you want
    /// it gone and the next frame in its place." The next photo that stayed, never the other half, by
    /// photo. Repeated, and from both ends.
    ///
    /// FALSIFIER (L28): in `support::halves_after`, walk with `None` for the other half: the first row
    /// compares BEE with itself. In `StayedWalk::from`, drop the forward walk or the walk back. Take the
    /// exact identity alone: the half-a-pair and cloud rows move on. Drop the `b < a` order: the row with
    /// the halves crossed. Drop the `new.len() < 2` line: the one photo shown twice.
    #[test]
    fn a_half_that_left_takes_the_next_photo_that_stayed_never_the_other_half() {
        let after = |on_screen: &[Shot], new: &[Shot], a: &str, b: &str| {
            let at = |n: &str| on_screen.iter().position(|s| s.name == n).unwrap();
            support::halves_after(on_screen, new, (at(a), at(b)), &[]).map(|(x, y)| (new[x].name.clone(), new[y].name.clone()))
        };
        let names = |on_screen: &[&str], new: &[&str], a: &str, b: &str| after(&list(on_screen), &list(new), a, b);
        let pair_of = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        // ANT goes, and BEE is next, in the other half: CAT. Each time, from where the last one left it.
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG", "EEL"], &["BEE", "CAT", "DOG", "EEL"], "ANT", "BEE"), pair_of("CAT", "BEE"));
        assert_eq!(names(&["BEE", "CAT", "DOG", "EEL"], &["BEE", "DOG", "EEL"], "CAT", "BEE"), pair_of("DOG", "BEE"));
        assert_eq!(names(&["BEE", "DOG", "EEL"], &["BEE", "EEL"], "DOG", "BEE"), pair_of("EEL", "BEE"));
        // The other half's photo goes: the same, with A untouched.
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG"], &["ANT", "CAT", "DOG"], "ANT", "BEE"), pair_of("ANT", "CAT"));
        // Nothing after it stayed: the nearest before it, and that's never the other half either.
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG"], &["ANT", "BEE", "CAT"], "ANT", "DOG"), pair_of("ANT", "CAT"));
        assert_eq!(names(&["ANT", "BEE", "CAT"], &["ANT", "BEE"], "BEE", "CAT"), pair_of("BEE", "ANT"));
        // Both go, as a Move rejected can: each takes its own, in order, either way round.
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG", "EEL"], &["CAT", "DOG", "EEL"], "ANT", "BEE"), pair_of("CAT", "DOG"));
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG", "EEL"], &["CAT", "DOG", "EEL"], "BEE", "ANT"), pair_of("DOG", "CAT"));
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG"], &["ANT", "BEE"], "CAT", "DOG"), pair_of("BEE", "ANT"));
        // A photo that arrived in the same listing was never on screen, so it's never taken for the next.
        assert_eq!(names(&["ANT", "BEE", "CAT", "DOG"], &["ANT", "NEW", "CAT", "DOG"], "ANT", "BEE"), pair_of("ANT", "CAT"));
        // Both halves on one photo, which the wheel allows: it stays, and if it goes they part.
        assert_eq!(names(&["ANT", "BEE"], &["ANT", "BEE"], "ANT", "ANT"), pair_of("ANT", "ANT"));
        assert_eq!(names(&["ANT", "BEE", "CAT"], &["BEE", "CAT"], "ANT", "ANT"), pair_of("BEE", "CAT"));
        // Photos that stayed follow, whatever the order did.
        assert_eq!(names(&["ANT", "BEE", "CAT"], &["CAT", "BEE", "ANT"], "ANT", "BEE"), pair_of("ANT", "BEE"));
        // A half of a pair gone, or a cloud flag just noticed: it's still that photo on screen.
        let lost_jpg = Shot { jpg: None, has_jpg: false, ..pair("ANT") };
        assert_eq!(after(&[pair("ANT"), shot("BEE"), shot("CAT")], &[lost_jpg, shot("BEE"), shot("CAT")], "ANT", "BEE"), pair_of("ANT", "BEE"));
        let freed = Shot { cloud_placeholder: true, ..shot("BEE") };
        assert_eq!(names(&["ANT", "BEE", "CAT"], &["ANT", "BEE", "CAT"], "ANT", "BEE"), pair_of("ANT", "BEE"));
        assert_eq!(after(&list(&["ANT", "BEE", "CAT"]), &[shot("ANT"), freed, shot("CAT")], "ANT", "BEE"), pair_of("ANT", "BEE"));
        // One photo left isn't a comparison, even shown twice.
        assert_eq!(names(&["ANT", "BEE"], &["ANT"], "ANT", "ANT"), None);
        assert_eq!(names(&["ANT", "BEE"], &["ANT"], "ANT", "BEE"), None);
    }

    /// R5: the person stays on the focused half, wherever its photo now is. `current` mirrors that half, so
    /// any other landing would read as a navigation at the top of the next tick, and move a half.
    ///
    /// FALSIFIER (L28): in `settle_by_photo`, land by the anchor alone: the first row lands on BEE, the
    /// other half. Read the focused half as A always: the B rows.
    #[test]
    fn the_person_stays_on_the_focused_half_wherever_it_goes() {
        let start = |on_screen: &[&str], left: &[&str], a: usize, b: usize, focus: i32| {
            let on_screen = list(on_screen);
            let mut done = reread(list(left), Named::default());
            let pair = support::ComparePair { a, b, focus };
            let landing = settle_by_photo(&mut done, &on_screen, pair.focused(), 4, Some(pair));
            assert!(matches!(landing, CompareLanding::Stays(..)));
            done.shots[done.start].name.clone()
        };
        let all = ["ANT", "BEE", "CAT", "DOG"];
        assert_eq!(start(&all, &["BEE", "CAT", "DOG"], 0, 1, 0), "CAT", "on ANT, in A, ANT goes");
        assert_eq!(start(&all, &["ANT", "CAT", "DOG"], 0, 1, 1), "CAT", "on BEE, in B, BEE goes");
        assert_eq!(start(&all, &["ANT", "CAT", "DOG"], 0, 1, 0), "ANT", "on ANT, BEE goes from the other half");
        assert_eq!(start(&all, &["BEE", "CAT", "DOG"], 0, 1, 1), "BEE", "on BEE, ANT goes from the other half");
        assert_eq!(start(&all, &["ANT", "BEE", "DOG"], 0, 1, 1), "BEE", "another photo goes");
        assert_eq!(start(&["ANT", "BEE", "CAT"], &["BEE", "CAT"], 0, 0, 1), "CAT", "one photo in both, and B has the focus");
        // Compare closing is the single view's landing.
        let mut done = reread(list(&["BEE"]), Named::default());
        let landing = settle_by_photo(&mut done, &list(&["ANT", "BEE"]), 0, 4, Some(support::ComparePair { a: 0, b: 1, focus: 0 }));
        assert_eq!((landing, done.shots[done.start].name.as_str()), (CompareLanding::Closes, "BEE"), "closes on the survivor");
    }

    /// R3: the tile a scroll holds, if it went: the first after it that stayed, else the nearest before
    /// it, as the landing walks. Twice over, naming photos.
    ///
    /// FALSIFIER (L28): in `KeptScroll::nearest_stayed`, drop the forward walk or the walk back.
    #[test]
    fn a_kept_scroll_walks_to_the_tile_that_stayed() {
        let (old, new) = (list(&["ANT", "BEE", "CAT", "DOG", "EEL"]), list(&["ANT", "DOG"]));
        let kept = carry_over(&old, &new, 0).keep_scroll(0, 1, false, None).expect("the order held");
        let now = |tile: usize| new[kept.nearest_stayed(tile)].name.as_str();
        assert_eq!((now(0), now(1), now(2), now(3)), ("ANT", "DOG", "DOG", "DOG"), "it stayed, or the next that did");
        assert_eq!((now(4), now(99)), ("DOG", "DOG"), "nothing after it stayed");
        let (old, new) = (new.clone(), list(&["ANT"]));
        let kept = carry_over(&old, &new, 0).keep_scroll(0, 2, false, None).expect("the order held");
        assert_eq!(new[kept.nearest_stayed(1)].name, "ANT");
    }

    // ── The one history (Delete round, 3 November). Owner: "If they are selected and deleted at the same
    //    time, bring back all 3; if sequentially, one by one, then the previous action". Rows name photos
    //    and repeat the action (L52, L53). The handlers only run in the app, so `Culling` does what they
    //    do through the calls they make, and each row pins them to those calls by source. ──

    /// The disk a job sees in these rows: the names of the files in the folder. No file, no Recycle Bin.
    #[derive(Default)]
    struct FakeDisk {
        files: BTreeSet<String>,
        locked: HashSet<String>,
        /// What's in the bin, as the jobs leave it.
        bin: BTreeSet<String>,
        /// Something has emptied these from the bin.
        gone_from_bin: HashSet<String>,
        crashes_on: Option<String>,
        /// The Mac's jobs confirm against the bin themselves.
        jobs_confirm: bool,
        /// What Falcon deleted itself, with no bin (R22).
        deleted: BTreeSet<String>,
        /// A file of that name has come to stand where these were, so they stay in the bin.
        occupied: HashSet<String>,
    }

    #[derive(Clone, Default)]
    struct FakeIo(Arc<Mutex<FakeDisk>>);

    fn file_name_of(p: &Path) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    impl file_queue::FileIo for FakeIo {
        fn barrier(&self) {}
        fn list_files(&self, targets: &[Shot]) -> Vec<PathBuf> {
            let crash = self.0.lock().unwrap().crashes_on.clone();
            assert!(!targets.iter().any(|s| Some(&s.name) == crash.as_ref()), "a job crashing, on purpose, for a row: the name is its own");
            targets.iter().flat_map(|s| [s.raw.clone(), s.jpg.clone()]).flatten().collect()
        }
        fn recycle(&self, paths: &[PathBuf]) -> file_queue::Recycled {
            let mut disk = self.0.lock().unwrap();
            let files: Vec<PathBuf> = paths.iter().filter(|p| !disk.locked.contains(&file_name_of(p))).cloned().collect();
            for f in &files {
                disk.files.remove(&file_name_of(f));
                disk.bin.insert(file_name_of(f));
            }
            file_queue::Recycled {
                files,
                confirmed: disk.jobs_confirm,
                #[cfg(target_os = "macos")]
                trash_paths: Vec::new(),
            }
        }
        fn remove_for_good(&self, files: &[PathBuf]) -> (usize, usize, Vec<PathBuf>, String) {
            let mut disk = self.0.lock().unwrap();
            let (mut gone, mut why) = (Vec::new(), String::new());
            for f in files {
                if disk.locked.contains(&file_name_of(f)) {
                    why = format!("{} is locked", file_name_of(f));
                    break;
                }
                disk.files.remove(&file_name_of(f));
                disk.deleted.insert(file_name_of(f));
                gone.push(f.clone());
            }
            (gone.len(), files.len() - gone.len(), gone, why)
        }
        fn restore(&self, _name: &str, shown: &str, sent: &support::DeleteOutcome) -> (usize, String) {
            let mut disk = self.0.lock().unwrap();
            if sent.files.iter().any(|f| disk.gone_from_bin.contains(&file_name_of(f))) {
                return (0, format!("{shown} is no longer in the bin"));
            }
            let back: Vec<&PathBuf> = sent.files.iter().filter(|f| !disk.occupied.contains(&file_name_of(f))).collect();
            for f in &back {
                disk.files.insert(file_name_of(f));
                disk.bin.remove(&file_name_of(f));
            }
            if back.len() < sent.files.len() {
                return (back.len(), format!("Recovered {}/{} file(s) for {shown}", back.len(), sent.files.len()));
            }
            (sent.files.len(), format!("Recovered {shown}"))
        }
        fn missing_from_bin(&self, asks: &[file_queue::BinAsk]) -> Vec<file_queue::BinFound> {
            let disk = self.0.lock().unwrap();
            asks.iter().filter_map(|ask| {
                let missing: Vec<PathBuf> = ask.files.iter()
                    .filter(|f| !disk.bin.contains(&file_name_of(f)) || disk.gone_from_bin.contains(&file_name_of(f)))
                    .cloned()
                    .collect();
                (!missing.is_empty()).then_some(file_queue::BinFound { seq: ask.seq, missing })
            }).collect()
        }
        fn undo_moves(&self, pairs: &[(PathBuf, PathBuf)]) -> (usize, usize, usize, usize) {
            let mut disk = self.0.lock().unwrap();
            for (from, _) in pairs {
                disk.files.insert(file_name_of(from));
            }
            (pairs.len(), 0, 0, 0)
        }
    }

    /// A folder as the handlers see it: the list on screen, the stack, the deletes and moves, the stamps,
    /// the photo the person is on, and the real file queue, running the real jobs on [`FakeIo`].
    /// `hold` keeps a job from starting, so a row can press while one is waiting or running.
    struct Culling {
        dir: String,
        shots: Vec<Shot>,
        on: String,
        nav: Cell<u64>,
        undo: RefCell<UndoRedo>,
        files: RefCell<support::FileHistory>,
        stamp: Cell<u64>,
        stars: HashMap<String, i32>,
        io: FakeIo,
        hold: &'static Mutex<()>,
        queue: file_queue::FileQueue,
        pending: RefCell<Vec<NotifEntry>>,
        landing_name: RefCell<Option<Named>>,
        /// A Move's worker posts here.
        reload: Mutex<Option<PathBuf>>,
        reread: RefCell<file_queue::Reread>,
        /// `listing_stamps`, as the last landing left them.
        stamps: RefCell<Option<support::ListingStamps>>,
        gen: Cell<u64>,
        /// The events panel's rows, once they've been folded in.
        log: RefCell<VecDeque<NotifEntry>>,
        waiting: u32,
        moved_back: String,
        /// Compare's two photos, if it's open, and which half has the focus.
        compare: Option<(String, String)>,
        focus: i32,
        /// What `tick::recover_over` would put on the card, by the same question (R36).
        recovered_card: Option<String>,
        /// R46: the blue edge, handed what came back at each landing, as `apply_scan` hands it.
        edge: crate::blue_edge::BlueEdge,
    }

    const APPLIED_BY_NAME: support::AppliedScan = support::AppliedScan { sort: (SortMethod::Name, false), review_load: support::ReviewLoad::Loaded };

    /// Holds the queue until the guard goes. The guard doesn't borrow the row's `Culling`.
    fn held_lock(cull: &Culling) -> std::sync::MutexGuard<'static, ()> {
        cull.hold.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Culling {
        fn new(names: &[&str]) -> Self {
            let files: Vec<String> = names.iter().map(|n| format!("{n}.png")).collect();
            Self::with_files(&files.iter().map(String::as_str).collect::<Vec<_>>())
        }
        /// From the files in the folder, by name: a `.CR3` and a `.png` of one name are a pair.
        fn with_files(files: &[&str]) -> Self {
            let io = FakeIo::default();
            io.0.lock().unwrap().files = files.iter().map(|f| f.to_string()).collect();
            let hold: &'static Mutex<()> = Box::leak(Box::new(Mutex::new(())));
            let io_q = io.clone();
            let queue = file_queue::FileQueue::with_runner(Box::new(move |job| {
                let _held = hold.lock().unwrap_or_else(|e| e.into_inner());
                file_queue::run_job(job, &io_q)
            }));
            let mut cull = Self {
                dir: "folder".into(), shots: Vec::new(), on: String::new(), nav: Cell::new(7), undo: Default::default(),
                files: Default::default(), stamp: Cell::new(0), stars: HashMap::new(), io, hold, queue,
                pending: Default::default(), landing_name: Default::default(), reload: Mutex::new(None),
                reread: Default::default(), stamps: Default::default(), gen: Cell::new(1), log: Default::default(),
                waiting: 0, moved_back: String::new(), compare: None, focus: 0, recovered_card: None,
                edge: Default::default(),
            };
            cull.shots = cull.on_disk();
            *cull.stamps.borrow_mut() = Some(cull.disk_stamps());
            cull.on = cull.shots.first().map(|s| s.name.clone()).unwrap_or_default();
            cull
        }
        /// Another program saves a file in the folder.
        fn save_beside(&self, file: &str) {
            self.io.0.lock().unwrap().files.insert(file.to_string());
        }
        fn lock_file(&self, file: &str) {
            self.io.0.lock().unwrap().locked.insert(file.to_string());
        }
        fn on_disk(&self) -> Vec<Shot> {
            let disk = self.io.0.lock().unwrap();
            let names: BTreeSet<String> = disk.files.iter().map(|f| Path::new(f).file_stem().unwrap().to_string_lossy().into_owned()).collect();
            names.into_iter().map(|n| {
                let (raw, jpg) = (disk.files.contains(&format!("{n}.CR3")), disk.files.contains(&format!("{n}.png")));
                match (raw, jpg) {
                    (true, true) => pair(&n),
                    (true, false) => Shot { jpg: None, has_jpg: false, ..pair(&n) },
                    _ => shot(&n),
                }
            }).collect()
        }
        /// The folder's stamps, as a listing reads them: they only change when a file comes or goes here.
        fn disk_stamps(&self) -> support::ListingStamps {
            let t0 = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
            self.io.0.lock().unwrap().files.iter().map(|f| (PathBuf::from(f), (4u64, t0))).collect()
        }
        fn names(&self) -> Vec<&str> {
            self.shots.iter().map(|s| s.name.as_str()).collect()
        }
        fn place(&self, photo: &str) -> usize {
            self.shots.iter().position(|s| s.name == photo).unwrap_or_else(|| panic!("{photo} is on screen"))
        }
        fn next_stamp(&self) -> u64 {
            self.stamp.set(self.stamp.get() + 1);
            self.stamp.get()
        }
        fn arm(&self) -> Option<support::UndoArm> {
            let files = self.files.borrow();
            support::live_undo_arm(self.undo.borrow().top_seq(), files.delete_arm(&self.dir), &self.dir, files.move_arm(&self.dir))
        }
        fn record_name(&self, seq: u64) -> String {
            self.files.borrow().delete_by_seq(seq).expect("a record").name.clone()
        }
        /// The person's own arrow, strip or jump.
        fn arrow_to(&mut self, photo: &str) {
            self.on = photo.to_string();
            support::note_navigation(&self.nav);
        }
        /// Compare opened on two photos, with `current` on the focused one.
        fn open_compare(&mut self, a: &str, b: &str, focus: i32) {
            self.compare = Some((a.to_string(), b.to_string()));
            self.focus = focus;
            self.on = if focus == 1 { b } else { a }.to_string();
        }
        fn pair_now(&self) -> Option<support::ComparePair> {
            self.compare.as_ref().map(|(a, b)| support::ComparePair { a: self.place(a), b: self.place(b), focus: self.focus })
        }
        fn halves(&self) -> (&str, &str) {
            let (a, b) = self.compare.as_ref().expect("Compare is open");
            (a, b)
        }
        /// `cull_target`: the photo Del, X and the stars act on.
        fn active(&self) -> String {
            let (a, b) = self.halves();
            if self.focus == 1 { b } else { a }.to_string()
        }
        /// The wheel takes a half to another photo, and `current` follows the half with the focus.
        fn browse_half(&mut self, half: i32, photo: &str) {
            let pair = self.compare.as_mut().expect("Compare is open");
            *(if half == 1 { &mut pair.1 } else { &mut pair.0 }) = photo.to_string();
            if half == self.focus {
                self.on = photo.to_string();
            }
            support::note_navigation(&self.nav);
        }
        fn star(&mut self, photo: &str, stars: i32) {
            let was = self.stars.insert(photo.to_string(), stars).unwrap_or(0);
            support::push_undo(&self.undo, self.next_stamp(), self.place(photo), was, 0);
        }
        fn turn(&self, photo: &str) {
            support::push_undo_rot(&self.undo, self.next_stamp(), photo, self.place(photo), 0);
        }
        fn star_together(&mut self, photos: &[&str], stars: i32) {
            let mut edits = Vec::new();
            for p in photos {
                let at = self.place(p);
                edits.push((at, self.stars.insert(p.to_string(), stars).unwrap_or(0), 0));
            }
            support::push_undo_bulk(&self.undo, self.next_stamp(), edits, "Rating");
        }
        fn turn_together(&self, photos: &[&str]) {
            let turns = photos.iter().map(|p| (p.to_string(), self.place(p), 0)).collect();
            support::push_undo_bulk_rot(&self.undo, self.next_stamp(), turns);
        }
        /// A landing, as `apply_scan` does it: by photo, the stack following each photo, then the steps
        /// coming back joining it, and a new generation.
        fn landed(&mut self, mut done: ScanDone) {
            let anchor = self.shots.iter().position(|s| s.name == self.on).unwrap_or(0);
            let landing = settle_by_photo(&mut done, &self.shots, anchor, self.nav.get(), self.pair_now());
            self.compare = match landing {
                CompareLanding::Stays(a, b) => Some((done.shots[a].name.clone(), done.shots[b].name.clone())),
                _ => None,
            };
            let identity = identity_map(&self.shots, &done.shots);
            remap_history(&mut self.undo.borrow_mut(), &identity);
            self.on = done.shots.get(done.start).map(|s| s.name.clone()).unwrap_or_default();
            *self.stamps.borrow_mut() = done.stamps.take();
            self.shots = done.shots;
            self.gen.set(self.gen.get() + 1);
            let returned = std::mem::take(&mut self.files.borrow_mut().returning);
            support::return_parked_steps(&mut self.undo.borrow_mut(), returned, &self.shots);
            // R46: what the person brought back, on the stage this landing settled, as `apply_scan` does. One
            // folder sorted by name, so every landing here keeps the scroll.
            let brought = std::mem::take(&mut self.files.borrow_mut().brought_back);
            let current = self.shots.iter().position(|s| s.name == self.on).unwrap_or(0);
            let stage = crate::blue_edge::Stage { current, halves: self.pair_now().map(|pair| (pair.a, pair.b)) };
            self.edge.landed(Some(&identity), &brought, &self.shots, stage);
        }
        /// R46: the photos the blue edge is about now, by name.
        fn edge_photos(&self) -> Vec<&str> {
            self.edge.photos().into_iter().map(|i| self.shots[i].name.as_str()).collect()
        }
        /// R46: what the strip's edges say on this stage with the strip at the photo on screen, `half` cells
        /// each side: the photos a click on the left and on the right go to.
        fn strip_edges(&mut self, half: f32) -> (Option<String>, Option<String>) {
            let stage = crate::blue_edge::Stage { current: self.place(&self.on), halves: self.pair_now().map(|pair| (pair.a, pair.b)) };
            let strip = crate::blue_edge::StripView { pos: self.place(&self.on) as f32, half };
            self.edge.step(Instant::now(), stage, Some(strip), None, false, false);
            let sides = self.edge.sides();
            let name = |i: i32| usize::try_from(i).ok().map(|i| self.shots[i].name.clone());
            (name(sides[0]), name(sides[1]))
        }
        /// What a listing's thread brings back now, as `support::reopen_scan` does: the disk's photos and
        /// their stamps, with the landing left for the tick.
        fn listing(&self) -> ScanDone {
            let mut done = reread(self.on_disk(), Named::default());
            done.land_by_photo = None;
            done.stamps = Some(self.disk_stamps());
            done
        }
        /// The tick's start of a listing, if one is due at `now`.
        fn start_listing(&self, now: Instant) -> Option<file_queue::ListingTicket> {
            if !self.reread.borrow().due(now, self.queue.quiet(), false) {
                return None;
            }
            self.reread.borrow_mut().start(self.gen.get(), self.queue.pushes())
        }
        /// The listing arrives, reading the folder as it is now.
        fn deliver(&self, ticket: &file_queue::ListingTicket) {
            let listed = file_queue::Listed { result: Ok(self.listing()), cloud_flipped: false, warn: None, took: Duration::ZERO };
            *ticket.arrived.lock().unwrap() = Some(listed);
        }
        /// A reread asked for now, and its listing there for the tick, in a row that wants one by hand.
        fn list_now(&self) {
            self.reread.borrow_mut().want(PathBuf::from(&self.dir), true);
            let ticket = self.start_listing(Instant::now()).expect("a listing is due");
            self.deliver(&ticket);
        }
        /// The tick's acceptance: what it did, in words. `held` is what a landing would close.
        fn accept(&mut self, held: bool) -> &'static str {
            let accepted = {
                let stamps = self.stamps.borrow();
                self.reread.borrow_mut().accept(self.gen.get(), self.queue.pushes(), stamps.as_ref(), || held)
            };
            match accepted {
                file_queue::Acceptance::Nothing => "nothing",
                file_queue::Acceptance::Discarded => "discarded",
                file_queue::Acceptance::Failed => {
                    let named = file_queue::no_landing(&self.files, &self.undo, &self.landing_name, None);
                    self.recover_over(named);
                    "failed"
                }
                file_queue::Acceptance::Unchanged(_) => {
                    let named = file_queue::no_landing(&self.files, &self.undo, &self.landing_name, Some(&self.shots));
                    self.recover_over(named);
                    "unchanged"
                }
                file_queue::Acceptance::Land(mut done, _) => {
                    let named = self.landing_name.borrow_mut().take();
                    done.land_by_photo = Some(named.clone().unwrap_or_default());
                    self.landed(*done);
                    self.recover_over(named);
                    "landed"
                }
            }
        }
        /// `tick::recover_over`'s question (R36): is the photo there to be seen, or does the card name it?
        fn recover_over(&mut self, named: Option<Named>) {
            let Some(Named { files, clean: Some(clean), .. }) = named else { return };
            let at = self.shots.iter().position(|s| s.name == self.on).unwrap_or(0);
            let halves = self.pair_now().map(|pair| (pair.a, pair.b));
            if !support::recovered_on_screen(&files, &self.shots, at, halves) {
                self.recovered_card = Some(clean.words);
            }
        }
        /// The bin check, run to its end here, and its fold. `true`: a row stopped offering.
        fn check_bin(&mut self) -> bool {
            let asks = self.files.borrow_mut().take_bin_asks();
            let found = file_queue::FileIo::missing_from_bin(&self.io, &asks);
            self.fold_found(found)
        }
        fn fold_found(&mut self, found: Vec<file_queue::BinFound>) -> bool {
            let to = file_queue::FoldTo { history: &self.files, pending: &self.pending, landing_name: &self.landing_name, reread: &self.reread };
            file_queue::fold_bin_check(found, &to, &self.log, Instant::now())
        }
        /// The reports so far, folded, in press order, as the tick's top does. What the last said, in words.
        fn fold_reports(&mut self, said: String) -> String {
            self.fold_reports_at(said, Instant::now())
        }
        fn fold_reports_at(&mut self, mut said: String, now: Instant) -> String {
            for done in self.queue.take_results() {
                said = match &done {
                    file_queue::FileDone::Recover { recovered: 0, msg: None, .. } => "silent".to_string(),
                    file_queue::FileDone::Recover { recovered: 0, .. } => "not back".to_string(),
                    file_queue::FileDone::Recover { files, .. } => {
                        format!("back {}", files.iter().map(|f| Path::new(f).file_stem().unwrap().to_string_lossy()).collect::<Vec<_>>().join(" "))
                    }
                    file_queue::FileDone::UndoMove { .. } => format!("moved back {}", self.moved_back),
                    _ => said,
                };
                file_queue::fold_file_done(done, &file_queue::FoldTo { history: &self.files, pending: &self.pending, landing_name: &self.landing_name, reread: &self.reread }, now);
            }
            said
        }
        fn until_quiet(&self) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !self.queue.quiet() {
                assert!(Instant::now() < deadline, "the queue should go quiet: is a row still holding it?");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        /// The tick, once the queue has done its work, and its quiet second has passed: the reports in
        /// press order, the reread, the presses that waited for it. What the last report said, in words.
        fn settle(&mut self) -> String {
            let mut said = String::new();
            for _ in 0..50 {
                self.until_quiet();
                said = self.fold_reports(said);
                let request = self.reload.lock().unwrap().take();
                if let Some(p) = request {
                    self.reread.borrow_mut().want(p, true);
                }
                let dir = self.dir.clone();
                if self.reread.borrow_mut().forget_unless(|d| d.display().to_string() == dir) {
                    let named = file_queue::no_landing(&self.files, &self.undo, &self.landing_name, None);
                    self.recover_over(named);
                }
                self.accept(false);
                for _ in 0..std::mem::take(&mut self.waiting) {
                    let pressed = self.press_z();
                    if pressed != "job" && pressed != "waits" {
                        said = pressed;
                    }
                }
                if let Some(ticket) = self.start_listing(Instant::now() + 2 * file_queue::QUIET_BEFORE_LISTING) {
                    self.deliver(&ticket);
                }
                if self.queue.quiet() && self.waiting == 0 && self.reload.lock().unwrap().is_none() && !self.reread.borrow().wanted_or_running() {
                    return said;
                }
            }
            panic!("a press still waits for a landing that never comes");
        }
        /// What step_notifs would make of the reports so far: each row's words, severity and pill.
        fn rows_so_far(&self) -> Vec<(String, i32, bool)> {
            let mut pending = self.pending.borrow_mut();
            pending.retain_mut(|e| crate::tick::delete_result_still_true(e, &self.files.borrow()));
            pending.drain(..).map(|e| (e.text, e.kind, e.recover)).collect()
        }
        /// `on_delete_recycle`'s press and the tick's drain, `take_out`: a selection is one record, the
        /// photos leave the list, the person lands by photo. The job is queued, not yet done.
        fn press_del(&mut self, photos: &[&str]) {
            self.press_del_on(photos, support::DeleteRoute::Bin { asks: false });
        }
        /// The same, by the route the person was told, or wasn't asked about.
        fn press_del_on(&mut self, photos: &[&str], route: support::DeleteRoute) {
            support::note_file_press(&self.undo);
            let targets = photos.iter().map(|p| {
                let at = self.place(p);
                (at, self.shots[at].clone())
            }).collect();
            let (name, count) = if let [one] = photos { (one.to_string(), 0) } else { (format!("{} photos", photos.len()), photos.len()) };
            let press = file_queue::TakeOut { dir: self.dir.clone(), gen: 1, name, photos: count, targets, route };
            let done = support::take_out_scan(&self.shots, &press.targets, PathBuf::from(&self.dir), self.stamps.borrow().as_ref(), FolderAttrs::default(), true, None, APPLIED_BY_NAME);
            let seats = self.pair_now().map_or_else(Vec::new, |pair| support::seats_emptied((pair.a, pair.b), &self.shots, &done.shots));
            let job = file_queue::press_delete(&self.files, &self.undo, self.next_stamp(), press, seats);
            let returning = self.files.borrow_mut().hold_returning();
            self.landed(done);
            self.files.borrow_mut().release_returning(returning);
            self.queue.push(job, None);
        }
        /// `on_delete_recycle`, and the tick that lands it.
        fn delete(&mut self, photos: &[&str]) {
            self.press_del(photos);
            self.settle();
        }
        /// The same, after a dialog that said which route.
        fn delete_on(&mut self, photos: &[&str], route: support::DeleteRoute) {
            self.press_del_on(photos, route);
            self.settle();
        }
        /// `on_move_rejects`, then the tick's fold when the worker's pairs arrive and the reread after it.
        fn move_rejected(&mut self, photos: &[&str]) {
            support::note_file_press(&self.undo);
            let pairs: Vec<(PathBuf, PathBuf)> = photos.iter().flat_map(|p| {
                let s = self.shots[self.place(p)].clone();
                [s.raw, s.jpg].into_iter().flatten().map(|f| {
                    let moved = PathBuf::from("Rejected").join(&f);
                    (f, moved)
                }).collect::<Vec<_>>()
            }).collect();
            for (from, _) in &pairs {
                self.io.0.lock().unwrap().files.remove(&file_name_of(from));
            }
            let seq = self.next_stamp();
            let steps = support::park_steps(&mut self.undo.borrow_mut(), &support::photos_in_pairs(&self.shots, &pairs));
            let subject = support::moved_subject(&self.shots, &pairs);
            self.files.borrow_mut().push_move(support::MoveRecord { folder: self.dir.clone(), pairs, seq, steps, subject });
            *self.reload.lock().unwrap() = Some(PathBuf::from(&self.dir));
            self.settle();
        }
        /// `on_recover_delete`, named by Ctrl+Z, a row or the card, and the tick that lands it.
        fn recover(&mut self, seq: u64) -> String {
            assert!(file_queue::press_recover(&self.files, &self.queue, seq, self.nav.get(), Instant::now()), "the press names a record");
            self.settle()
        }
        /// `on_undo`, up to what it does at once: "job" when a job has been queued, "waits" when the press
        /// waits for a landing, otherwise what it took back, in words.
        fn press_z(&mut self) -> String {
            let arm = self.arm();
            if support::undo_waits_for_landing(arm, self.files.borrow().newest_returning()) {
                self.waiting += 1;
                return "waits".into();
            }
            match arm {
                Some(support::UndoArm::Delete(seq)) => {
                    assert!(file_queue::press_recover(&self.files, &self.queue, seq, self.nav.get(), Instant::now()));
                    "job".into()
                }
                Some(support::UndoArm::Move(seq)) => self.press_undo_move(seq),
                Some(support::UndoArm::Stack(seq)) => {
                    let entry = self.undo.borrow_mut().undo.pop().expect("the arm names a step").entry;
                    let (said, mirror) = self.apply(entry);
                    self.undo.borrow_mut().redo.push(Act { seq, entry: mirror });
                    said
                }
                None => "nothing".into(),
            }
        }
        /// `on_undo_move`, named by Ctrl+Z, a row or the card.
        fn press_undo_move(&mut self, seq: u64) -> String {
            let names: Vec<String> = self.files.borrow().move_by_seq(seq).expect("the press names a record").pairs.iter().map(|(from, _)| {
                from.file_stem().unwrap().to_string_lossy().into_owned()
            }).collect();
            self.moved_back = names.join(" ");
            assert!(file_queue::press_undo_move(&self.files, &self.queue, seq, Instant::now()));
            "job".into()
        }
        /// `on_undo`, and the tick that lands what it queued: what the press took back, in words.
        fn ctrl_z(&mut self) -> String {
            let pressed = self.press_z();
            if pressed == "job" || pressed == "waits" { self.settle() } else { pressed }
        }
        /// `on_redo`: only the stack can arm it.
        fn ctrl_y(&mut self) -> String {
            let Some(Act { seq, entry }) = self.undo.borrow_mut().redo.pop() else { return "nothing".into() };
            let (said, mirror) = self.apply(entry);
            self.undo.borrow_mut().undo.push(Act { seq, entry: mirror });
            said
        }
        /// A step put into effect, by the photos its indices address now, and its mirror.
        fn apply(&mut self, entry: UndoEntry) -> (String, UndoEntry) {
            let name_at = |shots: &[Shot], i: usize| shots[i].name.clone();
            match entry {
                UndoEntry::Cull { idx, rating, mark } => {
                    let name = name_at(&self.shots, idx);
                    let was = self.stars.insert(name.clone(), rating).unwrap_or(0);
                    (format!("star {name}"), UndoEntry::Cull { idx, rating: was, mark })
                }
                UndoEntry::Rot { name, idx, delta } => {
                    assert_eq!(name_at(&self.shots, idx), name, "the index follows the photo");
                    (format!("turn {name}"), UndoEntry::Rot { name, idx, delta })
                }
                UndoEntry::Bulk { edits, verb } => {
                    let mut names = Vec::new();
                    let mirror = edits.iter().map(|&(idx, rating, mark)| {
                        let name = name_at(&self.shots, idx);
                        let was = self.stars.insert(name.clone(), rating).unwrap_or(0);
                        names.push(name);
                        (idx, was, mark)
                    }).collect();
                    names.sort();
                    (format!("stars {}", names.join(" ")), UndoEntry::Bulk { edits: mirror, verb })
                }
                UndoEntry::BulkRot { turns } => {
                    let mut names: Vec<String> = turns.iter().map(|(name, idx, _)| {
                        assert_eq!(&name_at(&self.shots, *idx), name, "the index follows the photo");
                        name.clone()
                    }).collect();
                    names.sort();
                    (format!("turns {}", names.join(" ")), UndoEntry::BulkRot { turns })
                }
            }
        }
    }

    /// R5: Del in Compare takes the active half and the next photo takes its place, and Ctrl+Z puts each
    /// back, "on that side". Three times each way, through the real presses, jobs and rereads.
    ///
    /// FALSIFIER (L28): `press_delete` files no seats, `press_recover` or the job drops them, or `run_recover`
    /// reports none: the photo returns to the list and the half stays. `support::halves_after` skips the
    /// seats: the same. Leave a half's seat on A always (`seat.half == 1`): the row with the focus on B.
    #[test]
    fn del_in_compare_takes_the_active_half_and_ctrl_z_puts_each_back() {
        // ANT is the active half and BEE, the next in the folder, is in the other: CAT is what takes its place.
        for (focus, kept, first, second, third) in [(0, "BEE", "ANT", "CAT", "DOG"), (1, "BEE", "ANT", "CAT", "DOG")] {
            let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
            let (a, b) = if focus == 0 { (first, kept) } else { (kept, first) };
            cull.open_compare(a, b, focus);
            let side = |cull: &Culling, photo: &str| if focus == 0 { (photo.to_string(), kept.to_string()) } else { (kept.to_string(), photo.to_string()) } == (cull.halves().0.to_string(), cull.halves().1.to_string());
            for (gone, next) in [(first, second), (second, third), (third, "EEL")] {
                assert_eq!(cull.active(), gone);
                cull.press_del(&[gone]);
                assert!(side(&cull, next), "focus {focus}, {gone} goes: {next} takes its side, at the press: {:?}", cull.halves());
                assert_eq!(cull.on, next, "focus {focus}: the person is on the active half");
                cull.settle();
                assert!(side(&cull, next), "focus {focus}: and still after the reread");
            }
            for (back, before) in [(third, "EEL"), (second, third), (first, second)] {
                assert!(side(&cull, before));
                assert_eq!(cull.ctrl_z(), format!("back {back}"));
                assert!(side(&cull, back), "focus {focus}: {back} is back on its side: {:?}", cull.halves());
                assert_eq!(cull.on, back, "focus {focus}: and the person is on it");
            }
            assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
            assert_eq!(cull.ctrl_z(), "nothing");
        }
    }

    /// R8 in Compare: star, Del, Ctrl+Z, Ctrl+Z ends unstarred on that photo, in that half.
    ///
    /// FALSIFIER (L28): the seat's return is lost as above: the first Ctrl+Z leaves CAT in the half.
    #[test]
    fn star_del_undo_undo_in_compare_ends_unstarred_in_that_half() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.open_compare("ANT", "BEE", 0);
        cull.star("ANT", 3);
        cull.delete(&["ANT"]);
        assert_eq!(cull.halves(), ("CAT", "BEE"));
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!((cull.halves(), cull.stars["ANT"]), (("ANT", "BEE"), 3));
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!((cull.halves(), cull.stars["ANT"]), (("ANT", "BEE"), 0));
        assert_eq!(cull.on, "ANT");
    }

    /// R5: a menu acts on its own photo. From the half that isn't active, the focus stays and that half takes
    /// the next. From a thumbnail, neither half moves, and its Ctrl+Z leaves them alone too: it has no half to
    /// go back to. Repeated.
    ///
    /// FALSIFIER (L28): `support::seats_emptied` files a seat for a half that kept its photo: the rows on it at
    /// the end. Leave a seat on A always: BEE returns to A. Let the second half take the first, at the end of the
    /// list where the nearest before it is the other half: CAT in both.
    #[test]
    fn a_menu_delete_in_compare_moves_only_the_half_it_took() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        cull.open_compare("ANT", "BEE", 0);
        for (gone, next) in [("BEE", "CAT"), ("CAT", "DOG")] {
            cull.delete(&[gone]);
            assert_eq!(cull.halves(), ("ANT", next), "{gone} leaves B");
            assert_eq!((cull.on.as_str(), cull.focus), ("ANT", 0), "the focus stays on A");
        }
        for third in ["FOX", "EEL"] {
            cull.delete(&[third]);
            assert_eq!(cull.halves(), ("ANT", "DOG"), "{third} was in neither half");
        }
        assert_eq!(cull.ctrl_z(), "back EEL");
        assert_eq!(cull.ctrl_z(), "back FOX");
        assert_eq!(cull.halves(), ("ANT", "DOG"), "they come back to the list, and nothing on the stage moves");
        assert_eq!(cull.on, "ANT");
        assert_eq!(cull.ctrl_z(), "back CAT");
        assert_eq!(cull.halves(), ("ANT", "CAT"), "that half");
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.halves(), ("ANT", "BEE"));
        assert_eq!(cull.on, "ANT", "and the focus never left A");
        // The last photo, with the other half just before it: the one before that.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.open_compare("CAT", "DOG", 0);
        cull.delete(&["DOG"]);
        assert_eq!(cull.halves(), ("CAT", "BEE"));
        assert_eq!(cull.on, "CAT");
        assert_eq!(cull.ctrl_z(), "back DOG");
        assert_eq!(cull.halves(), ("CAT", "DOG"));
        // What a press remembers: only the half it empties, by photo.
        let on_screen = list(&["ANT", "BEE", "CAT"]);
        assert!(support::seats_emptied((0, 1), &on_screen, &list(&["ANT", "BEE"])).is_empty(), "a third photo empties no half");
        let files = |photo: &str| shot(photo).jpg.into_iter().collect::<Vec<_>>();
        assert_eq!(support::seats_emptied((0, 1), &on_screen, &list(&["ANT", "CAT"])),
            [support::CompareSeat { half: 1, photo: files("BEE"), replacement: files("CAT") }], "BEE empties B");
    }

    /// R5: "if that half still shows the replacement". A half the person has browsed on keeps what they
    /// chose, and the photo returns to the list. The other half browsing doesn't count. Repeated.
    ///
    /// FALSIFIER (L28): in `support::halves_after`, drop the check on the replacement: DOG is thrown out of
    /// the half for ANT.
    #[test]
    fn ctrl_z_leaves_a_half_the_person_has_browsed_on() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        cull.open_compare("ANT", "BEE", 0);
        cull.delete(&["ANT"]);
        assert_eq!(cull.halves(), ("CAT", "BEE"));
        cull.browse_half(0, "DOG");
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.halves(), ("DOG", "BEE"), "the person went on to DOG");
        assert_eq!(cull.on, "DOG");
        assert_eq!(cull.names()[0], "ANT", "and ANT is back in the list");
        // Again, on the other half, with A browsed instead: B still shows what took BEE's place.
        cull.focus = 1;
        cull.on = "BEE".to_string();
        cull.delete(&["BEE"]);
        assert_eq!(cull.halves(), ("DOG", "CAT"));
        cull.browse_half(0, "EEL");
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.halves(), ("EEL", "BEE"), "B goes back, and A keeps what the person chose");
        assert_eq!(cull.on, "BEE");
    }

    /// R5 and L53: presses faster than a listing. Each goes back in turn, because the second's half shows
    /// what it expects only once the first's has gone back.
    ///
    /// FALSIFIER (L28): in `fold_file_done`, keep only the newest press's seats: the halves stay on EEL.
    #[test]
    fn three_ctrl_z_in_one_listing_put_all_three_back() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        cull.open_compare("ANT", "BEE", 0);
        for gone in ["ANT", "CAT", "DOG"] {
            cull.delete(&[gone]);
        }
        assert_eq!(cull.halves(), ("EEL", "BEE"));
        let held = held_lock(&cull);
        for _ in 0..3 {
            assert_eq!(cull.press_z(), "job");
        }
        drop(held);
        cull.settle();
        assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        assert_eq!(cull.halves(), ("ANT", "BEE"), "the oldest of the three, back on its side");
        assert_eq!(cull.on, "ANT");
    }

    /// R5, R1: "The same answer covers a Compare photo moved by Move rejected", and Undo move moves nothing.
    /// Through the real reread, naming photos, on one half and on both.
    ///
    /// FALSIFIER (L28): in `support::halves_after`, read a half that left as closing: the first row. Follow
    /// nothing there (`Some(a)`, `Some(b)`): every Undo move shifts the halves onto other photos.
    #[test]
    fn move_rejected_of_a_half_follows_the_rule_and_undo_move_moves_nothing() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        cull.open_compare("ANT", "BEE", 0);
        cull.move_rejected(&["BEE"]);
        assert_eq!(cull.halves(), ("ANT", "CAT"), "BEE moved: the next that stayed");
        assert_eq!(cull.on, "ANT");
        assert_eq!(cull.ctrl_z(), "moved back BEE");
        assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        assert_eq!(cull.halves(), ("ANT", "CAT"), "Undo move moves nothing (R1)");
        assert_eq!(cull.on, "ANT");
        // Both halves in one move, and again.
        cull.move_rejected(&["ANT", "CAT"]);
        assert_eq!(cull.halves(), ("BEE", "DOG"), "each takes its own, and A was before B");
        assert_eq!(cull.on, "BEE", "the person is on the active half");
        cull.move_rejected(&["BEE", "DOG"]);
        assert_eq!(cull.halves(), ("EEL", "FOX"));
        assert_eq!(cull.on, "EEL");
        for _ in 0..2 {
            cull.ctrl_z();
            assert_eq!(cull.halves(), ("EEL", "FOX"), "and every Undo move leaves them");
        }
        assert_eq!(cull.names().len(), 6);
    }

    /// R5: "Fewer than two photos left: closes on the survivor". A panel that is gone is gone, so Ctrl+Z
    /// shows the photo in the single view.
    ///
    /// FALSIFIER (L28): `fold_file_done` names no files for the landing: Ctrl+Z stays on BEE. The view's half is
    /// `keep_view_has_two_forms_and_resets_only_where_it_must`.
    #[test]
    fn fewer_than_two_left_closes_compare_on_the_survivor() {
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.open_compare("ANT", "BEE", 0);
        cull.delete(&["ANT"]);
        assert!(cull.compare.is_none(), "one photo isn't a comparison");
        assert_eq!(cull.on, "BEE", "on the survivor");
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert!(cull.compare.is_none(), "it doesn't come back");
        assert_eq!(cull.on, "ANT", "and Ctrl+Z shows what it brought back");
        // By Move rejected, the same.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        cull.open_compare("ANT", "BEE", 1);
        cull.move_rejected(&["ANT", "BEE"]);
        assert!(cull.compare.is_none());
        assert_eq!(cull.on, "CAT");
    }

    /// R5 and the failure rule: a delete that doesn't happen brings the photo back to the list, and the
    /// person doesn't move, so the half keeps what took its place. There's no record, so no Ctrl+Z for it.
    /// A for-good delete has no record either (R22).
    ///
    /// FALSIFIER (L28): in `support::halves_after`, read a half that left as closing: the first row. In
    /// `FileHistory::settle_delete`, keep the record of a delete that sent nothing: Ctrl+Z takes that, not the
    /// star. In `press_delete`, give the for-good route a record: the last row brings ANT back.
    #[test]
    fn a_half_keeps_its_new_photo_when_the_delete_has_no_way_back() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.open_compare("ANT", "BEE", 0);
        cull.star("ANT", 2);
        cull.lock_file("ANT.png");
        cull.press_del(&["ANT"]);
        assert_eq!(cull.halves(), ("CAT", "BEE"));
        cull.settle();
        assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG"], "it never left the disk");
        assert_eq!(cull.halves(), ("CAT", "BEE"), "and the half stays where the person now is");
        assert_eq!(cull.ctrl_z(), "star ANT", "the delete isn't there to take back, and its star's step is");
        assert_eq!(cull.halves(), ("CAT", "BEE"));

        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.open_compare("ANT", "BEE", 0);
        cull.delete_on(&["ANT"], support::DeleteRoute::NoBin);
        assert_eq!(cull.halves(), ("CAT", "BEE"));
        assert_eq!(cull.ctrl_z(), "nothing", "for good, so no way back");
        assert_eq!(cull.halves(), ("CAT", "BEE"));
    }

    /// R5's wiring, which only runs in the app. The key's target is the active half, by the one question X
    /// and the stars ask, behind the same gate, and the refusal has gone. The drain remembers the seats before
    /// the landing moves the halves. `apply_scan` works Compare out once, for the landing and for the view.
    ///
    /// FALSIFIER (L28): `on_delete_key` reads `current`, or asks `app.get_compare()` again; the sentence comes
    /// back; the gate leaves Compare's branch; the drain files no seats, or reads them after `apply_scan`;
    /// `keep_view` is handed anything but the settled answer; the Mac's row promises a dialog in Compare.
    #[test]
    fn del_in_compare_is_wired_to_the_active_half() {
        let main = main_code();
        let key = section(&main, "app.on_delete_key(move || {", "\n        });");
        assert!(key.contains("let c = target_dk();"), "the key's photo is the cull target");
        assert!(section(&main, "let shots_dk = shots.clone();", "app.on_delete_key(move || {").contains("let target_dk = cull_target.clone();"));
        assert!(!key.contains("get_compare"), "and Compare isn't a reason to refuse");
        assert!(key.contains("if !delete_key_allowed(has_shot, app.get_modal_open(), false) {"));
        assert!(!main.contains(concat!("Delete needs a single ", "photo")), "the sentence has gone");
        let target = section(&main, "let cull_target: Rc<dyn Fn() -> usize> = {", "\n    };");
        assert!(target.contains("compare_cull_target(focus, a_idx.get(), b_idx.get())"), "the focused half");
        let gate = section(&main, "let cull_gate = {", "if blocked {");
        assert!(gate.contains("compare_cull_blocked("), "and it has to be showing, the way X asks");

        let drain = section(&main, "let take_out: Rc<dyn Fn(file_queue::TakeOut)> = {", "\n    };");
        let at = |s: &str| drain.find(s).unwrap_or_else(|| panic!("the drain: {s}"));
        let seats = at("let seats = if compare.get() { support::seats_emptied((cmp_a_idx.get(), cmp_b_idx.get()), &on_screen, &done.shots) } else { Vec::new() };");
        assert!(at("support::take_out_scan(") < seats && seats < at("file_queue::press_delete(&file_history, &undo_stack, seq, press, seats);"), "from the list leaving, into the record");
        assert!(seats < at("apply_scan(done);"), "before the landing moves the halves");

        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        let at = |s: &str| apply.find(s).unwrap_or_else(|| panic!("apply_scan: {s}"));
        let pair = at("let compare = view.compare.get().then(|| support::ComparePair { a: cmp_a_idx.get(), b: cmp_b_idx.get(), focus: aw.upgrade().map_or(0, |a| a.get_cmp_focus()) });");
        let settled = at("let landing = photo_open::settle_by_photo(&mut done, &on_screen, *current.borrow(), nav_count.get(), compare);");
        let kept = at("carry.keep_view(a.get_zoom(), (a.get_pan_x(), a.get_pan_y()), *current.borrow(), compare_landing, &roi.tiles.borrow())");
        assert!(pair < settled && settled < kept, "one answer, for both");

        let queue = queue_code();
        assert!(section(&queue, "pub(crate) fn press_recover(", "\n}").contains("seats: rec.seats, waited: waited.clone() }"), "Ctrl+Z takes its record's seats");
        assert!(section(&queue, "fn run_recover(", "\n}").contains("msg: Some(msg), nav_at_press, seats, waited }"), "and the job hands them on");
        assert!(main.contains("trash_at_once: !drive_asks_t.get() && (compare_t.get() || {"), "the Mac's row promises what its key does");
    }

    /// Deletes one at a time come back one at a time, newest first, then the action before. A slot lost
    /// the first delete to the second, and the next press reached the star.
    ///
    /// FALSIFIER (L28): `push_delete` clears the list first: the second press reaches the star. Ctrl+Z
    /// takes a record other than the arm's, or asks a list other than the folder's newest: the wiring rows.
    #[test]
    fn three_deletes_three_presses_then_the_star() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.star("BEE", 3);
        for photo in ["ANT", "CAT", "DOG"] {
            cull.delete(&[photo]);
        }
        assert_eq!(cull.names(), ["BEE", "EEL"]);
        assert_eq!(cull.ctrl_z(), "back DOG");
        assert_eq!(cull.ctrl_z(), "back CAT");
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG", "EEL"], "all three are back");
        assert_eq!(cull.ctrl_z(), "star BEE", "then the previous action");
        assert_eq!(cull.stars["BEE"], 0);
        assert_eq!(cull.ctrl_z(), "nothing");

        let main = main_code();
        let undo = section(&main, "app.on_undo(move || {", "let entry = undo_u.borrow_mut().undo.pop()");
        assert!(undo.contains("hist.delete_arm(&sd_u.borrow()),") && undo.contains("hist.move_arm(&sd_u.borrow()),"), "the newest in the folder on screen");
        assert!(undo.contains("Some(support::UndoArm::Delete(s)) => {\n                if let Some(a) = awz_u.upgrade() {\n                    a.invoke_recover_delete(s as i32);"), "the press names the record the arm found");
        assert!(undo.contains("Some(support::UndoArm::Move(s)) => {\n                if let Some(a) = awz_u.upgrade() {\n                    a.invoke_undo_move(s as i32);"));
        let recover = section(&main, "app.on_recover_delete(move |seq| {", "let shots_fr = shots.clone();");
        assert!(recover.contains("history_rc.borrow().delete_by_seq(seq)") && recover.contains("file_queue::press_recover(&history_rc, &queue_rc, seq, nav_at_press, Instant::now())"), "and the handler takes that one");
        assert!(queue_code().contains("let Some(rec) = hist.take_delete(seq) else { return false };"), "by its stamp");
        assert!(main.contains("hist.delete_arm(&scanned_dir_t.borrow()),\n                &scanned_dir_t.borrow(),"), "the tick publishes from the same question");
        assert!(main.contains("app.set_can_recover(file_history_t.borrow().delete_arm(&scanned_dir_t.borrow()).is_some());"));
    }

    /// What was deleted together comes back together: one record, one press, then the delete before.
    ///
    /// FALSIFIER (L28): file a record per photo in the selection arm: the wiring row. `with_back` is the
    /// test's own, so the handler's one `files` list is pinned there too.
    #[test]
    fn selection_of_three_is_one_entry_then_the_previous_action() {
        let mut cull = Culling::new(&["ANT", "ANT2", "BEE", "CAT", "DOG"]);
        cull.delete(&["ANT"]);
        cull.delete(&["ANT2", "BEE", "CAT"]);
        assert_eq!(cull.names(), ["DOG"]);
        assert_eq!(cull.files.borrow().deletes.len(), 2, "two things done, two entries");
        assert_eq!(cull.ctrl_z(), "back ANT2 BEE CAT", "one press, all three");
        assert_eq!(cull.ctrl_z(), "back ANT", "then the delete before");
        assert_eq!(cull.ctrl_z(), "nothing");

        let main = main_code();
        let handler = section(&main, "app.on_delete_recycle(move || {", "app.on_recover_delete(move |seq| {");
        let (selection, single) = handler.split_once("let Some((pgen, pdir, idx, name, route)) = del_pending_dr").expect("the two arms");
        assert_eq!(selection.matches("*take_out_dr.borrow_mut() = Some(file_queue::TakeOut {").count(), 1, "one record for the whole selection");
        assert_eq!(single.matches("*take_out_dr.borrow_mut() = Some(file_queue::TakeOut {").count(), 1);
        assert!(selection.contains("targets: Vec<(usize, Shot)> = ok_targets.iter()"), "of every photo it names");
        let queue = queue_code();
        let press = section(&queue, "pub(crate) fn press_delete(", "\n}");
        assert_eq!(press.matches("push_delete(").count(), 1, "one record for each press");
        assert!(press.contains("support::park_steps(&mut undo.borrow_mut(), &leaving)"), "and all their steps");
        assert!(press.contains("let steps = park(&binned);"), "by the same call, for what goes to the bin");
        assert!(section(&queue, "fn run_delete(", "\n}").contains("files: recycled.files,"), "with every file that reached the bin");
    }

    /// Move rejected done twice takes two Ctrl+Z, and an older row's Undo takes its own move.
    ///
    /// FALSIFIER (L28): `push_move` clears the list first: the second press does nothing. `take_move`
    /// ignores the stamp: the older row. `drop_moves_in` forgets every folder's: the Empty row. Fold with
    /// anything but `push_move`, or stop retiring the row: the wiring rows.
    #[test]
    fn two_moves_two_undos() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.move_rejected(&["ANT"]);
        cull.move_rejected(&["CAT"]);
        assert_eq!(cull.names(), ["BEE", "DOG"]);
        assert_eq!(cull.ctrl_z(), "moved back CAT");
        assert_eq!(cull.ctrl_z(), "moved back ANT");
        assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG"]);
        assert_eq!(cull.ctrl_z(), "nothing");

        // An older row's Undo, out of order: its own files, and the newer move is still next.
        cull.move_rejected(&["ANT"]);
        cull.move_rejected(&["CAT"]);
        let (older, newer) = (cull.files.borrow().moves[0].seq, cull.files.borrow().moves[1].seq);
        let taken = cull.files.borrow_mut().take_move(older).expect("the older move is there");
        assert_eq!(taken.pairs[0].0, PathBuf::from("ANT.png"));
        assert_eq!(cull.arm(), Some(support::UndoArm::Move(newer)));

        // Empty ./Rejected forgets this folder's moves and no other's.
        cull.files.borrow_mut().push_move(support::MoveRecord { folder: "another".into(), pairs: vec![(PathBuf::from("a"), PathBuf::from("b"))], seq: 90, steps: Vec::new(), subject: String::new() });
        assert_eq!(cull.files.borrow_mut().drop_moves_in("folder"), [newer]);
        assert_eq!(cull.arm(), None);
        assert_eq!(cull.files.borrow().moves.iter().map(|m| m.seq).collect::<Vec<_>>(), [90]);

        let main = main_code();
        assert!(section(&main, "let fold_move = || {", "fold_move();").contains("file_history_t.borrow_mut().push_move(support::MoveRecord { folder, pairs, seq, steps, subject });"), "the fold adds to the list");
        let handler = section(&main, "app.on_undo_move(move |seq| {", "std::thread::spawn(move");
        assert!(handler.contains("history_um.borrow().move_live(seq, &dir)") && handler.contains("file_queue::press_undo_move(&history_um, &queue_um, seq, Instant::now())"), "the handler takes the one it was named");
        assert!(queue_code().contains("let Some(rec) = hist.take_move(seq) else { return false };"), "by its stamp");
        assert!(handler.contains("tick::retire_row_offer(&mut log_um.borrow_mut(), seq);"), "and its own row stops offering it");
        let empty = section(&main, "app.on_empty_rejected(move || {", "std::thread::spawn(move");
        assert!(empty.contains("history_er.borrow_mut().drop_moves_in(&dir)") && empty.contains("tick::retire_row_offer(&mut log_er.borrow_mut(), seq);"), "Empty drops that folder's");
    }

    /// In another folder, Ctrl+Z says so once and forgets nothing, so coming back still works. The press
    /// used to consume the record, and the photo lost its way back.
    ///
    /// FALSIFIER (L28): take the record in the refusal, or let `nudge_once` speak every time, or keep
    /// `nudged` across folders: the rows below.
    #[test]
    fn other_folder_press_speaks_once_and_forgets_nothing() {
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.delete(&["ANT"]);
        let ant = cull.files.borrow().deletes[0].seq;
        cull.dir = "another folder".into();
        assert_eq!(cull.arm(), None, "nothing lights here");
        assert_eq!(cull.files.borrow_mut().nudge_once(), Some(ant), "the press speaks");
        assert_eq!(cull.files.borrow_mut().nudge_once(), None, "the second press is silent");
        assert_eq!(cull.files.borrow().nudge_seq(), None, "and the Mac row goes grey");
        assert!(cull.files.borrow().has_delete(ant), "nothing is forgotten");
        cull.dir = "folder".into();
        assert_eq!(cull.arm(), Some(support::UndoArm::Delete(ant)), "back in its folder the arm is ANT");
        assert_eq!(cull.ctrl_z(), "back ANT");
        // A new delete, and a press in another folder again: it speaks again.
        cull.delete(&["BEE"]);
        cull.dir = "another folder".into();
        assert!(cull.files.borrow_mut().nudge_once().is_some());

        let main = main_code();
        let recover = section(&main, "app.on_recover_delete(move |seq| {", "let shots_fr = shots.clone();");
        let refusal = section(recover, "if !support::recover_offer_live(&rec_dir, &live_dir) {", "let nav_at_press = nav_rc.get();");
        assert!(refusal.contains("&platform::cross_folder_recover_text(&subject, photos)") && !refusal.contains("press_recover"), "the refusal speaks and takes nothing:\n{refusal}");
        let undo = section(&main, "app.on_undo(move || {", "let entry = undo_u.borrow_mut().undo.pop()");
        assert!(section(undo, "None => {", "return;").contains("history_u.borrow_mut().nudge_once()"), "Ctrl+Z with nothing live asks once");
        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        assert!(apply.contains("if scan_same_folder {\n                    support::return_parked_steps(&mut undo_stack.borrow_mut(), returned, &new_arc);\n                } else {\n                    hist.nudged = false;\n                }"), "another folder speaks afresh");
        assert!(main.contains("hist.nudge_seq().is_some(),\n                    )"), "the Mac row follows it");
    }

    /// R7, the owner: Del, Ctrl+Z, then Ctrl+Y does nothing. There is no redo of a delete, so Ctrl+Y must
    /// not reach an older undone star on another photo and jump there.
    ///
    /// FALSIFIER (L28): `note_file_press` clears nothing: Ctrl+Y brings the star back.
    #[test]
    fn ctrl_y_after_delete_does_not_reach_an_older_star() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("BEE", 3);
        assert_eq!(cull.ctrl_z(), "star BEE");
        assert_eq!(cull.stars["BEE"], 0);
        assert_eq!(cull.undo.borrow().redo.len(), 1, "Ctrl+Y could bring it back, for now");
        cull.delete(&["CAT"]);
        assert_eq!(cull.ctrl_z(), "back CAT");
        assert_eq!(cull.ctrl_y(), "nothing");
        assert_eq!(cull.stars["BEE"], 0, "BEE's star stays off");
        let undo = cull.undo.borrow();
        let files = cull.files.borrow();
        let (_, redo_armed) = support::cull_undo_armed(undo.top_seq(), undo.redo.len(), files.delete_arm(&cull.dir), &cull.dir, files.move_arm(&cull.dir));
        assert!(!redo_armed, "and its button is grey");
    }

    /// R7: the press of a delete and of a move each end the redo branch, whichever way they end, and a
    /// Recover or an Undo move doesn't: what's on it was undone since then.
    ///
    /// FALSIFIER (L28): `note_file_press` clears nothing: the first row. Drop the call from the take-out's
    /// drain or from Move rejected, or add it to Recover or Undo move: the wiring rows.
    #[test]
    fn redo_is_empty_after_a_delete_and_a_move() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        let redo_len = |cull: &Culling| cull.undo.borrow().redo.len();
        cull.star("ANT", 1);
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!(redo_len(&cull), 1);
        cull.delete(&["DOG"]);
        assert_eq!(redo_len(&cull), 0, "a delete");
        cull.star("ANT", 2);
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!(redo_len(&cull), 1);
        cull.move_rejected(&["CAT"]);
        assert_eq!(redo_len(&cull), 0, "a move");
        cull.star("ANT", 3);
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!(cull.ctrl_z(), "moved back CAT");
        assert_eq!(cull.ctrl_z(), "back DOG");
        assert_eq!(redo_len(&cull), 1, "taking those back leaves it");
        assert_eq!(cull.ctrl_y(), "star ANT");
        assert_eq!(cull.stars["ANT"], 3);

        let main = main_code();
        let handler = section(&main, "app.on_delete_recycle(move || {", "app.on_recover_delete(move |seq| {");
        assert!(!handler.contains("note_file_press"), "R40: not in the handler, because the Mac's menu arm presses from inside a tick");
        let drain = section(&main, "let take_out: Rc<dyn Fn(file_queue::TakeOut)> = {", "\n    };");
        let press = drain.find("support::note_file_press(&undo_stack);").expect("the drain clears it, for both arms");
        let refusal = drain.rfind("delete cancelled").expect("after the drain's gate");
        assert!(refusal < press && press < drain.find("file_queue::press_delete(").unwrap(), "after the gates, before the record");
        let moving = section(&main, "app.on_move_rejects(move || {", "std::thread::spawn(move");
        assert!(moving.contains("support::note_file_press(&undo_mv);"), "Move rejected");
        for handler in [section(&main, "app.on_recover_delete(move |seq| {", "let shots_fr = shots.clone();"), section(&main, "app.on_undo_move(move |seq| {", "app.on_empty_rejected(move || {")] {
            assert!(!handler.contains("note_file_press"), "taking something back leaves the redo branch");
        }
    }

    /// Each list keeps its newest 500, and stars have their own cap, so a run of stars can't push a
    /// delete out of reach.
    ///
    /// FALSIFIER (L28): drop either cap, or drop the oldest at the wrong end: the first rows. Keep the stack
    /// over 500 when steps come back: the last.
    #[test]
    fn history_caps_each_list_and_stars_do_not_evict_deletes() {
        let dir = "folder";
        let mut files = support::FileHistory::default();
        for seq in 1..=501 {
            files.push_delete(support::DeleteRecord::for_test(dir, "P", 0, Vec::new(), seq, Vec::new()));
            files.push_move(support::MoveRecord { folder: dir.into(), pairs: vec![(PathBuf::from("a"), PathBuf::from("b"))], seq: 1000 + seq, steps: Vec::new(), subject: String::new() });
        }
        assert_eq!((files.deletes.len(), files.moves.len()), (500, 500));
        assert!(!files.has_delete(1) && files.has_delete(2) && files.has_delete(501), "the newest 500");
        assert_eq!((files.moves.first().map(|m| m.seq), files.moves.last().map(|m| m.seq)), (Some(1002), Some(1501)));

        let undo = RefCell::new(UndoRedo::default());
        for seq in 2000..2600 {
            support::push_undo(&undo, seq, 0, 3, 0);
        }
        assert_eq!(undo.borrow().undo.len(), 500, "the stack's own cap");
        assert_eq!(files.deletes.len(), 500, "and no star took a delete's place");
        undo.borrow_mut().undo.clear();
        assert_eq!(support::live_undo_arm(undo.borrow().top_seq(), files.delete_arm(dir), dir, None), Some(support::UndoArm::Delete(501)));

        // A step coming back into a full stack still keeps it to the cap, and goes in by its stamp.
        for seq in 2000..2500 {
            support::push_undo(&undo, seq, 0, 3, 0);
        }
        let back = vec![support::ParkedStep { seq: 2250, name: "ANT".into(), entry: support::ParkedEntry::Cull { rating: 0, mark: 0 } }];
        support::return_parked_steps(&mut undo.borrow_mut(), back, &list(&["ANT"]));
        let stack = undo.borrow();
        assert_eq!(stack.undo.len(), 500);
        assert!(stack.undo.windows(2).all(|w| w[0].seq <= w[1].seq), "still in order");
    }

    /// R8: a photo's undo steps leave with it and come back with it, at their old place in the line, so
    /// star, Del, Ctrl+Z, Ctrl+Z ends unstarred on that photo. The second press used to reach an older
    /// star on another photo.
    ///
    /// FALSIFIER (L28): `park_steps` leaves the steps for the reread to drop: the second press reaches ANT.
    /// `return_parked_steps` puts them on top, or addresses the old place: the rows with newer steps and
    /// shifted photos. Stop parking in an arm, stop returning them, return them before the carry, or stop
    /// a press waiting: the wiring rows.
    #[test]
    fn star_del_undo_undo_ends_unstarred_on_that_photo() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.star("ANT", 2); // an older star, on another photo
        cull.star("BEE", 3);
        cull.turn("BEE");
        cull.delete(&["BEE"]);
        assert_eq!(cull.files.borrow().deletes[0].steps.len(), 2, "both of BEE's steps went with it");
        assert_eq!(cull.stars["BEE"], 3, "and the star itself stays, by name, as it always has");
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.ctrl_z(), "turn BEE", "BEE's own steps, on BEE");
        assert_eq!(cull.ctrl_z(), "star BEE");
        assert_eq!(cull.stars["BEE"], 0, "it ends unstarred");
        assert_eq!(cull.stars["ANT"], 2, "and the older star stays on");
        assert_eq!(cull.ctrl_z(), "star ANT", "then the action before");

        // Photos before it left while it was away, so its old place is another photo's.
        cull.star("DOG", 4);
        cull.delete(&["DOG"]);
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.ctrl_z(), "back DOG");
        assert_eq!(cull.ctrl_z(), "star DOG", "still DOG, in its place now");
        assert_eq!(cull.stars["DOG"], 0);

        // A newer step stays ahead of it: the line is what the person did, in order.
        cull.star("DOG", 1);
        cull.delete(&["DOG"]);
        cull.star("CAT", 5);
        assert_eq!(cull.ctrl_z(), "star CAT");
        assert_eq!(cull.ctrl_z(), "back DOG");
        cull.star("EEL", 2);
        assert_eq!(cull.ctrl_z(), "star EEL", "a star after the return is newer");
        assert_eq!(cull.ctrl_z(), "star DOG");

        // An older row's Recover, with a newer star still on the stack: its step goes back under that star.
        cull.star("DOG", 2);
        cull.delete(&["DOG"]);
        cull.star("CAT", 4);
        let older = cull.files.borrow().deletes[0].seq;
        assert_eq!(cull.recover(older), "back DOG");
        assert_eq!(cull.ctrl_z(), "star CAT", "the newest thing done is still the newest");
        assert_eq!(cull.ctrl_z(), "star DOG");

        // A photo that didn't come back: its steps have nothing to land on.
        cull.star("EEL", 3);
        cull.delete(&["EEL"]);
        cull.io.0.lock().unwrap().gone_from_bin.insert("EEL.png".into());
        assert_eq!(cull.ctrl_z(), "not back");
        assert_eq!(cull.ctrl_z(), "nothing", "the list is the truth");

        // A press while the photo is still on its way waits for it, and only then.
        use support::{undo_waits_for_landing as waits, UndoArm::{Delete, Stack}};
        assert!(waits(Some(Stack(3)), Some(5)), "an older step would be next: it waits");
        assert!(waits(None, Some(5)), "nothing else to reach: it waits");
        assert!(!waits(Some(Delete(9)), Some(5)), "a newer delete comes first");
        assert!(!waits(Some(Stack(3)), None), "nothing on its way");

        let main = main_code();
        let handler = section(&main, "app.on_delete_recycle(move || {", "app.on_recover_delete(move |seq| {");
        assert_eq!(handler.matches("*take_out_dr.borrow_mut() = Some(file_queue::TakeOut {").count(), 2, "both arms go through the one drain");
        assert!(section(&main, "let take_out: Rc<dyn Fn(file_queue::TakeOut)> = {", "\n    };").contains("file_queue::press_delete(&file_history, &undo_stack, seq, press, seats);"), "and the one press parks");
        let queue = queue_code();
        let press = section(&queue, "pub(crate) fn press_delete(", "\n}");
        assert!(press.contains("support::park_steps(&mut undo.borrow_mut(), &leaving)") && press.contains("seq, steps, outcome"), "and the record holds them");
        assert!(section(&queue, "pub(crate) fn press_recover(", "\n}").contains("hist.start_return(seq, rec.steps);"), "a press sets them on their way");
        assert!(section(&queue, "FileDone::Recover { seq, dir, files, recovered, failed, msg, nav_at_press, seats, waited } => {", "FileDone::UndoMove {").contains("send_back(seq)"), "and its report sends them on");
        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        let at = |s: &str| apply.find(s).unwrap_or_else(|| panic!("apply_scan: {s}"));
        assert!(at("carry.restore((&fast, &detail, &film, &roi), &l2, &rot, &undo_stack, (&meta, generation.load(Ordering::Relaxed)));") < at("support::return_parked_steps("), "after the carry, so they address the same list");
        let undo = section(&main, "app.on_undo(move || {", "let entry = undo_u.borrow_mut().undo.pop()");
        assert!(undo.contains("if support::undo_waits_for_landing(arm, returning) {\n            waiting_u.set(waiting_u.get() + 1);\n            return;\n        }"), "a press waits");
        let drain = section(&main, "let accepted = {", "let sort_asked =");
        let (landed, ran) = (drain.find("apply_scan_t(*done);"), drain.find("for _ in 0..undo_waiting_t.take() {\n            app.invoke_undo();"));
        assert!(landed.is_some() && ran.is_some() && landed < ran, "and runs right after the landing:\n{drain}");
        assert!(drain.contains("file_queue::Acceptance::Failed => recover_over(file_queue::no_landing(&file_history_t, &undo_stack_t, &landing_name_t, None)),"), "a folder that couldn't be read holds nothing up");
    }

    /// R8: a group step keeps its other members when one leaves, and takes it back when it returns, so
    /// one more press still takes back what was done together. Rate 12, delete one: that Ctrl+Z used to
    /// be gone for all 12.
    ///
    /// FALSIFIER (L28): `park_steps` removes the whole group: the first row. `return_parked_steps` never
    /// rejoins: the press after the return, and the Move row.
    #[test]
    fn a_group_step_keeps_its_other_members() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        let on_stack = |cull: &Culling| -> Vec<String> {
            cull.undo.borrow().undo.iter().flat_map(|a| match &a.entry {
                UndoEntry::Bulk { edits, .. } => edits.iter().map(|e| cull.shots[e.0].name.clone()).collect(),
                UndoEntry::BulkRot { turns } => turns.iter().map(|t| t.0.clone()).collect(),
                _ => Vec::new(),
            }).collect()
        };
        cull.star_together(&["ANT", "BEE", "CAT"], 4);
        cull.delete(&["BEE"]);
        assert_eq!(on_stack(&cull), ["ANT", "CAT"], "the others stay");
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(on_stack(&cull), ["ANT", "CAT", "BEE"], "and it rejoins");
        assert_eq!(cull.ctrl_z(), "stars ANT BEE CAT", "one press, all three");
        assert!(["ANT", "BEE", "CAT"].iter().all(|p| cull.stars[*p] == 0));

        // Turned together, the same.
        cull.turn_together(&["ANT", "BEE", "CAT"]);
        cull.delete(&["BEE"]);
        assert_eq!(on_stack(&cull), ["ANT", "CAT"]);
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.ctrl_z(), "turns ANT BEE CAT");

        // Two members leave with two deletes, and come back one at a time.
        cull.star_together(&["ANT", "BEE", "CAT"], 2);
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        assert_eq!(on_stack(&cull), ["CAT"]);
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.ctrl_z(), "stars ANT BEE CAT");

        // All of them leave together: the group goes, and returns whole.
        cull.star_together(&["DOG", "EEL"], 5);
        cull.delete(&["DOG", "EEL"]);
        assert!(cull.undo.borrow().undo.is_empty());
        assert_eq!(cull.ctrl_z(), "back DOG EEL");
        assert_eq!(cull.ctrl_z(), "stars DOG EEL");

        // The group left the stack while the photo was away (the stack is another folder's by then, and
        // this one's is new when the person comes back): the photo's step stands alone at its old stamp.
        cull.star_together(&["ANT", "BEE"], 1);
        cull.delete(&["BEE"]);
        cull.undo.borrow_mut().undo.clear();
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.ctrl_z(), "stars BEE");

        // A member leaving by Move rejected: its step goes with it and rejoins at Undo move (R44).
        cull.star_together(&["ANT", "BEE"], 3);
        cull.move_rejected(&["BEE"]);
        assert_eq!(on_stack(&cull), ["ANT"]);
        assert_eq!(cull.ctrl_z(), "moved back BEE");
        assert_eq!(on_stack(&cull), ["ANT", "BEE"]);
        assert_eq!(cull.ctrl_z(), "stars ANT BEE");
    }

    // ── The file queue (Delete round, Stage B). The presses and jobs are the real ones, on `FakeIo`. ──

    /// The record exists from the press, so Ctrl+Z always reaches the delete just made, with its job still
    /// queued, and each press takes one step back with no refusal. A star on another photo is older, so it
    /// waits its turn. Nothing lights a Recover for a record already taken (R9).
    ///
    /// FALSIFIER (L28): `press_delete` leaves the record for the fold: the first arm is the star. FIFO the
    /// wrong way round puts a Recover before its delete: "silent" for CAT.
    #[test]
    fn ctrl_z_right_after_del_reaches_that_delete() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.star("EEL", 3);
        let held = held_lock(&cull);
        cull.press_del(&["ANT"]);
        let Some(support::UndoArm::Delete(ant)) = cull.arm() else { panic!("the delete, not the star") };
        assert_eq!(cull.record_name(ant), "ANT");
        cull.press_del(&["BEE"]);
        cull.press_del(&["CAT"]);
        for photo in ["CAT", "BEE", "ANT"] {
            let Some(support::UndoArm::Delete(seq)) = cull.arm() else { panic!("{photo}'s delete") };
            assert_eq!(cull.record_name(seq), photo, "each press reaches the one before");
            assert_eq!(cull.press_z(), "job", "and is never refused");
        }
        assert_eq!(cull.arm(), Some(support::UndoArm::Stack(1)), "then the star");
        drop(held);
        cull.settle();
        assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG", "EEL"], "all three are back");
        let rows = cull.rows_so_far();
        assert!(rows.iter().all(|(_, _, pill)| !pill), "nothing offers a Recover that was already taken: {rows:?}");
        assert!(rows.iter().all(|(text, _, _)| !text.starts_with("Sent")), "and no Sent for what came back: {rows:?}");
        assert!(rows.is_empty(), "and a clean Recover pushes no row of its own (R36): {rows:?}");
        assert_eq!(cull.ctrl_z(), "star EEL");
    }

    /// A delete that sent nothing has already said so in red. A Recover pressed before that result says
    /// nothing more, and what it took off the stack comes back: Stars, flags and turns are never lost.
    /// R15: the same when the delete's job crashed and set no outcome.
    ///
    /// FALSIFIER (L28): `run_recover` restores whatever the outcome holds: "Nothing to recover", or
    /// no longer silent. `settle_delete` forgets the steps: the star's press says "nothing".
    /// `send_back` forgets them: the second row's star.
    #[test]
    fn recover_after_a_failed_delete_stays_silent() {
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.star("ANT", 2);
        cull.lock_file("ANT.png");
        let held = held_lock(&cull);
        cull.press_del(&["ANT"]);
        assert_eq!(cull.press_z(), "job");
        drop(held);
        assert_eq!(cull.settle(), "silent");
        assert_eq!(cull.names(), ["ANT", "BEE"], "the file never left");
        let rows = cull.rows_so_far();
        assert_eq!(rows.len(), 1, "the delete's red line is all that speaks: {rows:?}");
        assert_eq!((rows[0].1, rows[0].2), (3, false), "red, with no pill");
        assert_eq!(cull.ctrl_z(), "star ANT", "its star comes off, on that photo");
        assert_eq!(cull.stars["ANT"], 0);

        // Again, with no Ctrl+Z before the result: the record goes, and the star stays reachable.
        cull.star("ANT", 4);
        cull.delete(&["ANT"]);
        assert!(cull.files.borrow().deletes.is_empty(), "nothing can come back, so nothing offers it");
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!(cull.stars["ANT"], 0);

        // A Recover whose delete never ran its course: R15, unset is nothing sent.
        let outcome = Arc::new(std::sync::OnceLock::new());
        let waited = file_queue::Waited { seq: 9, pressed: Instant::now(), text: "Recovering ANT…".to_string() };
        let job = file_queue::FileJob::Recover { seq: 9, dir: "folder".into(), name: "ANT".into(), plural: false, outcome, not_in_bin: Vec::new(), nav_at_press: 0, seats: Vec::new(), waited };
        let done = file_queue::run_job(job, &cull.io);
        assert!(matches!(done, file_queue::FileDone::Recover { recovered: 0, msg: None, .. }));
    }

    /// R29, through the real presses, jobs and fold: a delete says the owner's approved words, by photo. One
    /// photo, a pair, a selection, part of each. And once it's back, the same subject.
    ///
    /// FALSIFIER (L28): `run_delete` reads files, not photos: every row. `photo_subject` drops the label: the pair
    /// row. `press_delete` files no subject: the record's. `settle_delete` keeps a pair's label on what's half: the half row.
    #[cfg(windows)]
    #[test]
    fn a_delete_says_the_approved_words_by_photo() {
        let mut cull = Culling::with_files(&["ANT.png", "BEE.png", "CAT.png", "DOG.png", "EEL.png", "IMG_7.CR3", "IMG_7.png", "IMG_8.CR3", "IMG_8.png"]);
        let said = |text: &str, kind, pill| [(text.to_string(), kind, pill)];
        cull.delete(&["ANT"]);
        assert_eq!(cull.rows_so_far(), said("Sent ANT to the Recycle Bin", 1, true));
        // From the press: Ctrl+Z can reach it with its job still queued, and "Recovering…" reads this.
        let held = held_lock(&cull);
        cull.press_del(&["IMG_7"]);
        assert_eq!(cull.files.borrow().deletes.last().unwrap().shown_name(), "IMG_7 [RAW+PNG]", "what Ctrl+Z's own sentences read");
        drop(held);
        cull.settle();
        assert_eq!(cull.rows_so_far(), said("Sent IMG_7 [RAW+PNG] to the Recycle Bin", 1, true));
        assert_eq!(cull.files.borrow().deletes.last().unwrap().shown_name(), "IMG_7 [RAW+PNG]", "and after the job");
        cull.delete(&["BEE", "CAT", "DOG"]);
        assert_eq!(cull.rows_so_far(), said("Sent 3 selected photos to the Recycle Bin", 1, true));
        assert_eq!(cull.files.borrow().deletes.last().unwrap().shown_name(), "3 photos");
        // A selection of two with a pair in it counts photos, not files, and one that stayed.
        cull.lock_file("EEL.png");
        cull.delete(&["EEL", "IMG_8"]);
        assert_eq!(cull.rows_so_far(), said("Sent 1 of 2 photos to the Recycle Bin. 1 is still in the folder.", 3, true));

        // Half a pair.
        let mut cull = Culling::with_files(&["IMG_7.CR3", "IMG_7.png", "IMG_8.png"]);
        cull.lock_file("IMG_7.png");
        cull.delete(&["IMG_7"]);
        assert_eq!(cull.rows_so_far(), said("Sent the RAW of IMG_7 to the Recycle Bin. The PNG is still in the folder.", 3, true));
        assert_eq!(cull.files.borrow().deletes[0].shown_name(), "IMG_7", "what's in the bin isn't the pair");
    }

    /// R47, found clicking Stage B: "Del on a photo another program holds open shows the raw system error today."
    /// Plain words, and the system's error stays in the log. The photo returns to its place, and nothing offers it
    /// back. Named photo, a pair and a group, each pressed twice.
    ///
    /// FALSIFIER (L28): `run_delete` says the crate's words again: each row, and the source row. `press_text` reads
    /// `Stayed` as a partial: its words. A pair's named without its label: its row. `recycle_shot_files` stops
    /// logging the error: the source row.
    #[test]
    fn a_photo_another_program_holds_says_so_in_plain_words() {
        let mut cull = Culling::with_files(&["ANT.png", "BEE.png", "CAT.png", "DOG.png", "IMG_7.CR3", "IMG_7.png", "ZED.png"]);
        let said = |text: &str| [(text.to_string(), 3, false)];
        cull.lock_file("ANT.png");
        for _ in 0..2 {
            cull.on = "ANT".to_string();
            cull.delete(&["ANT"]);
            assert_eq!(cull.rows_so_far(), said("Couldn't delete ANT, another program may have it open. It's still in the folder."));
            assert!(cull.names().contains(&"ANT"), "it's back in its place");
            assert_eq!(cull.on, "BEE", "and you stay on the next");
        }
        cull.lock_file("IMG_7.CR3");
        cull.lock_file("IMG_7.png");
        for _ in 0..2 {
            cull.delete(&["IMG_7"]);
            assert_eq!(cull.rows_so_far(), said("Couldn't delete IMG_7 [RAW+PNG], another program may have it open. It's still in the folder."));
            assert!(cull.names().contains(&"IMG_7"));
        }
        for held in ["BEE.png", "CAT.png", "DOG.png"] {
            cull.lock_file(held);
        }
        for _ in 0..2 {
            cull.delete(&["BEE", "CAT", "DOG"]);
            assert_eq!(cull.rows_so_far(), said("Couldn't delete 3 photos, another program may have them open. They're still in the folder."));
            assert!(cull.files.borrow().deletes.is_empty(), "nothing can come back, so nothing offers it");
        }
        // The system's own words stay in the log, and only there.
        let support = include_str!("support.rs");
        let recycle = section(support, "pub(crate) fn recycle_shot_files(paths: &[PathBuf]) -> (usize, usize, Vec<PathBuf>) {", "\n}");
        assert!(recycle.contains("delete-recycle: Recycle Bin op FAILED ({e})"), "the log keeps it");
        assert!(!recycle.contains("tr_format!") && !recycle.contains("i18n::tr"), "and the function no longer speaks:\n{recycle}");
        assert!(!include_str!("file_queue.rs").contains(concat!("folder ", "re-checked\"")), "no leftover");
    }

    /// R36: "A clean Ctrl+Z raises no card when the photo lands on screen." The photo is the word. No row, because
    /// the bell list is what's still gone. Three in a row, at a pace and as fast as you can. In Compare, a photo that
    /// goes back in its half is on screen. The card's words are from `recover_over`'s own question.
    ///
    /// FALSIFIER (L28): the fold pushes its row: the rows. `recovered_on_screen` answers no: the card, each press.
    /// `Named.clean` isn't filled: `a_ctrl_z_from_another_photo_names_it_on_the_short_card`.
    #[test]
    fn a_clean_ctrl_z_says_nothing_when_the_photo_lands_on_screen() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        cull.delete(&["CAT"]);
        assert_eq!(cull.rows_so_far().len(), 3);
        for back in ["CAT", "BEE", "ANT"] {
            assert_eq!(cull.ctrl_z(), format!("back {back}"));
            assert_eq!(cull.on, back, "on screen");
            assert_eq!(cull.rows_so_far(), [], "no row for {back}");
            assert_eq!(cull.recovered_card, None, "and no card for {back}");
        }
        // As fast as you can: three presses inside one listing, and you end on the oldest.
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        cull.delete(&["CAT"]);
        cull.rows_so_far();
        for _ in 0..3 {
            assert_eq!(cull.press_z(), "job");
        }
        cull.settle();
        assert_eq!(cull.on, "ANT");
        assert_eq!(cull.rows_so_far(), []);
        assert_eq!(cull.recovered_card, None, "the photo you pressed for last is on screen");
        // In Compare, back in its half.
        cull.open_compare("ANT", "BEE", 0);
        cull.delete(&["ANT"]);
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.halves(), ("ANT", "BEE"), "back in A");
        assert_eq!(cull.recovered_card, None, "where it can be seen");
    }

    /// R46: N000 to N079, a folder of numbered photos, less `gone`, on `on`.
    fn numbered(on: &str, gone: &[&str]) -> Culling {
        let names: Vec<String> = (0..80).map(|i| format!("N{i:03}")).filter(|n| !gone.contains(&n.as_str())).collect();
        let mut cull = Culling::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
        cull.on = on.to_string();
        cull
    }

    /// R46: half a 1200 px strip, in cells.
    const STRIP_HALF: f32 = 1200.0 / 256.0;

    /// R46 (owner: "if the user already scrolled past the recovered image and it is off the visible filmstrip, the edge
    /// of the filmstrip (left or right depending on where it is) should lit up in a blue gradient"), where the person
    /// meets it first (B-CLICKS row 32): on N041, Move rejected of N040, browse on to N060, Ctrl+Z. Undo move never
    /// moves you, so N040 comes back far off the strip and the strip's left edge points at it. Twice (L53). Moving on
    /// ends it. Then on N059, N075 back: the right edge. Then on N041 itself, where N040 lands beside the photo on
    /// screen and lights nothing. Through the real presses, jobs, fold and landing.
    ///
    /// FALSIFIER (L28): the Undo move's report hands nothing (`back` empty in `run_undo_move`): every row. The fold
    /// leaves it out (no `brought_back.extend(back)`): the same.
    #[test]
    fn undo_move_far_from_the_photo_points_the_strip_edge_at_it() {
        let mut cull = numbered("N041", &[]);
        for _ in 0..2 {
            cull.arrow_to("N041");
            cull.move_rejected(&["N040"]);
            cull.arrow_to("N060");
            assert_eq!(cull.ctrl_z(), "moved back N040");
            assert_eq!(cull.on, "N060", "you stay on 60");
            assert_eq!(cull.edge_photos(), ["N040"]);
            assert_eq!(cull.strip_edges(STRIP_HALF), (Some("N040".to_string()), None), "the left edge, going to N040");
        }
        cull.arrow_to("N059");
        assert_eq!(cull.strip_edges(STRIP_HALF), (None, None), "moving on ends it");
        cull.move_rejected(&["N075"]);
        assert_eq!(cull.ctrl_z(), "moved back N075");
        assert_eq!(cull.strip_edges(STRIP_HALF), (None, Some("N075".to_string())), "the right edge, going to N075");
        cull.arrow_to("N041");
        cull.move_rejected(&["N040"]);
        assert_eq!(cull.ctrl_z(), "moved back N040");
        assert_eq!(cull.on, "N041");
        assert_eq!(cull.strip_edges(STRIP_HALF), (None, None), "N040 lands beside the photo on screen");
    }

    /// R46 for a Ctrl+Z on a delete, which shows the photo that came back unless the person moved on before it landed
    /// (R2). On N040: Del, then Ctrl+Z: it lands on N040, so no edge. Del again, Ctrl+Z, and on to N060 before it
    /// lands: N040 comes back off the strip and the left edge points at it. Compare (B-CLICKS row 17): N040 and N041, Del N040 (A takes N042),
    /// browse A on to N060, Ctrl+Z: N040 goes back to the strip, A stays on N060, and the edge points at it; without
    /// the browse it goes back into A and needs no edge. Each twice (L53).
    ///
    /// FALSIFIER (L28): the Recover's fold hands nothing (no `brought_back.extend`): the browse rows. The landing
    /// lights though the stage shows the photo (the `stage.shows` term dropped): the at-once rows.
    #[test]
    fn a_ctrl_z_after_moving_on_points_the_strip_edge_at_the_photo() {
        let mut cull = numbered("N040", &[]);
        for _ in 0..2 {
            cull.arrow_to("N040");
            cull.delete(&["N040"]);
            cull.rows_so_far();
            assert_eq!(cull.ctrl_z(), "back N040");
            assert_eq!(cull.on, "N040", "it shows the photo that came back");
            assert_eq!(cull.edge_photos(), Vec::<&str>::new(), "so no edge");
            cull.delete(&["N040"]);
            cull.rows_so_far();
            assert_eq!(cull.press_z(), "job");
            cull.arrow_to("N060"); // before it lands (R2: a step before the press doesn't count)
            assert_eq!(cull.settle(), "back N040");
            assert_eq!(cull.on, "N060");
            assert_eq!(cull.strip_edges(STRIP_HALF), (Some("N040".to_string()), None));
        }
        for _ in 0..2 {
            cull.open_compare("N040", "N041", 0);
            cull.delete(&["N040"]);
            cull.rows_so_far();
            assert_eq!(cull.halves(), ("N042", "N041"));
            assert_eq!(cull.ctrl_z(), "back N040");
            assert_eq!(cull.halves(), ("N040", "N041"), "back in A");
            assert_eq!(cull.edge_photos(), Vec::<&str>::new(), "so no edge");
            cull.delete(&["N040"]);
            cull.rows_so_far();
            cull.browse_half(0, "N060");
            assert_eq!(cull.ctrl_z(), "back N040");
            assert_eq!(cull.halves(), ("N060", "N041"), "A stays where the person took it");
            assert_eq!(cull.strip_edges(STRIP_HALF), (Some("N040".to_string()), None), "the strip follows A, and N040 is off it");
            cull.compare = None;
        }
    }

    /// R46: the edge is for a photo the person brought back. Another program saves N030 while the person is on N060,
    /// and a reread lists it (the refresh on coming back will bring such a photo): nothing points at it. Then N040,
    /// deleted, browsed away from and brought back by Ctrl+Z, is the only photo the edge is about.
    ///
    /// FALSIFIER (L28): light every photo a landing adds (the landing's arrivals in place of `brought`): the first row.
    #[test]
    fn a_photo_saved_by_another_program_points_no_edge_at_it() {
        let mut cull = numbered("N060", &["N030"]);
        cull.save_beside("N030.png");
        cull.list_now();
        assert_eq!(cull.accept(false), "landed");
        assert!(cull.names().contains(&"N030"), "premise: it arrived");
        assert_eq!(cull.edge_photos(), Vec::<&str>::new());
        assert_eq!(cull.strip_edges(STRIP_HALF), (None, None));
        cull.arrow_to("N040");
        cull.delete(&["N040"]);
        cull.rows_so_far();
        assert_eq!(cull.press_z(), "job");
        cull.arrow_to("N060");
        assert_eq!(cull.settle(), "back N040");
        assert_eq!(cull.edge_photos(), ["N040"]);
    }

    /// R36: "a short card "Recovered {photo}" if the person had moved." The person pressed Ctrl+Z and went on before
    /// it landed, or in Compare the half has moved on or there was never a half to go back to: it goes back to the
    /// strip, which alone doesn't say so. It reads with the pair's label, and half a pair without it.
    ///
    /// FALSIFIER (L28): the fold drops the words (`clean: None`): the first row. `Named` keeps an older press's
    /// words over a newer clean press's: the row of two. It speaks in Compare as if `current` were all there is: the half rows.
    #[test]
    fn a_ctrl_z_from_another_photo_names_it_on_the_short_card() {
        let mut cull = Culling::with_files(&["ANT.png", "BEE.CR3", "BEE.png", "CAT.png", "DOG.png", "EEL.png", "FOX.png"]);
        let card = |cull: &mut Culling| cull.recovered_card.take();
        // A step taken after the press, twice over.
        for _ in 0..2 {
            cull.on = "BEE".to_string();
            cull.delete(&["BEE"]);
            cull.rows_so_far();
            assert_eq!(cull.press_z(), "job");
            cull.arrow_to("DOG");
            cull.settle();
            assert_eq!(cull.on, "DOG", "you stay where you went");
            assert_eq!(cull.rows_so_far(), [], "no row");
            assert_eq!(card(&mut cull), Some("Recovered BEE [RAW+PNG]".to_string()));
        }
        // Two presses in one listing and a step: the newest press names it.
        cull.on = "ANT".to_string();
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        cull.rows_so_far();
        assert_eq!(cull.press_z(), "job");
        assert_eq!(cull.press_z(), "job");
        cull.arrow_to("FOX");
        cull.settle();
        assert_eq!(card(&mut cull), Some("Recovered ANT".to_string()));
        // A selection, the same.
        cull.delete(&["ANT", "CAT"]);
        cull.rows_so_far();
        assert_eq!(cull.press_z(), "job");
        cull.arrow_to("EEL");
        cull.settle();
        assert_eq!(card(&mut cull), Some(format!("Recovered {}", support::deleted_subject("", 2))));

        // Compare: deleted before it opened, so there's no half to go back to.
        cull.on = "ANT".to_string();
        cull.delete(&["ANT"]);
        cull.rows_so_far();
        cull.open_compare("CAT", "DOG", 0);
        assert_eq!(cull.ctrl_z(), "back ANT");
        assert_eq!(cull.halves(), ("CAT", "DOG"), "the two photos stay put");
        assert_eq!(card(&mut cull), Some("Recovered ANT".to_string()), "it only went back to the strip");
        // Its half has moved on.
        cull.delete(&["CAT"]);
        cull.rows_so_far();
        assert_eq!(cull.halves().0, "EEL", "premise: EEL took its place");
        cull.browse_half(0, "FOX");
        assert_eq!(cull.ctrl_z(), "back CAT");
        assert_eq!(cull.halves().0, "FOX", "you stay where you went");
        assert_eq!(card(&mut cull), Some("Recovered CAT".to_string()));
    }

    /// R36: "a partial recover keeps an amber row", and nothing back keeps a red one. The owner's flow: a new file of
    /// that name saved beside the original, then Ctrl+Z. It speaks in its row, so its job's report ends the wait,
    /// and there's no short card on top.
    ///
    /// FALSIFIER (L28): `failed` isn't read: the row reads as clean and goes, with the message's card: the first row.
    /// Severity stays success: its kind. A clean Recover keeps a row: the first row of the other test.
    #[test]
    fn a_partial_recover_keeps_an_amber_row() {
        let mut cull = Culling::with_files(&["IMG_7.CR3", "IMG_7.png", "IMG_8.png", "IMG_9.png"]);
        cull.delete(&["IMG_7"]);
        cull.rows_so_far();
        cull.io.0.lock().unwrap().occupied.insert("IMG_7.png".to_string());
        assert_eq!(cull.press_z(), "job");
        cull.arrow_to("IMG_9");
        cull.settle();
        assert_eq!(cull.rows_so_far(), [("Recovered 1/2 file(s) for IMG_7 [RAW+PNG]".to_string(), 2, false)], "amber, and it stays");
        assert_eq!(cull.recovered_card, None, "and no short card as well");
        // Nothing back: red.
        cull.delete(&["IMG_8"]);
        cull.rows_so_far();
        cull.io.0.lock().unwrap().gone_from_bin.insert("IMG_8.png".to_string());
        cull.ctrl_z();
        assert_eq!(cull.rows_so_far(), [("IMG_8 is no longer in the bin".to_string(), 3, false)]);
        assert_eq!(cull.recovered_card, None);
    }

    /// The owner's approved words for Undo move: "Brought back 30 photos", "Brought back IMG_1234 [RAW+JPG]". It keeps
    /// its card: a moved photo doesn't land on screen (R1).
    ///
    /// FALSIFIER (L28): `press_undo_move` drops the subject: the old words, each row. The fold's subject counts files: the plural.
    #[test]
    fn undo_move_says_what_it_brought_back() {
        let mut cull = Culling::with_files(&["ANT.png", "BEE.png", "CAT.png", "IMG_7.CR3", "IMG_7.png", "ZED.png"]);
        cull.arrow_to("ZED");
        cull.move_rejected(&["IMG_7"]);
        cull.ctrl_z();
        assert_eq!(cull.rows_so_far(), [("Brought back IMG_7 [RAW+PNG]".to_string(), 1, false)]);
        cull.move_rejected(&["ANT"]);
        cull.ctrl_z();
        assert_eq!(cull.rows_so_far(), [("Brought back ANT".to_string(), 1, false)]);
        cull.move_rejected(&["ANT", "BEE", "IMG_7"]);
        cull.ctrl_z();
        assert_eq!(cull.rows_so_far(), [(format!("Brought back {}", support::deleted_subject("", 3)), 1, false)]);
        assert_eq!(cull.on, "ZED", "and you stay where you are, each time");
    }

    /// R45's and R36's wiring: the reread's landing notes a returned photo's preview, and asks what the Recover has left
    /// to say, after the landing, from the name it took. An unnamed landing writes no preview.
    ///
    /// FALSIFIER (L28): the Land arm doesn't write the slot, or writes it for any landing: the source rows here. The
    /// order: a read of the list before `apply_scan` for `now_on`.
    #[test]
    fn a_returned_photos_landing_is_wired() {
        let main = include_str!("main.rs");
        let arm = section(main, "file_queue::Acceptance::Land(mut done, warn) => {", "// R8: a Ctrl+Z pressed while");
        let order = [
            "let named = landing_name_t.borrow_mut().take();",
            "let was_on = photo_on(",
            "apply_scan_t(*done);",
            "if named.is_some() {",
            "tick::note_preview_landing(&preview_landing_t, was_on, photo_on(",
            "recover_over(named);",
        ];
        let at: Vec<usize> = order.iter().map(|s| arm.find(s).unwrap_or_else(|| panic!("the arm: {s}"))).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "the arm's order: {at:?}\n{arm}");
        let tick = main.split_once("let due = file_queue::working_due(").expect("R17's question").1;
        assert!(tick.starts_with("&file_queue_t, landing_name_t.borrow().as_ref(), now, said);"), "the name's wait counts too");
    }

    /// R15: a job that crashes doesn't strand the queue. The next jobs run on a new thread, the queue goes
    /// quiet, the crash says so, and a Recover queued behind it finds nothing sent and stays silent. The
    /// photo is still on disk, so the reread keeps it, with its star's step.
    ///
    /// FALSIFIER (L28): the guard doesn't respawn: CAT's job never runs and the queue never goes quiet. The
    /// guard doesn't report: no red row, and the queue never goes quiet. The fold keeps the record: the
    /// arm is its delete.
    #[test]
    fn a_job_that_crashes_leaves_the_queue_working() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("BEE", 2);
        cull.io.0.lock().unwrap().crashes_on = Some("BEE".into());
        let held = held_lock(&cull);
        cull.press_del(&["BEE"]);
        assert_eq!(cull.press_z(), "job", "a Recover behind it");
        cull.press_del(&["CAT"]);
        drop(held);
        cull.settle();
        assert_eq!(cull.names(), ["ANT", "BEE", "DOG"], "BEE never left, and CAT's job still ran");
        let rows = cull.rows_so_far();
        assert_eq!(rows.iter().filter(|(_, kind, _)| *kind == 3).count(), 1, "the crash says so, once: {rows:?}");
        assert!(rows.iter().any(|(text, kind, pill)| text.starts_with("Sent") && *kind == 1 && *pill), "CAT's delete offers its Recover: {rows:?}");
        assert!(!rows.iter().any(|(text, _, _)| text.starts_with("Recovered") || text.contains("bin")), "the Recover says nothing: {rows:?}");
        assert_eq!(cull.ctrl_z(), "back CAT");
        assert_eq!(cull.ctrl_z(), "star BEE", "its star's step came back with the reread");

        // And with no Recover behind it: the record goes, the step comes back.
        cull.star("BEE", 3);
        cull.delete(&["BEE"]);
        assert!(cull.files.borrow().deletes.is_empty(), "a crashed delete offers nothing");
        assert_eq!(cull.ctrl_z(), "star BEE");
    }

    /// F-P11: the record holds what really left, and the gate is that something did, never `fail == 0`. One
    /// file of a pair locked: Recover is offered for the RAW, the row is red, and the photo returns to its
    /// place as the JPEG, with its steps kept for its return. The person went on to the next photo at the
    /// press, and stays there. A selection counts the photos that left, and a photo that never left has
    /// its steps back straight away.
    ///
    /// FALSIFIER (L28): gate `can_recover` on `fail == 0`: no record, no pill. Record the attempted files:
    /// the file list. Keep the attempted count in `settle_delete`: "3 photos". Leave a photo that stayed
    /// in the record's steps: the star row. Count a photo as left only when every file went: the pair row.
    #[test]
    fn partial_pair_records_only_what_left() {
        let mut cull = Culling::with_files(&["IMG_6.png", "IMG_7.CR3", "IMG_7.png", "IMG_8.png"]);
        cull.arrow_to("IMG_7");
        cull.star("IMG_7", 3);
        cull.lock_file("IMG_7.png");
        cull.delete(&["IMG_7"]);
        let seq = {
            let files = cull.files.borrow();
            assert_eq!(files.deletes.len(), 1, "a record");
            assert_eq!(files.deletes[0].files(), [PathBuf::from("IMG_7.CR3")], "of the file that left");
            assert_eq!(files.deletes[0].steps.len(), 1, "and it holds the photo's star");
            files.deletes[0].seq
        };
        let rows = cull.rows_so_far();
        assert_eq!(rows, [(platform::sent_part_text("RAW", "IMG_7", "PNG"), 3, true)], "red, and with Recover");
        assert_eq!(cull.on, "IMG_8", "the next photo, from the press, and the half that's left returns to its place");
        assert_eq!(cull.names(), ["IMG_6", "IMG_7", "IMG_8"]);
        assert!(cull.shots[cull.place("IMG_7")].raw.is_none());
        assert_eq!(cull.recover(seq), "back IMG_7");
        assert!(cull.shots[cull.place("IMG_7")].raw.is_some(), "the pair re-forms");
        assert_eq!(cull.ctrl_z(), "star IMG_7");

        // A selection of three with one locked: two left, and the third keeps its star on the stack.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("ANT", 1);
        cull.star("BEE", 2);
        cull.lock_file("BEE.png");
        cull.delete(&["ANT", "BEE", "CAT"]);
        {
            let files = cull.files.borrow();
            let rec = &files.deletes[0];
            assert_eq!((rec.photos, rec.name.as_str()), (2, "2 photos"), "the subset, never the batch");
            assert_eq!(rec.files(), [PathBuf::from("ANT.png"), PathBuf::from("CAT.png")]);
            assert_eq!(rec.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["ANT"]);
        }
        assert_eq!(cull.names(), ["BEE", "DOG"]);
        assert_eq!(cull.ctrl_z(), "back ANT CAT", "the record is the newest act");
        assert_eq!(cull.ctrl_z(), "star BEE", "BEE's star, at its old place in the line");
        assert_eq!(cull.ctrl_z(), "star ANT");
    }

    /// R44. X, X, Move rejected, Ctrl+Z, Ctrl+Z: the first brings the photos back, the second takes off
    /// the last X, and so on, one by one, then the previous action. Owner: "yes fix it". The second press
    /// used to reach whatever came before, because the reread dropped a moved photo's steps. Twice over.
    ///
    /// FALSIFIER (L28): `fold_move` parks nothing: the second press reaches the older star. `press_undo_move`
    /// drops the steps, or the fold doesn't send them back: the same. The fold comes after the reread, or a
    /// request doesn't fold first: the wiring rows.
    #[test]
    fn x_x_move_undo_undo_takes_off_the_last_x() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("DOG", 5); // the previous action, on another photo
        for _ in 0..2 {
            cull.star("BEE", -1); // X, X: named as stars, the stack can't tell them apart
            cull.star("CAT", -1);
            cull.move_rejected(&["BEE", "CAT"]);
            assert_eq!(cull.names(), ["ANT", "DOG"]);
            assert_eq!(cull.files.borrow().moves[0].steps.len(), 2, "both X's went with their photos");
            assert_eq!(cull.ctrl_z(), "moved back BEE CAT", "the first brings them back");
            assert_eq!(cull.names(), ["ANT", "BEE", "CAT", "DOG"]);
            assert_eq!(cull.ctrl_z(), "star CAT", "the second takes off the last X");
            assert_eq!(cull.ctrl_z(), "star BEE", "then the one before");
            assert_eq!(cull.stars["DOG"], 5, "and the older star is still on");
            cull.stars.remove("BEE");
            cull.stars.remove("CAT");
        }
        assert_eq!(cull.ctrl_z(), "star DOG", "then the previous action");

        // At speed: the second press comes while the first's job is still on the queue, and waits for
        // the photos rather than reaching the older star.
        cull.star("DOG", 5);
        cull.star("BEE", -1);
        cull.move_rejected(&["BEE"]);
        let held = held_lock(&cull);
        assert_eq!(cull.press_z(), "job");
        assert_eq!(cull.press_z(), "waits", "not the star on DOG");
        drop(held);
        assert_eq!(cull.settle(), "star BEE", "the press runs right after the landing");
        assert_eq!(cull.stars["DOG"], 5);

        let main = main_code();
        let at = |code: &str, s: &str| main.find(code).unwrap_or_else(|| panic!("{s}: {code}"));
        assert!(at("let fold_move = || {", "the fold") < at("if let Some(p) = reload_req_t.try_lock().ok()", "the drain"), "the fold is defined ahead of the drain");
        let drain = section(&main, "if let Some(p) = reload_req_t.try_lock().ok().and_then(|mut g| g.take()) {", "reread_t.borrow_mut().want(p, true);");
        assert!(drain.contains("fold_move();"), "and a request folds first, while the list still holds the moved photos");
        let fold = section(&main, "let fold_move = || {", "fold_move();");
        assert!(fold.contains("support::park_steps(&mut undo_stack_t.borrow_mut(), &support::photos_in_pairs(&on_screen, &pairs));"), "from the pairs' names");
        let moving = section(&main, "app.on_move_rejects(move || {", "app.on_undo_move(move |seq| {");
        let (pairs, request) = (moving.find("*mv_slot.lock()").expect("its pairs"), moving.find("*rl.lock()").expect("its request"));
        assert!(pairs < request, "the worker posts its pairs before its request");
        assert!(section(&queue_code(), "pub(crate) fn press_undo_move(", "\n}").contains("hist.start_return(seq, rec.steps);"), "and an Undo move sets them on their way");
    }

    /// R44: the same for stars and turns on a moved photo, and for what was done together. Moved photos
    /// come back in any order with an older move still there, each step at its old place in the line, by
    /// name, whatever else has left or come back meanwhile. A photo that half moved keeps its steps too.
    ///
    /// FALSIFIER (L28): `return_parked_steps` puts them on top: the out-of-order row. `photos_in_pairs`
    /// reads only the finished file, or matches nothing: the pair row. `drop_moves_in` keeps the steps
    /// alive: nothing to see, since the record goes.
    #[test]
    fn moved_photos_come_back_in_any_order_with_an_older_move_still_there() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        // A star and a turn on ANT, a star on CAT, each with its own move.
        cull.star("ANT", 2);
        cull.turn("ANT");
        cull.move_rejected(&["ANT"]);
        cull.star("CAT", 3);
        cull.move_rejected(&["CAT"]);
        cull.star("EEL", 1);
        assert_eq!(cull.names(), ["BEE", "DOG", "EEL"]);
        let older = cull.files.borrow().moves[0].seq;
        // The older row's Undo first: ANT comes back under the newer move and the newer star.
        assert_eq!(cull.press_undo_move(older), "job");
        assert_eq!(cull.settle(), "moved back ANT");
        assert_eq!(cull.ctrl_z(), "star EEL", "the newest thing done is still the newest");
        assert_eq!(cull.ctrl_z(), "moved back CAT", "then the newer move");
        assert_eq!(cull.ctrl_z(), "star CAT", "then its star, on CAT in its place now");
        assert_eq!(cull.ctrl_z(), "turn ANT", "then ANT's turn and star, in the order they were done");
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!(cull.ctrl_z(), "nothing");

        // The other way round: the newest first, with photos before them gone and back.
        cull.star("BEE", 4);
        cull.move_rejected(&["BEE"]);
        cull.star("DOG", 4);
        cull.move_rejected(&["ANT", "DOG"]);
        assert_eq!(cull.ctrl_z(), "moved back ANT DOG");
        assert_eq!(cull.ctrl_z(), "star DOG", "still DOG");
        assert_eq!(cull.ctrl_z(), "moved back BEE");
        assert_eq!(cull.ctrl_z(), "star BEE");

        // Rated together, moved in two moves, each back by its own: the group re-forms, however they return.
        cull.star_together(&["ANT", "BEE", "CAT"], 2);
        cull.move_rejected(&["ANT"]);
        cull.move_rejected(&["BEE", "CAT"]);
        let (first, second) = (cull.files.borrow().moves[0].seq, cull.files.borrow().moves[1].seq);
        cull.press_undo_move(first);
        cull.settle();
        cull.press_undo_move(second);
        cull.settle();
        assert_eq!(cull.ctrl_z(), "stars ANT BEE CAT", "one press, all three");

        // A pair: its RAW moved and its JPEG stayed. Half gone counts, and Undo move brings the step back.
        let mut cull = Culling::with_files(&["IMG_6.png", "IMG_7.CR3", "IMG_7.png"]);
        cull.star("IMG_7", 3);
        let raw_only = vec![(PathBuf::from("IMG_7.CR3"), PathBuf::from("Rejected/IMG_7.CR3"))];
        assert_eq!(support::photos_in_pairs(&cull.shots, &raw_only), [(1, "IMG_7")]);
        cull.move_rejected(&["IMG_7"]);
        assert_eq!(cull.ctrl_z(), "moved back IMG_7 IMG_7");
        assert_eq!(cull.ctrl_z(), "star IMG_7");

        // Nothing of a move's steps survives Empty: nothing can come back.
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.star("ANT", 1);
        cull.move_rejected(&["ANT"]);
        cull.files.borrow_mut().drop_moves_in("folder");
        assert_eq!(cull.ctrl_z(), "nothing");
    }

    // ── The take-out (Delete round, Stage B): the photo leaves the list at the press, and the file goes
    //    behind it. Commit 5 of the round's plan. ──

    /// A delete's next photo is there at the press, with the disk not yet touched, press after press:
    /// the list really is shorter, so the arrows work too. The job's reread then changes nothing. On the
    /// last photo the one before it, and on the only one the empty folder. A menu delete of another
    /// photo, or a selection without yours, leaves you on your photo.
    ///
    /// FALSIFIER (L28): `take_out_scan` keeps the photo (the filter's `!`): the first row. It takes a photo
    /// that isn't a target: the second. The handler's stash goes to the job directly, or the drain moves
    /// after the fold: `the_handler_stashes_and_the_tick_takes_it_out`.
    #[test]
    fn the_next_photo_is_on_screen_with_its_delete_still_queued() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        let on_disk = || ["ANT", "BEE", "CAT", "DOG", "EEL"].map(|n| format!("{n}.png")).into_iter().collect::<BTreeSet<_>>();
        let held = held_lock(&cull);
        for (photo, next, left) in [("ANT", "BEE", vec!["BEE", "CAT", "DOG", "EEL"]), ("BEE", "CAT", vec!["CAT", "DOG", "EEL"]), ("CAT", "DOG", vec!["DOG", "EEL"])] {
            assert_eq!(cull.on, photo);
            cull.press_del(&[photo]);
            assert_eq!(cull.on, next, "the next photo, at the press");
            assert_eq!(cull.names(), left, "and the list is shorter");
            assert_eq!(cull.io.0.lock().unwrap().files, on_disk(), "with the file still in the folder");
        }
        drop(held);
        cull.settle();
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["DOG", "EEL"], "DOG"), "the job's reread changes nothing");
        // The last photo: the one before it. The only one: the empty folder, and Ctrl+Z still reaches it.
        cull.arrow_to("EEL");
        let held = held_lock(&cull);
        cull.press_del(&["EEL"]);
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["DOG"], "DOG"));
        cull.press_del(&["DOG"]);
        assert!(cull.names().is_empty(), "no photos left");
        drop(held);
        cull.settle();
        assert_eq!(cull.ctrl_z(), "back DOG");
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["DOG"], "DOG"), "back on it");
        // A menu delete of another photo, then a selection without yours: you stay on your photo.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.arrow_to("CAT");
        let held = held_lock(&cull);
        cull.press_del(&["ANT"]);
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["BEE", "CAT", "DOG", "EEL"], "CAT"));
        cull.press_del(&["BEE", "EEL"]);
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["CAT", "DOG"], "CAT"));
        // A selection with yours: the next that stayed.
        cull.press_del(&["CAT"]);
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["DOG"], "DOG"));
        drop(held);
        cull.settle();
    }

    /// The whole pair goes from the list and from the stamps, the order stays whatever the sort, and the
    /// other photos' stamps don't change, so what's ready for them is kept. The sort is the one applied
    /// to the list, never a pending pick in the menu. It lands by photo, and reads nothing from disk: the
    /// folder in these rows doesn't exist.
    ///
    /// FALSIFIER (L28): the stamps lose only one file of the pair, or none: the stamp rows. `take_out_scan`
    /// reads the sort from anywhere but `applied`, or sets `land_by_photo` to `None`: those rows. It
    /// renumbers or re-sorts by name: the order rows. `apply_scan` stops writing the applied facts, or the
    /// drain reads the menu: `the_handler_stashes_and_the_tick_takes_it_out`.
    #[test]
    fn take_out_removes_the_whole_pair_and_keeps_order() {
        let t0 = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
        let live = vec![shot("ZED"), pair("BEE"), shot("ANT"), pair("CAT")];
        let stamps: support::ListingStamps = live.iter().flat_map(|s| s.raw.iter().chain(s.jpg.iter())).map(|f| (f.clone(), (4u64, t0))).collect();
        let by_date = support::AppliedScan { sort: (SortMethod::Modified, true), review_load: support::ReviewLoad::Loaded };
        let attrs = FolderAttrs { sort: Some("the live attr".into()), ..FolderAttrs::default() };
        let out = |list: &[Shot], photo: &str, stamps: &support::ListingStamps| {
            let at = list.iter().position(|s| s.name == photo).unwrap();
            support::take_out_scan(list, &[(at, list[at].clone())], PathBuf::from("no/such/folder"), Some(stamps), attrs.clone(), true, None, by_date)
        };
        let mut done = out(&live, "BEE", &stamps);
        let names = |d: &ScanDone| d.shots.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&done), ["ZED", "ANT", "CAT"], "the order stays, not by name");
        let after = done.stamps.clone().expect("stamps");
        assert!(!after.contains_key(&PathBuf::from("BEE.CR3")) && !after.contains_key(&PathBuf::from("BEE.png")), "both of the pair's files");
        assert_eq!(after.len(), 4, "and only them");
        assert_eq!(after.get(&PathBuf::from("CAT.CR3")), stamps.get(&PathBuf::from("CAT.CR3")), "CAT's are as they were");
        assert_eq!(done.sort, (SortMethod::Modified, true), "the sort applied, whatever the menu shows");
        assert_eq!(menu_after(&done), (SortMethod::Taken, true), "and the menu keeps the pick that's waiting");
        assert_eq!(done.attrs.sort.as_deref(), Some("the live attr"));
        assert_eq!((done.id, done.had_file, done.partial, done.scan_err, done.review_load), (0, true, false, false, support::ReviewLoad::Loaded));
        // By photo, at acceptance: on BEE, the next is ANT.
        settle_by_photo(&mut done, &live, 1, 0, None);
        assert_eq!(done.shots[done.start].name, "ANT");
        // What's ready for the rest is kept: only BEE's work goes, and it's the map the carry reads.
        let identity = identity_map(&live, &done.shots);
        assert_eq!(unchanged_map(&live, &identity, &stamps, &after), [Some(0), None, Some(1), Some(2)]);
        // Again, on the shorter list, with the pair that's left.
        let next = out(&done.shots, "CAT", &after);
        assert_eq!(names(&next), ["ZED", "ANT"]);
        assert_eq!(next.stamps.as_ref().unwrap().len(), 2);
        // With no stamps (a listing that failed), nothing to keep them by. A Rating order stays as it is:
        // no `source_order`, so `settle_by_photo` has nothing to re-sort by.
        let rated = support::AppliedScan { sort: (SortMethod::Rating, true), review_load: support::ReviewLoad::Loaded };
        let list = vec![shot("LOW"), shot("HIGH"), shot("MID")];
        let mut done = support::take_out_scan(&list, &[(2, shot("MID"))], PathBuf::from("no/such/folder"), None, attrs.clone(), false, None, rated);
        assert!(done.stamps.is_none());
        done.ratings = vec![1, 5];
        settle_by_photo(&mut done, &list, 2, 0, None);
        assert_eq!(names(&done), ["LOW", "HIGH"], "a delete re-sorts nothing");
        assert_eq!(done.shots[done.start].name, "HIGH", "nothing after it: the one before");
    }

    /// What the menu would show after that landing: a Date taken pick still waiting.
    fn menu_after(done: &ScanDone) -> (SortMethod, bool) {
        support::menu_sort(done.sort, Some(true))
    }

    /// R12: `Shot.id` is read as a position (the cloud-only set, the "not downloaded" wording, the retry
    /// sweep), and a take-out goes through no sort or listing that renumbers. A cloud-only photo stays
    /// the only one marked, press after press, wherever it has moved to.
    ///
    /// FALSIFIER (L28): remove the renumbering in `take_out_scan`: the first press leaves DOG's id one too
    /// high, and the set marks the wrong photo.
    #[test]
    fn shot_ids_match_index_after_a_take_out() {
        let numbered = |names: &[&str]| -> Vec<Shot> {
            names.iter().enumerate().map(|(i, n)| Shot { id: i, cloud_placeholder: *n == "DOG", ..shot(n) }).collect()
        };
        let mut live = numbered(&["ANT", "BEE", "CAT", "DOG", "EEL", "FOX"]);
        let meta = crate::meta::PerShotMeta::new(live.len());
        for (generation, gone) in [(2, vec!["ANT"]), (3, vec!["CAT"]), (4, vec!["BEE", "FOX"])] {
            let targets: Vec<(usize, Shot)> = gone.iter().map(|g| {
                let at = live.iter().position(|s| s.name == *g).unwrap();
                (at, live[at].clone())
            }).collect();
            let done = support::take_out_scan(&live, &targets, PathBuf::from("folder"), None, FolderAttrs::default(), true, None, APPLIED_BY_NAME);
            live = done.shots;
            assert!(live.iter().enumerate().all(|(i, s)| s.id == i), "{gone:?}: id is the index: {:?}", live.iter().map(|s| (&s.name, s.id)).collect::<Vec<_>>());
            meta.on_folder_swap(generation, live.len(), &live);
            let dog = live.iter().position(|s| s.name == "DOG").unwrap();
            assert_eq!(*meta.cloud_tagged.borrow(), HashSet::from([dog]), "{gone:?}: the cloud-only photo is still DOG");
        }
    }

    /// A star on a photo leaving waits by name in the moved-out entries, and comes back with it. Stars on
    /// three photos, each taken out in turn, then a listing with one of them back: it has its star, and
    /// the other two keep theirs, waiting.
    ///
    /// FALSIFIER (L28): `carry_live_review_on_rescan` drops the moved-out entries: ANT's star is gone by
    /// the second press. It drops the leftovers: the same. `take_out_scan`'s list is applied without the
    /// carry (`apply_scan`'s same-folder test): the wiring row.
    #[test]
    fn take_out_keeps_stars_by_name_across_three_deletes_and_returns_them() {
        let mut list = list(&["ANT", "BEE", "CAT", "DOG"]);
        let (mut ratings, mut marks, mut extra) = (vec![5, 4, 3, 2], vec![0, MARK_FLAG, 0, 0], BTreeMap::new());
        for photo in ["ANT", "BEE", "CAT"] {
            let at = list.iter().position(|s| s.name == photo).unwrap();
            let mut done = support::take_out_scan(&list, &[(at, list[at].clone())], PathBuf::from("folder"), None, FolderAttrs::default(), true, None, APPLIED_BY_NAME);
            support::carry_live_review_on_rescan(&mut done, &list, &ratings, &marks, &extra, BTreeMap::new());
            (list, ratings, marks, extra) = (done.shots, done.ratings, done.marks, done.extra);
            assert!(extra.contains_key(photo), "{photo}: its entry waits");
        }
        assert_eq!((ratings.as_slice(), marks.as_slice()), (&[2][..], &[0][..]), "DOG keeps its own");
        assert_eq!(extra.keys().map(String::as_str).collect::<Vec<_>>(), ["ANT", "BEE", "CAT"]);
        let mut back = reread(self::list(&["BEE", "DOG"]), Named::default());
        support::carry_live_review_on_rescan(&mut back, &list, &ratings, &marks, &extra, BTreeMap::new());
        assert_eq!(back.ratings, [4, 2], "BEE has its star");
        assert_eq!(back.marks, [MARK_FLAG, 0], "and its flag");
        assert_eq!(back.extra.get("ANT").map(|s| s.rating), Some(5), "ANT's waits");
        assert_eq!(back.extra.get("CAT").map(|s| s.rating), Some(3), "and CAT's");
        assert!(!back.extra.contains_key("BEE"), "BEE's has come back");
    }

    /// R22: a photo that went for good has no record, no pill, no Ctrl+Z entry. Ctrl+Z reaches the action
    /// before, and the row says what happened. Del, Del, Ctrl+Z, because culling repeats.
    ///
    /// FALSIFIER (L28): `press_delete` sends every photo to the bin (`goes_for_good` ignored): the disk, and a
    /// record. The job's result offers a pill: the rows.
    #[test]
    fn a_for_good_delete_has_no_way_back_and_ctrl_z_reaches_the_action_before() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("BEE", 3);
        for photo in ["ANT", "CAT"] {
            cull.delete_on(&[photo], support::DeleteRoute::NoBin);
            assert!(!cull.names().contains(&photo), "{photo} has gone");
            assert!(cull.files.borrow().deletes.is_empty(), "{photo}: no record");
            assert_eq!(cull.rows_so_far(), [(format!("Deleted {photo} for good"), 1, false)], "{photo}: says so, with no Recover");
            let disk = cull.io.0.lock().unwrap();
            assert!(disk.deleted.contains(&format!("{photo}.png")) && !disk.files.contains(&format!("{photo}.png")), "{photo}: deleted");
            assert!(disk.bin.is_empty(), "{photo}: nothing in the bin");
        }
        assert_eq!(cull.ctrl_z(), "star BEE", "the action before");
        assert_eq!(cull.stars["BEE"], 0);
        assert_eq!(cull.ctrl_z(), "nothing", "and nothing brings a photo back");
        assert_eq!(cull.names(), ["BEE", "DOG"]);
    }

    /// R22, and "Star, Del, Ctrl+Z, Ctrl+Z" on a drive with no bin. A star on the deleted photo waits for
    /// its job, so a press right after Del can't reach an older star first and can't lose it if the delete
    /// fails. When the photo really has gone, its step goes with it and the press reaches the step before.
    ///
    /// FALSIFIER (L28): `press_delete` files a record whatever the route: Ctrl+Z queues a Recover rather than
    /// waiting. The steps drop at the press instead of `start_return`: the failed row has nothing to take
    /// back. `press_delete` parks nothing: the landing drops them, and the press doesn't wait. The fold
    /// doesn't `send_back`, or the report says no for-good: "a press still waits". The fold asks for no
    /// reread: the failed row's photo stays gone.
    #[test]
    fn a_for_good_photos_steps_wait_for_its_job_and_go_with_it() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        cull.star("BEE", 2);
        cull.star("ANT", 3);
        let held = held_lock(&cull);
        cull.press_del_on(&["ANT"], support::DeleteRoute::NoBin);
        assert_eq!(cull.on, "BEE", "the next photo");
        assert_eq!(cull.press_z(), "waits", "the star was on the way out, so the press waits for the job");
        assert_eq!(cull.stars["BEE"], 2, "and takes nothing older meanwhile");
        drop(held);
        assert_eq!(cull.settle(), "star BEE", "ANT has gone, its step with it, and the press reaches the step before");
        assert_eq!((cull.stars["BEE"], cull.undo.borrow().undo.len()), (0, 0));
        assert!(cull.files.borrow().on_the_way.is_empty() && cull.files.borrow().returning.is_empty());

        // The same with the file refusing, twice: it comes back with its step.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        cull.star("BEE", 2);
        cull.star("ANT", 3);
        cull.lock_file("ANT.png");
        for _ in 0..2 {
            cull.delete_on(&["ANT"], support::DeleteRoute::NoBin);
            assert_eq!((cull.names(), cull.on.as_str()), (vec!["ANT", "BEE", "CAT"], "BEE"), "back in its place, and the person stays");
            assert!(cull.files.borrow().deletes.is_empty());
            assert_eq!(cull.rows_so_far(), [("Couldn't delete ANT, another program may have it open. It's still in the folder.".to_string(), 3, false)]);
        }
        assert_eq!(cull.ctrl_z(), "star ANT", "its step is still there");
        assert_eq!(cull.stars["ANT"], 0);
        assert_eq!(cull.ctrl_z(), "star BEE");
    }

    /// R24: a mixed selection recycles the rest. One press, one record of what can come back, and Ctrl+Z
    /// brings back only that, with its steps. The photo that went from the cloud stays gone, and its step
    /// with it. The row says both halves in plain words. A record of one left reads by name (R25).
    ///
    /// FALSIFIER (L28): `press_delete` records all the targets: a file that never reached the bin. It parks
    /// nothing for the cloud half: the step reaches the wrong photo. `photos` keeps the press's count: the name.
    #[test]
    fn a_mixed_selection_recycles_the_rest_and_says_so() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("BEE", 2);
        cull.star("CAT", 4);
        let cloud = support::DeleteRoute::CloudOnly(vec!["BEE".to_string()]);
        let held = held_lock(&cull);
        cull.press_del_on(&["ANT", "BEE", "CAT"], cloud.clone());
        {
            let files = cull.files.borrow();
            assert_eq!((files.deletes[0].photos, files.deletes[0].name.as_str()), (2, "2 photos"), "from the press, as Ctrl+Z can reach it with its job still queued");
        }
        drop(held);
        cull.settle();
        assert_eq!(cull.names(), ["DOG"]);
        {
            let disk = cull.io.0.lock().unwrap();
            assert!(disk.deleted.contains("BEE.png") && !disk.bin.contains("BEE.png"), "BEE skipped the bin");
            assert!(disk.bin.contains("ANT.png") && disk.bin.contains("CAT.png"), "the rest reached it");
            assert!(!disk.deleted.contains("ANT.png") && !disk.deleted.contains("CAT.png"));
        }
        // The bin and the cloud go by the platform's own names ("Trash" and "iCloud Drive" on the Mac).
        let said = format!("Sent 2 photos to the {}. Deleted 1 from {}.", platform::bin_noun(), platform::PLATFORM.cloud_hint);
        assert_eq!(cull.rows_so_far(), [(said, 1, true)], "and it says so, with Recover for what can come back");
        let seq = {
            let files = cull.files.borrow();
            assert_eq!(files.deletes.len(), 1, "one press, one record");
            assert_eq!(files.deletes[0].photos, 2);
            assert_eq!(files.deletes[0].name, "2 photos");
            assert_eq!(files.deletes[0].files().len(), 2);
            files.deletes[0].seq
        };
        assert_eq!(cull.recover(seq), "back ANT CAT", "one press brings back everything that can come back");
        assert_eq!(cull.names(), ["ANT", "CAT", "DOG"], "and BEE stays gone");
        assert_eq!(cull.ctrl_z(), "star CAT", "CAT's step came back with it");
        assert_eq!(cull.ctrl_z(), "nothing", "BEE's went with BEE");

        // Two of a selection, one cloud-only: the record is that one photo, by name. It's a clean report,
        // and BEE's steps wait for a listing, so that doesn't wait out a clean delete's quiet second.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        cull.star("BEE", 1);
        cull.press_del_on(&["ANT", "BEE"], cloud);
        cull.until_quiet();
        let now = Instant::now();
        cull.fold_reports_at(String::new(), now);
        assert!(cull.reread.borrow().due(now, true, false), "the listing is due at once");
        cull.settle();
        let files = cull.files.borrow();
        assert_eq!((files.deletes[0].photos, files.deletes[0].name.as_str()), (0, "ANT"), "a record of one reads by name");
    }

    /// R22: a pair's for-good result, through the press, the job and the reread: the half that stayed comes
    /// back to its place, on its own, and there's no way back for the half that left.
    ///
    /// FALSIFIER (L28): `how_it_went` reads the attempt: the sentence. The fold asks for no reread: the half.
    #[test]
    fn a_locked_half_stays_and_says_which() {
        let mut cull = Culling::with_files(&["IMG_6.png", "IMG_7.CR3", "IMG_7.png", "IMG_8.png"]);
        cull.arrow_to("IMG_7");
        cull.lock_file("IMG_7.png");
        cull.delete_on(&["IMG_7"], support::DeleteRoute::NoBin);
        assert_eq!(cull.rows_so_far(), [("Deleted the RAW of IMG_7 for good. The PNG is still in the folder.".to_string(), 3, false)]);
        assert_eq!(cull.names(), ["IMG_6", "IMG_7", "IMG_8"], "the half that stayed is back");
        assert!(cull.shots[cull.place("IMG_7")].raw.is_none(), "as a photo of its own");
        assert_eq!(cull.on, "IMG_8", "and the person is on the next photo, from the press");
        assert_eq!(cull.ctrl_z(), "nothing");
    }

    /// R22: "Decide at dialog open and store the choice with the pending identity, so the words read and the
    /// call run can't differ." The same route picks the dialog's words and the job's call, for each route,
    /// and the drive's answer changing while the dialog is open changes neither.
    ///
    /// FALSIFIER (L28): the confirm asks the drive again (`delete_route` in `on_delete_recycle`): the wiring
    /// rows. A dialog opens without storing its route: the wiring rows. `goes_for_good` disagrees with the
    /// words: the rows that read the disk.
    #[test]
    fn the_words_and_the_call_are_decided_together() {
        for (has_for_good, drive_asks, cloud_only, says_for_good, says_cloud) in [
            (true, true, false, true, false),   // a card, on Windows
            (true, false, true, false, true),   // a cloud-only photo on a fixed disk
            (false, true, false, false, false), // a Mac's network volume
            (false, false, true, false, false), // a Mac's iCloud photo on a local volume
        ] {
            let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
            let ant = cull.shots[cull.place("ANT")].clone();
            let probe = |_: &Path| cloud_only;
            // The dialog opens, from one route.
            let route = support::delete_route_on(has_for_good, drive_asks, &[&ant], probe);
            let words = crate::one_delete_words(&route, &ant, 0);
            let pending = route.clone();
            // The drive's answer flips while it's open: a reread lands, or a card swaps under the letter.
            let flipped = support::delete_route_on(has_for_good, !drive_asks, &[&ant], probe);
            assert_ne!(flipped, route, "the row's premise: another answer is another route");
            // The confirm reads what was stored.
            cull.delete_on(&["ANT"], pending);
            let (deleted, in_bin) = {
                let disk = cull.io.0.lock().unwrap();
                (disk.deleted.contains("ANT.png"), disk.bin.contains("ANT.png"))
            };
            let label = format!("mac {}, asks {drive_asks}, cloud {cloud_only}", !has_for_good);
            assert_eq!(deleted, says_for_good || says_cloud, "{label}: the call");
            assert_eq!(in_bin, !deleted, "{label}: one or the other");
            assert_eq!(words.confirm.to_lowercase().contains("for good"), says_for_good, "{label}: the button: {}", words.confirm);
            assert_eq!(words.title.contains(crate::platform::PLATFORM.cloud_hint), says_cloud, "{label}: the title: {}", words.title);
            assert_eq!(cull.files.borrow().deletes.is_empty(), deleted, "{label}: what can come back has a record, and only that");
            let rows = cull.rows_so_far();
            assert_eq!(rows[0].0.ends_with("for good"), says_for_good, "{label}: the message: {rows:?}");
            assert_eq!(rows[0].0.contains(crate::platform::PLATFORM.cloud_hint), says_cloud, "{label}: the message: {rows:?}");
        }

        let main = main_code();
        let confirm = section(&main, "app.on_delete_recycle(move || {", "app.on_recover_delete(move |seq| {");
        assert!(!confirm.contains("delete_route") && !confirm.contains("drive_asks") && !confirm.contains("file_is_cloud_placeholder"), "the confirm asks nothing again");
        assert!(confirm.contains("let Some((pgen, pdir, targets, route)) = del_bulk_pending_dr.borrow().clone()"), "a selection's route is the pending one");
        assert!(confirm.contains("let Some((pgen, pdir, idx, name, route)) = del_pending_dr.borrow().clone()"), "and so is a photo's");
        assert_eq!(confirm.matches(" route,\n").count(), 2, "and both go on to the job");
        let one = section(&main, "fn open_delete_confirm(", "\n}");
        assert!(one.contains("let words = one_delete_words(&route, shot, sidecars);"), "the words come from it");
        assert!(one.contains("*del_pending.borrow_mut() = Some((gen_now, dir_now, idx, shot.name.clone(), route));"), "and it's stored beside the identity");
        assert!(one.contains("app.set_delete_title(words.title.into());") && one.contains("app.set_delete_body(words.body.into());") && one.contains("app.set_delete_confirm_label(words.confirm.into());"), "all three things it says");
        let many = section(&main, "let open_bulk_delete: Rc<dyn Fn()> = {", "app.on_bulk_delete_ask");
        assert!(many.contains("let words = support::bulk_delete_words(&route, names.len(), files, sidecars);"), "a selection's too");
        assert!(many.contains("*pending.borrow_mut() = Some((gen_now, dir_now, names, route));"));
        assert!(many.contains("a.set_delete_title(words.title.into());") && many.contains("a.set_delete_body(words.body.into());") && many.contains("a.set_delete_confirm_label(words.confirm.into());"));
        let queue = queue_code();
        let press = section(&queue, "pub(crate) fn press_delete(", "\n}");
        assert!(press.contains("partition(|(_, s)| route.goes_for_good(s))"), "and the job is what the route says");
    }

    /// R21, R25, R27: every route funnels through one rule: the key, a menu's row, a selection's. If it can
    /// come back, it goes with no dialog; if it can't, the dialog says so. At once takes the same stash as a
    /// confirm, behind the confirm's own gate for another operation. A selection of one is a single delete.
    ///
    /// FALSIFIER (L28): `delete_one` opens the dialog always, or never: its row. `press_now` loses the busy
    /// gate: its row. Either menu or the key loses R6's gate: its row. A selection of one goes on the plural
    /// route: the row naming it.
    #[test]
    fn every_route_goes_at_once_or_asks_by_one_rule() {
        let main = main_code();
        let one = section(&main, "let delete_one: Rc<DeleteOne> = {", "\n    };");
        let at: Vec<usize> = [
            "let route = support::delete_route(drive_asks_d1.get(), &[shot]);",
            "if route.asks() {",
            "open_delete_confirm(app, shot, idx, gen_now, dir_now, route, &del_pending_d1);",
            "press_now_d1(file_queue::TakeOut { dir: dir_now, gen: gen_now, name: shot.name.clone(), photos: 0, targets: vec![(idx, shot.clone())], route });",
        ].iter().map(|s| one.find(s).unwrap_or_else(|| panic!("the door: {s}"))).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "the door's order: {at:?}");
        // The same gate as the dialog's confirm, and the same words.
        let now = section(&main, "let press_now: Rc<dyn Fn(file_queue::TakeOut)> = {", "\n    };");
        let (gate, stash) = (now.find("if busy.load(Ordering::SeqCst) {").expect("the gate"), now.find("*take_out.borrow_mut() = Some(press);").expect("the stash"));
        assert!(gate < stash && now[gate..stash].contains("return;"), "another file operation stops it");
        assert!(now.contains("i18n::tr(\"Busy with another file operation — delete cancelled\")"), "out loud");
        // The key: R6's refusal, in every case, then the selection, then the photo.
        let key = section(&main, "app.on_delete_key(move || {", "\n        });");
        let order: Vec<usize> = ["if !delete_key_allowed(", "if gate_dk() {", "if bulk_armed(&app, &ss_dk) {", "open_bulk_dk();", "delete_one_dk(&app, &snap, c);"]
            .iter().map(|s| key.find(s).unwrap_or_else(|| panic!("the key: {s}"))).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "the key's order: {order:?}");
        assert!(!key.contains("open_delete_confirm("), "the key has no door of its own");
        // A menu row: the photo on screen can't go before it's shown, and another photo's tile shows what it is.
        let menu = section(&main, "app.on_delete_shot(move |idx: i32| {", "\n        });");
        let order: Vec<usize> = ["if !delete_key_allowed(", "if idx == on_screen && !app.get_compare() && gate_ds() {", "delete_one_ds(&app, &snap, idx);"]
            .iter().map(|s| menu.find(s).unwrap_or_else(|| panic!("the menu: {s}"))).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "the menu's order: {order:?}");
        assert!(!menu.contains("open_delete_confirm("), "nor does a menu");
        // A selection: R25, and R6 where it reaches the photo on screen. One photo is a single delete.
        let many = section(&main, "let open_bulk_delete: Rc<dyn Fn()> = {", "app.on_bulk_delete_ask");
        let order: Vec<usize> = [
            "let route = support::delete_route(drive_asks_bd.get(), &photos);",
            "if support::selection_goes_at_once(&route, &live, on_screen) {",
            "if gate_bd() {",
            "[one] => (one.name.clone(), 0),",
            "press_now_bd(file_queue::TakeOut {",
            "if let [one] = live[..] {",
            "open_delete_confirm(&a, &snap[one], one, gen_now, dir_now, route, &del_pending_bd);",
            "a.set_del_bulk(true);",
        ].iter().map(|s| many.find(s).unwrap_or_else(|| panic!("the selection: {s}"))).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "the selection's order: {order:?}");
        let ask = section(&main, "app.on_bulk_delete_ask(move || {", "\n    }");
        assert!(ask.contains("if take_out_bd.borrow().is_some() { return; }"), "a menu's press waits for one stashed, as the key's does");
        // R6's gate is the cull keys' own.
        let gate = section(&main, "let cull_gate = {", "let rt = ratings.clone();");
        assert!(gate.contains("support::photo_not_on_stage(shown.borrow().as_ref().map(|(i, _, _)| *i), c),"), "the one question");
        assert!(gate.contains("i18n::tr(\"Still loading — press again when the photo appears\"),"), "the one sentence");
        assert!(gate.contains("support::REFUSAL_TOAST_GAP_MS"), "at the one rate");
    }

    /// R21: the drive's answer lives with the folder's list: the thread that read the list says it, a landing
    /// or an unchanged reread takes it, and a take-out reads no disk and leaves it. Never saved.
    ///
    /// FALSIFIER (L28): `Unchanged` drops the answer: the accept row. `reopen_scan` says nothing: the real
    /// folder's row. `take_out_scan` asks: its row. The tick or `apply_scan` ignores it: the wiring rows.
    #[test]
    fn each_reread_says_what_the_drive_answered() {
        // A real generated folder, through the real listing.
        let dir = std::env::temp_dir().join(format!("falcon_reread_answer_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ANT.jpg"), b"not a real jpeg").unwrap();
        let input = file_queue::ListingInput { live: Arc::new(Vec::new()), live_sort: None, sort_lru: Vec::new(), sort_cap: 10, inline_dates: false };
        let listed = file_queue::run_listing(&dir, input, falcon_decode::scan_folder_with_metadata);
        let answer = listed.result.expect("the folder reads").delete_asks;
        assert_eq!(answer, Some(support::delete_asks(&dir)), "the listing says it");
        let _ = std::fs::remove_dir_all(&dir);

        // An unchanged listing lands nothing, and still hands it over, both ways, because letters get reused.
        for said in [true, false] {
            let cull = Culling::new(&["ANT", "BEE"]);
            cull.reread.borrow_mut().want(PathBuf::from(&cull.dir), true);
            let ticket = cull.start_listing(Instant::now()).expect("a listing is due");
            let mut listing = cull.listing();
            listing.delete_asks = Some(said);
            *ticket.arrived.lock().unwrap() = Some(file_queue::Listed { result: Ok(listing), cloud_flipped: false, warn: None, took: Duration::ZERO });
            let accepted = {
                let stamps = cull.stamps.borrow();
                cull.reread.borrow_mut().accept(cull.gen.get(), cull.queue.pushes(), stamps.as_ref(), || false)
            };
            assert!(matches!(accepted, file_queue::Acceptance::Unchanged(Some(a)) if a == said), "unchanged, and it says {said}");
        }

        // A take-out reads no disk.
        let cull = Culling::new(&["ANT", "BEE"]);
        let done = support::take_out_scan(&cull.shots, &[(0, cull.shots[0].clone())], PathBuf::from(&cull.dir), None, FolderAttrs::default(), true, None, APPLIED_BY_NAME);
        assert_eq!(done.delete_asks, None, "the answer stands");

        let main = main_code();
        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        assert!(apply.contains("if let Some(asks) = delete_asks {\n                drive_asks.set(asks);"), "a landing takes it");
        let tick = section(&main, "file_queue::Acceptance::Unchanged(delete_asks) => {", "file_queue::Acceptance::Land(mut done, warn) => {");
        assert!(tick.contains("if let Some(asks) = delete_asks {\n                    drive_asks_t.set(asks);"), "and so does a reread that lands nothing");
        // A folder's open: once, with its log line, off the interface thread, for each stage it lands.
        assert_eq!(main.matches("support::delete_asks_at_open(&dir)").count(), 1);
        assert_eq!(main.matches("delete_asks: Some(delete_asks)").count(), 3, "the first photo, the nearby ones and the whole list");
        let log = include_str!("support.rs").replace("\r\n", "\n");
        let at_open = log.split_once("pub(crate) fn delete_asks_at_open(").expect("the open's own").1.split_once("\n}\n").expect("to its end").0;
        assert!(at_open.contains("this folder is on {root}, drive type {kind}"), "the letter and the type");
        assert!(!at_open.contains("dir.display()"), "and never a folder's name, which is private");
        // Never saved.
        let settings = section(&log, "pub(crate) struct Settings {", "\n}\n");
        assert!(!settings.contains("delete_asks") && !settings.contains("drive_asks"), "letters get reused");
    }

    /// R23: Empty ./Rejected decides when its dialog opens, words and call together, and the confirm reads
    /// that. The panel's button goes through Rust, so nothing can open the dialog undecided.
    ///
    /// FALSIFIER (L28): the handler asks the drive itself: its row. The button sets the kind itself: the slint
    /// row. The tick's title forgets the decision: its row. The worker calls the bin either way: its row.
    #[test]
    fn empty_rejected_decides_when_its_dialog_opens() {
        let main = main_code();
        let ask = section(&main, "app.on_empty_rejected_ask(move || {", "\n        });");
        let order: Vec<usize> = [
            "let for_good = support::HAS_FOR_GOOD_ROUTE && drive_asks_ea.get();",
            "for_good_ea.set(for_good);",
            "support::empty_words(for_good, ",
            "a.set_empty_confirm_title(title.into());",
            "a.set_empty_confirm_body(body.into());",
            "a.set_empty_confirm_label(confirm.into());",
            "a.set_confirm_kind(3);",
        ].iter().map(|s| ask.find(s).unwrap_or_else(|| panic!("the open: {s}"))).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "the open's order: {order:?}");
        let confirm = section(&main, "app.on_empty_rejected(move || {", "\n        });");
        assert!(!confirm.contains("drive_asks"), "the confirm asks nothing again");
        assert!(confirm.contains("let for_good = for_good_er.get();"), "it reads what the dialog said");
        assert!(confirm.contains("if for_good { support::empty_rejected_for_good(Path::new(&dir)) } else { empty_rejected_to_bin(Path::new(&dir)) }"), "and that's the call");
        assert!(main.contains("app.set_empty_confirm_title(support::empty_words(empty_for_good_t.get(), n).0.into());"), "the tick's count keeps its words");
        let window = include_str!("../ui/main_window.slint").replace("\r\n", "\n");
        assert!(window.contains("clicked => { root.empty-rejected-ask(); }"), "the panel's button asks");
        assert!(!window.contains("root.confirm-kind = 3;"), "and nothing else opens it");
    }

    /// The design's failure rule, now that the photo left at the press. A delete that fails: the red line,
    /// the photo back in its place at the reread, the person where they are, and nothing lost. Its star's
    /// step goes on Ctrl+Z at its old place in the line, as the Stage A record asked. Twice, then a
    /// selection, because culling repeats.
    ///
    /// FALSIFIER (L28): `settle_delete` forgets the steps: "nothing", or the older star first. The fold stops
    /// asking for the reread: the photo stays gone. The job's reread lands on the photo: the person moves.
    #[test]
    fn a_delete_that_fails_brings_the_photo_back_and_loses_no_step() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("DOG", 1);
        cull.star("ANT", 3);
        cull.turn("ANT");
        cull.lock_file("ANT.png");
        for _ in 0..2 {
            let held = held_lock(&cull);
            cull.press_del(&["ANT"]);
            assert_eq!((cull.names(), cull.on.as_str()), (vec!["BEE", "CAT", "DOG"], "BEE"), "gone from the list at the press");
            assert!(cull.undo.borrow().undo.iter().all(|a| !matches!(&a.entry, UndoEntry::Rot { name, .. } if name == "ANT")), "its steps went with it");
            drop(held);
            cull.settle();
            assert_eq!((cull.names(), cull.on.as_str()), (vec!["ANT", "BEE", "CAT", "DOG"], "BEE"), "back in its place, and the person stays");
            assert!(cull.files.borrow().deletes.is_empty(), "nothing can come back, so nothing offers it");
            let rows = cull.rows_so_far();
            assert_eq!(rows.len(), 1, "one red line: {rows:?}");
            assert_eq!((rows[0].1, rows[0].2), (3, false));
            cull.arrow_to("ANT");
        }
        assert_eq!(cull.ctrl_z(), "turn ANT", "its steps are back, on it");
        assert_eq!(cull.ctrl_z(), "star ANT");
        assert_eq!(cull.ctrl_z(), "star DOG", "then the older star");
        // A selection where one is locked: the rest go, and that one comes back with its star.
        cull.star("ANT", 2);
        cull.star("BEE", 2);
        cull.delete(&["ANT", "BEE"]);
        assert_eq!(cull.names(), ["ANT", "CAT", "DOG"]);
        assert_eq!(cull.ctrl_z(), "back BEE", "the record is what left");
        assert_eq!(cull.ctrl_z(), "star BEE");
        assert_eq!(cull.ctrl_z(), "star ANT");
    }

    /// A take-out lands from the list on screen, which doesn't hold a photo on its way back. The steps
    /// of that photo sit this landing out and return at the reread that lists it, so star, Del, Ctrl+Z, Del
    /// of another photo, Ctrl+Z still ends unstarred on that photo. Twice over.
    ///
    /// FALSIFIER (L28): `hold_returning` leaves them for the landing: the second press reaches the older
    /// star. `release_returning` forgets them: the same. The drain stops holding them: the wiring row.
    #[test]
    fn a_take_out_leaves_steps_on_their_way_back_alone() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.star("EEL", 5);
        for _ in 0..2 {
            cull.star("ANT", 3);
            cull.delete(&["ANT"]);
            // Ctrl+Z, and its job reports, with the reread still to come...
            assert_eq!(cull.press_z(), "job");
            cull.until_quiet();
            assert_eq!(cull.fold_reports(String::new()), "back ANT");
            assert_eq!(cull.files.borrow().returning.len(), 1, "ANT's star is on its way");
            // ...when a Del of another photo lands first.
            cull.press_del(&["BEE"]);
            assert_eq!(cull.files.borrow().returning.len(), 1, "it isn't ANT's landing");
            cull.settle();
            assert_eq!(cull.names(), ["ANT", "CAT", "DOG", "EEL"]);
            assert_eq!(cull.ctrl_z(), "back BEE");
            assert_eq!(cull.ctrl_z(), "star ANT", "ANT's own star, not the older one on EEL");
            assert_eq!(cull.stars["EEL"], 5);
            cull.arrow_to("ANT");
        }
    }

    /// The press, as the design has it. The handler only runs its gates and stashes. The tick takes it first
    /// of all, so its places address the list and a report can't land steps ahead of it, and clears redo
    /// there (R40). The drain's order keeps the star that was just pressed: the record and its steps from
    /// the list they address, the landing with its flush, then the job, whose barrier covers that flush.
    /// A second Del in that window is ignored, and a Ctrl+Z waits for the record.
    ///
    /// FALSIFIER (L28): swap any two of the drain's lines below, or drop one. Run the drain after the fold or
    /// the reread. Drop the key's or the menu's second-press gate, or Ctrl+Z's wait. Have the handler press
    /// or flush. Stop writing the applied facts, or read the menu's sort. Drop the log line.
    #[test]
    fn the_handler_stashes_and_the_tick_takes_it_out() {
        let main = main_code();
        let handler = section(&main, "app.on_delete_recycle(move || {", "let history_rc = file_history.clone();");
        assert!(!handler.contains("press_delete") && !handler.contains("flush") && !handler.contains(".push(") && !handler.contains("next_act_seq"), "the handler only stashes:\n{handler}");
        let drain = section(&main, "let take_out: Rc<dyn Fn(file_queue::TakeOut)> = {", "\n    };");
        let order = [
            "if press.gen != generation.load(Ordering::Relaxed) {",
            "support::note_file_press(&undo_stack);",
            "tick::clear_recovered_card(&a, &tleft);",
            "let pictures: Vec<(String, slint::Image)> = {",
            // Commit 10 (R33, R35): each photo's own, for a selection's lines too.
            "press.targets.iter().map(|(at, shot)| (shot.name.clone(), thumbs.get(at).map_or_else(slint::Image::default, |(img, _)| img.clone()))).collect()",
            "reopen_pending.borrow_mut().superseded_by_rescan(scan_id.load(Ordering::Relaxed))",
            "support::begin_explicit_scan(&scan_id, &scan_done);",
            "support::take_out_scan(",
            "let seq = next_act_seq(&act_seq);",
            "file_queue::press_delete(&file_history, &undo_stack, seq, press, seats);",
            "file_history.borrow_mut().hold_picture(seq, &photo, picture);",
            "file_history.borrow_mut().hold_returning();",
            "apply_scan(done);",
            "file_history.borrow_mut().release_returning(returning);",
            "file_queue.push(job, None);",
            "tick::note_preview_landing(&preview_landing, was_on, now_on);",
            "delete: take-out {name} in {} ms ({} photos left, landing by photo {} us)",
        ];
        let at: Vec<usize> = order.iter().map(|s| drain.find(s).unwrap_or_else(|| panic!("the drain: {s}"))).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "the drain's order: {at:?}");
        assert!(drain.contains("applied_scan.get()") && !drain.contains("get_sort_method") && !drain.contains("get_sort_desc"), "the sort applied, not the menu's");
        assert!(drain.contains("PathBuf::from(last_scanned_dir.borrow().dir())"), "the folder `apply_scan` will read as this same one, so the live review comes across");
        assert!(drain.contains("sel_json_present.get()") && drain.contains("folder_attrs.borrow().clone()") && drain.contains("export_manifest.borrow_mut().take()"), "the live facts");
        let apply = section(&main, "    let apply_scan: Rc<dyn Fn(ScanDone)> = {", "\n    };");
        assert!(apply.contains("applied_scan.set(support::AppliedScan { sort, review_load });"), "every landing writes what it applied");
        assert!(apply.contains("support::carry_live_review_on_rescan(\n                    &mut done, &old, &ratings.borrow(), &marks.borrow(), &sel_extra.borrow(),"), "and carries the live review over it by name");
        // The tick: first of all.
        let tick = main.split_once("let pressed = take_out_stash_t.borrow_mut().take();").expect("the tick takes the stash").1;
        let later = |s: &str| tick.find(s).unwrap_or_else(|| panic!("after the drain: {s}"));
        assert!(tick.trim_start().starts_with("if let Some(press) = pressed {\n            take_out_t(press);\n        }\n        mk!(\"take_out\");"), "its own statement, its own mark:\n{}", &tick[..120]);
        assert!(later("for done in file_queue_t.take_results() {") < later("if let Some(p) = reload_req_t.try_lock()"), "then the reports, then the reread");
        assert!(later("mk!(\"take_out\");") < later("for done in file_queue_t.take_results() {"), "ahead of both");
        assert!(!main.split_once("let pressed = take_out_stash_t").unwrap().0.split_once("let fold_move = || {").unwrap().1.contains("reload_t"), "with no landing before it");
        // A second press in the window.
        let key = section(&main, "app.on_delete_key(move || {", "if app.invoke_menu_action(\"delete\".into(), true)");
        assert!(key.contains("if take_out_dk.borrow().is_some() { return; }"), "the key ignores a second Del");
        let menu = section(&main, "app.on_delete_shot(move |idx: i32| {", "let snap = shots_ds.lock()");
        assert!(menu.contains("if take_out_ds.borrow().is_some() { return; }"), "and so does the menu");
        let undo = section(&main, "app.on_undo(move || {", "let (arm, returning) = {");
        assert!(undo.contains("if take_out_u.borrow().is_some() {\n            waiting_u.set(waiting_u.get() + 1);\n            return;\n        }"), "Ctrl+Z waits for the record, and the tick asks again");
    }

    /// A take-out and a job's reread can land in one tick. The steps that hold the scroll only saw the list
    /// before the first, so they read one landing: the first's anchor and generation, a tile through both
    /// maps, the photo the same only if it stayed through both. A re-sort in either keeps its recentre.
    ///
    /// FALSIFIER (L28): `after` keeps the second's map, anchor or generation, or reads `same_photo` from one
    /// of them: the rows below. It keeps the first when the second has no scroll: the re-sort row.
    /// The real grid and panel: `tipgeom_tests::a_take_out_and_a_reread_in_one_tick_keep_the_scroll`.
    #[test]
    fn two_landings_in_one_tick_read_as_one_for_the_scroll() {
        let (a, b, c) = (list(&["ANT", "BEE", "CAT", "DOG", "EEL"]), list(&["ANT", "CAT", "DOG", "EEL"]), list(&["NEW", "ANT", "CAT", "EEL"]));
        // On CAT: a menu delete of BEE, and a reread that brings NEW and has lost DOG.
        let first = carry_over(&a, &b, 1).keep_scroll(2, 41, true, None);
        let second = carry_over(&b, &c, 2).keep_scroll(1, 42, false, Some((3.0, 4.0)));
        let kept = KeptScroll::after(first, second).expect("the order held in both");
        assert_eq!((kept.anchor, kept.landing, kept.gen_before), (2, 2, 41), "the first's anchor and generation, the last landing");
        assert!(kept.same_photo, "CAT stayed through both");
        assert!(kept.leading_before, "and the first's opening edge");
        assert_eq!(kept.strip, Some((3.0, 4.0)), "the strip's place is the last one's");
        let now: Vec<Option<&str>> = (0..5).map(|tile| kept.stayed(tile).map(|i| c[i].name.as_str())).collect();
        assert_eq!(now, [Some("ANT"), None, Some("CAT"), None, Some("EEL")], "each tile of the first list, through both");
        assert_eq!(c[kept.nearest_stayed(3)].name, "EEL", "DOG went: the next that stayed");
        // The photo on screen left at the first, or at the second: not the same photo.
        let moved_on = |first_on, second_on, start_a, start_b, b: &[Shot], c: &[Shot]| {
            let first = carry_over(&a, b, start_a).keep_scroll(first_on, 1, false, None);
            let second = carry_over(b, c, start_b).keep_scroll(second_on, 2, false, None);
            KeptScroll::after(first, second).expect("the order held").same_photo
        };
        assert!(!moved_on(1, 1, 1, 1, &b, &b), "the first took the photo on screen");
        assert!(!moved_on(3, 2, 2, 2, &b, &list(&["ANT", "CAT", "EEL"])), "on DOG: the second did");
        assert!(moved_on(3, 2, 2, 2, &b, &b), "on DOG, through both");
        // Nothing before it: it stands alone.
        let alone = KeptScroll::after(None, carry_over(&a, &b, 1).keep_scroll(2, 7, false, None)).expect("it stands");
        assert_eq!(alone.gen_before, 7);
        // A re-sort at the second: the tick keeps nothing, and today's recentre runs.
        let reversed: Vec<Shot> = b.iter().rev().cloned().collect();
        let first = carry_over(&a, &b, 1).keep_scroll(2, 1, false, None);
        let second = carry_over(&b, &reversed, 2).keep_scroll(1, 2, false, None);
        assert!(second.is_none());
        assert!(KeptScroll::after(first, second).is_none());
    }

    // ── The reread off the thread, and the bin check (Delete round, Stage B, R13 and R14). The acceptance and
    //    the check's fold are the real ones. The listing is the disk as `Culling` has it. ──

    /// R13: a listing is judged at acceptance, not at arrival, because a hold can outlast a press. Del ANT,
    /// a photo saved from another program, right-click BEE, and the listing arrives and waits for the
    /// menu. Delete this photo: BEE is taken out and the menu closes. The listing still holds BEE, and
    /// would bring it back until the job's reread. Each thing that moves on from a listing counts: a
    /// take-out, a Recover press (R2), a sort pick's landing, a newer ask. Then it lists again.
    ///
    /// FALSIFIER (L28): `accept` ignores the generation: BEE comes back. It ignores the pushes: the Recover row
    /// lands without the photo. It ignores `stale`: the newer ask's row. A discard doesn't list again:
    /// "and it lists again". `want` doesn't mark what's out there: the newer ask's row.
    #[test]
    fn a_stale_listing_is_discarded_at_acceptance() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        cull.delete(&["ANT"]);
        cull.save_beside("NEW.png");
        cull.list_now();
        assert_eq!(cull.accept(true), "nothing", "it waits for the menu");
        let held = held_lock(&cull);
        cull.press_del(&["BEE"]);
        assert_eq!(cull.accept(false), "discarded", "the menu has closed, and this read the list before the take-out");
        assert_eq!(cull.names(), ["CAT", "DOG", "EEL"], "BEE stays out");
        assert!(cull.reread.borrow().wanted_or_running(), "and it lists again");
        drop(held);
        cull.settle();
        assert_eq!((cull.names(), cull.on.as_str()), (vec!["CAT", "DOG", "EEL", "NEW"], "CAT"), "with what was saved, and the person stays");

        // R2: a Recover press invalidates a listing in flight: it read the folder without the photo.
        cull.save_beside("NEWER.png");
        cull.list_now();
        let held = held_lock(&cull);
        assert_eq!(cull.press_z(), "job");
        assert_eq!(cull.accept(false), "discarded", "older than the press");
        drop(held);
        assert_eq!(cull.settle(), "back BEE");
        assert_eq!(cull.on, "BEE", "and the listing after it lands on the photo");
        assert!(cull.names().contains(&"NEWER"));

        // A sort pick's landing, on this thread, reads the folder after this listing did.
        cull.save_beside("LATE.png");
        cull.list_now();
        let picked = cull.listing();
        cull.landed(picked);
        assert_eq!(cull.accept(false), "discarded");
        assert_eq!(cull.reread.borrow_mut().start(cull.gen.get(), cull.queue.pushes()).map(|t| t.dir), Some(PathBuf::from("folder")), "it asks again");
        cull.reread = Default::default();

        // A newer ask: a Move's worker has finished since this started, so it read the folder half moved.
        cull.save_beside("NOW.png");
        cull.list_now();
        cull.reread.borrow_mut().want(PathBuf::from("folder"), true);
        assert_eq!(cull.accept(false), "discarded");
        assert!(!cull.names().contains(&"NOW"));
        cull.settle();
        assert!(cull.names().contains(&"NOW"), "the one after it lands");
    }

    /// The design's row: a listing that starts with the photo in the folder can't bring it back once it has
    /// been deleted. Press after press, each with a listing in flight. And a queue still running starts no
    /// listing, so the first job's reread can't list the second photo again (`Del` twice within 0.1 s).
    ///
    /// FALSIFIER (L28): `accept` ignores the generation: the first press brings ANT back. `due` ignores the
    /// queue: the row with two jobs held. Starts a second listing beside the first: its row.
    #[test]
    fn listing_older_than_a_take_out_cannot_bring_a_photo_back() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        for (photo, left) in [("ANT", vec!["BEE", "CAT", "DOG", "EEL"]), ("BEE", vec!["CAT", "DOG", "EEL"]), ("CAT", vec!["DOG", "EEL"])] {
            cull.save_beside("OTHER.png"); // so it would land, if it could
            cull.list_now();
            cull.press_del(&[photo]);
            assert_eq!(cull.accept(false), "discarded", "{photo}");
            assert_eq!(cull.names(), left, "{photo} stays out");
            cull.until_quiet();
            cull.fold_reports(String::new());
            assert_eq!(cull.accept(false), "nothing", "nothing's waiting");
            cull.io.0.lock().unwrap().files.remove("OTHER.png");
            cull.reread = Default::default();
        }
        // Two presses, with their jobs held: no listing while they run, however long.
        let later = Instant::now() + Duration::from_secs(60);
        let held = held_lock(&cull);
        cull.press_del(&["DOG"]);
        cull.press_del(&["EEL"]);
        cull.reread.borrow_mut().want(PathBuf::from("folder"), true);
        assert!(cull.start_listing(later).is_none(), "the queue isn't quiet");
        drop(held);
        cull.settle();
        assert!(cull.names().is_empty(), "and neither flickered back");
        // One at a time: a second ask waits for the first listing.
        let cull = Culling::new(&["ANT"]);
        cull.list_now();
        cull.reread.borrow_mut().want(PathBuf::from("folder"), true);
        assert!(cull.start_listing(later).is_none(), "one listing at a time");
    }

    /// R13: after a clean delete the folder is what the list expects, so nothing lands: no new generation,
    /// which would drop a decode in flight, no blink, and it doesn't wait for a menu it wouldn't close.
    /// Press after press, as culling is. Nothing is left waiting for it.
    ///
    /// FALSIFIER (L28): `accept` always lands, or compares the stamps wrongly (`!=`): "landed". It asks the holds
    /// first: "nothing", with the menu open. `no_landing` keeps the name: the last row.
    #[test]
    fn unchanged_listing_lands_nothing() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        let later = Instant::now() + 2 * file_queue::QUIET_BEFORE_LISTING;
        for (photo, next) in [("ANT", "BEE"), ("BEE", "CAT"), ("CAT", "DOG")] {
            cull.press_del(&[photo]);
            cull.until_quiet();
            cull.fold_reports(String::new());
            let gen = cull.gen.get();
            let ticket = cull.start_listing(later).expect("its quiet second is up");
            cull.deliver(&ticket);
            assert_eq!(cull.accept(true), "unchanged", "{photo}: with a menu open, which stays open");
            assert_eq!(cull.gen.get(), gen, "{photo}: no landing");
            assert_eq!(cull.on, next, "{photo}: the person stays");
            assert!(!cull.reread.borrow().wanted_or_running(), "{photo}: nothing waits");
        }
        // With no stamps to compare (a listing failed earlier), it can't say, so it lands.
        *cull.stamps.borrow_mut() = None;
        cull.list_now();
        assert_eq!(cull.accept(false), "landed");
        // A name with nothing to land on goes: the photo is already on screen some other way.
        *cull.landing_name.borrow_mut() = Some(recovering("ANT", 0));
        cull.list_now();
        assert_eq!(cull.accept(false), "unchanged");
        assert!(cull.landing_name.borrow().is_none());
    }

    /// R13: a listing that found a real change waits for what a landing would close, and closes nothing,
    /// because it doesn't land. A photo saved from another program appears when the menu closes, and the
    /// person doesn't move. Its holds don't count the reread itself, or it would never land (the
    /// challenge's finding): a changed listing with nothing open lands at once. The tick stays free while
    /// it waits.
    ///
    /// FALSIFIER (L28): `accept` ignores `held`: it lands with the menu open. The tick's holds read
    /// `reload_pending` true: the wiring row, and every changed reread would wait for itself. `working` counts
    /// a listing that has arrived: the full rate holds for as long as the menu is open.
    #[test]
    fn reread_waits_for_an_open_menu_and_closes_nothing() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        for saved in ["ANT2.png", "ANT3.png"] {
            cull.save_beside(saved);
            cull.list_now();
            assert!(cull.reread.borrow().working(), "listing");
            let gen = cull.gen.get();
            let before = cull.names().join(" ");
            for _ in 0..3 {
                assert_eq!(cull.accept(true), "nothing", "a menu is open");
                assert_eq!((cull.gen.get(), cull.names().join(" ")), (gen, before.clone()), "nothing landed, so nothing closes");
            }
            assert!(cull.reread.borrow().wanted_or_running(), "a Date taken landing waits for it");
            assert!(!cull.reread.borrow().working(), "and an open menu doesn't keep the tick at its full rate");
            assert_eq!(cull.accept(false), "landed", "the menu has closed");
            assert!(cull.names().join(" ").contains(saved.trim_end_matches(".png")));
            assert_eq!(cull.on, "ANT", "and the person doesn't move");
        }
        // Nothing open: at once. Its own wait isn't one of its holds.
        cull.save_beside("ANT4.png");
        cull.list_now();
        assert_eq!(cull.accept(false), "landed");

        let main = main_code();
        let holds = section(&main, "let landing_holds = |reload_pending: bool| support::LandingHolds {", "};");
        assert!(holds.contains("position_op: position_op_t.load(Ordering::SeqCst) || !file_queue_t.quiet(),") && holds.contains("reload_pending,") && holds.contains("..support::LandingHolds::from_window(&app)"), "one list of holds");
        assert!(main.contains("reread_t.borrow_mut().accept(gen_t.load(Ordering::Relaxed), file_queue_t.pushes(), stamps.as_ref(), || landing_holds(false).any())"), "the reread doesn't wait for itself");
        assert!(main.contains("let holds = landing_holds(reload_req_t.try_lock().map_or(true, |g| g.is_some()) || reread_t.borrow().wanted_or_running());"), "a Date taken landing waits for it");
    }

    /// R13: stamps can't see OneDrive free a file or download it, and every reread used to heal the flag. On
    /// an unchanged-stamp listing, a flag that differs by path from the list on screen counts as changed, or
    /// a freed photo would stay marked local. Through the real listing on a real generated folder, either
    /// way round, and a pair by either of its files. A photo only one list has isn't a flip.
    ///
    /// FALSIFIER (L28): `cloud_flag_differs` always says no, or reads the wrong way (`==`): the rows below.
    /// `run_listing` doesn't ask it. `accept` doesn't read `cloud_flipped`: the acceptance rows.
    #[test]
    fn an_unchanged_listing_with_a_cloud_flip_counts_as_changed() {
        let dir = std::env::temp_dir().join(format!("falcon_cloud_flip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["ANT.png", "BEE.png"] {
            std::fs::write(dir.join(name), []).unwrap();
        }
        let scan = falcon_decode::scan_folder_with_metadata(&dir).unwrap();
        let (live, stamps) = (scan.shots.clone(), support::listing_stamps(scan.metadata));
        let accept_with = |on_screen: Vec<Shot>| {
            let input = file_queue::ListingInput { live: Arc::new(on_screen), live_sort: None, sort_lru: Vec::new(), sort_cap: 0, inline_dates: false };
            let mut reread = file_queue::Reread::default();
            reread.want(dir.clone(), true);
            let ticket = reread.start(1, 0).expect("wanted");
            *ticket.arrived.lock().unwrap() = Some(file_queue::run_listing(&dir, input, falcon_decode::scan_folder_with_metadata));
            match reread.accept(1, 0, Some(&stamps), || false) {
                file_queue::Acceptance::Unchanged(_) => "unchanged",
                file_queue::Acceptance::Land(..) => "landed",
                _ => "something else",
            }
        };
        assert_eq!(accept_with(live.clone()), "unchanged", "the same flags");
        let mut freed = live.clone();
        freed[1].cloud_placeholder = true;
        assert_eq!(accept_with(freed), "landed", "BEE's flag differs");
        // The table, by path.
        let (a, b) = (vec![shot("ANT"), pair("BEE")], vec![shot("ANT"), pair("BEE")]);
        assert!(!support::cloud_flag_differs(&a, &b));
        let local_now = vec![shot("ANT"), Shot { cloud_placeholder: true, ..pair("BEE") }];
        assert!(support::cloud_flag_differs(&local_now, &b), "the other way round");
        assert!(support::cloud_flag_differs(&b, &local_now), "either way");
        let half = vec![Shot { raw: None, cloud_placeholder: true, ..pair("BEE") }];
        assert!(support::cloud_flag_differs(&b, &half), "a pair, by its JPEG");
        let raw_half = vec![Shot { jpg: None, cloud_placeholder: true, ..pair("BEE") }];
        assert!(support::cloud_flag_differs(&b, &raw_half), "a pair, by its RAW");
        assert!(!support::cloud_flag_differs(&a, &[Shot { cloud_placeholder: true, ..shot("NEW") }]), "a photo the list doesn't have is the stamps' to see");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R13, `reload`'s rule on the new thread: a reread never starts a date read, and with no read running it
    /// reads a few dates, so a photo saved beside the original sorts to its real date. During a read it
    /// reads none, so the order on screen changes once. It sorts by the folder's live sort.
    ///
    /// FALSIFIER (L28): `reopen_scan` ignores `inline_dates`, either way: its row. Reads for any sort: the
    /// name row. `run_listing` starts a date read: `taken_pass`. The handler passes a constant: the wiring row.
    #[test]
    fn a_reread_reads_a_few_dates_only_with_no_read_running() {
        let list_with = |folder: &str, sort: SortMethod, inline_dates: bool| {
            let dir = std::env::temp_dir().join(format!("falcon_reread_dates_{folder}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("noexif.jpg"), b"not a real jpeg").unwrap();
            let input = file_queue::ListingInput { live: Arc::new(Vec::new()), live_sort: Some(support::encode_sort_spec(sort, false)), sort_lru: Vec::new(), sort_cap: 10, inline_dates };
            let listed = file_queue::run_listing(&dir, input, falcon_decode::scan_folder_with_metadata);
            let done = listed.result.expect("the folder reads");
            let scan = falcon_decode::scan_folder_with_metadata(&dir).unwrap();
            let unread = support::read_dates_inline(&scan.shots, &scan.metadata, 1);
            let _ = std::fs::remove_dir_all(&dir);
            (done.sort.0, done.taken_pass.is_some(), unread)
        };
        assert_eq!(list_with("none_running", SortMethod::Taken, true), (SortMethod::Taken, false, 0), "with no read running it read this one's date, and started no read");
        assert_eq!(list_with("one_running", SortMethod::Taken, false), (SortMethod::Taken, false, 1), "during a read it reads none");
        assert_eq!(list_with("by_name", SortMethod::Name, true), (SortMethod::Name, false, 1), "another sort reads nothing");
        let main = main_code();
        let start = section(&main, "let start_listing: Rc<dyn Fn(file_queue::ListingTicket)> = {", "\n    };");
        assert!(start.contains("inline_dates: taken_pass.borrow().is_none(),"), "the same question `reload` asks");
        assert!(start.contains("live_sort: folder_attrs.borrow().sort.clone(),"), "the live sort");
    }

    /// R13: a clean delete's reread waits for a quiet second, counted from the last report, so a burst lists
    /// once. A failure, a Recover and a Move list at once: someone is looking for a photo. Nothing lists
    /// while a job or a Move runs, so it reads what they did. The bin check's second is the same one.
    ///
    /// FALSIFIER (L28): a clean delete asks at once: the 999 row. `due` skips the second, or the report doesn't
    /// restart it: its rows. A failure or a Recover waits: their rows. `due` ignores a Move running: its row.
    /// A clean report takes `at_once` away from a failure's ask: the failure-then-clean row.
    #[test]
    fn a_clean_delete_lists_after_a_quiet_second_and_a_failure_at_once() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let due = |cull: &Culling, ms: u64| cull.reread.borrow().due(at(ms), cull.queue.quiet(), false);
        let press = |cull: &mut Culling, photo: &str, ms: u64| {
            cull.press_del(&[photo]);
            cull.until_quiet();
            cull.fold_reports_at(String::new(), at(ms));
        };
        press(&mut cull, "ANT", 0);
        assert!(!due(&cull, 999), "a clean delete waits");
        assert!(due(&cull, 1_000), "a quiet second");
        press(&mut cull, "BEE", 500);
        assert!(!due(&cull, 1_499), "the next delete starts it again, so a burst lists once");
        assert!(due(&cull, 1_500));
        assert!(!cull.reread.borrow().quiet_a_second(at(1_499)), "the bin check's is the same second");
        assert!(cull.reread.borrow().quiet_a_second(at(1_500)));
        assert!(!cull.reread.borrow().due(at(9_000), true, true), "a Move or Empty is running");
        assert!(!cull.reread.borrow().due(at(9_000), false, false), "a job is running");
        // A failure: at once, and a clean delete right behind it doesn't make it wait.
        cull.lock_file("CAT.png");
        press(&mut cull, "CAT", 1_000);
        assert!(due(&cull, 1_000), "the photo is coming back");
        cull.arrow_to("DOG");
        press(&mut cull, "DOG", 1_100);
        assert!(due(&cull, 1_100), "still at once");
        cull.settle();
        // A Recover, at once.
        assert_eq!(cull.press_z(), "job");
        cull.until_quiet();
        cull.fold_reports_at(String::new(), at(5_000));
        assert!(due(&cull, 5_000));
        cull.settle();
        // A Move's request, at once: `true` in the tick.
        let main = main_code();
        assert!(section(&main, "if let Some(p) = reload_req_t.try_lock().ok().and_then(|mut g| g.take()) {", "let landing_holds").contains("reread_t.borrow_mut().want(p, true);"));
    }

    /// R14: the check reports a file that isn't in the bin. Its record goes or shrinks, its row stops offering
    /// Recover, and an amber line says it can't be brought back. Another delete's record is untouched. A
    /// Recover from then on brings back only what can come back. The check asks once about each.
    ///
    /// FALSIFIER (L28): `fold_bin_check` ignores what's missing, or speaks nothing: the first row. The record
    /// stays: Ctrl+Z reaches ANT. `retire_row_offer` isn't called: the row's row. `settle_not_in_bin` keeps a
    /// whole selection's count, or `without` restores what's missing: the selection row. It names the
    /// photo for a half: the pair row. A side file alone speaks: its row. `take_bin_asks` asks twice: "once".
    /// The Mac's asks: `confirmed` is ignored in the fold.
    #[test]
    fn file_missing_from_the_bin_loses_recover_and_says_so() {
        // The words name the platform's own bin ("Trash" on the Mac), so the expected lines do too.
        let bin = platform::bin_noun();
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        cull.star("ANT", 2);
        cull.delete(&["ANT"]);
        cull.delete(&["BEE"]);
        let (ant, bee) = (cull.files.borrow().deletes[0].seq, cull.files.borrow().deletes[1].seq);
        cull.log.borrow_mut().extend(cull.pending.borrow_mut().drain(..));
        assert!(cull.log.borrow().iter().all(|e| e.recover), "both offer Recover, on the job's word");
        cull.io.0.lock().unwrap().gone_from_bin.insert("ANT.png".into());
        assert!(cull.check_bin(), "a row stopped offering");
        let rows = cull.rows_so_far();
        assert_eq!(rows, [(format!("ANT isn't in the {bin}, so it can't be brought back."), 2, false)], "amber, and no pill");
        assert!(!cull.files.borrow().has_delete(ant), "ANT's record goes");
        assert!(cull.files.borrow().has_delete(bee), "BEE's is untouched");
        let offers: Vec<(Option<u64>, bool)> = cull.log.borrow().iter().map(|e| (e.seq, e.recover)).collect();
        // Commit 10 (R32): a line's life is its record's, so ANT's row of one goes, and the amber line says why.
        assert_eq!(offers, [(Some(bee), true)], "its row goes, and BEE's keeps its Recover");
        assert!(cull.files.borrow_mut().take_bin_asks().is_empty(), "once each");
        assert!(!cull.check_bin(), "and nothing more to say");
        assert!(cull.rows_so_far().is_empty());
        cull.settle();
        assert_eq!(cull.ctrl_z(), "back BEE");
        assert_eq!(cull.ctrl_z(), "nothing", "ANT can't come back, and its star's step went with it");

        // A selection: the one that's gone is named, and the rest come back with one press.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.delete(&["ANT", "BEE", "CAT"]);
        cull.io.0.lock().unwrap().gone_from_bin.insert("BEE.png".into());
        cull.check_bin();
        assert_eq!(cull.rows_so_far().last().map(|r| r.0.clone()), Some(format!("BEE isn't in the {bin}, so it can't be brought back.")));
        {
            let files = cull.files.borrow();
            let rec = &files.deletes[0];
            assert_eq!((rec.photos, rec.name.as_str()), (2, "2 photos"), "it counts what can come back");
            assert_eq!(rec.not_in_bin, [PathBuf::from("BEE.png")]);
        }
        cull.settle();
        assert_eq!(cull.ctrl_z(), "back ANT CAT");
        assert_eq!(cull.names(), ["ANT", "CAT", "DOG"], "BEE doesn't");
        assert_eq!(cull.ctrl_z(), "nothing");
        // Two of them, in one line.
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.delete(&["ANT", "BEE", "CAT"]);
        for f in ["ANT.png", "BEE.png"] {
            cull.io.0.lock().unwrap().gone_from_bin.insert(f.into());
        }
        cull.check_bin();
        assert_eq!(cull.rows_so_far().last().map(|r| r.0.clone()), Some(format!("2 photos aren't in the {bin}, so they can't be brought back.")));
        // All of them: the record goes.
        cull.io.0.lock().unwrap().gone_from_bin.insert("CAT.png".into());
        let seq = cull.files.borrow().deletes[0].seq;
        cull.files.borrow_mut().deletes[0].unconfirmed = true;
        cull.check_bin();
        assert!(!cull.files.borrow().has_delete(seq));

        // One file of a pair: that file is named, the photo keeps its Recover, and its star's step stays with
        // the record. Then the rest goes too, and the half that stayed has its step back.
        let mut cull = Culling::with_files(&["IMG_6.png", "IMG_7.CR3", "IMG_7.png", "IMG_8.png"]);
        cull.star("IMG_7", 3);
        cull.arrow_to("IMG_7");
        cull.delete(&["IMG_7"]);
        cull.io.0.lock().unwrap().gone_from_bin.insert("IMG_7.CR3".into());
        cull.check_bin();
        assert_eq!(cull.rows_so_far().last().map(|r| r.0.clone()), Some(format!("IMG_7.CR3 isn't in the {bin}, so it can't be brought back.")));
        assert_eq!(cull.files.borrow().deletes[0].steps.len(), 1);
        cull.settle();
        assert_eq!(cull.ctrl_z(), "back IMG_7", "the JPEG");
        assert_eq!(cull.names(), ["IMG_6", "IMG_7", "IMG_8"]);
        assert!(cull.shots[cull.place("IMG_7")].raw.is_none(), "with no RAW");
        assert_eq!(cull.ctrl_z(), "star IMG_7");

        // A side file only: nothing a person would call a photo. The record shrinks, and nothing speaks.
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.delete(&["ANT"]);
        let seq = cull.files.borrow().deletes[0].seq;
        cull.rows_so_far();
        let found = vec![file_queue::BinFound { seq, missing: vec![PathBuf::from("ANT.xmp")] }];
        assert!(!cull.fold_found(found));
        assert!(cull.rows_so_far().is_empty(), "silent");
        assert!(cull.files.borrow().has_delete(seq));

        // The Mac's job has confirmed already: nothing to ask.
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.io.0.lock().unwrap().jobs_confirm = true;
        cull.delete(&["ANT"]);
        assert!(cull.files.borrow_mut().take_bin_asks().is_empty());
        // A listing that can't be had: the check says nothing, and the re-stat stands (`FileIo::missing_from_bin`).
        assert!(queue_code().contains("log_event(\"delete-recycle: the bin could not be listed — the re-stat's verdict stands\");\n                return Vec::new();"));
    }

    /// R14: Del then Recover in one burst gives no amber line. Whenever the press comes, the file the check
    /// can't find is back in the folder, and nothing's wrong. Before the delete's report; after the report and
    /// before the check; and with the check reading the bin as the photo comes back, on the real thread.
    /// Twice. A delete in the same burst that stays in the bin is still asked about.
    ///
    /// FALSIFIER (L28): `settle_not_in_bin` speaks with no record, or `fold_bin_check` pushes before asking it: an
    /// amber row. `take_bin_asks` keeps a taken record: nothing to see, as it's gone from the list. `note_unconfirmed`
    /// runs before `settle_delete`, or skips the record: the unasked row.
    #[test]
    fn del_then_recover_in_one_burst_gives_no_amber_line() {
        let amber = |cull: &Culling| cull.pending.borrow().iter().filter(|e| e.kind == 2).count();
        for _ in 0..2 {
            // Before the delete's report: its record has gone, and it's never asked about.
            let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
            let held = held_lock(&cull);
            cull.press_del(&["ANT"]);
            assert_eq!(cull.press_z(), "job");
            drop(held);
            assert_eq!(cull.settle(), "back ANT");
            assert!(cull.files.borrow_mut().take_bin_asks().is_empty(), "nothing to ask");
            assert_eq!(amber(&cull), 0);

            // After the report, before the check.
            cull.delete(&["ANT"]);
            assert_eq!(cull.ctrl_z(), "back ANT");
            assert!(cull.files.borrow_mut().take_bin_asks().is_empty(), "a Recover takes its seq out of what's pending");
            assert_eq!(amber(&cull), 0);

            // While the check reads the bin: it finds ANT isn't there, because it's back in the folder.
            static GATE: Mutex<()> = Mutex::new(());
            let io = cull.io.clone();
            let check = file_queue::BinCheck::with_runner(Box::new(move |asks| {
                let _held = GATE.lock().unwrap_or_else(|e| e.into_inner());
                file_queue::FileIo::missing_from_bin(&io, asks)
            }));
            cull.delete(&["ANT"]);
            cull.delete(&["BEE"]);
            let bee = cull.files.borrow().deletes[1].seq;
            let reading = GATE.lock().unwrap();
            check.start(cull.files.borrow_mut().take_bin_asks());
            let ant = cull.files.borrow().deletes[0].seq;
            assert_eq!(cull.recover(ant), "back ANT");
            assert!(cull.io.0.lock().unwrap().files.contains("ANT.png"));
            drop(reading);
            let deadline = Instant::now() + Duration::from_secs(20);
            while check.running() {
                assert!(Instant::now() < deadline, "the check should end");
                std::thread::sleep(Duration::from_millis(1));
            }
            let found = check.take_found();
            assert_eq!(found.len(), 1, "it did find ANT missing");
            cull.fold_found(found);
            assert_eq!(amber(&cull), 0, "and nothing speaks: ANT is back");
            assert!(cull.files.borrow().has_delete(bee), "BEE's delete stands");
            assert_eq!(cull.names(), ["ANT", "CAT"]);
        }
        // Not a job's worth of time later, the delete that stays is asked about.
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.delete(&["ANT"]);
        let asks = cull.files.borrow_mut().take_bin_asks();
        assert_eq!(asks.iter().map(|a| a.files.clone()).collect::<Vec<_>>(), [vec![PathBuf::from("ANT.png")]], "what the job says left");
    }

    /// Stage A left this for here. Star, Del, Ctrl+Z, Ctrl+Z at speed: the second press waits for the first's
    /// photo, which now lasts until its listing lands, and the steps are given back at that acceptance,
    /// not when the listing was asked for. Meanwhile the surfaces read what the press would do: they hold
    /// as they are, rather than greying or naming the step before. Unchanged, failed or forgotten, the wait
    /// ends: nothing stays stuck on a landing that isn't coming.
    ///
    /// FALSIFIER (L28): steps are cleared at the request: "star ANT" is gone and the press reaches the older one.
    /// `no_landing` forgets them on an unchanged listing: the half row. It keeps them on a failure: the press
    /// waits for ever. The tick's surfaces stop holding, or hold after the first publish: the wiring row.
    #[test]
    fn a_press_waits_for_the_listing_and_the_surfaces_hold() {
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG"]);
        cull.star("DOG", 5); // the older star
        cull.star("ANT", 3);
        cull.delete(&["ANT"]);
        assert_eq!(cull.press_z(), "job");
        let surfaces_hold = |cull: &Culling| support::undo_waits_for_landing(cull.arm(), cull.files.borrow().newest_returning());
        assert!(surfaces_hold(&cull), "while the job runs");
        cull.until_quiet();
        cull.fold_reports(String::new());
        assert_eq!(cull.files.borrow().returning.len(), 1);
        let ticket = cull.start_listing(Instant::now()).expect("a Recover lists at once");
        assert_eq!(cull.press_z(), "waits", "the reread has started, and hasn't landed");
        assert_eq!(cull.accept(false), "nothing", "still listing");
        assert!(surfaces_hold(&cull), "while it lists");
        assert_eq!(cull.files.borrow().returning.len(), 1, "they wait for the landing");
        cull.deliver(&ticket);
        assert_eq!(cull.accept(true), "nothing", "a menu is open");
        assert!(surfaces_hold(&cull), "while it waits");
        assert_eq!(cull.accept(false), "landed");
        assert!(!surfaces_hold(&cull), "the step is on the stack");
        assert_eq!(cull.arm(), Some(support::UndoArm::Stack(2)), "and the surfaces name it");
        assert_eq!(cull.on, "ANT");
        cull.waiting = 0;
        assert_eq!(cull.press_z(), "star ANT", "the press that waited");
        assert_eq!(cull.stars["DOG"], 5);

        // An unchanged listing: the photo was never out of the list by the time it came back, as far as
        // its stamps can tell, and the steps go back all the same. Its folder couldn't list: they're gone.
        cull.star("BEE", 2);
        cull.files.borrow_mut().returning.extend(support::park_steps(&mut cull.undo.borrow_mut(), &[(cull.place("BEE"), "BEE")]));
        assert!(surfaces_hold(&cull));
        cull.list_now();
        assert_eq!(cull.accept(false), "unchanged");
        assert_eq!(cull.ctrl_z(), "star BEE", "on the list on screen");
        cull.star("BEE", 2);
        cull.files.borrow_mut().returning.extend(support::park_steps(&mut cull.undo.borrow_mut(), &[(cull.place("BEE"), "BEE")]));
        cull.reread.borrow_mut().want(PathBuf::from("folder"), true);
        let ticket = cull.start_listing(Instant::now()).expect("at once");
        *ticket.arrived.lock().unwrap() = Some(file_queue::Listed { result: Err(std::io::Error::other("offline")), cloud_flipped: false, warn: None, took: Duration::ZERO });
        assert_eq!(cull.accept(false), "failed");
        assert!(!surfaces_hold(&cull), "M2: the list stays, and the wait ends");
        assert_eq!(cull.press_z(), "star DOG", "with nothing to give those steps");
        // A folder left, with its reread still wanted: the same, and its name goes.
        let mut cull = Culling::new(&["ANT", "BEE"]);
        cull.star("ANT", 1);
        cull.delete(&["ANT"]);
        assert_eq!(cull.press_z(), "job");
        cull.until_quiet();
        cull.fold_reports(String::new());
        cull.dir = "another folder".into();
        cull.settle();
        assert!(!surfaces_hold(&cull), "a folder left holds nothing up");
        assert!(cull.landing_name.borrow().is_none());
        assert_eq!(cull.names(), ["BEE"], "and lands nothing");

        let main = main_code();
        let surfaces = section(&main, "'surfaces: {", "app.set_can_cull_redo(redo_armed);");
        let published = surfaces.split_once("app.set_can_cull_undo(").expect("the first publish").0;
        let held = section(published, "if support::undo_waits_for_landing(arm, hist.newest_returning()) {", "break 'surfaces;");
        assert!(held.contains("let (target, redo_target) = rim_targets_t.get();") && held.contains("app.set_undo_target_row(support::seq_row_index(log.iter().map(|e| e.seq), target));"), "before anything is published, and the rim follows its stamp");
    }

    /// The reread's wiring, which only runs in the app. A Move's request is a request. The acceptance is at
    /// the tick's top, ahead of the snapshot the rest of the tick reads, and a real landing goes through
    /// `apply_scan`, retiring an older reopen, as `reload` does. The listing is on its own thread, started only
    /// where the tick says it's due. A sort pick waits for the queue, and the tick stays awake for a listing.
    ///
    /// FALSIFIER (L28): drop or move any of the lines below.
    #[test]
    fn the_reread_runs_off_the_interface_thread() {
        let main = main_code();
        let tick = main.split_once("for done in file_queue_t.take_results() {").expect("the tick's reports").1;
        let later = |s: &str| tick.find(s).unwrap_or_else(|| panic!("the tick: {s}"));
        let request = section(tick, "if let Some(p) = reload_req_t.try_lock().ok().and_then(|mut g| g.take()) {", "let landing_holds");
        assert!(!request.contains("reload_t"), "a request lists nothing on this thread:\n{request}");
        for (first, then) in [
            ("reread_t.borrow_mut().want(p, true);", "reread_t.borrow_mut().forget_unless("),
            ("reread_t.borrow_mut().forget_unless(", "let accepted = {"),
            ("let accepted = {", "for _ in 0..undo_waiting_t.take() {"),
            ("for _ in 0..undo_waiting_t.take() {", "let sort_asked = file_queue_t.quiet()"),
            ("let sort_asked = file_queue_t.quiet()", "if reread_t.borrow().due(now, file_queue_t.quiet(), position_op_t.load(Ordering::SeqCst)) {"),
            ("if reread_t.borrow().due(now, file_queue_t.quiet(), position_op_t.load(Ordering::SeqCst)) {", "mk!(\"reload_req\");"),
        ] {
            assert!(later(first) < later(then), "`{first}` comes before `{then}`");
        }
        let forgot = section(tick, "reread_t.borrow_mut().forget_unless(", "let accepted = {");
        assert!(forgot.contains("last_scanned_t.borrow().still_on_screen(&dir.display().to_string())") && forgot.contains("recover_over(file_queue::no_landing(&file_history_t, &undo_stack_t, &landing_name_t, None));"), "the folder guard (N7)");
        let landing = section(tick, "file_queue::Acceptance::Land(mut done, warn) => {", "apply_scan_t(*done);");
        assert!(landing.contains("reopen_pending_t.borrow_mut().superseded_by_rescan(scan_id_t.load(Ordering::Relaxed))") && landing.contains("support::begin_explicit_scan(&scan_id_t, &scan_done_t);"), "R3, as `reload`");
        assert!(landing.contains("*sel_status_slot_t.lock().unwrap_or_else(|e| e.into_inner()) = Some(w);"), "a warning only from a listing that lands");
        assert!(section(tick, "file_queue::Acceptance::Unchanged(delete_asks) => {", "file_queue::Acceptance::Land").contains("recover_over(file_queue::no_landing(&file_history_t, &undo_stack_t, &landing_name_t, Some(&on_screen)));"), "steps go back on the list on screen");
        let start = section(tick, "if reread_t.borrow().due(", "mk!(\"reload_req\");");
        assert!(start.contains("reread_t.borrow_mut().start(gen_t.load(Ordering::Relaxed), file_queue_t.pushes())") && start.contains("start_listing_t(ticket);"), "from the generation and the pushes it will be judged by");
        let queue = queue_code();
        assert!(section(&queue, "pub(crate) fn spawn_listing(", "\n}").contains("falcon_decode::scan_folder_with_metadata_cancellable(d, moved_on)"), "a listing that's out of date stops reading");
        assert!(section(&queue, "pub(crate) fn spawn_listing_with(", "\n}").contains(".name(\"falcon-reread\".into()).spawn("), "on its own thread");
        assert!(section(&queue, "pub(crate) fn run_listing(", "\n}").contains("support::reopen_scan(0, dir.to_path_buf(), None, input.live_sort, &input.sort_lru, input.sort_cap, None, input.inline_dates, &warn, read)"), "id 0, no photo asked for, no date read started");
        assert!(section(&main, "let reload: Rc<dyn Fn(PathBuf)> = {", "\n    };").contains("scan_folder_with_metadata(&dir)"), "a sort pick's is still on this thread, as logic.md says");
        assert!(main.contains("else if reread_t.borrow().working() { \"reread\" }"), "the tick stays awake for it");
        assert!(section(&queue, "pub(crate) fn fold_file_done(", "match done {").contains("to.reread.borrow_mut().note_report(now);"), "every report restarts the quiet second");
    }

    /// The refresh on coming back: the person goes to another program and comes back to Falcon, as the tick
    /// reads `falcon_is_active`, through the tick's own ask. `true`: it asked.
    fn away_and_back(cull: &Culling, front: &Cell<Option<bool>>) -> bool {
        file_queue::ask_on_return(front, Some(false), &cull.dir, &cull.reread);
        file_queue::ask_on_return(front, Some(true), &cull.dir, &cull.reread)
    }

    /// …and the return's listing, read now and there for the tick's next acceptance.
    fn back_and_listed(cull: &Culling, front: &Cell<Option<bool>>) {
        assert!(away_and_back(cull, front), "a return asks");
        let ticket = cull.start_listing(Instant::now()).expect("at once");
        cull.deliver(&ticket);
    }

    /// The refresh on coming back must not land an older or a half list over a folder that is still opening. The
    /// existing holds cover it, and this row proves it through the return's ask, the real reread and its landing:
    /// - The first-photo stage: the folder's open has landed its first photos with no stamps yet (L43), so any
    ///   listing reads as changed. It waits while the photo opens (`opening_photo`), the open's full list lands,
    ///   which makes the listing out of date, and the one after it finds nothing more. Twice (L53).
    /// - Another folder opening while this one is on screen (`scanning`): the return asks for the folder on
    ///   screen, its listing waits, and once the new folder lands it is forgotten (N7), never landed.
    /// - A reopen of the folder on screen still reading (`reopen_pending`): the same as the first.
    ///
    /// FALSIFIER (L28): `LandingHolds::any` leaves out `opening_photo`: the first stage lands over the open. It
    /// leaves out `scanning` or `reopen_pending`: those rows. `accept` ignores the generation: "unchanged" where
    /// the open's landing made it "discarded". `forget_unless` leaves a listing in flight alone: the other folder.
    #[test]
    fn coming_back_while_a_folder_opens_lands_nothing_over_it() {
        let front = Cell::new(Some(true));
        let mut cull = Culling::new(&["ANT", "BEE", "CAT"]);
        let held_by = |holds: support::LandingHolds| holds.any();
        let opening = support::LandingHolds { opening_photo: true, ..Default::default() };
        for saved in ["ANT2", "ANT3"] {
            *cull.stamps.borrow_mut() = None; // the first-photo stage's landing
            cull.save_beside(&format!("{saved}.png"));
            back_and_listed(&cull, &front);
            let gen = cull.gen.get();
            for _ in 0..3 {
                assert_eq!(cull.accept(held_by(opening)), "nothing", "{saved}: the photo is still opening");
                assert_eq!(cull.gen.get(), gen, "{saved}: nothing lands over the open");
            }
            let full = cull.listing();
            cull.landed(full); // the open's own full list
            assert_eq!(cull.accept(false), "discarded", "{saved}: older than the open's landing");
            let again = cull.start_listing(Instant::now()).expect("it lists again, at once");
            cull.deliver(&again);
            assert_eq!(cull.accept(false), "unchanged", "{saved}: the open already listed it");
            assert_eq!(cull.names().iter().filter(|n| **n == saved).count(), 1, "{saved}: once");
            assert_eq!(cull.on, "ANT", "{saved}: and the person is on the photo they opened");
        }

        // Another folder opening, this one still on screen.
        cull.save_beside("ANT4.png");
        back_and_listed(&cull, &front);
        let gen = cull.gen.get();
        assert_eq!(cull.accept(held_by(support::LandingHolds { scanning: true, ..Default::default() })), "nothing", "the other folder is scanning");
        cull.dir = "other".into(); // it lands: the scan's folder memory names it
        let dir = cull.dir.clone();
        assert!(cull.reread.borrow_mut().forget_unless(|d| d.display().to_string() == dir), "the folder left is forgotten");
        assert_eq!(cull.accept(false), "nothing", "nothing of it lands");
        assert!(!cull.reread.borrow().wanted_or_running());
        assert_eq!(cull.gen.get(), gen);
        cull.dir = "folder".into();

        // A reopen of the folder on screen, still reading.
        let mut reopen = support::PendingReopen::default();
        reopen.start(9, None);
        let reopening = support::LandingHolds { reopen_pending: reopen.holds_landing(Instant::now()), ..Default::default() };
        cull.save_beside("ANT5.png");
        back_and_listed(&cull, &front);
        assert_eq!(cull.accept(held_by(reopening)), "nothing", "the reopen is still reading");
        let reopened = cull.listing();
        cull.landed(reopened);
        assert_eq!(cull.accept(false), "discarded");
        let again = cull.start_listing(Instant::now()).expect("it lists again");
        cull.deliver(&again);
        assert_eq!(cull.accept(false), "unchanged");
        assert!(cull.names().contains(&"ANT5") && cull.names().contains(&"ANT4"), "the folder on screen is up to date");
    }

    /// The refresh on coming back while Falcon is still at work on the folder: `due` and the holds cover each one,
    /// through the return's ask, the real reread, presses and jobs, and the landing:
    /// - Apply rotations is writing: the listing waits for it, then lands, and the person stays. Twice.
    /// - Move rejected is under way: no listing starts until it has finished, and its reread and the return's are
    ///   one listing. A listing that started before it read the folder half moved: the Move's ask makes it out of
    ///   date, and the one after it lands.
    /// - A delete in the queue: no listing while its job runs, and one that started before the press is older
    ///   than it. Twice, as culling is.
    ///
    /// FALSIFIER (L28): `LandingHolds::any` leaves out `applying_rotations` or `position_op`: it lands during Apply
    /// or the Move. `due` ignores `position_op`: a listing during the Move. `due` ignores the queue: a listing
    /// during the job. `accept` ignores the generation and the pushes: the listing older than the press lands, and
    /// the deleted photo is back.
    #[test]
    fn coming_back_during_apply_a_move_or_a_delete_waits_for_it() {
        let front = Cell::new(Some(true));
        let mut cull = Culling::new(&["ANT", "BEE", "CAT", "DOG", "EEL"]);
        let applying = support::LandingHolds { applying_rotations: true, ..Default::default() };
        for saved in ["ANT2", "ANT3"] {
            cull.save_beside(&format!("{saved}.png"));
            back_and_listed(&cull, &front);
            let gen = cull.gen.get();
            assert_eq!(cull.accept(applying.any()), "nothing", "{saved}: Apply rotations is writing");
            assert_eq!(cull.gen.get(), gen);
            assert_eq!(cull.accept(false), "landed", "{saved}: Apply has finished");
            assert!(cull.names().contains(&saved) && cull.on == "ANT", "{saved}: listed, and the person stays");
        }

        cull.save_beside("ANT4.png");
        assert!(away_and_back(&cull, &front));
        assert!(!cull.reread.borrow().due(Instant::now(), cull.queue.quiet(), true), "no listing while a Move runs");
        cull.move_rejected(&["CAT"]);
        assert_eq!(cull.names(), ["ANT", "ANT2", "ANT3", "ANT4", "BEE", "DOG", "EEL"], "one listing after the Move");
        cull.save_beside("ANT5.png");
        back_and_listed(&cull, &front);
        let moving = support::LandingHolds { position_op: true, ..Default::default() };
        assert_eq!(cull.accept(moving.any()), "nothing", "it waits for the Move");
        cull.move_rejected(&["DOG"]);
        assert_eq!(cull.names(), ["ANT", "ANT2", "ANT3", "ANT4", "ANT5", "BEE", "EEL"]);
        assert_eq!(cull.on, "ANT");

        for (photo, saved) in [("BEE", "ANT6"), ("EEL", "ANT7")] {
            cull.save_beside(&format!("{saved}.png"));
            back_and_listed(&cull, &front);
            let held = held_lock(&cull);
            cull.press_del(&[photo]);
            assert_eq!(cull.accept(false), "discarded", "{photo}: older than the press");
            assert!(!cull.names().contains(&photo), "{photo} stays out");
            assert!(away_and_back(&cull, &front), "{photo}: and back again while its job runs");
            assert!(cull.start_listing(Instant::now() + Duration::from_secs(60)).is_none(), "{photo}: no listing while the queue runs");
            drop(held);
            cull.settle();
            assert!(!cull.names().contains(&photo) && cull.names().contains(&saved), "{photo}: gone, and {saved} listed");
            assert_eq!(cull.on, "ANT");
        }
    }

    /// R46: the blue edge is only for a photo the person brought back. Photos another program saved while the
    /// person was away, far left, far right and beside the photo on screen, arrive at the return's landing and
    /// nothing points at them. Twice. Then N040, moved and brought back by Undo move while on N060, lights the
    /// left edge, and a return that lists one more photo keeps it on N040, through the landing's map.
    ///
    /// FALSIFIER (L28): light every photo a landing adds (`blue_edge`'s arrivals in place of `brought`): the first
    /// rows. A landing that keeps the order forgets what is lit: the last row.
    #[test]
    fn coming_back_points_no_blue_edge_at_what_another_program_saved() {
        let front = Cell::new(Some(true));
        let mut cull = numbered("N060", &["N010", "N020", "N030", "N061", "N070", "N075"]);
        for saved in [["N030", "N075", "N061"], ["N010", "N070", "N020"]] {
            for photo in saved {
                cull.save_beside(&format!("{photo}.png"));
            }
            assert!(away_and_back(&cull, &front));
            cull.settle();
            assert!(saved.iter().all(|p| cull.names().contains(p)), "{saved:?}: listed");
            assert_eq!(cull.on, "N060");
            assert_eq!(cull.edge_photos(), Vec::<&str>::new(), "{saved:?}: nothing points at them");
            assert_eq!(cull.strip_edges(STRIP_HALF), (None, None));
        }
        cull.arrow_to("N041");
        cull.move_rejected(&["N040"]);
        cull.arrow_to("N060");
        assert_eq!(cull.ctrl_z(), "moved back N040");
        assert_eq!(cull.edge_photos(), ["N040"], "premise: brought back off the strip");
        cull.save_beside("N045-Edit.png");
        assert!(away_and_back(&cull, &front));
        cull.settle();
        assert!(cull.names().contains(&"N045-Edit"));
        assert_eq!(cull.edge_photos(), ["N040"], "still about N040");
        assert_eq!(cull.strip_edges(STRIP_HALF), (Some("N040".to_string()), None), "and the left edge goes to it");
    }

    /// The refresh on coming back, wired: the tick reads `falcon_is_active` into the ask once a tick, beside its
    /// last known answer, after a Move's ask and before the reread's folder guard and acceptance, with the folder
    /// the guard asks about (`last_scanned`, not `cur_dir`). The ask only asks: no file is opened, listed or read
    /// here. What lists is the reread's own listing, which reads names, sizes and dates and asks for no photo
    /// (`the_reread_runs_off_the_interface_thread` pins it: `reopen_scan` with id 0, no photo asked for, no date
    /// read started), never reads a cloud-only file's bytes (`the_sniff_never_touches_a_cloud_placeholder`, in
    /// `falcon-decode`), and reads a few dates inline only of photos that aren't cloud-only
    /// (`a_resort_never_reads_a_file_freed_up_since_the_scan`). So a return downloads nothing.
    ///
    /// FALSIFIER (L28): the tick doesn't call it; the ask asks to wait the quiet second, or asks with no folder on
    /// screen.
    #[test]
    fn coming_back_asks_from_the_tick_and_lists_only() {
        let main = main_code();
        assert_eq!(main.matches("let in_front_t = Cell::new(None::<bool>);").count(), 1, "one last known answer, beside the tick's state");
        let call = "file_queue::ask_on_return(&in_front_t, background_memory::falcon_is_active(), last_scanned_t.borrow().dir(), &reread_t);";
        assert_eq!(main.matches(call).count(), 1, "the tick asks once a tick, for the folder on screen");
        let tick = main.split_once("for done in file_queue_t.take_results() {").expect("the tick's reports").1;
        let at = |s: &str| tick.find(s).unwrap_or_else(|| panic!("the tick: {s}"));
        assert!(at("reread_t.borrow_mut().want(p, true);") < at(call), "after a Move's ask");
        assert!(at(call) < at("reread_t.borrow_mut().forget_unless("), "before the folder guard");
        assert!(at(call) < at("let accepted = {"), "and the acceptance, so a listing out there is older than the ask");
        assert!(at(call) < at("if reread_t.borrow().due(now, file_queue_t.quiet(), position_op_t.load(Ordering::SeqCst)) {"), "so it starts in this tick");
        let queue = queue_code();
        let ask = section(&queue, "pub(crate) fn ask_on_return(", "\n}");
        assert!(ask.contains("if !came_back(was, now) || on_screen.is_empty() {") && ask.contains("reread.borrow_mut().want(PathBuf::from(on_screen), true);"), "on a return, at once, for the folder on screen");
        for reads in ["std::fs", "scan_folder", "spawn", "File::", "read_dir", "metadata"] {
            assert!(!ask.contains(reads), "the ask itself reads nothing: {reads}");
        }
    }

    /// Queue entry 6 (owner, 2 November 2026: "always allow and remove the setting"): a settings file
    /// written while "While the folder is loading" existed still loads, with either choice in it, and
    /// keeps every other setting; the next save leaves the key out. The same two steps `load_settings`
    /// takes (`serde_json::from_str`, then `normalize_loaded_settings`).
    ///
    /// FALSIFIER (L28): `#[serde(deny_unknown_fields)]` on `Settings`: the file is unreadable and every
    /// setting resets.
    #[test]
    fn a_settings_file_from_before_the_setting_went_still_loads() {
        for choice in ["false", "true"] {
            let old = format!(r#"{{"wheel_nav":false,"same_key_clear":false,"photo_open_edits":{choice},"sort_cap_tier":1,"scrub_wait_cache":true}}"#);
            let loaded = support::normalize_loaded_settings(serde_json::from_str::<Settings>(&old).expect("an old file still loads"));
            assert!(!loaded.wheel_nav && !loaded.same_key_clear && loaded.scrub_wait_cache, "{choice}: the rest is kept");
            assert_eq!(loaded.sort_cap_tier, 1, "{choice}");
            let saved = serde_json::to_string(&loaded).unwrap();
            assert!(!saved.contains("photo_open_edits"), "{choice}: the save leaves it out");
        }
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
