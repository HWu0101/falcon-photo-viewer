//! Shared native Mac toolbar plus explicitly isolated diagnostic variants.
//! Bundle mode is frozen before settings/window creation. Every mode has isolated data.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static NATIVE_TOOLBAR_FAILED: AtomicBool = AtomicBool::new(false);

/// Selection is not health: after attachment fails, every chrome entry point
/// must use the in-window toolbar's window-button/fullscreen handling again.
fn native_toolbar_route(selected: bool, failed: bool) -> bool {
    selected && !failed
}
pub(crate) fn mark_toolbar_failed() {
    NATIVE_TOOLBAR_FAILED.store(true, Ordering::Release);
}

/// A missing layer is temporary during native transitions. Remember the surface,
/// not a stale colour choice; retry with the current output profile after it renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColourTagResult {
    Applied,
    SurfaceUnavailable,
    TargetRejected,
}
impl ColourTagResult {
    pub(crate) fn needs_retry(self) -> bool {
        self == Self::SurfaceUnavailable
    }
}
#[derive(Default)]
pub(crate) struct ColourTagRetry(std::collections::BTreeSet<usize>);
impl ColourTagRetry {
    pub(crate) fn record(&mut self, surface: usize, result: ColourTagResult) {
        if result.needs_retry() {
            self.0.insert(surface);
        } else {
            self.0.remove(&surface);
        }
    }
    pub(crate) fn pending(&self, surface: usize) -> bool {
        self.0.contains(&surface)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Candidate,
    Control,
    NativeHost,
    NativeReference,
    CompatHost,
}

impl Mode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Control => "control",
            Self::NativeHost => "native-host",
            Self::NativeReference => "native-reference",
            Self::CompatHost => "compat-host",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "candidate" => Some(Self::Candidate),
            "control" => Some(Self::Control),
            "native-host" => Some(Self::NativeHost),
            "native-reference" => Some(Self::NativeReference),
            "compat-host" => Some(Self::CompatHost),
            _ => None,
        }
    }
}

pub(crate) fn mode() -> Option<Mode> {
    #[cfg(all(target_os = "macos", feature = "mac-chrome-experiment", not(test)))]
    {
        static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
        Some(*MODE.get_or_init(|| {
            resources()
                .and_then(|p| std::fs::read_to_string(p.join("experiment-mode.txt")).ok())
                .as_deref()
                .and_then(Mode::parse)
                .unwrap_or(Mode::Candidate)
        }))
    }
    #[cfg(not(all(target_os = "macos", feature = "mac-chrome-experiment", not(test))))]
    {
        None
    }
}

pub(crate) fn full_candidate() -> bool {
    mode() == Some(Mode::Candidate)
}
fn requires_fixture(mode: Option<Mode>, ci: bool) -> bool {
    ci || (mode.is_some() && mode != Some(Mode::Candidate))
}
pub(crate) fn fixture_only() -> bool {
    requires_fixture(mode(), ci_smoke())
}

pub(crate) fn active() -> bool {
    mode().is_some()
}
/// Native toolbar selection is independent of diagnostic bundle identity/profile.
fn native_toolbar_for(mac: bool, mode: Option<Mode>) -> bool {
    mac && mode != Some(Mode::Control)
}
pub(crate) fn native_host() -> bool {
    native_toolbar_route(
        native_toolbar_for(cfg!(all(target_os = "macos", not(test))), mode()),
        NATIVE_TOOLBAR_FAILED.load(Ordering::Acquire),
    )
}
pub(crate) fn full_toolbar() -> bool {
    native_toolbar_route(
        full_toolbar_for(cfg!(all(target_os = "macos", not(test))), mode()),
        NATIVE_TOOLBAR_FAILED.load(Ordering::Acquire),
    )
}
fn full_toolbar_for(mac: bool, mode: Option<Mode>) -> bool {
    native_toolbar_for(mac, mode) && matches!(mode, None | Some(Mode::Candidate))
}
pub(crate) fn ci_smoke() -> bool {
    cfg!(all(target_os = "macos", not(test)))
        && std::env::var_os("FALCON_MAC_PROBE_SMOKE_OUT").is_some()
}
pub(crate) fn associations_allowed() -> bool {
    !active() && !ci_smoke()
}

