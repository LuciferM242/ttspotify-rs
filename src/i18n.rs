//! Lightweight i18n engine.
//!
//! English is embedded in the binary (`src/i18n/en.lang`) and is both the
//! fallback and the translator's template. Other languages are plain
//! `key = value` text files (`<config_dir>/lang/<code>.lang`) loaded at
//! startup. Any missing key or unknown language falls back to English, so a
//! partial translation never breaks a reply.
//!
//! Templates use named `{slot}` placeholders. Translators may move a slot
//! anywhere in their sentence (substitution is by name, not position), but
//! must not rename slots or invent new ones; an unknown slot is left visible
//! rather than silently dropped.

use std::collections::{BTreeSet, HashMap};

/// The language code of the embedded fallback catalog.
pub const ENGLISH: &str = "en";

/// Special `.lang` entry holding the language's own display name.
const LANGUAGE_NAME_KEY: &str = "language_name";

const EMBEDDED_EN: &str = include_str!("i18n/en.lang");

/// Defines `Key`, `Key::id()`, and `Key::ALL` from a single list so the enum,
/// the `.lang` file ids, and the completeness check can never drift apart.
macro_rules! keys {
    ($($variant:ident => $id:literal),* $(,)?) => {
        /// A translatable message. One variant per string the bot actually
        /// sends; the id is the key used in `.lang` files. Referencing a
        /// variant (not a raw string) makes a typo a compile error.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Key {
            $($variant),*
        }

        impl Key {
            /// The stable snake_case id used in `.lang` files.
            pub fn id(self) -> &'static str {
                match self {
                    $(Key::$variant => $id),*
                }
            }

            /// Every key, for completeness and validation checks.
            pub const ALL: &'static [Key] = &[$(Key::$variant),*];
        }
    };
}

keys! {
    LangSet => "lang_set",
}

/// Parse `.lang` file text into a key -> template map.
///
/// Format: `key = value` per line; `#` comments and blank lines ignored;
/// everything after the first `=` is the value (so values may contain `=`);
/// key and value are trimmed; `\n` in a value becomes a newline. A malformed
/// line is skipped with a warning — it never invalidates the rest of the file.
pub fn parse_lang(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some((key, value)) => {
                let key = key.trim();
                if key.is_empty() {
                    tracing::warn!("Ignoring translation line {} with empty key", idx + 1);
                    continue;
                }
                map.insert(key.to_string(), value.trim().replace("\\n", "\n"));
            }
            None => {
                tracing::warn!("Ignoring malformed translation line {}: {line}", idx + 1);
            }
        }
    }
    map
}

