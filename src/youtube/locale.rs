//! YouTube's search locations and languages, from rustypipe's lists.

use rustypipe::param::{Country, Language, COUNTRIES, LANGUAGES};

/// One picker entry: the code saved in the config, and what is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleOption {
    pub code: String,
    pub label: String,
}

pub const DEFAULT_COUNTRY_LABEL: &str = "YouTube default (United States)";
pub const DEFAULT_LANGUAGE_LABEL: &str = "YouTube default (English (US))";

/// A list entry with every name it can be found by.
struct Entry {
    code: String,
    names: Vec<String>,
    label: String,
}

impl Entry {
    fn option(&self) -> LocaleOption {
        LocaleOption { code: self.code.clone(), label: self.label.clone() }
    }

    fn is_exactly(&self, input: &str) -> bool {
        self.code.eq_ignore_ascii_case(input) || self.names.iter().any(|n| n.to_lowercase() == input)
    }

    fn starts_with(&self, input: &str) -> bool {
        std::iter::once(&self.code).chain(&self.names).any(|name| {
            let name = name.to_lowercase();
            name.starts_with(input) || name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(input))
        })
    }
}

fn country_entries() -> Vec<Entry> {
    sorted(COUNTRIES.iter().map(|&c| {
        let code = c.to_string();
        Entry { label: format!("{} - {code}", c.name()), names: vec![c.name().to_string()], code }
    }))
}

fn language_entries() -> Vec<Entry> {
    sorted(LANGUAGES.iter().map(|&l| {
        let code = l.to_string();
        let native = l.name();
        let english = match english_name(l) {
            "" => native,
            english => english,
        };
        if english == native {
            Entry { label: format!("{english} - {code}"), names: vec![english.to_string()], code }
        } else {
            Entry {
                label: format!("{english}, {native} - {code}"),
                names: vec![english.to_string(), native.to_string()],
                code,
            }
        }
    }))
}

fn sorted(entries: impl Iterator<Item = Entry>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = entries.collect();
    entries.sort_by_key(|e| e.label.to_lowercase());
    entries
}

fn with_default(entries: Vec<Entry>, default_label: &str) -> Vec<LocaleOption> {
    let default = LocaleOption { code: String::new(), label: default_label.to_string() };
    std::iter::once(default).chain(entries.iter().map(Entry::option)).collect()
}

/// Every location by name, after an entry for the default (an empty code).
pub fn country_options() -> Vec<LocaleOption> {
    with_default(country_entries(), DEFAULT_COUNTRY_LABEL)
}

/// Every language by English name, after an entry for the default (an empty code).
pub fn language_options() -> Vec<LocaleOption> {
    with_default(language_entries(), DEFAULT_LANGUAGE_LABEL)
}

fn find_code(input: &str, entries: Vec<Entry>) -> Option<String> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return Some(String::new());
    }
    entries.into_iter().find(|e| e.is_exactly(&input)).map(|e| e.code)
}

/// Entries matching typed text: the exact code or name alone when there is
/// one, otherwise every entry with a name or word starting with it.
fn search(input: &str, entries: Vec<Entry>) -> Vec<LocaleOption> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return Vec::new();
    }
    if let Some(exact) = entries.iter().find(|e| e.is_exactly(&input)) {
        return vec![exact.option()];
    }
    entries.iter().filter(|e| e.starts_with(&input)).map(Entry::option).collect()
}

/// The config code for a typed location: a code in any case, or a name.
/// `Some("")` for blank input, `None` for anything YouTube does not offer.
pub fn country_code(input: &str) -> Option<String> {
    find_code(input, country_entries())
}

/// As `country_code`, for languages, by English or native name.
pub fn language_code(input: &str) -> Option<String> {
    find_code(input, language_entries())
}

pub fn search_countries(input: &str) -> Vec<LocaleOption> {
    search(input, country_entries())
}

pub fn search_languages(input: &str) -> Vec<LocaleOption> {
    search(input, language_entries())
}

/// The location a saved code names. `None` when empty or unknown.
pub fn parse_country(code: &str) -> Option<Country> {
    country_code(code).filter(|c| !c.is_empty())?.parse().ok()
}

