//! v0.9.22: the macOS **menu-bar MODEL** — platform-neutral, pure, Windows-unit-tested.
//!
//! The native NSMenu realization lives in `mac_menu.rs` (cfg macOS); THIS file is the single source
//! of truth for the menu TREE (titles, tags, key equivalents, check state, structure) and the
//! ENABLEMENT predicate, computed from a plain `MenuSnapshot` the tick assembles each frame. The
//! split follows the round rule "cargo check never compiles cfg-test code": every menu DECISION is
//! testable on the Windows host; only the AppKit realization is compiler-blind here.
//!
//! ── THE ROUND'S BINDING DESIGN CONSTRAINTS (owner ruling, v0.9.22) ────────────────────────────────
//! * Only ⌘-chords are LIVE key equivalents (⌘, ⌘O ⌘W ⌘Z ⌘C ⌃⌘F — plus ⌘Q/⌘M outside this model:
//!   ⌘Q stays on muda's retargeted Quit item, ⌘M on the Window menu's standard Minimize). Bare cull
//!   keys are DISPLAY-ONLY: NSMenu cannot show a right-column key without registering it live (AppKit
//!   would then intercept unmodified keys before text fields), so they ride the title as a suffix in
//!   the natural form "Flag (P)" / "Rate ★★★ (3)". `live_equivalents_are_cmd_chords_only` PINS the
//!   invariant.
//! * Every displayed key renders from the LIVE keymap (`MenuKeys` is built via `support::
//!   menu_shortcut`, the same formatter the in-app context-menu chips and Settings rows use) — the
//!   tick rebuilds the snapshot each frame and the mac side re-titles on fingerprint change AND on
//!   menu open. No key string is hardcoded here.
//! * Enablement honesty (ledger L15): photo-dependent items disable in the empty state; Undo
//!   enables only when something is undoable; Sort/recents reflect the live state.
//!
//! Tags are stable i32s (NSMenuItem.tag); the dispatch match lives in main.rs's tick drain and
//! routes every tag to the SAME `invoke_*` callback the in-app control uses (single dispatch path).

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::hash::{Hash, Hasher};

use crate::i18n;

// ── command tags (NSMenuItem.tag; 0 = structural item with no command) ───────────────────────────
pub(crate) const TAG_SETTINGS: i32 = 1; // app menu: Settings… ⌘,
pub(crate) const TAG_WELCOME: i32 = 2; // app menu: Show Welcome Guide
pub(crate) const TAG_QUIT: i32 = 3; // app menu: muda's Quit item, retargeted to the graceful close
pub(crate) const TAG_OPEN_IMAGE: i32 = 10;
pub(crate) const TAG_OPEN_FOLDER: i32 = 11; // ⌘O
pub(crate) const TAG_RECENT_PARENT: i32 = 12; // Open Recent ▸ (explicit-enable File menu)
pub(crate) const TAG_RECENT_EMPTY: i32 = 13; // the disabled "No Recent Folders" placeholder
pub(crate) const TAG_REVEAL: i32 = 14;
pub(crate) const TAG_CLOSE: i32 = 15; // ⌘W
pub(crate) const TAG_UNDO: i32 = 20; // ⌘Z
pub(crate) const TAG_COPY: i32 = 21; // ⌘C
pub(crate) const TAG_RATE1: i32 = 30; // ..TAG_RATE5 = 34 (contiguous — dispatch relies on it)
pub(crate) const TAG_RATE5: i32 = 34;
pub(crate) const TAG_RATE0: i32 = 35;
pub(crate) const TAG_FLAG: i32 = 36;
pub(crate) const TAG_REJECT: i32 = 37;
pub(crate) const TAG_UNMARK: i32 = 38;
pub(crate) const TAG_ROT_LEFT: i32 = 39;
pub(crate) const TAG_ROT_RIGHT: i32 = 40;
pub(crate) const TAG_TRASH: i32 = 41;
pub(crate) const TAG_XMP: i32 = 42;
pub(crate) const TAG_COMPARE: i32 = 50;
pub(crate) const TAG_SWAP: i32 = 51;
pub(crate) const TAG_PIN: i32 = 52;
pub(crate) const TAG_ZOOM_11: i32 = 53;
pub(crate) const TAG_ZOOM_FIT: i32 = 54;
pub(crate) const TAG_GRID: i32 = 55;
pub(crate) const TAG_FILM: i32 = 56;
pub(crate) const TAG_INFO_PANEL: i32 = 59; // v0.9.24: the state-derived Show/Expand/Minimise info-panel row
pub(crate) const TAG_FULLSCREEN: i32 = 58; // ⌃⌘F (the ONE fullscreen row — the old TAG_IMMERSIVE=57 synonym was retired by owner ruling; the in-app F still toggles immersive but is no longer a menu row)
pub(crate) const TAG_SORT_PARENT: i32 = 69;
pub(crate) const TAG_SORT_M0: i32 = 70; // ..TAG_SORT_M6 = 76 (contiguous, mirrors sort-pick(int))
pub(crate) const TAG_SORT_M6: i32 = 76;
pub(crate) const TAG_SORT_ASC: i32 = 77;
pub(crate) const TAG_SORT_DESC: i32 = 78;
pub(crate) const TAG_HELP_SHORTCUTS: i32 = 80;
/// v0.8.119 (design-sweep Y58): Help → "Show log file" — the same dispatch the photo's right-click
/// menu runs (`show-log`). Always available: the log exists from the first line of boot.
pub(crate) const TAG_HELP_LOG: i32 = 81;
pub(crate) const TAG_RECENT_BASE: i32 = 100; // + index into MenuSnapshot.recents

/// Cap on the Open Recent submenu length. The backing store (`Settings.last_viewed`, the resume-
/// position LRU — the app's ONLY folder history) can hold up to the user's resume cap (default 10,
/// custom up to 500); the menu shows the most-recent 10.
pub(crate) const RECENTS_MAX: usize = 10;

/// What the next ⌘Z would undo — drives the Edit item's honest dynamic title (L17: a control's
/// label is a contract). `Delete` is on_undo's own answer ([`undo_kind_for`]); `Other` is a
/// rating/flag/rotation undo or a move.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum UndoKind {
    #[default]
    None,
    Delete,
    Other,
}

/// Keeping prepared work (2 November): the arm `on_undo` would take (`support::live_undo_arm`). The
/// stack now survives a Delete, so "empty stack" no longer says the delete is next. No arm but a
/// live record is a delete from another folder: ⌘Z answers it with the honest refusal.
pub(crate) fn undo_kind_for(arm: Option<crate::support::UndoArm>, has_record: bool) -> UndoKind {
    use crate::support::UndoArm;
    match arm {
        Some(UndoArm::Delete(_)) => UndoKind::Delete,
        Some(UndoArm::Stack(_) | UndoArm::Move(_)) => UndoKind::Other,
        None if has_record => UndoKind::Delete,
        None => UndoKind::None,
    }
}

/// The LIVE display keys (already pretty-formatted via `support::menu_shortcut` — the same single
/// source the Settings rows and context-menu chips render from). Empty string = unbound → no hint.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct MenuKeys {
    pub(crate) rate: [String; 5], // rate1..rate5
    pub(crate) rate0: String,
    pub(crate) flag: String,
    pub(crate) reject: String,
    pub(crate) unflag: String,
    pub(crate) rotcw: String,
    pub(crate) rotccw: String,
    pub(crate) delete: String,
    pub(crate) compare: String,
    pub(crate) cmpswap: String,
    pub(crate) cmppin: String,
    pub(crate) zoom: String,
    pub(crate) full: String,
    pub(crate) info: String, // v0.9.24: the info-panel row's display-only bare-key suffix
}

/// The per-tick app-state snapshot the menu renders from. Assembled on the UI thread (tick), stored
/// in `mac_menu`'s static for the AppKit callbacks (validate / menuNeedsUpdate) to read.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct MenuSnapshot {
    pub(crate) has_photo: bool, // !empty-state — a folder with ≥1 shot is loaded
    pub(crate) count: i32,      // count-all (Compare needs ≥ 2)
    pub(crate) modal: bool,     // the ONE ui modal-open predicate (mirrors every in-app key gate)
    // v0.8.121 (Round-B fix F5 = audit A7, L26): the SUBSET of `modal` that puts a SCRIM on screen
    // (`main_window.slint: modal-blocking` — a confirm, the rotation reminder, the export sheet, the
    // display-forget confirm, the first-launch association popup). `modal` above includes
    // `settings-open` and `welcome-open` themselves, so it cannot express "a panel would mount
    // UNDER a scrim": gating Settings on it would disable the row that CLOSES the Settings panel.
    // Wave 2 (O8/O9) gave the four title-bar panel buttons `enabled: !modal-blocking` and left the
    // macOS twins unconditionally enabled — so ⌘, under an Empty-to-Trash confirm still raised the
    // full-height Settings panel below the scrim, completely inert, its own close × included, with
    // no cue; and Falcon → Show Welcome Guide still stacked a SECOND scrim over the dialog. Same
    // body, one guarded invoker, one ungated invoker.
    pub(crate) modal_blocking: bool,
    /// v0.9.67 (OWNER RULING 08-08 (ii)): `modal` MINUS `welcome-open` — the fullscreen action's own
    /// gate, and only that action's. Fed from the `.slint` `modal-open-fs` property, the same single
    /// source the F key and the F11/⌃⌘F arms read, so the three doors into one command cannot
    /// disagree (L26/L21: an enablement only one invoker honours is not an enablement). Everything
    /// else in this table keeps reading `modal`.
    pub(crate) modal_fs: bool,
    pub(crate) context: Option<u64>, // identity of an open photo context for queued key equivalents
    pub(crate) opening_photo: bool, // folder-wide sort is unavailable in either early-edit mode
    /// Fix round 1 (1 November, F13-apply-4, L21/L26): Apply rotations is running, so sorting is
    /// unavailable (`apply_pick` refuses then). The `.slint` `applying-rotations` property the
    /// in-app sort rows grey on.
    pub(crate) applying_rotations: bool,
    pub(crate) inspect_open: bool, // ready photo may be inspected while View only blocks collection actions
    pub(crate) compare: bool,
    pub(crate) at_fit: bool, // single-view zoom == 1.0 (±0.01 — on_photo_clicked's own tolerance); drives the honest Zoom 1:1 / Zoom to Fit enablement split (L17)
    pub(crate) immersive: bool,
    /// v0.9.63 (B-R5-2): is the NSWindow itself in NATIVE fullscreen? Independent of `immersive` —
    /// a green-button / ⌃⌘F / Spaces fullscreen leaves Falcon's chrome on and `immersive` false, and
    /// that combination is a valid state the app deliberately does not fight. It is a snapshot field
    /// because the View row's title must say which way the ONE fullscreen command will go, and since
    /// B-R5-2 that command leaves a foreign fullscreen too. Fed from the tick's existing 4 Hz poll
    /// (no extra AppKit round-trip); hashed like every other field, so the row re-titles within a
    /// poll of the user hitting the green button.
    pub(crate) os_fullscreen: bool,
    pub(crate) grid_open: bool,
    pub(crate) film_visible: bool,
    // v0.9.24: the info panel's TWO fields — the row title is derived from them (support::
    // info_menu_title), never from a third model, and hashing them here is what moves the
    // fingerprint so the tick re-titles the row the frame after any surface changes the state.
    // v0.8.97 (ruling 5): + the DEVELOP panel's open flag, because the row's OFF title depends on
    // it — with both panels hidden the command restores both, and says "Show panels". It is hashed
    // like the other two, so hiding/showing the develop panel re-titles the row on the next frame.
    pub(crate) info_open: bool,
    pub(crate) info_min: bool,
    pub(crate) raw_open: bool,
    pub(crate) sort_method: i32, // 0..6 — the in-app sort menu's method rows
    pub(crate) sort_desc: bool,
    pub(crate) xmp_sync: bool,
    pub(crate) rating: i32, // current shot (Rate items' check state)
    pub(crate) flagged: bool,
    pub(crate) rejected: bool,
    // ── v0.9.64 (OWNER RULING 08-04, option (a) — FULL PLURALITY): the SELECTION's four scalars ──
    // With a selection live, all five Photo rows act on the whole set and SAY SO, so the tree needs
    // the count and the group's state to title itself with. All four are read straight off
    // properties the tick's OWN gated selection sweep already publishes (`tick::step_selection`:
    // count-selected, bulk-flag-on, bulk-reject-on, bulk-uniform-rating), so they are four scalar
    // property reads inside the v0.9.60 allocation-free probe — no new walk, no new AppKit work,
    // and no second producer for numbers the Review panel's own controls already render from.
    /// How many photographs are in the transient selection (0 = none). NOT by itself the arming
    /// question — see [`bulk_count`], which asks `support::bulk_actions_allowed` about this.
    pub(crate) sel_count: i32,
    /// Would a bulk flag SET the bit (nobody, or not everybody, carries it) or CLEAR it (everybody
    /// does)? This is `support::bulk_mark_sets`' own answer, carried rather than re-derived — the
    /// sweep publishes its complement as `bulk-flag-on`, which is the GROUP's state. It decides the
    /// row's verb ("Flag 12 Photos" vs "Unflag 12 Photos") and its checkmark, which is why the two
    /// can never disagree with the write or with the always-on-screen cull button.
    pub(crate) sel_flag_sets: bool,
    /// …and the same bit for reject ("Reject 12 Photos" / "Un-reject 12 Photos").
    pub(crate) sel_reject_sets: bool,
    /// The selection's UNIFORM rating, or -1 for a mixed spread — `support::uniform_rating`'s
    /// answer, as the sweep publishes it to `bulk-uniform-rating`. It drives the Rate rows' check
    /// state while armed, exactly as `rating` does while not: a checkmark means "the whole group is
    /// already here", which is also the state in which a re-press clears.
    pub(crate) sel_rating: i32,
    pub(crate) undo: UndoKind,
    pub(crate) recents: Vec<String>, // full folder paths, most-recent FIRST, ≤ RECENTS_MAX
    pub(crate) keys: MenuKeys,
}

/// One menu item in the model tree. `key`/`ctrl` describe a LIVE equivalent (⌘-chord only — pinned
/// by test); `title` carries any display-only bare-key hint as a suffix.
pub(crate) struct ItemDef {
    pub(crate) tag: i32,
    pub(crate) title: String,
    pub(crate) key: &'static str, // "" = no live equivalent
    pub(crate) ctrl: bool,        // adds ⌃ to the implicit ⌘ (only ⌃⌘F uses it)
    pub(crate) checked: bool,
    pub(crate) separator: bool,
    pub(crate) submenu: Option<Vec<ItemDef>>,
}

impl ItemDef {
    fn sep() -> Self {
        Self { tag: 0, title: String::new(), key: "", ctrl: false, checked: false, separator: true, submenu: None }
    }
    fn act(tag: i32, title: impl Into<String>) -> Self {
        Self { tag, title: title.into(), key: "", ctrl: false, checked: false, separator: false, submenu: None }
    }
    fn key(mut self, k: &'static str) -> Self {
        self.key = k;
        self
    }
    fn ctrl(mut self) -> Self {
        self.ctrl = true;
        self
    }
    fn check(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }
    fn parent(tag: i32, title: impl Into<String>, items: Vec<ItemDef>) -> Self {
        Self { tag, title: title.into(), key: "", ctrl: false, checked: false, separator: false, submenu: Some(items) }
    }
}

/// "Flag" + "P" → "Flag (P)"; unbound key → bare title. The RULED presentation for display-only
/// bare-key hints (NSMenu can't show a right-column key without registering it live).
/// Language packs (round 2): the title and its key make one message, so a language can place them.
fn hint(title: &str, key: &str) -> String {
    if key.is_empty() {
        title.to_string()
    } else {
        tr_format!("{title} ({key})", title = title, key = key)
    }
}

