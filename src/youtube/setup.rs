//! YouTube binaries auto-installer.
//!
//! Downloads `yt-dlp`, the bgutil-pot binary, and the bgutil yt-dlp plugin
//! into `<exe-dir>/lib/` so the bot can resolve them at runtime without the
//! user installing anything by hand.
//!
//! Pinned versions live as constants below. Bump them periodically and ship
//! a new release; users re-run `--setup-youtube` to update.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::BotError;

const YT_DLP_VERSION: &str = "2026.03.17";
const BGUTIL_VERSION: &str = "v0.8.1";

/// Filename for the sidecar that records which bgutil version is on disk.
/// Lives next to the bgutil binary in `lib/`.
const BGUTIL_VERSION_FILE: &str = ".bgutil-version";

/// Resolved on-disk paths for all three components.
#[derive(Debug, Clone)]
pub struct YoutubeSetupPaths {
    /// Directory for binaries: `<exe-dir>/lib`.
    pub lib_dir: PathBuf,
    /// `lib/yt-dlp` (Linux) or `lib/yt-dlp.exe` (Windows).
    pub yt_dlp: PathBuf,
    /// `lib/bgutil-pot` or `lib/bgutil-pot.exe`.
    pub bgutil_pot: PathBuf,
    /// `lib/yt-dlp-plugins` (the dir we pass to `--plugin-dirs`).
    pub plugin_dir: PathBuf,
}

/// Compute where the binaries should live.
/// `<dir of current_exe>/lib/...`. On debug builds that's `target/debug/lib`.
pub fn resolve_paths() -> Result<YoutubeSetupPaths, BotError> {
    let exe = std::env::current_exe()
        .map_err(|e| BotError::Config(format!("current_exe failed: {e}")))?;
    let exe_dir = exe.parent()
        .ok_or_else(|| BotError::Config("current_exe has no parent".to_string()))?;
    let lib_dir = exe_dir.join("lib");
    let (yt_dlp_name, bgutil_name) = if cfg!(windows) {
        ("yt-dlp.exe", "bgutil-pot.exe")
    } else {
        ("yt-dlp", "bgutil-pot")
    };
    Ok(YoutubeSetupPaths {
        yt_dlp: lib_dir.join(yt_dlp_name),
        bgutil_pot: lib_dir.join(bgutil_name),
        plugin_dir: lib_dir.join("yt-dlp-plugins"),
        lib_dir,
    })
}

/// True if all three components are present on disk.
pub fn is_installed(paths: &YoutubeSetupPaths) -> bool {
    paths.yt_dlp.is_file() && paths.bgutil_pot.is_file() && paths.plugin_dir.is_dir()
}

/// Download + install yt-dlp, bgutil-pot, and the plugin zip.
/// Reports progress via the callback.
pub async fn install(
    paths: &YoutubeSetupPaths,
    progress: impl Fn(&str),
) -> Result<(), BotError> {
    fs::create_dir_all(&paths.lib_dir)
        .map_err(|e| BotError::Config(format!("create lib dir: {e}")))?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("tt-spotify-bot/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| BotError::Config(format!("HTTP client: {e}")))?;

    // 1. yt-dlp — verify against the release's SHA2-256SUMS manifest.
    progress(&format!("Downloading yt-dlp {YT_DLP_VERSION}..."));
    let yt_dlp_asset = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp_linux" };
    let yt_dlp_url = format!(
        "https://github.com/yt-dlp/yt-dlp/releases/download/{YT_DLP_VERSION}/{yt_dlp_asset}"
    );
    let yt_dlp_hash = match fetch_text(
        &client,
        &format!("https://github.com/yt-dlp/yt-dlp/releases/download/{YT_DLP_VERSION}/SHA2-256SUMS"),
    ).await {
        Ok(sums) => parse_sums_file(&sums, yt_dlp_asset),
        Err(e) => {
            tracing::warn!("Could not fetch yt-dlp checksums: {e}");
            None
        }
    };
    download_verified(&client, &yt_dlp_url, &paths.yt_dlp, yt_dlp_hash.as_deref(), true).await?;
    make_executable(&paths.yt_dlp)?;
    progress("  yt-dlp installed.");

    // Fetch bgutil release asset digests once for the binary + zip.
    let bgutil_digests = fetch_release_asset_digests(
        &client,
        "jim60105/bgutil-ytdlp-pot-provider-rs",
        BGUTIL_VERSION,
    ).await;

    // 2. bgutil-pot
    progress(&format!("Downloading bgutil-pot {BGUTIL_VERSION}..."));
    let bgutil_asset = if cfg!(windows) { "bgutil-pot-windows-x86_64.exe" } else { "bgutil-pot-linux-x86_64" };
    let bgutil_url = format!(
        "https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{BGUTIL_VERSION}/{bgutil_asset}"
    );
    download_verified(&client, &bgutil_url, &paths.bgutil_pot, bgutil_digests.get(bgutil_asset).map(|s| s.as_str()), true).await?;
    make_executable(&paths.bgutil_pot)?;
    progress("  bgutil-pot installed.");

    // 3. plugin zip
    progress(&format!("Downloading bgutil yt-dlp plugin {BGUTIL_VERSION}..."));
    let zip_asset = "bgutil-ytdlp-pot-provider-rs.zip";
    let plugin_url = format!(
        "https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{BGUTIL_VERSION}/{zip_asset}"
    );
    let zip_path = paths.lib_dir.join("bgutil-plugin.zip");
    download_verified(&client, &plugin_url, &zip_path, bgutil_digests.get(zip_asset).map(|s| s.as_str()), false).await?;
    extract_plugin_zip(&zip_path, &paths.plugin_dir)?;
    let _ = fs::remove_file(&zip_path);
    progress("  Plugin extracted.");

    // Record what we just installed so --update-tools can compare later.
    let _ = fs::write(paths.lib_dir.join(BGUTIL_VERSION_FILE), BGUTIL_VERSION);

    progress(&format!("YouTube support ready in {}", paths.lib_dir.display()));
    Ok(())
}

/// Pinned version we'd lay down on a fresh install. Read by --update-tools
/// to know what to download if the sidecar is missing.
pub fn pinned_bgutil_version() -> &'static str {
    BGUTIL_VERSION
}

