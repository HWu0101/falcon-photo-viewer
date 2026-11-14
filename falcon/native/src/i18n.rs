//! Languages: which one Falcon speaks this run, and the lookup every marked message goes through.
//!
//! English is the source language. Other languages are language packs:
//! `translations/languages.json` lists them, and `translations/<code>.json` holds each one's
//! messages keyed by the exact English text (docs/development/translations.md). `build.rs` embeds
//! the packs; nothing is read from disk or the network at run time.
//!
//! Rust text is marked with `tr`, `tr_format!`, `tr_plural!` and `tr_noop!`; Slint text with
//! `@tr("falcon" => …)`. The language is chosen once at startup (`init`) and applies until restart.
//! Logs stay English: never mark a `log_event` line.

use std::collections::HashMap;
use std::fmt::{Display, Write as _};
use std::sync::OnceLock;

#[allow(dead_code)]
#[path = "i18n_rules.rs"]
mod rules;

/// One entry of `translations/languages.json`, embedded by `build.rs`.
pub(crate) struct Language {
    pub(crate) code: &'static str,
    /// The language's own name, shown in the Settings picker.
    pub(crate) name: &'static str,
    /// System locale tags this language serves, such as `zh-Hans` or `zh`.
    pub(crate) matches: &'static [&'static str],
    pub(crate) plural: &'static str,
    pub(crate) pack: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/packs.rs"));

/// What `Settings.language` holds for English; an empty preference follows the system.
pub(crate) const ENGLISH: &str = "en";
const ENGLISH_NAME: &str = "English";

enum Entry {
    Text(&'static str),
    /// One form per plural category, in the rule's order.
    Forms(Vec<&'static str>),
}

struct Pack {
    plural: &'static str,
    entries: HashMap<&'static str, Entry>,
}

impl Pack {
    /// Parsed once; its strings live for the rest of the run, so lookups return `&'static str` and
    /// functions that return `&'static str` keep their signatures. The build (`build_translations.rs`,
    /// run by the interface crate's `ui/build.rs`) has already refused a
    /// malformed pack, so `None` here only guards against the impossible.
    fn parse(json: &str, plural: &'static str) -> Option<Pack> {
        let doc: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json).ok()?;
        let categories = rules::plural_rule(plural)?.categories;
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let mut entries = HashMap::with_capacity(doc.len());
        for (key, value) in doc {
            let entry = match value {
                serde_json::Value::String(s) => Entry::Text(leak(s)),
                serde_json::Value::Object(mut forms) => Entry::Forms(
                    categories
                        .iter()
                        .map(|c| match forms.remove(*c) {
                            Some(serde_json::Value::String(s)) => Some(leak(s)),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()?,
                ),
                _ => return None,
            };
            entries.insert(leak(key), entry);
        }
        Some(Pack { plural, entries })
    }
}

struct Active {
    code: &'static str,
    pack: Pack,
}

/// This run's language; `None` inside is English. Set once by `init`.
static ACTIVE: OnceLock<Option<Active>> = OnceLock::new();
/// The language the system settings alone would pick, for the picker's "System (…)" label.
static SYSTEM_CHOICE: OnceLock<Option<usize>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Tests choose a language per thread and default to English whatever the machine's locale.
    static TEST_ACTIVE: std::cell::Cell<Option<&'static Active>> = const { std::cell::Cell::new(None) };
}

fn active() -> Option<&'static Active> {
    #[cfg(test)]
    {
        TEST_ACTIVE.with(|a| a.get())
    }
    #[cfg(not(test))]
    {
        ACTIVE.get().and_then(Option::as_ref)
    }
}

/// True while Falcon speaks English, the language the messages are written in.
pub(crate) fn is_source() -> bool {
    active().is_none()
}

/// The code of this run's language, `"en"` for English.
pub(crate) fn running_code() -> &'static str {
    active().map_or(ENGLISH, |a| a.code)
}

