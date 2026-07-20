//! Live-log progress window for long-running background tasks.
//!
//! Runs a worker on a background thread and streams its progress lines into a
//! read-only text box. The Close button stays disabled until the worker ends.

use wxdragon::prelude::*;
use wxdragon::timer::Timer;

use crate::youtube::setup;

enum Msg {
    Line(String),
    Done(Result<(), String>),
}

/// Open a modeless progress window that runs `worker` on a background thread.
/// The worker reports progress through the passed callback; each line is
/// appended to the log as it arrives. `on_done` runs on the GUI thread once the
/// worker finishes, with `true` on success — use it to defer follow-up work
/// (e.g. starting a bot) until the task actually completes.
pub fn run_progress_dialog<F, D>(title: &str, worker: F, on_done: D)
where
    F: FnOnce(&dyn Fn(&str)) -> Result<(), String> + Send + 'static,
    D: FnOnce(bool) + 'static,
{
    let frame = Frame::builder()
        .with_title(title)
        .with_size(Size::new(520, 340))
        .build();
    let panel = Panel::builder(&frame).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let log = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .build();
    let close_btn = Button::builder(&panel).with_label("Close").build();
    close_btn.enable(false);

    sizer.add(&log, 1, SizerFlag::Expand | SizerFlag::All, 8);
    sizer.add(&close_btn, 0, SizerFlag::AlignRight | SizerFlag::Right | SizerFlag::Bottom, 8);
    panel.set_sizer(sizer, true);

    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    std::thread::spawn(move || {
        let progress = |line: &str| {
            let _ = tx.send(Msg::Line(line.to_string()));
        };
        let result = worker(&progress);
        let _ = tx.send(Msg::Done(result));
    });

    let timer = Timer::new(&frame);
    let log_tick = log;
    let btn_tick = close_btn;
    let on_done = std::cell::RefCell::new(Some(on_done));
    timer.on_tick(move |_| {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Line(line) => log_tick.append_text(&format!("{line}\n")),
                Msg::Done(result) => {
                    match &result {
                        Ok(()) => log_tick.append_text("\nDone.\n"),
                        Err(e) => log_tick.append_text(&format!("\nFailed: {e}\n")),
                    }
                    btn_tick.enable(true);
                    if let Some(cb) = on_done.borrow_mut().take() {
                        cb(result.is_ok());
                    }
                }
            }
        }
    });
    timer.start(150, false);

    close_btn.on_click(move |_| {
        frame.close(true);
    });

    frame.on_destroy(move |evt| {
        timer.stop();
        evt.skip(true);
    });

    frame.show(true);
    frame.centre();
}

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
    update_cmd.arg("--update").creation_flags(CREATE_NO_WINDOW);
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