/// Returns the bgutil version actually installed on disk (read from the
/// sidecar). Falls back to the pinned const if the sidecar is missing,
/// which covers older installs that predate the sidecar.
pub fn installed_bgutil_version(paths: &YoutubeSetupPaths) -> String {
    fs::read_to_string(paths.lib_dir.join(BGUTIL_VERSION_FILE))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| BGUTIL_VERSION.to_string())
}

/// Re-download just the bgutil binary + plugin at a specific version,
/// overwriting any existing files. Updates the sidecar.
pub async fn install_bgutil_version(
    paths: &YoutubeSetupPaths,
    version: &str,
    progress: impl Fn(&str),
) -> Result<(), BotError> {
    fs::create_dir_all(&paths.lib_dir)
        .map_err(|e| BotError::Config(format!("create lib dir: {e}")))?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("tt-spotify-bot/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| BotError::Config(format!("HTTP client: {e}")))?;

    let digests = fetch_release_asset_digests(
        &client,
        "jim60105/bgutil-ytdlp-pot-provider-rs",
        version,
    ).await;

    progress(&format!("Downloading bgutil-pot {version}..."));
    let bgutil_asset = if cfg!(windows) { "bgutil-pot-windows-x86_64.exe" } else { "bgutil-pot-linux-x86_64" };
    let bgutil_url = format!(
        "https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{version}/{bgutil_asset}"
    );
    download_verified(&client, &bgutil_url, &paths.bgutil_pot, digests.get(bgutil_asset).map(|s| s.as_str()), true).await?;
    make_executable(&paths.bgutil_pot)?;

    progress(&format!("Downloading bgutil yt-dlp plugin {version}..."));
    let zip_asset = "bgutil-ytdlp-pot-provider-rs.zip";
    let plugin_url = format!(
        "https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{version}/{zip_asset}"
    );
    let zip_path = paths.lib_dir.join("bgutil-plugin.zip");
    download_verified(&client, &plugin_url, &zip_path, digests.get(zip_asset).map(|s| s.as_str()), false).await?;
    // Wipe the old plugin dir to avoid stale files lingering after a version bump.
    let _ = fs::remove_dir_all(&paths.plugin_dir);
    extract_plugin_zip(&zip_path, &paths.plugin_dir)?;
    let _ = fs::remove_file(&zip_path);

    let _ = fs::write(paths.lib_dir.join(BGUTIL_VERSION_FILE), version);
    progress(&format!("bgutil-pot updated to {version}."));
    Ok(())
}

/// Hit the GitHub API for the latest bgutil release tag.
pub async fn latest_bgutil_version() -> Result<String, BotError> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("tt-spotify-bot/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| BotError::Config(format!("HTTP client: {e}")))?;
    let response = client
        .get("https://api.github.com/repos/jim60105/bgutil-ytdlp-pot-provider-rs/releases/latest")
        .send().await
        .map_err(|e| BotError::Config(format!("GitHub API: {e}")))?;
    if !response.status().is_success() {
        return Err(BotError::Config(format!("GitHub API returned {}", response.status())));
    }
    let json: serde_json::Value = response.json().await
        .map_err(|e| BotError::Config(format!("GitHub API JSON: {e}")))?;
    let tag = json.get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BotError::Config("GitHub API: missing tag_name".to_string()))?
        .to_string();
    Ok(tag)
}

