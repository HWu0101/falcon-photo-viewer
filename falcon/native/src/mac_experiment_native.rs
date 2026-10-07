//! Mac full04 toolbar host; optional retained diagnostic variants, no delegate replacement.
//! A second, deliberately small Slint/winit/FemtoVG-wgpu surface supplies the accessory
//! pixels. The opt-in winit extension retains its typed view independently of the hidden
//! donor's contentView. Event identity stays on the donor; coordinates/scale use the real
//! AppKit host. Focus/tab traversal and display changes still need real-Mac verification.
use super::probe_ui::MacExperimentBar;
use crate::ui::{Theme, Tip};
use crate::{support::log_event, MainWindow};
use i_slint_backend_winit::winit::platform::macos::WindowExtMacOS;
use i_slint_backend_winit::WinitWindowAccessor;
use objc2::declare::ClassBuilder;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, Sel};
use objc2::{class, msg_send, msg_send_id, sel, ClassType};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[path = "mac_chrome_compat.rs"]
mod compatibility;

type OpenFile = std::rc::Rc<dyn Fn(std::path::PathBuf)>;

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static TIMER: slint::Timer = slint::Timer::default();
    static OPEN_FILE: RefCell<Option<OpenFile>> = const { RefCell::new(None) };
    static CREATING_DONOR: Cell<bool> = const { Cell::new(false) };
}
static EVENTS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static TRACE_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static WAKE_QUEUED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const NOTIFICATIONS: &[&str] = &[
    "NSWindowWillEnterFullScreenNotification",
    "NSWindowDidEnterFullScreenNotification",
    "NSWindowWillExitFullScreenNotification",
    "NSWindowDidExitFullScreenNotification",
    "NSWindowDidResizeNotification",
    "NSWindowDidMoveNotification",
    "NSWindowDidChangeBackingPropertiesNotification",
    "NSWindowDidChangeScreenNotification",
    "NSWindowDidBecomeKeyNotification",
    "NSWindowDidResignKeyNotification",
    "NSWindowDidMiniaturizeNotification",
    "NSWindowDidDeminiaturizeNotification",
    "NSWindowDidChangeOcclusionStateNotification",
    "NSWindowWillCloseNotification",
];

pub(crate) fn set_open_handler(handler: OpenFile) {
    OPEN_FILE.with(|slot| *slot.borrow_mut() = Some(handler));
}

pub(crate) fn donor_being_created() -> bool {
    CREATING_DONOR.with(Cell::get)
}

fn elapsed_ms() -> u128 {
    TRACE_START.get_or_init(Instant::now).elapsed().as_millis()
}
fn record(s: impl AsRef<str>) {
    log_event(&format!(
        "mac-experiment: +{}ms {}",
        elapsed_ms(),
        s.as_ref()
    ));
}

fn queue(s: impl Into<String>) {
    {
        let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
        if events.len() < 512 {
            events.push_back(s.into());
        } else if events
            .back()
            .is_none_or(|s| s != "diagnostic event queue overflow")
        {
            events.push_back("diagnostic event queue overflow".into());
        }
    }
    if !WAKE_QUEUED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let result = slint::invoke_from_event_loop(|| {
            WAKE_QUEUED.store(false, std::sync::atomic::Ordering::Relaxed);
            TIMER.with(|timer| {
                if timer.running() {
                    timer.set_interval(Duration::from_millis(16));
                    timer.restart();
                }
            });
        });
        if result.is_err() {
            WAKE_QUEUED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

extern "C" fn shell_will_move(this: *mut AnyObject, _sel: Sel, next: *mut AnyObject) {
    unsafe {
        let old: *mut AnyObject = msg_send![this, window];
        if old != next {
            compatibility::detach(old);
        }
        let _: () = msg_send![super(this, class!(NSView)), viewWillMoveToWindow: next];
    }
}

extern "C" fn shell_moved(this: *mut AnyObject, _sel: Sel) {
    // Native only, no Slint or Host borrow on a re-entrant AppKit lifecycle edge.
    unsafe {
        let _: () = msg_send![super(this, class!(NSView)), viewDidMoveToWindow];
        let window: *mut AnyObject = msg_send![this, window];
        compatibility::attach(window);
        queue(format!("accessory-lifecycle host={window:p}"));
    }
}

fn shell_class() -> Option<&'static AnyClass> {
    if let Some(c) = AnyClass::get("FalconChrome02Accessory") {
        return Some(c);
    }
    let mut c = ClassBuilder::new("FalconChrome02Accessory", class!(NSView))?;
    unsafe {
        c.add_method(
            sel!(viewDidMoveToWindow),
            shell_moved as extern "C" fn(_, _),
        );
    }
    unsafe {
        c.add_method(
            sel!(viewWillMoveToWindow:),
            shell_will_move as extern "C" fn(_, _, _),
        );
    }
    Some(c.register())
}

fn show_styled_popup(x: f64, y: f64) {
    let refs = HOST.with(|h| {
        h.borrow().as_ref().and_then(|h| {
            Some((
                h.donor_view.as_ref()?.clone(),
                h.main_view.clone(),
                h.window.clone(),
                h.app.clone(),
            ))
        })
    });
    let Some((view, main_view, main_window, app)) = refs else {
        return;
    };
    let Some(app) = app.upgrade() else {
        return;
    };
    unsafe {
        let actual: *mut AnyObject = msg_send![&*view, window];
        if actual.is_null() {
            return;
        }
        let p: NSPoint = msg_send![&*view, convertPoint: NSPoint::new(x, y) toView: std::ptr::null::<AnyObject>()];
        let p: NSPoint = msg_send![actual, convertPointToScreen: p];
        let p: NSPoint = msg_send![&*main_window, convertPointFromScreen: p];
        let p: NSPoint =
            msg_send![&*main_view, convertPoint: p fromView: std::ptr::null::<AnyObject>()];
        if !p.x.is_finite() || !p.y.is_finite() {
            return;
        }
        app.set_mac_experiment_popup_x(p.x as f32);
        app.set_mac_experiment_popup_y(p.y as f32);
        focus_photo(&app);
        app.set_mac_experiment_popup_open(true);
        record(format!("styled popup main anchor={p:?}"));
    }
}

extern "C" fn notification(_this: *mut AnyObject, _sel: Sel, note: *mut AnyObject) {
    // AppKit can call while Host is being changed. Queue data only; never borrow Host or
    // call Slint here. The event name records COMPLETED did-enter/exit separately from intent.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if note.is_null() {
            return;
        }
        let name: *mut NSString = msg_send![note, name];
        let object: *mut AnyObject = msg_send![note, object];
        if name.is_null() {
            return;
        }
        let text = format!(
            "event={} object={object:p} received_at={}ms",
            &*name,
            elapsed_ms()
        );
        queue(text);
    }));
    if result.is_err() {
        record("notification panic recovered");
    }
}

fn observer_class() -> Option<&'static AnyClass> {
    if let Some(c) = AnyClass::get("FalconChromeExperimentObserver") {
        return Some(c);
    }
    let mut c = ClassBuilder::new("FalconChromeExperimentObserver", NSObject::class())?;
    unsafe {
        c.add_method(
            sel!(changed:),
            notification as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
        );
    }
    unsafe {
        c.add_method(
            sel!(nativeClick:),
            native_click as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
        );
    }
    unsafe {
        c.add_method(
            sel!(popupClick:),
            popup_click as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
        );
    }
    Some(c.register())
}

extern "C" fn native_click(_this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
    queue("native-click");
}

extern "C" fn popup_click(_this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
    queue("popup-click");
}

