//! Spawning the Deno sidecar that fetches YouTube audio.
//!
//! The script is embedded in the binary rather than installed, so an update
//! ships a new one without a separate install step. `youtube install` only has
//! to provide Deno and warm its npm cache.

use std::path::{Path, PathBuf};

use crate::error::BotError;

/// The sidecar script, compiled in. See `sidecar.ts`.
pub const SCRIPT: &str = include_str!("sidecar.ts");

/// Filename the script is written under, inside the tools directory.
pub const SCRIPT_NAME: &str = "sidecar.ts";

/// Deno's import map and its lockfile, compiled in alongside the script.
///
/// Both are required, and both must sit beside the script: Deno finds its
/// config by walking up from the file it is running. The lockfile is the point
/// of the exercise - without it `youtubei.js@18` resolves to whatever 18.x is
/// newest on each machine, so users drift onto different versions and a bad
/// upstream release breaks everyone at once with nothing to roll back to.
pub const DENO_CONFIG: &str = include_str!("sidecar_deno.json");
pub const DENO_LOCK: &str = include_str!("sidecar_deno.lock");

/// Names the config and lockfile are written under. Deno requires these exact
/// names; only our copies in the source tree carry the `sidecar_` prefix.
pub const DENO_CONFIG_NAME: &str = "deno.json";
pub const DENO_LOCK_NAME: &str = "deno.lock";

/// Longest complaint we will hand back. The server rejects an oversized
/// message outright and the reply then vanishes silently, so cap it here.
pub const MAX_COMPLAINT: usize = 300;

/// Write the embedded script, import map and lockfile to `lib_dir`, replacing
/// any that differ. Returns the path to the script.
///
/// Compares contents rather than timestamps: a binary replaced by tarball or
/// package leaves older files beside a newer exe, and mtimes do not reliably
/// say which is which.
pub fn ensure_script(lib_dir: &Path) -> Result<PathBuf, BotError> {
    for (name, body) in [
        (SCRIPT_NAME, SCRIPT),
        (DENO_CONFIG_NAME, DENO_CONFIG),
        (DENO_LOCK_NAME, DENO_LOCK),
    ] {
        let path = lib_dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(body) {
            std::fs::create_dir_all(lib_dir).map_err(|e| {
                BotError::Playback(format!("could not create {}: {e}", lib_dir.display()))
            })?;
            crate::paths::write_atomic(&path, body.as_bytes()).map_err(|e| {
                BotError::Playback(format!("could not write {}: {e}", path.display()))
            })?;
        }
    }
    Ok(lib_dir.join(SCRIPT_NAME))
}

/// Where Deno keeps the sidecar's npm packages.
///
/// Inside the bot's data root, not the user's shared cache. Exposed so the
/// installer can warm the same directory the bot will later read.
pub fn deno_cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("deno")
}

/// Spawn the sidecar for one track. Its stdout ends with one JSON line
/// describing the audio stream; read it with [`parse_stream_info`].
pub fn spawn(
    script: &Path,
    deno: &Path,
    video_id: &str,
    cookies_file: &str,
) -> Result<std::process::Child, BotError> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(deno);
    cmd.args([
        "run",
        "--allow-net",
        "--allow-read",
        "--allow-write",
        "--allow-env",
    ]);
    // Name the config and lockfile rather than relying on discovery: Deno
    // searches upward from the working directory, not from the script, and the
    // bot's working directory is wherever systemd or the tray happened to
    // start it. Left implicit, the import map is simply not found and every
    // track fails with "Import 'jsdom' not a dependency".
    let dir = script.parent().unwrap_or(script);
    cmd.arg("--config").arg(dir.join(DENO_CONFIG_NAME));
    cmd.arg("--lock").arg(dir.join(DENO_LOCK_NAME));
    // Refuse to silently resolve a different dependency version than the one
    // that was tested. Without this a stale lockfile is a shrug; with it, it
    // is a loud failure we can report.
    cmd.arg("--frozen");
    cmd.arg(script);
    cmd.arg(video_id);
    if !cookies_file.is_empty() {
        cmd.arg(cookies_file);
    }

    // Keep Deno's package cache inside the bot's own data root instead of the
    // user's shared ~/.cache/deno. The runtime may well be one the user
    // installed for their own projects; our 25MB of npm packages should not
    // land in the same cache as theirs, and uninstalling the bot should take
    // them with it. It also removes any dependence on the systemd unit keeping
    // ~/.cache writable.
    cmd.env("DENO_DIR", deno_cache_dir());

    // Same reasoning as the old yt-dlp spawn: deny the child a console so a
    // black window does not appear over the user's desktop while a track plays.
    crate::proc::hide_console_window(&mut cmd);

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| BotError::Playback(format!("sidecar spawn failed: {e}")))
}

