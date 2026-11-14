//! Delete round, R41 to R43: THE MOVEMENT. The owner, 3 November 2026, about Undo move: "#9 do not move
//! but the filmstrip and grid should update with an animation". Then, of the looks shown, A (neighbours
//! slide apart, returning photos fade in), and the same movement for Move rejected, Del and Recover.
//!
//! What a person sees: the photo on screen doesn't move or blink. When a landing changes the list, the
//! filmstrip's and the grid's tiles that stayed slide from where they were on screen to their new place in
//! about 0.2 s, on the strip's own chase curve; a photo that arrived fades in where it now is; a photo that
//! left simply goes. An arrow, a jump, a strip drag, the wheel or a scroll ends it at once. A re-sort, a
//! folder change, a landing that keeps nothing, and "Interface motion" off keep today's snap.
//!
//! How (R42). Everything is final at the landing's first frame, exactly as a plain update leaves it: the
//! list, the strip's place, the grid's scroll and every row. The movement is a paint offset per tile only:
//! `slide-x` and `slide-y` on the tile's row hold its offset at the start, and one Rust clock the tick
//! writes to the window, `tile-slide-k`, scales them down to exactly 0 (one write at the end, L48).
//! Arrivals fade by real opacity, `tile-fade-k`. The offsets are kept here by photo (its index in the list
//! since the landing, which addresses that photo until the next landing reads them again) and every
//! rebuild of a row puts them back, so a thumbnail landing mid-slide, which rewrites every row, moves
//! nothing. Never a per-tile `animate`: Slint restarts one whenever its input is marked dirty, and
//! thumbnail landings do exactly that.
//!
//! When. Read at the landing, beside `kept_view` (L54), while the old list is what every index addresses:
//! where each photo was on screen, including where a slide still running had it, so a second landing
//! carries on from where the tiles visibly are ([`SlideFrom::read`], `apply_scan`). Placed by each step,
//! the strip's and the grid's, at its first rebuild after the landing, from the place it has just settled
//! on ([`TileSlide::place_strip`], [`TileSlide::place_grid`]). Started or skipped once both have run, and
//! clocked every tick after (`tick::step_tile_slide`).

use crate::tick::{GRID_CELL_H, GRID_COL_PITCH, GRID_PAD, GRID_ROW_PITCH};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Instant;

/// The strip's cell pitch, a 120 px tile and its 8 px gap, as `tick::step_filmstrip` lays the cells out.
pub(crate) const STRIP_PITCH: f32 = 128.0;
/// R42: "Capped at 2 cells or rows". A tile from further away starts two cells (or rows) from home. The
/// strip's model and the grid's live window both hold two cells beyond each edge, so a tile drawn from
/// further off could leave a hole where nothing reaches.
pub(crate) const CAP: f32 = 2.0;
/// The strip's chase (`tick::step_filmstrip`): what's left of the way after one 60 Hz frame. R41: "the
/// strip's chase curve".
const CHASE: f32 = 0.65;
/// It ends when what's left of the largest offset is under a pixel: about 0.19 s for one cell, 0.21 s for
/// two, 11 to 13 frames. An arrival with nothing sliding fades as one cell's slide would.
const END_PX: f32 = 1.0;

/// The grid as a landing found it: its scroll, its columns, and the pending row's cells before the first
/// photo (`photo_open::pending_edges`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridWas {
    pub(crate) scroll: f32,
    pub(crate) cols: usize,
    pub(crate) leading: usize,
}

/// Where a cell of the grid is on screen: its left edge in the dock and its top edge in the viewport.
fn grid_cell(cell: usize, cols: usize, scroll: f32) -> (f32, f32) {
    let cols = cols.max(1);
    (GRID_PAD + (cell % cols) as f32 * GRID_COL_PITCH, GRID_PAD + (cell / cols) as f32 * GRID_ROW_PITCH - scroll)
}

/// What of the strip and the grid is on screen as a landing begins, `old_len` photos and the strip at
/// `film_pos`: the strip if the filmstrip shows (not immersive, not empty, not hidden), the grid if its dock
/// is open and laid out, the test its step uses (`tick::grid_refills`).
pub(crate) fn seen(app: &crate::MainWindow, old_len: usize, film_pos: f32) -> (Option<f32>, Option<GridWas>) {
    let strip = (!app.get_immersive() && !app.get_empty_state() && app.get_film_visible()).then_some(film_pos);
    let grid = crate::tick::grid_refills(app, old_len).then(|| {
        let cols = app.get_grid_cols().max(1) as usize;
        GridWas { scroll: (-app.get_grid_vp_y()).max(0.0), cols, leading: if crate::photo_open::pending_edges(app).0 { cols } else { 0 } }
    });
    (strip, grid)
}

