//! Background release: give graphics memory back while the photographer works in another app.
//!
//! Round 1 of [the sharing plan](../../../docs/planning/gpu-memory-sharing.md) (owner-approved
//! 25 October 2026). After the policy's wait (the user's setting since round 2a, 20 s by default) in
//! another application, or at once when the main
//! window is minimised, speculative preparation stops and the photo caches shrink GRADUALLY: the
//! allowed size falls linearly to the tier's target over the ramp (10 s; in Custom, its own release
//! time), farthest from the current
//! photo first. What the user asked for (`support::ExplicitSet`: the photo on screen, both Compare
//! halves, the hovered Review tile, the awaited frame) is never released, and thumbnails are not
//! touched. Coming back cancels at once; the existing scheduler refills, previews from the RAM cache
//! where it holds them. Pure decisions here; the tick applies them.
use crate::*;

/// Where the release ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Balanced and Power saver (and Custom's "Only the photos in use"): keep only the photos the
    /// user asked for.
    RequestedOnly,
    /// Performance (and Custom's 10%, 25% or 50%): Falcon's own card use down to this share, in
    /// percent, of the rendering card's measured dedicated memory (on the Mac, since round 2c, of
    /// Apple's recommended graphics budget, while macOS reports normal pressure).
    CardShare(u8),
}

/// What a share is taken of, as the log names it.
const SHARE_OF: &str = if cfg!(target_os = "macos") { "Apple's recommended graphics budget" } else { "the card" };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    pub(crate) wait: Duration,
    pub(crate) ramp: Duration,
    pub(crate) target: Target,
}

/// The owner's 20 s (25 October). Since round 2a the wait is the user's setting (`Settings::bg_wait_s`,
/// 5 s – 5 min, this by default), read every tick.
pub(crate) const WAIT_DEFAULT: Duration = Duration::from_secs(20);
/// Start at the end of the wait, reach the target 10 s later (owner ruling, 25 October).
pub(crate) const RAMP: Duration = Duration::from_secs(10);
/// Performance's background target: a quarter of the card (about 8 GB on a 32 GB card).
pub(crate) const PERFORMANCE_CARD_SHARE: u8 = 25;

impl Policy {
    pub(crate) const BALANCED: Policy =
        Policy { wait: WAIT_DEFAULT, ramp: RAMP, target: Target::RequestedOnly };
    pub(crate) const PERFORMANCE: Policy =
        Policy { wait: WAIT_DEFAULT, ramp: RAMP, target: Target::CardShare(PERFORMANCE_CARD_SHARE) };

    /// Round 2a: the policy of the mode IN EFFECT, every tick, with the user's wait. Performance keeps
    /// a quarter of the card; Balanced and Power saver keep only the requested photos. A release in
    /// progress keeps the start and end sizes and the release time it began with (`Background::step`
    /// snapshots them), so a mode change mid-release takes effect at the next release and the curve
    /// never jumps.
    /// `forced` is `FALCON_BACKGROUND_TIER` ([`Policy::forced_from_env`]), which wins.
    ///
    /// Round 2b: Custom keeps what its card says — only the photos in use, or 10%, 25% or 50% of the
    /// card — and releases over its own time (at once, 10 s or 30 s; a zero ramp releases at once,
    /// `Background::progress`). `custom` is read only in Custom. On the Mac a share is of Apple's
    /// recommended graphics budget and holds only while macOS reports normal pressure (round 2c,
    /// [`final_cache_target`] and [`Background::guard`]).
    pub(crate) fn for_mode(mode: u8, custom: &support::CustomLimits, forced: Option<Target>, wait_s: u32)
        -> Policy {
        let (target, ramp) = match mode {
            support::MODE_PERFORMANCE => (Target::CardShare(PERFORMANCE_CARD_SHARE), RAMP),
            support::MODE_CUSTOM => {
                let c = custom.clamped();
                let target = if c.bg_keep == 0 { Target::RequestedOnly } else { Target::CardShare(c.bg_keep) };
                (target, Duration::from_secs(u64::from(c.bg_release_s)))
            }
            _ => (Target::RequestedOnly, RAMP),
        };
        Policy { wait: Duration::from_secs(u64::from(wait_s)), ramp, target: forced.unwrap_or(target) }
    }