/// A plain message in this run's language; English when the pack lacks it.
pub(crate) fn tr(english: &'static str) -> &'static str {
    match active().and_then(|a| a.pack.entries.get(english)) {
        Some(Entry::Text(t)) => t,
        _ => english,
    }
}

/// The template for a counted message: the pack's form for `n`, keyed by the English singular.
pub(crate) fn plural_template(one: &'static str, other: &'static str, n: u64) -> &'static str {
    if let Some(a) = active() {
        if let Some(Entry::Forms(forms)) = a.pack.entries.get(one) {
            if let Some(form) = forms.get(rules::plural_index(a.pack.plural, n)) {
                return form;
            }
        }
    }
    if n == 1 { one } else { other }
}

/// Put already formatted values into a template's `{name}` placeholders. `{{` and `}}` are literal
/// braces, as in `format!`. An unknown name stays as written; the checker refuses such a template.
pub(crate) fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if let Some(end) = tail.strip_prefix('{').and_then(|t| t.find(['{', '}'])) {
            let name = &tail[1..1 + end];
            if tail.as_bytes()[1 + end] == b'}' {
                if let Some((_, v)) = args.iter().find(|(n, _)| *n == name) {
                    let _ = write!(out, "{v}");
                    rest = &tail[end + 2..];
                    continue;
                }
            }
        }
        out.push_str(&tail[..1]);
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Compile-time check behind `tr_format!` and `tr_plural!`: every `{…}` in `template` is a bare
/// name listed in `names`, and every listed name appears, except `optional`, which may be left out.
/// `{{` and `}}` are literal braces.
///
/// Without it, `tr_format!("Copied {count} photos")` would compile: English `format!` quietly
/// captures a local `count`, while a translation has no value to put there and shows `{count}`
/// (round-1 review R2). A const fn, so the macros reject such a call while compiling.
pub(crate) const fn placeholders_match(template: &str, names: &[&str], optional: &str) -> bool {
    const fn same(a: &[u8], b: &[u8], from: usize, to: usize) -> bool {
        if a.len() != to - from {
            return false;
        }
        let mut k = 0;
        while k < a.len() {
            if a[k] != b[from + k] {
                return false;
            }
            k += 1;
        }
        true
    }
    let b = template.as_bytes();
    if names.len() > 64 {
        return false;
    }
    let mut seen: u64 = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            if i + 1 < b.len() && b[i + 1] == b'{' {
                i += 2;
                continue;
            }
            let start = i + 1;
            let mut end = start;
            while end < b.len() && b[end] != b'}' {
                if !(b[end] == b'_' || b[end].is_ascii_alphanumeric()) {
                    return false; // `{}`-style, `{0}`-style and `{x:.1}`-style are all refused
                }
                end += 1;
            }
            if end == b.len() || end == start || b[start].is_ascii_digit() {
                return false;
            }
            let mut j = 0;
            let mut found = false;
            while j < names.len() {
                if same(names[j].as_bytes(), b, start, end) {
                    seen |= 1 << j;
                    found = true;
                }
                j += 1;
            }
            if !found {
                return false;
            }
            i = end + 1;
        } else if b[i] == b'}' {
            if i + 1 < b.len() && b[i + 1] == b'}' {
                i += 2;
                continue;
            }
            return false;
        } else {
            i += 1;
        }
    }
    let mut j = 0;
    while j < names.len() {
        let o = optional.as_bytes();
        if seen & (1 << j) == 0 && !same(o, names[j].as_bytes(), 0, names[j].len()) {
            return false;
        }
        j += 1;
    }
    true
}

/// `tr_format!("Copied {photos} photos", photos = n)`: a message with named values.
///
/// For English it expands to `format!` with the same literal, so English output is unchanged. A
/// translation may put the placeholders in any order. Every placeholder needs a named value and
/// every value a placeholder; `placeholders_match` refuses anything else at compile time. Values
/// use `Display`; format a value first when it needs a precision (`age = format!("{age:.1}")`),
/// because templates hold bare `{name}` placeholders only.
macro_rules! tr_format {
    ($en:literal $(, $name:ident = $val:expr)* $(,)?) => {{
        const _: () = assert!(
            $crate::i18n::placeholders_match($en, &[$(::core::stringify!($name)),*], ""),
            "tr_format!: each placeholder needs a named value, and each value a placeholder"
        );
        match ($(&$val,)*) {
            ($($name,)*) => {
                if $crate::i18n::is_source() {
                    ::std::format!($en $(, $name = $name)*)
                } else {
                    $crate::i18n::fill(
                        $crate::i18n::tr($en),
                        &[$((::core::stringify!($name), $name as &dyn ::core::fmt::Display)),*],
                    )
                }
            }
        }
    }};
}

