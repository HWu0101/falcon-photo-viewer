//! macOS: the Apple-Events **"odoc"** (open-documents) delegate arm — v0.9.15 (Round B prerequisite
//! spike). It lets Finder **"Open With → Falcon"** and a double-click of an *associated* file reach
//! the app: macOS delivers those as an Apple Event / `application:openURLs:` message to the
//! **NSApplicationDelegate**, which winit owns. Before this arm the app received files ONLY via
//! `argv[1]` or a drag-drop onto the window (winit `DroppedFile`); Finder never uses argv for a
//! bundled `.app`, so associations were dead.
//!
//! ── ARCHITECTURE (winit 0.30.13 / Slint 1.17, read from the vendored sources) ─────────────────────
//! * winit installs its OWN delegate — a `declare_class!`-built NSObject subclass named
//!   `WinitApplicationDelegate` — via `app.setDelegate(..)` **once**, inside `EventLoop::new()`
//!   (`winit .../macos/event_loop.rs:240`). It NEVER re-asserts it (that is the only `setDelegate`
//!   call on the application in the whole backend). Slint drives that same delegate through winit's
//!   `ApplicationHandler`/`run_app` path, and `BackendSelector::select()` is what builds the event
//!   loop — so the delegate exists from boot, well before `app.run()`.
//! * That delegate implements only `applicationDidFinishLaunching:` / `applicationWillTerminate:`
//!   (`macos/app_state.rs:63`) — **never** `application:openURLs:`. So winit surfaces no open-files
//!   event and there is nothing to forward-to.
//! * winit's `send_event` swizzle calls `ApplicationDelegate::get(mtm)` on effectively every event,
//!   and `get()` does `delegate.is_kind_of::<WinitApplicationDelegate>()` and **panics** otherwise
//!   (`app.rs:45`, `app_state.rs:174-184`). So we must NOT replace the delegate with a proxy, and we
//!   must not disturb the delegate object's class identity.
//!
//! ── STRATEGY: extend the EXISTING class, do not replace the object ────────────────────────────────
//! We `class_addMethod` `application:openURLs:` onto the LIVE `WinitApplicationDelegate` **class**:
//! * winit never implements that selector, so the add always succeeds (nothing is overridden).
//! * the delegate OBJECT is untouched — same `isa`, same ivars — so winit's hot path
//!   (`is_kind_of` + `ivars()` on every event) is byte-for-byte unperturbed. This is the minimal
//!   perturbation; a subclass + `object_setClass` would work too (is_kind_of walks the chain) but
//!   needlessly changes the object's class.
//! * AppKit caches a delegate's `respondsToSelector:` answers at `setDelegate:` time, so after the
//!   add we **re-assign the same delegate** to force AppKit to re-scan — otherwise a launch-time
//!   "does not respond to openURLs" verdict would stick and the method would never be called.
//!
//! ── COLD-LAUNCH ORDERING (why no launch file is lost) ─────────────────────────────────────────────
//! When the app is launched BY opening a file, AppKit delivers `application:openURLs:` only after the
//! run loop starts pumping (it arrives around `applicationDidFinishLaunching:`, i.e. inside
//! `app.run()`). We inject on the main thread synchronously **before** `app.run()` (see
//! `install_odoc_handler`), so the method is provably in place before any odoc delivery — there is no
//! race. The handler hands the path to the UI thread's existing folder-open channel (drained by the
//! 16 ms tick → `begin_reload`), so a cold launch briefly shows the empty state and then opens the
//! delivered folder within a tick.
//!
//! ── DEFENSIVE POSTURE ─────────────────────────────────────────────────────────────────────────────
//! Every objc step is nil-/failure-checked; any miss logs `odoc: injection failed (<step>)` and the
//! app continues (associations simply won't open — never a crash). The callback body is wrapped in
//! `catch_unwind` because a Rust panic must not unwind across the ObjC frame (UB).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject, Sel};
use objc2::{class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationDelegate};
use objc2_foundation::{MainThreadMarker, NSArray, NSURL};

use crate::support::{self, log_event, QuitRequest, SaveSequence, SystemQuitState, TerminateHooks, TerminateReply};