pub(crate) fn uses_slint_bar(mode: Mode) -> bool {
    matches!(mode, Mode::Candidate | Mode::NativeHost | Mode::CompatHost)
}

pub(crate) fn build_label() -> String {
    if cfg!(feature = "mac-chrome-experiment") {
        format!("{}-mac-full04", env!("CARGO_PKG_VERSION"))
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

fn profile_for(home: &Path, mode: Mode) -> PathBuf {
    home.join("Library/Application Support/Falcon Mac Full 04")
        .join(mode.name())
}

pub(crate) fn profile_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if let Some(mode) = mode() {
        let p = profile_for(Path::new(&home), mode);
        Some(if ci_smoke() { p.join("ci-smoke") } else { p })
    } else if ci_smoke() {
        Some(Path::new(&home).join("Library/Application Support/Falcon/ci-smoke"))
    } else {
        None
    }
}

fn resources() -> Option<PathBuf> {
    Some(
        std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .join("Resources"),
    )
}

pub(crate) fn label() -> String {
    let revision = resources()
        .and_then(|p| std::fs::read_to_string(p.join("source-revision.txt")).ok())
        .filter(|s| s.trim().len() == 40 && s.trim().bytes().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.trim()[..8].to_string())
        .unwrap_or_else(|| "unbundled".into());
    format!(
        "{} · {} · {revision}",
        build_label(),
        mode().map_or("off", Mode::name)
    )
}

pub(crate) fn fixture() -> Option<PathBuf> {
    profile_dir().map(|p| p.join("fixture/chrome-test.png"))
}

pub(crate) fn allows_open(path: &Path) -> bool {
    !fixture_only() || fixture().is_some_and(|p| is_fixture_path(path, &p))
}

fn is_fixture_path(path: &Path, fixture: &Path) -> bool {
    path == fixture || Some(path) == fixture.parent()
}

pub(crate) fn prepare() -> Result<(), Box<dyn std::error::Error>> {
    if !active() && !ci_smoke() {
        return Ok(());
    }
    let path = fixture().ok_or("Mac experiment requires HOME for its isolated profile")?;
    std::fs::create_dir_all(path.parent().ok_or("invalid fixture path")?)?;
    std::fs::write(path, include_bytes!("../assets/chrome-test.png"))?;
    Ok(())
}

/// Apply at the settings-load boundary, including the later UI settings read.
pub(crate) fn prepare_settings(settings: &mut crate::support::Settings, diagnostic: bool) {
    if diagnostic {
        settings.onboarding_shown = true;
        settings.assoc_prompt_shown = true;
        // Round 2a: …and the guide's return after a feature update, for the same reason.
        settings.guide_seen_feature = crate::support::feature_version_text(env!("CARGO_PKG_VERSION"));
    }
}

/// The Mac title bar's height as AppKit laid it out: the content view's top minus the top of the
/// window's `contentLayoutRect`, both in window coordinates. The title-bar toolbar is sized to this,
/// never to Falcon's 44 pt Windows bar: AppKit fits a trailing title-bar accessory to the title bar
/// (38 pt in the compact toolbar style on macOS 15.7, 26.6 and 27.0) and clips anything taller.
/// `None` when the reading is not a plausible title bar (a transition's degenerate rect, NaN).
pub(crate) fn titlebar_band(content_top: f64, layout_top: f64) -> Option<f64> {
    let band = content_top - layout_top;
    (band.is_finite() && (24.0..=80.0).contains(&band)).then_some(band)
}

/// True when AppKit shows less of the toolbar surface than its frame, i.e. part of it is cut off.
/// Half a point of slack absorbs fractional layout.
pub(crate) fn toolbar_clipped(frame_h: f64, visible_h: f64) -> bool {
    frame_h.is_finite() && visible_h.is_finite() && visible_h + 0.5 < frame_h
}

/// The keys the Mac field log may name, by winit `KeyCode` debug name: modifiers, Caps Lock, the
/// input-source keys, arrows and paging, function keys and Escape. They tell an input-source switch
/// or a shortcut apart without revealing typed text. Every other key is `other`, even when macOS
/// supplies no text for it: a dead key or an unreadable keyboard layout also arrives without text
/// (Codex review O1, 2026-09-27).
pub(crate) fn diagnostic_key_name(debug_name: &str) -> &str {
    const NAMED: &[&str] = &[
        "ShiftLeft",
        "ShiftRight",
        "ControlLeft",
        "ControlRight",
        "AltLeft",
        "AltRight",
        "SuperLeft",
        "SuperRight",
        "Meta",
        "Hyper",
        "Fn",
        "FnLock",
        "CapsLock",
        "Lang1",
        "Lang2",
        "Lang3",
        "Lang4",
        "Lang5",
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp",
        "ArrowDown",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "Escape",
    ];
    let function_key = debug_name
        .strip_prefix('F')
        .is_some_and(|n| (1..=2).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()));
    if function_key || NAMED.contains(&debug_name) {
        debug_name
    } else {
        "other"
    }
}

