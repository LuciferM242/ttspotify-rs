//! What to tell the user to do next, in the words of the platform they are on.
//!
//! These sentences are printed from code shared by both builds, and the two
//! platforms are driven completely differently: Linux has commands, Windows has
//! a tray menu and no CLI to speak of. Written inline, they drifted — the CLI
//! moved from flags to subcommands and a dozen "run this to fix it" strings
//! went on naming flags that no longer parsed, which is worse than saying
//! nothing. Keeping them here means one place to change, and a test that
//! notices when a suggestion stops being real.

/// How to create a bot.
pub fn create_bot() -> String {
    #[cfg(windows)]
    {
        "use Add Server in the tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} add myserver", crate::paths::program_name())
    }
}

/// How to sign in to Spotify.
pub fn sign_in_spotify() -> String {
    #[cfg(windows)]
    {
        "use Spotify sign-in in the tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} auth", crate::paths::program_name())
    }
}

/// How to install the YouTube tools.
pub fn install_youtube_tools() -> String {
    #[cfg(windows)]
    {
        "use Install YouTube tools in the tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} youtube install", crate::paths::program_name())
    }
}

/// How to update the YouTube tools.
pub fn update_youtube_tools() -> String {
    #[cfg(windows)]
    {
        "use Update tools in the tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} youtube update", crate::paths::program_name())
    }
}

/// How to install the systemd service. Windows has no equivalent — the tray
/// keeps bots running — so it says what is true there instead.
pub fn install_service() -> String {
    #[cfg(windows)]
    {
        "the tray keeps bots running; there is no service to install".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} service install", crate::paths::program_name())
    }
}

/// How to put the binary on PATH.
pub fn install_binary() -> String {
    #[cfg(windows)]
    {
        "keep the extracted folder wherever you want it".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} install", crate::paths::program_name())
    }
}

/// How to run one bot in this terminal.
pub fn run_bot(name: &str) -> String {
    #[cfg(windows)]
    {
        let _ = name;
        "use Start in that bot's tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} run {name}", crate::paths::program_name())
    }
}

/// How to restart one bot.
pub fn restart_bot(name: &str) -> String {
    #[cfg(windows)]
    {
        let _ = name;
        "use Restart in that bot's tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} restart {name}", crate::paths::program_name())
    }
}

/// How to change a bot's settings.
pub fn edit_bot(name: &str) -> String {
    #[cfg(windows)]
    {
        let _ = name;
        "use Edit Config in that bot's tray menu".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run: {} edit {name}", crate::paths::program_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every suggestion, so the checks below cannot miss one that was added
    /// later.
    fn all() -> Vec<(&'static str, String)> {
        vec![
            ("create_bot", create_bot()),
            ("sign_in_spotify", sign_in_spotify()),
            ("install_youtube_tools", install_youtube_tools()),
            ("update_youtube_tools", update_youtube_tools()),
            ("install_service", install_service()),
            ("install_binary", install_binary()),
            ("restart_bot", restart_bot("home")),
            ("edit_bot", edit_bot("home")),
            ("run_bot", run_bot("home")),
        ]
    }

    #[test]
    fn no_suggestion_names_a_command_line_flag() {
        // The bug this file exists to prevent: the CLI moved from flags to
        // subcommands and every "fix it with --this" string kept naming a flag
        // that now fails to parse. Nothing here may mention one.
        for (name, hint) in all() {
            assert!(
                !hint.contains("--"),
                "{name} suggests a flag, which this build does not accept: {hint}"
            );
        }
    }

    #[test]
    fn every_suggestion_says_something() {
        for (name, hint) in all() {
            assert!(!hint.trim().is_empty(), "{name} is empty");
            // A hint that names no command and no menu item tells nobody
            // anything.
            assert!(
                hint.contains("run:") || hint.contains("tray") || hint.contains("folder"),
                "{name} gives no way to act on it: {hint}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn commands_name_real_subcommands() {
        // Each of these is a subcommand the CLI actually defines; a typo here
        // would send someone to a command that does not exist.
        let expected = [
            ("create_bot", " add "),
            ("sign_in_spotify", " auth"),
            ("install_youtube_tools", " youtube install"),
            ("update_youtube_tools", " youtube update"),
            ("install_service", " service install"),
            ("install_binary", " install"),
            ("restart_bot", " restart home"),
            ("edit_bot", " edit home"),
            ("run_bot", " run home"),
        ];
        for (name, needle) in expected {
            let hint = all().into_iter().find(|(n, _)| *n == name).unwrap().1;
            assert!(hint.contains(needle), "{name} should mention `{needle}`: {hint}");
        }
    }
}
