//! Spotify sign-in from the tray: choose a method, and for a pair code, show
//! the code while Spotify waits for it to be confirmed.
//!
//! The pair-code wait runs on a worker thread with its own runtime and reports
//! through a channel drained on a timer, so the message loop never blocks.
//! Cancel drops the wait, which is the whole cancellation.

use std::cell::RefCell;
use std::rc::Rc;

use winsafe::gui;
use winsafe::prelude::*;
use winsafe::{self as w, co};

use super::resource_ids::*;
use crate::spotify::auth::{PairCode, SignInMethod, SpotifyAuth};

const TIMER_DRAIN: usize = 1;
const TIMER_DRAIN_MS: u32 = 150;

/// Ask how to sign in. `None` means cancelled.
pub fn choose(parent: &(impl GuiParent + 'static)) -> Option<SignInMethod> {
    let result: Rc<RefCell<Option<SignInMethod>>> = Rc::new(RefCell::new(None));

    let dlg = gui::WindowModal::new_dlg(IDD_SIGNIN);
    let methods = gui::RadioGroup::new_dlg(
        &dlg,
        &[
            (IDC_SIGNIN_BROWSER, gui::Horz::None, gui::Vert::None),
            (IDC_SIGNIN_CODE, gui::Horz::None, gui::Vert::None),
        ],
    );

    {
        let methods = methods.clone();
        dlg.on().wm_init_dialog(move |_| {
            if let Some(browser) = methods.iter().next() {
                browser.select(true);
                let _ = browser.hwnd().SetFocus();
            }
            // Focus was placed by hand.
            Ok(false)
        });
    }
    {
        let dlg2 = dlg.clone();
        let methods = methods.clone();
        let result = result.clone();
        dlg.on().wm_command_acc_menu(IDOK, move || {
            *result.borrow_mut() = Some(match methods.selected_index() {
                Some(1) => SignInMethod::Code,
                _ => SignInMethod::Browser,
            });
            let _ = dlg2.hwnd().EndDialog(IDOK as isize);
            Ok(())
        });
    }
    {
        let dlg2 = dlg.clone();
        dlg.on().wm_command_acc_menu(IDCANCEL, move || {
            let _ = dlg2.hwnd().EndDialog(IDCANCEL as isize);
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Sign-in dialog failed: {e}");
        return None;
    }
    let method = result.borrow_mut().take();
    method
}

/// How a pair-code sign-in ended.
pub enum PairOutcome {
    SignedIn,
    Failed(String),
    Cancelled,
}

enum Msg {
    Code(PairCode),
    Done(Result<(), String>),
}

/// Show a pair code and wait for it to be confirmed. Blocks until the sign-in
/// ends or is cancelled.
pub fn pair(parent: &(impl GuiParent + 'static)) -> PairOutcome {
    let dlg = gui::WindowModal::new_dlg(IDD_PAIR);
    let code_edit = gui::Edit::new_dlg(&dlg, IDC_PAIR_CODE, (gui::Horz::None, gui::Vert::None));
    let url_edit = gui::Edit::new_dlg(&dlg, IDC_PAIR_URL, (gui::Horz::None, gui::Vert::None));
    let status = gui::Label::new_dlg(&dlg, IDC_PAIR_STATUS, (gui::Horz::None, gui::Vert::None));
    let buttons = [IDC_PAIR_COPY_CODE, IDC_PAIR_COPY_URL, IDC_PAIR_OPEN]
        .map(|id| gui::Button::new_dlg(&dlg, id, (gui::Horz::None, gui::Vert::None)));

    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let cancel_rx = RefCell::new(Some(cancel_rx));
    let cancel_tx = Rc::new(RefCell::new(Some(cancel_tx)));
    let code: Rc<RefCell<Option<PairCode>>> = Rc::new(RefCell::new(None));
    let outcome = Rc::new(RefCell::new(PairOutcome::Cancelled));

    {
        let dlg2 = dlg.clone();
        let status = status.clone();
        let buttons = buttons.clone();
        dlg.on().wm_init_dialog(move |_| {
            let _ = status.hwnd().SetWindowText("Asking Spotify for a pair code...");
            // Nothing to copy or open until the code arrives.
            for button in &buttons {
                button.hwnd().EnableWindow(false);
            }
            if let Some(cancel_rx) = cancel_rx.borrow_mut().take() {
                let tx = tx.clone();
                std::thread::spawn(move || run_pair(tx, cancel_rx));
            }
            let _ = dlg2.hwnd().SetTimer(TIMER_DRAIN, TIMER_DRAIN_MS, None);
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let code_edit = code_edit.clone();
        let url_edit = url_edit.clone();
        let status = status.clone();
        let buttons = buttons.clone();
        let code = code.clone();
        let outcome = outcome.clone();
        dlg.on().wm_timer(TIMER_DRAIN, move || {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Code(pair) => {
                        let _ = code_edit.hwnd().SetWindowText(&pair.user_code);
                        let _ = url_edit.hwnd().SetWindowText(&pair.url);
                        for button in &buttons {
                            button.hwnd().EnableWindow(true);
                        }
                        let _ = status.hwnd().SetWindowText(
                            "Confirm the code on the pairing page, on this PC or any other \
                             device. This window closes by itself.",
                        );
                        // The code is what someone may need to read or type.
                        let _ = code_edit.hwnd().SetFocus();
                        open_url(&pair.url);
                        *code.borrow_mut() = Some(pair);
                    }
                    Msg::Done(result) => {
                        *outcome.borrow_mut() = match result {
                            Ok(()) => PairOutcome::SignedIn,
                            Err(e) => PairOutcome::Failed(e),
                        };
                        let _ = dlg2.hwnd().KillTimer(TIMER_DRAIN);
                        let _ = dlg2.hwnd().EndDialog(IDOK as isize);
                    }
                }
            }
            Ok(())
        });
    }

    for (id, pick) in [
        (IDC_PAIR_COPY_CODE, (|c: &PairCode| c.user_code.clone()) as fn(&PairCode) -> String),
        (IDC_PAIR_COPY_URL, |c: &PairCode| c.url.clone()),
    ] {
        let dlg2 = dlg.clone();
        let status = status.clone();
        let code = code.clone();
        dlg.on().wm_command_acc_menu(id, move || {
            if let Some(pair) = code.borrow().as_ref() {
                let copied = copy_text(dlg2.hwnd(), &pick(pair)).is_ok();
                let _ = status.hwnd().SetWindowText(match (copied, id == IDC_PAIR_COPY_CODE) {
                    (true, true) => "Code copied.",
                    (true, false) => "Link copied.",
                    (false, _) => "Could not copy to the clipboard.",
                });
            }
            Ok(())
        });
    }
    {
        let code = code.clone();
        dlg.on().wm_command_acc_menu(IDC_PAIR_OPEN, move || {
            if let Some(pair) = code.borrow().as_ref() {
                open_url(&pair.url);
            }
            Ok(())
        });
    }
    {
        let dlg2 = dlg.clone();
        let cancel_tx = cancel_tx.clone();
        dlg.on().wm_command_acc_menu(IDCANCEL, move || {
            if let Some(cancel) = cancel_tx.borrow_mut().take() {
                let _ = cancel.send(());
            }
            let _ = dlg2.hwnd().EndDialog(IDCANCEL as isize);
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Pair-code dialog failed: {e}");
        return PairOutcome::Failed(format!("the pair-code window could not open: {e}"));
    }
    // Closed some other way than Cancel: stop the wait all the same.
    if let Some(cancel) = cancel_tx.borrow_mut().take() {
        let _ = cancel.send(());
    }
    let ended = std::mem::replace(&mut *outcome.borrow_mut(), PairOutcome::Cancelled);
    ended
}

/// The worker: get a code, report it, then wait for it to be confirmed.
fn run_pair(tx: crossbeam_channel::Sender<Msg>, cancel: tokio::sync::oneshot::Receiver<()>) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = tx.send(Msg::Done(Err(format!("could not start the sign-in: {e}"))));
            return;
        }
    };
    let mut auth = SpotifyAuth::new();
    let work = async {
        let pending = auth.request_pair_code().await.map_err(|e| e.to_string())?;
        let _ = tx.send(Msg::Code(pending.code.clone()));
        auth.finish_pair(pending).await.map(drop).map_err(|e| e.to_string())
    };
    let result = rt.block_on(async {
        tokio::select! {
            result = work => Some(result),
            _ = cancel => None,
        }
    });
    match result {
        Some(Ok(())) => tracing::info!("Spotify pair-code sign-in successful"),
        Some(Err(ref e)) => tracing::error!("Spotify pair-code sign-in failed: {e}"),
        None => tracing::info!("Spotify pair-code sign-in cancelled"),
    }
    if let Some(result) = result {
        let _ = tx.send(Msg::Done(result));
    }
}

fn open_url(url: &str) {
    if let Err(e) = open::that_detached(url) {
        tracing::warn!("Could not open the pairing page: {e}");
    }
}

/// `text` as the clipboard's UTF-16, null-terminated.
fn clipboard_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn copy_text(hwnd: &w::HWND, text: &str) -> w::SysResult<()> {
    let clipboard = hwnd.OpenClipboard()?;
    clipboard.EmptyClipboard()?;
    clipboard.SetClipboardData(co::CF::UNICODETEXT, &clipboard_bytes(text))
}

#[cfg(test)]
mod tests {
    use super::clipboard_bytes;

    #[test]
    fn clipboard_text_is_null_terminated_utf16() {
        assert_eq!(clipboard_bytes("AB"), vec![b'A', 0, b'B', 0, 0, 0]);
        assert_eq!(clipboard_bytes(""), vec![0, 0]);
    }
}
