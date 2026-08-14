//! Update available, and the download that follows.
//!
//! Two dialogs. The first shows the release notes and offers Download or Later.
//! The second runs the download on a worker thread with a progress bar and a
//! working Cancel, then relaunches the replaced executable.
//!
//! The dismiss action matters more than it looks: at startup the bots are held
//! back until the user has answered, so every way out of the first dialog -
//! Later, Escape, the title bar's X, a cancelled download, a failed download -
//! has to release them. Only a successful update does not, because that
//! relaunches into a fresh process.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use winsafe::prelude::*;
use winsafe::{self as w, co, gui};

use crate::update::{download_and_apply, UpdateInfo};

const IDD_UPDATE: u16 = 300;
const IDC_UPD_HEADING: u16 = 301;
const IDC_UPD_NOTES: u16 = 302;
const IDC_UPD_DOWNLOAD: u16 = 303;
const IDC_UPD_LATER: u16 = 304;

const IDD_DOWNLOAD: u16 = 400;
const IDC_DL_LABEL: u16 = 401;
const IDC_DL_BAR: u16 = 402;
const IDC_DL_CANCEL: u16 = 403;

const TIMER_DRAIN: usize = 1;
const TIMER_DRAIN_MS: u32 = 100;

/// Runs before the process relaunches into the new version, so bots leave the
/// server tidily. Set by the tray.
type PrepareRelaunch = Box<dyn Fn()>;
thread_local! {
    static PREPARE_RELAUNCH: RefCell<Option<PrepareRelaunch>> = const { RefCell::new(None) };
}

