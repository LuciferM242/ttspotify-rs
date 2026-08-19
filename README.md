# ttspotify-rs

A blazing fast Spotify and YouTube bot for [TeamTalk](https://bearware.dk/) servers, built in Rust.

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

**No virtual audio devices, no loopback cables, no routing setup.** The bot
injects decoded PCM straight into TeamTalk's audio mixer, so there is nothing to
configure on the audio side — install it, point it at a server, and it plays.

## Supported services

### Spotify

Tracks, albums, playlists, search, and radio recommendations.

> A **Spotify Premium** account is required — free accounts will not work.

### YouTube

Videos, Shorts, playlists, albums, and search, played through
[yt-dlp](https://github.com/yt-dlp/yt-dlp).

YouTube requires **cookies** to play reliably. Export them with a browser extension:

1. Install a cookies-export extension — **Get cookies.txt LOCALLY** ([Chrome / Edge](https://chromewebstore.google.com/detail/get-cookiestxt-locally/cclelndahbckbenkjhflpdbgdldlbecc), or the equivalent for Firefox).
2. Open a **private / incognito** window and sign in to YouTube.
3. With the YouTube tab open, use the extension to export a `cookies.txt` file.
4. **Close the incognito window** (do *not* log out) so the exported cookies stay valid.
5. Put the file where the bot looks for it — `data/config/cookies.txt` (Windows) or `~/.config/ttspotify/config/cookies.txt` (Linux) — or set `youtubeCookiesFile` in your config to its path.

### Limiting a bot to one service

A bot can be restricted to just one of the two. Setup asks, on Windows and on
Linux. A YouTube-only bot never touches the Spotify login saved on your
machine. `sp` or `yt` on a bot without that service says so instead of
switching, and `info` shows what a bot offers.

## Requirements

- A **TeamTalk 5 server** to connect to, and a TeamTalk account for the bot.

## Installation

Download the latest build from the [**Releases page**](https://github.com/LuciferM242/ttspotify-rs/releases).

### Windows

1. Download `tt-spotify-bot-windows-x86_64.zip`, extract it, and run the `.exe` — a tray icon appears.
2. On first run it prompts you to create a config (a setup dialog). Fill it in and the bot connects.
3. Use the tray menu for **Spotify auth**, **Install YouTube tools**, and each bot's start / stop / restart / logs / edit / **Remove Server**.

### Linux (x86_64, Ubuntu 22.04+ / glibc)

Install the one runtime dependency — `libpulse0`, a shared library the TeamTalk
SDK links against (the bot tells you if it is missing, and `ttspotify doctor`
checks for it):

```bash
sudo apt install -y libpulse0
```

Extract the archive:

```bash
tar -xzf tt-spotify-bot-linux-x86_64.tar.gz
```

Put the binary on your `PATH` — it installs itself, into `~/.local/bin` when
that is writable, and tells you if that directory is not on your `PATH` yet:

```bash
./tt-spotify-bot install
```

Run it — on first launch, with nothing configured yet, it offers to install
itself on your `PATH`, walks you through the **setup wizard**, then connects:

```bash
ttspotify
```

To install the YouTube tools (yt-dlp, bgutil-pot, and Deno — the JavaScript
runtime YouTube playback now needs; an existing Deno is used as-is):

```bash
ttspotify yt install
```

To update the YouTube tools later:

```bash
ttspotify yt update
```

Optional systemd service — install it once:

```bash
ttspotify service install
```

then start a bot by its config name:

```bash
ttspotify start myserver
```

If something does not work, `ttspotify doctor` prints what is installed, what
is running, and the command that fixes whatever looks wrong.

### Linux (aarch64 / Raspberry Pi)

Runs on a Raspberry Pi (Pi Zero 2 W through Pi 5) on **64-bit Raspberry Pi OS**
(Debian 12 / bookworm or newer). 32-bit boards (Pi Zero / 1 / 2) are not
supported. Same steps as x86_64, using the aarch64 archive:

```bash
sudo apt install -y libpulse0
```

```bash
tar -xzf tt-spotify-bot-linux-aarch64.tar.gz
```

```bash
./tt-spotify-bot install
```

> **Platform support:** Windows x64, Linux x86_64, and Linux aarch64 (glibc,
> Ubuntu 22.04 / Debian 12 or newer).

## Updating the bot

The bot has a built-in self-updater: it checks GitHub for a newer release,
shows you what changed, verifies the download's signature, and swaps the
binary in place. No manual re-download needed.

- **Windows:** the tray checks on startup and offers the update; there's also
  a **Check for updates** item in the tray menu.
- **Linux:** run `ttspotify update`. If bots are running as systemd
  services, it offers to restart them on the new version — and to refresh
  your service file when a release improves it.

Updating the YouTube tools (yt-dlp and friends) is separate:
`ttspotify yt update`, or **Update tools** in the tray menu.

## Running multiple bots

Multiple instances are supported out of the box — one per config file, each with its own server and account.

**Windows:** the tray manages them all. Right-click → **Add Server** once per bot; every config shows up in the tray menu with its own start / stop / restart / logs, and a **Remove Server** that asks before deleting anything.

**Linux:** create each bot's config with the wizard, giving it a name:

```bash
ttspotify add server1
```

On systemd systems the wizard ends by offering to enable and start
`ttspotify@server1` for you — say yes and the bot is up (if the service isn't
installed yet, it offers to install it first).

After that, manage bots by the name you gave them — no systemd unit names
needed:

- `ttspotify list` — the bots configured here
- `ttspotify status` — which are running, and since when; a bot that keeps
  stopping is reported as failing rather than as merely stopped
- `ttspotify start server1` — also `stop` and `restart`; each takes a bot name
  or `all`
- `ttspotify logs server1` — what the bot has been doing
- `ttspotify watch server1` — follow the log live (Ctrl+C stops watching, not
  the bot)
- `ttspotify edit server1` — change any setting, with the current value offered
  as the default for every question
- `ttspotify remove server1` — delete a bot, after confirming, and ask about
  its logs separately
- `ttspotify doctor` — what is installed, what is running, what to fix

Each instance reads `~/.config/ttspotify/config/<name>.json`. To run one in
your terminal rather than in the background, use `ttspotify run server1`.
Running `ttspotify` with no arguments lists the commands. See
`ttspotify --help` for everything else.

Shell completion, if you want it — this is bash, and `zsh`, `fish` and others
work the same way:

```bash
ttspotify completions bash > ~/.local/share/bash-completion/completions/ttspotify
```

To remove the program entirely, `ttspotify uninstall` stops the bots, removes
the service and the binary, and leaves your configs and logs alone unless you
add `--purge`. (To remove one bot rather than the program, use
`ttspotify remove <name>`.)

## Configuration

Config is a JSON file, generated by the first-run setup wizard. Locations:

- **Windows:** `data\config\<name>.json` (`data` sits next to the executable)
- **Linux:** `~/.config/ttspotify/config/<name>.json`

Everything the bot keeps lives under that one folder, sorted by what it is:
`config/` (yours to edit), `lang/`, `state/`, `auth/` (saved logins — never
share it), `cache/` (safe to delete at any time) and `logs/`. A `README.txt`
in the folder says the same. Upgrading from an older version moves your files
there automatically; nothing is deleted.

Songs are saved after the first play, so asking for the same one again starts
it straight away. That saved music is shared by every bot on the computer and
is capped at 1 GB, with the tracks nobody has played for longest dropped first,
and anything unplayed for a week dropped whatever the size. Both figures live
in `settings.json` (`cacheLimitMb`, `cacheKeepDays`); 0 for the limit keeps
nothing, 0 for the days never drops a track for its age alone. On Windows the
tray's Settings window has both, and its menu has "Clear cache"; on Linux,
`ttspotify cache` shows what is saved and `ttspotify cache clear` empties it.
Deleting it costs nothing but downloading those songs again.

Common fields you might edit (the wizard sets sensible defaults for the rest):

| Field | What it does |
|---|---|
| `host` | TeamTalk server address |
| `tcpPort` / `udpPort` | server ports (usually both `10333`) |
| `botName` | the bot's display name in the channel |
| `username` / `password` | the bot's TeamTalk login |
| `ChannelName` | channel to join, e.g. `/Music` |
| `ChannelPassword` | password if the channel is protected |
| `spotifyQuality` | `NORMAL`, `HIGH`, or `VERY_HIGH` |
| `spotifyMaxVolume` | volume cap, 0–100 |
| `defaultService` | `Spotify` or `YouTube` on startup (capitalisation does not matter) |
| `enabledServices` | which services this bot may use at all, e.g. `["youtube"]`. Missing means both |
| `youtubeCookiesFile` | path to your YouTube `cookies.txt` (optional) |
| `adminMode` | who may use admin commands: `Everyone`, `TtRights`, `List`, or `Both` (default) |
| `admins` | usernames treated as admins (used by `List` / `Both`) |
| `defaultLanguage` | language code for bot replies, e.g. `en` (default) or `pt` |

## Admin permissions

The `q` (quit), `rs` (restart), `jc` (join channel), and `glang` (default
language) commands can be limited to admins. Pick who counts as an admin in the
config editor (Windows) or setup wizard (Linux):

- **Everyone** — no restrictions; any user can run every command.
- **TeamTalk server admins** — accounts your TeamTalk server marks as admin.
- **Username list** — only the usernames you list in `admins`.
- **Both** (default) — a server admin *or* a listed username.

Non-admins don't see the admin commands in help and get no response if they try
them.

## Languages

Bot replies can be translated. English, Spanish, Portuguese, and Russian are
built in; add other languages (or adjust the built-in ones) with plain text
files you drop into the `lang` folder next to your config
(`data/lang/` on Windows, `~/.config/ttspotify/lang/` on Linux).

To translate:

1. Start the bot once — it writes `lang/en.lang`, the commented English
   template.
2. Copy it, translate the text after each `=`, and save as `<code>.lang`
   (for example `pt.lang`). You can move the `{words in braces}` anywhere in
   your sentence, but don't rename them. Skip or delete any line to leave that
   message in English — partial translations are fine.
3. Restart the bot. The startup log shows how many messages each file covers.

Users pick their own language with `lang <code>` (remembered by username);
`lang clear` goes back to the server default. Admins set the server-wide
default with `glang <code>`. Help text is translated too; command names
themselves stay in English, since those are what you type.

## Commands

Send these to the bot in a **private message** — it only responds to PMs, not to channel or broadcast messages.

| Command | Description |
|---|---|
| `p <query>` | Search and play a track, playlist, or album — a track you name plays next, ahead of a playlist or radio |
| `p` | Toggle play / pause |
| `s` | Stop and clear the queue |
| `n` | Next track |
| `o` | Previous track, or restart the current one if you are more than 3 seconds in |
| `replay` | Restart the current track (alias: `rp`) |
| `c` | Show the current track (position, duration, modes) |
| `queue` | Show the current track and what is coming, numbered from 1 |
| `queue clear` | Clear upcoming tracks |
| `queue rm <N>` | Remove the Nth upcoming track (the number `queue` shows) |
| `mode [r\|rq\|off]` | Repeat track / repeat queue / off |
| `shuffle [on\|off]` | Shuffle what is coming; no argument toggles it. Works alongside repeat |
| `v [0-100]` | Get or set volume |
| `sf [N]` / `sb [N]` | Seek forward / backward N seconds (default 10) |
| `search <query>` | Search, then type a number to pick (`a` to cancel) |
| `pick <N>` | Pick from the last search by number |
| `radio [on\|off]` | Toggle Spotify recommendations (Spotify only) |
| `liked` | Play your Spotify Liked Songs (alias: `fav`, Spotify only) |
| `sp` / `yt` | Switch between Spotify and YouTube |
| `link` | URL of the current track |
| `lang [code]` | Show available languages, or set yours (`lang clear` to reset) |
| `cn <name>` | Change the bot's nickname |
| `gender` | Set the bot's gender |
| `stats` | Session stats |
| `info` | Bot info |
| `h` / `h <command>` | Help, or detailed help for one command |

Admin-only (see [Admin permissions](#admin-permissions)):

| Command | Description |
|---|---|
| `jc <path>` | Join a channel |
| `glang <code>` | Set the server default language |
| `rs` | Restart the bot |
| `q` | Quit the bot |

## Building from source

Build prerequisites — **Linux:** gcc, pkg-config, libssl-dev, libclang-dev.
**Windows:** Visual Studio Build Tools with the **Desktop development with C++**
workload, plus LLVM.

On Windows you must install the [Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
yourself first (the script only checks for them). The helper scripts install the
rest — Rust and LLVM. The Windows GUI is native Win32, so there is no toolkit
to build and CMake and Ninja are no longer needed.

Linux (x86_64 and aarch64):

```bash
./scripts/setup.sh
```

Windows:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup.ps1
```

Then build the binary for your platform:

```bash
cargo build --release
```

and run the unit tests:

```bash
cargo test --lib
```

The TeamTalk SDK and YouTube tools are fetched at runtime — nothing proprietary is bundled or committed.

## License

Licensed under the [GNU General Public License v3.0](LICENSE).
