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
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

/// The language code of the embedded fallback catalog.
pub const ENGLISH: &str = "en";

/// Special `.lang` entry holding the language's own display name.
const LANGUAGE_NAME_KEY: &str = "language_name";

const EMBEDDED_EN: &str = include_str!("i18n/en.lang");

/// Translations bundled into the binary, so they work identically for release
/// downloads and source builds with no files to install. A same-code file in
/// `<config_dir>/lang/` overrides these per key. English is NOT in this list:
/// it is the authoritative fallback and cannot be overridden.
const EMBEDDED_LANGS: &[(&str, &str)] = &[
    ("es", include_str!("i18n/es.lang")),
    ("pt", include_str!("i18n/pt.lang")),
    ("ru", include_str!("i18n/ru.lang")),
];

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
    // Language
    LangSet => "lang_set",
    // Playback
    Searching => "searching",
    LoadingTrack => "loading_track",
    Resuming => "resuming",
    Paused => "paused",
    NothingToPlay => "nothing_to_play",
    RestartingTrack => "restarting_track",
    LoadingLiked => "loading_liked",
    NothingPlaying => "nothing_playing",
    CurrentTrack => "current_track",
    SearchCancelled => "search_cancelled",
    // Volume and seek
    VolumeSet => "volume_set",
    VolumeShow => "volume_show",
    VolumeRange => "volume_range",
    SeekForward => "seek_forward",
    SeekBackward => "seek_backward",
    SeekUsage => "seek_usage",
    // Queue
    QueueCleared => "queue_cleared",
    IndexStartsAtOne => "index_starts_at_one",
    NoTrackAtPosition => "no_track_at_position",
    Removed => "removed",
    QueueRmUsage => "queue_rm_usage",
    // Modes
    ModeRepeatTrack => "mode_repeat_track",
    ModeRepeatQueue => "mode_repeat_queue",
    ModeShuffle => "mode_shuffle",
    ModeOff => "mode_off",
    ModeUsage => "mode_usage",
    // Search and pick
    SearchUsage => "search_usage",
    SearchResultsHeader => "search_results_header",
    SearchResultsFooter => "search_results_footer",
    PickUsage => "pick_usage",
    PickTooLow => "pick_too_low",
    // Radio
    RadioAlreadyOn => "radio_already_on",
    RadioEnabled => "radio_enabled",
    RadioAlreadyOff => "radio_already_off",
    RadioDisabled => "radio_disabled",
    RadioStatusOn => "radio_status_on",
    RadioStatusOff => "radio_status_off",
    // Service switching
    AlreadyOnService => "already_on_service",
    SwitchedService => "switched_service",
    // Bot management
    Nickname => "nickname",
    GenderSet => "gender_set",
    GenderUsage => "gender_usage",
    Info => "info",
    Stats => "stats",
    // Player events (command processor)
    SpotifyUnavailable => "spotify_unavailable",
    NoResults => "no_results",
    NowPlaying => "now_playing",
    NowPlayingQueued => "now_playing_queued",
    MoreLoading => "more_loading",
    QueuedMany => "queued_many",
    QueuedOne => "queued_one",
    AlreadyQueuedLoadingRest => "already_queued_loading_rest",
    AlreadyInQueue => "already_in_queue",
    SearchFailed => "search_failed",
    RadioFetching => "radio_fetching",
    RadioPlaying => "radio_playing",
    RadioNoRecs => "radio_no_recs",
    RadioFailed => "radio_failed",
    EndOfQueue => "end_of_queue",
    InvalidPick => "invalid_pick",
    ChannelNotFound => "channel_not_found",
    FailedToStart => "failed_to_start",
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

    /// Register a runtime language (code is lowercased), replacing any
    /// existing map for that code.
    pub fn add_language(&mut self, code: &str, entries: HashMap<String, String>) {
        self.langs.insert(code.to_lowercase(), entries);
    }

    /// Merge entries into a language: given keys override, everything else is
    /// kept. Used for `<config_dir>/lang/` files so a partial file can patch
    /// individual messages of a bundled translation (a new code just inserts).
    pub fn merge_language(&mut self, code: &str, entries: HashMap<String, String>) {
        self.langs
            .entry(code.to_lowercase())
            .or_default()
            .extend(entries);
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

/// Write the embedded English template to `<lang_dir>/en.lang` so translators
/// have a commented, always-current file to copy (the loader ignores an
/// en.lang file — the embedded English stays authoritative, so overwriting is
/// safe and keeps the template in sync after updates). Best-effort: failure is
/// logged, never fatal.
fn export_english_template(lang_dir: &Path) {
    let path = lang_dir.join("en.lang");
    // Skip the write when the on-disk copy is already current.
    if std::fs::read_to_string(&path).is_ok_and(|current| current == EMBEDDED_EN) {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(lang_dir)
        .and_then(|()| std::fs::write(&path, EMBEDDED_EN))
    {
        tracing::warn!("Could not write English template {}: {e}", path.display());
    }
}

/// Language codes available: embedded (English + bundled translations) plus
/// any on-disk `.lang` files, sorted. Used by the config editor and setup
/// wizard, which need the list without loading a full catalog (the bot itself
/// uses `I18n::load`).
pub fn installed_language_codes(config_dir: &Path) -> Vec<String> {
    let mut codes = vec![ENGLISH.to_string()];
    for (code, _) in EMBEDDED_LANGS {
        codes.push((*code).to_string());
    }
    if let Ok(entries) = std::fs::read_dir(config_dir.join("lang")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lang") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let code = stem.to_lowercase();
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
    }
    codes.sort();
    codes
}

/// Per-user language picks, keyed by lowercased TeamTalk username. Stored as
/// machine-written JSON (`<config_dir>/lang_prefs.json`) — unlike `.lang`
/// files, this is never hand-edited.
pub struct LangPrefs {
    map: HashMap<String, String>,
    path: PathBuf,
}

impl LangPrefs {
    /// Load prefs from `path`. A missing or unreadable file yields empty prefs
    /// (never an error — a lost prefs file just means everyone is back on the
    /// server default until they pick again).
    pub fn load(path: PathBuf) -> LangPrefs {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<HashMap<String, String>>(&text).ok())
            .map(|m| {
                m.into_iter()
                    .map(|(user, code)| (user.to_lowercase(), code))
                    .collect()
            })
            .unwrap_or_default();
        LangPrefs { map, path }
    }

    pub fn get(&self, username: &str) -> Option<&str> {
        self.map.get(&username.to_lowercase()).map(String::as_str)
    }

    /// Set and persist a user's language pick. Persisting is atomic
    /// (temp + rename); a write error is logged, never fatal.
    pub fn set(&mut self, username: &str, code: &str) {
        self.map
            .insert(username.to_lowercase(), code.to_lowercase());
        self.save();
    }

    /// Remove a user's pick (they go back to following the server default).
    /// Returns whether a pick existed. Persists on change.
    pub fn remove(&mut self, username: &str) -> bool {
        let existed = self.map.remove(&username.to_lowercase()).is_some();
        if existed {
            self.save();
        }
        existed
    }

    fn save(&self) {
        let json = match serde_json::to_string_pretty(&self.map) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("Could not serialize language prefs: {e}");
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) =
            std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &self.path))
        {
            tracing::warn!(
                "Could not save language prefs {}: {e}",
                self.path.display()
            );
        }
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