    /// `FALCON_BACKGROUND_TIER=performance` or `=balanced` forces that target whatever the mode, for
    /// verification only — a diagnostic switch like the `FALCON_DEBUG_*` family, read once at boot.
    /// Anything else follows the mode.
    pub(crate) fn forced_from_env(value: Option<&str>) -> Option<Target> {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("performance") => Some(Policy::PERFORMANCE.target),
            Some(v) if v.eq_ignore_ascii_case("balanced") => Some(Policy::BALANCED.target),
            _ => None,
        }
    }

    /// `FALCON_BACKGROUND_RELEASE=0` turns the release off for comparison runs, like the other
    /// `FALCON_*=0` reverts. Read once at boot; anything else leaves it on.
    pub(crate) fn release_enabled(value: Option<&str>) -> bool {
        value.map(str::trim) != Some("0")
    }

    /// The target as the log names it: "balanced" (only the requested photos), "performance"
    /// (Performance's quarter of the card), and since round 2b Custom's other shares by number.
    pub(crate) fn name(&self) -> String {
        match self.target {
            Target::RequestedOnly => "balanced".to_string(),
            Target::CardShare(PERFORMANCE_CARD_SHARE) => "performance".to_string(),
            Target::CardShare(p) => format!("{p}% of {SHARE_OF}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Foreground,
    /// In another application, inside the wait: nothing changes yet.
    Waiting,
    /// Preparation stopped; the allowed cache size is falling toward the target.
    Releasing,
    /// At the target. Preparation stays stopped until the user comes back.
    Reduced,
}

/// One tick's view of the application. `active` is `None` when the platform could not tell (a
/// null foreground window during a switch, a locked desktop): an unknown sample never starts a
/// release and never ends one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    pub(crate) active: Option<bool>,
    pub(crate) minimised: bool,
}

/// User input that reached Falcon's window this recently means Falcon is in use, whichever window
/// is active: Windows and macOS deliver wheel scrolling to a window that is not active, so the
/// photographer can browse Falcon with the wheel while another application keeps focus.
pub(crate) const INPUT_KEEPS_ACTIVE: Duration = Duration::from_secs(2);

/// One tick's observation: the platform's activity reading, overridden by recent input.
pub(crate) fn observe(foreground: Option<bool>, since_input: Option<Duration>, minimised: bool) -> Observation {
    let recent_input = since_input.is_some_and(|d| d < INPUT_KEEPS_ACTIVE);
    Observation { active: if recent_input { Some(true) } else { foreground }, minimised }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Away,
    AwayCancelled,
    ReleaseStarted { minimised: bool },
    Reduced,
    Back,
    /// Round 2c: memory pressure or swapping on the Mac dropped a release that kept a share to the
    /// requested photos at once.
    Guarded,
}

pub(crate) struct Background {
    phase: Phase,
    away_since: Option<Instant>,
    release_at: Option<Instant>,
    start_bytes: u64,
    final_bytes: u64,
    /// The release time this release began with (round 2b: Custom's is its own; review E1).
    ramp: Duration,
    returned_at: Option<Instant>,
    /// Round 2c: this release keeps a share (its policy's target was one when it began)…
    keeps_share: bool,
    /// …and the Mac's guard has dropped it to the requested photos, until the user comes back.
    guarded: bool,
}

impl Background {
    pub(crate) fn new() -> Background {
        Background { phase: Phase::Foreground, away_since: None, release_at: None, start_bytes: 0,
            final_bytes: 0, ramp: RAMP, returned_at: None, keeps_share: false, guarded: false }
    }

    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    /// Speculative preparation stops once the release starts, not during the wait.
    pub(crate) fn holds(&self) -> bool {
        matches!(self.phase, Phase::Releasing | Phase::Reduced)
    }

    /// Time since the user last came back, for the full-detail fill's stillness interval.
    pub(crate) fn since_return(&self, now: Instant) -> Option<Duration> {
        self.returned_at.map(|at| now.saturating_duration_since(at))
    }

    /// One tick. `cache_bytes` and `final_bytes` are snapshotted when the release starts, so the
    /// ramp cannot wander with a once-a-second measurement; so is the policy's release time.
    pub(crate) fn step(&mut self, now: Instant, obs: Observation, policy: &Policy, cache_bytes: u64,
        final_bytes: u64) -> Option<Change> {
        let back = obs.active == Some(true) && !obs.minimised;
        let away = obs.minimised || obs.active == Some(false);
        let share = matches!(policy.target, Target::CardShare(_));
        match self.phase {
            Phase::Foreground if away => {
                self.away_since = Some(now);
                self.phase = Phase::Waiting;
                if obs.minimised {
                    return Some(self.start_release(now, cache_bytes, final_bytes, true, policy.ramp, share));
                }
                Some(Change::Away)
            }
            Phase::Foreground => None,
            Phase::Waiting if back => {
                self.phase = Phase::Foreground;
                self.away_since = None;
                Some(Change::AwayCancelled)
            }
            Phase::Waiting => {
                let waited = self.away_since.map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
                if obs.minimised || (away && waited >= policy.wait) {
                    Some(self.start_release(now, cache_bytes, final_bytes, obs.minimised, policy.ramp, share))
                } else {
                    None
                }
            }
            Phase::Releasing | Phase::Reduced if back => {
                self.phase = Phase::Foreground;
                self.away_since = None;
                self.release_at = None;
                self.returned_at = Some(now);
                Some(Change::Back)
            }
            Phase::Releasing if self.progress(now) >= 1.0 => {
                self.phase = Phase::Reduced;
                Some(Change::Reduced)
            }
            Phase::Releasing | Phase::Reduced => None,
        }
    }

    fn start_release(&mut self, now: Instant, cache_bytes: u64, final_bytes: u64, minimised: bool,
        ramp: Duration, keeps_share: bool) -> Change {
        self.phase = Phase::Releasing;
        self.release_at = Some(now);
        self.start_bytes = cache_bytes;
        self.final_bytes = final_bytes;
        self.ramp = ramp;
        self.keeps_share = keeps_share;
        self.guarded = false;
        Change::ReleaseStarted { minimised }
    }

    /// Round 2c, the Mac rule (the owner's option a, 26 October): a release that keeps a share
    /// keeps it only while macOS reports normal memory pressure and nothing is being swapped. Once
    /// either stops being true during the release (or already is not as it starts: the tick asks on
    /// the release's first tick too), it drops to the requested photos AT ONCE, and stays there until
    /// the user comes back (`Change::Back` ends the hold; the next release starts unguarded and
    /// decides afresh). The wait before a
    /// release is not cut short, and a release that keeps only the requested photos keeps its own
    /// release time: the existing pressure hold (`memory_pressure`, the pool governor's low-RAM
    /// sample) already sheds speculative work in any phase. `trips` is [`mac_guard_trips`].
    pub(crate) fn guard(&mut self, trips: bool) -> Option<Change> {
        if !trips || self.guarded || !self.keeps_share || !self.holds() {
            return None;
        }
        self.guarded = true;
        self.final_bytes = 0;
        self.ramp = Duration::ZERO;
        Some(Change::Guarded)
    }

    fn progress(&self, now: Instant) -> f64 {
        let Some(at) = self.release_at else { return 1.0 };
        if self.ramp.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(at).as_secs_f64() / self.ramp.as_secs_f64()).min(1.0)
    }

    /// The photo-cache bytes allowed now, or `None` when nothing is being released. Linear from
    /// the size at the start to the target; a cache already under its target keeps what it has.
    pub(crate) fn allowed_bytes(&self, now: Instant) -> Option<u64> {
        match self.phase {
            Phase::Foreground | Phase::Waiting => None,
            Phase::Reduced => Some(self.final_bytes),
            Phase::Releasing => {
                if self.start_bytes <= self.final_bytes {
                    return Some(self.final_bytes);
                }
                let left = 1.0 - self.progress(now);
                Some(self.final_bytes + ((self.start_bytes - self.final_bytes) as f64 * left) as u64)
            }
        }
    }

    pub(crate) fn final_bytes(&self) -> u64 {
        self.final_bytes
    }
}

