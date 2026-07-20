//! "Update available" dialog + download progress dialog (Windows tray).
//!
//! Mirrors progress.rs: a worker thread runs the async update on its own tokio
//! runtime and reports through a crossbeam channel that a wx Timer drains on the
//! GUI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wxdragon::prelude::*;
use wxdragon::timer::Timer;

use crate::update::{download_and_apply, UpdateInfo};

enum Msg {
    Progress(u64),
    Done(Result<(), String>),
}

/// Shared single-shot dismiss action for the update dialog (see
/// `show_update_available`): whichever handler fires first `take()`s and runs it.
type DismissAction = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

thread_local! {
    /// GUI-thread hook run right before a successful update relaunches the
    /// process. The tray registers "stop all bots (bounded wait)" here so the
    /// running bots disconnect cleanly from the TeamTalk server and persist
    /// their config instead of being killed mid-flight by process::exit.
    /// A thread_local (not a parameter) because the dialog can be opened from
    /// a Send closure (`call_after` from the update-check worker) that cannot
    /// capture the GUI-only BotManager Rc.
    static PREPARE_RELAUNCH: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Register the GUI-thread cleanup hook that runs before a successful update
/// exits and relaunches. Call once at tray startup.
pub fn set_prepare_relaunch(hook: impl Fn() + 'static) {
    PREPARE_RELAUNCH.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

/// Modal-style "update available" window: read-only changelog + Download / Later.
///
/// `on_dismiss` runs if the user picks Later or closes the window (X) — used at
/// startup to start the bots once the user declines the update. It does NOT run
/// when the user picks Download, since a successful update relaunches fresh.
/// Pass a no-op closure for the manual (menu-triggered) check.
pub fn show_update_available(info: UpdateInfo, on_dismiss: impl FnOnce() + 'static) {
    let frame = Frame::builder()
        .with_title(&format!("Update available - {}", info.tag))
        .with_size(Size::new(520, 420))
        .build();
    let panel = Panel::builder(&frame).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let heading = StaticText::builder(&panel)
        .with_label(&format!(
            "A new version ({}) is available. You have v{}.",
            info.tag,
            env!("CARGO_PKG_VERSION")
        ))
        .build();

    let notes = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .build();
    notes.set_value(&crate::update::plain_changelog(&info.changelog));
    notes.set_name("Release notes");

    let btn_row = BoxSizer::builder(Orientation::Horizontal).build();
    let download_btn = Button::builder(&panel).with_label("Download").build();
    let later_btn = Button::builder(&panel).with_label("Later").build();
    btn_row.add(&download_btn, 0, SizerFlag::All, 5);
    btn_row.add(&later_btn, 0, SizerFlag::All, 5);

    sizer.add(&heading, 0, SizerFlag::All, 8);
    sizer.add(&notes, 1, SizerFlag::Expand | SizerFlag::All, 8);
    sizer.add_sizer(&btn_row, 0, SizerFlag::AlignRight | SizerFlag::All, 4);
    panel.set_sizer(sizer, true);

    // Single-shot dismiss action, shared across the three ways the dialog can
    // end. Whoever fires first `take()`s it, so it runs at most once.
    let dismiss: DismissAction = Rc::new(RefCell::new(Some(Box::new(on_dismiss))));

    let dismiss_later = dismiss.clone();
    later_btn.on_click(move |_| {
        if let Some(cb) = dismiss_later.borrow_mut().take() {
            cb();
        }
        frame.close(true);
    });

    let dismiss_dl = dismiss.clone();
    download_btn.on_click(move |_| {
        // Hand the dismiss action to the downloader: it runs (start bots) if the
        // download is cancelled or fails, but not on success (which relaunches).
        let on_abort = dismiss_dl
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Box::new(|| {}));
        frame.close(true);
        run_download(info.clone(), on_abort);
    });

    frame.on_destroy(move |evt| {
        // Closed via the window's X without choosing = same as Later.
        if let Some(cb) = dismiss.borrow_mut().take() {
            cb();
        }
        evt.skip(true);
    });

    frame.centre();
    frame.show(true);
}

/// Download progress window: a gauge + Cancel. Runs download_and_apply on a
/// worker thread; on success, relaunches the (replaced) exe and exits. If the
/// download is cancelled or fails, `on_abort` runs (start bots on the current
/// version) so a failed update doesn't leave the app idle.
fn run_download(info: UpdateInfo, on_abort: Box<dyn FnOnce()>) {
    let on_abort = RefCell::new(Some(on_abort));
    let frame = Frame::builder()
        .with_title("Downloading update")
        .with_size(Size::new(420, 150))
        .build();
    let panel = Panel::builder(&frame).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let label = StaticText::builder(&panel)
        .with_label("Downloading...")
        .build();
    let gauge = Gauge::builder(&panel).with_range(100).build();
    gauge.set_name("Download progress");
    let cancel_btn = Button::builder(&panel).with_label("Cancel").build();

    sizer.add(&label, 0, SizerFlag::All, 8);
    sizer.add(&gauge, 0, SizerFlag::Expand | SizerFlag::All, 8);
    sizer.add(&cancel_btn, 0, SizerFlag::AlignRight | SizerFlag::All, 6);
    panel.set_sizer(sizer, true);

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();

    {
        let cancel = cancel.clone();
        let info = info.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Msg::Done(Err(format!("tokio runtime: {e}"))));
                    return;
                }
            };
            let tx_progress = tx.clone();
            let progress = move |done: u64, total: Option<u64>| {
                let pct = match total {
                    Some(t) if t > 0 => (done * 100 / t).min(100),
                    _ => 0,
                };
                let _ = tx_progress.send(Msg::Progress(pct));
            };
            let result = rt
                .block_on(download_and_apply(&info, &progress, &cancel))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Done(result));
        });
    }

    let cancel_for_btn = cancel.clone();
    cancel_btn.on_click(move |_| {
        cancel_for_btn.store(true, Ordering::Relaxed);
    });

    let cancel_destroy = cancel.clone();
    let timer = Timer::new(&frame);
    timer.on_tick(move |_| {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Progress(pct) => gauge.set_value(pct as i32),
                Msg::Done(Ok(())) => {
                    frame.close(true);
                    relaunch_and_exit();
                }
                Msg::Done(Err(e)) => {
                    use MessageDialogStyle as MDS;
                    if e != "Update cancelled" {
                        MessageDialog::builder(&frame, &e, "Update failed")
                            .with_style(MDS::OK | MDS::IconError)
                            .build()
                            .show_modal();
                    }
                    // Closing funnels through on_destroy, which starts bots on the
                    // current version (the update didn't happen).
                    frame.close(true);
                }
            }
        }
    });
    timer.start(100, false);

    frame.on_destroy(move |evt| {
        timer.stop();
        // If the window is closed mid-download, cancel it and fall back to
        // starting bots on the current version (unless already handled).
        cancel_destroy.store(true, Ordering::Relaxed);
        if let Some(cb) = on_abort.borrow_mut().take() {
            cb();
        }
        evt.skip(true);
    });

    frame.centre();
    frame.show(true);
}

