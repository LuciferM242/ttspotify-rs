//! Self-updater: check GitHub Releases, verify a minisign signature over
//! SHA256SUMS, then verify + apply the matching asset.

mod apply;
mod github;
mod verify;

pub use apply::download_and_apply;
pub use github::{check, current_asset_name, newer_than_current, UpdateInfo};
pub use verify::{expected_hash, sha256_hex};

use std::fmt;

/// Embedded minisign public key (base64 body only, no comment line). Signatures
/// are made in CI by the matching secret key (GitHub Secret MINISIGN_SECRET_KEY).
pub const PUBLIC_KEY: &str = "RWTvwlFryO9VLtB1R7ZmCYzRB2iGuBAHWEmx8dsI8UH4LlLBEG0N61I+";

#[derive(Debug)]
pub enum UpdateError {
    Http(String),
    Parse(String),
    Signature,
    Hash,
    Extract(String),
    Io(String),
    Cancelled,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::Http(e) => write!(f, "Network error: {e}"),
            UpdateError::Parse(e) => write!(f, "Could not read release info: {e}"),
            UpdateError::Signature => write!(f, "Signature verification failed"),
            UpdateError::Hash => write!(f, "Downloaded file failed its checksum"),
            UpdateError::Extract(e) => write!(f, "Could not extract the update: {e}"),
            UpdateError::Io(e) => write!(f, "File error: {e}"),
            UpdateError::Cancelled => write!(f, "Update cancelled"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Convert a release-body / CHANGELOG markdown snippet into readable plain text
/// for display in a non-markdown control (the update dialog's text box, the CLI
/// print). Strips heading markers (`#`), turns `- ` bullets into `• `, and
/// removes bold (`**`) and inline-code (backtick) markers. Underscores and lone
/// asterisks are left alone so identifiers like `yt_dlp` aren't corrupted.
pub fn plain_changelog(md: &str) -> String {
    let mut out = String::new();
    for line in md.lines() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        let rendered = if trimmed.starts_with('#') {
            // Heading: drop the leading #'s and the space after them.
            trimmed.trim_start_matches('#').trim_start().to_string()
        } else if let Some(rest) = trimmed.strip_prefix("- ") {
            format!("\u{2022} {rest}")
        } else if let Some(rest) = trimmed.strip_prefix("* ") {
            format!("\u{2022} {rest}")
        } else {
            line.to_string()
        };
        out.push_str(&rendered.replace("**", "").replace('`', ""));
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::plain_changelog;

    #[test]
    fn strips_heading_markers() {
        assert_eq!(plain_changelog("### Added"), "Added");
        assert_eq!(plain_changelog("## [0.4.0] - 2026"), "[0.4.0] - 2026");
    }

    #[test]
    fn converts_bullets() {
        assert_eq!(plain_changelog("- a thing"), "\u{2022} a thing");
        assert_eq!(plain_changelog("* other"), "\u{2022} other");
    }

    #[test]
    fn removes_bold_and_code_but_keeps_identifiers() {
        assert_eq!(plain_changelog("run `ttspotify --update`"), "run ttspotify --update");
        assert_eq!(plain_changelog("**bold** text"), "bold text");
        // underscores in identifiers survive
        assert_eq!(plain_changelog("- installs yt_dlp"), "\u{2022} installs yt_dlp");
    }

    #[test]
    fn multiline_block() {
        let md = "### Added\n- self-updater\n- signed releases";
        assert_eq!(
            plain_changelog(md),
            "Added\n\u{2022} self-updater\n\u{2022} signed releases"
        );
    }
}
