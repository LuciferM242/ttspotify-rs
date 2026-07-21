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
5. Put the file where the bot looks for it — `data/cookies.txt` (Windows) or `~/.config/ttspotify/cookies.txt` (Linux) — or set `youtubeCookiesFile` in your config to its path.

## Requirements

- A **TeamTalk 5 server** to connect to, and a TeamTalk account for the bot.

## Installation

Download the latest build from the [**Releases page**](https://github.com/LuciferM242/ttspotify-rs/releases).

### Windows

1. Download `tt-spotify-bot-windows-x86_64.zip`, extract it, and run the `.exe` — a tray icon appears.
2. On first run it prompts you to create a config (a setup dialog). Fill it in and the bot connects.
3. Use the tray menu for **Spotify auth**, **Install YouTube tools**, and start/stop/restart/logs.

### Linux (x86_64, Ubuntu 22.04+ / glibc)

Install the one runtime dependency — `libpulse0`, a shared library the TeamTalk
SDK links against (without it the bot fails with `Init failed`):

```bash
sudo apt install -y libpulse0
```

Extract the archive:

```bash
tar -xzf tt-spotify-bot-linux-x86_64.tar.gz
```

Put the binary on your `PATH`:

```bash
sudo install -m755 tt-spotify-bot /usr/local/bin/ttspotify
```

Run it — on first launch (no config yet) it walks you through the **setup wizard**, then connects:

```bash
ttspotify
```

To install the YouTube tools (yt-dlp, bgutil-pot):

```bash
ttspotify --setup-yt
```

To update the YouTube tools later:

```bash
ttspotify --update-tools
```

Optional systemd service — install it once:

```bash
ttspotify --install-service
```

then enable an instance per config:

```bash
systemctl --user enable --now ttspotify@myserver
```

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
sudo install -m755 tt-spotify-bot /usr/local/bin/ttspotify
```

> **Platform support:** Windows x64, Linux x86_64, and Linux aarch64 (glibc,
> Ubuntu 22.04 / Debian 12 or newer).

## Updating the bot

The bot has a built-in self-updater: it checks GitHub for a newer release,
shows you what changed, verifies the download's signature, and swaps the
binary in place. No manual re-download needed.

- **Windows:** the tray checks on startup and offers the update; there's also
  a **Check for updates** item in the tray menu.
- **Linux:** run `ttspotify --update`. If bots are running as systemd
  services, it offers to restart them on the new version — and to refresh
  your service file when a release improves it.

Updating the YouTube tools (yt-dlp and friends) is separate:
`ttspotify --update-tools`, or **Update tools** in the tray menu.

## Running multiple bots

Multiple instances are supported out of the box — one per config file, each with its own server and account.

**Windows:** the tray manages them all. Right-click → **Add Server** once per bot; every config shows up in the tray menu with its own start / stop / restart / logs.

**Linux:** create each bot's config with the wizard, giving it a name:

```bash
ttspotify --setup server1
```

On systemd systems the wizard ends by offering to enable and start
`ttspotify@server1` for you — say yes and the bot is up (if the service isn't
installed yet, it offers to install it first). You can also manage instances
yourself at any time, one per config:

```bash
systemctl --user enable --now ttspotify@server1
```

Each instance reads `~/.config/ttspotify/<name>.json`. Running `ttspotify`
without `--config` picks the first config alphabetically; pass
`--config ~/.config/ttspotify/<name>.json` to run a specific one by hand.
See `ttspotify --help` for all command-line options.

## Configuration

Config is a JSON file, generated by the first-run setup wizard. Locations:

- **Windows:** `data/<name>.json` (next to the executable)
- **Linux:** `~/.config/ttspotify/<name>.json`

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
| `defaultService` | `Spotify` or `YouTube` on startup |
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
default with `glang <code>`. Help text is currently English only.

## Commands

Send these to the bot in a **private message** — it only responds to PMs, not to channel or broadcast messages.

| Command | Description |
|---|---|
| `p <query>` | Search and play a track, playlist, or album |
| `p` | Toggle play / pause |
| `s` | Stop and clear the queue |
| `n` | Next track |
| `o` | Previous track |
| `replay` | Restart the current track |
| `c` | Show the current track (position, duration, modes) |
| `queue` | Show the queue |
| `queue clear` | Clear upcoming tracks |
| `queue rm <N>` | Remove the Nth upcoming track |
| `mode [r\|rq\|s\|off]` | Repeat track / repeat queue / shuffle / off |
| `v [0-100]` | Get or set volume |
| `sf [N]` / `sb [N]` | Seek forward / backward N seconds (default 10) |
| `search <query>` | Search, then type a number to pick (`a` to cancel) |
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
workload, plus CMake, Ninja, and LLVM.

On Windows you must install the [Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
yourself first (the script only checks for them). The helper scripts install the
rest — Rust, CMake, Ninja, and LLVM.

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
