//! Falcon's interface as a library: the `slint::slint!` compilation unit that `falcon-native`
//! reaches as `crate::ui` (`use falcon_ui as ui;` in its `main.rs`), so `use ui::*` and
//! `crate::ui::X` resolve exactly as they did when this was `native/src/ui.rs`.
//!
//! Every component, global and struct lives in a `.slint` file in this folder, `MainWindow`
//! included (`main_window.slint`). The macro imports and re-exports exactly the symbols the app
//! needs. The macro records every imported file, so editing any `.slint` file rebuilds this crate
//! (and only this crate and what links it).
//!
//! `build.rs` beside this file sets what the Slint compiler reads at expansion time: element debug
//! info for debug builds (the headless rig), the bundled translations, and the scale-factor guard.

slint::slint! {
    // ONE compilation unit: main_window.slint pulls in the component files.
    import { ToolbarState } from "toolbar.slint";
    import { MacToolbarWindow } from "mac_toolbar.slint";
    import { MainWindow } from "main_window.slint";
    import { Theme, Tip } from "theme.slint";
    import { FilmItem, SelTileRow, ExifRow, ExifBrief, KeybindRow, NotifRow, CopyPrefRow, WmFontRow, DisplaySection } from "structs.slint";
    export { MainWindow, MacToolbarWindow, ToolbarState, Theme, Tip, FilmItem, SelTileRow, ExifRow, ExifBrief, KeybindRow, NotifRow, CopyPrefRow, WmFontRow, DisplaySection }
}

/// The Mac title-bar diagnostics bar: its own compilation unit, as it was in `falcon-native`
/// (`mac_experiment_ui.rs`), so its `Theme` stays separate from the main window's. Compiled on
/// macOS and for the app's tests only (the `mac-probe` feature); a Windows release never builds it.
#[cfg(feature = "mac-probe")]
pub mod mac_probe {
    slint::slint! {
        import { MacExperimentBar } from "mac_experiment.slint";
        export { MacExperimentBar }
    }
}
