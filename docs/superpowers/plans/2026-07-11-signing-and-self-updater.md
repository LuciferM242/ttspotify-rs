# Release Signing + Self-Updater Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sign releases with minisign and add an in-app updater that checks GitHub Releases, verifies signature + hash, and replaces the binary — user-triggered, never silent.

**Architecture:** A platform-agnostic `src/update/` core (check / verify / download / apply) drives both the Windows tray GUI (dialogs + progress bar) and the Linux CLI (`--update` + startup log breadcrumb). Signing happens in CI over `SHA256SUMS`; the client verifies that signature against an embedded public key before trusting the hashes. An app-global `settings.json` holds the startup-check toggle; Windows autostart lives in the registry.

**Tech Stack:** Rust, tokio, reqwest (async, rustls), minisign-verify, self-replace, tar+flate2, zip, sha2, semver, winreg (Windows), wxDragon (Windows GUI).

## Global Constraints

- Embedded minisign public key (verbatim): `RWTvwlFryO9VLtB1R7ZmCYzRB2iGuBAHWEmx8dsI8UH4LlLBEG0N61I+`
- GitHub repo slug (verbatim): `LuciferM242/ttspotify-rs`
- Release asset names (verbatim): `tt-spotify-bot-windows-x86_64.zip`, `tt-spotify-bot-linux-x86_64.tar.gz`, `tt-spotify-bot-linux-aarch64.tar.gz`, plus `SHA256SUMS`, `SHA256SUMS.minisig`.
- CI secret name (verbatim): `MINISIGN_SECRET_KEY` (passwordless key — no `MINISIGN_PASSWORD`).
- **verify-before-write invariant:** the binary is never touched unless the SHA256SUMS signature AND the asset hash both pass.
- No emojis in messages or code (CLAUDE.md).
- App is GPL-3-or-later; no new GPL constraints introduced.
- Update failures are always non-fatal — the bot keeps running.
- No background polling: check on startup (if toggle on) + manual only.
- Async throughout the update core; GUI calls it from a worker thread via a dedicated `tokio::runtime::Runtime`.
- Follow existing patterns: atomic write = `tmp` + `rename` (see `config.rs:335`), GUI checkbox = `add_checkbox` helper (`config_dialog.rs:347`), tray menu = `Menu::builder()` / `append_item` (`tray.rs:342`).

---

## File Structure

**Create:**
- `src/update/mod.rs` — public API, `UpdateInfo`, `UpdateError`, embedded `PUBLIC_KEY`, `current_asset_name()`, re-exports.
- `src/update/verify.rs` — pure: sha256 hex, parse SHA256SUMS, look up asset hash, minisign signature verify.
- `src/update/github.rs` — async: fetch `releases/latest`, parse JSON into `UpdateInfo`, semver compare, asset selection.
- `src/update/apply.rs` — async: download SUMS/sig/asset (with progress + cancel), verify, extract, self-replace.
- `src/settings.rs` — `AppSettings`, load-or-default, atomic save.
- `src/gui/autostart.rs` — Windows registry autostart (`is_enabled` / `set_enabled`).
- `src/gui/update_dialog.rs` — "update available" dialog + download progress dialog.
- `src/gui/settings_dialog.rs` — Settings window (2 checkboxes).

**Modify:**
- `Cargo.toml` — new deps.
- `src/lib.rs` — `pub mod update; pub mod settings;`.
- `src/gui/mod.rs` — `mod autostart; mod update_dialog; mod settings_dialog;`.
- `src/main.rs` — `--update` flag + handler; Linux startup breadcrumb.
- `src/bot/runner.rs` — non-blocking startup check that logs the breadcrumb (Linux path).
- `src/gui/tray.rs` — "Check for updates" + "Settings" menu items; startup check trigger.
- `.github/workflows/release.yml` — minisign signing step + publish the `.minisig`.

---

## Task 1: Add dependencies

**Files:**
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: crates `minisign-verify`, `self_replace`, `tar`, `flate2`, `semver`, and Windows-only `winreg`; enables `reqwest` `blocking`-free async streaming (already has `stream`).

- [ ] **Step 1: Add the cross-platform deps**

In `Cargo.toml`, under `[dependencies]` (after the `zip` line ~37), add:

```toml
# Self-updater: verify minisign signature, swap the running binary, decode the
# Linux/arm .tar.gz release archives, and compare semver versions.
minisign-verify = "0.2"
self_replace = "1"
tar = "0.4"
flate2 = "1"
semver = "1"
```

- [ ] **Step 2: Add the Windows-only dep**

In `Cargo.toml`, under `[target.'cfg(windows)'.dependencies]` (next to `wxdragon`), add:

```toml
winreg = "0.52"
```

- [ ] **Step 3: Verify it resolves**

Run: `cargo fetch`
Expected: resolves and downloads the new crates, no version conflicts.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "Add self-updater dependencies"
```

---

## Task 2: Pure verify helpers (sha256 + SHA256SUMS parsing)

**Files:**
- Create: `src/update/mod.rs` (module skeleton + `UpdateError`)
- Create: `src/update/verify.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub enum UpdateError` (variants: `Http(String)`, `Parse(String)`, `Signature`, `Hash`, `Extract(String)`, `Io(String)`, `Cancelled`) with `Display`.
  - `pub fn sha256_hex(bytes: &[u8]) -> String`
  - `pub fn expected_hash<'a>(sums: &'a str, asset: &str) -> Option<&'a str>`

- [ ] **Step 1: Declare the module**

In `src/lib.rs`, after `pub mod track;` (line 14), add:

```rust
pub mod update;
```

In a new file `src/update/mod.rs`:

```rust
//! Self-updater: check GitHub Releases, verify a minisign signature over
//! SHA256SUMS, then verify + apply the matching asset.

mod verify;

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
```

- [ ] **Step 2: Write the failing test for `verify.rs`**

In a new file `src/update/verify.rs`:

```rust
use super::UpdateError;
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Look up the expected hex hash for `asset` in a SHA256SUMS body.
/// Lines look like: `<hex>  <filename>` (two spaces, `sha256sum` format).
pub fn expected_hash<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (hash, name) = line.split_once("  ")?;
        if name.trim() == asset {
            return Some(hash.trim());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_empty_is_known() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_of_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn expected_hash_finds_asset() {
        let sums = "aaaa  tt-spotify-bot-linux-x86_64.tar.gz\nbbbb  tt-spotify-bot-windows-x86_64.zip\n";
        assert_eq!(expected_hash(sums, "tt-spotify-bot-windows-x86_64.zip"), Some("bbbb"));
    }

    #[test]
    fn expected_hash_missing_asset_is_none() {
        let sums = "aaaa  other.tar.gz\n";
        assert_eq!(expected_hash(sums, "tt-spotify-bot-windows-x86_64.zip"), None);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test --lib update::verify`
Expected: 4 tests PASS. (`UpdateError` import is used by the signature fn added in Task 3; if the unused-import lint fires now, add `#[allow(unused_imports)]` on the `use super::UpdateError;` line with a comment "used by verify_signature in the next task" — removed in Task 3.)

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/update/mod.rs src/update/verify.rs
git commit -m "Add update-verify hash helpers + UpdateError"
```

---

## Task 3: minisign signature verification

**Files:**
- Modify: `src/update/verify.rs`

**Interfaces:**
- Produces: `pub fn verify_signature(signed_data: &[u8], sig_body: &str) -> Result<(), UpdateError>` — verifies `sig_body` (the full `.minisig` file contents) over `signed_data` using the embedded `PUBLIC_KEY`. `Ok(())` on success, `Err(UpdateError::Signature)` otherwise.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/update/verify.rs`:

```rust
    // A real minisign signature fixture, generated with the project key over the
    // bytes "hello\n". Regenerate with:
    //   printf 'hello\n' > /tmp/m && minisign -S -s minisign.key -m /tmp/m
    //   cat /tmp/m.minisig
    const SIG_HELLO: &str = "<PASTE .minisig CONTENTS FOR b\"hello\\n\">";

    #[test]
    fn valid_signature_passes() {
        assert!(verify_signature(b"hello\n", SIG_HELLO).is_ok());
    }

    #[test]
    fn tampered_data_fails() {
        assert!(matches!(
            verify_signature(b"HELLO\n", SIG_HELLO),
            Err(UpdateError::Signature)
        ));
    }
```

Before running, generate the fixture (one-time, using the repo-local key):

```bash
printf 'hello\n' > /tmp/m
"/c/Users/aloys/AppData/Local/Microsoft/WinGet/Packages/jedisct1.minisign_Microsoft.Winget.Source_8wekyb3d8bbwe/minisign-win64/x86_64/minisign.exe" -S -s minisign.key -m /tmp/m
cat /tmp/m.minisig
```

Paste the full output (all lines) into `SIG_HELLO` as a raw string. Use a Rust raw string with the comment line included, e.g. `const SIG_HELLO: &str = "untrusted comment: ...\n<base64sig>\ntrusted comment: ...\n<base64globalsig>\n";` — include the trailing newline.

- [ ] **Step 2: Write the implementation**

Add to `src/update/verify.rs` (top-level, above `tests`):

```rust
use super::PUBLIC_KEY;
use minisign_verify::{PublicKey, Signature};

/// Verify a minisign signature (`.minisig` file contents) over `signed_data`
/// using the embedded public key.
pub fn verify_signature(signed_data: &[u8], sig_body: &str) -> Result<(), UpdateError> {
    let pk = PublicKey::from_base64(PUBLIC_KEY).map_err(|_| UpdateError::Signature)?;
    let sig = Signature::decode(sig_body).map_err(|_| UpdateError::Signature)?;
    pk.verify(signed_data, &sig, false)
        .map_err(|_| UpdateError::Signature)
}
```

Remove any temporary `#[allow(unused_imports)]` on `use super::UpdateError;` from Task 2.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test --lib update::verify`
Expected: all PASS (2 new signature tests + the 4 from Task 2).

- [ ] **Step 4: Commit**

```bash
git add src/update/verify.rs
git commit -m "Add minisign signature verification"
```

---

## Task 4: Asset selection + version comparison (GitHub core, offline parts)

**Files:**
- Create: `src/update/github.rs`
- Modify: `src/update/mod.rs`

**Interfaces:**
- Produces:
  - `pub struct UpdateInfo { pub version: semver::Version, pub tag: String, pub changelog: String, pub asset_url: String, pub sums_url: String, pub sig_url: String }`
  - `pub fn current_asset_name() -> &'static str`
  - `pub fn newer_than_current(tag: &str) -> Option<semver::Version>` — parses `tag` (strips leading `v`), returns `Some(version)` if strictly greater than `CARGO_PKG_VERSION`, else `None`.
  - `fn select_from_release(json: &serde_json::Value) -> Result<Option<UpdateInfo>, UpdateError>` (crate-private; tested).

- [ ] **Step 1: Wire the module + exports**

In `src/update/mod.rs`, add near the top (after `mod verify;`):

```rust
mod github;

pub use github::{check, current_asset_name, newer_than_current, UpdateInfo};
```

(`check` is added in Task 5; add it to the `pub use` now and stub it in Step 2 so the module compiles.)

- [ ] **Step 2: Write the failing tests**

In a new file `src/update/github.rs`:

```rust
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
```

Add a temporary stub so `pub use github::check` in `mod.rs` compiles:

```rust
// Removed in Task 5 (replaced by the real async check()).
pub fn check() {}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test --lib update::github`
Expected: 7 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/update/mod.rs src/update/github.rs
git commit -m "Add release asset selection + version comparison"
```

---

## Task 5: Async `check()` against GitHub

**Files:**
- Modify: `src/update/github.rs`

**Interfaces:**
- Consumes: `select_from_release`, `REPO`.
- Produces: `pub async fn check() -> Result<Option<UpdateInfo>, UpdateError>` — GETs `releases/latest`, returns `select_from_release`'s result.

- [ ] **Step 1: Replace the stub with the real implementation**

In `src/update/github.rs`, remove the `pub fn check() {}` stub and add:

```rust
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
```

Note: the GitHub API requires a `User-Agent` header — omitting it returns 403. This is why the `.user_agent(...)` line is mandatory.

- [ ] **Step 2: Verify it compiles + existing tests still pass**

Run: `cargo test --lib update::`
Expected: all `update::verify` and `update::github` tests PASS; no compile errors. (`check()` itself is exercised in manual smoke, not a unit test — it hits the network.)

- [ ] **Step 3: Commit**

```bash
git add src/update/github.rs
git commit -m "Add async GitHub releases check"
```

---

## Task 6: Download + verify + apply

**Files:**
- Create: `src/update/apply.rs`
- Modify: `src/update/mod.rs`

**Interfaces:**
- Consumes: `UpdateInfo`, `verify::{verify_signature, expected_hash, sha256_hex}`, `UpdateError`, `current_asset_name`.
- Produces: `pub async fn download_and_apply(info: &UpdateInfo, progress: &(dyn Fn(u64, Option<u64>) + Sync), cancel: &std::sync::atomic::AtomicBool) -> Result<(), UpdateError>`
  - `progress(downloaded_bytes, total_bytes_opt)` is called as the asset downloads.
  - `cancel` is polled between chunks; if set, returns `Err(UpdateError::Cancelled)` with nothing written.

- [ ] **Step 1: Wire the module + export**

In `src/update/mod.rs`, add (after `mod github;`):

```rust
mod apply;

pub use apply::download_and_apply;
```

- [ ] **Step 2: Write the extraction helper + its failing tests**

In a new file `src/update/apply.rs`:

```rust
use super::verify::{expected_hash, sha256_hex, verify_signature};
use super::{current_asset_name, UpdateError, UpdateInfo};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

/// Extract the bot binary from a downloaded release archive held in memory.
/// Windows assets are `.zip`; Linux/arm are `.tar.gz`. Returns the raw binary
/// bytes. `binary_name` is the file to pull out of the archive.
fn extract_binary(archive: &[u8], is_zip: bool, binary_name: &str) -> Result<Vec<u8>, UpdateError> {
    if is_zip {
        let reader = std::io::Cursor::new(archive);
        let mut zip = zip::ZipArchive::new(reader).map_err(|e| UpdateError::Extract(e.to_string()))?;
        let mut file = zip
            .by_name(binary_name)
            .map_err(|e| UpdateError::Extract(format!("{binary_name}: {e}")))?;
        let mut out = Vec::new();
        file.read_to_end(&mut out).map_err(|e| UpdateError::Extract(e.to_string()))?;
        Ok(out)
    } else {
        let gz = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries().map_err(|e| UpdateError::Extract(e.to_string()))? {
            let mut entry = entry.map_err(|e| UpdateError::Extract(e.to_string()))?;
            let path = entry.path().map_err(|e| UpdateError::Extract(e.to_string()))?;
            if path.file_name().and_then(|n| n.to_str()) == Some(binary_name) {
                let mut out = Vec::new();
                entry.read_to_end(&mut out).map_err(|e| UpdateError::Extract(e.to_string()))?;
                return Ok(out);
            }
        }
        Err(UpdateError::Extract(format!("{binary_name} not found in archive")))
    }
}