/// A small info/error box with its own throwaway parent frame, so it can be
/// shown from a `call_after` closure (which may not capture an existing Frame —
/// wx widgets are not `Send`). Runs on the GUI thread.
fn info_box(title: &str, msg: &str, style: MessageDialogStyle) {
    let parent = Frame::builder().with_size(Size::new(1, 1)).build();
    MessageDialog::builder(&parent, msg, title)
        .with_style(style)
        .build()
        .show_modal();
    parent.destroy();
}

/// "You're up to date" box for the manual Check-for-updates path.
pub fn show_up_to_date() {
    use MessageDialogStyle as MDS;
    info_box(
        "Check for updates",
        &format!("You're up to date (v{}).", env!("CARGO_PKG_VERSION")),
        MDS::OK | MDS::IconInformation,
    );
}

/// Error box for a failed manual update check.
pub fn show_check_error(msg: &str) {
    use MessageDialogStyle as MDS;
    info_box("Check for updates", msg, MDS::OK | MDS::IconError);
}

/// Relaunch the (now-replaced) exe and exit the current process. Runs the
/// registered prepare hook first so running bots shut down cleanly —
/// process::exit skips the tray frame's on_destroy cleanup.
fn relaunch_and_exit() {
    PREPARE_RELAUNCH.with(|h| {
        if let Some(hook) = h.borrow().as_ref() {
            hook();
        }
    });
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}