fn show_popup() {
    // Release Host's RefCell borrow BEFORE native menu tracking can re-enter the run loop.
    let refs = HOST.with(|h| {
        h.borrow()
            .as_ref()
            .and_then(|h| Some((h.shell.as_ref()?.clone(), h.observer.clone())))
    });
    let Some((anchor, observer)) = refs else {
        return;
    };
    unsafe {
        let menu: Retained<AnyObject> = msg_send_id![class!(NSMenu), new];
        let item: Allocated<AnyObject> = msg_send_id![class!(NSMenuItem), alloc];
        let title = NSString::from_str("Count + close");
        let key = NSString::from_str("");
        let item: Retained<AnyObject> = msg_send_id![item, initWithTitle: &*title action: sel!(popupClick:) keyEquivalent: &*key];
        let _: () = msg_send![&*item, setTarget: &*observer];
        let _: () = msg_send![&*menu, addItem: &*item];
        record("native popup opened from Slint control");
        let _: Bool = msg_send![&*menu, popUpMenuPositioningItem: std::ptr::null::<AnyObject>() atLocation: NSPoint::new(220.0, 0.0) inView: &*anchor];
        record("native popup tracking finished");
    }
}

unsafe fn retained(ptr: *mut AnyObject) -> Result<Retained<AnyObject>, &'static str> {
    Retained::retain(ptr).ok_or("missing native object")
}

// The full candidate and retained diagnostic modes share one native lifecycle.
enum Bar {
    Probe(MacExperimentBar),
    Full(crate::ui::MacToolbarWindow),
}
impl Bar {
    fn window(&self) -> &slint::Window {
        match self {
            Self::Probe(b) => b.window(),
            Self::Full(b) => b.window(),
        }
    }
    fn show(&self) -> Result<(), slint::PlatformError> {
        match self {
            Self::Probe(b) => b.show(),
            Self::Full(b) => b.show(),
        }
    }
    fn hide(&self) -> Result<(), slint::PlatformError> {
        match self {
            Self::Probe(b) => b.hide(),
            Self::Full(b) => b.hide(),
        }
    }
    fn count_click(&self) {
        if let Self::Probe(b) = self {
            b.set_clicks(b.get_clicks() + 1);
        }
    }
}

// Coordinates cross NSWindow boundaries in fullscreen. Never assume the accessory stays
// in the main NSWindow or subtract a guessed system-titlebar height.
unsafe fn main_anchor(host: &Host, x: f64, y: f64) -> Option<NSPoint> {
    if !host.attached || !host.initialized {
        return None;
    }
    let view = host.donor_view.as_ref()?;
    let actual: *mut AnyObject = msg_send![&**view, window];
    if actual.is_null() {
        return None;
    }
    let p: NSPoint =
        msg_send![&**view, convertPoint: NSPoint::new(x, y) toView: std::ptr::null::<AnyObject>()];
    let p: NSPoint = msg_send![actual, convertPointToScreen: p];
    let p: NSPoint = msg_send![&*host.window, convertPointFromScreen: p];
    let p: NSPoint =
        msg_send![&*host.main_view, convertPoint: p fromView: std::ptr::null::<AnyObject>()];
    (p.x.is_finite() && p.y.is_finite()).then_some(p)
}

fn toolbar_action(action: slint::SharedString, x: f32, y: f32) {
    let target = HOST.with(|slot| {
        let slot = slot.try_borrow().ok()?;
        let host = slot.as_ref()?;
        let app = host.app.upgrade()?;
        if app.get_immersive() {
            return None;
        }
        Some((app, unsafe { main_anchor(host, x as f64, y as f64)? }))
    });
    if let Some((app, p)) = target {
        // Transfer native AND Slint focus, before a popup installs its own focus scope.
        // Drag must stay on the original mouse event's stack; there is no Host borrow here.
        if !app.get_modal_blocking() {
            unsafe {
                focus_photo(&app);
            }
        }
        // AppKit can place the accessory above the main content view. Its popup
        // must still begin inside drawable content, even across a native reveal gap.
        app.invoke_toolbar_action(action, p.x as f32, (p.y as f32).max(app.get_content_top()));
        sync_toolbar(&app);
    }
}

fn create_full_bar(app: &MainWindow) -> Result<crate::ui::MacToolbarWindow, slint::PlatformError> {
    CREATING_DONOR.with(|v| v.set(true));
    let created = crate::ui::MacToolbarWindow::new().inspect(|b| {
        let _ = b.window().has_winit_window();
    });
    CREATING_DONOR.with(|v| v.set(false));
    let bar = created?;
    bar.set_state(app.get_toolbar_state());
    copy_theme(app, &bar);
    bar.on_action(toolbar_action);
    bar.on_meter_hover(|hover, x, y| {
        // A width/state observer can invoke this while Host is borrowed. Defer it.
        let _ = slint::invoke_from_event_loop(move || {
            let target = HOST.with(|slot| {
                let slot = slot.borrow();
                let host = slot.as_ref()?;
                Some((host.app.upgrade()?, unsafe {
                    main_anchor(host, x as f64, y as f64)
                }))
            });
            if let Some((app, p)) = target {
                app.set_cache_meter_hover(hover && p.is_some());
                if let Some(p) = p {
                    app.set_cache_meter_cx(p.x as f32);
                    app.set_cache_meter_cy((p.y as f32 + 3.0).max(3.0));
                }
            }
        });
    });
    let aw = app.as_weak();
    bar.on_forward_key(move |event, pressed| {
        if let Some(app) = aw.upgrade() {
            // There are no text fields in the toolbar. Reuse the original handler verbatim:
            // modifiers, repeats, remapping, modal gates and menu-target routing all survive.
            app.invoke_toolbar_key(event, pressed);
        }
    });
    let aw = app.as_weak();
    bar.on_dbg_chips_log(move |a, b, c, d, e, f, g, h, i| {
        if let Some(app) = aw.upgrade() {
            app.invoke_dbg_chips_log(a, b, c, d, e, f, g, h, i);
        }
    });
    Ok(bar)
}

fn copy_theme(app: &MainWindow, bar: &crate::ui::MacToolbarWindow) {
    let from = app.global::<Theme>();
    let to = bar.global::<Theme>();
    // Same transformed tokens as the main surface; compare before dirtying any binding.
    macro_rules! token {
        ($get:ident, $set:ident) => {
            if to.$get() != from.$get() {
                to.$set(from.$get());
            }
        };
    }
    token!(get_stage, set_stage);
    token!(get_base, set_base);
    token!(get_block, set_block);
    token!(get_well, set_well);
    token!(get_well_hover, set_well_hover);
    token!(get_block_hover, set_block_hover);
    token!(get_badge_bg, set_badge_bg);
    token!(get_scrim, set_scrim);
    token!(get_scrim_heavy, set_scrim_heavy);
    token!(get_scrim_light, set_scrim_light);
    token!(get_scrim_edge, set_scrim_edge);
    token!(get_scrim_edge_clear, set_scrim_edge_clear);
    token!(get_border, set_border);
    token!(get_border_strong, set_border_strong);
    token!(get_border_lift, set_border_lift);
    token!(get_border_select, set_border_select);
    token!(get_glow_select, set_glow_select);
    token!(get_glow_select_hi, set_glow_select_hi);
    token!(get_divider, set_divider);
    token!(get_text, set_text);
    token!(get_text_dim, set_text_dim);
    token!(get_text_mute, set_text_mute);
    token!(get_text_on_accent, set_text_on_accent);
    token!(get_text_on_star, set_text_on_star);
    token!(get_accent, set_accent);
    token!(get_accent_soft, set_accent_soft);
    token!(get_accent_soft_hover, set_accent_soft_hover);
    token!(get_accent_web, set_accent_web);
    token!(get_accent_web_soft, set_accent_web_soft);
    token!(get_danger, set_danger);
    token!(get_danger_soft, set_danger_soft);
    token!(get_danger_soft_hover, set_danger_soft_hover);
    token!(get_star, set_star);
    token!(get_warn, set_warn);
    token!(get_warn_soft, set_warn_soft);
    token!(get_meter_good, set_meter_good);
    token!(get_leaf, set_leaf);
    token!(get_hover, set_hover);
    token!(get_hover_flat, set_hover_flat);
    token!(get_hover_flat_block, set_hover_flat_block);
    token!(get_accent_hover_flat, set_accent_hover_flat);
    token!(get_danger_hover_flat, set_danger_hover_flat);
    token!(get_accent_soft_flat, set_accent_soft_flat);
    token!(get_danger_soft_flat, set_danger_soft_flat);
    token!(get_wash_rest, set_wash_rest);
    token!(get_shadow_control, set_shadow_control);
    token!(get_shadow_panel, set_shadow_panel);
    token!(get_shadow_lift, set_shadow_lift);
}