/// File paths delivered by `application:openURLs:` — pushed by the objc callback (on the AppKit main
/// thread) and drained on the UI thread by the §tick, which forwards each into `begin_reload` (the
/// SAME channel argv / drag-drop use: a file → its parent folder, landing on the file via
/// `select_start`). A plain `Mutex<Vec<PathBuf>>` rather than the G7 picker channel, whose `PickKind`
/// is a `main`-local enum we cannot name here — `Mutex::new(Vec::new())` is const so this can be a
/// bare `static` (always `Sync`), and the lock is uncontended (both ends run on the main thread; the
/// Mutex only satisfies the `static` requirement).
pub(crate) static ODOC_PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The `application:openURLs:` implementation grafted onto winit's live delegate class. Runs on the
/// AppKit main thread. `catch_unwind` guards the ObjC boundary; every read is nil-checked.
///
/// Signature matches the ObjC method exactly — `void application:(NSApplication*)app
/// openURLs:(NSArray<NSURL*>*)urls` — so the raw `class_addMethod` type encoding is `"v@:@@"`.
extern "C" fn open_urls(
    _this: &AnyObject,
    _cmd: Sel,
    _app: &NSApplication,
    urls: &NSArray<NSURL>,
) {
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let count = urls.count();
        let mut paths: Vec<PathBuf> = Vec::with_capacity(count);
        for i in 0..count {
            // `i < count`, so `objectAtIndex` is in bounds; `path()` is `None` for a non-file URL.
            let url = unsafe { urls.objectAtIndex(i) };
            if let Some(ns) = unsafe { url.path() } {
                paths.push(PathBuf::from(ns.to_string()));
            }
        }
        let first = paths
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<no file url>".to_string());
        // The one greppable line per event.
        log_event(&format!("odoc: {count} url(s) — {first}"));
        if !paths.is_empty() {
            // Hand off to the UI thread (drained by the tick → begin_reload).
            let mut q = ODOC_PENDING.lock().unwrap_or_else(|e| e.into_inner());
            q.extend(paths);
        }
    }));
    if run.is_err() {
        log_event("odoc: handler panicked (recovered)");
    }
}

