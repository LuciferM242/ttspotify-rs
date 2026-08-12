//! Tray tooltip text.
//!
//! Win32's `NOTIFYICONDATA.szTip` is a fixed `[u16; 128]` buffer, so the text
//! must fit in 127 UTF-16 code units plus a terminator. Overlong text is cut
//! here rather than left to the shell, which would truncate mid-character.

use crate::gui::manager::BotStatus;

/// Room in `szTip`, in UTF-16 code units, excluding the NUL terminator.
pub const MAX_TIP_UTF16: usize = 127;

/// Shorten `text` to at most `max` UTF-16 code units, marking the cut with an
/// ellipsis. Never splits a surrogate pair: a character that would straddle the
/// limit is dropped whole.
pub fn truncate_utf16(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Leave room for the ellipsis, which is itself one code unit.
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if used + width > budget {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('\u{2026}');
    out
}

/// Tray tooltip describing every configured bot.
pub fn build_tooltip(statuses: &[(String, BotStatus)]) -> String {
    let text = compose_tooltip(statuses);
    truncate_utf16(&text, MAX_TIP_UTF16)
}

/// The untruncated tooltip text.
fn compose_tooltip(statuses: &[(String, BotStatus)]) -> String {
    if statuses.is_empty() {
        return "TT Spotify - no bots configured".to_string();
    }

    // One bot: name and status read directly, no counting needed.
    if statuses.len() == 1 {
        let (name, status) = &statuses[0];
        return format!("TT Spotify - {name}: {status}");
    }

    let mut connected = 0u32;
    let mut playing = 0u32;
    let mut failed = 0u32;
    let mut stopped = 0u32;
    let mut starting = 0u32;

    for (_, status) in statuses {
        match status {
            BotStatus::Connected => connected += 1,
            BotStatus::Playing(_) => {
                connected += 1;
                playing += 1;
            }
            BotStatus::Error(_) | BotStatus::Disconnected => failed += 1,
            BotStatus::Stopped => stopped += 1,
            BotStatus::Starting | BotStatus::Connecting | BotStatus::Authenticating => {
                starting += 1
            }
        }
    }

    let mut parts = Vec::new();
    if connected > 0 {
        parts.push(format!("{connected} connected"));
    }
    if playing > 0 {
        parts.push(format!("{playing} playing"));
    }
    if starting > 0 {
        parts.push(format!("{starting} starting"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if stopped > 0 {
        parts.push(format!("{stopped} stopped"));
    }

    // No empty-parts fallback: the match above is exhaustive and every variant
    // increments a counter, so with bots present `parts` always has something.
    // wx carried a "{n} bots" branch here that could never run.
    format!("TT Spotify - {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bots(items: &[(&str, BotStatus)]) -> Vec<(String, BotStatus)> {
        items
            .iter()
            .map(|(n, s)| (n.to_string(), s.clone()))
            .collect()
    }

    #[test]
    fn no_bots_says_so() {
        assert_eq!(build_tooltip(&[]), "TT Spotify - no bots configured");
    }

    #[test]
    fn a_single_bot_shows_its_name_and_status() {
        let s = bots(&[("myserver", BotStatus::Connected)]);
        assert_eq!(build_tooltip(&s), "TT Spotify - myserver: Connected, Idle");
    }

    #[test]
    fn several_bots_are_summarised_by_count() {
        let s = bots(&[
            ("a", BotStatus::Connected),
            ("b", BotStatus::Playing("song".into())),
            ("c", BotStatus::Stopped),
        ]);
        // The playing bot counts as connected too, so 2 connected of 3.
        assert_eq!(build_tooltip(&s), "TT Spotify - 2 connected, 1 playing, 1 stopped");
    }

    #[test]
    fn several_servers_are_summarised_in_a_fixed_order() {
        // The multi-server line the tray shows most of the time. Order is
        // fixed so the text does not reshuffle as bots change state, which
        // would make it hard to read at a glance.
        let s = bots(&[
            ("a", BotStatus::Stopped),
            ("b", BotStatus::Error("boom".into())),
            ("c", BotStatus::Playing("song".into())),
            ("d", BotStatus::Connecting),
            ("e", BotStatus::Connected),
        ]);
        assert_eq!(
            build_tooltip(&s),
            "TT Spotify - 2 connected, 1 playing, 1 starting, 1 failed, 1 stopped"
        );
    }

    #[test]
    fn every_status_is_counted_somewhere() {
        // Guards the summary against a new BotStatus variant being counted
        // nowhere and silently vanishing from the total.
        let all = bots(&[
            ("a", BotStatus::Stopped),
            ("b", BotStatus::Starting),
            ("c", BotStatus::Connecting),
            ("d", BotStatus::Authenticating),
            ("e", BotStatus::Connected),
            ("f", BotStatus::Playing("x".into())),
            ("g", BotStatus::Disconnected),
            ("h", BotStatus::Error("x".into())),
        ]);
        let tip = build_tooltip(&all);
        let counted: u32 = tip
            .trim_start_matches("TT Spotify - ")
            .split(", ")
            .filter_map(|part| part.split_whitespace().next())
            .filter_map(|n| n.parse::<u32>().ok())
            .sum();
        // Playing also counts as connected, so one bot is counted twice.
        assert_eq!(counted, all.len() as u32 + 1, "got: {tip}");
    }

    #[test]
    fn a_disconnected_server_reads_as_failed_not_stopped() {
        // They mean different things: stopped is the user's doing, failed is
        // not, and conflating them hides a server that needs attention.
        let s = bots(&[
            ("a", BotStatus::Disconnected),
            ("b", BotStatus::Stopped),
        ]);
        assert_eq!(build_tooltip(&s), "TT Spotify - 1 failed, 1 stopped");
    }

    #[test]
    fn a_long_track_name_is_cut_to_fit_the_win32_buffer() {
        // szTip is [u16; 128]; a Spotify title plus a remix suffix passes that
        // easily, and an overlong string is silently dropped by the shell.
        let long = "x".repeat(400);
        let s = bots(&[("srv", BotStatus::Playing(long))]);
        let tip = build_tooltip(&s);
        assert!(
            tip.encode_utf16().count() <= MAX_TIP_UTF16,
            "tooltip was {} UTF-16 units",
            tip.encode_utf16().count()
        );
        assert!(tip.ends_with('\u{2026}'), "a cut tooltip should show it was cut");
        assert!(tip.starts_with("TT Spotify - srv:"), "got: {tip}");
    }

    #[test]
    fn text_that_already_fits_is_left_alone() {
        let tip = truncate_utf16("short", MAX_TIP_UTF16);
        assert_eq!(tip, "short");
    }

    #[test]
    fn truncation_never_splits_a_surrogate_pair() {
        // Track titles carry emoji and rare CJK, which are two UTF-16 units
        // each. Cutting one in half writes a lone surrogate into szTip.
        let text = "\u{1F600}".repeat(20); // 40 UTF-16 units
        let cut = truncate_utf16(&text, 11);
        assert!(cut.encode_utf16().count() <= 11);
        // Decoding back must not fail, which it would on a lone surrogate.
        let units: Vec<u16> = cut.encode_utf16().collect();
        assert!(
            String::from_utf16(&units).is_ok(),
            "produced an unpaired surrogate"
        );
        // 11 units = ellipsis (1) + 5 whole emoji (10).
        assert_eq!(cut, format!("{}\u{2026}", "\u{1F600}".repeat(5)));
    }

    #[test]
    fn a_zero_budget_yields_nothing_rather_than_panicking() {
        assert_eq!(truncate_utf16("abc", 0), "");
    }
}