/// The binary name inside the archive (no directory prefix — CI archives the
/// bare binary from the working dir).
fn binary_name() -> &'static str {
    if cfg!(windows) {
        "tt-spotify-bot.exe"
    } else {
        "tt-spotify-bot"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extract_from_tar_gz() {
        // Build a tar.gz in memory containing "tt-spotify-bot" -> b"BINARY".
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let data = b"BINARY";
            let mut header = tar::Header::new_gnu();
            header.set_path("tt-spotify-bot").unwrap();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, &data[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }
        let out = extract_binary(&gz, false, "tt-spotify-bot").unwrap();
        assert_eq!(out, b"BINARY");
    }

    #[test]
    fn extract_from_zip() {
        let mut buf = Vec::new();
        {
            let w = std::io::Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(w);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("tt-spotify-bot.exe", opts).unwrap();
            zip.write_all(b"WINBIN").unwrap();
            zip.finish().unwrap();
        }
        let out = extract_binary(&buf, true, "tt-spotify-bot.exe").unwrap();
        assert_eq!(out, b"WINBIN");
    }

    #[test]
    fn extract_missing_binary_errors() {
        let mut buf = Vec::new();
        {
            let w = std::io::Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(w);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("something-else", opts).unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        assert!(matches!(extract_binary(&buf, true, "tt-spotify-bot.exe"), Err(UpdateError::Extract(_))));
    }
}
```

- [ ] **Step 3: Run the extraction tests**

Run: `cargo test --lib update::apply`
Expected: 3 tests PASS. (You may need `zip`'s `write` feature — verify `zip` in `Cargo.toml` includes writing; it does by default with `deflate`. If `ZipWriter` is missing, add `"deflate"` is already present, so no change.)

- [ ] **Step 4: Add the async download+apply implementation**

Append to `src/update/apply.rs` (above `#[cfg(test)]`):

```rust
async fn get_bytes(
    client: &reqwest::Client,
    url: &str,
    progress: Option<&(dyn Fn(u64, Option<u64>) + Sync)>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, UpdateError> {
    use futures_util::StreamExt;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Http(format!("HTTP {}", resp.status())));
    }
    let total = resp.content_length();
    let mut out = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            return Err(UpdateError::Cancelled);
        }
        let chunk = chunk.map_err(|e| UpdateError::Http(e.to_string()))?;
        out.extend_from_slice(&chunk);
        if let Some(cb) = progress {
            cb(out.len() as u64, total);
        }
    }
    Ok(out)
}

/// Download SHA256SUMS + its signature + the platform asset, verify the
/// signature and hash, extract the binary, and replace the running executable.
/// Nothing is written to the binary unless BOTH signature and hash pass.
pub async fn download_and_apply(
    info: &UpdateInfo,
    progress: &(dyn Fn(u64, Option<u64>) + Sync),
    cancel: &AtomicBool,
) -> Result<(), UpdateError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(concat!("ttspotify-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| UpdateError::Http(e.to_string()))?;

    // 1. SHA256SUMS + signature (small; no progress).
    let sums = get_bytes(&client, &info.sums_url, None, cancel).await?;
    let sig = get_bytes(&client, &info.sig_url, None, cancel).await?;
    let sig_str = String::from_utf8(sig).map_err(|_| UpdateError::Signature)?;

    // 2. Verify signature over the SUMS bytes. Abort before touching anything.
    verify_signature(&sums, &sig_str)?;

    // 3. Download the asset with progress.
    let asset = get_bytes(&client, &info.asset_url, Some(progress), cancel).await?;

    // 4. Hash-check the asset against the (now-trusted) SUMS.
    let sums_text = String::from_utf8_lossy(&sums);
    let want = expected_hash(&sums_text, current_asset_name()).ok_or(UpdateError::Hash)?;
    if sha256_hex(&asset) != want {
        return Err(UpdateError::Hash);
    }

    // 5. Extract the binary.
    let is_zip = cfg!(windows);
    let bin = extract_binary(&asset, is_zip, binary_name())?;

    // 6. Write to a temp file next to the current exe, then self-replace.
    let exe = std::env::current_exe().map_err(|e| UpdateError::Io(e.to_string()))?;
    let dir = exe.parent().ok_or_else(|| UpdateError::Io("no exe dir".into()))?;
    let tmp = dir.join("tt-spotify-bot.update.tmp");
    std::fs::write(&tmp, &bin).map_err(|e| UpdateError::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    self_replace::self_replace(&tmp).map_err(|e| UpdateError::Io(e.to_string()))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}
```

- [ ] **Step 5: Add the `futures-util` dep (needed for `bytes_stream().next()`)**

Check whether `futures-util` is already available (reqwest pulls it, but not necessarily as a direct dep). Add to `Cargo.toml` `[dependencies]`:

```toml
futures-util = { version = "0.3", default-features = false }
```

- [ ] **Step 6: Verify compile + all update tests pass**

Run: `cargo test --lib update::`
Expected: all PASS (verify + github + apply extraction tests). `download_and_apply` is covered by manual smoke.

- [ ] **Step 7: Commit**

```bash
git add src/update/mod.rs src/update/apply.rs Cargo.toml Cargo.lock
git commit -m "Add update download, verify, and self-replace"
```

---

## Task 7: App-global settings

**Files:**
- Create: `src/settings.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct AppSettings { pub check_updates_on_startup: bool }` (serde, default `true`).
  - `pub fn settings_path() -> std::path::PathBuf` — `config_dir().join("settings.json")`.
  - `pub fn load() -> AppSettings` — load-or-default (missing/corrupt file returns default).
  - `impl AppSettings { pub fn save(&self) -> Result<(), crate::error::BotError> }` — atomic write.

- [ ] **Step 1: Declare the module**

In `src/lib.rs`, after `pub mod service;` (or near other top-level modules), add:

```rust
pub mod settings;
```

- [ ] **Step 2: Write the failing tests**

In a new file `src/settings.rs`:

```rust
//! App-global preferences shared across all bot instances (they run one shared
//! binary, so these are not per-bot config fields). Stored as settings.json in
//! the platform config dir. "Launch on startup" is NOT here — on Windows that
//! lives in the registry (see gui::autostart).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::config_dir;
use crate::error::BotError;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_true", rename = "checkUpdatesOnStartup")]
    pub check_updates_on_startup: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { check_updates_on_startup: true }
    }
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// Load settings, falling back to defaults if the file is missing or unreadable.
pub fn load() -> AppSettings {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => AppSettings::default(),
    }
}

impl AppSettings {
    /// Persist atomically (tmp + rename), matching config.rs's write pattern.
    pub fn save(&self) -> Result<(), BotError> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BotError::Config(format!("Failed to serialize settings: {e}")))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_update_check_on() {
        assert!(AppSettings::default().check_updates_on_startup);
    }

    #[test]
    fn deserialize_missing_field_defaults_on() {
        let s: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(s.check_updates_on_startup);
    }

    #[test]
    fn round_trips_false() {
        let s = AppSettings { check_updates_on_startup: false };
        let json = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(!back.check_updates_on_startup);
    }

    #[test]
    fn serializes_with_camelcase_key() {
        let json = serde_json::to_string(&AppSettings::default()).unwrap();
        assert!(json.contains("checkUpdatesOnStartup"));
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib settings::`
Expected: 4 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/settings.rs
git commit -m "Add app-global settings (update-check toggle)"
```

---

## Task 8: CLI `--update` + Linux startup breadcrumb

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `update::{check, download_and_apply}`, `settings`.
- Produces: `--update` CLI flag behavior; a startup check that logs `Update vX available - run: ttspotify --update`.

- [ ] **Step 1: Add the CLI flag**

In `src/main.rs`, in the `Args` struct (after `update_tools` at line 70), add:

```rust
    /// Check GitHub for a newer release; if found, show the changelog and
    /// (with confirmation) download, verify, and replace this binary.
    #[arg(long)]
    update: bool,
```

- [ ] **Step 2: Handle `--update` in the non-Windows main**

In `src/main.rs`, in the `#[cfg(not(windows))] async fn main`, after the `args.update_tools` block (line 125), add:

```rust
    if args.update {
        return run_cli_update().await;
    }
```

Then add this function below `main` (still `#[cfg(not(windows))]`):

```rust
#[cfg(not(windows))]
async fn run_cli_update() -> Result<(), BotError> {
    use std::io::{IsTerminal, Write};
    use std::sync::atomic::AtomicBool;

    let info = match tt_spotify_bot::update::check().await {
        Ok(Some(info)) => info,
        Ok(None) => {
            println!("Already up to date (v{}).", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(e) => {
            eprintln!("Update check failed: {e}");
            std::process::exit(1);
        }
    };

    println!("Update available: {} (you have v{})", info.tag, env!("CARGO_PKG_VERSION"));
    println!("\n{}\n", info.changelog.trim());

    if !std::io::stdin().is_terminal() {
        eprintln!("Not a terminal; refusing to update non-interactively. Run `ttspotify --update` from a shell.");
        std::process::exit(1);
    }

    print!("Download and install {}? [y/N] ", info.tag);
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok();
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        println!("Cancelled.");
        return Ok(());
    }

    let cancel = AtomicBool::new(false);
    let progress = |done: u64, total: Option<u64>| {
        match total {
            Some(t) if t > 0 => print!("\rDownloading... {}%   ", done * 100 / t),
            _ => print!("\rDownloading... {done} bytes   "),
        }
        let _ = std::io::stdout().flush();
    };
    match tt_spotify_bot::update::download_and_apply(&info, &progress, &cancel).await {
        Ok(()) => {
            println!("\nUpdated to {}.", info.tag);
            println!("If running as a service, restart it: systemctl --user restart ttspotify@<name>");
            Ok(())
        }
        Err(e) => {
            eprintln!("\nUpdate failed: {e}");
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 3: Verify compile + manual check**

Run: `cargo build --release`
Expected: builds. Manual: `./target/release/tt-spotify-bot --update` prints "Already up to date (v0.3.0)." (until a newer release exists).

- [ ] **Step 4: Add the non-blocking startup breadcrumb**

In `src/bot/runner.rs`, find where `run_bot` begins its main work after connecting (near where it logs a successful connect). Add, guarded so it only runs on non-Windows and respects the toggle:

```rust
    // One-shot, non-blocking update check. Logs a breadcrumb if a newer release
    // exists; never blocks startup and never self-updates a running service.
    #[cfg(not(windows))]
    if crate::settings::load().check_updates_on_startup {
        tokio::spawn(async {
            if let Ok(Some(info)) = crate::update::check().await {
                tracing::info!("Update {} available - run: ttspotify --update", info.tag);
            }
        });
    }
```

Place it after the bot has connected and logging is active (so the line lands in the log). Exact insertion point: immediately after the existing "connected" info log in the connect path of `run_bot`. If unsure, locate it with:

```bash
grep -n "Connected\|connected\|info!(" src/bot/runner.rs | head
```

Pick the first log line emitted once per successful startup and insert the block right after it.

- [ ] **Step 5: Verify + commit**

Run: `cargo build --release && cargo test --lib`
Expected: builds, all tests pass.

```bash
git add src/main.rs src/bot/runner.rs
git commit -m "Add CLI --update and Linux startup update breadcrumb"
```

---

## Task 9: Windows autostart registry helper

**Files:**
- Create: `src/gui/autostart.rs`
- Modify: `src/gui/mod.rs`

**Interfaces:**
- Produces (Windows-only):
  - `pub fn is_enabled() -> bool`
  - `pub fn set_enabled(enabled: bool) -> std::io::Result<()>`

- [ ] **Step 1: Declare the module**

In `src/gui/mod.rs`, add with the other `mod` lines:

```rust
mod autostart;
```

- [ ] **Step 2: Write the implementation (with tests)**

In a new file `src/gui/autostart.rs`:

```rust
//! Windows per-user autostart via the HKCU Run key. We touch exactly one value
//! (VALUE_NAME) under one key — never the key itself, never HKLM, never other
//! values — so we can't orphan entries or need admin. Shows up in Task Manager
//! and Settings > Apps > Startup under VALUE_NAME.

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "ttspotify-rs";

fn quoted_exe_path() -> std::io::Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("\"{}\"", exe.display()))
}