/// v0.9.64 (OWNER RULING 08-04): HOW MANY PHOTOGRAPHS THE PHOTO ROWS ARE ABOUT — `Some(n)` when the
/// rows are plural, `None` when they are their single-photo selves.
///
/// It does not MIRROR the bulk gate, it CALLS it. `support::bulk_actions_allowed` is the one
/// predicate the keyboard's `bulk_armed`, the Slint `bulk-actions-armed` mirror and every bulk
/// handler's Rust-side re-ask all answer to, and the ruling was explicit that the menu bar joins it
/// rather than growing a menu-only variant. Everything a menu-only variant would have cost is
/// avoided by there being no second expression to keep in step: the dormancy pair (a lingering
/// selection in compare is a write over photographs in NEITHER half; immersive unmounts the
/// selection's whole disclosure) rides in for free, and a future term arrives here without an edit.
///
/// Taken as three scalars rather than a `&MenuSnapshot` so the TICK's dispatch — which decides
/// which door to knock on at click time, from the live properties rather than from a snapshot that
/// may be one pump cycle old — asks the identical question through the identical call.
pub(crate) fn bulk_count(sel_count: i32, compare: bool, immersive: bool) -> Option<usize> {
    let n = sel_count.max(0) as usize;
    crate::support::bulk_actions_allowed(n, compare, immersive).then_some(n)
}

/// Folder path → the menu row title (its basename; the full path when the basename is empty, e.g.
/// a drive root).
pub(crate) fn recent_title(path: &str) -> String {
    let base = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base.is_empty() {
        path.to_string()
    } else {
        base
    }
}

/// The two items grafted into muda's default app menu (after About + its separator):
/// Settings… ⌘, and Show Welcome Guide. (About/Services/Hide/Hide Others/Show All stay muda's;
/// Quit is retargeted in place — see mac_menu.)
pub(crate) fn app_menu_inserts() -> Vec<ItemDef> {
    vec![
        ItemDef::act(TAG_SETTINGS, i18n::tr("Settings…")).key(","),
        ItemDef::act(TAG_WELCOME, i18n::tr("Show Welcome Guide")),
    ]
}

/// The top-level menus Falcon adds to the bar, by a stable id.
///
/// Language packs (round 2, PLAN §4): a language pack translates each menu's drawn title, so the
/// bar must never find a menu by its title. `mac_menu` asks [`TopMenu::role`] which menu is Help
/// (registered as the help menu, with the Window menu placed before it) and which run explicit
/// enablement (File and View), and draws [`TopMenu::title`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopMenu {
    File,
    Edit,
    Photo,
    View,
    Help,
}

/// How the bar realizes one top-level menu. Decided from the menu's id, never from its title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TopMenuRole {
    /// The standard Window menu goes right before this menu (macOS puts it before Help).
    pub(crate) window_before: bool,
    /// `autoenablesItems` off, so a submenu parent can honestly disable: File → Open Recent and
    /// View → Sort take their enabled state from [`enabled_for`].
    pub(crate) explicit_enables: bool,
    /// Registered as `NSApp.helpMenu` (the system search field comes with it).
    pub(crate) help_menu: bool,
}

impl TopMenu {
    /// The title drawn on the bar, in this run's language.
    pub(crate) fn title(self) -> &'static str {
        let english = match self {
            TopMenu::File => tr_noop!("File"),
            TopMenu::Edit => tr_noop!("Edit"),
            TopMenu::Photo => tr_noop!("Photo"),
            TopMenu::View => tr_noop!("View"),
            TopMenu::Help => tr_noop!("Help"),
        };
        i18n::tr(english)
    }

    pub(crate) fn role(self) -> TopMenuRole {
        TopMenuRole {
            window_before: self == TopMenu::Help,
            explicit_enables: matches!(self, TopMenu::File | TopMenu::View),
            help_menu: self == TopMenu::Help,
        }
    }
}

/// The full custom menu tree with each menu's drawn title: File · Edit · Photo · View · Help.
pub(crate) fn build_menus(s: &MenuSnapshot) -> Vec<(&'static str, Vec<ItemDef>)> {
    top_menus(s).into_iter().map(|(id, items)| (id.title(), items)).collect()
}

/// The full custom menu tree: File · Edit · Photo · View · Help (Window is built mac-side from
/// AppKit standard selectors so the OS populates it). Titles/checks re-derive from the snapshot on
/// every fingerprint change and on menu open.
pub(crate) fn top_menus(s: &MenuSnapshot) -> Vec<(TopMenu, Vec<ItemDef>)> {
    let k = &s.keys;

    // File — realized with autoenablesItems OFF (explicit enables via `enabled_for`) so the
    // Open Recent PARENT can honestly disable when the recents list is empty (constraint 6).
    let recents: Vec<ItemDef> = if s.recents.is_empty() {
        vec![ItemDef::act(TAG_RECENT_EMPTY, i18n::tr("No Recent Folders"))]
    } else {
        s.recents
            .iter()
            .enumerate()
            .take(RECENTS_MAX)
            .map(|(i, p)| ItemDef::act(TAG_RECENT_BASE + i as i32, recent_title(p)))
            .collect()
    };
    let file = vec![
        ItemDef::act(TAG_OPEN_IMAGE, i18n::tr("Open Image…")),
        ItemDef::act(TAG_OPEN_FOLDER, i18n::tr("Open Folder…")).key("o"),
        ItemDef::parent(TAG_RECENT_PARENT, i18n::tr("Open Recent"), recents),
        ItemDef::sep(),
        // v1.0.0-rc (queue item 27, sheet 2.1 B3 b — the L26 parity question, ANSWERED): the two
        // context menus gain a counted "Reveal N in Finder" this round and THIS ROW STAYS SINGULAR,
        // because the File menu's subject is the CURRENT file — it sits between "Open Image…" and
        // "Close", both of which are about that one file too — and a counted row here would be the
        // only member of the menu whose subject is the selection.
        ItemDef::act(TAG_REVEAL, i18n::tr(crate::platform::PLATFORM.reveal_verb)),
        ItemDef::sep(),
        ItemDef::act(TAG_CLOSE, i18n::tr("Close")).key("w"),
    ];

    // Edit — the Undo title is DYNAMIC: "Undo Delete" exactly when the next ⌘Z recovers the deleted
    // shot (`undo_kind_for`, on_undo's own answer), plain "Undo" for a rating/flag/rotation undo or a
    // move. The ⌘Z equivalent routes to the SAME invoke_undo() as the in-app key, so behavior is identical.
    let undo_title = match s.undo {
        UndoKind::Delete => i18n::tr("Undo Delete"),
        _ => i18n::tr("Undo"),
    };
    //
    // ── v0.9.63: WHY THERE IS NO "Select All" ROW HERE [VD] ─────────────────────────────────────
    // main's selection feature ships ⌘A (select every DISPLAYED photo) and ⌘D (deselect all), and
    // macOS convention would put Select All in this menu. It is deliberately absent, for a reason
    // that is about NSMenu rather than about taste.
    //
    // THE CHORDS ALREADY WORK, BY CONSTRUCTION. Slint's winit backend swaps Command and Control on
    // Apple platforms before the key reaches anything — `event_loop.rs`: "For now: Match Qt's
    // behavior of mapping command to control and control to meta", rewriting `NamedKey::Super` to
    // `NamedKey::Control`, which `to_slint_key` turns into the Control char, which
    // `InternalKeyboardModifierState::state_update` records as `left_control`. So `e.modifiers
    // .control` in the FocusScope ladder is TRUE for ⌘, and main's `Ctrl+A` / `Ctrl+D` arms fire on
    // ⌘A / ⌘D with no code of ours involved. It is the same mechanism this branch's shipped ⌘C, ⌘Z
    // and ⌘Y already ride, and the same one `PLATFORM.mod_ctrl` ("⌘") and
    // `deselect_shortcut_display` ("⌘D") already assume when they caption those chords.
    //
    // ADDING THE ROW WOULD TAKE THEM AWAY. An NSMenuItem with a ⌘A key equivalent INTERCEPTS the
    // chord before the window ever sees it, so the row would REPLACE the ladder's path with the
    // dispatch's — and the ladder gates on four terms (`!modal-open && !capture-armed && !compare
    // && !immersive`) of which `capture_armed` is not in `MenuSnapshot`. A row shipped with a gate
    // that is a superset of the key's would fire ⌘A while a rebind row is listening for a keypress:
    // a working chord traded for a differently-gated one, on a platform this repo cannot execute
    // outside CI. The row is a good idea and it is one snapshot field away; it is not a merge rider.
    let edit = vec![
        ItemDef::act(TAG_UNDO, undo_title).key("z"),
        ItemDef::act(TAG_COPY, i18n::tr("Copy")).key("c"),
    ];

    // Photo — the five rating actions (same-key-clears semantics live in the dispatch, exactly like
    // the keyboard path), marks, rotation, trash (SAME confirm-dialog path), and the XMP checkbox
    // mirroring the REVIEW panel's toggle (v0.9.63: main's v0.8.130 renamed that panel from
    // "Selection" to "Review" in all six of its places; this comment was the seventh). Rust gates
    // OFF→ON behind the backfill confirm; the check state mirrors xmp-sync, so a cancelled backfill
    // stays visibly OFF.
    //
    // ── v0.9.64 (OWNER RULING 08-04, option (a)): THESE ROWS ARE PLURAL, AND THEY SAY SO ────────
    //
    // v0.9.63 shipped them single-photo and recorded exactly this decision point. main's selection
    // feature (v0.8.129→135) had made the KEYBOARD's cull keys plural whenever
    // `support::bulk_actions_allowed` holds — with twelve photos selected, P flags twelve — while
    // these rows dispatched to the single-photo `invoke_set_flag` / `invoke_set_rating` and flagged
    // ONE, all the while carrying the very key hint ("Flag (P)") that promises they are the same
    // command. (Trash was the exception nobody had noticed: `delete-key`'s own body has been plural
    // since v0.8.130, so the row titled "Move to Trash…" was ALREADY opening a "Delete 12 photos?"
    // confirm — a live L20 defect, an imperative label stating the opposite scale of what it does.)
    //
    // The owner ruled full plurality: all five act on the whole selection, with counted,
    // direction-stating titles in the context menus' established grammar. Which is the half that
    // matters. A row that silently widens from "Flag" to twelve photographs is the exact hazard the
    // counted context rows exist to prevent; a row that says "Unflag 12 Photos" cannot be misread,
    // and cannot promise the opposite of what it does, because both the verb and the count come out
    // of the same `bulk_mark_sets` bit the write itself consumes. With NO selection every title and
    // every behaviour below is byte-identical to v0.9.63 — pinned by test, in both directions.
    //
    // The gate is [`bulk_count`] → `support::bulk_actions_allowed`, the keys' own predicate, called
    // rather than mirrored. So: compare and immersive are single-photo here for the same reasons
    // they are single-photo everywhere else, and the menu bar has no gate of its own to drift.
    let bulk = bulk_count(s.sel_count, s.compare, s.immersive);
    let mut photo: Vec<ItemDef> = Vec::with_capacity(16);
    for n in 1..=5i32 {
        let stars = "★".repeat(n as usize);
        // Armed, the row names the group and the check follows the GROUP's uniform rating (-1 =
        // mixed ⇒ nothing checked, which is what mixed honestly looks like). The direction of a
        // bulk rating is not stated in the title on purpose and the ruling is why: it is decided at
        // press time by `support::bulk_rate_target` from that same uniform state, and then the
        // ask-first toast SAYS it in full ("Clear rating on 12 photos?") before a byte is written.
        // The checkmark is the same cue the single-photo row has always carried — checked means the
        // press is a re-press, which is the state a clear can come out of.
        // Language packs (round 2): each counted row in this menu is one whole message ("Rate {n}
        // Photos {stars}"), no longer a verb joined to a separately counted "12 Photos".
        let (title, checked) = match bulk {
            Some(n_sel) => {
                (tr_plural!(n_sel, "Rate {n} Photo {stars}", "Rate {n} Photos {stars}", stars = stars), s.sel_rating == n)
            }
            None => (tr_format!("Rate {stars}", stars = stars), s.rating == n),
        };
        photo.push(ItemDef::act(TAG_RATE1 + n - 1, hint(&title, &k.rate[(n - 1) as usize])).check(checked));
    }
    let rate0_title = match bulk {
        // "Clear Rating on 12 Photos" — the toast's own preposition, one register up.
        Some(n) => tr_plural!(n, "Clear Rating on {n} Photo", "Clear Rating on {n} Photos"),
        None => i18n::tr("Clear Rating").to_string(),
    };
    photo.push(ItemDef::act(TAG_RATE0, hint(&rate0_title, &k.rate0)));
    photo.push(ItemDef::sep());
    // Flag / Reject: the counted DIRECTED title, and a checkmark that mirrors the GROUP's state —
    // the complement of "the press would set it", which is precisely `bulk-flag-on`, the property
    // the always-on-screen cull button lights from. One bit, three readings, no way to disagree.
    let (flag_title, flag_check) = match bulk {
        Some(n) => (
            crate::support::bulk_menubar_mark_title("Flag", "Unflag", n, s.sel_flag_sets),
            !s.sel_flag_sets,
        ),
        None => (i18n::tr("Flag").to_string(), s.flagged),
    };
    let (reject_title, reject_check) = match bulk {
        Some(n) => (
            crate::support::bulk_menubar_mark_title("Reject", "Un-reject", n, s.sel_reject_sets),
            !s.sel_reject_sets,
        ),
        None => (i18n::tr("Reject").to_string(), s.rejected),
    };
    photo.push(ItemDef::act(TAG_FLAG, hint(&flag_title, &k.flag)).check(flag_check));
    photo.push(ItemDef::act(TAG_REJECT, hint(&reject_title, &k.reject)).check(reject_check));
    // Unmark carries no direction because it HAS none: `BulkOp::Clear` clears both marks, always —
    // it is an imperative, not a toggle, exactly as the `u` key's plural is.
    let unmark_title = match bulk {
        Some(n) => tr_plural!(n, "Unmark {n} Photo", "Unmark {n} Photos"),
        None => i18n::tr("Unmark").to_string(),
    };
    photo.push(ItemDef::act(TAG_UNMARK, hint(&unmark_title, &k.unflag)));
    photo.push(ItemDef::sep());
    // v1.0.0-rc (queue item 27, sheet 2.1 B2 b, OWNER RULING): ROTATION COUNTS NOW, and the
    // v0.9.64 note this replaces said the opposite for a reason that has just stopped being true.
    // It read: "there is no bulk rotate anywhere in the app — not on the keys, not in the context
    // menus — so a plural title here would be the first surface to promise one." This commit builds
    // exactly that: `bulk-rotate(dir)` behind the plural rows on both context menus and behind the
    // R / Shift+R keys while a selection is armed. So the premise is gone and the rows take the
    // ruling's plurality, through the SAME `bulk_count` gate every other Photo row consults.
    //
    // NO DIRECTION TERM, and that is not an omission: `bulk_menubar_mark_title` derives Flag ⇄ Unflag
    // from the group's uniform state because a mark is a TOGGLE. A rotate is an imperative, like
    // Unmark two rows up — a quarter-turn right is a quarter-turn right whatever the set already
    // carries — so the direction word is the row's own and only the count moves.
    let (rot_left_title, rot_right_title) = match bulk {
        Some(n) => (
            tr_plural!(n, "Rotate {n} Photo Left", "Rotate {n} Photos Left"),
            tr_plural!(n, "Rotate {n} Photo Right", "Rotate {n} Photos Right"),
        ),
        None => (i18n::tr("Rotate Left").to_string(), i18n::tr("Rotate Right").to_string()),
    };
    photo.push(ItemDef::act(TAG_ROT_LEFT, hint(&rot_left_title, &k.rotccw)));
    photo.push(ItemDef::act(TAG_ROT_RIGHT, hint(&rot_right_title, &k.rotcw)));
    photo.push(ItemDef::sep());
    // Trash: the ruling's verbatim "Delete 12 Photos…", whose ellipsis is the count-naming confirm
    // (`support::bulk_delete_title` → "Delete 12 photos?"). The armed title deliberately drops the
    // "Move to Trash" phrasing: the verb the dialog uses is Delete, and a row promising a plural
    // action should be answerable by reading the dialog it opens.
    let trash_title = match bulk {
        Some(n) => tr_plural!(n, "Delete {n} Photo…", "Delete {n} Photos…"),
        None => tr_format!("{title}…", title = i18n::tr(crate::platform::PLATFORM.move_to_trash)),
    };
    photo.push(ItemDef::act(TAG_TRASH, hint(&trash_title, &k.delete)));
    photo.push(ItemDef::sep());
    photo.push(ItemDef::act(TAG_XMP, i18n::tr("Sync ratings to XMP")).check(s.xmp_sync));

    // View — compare trio, zoom pair, the two dock toggles (check state = live visibility), the ONE
    // fullscreen row (dynamic Enter/Exit Full Screen carrying the live ⌃⌘F chord), and the Sort
    // submenu radio-mirroring the in-app position-chip menu. Owner ruling: the old "Immersive Full
    // Screen" row (an honest F synonym — it drove the same toggle-fullscreen handler) was removed as
    // confusing; the in-app F still toggles immersive, it simply is not a menu row anymore.
    let sort_items: Vec<ItemDef> = {
        // Mirrors the in-app sort menu's rows (main_window.slint ~6014) — structure = that menu.
        const METHODS: [&str; 7] = [
            tr_noop!("Name"),
            tr_noop!("Date taken"),
            tr_noop!("Date modified"),
            tr_noop!("Date created"),
            tr_noop!("Size"),
            tr_noop!("Type"),
            tr_noop!("Rating"),
        ];
        let mut v: Vec<ItemDef> = METHODS
            .iter()
            .enumerate()
            .map(|(i, &m)| ItemDef::act(TAG_SORT_M0 + i as i32, i18n::tr(m)).check(s.sort_method == i as i32))
            .collect();
        v.push(ItemDef::sep());
        v.push(ItemDef::act(TAG_SORT_ASC, i18n::tr("Ascending")).check(!s.sort_desc));
        v.push(ItemDef::act(TAG_SORT_DESC, i18n::tr("Descending")).check(s.sort_desc));
        v
    };
    // v0.9.63 (B-R5-2): the row says which way the command will actually go, and since B-R5-2 the
    // command leaves a fullscreen NOBODY IN FALCON OWNS too — a green-button / ⌃⌘F / Spaces one. The
    // title used to read `immersive` alone, so inside a green-button fullscreen it said "Enter Full
    // Screen" over a window that was already fullscreen. Under the old F semantics that was merely
    // odd; under the new ones it would have been a row promising the opposite of what it does.
    let fs_title = if s.immersive || s.os_fullscreen {
        i18n::tr("Exit Full Screen")
    } else {
        i18n::tr("Enter Full Screen")
    };
    let view = vec![
        ItemDef::act(TAG_COMPARE, hint(i18n::tr("Compare A|B"), &k.compare)).check(s.compare),
        ItemDef::act(TAG_SWAP, hint(i18n::tr("Swap"), &k.cmpswap)),
        ItemDef::act(TAG_PIN, hint(i18n::tr("Pin"), &k.cmppin)),
        ItemDef::sep(),
        ItemDef::act(TAG_ZOOM_11, i18n::tr("Zoom 1:1")),
        ItemDef::act(TAG_ZOOM_FIT, hint(i18n::tr("Zoom to Fit"), &k.zoom)),
        ItemDef::sep(),
        ItemDef::act(TAG_GRID, i18n::tr("Photo Grid")).check(s.grid_open),
        ItemDef::act(TAG_FILM, i18n::tr("Filmstrip")).check(s.film_visible),
        // v0.9.24 (owner ruling): the info panel joins its sibling panel-visibility toggles — the
        // group directly above the separator and the fullscreen row, which is where macOS puts
        // view-furniture commands. NOT a checkbox: three states cannot be told by one checkmark, so
        // the row carries a STATE-DERIVED title instead (ledger L20), the VERBATIM strings the in-app
        // right-click menu shows, plus the live bare-key suffix in the "(P)" grammar — display-only,
        // like every other bare key here (only ⌘-chords register as real equivalents). The suffix is
        // suppressed in the OFF state, mirroring the context menu: there the key reveals the
        // transient stub and takes a second press, so advertising it beside "Show info panel" would
        // promise a one-press action the key does not perform.
        ItemDef::act(TAG_INFO_PANEL, {
            let t = crate::support::info_menu_title(s.info_open, s.info_min, s.raw_open);
            if crate::support::info_menu_hints_key(s.info_open) {
                hint(t, &k.info)
            } else {
                t.to_string()
            }
        }),
        ItemDef::sep(),
        ItemDef::act(TAG_FULLSCREEN, fs_title).key("f").ctrl(),
        ItemDef::sep(),
        ItemDef::parent(TAG_SORT_PARENT, i18n::tr("Sort"), sort_items),
    ];

    // Help — registered as NSApp.helpMenu (the system search field comes free). Opens Settings
    // (the shortcuts sections live there; the panel has no scroll-anchor machinery, so anchoring
    // was judged disproportionate — reported).
    // v0.8.119 (design-sweep Y58): the row's ellipsis promised a dedicated shortcuts surface;
    // the dispatch opens Settings at the top of an eight-section panel and always did (the code
    // comment conceded it, and MAC_TESTER_DELTA_v6 item 26 tells the field tester to EXPECT it).
    // Anchoring is not implementable without new machinery — the CONTROLS section's y lives inside
    // the Settings panel's mount-gated `if`, which nothing outside can read, and the panel does not
    // exist yet when the command runs — so the label states what actually happens (L17: a label is
    // a contract). Its in-app twin, the welcome panel's shortcuts link, took the same route.
    // + "Show log file": that command existed ONLY in the photo's right-click menu, and Help is
    // where a Mac user (and a field tester chasing a repro) looks for it. Same dispatch.
    let help = vec![
        ItemDef::act(TAG_HELP_SHORTCUTS, i18n::tr("Shortcuts in Settings…")),
        ItemDef::act(TAG_HELP_LOG, i18n::tr("Show log file")),
    ];

    vec![
        (TopMenu::File, file),
        (TopMenu::Edit, edit),
        (TopMenu::Photo, photo),
        (TopMenu::View, view),
        (TopMenu::Help, help),
    ]
}