/// The sidecar's account of a failure, taken from its stderr.
///
/// Prefers the summary line, then a token failure, then the last thing it
/// said. Empty stderr yields an empty string; callers decide what silence
/// means.
pub fn complaint(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.trim().strip_prefix("[sidecar] "))
        .filter(|l| !l.is_empty())
        .collect();

    let chosen = lines
        .iter()
        .find(|l| l.starts_with("all clients failed"))
        .or_else(|| lines.iter().find(|l| l.starts_with("po-token mint failed")))
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("");

    let mut out = chosen.to_string();
    if out.len() > MAX_COMPLAINT {
        out.truncate(
            (0..=MAX_COMPLAINT)
                .rev()
                .find(|i| out.is_char_boundary(*i))
                .unwrap_or(0),
        );
    }
    out
}

/// What the sidecar found: the client that serves the track, the direct url
/// (carrying its token when one was needed), the file's size in bytes, and
/// whether it took the cookies file's sign-in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    pub client: String,
    pub url: String,
    pub content_length: u64,
    #[serde(default)]
    pub signed_in: bool,
}

/// Whether a Netscape cookies file holds a YouTube sign-in the sidecar can use.
pub fn cookies_have_sign_in(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let fields: Vec<&str> = line.split('\t').collect();
        !line.starts_with('#')
            && fields.len() >= 7
            && ["youtube.com", "google.com"].iter().any(|site| {
                let domain = fields[0].trim_start_matches('.').to_ascii_lowercase();
                domain == *site || domain.ends_with(&format!(".{site}"))
            })
            && matches!(fields[5], "SAPISID" | "__Secure-3PAPISID")
            && !fields[6].is_empty()
    })
}

