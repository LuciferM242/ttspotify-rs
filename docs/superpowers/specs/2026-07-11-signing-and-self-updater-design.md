# Release Signing + Self-Updater — Design

**Date:** 2026-07-11
**Branch:** `signing-updater`
**Status:** Approved design, pending implementation plan

## Goal

Add authenticity to releases (minisign signature) and an in-app updater that
checks GitHub Releases, verifies signature + hash, and replaces the binary. User
stays in control: notify + manual trigger, never silent auto-replace.

Distribution stays GitHub Releases over HTTPS. Signing exists specifically because
the updater auto-replaces the executable — an unverified swap would be an RCE
vector if a release were ever tampered with.

## Non-goals

- No background polling. Check on startup (if enabled) + manual only.
- No periodic mid-session re-check.
- No Windows code-signing certificate (SmartScreen) — separate paid concern.
- No Spotify-only / variant matching (only one build variant exists today).
- No auto-update of a running systemd service — Linux updates are user-run.

---

## Section 1 — Signing (CI side)

`release.yml`, `release` job, new step after `SHA256SUMS` is generated and before
publish:

- Install minisign.
- `minisign -S -s <secret> -m dist/SHA256SUMS` -> `dist/SHA256SUMS.minisig`.
- Secret key + password come from GitHub Secrets `MINISIGN_SECRET_KEY` and
  `MINISIGN_PASSWORD`.
- Add `dist/SHA256SUMS.minisig` to the published `files:` list.

We sign **SHA256SUMS only** (one signature covers all assets): lightest (1 extra
asset), fastest (one sign / one verify), and folds authenticity + integrity into
one client flow. Standard Debian-repo pattern.

### User one-time setup (blocking prerequisite)

The maintainer must, before this ships:

1. `minisign -G` locally -> password-protected secret key + public key.
2. Put the secret key file contents into GitHub Secret `MINISIGN_SECRET_KEY`, the
   password into `MINISIGN_PASSWORD`.
3. Hand the **public key** to embed as a `const` in the app.