/// The ONE enablement predicate — read by validateMenuItem: (autoenabled menus: Edit/Photo/View/
/// Help + the app-menu inserts) AND by the File menu's explicit setEnabled refresh. Mirrors the
/// in-app gates: `modal` is the same modal-open predicate every FocusScope key arm checks; the
/// photo-dependent items additionally require a loaded folder (L15 honesty).
pub(crate) fn enabled_for(tag: i32, s: &MenuSnapshot) -> bool {
    let photo_ok = s.has_photo && !s.modal;
    match tag {
        // v0.8.121 (Round-B fix F5 = audit A7): the two rows that RAISE A PANEL mirror the in-app
        // O8/O9 gate. Quit and the two Help rows stay unconditional: Quit must remain reachable
        // (it is the one command a user reaches for when a dialog has them stuck), and the Help
        // rows open no panel of their own.
        // v0.9.59 (OWNER RULING 08-02): ⌘Q is now IMMEDIATE — its handler quits the loop outright,
        // with no deferral, refusal or reminder — so `true` here is the exact L21 mirror of a
        // handler carrying no gate, not a deliberate divergence from one.
        TAG_SETTINGS | TAG_WELCOME => !s.modal_blocking,
        TAG_QUIT | TAG_HELP_SHORTCUTS | TAG_HELP_LOG => true,
        TAG_OPEN_IMAGE | TAG_OPEN_FOLDER | TAG_CLOSE => true,
        TAG_RECENT_PARENT => !s.recents.is_empty(),
        TAG_RECENT_EMPTY => false,
        t if t >= TAG_RECENT_BASE => ((t - TAG_RECENT_BASE) as usize) < s.recents.len(),
        TAG_REVEAL | TAG_COPY => photo_ok,
        TAG_UNDO => s.undo != UndoKind::None && !s.modal,
        t if (TAG_RATE1..=TAG_RATE5).contains(&t) => photo_ok,
        TAG_RATE0 | TAG_FLAG | TAG_REJECT | TAG_UNMARK => photo_ok,
        TAG_ROT_LEFT | TAG_ROT_RIGHT | TAG_XMP => photo_ok,
        // L15: Trash's dispatch (invoke_delete_key → delete_key_allowed, main.rs) is compare-gated
        // (A|B has two candidates), so an enabled item in compare is a silent no-op. Mirror that gate
        // here; text_focus is unreachable from a menu click and modal is folded into photo_ok.
        TAG_TRASH => photo_ok && !s.compare,
        TAG_COMPARE => photo_ok && s.count >= 2,
        TAG_SWAP | TAG_PIN => s.compare && !s.modal,
        // L17 zoom-pair enablement honesty. Single view: "Zoom 1:1" dispatches the on_photo_clicked
        // TOGGLE (Fit→1:1, but ANY zoom→Fit), so it tells the truth ONLY from Fit; "Zoom to Fit"
        // (on_pan_reset) tells the truth only when there IS a zoom to reset. Mutually exclusive → the
        // pair can never duplicate. Compare: "Zoom 1:1" is invoke_compare_one_to_one (one-way, always
        // honest) and "Zoom to Fit" is on_pan_reset, which resets the shared zoom AND recentres the
        // compare pan (cmp_pan_fx/fy) — real work — so both stay live.
        TAG_ZOOM_11 => (photo_ok || s.inspect_open) && (s.compare || s.at_fit),
        TAG_ZOOM_FIT => (photo_ok || s.inspect_open) && (s.compare || !s.at_fit),
        // v0.9.24 (L21): the info row's dispatch is the tick arm that invokes the SHARED
        // `toggle-info-panel` callback (main.rs). Deliberately NOT compare-gated: the callback works
        // in compare (the expanded panel mounts over the A/B split), and the compare-mode recovery
        // ruling depends on this row staying live there (pinned by test) alongside the context menu
        // — v0.9.26 (v0.9.25-audit V7): reached in compare from a FILMSTRIP thumbnail / grid-dock
        // tile right-click, not from the photo (the stage right-click handler is inside
        // `if !root.compare : TouchArea`). Same menu, same row, a surface that exists there.
        // v0.9.25 (v0.8.93-audit C21): + `!s.immersive`, because the SHARED CALLBACK ITSELF is now a
        // no-op there (main_window.slint `toggle-info-panel`) — neither info surface can mount in
        // immersive, so the row's state-derived imperative ("Show info panel") promised a visible
        // change nothing could deliver, and a second click left the user on the stub after exit.
        // This is L21 BY CONSTRUCTION, not a divergence: enablement mirrors the body's own gate, the
        // same way it mirrors `!modal && !empty`. Grid/Filmstrip keep `photo_ok` — their bodies
        // carry no immersive gate, and they are checkbox rows whose promise is weaker; the general
        // question ("what should a view-furniture command do when its target cannot mount in the
        // current display mode?") is a ledger CLASS item, not this row's business.
        TAG_INFO_PANEL => photo_ok && !s.immersive,
        // v0.8.119 (design-sweep Y49): …and so do these two, for the same reason. Both surfaces
        // are immersive-gated in the UI (main_window.slint mount gates carry `!root.immersive`), so
        // in full screen the rows flipped a CHECKMARK for a surface that cannot mount, nothing
        // happened on screen, and the state was waiting to surprise the user on the way out. The
        // L26 pattern (a gated body with an ungated invoker) on the two rows the info-panel fix
        // left behind.
        TAG_GRID | TAG_FILM => photo_ok && !s.immersive,
        // v0.9.67 (OWNER RULING 08-08 (ii)): the ONE row that reads `modal_fs` — the welcome screen
        // stops disabling Enter Full Screen, and nothing else about the modal gate moves.
        TAG_FULLSCREEN => !s.modal_fs,
        // Fix round 1 (1 November): sorting also waits while Apply rotations runs (`apply_pick`'s guard).
        TAG_SORT_PARENT => photo_ok && !s.opening_photo && !s.applying_rotations,
        t if (TAG_SORT_M0..=TAG_SORT_M6).contains(&t) => photo_ok && !s.opening_photo && !s.applying_rotations,
        TAG_SORT_ASC | TAG_SORT_DESC => photo_ok && !s.opening_photo && !s.applying_rotations,
        _ => true, // unknown/structural tags never block (defensive)
    }
}

// ───────── v0.9.59 (round-4 item S1 / findings §A): the GRAFT-OWNERSHIP decisions ─────────
// The whole failure: `try_install` ran ONCE, gated only on `NSApp.mainMenu != nil`, and never
// re-read it. muda installs its default bar at the FIRST WINDOW ACTIVATION and re-installs on
// window re-creation, so Falcon grafted seven menus into an NSMenu macOS had already discarded —
// and logged "menubar: installed (7 menus; graft=ok)" over an empty bar (ledger L30). These two
// decisions are the fix's decidable core: WHICH menu is the real one, and WHETHER ours is still in
// it. Pure so they are unit-tested on Windows; the objc plumbing around them is compile-gated and
// field-proven only.

/// v0.9.59 (S1 ruling 2): the POSITIVE fingerprint for a graft target. muda's app submenu ALWAYS
/// carries a Quit item with key equivalent "q" (`muda/src/items/predefined.rs` →
/// `initWithTitle:action:keyEquivalent:`), so a first item whose submenu has no "q" is not the bar
/// we mean to graft into — keep polling rather than append into a stranger. Non-nil is not identity.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // runtime caller is mac_menu::try_install
pub(crate) fn app_submenu_fingerprint_ok(key_equivalents: &[String]) -> bool {
    key_equivalents.iter().any(|k| k == "q")
}

/// What the tick must do about the graft this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraftState {
    /// Nothing installed yet — try the first graft.
    Install,
    /// Our menus live in the menu AppKit is showing — do nothing.
    Healthy,
    /// `NSApp.mainMenu` is a DIFFERENT object than the one we grafted into: muda replaced the bar
    /// (activation / window re-creation) and everything we appended is orphaned. Tear down + re-graft.
    Regraft,
}

/// v0.9.59 (S1 ruling 1): the per-tick ownership re-check, on POINTER IDENTITY — muda re-asserts
/// the SAME NSMenu object on every activation, so pointer equality is stable and cannot thrash,
/// while content equality would compare a menu we mutate ourselves. `grafted` is the pointer the
/// live graft recorded (`None` = nothing installed); `live` is `NSApp.mainMenu` as a bare address
/// (0 = nil). A nil live menu while we hold a graft is also a mismatch — the bar we own is gone.
/// Pure → unit-tested on any platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // runtime caller is mac_menu::tick_pump
pub(crate) fn graft_state(grafted: Option<usize>, live: usize) -> GraftState {
    match grafted {
        None => GraftState::Install,
        Some(p) if p == live && live != 0 => GraftState::Healthy,
        Some(_) => GraftState::Regraft,
    }
}

/// Ticks of full-rate graft polling (~10 s at the 16 ms tick) before it drops to `GRAFT_POLL_SLOW`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const GRAFT_POLL_FULL_TICKS: u32 = 600;
/// …and the slow rate after that: one attempt every N ticks (~2 Hz).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const GRAFT_POLL_SLOW: u32 = 30;

/// v0.9.59 (S1 ruling 2, energy): with the fingerprint gate a graft can now be REFUSED forever (a
/// backend that never builds muda's bar), and the old code's "install once, then never look again"
/// meant nobody had to think about the cost of waiting. Full rate through the ~10 s diagnostic
/// window (so a normal boot grafts on the very tick the bar appears), ~2 Hz after it.
/// Pure → unit-tested on any platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // runtime caller is mac_menu::tick_pump
pub(crate) fn should_attempt_graft(wait_ticks: u32) -> bool {
    wait_ticks < GRAFT_POLL_FULL_TICKS || wait_ticks % GRAFT_POLL_SLOW == 0
}

