//! v0.9.3 (P3, PLAN §66): the per-OS user-visible-string + glyph table — the cross-platform
//! transition series' "platform seams" round. ONE place holds every OS-specific noun/verb/glyph
//! so the macOS arm becomes a table swap, not a code sweep. STRINGS ONLY — the actual shell calls
//! (reveal-in-explorer, recycle) are BEHAVIOR and stay cfg-seamed at their sites (main.rs / tick.rs).
//!
//! HARD CONSTRAINT (Windows trunk): every field's Windows value below is the pre-refactor literal
//! VERBATIM. `windows_strings_unchanged` (see main.rs `#[cfg(test)] mod platform_pin`) pins each,
//! byte-for-byte — that test IS the byte-identity contract. The macOS arm is INERT scaffolding on
//! Windows (compiled out under `#[cfg(not(windows))]`); its values are the sensible Mac equivalents
//! the branch's later Mac wiring will surface (Finder, Trash, ⇧/⌘, Info.plist associations).

/// The OS-specific user-visible strings + modifier glyphs, resolved once per target at compile time.
/// Any field that names a Recycle-Bin/Trash concept, an Explorer/Finder reveal, a modifier glyph, or a
/// cloud provider lives here.
///
/// Language packs (round 2): the fields that are words are marked with `tr_noop!` in both tables and
/// translated with `i18n::tr` where they are shown; the values themselves stay the English literals
/// (`windows_strings_unchanged`). Brand names, key chords and modifier glyphs are not marked.
#[allow(dead_code)] // some fields are macOS-arm scaffolding, deliberately unread on the Windows trunk
                    // (they pin the future Mac wiring's Windows counterpart via windows_strings_unchanged).