/// The language a saved code names. `None` when empty or unknown.
pub fn parse_language(code: &str) -> Option<Language> {
    language_code(code).filter(|c| !c.is_empty())?.parse().ok()
}

fn english_name(language: Language) -> &'static str {
    match language {
        Language::Af => "Afrikaans",
        Language::Am => "Amharic",
        Language::Ar => "Arabic",
        Language::As => "Assamese",
        Language::Az => "Azerbaijani",
        Language::Be => "Belarusian",
        Language::Bg => "Bulgarian",
        Language::Bn => "Bangla",
        Language::Bs => "Bosnian",
        Language::Ca => "Catalan",
        Language::Cs => "Czech",
        Language::Da => "Danish",
        Language::De => "German",
        Language::El => "Greek",
        Language::En => "English (US)",
        Language::EnGb => "English (UK)",
        Language::EnIn => "English (India)",
        Language::Es => "Spanish (Spain)",
        Language::Es419 => "Spanish (Latin America)",
        Language::EsUs => "Spanish (US)",
        Language::Et => "Estonian",
        Language::Eu => "Basque",
        Language::Fa => "Persian",
        Language::Fi => "Finnish",
        Language::Fil => "Filipino",
        Language::Fr => "French",
        Language::FrCa => "French (Canada)",
        Language::Gl => "Galician",
        Language::Gu => "Gujarati",
        Language::Hi => "Hindi",
        Language::Hr => "Croatian",
        Language::Hu => "Hungarian",
        Language::Hy => "Armenian",
        Language::Id => "Indonesian",
        Language::Is => "Icelandic",
        Language::It => "Italian",
        Language::Iw => "Hebrew",
        Language::Ja => "Japanese",
        Language::Ka => "Georgian",
        Language::Kk => "Kazakh",
        Language::Km => "Khmer",
        Language::Kn => "Kannada",
        Language::Ko => "Korean",
        Language::Ky => "Kyrgyz",
        Language::Lo => "Lao",
        Language::Lt => "Lithuanian",
        Language::Lv => "Latvian",
        Language::Mk => "Macedonian",
        Language::Ml => "Malayalam",
        Language::Mn => "Mongolian",
        Language::Mr => "Marathi",
        Language::Ms => "Malay",
        Language::My => "Burmese",
        Language::Ne => "Nepali",
        Language::Nl => "Dutch",
        Language::No => "Norwegian",
        Language::Or => "Odia",
        Language::Pa => "Punjabi",
        Language::Pl => "Polish",
        Language::Pt => "Portuguese (Brazil)",
        Language::PtPt => "Portuguese (Portugal)",
        Language::Ro => "Romanian",
        Language::Ru => "Russian",
        Language::Si => "Sinhala",
        Language::Sk => "Slovak",
        Language::Sl => "Slovenian",
        Language::Sq => "Albanian",
        Language::Sr => "Serbian (Cyrillic)",
        Language::SrLatn => "Serbian (Latin)",
        Language::Sv => "Swedish",
        Language::Sw => "Swahili",
        Language::Ta => "Tamil",
        Language::Te => "Telugu",
        Language::Th => "Thai",
        Language::Tr => "Turkish",
        Language::Uk => "Ukrainian",
        Language::Ur => "Urdu",
        Language::Uz => "Uzbek",
        Language::Vi => "Vietnamese",
        Language::ZhCn => "Chinese (Simplified)",
        Language::ZhHk => "Chinese (Hong Kong)",
        Language::ZhTw => "Chinese (Traditional)",
        Language::Zu => "Zulu",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_list_offers_the_default_first_then_every_entry() {
        let countries = country_options();
        assert_eq!(countries[0], LocaleOption { code: String::new(), label: DEFAULT_COUNTRY_LABEL.into() });
        assert_eq!(countries.len(), COUNTRIES.len() + 1);

        let languages = language_options();
        assert_eq!(languages[0], LocaleOption { code: String::new(), label: DEFAULT_LANGUAGE_LABEL.into() });
        assert_eq!(languages.len(), LANGUAGES.len() + 1);
    }

    #[test]
    fn every_offered_code_is_one_youtube_accepts() {
        for option in country_options().iter().skip(1) {
            assert!(parse_country(&option.code).is_some(), "{option:?}");
            assert!(option.label.ends_with(&format!(" - {}", option.code)), "{option:?}");
        }
        for option in language_options().iter().skip(1) {
            assert!(parse_language(&option.code).is_some(), "{option:?}");
            assert!(option.label.ends_with(&format!(" - {}", option.code)), "{option:?}");
        }
    }

    #[test]
    fn every_language_has_an_english_name() {
        for &language in LANGUAGES.iter() {
            let name = english_name(language);
            assert!(!name.is_empty() && name.is_ascii(), "{language:?}");
        }
    }

    #[test]
    fn entries_are_sorted_by_name() {
        for list in [country_options(), language_options()] {
            let names: Vec<String> = list.iter().skip(1).map(|o| o.label.to_lowercase()).collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted);
        }
    }

    #[test]
    fn languages_show_the_english_and_the_native_name() {
        let labels: Vec<String> = language_options().into_iter().map(|o| o.label).collect();
        assert!(labels.contains(&"German, Deutsch - de".to_string()));
        assert!(labels.contains(&"English (UK) - en-GB".to_string()));
    }

    #[test]
    fn a_location_is_found_by_code_or_name_in_any_case() {
        assert_eq!(country_code("in").as_deref(), Some("IN"));
        assert_eq!(country_code("  GB ").as_deref(), Some("GB"));
        assert_eq!(country_code("india").as_deref(), Some("IN"));
        assert_eq!(country_code("").as_deref(), Some(""));
        assert_eq!(country_code("Atlantis"), None);
    }

    #[test]
    fn a_language_is_found_by_code_or_either_name_in_any_case() {
        assert_eq!(language_code("en-gb").as_deref(), Some("en-GB"));
        assert_eq!(language_code("EN-GB").as_deref(), Some("en-GB"));
        assert_eq!(language_code("Deutsch").as_deref(), Some("de"));
        assert_eq!(language_code("german").as_deref(), Some("de"));
        assert_eq!(language_code("english (uk)").as_deref(), Some("en-GB"));
        assert_eq!(language_code(" ").as_deref(), Some(""));
        assert_eq!(language_code("zzz"), None);
    }

    #[test]
    fn an_exact_code_or_name_is_the_only_match() {
        let codes = |found: Vec<LocaleOption>| found.into_iter().map(|o| o.code).collect::<Vec<_>>();
        assert_eq!(codes(search_countries("in")), ["IN"]);
        assert_eq!(codes(search_languages("german")), ["de"]);
    }

    #[test]
    fn part_of_a_name_finds_every_entry_it_starts_a_word_of() {
        let codes = |found: Vec<LocaleOption>| found.into_iter().map(|o| o.code).collect::<Vec<_>>();
        assert_eq!(codes(search_languages("english")), ["en-IN", "en-GB", "en"]);
        assert_eq!(codes(search_countries("ger")), ["DE"]);
        let united = codes(search_countries("united"));
        assert!(united.contains(&"GB".to_string()) && united.contains(&"US".to_string()), "{united:?}");
        assert_eq!(codes(search_countries("kingdom")), ["GB"]);
        assert_eq!(codes(search_languages("simplified")), ["zh-CN"]);
        assert!(search_countries("atlantis").is_empty());
        assert!(search_countries("  ").is_empty());
    }

    #[test]
    fn a_regional_language_keeps_its_region() {
        // Lowercasing "en-GB" before parsing used to fall back to plain English.
        assert_eq!(parse_language("en-GB"), Some(Language::EnGb));
        assert_eq!(parse_language("en-gb"), Some(Language::EnGb));
        assert_eq!(parse_language("fr-CA"), Some(Language::FrCa));
    }

    #[test]
    fn saved_codes_parse_in_any_case_and_blanks_do_not() {
        assert_eq!(parse_country("in"), Some(Country::In));
        assert_eq!(parse_country("IN"), Some(Country::In));
        assert_eq!(parse_language("DE"), Some(Language::De));
        assert_eq!(parse_country(""), None);
        assert_eq!(parse_language("   "), None);
        assert_eq!(parse_country("XX"), None);
        assert_eq!(parse_language("not a language"), None);
    }
}
