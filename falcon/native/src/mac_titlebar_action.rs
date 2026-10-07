//! Match macOS's title-bar preference using AppKit actions on the real host window.
use objc2::{class, msg_send, msg_send_id, sel};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2_foundation::NSString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action { Zoom, Minimize, Fill, None }

fn parse_action(value: Option<&str>) -> Action {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("maximize" | "zoom") => Action::Zoom,
        Some("minimize") => Action::Minimize,
        Some("fill") => Action::Fill,
        _ => Action::None,
    }
}
pub(crate) fn requested_action() -> Action {
    unsafe {
        let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = NSString::from_str("AppleActionOnDoubleClick");
        let value: Option<Retained<NSString>> = msg_send_id![defaults, stringForKey: &*key];
        parse_action(value.as_ref().map(|v| v.to_string()).as_deref())
    }
}
/// Called on the UI thread after clearing Falcon's re-maximise intent. The
/// native host, never the hidden toolbar donor, receives the same actions as
/// the Window menu. Fill's selector is availability-checked (macOS 15+).
pub(crate) unsafe fn perform(window: &AnyObject, action: Action) {
    match action {
        Action::Zoom => { let _: () = msg_send![window, performZoom: std::ptr::null::<AnyObject>()]; }
        Action::Minimize => { let _: () = msg_send![window, performMiniaturize: std::ptr::null::<AnyObject>()]; }
        Action::Fill => {
            // AppKit's Fill action, also used by Chromium's native custom-titlebar handler.
            let supports_fill: Bool = msg_send![window, respondsToSelector: sel!(_zoomFill:)];
            if supports_fill.as_bool() {
                let _: () = msg_send![window, _zoomFill: std::ptr::null::<AnyObject>()];
            } else {
                let _: () = msg_send![window, performZoom: std::ptr::null::<AnyObject>()];
            }
        }
        Action::None => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_preference_variants_and_default() {
        assert_eq!(parse_action(None), Action::Zoom);
        for value in ["Maximize", "maximize", "Zoom", "zoom"] {
            assert_eq!(parse_action(Some(value)), Action::Zoom);
        }
        for value in ["Minimize", "minimize"] { assert_eq!(parse_action(Some(value)), Action::Minimize); }
        for value in ["Fill", "fill"] { assert_eq!(parse_action(Some(value)), Action::Fill); }
        for value in ["None", "none", "", "future-action"] { assert_eq!(parse_action(Some(value)), Action::None); }
    }
    #[test]
    fn dispatches_native_actions_and_availability_checked_fill_without_toggling_none() {
        use objc2::{declare::ClassBuilder, runtime::Sel, ClassType};
        use objc2_foundation::NSObject;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLED: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn zoom(_: *mut AnyObject, _: Sel, _: *mut AnyObject) { CALLED.fetch_add(1, Ordering::Relaxed); }
        extern "C" fn mini(_: *mut AnyObject, _: Sel, _: *mut AnyObject) { CALLED.fetch_add(10, Ordering::Relaxed); }
        extern "C" fn fill(_: *mut AnyObject, _: Sel, _: *mut AnyObject) { CALLED.fetch_add(100, Ordering::Relaxed); }
        for has_fill in [false, true] {
            let name = if has_fill { "FalconTitlebarActionTestFill" } else { "FalconTitlebarActionTestLegacy" };
            let mut builder = ClassBuilder::new(name, NSObject::class()).unwrap();
            unsafe {
                builder.add_method(sel!(performZoom:), zoom as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject));
                builder.add_method(sel!(performMiniaturize:), mini as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject));
                if has_fill { builder.add_method(sel!(_zoomFill:), fill as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject)); }
                let object: Retained<AnyObject> = msg_send_id![builder.register(), new];
                for (action, expected) in [(Action::None, 0), (Action::Zoom, 1), (Action::Minimize, 10),
                    (Action::Fill, if has_fill { 100 } else { 1 })] {
                    CALLED.store(0, Ordering::Relaxed);
                    perform(&object, action);
                    assert_eq!(CALLED.load(Ordering::Relaxed), expected, "{action:?}, fill={has_fill}");
                }
            }
        }
    }

}