/// `tr_plural!(count, "Delete {n} photo", "Delete {n} photos", folder = name)`: a counted message.
///
/// `{n}` is the count. English picks the first form for 1 and the second otherwise, exactly as the
/// old `if n == 1` code did. A pack gives one form per plural category of its language, under the
/// English singular. Checked at compile time: the plural form uses `{n}` and every named value;
/// the singular may leave out only `{n}` ("Delete this photo from {place}?").
macro_rules! tr_plural {
    ($count:expr, $one:literal, $other:literal $(, $name:ident = $val:expr)* $(,)?) => {{
        const _: () = {
            assert!(
                $crate::i18n::placeholders_match($one, &["n" $(, ::core::stringify!($name))*], "n"),
                "tr_plural!: the singular may leave out only n; each other placeholder needs a value"
            );
            assert!(
                $crate::i18n::placeholders_match($other, &["n" $(, ::core::stringify!($name))*], ""),
                "tr_plural!: the plural form uses n and each named value, and nothing else"
            );
        };
        match (&$count, $(&$val,)*) {
            (n, $($name,)*) => {
                $crate::i18n::fill(
                    $crate::i18n::plural_template(
                        $one,
                        $other,
                        ::core::convert::TryFrom::try_from(*n).unwrap_or(u64::MAX),
                    ),
                    &[("n", n as &dyn ::core::fmt::Display) $(, (::core::stringify!($name), $name as &dyn ::core::fmt::Display))*],
                )
            }
        }
    }};
}

/// Marks English text kept in a `const` or `static` table so the checker finds it. Translate it
/// with `tr(..)` where it is shown.
#[allow(unused_macros)]
macro_rules! tr_noop {
    ($en:literal) => {
        $en
    };
}

// ── Choosing the language ──────────────────────────────────────────────────────────────────────

/// Index into `langs` of the language for a saved preference, or `None` for English. An empty or
/// unknown preference follows the system.
pub(crate) fn resolve(pref: &str, system: &[String], langs: &[Language]) -> Option<usize> {
    if pref.eq_ignore_ascii_case(ENGLISH) {
        return None;
    }
    match langs.iter().position(|l| l.code.eq_ignore_ascii_case(pref)) {
        Some(i) => Some(i),
        None => system_language(system, langs),
    }
}

/// The first of the system's preferred languages that a pack serves; English when English comes
/// first or nothing matches.
pub(crate) fn system_language(system: &[String], langs: &[Language]) -> Option<usize> {
    for raw in system {
        let tag = normalise(raw);
        let mut parts = tag.split('-');
        let lang = parts.next().unwrap_or_default();
        if lang.is_empty() || lang == "c" || lang == "posix" {
            continue;
        }
        if lang == ENGLISH {
            return None;
        }
        let rest: Vec<&str> = parts.collect();
        let script = rest.iter().find(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic()));
        let region_script = rest.iter().find_map(|r| region_script(lang, r));
        let mut candidates = vec![tag.clone()];
        match (script, region_script) {
            (Some(s), _) => candidates.push(format!("{lang}-{s}")),
            (None, Some(s)) => candidates.push(format!("{lang}-{s}")),
            // Only a tag that says nothing about its script may fall back to the bare language, so
            // Traditional Chinese never lands on a Simplified pack.
            (None, None) => candidates.push(lang.to_string()),
        }
        for c in &candidates {
            let serves = |l: &Language| l.code.eq_ignore_ascii_case(c) || l.matches.iter().any(|m| m.eq_ignore_ascii_case(c));
            if let Some(i) = langs.iter().position(serves) {
                return Some(i);
            }
        }
    }
    None
}