/// The photo-cache size the release ends at. Performance keeps Falcon's whole card use at its
/// share of the card, so the caches get the share minus what Falcon uses outside them. Both
/// numbers come from one reading of the adapter Slint renders with: its dedicated memory
/// (`card_total`) and Falcon's own use of it. Never the boot budget estimate (`vram_budget_bytes`),
/// which counts shared system memory and takes the largest adapter, so it can be several times
/// this card (Codex review R2, 25 October). Without a reading with a card size (a failed query),
/// Performance keeps only the requested photos, as Balanced does.
///
/// Round 2c, the Mac: `card_total` is Apple's recommended graphics budget and `own_local` Falcon's
/// footprint, so the caches get the share of the budget minus Falcon's other use (the plan's
/// `share × recommended budget − (footprint − cache bytes)`), and only while macOS reports normal
/// pressure and nothing is being swapped ([`gpu_memory::MacMemory::calm`]); otherwise, or with no
/// reading, only the requested photos.
pub(crate) fn final_cache_target(target: Target, measured: Option<gpu_memory::Sample>, cache_bytes: u64) -> u64 {
    match (target, measured) {
        (Target::CardShare(percent), Some(s)) if s.card_total > 0 && s.mac.is_none_or(|m| m.calm()) => {
            let share = s.card_total / 100 * u64::from(percent);
            share.saturating_sub(s.own_local.saturating_sub(cache_bytes))
        }
        _ => 0,
    }
}

/// Round 2c: does the Mac's guard trip this tick ([`Background::guard`])? Only on the Mac
/// (`mac_platform`, the caller's `cfg!`): when its reading is missing or stale, or macOS does not
/// report normal pressure with nothing swapped. Never elsewhere.
pub(crate) fn mac_guard_trips(mac_platform: bool, measured: Option<gpu_memory::Sample>) -> bool {
    mac_platform && !measured.and_then(|s| s.mac).is_some_and(|m| m.calm())
}