/// Fill named `{slot}` placeholders in a template.
///
/// Single-pass by name: slots may appear in any order, an unknown slot is left
/// visible as-is, and substituted values are never re-scanned (a value that
/// happens to contain braces cannot trigger a second substitution).
pub fn fill(template: &str, args: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let brace = &rest[start..];
        match brace.find('}') {
            Some(end) => {
                let name = &brace[1..end];
                match args.iter().find(|(n, _)| *n == name) {
                    Some((_, value)) => out.push_str(value),
                    None => out.push_str(&brace[..=end]),
                }
                rest = &brace[end + 1..];
            }
            None => {
                // Unmatched '{' — copy the remainder verbatim.
                out.push_str(brace);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Extract the set of `{slot}` names in a template (for validation).
fn slots_of(template: &str) -> BTreeSet<String> {
    let mut slots = BTreeSet::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let brace = &rest[start..];
        match brace.find('}') {
            Some(end) => {
                slots.insert(brace[1..end].to_string());
                rest = &brace[end + 1..];
            }
            None => break,
        }
    }
    slots
}

/// All loaded languages: the embedded English plus any runtime `.lang` files.
pub struct Catalog {
    langs: HashMap<String, HashMap<String, String>>,
}

impl Catalog {
    /// A catalog holding only the embedded English.
    pub fn new_embedded() -> Catalog {
        let mut langs = HashMap::new();
        langs.insert(ENGLISH.to_string(), parse_lang(EMBEDDED_EN));
        Catalog { langs }
    }

    /// Register a runtime language (code is lowercased).
    pub fn add_language(&mut self, code: &str, entries: HashMap<String, String>) {
        self.langs.insert(code.to_lowercase(), entries);
    }

    fn template(&self, lang: &str, id: &str) -> Option<&str> {
        self.langs.get(lang)?.get(id).map(String::as_str)
    }

    /// Translate `key` into `lang`, falling back to English on a missing key
    /// or unknown language, then fill the `{slot}` placeholders. As a last
    /// resort (a key absent even from English — prevented by the completeness
    /// test) the key id itself is returned so the gap is visible.
    pub fn t(&self, lang: &str, key: Key, args: &[(&str, String)]) -> String {
        let id = key.id();
        let template = self
            .template(lang, id)
            .or_else(|| self.template(ENGLISH, id))
            .unwrap_or(id);
        fill(template, args)
    }

    /// The language's self-declared display name, or its code if absent.
    pub fn language_name(&self, code: &str) -> String {
        self.langs
            .get(code)
            .and_then(|m| m.get(LANGUAGE_NAME_KEY))
            .cloned()
            .unwrap_or_else(|| code.to_string())
    }

    /// All loaded language codes, sorted.
    pub fn codes(&self) -> Vec<String> {
        let mut codes: Vec<String> = self.langs.keys().cloned().collect();
        codes.sort();
        codes
    }

    pub fn has_language(&self, code: &str) -> bool {
        self.langs.contains_key(code)
    }
}

/// Structured result of validating one loaded language against English.
pub struct LangValidation {
    pub code: String,
    /// How many of the bot's keys this language translates.
    pub present: usize,
    /// Total number of translatable keys.
    pub total: usize,
    /// Keys whose `{slot}` set differs from the English template (renamed,
    /// dropped, or invented placeholders).
    pub slot_mismatches: Vec<String>,
    /// Keys in the file that the bot does not know (typos or removed keys).
    pub unknown_keys: Vec<String>,
}

/// Validate a loaded language: coverage count, placeholder-slot mismatches,
/// and unknown keys. Returns structured results so callers can log or test.
pub fn validate(catalog: &Catalog, code: &str) -> LangValidation {
    let total = Key::ALL.len();
    let mut present = 0;
    let mut slot_mismatches = Vec::new();
    for key in Key::ALL {
        let id = key.id();
        if let Some(translated) = catalog.template(code, id) {
            present += 1;
            if let Some(english) = catalog.template(ENGLISH, id) {
                if slots_of(translated) != slots_of(english) {
                    slot_mismatches.push(id.to_string());
                }
            }
        }
    }
    let known: BTreeSet<&str> = Key::ALL.iter().map(|k| k.id()).collect();
    let mut unknown_keys: Vec<String> = catalog
        .langs
        .get(code)
        .map(|entries| {
            entries
                .keys()
                .filter(|k| k.as_str() != LANGUAGE_NAME_KEY && !known.contains(k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    unknown_keys.sort();
    LangValidation {
        code: code.to_string(),
        present,
        total,
        slot_mismatches,
        unknown_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_lang --

    #[test]
    fn parse_lang_basic_trim_and_comments() {
        let map = parse_lang(
            "# a comment\n\n  paused =  Pausiert  \nlang_set = Sprache: {language}\n",
        );
        assert_eq!(map.get("paused").unwrap(), "Pausiert");
        assert_eq!(map.get("lang_set").unwrap(), "Sprache: {language}");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_lang_keeps_equals_in_value() {
        let map = parse_lang("formula = a = b + c");
        assert_eq!(map.get("formula").unwrap(), "a = b + c");
    }

    #[test]
    fn parse_lang_skips_malformed_line_not_whole_file() {
        let map = parse_lang("good = ok\nthis line has no equals sign\nalso = fine");
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("good").unwrap(), "ok");
        assert_eq!(map.get("also").unwrap(), "fine");
    }

    #[test]
    fn parse_lang_skips_empty_key() {
        let map = parse_lang("= orphan value\nok = yes");
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("ok"));
    }

    #[test]
    fn parse_lang_unescapes_newline() {
        let map = parse_lang(r"two_lines = first\nsecond");
        assert_eq!(map.get("two_lines").unwrap(), "first\nsecond");
    }

    #[test]
    fn parse_lang_reads_language_name() {
        let map = parse_lang("language_name = Deutsch");
        assert_eq!(map.get("language_name").unwrap(), "Deutsch");
    }

    // -- fill --

    #[test]
    fn fill_substitutes_named_slots() {
        assert_eq!(
            fill("Volume: {percent}%", &[("percent", "40".to_string())]),
            "Volume: 40%"
        );
    }

    #[test]
    fn fill_allows_reordered_slots() {
        // Translator moved the slots around; substitution is by name.
        assert_eq!(
            fill(
                "Max {max}%, now {percent}%",
                &[("percent", "30".to_string()), ("max", "90".to_string())]
            ),
            "Max 90%, now 30%"
        );
    }

    #[test]
    fn fill_leaves_unknown_slot_visible() {
        assert_eq!(fill("Hello {nobody}", &[]), "Hello {nobody}");
    }

    #[test]
    fn fill_handles_no_slots_and_unmatched_brace() {
        assert_eq!(fill("Paused", &[]), "Paused");
        assert_eq!(fill("odd { brace", &[]), "odd { brace");
    }

    #[test]
    fn fill_does_not_rescan_substituted_values() {
        // A value containing a slot-shaped string must not be substituted again.
        assert_eq!(
            fill(
                "{a} {b}",
                &[("a", "{b}".to_string()), ("b", "two".to_string())]
            ),
            "{b} two"
        );
    }

    // -- Catalog::t --

    fn catalog_with_de() -> Catalog {
        let mut c = Catalog::new_embedded();
        c.add_language(
            "de",
            parse_lang("language_name = Deutsch\nlang_set = Sprache auf {language} gesetzt"),
        );
        c
    }

    #[test]
    fn t_uses_language_when_present() {
        let c = catalog_with_de();
        assert_eq!(
            c.t("de", Key::LangSet, &[("language", "Deutsch".to_string())]),
            "Sprache auf Deutsch gesetzt"
        );
    }

    #[test]
    fn t_falls_back_to_english_for_unknown_language() {
        let c = Catalog::new_embedded();
        assert_eq!(
            c.t("xx", Key::LangSet, &[("language", "English".to_string())]),
            "Language set to English"
        );
    }

    #[test]
    fn t_falls_back_to_english_for_missing_key() {
        let mut c = Catalog::new_embedded();
        // A language file with no lang_set entry at all.
        c.add_language("pt", parse_lang("language_name = Portugues"));
        assert_eq!(
            c.t("pt", Key::LangSet, &[("language", "Portugues".to_string())]),
            "Language set to Portugues"
        );
    }

    #[test]
    fn language_name_falls_back_to_code() {
        let c = catalog_with_de();
        assert_eq!(c.language_name("de"), "Deutsch");
        assert_eq!(c.language_name("zz"), "zz");
    }

    // -- completeness --

    #[test]
    fn every_key_has_an_english_entry() {
        let c = Catalog::new_embedded();
        for key in Key::ALL {
            assert!(
                c.template(ENGLISH, key.id()).is_some(),
                "en.lang is missing an entry for key `{}`",
                key.id()
            );
        }
    }

    // -- validation --

    #[test]
    fn validate_reports_coverage_and_mismatches() {
        let mut c = Catalog::new_embedded();
        // lang_set drops {language} and adds a typo'd key.
        c.add_language("de", parse_lang("lang_set = Sprache gesetzt\npausd = Pausiert"));
        let v = validate(&c, "de");
        assert_eq!(v.present, 1);
        assert_eq!(v.total, Key::ALL.len());
        assert_eq!(v.slot_mismatches, vec!["lang_set".to_string()]);
        assert_eq!(v.unknown_keys, vec!["pausd".to_string()]);
    }

    #[test]
    fn validate_accepts_moved_slots() {
        let mut c = Catalog::new_embedded();
        // Same slot, different position: valid, no mismatch.
        c.add_language("de", parse_lang("lang_set = {language} ist jetzt aktiv"));
        let v = validate(&c, "de");
        assert!(v.slot_mismatches.is_empty());
    }
}
