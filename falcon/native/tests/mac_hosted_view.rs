//! Real AppKit regression: run on the process main thread, before the release build.
//! No renderer, photo data, fullscreen transition or input-source mutation is required.
#[cfg(not(target_os = "macos"))]
fn main() {
    println!("mac_hosted_view: native AppKit check skipped on this platform");
}

#[cfg(target_os = "macos")]
fn main() {
    use objc2::rc::Retained;
    use objc2::{class, msg_send, msg_send_id};
    use objc2_app_kit::{NSView, NSWindow};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use winit::application::ApplicationHandler;
    use winit::dpi::{LogicalPosition, LogicalSize, PhysicalSize};
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::macos::{WindowAttributesExtMacOS, WindowExtMacOS};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{CursorIcon, Window, WindowId};

    fn view(window: &Window) -> Retained<NSView> {
        let RawWindowHandle::AppKit(raw) = window.window_handle().unwrap().as_raw() else {
            panic!("AppKit handle required");
        };
        unsafe { Retained::retain(raw.ns_view.as_ptr().cast()).unwrap() }
    }

    #[derive(Default)]
    struct Check {
        done: bool,
        windows: Option<(Window, Window)>,
        pending_sizes: Vec<PhysicalSize<u32>>,
        deadline: Option<std::time::Instant>,
    }
    impl ApplicationHandler for Check {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.windows.is_some() {
                return;
            }
            let hidden = Window::default_attributes()
                .with_visible(false)
                .with_active(false)
                .with_inner_size(LogicalSize::new(600., 400.));
            let host = event_loop.create_window(hidden.clone()).unwrap();
            let donor = event_loop
                .create_window(hidden.with_falcon_hosted_view(true))
                .unwrap();
            let renderer = view(&donor);
            let donor_native = renderer.window().unwrap();
            let owner_id = donor.id();
            let host_view = view(&host);
            let frame = NSRect::new(NSPoint::new(13., 21.), NSSize::new(417., 44.));
            let shell: Retained<NSView> =
                unsafe { msg_send_id![msg_send_id![class!(NSView), alloc], initWithFrame: frame] };
            unsafe {
                assert_eq!(host_view.isOpaque(), shell.isOpaque());
                assert_eq!(
                    host_view.mouseDownCanMoveWindow(), shell.mouseDownCanMoveWindow(),
                    "ordinary views must retain AppKit's inherited mouse handling"
                );
            }
            unsafe { host_view.addSubview(&shell) };

            for width in [417., 263., 701.] {
                // Regression: moving an NSWindow contentView to another view clears
                // the donor's contentView. Raw handles and IME/cursor APIs must still
                // reference the original WinitView, never unwrap nil or cast a dummy.
                unsafe {
                    shell.addSubview(&renderer);
                    renderer.setFrame(NSRect::new(NSPoint::new(0., 0.), NSSize::new(width, 44.)));
                    assert!(
                        !renderer.mouseDownCanMoveWindow(),
                        "hosted toolbar input must not also drag/zoom the native title bar"
                    );
                }
                assert_ne!(
                    donor_native.contentView().as_deref(),
                    Some(&*renderer),
                    "AppKit must exercise the original contentView-identity failure"
                );
                donor.sync_falcon_hosted_view();
                assert_eq!(donor.id(), owner_id);
                assert_eq!(Retained::as_ptr(&view(&donor)), Retained::as_ptr(&renderer));
                let native_host = renderer.window().unwrap();
                assert_eq!(native_host, host_view.window().unwrap());
                assert_eq!(donor.scale_factor(), native_host.backingScaleFactor());
                let expected =
                    LogicalSize::new(width, 44.).to_physical::<u32>(donor.scale_factor());
                self.pending_sizes.push(expected);
                assert_eq!(donor.inner_size(), expected);
                assert_eq!(
                    donor.request_inner_size(LogicalSize::new(999., 999.)),
                    Some(expected)
                );
                assert_eq!(
                    donor.inner_size(),
                    expected,
                    "hidden window constraints must not own accessory size"
                );
                donor.set_ime_allowed(false);
                donor.set_ime_cursor_area(LogicalPosition::new(5., 7.), LogicalSize::new(2., 3.));
                donor.set_cursor(CursorIcon::Pointer);
                donor.set_cursor_visible(true);
                // Slint re-applies set_resizable (a style-mask write) on every toolbar width
                // change. The hidden donor must not claim the adopted view as its responder.
                donor.set_resizable(false);
                assert!(
                    donor_native.firstResponder().is_none_or(|r| !std::ptr::eq(
                        Retained::as_ptr(&r).cast::<objc2::runtime::AnyObject>(),
                        Retained::as_ptr(&renderer).cast()
                    )),
                    "a view hosted in another window must not become the donor's first responder"
                );
                // The IME selector executes winit's coordinate conversion even with
                // composition off. Compare it with AppKit's actual host conversion.
                let caret = NSRect::new(NSPoint::new(5., 7.), NSSize::new(2., 3.));
                let expected_caret =
                    native_host.convertRectToScreen(renderer.convertRect_toView(caret, None));
                let actual: NSRect = unsafe {
                    msg_send![&*renderer, firstRectForCharacterRange: objc2_foundation::NSRange::new(0, 0)
                        actualRange: std::ptr::null_mut::<objc2_foundation::NSRange>()]
                };
                assert_eq!(actual, expected_caret);

                // Exercise temporary detachment and restoration, then repeat adoption.
                unsafe { renderer.removeFromSuperview() };
                donor.set_ime_allowed(false);
                assert_eq!(Retained::as_ptr(&view(&donor)), Retained::as_ptr(&renderer));
                donor_native.setContentView(Some(&renderer));
                assert_eq!(renderer.window().as_deref(), Some(&*donor_native));
                assert_eq!(Retained::as_ptr(&view(&donor)), Retained::as_ptr(&renderer));
            }
            // An ordinary, non-opted-in window retains the normal contentView identity.
            let plain: Retained<NSWindow> = host_view.window().unwrap();
            assert_eq!(plain.contentView().as_deref(), Some(&*host_view));
            self.windows = Some((host, donor));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            self.deadline = Some(deadline);
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        }

        fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
            if self
                .windows
                .as_ref()
                .is_some_and(|(_, donor)| donor.id() == id)
            {
                if let WindowEvent::Resized(size) = event {
                    self.pending_sizes.retain(|expected| *expected != size);
                }
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.windows.is_some() && self.pending_sizes.is_empty() {
                self.done = true;
                event_loop.exit();
            } else if self
                .deadline
                .is_some_and(|d| std::time::Instant::now() >= d)
            {
                panic!(
                    "hosted resize events lost or routed to another window: {:?}",
                    self.pending_sizes
                );
            }
        }
    }

    // Bound a stuck AppKit call too, not only an event loop that is still pumping.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("mac_hosted_view: native regression exceeded 15 seconds");
        std::process::exit(2);
    });
    let mut check = Check::default();
    EventLoop::new().unwrap().run_app(&mut check).unwrap();
    assert!(check.done);
    println!("mac_hosted_view: native adoption, identity, resize authority, IME coordinates and restoration passed");
}
