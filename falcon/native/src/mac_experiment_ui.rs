//! Compiled on Mac diagnostics and in automated UI tests; no donor window in shipping Windows.
//! The bar itself is compiled in the interface crate (`falcon_ui::mac_probe`, the `mac-probe`
//! feature, which `Cargo.toml` turns on for macOS and for tests).
pub(crate) use falcon_ui::mac_probe::MacExperimentBar;

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition};

    fn pointer(window: &slint::Window, position: LogicalPosition, button: PointerEventButton) {
        window.dispatch_event(WindowEvent::PointerPressed { position, button });
        window.dispatch_event(WindowEvent::PointerReleased { position, button });
    }

    #[test]
    fn real_probe_controls_deliver_primary_and_secondary_input() {
        std::thread::spawn(|| {
            i_slint_backend_testing::init_no_event_loop();
            let bar = MacExperimentBar::new().unwrap();
            bar.show().unwrap();
            pointer(
                bar.window(),
                LogicalPosition::new(20., 22.),
                PointerEventButton::Left,
            );
            assert_eq!(bar.get_clicks(), 1);
            let element = i_slint_backend_testing::ElementHandle::find_by_element_id(
                &bar,
                "MacExperimentBar::event-box",
            )
            .next()
            .unwrap();
            let pos = element.absolute_position();
            pointer(
                bar.window(),
                LogicalPosition::new(pos.x + 15., pos.y + 22.),
                PointerEventButton::Right,
            );
            assert_eq!(bar.get_secondary_clicks(), 1);
            assert_eq!(bar.get_clicks(), 1);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn diagnostic_popup_is_gated_off_in_normal_windows_and_dismisses_escape() {
        std::thread::spawn(|| {
            i_slint_backend_testing::init_no_event_loop();
            let app = crate::MainWindow::new().unwrap();
            app.window().set_size(slint::LogicalSize::new(900., 650.));
            app.show().unwrap();
            assert_eq!(
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                    &app,
                    "MacExperimentPopup"
                )
                .count(),
                0
            );
            assert_eq!(
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                    &app,
                    "MacExperimentStatus"
                )
                .count(),
                0
            );
            app.set_mac_experiment_label("test".into());
            app.set_mac_experiment_popup_open(true);
            assert_eq!(
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                    &app,
                    "MacExperimentPopup"
                )
                .count(),
                1
            );
            slint::platform::update_timers_and_animations();
            app.window().dispatch_event(WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
            assert!(!app.get_mac_experiment_popup_open());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn probe_refocus_delivers_f_to_the_normal_key_dispatcher() {
        std::thread::spawn(|| {
            use std::{cell::Cell, rc::Rc};
            i_slint_backend_testing::init_no_event_loop();
            let app = crate::MainWindow::new().unwrap();
            app.set_welcome_open(false);
            app.set_assoc_prompt_open(false);
            app.set_mac_experiment_label("probe".into());
            app.show().unwrap();
            let calls = Rc::new(Cell::new(0));
            let count = calls.clone();
            // Printable shortcuts enter Rust through key-typed; the normal Rust keymap
            // resolves "f" to the fullscreen command (confirmed in the recovered settings).
            app.on_key_typed(move |token, _repeat| {
                if token == "f" {
                    count.set(count.get() + 1);
                }
            });
            let ime = Rc::new(Cell::new(true));
            let set_ime = ime.clone();
            app.on_set_ime(move |allowed| set_ime.set(allowed));
            app.invoke_mac_experiment_refocus();
            assert!(!ime.get());
            app.window()
                .dispatch_event(WindowEvent::KeyPressed { text: "f".into() });
            assert_eq!(calls.get(), 1);
        })
        .join()
        .unwrap();
    }
}