fn sync_bar_state(host: &mut Host, app: &MainWindow) {
    let Some(Bar::Full(bar)) = &host.bar else {
        return;
    };
    let state = app.get_toolbar_state();
    if host.last_state.as_ref() != Some(&state) {
        bar.set_state(state.clone());
        host.last_state = Some(state);
    }
    if host.colour_dirty {
        copy_theme(app, bar);
        if let Some(view) = crate::mac_ns_view_of(bar.window()) {
            // A fullscreen transition can temporarily remove the layer. Keep retrying
            // on the existing adaptive tick until the actual surface accepts its tag.
            host.colour_dirty =
                crate::support::apply_metal_layer_colorspace(view, app.get_output_gamut())
                    .needs_retry();
        }
    }
    let tip = bar.get_tip_text();
    let main_tip = app.global::<Tip>();
    if !tip.is_empty() && host.attached && !app.get_modal_blocking() {
        if let Some(p) = unsafe { main_anchor(host, bar.get_tip_x() as f64, host.bar_height(app)) }
        {
            // Toolbar owns this channel only while its own hover remains live.
            if main_tip.get_text() != tip {
                main_tip.set_text(tip.clone());
            }
            main_tip.set_cx(p.x as f32);
            main_tip.set_cy((p.y as f32 + 3.0).max(3.0));
            host.last_tip = tip;
        }
    } else if !host.last_tip.is_empty() {
        if main_tip.get_text() == host.last_tip {
            main_tip.set_text("".into());
            main_tip.set_cy(0.0);
        }
        host.last_tip = "".into();
    }
}

// Piggyback on Falcon's existing adaptive UI tick; no extra permanent 60Hz timer.
pub(crate) fn sync_toolbar(app: &MainWindow) {
    if !super::full_toolbar() {
        return;
    }
    HOST.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            if let Some(host) = slot.as_mut() {
                sync_bar_state(host, app);
            }
        }
    });
}
pub(crate) fn invalidate_toolbar_colour() {
    HOST.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            if let Some(host) = slot.as_mut() {
                host.colour_dirty = true;
            }
        }
    });
}

struct Host {
    app: slint::Weak<MainWindow>,
    window: Retained<AnyObject>,
    main_view: Retained<AnyObject>,
    observer: Retained<AnyObject>,
    bar: Option<Bar>,
    donor: Option<Retained<AnyObject>>,
    donor_view: Option<Retained<AnyObject>>,
    observed_host: Option<Retained<AnyObject>>,
    accessory: Option<Retained<AnyObject>>,
    shell: Option<Retained<AnyObject>>,
    toolbar: Option<Retained<AnyObject>>,
    native_button: Option<Retained<AnyObject>>,
    native_clicks: u64,
    transition: Option<(bool, Instant)>,
    old_toolbar: Option<Retained<AnyObject>>,
    old_style: isize,
    attached: bool,
    immersive: Option<bool>,
    last_geometry: String,
    last_report: Instant,
    last_sample: Instant,
    hot_until: Instant,
    events: u64,
    writes: u64,
    last_focus: Option<bool>,
    scale_mismatch_samples: u8,
    /// The last plausible title-bar height AppKit reported (`super::titlebar_band`).
    bar_h: Option<f64>,
    band_warned: bool,
    /// Whether AppKit showed less of the toolbar than its height at the last sample.
    clipped: Option<bool>,
    initialized: bool,
    startup: Instant,
    bar_frames: std::rc::Rc<Cell<u64>>,
    attached_frame_baseline: u64,
    smoke_phase: u8,
    smoke_original_grid: bool,
    /// When the launch check queued its first Grid press (`super::smoke_round_trip`).
    smoke_started: Option<Instant>,
    colour_dirty: bool,
    last_tip: slint::SharedString,
    last_state: Option<crate::ui::ToolbarState>,
}

impl Host {
    unsafe fn new(app: &MainWindow) -> Result<Self, Box<dyn std::error::Error>> {
        let main_view = retained(
            crate::mac_ns_view_of(app.window())
                .ok_or("main view not ready")?
                .cast(),
        )?;
        let window = retained(msg_send![&*main_view, window])?;
        let c = observer_class().ok_or("observer class registration failed")?;
        let observer: Retained<AnyObject> = msg_send_id![c, new];
        let old_toolbar = Retained::retain(msg_send![&*window, toolbar]);
        let old_style = msg_send![&*window, toolbarStyle];
        let now = Instant::now();
        let mut host = Self {
            app: app.as_weak(),
            window,
            main_view,
            observer,
            bar: None,
            donor: None,
            donor_view: None,
            observed_host: None,
            accessory: None,
            shell: None,
            toolbar: None,
            native_button: None,
            native_clicks: 0,
            transition: None,
            old_toolbar,
            old_style,
            attached: false,
            immersive: None,
            last_geometry: String::new(),
            last_report: now,
            last_sample: now,
            hot_until: now + Duration::from_secs(2),
            events: 0,
            writes: 0,
            last_focus: None,
            scale_mismatch_samples: 0,
            bar_h: None,
            band_warned: false,
            clipped: None,
            initialized: !super::native_host(),
            startup: now,
            bar_frames: std::rc::Rc::new(Cell::new(0)),
            attached_frame_baseline: 0,
            smoke_phase: 0,
            smoke_original_grid: false,
            smoke_started: None,
            colour_dirty: true,
            last_tip: slint::SharedString::default(),
            last_state: None,
        };
        host.observe(&host.window, NOTIFICATIONS);
        if super::native_host() {
            host.initialized = host.create_accessory(app)?;
        }
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        let os: *mut NSString = msg_send![info, operatingSystemVersionString];
        record(format!(
            "{} os={} target_bar={} target_light_center_x=20 legacy_chrome={}",
            super::label(),
            if os.is_null() {
                "unknown".into()
            } else {
                (&*os).to_string()
            },
            app.get_titlebar_h(),
            !super::native_host()
        ));
        if host.initialized {
            host.finish_initialization(app);
        } else {
            record("waiting for asynchronous donor creation; component retained, not recreated");
            app.set_mac_experiment_status("Preparing the toolbar's hidden window…".into());
        }
        Ok(host)
    }

    unsafe fn finish_initialization(&mut self, app: &MainWindow) {
        self.update(app.get_immersive());
        sync_bar_state(self, app);
        // The toolbar style and NSToolbar were set in this same turn; settle AppKit's title-bar
        // layout so the first height reading is the compact title bar's, not the plain one's.
        let _: () = msg_send![&*self.window, layoutIfNeeded];
        self.layout(); // Attachment precedes the initial layout; no first-frame narrow bar.
        self.attached_frame_baseline = self.bar_frames.get();
        if let Some(bar) = &self.bar {
            bar.window().request_redraw();
        }
        focus_photo(app);
        self.sample("installed");
        app.set_mac_experiment_status(
            "Toolbar placed in the title bar; waiting for it to draw…".into(),
        );
        record("HOST ATTACHED");
    }

    unsafe fn observe(&self, object: &AnyObject, names: &[&str]) {
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        for name in names {
            let name = NSString::from_str(name);
            let _: () = msg_send![center, addObserver: &*self.observer selector: sel!(changed:) name: &*name object: object];
        }
    }