/// `zh_SG.UTF-8` → `zh-sg`: underscores to hyphens, encoding and modifier dropped, lower case.
fn normalise(tag: &str) -> String {
    let tag = tag.trim();
    let tag = tag.split(['.', '@']).next().unwrap_or_default();
    tag.replace('_', "-").to_ascii_lowercase()
}

/// The script a region implies for a language whose regions differ in script.
fn region_script(lang: &str, region: &str) -> Option<&'static str> {
    match (lang, region) {
        ("zh", "cn" | "sg" | "my") => Some("hans"),
        ("zh", "tw" | "hk" | "mo") => Some("hant"),
        _ => None,
    }
}

/// Choose this run's language from the saved preference and the system's preferred languages.
/// Call once, after the settings load and before the first window. Returns the system's tags and
/// the chosen code for the boot log.
pub(crate) fn init(pref: &str) -> (Vec<String>, &'static str) {
    let system: Vec<String> = sys_locale::get_locales().collect();
    let _ = SYSTEM_CHOICE.set(system_language(&system, LANGUAGES));
    let active = resolve(pref, &system, LANGUAGES).and_then(|i| {
        let l = &LANGUAGES[i];
        Pack::parse(l.pack, l.plural).map(|pack| Active { code: l.code, pack })
    });
    let code = active.as_ref().map_or(ENGLISH, |a| a.code);
    let _ = ACTIVE.set(active);
    (system, code)
}

/// Point Slint's bundled translations at this run's language. Call right after the first window
/// exists: it replaces Slint's own guess from the system locale. A build without packs has nothing
/// bundled, and English needs nothing.
pub(crate) fn select_for_slint() {
    let _ = slint::select_bundled_translation(running_code());
}

// ── Fonts for Chinese characters ───────────────────────────────────────────────────────────────

/// The system's own Simplified Chinese UI fonts, in order of preference, while a Simplified Chinese
/// pack is active; empty for every other language. Inter has no Chinese characters, and Slint gives
/// the text shaper no language, so the system picks a font for Han characters by itself. On Windows
/// that can be a Japanese font without the Simplified forms, which then draw as empty boxes.
pub(crate) fn han_fallback_families(code: &str) -> &'static [&'static str] {
    let simplified = matches!(code, "zh-CN" | "zh-SG") || code.starts_with("zh-Hans");
    if !simplified {
        return &[];
    }
    if cfg!(windows) {
        &["Microsoft YaHei UI", "Microsoft YaHei", "DengXian", "SimHei"]
    } else if cfg!(target_os = "macos") {
        &["PingFang SC", "Hiragino Sans GB", "Heiti SC"]
    } else {
        &["Noto Sans CJK SC", "Source Han Sans SC", "WenQuanYi Micro Hei"]
    }
}

/// Whose fonts draw Han characters, as a language code for [`han_fallback_families`]: the running
/// language when it is Chinese. Otherwise Simplified Chinese — the only pack in Han characters, and
/// Settings → Language always shows its name — unless the computer's own languages put Japanese,
/// Korean or Traditional Chinese first, whose system fonts already suit (empty: keep the system's).
pub(crate) fn han_font_code(running: &str, system: &[String]) -> &'static str {
    if !han_fallback_families(running).is_empty() {
        return "zh-CN";
    }
    for tag in system.iter().map(|t| normalise(t)) {
        let tag = tag.to_ascii_lowercase();
        let lang = tag.split('-').next().unwrap_or("");
        if lang == "ja" || lang == "ko" {
            return "";
        }
        if lang == "zh" {
            let traditional = !tag.contains("-hans")
                && (tag.contains("-hant") || ["-tw", "-hk", "-mo"].iter().any(|r| tag.ends_with(r)));
            return if traditional { "" } else { "zh-CN" };
        }
    }
    "zh-CN"
}