pub(crate) struct PlatformStrings {
    /// Reveal-in-file-manager menu verb. Windows: "Reveal in Explorer" / macOS: "Reveal in Finder".
    pub(crate) reveal_verb: &'static str,
    /// v1.0.0-rc (item 27, sheet 2.1 B3 b): the file manager as a NOUN — "Explorer" / "Finder".
    /// The counted reveal row composes `Reveal {n} in {file_manager}` (`support::reveal_label`), which
    /// the single verb above cannot supply: a count has to sit between the verb and the noun. Two
    /// strings naming one application is exactly how a table swap reworders half a pair, so a row
    /// asserts `reveal_verb == format!("Reveal in {file_manager}")` on both arms.
    pub(crate) file_manager: &'static str,
    /// The OS trash concept as a NOUN, woven into Rust-composed toasts/bodies. "Recycle Bin" / "Trash".
    pub(crate) trash_noun: &'static str,
    /// Confirm-button label to send a shot to the trash. "Move to Recycle Bin" / "Move to Trash".
    /// (macOS-arm scaffolding: the confirm-dialog buttons live as ui.rs literals this round — see the
    /// deliberate-literal record in PLAN §66 — so this is inert on Windows until that dialog is routed.)
    pub(crate) move_to_trash: &'static str,
    /// Confirm-button label to empty ./Rejected to the trash. "Empty to Bin" / "Empty to Trash".
    /// (Same scaffolding note as `move_to_trash`.)
    pub(crate) empty_to_trash: &'static str,
    /// v0.8.119 (design-sweep O24): the Selection panel's compact INVOKER label for that same op —
    /// "Empty → Bin" / "Empty → Trash". It is a separate field rather than a reuse of
    /// `empty_to_trash` because the panel button's arrow grammar and the dialog button's sentence
    /// grammar are different registers; sharing the string would have silently reworded the Windows
    /// button, which O24 explicitly leaves alone. Both nouns still come from this one table, which
    /// is the point of the fix (L26: the gated body's every invoker speaks the body's noun).
    pub(crate) empty_action_label: &'static str,
    /// The cloud provider named in the "file not downloaded" note. "OneDrive" / "iCloud Drive".
    pub(crate) cloud_hint: &'static str,
    /// key→glyph SHIFT prefix (the `pretty_key` transformer's home). "Shift+" / "⇧".
    pub(crate) mod_shift: &'static str,
    /// key→glyph primary-modifier prefix. "Ctrl+" / "⌘". (macOS-arm scaffolding: Windows composes the
    /// copy chip from `copy_shortcut_display` directly, so this prefix is inert on the trunk.)
    pub(crate) mod_ctrl: &'static str,
    /// Delete-key chip label. "Del" / "⌦".
    pub(crate) key_delete: &'static str,
    /// Copy-shortcut chip display. "Ctrl+C" / "⌘C".
    pub(crate) copy_shortcut_display: &'static str,
    /// v0.8.137 (W3): deselect-shortcut chip display. "Ctrl+D" / "⌘D". A FIXED chord like Ctrl+C —
    /// it is spelled in the FocusScope ladder, not in ACTIONS, so `menu_shortcut` cannot supply it
    /// and it comes from this table. Sourcing it here rather than from the live keymap also makes it
    /// immune to the `refresh_menu_shortcuts` staleness that only keymap-derived caps can suffer.
    pub(crate) deselect_shortcut_display: &'static str,
    /// v1.0.0-rc (item 27, sheet 2.1 B7): select-all-shortcut chip display. "Ctrl+A" / "⌘A". The
    /// same class as `copy` and `deselect` above — Ctrl+A is spelled in the FocusScope ladder, not in
    /// ACTIONS, so `menu_shortcut` cannot supply it and it cannot go stale after a rebind.
    pub(crate) select_all_shortcut_display: &'static str,
    /// v0.8.184 (D1): undo-shortcut chip display. "Ctrl+Z" / "⌘Z". Same class as `copy` and
    /// `deselect` above — a FIXED chord spelled in the FocusScope ladder rather than in ACTIONS, so
    /// `menu_shortcut` cannot supply it and it cannot go stale after a rebind.
    pub(crate) undo_shortcut_display: &'static str,
    /// v0.8.184 (D1): redo-shortcut chip display. Windows shows "Ctrl+Y" — the FocusScope accepts
    /// BOTH Ctrl+Y and Ctrl+Shift+Z, and Ctrl+Y is the shorter of the two and the Windows
    /// convention. The Mac arm names ⇧⌘Z, which is that platform's convention and is the arm the
    /// ladder's Ctrl+Shift+Z branch answers.
    pub(crate) redo_shortcut_display: &'static str,
    /// Whether the Settings "FILE ASSOCIATIONS" card is shown. Windows registers Open-With per-user
    /// (the checkbox card); macOS (v0.9.16, Round B) shows the DEFAULT-HANDLER card instead — the
    /// Info.plist declares the types, and the card's per-family Make-default/Reset buttons drive
    /// LaunchServices (`assoc-mac-mode` picks which card mounts; both true now).
    pub(crate) file_assoc_visible: bool,
    /// v1.0.0-rc TAIL (OWNER RULING, CMYK): does the Settings list mount the **CMYK JPEG decoding**
    /// row? WINDOWS ONLY, and the reason is the L26 one rather than a capability one: the row's own
    /// label and caption name *Windows'* colour-managed codec, and a platform noun and the
    /// behaviour it describes have to travel together. macOS has an equivalent codec (Image I/O),
    /// but it is a different noun, a different default profile and a different answer — offering it
    /// under this row's words would be the merge round's own mistake repeated. The Mac therefore
    /// keeps Falcon's portable conversion, which is what it has had since the rider landed.
    pub(crate) cmyk_route_visible: bool,
    /// v0.8.65 (C3/M9, the M12 pattern): the Output-colour-gamut card's main description — it names
    /// the OS colour-management concept ("Windows Auto Colour Management" vs ColorSync in System
    /// Settings → Displays), so it routes per-OS. Windows value = the pre-v0.8.65 Slint literal
    /// VERBATIM (pinned by `windows_strings_unchanged`); the Slint Text now binds `gamut-copy`.
    ///
    /// v0.8.193 (D1) — THE ONE EDIT this string has ever taken, so the "VERBATIM" claim above is now
    /// "verbatim except this clause": "turn OFF Windows Auto Color Management" → "turn OFF Windows
    /// Auto Colour Management and HDR". Two reasons, both measured this round: HDR has the SAME
    /// double-conversion effect as Auto Colour Management (§0.4) and the sentence named only one of
    /// them, so a photographer in HDR followed advice that could not fix what he was seeing; and
    /// "Color" was the file's one American spelling in a British-spelling app. COPY IS THE OWNER'S —
    /// flagged in the round's close for his veto.
    pub(crate) gamut_copy: &'static str,
    /// v0.8.119 (design-sweep O29): why the Settings PERFORMANCE "GPU JPEG decode (nvJPEG)" row is
    /// unavailable on this machine. Empty means "never say anything" — on macOS `accel_avail` is
    /// always true, so the row never disables there and the line must never mount.
    pub(crate) accel_unavail_note: &'static str,
}