/// What a landing saw on screen before its list changed. See the module's "When".
pub(crate) struct SlideFrom {
    /// For each photo of the new list, its place in the old one. `None`: it arrived.
    was: Vec<Option<usize>>,
    /// The strip's place (`film_pos`), if the strip was on screen.
    strip: Option<f32>,
    /// The grid's, if it was open on screen.
    grid: Option<GridWas>,
    /// Where a slide still running had the old list's tiles from their places, by old index: the strip's
    /// offset in px, the grid's.
    strip_now: HashMap<usize, f32>,
    grid_now: HashMap<usize, (f32, f32)>,
}

impl SlideFrom {
    /// `identity`: where each photo of the old list went (`None`: it left), the landing's own map
    /// (`photo_open::KeptScroll::identity`). `new_len`: how many photos the new list has. `running`: the
    /// slide on screen now, if any.
    pub(crate) fn read(identity: &[Option<usize>], new_len: usize, strip: Option<f32>, grid: Option<GridWas>, running: &TileSlide) -> Self {
        let mut was = vec![None; new_len];
        for (old, new) in identity.iter().enumerate() {
            if let Some(slot) = new.and_then(|n| was.get_mut(n)) {
                *slot = Some(old);
            }
        }
        let k = running.k;
        let now = |offset: f32| offset * k;
        Self {
            was,
            strip,
            grid,
            strip_now: if k > 0.0 { running.strip.iter().map(|(&i, &dx)| (i, now(dx))).collect() } else { HashMap::new() },
            grid_now: if k > 0.0 { running.grid.iter().map(|(&i, &(dx, dy))| (i, (now(dx), now(dy)))).collect() } else { HashMap::new() },
        }
    }

    /// A take-out and a reread landing in one tick are one landing to what's on screen, which only the
    /// first saw: its places stay, and a photo goes through both maps (`photo_open::KeptScroll::after`).
    fn then(self, later: SlideFrom) -> SlideFrom {
        SlideFrom {
            was: later.was.iter().map(|at| at.and_then(|m| self.was.get(m).copied().flatten())).collect(),
            ..self
        }
    }
}

/// What the tick checks has stayed as it was since the slide began. Anything else is the person moving
/// something, which ends it at once (R43): a navigation of any kind changes `current`, a wheel on the strip
/// its free target, a scroll of the grid or of the Review panel, a stage drag or a zoom the input clock
/// (`last_input`), and a resize the columns or the strip's width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Watch {
    pub(crate) current: usize,
    pub(crate) follow: bool,
    pub(crate) target: f32,
    pub(crate) cols: i32,
    pub(crate) strip_w: f32,
    pub(crate) input: Instant,
}

/// The slide on screen, if any, and the landing waiting for this tick's steps. Kept in the window's `Film`,
/// so the strip, the grid and `apply_scan` reach the one copy.
#[derive(Default)]
pub(crate) struct TileSlide {
    from: Option<SlideFrom>,
    /// By photo, its index in the list now: the strip's offset at the start, in px.
    strip: HashMap<usize, f32>,
    /// …the grid's, across and down.
    grid: HashMap<usize, (f32, f32)>,
    /// The photos that arrived, which fade in.
    arriving: HashSet<usize>,
    /// Whether any of it would show: a tile that moves within view, or arrives there.
    shows: bool,
    /// The largest offset, which sets when it ends.
    reach: f32,
    /// The clock: 1 at the landing's first frame, 0 when it's over.
    k: f32,
    /// What the window was last given.
    written: f32,
    last: Option<Instant>,
    watch: Option<Watch>,
}

impl TileSlide {
    /// At every landing (`apply_scan`): what it saw, or `None` for a landing that keeps nothing (a re-sort,
    /// another folder, a refused carry), which ends any slide with today's snap. The offsets of a slide
    /// that is running are in `from` now, and its tables are cleared, so no row of the new list can wear an
    /// offset that was another photo's.
    pub(crate) fn landed(&mut self, from: Option<SlideFrom>) {
        self.from = match (self.from.take(), from) {
            (Some(earlier), Some(later)) => Some(earlier.then(later)),
            (_, from) => from,
        };
        self.strip.clear();
        self.grid.clear();
        self.arriving.clear();
        self.shows = false;
        self.reach = 0.0;
        if self.from.is_none() {
            self.k = 0.0;
            self.watch = None;
        }
    }