/// The Mac field log line for a key press on the main window. `key` is the physical key's winit
/// `KeyCode` debug name, `None` when winit could not identify it. Typed text is never logged: only
/// whether there was any, its length, and whether it was exactly the F shortcut.
pub(crate) fn main_key_line(
    key: Option<&str>,
    text: Option<&str>,
    repeat: bool,
    ime_allowed: bool,
) -> String {
    format!(
        "main key pressed key={} physical_f={} text_present={} text_len={} exact_f={} repeat={repeat} ime_allowed={ime_allowed}",
        key.map_or("unidentified", diagnostic_key_name),
        key == Some("KeyF"),
        text.is_some(),
        text.map_or(0, |t| t.chars().count()),
        text == Some("f"),
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DonorReadiness {
    Waiting,
    Ready,
    WrongHandle,
    TimedOut,
}

pub(crate) fn donor_readiness(
    has_window: bool,
    has_appkit: bool,
    elapsed_ms: u128,
) -> DonorReadiness {
    if has_appkit {
        DonorReadiness::Ready
    } else if has_window {
        DonorReadiness::WrongHandle
    } else if elapsed_ms >= 5_000 {
        DonorReadiness::TimedOut
    } else {
        DonorReadiness::Waiting
    }
}

/// One step of the CI launch check's toolbar round trip (`FALCON_MAC_PROBE_SMOKE_OUT` only): press
/// the toolbar's Grid button, see the grid flip, press again, see it flip back.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SmokeStep {
    /// The last press has not run yet; look again on the next tick.
    Wait,
    /// Queue a Grid press, then continue in this phase.
    Press(u8),
    /// The round trip is over: `true` when the grid flipped and flipped back.
    Finish(bool),
}

/// How long the launch check waits for a queued press to run before it fails.
pub(crate) const SMOKE_PRESS_LIMIT_MS: u128 = 10_000;

/// `phase` is 0 before the first press, then the number of presses queued. `delivered` counts the
/// presses that have run, and `waited_ms` is the time since the first press was queued.
///
/// A press is queued from the timer callback so it runs outside the host's borrow, like a real
/// click. A due timer can fire before that queued work runs (for example from Slint's display-link
/// frame tick on macOS 14+, which runs timers outside winit's delivery of queued work), so on a slow
/// machine the next tick can arrive before the press has run (1.0.12's first cloud run). Each step
/// therefore waits for its press to be delivered, never for a tick.
pub(crate) fn smoke_round_trip(
    phase: u8,
    delivered: u32,
    grid_open: bool,
    original: bool,
    waited_ms: u128,
) -> SmokeStep {
    match phase {
        0 => SmokeStep::Press(1),
        p if delivered < u32::from(p) => {
            if waited_ms >= SMOKE_PRESS_LIMIT_MS {
                SmokeStep::Finish(false)
            } else {
                SmokeStep::Wait
            }
        }
        1 if grid_open == original => SmokeStep::Finish(false),
        1 => SmokeStep::Press(2),
        _ => SmokeStep::Finish(grid_open == original),
    }
}

#[cfg(target_os = "macos")]
#[path = "mac_experiment_native.rs"]
mod native;

#[cfg(any(test, target_os = "macos"))]
#[path = "mac_experiment_ui.rs"]
mod probe_ui;
#[cfg(target_os = "macos")]
pub(crate) use native::{
    apply, donor_being_created, invalidate_toolbar_colour, record_main_input, set_open_handler,
    shutdown, start, sync_toolbar,
};

#[cfg(test)]
mod tests {
    use super::*;
    // Falsifier: drop the `delivered` guard (check the grid on the tick after a press, as the check
    // did in 1.0.12's first cloud run) and the slow-machine step below fails instead of waiting.
    #[test]
    fn the_launch_check_waits_for_each_toolbar_press() {
        use SmokeStep::*;
        assert_eq!(smoke_round_trip(0, 0, false, false, 0), Press(1));
        // A slow machine: the next tick comes before press 1 has run.
        assert_eq!(smoke_round_trip(1, 0, false, false, 240), Wait);
        assert_eq!(smoke_round_trip(1, 1, true, false, 300), Press(2));
        assert_eq!(smoke_round_trip(2, 1, true, false, 320), Wait);
        assert_eq!(smoke_round_trip(2, 2, false, false, 400), Finish(true));
        // Starting from an open grid works the same way.
        assert_eq!(smoke_round_trip(1, 1, false, true, 300), Press(2));
        assert_eq!(smoke_round_trip(2, 2, true, true, 400), Finish(true));
        // Real failures still fail: a press that runs without flipping the grid, a second press
        // that leaves it flipped, and a press that never runs.
        assert_eq!(smoke_round_trip(1, 1, false, false, 300), Finish(false));
        assert_eq!(smoke_round_trip(2, 2, true, false, 400), Finish(false));
        assert_eq!(smoke_round_trip(1, 0, false, false, SMOKE_PRESS_LIMIT_MS), Finish(false));
        assert_eq!(smoke_round_trip(2, 1, true, false, SMOKE_PRESS_LIMIT_MS), Finish(false));
    }
    // O2 falsifier: route by selection alone; a failed native host would keep
    // suppressing the in-window toolbar's positioning and fullscreen observers.
    #[test]
    fn failed_native_toolbar_routes_all_chrome_calls_to_the_fallback() {
        for (selected, failed, use_native) in [
            (false, false, false),
            (false, true, false),
            (true, false, true),
            (true, true, false),
        ] {
            assert_eq!(native_toolbar_route(selected, failed), use_native);
        }
        assert!(!native_toolbar_route(native_toolbar_for(true, None), true));
        assert!(native_toolbar_route(native_toolbar_for(true, None), false));
    }
    // Falsifier: clear on an attempted application, or clear all surfaces when
    // one succeeds; a temporary missing layer then loses its retry.
    #[test]
    fn colour_tag_retry_waits_for_success_on_each_surface() {
        use ColourTagResult::{Applied, SurfaceUnavailable};
        let mut retry = ColourTagRetry::default();
        retry.record(1, SurfaceUnavailable);
        retry.record(2, SurfaceUnavailable);
        retry.record(1, Applied);
        assert!(!retry.pending(1));
        assert!(retry.pending(2));
        retry.record(2, SurfaceUnavailable);
        assert!(retry.pending(2));
        retry.record(2, Applied);
        assert!(!retry.pending(2));
    }

    // Y1 falsifier: treat every non-Applied result as retryable. A rejected ICC
    // target would then be parsed again on each render/tick.
    #[test]
    fn rejected_colour_targets_stop_retrying_until_an_explicit_new_attempt() {
        use ColourTagResult::{Applied, SurfaceUnavailable, TargetRejected};
        let mut retry = ColourTagRetry::default();
        retry.record(1, SurfaceUnavailable);
        retry.record(2, SurfaceUnavailable);
        retry.record(1, TargetRejected);
        assert!(!retry.pending(1));
        assert!(retry.pending(2), "other surface keeps its own retry");
        assert!(
            !TargetRejected.needs_retry(),
            "toolbar must clear its dirty retry too"
        );
        retry.record(1, SurfaceUnavailable); // a later explicit profile/host request
        assert!(retry.pending(1));
        retry.record(1, Applied);
        assert!(!retry.pending(1));
    }
    // Falsifier: require diagnostic mode for native_toolbar_for; normal Mac falls back
    // to the old traffic-light implementation despite a successful candidate test.
    #[test]
    fn normal_mac_gets_the_full_toolbar_without_a_diagnostic_identity() {
        assert!(native_toolbar_for(true, None));
        assert!(full_toolbar_for(true, None));
        assert!(!native_toolbar_for(false, None));
        assert!(!full_toolbar_for(false, Some(Mode::Candidate)));
        assert!(!native_toolbar_for(true, Some(Mode::Control)));
        assert!(native_toolbar_for(true, Some(Mode::NativeReference)));
        assert!(!full_toolbar_for(true, Some(Mode::NativeReference)));
        assert!(!requires_fixture(None, false));
        assert!(requires_fixture(None, true));
    }
    #[test]
    fn diagnostic_profiles_cannot_share_shipping_state_or_each_others_state() {
        let home = Path::new("/Users/tester");
        let control = profile_for(home, Mode::Control);
        let candidate = profile_for(home, Mode::NativeHost);
        let shipping = home.join("Library/Application Support/Falcon");
        assert_ne!(control, candidate);
        assert!(!control.starts_with(&shipping));
        assert!(!candidate.starts_with(&shipping));
        assert!(control.starts_with(home));
    }
    #[test]
    fn only_known_bundle_modes_are_accepted() {
        assert_eq!(Mode::parse("candidate"), Some(Mode::Candidate));
        assert_eq!(Mode::parse("control\n"), Some(Mode::Control));
        assert_eq!(Mode::parse("native-host"), Some(Mode::NativeHost));
        assert_eq!(Mode::parse("native-reference"), Some(Mode::NativeReference));
        assert_eq!(Mode::parse("compat-host"), Some(Mode::CompatHost));
        assert_eq!(Mode::parse("../Falcon"), None);
        assert_eq!(Mode::parse(""), None);
    }
    #[test]
    fn feature_cannot_activate_on_windows() {
        if !cfg!(target_os = "macos") {
            assert!(!active());
        }
    }
    #[test]
    fn all_modes_are_isolated_and_reference_has_no_donor() {
        let modes = [
            Mode::Candidate,
            Mode::Control,
            Mode::NativeHost,
            Mode::NativeReference,
            Mode::CompatHost,
        ];
        let paths: std::collections::HashSet<_> = modes
            .map(|m| profile_for(Path::new("/Users/tester"), m))
            .into();
        assert_eq!(paths.len(), 5);
        assert!(uses_slint_bar(Mode::Candidate));
        assert!(!uses_slint_bar(Mode::NativeReference));
        assert!(!uses_slint_bar(Mode::Control));
        assert!(uses_slint_bar(Mode::NativeHost));
        assert!(uses_slint_bar(Mode::CompatHost));
        assert_eq!(
            build_label().ends_with("-mac-full04"),
            cfg!(feature = "mac-chrome-experiment")
        );
    }
    #[test]
    fn candidate_opens_real_folders_but_smoke_and_probes_remain_fixture_only() {
        assert!(!requires_fixture(Some(Mode::Candidate), false));
        assert!(requires_fixture(Some(Mode::Candidate), true));
        assert!(requires_fixture(Some(Mode::NativeHost), false));
        assert!(requires_fixture(None, true));
    }
    #[test]
    fn isolated_open_gate_does_not_admit_sibling_photos_or_ancestors() {
        let fixture = Path::new("/Users/tester/experiment/fixture/chrome-test.png");
        assert!(is_fixture_path(fixture, fixture));
        assert!(is_fixture_path(fixture.parent().unwrap(), fixture));
        assert!(!is_fixture_path(
            Path::new("/Users/tester/experiment"),
            fixture
        ));
        assert!(!is_fixture_path(
            Path::new("/Users/tester/Pictures/photo.png"),
            fixture
        ));
        assert!(!is_fixture_path(
            Path::new("/Users/tester/experiment/fixture/other.png"),
            fixture
        ));
    }

    // full04-3 field log (macOS 27.0): window 1280×800 with contentLayoutRect 1280×762, and in Mac
    // full screen a 949 pt content view over a 911 pt layout rect, so the title bar is 38 pt both
    // ways. FALSIFIER: return `Some(44.0)` (the Windows bar height that the full04-3 build imposed)
    // and the first two rows fail.
    #[test]
    fn the_title_bar_height_is_read_from_appkit_not_imposed() {
        assert_eq!(titlebar_band(800.0, 762.0), Some(38.0));
        assert_eq!(titlebar_band(949.0, 911.0), Some(38.0));
        assert_eq!(
            titlebar_band(800.0, 800.0),
            None,
            "no title bar in the reading"
        );
        assert_eq!(
            titlebar_band(0.0, 0.0),
            None,
            "a detached view reads as zero"
        );
        assert_eq!(
            titlebar_band(762.0, 800.0),
            None,
            "layout above the content top"
        );
        assert_eq!(
            titlebar_band(900.0, 762.0),
            None,
            "138 pt is not a title bar"
        );
        assert_eq!(titlebar_band(f64::NAN, 762.0), None);
    }

    // Both the full04-3 tester log and cloud run 36111887084 (macOS 15.7.9) show the toolbar surface
    // at frame 44 / visible 38. FALSIFIER: return `false` and the first row fails.
    #[test]
    fn a_toolbar_taller_than_its_visible_slot_is_reported_as_clipped() {
        assert!(toolbar_clipped(44.0, 38.0));
        assert!(!toolbar_clipped(38.0, 38.0));
        assert!(
            !toolbar_clipped(38.0, 37.8),
            "fractional layout is not clipping"
        );
        assert!(!toolbar_clipped(f64::NAN, 0.0));
    }

    // Codex review O1 (2026-09-27): 7fa7de7 named every key outside KeyA–Z and Digit0–9 whenever
    // macOS gave no text, so a text-less Quote or Numpad1 press logged its name. Only diagnostic
    // keys may be named, and typed text never appears. FALSIFIER: make `diagnostic_key_name` return
    // `debug_name` for every key (7fa7de7's text-less fallback) and the first loop fails.
    #[test]
    fn the_field_key_log_names_only_diagnostic_keys_and_never_text() {
        let character_keys = [
            "KeyA",
            "KeyF",
            "Digit1",
            "Quote",
            "Numpad1",
            "NumpadAdd",
            "IntlBackslash",
            "IntlRo",
            "IntlYen",
            "Backquote",
            "Comma",
            "Semicolon",
            "Space",
            "Enter",
            "Backspace",
            "Tab",
        ];
        for key in character_keys {
            for text in [None, Some(""), Some("\u{0}"), Some("\u{1b}")] {
                let line = main_key_line(Some(key), text, false, false);
                assert!(
                    line.starts_with("main key pressed key=other "),
                    "{key} with text {text:?} must stay anonymous: {line}"
                );
            }
        }
        let diagnostic_keys = [
            "ControlLeft",
            "ShiftRight",
            "SuperLeft",
            "AltLeft",
            "CapsLock",
            "Fn",
            "Lang1",
            "Lang2",
            "F11",
            "F24",
            "ArrowLeft",
            "PageDown",
            "Escape",
        ];
        for key in diagnostic_keys {
            let line = main_key_line(Some(key), None, false, false);
            assert!(line.contains(&format!("key={key} ")), "{key}: {line}");
        }
        assert!(main_key_line(None, None, false, false).contains("key=unidentified "));
        // The deliberate F diagnostic survives; the text itself never appears.
        assert_eq!(
            main_key_line(Some("KeyF"), Some("f"), false, false),
            "main key pressed key=other physical_f=true text_present=true text_len=1 exact_f=true repeat=false ime_allowed=false"
        );
        let line = main_key_line(Some("KeyQ"), Some("拼"), true, true);
        assert!(!line.contains('拼') && line.contains("key=other ") && line.contains("text_len=1"));
    }

    #[test]
    fn a_deferred_native_window_waits_without_recreating_the_component() {
        use DonorReadiness::*;
        assert_eq!(donor_readiness(false, false, 0), Waiting);
        assert_eq!(donor_readiness(false, false, 100), Waiting);
        assert_eq!(donor_readiness(true, true, 100), Ready);
        assert_eq!(donor_readiness(false, false, 5_000), TimedOut);
        assert_eq!(donor_readiness(true, false, 100), WrongHandle);
    }

    #[test]
    fn every_diagnostic_settings_read_suppresses_both_first_run_surfaces() {
        for _read in 0..2 {
            let mut settings = crate::support::Settings::default();
            prepare_settings(&mut settings, true);
            assert!(!crate::support::onboarding_boot(settings.onboarding_shown).0);
            assert_eq!(
                crate::support::assoc_boot(settings.onboarding_shown, settings.assoc_prompt_shown),
                (false, false, false)
            );
        }
        let mut normal = crate::support::Settings::default();
        let before = (normal.onboarding_shown, normal.assoc_prompt_shown);
        prepare_settings(&mut normal, false);
        assert_eq!((normal.onboarding_shown, normal.assoc_prompt_shown), before);
    }
}
