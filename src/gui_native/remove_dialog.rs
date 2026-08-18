//! The "remove this bot" confirmation.
//!
//! One dialog rather than a message box followed by a second question: keeping
//! or deleting the logs is part of the same decision, and answering two boxes
//! in a row makes the second easy to answer on autopilot.
//!
//! The layout comes from the resource template, so tab order, the Alt shortcut
//! on the checkbox, Enter and Escape all behave the way Windows users (and
//! screen readers) expect.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use winsafe::gui;
use winsafe::prelude::*;

use super::resource_ids::*;

/// What the user chose. Returned only when they went through with it.
pub struct RemoveChoice {
    pub delete_logs: bool,
}

/// Show the confirmation. `None` means cancelled — nothing should be deleted.
///
/// `has_logs` unticks and disables the checkbox when there is no log folder to
/// delete, so the dialog never offers to remove something that is not there.
pub fn show(
    parent: &(impl GuiParent + 'static),
    name: &str,
    config_path: &Path,
    has_logs: bool,
) -> Option<RemoveChoice> {
    let result: Rc<RefCell<Option<RemoveChoice>>> = Rc::new(RefCell::new(None));

    let dlg = gui::WindowModal::new_dlg(IDD_REMOVE);
    let text = gui::Label::new_dlg(&dlg, IDC_REMOVE_TEXT, (gui::Horz::Resize, gui::Vert::None));
    let logs = gui::CheckBox::new_dlg(&dlg, IDC_REMOVE_LOGS, (gui::Horz::Resize, gui::Vert::None));

    {
        let text = text.clone();
        let logs = logs.clone();
        let message = format!(
            "Remove the bot \"{name}\"?\r\n\r\nIt will be stopped, and this file deleted:\r\n{}\r\n\r\nThis cannot be undone.",
            config_path.display()
        );
        dlg.on().wm_init_dialog(move |_| {
            // SetWindowText rather than set_text_and_resize: the template
            // sized these, and resizing to fit a long path would push the
            // controls out of the dialog.
            let _ = text.hwnd().SetWindowText(&message);
            if !has_logs {
                let _ = logs.hwnd().SetWindowText("Also delete this bot's &logs (none saved)");
                logs.hwnd().EnableWindow(false);
            }
            // Cancel is the default button; leave focus where the template put
            // it rather than moving it onto the destructive one.
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let logs = logs.clone();
        let result = result.clone();
        dlg.on().wm_command_acc_menu(IDOK, move || {
            *result.borrow_mut() = Some(RemoveChoice {
                delete_logs: has_logs && logs.is_checked(),
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
        tracing::error!("Remove dialog failed: {e}");
        return None;
    }
    let choice = result.borrow_mut().take();
    choice
}