/// Register what to run before relaunching.
pub fn set_prepare_relaunch(hook: impl Fn() + 'static) {
    PREPARE_RELAUNCH.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

enum Msg {
    Progress(u64),
    Done(Result<(), String>),
}

/// Offer an available update.
///
/// Returns `true` when the caller should carry on as normal (the user declined,
/// or the download did not happen). Returns `false` only when the process is
/// about to relaunch, in which case the caller must not start anything.
#[must_use]
pub fn show_update_available(parent: &(impl GuiParent + 'static), info: UpdateInfo) -> bool {
    let dlg = gui::WindowModal::new_dlg(IDD_UPDATE);
    let heading = gui::Label::new_dlg(&dlg, IDC_UPD_HEADING, (gui::Horz::Resize, gui::Vert::None));
    let notes = gui::Edit::new_dlg(&dlg, IDC_UPD_NOTES, (gui::Horz::Resize, gui::Vert::Resize));
    let wants_download = Rc::new(Cell::new(false));

    {
        let info = info.clone();
        let heading = heading.clone();
        let notes = notes.clone();
        dlg.on().wm_init_dialog(move |_| {
            let _ = heading.hwnd().SetWindowText(&format!(
                "A new version ({}) is available. You have v{}.",
                info.tag,
                env!("CARGO_PKG_VERSION")
            ));
            // Edit controls need CRLF; the changelog arrives with bare newlines.
            let body = crate::update::plain_changelog(&info.changelog).replace('\n', "\r\n");
            let _ = notes.set_text(&body);
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let wants = wants_download.clone();
        dlg.on().wm_command_acc_menu(IDC_UPD_DOWNLOAD, move || {
            wants.set(true);
            let _ = dlg2.hwnd().EndDialog(0);
            Ok(())
        });
    }

    {
        let dlg2 = dlg.clone();
        dlg.on().wm_command_acc_menu(IDC_UPD_LATER, move || {
            let _ = dlg2.hwnd().EndDialog(0);
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Update dialog failed: {e}");
        return true;
    }

    if !wants_download.get() {
        // Later, Escape or the X: nothing happens, the caller carries on.
        return true;
    }
    // The downloader relaunches on success, so reaching the end of it means
    // the update did not happen.
    run_download(parent, info)
}

/// Download and apply, with a progress bar and a Cancel that really cancels.
///
/// Returns `true` if the caller should carry on (cancelled or failed). Does not
/// return at all on success: the process relaunches.
#[must_use]
fn run_download(parent: &(impl GuiParent + 'static), info: UpdateInfo) -> bool {
    let dlg = gui::WindowModal::new_dlg(IDD_DOWNLOAD);
    let label = gui::Label::new_dlg(&dlg, IDC_DL_LABEL, (gui::Horz::Resize, gui::Vert::None));
    let bar = gui::ProgressBar::new_dlg(&dlg, IDC_DL_BAR, (gui::Horz::Resize, gui::Vert::None));

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let carry_on = Rc::new(Cell::new(true));
    let started = RefCell::new(Some((info, tx.clone())));

    {
        let dlg2 = dlg.clone();
        let bar = bar.clone();
        let cancel = cancel.clone();
        dlg.on().wm_init_dialog(move |_| {
            bar.set_range(0, 100);
            if let Some((info, tx)) = started.borrow_mut().take() {
                let cancel = cancel.clone();
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
            let _ = dlg2.hwnd().SetTimer(TIMER_DRAIN, TIMER_DRAIN_MS, None);
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let bar = bar.clone();
        let label = label.clone();
        let carry_on = carry_on.clone();
        dlg.on().wm_timer(TIMER_DRAIN, move || {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Progress(pct) => {
                        bar.set_position(pct as u32);
                        let _ = label.hwnd().SetWindowText(&format!("Downloading... {pct}%"));
                    }
                    Msg::Done(Ok(())) => {
                        let _ = dlg2.hwnd().KillTimer(TIMER_DRAIN);
                        carry_on.set(false);
                        let _ = dlg2.hwnd().EndDialog(0);
                        relaunch_and_exit();
                    }
                    Msg::Done(Err(e)) => {
                        let _ = dlg2.hwnd().KillTimer(TIMER_DRAIN);
                        // A cancel is the user's own doing, so it needs no
                        // report; anything else does.
                        if e != "Update cancelled" {
                            let _ = dlg2.hwnd().MessageBox(
                                &e,
                                "Update failed",
                                co::MB::OK | co::MB::ICONERROR,
                            );
                        }
                        let _ = dlg2.hwnd().EndDialog(0);
                    }
                }
            }
            Ok(())
        });
    }

    {
        let cancel = cancel.clone();
        let label = label.clone();
        dlg.on().wm_command_acc_menu(IDC_DL_CANCEL, move || {
            // Tell the worker and say so; it stops at its next chunk, and the
            // dialog closes when the Done message arrives.
            cancel.store(true, Ordering::Relaxed);
            let _ = label.hwnd().SetWindowText("Cancelling...");
            Ok(())
        });
    }

    {
        // Closing mid-download must not abandon the worker: past its last
        // cancel check it is still verifying, extracting and replacing the
        // exe. Ending the dialog here let all of that finish against a dead
        // window — the executable was swapped despite the "cancel" and, with
        // no relaunch, the running process was silently a different version
        // from the one on disk. Behave exactly like the Cancel button:
        // signal, say why the window stays up, and let Msg::Done close it.
        let cancel = cancel.clone();
        let label = label.clone();
        dlg.on().wm_close(move || {
            cancel.store(true, Ordering::Relaxed);
            let _ = label.hwnd().SetWindowText("Cancelling...");
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Download dialog failed: {e}");
    }
    carry_on.get()
}

/// "You're up to date", for the manual check.
pub fn show_up_to_date(parent: &w::HWND) {
    let _ = parent.MessageBox(
        &format!("You're up to date (v{}).", env!("CARGO_PKG_VERSION")),
        "Check for updates",
        co::MB::OK | co::MB::ICONINFORMATION,
    );
}

/// A failed manual check.
pub fn show_check_error(parent: &w::HWND, msg: &str) {
    let _ = parent.MessageBox(msg, "Check for updates", co::MB::OK | co::MB::ICONERROR);
}

/// Relaunch the replaced executable and quit. The prepare hook runs first so
/// bots disconnect cleanly: `process::exit` skips every destructor.
fn relaunch_and_exit() -> ! {
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
