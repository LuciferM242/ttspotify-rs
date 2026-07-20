# Changelog

## [Unreleased]
### Changed
- Installing YouTube support now downloads the latest yt-dlp (still verified
  against its published SHA-256 checksum) instead of a fixed bundled version.
  A fresh install is already current, so it no longer has to re-download the
  whole binary the first time you run Update tools.
- The tray "Update tools" window now reports which yt-dlp version it updated
  from and to (or that it was already up to date), instead of just saying the
  check finished.

### Fixed
- Audio no longer comes out garbled after the bot is moved to another channel.
  Playback now restarts its stream cleanly on a channel change.
- Removed a spurious startup warning about lang_prefs.json being an invalid
  config file; it is the per-user language store, not a bot config.
- Spotify playback now recovers on its own when its streaming session drops.
  A dropped session previously left the bot unable to play until you restarted
  it; the bot now notices the dead session and rebuilds it quietly in the
  background (with backoff and a limit so it never hammers Spotify), and
  playback carries on without you having to do anything.
- Windows: the tray "Update tools" action and opening a log or config file
  from the tray menu no longer briefly flash a black console window.

## [0.6.1] - 2026-07-19
### Fixed
- Spotify search now works with non-Latin queries (Russian, and any other
  non-ASCII text). Searching in Cyrillic previously failed with "invalid
  argument, 400 Bad Request" because the query text wasn't encoded properly
  before being sent to Spotify.
- Linux: a bot running as a systemd service no longer crashes (and gets
  restarted over and over, appearing to log in and out of the TeamTalk server
  nonstop) when Spotify credentials are missing or rejected. A service has no
  browser and no keyboard, so the interactive Spotify login could never
  succeed there; the bot now detects this, logs a clear message telling you
  to run `tt-spotify-bot --auth`, and keeps running with Spotify disabled
  (YouTube still works). Interactive runs in a terminal behave as before.

## [0.6.0] - 2026-07-19
### Added
- Translations: the bot's replies can now be shown in other languages. Spanish,
  Portuguese, and Russian are built in; add or adjust any language by dropping a
  `<code>.lang` file (a simple text file: copy the `lang/en.lang` template the
  bot writes on startup and translate line by line) into the `lang` folder next
  to your config. Users pick their own language with `lang <code>` (remembered
  by username, `lang clear` to reset); admins set the server default with
  `glang <code>`. Anything not translated falls back to English, so partial
  translations are fine. Help text stays English for now.
- A "Default Language" option in the config editor and setup wizard.
- Admin permissions: the `q` (quit), `rs` (restart), `jc` (join channel), and
  `glang` (default language) commands can now be limited to admins. Pick who
  counts as an admin in the config editor or the setup wizard: everyone, your
  TeamTalk server's admins, a username list, or both. Non-admins don't see
  these commands in help and get no response if they try them. The default
  after upgrading is "Both" — if you used `q` or `rs` from a non-admin
  TeamTalk account, add your username to the admin list (or pick "Everyone").
- New `liked` command (alias `fav`): queues your Spotify Liked Songs.
- Big playlists and Liked Songs now start playing after the first 50 tracks;
  the rest load quietly in the background instead of making you wait.
- Update notes now cover every version since the one you have installed, not
  just the newest release, so skipped releases are no longer invisible.
- Linux: after `ttspotify --update` succeeds, the bot offers to restart your
  running systemd instances so they pick up the new version immediately.
- Linux: `--install-service` now offers to enable systemd lingering so the
  bot keeps running after you log out (important on a headless VPS). It only
  asks when lingering isn't already on.
- Linux: after the setup wizard creates a config, it now offers to enable and
  start that bot's systemd instance right away — and offers to install the
  service first if it isn't yet — so adding a server no longer ends with a
  config on disk but nothing running. Skipped on non-systemd systems.

### Changed
- After a successful update, newly added settings are written into your existing
  config files automatically, so you no longer have to start each bot for them
  to appear.
- Headless Spotify login now warns that the browser's "site can't be reached"
  page after authorizing is expected, so remote/VPS users no longer mistake it
  for a failure and know to copy the address-bar URL back to the bot.

### Fixed
- Empty or invalid `.json` files in the config folder are no longer mistaken for
  bot configs; only files with a real host and username are loaded.
- Linux: `--install-service` on systems without systemd (Alpine, Void, etc.)
  no longer writes a dead unit file and claims success; it now explains that
  systemd is required and points to running the binary directly or via another
  init.
- Smoother playback at track start: audio now buffers briefly before playing,
  so tracks no longer stutter when the connection is slow to get going.
- `p <song name>` now plays just the best match instead of queueing several
  search results.
- Editing an existing config from the tray no longer re-asks about installing
  YouTube support on every save; the prompt now only appears when creating a
  new config.
- Saving a config edit with no changes no longer rewrites the file or restarts
  the bot; the dialog just closes.

## [0.5.0] - 2026-07-13
### Added
- Self-updater: checks GitHub for a newer release and installs it (Windows via a
  tray dialog, Linux via `ttspotify --update`). Downloads are minisign-signed and
  verified before anything is replaced.
- Windows tray Settings: toggle update checks on startup and launch-on-startup.

## [0.3.0] - 2026-07-11
### Added
- aarch64 Linux support: runs on Raspberry Pi (Pi Zero 2 W through Pi 5) on
  64-bit Raspberry Pi OS. The release workflow builds a native aarch64 binary,
  and `--setup-yt` installs arch-correct yt-dlp and bgutil-pot binaries.

### Changed
- Release binaries are now packaged (Windows `.zip`, Linux/arm `.tar.gz`)
  instead of shipped bare.

### Note
- aarch64 Linux needs `libpulse0` installed at runtime (the TeamTalk SDK links
  PulseAudio); a headless Debian without it fails with "Init failed".

## [0.2.0] - 2026-07-10
### Added
- YouTube seek in both directions with accurate live position tracking.
- `replay` command to restart the current track.
- Startup log line reporting the app, TeamTalk SDK, yt-dlp, and bgutil-pot versions.
- Config validation on load (clamps volume, ports, and other out-of-range fields).
- Crash log: panics are written to `logs/panics.log` even when the tray has no console.

### Changed
- YouTube playback buffers the full track, making seek instant in both directions.
- Reconnect hardened: a watchdog recovers instead of spinning forever, the bot
  rejoins the correct channel, and the tray retries with backoff.
- The current channel is remembered across an `rs` restart (config default is untouched).
- Runtime config writes go through a single atomic writer (no more clobbering).
- Config directory resolves next to the executable on Windows.
- Slimmer build: a single TLS stack (rustls) instead of two, and the unused speaker
  backend removed.
- Updated the TeamTalk SDK integration (password now zeroized in memory / redacted in logs).
- Audio hot-path optimizations.

### Fixed
- End-of-queue no longer leaves the status stuck on "Playing".
- Fixed a YouTube double queue-advance race on track end.
- `sblah` no longer performs a seek; `queue rm <non-number>` shows usage; volume is clamped.
- Track-start failures are reported to the requester and auto-skipped.

### Removed
- Unused audio decoders and the unused local-speaker playback backend.

### Security
- Downloaded yt-dlp and bgutil-pot binaries are verified (SHA-256) before they are executed.

## [0.1.0]
- Initial release.
