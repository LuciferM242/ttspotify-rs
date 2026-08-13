//! YouTube binaries auto-installer.
//!
//! Downloads `yt-dlp`, the bgutil-pot binary, and the bgutil yt-dlp plugin
//! into `<exe-dir>/lib/` so the bot can resolve them at runtime without the
//! user installing anything by hand.
//!
//! yt-dlp installs the newest GitHub release (verified against that release's
//! own SHA2-256SUMS), so a fresh install is already current — no second
//! `--update` download on metered connections. bgutil stays pinned below;
//! bump it periodically and ship a new release.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::BotError;

const BGUTIL_VERSION: &str = "v0.8.1";

/// Filename for the sidecar that records which bgutil version is on disk.
/// Lives next to the bgutil binary in `lib/`.
const BGUTIL_VERSION_FILE: &str = ".bgutil-version";

/// Records which Deno we installed, so --update-tools can compare.
const DENO_VERSION_FILE: &str = ".deno-version";

/// Oldest Deno yt-dlp's EJS solver supports. An older one on PATH is ignored
/// rather than used, because the failure it produces looks like a YouTube
/// problem rather than a runtime problem.
const MIN_DENO_VERSION: (u32, u32, u32) = (2, 3, 0);

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
    /// `lib/deno` or `lib/deno.exe`: the JavaScript runtime yt-dlp needs to
    /// solve YouTube's player challenges. Only present when we installed it;
    /// a system-wide Deno is used as-is.
    pub deno: PathBuf,
}

/// Pick the directory the YouTube tools live in.
/// An exe-side `lib/` that already holds tools wins, so existing installs and
/// dev checkouts keep working. Otherwise use `<data_dir>/ttspotify/lib`, which
/// stays user-writable when the binary itself is installed somewhere
/// root-owned like /usr/local/bin.
#[cfg_attr(windows, allow(dead_code))] // Linux-only policy; kept cross-platform for tests
fn pick_tools_dir(legacy: PathBuf, legacy_has_tools: bool, data_dir: Option<PathBuf>) -> PathBuf {
    if legacy_has_tools {
        return legacy;
    }
    match data_dir {
        Some(d) => d.join("ttspotify").join("lib"),
        None => legacy,
    }
}