    unsafe fn create_accessory(
        &mut self,
        app: &MainWindow,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let compat = super::mode() == Some(super::Mode::CompatHost);
        if compat {
            let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
            let version: objc2_foundation::NSOperatingSystemVersion =
                msg_send![info, operatingSystemVersion];
            if version.majorVersion != 26 {
                return Err("compatibility probe is restricted to macOS 26; use Public Host or Native Reference on other versions".into());
            }
        }
        if super::full_toolbar() || super::mode().is_some_and(super::uses_slint_bar) {
            if self.bar.is_none() {
                let bar = if super::full_toolbar() {
                    Bar::Full(create_full_bar(app)?)
                } else {
                    // The attributes hook runs at adapter creation (new), not at show().
                    CREATING_DONOR.with(|v| v.set(true));
                    let created = MacExperimentBar::new().inspect(|bar| {
                        // Force even a lazily allocated adapter to capture the donor-only attributes.
                        let _ = bar.window().has_winit_window();
                    });
                    CREATING_DONOR.with(|v| v.set(false));
                    let bar = created?;
                    bar.on_action(|s| {
                        record(format!("Slint {s}"));
                        queue("input-edge");
                    });
                    bar.on_popup(|| queue("show-native-menu"));
                    bar.on_styled_popup(|x, y| queue(format!("show-styled-menu {x} {y}")));
                    // Don't log text content: a length + count proves delivery without recording accidental typing.
                    bar.on_text_check(|s| {
                        record(format!("Slint text edited chars={}", s.chars().count()))
                    });
                    let aw = app.as_weak();
                    bar.on_immersive(move || {
                        if let Some(a) = aw.upgrade() {
                            a.invoke_toggle_fullscreen();
                        }
                    });
                    let aw = app.as_weak();
                    bar.on_refocus(move || {
                        if let Some(a) = aw.upgrade() {
                            unsafe {
                                focus_photo(&a);
                            }
                            record("photo focus requested");
                        }
                    });
                    let aw = app.as_weak();
                    bar.on_drag(move || {
                        if let Some(a) = aw.upgrade() {
                            a.window().with_winit_window(|w| {
                                let _ = w.drag_window();
                            });
                        }
                    });
                    Bar::Probe(bar)
                };
                let rendered = std::rc::Rc::new(Cell::new(false));
                let frames = self.bar_frames.clone();
                let colour_app = app.as_weak();
                let colour_bar = match &bar {
                    Bar::Full(b) => Some(b.as_weak()),
                    _ => None,
                };
                bar.window().set_rendering_notifier(move |state, _| {
                    if matches!(state, slint::RenderingState::AfterRendering) {
                        frames.set(frames.get().saturating_add(1));
                        if let (Some(a), Some(b)) = (colour_app.upgrade(), colour_bar.as_ref().and_then(|b| b.upgrade())) {
                            if let Some(view) = crate::mac_ns_view_of(b.window()) {
                                crate::support::retry_metal_layer_colorspace(view, a.get_output_gamut());
                            }
                        }
                    }
            if matches!(state, slint::RenderingState::AfterRendering) && !rendered.replace(true) {
                // Layer creation can be later than host attachment. Tag the first actual
                // layer as well as subsequent colour/scale invalidations.
                if let (Some(a), Some(b)) = (colour_app.upgrade(), colour_bar.as_ref().and_then(|b| b.upgrade())) {
                    if let Some(view) = crate::mac_ns_view_of(b.window()) {
                        crate::support::apply_metal_layer_colorspace(view, a.get_output_gamut());
                    }
                }
                record("Slint accessory first render submitted (visual presence still needs Mac observation)");
            }
        })?;
                bar.window().on_winit_window_event(|_, event| {
                    use i_slint_backend_winit::{winit::event::WindowEvent as W, EventResult};
                    if !matches!(event, W::RedrawRequested) {
                        crate::note_menu_activity();
                    }
                    if let W::DroppedFile(path) = event {
                        let path = path.clone();
                        let callback = OPEN_FILE.with(|slot| slot.borrow().clone());
                        if let Some(callback) = callback {
                            callback(path);
                        }
                        return EventResult::PreventDefault;
                    }
                    match event {
                        W::Occluded(_) | W::Focused(_) => {
                            // Donor remains ordered out; its occlusion/focus is not the hosted view's.
                            // Actual host focus is forwarded by the observer below. Renderer redraw
                            // remains on-demand; do not start a continuous repaint loop.
                            return EventResult::PreventDefault;
                        }
                        W::KeyboardInput { event, .. } => queue(format!(
                            "input-key state={:?} repeat={}",
                            event.state, event.repeat
                        )),
                        W::ModifiersChanged(m) => queue(format!("input-modifiers {:?}", m.state())),
                        W::MouseInput { state, button, .. } => {
                            queue(format!("input-button {button:?} {state:?}"))
                        }
                        W::ScaleFactorChanged { scale_factor, .. } => {
                            queue(format!("input-scale {scale_factor}"))
                        }
                        _ => {}
                    }
                    EventResult::Propagate
                });
                // Slint registers a newly constructed adapter for native creation at about_to_wait.
                // Keep it hidden until a later event-loop turn exposes the winit window.
                self.bar = Some(bar);
                return Ok(false);
            }
            let bar = self.bar.as_ref().ok_or("bar missing")?;
            let raw = crate::mac_ns_view_of(bar.window());
            match super::donor_readiness(
                bar.window().has_winit_window(),
                raw.is_some(),
                self.startup.elapsed().as_millis(),
            ) {
                super::DonorReadiness::Waiting => return Ok(false),
                super::DonorReadiness::TimedOut => {
                    return Err("the toolbar's hidden window was not created within 5 s".into())
                }
                super::DonorReadiness::WrongHandle => {
                    return Err("the toolbar's hidden window has no Mac view to move".into())
                }
                super::DonorReadiness::Ready => {}
            }
            let donor_view = retained(raw.ok_or("ready donor handle lost")?.cast())?;
            let donor = retained(msg_send![&*donor_view, window])?;
            let _: () = msg_send![&*donor, setAlphaValue: 0.0f64];
            let _: () = msg_send![&*donor, setIgnoresMouseEvents: Bool::YES];
            let _: () = msg_send![&*donor, setExcludedFromWindowsMenu: Bool::YES];
            // This owned renderer donor must not become an invisible window-cycling target.
            let _: () = msg_send![&*donor, setCollectionBehavior: 72usize]; // Transient | IgnoresCycle
            self.donor = Some(donor);
            self.donor_view = Some(donor_view);
            // Show only AFTER making the already-created native window transparent/inactive.
            // Slint's renderer becomes active without exposing a temporary donor window.
            if super::full_toolbar() {
                bar.window().with_winit_window(|w| w.set_ime_allowed(false));
            }
            bar.show()?;
            let _: () = msg_send![&**self.donor.as_ref().ok_or("donor missing")?, orderOut: std::ptr::null::<AnyObject>()];
        }

        // Placeholder until attachment: AppKit fits a trailing accessory to the title bar, and the
        // first `layout()` then sizes shell, surface and `fullScreenMinHeight` to the measured height.
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(780.0, app.get_titlebar_h() as f64),
        );
        let shell_alloc: Allocated<AnyObject> =
            msg_send_id![shell_class().ok_or("shell class failed")?, alloc];
        let shell: Retained<AnyObject> = msg_send_id![shell_alloc, initWithFrame: frame];
        if !super::full_toolbar() {
            let title = NSString::from_str("Native 0");
            let button: Retained<AnyObject> = msg_send_id![class!(NSButton), buttonWithTitle: &*title target: &*self.observer action: sel!(nativeClick:)];
            let button_frame = NSRect::new(NSPoint::new(0.0, 8.0), NSSize::new(76.0, 28.0));
            let _: () = msg_send![&*button, setFrame: button_frame];
            let _: () = msg_send![&*shell, addSubview: &*button];
            self.native_button = Some(button);
        }
        let accessory: Retained<AnyObject> =
            msg_send_id![class!(NSTitlebarAccessoryViewController), new];
        let _: () = msg_send![&*accessory, setView: &*shell];
        let _: () = msg_send![&*accessory, setLayoutAttribute: 6isize]; // NSLayoutAttributeTrailing
        let _: () = msg_send![&*accessory, setFullScreenMinHeight: frame.size.height];
        let toolbar: Allocated<AnyObject> = msg_send_id![class!(NSToolbar), alloc];
        let identifier = NSString::from_str("FalconMacChrome02");
        let toolbar: Retained<AnyObject> = msg_send_id![toolbar, initWithIdentifier: &*identifier];
        let _: () = msg_send![&*toolbar, setAllowsUserCustomization: Bool::NO];
        let _: () = msg_send![&*toolbar, setAutosavesConfiguration: Bool::NO];