/// Change fingerprint for the tick's re-title/re-check refresh (the snapshot hashes cheaply; the
/// mac side refreshes NSMenuItem state only when this moves — plus unconditionally on menu open).
pub(crate) fn fingerprint(s: &MenuSnapshot) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ── v0.9.60 (W2-1 / findings "ENERGY EVIDENCE" item 2a): DON'T BUILD WHAT NOTHING READS ─────────
//
// The tick assembled a fresh [`MenuSnapshot`] every 16 ms — ~20 `support::menu_shortcut` String
// lookups plus a `recents` Vec<String> clone, cloned AGAIN into `mac_menu`'s mutex: ≈50 heap
// allocations 62.5×/s, forever, on a machine with no folder open. Only the AppKit refresh was
// fingerprint-gated; the allocation that FED the fingerprint was not. That is the best single
// explanation the round-4 evidence has for "energy impact 2.2 at No folder open".
//
// The gate below is the same hash, computed from inputs that allocate NOTHING:
//   * the snapshot's SCALAR state — hashed by handing this fn a `MenuSnapshot` whose two
//     collection fields are still empty (`String::new()` and `Vec::new()` never touch the heap),
//     which is why the caller builds the scalars FIRST and fills `keys`/`recents` only on a miss.
//     Rust's struct-literal exhaustiveness is what keeps that probe honest: a new field on
//     `MenuSnapshot` fails to compile at the tick's construction site until it is written there,
//     so a scalar can never quietly fall outside the gate.
//   * `keys_h` / `recents_h` — [`hash_keymap`] and [`hash_recents`] over the SOURCES the two
//     collections are derived from (the raw keybind map; the LRU's top-N paths), by reference.
//
// EQUAL FINGERPRINT ⇒ EQUAL SNAPSHOT, so a skipped tick leaves `mac_menu`'s stored snapshot
// CURRENT, not stale — `validateMenuItem:` reads exactly what it would have read. That is the
// property the caller's belt-and-braces [`MENU_REBUILD_FLOOR_MS`] rebuild backstops: if some
// future field ever escapes the probe, it goes stale for ≤ 250 ms, not forever.
pub(crate) fn tick_fingerprint(state: &MenuSnapshot, keys_h: u64, recents_h: u64) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    state.hash(&mut h);
    keys_h.hash(&mut h);
    recents_h.hash(&mut h);
    h.finish()
}

/// The belt-and-braces floor: rebuild the snapshot at least this often even when the gate says
/// nothing moved (≈4 Hz — the spec's ceiling for this item). 62.5 → 4 rebuilds/s at idle.
pub(crate) const MENU_REBUILD_FLOOR_MS: u64 = 250;

/// v0.9.61 (J3): THE EXHAUSTIVENESS WITNESS, IN SHARED CODE.
///
/// The gate's safety argument is "equal fingerprint ⇒ equal snapshot", and it holds only while
/// every field the menu renders from is inside the cheap probe. What keeps that true is Rust's
/// struct-literal exhaustiveness: a literal with no `..Default::default()` fails to compile until a
/// newly-added field is written into it. v0.9.60 put that literal at the tick's snapshot site in
/// `main.rs` — inside `#[cfg(target_os = "macos")]` — so on the Windows host that runs `cargo test`
/// it is not compiled at all, and the test's claim that "the same deletion is ALSO a compile error
/// there" was true only for whoever ran the aarch64 check.
///
/// This literal lives in a file every host compiles. Add a field to [`MenuSnapshot`] and THIS fails
/// to build under a plain `cargo build`, before any platform check is reached. Every scalar is set
/// to a NON-default value so the fingerprint rows below can tell it from `Default`, and the two
/// COLLECTION fields carry the exact placeholders the tick uses — `Vec::new()` and
/// `MenuKeys::default()`, neither of which touches the heap, which is what makes the tick's probe
/// allocation-free in the first place.
///
/// HONEST SCOPE (the claim v0.9.60 overstated): this guarantees a new field is NOTICED on every
/// host. It does not guarantee the tick WRITES it — that is the mac-side literal's own
/// exhaustiveness, and it is checked by `cargo check --target aarch64-apple-darwin`, which is the
/// only compiler this repo has for that arm.
// The "never used" is the POINT: nothing calls this in a shipping build, and it must still be
// COMPILED in one, because compilation is the whole guarantee. (The `#![cfg_attr(not(macos),
// allow(dead_code))]` at the top of this file already covers the Windows build; macOS needs it said
// here, since that file-level allow does not apply there.)
#[allow(dead_code)]
pub(crate) fn probe_literal() -> MenuSnapshot {
    MenuSnapshot {
        has_photo: true,
        count: 5,
        modal: true,
        modal_fs: true, // v0.9.67 (OWNER RULING 08-08 (ii))
        opening_photo: true,
        applying_rotations: true, // fix round 1
        inspect_open: true,
        context: Some(1),
        modal_blocking: true,
        compare: true,
        at_fit: true,
        immersive: true,
        os_fullscreen: true, // v0.9.63 (B-R5-2)
        grid_open: true,
        film_visible: true,
        info_open: true,
        info_min: true,
        raw_open: true,
        sort_method: 3,
        sort_desc: true,
        xmp_sync: true,
        rating: 4,
        flagged: true,
        rejected: true,
        // v0.9.64: the selection's four scalars. `sel_rating` is 2 rather than 4 so it is
        // distinguishable from `rating` as well as from Default, and `sel_count` is 3 rather than
        // `count`'s 5 for the same reason — a probe whose fields collide cannot witness a swap.
        sel_count: 3,
        sel_flag_sets: true,
        sel_reject_sets: true,
        sel_rating: 2,
        undo: UndoKind::Delete,
        // The placeholders, asserted explicitly by `the_shared_probe_literal_is_exhaustive_and_empty`.
        recents: Vec::new(),
        keys: MenuKeys::default(),
    }
}

/// Order-independent hash of the RAW keybind map — the source `MenuKeys`' twenty
/// `support::menu_shortcut` strings are formatted from. Allocation-free (hashes borrowed keys and
/// values in place) and order-independent, because `HashMap` iteration order is not a contract:
/// per-entry hashes are combined with `wrapping_add`, so the same map always answers the same u64
/// however it enumerates. A rebind anywhere in the map costs ONE spurious snapshot rebuild — the
/// conservative direction, and rebinding is a once-in-a-session act.
pub(crate) fn hash_keymap(km: &std::collections::HashMap<String, String>) -> u64 {
    let mut acc: u64 = 0;
    for (k, v) in km {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        k.hash(&mut h);
        v.hash(&mut h);
        acc = acc.wrapping_add(h.finish());
    }
    acc
}

/// ORDER-DEPENDENT hash of the recents the snapshot would carry — the caller passes the identical
/// `rev().take(RECENTS_MAX)` walk the snapshot's `recents` field is built from, as `&str`, so no
/// String is cloned to decide whether any String needs cloning. Order matters here (the menu shows
/// the list in this order, and an LRU re-order IS a change the bar must render).
pub(crate) fn hash_recents<'a>(paths: impl Iterator<Item = &'a str>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut n: usize = 0;
    for p in paths {
        p.hash(&mut h);
        n += 1;
    }
    // Belt and braces: `str`'s own Hash impl is length-delimited (`write_str` terminates each
    // string), so a truncated list already hashes differently — this makes the length an EXPLICIT
    // part of the identity rather than a property of someone else's impl.
    n.hash(&mut h);
    h.finish()
}