/// True if our autostart value exists under HKCU Run.
pub fn is_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) {
        Ok(key) => key.get_value::<String, _>(VALUE_NAME).is_ok(),
        Err(_) => false,
    }
}

/// Create (on) or delete (off) our single autostart value. On writes/overwrites
/// with the current exe path (self-heals a stale path); off deletes only our
/// value, leaving the key and any other values untouched.
pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)?;
    if enabled {
        key.set_value(VALUE_NAME, &quoted_exe_path()?)
    } else {
        match key.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Round-trips through the real (per-user) registry. Safe: uses our own value
    // name, cleans up after itself, requires no admin. Restores prior state.
    #[test]
    fn toggle_round_trip() {
        let prior = is_enabled();

        set_enabled(true).unwrap();
        assert!(is_enabled());

        set_enabled(false).unwrap();
        assert!(!is_enabled());

        // idempotent off
        set_enabled(false).unwrap();
        assert!(!is_enabled());

        // restore prior state
        set_enabled(prior).unwrap();
    }
}
```

- [ ] **Step 3: Run the test (Windows only)**

Run: `cargo test --lib gui::autostart`
Expected: 1 test PASS. (This test only compiles/runs on Windows; the CI Linux job skips `gui`.)

- [ ] **Step 4: Commit**

```bash
git add src/gui/mod.rs src/gui/autostart.rs
git commit -m "Add Windows autostart registry helper"
```

---

## Task 10: Windows update dialog + progress dialog

**Files:**
- Create: `src/gui/update_dialog.rs`
- Modify: `src/gui/mod.rs`

**Interfaces:**
- Consumes: `update::{UpdateInfo, download_and_apply}`.
- Produces:
  - `pub fn show_update_available(parent: &Frame, info: UpdateInfo)` — modal dialog with a read-only changelog field + Download / Later. Download opens the progress dialog.
  - Internally: a progress dialog with a `Gauge` + Cancel that runs `download_and_apply` on a worker thread (own `tokio::runtime::Runtime`), then relaunches on success.

- [ ] **Step 1: Declare the module**

In `src/gui/mod.rs`, add:

```rust
mod update_dialog;
pub use update_dialog::show_update_available;
```

- [ ] **Step 2: Implement the dialogs**

In a new file `src/gui/update_dialog.rs`. This follows the `config_dialog.rs` Frame/Panel/Button pattern and `progress.rs` for the worker-thread structure. GUI code is verified by manual smoke, not unit tests.

```rust
//! "Update available" dialog + download progress dialog (Windows tray).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wxdragon::prelude::*;