/// The tracked photo-cache bytes: the same width × height × 4 the developer HUD sums.
pub(crate) fn cache_bytes(fast: &FastTier, detail: &DetailTier) -> u64 {
    let f: u64 = fast.cache.borrow().values().map(|e| e.w as u64 * e.h as u64 * 4).sum();
    let d: u64 = detail.cache.borrow().values().map(|(_, w, h)| *w as u64 * *h as u64 * 4).sum();
    f + d
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Released {
    pub(crate) bytes: u64,
    pub(crate) previews: usize,
    pub(crate) full: usize,
}

/// Evict until the tracked caches fit `allowed`: farthest from `c` first, the larger entry first
/// at the same distance, never an entry `explicit` holds. A preview leaving moves `fast_gen`, the
/// key every tile feed rebuilds on, so open models let go of their stopgap copies.
pub(crate) fn release_to(allowed: u64, c: usize, explicit: &support::ExplicitSet, fast: &FastTier,
    detail: &DetailTier, fast_gen: &Cell<u64>) -> Released {
    let mut total = cache_bytes(fast, detail);
    let mut out = Released::default();
    if total <= allowed {
        return out;
    }
    let mut candidates: Vec<(usize, u64, bool, usize)> = Vec::new();
    for (&id, e) in fast.cache.borrow().iter() {
        if !explicit.holds(id) {
            candidates.push((id.abs_diff(c), e.w as u64 * e.h as u64 * 4, false, id));
        }
    }
    for (&id, (_, w, h)) in detail.cache.borrow().iter() {
        if !explicit.holds(id) {
            candidates.push((id.abs_diff(c), *w as u64 * *h as u64 * 4, true, id));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    for (_, bytes, is_full, id) in candidates {
        if total <= allowed {
            break;
        }
        if is_full {
            detail.cache.borrow_mut().remove(&id);
            detail.order.borrow_mut().retain(|&o| o != id);
            out.full += 1;
        } else {
            fast.cache.borrow_mut().remove(&id);
            out.previews += 1;
        }
        total = total.saturating_sub(bytes);
        out.bytes += bytes;
    }
    if out.previews > 0 {
        fast_gen.set(fast_gen.get().wrapping_add(1));
    }
    out
}

/// A Review panel or grid that is not refreshing keeps its rows, and a row can keep a released
/// preview's texture alive (`tick::lookup_thumb` serves a cached preview as the stopgap until a
/// thumbnail lands). The Review panel refreshes while it is open; the grid only while
/// `tick::grid_refills` says so, which is false while immersive mode hides it with its open setting
/// kept. Empty the others and reset their keys to the boot values, so the next refresh rebuilds as
/// a first open. The tick runs this on every held tick, not only when a release call evicts a
/// preview: an earlier call or the Mac pressure hold may already have evicted the preview a row
/// still holds (Codex review R1, 25 October).
#[allow(clippy::type_complexity)]
pub(crate) fn release_unrefreshed_models(sel_refreshes: bool, sel_model: &slint::VecModel<SelTileRow>,
    last_sel_key: &Cell<(u64, i32, u64, u64, u64, u64)>, grid_refreshes: bool,
    grid_model: &slint::VecModel<FilmItem>, last_grid_key: &Cell<(i64, usize, usize, u64, u64, u64, u64, u64, (i64, i64))>) {
    use slint::Model;
    if !sel_refreshes && sel_model.row_count() > 0 {
        sel_model.set_vec(Vec::new());
        last_sel_key.set(SEL_KEY_UNBUILT);
    }
    if !grid_refreshes && grid_model.row_count() > 0 {
        grid_model.set_vec(Vec::new());
        last_grid_key.set(GRID_KEY_UNBUILT);
    }
}

/// 2026-10-26 (round 2a re-check R3): **A MODE'S PREVIEW TRIM, LET GO BY THE PANELS THAT ARE NOT
/// REFRESHING.** `tick::step_cache_retention` releases previews in the foreground when a mode's kept
/// window narrows over a warm cache, and bumps `fast_gen` so the refreshing tile feeds rebuild
/// without them. A closed Review panel or a grid hidden by immersive mode cannot rebuild, and its
/// rows still hold the stopgap previews `tick::lookup_thumb` gave them: the ownership edge the
/// background release closes on every held tick, closed here on every tick a trim released
/// anything. The panels that are refreshing are left to their own rebuild, as there. The Mac's
/// foreground pressure hold uses it too, with the count `memory_pressure::shed` removed (round 2c
/// review P1).
#[allow(clippy::type_complexity)]
pub(crate) fn release_after_trim(previews_released: usize, sel_refreshes: bool,
    sel_model: &slint::VecModel<SelTileRow>, last_sel_key: &Cell<(u64, i32, u64, u64, u64, u64)>,
    grid_refreshes: bool, grid_model: &slint::VecModel<FilmItem>,
    last_grid_key: &Cell<(i64, usize, usize, u64, u64, u64, u64, u64, (i64, i64))>) {
    if previews_released > 0 {
        release_unrefreshed_models(sel_refreshes, sel_model, last_sel_key, grid_refreshes, grid_model,
            last_grid_key);
    }
}

/// The Review panel's tile key before its first build (`main.rs` seeds the cell with it).
pub(crate) const SEL_KEY_UNBUILT: (u64, i32, u64, u64, u64, u64) =
    (u64::MAX, i32::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX);
/// The folder grid's refill key before its first build (`main.rs` seeds the cell with it).
pub(crate) const GRID_KEY_UNBUILT: (i64, usize, usize, u64, u64, u64, u64, u64, (i64, i64)) =
    (i64::MIN, usize::MAX, 0, 0, u64::MAX, u64::MAX, u64::MAX, u64::MAX, (i64::MIN, i64::MIN));

const MB: u64 = 1_000_000;

/// One line per transition (Diagnostic logging), with the numbers that say what the release did.
/// `released` is this absence's running total: reset when a release starts, reported at the
/// target and on return.
pub(crate) fn log_change(change: Change, policy: &Policy, bg: &Background, released: &Cell<Released>,
    cache_bytes: u64, measured: Option<gpu_memory::Sample>) {
    // Round 2c: on the Mac, Falcon's footprint (what macOS charges it), not a card's share.
    let own = match measured {
        Some(s) if s.mac.is_some() => format!("Falcon's footprint {} MB", s.own_local / MB),
        Some(s) => format!("Falcon on the card {} MB", s.own_local / MB),
        None if cfg!(target_os = "macos") => "Falcon's footprint not measured".to_string(),
        None => "Falcon on the card not measured".to_string(),
    };
    let line = match change {
        Change::Away => format!(
            "background: Falcon is in another application — graphics memory is released if it stays away {} s",
            policy.wait.as_secs()),
        Change::AwayCancelled => "background: back within the wait — nothing released".to_string(),
        Change::ReleaseStarted { minimised } => {
            released.set(Released::default());
            format!(
                "background: releasing graphics memory ({}) — photo caches {} MB → {} MB {}, farthest photos \
                 first; preparing photos ahead is paused (tier {}; {own})",
                if minimised { "minimised" } else { "in another application" },
                cache_bytes / MB, bg.final_bytes() / MB,
                if bg.ramp.is_zero() { "at once".to_string() } else { format!("over {} s", bg.ramp.as_secs()) },
                policy.name())
        }
        Change::Reduced => {
            let r = released.get();
            format!(
                "background: release reached its target — {} MB released ({} previews, {} full-resolution); \
                 photo caches {} MB; {own}",
                r.bytes / MB, r.previews, r.full, cache_bytes / MB)
        }
        Change::Back => {
            let r = released.replace(Released::default());
            format!(
                "background: back — {} MB had been released ({} previews, {} full-resolution); preparing \
                 photos ahead resumes ({own})",
                r.bytes / MB, r.previews, r.full)
        }
        Change::Guarded => format!(
            "background: macOS does not report normal memory pressure with nothing swapped — keeping only the \
             photos in use, released at once, until Falcon is back in front ({})",
            measured.as_ref().and_then(gpu_memory::describe_mac).unwrap_or_else(|| "Mac readings unavailable".to_string())),
    };
    log_event(&line);
}

/// The memory line (Diagnostic logging): every 10 s away from the foreground, every 60 s in it.
pub(crate) fn log_memory(now: Instant, last: &Cell<Instant>, phase: Phase, cache_bytes: u64,
    previews: usize, full: usize, measured: Option<gpu_memory::Sample>) {
    let every = if phase == Phase::Foreground { Duration::from_secs(60) } else { Duration::from_secs(10) };
    if now.saturating_duration_since(last.get()) < every {
        return;
    }
    last.set(now);
    let readings = match measured {
        None if cfg!(target_os = "macos") => "Mac memory readings unavailable".to_string(),
        None => "card readings unavailable".to_string(),
        // Round 2c: the Mac's readings by name.
        Some(s) => gpu_memory::describe_mac(&s).unwrap_or_else(|| format!(
            "Falcon on the card {} MB (allowance {} MB, through system memory {} MB) · whole card {} / {} MB",
            s.own_local / MB, s.own_budget / MB, s.own_nonlocal / MB,
            s.card_used.map_or_else(|| "unavailable".to_string(), |u| (u / MB).to_string()), s.card_total / MB)),
    };
    log_event(&format!(
        "memory: {readings} · photo caches {} MB ({previews} previews, {full} full-resolution) · {phase:?}",
        cache_bytes / MB
    ));
}

/// Is Falcon the active application? Windows: the foreground window belongs to this process, so
/// Falcon's own dialogs count as Falcon. A null foreground (mid-switch, a locked desktop) is unknown.
#[cfg(windows)]
pub(crate) fn falcon_is_active() -> Option<bool> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then(|| pid == std::process::id())
}

/// macOS: AppKit's application-level activity, so sheets, full-screen transitions and the toolbar
/// window never read as leaving. Main thread only; anywhere else it is unknown.
#[cfg(target_os = "macos")]
pub(crate) fn falcon_is_active() -> Option<bool> {
    let mtm = objc2_foundation::MainThreadMarker::new()?;
    Some(unsafe { objc2_app_kit::NSApplication::sharedApplication(mtm).isActive() })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn falcon_is_active() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE: Observation = Observation { active: Some(true), minimised: false };
    const AWAY: Observation = Observation { active: Some(false), minimised: false };
    const UNKNOWN: Observation = Observation { active: None, minimised: false };
    const MINIMISED: Observation = Observation { active: Some(true), minimised: true };

    fn img() -> slint::Image {
        slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(1, 1))
    }

    /// Fails if the wait is shortened or skipped, if coming back does not cancel, if an unknown
    /// sample starts or ends a release, or if leaving again does not restart the wait
    /// (`away_since` kept from an earlier absence).
    #[test]
    fn the_release_waits_twenty_seconds_away_and_coming_back_cancels_it() {
        let p = Policy::BALANCED;
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut b = Background::new();
        assert_eq!(b.step(at(0.0), AWAY, &p, 100, 0), Some(Change::Away));
        assert_eq!(b.step(at(19.9), AWAY, &p, 100, 0), None);
        assert!(!b.holds(), "a brief switch away changes nothing");
        assert_eq!(b.step(at(10.0), ACTIVE, &p, 100, 0), Some(Change::AwayCancelled));
        // Leaving again restarts the wait from the new moment.
        assert_eq!(b.step(at(30.0), AWAY, &p, 100, 0), Some(Change::Away));
        assert_eq!(b.step(at(49.9), AWAY, &p, 100, 0), None);
        assert_eq!(b.step(at(50.0), AWAY, &p, 100, 0), Some(Change::ReleaseStarted { minimised: false }));
        assert!(b.holds());
        assert_eq!(b.step(at(52.0), UNKNOWN, &p, 100, 0), None, "unknown never ends a release");
        assert!(b.holds());
        assert_eq!(b.step(at(53.0), ACTIVE, &p, 100, 0), Some(Change::Back));
        assert!(!b.holds());
        assert_eq!(b.since_return(at(53.5)), Some(Duration::from_millis(500)));
        // Unknown never starts one, however long it lasts.
        let mut u = Background::new();
        assert_eq!(u.step(at(0.0), AWAY, &p, 100, 0), Some(Change::Away));
        for s in 1..120 {
            assert_eq!(u.step(at(s as f64 * 0.5 + 0.1), UNKNOWN, &p, 100, 0), None);
        }
        assert_eq!(u.phase(), Phase::Waiting);
    }

    /// Fails if wheel browsing over an inactive Falcon reads as away (the input override removed), or
    /// if input keeps Falcon "in use" for longer than the grace, or overrides minimising.
    #[test]
    fn input_reaching_falcon_counts_as_in_use_while_another_app_has_focus() {
        let ms = Duration::from_millis;
        assert_eq!(observe(Some(false), Some(ms(300)), false), ACTIVE, "a wheel scroll a moment ago");
        assert_eq!(observe(None, Some(ms(300)), false), ACTIVE, "…even when the platform cannot tell");
        assert_eq!(observe(Some(false), Some(INPUT_KEEPS_ACTIVE), false), AWAY);
        assert_eq!(observe(Some(false), None, false), AWAY);
        assert_eq!(observe(None, None, false), UNKNOWN);
        assert_eq!(observe(Some(true), Some(ms(10)), true), MINIMISED, "minimised stays minimised");
        // Browsing with the wheel over an inactive Falcon: the wait never completes.
        let p = Policy::BALANCED;
        let t0 = Instant::now();
        let mut b = Background::new();
        for s in 0..60 {
            let obs = observe(Some(false), Some(ms(if s % 2 == 0 { 100 } else { 1_500 })), false);
            b.step(t0 + Duration::from_secs(s), obs, &p, 100, 0);
        }
        assert!(!b.holds(), "input every second or two keeps Falcon in use");
    }

    /// Fails if minimising waits like a switch away, or if the release jumps straight to the target
    /// instead of falling linearly over the ramp.
    #[test]
    fn minimising_starts_at_once_and_the_allowed_size_falls_over_ten_seconds() {
        let p = Policy::BALANCED;
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut b = Background::new();
        assert_eq!(b.allowed_bytes(at(0.0)), None);
        assert_eq!(b.step(at(0.0), MINIMISED, &p, 1000, 200), Some(Change::ReleaseStarted { minimised: true }));
        assert_eq!(b.allowed_bytes(at(0.0)), Some(1000));
        assert_eq!(b.allowed_bytes(at(5.0)), Some(600));
        assert_eq!(b.step(at(9.9), MINIMISED, &p, 0, 0), None);
        assert_eq!(b.step(at(10.0), MINIMISED, &p, 0, 0), Some(Change::Reduced));
        assert_eq!(b.allowed_bytes(at(10.0)), Some(200));
        assert_eq!(b.allowed_bytes(at(60.0)), Some(200));
        // A cache already under its target keeps what it has.
        let mut small = Background::new();
        small.step(at(0.0), MINIMISED, &p, 100, 500);
        assert_eq!(small.allowed_bytes(at(5.0)), Some(500));
        // Waiting, then minimised inside the wait: no further wait.
        let mut w = Background::new();
        w.step(at(0.0), AWAY, &p, 100, 0);
        assert_eq!(w.step(at(3.0), MINIMISED, &p, 100, 0), Some(Change::ReleaseStarted { minimised: true }));
    }

    /// Fails if the share is taken of anything but the measured card's dedicated memory, if it
    /// ignores Falcon's own non-cache use, if a missing or sizeless reading keeps more than the
    /// requested photos, or if the subtraction can wrap.
    #[test]
    fn the_performance_target_is_a_quarter_of_the_card_minus_falcons_other_use() {
        let reading = |card_total: u64, own_local: u64| Some(gpu_memory::Sample { at: Instant::now(), own_local,
            own_budget: card_total, own_nonlocal: 0, card_used: None, card_total, mac: None });
        let card = 33_750_000_000;
        assert_eq!(final_cache_target(Target::RequestedOnly, reading(card, 14_000_000_000), 10_000_000_000), 0);
        // Measured: 14 GB used, 10 GB of it caches → 4 GB elsewhere; a quarter is 8.4375 GB.
        assert_eq!(final_cache_target(Target::CardShare(25), reading(card, 14_000_000_000), 10_000_000_000),
            8_437_500_000 - 4_000_000_000);
        // An 8 GB card: a quarter of it, whatever the boot estimate says (shared memory and other
        // adapters can put that at 16 GB or more).
        assert_eq!(final_cache_target(Target::CardShare(25), reading(8_000_000_000, 3_000_000_000), 2_000_000_000),
            2_000_000_000 - 1_000_000_000);
        // No reading, or one without a card size: only the requested photos, as Balanced.
        assert_eq!(final_cache_target(Target::CardShare(25), None, 10_000_000_000), 0);
        assert_eq!(final_cache_target(Target::CardShare(25), reading(0, 14_000_000_000), 10_000_000_000), 0);
        // More outside the caches than the share: nothing is kept beyond the requested photos.
        assert_eq!(final_cache_target(Target::CardShare(25), reading(4_000_000_000, 9_000_000_000), 1), 0);
        assert_eq!(Policy::forced_from_env(Some(" Performance ")), Some(Policy::PERFORMANCE.target));
        assert_eq!(Policy::forced_from_env(Some("balanced")), Some(Policy::BALANCED.target));
        assert_eq!(Policy::forced_from_env(Some("anything")), None);
        assert_eq!(Policy::forced_from_env(None), None);
        assert!(!Policy::release_enabled(Some(" 0 ")));
        assert!(Policy::release_enabled(None) && Policy::release_enabled(Some("1")));
    }

    /// A Mac reading: Apple's recommended graphics budget, Falcon's footprint, and macOS's state.
    fn mac_reading(budget: u64, footprint: u64, pressure: Option<gpu_memory::PressureLevel>, swapouts: Option<u64>)
        -> Option<gpu_memory::Sample> {
        Some(gpu_memory::Sample { at: Instant::now(), own_local: footprint, own_budget: 0, own_nonlocal: 0,
            card_used: None, card_total: budget,
            mac: Some(gpu_memory::MacMemory { pressure, swap_used: Some(0), swapouts }) })
    }

    /// Round 2c, the Mac rule (owner's option a). FALSIFIERS (L28): keep the share whatever macOS
    /// reports (drop the `calm` term of `final_cache_target`) and the warning row reddens; let swapping
    /// count as calm and the swap row does; read a missing reading as calm and the unread rows do.
    #[test]
    fn the_mac_keeps_its_share_only_while_macos_reports_calm() {
        use gpu_memory::PressureLevel::{Critical, Normal, Warning};
        let (budget, footprint, caches) = (21_000_000_000u64, 3_000_000_000u64, 2_000_000_000u64);
        // Calm: a quarter of the budget minus Falcon's use outside the caches (5.25 GB − 1 GB).
        assert_eq!(final_cache_target(Target::CardShare(25), mac_reading(budget, footprint, Some(Normal), Some(0)), caches),
            5_250_000_000 - 1_000_000_000);
        assert_eq!(final_cache_target(Target::CardShare(10), mac_reading(budget, footprint, Some(Normal), Some(0)), caches),
            2_100_000_000 - 1_000_000_000, "Custom's 10% of the budget");
        for (pressure, swapouts, why) in [
            (Some(Warning), Some(0), "pressure warning"),
            (Some(Critical), Some(0), "pressure critical"),
            (Some(Normal), Some(7), "pages are being swapped"),
            (None, Some(0), "the pressure level was not read"),
            (Some(Normal), None, "swapping was not measured (the first reading)"),
        ] {
            assert_eq!(final_cache_target(Target::CardShare(25), mac_reading(budget, footprint, pressure, swapouts), caches), 0,
                "{why}: only the requested photos");
            assert!(mac_guard_trips(true, mac_reading(budget, footprint, pressure, swapouts)), "{why}: the guard trips");
        }
        assert_eq!(final_cache_target(Target::CardShare(25), mac_reading(0, footprint, Some(Normal), Some(0)), caches), 0,
            "no budget from Metal: only the requested photos");
        assert_eq!(final_cache_target(Target::RequestedOnly, mac_reading(budget, footprint, Some(Normal), Some(0)), caches), 0);
        assert!(!mac_guard_trips(true, mac_reading(budget, footprint, Some(Normal), Some(0))), "calm: no guard");
        assert!(mac_guard_trips(true, None), "a missing or stale Mac reading does not report calm");
        assert!(!mac_guard_trips(false, None), "never on Windows");
        assert!(!mac_guard_trips(false, mac_reading(budget, footprint, Some(Critical), Some(9))), "never on Windows");
    }

    /// Round 2c: the guard drops a release that keeps a share to the requested photos at once and
    /// holds it there until the user comes back; it never touches the wait, a release that keeps only
    /// the requested photos, or the next release. FALSIFIERS (L28): leave the ramp running (drop the
    /// guard's `ramp = ZERO`) and the at-once row reddens; keep the latch into the next release (drop
    /// its reset in `start_release`) and the second release's row does; guard a requested-only
    /// release (drop `keeps_share`) and Balanced's row does; guard outside a release (drop `holds`)
    /// and the next wait's row does.
    #[test]
    fn memory_pressure_drops_a_mac_release_that_keeps_a_share_at_once_until_back() {
        let p = Policy::PERFORMANCE;
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut b = Background::new();
        // Inside the wait: nothing to guard, and the wait is not cut short.
        assert_eq!(b.step(at(0.0), AWAY, &p, 10_000, 4_000), Some(Change::Away));
        assert_eq!(b.guard(true), None);
        assert_eq!(b.phase(), Phase::Waiting);
        // The release starts toward its share over 10 s…
        assert_eq!(b.step(at(20.0), AWAY, &p, 10_000, 4_000), Some(Change::ReleaseStarted { minimised: false }));
        assert_eq!(b.guard(false), None, "calm: the release keeps its share");
        assert_eq!(b.allowed_bytes(at(23.0)), Some(8_200));
        // …and pressure rises at 23 s: everything not requested goes at once.
        assert_eq!(b.guard(true), Some(Change::Guarded));
        assert_eq!(b.allowed_bytes(at(23.0)), Some(0), "at once");
        assert_eq!(b.step(at(23.1), AWAY, &p, 0, 4_000), Some(Change::Reduced));
        assert_eq!(b.guard(true), None, "once per release");
        // Calm again while still away: it stays at the requested photos.
        assert_eq!(b.guard(false), None);
        assert_eq!(b.allowed_bytes(at(60.0)), Some(0), "no refill while away");
        // Back: the next release decides afresh, and is guarded afresh.
        assert_eq!(b.step(at(61.0), ACTIVE, &p, 0, 4_000), Some(Change::Back));
        assert_eq!(b.step(at(62.0), MINIMISED, &p, 9_000, 4_000), Some(Change::ReleaseStarted { minimised: true }));
        assert_eq!(b.allowed_bytes(at(62.0)), Some(9_000), "the new release starts from its own size");
        assert_eq!(b.guard(true), Some(Change::Guarded), "the latch ended with the last absence");
        assert_eq!(b.allowed_bytes(at(62.0)), Some(0));
        // Pressure already up as a release starts: the tick asks on its first tick, so it is at once.
        let mut s = Background::new();
        s.step(at(0.0), MINIMISED, &p, 5_000, 0);
        assert_eq!(s.guard(true), Some(Change::Guarded));
        assert_eq!(s.allowed_bytes(at(0.0)), Some(0));
        // Balanced keeps only the requested photos anyway, over its own 10 s: never guarded.
        let mut bal = Background::new();
        bal.step(at(0.0), MINIMISED, &Policy::BALANCED, 1_000, 0);
        assert_eq!(bal.guard(true), None);
        assert_eq!(bal.allowed_bytes(at(5.0)), Some(500), "its ramp is untouched");
        // After a share release that was never guarded, inside the next wait: still nothing to guard
        // (review of the build; a guarded release's latch would hide this).
        let mut w = Background::new();
        w.step(at(0.0), MINIMISED, &p, 5_000, 1_000);
        assert_eq!(w.step(at(1.0), ACTIVE, &p, 1_000, 1_000), Some(Change::Back));
        assert_eq!(w.step(at(2.0), AWAY, &p, 1_000, 1_000), Some(Change::Away));
        assert_eq!(w.guard(true), None, "the wait is never cut short");
    }

    /// Fails if the order is reversed (nearest released first), if the `holds` exemption is dropped,
    /// if eviction overshoots the allowance, or if the tile-feed key is not moved.
    #[test]
    fn release_takes_the_farthest_photos_first_and_never_a_requested_one() {
        let fast = FastTier::new();
        let detail = DetailTier::new();
        // Previews of 40 bytes (10 × 1 × 4) at 0..=20; full-resolution frames of 400 bytes at
        // 9..=11. Current = 10, Compare halves 0 and 20 (on screen), awaited 19.
        for id in 0..=20 {
            fast.cache.borrow_mut().insert(id, tick::FastEntry { img: img(), w: 10, h: 1,
                blur: Arc::from([0u8; 4]), bw: 1, bh: 1, dim: 1, turns: 0 });
        }
        for id in 9..=11 {
            detail.cache.borrow_mut().insert(id, (img(), 100, 1));
            detail.order.borrow_mut().push_back(id);
        }
        let explicit = support::ExplicitSet::new(10, true, 0, 20, 21, None, 19);
        let gen = Cell::new(3);
        let start = cache_bytes(&fast, &detail);
        assert_eq!(start, 21 * 40 + 3 * 400);
        // Allow all but 100 bytes: the farthest unprotected previews go first — 1 (distance 9; 19
        // beside it is awaited), then 2 and 18 (distance 8) — and the release stops there.
        let r = release_to(start - 100, 10, &explicit, &fast, &detail, &gen);
        assert_eq!((r.previews, r.full), (3, 0));
        assert!(cache_bytes(&fast, &detail) <= start - 100);
        for id in [0, 19, 20] {
            assert!(fast.cache.borrow().contains_key(&id), "requested photo {id} kept");
        }
        assert!(!fast.cache.borrow().contains_key(&18) && !fast.cache.borrow().contains_key(&2));
        assert!(fast.cache.borrow().contains_key(&9) && fast.cache.borrow().contains_key(&11), "nearest kept");
        assert_eq!(gen.get(), 4, "open tile feeds must rebuild and drop their copies");
        // Down to the requested photos only.
        let r = release_to(0, 10, &explicit, &fast, &detail, &gen);
        assert_eq!(r.full, 2, "the current photo's full-resolution frame stays");
        let mut left: Vec<usize> = fast.cache.borrow().keys().copied().collect();
        left.sort_unstable();
        assert_eq!(left, vec![0, 10, 19, 20]);
        assert_eq!(detail.cache.borrow().keys().copied().collect::<Vec<_>>(), vec![10]);
        assert_eq!(detail.order.borrow().iter().copied().collect::<Vec<_>>(), vec![10]);
        // Nothing over the allowance: nothing moves, not even the key.
        let before = gen.get();
        assert_eq!(release_to(u64::MAX, 10, &explicit, &fast, &detail, &gen), Released::default());
        assert_eq!(gen.get(), before);
    }

    /// Fails if a panel that is not refreshing keeps its rows (pinning released textures) or its key
    /// is not reset (the next refresh would skip the rebuild); refreshing panels are left to their
    /// own rebuild. The grid's real refresh condition, immersive mode included, is
    /// `tipgeom_tests::a_grid_hidden_by_immersive_mode_lets_go_of_released_previews`.
    #[test]
    fn closed_panels_let_go_of_their_rows_and_rebuild_on_the_next_open() {
        use slint::Model;
        let sel = slint::VecModel::<SelTileRow>::from(vec![SelTileRow::default(); 3]);
        let grid = slint::VecModel::<FilmItem>::from(vec![FilmItem::default(); 4]);
        let sel_key = Cell::new((1, 2, 3, 4, 5, 6));
        let grid_key = Cell::new((1, 2, 3, 4, 5, 6, 7, 8, (9, 10)));
        release_unrefreshed_models(true, &sel, &sel_key, true, &grid, &grid_key);
        assert_eq!((sel.row_count(), grid.row_count()), (3, 4), "refreshing panels rebuild themselves");
        release_unrefreshed_models(false, &sel, &sel_key, false, &grid, &grid_key);
        assert_eq!((sel.row_count(), grid.row_count()), (0, 0));
        assert_eq!(sel_key.get(), SEL_KEY_UNBUILT);
        assert_eq!(grid_key.get(), GRID_KEY_UNBUILT);
    }
}