/// Everything our installer puts into the tools dir. Used by the migration to
/// move exactly our items and nothing else.
fn tool_item_names() -> [&'static str; 6] {
    if cfg!(windows) {
        ["yt-dlp.exe", "bgutil-pot.exe", "yt-dlp-plugins", BGUTIL_VERSION_FILE, "deno.exe", DENO_VERSION_FILE]
    } else {
        ["yt-dlp", "bgutil-pot", "yt-dlp-plugins", BGUTIL_VERSION_FILE, "deno", DENO_VERSION_FILE]
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// One-time move of our tools from a legacy exe-side `lib/` into the new
/// location. Runs only when the legacy dir is provably ours — the
/// `.bgutil-version` sidecar is written by nothing but our installer.
///
/// Copy-verify-delete rather than rename: if anything fails mid-way the
/// legacy dir is still complete and stays the active tools dir, so the bot
/// can never end up with the tools split across two half-dirs. Files the
/// installer didn't create are left alone; the legacy dir itself is removed
/// only when the move emptied it. Returns whether a migration happened.
pub fn migrate_tools_dir(legacy: &Path, target: &Path) -> bool {
    if legacy == target || !legacy.join(BGUTIL_VERSION_FILE).is_file() {
        return false;
    }
    // Copy phase: legacy stays intact until everything landed.
    for name in tool_item_names() {
        let src = legacy.join(name);
        if !src.exists() {
            continue;
        }
        let dest = target.join(name);
        let copied = if src.is_dir() {
            copy_dir_recursive(&src, &dest)
        } else {
            std::fs::create_dir_all(target).and_then(|()| std::fs::copy(&src, &dest).map(|_| ()))
        };
        if let Err(e) = copied {
            tracing::warn!(
                "YouTube tools migration aborted (copying {name}: {e}); staying in {}",
                legacy.display()
            );
            return false;
        }
    }
    // Delete phase: failures here leave harmless duplicates, never a split.
    for name in tool_item_names() {
        let src = legacy.join(name);
        let removed = if src.is_dir() {
            std::fs::remove_dir_all(&src)
        } else if src.exists() {
            std::fs::remove_file(&src)
        } else {
            Ok(())
        };
        if let Err(e) = removed {
            tracing::warn!("Could not remove migrated {name} from old tools dir: {e}");
        }
    }
    // Only ours in there? Then the folder goes too. remove_dir refuses
    // non-empty dirs, which is exactly the guard we want.
    let _ = std::fs::remove_dir(legacy);
    tracing::info!(
        "Moved YouTube tools from {} to {}",
        legacy.display(),
        target.display()
    );
    true
}

/// Move a legacy exe-side tools install to the XDG data dir (Linux only; on
/// Windows the exe-side dir remains the home). Call at startup before
/// anything resolves tool paths.
#[cfg(not(windows))]
pub fn migrate_legacy_tools() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(exe_dir) = exe.parent() else { return };
    let Some(data) = dirs::data_dir() else { return };
    migrate_tools_dir(&exe_dir.join("lib"), &data.join("ttspotify").join("lib"));
}

/// Compute where the binaries should live.
/// Windows: `<dir of current_exe>\lib` (unchanged; installs are per-user).
/// Linux: exe-side `lib/` when it already holds the tools, else
/// `~/.local/share/ttspotify/lib` (see `pick_tools_dir`).
pub fn resolve_paths() -> Result<YoutubeSetupPaths, BotError> {
    let exe = std::env::current_exe()
        .map_err(|e| BotError::Config(format!("current_exe failed: {e}")))?;
    let exe_dir = exe.parent()
        .ok_or_else(|| BotError::Config("current_exe has no parent".to_string()))?;
    let legacy_lib = exe_dir.join("lib");
    #[cfg(windows)]
    let lib_dir = legacy_lib;
    #[cfg(not(windows))]
    let lib_dir = {
        let has_tools = legacy_lib.join("yt-dlp").is_file() || legacy_lib.join("bgutil-pot").is_file();
        pick_tools_dir(legacy_lib, has_tools, dirs::data_dir())
    };
    let (yt_dlp_name, bgutil_name, deno_name) = if cfg!(windows) {
        ("yt-dlp.exe", "bgutil-pot.exe", "deno.exe")
    } else {
        ("yt-dlp", "bgutil-pot", "deno")
    };
    Ok(YoutubeSetupPaths {
        yt_dlp: lib_dir.join(yt_dlp_name),
        bgutil_pot: lib_dir.join(bgutil_name),
        plugin_dir: lib_dir.join("yt-dlp-plugins"),
        deno: lib_dir.join(deno_name),
        lib_dir,
    })
}

/// True if all three components are present on disk.
pub fn is_installed(paths: &YoutubeSetupPaths) -> bool {
    paths.yt_dlp.is_file() && paths.bgutil_pot.is_file() && paths.plugin_dir.is_dir()
}

/// Detected versions of the YouTube tools, for the startup version log.
/// `None` means the tool isn't installed.
pub struct ToolVersions {
    pub yt_dlp: Option<String>,
    pub bgutil: Option<String>,
    /// The JavaScript runtime yt-dlp uses for YouTube. `None` means none was
    /// found, which is why formats can go missing.
    pub js_runtime: Option<String>,
}

/// Detect installed YouTube tool versions: `yt-dlp --version` (bundled first,
/// then PATH) and the bgutil sidecar version file.
pub fn installed_tool_versions() -> ToolVersions {
    let paths = resolve_paths().ok();

    let yt_dlp_exe = paths
        .as_ref()
        .map(|p| p.yt_dlp.clone())
        .filter(|p| p.is_file())
        .or_else(|| which("yt-dlp"));
    let yt_dlp = yt_dlp_exe.and_then(|exe| {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--version");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = cmd.output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    });

    // Run the binary rather than trusting the sidecar: the sidecar says what
    // was downloaded, not what works.
    let bgutil = paths.as_ref().and_then(|p| {
        describe_bgutil(
            p.bgutil_pot.is_file(),
            probed_bgutil_version(p),
            &installed_bgutil_version(p),
        )
    });

    let js_runtime = paths.as_ref().and_then(|p| match find_js_runtime(p) {
        JsRuntime::Bundled(exe) => Some(
            deno_version_of(&exe)
                .map(|(a, b, c)| format!("deno {a}.{b}.{c} (bundled)"))
                .unwrap_or_else(|| "deno (bundled)".to_string()),
        ),
        JsRuntime::OnPath => Some(
            which("deno")
                .and_then(|exe| deno_version_of(&exe))
                .map(|(a, b, c)| format!("deno {a}.{b}.{c} (system)"))
                .unwrap_or_else(|| "deno (system)".to_string()),
        ),
        JsRuntime::Missing => None,
    });

    ToolVersions { yt_dlp, bgutil, js_runtime }
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

    // 1. yt-dlp — install the newest release, verified against that release's
    // SHA2-256SUMS manifest. The `latest` alias redirects to the current tag;
    // fetching the asset and its manifest from the same alias keeps them paired.
    progress("Downloading yt-dlp (latest)...");
    let yt_dlp_asset = if cfg!(windows) {
        "yt-dlp.exe"
    } else if cfg!(target_arch = "aarch64") {
        "yt-dlp_linux_aarch64"
    } else {
        "yt-dlp_linux"
    };
    let yt_dlp_url = format!(
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/{yt_dlp_asset}"
    );
    let yt_dlp_hash = match fetch_text(
        &client,
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS",
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
    let bgutil_asset = if cfg!(windows) {
        "bgutil-pot-windows-x86_64.exe"
    } else if cfg!(target_arch = "aarch64") {
        "bgutil-pot-linux-aarch64"
    } else {
        "bgutil-pot-linux-x86_64"
    };
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

    // 4. JavaScript runtime. Since yt-dlp 2025.11.12 YouTube's player
    // challenges are solved by running its own JavaScript, so without a
    // runtime formats go missing. Deno is the one yt-dlp enables by default,
    // and it sandboxes that untrusted code away from the file system and
    // network. Skipped when the machine already has a usable one.
    match find_js_runtime(paths) {
        JsRuntime::OnPath => {
            progress("  JavaScript runtime: using the Deno already installed on this system.");
        }
        JsRuntime::Bundled(path) => {
            progress(&format!("  JavaScript runtime: already installed ({}).", path.display()));
        }
        JsRuntime::Missing => {
            progress("Downloading Deno (JavaScript runtime for YouTube)...");
            if let Err(e) = install_deno(&client, paths, &progress).await {
                // Not fatal: YouTube still plays, just with fewer formats.
                tracing::warn!("Deno install failed: {e}");
                progress(&format!(
                    "  Could not install Deno ({e}). YouTube will still work, but some formats may be unavailable."
                ));
            }
        }
    }

    // Record what we just installed so --update-tools can compare later.
    let _ = fs::write(paths.lib_dir.join(BGUTIL_VERSION_FILE), BGUTIL_VERSION);

    progress(&format!("YouTube support ready in {}", paths.lib_dir.display()));
    Ok(())
}

/// The Deno release asset for this platform. Deno ships one zip per target,
/// each holding a single binary.
fn deno_asset_name() -> &'static str {
    if cfg!(windows) {
        if cfg!(target_arch = "aarch64") {
            "deno-aarch64-pc-windows-msvc.zip"
        } else {
            "deno-x86_64-pc-windows-msvc.zip"
        }
    } else if cfg!(target_arch = "aarch64") {
        "deno-aarch64-unknown-linux-gnu.zip"
    } else {
        "deno-x86_64-unknown-linux-gnu.zip"
    }
}

/// Pull a version out of `deno --version` output, whose first line reads
/// `deno 2.9.5 (stable, release, x86_64-pc-windows-msvc)`.
fn parse_deno_version(output: &str) -> Option<(u32, u32, u32)> {
    let first = output.lines().next()?;
    let version = first.split_whitespace().nth(1)?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // A pre-release suffix ("3.0.0-rc.1") still counts as that patch level.
    let patch = parts
        .next()
        .unwrap_or("0")
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether a Deno is new enough for yt-dlp's solver.
fn deno_is_supported(version: (u32, u32, u32)) -> bool {
    version >= MIN_DENO_VERSION
}

/// Ask a Deno binary its version.
fn deno_version_of(exe: &Path) -> Option<(u32, u32, u32)> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    // Without this the tray, which has no console of its own, makes Windows
    // give deno a new one: a black window titled with deno's path, at startup
    // and again whenever a bundled deno is looked up. The other probes in this
    // file already do it.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok()?;
    parse_deno_version(&String::from_utf8_lossy(&out.stdout))
}

/// A usable JavaScript runtime, preferring one already on the system so an
/// install does not download 100 MB somebody already has.
///
/// Returns the path only when we must tell yt-dlp about it: a Deno on PATH is
/// found by yt-dlp itself.
pub enum JsRuntime {
    /// Found on PATH; yt-dlp picks it up with no arguments.
    OnPath,
    /// Ours, in `lib/`; yt-dlp must be pointed at it.
    Bundled(PathBuf),
    /// Nothing usable. YouTube still works, but formats will be missing.
    Missing,
}

/// Find a JavaScript runtime for yt-dlp: ours if installed, else a new enough
/// one on PATH.
pub fn find_js_runtime(paths: &YoutubeSetupPaths) -> JsRuntime {
    if paths.deno.is_file() {
        return JsRuntime::Bundled(paths.deno.clone());
    }
    if let Some(system) = which("deno") {
        match deno_version_of(&system) {
            Some(v) if deno_is_supported(v) => return JsRuntime::OnPath,
            Some(v) => tracing::info!(
                "Ignoring Deno {}.{}.{} on PATH: yt-dlp needs {}.{}.{} or newer",
                v.0, v.1, v.2, MIN_DENO_VERSION.0, MIN_DENO_VERSION.1, MIN_DENO_VERSION.2
            ),
            None => tracing::debug!("Could not read the version of the Deno on PATH"),
        }
    }
    JsRuntime::Missing
}

/// Download Deno into `lib/`, verified against the release's sha256sum file.
async fn install_deno(
    client: &reqwest::Client,
    paths: &YoutubeSetupPaths,
    progress: &impl Fn(&str),
) -> Result<(), BotError> {
    let asset = deno_asset_name();
    let base = format!("https://github.com/denoland/deno/releases/latest/download/{asset}");
    // Deno publishes one checksum file per asset.
    let hash = match fetch_text(client, &format!("{base}.sha256sum")).await {
        Ok(text) => text.split_whitespace().next().map(str::to_string),
        Err(e) => {
            tracing::warn!("Could not fetch the Deno checksum: {e}");
            None
        }
    };
    let zip_path = paths.lib_dir.join("deno.zip");
    download_verified(client, &base, &zip_path, hash.as_deref(), false).await?;
    extract_single_file(&zip_path, &paths.deno)?;
    let _ = fs::remove_file(&zip_path);
    make_executable(&paths.deno)?;

    let version = deno_version_of(&paths.deno)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .unwrap_or_else(|| "unknown".to_string());
    let _ = fs::write(paths.lib_dir.join(DENO_VERSION_FILE), &version);
    progress(&format!("  Deno {version} installed."));
    Ok(())
}

/// Pull the one binary out of a single-file archive.
fn extract_single_file(zip_path: &Path, dest: &Path) -> Result<(), BotError> {
    let file = fs::File::open(zip_path)
        .map_err(|e| BotError::Config(format!("open zip: {e}")))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| BotError::Config(format!("read zip: {e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| BotError::Config(format!("zip entry {i}: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| BotError::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        let mut out = fs::File::create(dest)
            .map_err(|e| BotError::Config(format!("create {}: {e}", dest.display())))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| BotError::Config(format!("write {}: {e}", dest.display())))?;
        return Ok(());
    }
    Err(BotError::Config("the Deno archive was empty".to_string()))
}

/// Install or refresh the JavaScript runtime as part of --update-tools.
///
/// Does nothing when the system provides one: that Deno is somebody else's to
/// update. Ours is replaced with the current release, which is the only way to
/// keep pace with the player challenges yt-dlp has to solve.
pub async fn update_js_runtime(
    paths: &YoutubeSetupPaths,
    progress: impl Fn(&str),
) -> Result<(), BotError> {
    if let JsRuntime::OnPath = find_js_runtime(paths) {
        progress("  Using the Deno installed on this system; nothing to update.");
        return Ok(());
    }
    let before = installed_deno_version(paths);
    let client = reqwest::Client::builder()
        .user_agent(concat!("tt-spotify-bot/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| BotError::Config(format!("HTTP client: {e}")))?;
    install_deno(&client, paths, &progress).await?;
    match (before, installed_deno_version(paths)) {
        (Some(old), Some(new)) if old == new => progress(&format!("  Deno already on {new}.")),
        (Some(old), Some(new)) => progress(&format!("  Deno updated from {old} to {new}.")),
        _ => {}
    }
    Ok(())
}

/// The Deno we installed, if any.
pub fn installed_deno_version(paths: &YoutubeSetupPaths) -> Option<String> {
    fs::read_to_string(paths.lib_dir.join(DENO_VERSION_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ask the bgutil binary its version, by running it.
///
/// The sidecar file records what we downloaded; this reports what actually
/// runs. They disagree when the binary is missing, corrupt, or built for
/// another architecture - and a PO token provider that cannot run is the
/// difference between YouTube playing and answering 403.
pub fn probed_bgutil_version(paths: &YoutubeSetupPaths) -> Option<String> {
    if !paths.bgutil_pot.is_file() {
        return None;
    }
    let mut cmd = std::process::Command::new(&paths.bgutil_pot);
    cmd.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_bgutil_version(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the version out of `bgutil-pot --version`, which prints
/// `bgutil-pot 0.8.1`.
fn parse_bgutil_version(output: &str) -> Option<String> {
    let first = output.lines().next()?.trim();
    let version = first.split_whitespace().nth(1)?;
    (!version.is_empty()).then(|| version.to_string())
}

/// How to describe bgutil in the startup log.
///
/// A binary that is present but will not run is the case worth naming: it
/// looks installed everywhere else, and its only symptom is tracks failing
/// with no stated reason.
pub fn describe_bgutil(file_exists: bool, probed: Option<String>, sidecar: &str) -> Option<String> {
    match (file_exists, probed) {
        (false, _) => None,
        (true, Some(version)) => Some(version),
        (true, None) => Some(format!(
            "{sidecar} (INSTALLED BUT WILL NOT RUN - YouTube will fail with 403)"
        )),
    }
}

/// Returns the bgutil version recorded on disk (read from the sidecar).
/// Falls back to the pinned const if the sidecar is missing, which covers
/// older installs that predate the sidecar. This is what `--update-tools`
/// compares against; for what actually runs, see `probed_bgutil_version`.
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
    let bgutil_asset = if cfg!(windows) {
        "bgutil-pot-windows-x86_64.exe"
    } else if cfg!(target_arch = "aarch64") {
        "bgutil-pot-linux-aarch64"
    } else {
        "bgutil-pot-linux-x86_64"
    };
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
        // Skip lines that don't parse (blank, comment) instead of aborting:
        // a `?` here made one blank line silently disable hash verification
        // for the whole file.
        let Some(hash) = parts.next() else { continue };
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
    crate::paths::configs_dir().join("cookies.txt")
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

    fn mig_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ttspotify_toolmig_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_legacy_install(legacy: &Path) {
        std::fs::create_dir_all(legacy).unwrap();
        for name in tool_item_names() {
            if name == "yt-dlp-plugins" {
                let plug = legacy.join(name).join("bgutil_ytdlp_pot_provider");
                std::fs::create_dir_all(&plug).unwrap();
                std::fs::write(plug.join("plugin.py"), "py").unwrap();
            } else {
                std::fs::write(legacy.join(name), name).unwrap();
            }
        }
    }

    #[test]
    fn migrates_marked_lib_and_removes_empty_legacy() {
        let base = mig_tmp("full");
        let legacy = base.join("lib");
        fake_legacy_install(&legacy);
        let target = base.join("data").join("ttspotify").join("lib");

        assert!(migrate_tools_dir(&legacy, &target));
        for name in tool_item_names() {
            assert!(target.join(name).exists(), "missing {name} in target");
            assert!(!legacy.join(name).exists(), "{name} left in legacy");
        }
        // Plugin contents survived the move.
        assert!(target.join("yt-dlp-plugins").join("bgutil_ytdlp_pot_provider").join("plugin.py").is_file());
        // Nothing of ours left: the folder itself goes too.
        assert!(!legacy.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_lib_without_our_marker() {
        // No .bgutil-version sidecar: could be anyone's lib folder. Hands off.
        let base = mig_tmp("unmarked");
        let legacy = base.join("lib");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("yt-dlp"), "x").unwrap();
        let target = base.join("data").join("lib");

        assert!(!migrate_tools_dir(&legacy, &target));
        assert!(legacy.join("yt-dlp").is_file());
        assert!(!target.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn keeps_legacy_dir_when_it_holds_foreign_files() {
        let base = mig_tmp("foreign");
        let legacy = base.join("lib");
        fake_legacy_install(&legacy);
        std::fs::write(legacy.join("users-own-notes.txt"), "keep me").unwrap();
        let target = base.join("data").join("lib");

        assert!(migrate_tools_dir(&legacy, &target));
        // Our items moved, the stranger's file and its folder stay.
        assert!(legacy.join("users-own-notes.txt").is_file());
        assert!(!legacy.join(BGUTIL_VERSION_FILE).exists());
        assert!(target.join(BGUTIL_VERSION_FILE).is_file());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn existing_exe_side_lib_with_tools_wins() {
        let legacy = PathBuf::from("/opt/bot/lib");
        let picked = pick_tools_dir(legacy.clone(), true, Some(PathBuf::from("/home/u/.local/share")));
        assert_eq!(picked, legacy);
    }

    #[test]
    fn fresh_install_uses_xdg_data_dir() {
        let picked = pick_tools_dir(
            PathBuf::from("/usr/local/bin/lib"),
            false,
            Some(PathBuf::from("/home/u/.local/share")),
        );
        assert_eq!(picked, PathBuf::from("/home/u/.local/share/ttspotify/lib"));
    }

    #[test]
    fn missing_data_dir_falls_back_to_exe_side_lib() {
        let legacy = PathBuf::from("/opt/bot/lib");
        assert_eq!(pick_tools_dir(legacy.clone(), false, None), legacy);
    }

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

    #[test]
    fn parse_sums_file_survives_blank_and_comment_lines() {
        // A blank line used to abort the scan via `?`, silently downgrading
        // yt-dlp verification to the magic-byte fallback.
        let text = "\n# comment\n\naaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111  yt-dlp.exe\n";
        assert_eq!(
            parse_sums_file(text, "yt-dlp.exe").as_deref(),
            Some("aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111")
        );
    }
}

#[cfg(test)]
mod bgutil_reporting_tests {
    use super::*;

    #[test]
    fn a_working_binary_reports_the_version_it_printed() {
        assert_eq!(
            describe_bgutil(true, Some("0.8.1".to_string()), "v0.8.0"),
            Some("0.8.1".to_string()),
            "what runs wins over what the sidecar remembers"
        );
    }

    #[test]
    fn a_binary_that_will_not_run_says_so_loudly() {
        // The case that hid a broken PO token provider for months: present on
        // disk, reported as installed, and silently doing nothing. Its only
        // symptom was tracks failing with no stated reason, so the log has to
        // name it.
        let described = describe_bgutil(true, None, "v0.8.1").expect("should describe");
        assert!(described.contains("WILL NOT RUN"), "got: {described}");
        assert!(described.contains("403"), "should name the symptom: {described}");
    }

    #[test]
    fn a_missing_binary_is_simply_absent() {
        // Not installed is a normal state for a Spotify-only user, and must not
        // read as a fault.
        assert_eq!(describe_bgutil(false, None, "v0.8.1"), None);
    }

    #[test]
    fn the_version_is_read_from_what_the_binary_prints() {
        assert_eq!(
            parse_bgutil_version("bgutil-pot 0.8.1
"),
            Some("0.8.1".to_string())
        );
        assert_eq!(
            parse_bgutil_version("bgutil-pot 1.0.0-rc.2"),
            Some("1.0.0-rc.2".to_string())
        );
    }

    #[test]
    fn unreadable_version_output_is_not_invented() {
        // Better to report "will not run" than to make a version up.
        assert_eq!(parse_bgutil_version(""), None);
        assert_eq!(parse_bgutil_version("bgutil-pot"), None);
        assert_eq!(parse_bgutil_version("

"), None);
    }
}

#[cfg(test)]
mod deno_tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // The real shape of `deno --version` output.
    #[case("deno 2.9.5 (stable, release, x86_64-pc-windows-msvc)\nv8 14.2\ntypescript 5.9", Some((2, 9, 5)))]
    #[case("deno 2.3.0", Some((2, 3, 0)))]
    // Pre-releases count as their patch level rather than failing to parse.
    #[case("deno 3.0.0-rc.1 (canary)", Some((3, 0, 0)))]
    #[case("deno 2.4", Some((2, 4, 0)))]
    // Anything else must not be mistaken for a version.
    #[case("", None)]
    #[case("bash: deno: command not found", None)]
    #[case("node v22.1.0", None)]
    fn parse_deno_version_reads_the_first_line(
        #[case] output: &str,
        #[case] expected: Option<(u32, u32, u32)>,
    ) {
        assert_eq!(parse_deno_version(output), expected);
    }

    #[rstest]
    // yt-dlp's EJS solver needs 2.3.0 or newer.
    #[case((2, 3, 0), true)]
    #[case((2, 9, 5), true)]
    #[case((3, 0, 0), true)]
    // An older Deno is worse than none: it fails in a way that reads as a
    // YouTube problem, so it must be rejected rather than used.
    #[case((2, 2, 9), false)]
    #[case((1, 46, 0), false)]
    fn only_new_enough_deno_is_accepted(
        #[case] version: (u32, u32, u32),
        #[case] supported: bool,
    ) {
        assert_eq!(deno_is_supported(version), supported);
    }

    #[test]
    fn the_deno_asset_matches_this_platform() {
        let asset = deno_asset_name();
        assert!(asset.starts_with("deno-"), "got {asset}");
        assert!(asset.ends_with(".zip"), "Deno ships zips: {asset}");
        if cfg!(windows) {
            assert!(asset.contains("pc-windows-msvc"), "got {asset}");
        } else {
            assert!(asset.contains("unknown-linux-gnu"), "got {asset}");
        }
        if cfg!(target_arch = "aarch64") {
            assert!(asset.starts_with("deno-aarch64"), "got {asset}");
        } else {
            assert!(asset.starts_with("deno-x86_64"), "got {asset}");
        }
    }

    #[test]
    fn resolved_paths_include_the_runtime_beside_the_other_tools() {
        let paths = resolve_paths().expect("paths resolve");
        assert_eq!(paths.deno.parent(), Some(paths.lib_dir.as_path()));
        let name = paths.deno.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, if cfg!(windows) { "deno.exe" } else { "deno" });
    }
}