        self.accessory = Some(accessory);
        self.shell = Some(shell);
        self.toolbar = Some(toolbar);
        if compat {
            compatibility::configure(
                self.window.clone(),
                self.donor_view.clone(),
                frame.size.height,
            );
            compatibility::attach((&*self.window as *const AnyObject).cast_mut());
        }
        let shell = self.shell.as_ref().ok_or("shell missing")?;
        if let (Some(donor), Some(donor_view)) = (&self.donor, &self.donor_view) {
            // AppKit is allowed to clear donor.contentView on adoption. The opted-in
            // backend retains the original typed WinitView independently and returns
            // that same view to Slint/Metal and input APIs throughout reparenting.
            let _: () = msg_send![&**shell, addSubview: &**donor_view];
            let content: *mut AnyObject = msg_send![&**donor, contentView];
            let raw = self
                .bar
                .as_ref()
                .and_then(|bar| crate::mac_ns_view_of(bar.window()));
            if raw.is_none_or(|ptr| !std::ptr::eq(ptr.cast(), &**donor_view)) {
                return Err(
                    "the toolbar's drawing surface was lost while moving it into the title bar"
                        .into(),
                );
            }
            record(format!(
                "hosted backend retained render view; donor contentView cleared={}",
                content.is_null()
            ));
            let _: () = msg_send![&**donor_view, setAutoresizingMask: 18usize]; // width + height sizeable
            let inset = if super::full_toolbar() { 0.0 } else { 80.0 };
            let slint_frame = NSRect::new(
                NSPoint::new(inset, 0.0),
                NSSize::new(frame.size.width - inset, frame.size.height),
            );
            let _: () = msg_send![&**donor_view, setFrame: slint_frame];
            self.observe(donor_view, &["NSViewFrameDidChangeNotification"]);
        }
        let _: () = msg_send![&**shell, setPostsFrameChangedNotifications: Bool::YES];
        let _: () = msg_send![&**shell, setPostsBoundsChangedNotifications: Bool::YES];
        self.observe(
            shell,
            &[
                "NSViewFrameDidChangeNotification",
                "NSViewBoundsDidChangeNotification",
            ],
        );
        let _: () = msg_send![&*self.window, setToolbarStyle: 4isize]; // NSWindowToolbarStyleUnifiedCompact (macOS 11+)
        let _: () = msg_send![&*self.window, setToolbar: &**self.toolbar.as_ref().ok_or("toolbar missing")?];
        record(format!("accessory allocated compat={compat} donor={} (typed render view retained); no band suppression", self.donor.is_some()));
        Ok(true)
    }

    unsafe fn update(&mut self, immersive: bool) {
        if self.immersive == Some(immersive) {
            return;
        } // NO steady-state repair writes
        self.immersive = Some(immersive);
        if let Some(Bar::Full(bar)) = &self.bar {
            bar.global::<Tip>().set_text("".into());
            if let Some(app) = self.app.upgrade() {
                app.set_cache_meter_hover(false);
            }
        }
        compatibility::set_active(!immersive);
        if let Some(app) = self.app.upgrade() {
            app.set_mac_experiment_popup_open(false);
        }
        self.hot_until = Instant::now() + Duration::from_secs(2);
        record(format!(
            "intent immersive={immersive} (not an OS transition completion)"
        ));
        if let (Some(accessory), Some(toolbar)) = (&self.accessory, &self.toolbar) {
            if immersive && self.attached {
                // A trailing accessory's hidden flag is not reliable across OS revisions.
                // Remove our own controller; no hidden native overlay may intercept input.
                let _: () = msg_send![&**accessory, removeFromParentViewController];
                self.attached = false;
            } else if !immersive && !self.attached {
                let _: () =
                    msg_send![&*self.window, addTitlebarAccessoryViewController: &**accessory];
                self.attached = true;
            }
            let _: () = msg_send![&**toolbar, setVisible: Bool::new(!immersive)];
            self.writes += 1;
        }
        if immersive {
            if let Some(app) = self.app.upgrade() {
                focus_photo(&app);
            }
        }
    }

    /// AppKit's title-bar height for the main window right now (see `super::titlebar_band`).
    unsafe fn measure_band(&self) -> Option<f64> {
        let bounds: NSRect = msg_send![&*self.main_view, bounds];
        let content: NSRect =
            msg_send![&*self.main_view, convertRect: bounds toView: std::ptr::null::<AnyObject>()];
        let layout: NSRect = msg_send![&*self.window, contentLayoutRect];
        super::titlebar_band(
            content.origin.y + content.size.height,
            layout.origin.y + layout.size.height,
        )
    }

    /// The toolbar's live height, i.e. what the Slint surface is sized to. Everything measured from
    /// the toolbar's bottom edge (tooltips, the photo area's top) reads this, never the 44 pt
    /// Windows bar, or a gap opens under a 38 pt title bar.
    fn bar_height(&self, app: &MainWindow) -> f64 {
        match &self.bar {
            Some(Bar::Full(bar)) => bar.get_host_height() as f64,
            _ => self.bar_h.unwrap_or(app.get_titlebar_h() as f64),
        }
    }

    /// Whether AppKit shows less of the toolbar surface than its height right now.
    unsafe fn clip_state(&self) -> Option<bool> {
        if !self.attached || self.transition.is_some() {
            return None;
        }
        let view = self.donor_view.as_ref()?;
        let frame: NSRect = msg_send![&**view, frame];
        let visible: NSRect = msg_send![&**view, visibleRect];
        Some(super::toolbar_clipped(
            frame.size.height,
            visible.size.height,
        ))
    }

    unsafe fn layout(&mut self) {
        if !self.attached || self.shell.is_none() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        // AppKit owns the title bar's height (38 pt in the compact toolbar style). Read it; imposing
        // Falcon's 44 pt Windows bar clipped the toolbar's top 6 pt (full04-3, 2026-09-25).
        match self.measure_band() {
            Some(h) if self.bar_h != Some(h) => {
                record(format!("title-bar height {h} pt, read from AppKit"));
                self.bar_h = Some(h);
                if let Some(accessory) = &self.accessory {
                    let _: () = msg_send![&**accessory, setFullScreenMinHeight: h];
                }
            }
            None if self.bar_h.is_none() && !self.band_warned => {
                self.band_warned = true;
                record(format!(
                    "TITLE-BAR HEIGHT UNREADABLE; using {} pt until AppKit reports one",
                    app.get_titlebar_h()
                ));
            }
            _ => {}
        }
        let Some(shell) = &self.shell else {
            return;
        };
        let bounds: NSRect = msg_send![&*self.main_view, bounds];
        let size = NSSize::new(
            (bounds.size.width
                - if super::full_toolbar() {
                    0.0
                } else {
                    app.get_titlebar_leading_inset() as f64
                })
            .max(1.0),
            self.bar_h.unwrap_or(app.get_titlebar_h() as f64),
        );
        if let Some(Bar::Full(bar)) = &self.bar {
            // NSView resizing alone does not update a fixed-size Slint root. Its geometry must
            // follow this same authoritative AppKit width and height.
            if bar.get_host_width() != size.width as f32 {
                bar.set_host_width(size.width as f32);
            }
            if bar.get_host_height() != size.height as f32 {
                bar.set_host_height(size.height as f32);
            }
        }
        let frame: NSRect = msg_send![&**shell, frame];
        if frame.size != size {
            let _: () = msg_send![&**shell, setFrameSize: size];
            if let Some(view) = &self.donor_view {
                // Full candidate uses the entire host; only old probes keep the comparison cell.
                let inset = if super::full_toolbar() { 0.0 } else { 80.0 };
                let _: () = msg_send![&**view, setFrame: NSRect::new(NSPoint::new(inset, 0.), NSSize::new((size.width - inset).max(1.), size.height))];
            }
            self.writes += 1;
        }
    }

    unsafe fn sync_input_host(&mut self) {
        let Some(view) = &self.donor_view else {
            return;
        };
        let actual: *mut AnyObject = msg_send![&**view, window];
        if actual.is_null() {
            return;
        } // detached while immersive; don't invent a host
        if self
            .observed_host
            .as_ref()
            .is_none_or(|old| !std::ptr::eq(&**old, actual))
        {
            let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
            if let Some(old) = self.observed_host.take() {
                if !std::ptr::eq(&*old, &*self.window) {
                    let _: () = msg_send![center, removeObserver: &*self.observer name: std::ptr::null::<AnyObject>() object: &*old];
                }
            }
            if let Ok(host) = retained(actual) {
                if !std::ptr::eq(&*host, &*self.window) {
                    self.observe(&host, NOTIFICATIONS);
                }
                record(format!(
                    "accessory host changed to {actual:p}/{}",
                    host.class().name()
                ));
                self.observed_host = Some(host);
            }
        }
        // Refresh backend scale/focus using the view's actual NSWindow. No donor
        // movement, coordinate emulation or casting AppKit fullscreen hosts to winit.
        if let Some(bar) = &self.bar {
            bar.window()
                .with_winit_window(|w| w.sync_falcon_hosted_view());
        }
        let responder: *mut AnyObject = msg_send![actual, firstResponder];
        let focused: Bool = msg_send![actual, isKeyWindow];
        let focused = focused.as_bool() && std::ptr::eq(responder, &**view);
        if self.last_focus != Some(focused) {
            self.last_focus = Some(focused);
            if let Some(bar) = &self.bar {
                bar.window()
                    .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(focused));
                if focused && super::full_toolbar() {
                    bar.window().with_winit_window(|w| w.set_ime_allowed(false));
                }
            }
        }
        // The hosted backend reads this same NSWindow's scale, so comparing against winit would
        // always agree. Compare what Slint renders at instead. The ScaleFactorChanged queued by
        // the sync above lands on the next event-loop turn, so only a mismatch that survives two
        // consecutive samples is a failure; log it once per episode.
        let actual_scale: f64 = msg_send![actual, backingScaleFactor];
        let slint_scale = self
            .bar
            .as_ref()
            .map(|bar| bar.window().scale_factor() as f64);
        let mismatch = std::env::var_os("SLINT_SCALE_FACTOR").is_none()
            && slint_scale.is_some_and(|s| (s - actual_scale).abs() > 1e-3);
        self.scale_mismatch_samples = if mismatch {
            self.scale_mismatch_samples.saturating_add(1)
        } else {
            0
        };
        if self.scale_mismatch_samples == 2 {
            record(format!(
                "HOST SCALE MISMATCH actual={actual_scale} slint={slint_scale:?} over two samples; field gate failed"
            ));
        }
    }

    unsafe fn sample(&mut self, reason: &str) {
        self.sync_input_host();
        if super::full_toolbar() {
            if let Some(app) = self.app.upgrade() {
                // The photo area starts at the toolbar's real bottom edge, not 44 pt down.
                let bar_h = self.bar_height(&app);
                if let Some(p) = main_anchor(self, 0.0, bar_h) {
                    app.set_native_toolbar_inset((p.y as f32).clamp(0.0, bar_h as f32));
                }
            }
        }
        let clipped = self.clip_state();
        if clipped.is_some() && clipped != self.clipped {
            if clipped == Some(true) {
                if let Some(view) = &self.donor_view {
                    let frame: NSRect = msg_send![&**view, frame];
                    let visible: NSRect = msg_send![&**view, visibleRect];
                    record(format!(
                        "TOOLBAR CLIPPED: AppKit shows {} of the toolbar's {} pt; launch check failed",
                        visible.size.height, frame.size.height
                    ));
                }
            } else if self.clipped == Some(true) {
                record("toolbar fully visible again");
            }
            self.clipped = clipped;
        }
        if !crate::support::diagnostic_logging_enabled() {
            return;
        }
        let mask: usize = msg_send![&*self.window, styleMask];
        let first_responder: *mut AnyObject = msg_send![&*self.window, firstResponder];
        let key: Bool = msg_send![&*self.window, isKeyWindow];
        let screen: *mut AnyObject = msg_send![&*self.window, screen];
        let screen_frame: NSRect = if screen.is_null() {
            NSRect::default()
        } else {
            msg_send![screen, frame]
        };
        let screen_visible: NSRect = if screen.is_null() {
            NSRect::default()
        } else {
            msg_send![screen, visibleFrame]
        };
        let safe: NSRect = msg_send![&*self.window, contentLayoutRect];
        let mut s = format!("window={:p} fs={} attached={} key={} responder={first_responder:p} layout={safe:?} screen={screen:p}/{screen_frame:?} visible={screen_visible:?}",
            &*self.window, (mask & (1 << 14)) != 0, self.attached, key.as_bool());
        for (label, object) in [
            ("main", Some(&self.main_view)),
            ("shell", self.shell.as_ref()),
            ("slint", self.donor_view.as_ref()),
        ] {
            if let Some(object) = object {
                s.push_str(&describe_view(label, object));
            }
        }
        for i in 0..3usize {
            let button: *mut AnyObject = msg_send![&*self.window, standardWindowButton: i];
            if let Some(button) = button.as_ref() {
                s.push_str(&describe_view(&format!("light{i}"), button));
            }
        }
        let scale: f64 = msg_send![&*self.window, backingScaleFactor];
        s.push_str(&format!(" main_scale={scale}"));
        if let Some(donor) = &self.donor {
            let scale: f64 = msg_send![&**donor, backingScaleFactor];
            let visible: Bool = msg_send![&**donor, isVisible];
            let frame: NSRect = msg_send![&**donor, frame];
            s.push_str(&format!(
                " donor={:p} scale={scale} visible={} frame={frame:?}",
                &**donor,
                visible.as_bool()
            ));
        }
        if s != self.last_geometry {
            record(format!("geometry reason={reason} {s}"));
            self.last_geometry = s;
        }
    }
}