/// The Windows trunk table — every string is the pre-v0.9.3 literal, VERBATIM (pinned).
#[cfg(windows)]
pub(crate) const PLATFORM: PlatformStrings = PlatformStrings {
    reveal_verb: tr_noop!("Reveal in Explorer"),
    file_manager: tr_noop!("Explorer"),
    trash_noun: tr_noop!("Recycle Bin"),
    move_to_trash: tr_noop!("Move to Recycle Bin"),
    empty_to_trash: tr_noop!("Empty to Bin"),
    empty_action_label: tr_noop!("Empty → Bin"),
    cloud_hint: "OneDrive",
    mod_shift: "Shift+",
    mod_ctrl: "Ctrl+",
    key_delete: "Del",
    copy_shortcut_display: "Ctrl+C",
    deselect_shortcut_display: "Ctrl+D",
    select_all_shortcut_display: "Ctrl+A",
    undo_shortcut_display: "Ctrl+Z",
    redo_shortcut_display: "Ctrl+Y",
    file_assoc_visible: true,
    cmyk_route_visible: true,
    gamut_copy: tr_noop!("Photos are converted to this gamut for display. Set it to match your monitor's gamut mode — and on a wide-gamut monitor turn OFF Windows Auto Colour Management and HDR. Interface colours follow this setting too, so the app matches colour-managed tools like Photoshop."),
    accel_unavail_note: tr_noop!("No CUDA/nvJPEG GPU detected — decodes run on the CPU."),
};

/// The macOS arm — inert scaffolding on the Windows trunk (compiled out). Sensible Mac equivalents
/// the branch's later Mac wiring surfaces; NOT byte-identity-pinned (the pin is Windows-only).
#[cfg(not(windows))]
pub(crate) const PLATFORM: PlatformStrings = PlatformStrings {
    reveal_verb: tr_noop!("Reveal in Finder"),
    file_manager: tr_noop!("Finder"),
    trash_noun: tr_noop!("Trash"),
    move_to_trash: tr_noop!("Move to Trash"),
    empty_to_trash: tr_noop!("Empty to Trash"),
    empty_action_label: tr_noop!("Empty → Trash"),
    cloud_hint: "iCloud Drive",
    mod_shift: "⇧",
    mod_ctrl: "⌘",
    key_delete: "⌦",
    copy_shortcut_display: "⌘C",
    deselect_shortcut_display: "⌘D",
    select_all_shortcut_display: "⌘A",
    undo_shortcut_display: "⌘Z",
    redo_shortcut_display: "⇧⌘Z",
    file_assoc_visible: true, // v0.9.16 (Round B): the Mac default-handler card is live
    cmyk_route_visible: false, // v1.0.0-rc TAIL: the row names Windows' codec — see the field's doc

    gamut_copy: tr_noop!("Photos are converted to this gamut for display. Set it to match your display's gamut — macOS assigns each display's profile in System Settings → Displays (ColorSync). Interface colours follow this setting too, so the app matches colour-managed tools like Photoshop."),
    // macOS: `accel_avail` is hardwired true, so the row never disables and this never mounts.
    accel_unavail_note: "",
};

