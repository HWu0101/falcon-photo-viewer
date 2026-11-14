//! Plural rules for language packs, shared by `build_translations.rs` (which writes Slint's `.po`
//! headers for the interface crate and the pack list for the app) and
//! `i18n.rs` (which picks a Rust message's plural form). `scripts/check-translations.py` keeps a
//! copy of the ids and categories; `languages.json` names a rule by its `id`.
//!
//! A rule's `categories` are CLDR plural category names in gettext form order: the form at index
//! `plural_index(n)` is the one gettext's `plural=` expression selects for `n`. Adding a rule means
//! adding a row here, a matching arm in `plural_index`, and the same row in the checker.

pub struct PluralRuleDef {
    pub id: &'static str,
    pub categories: &'static [&'static str],
    /// The gettext `Plural-Forms` header Slint's compiler parses for the bundled `.po`.
    pub po_header: &'static str,
}

pub const PLURAL_RULES: &[PluralRuleDef] = &[
    // Chinese, Japanese, Korean and others with one form for every count.
    PluralRuleDef { id: "none", categories: &["other"], po_header: "nplurals=1; plural=0;" },
    // `one` for exactly 1, `other` for every other count including 0 (English, German, Dutch).
    // Not French: it treats 0 like 1 and needs its own rule.
    PluralRuleDef { id: "one_other", categories: &["one", "other"], po_header: "nplurals=2; plural=(n != 1);" },
];

pub fn plural_rule(id: &str) -> Option<&'static PluralRuleDef> {
    PLURAL_RULES.iter().find(|r| r.id == id)
}

/// Index into `categories` for a count; must agree with `po_header`.
pub fn plural_index(id: &str, n: u64) -> usize {
    match id {
        "one_other" => usize::from(n != 1),
        _ => 0,
    }
}

/// The test-only pseudo-language `xx-TEST` is bundled into every build except a release build, so
/// a shipped package can never offer or contain it.
pub fn include_pseudo_language(profile: &str) -> bool {
    profile != "release"
}

pub const PSEUDO_CODE: &str = "xx-TEST";
pub const PSEUDO_RULE: &str = "one_other";