unsafe fn describe_view(label: &str, object: &AnyObject) -> String {
    let parent: *mut AnyObject = msg_send![object, superview];
    let window: *mut AnyObject = msg_send![object, window];
    let frame: NSRect = msg_send![object, frame];
    let visible: NSRect = msg_send![object, visibleRect];
    let hidden: Bool = msg_send![object, isHiddenOrHasHiddenAncestor];
    let alpha: f64 = msg_send![object, alphaValue];
    let bounds: NSRect = msg_send![object, bounds];
    let base: NSRect = msg_send![object, convertRect: bounds toView: std::ptr::null::<AnyObject>()];
    let screen: NSRect = if window.is_null() {
        NSRect::default()
    } else {
        msg_send![window, convertRectToScreen: base]
    };
    let scale: f64 = if window.is_null() {
        0.0
    } else {
        msg_send![window, backingScaleFactor]
    };
    format!(" {label}={object:p}/{} parent={parent:p} host={window:p} scale={scale} frame={frame:?} visible={visible:?} screen={screen:?} hidden={} alpha={alpha}",
        object.class().name(), hidden.as_bool())
}

unsafe fn focus_photo(app: &MainWindow) {
    if let Some(ptr) = crate::mac_ns_view_of(app.window()) {
        let view = ptr.cast::<AnyObject>();
        let window: *mut AnyObject = msg_send![view, window];
        if !window.is_null() {
            let _: () = msg_send![window, makeKeyWindow];
            let _: Bool = msg_send![window, makeFirstResponder: view];
            app.invoke_mac_experiment_refocus();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            compatibility::set_active(false);
            let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
            let _: () = msg_send![center, removeObserver: &*self.observer];
            if let Some(accessory) = &self.accessory {
                let _: () = msg_send![&**accessory, removeFromParentViewController];
            }
            if self.toolbar.is_some() {
                let current: *mut AnyObject = msg_send![&*self.window, toolbar];
                // Restore only the object still owned by this experiment.
                if self
                    .toolbar
                    .as_ref()
                    .is_some_and(|p| std::ptr::eq(&**p, current))
                {
                    let old = self
                        .old_toolbar
                        .as_deref()
                        .map_or(std::ptr::null(), |p| p as *const _);
                    let _: () = msg_send![&*self.window, setToolbar: old];
                    let _: () = msg_send![&*self.window, setToolbarStyle: self.old_style];
                }
            }
            if let (Some(donor), Some(view)) = (&self.donor, &self.donor_view) {
                // The backend retains its view during synchronous detach/restore callbacks.
                let _: () = msg_send![&**donor, setContentView: &**view];
            }
            compatibility::shutdown();
            if let Some(bar) = &self.bar {
                let _ = bar.hide();
            }
            record(format!(
                "restored native host; events={} writes={}",
                self.events, self.writes
            ));
        }
    }
}