/// Compute the lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Verify `bytes` hash against an expected hex digest (case-insensitive).
fn verify_sha256(bytes: &[u8], expected_hex: &str) -> bool {
    sha256_hex(bytes).eq_ignore_ascii_case(expected_hex.trim())
}

/// Parse a `SHA2-256SUMS`-style file (`<hex>  <filename>` per line) and return
/// the digest for `asset_name`, if present.
fn parse_sums_file(text: &str, asset_name: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        // The filename is the remainder (may be prefixed with '*' for binary).
        let name = parts.next().unwrap_or("").trim_start_matches('*');
        if name == asset_name && hash.len() == 64 {
            return Some(hash.to_string());
        }
    }
    None
}

/// Basic executable magic-byte sanity check, used as a fallback when no hash
/// is available: PE ("MZ") on Windows, ELF ("\x7fELF") on Unix.
fn looks_like_executable(bytes: &[u8]) -> bool {
    if cfg!(windows) {
        bytes.starts_with(b"MZ")
    } else {
        bytes.starts_with(b"\x7fELF")
    }
}

/// Fetch a URL as text (used for the SHA2-256SUMS manifest).
async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, BotError> {
    let response = client.get(url).send().await
        .map_err(|e| BotError::Config(format!("fetch {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(BotError::Config(format!("fetch {url} returned {}", response.status())));
    }
    response.text().await
        .map_err(|e| BotError::Config(format!("read {url}: {e}")))
}

/// Download `url` to `dest` atomically (temp file + rename), verifying the
/// SHA-256 when `expected_sha256` is provided. A hash mismatch aborts the
/// install and leaves no file behind — these bytes are executed later, so a
/// tampered or corrupted download must never land on disk. When no hash is
/// available, fall back to a magic-byte sanity check for executables.
/// Fetch a GitHub release's asset SHA-256 digests, keyed by asset filename.
/// GitHub populates `assets[].digest` as `sha256:<hex>` for most releases; any
/// asset without a digest is simply absent from the map. Returns an empty map
/// (not an error) if the release can't be fetched, so verification degrades to
/// the magic-byte fallback rather than blocking installs.
async fn fetch_release_asset_digests(
    client: &reqwest::Client,
    repo: &str,
    tag: &str,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
    let json: serde_json::Value = match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("bgutil release JSON parse failed: {e}");
                return map;
            }
        },
        Ok(resp) => {
            tracing::warn!("bgutil release API returned {}", resp.status());
            return map;
        }
        Err(e) => {
            tracing::warn!("bgutil release API request failed: {e}");
            return map;
        }
    };
    if let Some(assets) = json.get("assets").and_then(|a| a.as_array()) {
        for asset in assets {
            let name = asset.get("name").and_then(|v| v.as_str());
            let digest = asset
                .get("digest")
                .and_then(|v| v.as_str())
                .and_then(|d| d.strip_prefix("sha256:"));
            if let (Some(name), Some(digest)) = (name, digest) {
                map.insert(name.to_string(), digest.to_string());
            }
        }
    }
    map
}