/// Points the Han script's fallback in `fonts` at [`han_fallback_families`] for `code`, using only the
/// families installed. Latin text keeps Inter. Returns the families used (empty: nothing changed).
pub(crate) fn apply_han_fallback(fonts: &mut slint::fontique_010::fontique::Collection, code: &str) -> Vec<&'static str> {
    use slint::fontique_010::fontique::{FallbackKey, Script};
    let found: Vec<_> =
        han_fallback_families(code).iter().filter_map(|&name| fonts.family_id(name).map(|id| (name, id))).collect();
    if !found.is_empty() {
        // The key the shaper asks with: the Han script and no language.
        fonts.set_fallbacks(FallbackKey::new(Script::from_bytes(*b"Hani"), None), found.iter().map(|&(_, id)| id));
    }
    found.into_iter().map(|(name, _)| name).collect()
}

// ── The Settings picker ────────────────────────────────────────────────────────────────────────

/// "System (<language>)", "English", then each pack in its own name.
pub(crate) fn picker_options(langs: &[Language], system_choice: Option<usize>) -> Vec<String> {
    let system_name = system_choice.and_then(|i| langs.get(i)).map_or(ENGLISH_NAME, |l| l.name);
    let mut options = vec![tr_format!("System ({language})", language = system_name), ENGLISH_NAME.to_string()];
    options.extend(langs.iter().map(|l| l.name.to_string()));
    options
}

/// The picker row for a saved preference; an unknown code shows as System, which is what it does.
pub(crate) fn picker_index(pref: &str, langs: &[Language]) -> i32 {
    if pref.eq_ignore_ascii_case(ENGLISH) {
        return 1;
    }
    langs.iter().position(|l| l.code.eq_ignore_ascii_case(pref)).map_or(0, |i| i as i32 + 2)
}

/// The preference a picker row saves.
pub(crate) fn picker_pref(index: i32, langs: &[Language]) -> &'static str {
    match index {
        1 => ENGLISH,
        i if i >= 2 => langs.get((i - 2) as usize).map_or("", |l| l.code),
        _ => "",
    }
}

/// Whether a picker row names a different language from the one running, so a restart is needed.
pub(crate) fn picker_needs_restart(index: i32, langs: &[Language], system_choice: Option<usize>, running: &str) -> bool {
    let chosen = match index {
        0 => system_choice,
        1 => None,
        i => langs.get((i - 2) as usize).map(|_| (i - 2) as usize),
    };
    chosen.map_or(ENGLISH, |i| langs[i].code) != running
}

/// The live app's picker: its options, the row for `pref`, and the two functions the callbacks use.
pub(crate) fn app_picker_options() -> Vec<String> {
    picker_options(LANGUAGES, SYSTEM_CHOICE.get().copied().flatten())
}
pub(crate) fn app_picker_index(pref: &str) -> i32 {
    picker_index(pref, LANGUAGES)
}
pub(crate) fn app_picker_pref(index: i32) -> &'static str {
    picker_pref(index, LANGUAGES)
}
pub(crate) fn app_picker_needs_restart(index: i32) -> bool {
    picker_needs_restart(index, LANGUAGES, SYSTEM_CHOICE.get().copied().flatten(), running_code())
}

// ── Tests ──────────────────────────────────────────────────────────────────────────────────────

/// Speak a test pack on this test thread until `use_english`. Leaked once per call; tests only.
#[cfg(test)]
pub(crate) fn use_test_pack(code: &'static str, plural: &'static str, json: &str) {
    let pack = Pack::parse(json, plural).expect("a valid test pack");
    let active: &'static Active = Box::leak(Box::new(Active { code, pack }));
    TEST_ACTIVE.with(|a| a.set(Some(active)));
}

#[cfg(test)]
pub(crate) fn use_english() {
    TEST_ACTIVE.with(|a| a.set(None));
}