/// Graft `application:openURLs:` onto winit's live application delegate. MUST be called on the main
/// thread, AFTER the Slint/winit backend exists (delegate created + set at `select()` during boot)
/// and BEFORE `app.run()` (so the method is in place before AppKit pumps any odoc event — the
/// cold-launch guarantee). Idempotent and fully defensive; on any miss it logs and returns.
pub(crate) fn install_odoc_handler() {
    let Some(mtm) = MainThreadMarker::new() else {
        log_event("odoc: injection failed (not main thread)");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // winit set this at `EventLoop::new()`; `None` would mean the backend has not been built yet
    // (it has, by boot's `select()`), so this arm is the belt-and-suspenders edge.
    let Some(delegate) = (unsafe { app.delegate() }) else {
        log_event("odoc: injection failed (no delegate)");
        return;
    };

    // The delegate's class == `WinitApplicationDelegate`. Both `ProtocolObject` and `AnyObject` are
    // repr-transparent wrappers over the same objc object pointer, so this reference cast is sound.
    let del_ref: &ProtocolObject<dyn NSApplicationDelegate> = &delegate;
    let del_obj: &AnyObject =
        unsafe { &*(del_ref as *const ProtocolObject<dyn NSApplicationDelegate> as *const AnyObject) };
    let cls: &AnyClass = del_obj.class();

    let sel = sel!(application:openURLs:);
    // Idempotent: if the class already carries the method (it won't, first call), skip the add.
    // `instance_method` walks the superclass chain; neither `WinitApplicationDelegate` nor its
    // supers (NSObject) implement openURLs, so first call this is `None`.
    if cls.instance_method(sel).is_some() {
        log_event("odoc: openURLs already present on the delegate class (skipping add)");
    } else {
        // Encoding: void return; self(@), _cmd(:), NSApplication*(@), NSArray*(@) → "v@:@@".
        let types = b"v@:@@\0";
        // Both are thin `extern "C"` fn pointers of identical size; ObjC will call it with the ABI
        // the encoding above declares (this is exactly how objc2's own `ClassBuilder` stores an IMP).
        let imp: objc2::ffi::IMP = Some(unsafe {
            std::mem::transmute::<
                extern "C" fn(&AnyObject, Sel, &NSApplication, &NSArray<NSURL>),
                unsafe extern "C" fn(),
            >(open_urls)
        });
        let added = unsafe {
            objc2::ffi::class_addMethod(
                cls as *const AnyClass as *mut objc2::ffi::objc_class,
                sel.as_ptr(),
                imp,
                types.as_ptr() as *const _,
            )
        };
        if !Bool::from_raw(added).as_bool() {
            log_event("odoc: injection failed (class_addMethod)");
            return;
        }
    }

    // Re-assign the (unchanged) delegate so AppKit refreshes its cached `respondsToSelector:` flags
    // and will actually dispatch `application:openURLs:` to our freshly-added method.
    app.setDelegate(Some(del_ref));
    log_event("odoc: openURLs handler installed on the winit app delegate");
    // Keep `delegate` alive to the end (it is only weakly held by NSApplication); winit's own strong
    // ref in `EventLoop` outlives us, but this makes the borrow explicit.
    let _keep: Retained<ProtocolObject<dyn NSApplicationDelegate>> = delegate;
}

// ── v0.9.61 (A1.5): `applicationShouldTerminate:` — the graft-independent quit door ──────────────
//
// WHY IT EXISTS. Everything that made ⌘Q graceful in v0.9.59 rides on the menu graft: muda's Quit
// item is retargeted to our controller, so the key equivalent enqueues a tag the tick drains. That
// retarget is only in place once the graft has taken, and it is only in place on the CURRENT bar —
// so between muda replacing the menu bar and our next tick re-grafting it, ⌘Q is muda's raw
// `terminate:` again, which kills the process inside AppKit and skips the whole flush tail. The
// same is true of every OTHER route into `terminate:`: the Dock's Quit item, `killall -TERM`-style
// polite quits, Apple-events quit, a restart/shut-down request. They all arrive HERE first.
//
// WHAT IT DOES FOR AN ORDINARY QUIT: nothing but ask for the loop to end. It sets the latch the
// shutdown tail reads, raises the workers' cancel flags so an in-flight batch stops at its next
// file boundary, asks Slint to leave the loop, and answers `NSTerminateCancel` — "do not tear the
// process down; I am leaving under my own power". `app.run()` then returns into `main()`, which
// runs the shared save sequence; the tail's `Once` is what stops this door and the ⌘Q door from
// both running it. No explicit hide: Slint's generated `run()` hides the window before it returns.
//
// 1.0.16 — A SYSTEM LOG-OUT, RESTART OR SHUT-DOWN (owner-approved 22 Oct; round
// `2026-10-22_mac_logout`). Answering those with Cancel made macOS report that Falcon interrupted
// log out, even though it exited a moment later. The quit Apple event's `kAEQuitReason` now
// identifies them (`support::classify_quit_request`), and for those the hook runs the SAME save
// sequence `main()` registered, right here, then answers `NSTerminateNow`. After Now, AppKit ends
// the process from inside `terminate:` (winit's `applicationWillTerminate:` closes the windows on
// the way), so nothing after `app.run()` runs — which is why the save cannot stay there. The save
// is synchronous: no `NSTerminateLater`, no nested loop. Its waits are the tail's existing ones
// (2 s + 10 s + 10 s); a hung disk can still hold log-out up, as it held up exit before. The
// decisions (once-only, re-entry, a missing or panicking save) are `support::answer_terminate_request`.

/// Raised by the hook; read by the shutdown tail via [`terminate_was_requested`]. A plain static
/// because the callback is a bare `extern "C" fn` with no place to keep a handle.
static TERMINATE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The worker cancel latches the hook must raise (the copy/move file-boundary flag and the web
/// export's own cancel, which lives inside an `ExportProgress` rather than in a bare `Arc`). Held as
/// closures so the hook does not need to know either shape. `Mutex::new(Vec::new())` is const, so
/// this can be a bare `static`; registration and the hook both run on the main thread.
#[allow(clippy::type_complexity)]
static QUIT_CANCELS: Mutex<Vec<Box<dyn Fn() + Send>>> = Mutex::new(Vec::new());

/// Register a latch the terminate hook should raise. Called at boot, before `app.run()`.
pub(crate) fn register_quit_cancel(raise: Box<dyn Fn() + Send>) {
    QUIT_CANCELS.lock().unwrap_or_else(|e| e.into_inner()).push(raise);
}

/// Did `applicationShouldTerminate:` ask us to quit? The shutdown tail ORs this into the ⌘Q latch,
/// so a terminate-driven exit honours the same in-flight file-operation boundary.
pub(crate) fn terminate_was_requested() -> bool {
    TERMINATE_REQUESTED.load(Ordering::SeqCst)
}

thread_local! {
    /// 1.0.16: the save sequence `main()` registered before the loop. Main-thread storage, because
    /// it holds Slint and `Rc` handles (not `Send`), and owned, so no borrow of `main()`'s locals is
    /// stretched. Taken out (the borrow ends) before it runs; whichever exit takes it runs it.
    static SAVE_SEQUENCE: RefCell<Option<SaveSequence>> = const { RefCell::new(None) };
    /// 1.0.16: where the system-quit save stands ([`SystemQuitState`]).
    static SYSTEM_QUIT: Cell<SystemQuitState> = const { Cell::new(SystemQuitState::Idle) };
}

/// Hand the hook the save sequence. Called at boot, before [`install_terminate_handler`].
pub(crate) fn register_save_sequence(save: SaveSequence) {
    SAVE_SEQUENCE.with(|s| *s.borrow_mut() = Some(save));
}

/// Take the save sequence back after the loop. `Err(state)` = a system quit already took it; since
/// the process is still here that is `Failed` (logged at the time; not retried), and `main()` then
/// exits without reporting a clean shutdown (review R2).
pub(crate) fn take_save_sequence() -> Result<SaveSequence, SystemQuitState> {
    SAVE_SEQUENCE.with(|s| s.borrow_mut().take()).ok_or_else(|| SYSTEM_QUIT.with(Cell::get))
}

/// The after-loop route finished the save. A system quit arriving later (a modal dialog after the
/// loop, such as the device-lost prompt, still receives Apple events) may then say yes at once.
pub(crate) fn note_saved_after_loop() {
    SYSTEM_QUIT.with(|s| s.set(SystemQuitState::Complete));
}

/// NSApplicationTerminateReply::Cancel — "I will quit myself; do not kill me."
const NS_TERMINATE_CANCEL: usize = 0;
/// NSApplicationTerminateReply::Now — "saved; you may end the process" (1.0.16, system quits only).
const NS_TERMINATE_NOW: usize = 1;

/// What the current Apple event says, read inside `applicationShouldTerminate:` (the manager only
/// holds it while AppKit's quit handler runs). Raw codes, for [`support::classify_quit_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuitEvent {
    /// (event class, event id); `None` when there is no current Apple event (⌘Q, `terminate:`).
    event: Option<(u32, u32)>,
    /// The `kAEQuitReason` code; `None` when the attribute is absent or reads as zero.
    reason: Option<u32>,
    /// The reason's descriptor type, for the log only (Apple does not document it).
    reason_type: Option<u32>,
}

fn current_quit_event() -> QuitEvent {
    let manager: *mut AnyObject = unsafe { msg_send![class!(NSAppleEventManager), sharedAppleEventManager] };
    if manager.is_null() {
        return QuitEvent { event: None, reason: None, reason_type: None };
    }
    let event: *mut AnyObject = unsafe { msg_send![manager, currentAppleEvent] };
    unsafe { read_quit_event(event) }
}

/// Read one `NSAppleEventDescriptor`. Typed messages: `objc2-foundation` 0.2 generates neither the
/// event class/id readers nor `attributeDescriptorForKeyword:`, and every return is checked.
///
/// SAFETY: `event` is null or a live `NSAppleEventDescriptor`.
unsafe fn read_quit_event(event: *mut AnyObject) -> QuitEvent {
    if event.is_null() {
        return QuitEvent { event: None, reason: None, reason_type: None };
    }
    let class: u32 = unsafe { msg_send![event, eventClass] };
    let id: u32 = unsafe { msg_send![event, eventID] };
    let why: *mut AnyObject =
        unsafe { msg_send![event, attributeDescriptorForKeyword: support::AE_QUIT_REASON] };
    if why.is_null() {
        return QuitEvent { event: Some((class, id)), reason: None, reason_type: None };
    }
    // Each reader coerces and answers 0 when it cannot, so the first nonzero reading wins:
    // enumerated, then type code, then the 32-bit integer Wine's Mac driver reads.
    let kind: u32 = unsafe { msg_send![why, descriptorType] };
    let as_enum: u32 = unsafe { msg_send![why, enumCodeValue] };
    let as_type: u32 = unsafe { msg_send![why, typeCodeValue] };
    let as_int: i32 = unsafe { msg_send![why, int32Value] };
    let reason = [as_enum, as_type, as_int as u32].into_iter().find(|c| *c != 0);
    QuitEvent { event: Some((class, id)), reason, reason_type: Some(kind) }
}

fn quit_event_text(q: &QuitEvent) -> String {
    let code = |c: Option<u32>| c.map_or_else(|| "none".to_string(), support::four_char_code_text);
    let event = q.event.map_or_else(
        || "none".to_string(),
        |(class, id)| format!("{}/{}", support::four_char_code_text(class), support::four_char_code_text(id)),
    );
    format!("event={event} reason={} reason_type={}", code(q.reason), code(q.reason_type))
}

/// The grafted `applicationShouldTerminate:`. Runs on the AppKit main thread; `catch_unwind` guards
/// the ObjC boundary (a Rust panic must not unwind an ObjC frame — UB). Returns NSUInteger.
extern "C" fn should_terminate(_this: &AnyObject, _cmd: Sel, _app: &NSApplication) -> usize {
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let event = current_quit_event();
        let request = support::classify_quit_request(event.event, event.reason);
        // The first request of either kind raises the cancels; `swap` makes it the only one.
        let first_arrival = !TERMINATE_REQUESTED.swap(true, Ordering::SeqCst);
        let mut raise_cancels = || {
            for raise in QUIT_CANCELS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                raise();
            }
        };
        let mut leave_loop = || {
            let _ = slint::quit_event_loop();
        };
        // The storage borrow ends inside this closure, before the sequence runs.
        let mut take_save = || SAVE_SEQUENCE.with(|s| s.borrow_mut().take());
        let mut hooks = TerminateHooks {
            raise_cancels: &mut raise_cancels,
            leave_loop: &mut leave_loop,
            take_save: &mut take_save,
        };
        match request {
            QuitRequest::Ordinary => {
                if first_arrival {
                    log_event(&format!(
                        "terminate: applicationShouldTerminate ({}) — ordinary quit: leaving the event loop for the graceful flush tail (answering NSTerminateCancel)",
                        quit_event_text(&event)
                    ));
                }
                SYSTEM_QUIT.with(|state| support::answer_terminate_request(request, first_arrival, state, &mut hooks))
            }
            QuitRequest::System { reason, name } => {
                log_event(&format!(
                    "terminate: system reason={} ({name}) — {}; saving before the reply",
                    support::four_char_code_text(reason),
                    quit_event_text(&event)
                ));
                let started = std::time::Instant::now();
                let reply =
                    SYSTEM_QUIT.with(|state| support::answer_terminate_request(request, first_arrival, state, &mut hooks));
                log_event(&format!(
                    "terminate: reply={} elapsed_ms={}",
                    if reply == TerminateReply::Now { "now" } else { "cancel" },
                    started.elapsed().as_millis()
                ));
                reply
            }
        }
    }));
    match reply {
        Ok(TerminateReply::Now) => NS_TERMINATE_NOW,
        Ok(TerminateReply::Cancel) => NS_TERMINATE_CANCEL,
        Err(_) => {
            // Outside the save itself (that has its own guard): fall back to the pre-1.0.16 route,
            // so the save sequence — still registered — runs after the loop.
            log_event("terminate: the terminate hook panicked (recovered) — answering NSTerminateCancel and leaving the loop");
            let _ = slint::quit_event_loop();
            NS_TERMINATE_CANCEL
        }
    }
}

