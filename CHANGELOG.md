# Changelog

## [0.7.0] - 2026-07-21
### Added
- YouTube Shorts links now play.
- Linux: after an update, the bot offers to refresh your systemd service file
  when this release improves it - no more finding out from the changelog.

### Changed
- Big YouTube playlists start playing right away and load the rest in the
  background, like Spotify playlists already did.
- Installing YouTube support now downloads the latest yt-dlp instead of a
  version fixed at release time.
- The tray "Update tools" window now says which yt-dlp version it updated
  from and to.
- Linux: the YouTube tools and the downloaded TeamTalk SDK now live in fixed
  folders instead of wherever the bot was started from. Existing copies are
  moved over automatically.

### Security
- Linux: services installed with --install-service now run sandboxed - the
  bot can write only to its own folders, the rest of the system is read-only
  to it. Re-run --install-service once to get this.

### Fixed
- Songs no longer end 6-7 seconds early: the bot now plays the buffered tail
  of a track (including the artist's fade-out) before starting the next one.
- Pausing no longer skips a few seconds on resume; playback continues from
  the exact spot you paused.
- Audio no longer comes out garbled after the bot is moved to another channel.
- Spotify playback recovers on its own when its streaming session drops,
  instead of staying broken until a restart.
- Broken YouTube tracks stop after three failures in a row instead of
  skipping forever (worst with repeat on).
- Skipping a track in the same instant it ends no longer jumps two tracks.
- Seeking a paused YouTube track now applies immediately, not on resume.
- "queue clear" also stops a playlist that is still loading in the background.
- Linux: a service started with a missing config name now stops with a clear
  error instead of restarting every 2 seconds (re-run --install-service once).
- Linux: config names with spaces or special characters work as service names.
- Tray: updating no longer cuts running bots off mid-session, Start right
  after Stop works, and no more brief black console window flashes.
- A failed update no longer leaves a temp file next to the program.
- Searches with extra spaces or tabs no longer fail on Spotify.
- Skipping past the last track in the queue no longer stops the current song;
  you get the end-of-queue message and the music plays on.
- Pausing during a song's final seconds no longer risks jumping to the next
  track after 30 seconds of pause.
- Small cleanups: cache files stay in the config folder, a stray startup
  warning about lang_prefs.json is gone, and the "yt-dlp not found" error
  names the right flag (--setup-yt).

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