// ── v0.9.4 (§66): the PURE toast/note composers (the v0.9.3-audit YELLOW fix) ──
// The six I/O-embedded toast/note templates that weave a `PLATFORM` field into user-visible text.
// Before v0.9.4 these `format!`s lived INLINE in the trash/recover/decode fns (which do real Recycle-Bin
// / UI work and so can't run under `cargo test`), and `windows_strings_unchanged` RE-TYPED each template
// as a local `format!` copy — a tautology that never touched the production string, so a template edit
// there would have passed the pin untouched. Extracting the composition here makes each a PURE `-> String`
// fn: the I/O fns now delegate to it, and the pin asserts THIS output against the spelled-out literal — so
// the test and production share one composition. Reading `PLATFORM.<field>` keeps the macOS arm free
// (these surface "Trash"/"iCloud Drive" wording automatically), exactly as the inline `format!`s did.

/// Language packs (round 2): the bin noun as shown, for the composers below. Each sentence is one
/// whole message with the noun as its `{bin}` value; Windows English is unchanged (pinned).
pub(crate) fn bin_noun() -> &'static str {
    crate::i18n::tr(PLATFORM.trash_noun)
}

/// Delete-recycle success toast: "Sent N file(s) to the Recycle Bin". Routes `trash_noun`.
/// v0.8.131 (F-P11 rule 5): a PARTIAL recycle, stated as an OBSERVATION rather than an inference.
/// The app knows two things after re-statting: how many files left the folder, and how many did
/// not. It does not know WHY any single one stayed (an `IFileOperationProgressSink` is the only
/// route to that, and it is a future item), so the sentence says what was counted and stops.
pub(crate) fn partial_recycle_toast_text(ok: usize, total: usize, failed: usize) -> String {
    tr_plural!(
        total,
        "Sent {ok} of {n} file to the {bin} — {failed} did not go",
        "Sent {ok} of {n} files to the {bin} — {failed} did not go",
        ok = ok,
        bin = bin_noun(),
        failed = failed
    )
}

/// Language packs (round 2): "file(s)" for every count is today's English, so both English forms
/// are equal; a pack can still give its own forms.
pub(crate) fn recycle_toast_text(n: usize) -> String {
    tr_plural!(n, "Sent {n} file(s) to the {bin}", "Sent {n} file(s) to the {bin}", bin = bin_noun())
}

/// Empty-./Rejected success toast: "Sent N file(s) from ./Rejected to the Recycle Bin." Routes `trash_noun`.
pub(crate) fn empty_rejected_toast_text(n: usize) -> String {
    tr_plural!(
        n,
        "Sent {n} file(s) from ./Rejected to the {bin}.",
        "Sent {n} file(s) from ./Rejected to the {bin}.",
        bin = bin_noun()
    )
}

/// Recover failure (bin unreadable): "Couldn't recover NAME — the Recycle Bin is unavailable". Routes
/// `trash_noun`. Windows-only since v0.9.18: the Mac recover renames the captured Trash URL directly and
/// never lists the Trash, so it has no "unavailable" path (a missing entry routes `recover_missing_text`).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn recover_unavailable_text(name: &str) -> String {
    tr_format!("Couldn't recover {name} — the {bin} is unavailable", name = name, bin = bin_noun())
}

/// Recover failure (no longer present): "Couldn't recover NAME — it's no longer in the Recycle Bin".
/// Routes `trash_noun`. Used on BOTH platforms now (v0.9.18): Windows when the bin holds no match;
/// macOS when every captured Trash entry is gone (the user emptied the Trash — see `mac_recover_message`).
pub(crate) fn recover_missing_text(name: &str) -> String {
    tr_format!("Couldn't recover {name} — it's no longer in the {bin}", name = name, bin = bin_noun())
}

/// Cross-folder recover-refused toast: "Deleted file was in another folder — recover it from the Recycle Bin". Routes `trash_noun`.
pub(crate) fn cross_folder_recover_text() -> String {
    tr_format!("Deleted file was in another folder — recover it from the {bin}", bin = bin_noun())
}

