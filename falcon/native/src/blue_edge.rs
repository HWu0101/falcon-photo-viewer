//! Delete round, R46: THE BLUE EDGE. The owner, 4 November 2026, verbatim: "if the user already scrolled past
//! the recovered image and it is off the visible filmstrip, the edge of the filmstrip (left or right depending
//! on where it is) should lit up in a blue gradient for a moment and clicking on the gradient should take them
//! to that image immediately", and then "the grid do get the blue edge (top/bottom instead of left/right as
//! that's how it scrolls)".
//!
//! What a person sees: a photo they brought back (a Recover, from Ctrl+Z, a row, a line or the card, or an Undo
//! move) lands while they stay where they are, and it's off what the filmstrip shows. The strip's edge on the
//! photo's side lights in the selected blue (`Theme.accent`, the selected button's) for about three seconds,
//! fading in and out as the edge pills do, and a click on it goes to that photo the way a tile's click does.
//! The grid does the same along its own scroll: its top edge for a photo above what it shows, its bottom edge
//! for one below. The strip and the grid each judge for themselves. Several photos back: each side points at the
//! nearest one on that side. The cue goes once it has nothing left to say: a photo the person scrolls into
//! view (or past) stops lighting that surface, the other photos on that side take over, and any move to
//! another photo ends it all. A landing that puts one of the photos on the stage needs no edge, and a photo
//! that lands where it can be seen lights nothing. A re-sort and another folder clear it.
//!
//! Only a photo the person brought back. `support::FileHistory::brought_back` is the one source: it is filled
//! by `file_queue::fold_file_done` from a Recover's report and an Undo move's, and nothing else writes it.
//! `apply_scan` takes it at every landing and hands it here ([`BlueEdge::landed`]); a landing with nothing in
//! it only carries what is already lit through the landing's map. So a reread for any other reason (another
//! program saved a file, the refresh on coming back to Falcon) can't light an edge: its photos arrive in the
//! list, and the movement fades them in, but no one named them.
//!
//! How. Read at the landing, by photo (its index in the new list); judged once a tick after the strip's and
//! the grid's steps have settled their place and scroll ([`BlueEdge::step`], `tick::step_blue_edge`), so a
//! landing's first frame already reads where everything will stay. The tick writes the four sides to the
//! window only when one changes, as photo indices or -1, and the slint mounts each edge only while it shows:
//! nothing is drawn, mounted or judged when no edge is lit.

use crate::tick::{GRID_CELL_H, GRID_PAD, GRID_ROW_PITCH};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// "For a moment": how long an edge stays lit once it has lit, unless the pointer is on it. The clean delete's
/// card is about 3 s (`support::CLEAN_DELETE_CARD_MS`), the app's quiet cue, and a person needs about that
/// long to see the edge and reach it. Its fades are the edge pills' 0.2 s, in the slint.
pub(crate) const MOMENT: Duration = Duration::from_millis(3_000);

/// The four sides as the window gets them, left, right, top, bottom: the photo a click goes to, or -1.
pub(crate) const OFF: [i32; 4] = [-1; 4];

/// What is on the stage: the photo, and Compare's two halves if it's open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stage {
    pub(crate) current: usize,
    pub(crate) halves: Option<(usize, usize)>,
}

impl Stage {
    fn shows(&self, photo: usize) -> bool {
        match self.halves {
            Some((a, b)) => a == photo || b == photo,
            None => self.current == photo,
        }
    }
}

/// The strip as its step left it this frame: the place at its middle (`film_pos`) and half its width, in cells.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StripView {
    pub(crate) pos: f32,
    pub(crate) half: f32,
}

/// The grid as its step left it this frame: its scroll, the height it shows, its columns, and the pending row's
/// cells before the first photo.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridView {
    pub(crate) scroll: f32,
    pub(crate) view_h: f32,
    pub(crate) cols: usize,
    pub(crate) leading: usize,
}

/// Where a photo has been on one surface since it came back. A photo counts as seen when at least half its
/// tile has shown (its middle inside what the surface shows), or when the person scrolled from one side of it
/// to the other between two frames. Seen stays seen: that surface has nothing more to say about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Where {
    Unread,
    Before,
    After,
    Seen,
}