/// The pseudo-language `xx-TEST`: every message wrapped in `⟦…⟧`. Not in release builds.
#[cfg(all(test, falcon_pseudo_language))]
pub(crate) fn use_pseudo_language() {
    use_test_pack(rules::PSEUDO_CODE, rules::PSEUDO_RULE, PSEUDO_PACK);
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn lang(code: &'static str, name: &'static str, matches: &'static [&'static str], plural: &'static str) -> Language {
        Language { code, name, matches, plural, pack: "{}" }
    }
    const FIXTURE: &[Language] = &[
        lang("zh-CN", "简体中文", &["zh-Hans", "zh-CN", "zh-SG", "zh"], "none"),
        lang("de", "Deutsch", &["de"], "one_other"),
    ];
    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }
    fn system(t: &[&str]) -> Option<&'static str> {
        system_language(&tags(t), FIXTURE).map(|i| FIXTURE[i].code)
    }

    /// FALSIFIER: drop the region-to-script step (let `(None, Some(_))` fall to the bare language)
    /// and zh-TW lands on the Simplified pack.
    #[test]
    fn system_tags_resolve_to_the_pack_that_serves_them() {
        for t in ["zh", "zh-CN", "zh_SG.UTF-8", "zh-Hans", "zh-Hans-US", "zh-Hans-CN", "ZH-cn"] {
            assert_eq!(system(&[t]), Some("zh-CN"), "{t}");
        }
        for t in ["zh-TW", "zh-HK", "zh-Hant-CN", "zh_TW.UTF-8", "zh-Hant"] {
            assert_eq!(system(&[t]), None, "{t} is Traditional Chinese, which has no pack");
        }
        assert_eq!(system(&["de-AT"]), Some("de"));
        assert_eq!(system(&["de_DE@euro"]), Some("de"));
        assert_eq!(system(&[]), None);
        assert_eq!(system(&["C", "POSIX"]), None);
    }

    /// FALSIFIER: read only the first preferred language.
    #[test]
    fn later_preferences_count_and_english_first_wins() {
        assert_eq!(system(&["fr-FR", "zh-CN"]), Some("zh-CN"));
        assert_eq!(system(&["en-US", "zh-CN"]), None, "English is preferred before Chinese");
        assert_eq!(system(&["fr-FR", "de-CH", "zh-CN"]), Some("de"));
    }

    #[test]
    fn a_saved_choice_overrides_the_system_and_an_unknown_one_follows_it() {
        let zh = tags(&["zh-CN"]);
        assert_eq!(resolve("", &zh, FIXTURE), Some(0));
        assert_eq!(resolve("en", &zh, FIXTURE), None);
        assert_eq!(resolve("de", &zh, FIXTURE), Some(1));
        assert_eq!(resolve("tlh", &zh, FIXTURE), Some(0), "an unknown code follows the system");
        assert_eq!(resolve("tlh", &tags(&["en-GB"]), FIXTURE), None);
    }

    /// FALSIFIER: substitute values by position instead of by name.
    #[test]
    fn named_values_land_in_place_whatever_their_order() {
        use_test_pack(
            "xx",
            "one_other",
            r#"{"Copied {photos} photos ({files} files)": "{files} files, {photos} photos {{ok}}"}"#,
        );
        let (photos, files) = (3, 7);
        let s = tr_format!("Copied {photos} photos ({files} files)", photos = photos, files = files);
        assert_eq!(s, "7 files, 3 photos {ok}");
        use_english();
        let s = tr_format!("Copied {photos} photos ({files} files)", photos = photos, files = files);
        assert_eq!(s, "Copied 3 photos (7 files)");
    }

    /// FALSIFIER: swap the `n == 1` choice in `plural_template`, or the categories in `plural_index`.
    #[test]
    fn counted_messages_keep_english_and_follow_the_packs_rule() {
        let msg = |n: usize| tr_plural!(n, "Delete this photo from {place}?", "Delete {n} photos from {place}?", place = "the folder");
        assert_eq!(msg(1), "Delete this photo from the folder?");
        assert_eq!(msg(0), "Delete 0 photos from the folder?");
        assert_eq!(msg(2), "Delete 2 photos from the folder?");
        use_test_pack(
            "xx",
            "one_other",
            r#"{"Delete this photo from {place}?": {"one": "[one] {place}", "other": "[{n}] {place}"}}"#,
        );
        assert_eq!(msg(1), "[one] the folder");
        assert_eq!(msg(5), "[5] the folder");
        use_test_pack("yy", "none", r#"{"Delete this photo from {place}?": {"other": "{place}: {n}"}}"#);
        assert_eq!(msg(1), "the folder: 1");
        assert_eq!(msg(9), "the folder: 9");
        use_english();
    }

    /// FALSIFIER: return the pack lookup without the English fallback.
    #[test]
    fn a_message_the_pack_lacks_stays_english() {
        use_test_pack("xx", "one_other", r#"{"Open": "Ouvrir"}"#);
        assert_eq!(tr("Open"), "Ouvrir");
        assert_eq!(tr("Close"), "Close");
        assert_eq!(tr_plural!(2usize, "{n} photo", "{n} photos"), "2 photos");
        assert_eq!(running_code(), "xx");
        use_english();
        assert_eq!(tr("Open"), "Open");
        assert_eq!(running_code(), "en");
    }

    /// The 1.0.13 release check drew Chinese as empty boxes on Windows: with no language given, the
    /// system's Han fallback was a Japanese font without the Simplified forms. With the zh-CN pack
    /// active, Han characters fall back to the system's Simplified Chinese font; other languages
    /// leave the system's choice alone. Uses the real system fonts, in its own collection.
    /// FALSIFIER: make `han_fallback_families` return `&[]` for zh-CN (or set the fallback on another
    /// key) and the zh-CN half fails wherever one of the fonts is installed.
    #[test]
    fn chinese_characters_fall_back_to_the_systems_simplified_chinese_font() {
        use slint::fontique_010::fontique::{Collection, CollectionOptions, FallbackKey, Script};
        assert!(han_fallback_families("en").is_empty() && han_fallback_families("zh-TW").is_empty());
        let mut fonts = Collection::new(CollectionOptions { shared: false, system_fonts: true });
        // The test's own list (not the function's), so a broken list fails rather than skips.
        let expected: &[&str] = if cfg!(windows) {
            &["Microsoft YaHei UI", "Microsoft YaHei", "DengXian", "SimHei"]
        } else if cfg!(target_os = "macos") {
            &["PingFang SC", "Hiragino Sans GB", "Heiti SC"]
        } else {
            &["Noto Sans CJK SC", "Source Han Sans SC", "WenQuanYi Micro Hei"]
        };
        let installed: Vec<&str> = expected.iter().copied().filter(|name| fonts.family_id(name).is_some()).collect();
        if installed.is_empty() {
            eprintln!("skipped: no Simplified Chinese system font on this machine");
            return;
        }
        assert_eq!(apply_han_fallback(&mut fonts, "zh-CN"), installed, "every installed family is used, in order");
        let first = fonts.fallback_families(FallbackKey::new(Script::from_bytes(*b"Hani"), None)).next();
        assert_eq!(first, fonts.family_id(installed[0]), "the shaper's Han key now resolves to that font");
        let mut keep = Collection::new(CollectionOptions { shared: false, system_fonts: true });
        assert!(apply_han_fallback(&mut keep, "").is_empty(), "an empty code keeps the system's choice");
    }

    /// Review of the font fix (R1): Settings → Language shows "简体中文" in every language, so Han
    /// characters use the Simplified Chinese fonts unless the computer's own languages call for
    /// another Han font (Japanese, Korean, Traditional Chinese first).
    /// FALSIFIERS: return "" for a running language that is not Chinese and the English rows fail;
    /// drop the Japanese/Korean or the Traditional check and its rows fail.
    #[test]
    fn han_characters_follow_the_running_language_or_the_computers() {
        let s = |tags: &[&str]| tags.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        for (running, system, want) in [
            ("zh-CN", s(&["ja-JP"]), "zh-CN"),
            ("en", s(&["en-US"]), "zh-CN"),
            ("en", s(&[]), "zh-CN"),
            ("en", s(&["en-GB", "zh-Hans-CN"]), "zh-CN"),
            ("en", s(&["zh_CN.UTF-8"]), "zh-CN"),
            ("en", s(&["zh-Hans-HK"]), "zh-CN"),
            ("en", s(&["fr-FR", "ja"]), ""),
            ("en", s(&["ko-KR", "zh-CN"]), ""),
            ("en", s(&["zh-TW"]), ""),
            ("en", s(&["zh-Hant-HK"]), ""),
            ("en", s(&["zh-HK"]), ""),
        ] {
            assert_eq!(han_font_code(running, &system), want, "{running} on {system:?}");
        }
    }

    /// The compile-time placeholder check behind both macros (round-1 review R2 and R4).
    /// FALSIFIER: return `true` from `placeholders_match` when a placeholder matches no name, and
    /// the review's `tr_format!("Copied {count} photos")` compiles again.
    #[test]
    fn every_placeholder_needs_a_named_value() {
        assert!(!placeholders_match("Copied {count} photos", &[], ""), "R2: a placeholder with no value");
        assert!(placeholders_match("Copied {count} photos", &["count"], ""));
        assert!(!placeholders_match("Copied photos", &["count"], ""), "a value with no placeholder");
        assert!(!placeholders_match("{0} files", &["x"], ""));
        assert!(!placeholders_match("{} files", &[], ""));
        assert!(!placeholders_match("{size:.1} MB", &["size"], ""));
        assert!(!placeholders_match("unclosed {name", &["name"], ""));
        assert!(!placeholders_match("stray } brace", &[], ""));
        assert!(placeholders_match("{{literal}} and }} {name}", &["name"], ""));
        // A plural's singular may leave out only the count.
        assert!(placeholders_match("Delete this photo from {place}?", &["n", "place"], "n"));
        assert!(!placeholders_match("Delete this photo?", &["n", "place"], "n"), "R4: the place stays");
        assert!(!placeholders_match("Delete {n} photos?", &["n", "place"], ""), "the plural uses every value");
    }

    #[test]
    fn fill_matches_format_for_plain_templates() {
        let a = 12;
        let b = "x";
        assert_eq!(fill("{a}-{b} {{a}} }} {{", &[("a", &a), ("b", &b)]), format!("{a}-{b} {{a}} }} {{"));
        assert_eq!(fill("{missing} {a", &[("a", &a)]), "{missing} {a");
        assert_eq!(fill("", &[]), "");
        assert_eq!(fill("⟦{a}⟧", &[("a", &a)]), "⟦12⟧");
    }

    /// FALSIFIER: hard-code the picker's options.
    #[test]
    fn the_picker_is_built_from_the_language_list() {
        assert_eq!(picker_options(FIXTURE, None), ["System (English)", "English", "简体中文", "Deutsch"]);
        assert_eq!(picker_options(FIXTURE, Some(0))[0], "System (简体中文)");
        assert_eq!(picker_options(&[], None), ["System (English)", "English"]);
        for (pref, row) in [("", 0), ("en", 1), ("zh-CN", 2), ("de", 3), ("tlh", 0)] {
            assert_eq!(picker_index(pref, FIXTURE), row, "{pref}");
        }
        for (row, pref) in [(0, ""), (1, "en"), (2, "zh-CN"), (3, "de"), (9, ""), (-1, "")] {
            assert_eq!(picker_pref(row, FIXTURE), pref, "{row}");
        }
        assert!(!picker_needs_restart(0, FIXTURE, None, "en"));
        assert!(!picker_needs_restart(1, FIXTURE, None, "en"), "System already means English here");
        assert!(picker_needs_restart(2, FIXTURE, None, "en"));
        assert!(picker_needs_restart(0, FIXTURE, Some(1), "en"));
        assert!(!picker_needs_restart(3, FIXTURE, None, "de"));
    }

    /// FALSIFIER: make `include_pseudo_language` return true.
    #[test]
    fn release_builds_leave_the_pseudo_language_out() {
        assert!(rules::include_pseudo_language("debug"));
        assert!(!rules::include_pseudo_language("release"));
    }

    #[test]
    fn every_listed_pack_parses_and_names_a_known_rule() {
        for l in LANGUAGES {
            assert!(rules::plural_rule(l.plural).is_some(), "{}", l.code);
            assert!(Pack::parse(l.pack, l.plural).is_some(), "{}", l.code);
        }
    }

    #[cfg(falcon_pseudo_language)]
    #[test]
    fn the_pseudo_language_wraps_marked_rust_messages() {
        use_pseudo_language();
        assert_eq!(picker_options(&[], None)[0], "⟦System (English)⟧");
        use_english();
    }
}