The secret key never enters the repo. Security rests on the secret staying secret,
not on hiding the signing code (Kerckhoffs's principle).

---

## Section 2 — Update core (shared, platform-agnostic)

New module `src/update/`. Pure logic, no GUI. Used by both entry points.

### `check() -> Result<Option<UpdateInfo>>`

- GET `https://api.github.com/repos/LuciferM242/ttspotify-rs/releases/latest`
  (unauthenticated; 60/hr limit is fine). Only the public repo slug is baked into
  the binary — no credentials, tokens, or account info.
- Parse `tag_name` (e.g. `v0.4.0`), semver-compare to `CARGO_PKG_VERSION`. Newer ->
  `Some(UpdateInfo)`, else `None`. Never downgrades.
- Select the asset matching this build via `cfg!`:
  - Windows -> `tt-spotify-bot-windows-x86_64.zip`
  - Linux x86_64 -> `tt-spotify-bot-linux-x86_64.tar.gz`
  - Linux aarch64 -> `tt-spotify-bot-linux-aarch64.tar.gz`
- `UpdateInfo { version, changelog (release body), asset_url, sums_url, sig_url }`.

### `download_and_apply(info, progress_cb, cancel_flag) -> Result<()>`

1. Download `SHA256SUMS` + `SHA256SUMS.minisig`.
2. **Verify signature** (`minisign-verify`) against the embedded public key.
   Fail -> abort, nothing on disk touched.
3. Download the asset archive, reporting bytes via `progress_cb`, checking
   `cancel_flag` between chunks.
4. **Hash** the archive (`sha2`), confirm it matches its line in the verified
   `SHA256SUMS`. Mismatch -> abort.
5. Extract the single binary to a temp file next to the target (`zip` on Windows,
   `tar` + `flate2` on Linux).
6. **Swap** via `self-replace` (handles Windows' locked running exe).

**Invariant: verify-before-write.** Signature and hash both pass before anything
replaces the binary. `progress_cb` is a closure, `cancel_flag` is
`Arc<AtomicBool>` — GUI supplies bar + Cancel, CLI supplies a text counter.

---

## Section 3 — Windows UI (tray GUI)

Builds on existing `gui/` (wxDragon).

### Startup check

If the app-global setting `check_updates_on_startup` is on: after the bot starts,
spawn `update::check()` off the GUI thread. `Some` -> marshal to the GUI thread,
show the update dialog. Never blocks startup.

### Update dialog (`gui/update_dialog.rs`, new)

- Title: `Update available - v0.4.0`.
- Read-only multi-line text field showing the changelog (release body).
- Buttons: **Download** / **Later**. Later dismisses. Download opens progress dialog.

### Progress dialog

- Modal: a `Gauge` (progress bar) + **Cancel** button.
- Runs `download_and_apply()` on a worker thread; `progress_cb` drives the gauge,
  `cancel_flag` wired to Cancel.
- Success: brief "Updated - restarting", relaunch the new exe, exit the old.
- Cancel/error: close, bot keeps running on the current version, temp files cleaned.

### Tray menu additions

- **Check for updates** — runs `check()` manually (ignores the toggle). Newer ->
  same dialog; current -> "You're up to date" info box.
- **Settings** — opens the Settings dialog.

### Settings dialog (`gui/settings_dialog.rs`, new)

Styled like the config editor. Two checkboxes + Save / Cancel:

1. **Check for updates on startup** — bound to `settings.json`
   `check_updates_on_startup`.
2. **Launch on Windows startup** — bound to the registry (see below). Reflects the
   current registry state.

### "Launch on Windows startup" — registry (Windows-only)

Exact target, no ambiguity:

- Hive: **`HKEY_CURRENT_USER`** (per-user, no admin). Never HKLM.
- Key: `Software\Microsoft\Windows\CurrentVersion\Run` (this exact subkey — not
  RunOnce, not Policies, not Winlogon).
- Value name: fixed constant `ttspotify-rs`, type `REG_SZ`. Same name for write and
  delete so no orphaned entries.
- Value data: **quoted** absolute path from `std::env::current_exe()`, e.g.
  `"C:\Path\tt-spotify-bot.exe"`.

Rules:
- On -> create/overwrite our named value with the current exe path (self-heals a
  stale path).
- Off -> delete only our named value. Never touch the key or other values.
- Checkbox state -> our value present = checked, absent = unchecked.

Shows up in Task Manager > Startup apps and Settings > Apps > Startup under the
value name. Known accepted edge: disabling via Task Manager sets a separate
`StartupApproved\Run` flag we do not read, so our checkbox may still show checked
while Task Manager shows disabled. Cosmetic only; not synced by design.

Dep: `winreg` (Windows-only), scoped to that one value.

---

## Section 4 — Data model + CLI + errors

### App-global settings (`src/settings.rs`, new)

- `AppSettings { check_updates_on_startup: bool }`, serde, default `true`.
- `data/settings.json` (Windows) / `~/.config/ttspotify/settings.json` (Linux).
- Load-or-default; atomic write (tmp + rename), same pattern as bot configs.
- App-global on purpose: one shared binary, so this is not a per-bot config field.
- "Launch on startup" is **not** here — registry is the Windows source of truth.

### CLI `--update` (`main.rs`)

- `ttspotify --update`: run `check()`.
  - None -> print `Already up to date (vX).`
  - Some -> print the changelog, prompt `Update to vY? [y/N]`. `y` ->
    `download_and_apply()` with a text percent line, then
    `Updated to vY. Restart the service: systemctl --user restart ttspotify@<name>`.
  - No TTY (piped/service) -> refuse with a message, take no action.
- **Startup breadcrumb:** Linux `run_bot`, if `check_updates_on_startup`, spawns a
  non-blocking check; on newer, logs one line
  `Update vY available - run: ttspotify --update`. Never blocks connect. A systemd
  service has no terminal, so it only logs; the interactive install is the
  user-run `ttspotify --update` in their own shell.

### Error handling

All update failures are non-fatal and never touch the binary unless signature AND
hash both pass. Network down, rate-limited, signature fail, hash fail, extract fail
-> log / report, bot keeps running. New `UpdateError` enum with short
`user_error()`-style messages for GUI/CLI; raw detail to logs only.

---

## New dependencies

- `minisign-verify` — signature verify (tiny, pure Rust).
- `self-replace` — swap the running exe (Windows locked-file safe).
- `tar` + `flate2` — decode Linux/arm `.tar.gz` (Windows uses existing `zip`).
- `semver` — version comparison.
- `winreg` — Windows-only, autostart registry value.

Reuses existing `reqwest` (HTTP) and `sha2` (hashing).

## Testing

- `check()`: version compare (newer / equal / older / malformed tag), asset
  selection per `cfg!`, no-downgrade.
- Signature verify: valid sig passes, tampered SUMS fails, wrong-key sig fails.
- Hash check: matching passes, mismatch aborts before any write.
- `AppSettings`: load-or-default, round-trip, atomic write.
- Registry helper (Windows): on writes the value, off deletes it, state reflects
  presence. (Behind `#[cfg(windows)]`.)
- Manual smoke: real signed release -> Windows dialog + progress + Cancel + swap +
  restart; Linux `ttspotify --update` interactive flow; tampered asset aborts loudly.

## Open follow-ups (not this branch)

- Spotify-only variant + variant-aware asset matching (embed a variant tag).
- Optional louder Linux notification (motd / login file) if journal breadcrumb is
  missed.