impl Where {
    fn then(self, now: Where) -> Where {
        match (self, now) {
            (Where::Seen, _) | (Where::Before, Where::After) | (Where::After, Where::Before) => Where::Seen,
            (_, now) => now,
        }
    }

    fn of(at: f32, lo: f32, hi: f32) -> Where {
        if at < lo {
            Where::Before
        } else if at > hi {
            Where::After
        } else {
            Where::Seen
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Back {
    photo: usize,
    strip: Where,
    grid: Where,
}

/// The photos the edges are about, and the stage they lit for. Kept in the window's `Film`, beside the slide.
pub(crate) struct BlueEdge {
    /// By index in the list now, in order.
    photos: Vec<Back>,
    /// The stage when they came back. Any other photo there is the person moving on.
    stage: Option<Stage>,
    /// When the moment began: the first frame it could show, after the landing or after a panel over it closed.
    since: Option<Instant>,
    /// What the window was last given.
    written: [i32; 4],
}

impl Default for BlueEdge {
    fn default() -> Self {
        Self { photos: Vec::new(), stage: None, since: None, written: OFF }
    }
}

impl BlueEdge {
    /// At every landing (`apply_scan`), after it has settled the stage. `kept`: the landing's map of the old list
    /// into this one (`photo_open::KeptScroll::identity`), or `None` for a landing that keeps nothing (a
    /// re-sort, another folder, a reopen, a refused carry), which clears the edge. `brought`: the files of the
    /// photos the person brought back since the last landing (`FileHistory::brought_back`), usually none.
    pub(crate) fn landed(&mut self, kept: Option<&[Option<usize>]>, brought: &[PathBuf], list: &[crate::Shot], stage: Stage) {
        let Some(identity) = kept else {
            self.end();
            return;
        };
        // What is lit addresses the old list: through the landing's map. A photo that left goes, and a stage
        // whose photo left is gone too, so the next step ends it.
        let moved = |i: usize| identity.get(i).copied().flatten();
        self.photos.retain_mut(|b| moved(b.photo).map(|now| b.photo = now).is_some());
        self.stage = self.stage.and_then(|s| {
            let halves = match s.halves {
                Some((a, b)) => Some((moved(a)?, moved(b)?)),
                None => None,
            };
            Some(Stage { current: moved(s.current)?, halves })
        });
        if brought.is_empty() {
            return;
        }
        let brought: HashSet<&Path> = brought.iter().map(PathBuf::as_path).collect();
        let back: Vec<usize> = list
            .iter()
            .enumerate()
            .filter(|(_, s)| [&s.raw, &s.jpg].into_iter().flatten().any(|f| brought.contains(f.as_path())))
            .map(|(i, _)| i)
            .collect();
        // A landing that goes to the photo needs no edge: the stage shows what came back. In Compare that's a
        // photo going back into its half.
        if back.is_empty() || back.iter().any(|&i| stage.shows(i)) {
            return;
        }
        if self.stage != Some(stage) {
            self.photos.clear(); // what was lit was for a stage the person has left
        }
        self.photos.extend(back.into_iter().map(|photo| Back { photo, strip: Where::Unread, grid: Where::Unread }));
        self.photos.sort_by_key(|b| b.photo); // stable: a photo already lit keeps what it has seen
        self.photos.dedup_by_key(|b| b.photo);
        self.stage = Some(stage);
        self.since = None; // a new moment, from the first frame that judges it
    }

    /// Once a tick, after the strip's and the grid's steps. `strip`, `grid`: each surface if it's on screen.
    /// `held`: the pointer is on a lit edge, so it doesn't go from under it. `covered`: the events panel or a
    /// sheet is over the strip and the grid, so nothing lights and the moment waits for them to close. The four
    /// sides to write to the window, only when they changed.
    pub(crate) fn step(&mut self, now: Instant, stage: Stage, strip: Option<StripView>, grid: Option<GridView>, held: bool, covered: bool) -> Option<[i32; 4]> {
        let sides = self.judge(now, stage, strip, grid, held, covered);
        (sides != self.written).then(|| {
            self.written = sides;
            sides
        })
    }

    fn judge(&mut self, now: Instant, stage: Stage, strip: Option<StripView>, grid: Option<GridView>, held: bool, covered: bool) -> [i32; 4] {
        if self.photos.is_empty() {
            return OFF;
        }
        if self.stage != Some(stage) {
            self.end(); // moved on
            return OFF;
        }
        if covered {
            self.since = None; // the moment waits for the strip and the grid to be clear
            return OFF;
        }
        let since = *self.since.get_or_insert(now);
        if !held && now.saturating_duration_since(since) >= MOMENT {
            self.end();
            return OFF;
        }
        let mut sides = OFF;
        let side = |photos: &[Back], of: fn(&Back) -> Where, want: Where| -> i32 {
            let mut found = photos.iter().filter(|b| of(b) == want);
            let nearest = if want == Where::Before { found.next_back() } else { found.next() };
            nearest.map_or(-1, |b| b.photo as i32)
        };
        if let Some(v) = strip {
            for b in &mut self.photos {
                b.strip = b.strip.then(Where::of(b.photo as f32 - v.pos, -v.half, v.half));
            }
            sides[0] = side(&self.photos, |b| b.strip, Where::Before);
            sides[1] = side(&self.photos, |b| b.strip, Where::After);
        }
        if let Some(g) = grid {
            let cols = g.cols.max(1);
            for b in &mut self.photos {
                let middle = GRID_PAD + ((b.photo + g.leading) / cols) as f32 * GRID_ROW_PITCH + GRID_CELL_H / 2.0 - g.scroll;
                b.grid = b.grid.then(Where::of(middle, 0.0, g.view_h));
            }
            sides[2] = side(&self.photos, |b| b.grid, Where::Before);
            sides[3] = side(&self.photos, |b| b.grid, Where::After);
        }
        if sides == OFF {
            self.end(); // nothing left to say, or nowhere to say it
        }
        sides
    }

    /// What the window was last given.
    #[cfg(test)]
    pub(crate) fn sides(&self) -> [i32; 4] {
        self.written
    }

    /// The photos it's about, by index in the list now.
    #[cfg(test)]
    pub(crate) fn photos(&self) -> Vec<usize> {
        self.photos.iter().map(|b| b.photo).collect()
    }

    /// Whether the tick has anything to do here: an edge lit, or photos waiting to be judged.
    pub(crate) fn live(&self) -> bool {
        !self.photos.is_empty() || self.written != OFF
    }

    fn end(&mut self) {
        self.photos.clear();
        self.stage = None;
        self.since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photos(n: usize) -> Vec<crate::Shot> {
        (0..n)
            .map(|i| {
                let name = format!("HWU_{i:04}");
                crate::Shot { id: i, raw: None, jpg: Some(PathBuf::from(format!("{name}.jpg"))), name, kind: crate::SrcKind::Jpeg,
                    has_raw: false, has_jpg: true, cloud_placeholder: false, sniffed: None }
            })
            .collect()
    }

    fn file(i: usize) -> PathBuf {
        PathBuf::from(format!("HWU_{i:04}.jpg"))
    }

    /// The strip of a 1200 px window on 60: cells 55.3 to 64.7 show.
    fn strip(pos: f32) -> Option<StripView> {
        Some(StripView { pos, half: 1200.0 / 256.0 })
    }

    fn stay(n: usize) -> Vec<Option<usize>> {
        (0..n).map(Some).collect()
    }

    /// The rule, alone, on 60 of 100: HWU_0040, 45, 58, 72 and 90 back. Each side names its nearest, 45 and
    /// 72; 58 shows, so it lights nothing. The strip glides past 72 in one frame: 72 counts as seen, and 90 is
    /// next on the right. Back again: 72 stays seen. FALSIFIER: take `found.next()` for the left (the farthest):
    /// the first row. Drop the crossing arm of `Where::then`: the glide row.
    #[test]
    fn each_side_names_its_nearest_photo_and_a_scroll_past_counts() {
        let list = photos(100);
        let t0 = Instant::now();
        let on = Stage { current: 60, halves: None };
        let mut edge = BlueEdge::default();
        edge.landed(Some(&stay(100)), &[file(40), file(45), file(58), file(72), file(90)], &list, on);
        assert_eq!(edge.step(t0, on, strip(60.0), None, false, false), Some([45, 72, -1, -1]), "45 and 72 are the nearest; 58 shows");
        assert_eq!(edge.step(t0, on, strip(85.0), None, false, false), Some([45, 90, -1, -1]), "past 72 in one frame");
        assert_eq!(edge.step(t0, on, strip(60.0), None, false, false), None, "72 stays seen");
    }
}