use crate::update::{download_and_apply, UpdateInfo};

/// Modal "update available" dialog. Read-only changelog + Download / Later.
pub fn show_update_available(parent: &Frame, info: UpdateInfo) {
    let frame = Frame::builder()
        .with_title(&format!("Update available - {}", info.tag))
        .with_size(Size::new(520, 420))
        .build();
    let panel = Panel::builder(&frame).build();

    let heading = StaticText::builder(&panel)
        .with_label(&format!(
            "A new version ({}) is available. You have v{}.",
            info.tag,
            env!("CARGO_PKG_VERSION")
        ))
        .build();

    let notes = TextCtrl::builder(&panel)
        .with_value(info.changelog.trim())
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .build();

    let download_btn = Button::builder(&panel).with_label("Download").build();
    let later_btn = Button::builder(&panel).with_label("Later").build();

    let btn_row = BoxSizer::builder(Orientation::Horizontal).build();
    btn_row.add(&download_btn, 0, SizerFlag::All, 5);
    btn_row.add(&later_btn, 0, SizerFlag::All, 5);

    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&heading, 0, SizerFlag::All, 8);
    sizer.add(&notes, 1, SizerFlag::Expand | SizerFlag::All, 8);
    sizer.add_sizer(&btn_row, 0, SizerFlag::AlignRight | SizerFlag::All, 4);
    panel.set_sizer(sizer, true);

    let frame_for_later = frame.clone();
    later_btn.on_click(move |_| {
        frame_for_later.close(true);
    });

    let frame_for_dl = frame.clone();
    download_btn.on_click(move |_| {
        frame_for_dl.close(true);
        run_download(info.clone());
    });

    frame.centre();
    frame.show(true);
}

/// Modal progress dialog: a gauge + Cancel. Runs download_and_apply on a worker
/// thread with its own tokio runtime. On success, relaunches the new exe.
fn run_download(info: UpdateInfo) {
    let frame = Frame::builder()
        .with_title("Downloading update")
        .with_size(Size::new(420, 140))
        .build();
    let panel = Panel::builder(&frame).build();

    let label = StaticText::builder(&panel).with_label("Downloading...").build();
    let gauge = Gauge::builder(&panel).with_range(100).build();
    let cancel_btn = Button::builder(&panel).with_label("Cancel").build();

    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&label, 0, SizerFlag::All, 8);
    sizer.add(&gauge, 0, SizerFlag::Expand | SizerFlag::All, 8);
    sizer.add(&cancel_btn, 0, SizerFlag::AlignRight | SizerFlag::All, 4);
    panel.set_sizer(sizer, true);

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_btn = cancel.clone();
    cancel_btn.on_click(move |_| cancel_for_btn.store(true, Ordering::Relaxed));

    // Progress updates cross threads via a wx idle-safe callback. Use a shared
    // atomic percent the UI polls on a short timer (wxDragon Timer), OR marshal
    // via CallAfter if available. Implement with a Timer that reads an
    // Arc<AtomicU64> percent updated by the worker's progress closure.
    let percent = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let result: Arc<parking_lot::Mutex<Option<Result<(), String>>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // Worker thread.
    {
        let percent = percent.clone();
        let done = done.clone();
        let result = result.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let progress = |d: u64, total: Option<u64>| {
                if let Some(t) = total {
                    if t > 0 {
                        percent.store(d * 100 / t, Ordering::Relaxed);
                    }
                }
            };
            let r = rt.block_on(download_and_apply(&info, &progress, &cancel));
            *result.lock() = Some(r.map_err(|e| e.to_string()));
            done.store(true, Ordering::Relaxed);
        });
    }

    // Poll timer: update gauge; when done, show result and (on success) relaunch.
    let timer = Timer::new(&frame);
    let frame_for_timer = frame.clone();
    let gauge_for_timer = gauge.clone();
    timer.on_tick(move |_| {
        gauge_for_timer.set_value(percent.load(Ordering::Relaxed) as i32);
        if done.load(Ordering::Relaxed) {
            let r = result.lock().take();
            frame_for_timer.close(true);
            match r {
                Some(Ok(())) => relaunch_and_exit(),
                Some(Err(e)) => {
                    MessageDialog::builder(&frame_for_timer, &e, "Update failed")
                        .with_style(MessageDialogStyle::Ok | MessageDialogStyle::IconError)
                        .build()
                        .show_modal();
                }
                None => {}
            }
        }
    });
    timer.start(100);

    frame.centre();
    frame.show(true);
}

/// Relaunch the (now-replaced) exe and exit the current process.
fn relaunch_and_exit() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}
```

Note on wxDragon API names: `Gauge`, `Timer`, `TextCtrlStyle::ReadOnly`, `MessageDialog` all exist in the 0.9.14 API this project pins. If a specific symbol differs, check `config_dialog.rs` / `progress.rs` for the exact import path (they already use `MessageDialog`, `TextCtrl`, `Button`, `Panel`, `Frame`, sizers). Adjust import/style spellings to match those files — do not change the logic.

- [ ] **Step 3: Build (Windows)**

Run: `cargo build --release`
Expected: compiles on Windows. Fix any wxDragon symbol/style name mismatches by aligning with `config_dialog.rs` usage (logic unchanged).

- [ ] **Step 4: Commit**

```bash
git add src/gui/mod.rs src/gui/update_dialog.rs
git commit -m "Add Windows update + download-progress dialogs"
```

---

## Task 11: Windows Settings dialog

**Files:**
- Create: `src/gui/settings_dialog.rs`
- Modify: `src/gui/mod.rs`

**Interfaces:**
- Consumes: `settings::{load, AppSettings}`, `gui::autostart`.
- Produces: `pub fn open_settings_dialog()` — a window with two checkboxes (update-check + launch-on-startup) and Save / Cancel.

- [ ] **Step 1: Declare the module**

In `src/gui/mod.rs`:

```rust
mod settings_dialog;
pub use settings_dialog::open_settings_dialog;
```

- [ ] **Step 2: Implement**

In a new file `src/gui/settings_dialog.rs` (mirrors `config_dialog.rs` structure):

```rust
//! App-global Settings window (Windows tray). Two checkboxes:
//!   1. Check for updates on startup  -> settings.json
//!   2. Launch on Windows startup     -> HKCU Run registry (gui::autostart)