/// Read the stream info from the sidecar's stdout.
///
/// Only the last non-empty line is the answer, so anything a library prints
/// to stdout before it cannot break the parse. The reason on failure is short
/// on purpose: it can end up in a reply, and never echoes the output itself.
pub fn parse_stream_info(stdout: &str) -> Result<StreamInfo, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or_else(|| "the sidecar printed no stream info".to_string())?;
    let info: StreamInfo = serde_json::from_str(line)
        .map_err(|e| format!("the sidecar's stream info is not valid: {e}"))?;
    if !info.url.starts_with("https://") {
        return Err("the sidecar's stream url is not https".to_string());
    }
    if info.content_length == 0 {
        return Err("the sidecar reported an empty stream".to_string());
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complaint_prefers_the_all_clients_failed_line() {
        let err = complaint(
            "[sidecar] VISIONOS: HTTP 403 at byte 0\n\
             [sidecar] all clients failed; last: IOS: HTTP 403 at byte 0\n",
        );
        assert_eq!(err, "all clients failed; last: IOS: HTTP 403 at byte 0");
    }

    #[test]
    fn complaint_reports_a_token_failure_distinctly() {
        let err = complaint("[sidecar] po-token mint failed: BotGuard returned no challenge\n");
        assert_eq!(err, "po-token mint failed: BotGuard returned no challenge");
    }

    #[test]
    fn complaint_falls_back_to_the_last_sidecar_line() {
        let err = complaint("[sidecar] VISIONOS: playability LOGIN_REQUIRED\n");
        assert_eq!(err, "VISIONOS: playability LOGIN_REQUIRED");
    }

    #[test]
    fn complaint_ignores_noise_without_the_prefix() {
        let err = complaint("Warning: something from deno\n[sidecar] IOS: no m4a audio format\n");
        assert_eq!(err, "IOS: no m4a audio format");
    }

    #[test]
    fn complaint_of_silence_is_empty() {
        assert_eq!(complaint(""), "");
        assert_eq!(complaint("   \n  \n"), "");
    }

    #[test]
    fn complaint_is_capped_so_it_cannot_flood_a_reply() {
        let long = format!("[sidecar] {}\n", "x".repeat(1000));
        assert!(complaint(&long).len() <= MAX_COMPLAINT);
    }

    #[test]
    fn script_config_and_lock_are_all_written_when_absent() {
        let dir = std::env::temp_dir().join(format!("sc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = ensure_script(&dir).expect("write");
        assert!(path.is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SCRIPT);
        // Deno resolves its config by walking up from the script, so all three
        // must land together or the pinning is silently inert.
        assert_eq!(
            std::fs::read_to_string(dir.join(DENO_CONFIG_NAME)).unwrap(),
            DENO_CONFIG
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(DENO_LOCK_NAME)).unwrap(),
            DENO_LOCK
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_files_are_all_rewritten() {
        let dir = std::env::temp_dir().join(format!("sc_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SCRIPT_NAME), "// an older version").unwrap();
        std::fs::write(dir.join(DENO_LOCK_NAME), "{}").unwrap();
        ensure_script(&dir).expect("rewrite");
        assert_eq!(
            std::fs::read_to_string(dir.join(SCRIPT_NAME)).unwrap(),
            SCRIPT
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(DENO_LOCK_NAME)).unwrap(),
            DENO_LOCK,
            "a stale lockfile must be replaced, or the pin is the old one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_deno_cache_lives_under_the_bots_own_root() {
        let cache = deno_cache_dir();
        assert!(
            cache.starts_with(crate::paths::cache_dir()),
            "the sidecar's packages must not land in the user's shared cache: {}",
            cache.display()
        );
        assert!(cache.ends_with("deno"));
    }

    #[test]
    fn the_lockfile_pins_the_versions_we_tested() {
        for pinned in ["youtubei.js@18.0.0", "bgutils-js@3.2.0", "jsdom@25.0.1"] {
            assert!(
                DENO_LOCK.contains(pinned),
                "lockfile no longer pins {pinned}; regenerate it deliberately, \
                 do not let it drift"
            );
        }
    }

    const GOOD: &str =
        r#"{"client":"VISIONOS","url":"https://rr1---sn.googlevideo.com/videoplayback?id=1","contentLength":4175364}"#;

    #[test]
    fn stream_info_is_read_from_a_good_line() {
        let info = parse_stream_info(GOOD).expect("parse");
        assert_eq!(
            info,
            StreamInfo {
                client: "VISIONOS".to_string(),
                url: "https://rr1---sn.googlevideo.com/videoplayback?id=1".to_string(),
                content_length: 4175364,
                signed_in: false,
            }
        );
    }

    #[test]
    fn stream_info_says_when_it_was_signed_in() {
        let out = GOOD.replace("}", r#","signedIn":true}"#);
        assert!(parse_stream_info(&out).unwrap().signed_in);
    }

    fn cookie_row(domain: &str, name: &str, value: &str) -> String {
        format!("{domain}\tTRUE\t/\tTRUE\t0\t{name}\t{value}")
    }

    #[test]
    fn a_signed_in_export_is_recognised() {
        assert!(cookies_have_sign_in(&cookie_row(".youtube.com", "SAPISID", "x")));
        let secure = format!("# Netscape HTTP Cookie File\r\n#HttpOnly_{}\r\n", cookie_row(".google.com", "__Secure-3PAPISID", "x"));
        assert!(cookies_have_sign_in(&secure));
    }

    #[test]
    fn a_signed_out_or_foreign_export_is_not_a_sign_in() {
        assert!(!cookies_have_sign_in(&cookie_row(".youtube.com", "YSC", "x")));
        assert!(!cookies_have_sign_in(&cookie_row("notyoutube.com", "SAPISID", "x")));
        assert!(!cookies_have_sign_in(&format!("# {}", cookie_row(".youtube.com", "SAPISID", "x"))));
        assert!(!cookies_have_sign_in(&cookie_row(".youtube.com", "SAPISID", "")));
        assert!(!cookies_have_sign_in(""));
    }

    #[test]
    fn stream_info_ignores_trailing_whitespace_and_blank_lines() {
        let out = format!("  {GOOD}  \r\n\n   \n");
        assert_eq!(parse_stream_info(&out).unwrap().content_length, 4175364);
    }

    #[test]
    fn stream_info_is_the_last_line_whatever_came_before() {
        // A library printing to stdout ahead of the answer must not break it.
        let out = format!("[YOUTUBEJS][Player]: some warning\nnot json either\n{GOOD}\n");
        assert_eq!(parse_stream_info(&out).unwrap().client, "VISIONOS");
    }

    #[test]
    fn stream_info_with_a_non_https_url_is_refused() {
        let out = GOOD.replace("https://", "http://");
        assert!(parse_stream_info(&out).unwrap_err().contains("https"));
    }

    #[test]
    fn stream_info_with_no_length_is_refused() {
        let out = GOOD.replace("4175364", "0");
        assert!(parse_stream_info(&out).unwrap_err().contains("empty"));
    }

    #[test]
    fn stream_info_that_is_not_json_is_refused() {
        let err = parse_stream_info("all clients failed\n").unwrap_err();
        assert!(err.contains("not valid"), "{err}");
        // A missing field is as unusable as garbage.
        assert!(parse_stream_info(r#"{"client":"VISIONOS","url":"https://x"}"#).is_err());
    }

    #[test]
    fn no_stream_info_at_all_is_refused() {
        assert!(parse_stream_info("").unwrap_err().contains("no stream info"));
        assert!(parse_stream_info(" \n\n ").unwrap_err().contains("no stream info"));
    }
}
