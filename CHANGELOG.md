# Changelog

## [Unreleased]
### Added
- The bot saves the music it plays, so a song somebody asks for again starts
  straight away instead of being fetched a second time. Spotify and YouTube
  share one pool, and so does every bot on the computer. It holds 1 GB by
  default: past that, the track nobody has played for longest goes first, and
  anything unplayed for a week is dropped, so what people keep asking for
  keeps starting instantly. It tells you how much it is holding and lets you
  clear it, on Windows and on Linux, and clearing costs nothing but
  downloading those tracks again next time somebody asks. Setup has the size
  limit and how many days an unplayed track is kept, or set "cacheLimitMb" and
  "cacheKeepDays" in settings.json; 0 means no limit and no age rule.
- Linux is driven by commands now, and they are words rather than switches:
  "ttspotify start apple", not "ttspotify --start apple". Running "ttspotify"
  on its own lists them. Everything the Windows tray does with menus has a
  command, and you never type a systemd unit name:
  - "list" and "status" show your bots and which are running; "start", "stop"
    and "restart" take a bot's name or "all"; "logs" shows what a bot has been
    doing and "watch" follows it live; "run" starts one in your terminal.
  - "add" creates a bot and "remove" deletes one. Removing asks first, shows
    exactly which file it will delete, and asks about that bot's logs
    separately. Your other bots, their logins and their settings are never
    touched.
  - "edit" changes a config without retyping it. Every question is filled in
    with the current value and Enter keeps it, and it covers the settings only
    the Windows editor could reach before, such as audio quality,
    normalisation, the jitter buffer and radio batching.
  - "doctor" prints one screen covering the version, where your files are,
    which bots are configured and running, whether the Spotify login and
    YouTube tools are in place, and anything that looks wrong - each with the
    command that fixes it. It is the thing to paste when asking for help.
  - "install" puts the binary on your PATH, replacing an earlier copy rather
    than leaving two at different versions, and offers to point an existing
    service at the new location. "uninstall" reverses it, asking before each
    step; add "purge" to be asked about configs, logins and logs as well.
  - "completions" gives tab completion for bash, zsh, fish and friends, and
    "man" a man page for anyone packaging the bot.
- "shuffle" is its own command now: "shuffle" toggles it, "shuffle on" and
  "shuffle off" force it. It works alongside repeat.
- Tracks you ask for by name play before the rest of a playlist or radio,
  instead of going to the back of the queue.
- YouTube support installs Deno, which YouTube playback now needs. If you
  already have it, that copy is used and nothing is downloaded.
- A bot can now be limited to YouTube only or Spotify only. A YouTube-only bot
  never touches the Spotify login saved on your machine. Setup asks which
  services each bot should offer, on Windows and on Linux, and "sp" on a bot
  without Spotify says so instead of switching. "info" shows what a bot
  offers.

### Changed
- The queue has been rebuilt to work the way a normal music player does. A
  track you ask for by name plays next, with playlists and radio filling in
  behind it, and what has already played is kept so you can step back through
  it. Shuffle is its own thing now and covers every queued track exactly once
  in random order, where it used to skip past tracks entirely - so "mode s" is
  gone in favour of "shuffle", and "mode" sets repeat only. "o" restarts the
  current track if you are more than three seconds in and only steps back a
  track if you press it early, while "replay" always restarts. The queue
  listing shows the current track and what is coming rather than what has
  already played, numbered from 1, so the number you read is the number
  "queue rm" takes.
- Volume now changes evenly as you turn it up and down. Before, most of the
  difference happened in the low numbers and the top half of the range barely
  changed anything. Volume numbers mean the new scale everywhere, so after
  updating, set each bot's volume once to taste - roughly, add 20 to what you
  had.
- Your files are now organised into config/, lang/, state/, auth/, cache/ and
  logs/, and each bot logs into its own folder, one file per day. Everything
  is moved across on first start and nothing is deleted. cache/ is always safe
  to delete; auth/ should never be shared. On Linux the configs moving means
  an installed service file still points at the old path - the bot copes on
  its own, but re-run "ttspotify service install" to update it properly.
- Help is translated too now, not just the bot's replies. Command names stay in
  English, since those are what you type.
- Setup questions with a few possible answers are numbered menus now: type 1,
  2 or 3 instead of spelling the answer out.
- Repeating a search within a few minutes answers instantly.
- The tray shows which Spotify account you are signed in as.

### Fixed
- Links you paste now play what you pasted. A Spotify link copied from an app
  set to any language other than English carries the language in it
  (open.spotify.com/intl-de/track/...), and the bot did not recognise it as a
  link at all - it searched for the whole address as if you had typed words
  and played whatever came back. Links from the embedded player did the same.
  On the YouTube side, "/live/" and "/embed/" links now play instead of being
  searched as text, and watch links with a "#" in them work.
