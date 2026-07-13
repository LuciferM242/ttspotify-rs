use super::UpdateError;
use serde_json::Value;

const REPO: &str = "LuciferM242/ttspotify-rs";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: semver::Version,
    pub tag: String,
    pub changelog: String,
    pub asset_url: String,
    pub sums_url: String,
    pub sig_url: String,
}

/// The release asset filename this build should download.
pub fn current_asset_name() -> &'static str {
    if cfg!(windows) {
        "tt-spotify-bot-windows-x86_64.zip"
    } else if cfg!(target_arch = "aarch64") {
        "tt-spotify-bot-linux-aarch64.tar.gz"
    } else {
        "tt-spotify-bot-linux-x86_64.tar.gz"
    }
}

/// Parse a release tag (`v0.4.0`) and return it only if strictly newer than the
/// running version (`CARGO_PKG_VERSION`). Never downgrades.
pub fn newer_than_current(tag: &str) -> Option<semver::Version> {
    let candidate = semver::Version::parse(tag.trim_start_matches('v')).ok()?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    if candidate > current {
        Some(candidate)
    } else {
        None
    }
}

/// Given a parsed `releases/latest` JSON body, produce an `UpdateInfo` if it is
/// newer than the running version and carries our platform asset + SHA256SUMS +
/// SHA256SUMS.minisig. Returns `Ok(None)` when not newer or assets are missing.
fn select_from_release(json: &Value) -> Result<Option<UpdateInfo>, UpdateError> {
    let tag = json["tag_name"]
        .as_str()
        .ok_or_else(|| UpdateError::Parse("missing tag_name".into()))?;
    let Some(version) = newer_than_current(tag) else {
        return Ok(None);
    };
    let changelog = json["body"].as_str().unwrap_or("").to_string();

    let assets = json["assets"]
        .as_array()
        .ok_or_else(|| UpdateError::Parse("missing assets".into()))?;
    let url_of = |name: &str| -> Option<String> {
        assets.iter().find_map(|a| {
            if a["name"].as_str() == Some(name) {
                a["browser_download_url"].as_str().map(str::to_string)
            } else {
                None
            }
        })
    };

    let asset = current_asset_name();
    let (Some(asset_url), Some(sums_url), Some(sig_url)) = (
        url_of(asset),
        url_of("SHA256SUMS"),
        url_of("SHA256SUMS.minisig"),
    ) else {
        return Ok(None);
    };

    Ok(Some(UpdateInfo {
        version,
        tag: tag.to_string(),
        changelog,
        asset_url,
        sums_url,
        sig_url,
    }))
}

/// Query GitHub for the latest release and return update info if newer.
pub async fn check() -> Result<Option<UpdateInfo>, UpdateError> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("ttspotify-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Http(format!("HTTP {}", resp.status())));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| UpdateError::Parse(e.to_string()))?;
    select_from_release(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_is_platform_specific() {
        let n = current_asset_name();
        assert!(n.starts_with("tt-spotify-bot-"));
        if cfg!(windows) {
            assert!(n.ends_with(".zip"));
        } else {
            assert!(n.ends_with(".tar.gz"));
        }
    }

    #[test]
    fn older_or_equal_tag_is_none() {
        assert!(newer_than_current(env!("CARGO_PKG_VERSION")).is_none());
        assert!(newer_than_current("v0.0.1").is_none());
    }

    #[test]
    fn much_newer_tag_is_some() {
        assert!(newer_than_current("v999.0.0").is_some());
    }

    #[test]
    fn malformed_tag_is_none() {
        assert!(newer_than_current("not-a-version").is_none());
    }

    #[test]
    fn select_returns_none_when_not_newer() {
        let json = serde_json::json!({
            "tag_name": "v0.0.1",
            "body": "old",
            "assets": []
        });
        assert!(select_from_release(&json).unwrap().is_none());
    }

    #[test]
    fn select_returns_none_when_assets_missing() {
        let json = serde_json::json!({
            "tag_name": "v999.0.0",
            "body": "notes",
            "assets": []
        });
        assert!(select_from_release(&json).unwrap().is_none());
    }

    #[test]
    fn select_builds_info_when_newer_and_complete() {
        let asset = current_asset_name();
        let json = serde_json::json!({
            "tag_name": "v999.0.0",
            "body": "release notes here",
            "assets": [
                { "name": asset, "browser_download_url": "https://x/asset" },
                { "name": "SHA256SUMS", "browser_download_url": "https://x/sums" },
                { "name": "SHA256SUMS.minisig", "browser_download_url": "https://x/sig" }
            ]
        });
        let info = select_from_release(&json).unwrap().unwrap();
        assert_eq!(info.tag, "v999.0.0");
        assert_eq!(info.changelog, "release notes here");
        assert_eq!(info.asset_url, "https://x/asset");
        assert_eq!(info.sums_url, "https://x/sums");
        assert_eq!(info.sig_url, "https://x/sig");
    }
}
