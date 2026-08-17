//! Installing and updating the YouTube tools.
//!
//! No window here: this is what the progress dialog runs on its worker thread.

use crate::youtube::setup;

/// Download and install the YouTube tools. Reports progress via `progress`.
pub fn youtube_install(progress: &dyn Fn(&str)) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let paths = setup::resolve_paths().map_err(|e| e.to_string())?;
    if setup::is_installed(&paths) {
        progress("YouTube tools already installed.");
        return Ok(());
    }
    rt.block_on(setup::install(&paths, |l| progress(l)))
        .map_err(|e| e.to_string())
}

/// Self-update yt-dlp, then re-download bgutil-pot if a newer release exists.
pub fn youtube_update(progress: &dyn Fn(&str)) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let paths = setup::resolve_paths().map_err(|e| e.to_string())?;
    if !setup::is_installed(&paths) {
        return Err("YouTube tools aren't installed yet. Install them first.".to_string());
    }

    progress("Updating yt-dlp...");
    // Snapshot the version before updating so we can report from -> to. Probing
    // --version (not parsing --update's prose) keeps this robust across yt-dlp
    // release-message changes.
    let before = setup::installed_tool_versions().yt_dlp;
    // Suppress the console-window flash: this GUI process has no console, so a
    // bare yt-dlp spawn pops a command window (same reason as spawn_ytdlp).
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut update_cmd = std::process::Command::new(&paths.yt_dlp);
    // The socket timeout bounds a dead network: this runs under the modal
    // progress dialog, and yt-dlp waiting on a half-open socket held the
    // whole tray hostage with no way to cancel.
    update_cmd
        .args(["--update", "--socket-timeout", "30"])
        .creation_flags(CREATE_NO_WINDOW);
    match update_cmd.status() {
        Ok(s) if s.success() => {
            let after = setup::installed_tool_versions().yt_dlp;
            progress(&ytdlp_update_summary(before, after));
        }
        Ok(s) => progress(&format!("yt-dlp --update exited with {s}")),
        Err(e) => progress(&format!("Could not run yt-dlp --update: {e}")),
    }

    progress("Checking bgutil-pot for updates...");
    let installed = setup::installed_bgutil_version(&paths);
    let latest = rt
        .block_on(setup::latest_bgutil_version())
        .map_err(|e| e.to_string())?;
    if latest == installed {
        progress(&format!("bgutil-pot is up to date ({installed})."));
    } else {
        progress(&format!("Updating bgutil-pot {installed} -> {latest}..."));
        rt.block_on(setup::install_bgutil_version(&paths, &latest, |l| progress(l)))
            .map_err(|e| e.to_string())?;
    }

    // The JavaScript runtime yt-dlp needs for YouTube's player challenges.
    progress("Checking the JavaScript runtime (Deno)...");
    if let Err(e) = rt.block_on(setup::update_js_runtime(&paths, |l| progress(l))) {
        // Not fatal: YouTube keeps working with fewer formats available.
        progress(&format!("Could not update Deno: {e}"));
    }
    Ok(())
}

/// Build the user-facing line describing a yt-dlp `--update` outcome from the
/// versions probed before and after. `None` means the version couldn't be read.
fn ytdlp_update_summary(before: Option<String>, after: Option<String>) -> String {
    match (before, after) {
        (Some(b), Some(a)) if b != a => format!("yt-dlp updated: {b} -> {a}"),
        (Some(_), Some(a)) => format!("yt-dlp already up to date ({a})"),
        (None, Some(a)) => format!("yt-dlp is now at version {a}"),
        (_, None) => "yt-dlp update check complete.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::ytdlp_update_summary;

    #[test]
    fn reports_from_to_when_version_changes() {
        let s = ytdlp_update_summary(Some("2025.11.01".into()), Some("2025.12.08".into()));
        assert_eq!(s, "yt-dlp updated: 2025.11.01 -> 2025.12.08");
    }

    #[test]
    fn reports_up_to_date_when_unchanged() {
        let s = ytdlp_update_summary(Some("2025.12.08".into()), Some("2025.12.08".into()));
        assert_eq!(s, "yt-dlp already up to date (2025.12.08)");
    }

    #[test]
    fn reports_current_version_when_before_unknown() {
        let s = ytdlp_update_summary(None, Some("2025.12.08".into()));
        assert_eq!(s, "yt-dlp is now at version 2025.12.08");
    }

    #[test]
    fn falls_back_when_after_unknown() {
        assert_eq!(
            ytdlp_update_summary(Some("2025.11.01".into()), None),
            "yt-dlp update check complete."
        );
        assert_eq!(ytdlp_update_summary(None, None), "yt-dlp update check complete.");
    }
}
