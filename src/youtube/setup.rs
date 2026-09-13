//! YouTube tooling installer.
//!
//! Installs the JavaScript runtime the audio sidecar runs on, writes the
//! sidecar and its pinned dependency files out of this binary, and warms the
//! dependency cache so the first track does not pay a cold npm fetch.
//!
//! Only the runtime is downloaded. The sidecar, its import map and its
//! lockfile are compiled in, so an update ships new ones without a download,
//! and every install runs the dependency versions that were tested.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::BotError;





/// Stamp written by versions that installed bgutil-pot. Nothing writes it any
/// more; it survives only as proof that a directory was made by an older
/// install, so the migration can recognise and clean it.
const BGUTIL_VERSION_FILE: &str = ".bgutil-version";

/// Records which Deno we installed, so --update-tools can compare.
const DENO_VERSION_FILE: &str = ".deno-version";

/// Oldest Deno the sidecar is known to run on. An older one on PATH is ignored
/// rather than used, because the failure it produces looks like a YouTube
/// problem rather than a runtime problem.
///
/// A minimum, deliberately not an exact pin: Deno is a general-purpose runtime
/// with a stability promise, and a user who already has one should not be made
/// to download a second. What actually breaks between releases is the npm
/// dependencies, and those are pinned by the sidecar's lockfile.
const MIN_DENO_VERSION: (u32, u32, u32) = (2, 3, 0);

/// Resolved on-disk paths for the YouTube tools.
#[derive(Debug, Clone)]
pub struct YoutubeSetupPaths {
    /// Directory holding the runtime and the sidecar's three files.
    pub lib_dir: PathBuf,
    /// `lib/deno` or `lib/deno.exe`. Only present when we installed it; a
    /// new enough Deno already on PATH is used as-is.
    pub deno: PathBuf,
}

fn deno_name() -> &'static str {
    if cfg!(windows) { "deno.exe" } else { "deno" }
}
fn ytdlp_name() -> &'static str {
    if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" }
}
fn bgutil_name() -> &'static str {
    if cfg!(windows) { "bgutil-pot.exe" } else { "bgutil-pot" }
}

/// What a current install puts in the tools dir. Carried across a migration.
fn live_item_names() -> [&'static str; 5] {
    [
        deno_name(),
        DENO_VERSION_FILE,
        crate::youtube::sidecar::SCRIPT_NAME,
        crate::youtube::sidecar::DENO_CONFIG_NAME,
        crate::youtube::sidecar::DENO_LOCK_NAME,
    ]
}

/// What older versions installed and nothing reads any more. Removed rather
/// than carried across, so an upgrade reclaims the disk instead of moving dead
/// weight into the directory the bot actively uses.
fn dead_item_names() -> [&'static str; 4] {
    [ytdlp_name(), bgutil_name(), "yt-dlp-plugins", BGUTIL_VERSION_FILE]
}

