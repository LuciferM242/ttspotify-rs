//! App-global Settings dialog: check for updates on startup, and launch on
//! Windows startup.
//!
//! Built from the `IDD_SETTINGS` resource template rather than by placing
//! controls, so tab order, Alt-key mnemonics, Enter/Escape and DPI scaling come
//! from Windows. The wx version had to set each control's accessible name by
//! hand, because wxCheckBox reported its window name ("check") rather than its
//! label; standard controls in a dialog template do not need that.
//!
//! Both settings are read and written by code that has nothing to do with the
//! toolkit (`crate::settings` and `crate::gui::autostart`), so only the window
//! itself changed in the port.

use winsafe::prelude::*;
use winsafe::{co, gui};

use crate::gui::autostart;
use crate::settings::{self, AppSettings};

const IDD_SETTINGS: u16 = 100;
const IDC_CHECK_UPDATES: u16 = 101;
const IDC_AUTOSTART: u16 = 102;
const IDOK: u16 = 1;
const IDCANCEL: u16 = 2;

/// Show the Settings dialog, modal to `parent`. Blocks until it closes.
pub fn show(parent: &impl GuiParent) {
    let dlg = gui::WindowModal::new_dlg(IDD_SETTINGS);
    let updates = gui::CheckBox::new_dlg(&dlg, IDC_CHECK_UPDATES, (gui::Horz::None, gui::Vert::None));
    let autostart_cb =
        gui::CheckBox::new_dlg(&dlg, IDC_AUTOSTART, (gui::Horz::None, gui::Vert::None));

    // Fill in the current state once the controls exist.
    {
        let updates = updates.clone();
        let autostart_cb = autostart_cb.clone();
        dlg.on().wm_init_dialog(move |_| {
            updates.set_check(settings::load().check_updates_on_startup);
            autostart_cb.set_check(autostart::is_enabled());
            // true lets Windows set the initial focus from the template, which
            // is what puts a screen reader on the first control.
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let updates = updates.clone();
        let autostart_cb = autostart_cb.clone();
        dlg.on().wm_command_acc_menu(IDOK, move || {
            let hwnd = dlg2.hwnd();
            let new = AppSettings {
                check_updates_on_startup: updates.is_checked(),
            };
            if let Err(e) = new.save() {
                // Stay open on failure so the user can retry or cancel, rather
                // than closing as though it had worked.
                let _ = hwnd.MessageBox(
                    &format!("Could not save settings: {e}"),
                    "Settings",
                    co::MB::OK | co::MB::ICONERROR,
                );
                return Ok(());
            }
            if let Err(e) = autostart::set_enabled(autostart_cb.is_checked()) {
                let _ = hwnd.MessageBox(
                    &format!("Could not change the startup setting: {e}"),
                    "Settings",
                    co::MB::OK | co::MB::ICONERROR,
                );
                return Ok(());
            }
            let _ = hwnd.EndDialog(IDOK as isize);
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
        tracing::error!("Settings dialog failed: {e}");
    }
}

/// The dialog only reads and writes through these, so the settings round trip
/// is what is worth testing; the window itself is Windows' job.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_reads_and_writes_the_same_setting() {
        // Guards against the save path and the load path drifting apart, which
        // would show as a checkbox that forgets what the user chose.
        let original = settings::load();

        let flipped = AppSettings {
            check_updates_on_startup: !original.check_updates_on_startup,
        };
        flipped.save().expect("settings should save");
        assert_eq!(
            settings::load().check_updates_on_startup,
            flipped.check_updates_on_startup,
            "what was saved is not what loads back"
        );

        original.save().expect("settings should restore");
        assert_eq!(
            settings::load().check_updates_on_startup,
            original.check_updates_on_startup
        );
    }

    /// The `#define NAME value` lines in the resource script.
    fn resource_defines() -> std::collections::HashMap<String, u16> {
        let rc = include_str!("../../assets/tray.rc");
        rc.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("#define ")?;
                let mut parts = rest.split_whitespace();
                let name = parts.next()?.to_string();
                let value = parts.next()?.parse().ok()?;
                Some((name, value))
            })
            .collect()
    }

    #[test]
    fn the_control_ids_match_the_resource_template() {
        // The real contract, and the reason this is worth a test: winsafe finds
        // each control by id, so if a Rust constant and the .rc drift apart the
        // control silently does nothing at runtime. Nothing else would catch
        // that until someone clicked it.
        let defines = resource_defines();
        for (name, ours) in [
            ("IDD_SETTINGS", IDD_SETTINGS),
            ("IDC_CHECK_UPDATES", IDC_CHECK_UPDATES),
            ("IDC_AUTOSTART", IDC_AUTOSTART),
            ("IDOK", IDOK),
            ("IDCANCEL", IDCANCEL),
        ] {
            let theirs = defines
                .get(name)
                .unwrap_or_else(|| panic!("{name} is missing from assets/tray.rc"));
            assert_eq!(*theirs, ours, "{name} differs between Rust and the .rc");
        }
    }

    #[test]
    fn the_template_declares_every_control_the_code_binds() {
        // Binding an id the template never defines is the same silent failure
        // seen from the other direction.
        let rc = include_str!("../../assets/tray.rc");
        for name in ["IDC_CHECK_UPDATES", "IDC_AUTOSTART", "IDOK", "IDCANCEL"] {
            let used_in_dialog = rc
                .lines()
                .filter(|l| !l.trim_start().starts_with("#define"))
                .any(|l| l.contains(name));
            assert!(used_in_dialog, "{name} is defined but never placed in a dialog");
        }
    }
}
