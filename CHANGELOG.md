# Changelog

## [Unreleased]
### Added
- YouTube support now installs Deno, the JavaScript runtime yt-dlp needs to
  work out YouTube's playback links. Since yt-dlp 2025.11.12 some tracks and
  qualities are simply unavailable without one, which looks like a YouTube
  fault rather than a missing tool. If you already have Deno installed that one
  is used and nothing is downloaded. "Update tools" keeps it current, and the
  startup log now says which runtime is in use.

### Changed
- The Windows tray and its windows are now plain Windows ones, built with the
  system's own dialogs instead of a bundled toolkit. They should feel and read
  the same, with better keyboard and screen-reader behaviour: every window has
  proper tab order, Alt shortcuts on the labels, and Enter and Escape work
  throughout. The download is smaller and starts faster.
- The tray icon can now be reached from the keyboard as well as the mouse, and
  its menu opens with Enter or the Applications key.
- The tray icon reappears by itself if Windows Explorer restarts. Previously it
  vanished until the app was restarted.
- Installing or updating the YouTube tools, and downloading an update, no longer
  make the tray stop responding while they run.
- The data folder is now organised into subfolders instead of one flat pile:
  config/ for your configs and cookies.txt, lang/ for translations, state/ for
  things the bot remembers, auth/ for saved logins, cache/ for anything it can
  fetch again, and logs/. Your existing files are moved there automatically the
  first time you start this version; nothing is deleted or overwritten. A short
  README.txt in the folder explains what each one is - the short version is
  that deleting cache/ is always safe, and auth/ should never be shared.
- Each bot now logs into its own folder (logs/<name>/), one file per day.
  Existing logs are moved there automatically. Keeping them apart means a busy
  bot can no longer age out a quiet one's history when old logs are pruned.
- Linux: because configs moved into config/, an already-installed service file
  still points at the old location. The bot follows the move on its own so
  nothing stops working, and logs a reminder; re-run --install-service (or say
  yes when an update offers to refresh the service file) to update it properly.

### Added
- "shuffle" is now its own command: "shuffle" turns it on or off, "shuffle on"
  and "shuffle off" force it either way. It can be combined with repeat, the
  way it works in a normal music player.
- Tracks you ask for by name now play before the rest of a playlist or radio,
  instead of going to the back of the queue - Spotify's "next in queue" ahead
  of "next from".

### Changed
- Shuffle now plays every queued track exactly once, in a random order.
  Previously it jumped to a random track and the ones it jumped over never
  played at all.
- "mode s" is gone; use "shuffle". "mode" now sets repeat only
  (mode r, mode rq, mode off) and no longer switches shuffle off.
- "o" (previous) restarts the current track if you are more than three seconds
  into it, and only steps back a track if you press it early - the way the
  Previous button behaves in Spotify. "replay" still always restarts.
- Searching for something that was searched in the last few minutes now
  answers instantly instead of going back out to Spotify or YouTube.
- The queue now shows the current track and what is coming, instead of every
  track played this session - the way Spotify and YouTube Music show a queue.
  Upcoming tracks are numbered from 1, so the number shown is the number
  "queue rm" takes; before, the numbers you read and the numbers you typed
  were different.

### Fixed
- When Spotify refuses a track's audio key, the bot now says so instead of
  reporting "Track unavailable". The track was never the problem: the account
  could not decrypt it, usually because it is not premium or is streaming on
  another device. The log also records the account type when the session
  connects, so this is answerable at a glance.
- Failed Spotify tracks no longer flood the log. A track that cannot be
  decrypted reaches the decoder as noise, which reported every byte of it -
  10,000 warnings and 781 KB of log for three tracks, burying the one line
  that explained anything.
- Tray: "Logs" now opens the log it meant to. It was building a filename that
  never existed, so the menu item did nothing.
- Tray: clicking a bot's Stop or Restart could act on a different bot if the
  list changed while the menu was open.
- The startup line that records which TeamTalk SDK is in use said "unknown" on
  Windows since 0.7.0; it was looking in the wrong folder.
- "n" no longer says the queue has ended while tracks are sitting in it. When
  radio refilled a queue that had run out, the bot started playing the new
  batch but still considered nothing to be playing, so skipping found nothing
  to skip from - and every track that finished added another batch that was
  never played, which is what grew the queue.
- The queue no longer grows without end. With radio on, the bot added new
  recommendations every time it reached the last track and never dropped
  anything, so an evening of listening left dozens of played tracks in the
  queue. It now keeps the last 20 played tracks, whatever filled the queue.
  You can still step back that far with "p".
- Radio no longer keeps adding the same song. Recommendations were only
  checked against tracks already queued by their Spotify id, so a remaster,
  single or re-release of a song you already heard counted as a new track.
- Repeat no longer fights with radio. With repeat on, radio kept extending the
  queue past the end - which also meant "repeat queue" never actually looped.
  Radio now stays out of the way while either repeat mode is on.
- Skipping with `n` while "repeat track" is on now moves to the next track
  instead of restarting the same one. At the end of the queue it loops back to
  the first track.
- A track that reports finishing more than once (for example a YouTube error
  arriving alongside its end-of-track) no longer restarts or skips twice.
- The bot no longer crashes when YouTube declines to hand out a session token.
  This could take the whole bot down with no visible error (only a panics.log
  entry). YouTube requests that fail once are now retried automatically, and the
  session token is fetched at startup so the first search is less likely to fail.

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