/// True when this directory is one our installer made. `.bgutil-version`
/// proves an old install; the sidecar script or the Deno stamp prove a
/// current one.
fn is_our_tools_dir(dir: &Path) -> bool {
    dir.join(BGUTIL_VERSION_FILE).is_file()
        || dir.join(crate::youtube::sidecar::SCRIPT_NAME).is_file()
        || dir.join(DENO_VERSION_FILE).is_file()
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
    if legacy == target || !is_our_tools_dir(legacy) {
        return false;
    }
    // Copy phase: legacy stays intact until everything landed.
    for name in live_item_names() {
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
    // Delete phase, live items: failures leave harmless duplicates, never a
    // split.
    for name in live_item_names() {
        remove_item(&legacy.join(name), name);
    }
    // Delete phase, dead items: nothing reads these again, so they are removed
    // rather than moved. Only these exact names - anything else the user keeps
    // in that folder is left alone.
    for name in dead_item_names() {
        let path = legacy.join(name);
        if path.exists() {
            tracing::info!("Removing obsolete YouTube tool: {}", path.display());
            remove_item(&path, name);
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

fn remove_item(path: &Path, name: &str) {
    let removed = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else if path.exists() {
        std::fs::remove_file(path)
    } else {
        Ok(())
    };
    if let Err(e) = removed {
        tracing::warn!("Could not remove {name} from the old tools dir: {e}");
    }
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

/// Where the binaries live.
/// Windows: `<dir of current_exe>\lib`. Linux: `~/.local/share/ttspotify/lib`.
///
/// No probing. The directory used to be chosen by checking whether yt-dlp sat
/// beside the executable, which meant the answer changed the moment the set of
/// installed tools changed - exactly what happened when yt-dlp was removed.
/// `migrate_legacy_tools()` runs at startup and moves any exe-side install
/// into place, so there is nothing left to infer.
pub fn resolve_paths() -> Result<YoutubeSetupPaths, BotError> {
    let exe = std::env::current_exe()
        .map_err(|e| BotError::Config(format!("current_exe failed: {e}")))?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| BotError::Config("current_exe has no parent".to_string()))?;

    #[cfg(windows)]
    let lib_dir = exe_dir.join("lib");
    #[cfg(not(windows))]
    let lib_dir = match dirs::data_dir() {
        Some(d) => d.join("ttspotify").join("lib"),
        None => exe_dir.join("lib"),
    };

    Ok(YoutubeSetupPaths { deno: lib_dir.join(deno_name()), lib_dir })
}

/// True when YouTube can play: the sidecar script is written and a usable
/// runtime exists, ours or a new enough one already on the system.
pub fn is_installed(paths: &YoutubeSetupPaths) -> bool {
    // The script first: without it there is no reason to go looking for Deno.
    let script_present = paths.lib_dir.join(crate::youtube::sidecar::SCRIPT_NAME).is_file();
    script_present && tools_installed(script_present, &find_js_runtime(paths))
}

/// Whether the tools count as installed, given what was found.
///
/// A Deno on PATH counts. Counting only ours in `lib/` meant a machine with its
/// own Deno played YouTube fine while the tray kept offering Install and never
/// enabled Update.
pub fn tools_installed(script_present: bool, runtime: &JsRuntime) -> bool {
    script_present && !matches!(runtime, JsRuntime::Missing)
}

/// Tools an older version installed for yt-dlp. They cannot play anything now,
/// so their presence without the sidecar means the tools need installing.
pub fn has_legacy_tools(paths: &YoutubeSetupPaths) -> bool {
    paths.lib_dir.join(BGUTIL_VERSION_FILE).is_file() || paths.lib_dir.join(ytdlp_name()).is_file()
}

/// Detected versions of the YouTube tools, for the startup version log.
/// `None` means the tool isn't installed.
pub struct ToolVersions {
    /// The JavaScript runtime the sidecar runs on. `None` means none was
    /// found, which is why YouTube playback cannot start.
    pub js_runtime: Option<String>,
}

/// Detect which JavaScript runtime is installed, bundled copy first.
pub fn installed_tool_versions() -> ToolVersions {
    let paths = resolve_paths().ok();

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

    ToolVersions { js_runtime }
}

/// Install the JavaScript runtime, write the sidecar, and warm its dependency
/// cache. Reports progress via the callback.
pub async fn install(
    paths: &YoutubeSetupPaths,
    progress: impl Fn(&str),
) -> Result<(), BotError> {
    fs::create_dir_all(&paths.lib_dir)
        .map_err(|e| BotError::Config(format!("create lib dir: {e}")))?;

    // 1. JavaScript runtime. The sidecar is a Deno program, so unlike the old
    // yt-dlp setup - where a missing runtime merely cost some formats - this
    // is the thing YouTube playback is. A Deno the user already installed is
    // used as-is rather than downloading a second copy.
    match find_js_runtime(paths) {
        JsRuntime::OnPath => {
            progress("  JavaScript runtime: using the Deno already installed on this system.");
        }
        JsRuntime::Bundled(path) => {
            progress(&format!("  JavaScript runtime: already installed ({}).", path.display()));
        }
        JsRuntime::Missing => {
            progress("Downloading Deno (the runtime YouTube playback needs)...");
            let client = http_client()?;
            install_deno(&client, paths, &progress).await?;
        }
    }

    // 2. The sidecar itself, plus its import map and lockfile. All three are
    // compiled into this binary, so there is nothing to download.
    crate::youtube::sidecar::ensure_script(&paths.lib_dir)?;
    progress("  Sidecar script installed.");

    // 3. Warm the dependency cache.
    warm_dependency_cache(paths, &progress);

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

/// Ask a Deno binary its version, once per binary.
///
/// The tray asks whether the tools are installed on its message loop, and a
/// Deno on PATH can only answer by being run. Remembering the answer per file
/// keeps a process launch off that thread after the first time, and an upgrade
/// that replaces the file is asked again.
fn deno_version_of(exe: &Path) -> Option<(u32, u32, u32)> {
    let Ok(meta) = fs::metadata(exe) else {
        return probe_deno_version(exe);
    };
    let key = RuntimeKey {
        path: exe.to_path_buf(),
        len: meta.len(),
        modified: meta.modified().ok(),
    };
    static VERSIONS: std::sync::OnceLock<VersionMemo> = std::sync::OnceLock::new();
    VERSIONS.get_or_init(VersionMemo::default).get(key, || probe_deno_version(exe))
}

fn probe_deno_version(exe: &Path) -> Option<(u32, u32, u32)> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    crate::proc::hide_console_window(&mut cmd);
    let out = cmd.output().ok()?;
    parse_deno_version(&String::from_utf8_lossy(&out.stdout))
}

/// Identifies one Deno binary on disk. A replaced file differs in size or
/// modification time, which is what makes an upgrade get asked again.
#[derive(Clone, PartialEq, Eq, Hash)]
struct RuntimeKey {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// Versions already read, by binary.
#[derive(Default)]
struct VersionMemo {
    seen: parking_lot::Mutex<std::collections::HashMap<RuntimeKey, (u32, u32, u32)>>,
}

impl VersionMemo {
    fn get(
        &self,
        key: RuntimeKey,
        probe: impl FnOnce() -> Option<(u32, u32, u32)>,
    ) -> Option<(u32, u32, u32)> {
        if let Some(v) = self.seen.lock().get(&key) {
            return Some(*v);
        }
        // Probed outside the lock: it runs a process. A failure is not kept,
        // so a Deno that could not start once is asked again next time rather
        // than reading as missing until the bot restarts.
        let found = probe();
        if let Some(v) = found {
            self.seen.lock().insert(key, v);
        }
        found
    }
}

/// What Update says when the runtime is one the user installed.
fn system_deno_update_note(version: Option<(u32, u32, u32)>) -> String {
    let deno = version
        .map(|(a, b, c)| format!("Deno {a}.{b}.{c}"))
        .unwrap_or_else(|| "Deno".to_string());
    format!(
        "Using the {deno} installed on this system, which the bot leaves alone. \
         Update it the way it was installed, for example with `deno upgrade`."
    )
}

/// Fetch the sidecar's npm dependencies into the bot's Deno cache.
///
/// Without this the first track pays a cold npm fetch, and a machine that is
/// offline afterwards never plays at all. Run on update too, because a new bot
/// version can ship a lockfile naming versions the cache does not hold yet.
/// Not fatal: Deno fetches them by itself on first use if the network was down.
pub fn warm_dependency_cache(paths: &YoutubeSetupPaths, progress: &dyn Fn(&str)) {
    progress("Fetching the sidecar's dependencies...");
    let deno = match find_js_runtime(paths) {
        JsRuntime::Bundled(p) => p,
        _ => which(deno_name()).unwrap_or_else(|| PathBuf::from(deno_name())),
    };
    let mut cmd = std::process::Command::new(&deno);
    cmd.arg("cache")
        .arg("--config")
        .arg(paths.lib_dir.join(crate::youtube::sidecar::DENO_CONFIG_NAME))
        .arg("--lock")
        .arg(paths.lib_dir.join(crate::youtube::sidecar::DENO_LOCK_NAME))
        .arg("--frozen")
        .arg(paths.lib_dir.join(crate::youtube::sidecar::SCRIPT_NAME))
        .env("DENO_DIR", crate::youtube::sidecar::deno_cache_dir());
    crate::proc::hide_console_window(&mut cmd);
    match cmd.output() {
        Ok(out) if out.status.success() => progress("  Dependencies cached."),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            tracing::warn!("deno cache failed: {}", err.trim());
            progress("  Could not pre-fetch dependencies; they will be fetched on first play.");
        }
        Err(e) => {
            tracing::warn!("deno cache could not run: {e}");
            progress("  Could not pre-fetch dependencies; they will be fetched on first play.");
        }
    }
}

/// A usable JavaScript runtime, preferring one already on the system so an
/// install does not download 40 MB somebody already has.
///
/// A minimum version rather than an exact pin: Deno has a stability promise,
/// and what actually breaks between releases is the npm dependencies, which
/// the sidecar's lockfile pins instead.
pub enum JsRuntime {
    /// A new enough Deno found on PATH. Used as-is.
    OnPath,
    /// Ours, in `lib/`, installed because none was found or it was too old.
    Bundled(PathBuf),
    /// Nothing usable. YouTube playback cannot start at all: the sidecar is a
    /// Deno program, not an accessory to one.
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
        // Extract to a temp file then rename, like download_verified: writing
        // straight to `dest` truncated the working binary first, so a failed
        // copy left a corrupt file that find_js_runtime still counted as
        // installed. The suffix differs from download_verified's so the two
        // steps never share a temp path (lib/deno.zip's download temp is
        // lib/deno.download.tmp, which lib/deno would also map to).
        let tmp = dest.with_extension("extract.tmp");
        let result = fs::File::create(&tmp)
            .map_err(|e| BotError::Config(format!("create {}: {e}", tmp.display())))
            .and_then(|mut out| {
                std::io::copy(&mut entry, &mut out)
                    .map_err(|e| BotError::Config(format!("write {}: {e}", tmp.display())))
            })
            .and_then(|_| {
                fs::rename(&tmp, dest)
                    .map_err(|e| BotError::Config(format!("rename to {}: {e}", dest.display())))
            });
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
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
        let version = which("deno").and_then(|exe| deno_version_of(&exe));
        progress(&format!("  {}", system_deno_update_note(version)));
        return Ok(());
    }
    let before = installed_deno_version(paths);
    let client = http_client()?;
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

/// Shared HTTP client for all tool downloads — the crate-wide stall-bounded
/// policy (see `crate::net`), with this module's error type.
fn http_client() -> Result<reqwest::Client, BotError> {
    crate::net::stall_bounded_client().map_err(|e| BotError::Config(format!("HTTP client: {e}")))
}

/// Verify `bytes` hash against an expected hex digest (case-insensitive).
fn verify_sha256(bytes: &[u8], expected_hex: &str) -> bool {
    sha256_hex(bytes).eq_ignore_ascii_case(expected_hex.trim())
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
    // leaves a half-written binary at the destination path. The temp itself is
    // removed on failure: a rename refused because the tool is running (the
    // usual Windows case — updating mid-track) used to strand a full-size
    // .download.tmp in lib/ on every retry.
    let tmp = dest.with_extension("download.tmp");
    let result = fs::File::create(&tmp)
        .map_err(|e| BotError::Config(format!("create {}: {e}", tmp.display())))
        .and_then(|mut f| {
            f.write_all(&bytes)
                .map_err(|e| BotError::Config(format!("write {}: {e}", tmp.display())))
        })
        .and_then(|_| {
            fs::rename(&tmp, dest)
                .map_err(|e| BotError::Config(format!("rename to {}: {e}", dest.display())))
        });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
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

    /// A tools dir as an older version left it: live tools plus the ones this
    /// release no longer uses.
    fn fake_legacy_install(legacy: &Path) {
        std::fs::create_dir_all(legacy).unwrap();
        for name in live_item_names().iter().chain(dead_item_names().iter()) {
            if *name == "yt-dlp-plugins" {
                let plug = legacy.join(name).join("bgutil_ytdlp_pot_provider");
                std::fs::create_dir_all(&plug).unwrap();
                std::fs::write(plug.join("plugin.py"), "py").unwrap();
            } else {
                std::fs::write(legacy.join(name), *name).unwrap();
            }
        }
    }

    #[test]
    fn migration_carries_live_tools_and_deletes_dead_ones() {
        let base = mig_tmp("full");
        let legacy = base.join("lib");
        fake_legacy_install(&legacy);
        let target = base.join("data").join("ttspotify").join("lib");

        assert!(migrate_tools_dir(&legacy, &target));
        for name in live_item_names() {
            assert!(target.join(name).exists(), "missing {name} in target");
            assert!(!legacy.join(name).exists(), "{name} left in legacy");
        }
        // Obsolete tools are removed, not carried into the directory the bot
        // now uses - otherwise every upgrade drags ~100MB of dead weight along.
        for name in dead_item_names() {
            assert!(!target.join(name).exists(), "{name} should not have been moved");
            assert!(!legacy.join(name).exists(), "{name} should have been deleted");
        }
        // Nothing of ours left: the folder itself goes too.
        assert!(!legacy.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migration_recognises_a_dir_holding_only_the_sidecar() {
        // A post-migration install has no .bgutil-version; it must still be
        // recognised as ours or a later migration would refuse to run.
        let base = mig_tmp("newonly");
        let legacy = base.join("lib");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(crate::youtube::sidecar::SCRIPT_NAME), "// x").unwrap();
        let target = base.join("data");

        assert!(migrate_tools_dir(&legacy, &target));
        assert!(target.join(crate::youtube::sidecar::SCRIPT_NAME).is_file());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tools_left_by_the_yt_dlp_versions_are_recognised() {
        let base = mig_tmp("legacy");
        let paths = YoutubeSetupPaths { deno: base.join(deno_name()), lib_dir: base.clone() };
        assert!(!has_legacy_tools(&paths), "an empty folder is not a legacy install");

        std::fs::write(base.join(BGUTIL_VERSION_FILE), "0.8.4").unwrap();
        assert!(has_legacy_tools(&paths));
        assert!(!is_installed(&paths), "a legacy install has no sidecar, so it cannot play");

        std::fs::remove_file(base.join(BGUTIL_VERSION_FILE)).unwrap();
        std::fs::write(base.join(ytdlp_name()), "binary").unwrap();
        assert!(has_legacy_tools(&paths), "yt-dlp alone is enough to tell");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migration_refuses_a_directory_that_is_not_ours() {
        let base = mig_tmp("foreign");
        let legacy = base.join("lib");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("somebody-elses-file"), "x").unwrap();

        assert!(!migrate_tools_dir(&legacy, &base.join("data")));
        assert!(legacy.join("somebody-elses-file").is_file(), "must not touch it");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg(not(windows))]
    fn tools_dir_is_the_data_dir_with_no_probing() {
        // The old code returned the exe-side dir when it spotted yt-dlp there,
        // so removing yt-dlp would have silently relocated every install.
        // Startup migration means there is one answer.
        let paths = resolve_paths().expect("resolve");
        if let Some(data) = dirs::data_dir() {
            assert_eq!(paths.lib_dir, data.join("ttspotify").join("lib"));
        }
    }

    #[test]
    fn resolve_paths_lands_in_lib_subdir() {
        let paths = resolve_paths().expect("resolve_paths");
        assert!(paths.lib_dir.ends_with("lib"));
        assert!(paths.deno.starts_with(&paths.lib_dir));
    }

    #[test]
    fn deno_filename_matches_platform() {
        let paths = resolve_paths().unwrap();
        let name = paths.deno.file_name().unwrap().to_str().unwrap();
        if cfg!(windows) {
            assert_eq!(name, "deno.exe");
        } else {
            assert_eq!(name, "deno");
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

    #[rstest]
    #[case(true, JsRuntime::Bundled(PathBuf::from("lib/deno")), true)]
    // A new enough Deno the user installed themselves is as good as ours. The
    // tray used to count only ours, so with a system Deno it kept offering
    // Install and never enabled Update, although playback worked.
    #[case(true, JsRuntime::OnPath, true)]
    #[case(true, JsRuntime::Missing, false)]
    #[case(false, JsRuntime::Bundled(PathBuf::from("lib/deno")), false)]
    #[case(false, JsRuntime::OnPath, false)]
    fn installed_means_the_script_and_a_usable_runtime(
        #[case] script_present: bool,
        #[case] runtime: JsRuntime,
        #[case] expected: bool,
    ) {
        assert_eq!(tools_installed(script_present, &runtime), expected);
    }

    #[test]
    fn a_runtime_is_asked_its_version_once_per_binary() {
        // The tray checks on the message loop; spawning `deno --version` every
        // time would put a process launch there.
        let memo = VersionMemo::default();
        let calls = std::cell::Cell::new(0);
        let key = |len| RuntimeKey { path: PathBuf::from("deno"), len, modified: None };
        let probe = || {
            calls.set(calls.get() + 1);
            Some((2, 6, 2))
        };
        assert_eq!(memo.get(key(10), probe), Some((2, 6, 2)));
        assert_eq!(memo.get(key(10), probe), Some((2, 6, 2)));
        assert_eq!(calls.get(), 1, "the same binary was asked twice");
        // An upgrade replaces the file, so it must be asked again.
        memo.get(key(11), probe);
        assert_eq!(calls.get(), 2, "a replaced binary kept its old answer");
    }

    #[test]
    fn a_system_deno_is_left_to_its_owner_with_directions() {
        let note = system_deno_update_note(Some((2, 6, 2)));
        assert!(note.contains("2.6.2"), "{note}");
        assert!(note.contains("deno upgrade"), "{note}");
        assert!(system_deno_update_note(None).contains("deno upgrade"));
    }
}