    /// The strip's step in a landing's tick (the only call while `from` waits: `step` takes it at the tick's
    /// end), at `pos`, the place its step has settled on for this frame: an offset for each photo in the
    /// model's cells `slots`, and whether any shows within `half` cells of the middle. A photo's place now is `(i - pos)` cells from the middle, and where it was is
    /// `(j - pos then)` plus what a running slide still had it off by.
    pub(crate) fn place_strip(&mut self, pos: f32, slots: Range<i64>, len: usize, half: f32) {
        let Some(from) = &self.from else { return };
        let Some(pos_was) = from.strip else { return };
        let in_view = |at: f32| at.abs() <= half + 0.5;
        for i in slots.filter_map(|i| usize::try_from(i).ok()).filter(|&i| i < len) {
            let at = i as f32 - pos;
            match from.was.get(i).copied().flatten() {
                None => {
                    self.arriving.insert(i);
                    self.shows |= in_view(at);
                }
                Some(j) => {
                    let then = j as f32 - pos_was + from.strip_now.get(&j).copied().unwrap_or(0.0) / STRIP_PITCH;
                    let dx = (then - at).clamp(-CAP, CAP) * STRIP_PITCH;
                    if dx.abs() > 0.01 {
                        self.strip.insert(i, dx);
                        self.reach = self.reach.max(dx.abs());
                        self.shows |= in_view(at) || in_view(at + dx / STRIP_PITCH);
                    }
                }
            }
        }
    }

    /// The grid's step in a landing's tick, at its settled `scroll`: an offset for each photo in the
    /// live window's cells `cells` (counted with the pending row's `leading` cells before the first photo),
    /// and whether any shows in the `view_h` the dock shows. Capped at two cells across and two rows down.
    pub(crate) fn place_grid(&mut self, scroll: f32, cols: usize, leading: usize, cells: Range<usize>, len: usize, view_h: f32) {
        let Some(from) = &self.from else { return };
        let Some(was) = from.grid else { return };
        let in_view = |y: f32| y + GRID_CELL_H > 0.0 && y < view_h;
        for cell in cells {
            let Some(i) = cell.checked_sub(leading).filter(|&i| i < len) else { continue };
            let (x, y) = grid_cell(cell, cols, scroll);
            match from.was.get(i).copied().flatten() {
                None => {
                    self.arriving.insert(i);
                    self.shows |= in_view(y);
                }
                Some(j) => {
                    let (x0, y0) = grid_cell(j + was.leading, was.cols, was.scroll);
                    let (mx, my) = from.grid_now.get(&j).copied().unwrap_or((0.0, 0.0));
                    let dx = (x0 + mx - x).clamp(-CAP * GRID_COL_PITCH, CAP * GRID_COL_PITCH);
                    let dy = (y0 + my - y).clamp(-CAP * GRID_ROW_PITCH, CAP * GRID_ROW_PITCH);
                    if dx.abs() > 0.01 || dy.abs() > 0.01 {
                        self.grid.insert(i, (dx, dy));
                        self.reach = self.reach.max(dx.abs()).max(dy.abs());
                        self.shows |= in_view(y) || in_view(y + dy);
                    }
                }
            }
        }
    }

    /// The strip's row for photo `i`: its offset at the start and whether it arrived. Every rebuild asks.
    pub(crate) fn strip_tile(&self, i: usize) -> (f32, bool) {
        (self.strip.get(&i).copied().unwrap_or(0.0), self.arriving.contains(&i))
    }

    /// The grid's row for photo `i`.
    pub(crate) fn grid_tile(&self, i: usize) -> (f32, f32, bool) {
        let (dx, dy) = self.grid.get(&i).copied().unwrap_or((0.0, 0.0));
        (dx, dy, self.arriving.contains(&i))
    }