/// Cloud-placeholder decode note: "Cloud file — not downloaded. Falcon will retry; check OneDrive if this persists." Routes `cloud_hint`.
pub(crate) fn cloud_not_downloaded_text() -> String {
    tr_format!(
        "Cloud file — not downloaded. Falcon will retry; check {provider} if this persists.",
        provider = PLATFORM.cloud_hint
    )
}

/// HEIC decode-failure note. Windows names the OS HEVC/HEIF Image Extension (which may not be installed
/// — WIC needs it); macOS decodes HEIC through Image I/O, which is built into the OS, so a HEIC failure
/// there is a genuinely corrupt or unsupported-variant file, not a missing codec. The Windows arm is the
/// pre-v0.9.9 literal VERBATIM (pinned by `windows_strings_unchanged`); the macOS arm is inert on the trunk.
pub(crate) fn heic_decode_failed_text() -> String {
    #[cfg(windows)]
    {
        crate::i18n::tr("Couldn't decode — the HEVC/HEIF Image Extension may not be installed").to_string()
    }
    #[cfg(not(windows))]
    {
        crate::i18n::tr("Couldn't decode this HEIC — the file may be corrupt or an unsupported variant").to_string()
    }
}


/// v0.9.12 (C2/M12): the "Empty ./Rejected" confirmation BODY. Same per-OS split as the title — the
/// Windows arm is the pre-v0.9.12 `.slint` literal VERBATIM (pinned; also the property default); the
/// macOS arm routes both "Recycle Bin" and "the bin" to the Trash noun.
pub(crate) fn empty_rejected_confirm_body() -> String {
    // v0.8.125 (C6a / Round-B W1-5): the NOUN is ROUTED, not typed. Both arms used to spell their
    // bin out by hand, which is the same one-noun-two-copies shape the O24 sweep item found on the
    // Selection panel's invoker — a rename in `trash_noun` would have moved the headline and left
    // this body naming the other OS's bin. The two arms stay separate because the SENTENCE differs
    // (macOS says "the Trash" twice; Windows says "the OS Recycle Bin" then "the bin"), and each
    // reads naturally on its own platform; what they now share is the single source of the word.
    // The Windows bytes are unchanged and stay pinned by `windows_strings_unchanged`.
    // Language packs (round 2): one whole message per platform, the noun as `{bin}`.
    #[cfg(windows)]
    {
        tr_format!(
            "Sends every file currently in ./Rejected to the OS {bin} — recoverable from there until you empty the bin. Photos NOT yet moved to ./Rejected are untouched.",
            bin = bin_noun()
        )
    }
    #[cfg(not(windows))]
    {
        tr_format!(
            "Sends every file currently in ./Rejected to the {bin} — recoverable from there until you empty the {bin}. Photos NOT yet moved to ./Rejected are untouched.",
            bin = bin_noun()
        )
    }
}

/// v0.9.9 (P6): the Settings → PERFORMANCE hardware-decode toggle label. It is ONE toggle enabling the
/// platform's accelerated decode slot (turning it off routes every decode to the CPU), so its name must
/// match the platform's engine: nvJPEG (CUDA GPU decode) on Windows, Image I/O (the Apple media engine —
/// hardware JPEG + native HEIC) on macOS. The Windows arm is the pre-v0.9.9 `.slint` literal VERBATIM
/// (pinned by `windows_strings_unchanged`); the macOS arm is inert on the Windows trunk.
pub(crate) fn accel_toggle_label() -> String {
    #[cfg(windows)]
    {
        crate::i18n::tr("GPU JPEG decode (nvJPEG)").to_string()
    }
    #[cfg(not(windows))]
    {
        crate::i18n::tr("Hardware JPEG/HEIC decode (Image I/O)").to_string()
    }
}