use wxdragon::prelude::*;

use super::autostart;
use crate::settings::{self, AppSettings};

pub fn open_settings_dialog() {
    let current = settings::load();
    let autostart_on = autostart::is_enabled();

    let frame = Frame::builder()
        .with_title("Settings")
        .with_size(Size::new(400, 200))
        .build();
    let panel = Panel::builder(&frame).build();

    let update_cb = CheckBox::builder(&panel)
        .with_label("Check for updates on startup")
        .with_value(current.check_updates_on_startup)
        .build();
    let autostart_cb = CheckBox::builder(&panel)
        .with_label("Launch on Windows startup")
        .with_value(autostart_on)
        .build();

    let save_btn = Button::builder(&panel).with_label("Save").build();
    let cancel_btn = Button::builder(&panel).with_label("Cancel").build();

    let btn_row = BoxSizer::builder(Orientation::Horizontal).build();
    btn_row.add(&save_btn, 0, SizerFlag::All, 5);
    btn_row.add(&cancel_btn, 0, SizerFlag::All, 5);

    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&update_cb, 0, SizerFlag::All, 10);
    sizer.add(&autostart_cb, 0, SizerFlag::All, 10);
    sizer.add_sizer(&btn_row, 0, SizerFlag::AlignRight | SizerFlag::All, 6);
    panel.set_sizer(sizer, true);

    let frame_for_cancel = frame.clone();
    cancel_btn.on_click(move |_| frame_for_cancel.close(true));

    let frame_for_save = frame.clone();
    let update_cb_s = update_cb.clone();
    let autostart_cb_s = autostart_cb.clone();
    save_btn.on_click(move |_| {
        let new = AppSettings { check_updates_on_startup: update_cb_s.get_value() };
        if let Err(e) = new.save() {
            MessageDialog::builder(&frame_for_save, &format!("Failed to save settings: {e}"), "Error")
                .with_style(MessageDialogStyle::Ok | MessageDialogStyle::IconError)
                .build()
                .show_modal();
            return;
        }
        if let Err(e) = autostart::set_enabled(autostart_cb_s.get_value()) {
            MessageDialog::builder(&frame_for_save, &format!("Failed to update autostart: {e}"), "Error")
                .with_style(MessageDialogStyle::Ok | MessageDialogStyle::IconError)
                .build()
                .show_modal();
            return;
        }
        frame_for_save.close(true);
    });

    frame.centre();
    frame.show(true);
}
```

- [ ] **Step 3: Build (Windows)**

Run: `cargo build --release`
Expected: compiles. Align any `CheckBox::get_value` / builder spellings with `config_dialog.rs:347` (`add_checkbox`) if they differ.

- [ ] **Step 4: Commit**

```bash
git add src/gui/mod.rs src/gui/settings_dialog.rs
git commit -m "Add Windows Settings dialog"
```

---

## Task 12: Wire tray menu + startup check

**Files:**
- Modify: `src/gui/tray.rs`

**Interfaces:**
- Consumes: `update::check`, `settings::load`, `gui::update_dialog::show_update_available`, `gui::settings_dialog::open_settings_dialog`.
- Produces: two new tray menu items and a one-shot startup check on the Windows side.

- [ ] **Step 1: Add menu item IDs**

In `src/gui/tray.rs`, near the other `const ID_*` definitions (e.g. `ID_ADD_SERVER`, `ID_EXIT`), add:

```rust
const ID_CHECK_UPDATES: i32 = /* next free id, e.g. */ 9101;
const ID_SETTINGS: i32 = 9102;
```

Pick ids that don't collide with existing ones (check the file's existing `ID_*` values and per-bot `base_id` ranges).

- [ ] **Step 2: Add the menu items**

In `build_menu` (`tray.rs:342`), before the final `Exit` item (line ~400), add:

```rust
    menu.append(ID_CHECK_UPDATES, "Check for updates", "", ItemKind::Normal);
    menu.append(ID_SETTINGS, "Settings", "", ItemKind::Normal);
    menu.append_separator();
```

- [ ] **Step 3: Handle the new menu ids**

In the `on_selected` handler where existing ids are matched (the handler bound on the menu around line 108-125), add arms:

```rust
        id if id == ID_CHECK_UPDATES => {
            // Manual check ignores the startup toggle. Runs on a worker thread.
            check_for_updates_manual(&taskbar_frame_handle);
        }
        id if id == ID_SETTINGS => {
            crate::gui::settings_dialog::open_settings_dialog();
        }
```

(Use whatever frame/parent handle the existing handler already has access to for dialog parenting; the other menu handlers show the pattern.)

- [ ] **Step 4: Add the check helpers**

Add to `tray.rs` (module-level fns). `check_for_updates_manual` shows a "you're up to date" box when current; the startup variant is silent when current:

```rust
/// Manual "Check for updates": worker thread checks GitHub; on the result,
/// shows the update dialog (newer) or an info box (up to date / error).
fn check_for_updates_manual(parent: &Frame) {
    spawn_update_check(parent.clone(), /* announce_up_to_date = */ true);
}

/// Startup check (Windows). Silent if already current; respects the toggle.
fn check_for_updates_on_startup(parent: &Frame) {
    if crate::settings::load().check_updates_on_startup {
        spawn_update_check(parent.clone(), /* announce_up_to_date = */ false);
    }
}

