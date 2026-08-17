//! Live-log window for long-running work: installing or updating the YouTube
//! tools.
//!
//! The work runs on a worker thread and reports lines through a channel; the
//! dialog drains that channel on a timer. Nothing slow happens on the message
//! loop, so the window keeps repainting and the tray behind it keeps updating
//! its status while a 50 MB download runs.
//!
//! Close stays disabled until the work finishes, so the window cannot be shut
//! on a half-installed toolchain.

use std::cell::RefCell;
use std::rc::Rc;

use winsafe::prelude::*;
use winsafe::gui;

use super::resource_ids::*;

const TIMER_DRAIN: usize = 1;
const TIMER_DRAIN_MS: u32 = 150;

enum Msg {
    Line(String),
    Done(Result<(), String>),
}

/// Run `worker` on a background thread, showing its output. Returns whether it
/// succeeded. Blocks until the user closes the window.
pub fn run<F>(parent: &(impl GuiParent + 'static), title: &str, worker: F) -> bool
where
    F: FnOnce(&dyn Fn(&str)) -> Result<(), String> + Send + 'static,
{
    let dlg = gui::WindowModal::new_dlg(IDD_PROGRESS);
    let log = gui::Edit::new_dlg(&dlg, IDC_PROG_LOG, (gui::Horz::Resize, gui::Vert::Resize));
    let close = gui::Button::new_dlg(&dlg, IDC_PROG_CLOSE, (gui::Horz::Repos, gui::Vert::Repos));

    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let text = Rc::new(RefCell::new(String::new()));
    let succeeded = Rc::new(std::cell::Cell::new(false));
    let finished = Rc::new(std::cell::Cell::new(false));
    // The worker is consumed by the thread, so it has to be moved out of the
    // init closure, which may only be called once.
    let worker = RefCell::new(Some(worker));
    let title = title.to_string();

    {
        let dlg2 = dlg.clone();
        let close = close.clone();
        let tx = tx.clone();
        dlg.on().wm_init_dialog(move |_| {
            let _ = dlg2.hwnd().SetWindowText(&title);
            // Nothing to close yet: the work has not finished.
            close.hwnd().EnableWindow(false);
            if let Some(worker) = worker.borrow_mut().take() {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let report = |line: &str| {
                        let _ = tx.send(Msg::Line(line.to_string()));
                    };
                    let result = worker(&report);
                    let _ = tx.send(Msg::Done(result));
                });
            }
            let _ = dlg2.hwnd().SetTimer(TIMER_DRAIN, TIMER_DRAIN_MS, None);
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let log = log.clone();
        let close = close.clone();
        let text = text.clone();
        let succeeded = succeeded.clone();
        let finished = finished.clone();
        dlg.on().wm_timer(TIMER_DRAIN, move || {
            // Only what arrived this tick, appended at the caret: that keeps
            // the control scrolled to the newest line without rewriting it.
            let mut fresh = String::new();
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Line(line) => {
                        fresh.push_str(&line);
                        fresh.push_str("\r\n");
                    }
                    Msg::Done(result) => {
                        match &result {
                            Ok(()) => fresh.push_str("\r\nDone.\r\n"),
                            Err(e) => fresh.push_str(&format!("\r\nFailed: {e}\r\n")),
                        }
                        succeeded.set(result.is_ok());
                        finished.set(true);
                        close.hwnd().EnableWindow(true);
                        // Put focus on Close once it becomes usable, so a
                        // keyboard user does not have to hunt for it.
                        let _ = close.hwnd().SetFocus();
                        let _ = dlg2.hwnd().KillTimer(TIMER_DRAIN);
                    }
                }
            }
            if !fresh.is_empty() {
                text.borrow_mut().push_str(&fresh);
                append(&log, &fresh);
            }
            Ok(())
        });
    }

    {
        let dlg2 = dlg.clone();
        let finished = finished.clone();
        dlg.on().wm_command_acc_menu(IDC_PROG_CLOSE, move || {
            if finished.get() {
                let _ = dlg2.hwnd().EndDialog(0);
            }
            Ok(())
        });
    }

    {
        // Escape and the title bar's X both route here. Refusing while the
        // work is running is what keeps a half-install from being abandoned.
        let finished = finished.clone();
        let dlg2 = dlg.clone();
        dlg.on().wm_close(move || {
            if finished.get() {
                let _ = dlg2.hwnd().EndDialog(0);
            }
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Progress dialog failed: {e}");
        return false;
    }
    succeeded.get()
}

/// Append at the end of the control. Placing the caret at the end first means
/// the control scrolls to follow it, so the newest line stays visible.
fn append(log: &gui::Edit, text: &str) {
    let len = log.text().map(|t| t.len()).unwrap_or(0) as i32;
    log.set_selection(len, len);
    log.replace_selection(text);
}

pub use crate::gui_native::youtube_tools::{youtube_install, youtube_update};