/// v0.8.71 (Round B assoc): the Settings HEIC-association row's explanation when the row is disabled
/// because no WIC HEIF codec is installed (the boot-time read-only probe — `heic_codec_state`; since
/// 1.1.0-rc.1 only a definite "not installed" disables the row).
/// Windows-only in practice: the row only disables there (macOS decodes HEIC natively via Image I/O,
/// so the row never fades and this line never mounts). One unconditional composition — the string
/// names the Windows codec story because only Windows can surface it. Pinned byte-for-byte by
/// `windows_strings_unchanged` (a NEW Windows-visible string, owner-directed wording).
pub(crate) fn heic_assoc_missing_tip() -> String {
    crate::i18n::tr("Requires the HEVC/HEIF Image Extensions from the Microsoft Store").to_string()
}

/// 2026-10-26 (round 2a) — **THE PERFORMANCE MODE'S LINE NAMES THE MACHINE IN THE PLATFORM'S WORD**
/// (the owner's draft says "this PC"; on a Mac it is "this Mac" — L26, a platform noun travels with
/// its platform). The `.slint` property's default is the Windows sentence.
pub(crate) fn mode_performance_line() -> String {
    #[cfg(not(target_os = "macos"))]
    {
        crate::i18n::tr("Fastest browsing this PC can sustain. Uses the most memory and processor.").to_string()
    }
    #[cfg(target_os = "macos")]
    {
        crate::i18n::tr("Fastest browsing this Mac can sustain. Uses the most memory and processor.").to_string()
    }
}

/// v1.0 MERGE TAIL [B-R1] — **THE BATTERY SWITCH'S BATTERY SAVER CLAUSE, ROUTED** (round 2a; it was
/// the Efficiency card's Auto clause).
///
/// "Use Power saver on battery power" also acts while Windows' Battery saver is on (`mode_in_effect`
/// reads `Dc || saver`), and the row says so under the switch. On macOS `saver` is structurally false
/// (Low Power Mode is not read), so the clause is empty there and its line does not mount: a row that
/// advertises an input its own implementation does not read is L17 (a control's label is a
/// contract). When Low Power Mode is wired, this is the one string to widen.
pub(crate) fn battery_switch_clause() -> String {
    #[cfg(windows)]
    {
        crate::i18n::tr("Also while Windows' Battery saver is on.").to_string()
    }
    #[cfg(not(windows))]
    {
        String::new()
    }
}

/// v1.0 MERGE TAIL [B-R3, L26] — **THE OVERWRITE WARNING'S BIN NOUN, ROUTED.**
///
/// The kind-8 export-collision modal warns that overwritten deliverables are "gone for good, not
/// moved to the Recycle Bin". Present on `c1ff8f8`, ABSENT from `a0de9e4` (the ladder is v0.8.187 /
/// v0.8.192 trunk work, later than the branch point), so the merge is the commit that puts a
/// hard-typed Windows noun into a destructive-action modal on macOS — and it does so three lines
/// above the button ladder the merge itself routed (N14 took mac's `empty-confirm-label` /
/// `delete-confirm-label`). Its two sibling BODIES in the same ternary are already Rust-composed
/// (`delete-body`, `empty-confirm-body`); kind 8 alone was a literal. L26: gate a shared body and
/// you owe the mirror on EVERY invoker.
///
/// One composition, both platforms, with the noun from the table — the `empty_rejected_confirm_body`
/// pattern. The Windows bytes are unchanged and pinned.
pub(crate) fn overwrite_confirm_body() -> String {
    tr_format!(
        "Overwrite re-renders those files and replaces them in place \u{2014} the old versions are gone for good, not moved to the {bin} (each replacement is atomic, so a file is never left half-written). Skip existing leaves them untouched and exports the rest. To keep both versions, cancel and turn on \u{201c}Append the preset name to filenames\u{201d}.",
        bin = bin_noun()
    )
}

/// Language packs (round 2): the phrase beside the dialog's big count holds no number, and a
/// counted message must, so the two English phrases are two whole messages chosen by English's
/// rule (`n == 1`) — exactly the `one_other` rule, and one text twice for a `none` language.
pub(crate) fn empty_confirm_title(n: usize) -> String {
    if n == 1 {
        tr_format!("file in ./Rejected → {bin}", bin = bin_noun())
    } else {
        tr_format!("files in ./Rejected → {bin}", bin = bin_noun())
    }
}