/// Shared i18n runtime: the loaded catalog plus per-user state. Wrapped in an
/// `Arc` and shared by the command dispatcher and the command processor.
///
/// Locks are only ever taken one at a time (never nested), and every critical
/// section is a quick map access — safe to call from async contexts.
pub struct I18n {
    catalog: Catalog,
    prefs: Mutex<LangPrefs>,
    default_lang: Mutex<String>,
    /// Session cache: TeamTalk user id -> resolved language code. Seeded at
    /// dispatch time (where the sender's username is known) so every later
    /// reply site can resolve by user id alone.
    session: Mutex<HashMap<i32, String>>,
}

impl I18n {
    /// Build the runtime: embedded English plus every `<config_dir>/lang/*.lang`
    /// file, and per-user prefs from `<config_dir>/lang_prefs.json`. Each loaded
    /// file gets a coverage log line and placeholder-mismatch warnings; a broken
    /// file degrades, it never fails startup.
    pub fn load(config_dir: &Path, default_language: &str) -> I18n {
        let mut catalog = Catalog::new_embedded();
        // Bundled translations first; files below override them per key.
        for (code, text) in EMBEDDED_LANGS {
            catalog.add_language(code, parse_lang(text));
        }
        let lang_dir = config_dir.join("lang");
        export_english_template(&lang_dir);
        if let Ok(entries) = std::fs::read_dir(&lang_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("lang") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let code = stem.to_lowercase();
                if code == ENGLISH {
                    // English is embedded and authoritative; an en.lang file
                    // in the lang dir is ignored.
                    continue;
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        // Merge, not replace: a partial file patches individual
                        // messages of a bundled translation instead of wiping it.
                        catalog.merge_language(&code, parse_lang(&text));
                        let v = validate(&catalog, &code);
                        tracing::info!(
                            "Loaded translation {}: {}/{} messages",
                            code, v.present, v.total
                        );
                        for id in &v.slot_mismatches {
                            tracing::warn!(
                                "{code}.lang `{id}`: {{placeholders}} differ from English (renamed or dropped)"
                            );
                        }
                        for id in &v.unknown_keys {
                            tracing::warn!("{code}.lang has unknown key `{id}` (ignored)");
                        }
                    }
                    Err(e) => tracing::warn!("Could not read {}: {e}", path.display()),
                }
            }
        }
        let default_lang = default_language.to_lowercase();
        if !catalog.has_language(&default_lang) {
            tracing::warn!(
                "Default language `{default_lang}` has no {default_lang}.lang file; English will be shown"
            );
        }
        I18n {
            catalog,
            prefs: Mutex::new(LangPrefs::load(config_dir.join("lang_prefs.json"))),
            default_lang: Mutex::new(default_lang),
            session: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve and cache the language for a user id. Called at dispatch, where
    /// the sender's username is known. Order: user pref -> server default.
    pub fn seed(&self, user_id: i32, username: &str) {
        let lang = self.resolve_for(username);
        self.session.lock().insert(user_id, lang);
    }

    fn resolve_for(&self, username: &str) -> String {
        if let Some(code) = self.prefs.lock().get(username) {
            return code.to_string();
        }
        self.default_lang.lock().clone()
    }

    /// The cached language for a user id; server default if never seeded.
    pub fn lang_of(&self, user_id: i32) -> String {
        self.session
            .lock()
            .get(&user_id)
            .cloned()
            .unwrap_or_else(|| self.default_lang.lock().clone())
    }

    /// Translate `key` for the user behind `user_id`.
    pub fn tr(&self, user_id: i32, key: Key, args: &[(&str, String)]) -> String {
        self.catalog.t(&self.lang_of(user_id), key, args)
    }

    /// Translate directly into an explicit language (used for the `lang_set`
    /// confirmation, which renders in the just-picked language).
    pub fn tr_in(&self, code: &str, key: Key, args: &[(&str, String)]) -> String {
        self.catalog.t(&code.to_lowercase(), key, args)
    }

    /// Persist a user's language pick and update their session immediately.
    pub fn set_pref(&self, user_id: i32, username: &str, code: &str) {
        let code = code.to_lowercase();
        self.prefs.lock().set(username, &code);
        self.session.lock().insert(user_id, code);
    }

    /// Drop a user's pick so they follow the server default again. Updates
    /// their session immediately. Returns whether a pick existed.
    pub fn clear_pref(&self, user_id: i32, username: &str) -> bool {
        let existed = self.prefs.lock().remove(username);
        let default = self.default_lang.lock().clone();
        self.session.lock().insert(user_id, default);
        existed
    }

    /// Change the server default (glang). Personal picks are untouched.
    pub fn set_default(&self, code: &str) {
        *self.default_lang.lock() = code.to_lowercase();
    }

    /// The current server default language code.
    pub fn default_language(&self) -> String {
        self.default_lang.lock().clone()
    }

    pub fn is_available(&self, code: &str) -> bool {
        self.catalog.has_language(&code.to_lowercase())
    }

    /// All loaded languages as (code, display name), sorted by code.
    pub fn available(&self) -> Vec<(String, String)> {
        self.catalog
            .codes()
            .into_iter()
            .map(|code| {
                let name = self.catalog.language_name(&code);
                (code, name)
            })
            .collect()
    }

    pub fn language_name(&self, code: &str) -> String {
        self.catalog.language_name(&code.to_lowercase())
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

    // -- LangPrefs --

    #[test]
    fn lang_prefs_set_get_persist_round_trip() {
        let dir = std::env::temp_dir().join(format!("ttspotify_langprefs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lang_prefs.json");

        // Missing file -> empty prefs.
        let mut prefs = LangPrefs::load(path.clone());
        assert!(prefs.get("alice").is_none());

        // Set persists; lookups are case-insensitive on username.
        prefs.set("Alice", "PT");
        assert_eq!(prefs.get("alice"), Some("pt"));
        assert_eq!(prefs.get("ALICE"), Some("pt"));

        // A fresh load reads the same pick back.
        let reloaded = LangPrefs::load(path.clone());
        assert_eq!(reloaded.get("alice"), Some("pt"));

        // A corrupt file degrades to empty prefs, not a crash.
        std::fs::write(&path, "not json").unwrap();
        let broken = LangPrefs::load(path);
        assert!(broken.get("alice").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_accepts_moved_slots() {
        let mut c = Catalog::new_embedded();
        // Same slot, different position: valid, no mismatch.
        c.add_language("de", parse_lang("lang_set = {language} ist jetzt aktiv"));
        let v = validate(&c, "de");
        assert!(v.slot_mismatches.is_empty());
    }

    // -- I18n runtime --

    /// A temp config dir with a de.lang translation, torn down by the caller.
    fn runtime_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ttspotify_i18n_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lang")).unwrap();
        std::fs::write(
            dir.join("lang").join("de.lang"),
            "language_name = Deutsch\nlang_set = Sprache auf {language} gesetzt\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn runtime_resolution_order_pref_then_default_then_english() {
        let dir = runtime_dir("resolve");
        let i18n = I18n::load(&dir, "de");

        // Unseeded user id -> server default (de).
        assert_eq!(i18n.lang_of(99), "de");

        // Seeded user without a pref -> server default.
        i18n.seed(1, "alice");
        assert_eq!(i18n.lang_of(1), "de");

        // A personal pick beats the default and survives re-seeding.
        i18n.set_pref(1, "alice", "en");
        assert_eq!(i18n.lang_of(1), "en");
        i18n.seed(1, "alice");
        assert_eq!(i18n.lang_of(1), "en");

        // Changing the default (glang) moves un-preffed users only.
        i18n.seed(2, "bob");
        i18n.set_default("en");
        i18n.seed(2, "bob");
        assert_eq!(i18n.lang_of(2), "en");
        assert_eq!(i18n.lang_of(1), "en"); // alice's own pick still stands

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_tr_translates_and_falls_back() {
        let dir = runtime_dir("tr");
        let i18n = I18n::load(&dir, "de");

        i18n.seed(1, "alice");
        assert_eq!(
            i18n.tr(1, Key::LangSet, &[("language", "Deutsch".to_string())]),
            "Sprache auf Deutsch gesetzt"
        );

        // tr_in renders in an explicit language regardless of session.
        assert_eq!(
            i18n.tr_in("en", Key::LangSet, &[("language", "English".to_string())]),
            "Language set to English"
        );

        // Availability and names. Embedded bundles (es/pt/ru) are always
        // present alongside English and the on-disk de file.
        assert!(i18n.is_available("de"));
        assert!(i18n.is_available("EN"));
        assert!(!i18n.is_available("xx"));
        assert_eq!(i18n.language_name("de"), "Deutsch");
        let codes: Vec<String> = i18n.available().into_iter().map(|(c, _)| c).collect();
        assert_eq!(
            codes,
            vec!["de", "en", "es", "pt", "ru"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedded_translations_work_without_any_files() {
        let dir = std::env::temp_dir().join(format!(
            "ttspotify_i18n_embed_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No lang files at all: bundled Portuguese still translates.
        let i18n = I18n::load(&dir, "en");
        assert!(i18n.is_available("pt"));
        assert!(i18n.is_available("es"));
        assert!(i18n.is_available("ru"));
        assert_eq!(
            i18n.tr_in("pt", Key::Paused, &[]),
            "Pausado"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lang_file_overrides_embedded_translation_per_key() {
        let dir = std::env::temp_dir().join(format!(
            "ttspotify_i18n_override_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lang")).unwrap();
        // A one-line pt.lang: overrides that key, keeps the rest of the
        // bundled Portuguese (merge, not replace).
        std::fs::write(dir.join("lang").join("pt.lang"), "paused = Em pausa\n").unwrap();
        let i18n = I18n::load(&dir, "en");
        assert_eq!(i18n.tr_in("pt", Key::Paused, &[]), "Em pausa"); // overridden
        assert_eq!(i18n.tr_in("pt", Key::Resuming, &[]), "Retomando"); // still bundled
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedded_translations_have_valid_keys_and_slots() {
        // Guards bundled translations (and future community PRs to them)
        // against typo'd keys or broken {placeholders}.
        let mut c = Catalog::new_embedded();
        for (code, text) in EMBEDDED_LANGS {
            c.add_language(code, parse_lang(text));
            let v = validate(&c, code);
            assert!(
                v.slot_mismatches.is_empty(),
                "{code}.lang has placeholder mismatches: {:?}",
                v.slot_mismatches
            );
            assert!(
                v.unknown_keys.is_empty(),
                "{code}.lang has unknown keys: {:?}",
                v.unknown_keys
            );
        }
    }

    #[test]
    fn load_exports_english_template_to_lang_dir() {
        let dir = std::env::temp_dir().join(format!("ttspotify_i18n_tpl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No lang dir yet: load creates it and writes the template.
        let _ = I18n::load(&dir, "en");
        let exported = std::fs::read_to_string(dir.join("lang").join("en.lang")).unwrap();
        assert_eq!(exported, EMBEDDED_EN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_clear_pref_removes_pick_and_follows_default() {
        let dir = runtime_dir("clear");
        {
            let i18n = I18n::load(&dir, "en");
            i18n.seed(1, "alice");
            i18n.set_pref(1, "alice", "de");
            assert_eq!(i18n.lang_of(1), "de");
            // Clear: pick removed, session follows the server default at once.
            assert!(i18n.clear_pref(1, "alice"));
            assert_eq!(i18n.lang_of(1), "en");
            assert!(!i18n.clear_pref(1, "alice")); // nothing left to remove
        }
        // The removal persisted: a fresh runtime no longer knows the pick.
        let i18n = I18n::load(&dir, "en");
        i18n.seed(7, "alice");
        assert_eq!(i18n.lang_of(7), "en");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_set_pref_persists_across_reload() {
        let dir = runtime_dir("persist");
        {
            let i18n = I18n::load(&dir, "en");
            i18n.seed(1, "alice");
            i18n.set_pref(1, "Alice", "de");
        }
        // A fresh runtime (bot restart) still knows alice's pick.
        let i18n = I18n::load(&dir, "en");
        i18n.seed(7, "alice"); // new session id after reconnect
        assert_eq!(i18n.lang_of(7), "de");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