pub(crate) fn apply(immersive: bool) {
    HOST.with(|h| {
        if let Ok(mut h) = h.try_borrow_mut() {
            if let Some(h) = h.as_mut().filter(|h| h.initialized) {
                unsafe {
                    h.update(immersive);
                }
            }
        }
    });
}

/// Called only after the failed Host has been dropped/restored. Switch routing
/// first, then install the former toolbar's native handling immediately.
fn restore_inline_toolbar_after_failure(app: &MainWindow) {
    super::mark_toolbar_failed();
    app.set_mac_experiment_host(false);
    if let Some(view) = crate::mac_ns_view_of(app.window()) {
        crate::support::apply_mac_window_chrome(
            view,
            app.get_immersive(),
            app.get_titlebar_h() as f64,
            app.get_titlebar_leading_inset() as f64,
        );
        crate::support::install_mac_fullscreen_observers(view);
    }
}

pub(crate) fn start(app: &MainWindow) {
    #[cfg(feature = "mac-chrome-experiment")]
    record("FALCON_MAC_CHROME_EXPERIMENT_04 starting");
    #[cfg(not(feature = "mac-chrome-experiment"))]
    record("FALCON_MAC_NATIVE_TOOLBAR_01 starting");
    app.set_mac_experiment_status("Starting the title-bar toolbar…".into());
    let log_path = (super::active() || super::ci_smoke())
        .then(crate::support::log_path)
        .flatten();
    app.set_mac_experiment_log_path(
        log_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Log path unavailable".into())
            .into(),
    );
    record(format!(
        "diagnostic profile={:?} log={log_path:?}",
        super::profile_dir()
    ));
    let aw = app.as_weak();
    app.on_mac_experiment_command(move |command| {
        if let Some(app) = aw.upgrade() {
            match command.as_str() {
                "logs" => {
                    if let Some(dir) = crate::support::log_path()
                        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
                    {
                        if let Err(e) = std::process::Command::new("/usr/bin/open").arg(dir).spawn()
                        {
                            app.set_mac_experiment_status(format!("Open logs failed: {e}").into());
                        }
                    }
                }
                "copy" => {
                    let report = format!(
                        "{}\n{}\nlog={}\nwelcome={} association={} immersive={}\n",
                        super::label(),
                        app.get_mac_experiment_status(),
                        app.get_mac_experiment_log_path(),
                        app.get_welcome_open(),
                        app.get_assoc_prompt_open(),
                        app.get_immersive()
                    );
                    let ok = crate::copy_text_to_clipboard(&report);
                    record(format!("copy diagnostics success={ok}"));
                }
                "focus" | "f" => {
                    unsafe {
                        focus_photo(&app);
                    }
                    record(format!(
                        "diagnostic command={command} welcome={} association={} immersive={}",
                        app.get_welcome_open(),
                        app.get_assoc_prompt_open(),
                        app.get_immersive()
                    ));
                    if command == "f" {
                        app.invoke_toggle_fullscreen();
                    }
                }
                _ => {}
            }
        }
    });
    app.on_mac_experiment_popup_action(|s| {
        record(format!("styled-popup {s}"));
        if s == "count-close" {
            queue("popup-click");
        }
    });
    let weak = app.as_weak();
    let started = Instant::now();
    let failed = std::rc::Rc::new(Cell::new(false));
    TIMER.with(|timer| {
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                if failed.get() {
                    return;
                }
                let Some(app) = weak.upgrade() else {
                    return;
                };
                HOST.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    if slot.is_none() {
                        if crate::mac_ns_view_of(app.window()).is_none()
                            && started.elapsed() < Duration::from_secs(3)
                        {
                            return;
                        }
                        match unsafe { Host::new(&app) } {
                            Ok(h) => *slot = Some(h),
                            Err(e) => {
                                failed.set(true);
                                restore_inline_toolbar_after_failure(&app);
                                app.set_mac_experiment_status(format!("TITLE-BAR TOOLBAR FAILED, normal toolbar in use: {e}").into());
                                record(format!(
                                    "TITLE-BAR TOOLBAR FAILED: {e}; normal toolbar and window state restored, no retry"
                                ));
                                TIMER.with(slint::Timer::stop);
                                finish_smoke(&app, false, (None, None, None));
                                return;
                            }
                        }
                    }
                    let Some(host) = slot.as_mut() else {
                        return;
                    };
                    if !host.initialized {
                        match unsafe { host.create_accessory(&app) } {
                            Ok(false) => return,
                            Ok(true) => { host.initialized = true; unsafe { host.finish_initialization(&app); } }
                            Err(e) => {
                                failed.set(true);
                                drop(slot.take());
                                restore_inline_toolbar_after_failure(&app);
                                app.set_mac_experiment_status(format!("TITLE-BAR TOOLBAR FAILED, normal toolbar in use: {e}").into());
                                record(format!("TITLE-BAR TOOLBAR FAILED: {e}; normal toolbar and window state restored"));
                                TIMER.with(slint::Timer::stop);
                                finish_smoke(&app, false, (None, None, None));
                                return;
                            }
                        }
                    }
                    let events = {
                        let mut q = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
                        std::mem::take(&mut *q)
                    };
                    // Native menu tracking and anchor conversion borrow Host only after this
                    // timer callback releases it. Invoke later, never while holding the slot.
                    for event in &events {
                        if event == "show-native-menu" { let _ = slint::invoke_from_event_loop(show_popup); }
                        if let Some(args) = event.strip_prefix("show-styled-menu ") {
                            let p: Vec<_> = args.split_whitespace().filter_map(|p| p.parse::<f64>().ok()).collect();
                            if let [x, y] = p.as_slice() { let (x, y) = (*x, *y); let _ = slint::invoke_from_event_loop(move || show_styled_popup(x, y)); }
                        }
                    }
            for event in &events {
                record(event);
                // Failure is a delegate method in AppKit, not a notification. Do not
                // invent a DidFail notification or replace winit's delegate to catch it.
                // A missing completion is reported explicitly as unobserved, not failed.
                if event.contains(&format!(" object={:p} ", &*host.window)) {
                    if event.starts_with("event=NSWindowWillEnterFullScreenNotification ") {
                        host.transition = Some((true, Instant::now()));
                    } else if event.starts_with("event=NSWindowWillExitFullScreenNotification ") {
                        host.transition = Some((false, Instant::now()));
                    } else if event.starts_with("event=NSWindowDidEnterFullScreenNotification ")
                        || event.starts_with("event=NSWindowDidExitFullScreenNotification ") {
                        host.transition = None;
                    }
                }
                        if event == "popup-click" {
                            if let Some(bar) = &host.bar {
                                bar.count_click();
                            }
                        }
                        if event == "native-click" {
                            host.native_clicks += 1;
                            if let Some(button) = &host.native_button {
                                unsafe {
                                    let title = NSString::from_str(&format!(
                                        "Native {}",
                                        host.native_clicks
                                    ));
                                    let _: () = msg_send![&**button, setTitle: &*title];
                                }
                            }
                        }
                    }
                    host.events += events.len() as u64;
            let now = Instant::now();
            if let Some((entering, since)) = host.transition {
                if now.duration_since(since) >= Duration::from_secs(5) {
                    record(format!("fullscreen completion NOT OBSERVED after 5s entering={entering}; inspect state/video (not a failure notification)"));
                    host.transition = None;
                }
            }
                    if !events.is_empty() {
                        host.hot_until = now + Duration::from_secs(2);
                    }
                    unsafe {
                        host.update(app.get_immersive());
                        // Layout on actual notification edges only, never as an idle repair loop.
                        if !events.is_empty() {
                            host.colour_dirty |= events.iter().any(|e| e.contains("BackingProperties") || e.contains("DidChangeScreen"));
                            host.layout();
                            if events.iter().any(|e| e.starts_with("event=NSWindowDidResize") || e.starts_with("event=NSWindowDidMove")) {
                                app.set_mac_experiment_popup_open(false);
                            }
                        }
                        if now < host.hot_until
                            || now.duration_since(host.last_sample) >= Duration::from_millis(250)
                        {
                            host.sample("observation");
                            host.last_sample = now;
                        }
                    }
                    if now.duration_since(host.last_report) >= Duration::from_secs(5) {
                let photo = app.get_photo().size();
                record(format!(
                    "heartbeat events={} writes={} photo_bound={}x{} empty={} welcome={} immersive={}",
                    host.events, host.writes, photo.width, photo.height,
                    app.get_empty_state(), app.get_welcome_open(), app.get_immersive()
                ));
                    host.last_report = now;
                    }
                    let hosted_frame = host.bar.is_none() || host.bar_frames.get() > host.attached_frame_baseline;
                    if host.initialized && hosted_frame {
                        let fs: usize = unsafe { msg_send![&*host.window, styleMask] };
                        app.set_mac_experiment_status(format!("Title-bar toolbar ready · Mac full screen: {} · Falcon immersive: {}", if fs & (1 << 14) != 0 { "yes" } else { "no" }, if app.get_immersive() { "yes" } else { "no" }).into());
                    }
                    if host.initialized && now.duration_since(started) >= Duration::from_secs(2)
                        && app.get_photo().size().width > 0 && hosted_frame {
                        // Drawn is not visible: the report carries whether AppKit cut the toolbar off.
                        let geometry = unsafe {
                            let native_mouse_down = host.donor_view.as_ref().map(|view| {
                                let moves: Bool = msg_send![&**view, mouseDownCanMoveWindow];
                                moves.as_bool()
                            });
                            (host.clip_state(), host.bar_h, native_mouse_down)
                        };
                        if super::full_toolbar() && std::env::var_os("FALCON_MAC_PROBE_SMOKE_OUT").is_some() {
                            // Each step waits for its queued press to have run, never for the
                            // next tick (see `super::smoke_round_trip`).
                            let delivered = SMOKE_PRESSES.with(Cell::get);
                            let waited_ms = host.smoke_started.map_or(0, |t| now.duration_since(t).as_millis());
                            match super::smoke_round_trip(host.smoke_phase, delivered, app.get_grid_open(), host.smoke_original_grid, waited_ms) {
                                super::SmokeStep::Wait => {}
                                super::SmokeStep::Press(next) => {
                                    if host.smoke_phase == 0 {
                                        host.smoke_original_grid = app.get_grid_open();
                                        host.smoke_started = Some(now);
                                    }
                                    host.smoke_phase = next;
                                    // Run outside the Host borrow, like a real pointer callback.
                                    let _ = slint::invoke_from_event_loop(smoke_toggle_grid);
                                }
                                super::SmokeStep::Finish(ok) => {
                                    record(format!(
                                        "smoke: toolbar round trip {} after {waited_ms} ms (presses queued {} delivered {delivered}; grid open {}, originally {})",
                                        if ok { "passed" } else { "FAILED" },
                                        host.smoke_phase, app.get_grid_open(), host.smoke_original_grid
                                    ));
                                    app.set_mac_smoke_toolbar_ok(ok);
                                    finish_smoke(&app, ok && !app.get_welcome_open() && !app.get_assoc_prompt_open(), geometry);
                                }
                            }
                        } else { finish_smoke(&app, !app.get_welcome_open() && !app.get_assoc_prompt_open(), geometry); }
                    }
                    TIMER.with(|timer| timer.set_interval(if now < host.hot_until {
                        Duration::from_millis(16)
                    } else {
                        Duration::from_millis(250)
                    }));
                });
            },
        )
    });
}

