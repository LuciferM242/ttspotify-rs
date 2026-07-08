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

    // 1. yt-dlp
    progress(&format!("Downloading yt-dlp {YT_DLP_VERSION}..."));
    let yt_dlp_url = if cfg!(windows) {
        format!("https://github.com/yt-dlp/yt-dlp/releases/download/{YT_DLP_VERSION}/yt-dlp.exe")
    } else {
        format!("https://github.com/yt-dlp/yt-dlp/releases/download/{YT_DLP_VERSION}/yt-dlp_linux")
    };
    download_to(&client, &yt_dlp_url, &paths.yt_dlp).await?;
    make_executable(&paths.yt_dlp)?;
    progress("  yt-dlp installed.");

    // 2. bgutil-pot
    progress(&format!("Downloading bgutil-pot {BGUTIL_VERSION}..."));
    let bgutil_url = if cfg!(windows) {
        format!("https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{BGUTIL_VERSION}/bgutil-pot-windows-x86_64.exe")
    } else {
        format!("https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{BGUTIL_VERSION}/bgutil-pot-linux-x86_64")
    };
    download_to(&client, &bgutil_url, &paths.bgutil_pot).await?;
    make_executable(&paths.bgutil_pot)?;
    progress("  bgutil-pot installed.");

    // 3. plugin zip
    progress(&format!("Downloading bgutil yt-dlp plugin {BGUTIL_VERSION}..."));
    let plugin_url = format!(
        "https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{BGUTIL_VERSION}/bgutil-ytdlp-pot-provider-rs.zip"
    );
    let zip_path = paths.lib_dir.join("bgutil-plugin.zip");
    download_to(&client, &plugin_url, &zip_path).await?;
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

    progress(&format!("Downloading bgutil-pot {version}..."));
    let bgutil_url = if cfg!(windows) {
        format!("https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{version}/bgutil-pot-windows-x86_64.exe")
    } else {
        format!("https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{version}/bgutil-pot-linux-x86_64")
    };
    download_to(&client, &bgutil_url, &paths.bgutil_pot).await?;
    make_executable(&paths.bgutil_pot)?;

    progress(&format!("Downloading bgutil yt-dlp plugin {version}..."));
    let plugin_url = format!(
        "https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/{version}/bgutil-ytdlp-pot-provider-rs.zip"
    );
    let zip_path = paths.lib_dir.join("bgutil-plugin.zip");
    download_to(&client, &plugin_url, &zip_path).await?;
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

async fn download_to(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
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
    let mut f = fs::File::create(dest)
        .map_err(|e| BotError::Config(format!("create {}: {e}", dest.display())))?;
    f.write_all(&bytes)
        .map_err(|e| BotError::Config(format!("write {}: {e}", dest.display())))?;
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
}