/// Greppable command name for the one-per-action log line.
pub(crate) fn tag_name(tag: i32) -> &'static str {
    match tag {
        TAG_SETTINGS => "settings",
        TAG_WELCOME => "welcome",
        TAG_QUIT => "quit",
        TAG_OPEN_IMAGE => "open-image",
        TAG_OPEN_FOLDER => "open-folder",
        TAG_REVEAL => "reveal",
        TAG_CLOSE => "close",
        TAG_UNDO => "undo",
        TAG_COPY => "copy",
        t if (TAG_RATE1..=TAG_RATE5).contains(&t) => "rate",
        TAG_RATE0 => "rate0",
        TAG_FLAG => "flag",
        TAG_REJECT => "reject",
        TAG_UNMARK => "unmark",
        TAG_ROT_LEFT => "rotate-left",
        TAG_ROT_RIGHT => "rotate-right",
        TAG_TRASH => "trash",
        TAG_XMP => "xmp-sync",
        TAG_COMPARE => "compare",
        TAG_SWAP => "swap",
        TAG_PIN => "pin",
        TAG_ZOOM_11 => "zoom-1to1",
        TAG_ZOOM_FIT => "zoom-fit",
        TAG_GRID => "grid",
        TAG_FILM => "filmstrip",
        TAG_INFO_PANEL => "info-panel",
        TAG_FULLSCREEN => "fullscreen",
        t if (TAG_SORT_M0..=TAG_SORT_M6).contains(&t) => "sort-method",
        TAG_SORT_ASC => "sort-asc",
        TAG_SORT_DESC => "sort-desc",
        TAG_HELP_SHORTCUTS => "help-shortcuts",
        TAG_HELP_LOG => "help-log",
        t if t >= TAG_RECENT_BASE => "open-recent",
        _ => "unknown",
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn keys_from(map: &std::collections::HashMap<String, String>) -> MenuKeys {
        // The same single-source formatter main.rs uses to assemble the snapshot.
        let g = |id: &str| crate::support::menu_shortcut(map, id);
        MenuKeys {
            rate: [g("rate1"), g("rate2"), g("rate3"), g("rate4"), g("rate5")],
            rate0: g("rate0"),
            flag: g("flag"),
            reject: g("reject"),
            unflag: g("unflag"),
            rotcw: g("rotcw"),
            rotccw: g("rotccw"),
            delete: g("delete"),
            compare: g("compare"),
            cmpswap: g("cmpswap"),
            cmppin: g("cmppin"),
            zoom: g("zoom"),
            full: g("full"),
            info: g("info"),
        }
    }

    fn default_keymap() -> std::collections::HashMap<String, String> {
        // Seed exactly like boot: ACTIONS + BASIC_ACTIONS defaults.
        let mut km = std::collections::HashMap::new();
        for (id, _, d) in crate::ACTIONS.iter().chain(crate::BASIC_ACTIONS) {
            km.insert(id.to_string(), d.to_string());
        }
        km
    }

    fn snap_with(km: &std::collections::HashMap<String, String>) -> MenuSnapshot {
        // v0.8.97: `raw_open` is spelled out rather than left to `Default` (which is `false` for a
        // bool) — the develop panel's real default is VISIBLE, and the info row's OFF title reads
        // this field, so a false default here would silently test the legacy pair everywhere.
        MenuSnapshot {
            has_photo: true,
            count: 5,
            raw_open: true,
            keys: keys_from(km),
            ..Default::default()
        }
    }

    fn walk<'a>(items: &'a [ItemDef], out: &mut Vec<&'a ItemDef>) {
        for it in items {
            out.push(it);
            if let Some(sub) = &it.submenu {
                walk(sub, out);
            }
        }
    }
    fn all_items(s: &MenuSnapshot) -> Vec<(&'static str, Vec<ItemDef>)> {
        let mut menus = build_menus(s);
        menus.push(("App", app_menu_inserts()));
        menus
    }

    // ── v0.9.60 (W2-1): the cheap tick gate ──────────────────────────────────────────────────

    /// The gate's whole contract: the ALLOCATION-FREE probe (scalars + two source hashes) moves
    /// whenever the snapshot the tick would have built moves. Each row here is one field of the
    /// snapshot the menu renders from — if any of them failed to shift the fingerprint, the tick
    /// would skip a rebuild and `validateMenuItem:` would read stale state.
    /// FALSIFIER: delete any field from the probe literal built at main.rs's snapshot site and the
    /// matching row here reddens — but the same deletion is ALSO a compile error there, because
    /// that literal lists every field with no `..Default::default()`. Belt and braces.
    #[test]
    fn the_tick_fingerprint_moves_with_every_field_the_menu_renders() {
        let base = MenuSnapshot::default();
        let fp = |s: &MenuSnapshot| tick_fingerprint(s, 0, 0);
        let base_fp = fp(&base);
        // A helper per field: mutate one thing, assert the fingerprint moved.
        let cases: Vec<(&str, MenuSnapshot)> = vec![
            ("has_photo", MenuSnapshot { has_photo: true, ..Default::default() }),
            ("count", MenuSnapshot { count: 2, ..Default::default() }),
            ("modal", MenuSnapshot { modal: true, ..Default::default() }),
            // v0.9.67: the fullscreen row's own gate. Without it in the fingerprint, closing the
            // welcome guide would leave "Enter Full Screen" disabled until some other field moved.
            ("modal_fs", MenuSnapshot { modal_fs: true, ..Default::default() }),
            ("opening_photo", MenuSnapshot { opening_photo: true, ..Default::default() }),
            ("applying_rotations", MenuSnapshot { applying_rotations: true, ..Default::default() }), // fix round 1
            ("inspect_open", MenuSnapshot { inspect_open: true, ..Default::default() }),
            ("context", MenuSnapshot { context: Some(1), ..Default::default() }),
            ("modal_blocking", MenuSnapshot { modal_blocking: true, ..Default::default() }),
            ("compare", MenuSnapshot { compare: true, ..Default::default() }),
            ("at_fit", MenuSnapshot { at_fit: true, ..Default::default() }),
            ("immersive", MenuSnapshot { immersive: true, ..Default::default() }),
            ("os_fullscreen", MenuSnapshot { os_fullscreen: true, ..Default::default() }),
            ("grid_open", MenuSnapshot { grid_open: true, ..Default::default() }),
            ("film_visible", MenuSnapshot { film_visible: true, ..Default::default() }),
            ("info_open", MenuSnapshot { info_open: true, ..Default::default() }),
            ("info_min", MenuSnapshot { info_min: true, ..Default::default() }),
            ("raw_open", MenuSnapshot { raw_open: true, ..Default::default() }),
            ("sort_method", MenuSnapshot { sort_method: 3, ..Default::default() }),
            ("sort_desc", MenuSnapshot { sort_desc: true, ..Default::default() }),
            ("xmp_sync", MenuSnapshot { xmp_sync: true, ..Default::default() }),
            ("rating", MenuSnapshot { rating: 4, ..Default::default() }),
            ("flagged", MenuSnapshot { flagged: true, ..Default::default() }),
            ("rejected", MenuSnapshot { rejected: true, ..Default::default() }),
            // v0.9.64: the selection's four scalars. They title five rows, so a fingerprint that
            // did not move with them would leave the bar saying "Flag 12 Photos" over a selection
            // the user has since cleared — the staleness class this gate exists to prevent.
            ("sel_count", MenuSnapshot { sel_count: 12, ..Default::default() }),
            ("sel_flag_sets", MenuSnapshot { sel_flag_sets: true, ..Default::default() }),
            ("sel_reject_sets", MenuSnapshot { sel_reject_sets: true, ..Default::default() }),
            ("sel_rating", MenuSnapshot { sel_rating: 3, ..Default::default() }),
            ("undo", MenuSnapshot { undo: UndoKind::Delete, ..Default::default() }),
        ];
        for (field, s) in cases {
            assert_ne!(fp(&s), base_fp, "changing `{field}` must move the tick fingerprint");
        }
        // …and the two COLLECTION fields, which the probe carries as hashes rather than values.
        assert_ne!(tick_fingerprint(&base, 1, 0), base_fp, "a keymap change must move it");
        assert_ne!(tick_fingerprint(&base, 0, 1), base_fp, "a recents change must move it");
        // Same inputs twice = same answer (the gate must not thrash on a stable app).
        assert_eq!(fp(&base), base_fp);
    }

    /// The keymap hash: order-independent (HashMap iteration order is not a contract) but
    /// sensitive to any rebind — the twenty display strings in `MenuKeys` are formatted from it.
    /// FALSIFIER: hash the values only (drop `k.hash`) and the swapped-binding row reddens; make
    /// it order-DEPENDENT (feed one hasher in iteration order) and the reinsertion row reddens.
    #[test]
    fn hash_keymap_ignores_order_and_notices_rebinds() {
        let mut a = std::collections::HashMap::new();
        a.insert("rate1".to_string(), "1".to_string());
        a.insert("flag".to_string(), "p".to_string());
        a.insert("reject".to_string(), "x".to_string());
        // Same pairs, inserted in a different order → the same map → the same hash.
        let mut b = std::collections::HashMap::new();
        b.insert("reject".to_string(), "x".to_string());
        b.insert("rate1".to_string(), "1".to_string());
        b.insert("flag".to_string(), "p".to_string());
        assert_eq!(hash_keymap(&a), hash_keymap(&b));
        // A rebind moves it…
        let mut c = a.clone();
        c.insert("flag".to_string(), "f".to_string());
        assert_ne!(hash_keymap(&a), hash_keymap(&c));
        // …and so does SWAPPING two bindings, which a value-only hash would miss entirely.
        let mut d = a.clone();
        d.insert("flag".to_string(), "x".to_string());
        d.insert("reject".to_string(), "p".to_string());
        assert_ne!(hash_keymap(&a), hash_keymap(&d));
        // Adding a binding moves it (the conservative direction: one spurious rebuild).
        let mut e = a.clone();
        e.insert("zoom".to_string(), "z".to_string());
        assert_ne!(hash_keymap(&a), hash_keymap(&e));
    }

    /// The recents hash: ORDER matters (the menu renders the list in it, and an LRU re-order is a
    /// change the bar must show) and so does length.
    /// FALSIFIER: give this fn `hash_keymap`'s order-INDEPENDENT shape (per-path hashers combined
    /// with `wrapping_add`) and the re-order row reddens — the two hashes are deliberately
    /// different in exactly this way. (Note the length term is NOT the falsifier: `str`'s Hash impl
    /// is already length-delimited, so the truncation row holds without it — it is belt and braces,
    /// and the code says so.)
    #[test]
    fn hash_recents_tracks_order_and_length() {
        let a = ["/a/one", "/b/two", "/c/three"];
        assert_eq!(hash_recents(a.iter().copied()), hash_recents(a.iter().copied()));
        let reordered = ["/b/two", "/a/one", "/c/three"];
        assert_ne!(hash_recents(a.iter().copied()), hash_recents(reordered.iter().copied()));
        assert_ne!(hash_recents(a.iter().copied()), hash_recents(a.iter().copied().take(2)));
        assert_ne!(hash_recents(a.iter().copied()), hash_recents(std::iter::empty()));
    }

    /// v0.9.61 (J3): the shared exhaustiveness witness — see [`probe_literal`]. The COMPILE guard is
    /// the literal itself (add a field to `MenuSnapshot` and that fn stops building on every host,
    /// not only under the aarch64 check); this row pins the two PLACEHOLDERS explicitly, because
    /// "the collections are empty" is the property that makes the tick's probe allocation-free and
    /// makes the scalar hash mean what the gate says it means.
    /// FALSIFIER: fill either placeholder in `probe_literal` (e.g. `recents: vec!["/a".into()]`) and
    /// the matching assert below reddens; add `..Default::default()` to that literal and this test
    /// still passes — which is exactly why the compile guard, not this test, is the real witness,
    /// and why the doc above says so instead of claiming coverage it does not have.
    #[test]
    fn the_shared_probe_literal_is_exhaustive_and_empty() {
        let p = probe_literal();
        assert!(p.recents.is_empty(), "the recents placeholder must be the empty Vec the tick uses");
        assert!(p.keys == MenuKeys::default(), "the keys placeholder must be the default MenuKeys");
        // Every scalar differs from Default, so the literal is distinguishable from an empty one —
        // which is what lets the per-field rows above mean something when read together with it.
        assert_ne!(fingerprint(&p), fingerprint(&MenuSnapshot::default()));
    }

    /// THE gate's safety property, stated as a test: equal fingerprint ⇒ equal snapshot, which is
    /// what makes a skipped tick leave a CURRENT snapshot in `mac_menu`'s mutex rather than a
    /// stale one. Built from the real producers (`keys_from` = the same `menu_shortcut` formatter
    /// main.rs uses), so a keymap that hashes equal really does format equal.
    ///
    /// v0.9.61 (J1): THE TAUTOLOGY IS GONE. This test used to open with
    /// `assert_eq!(probe(&km, &recents), probe(&km, &recents))` and
    /// `assert_eq!(fingerprint(&full(&km)), fingerprint(&full(&km)))` — `f(x) == f(x)` over a pure
    /// fn, which holds for EVERY implementation including `fn hash(_) -> 0`, and which no edit to
    /// the code under test can redden. What the gate actually promises is DIFFERENTIAL: for a pair
    /// of states, the cheap probe agrees iff the real snapshot agrees. Each row below is such a
    /// pair, and each names which side of the iff it is testing.
    /// FALSIFIERS (assert granularity): make `hash_keymap` return a constant → the REBIND row's
    /// `assert_ne!(probe(&km, R), probe(&km2, R))` reddens; make `hash_recents` order-independent →
    /// the REORDER row reddens; drop `keys_h` from `tick_fingerprint` → the rebind row reddens;
    /// drop `recents_h` → the reorder and truncation rows redden.
    #[test]
    fn an_unmoved_fingerprint_means_an_unchanged_snapshot() {
        let km = default_keymap();
        let probe = |km: &std::collections::HashMap<String, String>, recents: &[&str]| {
            // exactly the tick's shape: scalars + EMPTY collections, hashed with the two sources
            let scalars = MenuSnapshot { has_photo: true, count: 5, raw_open: true, ..Default::default() };
            tick_fingerprint(&scalars, hash_keymap(km), hash_recents(recents.iter().copied()))
        };
        let full = |km: &std::collections::HashMap<String, String>, recents: &[&str]| MenuSnapshot {
            has_photo: true,
            count: 5,
            raw_open: true,
            keys: keys_from(km),
            recents: recents.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        // Each row is a PAIR of states plus the answer both sides must give about it.
        let base_r: &[&str] = &["/a", "/b"];
        let mut rebound = km.clone();
        rebound.insert("flag".to_string(), "F9".to_string());
        let mut renamed_only = km.clone();
        // A key the MENU does not render (`menu_shortcut` is never asked for it) — the snapshot is
        // unchanged, so BOTH sides must hold still. This is the direction a tautology cannot test:
        // it is where an over-eager probe would rebuild 62 times a second for nothing.
        renamed_only.insert("falcon-not-a-menu-action".to_string(), "ctrl+q".to_string());

        let rows: [(&str, &std::collections::HashMap<String, String>, &[&str], bool); 4] = [
            // (what changed, keymap, recents, must the snapshot differ?)
            ("nothing at all", &km, base_r, false),
            ("a rendered rebind", &rebound, base_r, true),
            ("the recents ORDER", &km, &["/b", "/a"], true),
            ("the recents LENGTH", &km, &["/a"], true),
        ];
        for (what, km2, r2, must_differ) in rows {
            let probe_moved = probe(km2, r2) != probe(&km, base_r);
            let snapshot_moved = fingerprint(&full(km2, r2)) != fingerprint(&full(&km, base_r));
            assert_eq!(
                snapshot_moved, must_differ,
                "the REAL snapshot's answer about `{what}` is wrong — the row's premise is broken"
            );
            assert_eq!(
                probe_moved, snapshot_moved,
                "the cheap probe and the real snapshot disagree about `{what}` — the gate's whole \
                 safety argument is that they cannot"
            );
        }
        // The one direction that is NOT an iff, stated so nobody reads more into the rows above than
        // is there: an unrendered keymap entry moves the probe (hash_keymap hashes the whole map)
        // while the snapshot holds still. That is a spurious REBUILD, which is safe — the conservative
        // side — and the code says so; a probe that missed a real change would be the unsafe side,
        // and that is what the four rows above rule out.
        assert!(
            probe(&renamed_only, base_r) != probe(&km, base_r),
            "an unrendered rebind still moves the probe (one wasted rebuild — the safe direction)"
        );
        assert_eq!(
            fingerprint(&full(&renamed_only, base_r)),
            fingerprint(&full(&km, base_r)),
            "…while the snapshot it stands for is genuinely unchanged"
        );
    }

    /// THE ruled invariant: every LIVE key equivalent in the model is a ⌘-chord from the ruling's
    /// exact set; bare cull keys never register (they would be intercepted before text fields).
    #[test]
    fn live_equivalents_are_cmd_chords_only() {
        let km = default_keymap();
        let s = snap_with(&km);
        for (_, items) in all_items(&s) {
            let mut flat = Vec::new();
            walk(&items, &mut flat);
            for it in &flat {
                if !it.key.is_empty() {
                    // (key, needs-ctrl) — the ruled live set reachable from this model.
                    let allowed =
                        [(",", false), ("o", false), ("w", false), ("z", false), ("c", false), ("f", true)];
                    assert!(
                        allowed.contains(&(it.key, it.ctrl)),
                        "unruled live equivalent {:?} (ctrl={}) on {:?}",
                        it.key,
                        it.ctrl,
                        it.title
                    );
                    // Live equivalents are single lowercase chars — AppKit ⌘-chords (an uppercase
                    // char would implicitly demand ⇧).
                    assert!(it.key.len() == 1 && !it.key.chars().next().unwrap().is_uppercase());
                }
            }
        }
    }

    /// Titles render the LIVE keymap: default "p" → "Flag (P)"; a rebind to "g" retitles; an
    /// unbound action drops the hint (never a stale or hardcoded key).
    #[test]
    fn titles_follow_live_keymap() {
        let mut km = default_keymap();
        let s = snap_with(&km);
        let photo = &build_menus(&s)[2].1;
        assert!(photo.iter().any(|i| i.title == "Flag (P)"), "default flag hint");
        assert!(photo.iter().any(|i| i.title == "Rate ★★★ (3)"), "default rate3 hint");
        km.insert("flag".into(), "g".into());
        km.insert("rate3".into(), "".into());
        let s2 = snap_with(&km);
        let photo2 = &build_menus(&s2)[2].1;
        assert!(photo2.iter().any(|i| i.title == "Flag (G)"), "rebound flag hint");
        assert!(photo2.iter().any(|i| i.title == "Rate ★★★"), "unbound → no hint");
        // Shift-carrying binding renders through the same platform formatter as Settings
        // (Windows "Shift+R" / macOS "⇧R") — single-sourced, so assert VIA the formatter.
        let want = format!("Rotate Left ({})", crate::support::menu_shortcut(&km, "rotccw"));
        assert!(photo2.iter().any(|i| i.title == want), "rotccw hint mirrors pretty_key");
    }

    /// Empty-state honesty (L15): photo-dependent items disable with no folder; Open/Settings/
    /// Welcome/Close/Quit stay live (the fresh-install debut state must work).
    #[test]
    fn empty_state_enablement() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.has_photo = false;
        s.count = 0;
        for t in [
            TAG_REVEAL, TAG_COPY, TAG_RATE1, TAG_RATE5, TAG_RATE0, TAG_FLAG, TAG_REJECT, TAG_UNMARK,
            TAG_ROT_LEFT, TAG_ROT_RIGHT, TAG_TRASH, TAG_XMP, TAG_COMPARE, TAG_SWAP, TAG_PIN,
            TAG_ZOOM_11, TAG_ZOOM_FIT, TAG_GRID, TAG_FILM, TAG_SORT_PARENT, TAG_SORT_M0, TAG_SORT_ASC,
        ] {
            assert!(!enabled_for(t, &s), "tag {t} must disable in empty state");
        }
        for t in [TAG_SETTINGS, TAG_WELCOME, TAG_QUIT, TAG_OPEN_IMAGE, TAG_OPEN_FOLDER, TAG_CLOSE, TAG_HELP_SHORTCUTS, TAG_HELP_LOG] {
            assert!(enabled_for(t, &s), "tag {t} must stay enabled in empty state");
        }
        // The fullscreen row rides only the modal gate (the in-app F arm has no empty gate).
        assert!(enabled_for(TAG_FULLSCREEN, &s));
    }

    /// The modal gate mirrors the in-app key arms: everything photo-scoped (and undo/fullscreen)
    /// disables while a modal surface is up.
    #[test]
    fn modal_gates_actions() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.modal = true;
        // v0.9.67: a REAL modal (Settings, a confirm, the export sheet) raises both flags — the
        // .slint source defines `modal-open` as `modal-open-fs || welcome-open`, so anything that
        // is not welcome sets them together. This row therefore still asserts what it always did.
        s.modal_fs = true;
        s.undo = UndoKind::Other;
        s.compare = true;
        for t in [TAG_FLAG, TAG_TRASH, TAG_COPY, TAG_UNDO, TAG_COMPARE, TAG_SWAP, TAG_PIN, TAG_FULLSCREEN, TAG_SORT_M0] {
            assert!(!enabled_for(t, &s), "tag {t} must disable under modal");
        }
        // Settings stays live under `modal` on purpose: `modal-open` INCLUDES `settings-open`, so
        // gating on it would disable the row that closes the panel.
        assert!(enabled_for(TAG_SETTINGS, &s) && enabled_for(TAG_QUIT, &s));
    }

    /// v0.9.67 (OWNER RULING 08-08 (ii)): THE WELCOME EXCEPTION, AND ITS NARROWNESS.
    /// On the welcome screen `modal` is true and `modal_fs` is false (the .slint source is one list
    /// plus welcome). Enter Full Screen must be the ONE row that goes live there; every other
    /// modal-gated row must stay exactly as disabled as it was before this ruling — a "narrow
    /// exception" that widened would be the ruling's own failure mode.
    ///
    /// FALSIFIER: revert `TAG_FULLSCREEN => !s.modal_fs` to `!s.modal` and the first assertion
    /// reddens; make any OTHER row read `modal_fs` and its row in the loop reddens.
    #[test]
    fn welcome_frees_the_fullscreen_row_and_nothing_else() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.modal = true; // welcome is up …
        s.modal_fs = false; // … and welcome ALONE is what raised it
        assert!(enabled_for(TAG_FULLSCREEN, &s), "F must work on the welcome screen (ruling ii)");
        for t in [TAG_FLAG, TAG_TRASH, TAG_COPY, TAG_UNDO, TAG_COMPARE, TAG_SWAP, TAG_PIN, TAG_SORT_M0] {
            assert!(!enabled_for(t, &s), "tag {t} must STILL disable on welcome — the exception is fullscreen-only");
        }
        // And with a real modal on top of welcome, the exception closes again.
        s.modal_fs = true;
        assert!(!enabled_for(TAG_FULLSCREEN, &s), "a real modal over welcome re-disables the row");
    }

    /// v0.8.121 (Round-B fix F5 = audit A7, L26): the macOS mirror of wave 2's O8/O9 gate. ⌘, under
    /// an open confirm used to mount the full-height Settings panel BELOW the scrim — inert, its own
    /// close × included, with no cue — and "Show Welcome Guide" stacked a second scrim over the
    /// dialog. Both are the defects wave 2 fixed for the title-bar buttons and not for the menu.
    ///
    /// FALSIFIER (L28): revert the arm to `TAG_SETTINGS | TAG_WELCOME | … => true` and rows 1 and 2
    /// redden; fold TAG_QUIT into the gated arm and row 3 reddens — the one command a user reaches
    /// for when a dialog has them stuck must stay reachable.
    #[test]
    fn a_scrim_bearing_modal_disables_the_two_panel_rows() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.modal_blocking = true;
        s.modal = true; // in production the blocking set is a subset of modal-open
        assert!(!enabled_for(TAG_SETTINGS, &s), "⌘, must not raise a panel under a scrim");
        assert!(!enabled_for(TAG_WELCOME, &s), "…nor may the welcome stack a SECOND scrim");
        // v0.9.59 (OWNER RULING 08-02): Quit's unconditional enablement is now an EXACT L21 mirror
        // rather than a judgement call — ⌘Q is immediate and can never refuse, so there is no gate
        // on the handler side for this row to mirror.
        assert!(enabled_for(TAG_QUIT, &s), "⌘Q always quits — the handler has no refusal to mirror");
        assert!(enabled_for(TAG_HELP_SHORTCUTS, &s) && enabled_for(TAG_HELP_LOG, &s), "Help opens no panel");

        // …and with no scrim-bearing modal the rows behave exactly as before.
        s.modal_blocking = false;
        assert!(enabled_for(TAG_SETTINGS, &s) && enabled_for(TAG_WELCOME, &s));
    }

    /// v0.9.59 (S1 ruling 1 / findings §A): the per-tick ownership re-check. muda replaces
    /// `NSApp.mainMenu` at the first window activation and on window re-creation, so the v0.9.22
    /// one-shot graft appended seven menus into a discarded NSMenu and said "installed". Pointer
    /// identity — muda re-asserts the SAME object on every activation, so a healthy bar must read
    /// Healthy forever (a content compare, or an inverted test, would re-graft on every frame).
    /// FALSIFIER: invert the comparison in `graft_state` (`Some(p) if p != live`) — the "healthy"
    /// and "replaced" asserts swap and both redden.
    /// SECOND FALSIFIER: drop the `&& live != 0` term — the LAST row reddens. That row is the
    /// defensive one: a recorded-nil graft (unreachable today, because `try_install` returns None
    /// on a nil mainMenu) must never read Healthy, or a nil bar would match a nil record and the
    /// menu would never be rebuilt. Named exactly, because the other nil row (`Some(0xAB00), 0`)
    /// survives that edit through the `Some(_)` arm and would have made the claim vacuous.
    #[test]
    fn graft_state_regrafts_only_when_the_main_menu_object_changed() {
        assert_eq!(graft_state(None, 0), GraftState::Install, "nothing installed, no bar yet");
        assert_eq!(graft_state(None, 0xAB00), GraftState::Install, "nothing installed, bar exists");
        assert_eq!(graft_state(Some(0xAB00), 0xAB00), GraftState::Healthy, "same object = healthy");
        assert_eq!(graft_state(Some(0xAB00), 0xCD00), GraftState::Regraft, "muda replaced the bar");
        assert_eq!(graft_state(Some(0xAB00), 0), GraftState::Regraft, "the bar we own vanished");
        assert_eq!(graft_state(Some(0), 0), GraftState::Regraft, "a nil record never reads healthy");
        // Stability: repeated activations re-assert the SAME pointer, so a healthy graft never
        // thrashes — the property that makes a per-tick check affordable.
        for _ in 0..1000 {
            assert_eq!(graft_state(Some(0xAB00), 0xAB00), GraftState::Healthy);
        }
    }

    /// v0.9.59 (S1 ruling 2): the positive fingerprint. `graft=ok` used to mean nothing but
    /// "the pointer was not nil" — the exact check-shaped hole that let the graft land in a
    /// stranger. muda's Quit item always carries keyEquivalent "q".
    /// FALSIFIER: make `app_submenu_fingerprint_ok` return `true` unconditionally (the pre-v0.9.59
    /// non-nil-only gate) — every "not muda's bar" assert reddens.
    #[test]
    fn app_submenu_fingerprint_requires_mudas_quit_key() {
        let muda = ["".into(), "".into(), "h".into(), "q".into()];
        assert!(app_submenu_fingerprint_ok(&muda), "About/Services/Hide/Quit — muda's bar");
        assert!(!app_submenu_fingerprint_ok(&[]), "an empty (or nil) submenu is not a target");
        let foreign: Vec<String> = ["", "w", "m", "H"].iter().map(|s| s.to_string()).collect();
        assert!(!app_submenu_fingerprint_ok(&foreign), "no ⌘Q ⇒ not the bar we graft into");
        // Case matters: AppKit's ⇧⌘Q (Log Out) is "Q", not "q".
        assert!(!app_submenu_fingerprint_ok(&["Q".to_string()]));
    }

    /// v0.9.59 (S1 ruling 2, energy): a refused fingerprint means polling can now run forever, so
    /// the poll drops to ~2 Hz once the ~10 s diagnostic window has passed. Full rate before it, so
    /// a normal boot still grafts on the very tick muda's bar appears.
    /// FALSIFIER: replace the body of `should_attempt_graft` with `true` — the "slow rate after the
    /// window" asserts redden (and the graft scan would run 62.5x/s forever on a broken backend).
    #[test]
    fn graft_poll_drops_to_slow_rate_after_the_diagnostic_window() {
        for t in [0, 1, 59, 599] {
            assert!(should_attempt_graft(t), "tick {t} is inside the full-rate window");
        }
        assert!(should_attempt_graft(GRAFT_POLL_FULL_TICKS), "600 is a multiple of 30 — attempt");
        assert!(!should_attempt_graft(601), "601 waits for the next slow slot");
        assert!(!should_attempt_graft(629));
        assert!(should_attempt_graft(630), "…which is 30 ticks later");
        // One attempt per GRAFT_POLL_SLOW ticks, forever.
        let attempts = (600..1200).filter(|&t| should_attempt_graft(t)).count();
        assert_eq!(attempts, 600 / GRAFT_POLL_SLOW as usize);
    }

    /// Sort radio: exactly the live method row checks; direction pair mutually exclusive; the
    /// options mirror the in-app sort menu (7 methods + Asc/Desc).
    #[test]
    fn sort_radio_mirrors_live_mode() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.sort_method = 3;
        s.sort_desc = true;
        let menus = build_menus(&s);
        let view = &menus[3].1;
        let sort = view.iter().find(|i| i.tag == TAG_SORT_PARENT).unwrap().submenu.as_ref().unwrap();
        let checked: Vec<&str> = sort.iter().filter(|i| i.checked).map(|i| i.title.as_str()).collect();
        assert_eq!(checked, ["Date created", "Descending"]);
        let rows: Vec<&str> = sort.iter().filter(|i| !i.separator).map(|i| i.title.as_str()).collect();
        assert_eq!(
            rows,
            ["Name", "Date taken", "Date modified", "Date created", "Size", "Type", "Rating", "Ascending", "Descending"]
        );
    }

    /// L17 zoom-pair enablement honesty: in single view the "Zoom 1:1"/"Zoom to Fit" pair is
    /// MUTUALLY EXCLUSIVE (never both enabled → never a mislabeled duplicate). "Zoom 1:1" is honest
    /// only at Fit (its dispatch is the on_photo_clicked toggle); "Zoom to Fit" only when zoomed. In
    /// compare BOTH stay live: "Zoom 1:1" is one-way and "Zoom to Fit" (on_pan_reset) does real work
    /// (resets the shared zoom + recentres the compare pan).
    #[test]
    fn a_ready_opening_photo_keeps_its_zoom_rows_while_view_only_blocks_edits() {
        // v1.0.6: View only makes `modal` true while a photo opens. The zoom rows follow the
        // inspection gate instead, so they stay live for a ready photo and only for zoom.
        let km = default_keymap();
        for at_fit in [true, false] {
            let mut s = snap_with(&km);
            s.compare = false;
            s.at_fit = at_fit;
            s.modal = true;
            s.inspect_open = false;
            assert!(!enabled_for(TAG_ZOOM_11, &s) && !enabled_for(TAG_ZOOM_FIT, &s),
                "a dialog (or an unready photo) still disables both zoom rows (at_fit={at_fit})");
            s.inspect_open = true;
            assert_ne!(enabled_for(TAG_ZOOM_11, &s), enabled_for(TAG_ZOOM_FIT, &s),
                "an opening photo keeps the honest zoom pair (at_fit={at_fit})");
            assert!(!enabled_for(TAG_TRASH, &s), "inspection does not unlock edits");
        }
    }

    #[test]
    fn zoom_pair_mutually_exclusive_enablement() {
        let km = default_keymap();

        // (single, at_fit): only the 1:1 toggle is honest (Fit → 1:1).
        let mut single_fit = snap_with(&km);
        single_fit.compare = false;
        single_fit.at_fit = true;
        assert!(enabled_for(TAG_ZOOM_11, &single_fit), "single@fit: 1:1 enabled");
        assert!(!enabled_for(TAG_ZOOM_FIT, &single_fit), "single@fit: Fit disabled");

        // (single, zoomed): only Fit is honest (there is a zoom to reset).
        let mut single_zoomed = snap_with(&km);
        single_zoomed.compare = false;
        single_zoomed.at_fit = false;
        assert!(!enabled_for(TAG_ZOOM_11, &single_zoomed), "single@zoom: 1:1 disabled");
        assert!(enabled_for(TAG_ZOOM_FIT, &single_zoomed), "single@zoom: Fit enabled");

        // Mutual exclusivity in single view, at BOTH zoom states — never a duplicate pair.
        for at_fit in [true, false] {
            let mut s = snap_with(&km);
            s.compare = false;
            s.at_fit = at_fit;
            assert_ne!(
                enabled_for(TAG_ZOOM_11, &s),
                enabled_for(TAG_ZOOM_FIT, &s),
                "single view: the zoom pair is mutually exclusive (at_fit={at_fit})"
            );
        }

        // Compare: both live regardless of the shared-zoom bit (1:1 one-way; Fit resets zoom + pan).
        for at_fit in [true, false] {
            let mut s = snap_with(&km);
            s.compare = true;
            s.at_fit = at_fit;
            assert!(enabled_for(TAG_ZOOM_11, &s), "compare: 1:1 enabled (at_fit={at_fit})");
            assert!(enabled_for(TAG_ZOOM_FIT, &s), "compare: Fit enabled (at_fit={at_fit})");
        }

        // Empty state kills the whole pair (photo_ok) in either mode.
        let mut empty = snap_with(&km);
        empty.has_photo = false;
        for compare in [false, true] {
            empty.compare = compare;
            assert!(!enabled_for(TAG_ZOOM_11, &empty) && !enabled_for(TAG_ZOOM_FIT, &empty));
        }
    }

    /// L15 Trash-in-compare honesty: TAG_TRASH disables in A|B compare (its dispatch is compare-gated
    /// — an enabled item would be a silent no-op), while the OTHER Photo-menu actions (rate/flag) stay
    /// enabled — encoding the audit's exact surprise case.
    #[test]
    fn trash_disabled_in_compare() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.compare = true;
        assert!(!enabled_for(TAG_TRASH, &s), "trash must disable in compare (dispatch no-ops there)");
        // The maximal-surprise contrast: sibling Photo actions still work in compare.
        assert!(enabled_for(TAG_RATE1 + 2, &s), "rate stays enabled in compare");
        assert!(enabled_for(TAG_FLAG, &s), "flag stays enabled in compare");
        // Outside compare (single view) trash is enabled again (photo_ok holds).
        s.compare = false;
        assert!(enabled_for(TAG_TRASH, &s), "trash enabled in single view");
    }

    /// Undo dynamic title honesty: delete-record state retitles to "Undo Delete"; plain undo keeps
    /// "Undo"; empty stack disables.
    #[test]
    fn undo_title_and_enablement() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.undo = UndoKind::Delete;
        assert_eq!(build_menus(&s)[1].1[0].title, "Undo Delete");
        assert!(enabled_for(TAG_UNDO, &s));
        s.undo = UndoKind::Other;
        assert_eq!(build_menus(&s)[1].1[0].title, "Undo");
        s.undo = UndoKind::None;
        assert!(!enabled_for(TAG_UNDO, &s));
    }

    /// Keeping prepared work (2 November): star a photo, delete another, and the row still reads
    /// "Undo Delete" with a star on the stack. This is what stands between the surviving stack and
    /// a label that lies.
    ///
    /// FALSIFIER (L28): map `Some(Delete)` to `Other`, map `Some(Move)` to `Delete` or `None`, or
    /// drop the `has_record` term so another folder's record goes quiet.
    #[test]
    fn the_undo_row_says_what_the_next_undo_reaches() {
        use crate::support::{live_undo_arm, UndoArm};
        for (arm, record, want) in [
            (Some(UndoArm::Delete(5)), true, UndoKind::Delete),
            (Some(UndoArm::Stack(5)), true, UndoKind::Other),
            (Some(UndoArm::Stack(5)), false, UndoKind::Other),
            (Some(UndoArm::Move(5)), false, UndoKind::Other),
            (None, true, UndoKind::Delete),
            (None, false, UndoKind::None),
        ] {
            assert!(undo_kind_for(arm, record) == want, "{:?} record={record}", arm.map(|a| a.seq()));
        }
        // The sequence itself: a surviving star, then a newer delete of another photo.
        let d = "folder";
        let starred_then_deleted = live_undo_arm(Some(1), Some((d, 2)), d, None);
        assert!(undo_kind_for(starred_then_deleted, true) == UndoKind::Delete);
        // A star made after the delete pops first, while the record stays live.
        let deleted_then_starred = live_undo_arm(Some(3), Some((d, 2)), d, None);
        assert!(undo_kind_for(deleted_then_starred, true) == UndoKind::Other);
        // A record from another folder, with nothing on the stack.
        let elsewhere = live_undo_arm(None, Some(("other", 2)), d, None);
        assert!(undo_kind_for(elsewhere, true) == UndoKind::Delete);
    }

    /// Recents: empty list → disabled parent + the disabled placeholder row; entries surface
    /// newest-first with basename titles and index-stable tags.
    #[test]
    fn recents_submenu_states() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        assert!(!enabled_for(TAG_RECENT_PARENT, &s));
        let file = &build_menus(&s)[0].1;
        let rec = file.iter().find(|i| i.tag == TAG_RECENT_PARENT).unwrap().submenu.as_ref().unwrap();
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].title, "No Recent Folders");
        assert!(!enabled_for(rec[0].tag, &s));

        s.recents = vec!["C:/photos/wedding".into(), "C:/x/street".into()];
        assert!(enabled_for(TAG_RECENT_PARENT, &s));
        let file = &build_menus(&s)[0].1;
        let rec = file.iter().find(|i| i.tag == TAG_RECENT_PARENT).unwrap().submenu.as_ref().unwrap();
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[0].title, "wedding");
        assert_eq!(rec[0].tag, TAG_RECENT_BASE);
        assert_eq!(rec[1].tag, TAG_RECENT_BASE + 1);
        assert!(enabled_for(TAG_RECENT_BASE + 1, &s));
        assert!(!enabled_for(TAG_RECENT_BASE + 2, &s), "out-of-range recent tag must disable");
    }

    /// XMP checkbox + the visibility toggles mirror live state; fullscreen row retitles.
    #[test]
    fn state_checks_mirror_snapshot() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        s.xmp_sync = true;
        s.grid_open = true;
        s.film_visible = false;
        s.immersive = true;
        s.rating = 2;
        s.flagged = true;
        let menus = build_menus(&s);
        let photo = &menus[2].1;
        assert!(photo.iter().find(|i| i.tag == TAG_XMP).unwrap().checked);
        assert!(photo.iter().find(|i| i.tag == TAG_FLAG).unwrap().checked);
        assert!(photo.iter().find(|i| i.tag == TAG_RATE1 + 1).unwrap().checked);
        assert!(!photo.iter().find(|i| i.tag == TAG_RATE1).unwrap().checked);
        let view = &menus[3].1;
        assert!(view.iter().find(|i| i.tag == TAG_GRID).unwrap().checked);
        assert!(!view.iter().find(|i| i.tag == TAG_FILM).unwrap().checked);
        assert_eq!(view.iter().find(|i| i.tag == TAG_FULLSCREEN).unwrap().title, "Exit Full Screen");
    }

    /// v0.9.63 (B-R5-2): THE FULLSCREEN ROW SAYS WHICH WAY THE COMMAND WILL GO — and since B-R5-2
    /// that command leaves a fullscreen NOBODY IN FALCON OWNS, so the title cannot read `immersive`
    /// alone any more. A green-button / ⌃⌘F / Spaces fullscreen leaves Falcon's chrome on and
    /// `immersive` false: the row used to say "Enter Full Screen" over an already-fullscreen window,
    /// which was merely odd while F was a chrome key there and would be a row promising the exact
    /// opposite of what it does now.
    ///
    /// FALSIFIER (assert granularity): revert the title to `if s.immersive` and the
    /// `"a green-button fullscreen is still a fullscreen"` row reddens with "Enter Full Screen";
    /// make it `s.immersive && s.os_fullscreen` and the immersive-over-an-ended-fullscreen row
    /// reddens, which is the state the 4 Hz reconciliation poll leaves live for up to 250 ms.
    #[test]
    fn the_fullscreen_row_titles_from_the_window_not_from_immersive_alone() {
        let km = default_keymap();
        let title = |imm: bool, os: bool| {
            let mut s = snap_with(&km);
            s.immersive = imm;
            s.os_fullscreen = os;
            build_menus(&s)[3].1.iter().find(|i| i.tag == TAG_FULLSCREEN).unwrap().title.clone()
        };
        assert_eq!(title(false, false), "Enter Full Screen", "a normal window offers the way in");
        assert_eq!(title(true, true), "Exit Full Screen", "Falcon's own immersive offers the way out");
        assert_eq!(
            title(false, true),
            "Exit Full Screen",
            "a green-button fullscreen is still a fullscreen — the row offers the way out of it"
        );
        assert_eq!(
            title(true, false),
            "Exit Full Screen",
            "immersive over a fullscreen that already ended still offers the exit that restores the chrome"
        );
    }

    /// v0.9.24 (ruling 4): the View menu's info-panel row. L20 — its title is derived from the live
    /// state and matches the in-app right-click menu VERBATIM; the bare-key suffix rides the LIVE
    /// keymap and is suppressed in the OFF state (where the key reveals first and takes a second
    /// press, so a hint would promise a one-press action). Placement: with its sibling panel-
    /// visibility toggles, above the separator that precedes the fullscreen row.
    #[test]
    fn info_panel_row_title_placement_and_keys() {
        let mut km = default_keymap();
        let mut s = snap_with(&km);
        let row = |s: &MenuSnapshot| {
            build_menus(s)[3].1.iter().find(|i| i.tag == TAG_INFO_PANEL).unwrap().title.clone()
        };
        // Expanded (the default): "Minimise …" + the live default key.
        s.info_open = true;
        s.info_min = false;
        assert_eq!(row(&s), "Minimise info panel (I)");
        // Minimized stub: "Expand …" — same key hint (one press still does exactly this).
        s.info_min = true;
        assert_eq!(row(&s), "Expand info panel (I)");
        // Fully off with the develop panel still visible (the LEGACY pair): "Show info panel", with
        // NO key hint, whatever info-min remembers.
        s.info_open = false;
        assert_eq!(row(&s), "Show info panel");
        s.info_min = false;
        assert_eq!(row(&s), "Show info panel");
        // v0.8.97 (ruling 5): both panels hidden — the only state the Settings seg's Hidden cell can
        // produce — and the row goes PLURAL, because the command restores both.
        s.raw_open = false;
        assert_eq!(row(&s), "Show panels");
        s.info_min = true;
        assert_eq!(row(&s), "Show panels", "the remembered min never changes the off verb");
        // …and the ON titles ignore the develop panel entirely (per-panel independence).
        s.info_open = true;
        assert_eq!(row(&s), "Expand info panel (I)");
        s.info_min = false;
        assert_eq!(row(&s), "Minimise info panel (I)");
        s.raw_open = true;
        // The hint follows a REBIND, and an unbound action drops it (never a stale/hardcoded key).
        km.insert("info".into(), "k".into());
        let mut s2 = snap_with(&km);
        s2.info_open = true;
        s2.info_min = true;
        assert_eq!(row(&s2), "Expand info panel (K)");
        km.insert("info".into(), "".into());
        let mut s3 = snap_with(&km);
        s3.info_open = true;
        assert_eq!(row(&s3), "Minimise info panel", "unbound → no hint");
        // The title always equals the single-sourced ctx-menu string (no second label table), in
        // every one of the eight states the three fields can take.
        for (open, min) in [(false, false), (false, true), (true, true), (true, false)] {
            for raw_open in [false, true] {
                let mut t = snap_with(&km); // km has "info" unbound → the bare title in every state
                t.info_open = open;
                t.info_min = min;
                t.raw_open = raw_open;
                assert_eq!(row(&t), crate::support::info_menu_title(open, min, raw_open));
            }
        }
        // Placement: Grid, Filmstrip, Info Panel form one group, then a separator, then the ONE
        // fullscreen row (macOS convention — the panel command sits above Enter Full Screen).
        let view = &build_menus(&s)[3].1;
        let pos = |tag: i32| view.iter().position(|i| i.tag == tag).unwrap();
        assert_eq!(pos(TAG_INFO_PANEL), pos(TAG_FILM) + 1, "the row joins the panel-toggle group");
        assert!(view[pos(TAG_INFO_PANEL) + 1].separator, "…and closes that group");
        assert_eq!(pos(TAG_FULLSCREEN), pos(TAG_INFO_PANEL) + 2, "…immediately above Enter Full Screen");
        // It is an honest 3-state row, so it never draws a checkmark (one tick cannot say three things).
        assert!(!view.iter().find(|i| i.tag == TAG_INFO_PANEL).unwrap().checked);
        assert!(view.iter().find(|i| i.tag == TAG_INFO_PANEL).unwrap().key.is_empty(), "display-only key");
    }

    /// v0.9.24 (L21 + the compare-recovery ruling): the row's enablement mirrors its dispatch arm's
    /// own gate (`!modal && !empty`) — so it stays LIVE in compare, which is where the Settings seg,
    /// this row, and the context menu opened from a FILMSTRIP thumbnail (v0.9.26 / V7: not from the
    /// photo — that TouchArea is unmounted in compare) are the only ways back to a fully-off panel.
    #[test]
    fn info_panel_row_enablement_mirrors_its_dispatch_gate() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        assert!(enabled_for(TAG_INFO_PANEL, &s), "normal single view: enabled");
        s.compare = true;
        assert!(enabled_for(TAG_INFO_PANEL, &s), "compare recovery depends on this row staying live");
        s.compare = false;
        s.immersive = true;
        // v0.9.25 (v0.8.93-audit C21): GREYED in immersive. This assertion used to read the other
        // way, justified by "the callback still works (no gate there)" — true of the callback and
        // false of the visible outcome, since both info surfaces are mount-culled in immersive. The
        // shared callback is now itself a no-op there, so an enabled row would advertise an action
        // its own body refuses: enablement mirrors the dispatch gate, which is the whole rule.
        assert!(
            !enabled_for(TAG_INFO_PANEL, &s),
            "immersive: the shared callback is a no-op there, so the row must not offer it"
        );
        s.immersive = false;
        s.modal = true;
        assert!(!enabled_for(TAG_INFO_PANEL, &s), "modal: mirrors the dispatch arm's !modal");
        s.modal = false;
        s.has_photo = false;
        assert!(!enabled_for(TAG_INFO_PANEL, &s), "empty state: mirrors the dispatch arm's !empty (L15)");
        // Out of immersive it is exactly the sibling panel toggles' predicate — the three
        // view-furniture rows share one rule everywhere their bodies share one gate.
        for (photo, modal) in [(true, false), (true, true), (false, false), (false, true)] {
            let mut t = snap_with(&km);
            t.has_photo = photo;
            t.modal = modal;
            assert_eq!(enabled_for(TAG_INFO_PANEL, &t), enabled_for(TAG_GRID, &t));
            // …and v0.8.119 (design-sweep Y49) settles the immersive axis for all THREE. The
            // deferred ledger-class item this block's v0.9.26 note named — "adding `&& !s.immersive`
            // to that shared arm would have silently greyed Grid and Filmstrip on macOS with the
            // suite green" — is exactly the fix, and it is no longer silent: both surfaces are
            // immersive-mount-gated in the UI, so the rows were flipping a CHECKMARK for something
            // that cannot appear, doing nothing on screen, and storing up a surprise for the way out
            // of full screen (L26 — a gated body with an ungated invoker). The three view-furniture
            // rows now share ONE rule in every state, which is what the block above already asserts
            // out of immersive.
            //
            // FALSIFIER (L28): drop `&& !s.immersive` from the `TAG_GRID | TAG_FILM` arm and the two
            // immersive asserts below fail. Note the v0.9.26 trap they were written against is still
            // avoided: Grid is compared against a LITERAL expectation, not against Filmstrip (which
            // resolves to the same match arm and so could never discriminate).
            let (grid_before, film_before) = (enabled_for(TAG_GRID, &t), enabled_for(TAG_FILM, &t));
            t.immersive = true;
            assert!(!enabled_for(TAG_INFO_PANEL, &t), "immersive greys the info row in every state");
            assert!(!enabled_for(TAG_GRID, &t), "v0.8.119 (Y49): …and Photo Grid, whose surface is mount-culled there");
            assert!(!enabled_for(TAG_FILM, &t), "v0.8.119 (Y49): …and Filmstrip, for the same reason");
            let _ = (grid_before, film_before); // pre-immersive values: unchanged by construction (asserted above)
        }
    }

    // ── v0.9.64 (OWNER RULING 08-04, option (a)): THE MENU BAR'S PLURALITY ───────────────────────

    /// Convenience: the Photo menu's rows, by tag, for a given snapshot.
    fn photo_row(s: &MenuSnapshot, tag: i32) -> ItemDef {
        let menus = build_menus(s);
        let it = menus[2].1.iter().find(|i| i.tag == tag).expect("Photo row missing");
        ItemDef {
            tag: it.tag,
            title: it.title.clone(),
            key: it.key,
            ctrl: it.ctrl,
            checked: it.checked,
            separator: it.separator,
            submenu: None,
        }
    }

    /// THE FIRST HALF OF THE RULING: with no armed selection, every Photo row is exactly the row
    /// v0.9.63 shipped — same title, same checkmark, same source for that checkmark.
    ///
    /// The load-bearing rows here are not the string comparisons, they are the CONTRADICTION rows:
    /// the four `sel_*` fields are set to values that would produce a completely different menu if
    /// anything read them without asking the gate first, and the menu does not move. That is what
    /// makes this a test of the gate rather than of the formatter.
    ///
    /// FALSIFIER: title any row from `sel_count` without going through [`bulk_count`] (e.g. make
    /// Flag read `if s.sel_count > 0`) and the three dormancy blocks below redden while the plain
    /// no-selection block stays green — which is precisely the bug shape that ordering catches.
    #[test]
    fn the_photo_rows_are_single_photo_and_unchanged_without_an_armed_selection() {
        let km = default_keymap();
        let g = |id: &str| crate::support::menu_shortcut(&km, id);
        // A snapshot whose SINGLE-photo state is unambiguous and whose GROUP state says the
        // opposite of it everywhere, so a leak in either direction is visible.
        let base = || {
            let mut s = snap_with(&km);
            s.rating = 2;
            s.flagged = true;
            s.rejected = false;
            s.sel_flag_sets = true; // group: "a press would FLAG" ⇒ would title "Flag N Photos"
            s.sel_reject_sets = false; // group: "a press would UN-REJECT"
            s.sel_rating = 5; // group: uniformly ★5
            s
        };
        let expect_single = |s: &MenuSnapshot, why: &str| {
            assert_eq!(photo_row(s, TAG_FLAG).title, hint("Flag", &g("flag")), "{why}");
            assert!(photo_row(s, TAG_FLAG).checked, "{why}: the check is the CURRENT SHOT's flag");
            assert_eq!(photo_row(s, TAG_REJECT).title, hint("Reject", &g("reject")), "{why}");
            assert!(!photo_row(s, TAG_REJECT).checked, "{why}: …and the current shot's reject");
            assert_eq!(photo_row(s, TAG_UNMARK).title, hint("Unmark", &g("unflag")), "{why}");
            assert_eq!(photo_row(s, TAG_RATE0).title, hint("Clear Rating", &g("rate0")), "{why}");
            for n in 1..=5i32 {
                let stars = "★".repeat(n as usize);
                let row = photo_row(s, TAG_RATE1 + n - 1);
                assert_eq!(row.title, hint(&format!("Rate {stars}"), &g(&format!("rate{n}"))), "{why}");
                assert_eq!(row.checked, s.rating == n, "{why}: rate{n} checks the CURRENT shot");
            }
            assert_eq!(
                photo_row(s, TAG_TRASH).title,
                hint(&format!("{}…", crate::platform::PLATFORM.move_to_trash), &g("delete")),
                "{why}"
            );
            // v1.0.0-rc (item 27, B2 b): rotation DOES count now — but only over an armed selection.
            // With none, these two rows are the single-photo ones v0.9.63 shipped, byte for byte.
            assert_eq!(photo_row(s, TAG_ROT_LEFT).title, hint("Rotate Left", &g("rotccw")), "{why}");
            assert_eq!(photo_row(s, TAG_ROT_RIGHT).title, hint("Rotate Right", &g("rotcw")), "{why}");
        };

        // 1) No selection at all — the ordinary case, and the one the ruling names verbatim
        //    ("When NO selection exists, titles and behaviour stay exactly today's single-photo forms").
        expect_single(&base(), "no selection");
        // 2) A selection that exists but is DORMANT. Each of these is the shared gate's own term,
        //    and each must produce the identical single-photo menu — the menu bar has no gate of
        //    its own to be more permissive with.
        let mut compare = base();
        compare.sel_count = 12;
        compare.compare = true;
        expect_single(&compare, "12 selected in A|B compare");
        let mut immersive = base();
        immersive.sel_count = 12;
        immersive.immersive = true;
        expect_single(&immersive, "12 selected in immersive");
        let mut empty_sel = base();
        empty_sel.sel_count = 0;
        expect_single(&empty_sel, "an empty selection arms nothing");
        // 3) …and the premise: the SAME snapshot with the dormancy terms lifted does move, or the
        //    three blocks above would be green for the trivial reason.
        let mut armed = base();
        armed.sel_count = 12;
        assert_eq!(photo_row(&armed, TAG_FLAG).title, hint("Flag 12 Photos", &g("flag")));
    }

    /// THE SECOND HALF: armed, every row carries the COUNT and — where the action has one — the
    /// DIRECTION, in the context menus' grammar with AppKit's capitalisation.
    ///
    /// Direction is the half that matters and the half a count alone cannot supply: "Flag 12
    /// Photos" over a set that is already entirely flagged states the opposite of what the row
    /// does (ledger L20), and a menu bar has no tile rims beside it to read the group's state from.
    /// The bit is `support::bulk_mark_sets`' own answer, carried in the snapshot, so the row's verb,
    /// the row's checkmark and the write are one fact seen three times.
    ///
    /// FALSIFIER (assert granularity): make `bulk_menubar_mark_title` ignore `sets` (always the set
    /// verb) and the four "Unflag"/"Un-reject" rows redden while every count row stays green — the
    /// exact defect the ruling forbids, isolated. Drop the `!s.sel_flag_sets` from the check and the
    /// checkmark rows redden alone. Make `menubar_photo_count` unconditionally plural and the n=1
    /// rows redden.
    #[test]
    fn the_photo_rows_count_and_state_their_direction_over_an_armed_selection() {
        let km = default_keymap();
        let g = |id: &str| crate::support::menu_shortcut(&km, id);
        let snap = |n: i32, flag_sets: bool, rej_sets: bool, sel_rating: i32| {
            let mut s = snap_with(&km);
            s.sel_count = n;
            s.sel_flag_sets = flag_sets;
            s.sel_reject_sets = rej_sets;
            s.sel_rating = sel_rating;
            // The CURRENT shot's state is deliberately the opposite of the group's throughout, so
            // any row still reading it is visible rather than accidentally agreeing.
            s.rating = 1;
            s.flagged = !flag_sets;
            s.rejected = !rej_sets;
            s
        };

        // ── DIRECTION, both ways, on both marks ────────────────────────────────────────────────
        let mixed = snap(12, true, true, -1); // nobody (or not everybody) carries either bit
        assert_eq!(photo_row(&mixed, TAG_FLAG).title, hint("Flag 12 Photos", &g("flag")));
        assert!(!photo_row(&mixed, TAG_FLAG).checked, "not all flagged ⇒ the row is not 'on'");
        assert_eq!(photo_row(&mixed, TAG_REJECT).title, hint("Reject 12 Photos", &g("reject")));
        assert!(!photo_row(&mixed, TAG_REJECT).checked);

        let all_marked = snap(12, false, false, -1); // every selected photo already carries it
        assert_eq!(
            photo_row(&all_marked, TAG_FLAG).title,
            hint("Unflag 12 Photos", &g("flag")),
            "a set that is entirely flagged gets the row that UNFLAGS it, and says so"
        );
        assert!(photo_row(&all_marked, TAG_FLAG).checked, "…and the check is the GROUP's state");
        assert_eq!(photo_row(&all_marked, TAG_REJECT).title, hint("Un-reject 12 Photos", &g("reject")));
        assert!(photo_row(&all_marked, TAG_REJECT).checked);
        // The two marks are independent — one direction must never decide the other.
        let split = snap(12, true, false, -1);
        assert_eq!(photo_row(&split, TAG_FLAG).title, hint("Flag 12 Photos", &g("flag")));
        assert_eq!(photo_row(&split, TAG_REJECT).title, hint("Un-reject 12 Photos", &g("reject")));

        // ── COUNT, including the singular ──────────────────────────────────────────────────────
        let one = snap(1, true, true, -1);
        assert_eq!(photo_row(&one, TAG_FLAG).title, hint("Flag 1 Photo", &g("flag")));
        assert_eq!(photo_row(&one, TAG_UNMARK).title, hint("Unmark 1 Photo", &g("unflag")));
        assert_eq!(photo_row(&one, TAG_TRASH).title, hint("Delete 1 Photo…", &g("delete")));
        assert_eq!(photo_row(&one, TAG_RATE0).title, hint("Clear Rating on 1 Photo", &g("rate0")));

        // ── THE FIVE RULED ROWS, at the ruling's own count ─────────────────────────────────────
        assert_eq!(photo_row(&mixed, TAG_UNMARK).title, hint("Unmark 12 Photos", &g("unflag")));
        // v1.0.0-rc (item 27, sheet 2.1 B2 b, OWNER RULING): "Rotate acts on all selected". The
        // v0.9.64 comment beside these two rows said a plural title here would be "the first surface
        // to promise" a bulk rotate, because there was none anywhere; this round builds it, on the
        // keys and in both context menus, so the premise is false and the rows take the ruling's own
        // plurality. The COUNT is `menubar_photo_count`, the same noun every other counted row here
        // carries; there is no direction to derive (a rotate is an imperative, like Unmark), so the
        // direction word is the row's own and does not move with the group's state.
        assert_eq!(photo_row(&mixed, TAG_ROT_RIGHT).title, hint("Rotate 12 Photos Right", &g("rotcw")));
        assert_eq!(photo_row(&mixed, TAG_ROT_LEFT).title, hint("Rotate 12 Photos Left", &g("rotccw")));
        assert_eq!(photo_row(&one, TAG_ROT_RIGHT).title, hint("Rotate 1 Photo Right", &g("rotcw")));
        assert_eq!(
            photo_row(&mixed, TAG_TRASH).title,
            hint("Delete 12 Photos…", &g("delete")),
            "the ellipsis is the count-naming confirm, which is the only thing that deletes"
        );
        for n in 1..=5i32 {
            let stars = "★".repeat(n as usize);
            let row = photo_row(&mixed, TAG_RATE1 + n - 1);
            assert_eq!(row.title, hint(&format!("Rate 12 Photos {stars}"), &g(&format!("rate{n}"))));
            assert!(!row.checked, "a MIXED spread fills no star — mixed's honest look");
        }
        // …and a uniform group checks exactly its own rating, never the current shot's (which is 1).
        let uniform3 = snap(12, true, true, 3);
        for n in 1..=5i32 {
            assert_eq!(
                photo_row(&uniform3, TAG_RATE1 + n - 1).checked,
                n == 3,
                "the Rate check follows the GROUP's uniform rating while armed"
            );
        }

        // ── The counted titles keep the noun the menu bar was ruled to carry ───────────────────
        // v0.9.66 (the round-7 sync): this row used to compare against `support::bulk_mark_label`
        // — the "Flag 12 photos" context-menu form — because the two surfaces then shared one
        // sentence and only its casing differed. The trunk's v0.8.163 §7-item-3 ruling DELETED
        // that form: in-app context menus now say the bare "Flag 12" (`bulk_btn_label`), so there
        // is no longer a shared sentence to derive this from, and comparing against the button
        // form would assert the menu bar has lost the noun the 08-04 mac ruling gave it. The
        // literals are asserted instead, which is what the ruling actually names.
        for sets in [true, false] {
            let bar = crate::support::bulk_menubar_mark_title("Flag", "Unflag", 12, sets);
            assert_eq!(bar, if sets { "Flag 12 Photos" } else { "Unflag 12 Photos" }, "sets={sets}");
        }
    }

    /// RULING TERM: "Plurality is gated by the same `support::bulk_actions_allowed` gate as the
    /// keys — ONE gate, no menu-only variant." [`bulk_count`] does not restate that predicate's
    /// terms, it CALLS it, and this row is the differential that proves the call rather than the
    /// coincidence: over every combination of the three inputs the menu's answer and the shared
    /// gate's answer are the same bit, and the count it hands back is the count it was given.
    ///
    /// FALSIFIER: give `bulk_count` its own terms (`sel_count > 0 && !compare` — the plausible
    /// half-copy, since a menu row disabling in compare is already covered by `enabled_for`) and
    /// every immersive row reddens. Make it `sel_count > 0` alone and the four dormancy rows
    /// redden. Return `Some(0)` for an empty selection and the "arms nothing" row reddens.
    #[test]
    fn the_menu_bars_plurality_gate_is_bulk_actions_allowed_itself() {
        for n in [0i32, 1, 2, 12, 500] {
            for compare in [false, true] {
                for immersive in [false, true] {
                    let got = bulk_count(n, compare, immersive);
                    let want = crate::support::bulk_actions_allowed(n as usize, compare, immersive);
                    assert_eq!(
                        got.is_some(),
                        want,
                        "bulk_count({n}, compare={compare}, immersive={immersive}) must be the \
                         shared gate's own answer"
                    );
                    if let Some(k) = got {
                        assert_eq!(k as i32, n, "the count is the selection's, not a re-derivation");
                    }
                }
            }
        }
        // The named terms, spelled out so a reader of this file sees the rule without chasing it.
        assert!(bulk_count(12, false, false).is_some(), "12 selected, bare view: plural");
        assert!(bulk_count(0, false, false).is_none(), "an empty selection arms nothing");
        assert!(bulk_count(12, true, false).is_none(), "compare: NEVER (addendum A-3)");
        assert!(bulk_count(12, false, true).is_none(), "immersive: the disclosure is unmounted (A-2)");
        // A negative count is not reachable from `count-selected`, but the cast must not wrap.
        assert!(bulk_count(-1, false, false).is_none(), "a negative count can never arm");
    }

    /// RULING TERMS (iv) and (v), pinned where they actually live: the macOS command drain is inside
    /// `#[cfg(target_os = "macos")]`, so no Windows test run can EXECUTE those arms — but it can read
    /// them. Source pins, in the house idiom (`tipgeom_tests` reads `main_window.slint` and
    /// `support.rs` the same way), asserting the two properties the ruling is most emphatic about:
    ///
    ///   * Rate reaches the ASK, never a write. The armed branch invokes `bulk-rate-ask-key`, whose
    ///     handler goes through `bulk_rate_request` — the one place a bulk rating's direction is
    ///     decided and the ask-first toast is raised — and never touches `BulkOp::Rate` itself.
    ///   * Trash reaches the CONFIRM, never a delete. The arm's only call is `invoke_delete_key`,
    ///     whose body hands a live selection to `open_bulk_delete`, which raises confirm-kind 6.
    ///
    /// FALSIFIER: replace `app.invoke_bulk_rate_ask_key(n)` with `app.invoke_set_rating(n)` and the
    /// Rate ordering row reddens; call `apply(BulkOp::Rate(stars), …)` from `on_bulk_rate_ask_key`
    /// and the "never rates directly" row reddens; delete `a.set_confirm_kind(6)` from
    /// `open_bulk_delete` and the confirm row reddens.
    #[test]
    fn the_plural_rate_and_trash_rows_reach_the_ask_and_the_confirm_never_the_write() {
        // Line endings are normalized because the repo checks out with `core.autocrlf=true`: the
        // working tree is CRLF on Windows and LF on the Mac, and a structural anchor that matched
        // on only one of them would be a test that passes for a reason that has nothing to do with
        // the code it pins.
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("main.rs is readable from its own crate")
            .replace("\r\n", "\n");
        // A slice of `src` between two anchors, so a pin is about ONE arm rather than the file.
        let between = |from: &str, to: &str| -> String {
            let a = src.find(from).unwrap_or_else(|| panic!("anchor missing: {from}"));
            let b = src[a..].find(to).unwrap_or_else(|| panic!("closing anchor missing: {to}")) + a;
            src[a..b].to_string()
        };
        // Ordered-substring helper: each needle must appear AFTER the previous one.
        let in_order = |hay: &str, needles: &[&str]| {
            let mut at = 0usize;
            for nd in needles {
                let found = hay[at..].find(nd).unwrap_or_else(|| panic!("missing (in order): {nd}"));
                at += found + nd.len();
            }
        };

        // (iv) THE RATE ARM — armed ⇒ the ask; unarmed ⇒ the single-photo helper, in that order.
        let rate_arm = between("t if (mm::TAG_RATE1..=mm::TAG_RATE5).contains(&t) =>", "mm::TAG_RATE0 =>");
        in_order(
            &rate_arm,
            &["if bulk.is_some()", "app.invoke_bulk_rate_ask_key(n);", "} else {", "app.invoke_set_rating(target);"],
        );
        assert!(
            !rate_arm.contains("invoke_bulk_rate_ask(") ,
            "the menu-bar row must use the KEY's re-press door, not the star row's"
        );
        // …and that door raises the ask rather than writing. Its handler consults the shared gate
        // and hands off to `bulk_rate_request`; `BulkOp::Rate` appears nowhere inside it.
        let ask_key = between("app.on_bulk_rate_ask_key(move |stars: i32|", "\n    }\n    {");
        assert!(ask_key.contains("support::bulk_actions_allowed("), "the gate is re-asked Rust-side");
        assert!(ask_key.contains("req(stars, a.get_same_key_clear(), support::BulkArmSource::Keys)"));
        assert!(!ask_key.contains("BulkOp::Rate"), "the menu row must NEVER rate directly");

        // The Unmark door: the same shape, landing in the SAME executor the `u` key uses.
        let unmark = between("app.on_bulk_unmark(move ||", "\n    }\n    {");
        assert!(unmark.contains("support::bulk_actions_allowed("), "the gate is re-asked Rust-side");
        assert!(unmark.contains("apply(BulkOp::Clear, &support::bulk_targets(&ss.borrow()))"));

        // (v) THE TRASH ARM — one call, and it is the key's. No delete of its own.
        let trash_arm = between("mm::TAG_TRASH => {", "mm::TAG_XMP =>");
        assert!(trash_arm.contains("app.invoke_delete_key();"), "the arm is the delete KEY's twin");
        for forbidden in ["BulkOp::", "trash_", "delete_shot", "set_del_bulk"] {
            assert!(!trash_arm.contains(forbidden), "the Trash row must not {forbidden} on its own");
        }
        // …and the body it reaches sends a live selection to the count-naming confirm.
        let dk = between("app.on_delete_key(move ||", "open_delete_confirm(&app,");
        in_order(&dk, &["if bulk_armed(&app, &ss_dk)", "open_bulk_dk();"]);
        let confirm = between("let open_bulk_delete: Rc<dyn Fn()> = {", "app.on_bulk_delete_ask");
        in_order(
            &confirm,
            &["support::bulk_delete_title(", "a.set_del_bulk(true);", "a.set_confirm_kind(6);"],
        );
    }

    /// The fingerprint moves on every menu-visible state change (retitle/re-check trigger).
    #[test]
    fn fingerprint_tracks_changes() {
        let km = default_keymap();
        let mut s = snap_with(&km);
        let f0 = fingerprint(&s);
        s.rating = 4;
        let f1 = fingerprint(&s);
        assert_ne!(f0, f1);
        s.keys.flag = "G".into();
        let f2 = fingerprint(&s);
        assert_ne!(f1, f2);
        // v0.9.24: the info-panel fields are in the snapshot, so the tick's fingerprint compare is
        // what re-titles the row when ANY surface (i key, − button, stub, seg, ctx menu) moves the
        // state — the same mechanism the "(P)" suffixes already ride, plus menuNeedsUpdate: on open.
        s.info_min = !s.info_min;
        let f3 = fingerprint(&s);
        assert_ne!(f2, f3, "a minimize/expand moves the fingerprint → the row re-titles");
        s.info_open = !s.info_open;
        let f4 = fingerprint(&s);
        assert_ne!(f3, f4, "an off/on move does too");
        // v0.8.97: …and so does the DEVELOP panel's open flag, which the OFF title now reads — the
        // row has to re-title from "Show info panel" to "Show panels" when the second panel goes.
        s.raw_open = !s.raw_open;
        assert_ne!(f4, fingerprint(&s), "hiding/showing the develop panel re-titles the row");
    }
    // Falsifier: gate native sorting only on modal; Allow photo edits removes
    // that modal while Rust still refuses every partial-folder sort request.
    #[test]
    fn refinement_native_sort_waits_for_discovery_in_both_opening_modes() {
        let mut s=MenuSnapshot { has_photo:true,count:21,opening_photo:true,..Default::default() };
        for modal in [false,true] {
            s.modal=modal;
            for tag in [TAG_SORT_PARENT,TAG_SORT_M0,TAG_SORT_M6,TAG_SORT_ASC,TAG_SORT_DESC] {
                assert!(!enabled_for(tag,&s),"sort tag {tag} promised a refused action");
            }
        }
        s.opening_photo=false; s.modal=false;
        assert!(enabled_for(TAG_SORT_M0,&s));
    }

    /// Fix round 1 (1 November, F13-apply-4, owner: "Apply rotations only"; L21/L26): the Mac Sort
    /// rows grey while Apply rotations runs, mirroring `apply_pick`'s guard, which refuses then.
    /// Other file operations don't set the switch, so they leave the rows lit.
    ///
    /// FALSIFIER (L28): drop `&& !s.applying_rotations` from any of the three sort arms in
    /// `enabled_for` → the matching tag's assert reddens.
    #[test]
    fn the_sort_rows_grey_while_apply_rotations_runs() {
        let tags = [TAG_SORT_PARENT, TAG_SORT_M0, TAG_SORT_M0 + 1, TAG_SORT_M0 + 2, TAG_SORT_M0 + 3, TAG_SORT_M0 + 4,
            TAG_SORT_M0 + 5, TAG_SORT_M6, TAG_SORT_ASC, TAG_SORT_DESC];
        assert_eq!(TAG_SORT_M6 - TAG_SORT_M0, 6, "seven method rows");
        let mut s = MenuSnapshot { has_photo: true, count: 5, applying_rotations: true, ..Default::default() };
        for tag in tags {
            assert!(!enabled_for(tag, &s), "sort tag {tag} lit while Apply rotations runs");
        }
        s.applying_rotations = false;
        for tag in tags {
            assert!(enabled_for(tag, &s), "sort tag {tag} grey with no Apply running");
        }
    }

    /// Language packs, round 2 (PLAN §4): the bar finds Help, File and View by their ids, never by
    /// their drawn titles, which a language pack translates. Under the pseudo-language every title
    /// is wrapped in ⟦…⟧, and the three roles `mac_menu` realizes still land on the same menus: the
    /// Window menu goes before Help and Help becomes the help menu; File (Open Recent) and View
    /// (Sort) run explicit enablement.
    ///
    /// FALSIFIERS: decide a role from the drawn title instead of the id — `help_menu:
    /// self.title() == "Help"`, `window_before: self.title() == "Help"`, or `explicit_enables:
    /// matches!(self.title(), "File" | "View")` — and the matching row fails.
    #[cfg(falcon_pseudo_language)]
    #[test]
    fn the_bar_finds_help_file_and_view_by_id_when_their_titles_are_translated() {
        let km = default_keymap();
        let s = snap_with(&km);
        let roles = |menus: &[(TopMenu, Vec<ItemDef>)], pick: fn(TopMenuRole) -> bool| -> Vec<TopMenu> {
            menus.iter().filter(|(id, _)| pick(id.role())).map(|(id, _)| *id).collect()
        };
        for pseudo in [false, true] {
            if pseudo {
                crate::i18n::use_pseudo_language();
            }
            let menus = top_menus(&s);
            let titles: Vec<&str> = menus.iter().map(|(id, _)| id.title()).collect();
            let want: [&str; 5] = if pseudo {
                ["⟦File⟧", "⟦Edit⟧", "⟦Photo⟧", "⟦View⟧", "⟦Help⟧"]
            } else {
                ["File", "Edit", "Photo", "View", "Help"]
            };
            assert_eq!(titles, want, "only the drawn titles change (pseudo={pseudo})");
            let drawn: Vec<&str> = build_menus(&s).iter().map(|(t, _)| *t).collect();
            assert_eq!(drawn, want, "the bar draws the same titles (pseudo={pseudo})");
            assert_eq!(roles(&menus, |r| r.help_menu), [TopMenu::Help], "the help menu (pseudo={pseudo})");
            assert_eq!(roles(&menus, |r| r.window_before), [TopMenu::Help], "Window goes before Help (pseudo={pseudo})");
            assert_eq!(
                roles(&menus, |r| r.explicit_enables),
                [TopMenu::File, TopMenu::View],
                "explicit enablement (pseudo={pseudo})"
            );
            // The menus found by id are the ones holding what each role exists for.
            let items_of = |id: TopMenu| &menus.iter().find(|(m, _)| *m == id).unwrap().1;
            assert!(items_of(TopMenu::File).iter().any(|i| i.tag == TAG_RECENT_PARENT));
            assert!(items_of(TopMenu::View).iter().any(|i| i.tag == TAG_SORT_PARENT));
            assert!(items_of(TopMenu::Help).iter().any(|i| i.tag == TAG_HELP_LOG));
            if pseudo {
                let log = items_of(TopMenu::Help).iter().find(|i| i.tag == TAG_HELP_LOG).unwrap();
                assert_eq!(log.title, "⟦Show log file⟧", "the rows' drawn titles are translated too");
            }
        }
        crate::i18n::use_english();
    }

}