    /// Once a tick, after the strip and the grid. The landing that has just been placed starts, unless
    /// nothing of it would show or `hold` (R43: a held arrow, a scrub, a strip drag, banked wheel notches,
    /// a drag from the grid, or "Interface motion" off), which keep the snap. A slide that's running ends
    /// at once on `hold` or when `watch` differs from where it began, and otherwise follows the strip's
    /// chase. The clock to write to the window, only when it changed: its last write is exactly 0.
    pub(crate) fn step(&mut self, now: Instant, hold: bool, watch: Watch) -> Option<f32> {
        if self.from.take().is_some() {
            if self.shows && !hold {
                self.k = 1.0;
                self.last = Some(now);
                self.watch = Some(watch);
            } else {
                self.end();
            }
        } else if self.k > 0.0 {
            if hold || self.watch != Some(watch) {
                self.end();
            } else {
                let dt = self.last.map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f32().min(0.25));
                self.k *= CHASE.powf(dt * 60.0);
                self.last = Some(now);
                if self.k * self.reach.max(STRIP_PITCH) < END_PX {
                    self.end();
                }
            }
        }
        (self.k != self.written).then(|| {
            self.written = self.k;
            self.k
        })
    }

    /// Whether a slide is running: the tick holds its full rate for it (the posture's "tile-slide").
    pub(crate) fn live(&self) -> bool {
        self.k > 0.0
    }

    /// The clock as it stands.
    #[cfg(test)]
    pub(crate) fn k(&self) -> f32 {
        self.k
    }

    /// Over: the tiles are home. The rows keep the offsets they were given until their next rebuild, which
    /// gives them 0, and at a clock of 0 they show nothing either way.
    fn end(&mut self) {
        self.strip.clear();
        self.grid.clear();
        self.arriving.clear();
        self.shows = false;
        self.reach = 0.0;
        self.k = 0.0;
        self.watch = None;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(current: usize, input: Instant) -> Watch {
        Watch { current, follow: true, target: current as f32, cols: 3, strip_w: 1200.0, input }
    }

    /// A take-out and a reread in one tick: each photo goes through both maps, and the places are the
    /// first landing's, the ones that were on screen. FALSIFIER: keep `later.was` in `then`: BEE's second
    /// move reads as an arrival.
    #[test]
    fn two_landings_in_a_tick_read_from_what_was_on_screen() {
        let none = TileSlide::default();
        // ANT BEE CAT DOG → ANT went (BEE CAT DOG), then CAT went (BEE DOG).
        let first = SlideFrom::read(&[None, Some(0), Some(1), Some(2)], 3, Some(1.0), None, &none);
        let second = SlideFrom::read(&[Some(0), None, Some(1)], 2, Some(0.0), None, &none);
        let both = first.then(second);
        assert_eq!(both.was, [Some(1), Some(3)], "BEE was 1, DOG was 3");
        assert_eq!(both.strip, Some(1.0), "the strip as it was on screen");
    }

    /// The clock follows the strip's chase and ends at exactly 0 in about 0.2 s for a one-cell slide, with one
    /// write at the end.
    /// FALSIFIER: drop the `END_PX` end: it never reaches 0. Use 0.5 for `CHASE`: it ends in 8 frames. Skip the
    /// write of 0: the frame loop finds nothing to write.
    #[test]
    fn the_clock_ends_at_zero_in_about_a_fifth_of_a_second() {
        let mut slide = TileSlide::default();
        let t0 = Instant::now();
        // ANT BEE CAT on BEE, Del: CAT slides in from one cell right.
        slide.landed(Some(SlideFrom::read(&[Some(0), None, Some(1)], 2, Some(1.0), None, &TileSlide::default())));
        slide.place_strip(1.0, -3..5, 2, 4.0);
        assert_eq!(slide.strip_tile(1), (STRIP_PITCH, false), "premise: CAT starts a cell to the right");
        let still = watch(0, t0);
        assert_eq!(slide.step(t0, false, still), Some(1.0));
        let mut frames = 0;
        let mut at = t0;
        while slide.live() {
            at += std::time::Duration::from_millis(16);
            frames += 1;
            let k = slide.step(at, false, still).expect("the clock moves every frame");
            assert!(k < 1.0);
            assert!(frames < 30, "it ends");
        }
        assert_eq!(slide.k(), 0.0, "exactly 0");
        assert!((11..=12).contains(&frames), "about 0.2 s for a cell: {frames} frames");
        assert_eq!(slide.step(at + std::time::Duration::from_millis(16), false, still), None, "nothing more to write");
    }
}