/// Graft `applicationShouldTerminate:` onto winit's live application delegate — the same
/// `class_addMethod` technique [`install_odoc_handler`] uses, for the same reason (winit's hot path
/// does `is_kind_of` + `ivars()` on the delegate OBJECT, so the object must not be replaced).
///
/// VERIFIED against the vendored winit 0.30.13 sources: `WinitApplicationDelegate` declares
/// `applicationDidFinishLaunching:` and `applicationWillTerminate:` and NOTHING else
/// (`platform_impl/macos/app_state.rs`), so this selector is a pure ADD — nothing is overridden and
/// no winit behaviour is displaced. (`applicationWillTerminate:` IS implemented, which is why that
/// one is off the table: taking it would need swizzling.)
///
/// MUST be called on the main thread, after the backend exists and before `app.run()`.
pub(crate) fn install_terminate_handler() {
    let Some(mtm) = MainThreadMarker::new() else {
        log_event("terminate: injection failed (not main thread)");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = (unsafe { app.delegate() }) else {
        log_event("terminate: injection failed (no delegate)");
        return;
    };
    let del_ref: &ProtocolObject<dyn NSApplicationDelegate> = &delegate;
    let del_obj: &AnyObject =
        unsafe { &*(del_ref as *const ProtocolObject<dyn NSApplicationDelegate> as *const AnyObject) };
    let cls: &AnyClass = del_obj.class();

    let sel = sel!(applicationShouldTerminate:);
    if cls.instance_method(sel).is_some() {
        log_event("terminate: applicationShouldTerminate already present on the delegate class (skipping add)");
    } else {
        // Encoding: NSUInteger return (LP64 `unsigned long` → "L"); self(@), _cmd(:),
        // NSApplication*(@). The encoding is metadata for the runtime's introspection — the actual
        // call ABI comes from the fn pointer, and `usize` IS NSUInteger on every target this ships to.
        let types = b"L@:@\0";
        let imp: objc2::ffi::IMP = Some(unsafe {
            std::mem::transmute::<
                extern "C" fn(&AnyObject, Sel, &NSApplication) -> usize,
                unsafe extern "C" fn(),
            >(should_terminate)
        });
        let added = unsafe {
            objc2::ffi::class_addMethod(
                cls as *const AnyClass as *mut objc2::ffi::objc_class,
                sel.as_ptr(),
                imp,
                types.as_ptr() as *const _,
            )
        };
        if !Bool::from_raw(added).as_bool() {
            log_event("terminate: injection failed (class_addMethod) — a raw terminate: would bypass the flush tail");
            return;
        }
    }
    // Same reason as the odoc graft: AppKit caches `respondsToSelector:` at setDelegate: time.
    app.setDelegate(Some(del_ref));
    log_event("terminate: applicationShouldTerminate handler installed on the winit app delegate");
    let _keep: Retained<ProtocolObject<dyn NSApplicationDelegate>> = delegate;
}

// 1.0.16: native rows. They run in the Mac Actions test step (`cargo test` on the runner), which is
// the only place the real Foundation selectors and the installed SDK headers exist.
#[cfg(test)]
mod tests {
    use super::*;

    fn code(c: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*c)
    }

    /// A real `quit` (or other) Apple event, optionally with a `kAEQuitReason` attribute.
    unsafe fn apple_event(id: u32, why: Option<*mut AnyObject>) -> *mut AnyObject {
        let event: *mut AnyObject = unsafe {
            msg_send![class!(NSAppleEventDescriptor), appleEventWithEventClass: support::AE_CORE_EVENT_CLASS eventID: id targetDescriptor: std::ptr::null::<AnyObject>() returnID: -1i16 transactionID: 0i32]
        };
        assert!(!event.is_null(), "NSAppleEventDescriptor built a quit event");
        if let Some(why) = why {
            let _: () = unsafe { msg_send![event, setAttributeDescriptor: why forKeyword: support::AE_QUIT_REASON] };
        }
        event
    }

    /// FALSIFIERS: misspell a selector or give a reader the wrong return type → this row panics in
    /// objc2's checked send; read only `enumCodeValue` → the type-code / integer rows read `None`.
    #[test]
    fn a_real_quit_event_reads_back_its_class_id_and_reason() {
        objc2::rc::autoreleasepool(|_| unsafe {
            let quit = support::AE_QUIT_APPLICATION;
            let enumerated: *mut AnyObject = msg_send![class!(NSAppleEventDescriptor), descriptorWithEnumCode: code(b"rlgo")];
            let type_code: *mut AnyObject = msg_send![class!(NSAppleEventDescriptor), descriptorWithTypeCode: code(b"rest")];
            let integer: *mut AnyObject = msg_send![class!(NSAppleEventDescriptor), descriptorWithInt32: code(b"shut") as i32];
            for (why, expect, name) in [(enumerated, b"rlgo", "log out"), (type_code, b"rest", "restart"), (integer, b"shut", "shut down")] {
                let read = read_quit_event(apple_event(quit, Some(why)));
                assert_eq!(read.event, Some((support::AE_CORE_EVENT_CLASS, quit)), "class and id read back");
                assert_eq!(read.reason, Some(code(expect)), "{name}: reason read back ({})", quit_event_text(&read));
                assert_eq!(
                    support::classify_quit_request(read.event, read.reason),
                    QuitRequest::System { reason: code(expect), name }
                );
            }
            let dock = read_quit_event(apple_event(quit, None));
            assert_eq!((dock.reason, dock.reason_type), (None, None), "a quit without a reason");
            assert_eq!(support::classify_quit_request(dock.event, dock.reason), QuitRequest::Ordinary);
            let odoc = read_quit_event(apple_event(code(b"odoc"), Some(enumerated)));
            assert_eq!(support::classify_quit_request(odoc.event, odoc.reason), QuitRequest::Ordinary);
            assert_eq!(read_quit_event(std::ptr::null_mut()).event, None, "no current event (⌘Q)");
        });
    }

    /// The four-character codes in `support` are the installed SDK's, not a remembered copy.
    /// FALSIFIER: change any code in `support::SYSTEM_QUIT_REASONS` → its name's row reddens.
    #[test]
    fn the_quit_codes_match_the_installed_sdk_headers() {
        let sdk = std::process::Command::new("xcrun").arg("--show-sdk-path").output().expect("xcrun runs");
        let sdk = String::from_utf8(sdk.stdout).expect("utf-8 SDK path");
        let core = std::path::Path::new(sdk.trim()).join("System/Library/Frameworks/CoreServices.framework");
        let headers = [
            core.join("Frameworks/AE.framework/Headers"),
            core.join("Versions/A/Frameworks/AE.framework/Versions/A/Headers"),
        ]
        .into_iter()
        .find(|p| p.is_dir())
        .unwrap_or_else(|| panic!("no AE.framework headers under {}", core.display()));
        let mut text = String::new();
        for entry in std::fs::read_dir(&headers).unwrap_or_else(|e| panic!("{}: {e}", headers.display())) {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|x| x == "h") {
                text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
        let fourcc = |c: u32| String::from_utf8_lossy(&c.to_be_bytes()).into_owned();
        let declares = |name: &str, c: u32| {
            text.lines().any(|line| {
                let line = line.trim_start();
                line.strip_prefix(name)
                    .is_some_and(|rest| rest.trim_start().starts_with('=') && rest.contains(&format!("'{}'", fourcc(c))))
            })
        };
        let names = [
            ("kCoreEventClass", support::AE_CORE_EVENT_CLASS),
            ("kAEQuitApplication", support::AE_QUIT_APPLICATION),
            ("kAEQuitReason", support::AE_QUIT_REASON),
            ("kAEQuitAll", support::SYSTEM_QUIT_REASONS[0].0),
            ("kAEShutDown", support::SYSTEM_QUIT_REASONS[1].0),
            ("kAERestart", support::SYSTEM_QUIT_REASONS[2].0),
            ("kAEReallyLogOut", support::SYSTEM_QUIT_REASONS[3].0),
            ("kAELogOut", support::SYSTEM_QUIT_REASONS[4].0),
            ("kAEShowRestartDialog", support::SYSTEM_QUIT_REASONS[5].0),
            ("kAEShowShutdownDialog", support::SYSTEM_QUIT_REASONS[6].0),
        ];
        for (name, c) in names {
            assert!(declares(name, c), "{name} = '{}' in {}", fourcc(c), headers.display());
        }
    }
}