fn spawn_update_check(parent: Frame, announce_up_to_date: bool) {
    let done: std::sync::Arc<parking_lot::Mutex<Option<Result<Option<crate::update::UpdateInfo>, String>>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(None));
    {
        let done = done.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let r = rt.block_on(crate::update::check()).map_err(|e| e.to_string());
            *done.lock() = Some(r);
        });
    }
    // Poll on a short timer; when the worker finishes, act on the GUI thread.
    let timer = Timer::new(&parent);
    let parent_for_timer = parent.clone();
    timer.on_tick(move |t| {
        let r = { done.lock().take() };
        let Some(r) = r else { return };
        t.stop();
        match r {
            Ok(Some(info)) => crate::gui::update_dialog::show_update_available(&parent_for_timer, info),
            Ok(None) => {
                if announce_up_to_date {
                    MessageDialog::builder(&parent_for_timer,
                        &format!("You're up to date (v{}).", env!("CARGO_PKG_VERSION")),
                        "Check for updates")
                        .with_style(MessageDialogStyle::Ok | MessageDialogStyle::IconInformation)
                        .build()
                        .show_modal();
                }
            }
            Err(e) => {
                if announce_up_to_date {
                    MessageDialog::builder(&parent_for_timer, &e, "Check for updates")
                        .with_style(MessageDialogStyle::Ok | MessageDialogStyle::IconError)
                        .build()
                        .show_modal();
                }
            }
        }
    });
    timer.start(150);
}
```

- [ ] **Step 5: Trigger the startup check once**

In `tray.rs`, where the tray app finishes initial setup (after the taskbar icon + main frame are created, once), call `check_for_updates_on_startup(&main_frame)` a single time. Locate the setup site:

```bash
grep -n "TaskBarIcon\|Frame::builder\|fn run\|fn main_tray\|set_icon" src/gui/tray.rs | head
```

Insert the one-shot call right after the main hidden frame is built and shown, so a parent exists for any dialog.

- [ ] **Step 6: Build + commit**

Run: `cargo build --release`
Expected: compiles on Windows. Align wx symbol spellings with existing tray code as needed (logic unchanged).

```bash
git add src/gui/tray.rs
git commit -m "Wire tray Check-for-updates + Settings + startup check"
```

---

## Task 13: CI signing step

**Files:**
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: GitHub Secret `MINISIGN_SECRET_KEY`.
- Produces: `SHA256SUMS.minisig` as a published release asset.

- [ ] **Step 1: Add the signing step**

In `.github/workflows/release.yml`, in the `release` job, between "Assemble dist + checksums" (ends line 130) and "Extract changelog section" (line 132), add:

```yaml
      - name: Sign SHA256SUMS (minisign)
        env:
          MINISIGN_SECRET_KEY: ${{ secrets.MINISIGN_SECRET_KEY }}
        run: |
          set -e
          # minisign is packaged in Ubuntu's repos.
          sudo apt-get update && sudo apt-get install -y minisign
          printf '%s\n' "$MINISIGN_SECRET_KEY" > minisign.key
          # Passwordless key: feed an empty passphrase on stdin.
          minisign -S -s minisign.key -m dist/SHA256SUMS </dev/null
          rm -f minisign.key
          test -f dist/SHA256SUMS.minisig
          echo "Signed SHA256SUMS:"; cat dist/SHA256SUMS.minisig
```

- [ ] **Step 2: Publish the signature**

In the "Publish release" step's `files:` list (line 151-155), add the `.minisig` line:

```yaml
          files: |
            dist/tt-spotify-bot-windows-x86_64.zip
            dist/tt-spotify-bot-linux-x86_64.tar.gz
            dist/tt-spotify-bot-linux-aarch64.tar.gz
            dist/SHA256SUMS
            dist/SHA256SUMS.minisig
```

(GitHub sorts assets alphabetically regardless of this order — the list only controls what gets uploaded.)

- [ ] **Step 3: Validate the workflow locally**

Run: `python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"`
Expected: `yaml ok`. (If Python/yaml unavailable, visually confirm indentation matches the surrounding steps.)

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "Sign SHA256SUMS with minisign in the release workflow"
```

---

## Task 14: Full-tree verification

**Files:** none (verification only).

- [ ] **Step 1: Clippy clean (both platforms' lints locally)**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings. Fix any (wrap ignored FFI/TT returns in `let _ =` per CLAUDE.md).

- [ ] **Step 2: All unit tests**

Run: `cargo test --lib`
Expected: all pass (146 prior + new: verify 6, github 7, apply 3, settings 4, autostart 1 on Windows).

- [ ] **Step 3: Release build**

Run: `cargo build --release`
Expected: builds clean.

- [ ] **Step 4: Manual smoke (documented, run before merge)**

- Windows: `--update` path via tray "Check for updates" against a real newer test release -> dialog shows changelog -> Download -> progress bar fills -> Cancel works mid-download -> full run replaces exe and relaunches.
- Windows: Settings dialog -> toggle both checkboxes -> Save -> reopen shows persisted state -> "Launch on Windows startup" appears in Task Manager > Startup.
- Linux: `ttspotify --update` with a newer release -> changelog + y/N -> installs -> restart hint printed. Piped (`echo | ttspotify --update`) -> refuses.
- Tamper test: hand-edit a downloaded SHA256SUMS or point at a bad asset -> update aborts with a signature/hash error, binary untouched.

- [ ] **Step 5: Push the branch (do NOT merge to main — user merges)**

```bash
git push -u origin signing-updater
```

---

## Post-implementation notes

- **Embedded public key** is already generated and its secret is in GitHub Secrets (`MINISIGN_SECRET_KEY`). The first signed release will be the first tag pushed after Task 13 lands.
- **Testing the updater end-to-end** requires a real GitHub release newer than the running build. Cut a throwaway prerelease tag on the branch (as done for v0.3.0 validation) or bump to a test version to exercise the full path, then delete it.
- **Branch policy:** everything lands on `signing-updater`; the user reviews and merges to `main`.