async fn download_verified(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha256: Option<&str>,
    verify_executable_magic: bool,
) -> Result<(), BotError> {
    let response = client.get(url).send().await
        .map_err(|e| BotError::Config(format!("download {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(BotError::Config(format!(
            "download {url} returned {}", response.status()
        )));
    }
    let bytes = response.bytes().await
        .map_err(|e| BotError::Config(format!("read body of {url}: {e}")))?;

    match expected_sha256 {
        Some(expected) => {
            if !verify_sha256(&bytes, expected) {
                return Err(BotError::Config(format!(
                    "checksum mismatch for {url}: expected {expected}, got {}",
                    sha256_hex(&bytes)
                )));
            }
        }
        None => {
            tracing::warn!("No checksum available for {url}; skipping hash verification");
            if verify_executable_magic && !looks_like_executable(&bytes) {
                return Err(BotError::Config(format!(
                    "{url} does not look like a valid executable for this platform"
                )));
            }
        }
    }

    // Write to a temp file then rename, so a failed/partial download never
    // leaves a half-written binary at the destination path.
    let tmp = dest.with_extension("download.tmp");
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| BotError::Config(format!("create {}: {e}", tmp.display())))?;
        f.write_all(&bytes)
            .map_err(|e| BotError::Config(format!("write {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, dest)
        .map_err(|e| BotError::Config(format!("rename to {}: {e}", dest.display())))?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), BotError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| BotError::Config(format!("stat {}: {e}", path.display())))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
        .map_err(|e| BotError::Config(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), BotError> {
    Ok(())
}

fn extract_plugin_zip(zip_path: &Path, dest_dir: &Path) -> Result<(), BotError> {
    let file = fs::File::open(zip_path)
        .map_err(|e| BotError::Config(format!("open zip: {e}")))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| BotError::Config(format!("read zip: {e}")))?;

    fs::create_dir_all(dest_dir)
        .map_err(|e| BotError::Config(format!("mkdir plugin dir: {e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)
            .map_err(|e| BotError::Config(format!("zip entry {i}: {e}")))?;
        let outpath = match entry.enclosed_name() {
            Some(p) => dest_dir.join(p),
            None => continue,
        };
        if entry.is_dir() {
            fs::create_dir_all(&outpath)
                .map_err(|e| BotError::Config(format!("mkdir {}: {e}", outpath.display())))?;
        } else {
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| BotError::Config(format!("mkdir {}: {e}", parent.display())))?;
            }
            let mut out = fs::File::create(&outpath)
                .map_err(|e| BotError::Config(format!("create {}: {e}", outpath.display())))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| BotError::Config(format!("write {}: {e}", outpath.display())))?;
        }
    }
    Ok(())
}

/// Default cookies file path. The bot auto-loads this if it exists when
/// `youtube_cookies_file` is empty.
///
/// Windows: `<config_dir>/cookies.txt` — same dir as `config.json`.
/// Linux/macOS: `~/.config/ttspotify/cookies.txt`.
pub fn default_cookies_path() -> PathBuf {
    crate::config::config_dir().join("cookies.txt")
}

/// Look up an executable on PATH. Returns `Some(path)` if found,
/// `None` otherwise. Mirrors `which`/`where` semantics.
pub fn which(exe_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<&str> = if cfg!(windows) { vec![".exe", ".cmd", ".bat", ""] } else { vec![""] };
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = if ext.is_empty() {
                dir.join(exe_name)
            } else {
                dir.join(format!("{exe_name}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_paths_lands_in_lib_subdir() {
        let paths = resolve_paths().expect("resolve_paths");
        assert!(paths.lib_dir.ends_with("lib"));
        assert!(paths.yt_dlp.starts_with(&paths.lib_dir));
        assert!(paths.bgutil_pot.starts_with(&paths.lib_dir));
        assert!(paths.plugin_dir.starts_with(&paths.lib_dir));
    }

    #[test]
    fn yt_dlp_filename_matches_platform() {
        let paths = resolve_paths().unwrap();
        let name = paths.yt_dlp.file_name().unwrap().to_str().unwrap();
        if cfg!(windows) {
            assert_eq!(name, "yt-dlp.exe");
        } else {
            assert_eq!(name, "yt-dlp");
        }
    }

    #[test]
    fn default_cookies_path_ends_in_cookies_txt() {
        let p = default_cookies_path();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("cookies.txt"));
    }

    #[test]
    fn sha256_of_known_input() {
        // SHA-256 of "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_sha256_matches_case_insensitively() {
        let h = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        assert!(verify_sha256(b"abc", h));
        assert!(!verify_sha256(b"abd", h));
    }

    #[test]
    fn parse_sums_file_finds_asset() {
        let text = "\
aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111  yt-dlp.exe
bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222 *yt-dlp_linux
short  ignored.bin";
        assert_eq!(
            parse_sums_file(text, "yt-dlp.exe").as_deref(),
            Some("aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111")
        );
        // Handles the '*' binary-mode prefix.
        assert_eq!(
            parse_sums_file(text, "yt-dlp_linux").as_deref(),
            Some("bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222")
        );
        // Missing asset -> None; malformed short hash -> not matched.
        assert_eq!(parse_sums_file(text, "nope.exe"), None);
        assert_eq!(parse_sums_file(text, "ignored.bin"), None);
    }
}