pub(crate) fn record_main_input(event: &i_slint_backend_winit::winit::event::WindowEvent) {
    use i_slint_backend_winit::winit::event::{ElementState, WindowEvent};
    use i_slint_backend_winit::winit::keyboard::PhysicalKey;
    match event {
        WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
            // Only diagnostic keys are named and the F shortcut identified; typed text never is.
            // The line is built by the portable, tested `super::main_key_line`.
            let key = match event.physical_key {
                PhysicalKey::Code(code) => Some(format!("{code:?}")),
                PhysicalKey::Unidentified(_) => None,
            };
            record(super::main_key_line(
                key.as_deref(),
                event.text.as_deref(),
                event.repeat,
                crate::support::ime_allowed(),
            ));
        }
        WindowEvent::Ime(_) => record("main IME event (contents omitted)"),
        _ => {}
    }
}

thread_local! {
    /// Launch-check Grid presses that have run, so each step can wait for its own.
    static SMOKE_PRESSES: Cell<u32> = const { Cell::new(0) };
}

fn smoke_toggle_grid() {
    let target = HOST.with(|slot| {
        let slot = slot.borrow();
        let host = slot.as_ref()?;
        match &host.bar {
            Some(Bar::Full(bar)) => Some((bar.as_weak(), host.app.clone())),
            _ => None,
        }
    });
    let press = SMOKE_PRESSES.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    match target.and_then(|(bar, app)| Some((bar.upgrade()?, app.upgrade()?))) {
        Some((bar, app)) => {
            let before = app.get_grid_open();
            bar.invoke_action("grid".into(), 0.0, bar.get_host_height());
            record(format!("smoke: toolbar Grid press {press} ran; grid open {before} -> {}", app.get_grid_open()));
        }
        None => record(format!("smoke: toolbar Grid press {press} found no full toolbar")),
    }
}

/// `geometry`: (whether AppKit shows less of the toolbar than its height, the measured title-bar
/// height, whether AppKit can also move the window for the toolbar's mouseDown).
/// A cut-off toolbar or duplicated native title-bar handling fails the launch check.
fn finish_smoke(app: &MainWindow, ready: bool, geometry: (Option<bool>, Option<f64>, Option<bool>)) {
    let (clipped, titlebar_height, native_mouse_down) = geometry;
    let ready = ready && clipped == Some(false) && native_mouse_down == Some(false);
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(path) = std::env::var_os("FALCON_MAC_PROBE_SMOKE_OUT").map(std::path::PathBuf::from)
    else {
        return;
    };
    if !path.is_absolute() {
        return;
    }
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // Explicit CI-only mode. The wrapper enforces timeout and checks this result + exit status.
    let report = serde_json::json!({
        "build": super::build_label(), "ready": ready, "status": app.get_mac_experiment_status().as_str(),
        "welcome": app.get_welcome_open(), "association": app.get_assoc_prompt_open(),
        "photo_width": app.get_photo().size().width,
        "full_toolbar": super::full_toolbar(), "toolbar_roundtrip": app.get_mac_smoke_toolbar_ok(),
        "toolbar_clipped": clipped, "titlebar_height": titlebar_height,
        "toolbar_mouse_down_can_move_window": native_mouse_down,
        "log": crate::support::log_path().map(|p| p.display().to_string())
    });
    if let Err(e) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, report.to_string().as_bytes()))
    {
        record(format!("smoke report write failed: {e}"));
    }
    let _ = slint::quit_event_loop();
}

pub(crate) fn shutdown() {
    OPEN_FILE.with(|slot| slot.borrow_mut().take());
    TIMER.with(slint::Timer::stop);
    HOST.with(|h| {
        h.borrow_mut().take();
    });
}