- Music on YouTube plays again. Songs uploaded by record labels and the
  automatic "- Topic" channels, which is most of what people ask for, had
  started coming back refused while ordinary videos still played, so the bot
  looked half-broken. It now asks YouTube through a third player as well, and
  that one still answers for them; tracks take a few seconds longer to start.
  Tracks that failed for no clear reason are retried another way before the
  bot gives up, a request YouTube turns down no longer crashes the bot, and
  neither do tracks with mono audio.
- Saved music no longer fills your disk. Every track played was kept forever
  and nothing removed it, so the bot's folder grew for as long as it ran - on
  a Raspberry Pi, until the card was full. It is now kept under a limit, and
  an install already over it is tidied up the next time it starts. Songs cut
  short part-way through a download are cleared away rather than left sitting
  in the folder.
- Radio and the queue behave themselves:
  - The queue no longer grows without end while radio is on, and radio no
    longer keeps adding the same song as a remaster or re-release - though a
    remix or extended mix is still treated as the different track it is.
  - "repeat queue" actually loops instead of fighting with radio, and "n" with
    "repeat track" on moves to the next track rather than restarting it.
  - "n" no longer says the queue has ended while tracks are still sitting in
    it, and a track that reports finishing twice no longer skips twice.
  - "queue rm" removes the track it says it removed, even if the queue moved
    at that same moment; it could delete the wrong one before.
  - Repeat mode survives a restart. It used to be forgotten unless the bot was
    shut down with "q".
- Playback and seeking:
  - Moving the bot to another channel mid-song no longer breaks its audio. On
    Windows the sound stopped at the moment of the move and the bot then
    refused to stop, having to be abandoned when you closed it. It now starts
    a fresh stream in the new channel, keeping the music it had already read
    ahead, so the song carries on and still reports the right position.
  - Seeking near the end of a track no longer cuts off its last seconds, and a
    very large seek ("sf3000000") no longer jumps backwards to the start.
  - Stopping the bot silences it at once instead of playing out the few
    seconds it had already buffered.
- Spotify problems no longer take the bot down with them. An outage in the
  middle of a song no longer freezes it: it reconnects on its own and resumes
  a few seconds before where it stopped, so nothing is missed. A brief network
  problem no longer opens a browser asking you to sign in, a string of
  unavailable tracks stops with a message instead of racing through the whole
  queue, and when Spotify will not hand over the key for a track the bot says
  so rather than blaming the track. Signing in from the tray now tells you
  when it fails.
- Running bots on Linux:
  - The same bot can no longer run twice at once. Starting one in a terminal
    while it was already running in the background put two copies on the same
    TeamTalk account, which knocked each other offline and looked like the bot
    disconnecting at random.
  - Stopping a bot, with systemctl or Ctrl+C, now leaves the server properly
    instead of being killed where it stood and leaving a ghost user behind.
    Press Ctrl+C twice if a stop ever takes too long.
  - Removing the systemd service stops and un-enables the bots first. They
    used to stay enabled with the service gone, so systemd complained at every
    login and a running bot could not be stopped or restarted; running the
    command again repairs an install left in that state.
  - A bot that cannot reach its server no longer restarts every two seconds
    for as long as the machine is on, logging in over and over with nothing on
    screen to say so. It waits half a minute between tries, gives up after
    five, and "status" and "doctor" then say the bot is failing and point at
    its log. Re-run "ttspotify service install" once to get this.
  - A missing audio library now names the package to install, instead of
    failing with a message about initialisation that read like a bug.
  - Running "ttspotify" on its own used to start whichever bot sorted first,
    silently; it lists the commands now. Its suggestions ("run this to log
    in") also name the command you actually launched it as, rather than
    sometimes naming one you do not have.
- The Windows tray: "Logs" opens the log it meant to, Stop and Restart can no
  longer act on the wrong bot if the list changes while the menu is open, and
  shutting down or logging off disconnects the bots properly, so the server no
  longer keeps a ghost copy of each one online afterwards.
- Clearer answers when something goes wrong. Long replies no longer vanish:
  anything over the server's message limit was rejected outright rather than
  split, so "queue" on a big queue and the full help text sometimes arrived as
  nothing at all. A search that finds nothing says "No results" instead of
  "Search failed: No results found". A bot that cannot reach its server says
  which address and port it tried and what to check, and a refused login names
  the username. "defaultService" written as "youtube" rather than "YouTube" no
  longer makes the bot disappear from every command, and a value naming no
  service says so, and says which file it is in. "auth status" says whether the
  login actually works rather than only whether one was saved here once - it
  could not tell you before that a password change had ended it. Help, errors
  and the man page name the command as you typed it, instead of the name the
  file has inside the download. If TeamTalk's own files fail to download, the
  bot keeps the version it already has instead of quietly switching to
  another.

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
