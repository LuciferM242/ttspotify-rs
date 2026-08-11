//! What kind of Spotify account is signed in.
//!
//! Playback needs Premium: Spotify refuses the audio key for anything else,
//! and librespot then streams undecryptable bytes rather than failing outright,
//! which surfaces as a decode error several thousand log lines later. Recording
//! the account type at startup means a log answers "is this account Premium?"
//! without anyone having to ask the user.

/// The `type` user attribute Spotify sends with product info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountType {
    Premium,
    /// A named non-Premium plan, e.g. `free`.
    Other(String),
    /// Product info never arrived, so Spotify never said. Not the same as
    /// knowing the account is not Premium.
    Unknown,
}

impl AccountType {
    /// Whether playback can work at all.
    pub fn can_play(&self) -> bool {
        // Unknown is not treated as broken: product info can lag the first
        // track, and calling a working account unplayable would be worse than
        // saying nothing.
        !matches!(self, AccountType::Other(_))
    }
}

/// Read the account type from Spotify's `type` attribute.
pub fn classify(attribute: Option<&str>) -> AccountType {
    match attribute.map(str::trim) {
        None | Some("") => AccountType::Unknown,
        // Spotify has used both plain "premium" and plan-qualified values.
        Some(t) if t.eq_ignore_ascii_case("premium") => AccountType::Premium,
        Some(t) => AccountType::Other(t.to_string()),
    }
}

/// One line for the log, naming the account type and country.
pub fn describe(account: &AccountType, country: &str) -> String {
    let country = if country.is_empty() { "unknown" } else { country };
    match account {
        AccountType::Premium => format!("Spotify account: premium (country {country})"),
        AccountType::Other(t) => format!(
            "Spotify account: {t} (country {country}) - playback needs premium, \
             so tracks will fail to decrypt"
        ),
        AccountType::Unknown => format!(
            "Spotify account: type not reported yet (country {country})"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premium_is_recognised() {
        assert_eq!(classify(Some("premium")), AccountType::Premium);
        assert!(classify(Some("premium")).can_play());
    }

    #[test]
    fn case_does_not_matter() {
        // Observed as both "premium" and "Premium" across Spotify changes.
        assert_eq!(classify(Some("Premium")), AccountType::Premium);
    }

    #[test]
    fn a_free_account_is_named_and_cannot_play() {
        let acct = classify(Some("free"));
        assert_eq!(acct, AccountType::Other("free".to_string()));
        assert!(!acct.can_play());
        assert!(describe(&acct, "EC").contains("needs premium"));
    }

    #[test]
    fn a_missing_attribute_is_unknown_not_free() {
        // Product info can arrive after the first track. Reporting "not
        // premium" here would blame the account for a timing quirk.
        assert_eq!(classify(None), AccountType::Unknown);
        assert_eq!(classify(Some("")), AccountType::Unknown);
        assert!(classify(None).can_play(), "unknown must not be treated as broken");
    }

    #[test]
    fn whitespace_around_the_value_is_ignored() {
        assert_eq!(classify(Some("  premium ")), AccountType::Premium);
    }

    #[test]
    fn the_description_names_the_country() {
        assert!(describe(&AccountType::Premium, "EC").contains("EC"));
        assert!(describe(&AccountType::Premium, "").contains("unknown"));
    }
}
