//! App-global Settings dialog: update checks, launching on Windows startup,
//! and how much saved music to keep.
//!
//! Built from the `IDD_SETTINGS` resource template rather than by placing
//! controls, so tab order, Alt-key mnemonics, Enter/Escape and DPI scaling come
//! from Windows, and a screen reader reads each control's real label.

use winsafe::prelude::*;
use winsafe::{co, gui};

use super::resource_ids::*;
use crate::gui_native::autostart;
use crate::settings::{self, AppSettings};

/// Show the Settings dialog, modal to `parent`. Blocks until it closes.
pub fn show(parent: &impl GuiParent) {
    let dlg = gui::WindowModal::new_dlg(IDD_SETTINGS);
    let updates = gui::CheckBox::new_dlg(&dlg, IDC_CHECK_UPDATES, (gui::Horz::None, gui::Vert::None));
    let autostart_cb =
        gui::CheckBox::new_dlg(&dlg, IDC_AUTOSTART, (gui::Horz::None, gui::Vert::None));
    let cache_limit =
        gui::Edit::new_dlg(&dlg, IDC_CACHE_LIMIT, (gui::Horz::None, gui::Vert::None));
    let cache_days =
        gui::Edit::new_dlg(&dlg, IDC_CACHE_DAYS, (gui::Horz::None, gui::Vert::None));

    // Fill in the current state once the controls exist.
    {
        let updates = updates.clone();
        let autostart_cb = autostart_cb.clone();
        let cache_limit = cache_limit.clone();
        let cache_days = cache_days.clone();
        dlg.on().wm_init_dialog(move |_| {
            let current = settings::load();
            updates.set_check(current.check_updates_on_startup);
            autostart_cb.set_check(autostart::is_enabled());
            let _ = cache_limit.set_text(&current.cache_limit_mb.to_string());
            let _ = cache_days.set_text(&current.cache_keep_days.to_string());
            // true lets Windows set the initial focus from the template, which
            // is what puts a screen reader on the first control.
            Ok(true)
        });
    }

    {
        let dlg2 = dlg.clone();
        let updates = updates.clone();
        let autostart_cb = autostart_cb.clone();
        let cache_limit = cache_limit.clone();
        let cache_days = cache_days.clone();
        dlg.on().wm_command_acc_menu(IDOK, move || {
            let hwnd = dlg2.hwnd();
            let limit_text = cache_limit.text().unwrap_or_default();
            let days_text = cache_days.text().unwrap_or_default();
            let (limit_mb, keep_days) =
                match parse_cache_fields(&limit_text, &days_text) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = hwnd.MessageBox(&e, "Settings", co::MB::OK | co::MB::ICONERROR);
                        return Ok(());
                    }
                };
            let new = AppSettings {
                check_updates_on_startup: updates.is_checked(),
                cache_limit_mb: limit_mb,
                cache_keep_days: keep_days,
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

/// Read the two cache numbers from the dialog, or say what is wrong with them.
///
/// Separate from the window so the rules can be tested. The boxes only accept
/// digits, so the cases left are an empty box and a number too large to mean
/// anything.
pub fn parse_cache_fields(limit: &str, days: &str) -> Result<(u64, u32), String> {
    let limit_mb: u64 = limit
        .trim()
        .parse()
        .map_err(|_| "Saved music limit must be a whole number of MB (0 for none).".to_string())?;
    let keep_days: u32 = days
        .trim()
        .parse()
        .map_err(|_| "Days must be a whole number (0 to keep tracks until the space runs out).".to_string())?;
    if limit_mb > 1024 * 1024 {
        return Err("Saved music limit is too large; use a value in MB.".to_string());
    }
    if keep_days > 3650 {
        return Err("Days is too large; use 3650 or fewer, or 0 to keep tracks.".to_string());
    }
    Ok((limit_mb, keep_days))
}

/// The dialog only reads and writes through these, so the settings round trip
/// is what is worth testing; the window itself is Windows' job.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_cache_numbers_are_accepted() {
        assert_eq!(parse_cache_fields("1024", "7"), Ok((1024, 7)));
        assert_eq!(parse_cache_fields(" 512 ", " 30 "), Ok((512, 30)));
    }

    #[test]
    fn zero_is_allowed_in_both_boxes() {
        // They mean different things - keep nothing, and never expire - but
        // both are legitimate and must not be rejected as empty.
        assert_eq!(parse_cache_fields("0", "0"), Ok((0, 0)));
    }

    #[test]
    fn an_empty_box_is_refused_rather_than_read_as_zero() {
        // Clearing the box and saving would otherwise silently mean "keep no
        // music at all".
        assert!(parse_cache_fields("", "7").is_err());
        assert!(parse_cache_fields("1024", "").is_err());
    }

    #[test]
    fn absurd_numbers_are_refused() {
        assert!(parse_cache_fields("99999999999", "7").is_err());
        assert!(parse_cache_fields("1024", "99999").is_err());
    }

    #[test]
    fn the_message_says_which_box_is_wrong() {
        let limit = parse_cache_fields("x", "7").unwrap_err();
        assert!(limit.to_lowercase().contains("limit"), "got {limit:?}");
        let days = parse_cache_fields("1024", "x").unwrap_err();
        assert!(days.to_lowercase().contains("days"), "got {days:?}");
    }

    #[test]
    fn the_cache_settings_survive_a_save_and_load() {
        let original = settings::load();
        let changed = AppSettings {
            cache_limit_mb: 321,
            cache_keep_days: 12,
            ..original.clone()
        };
        changed.save().expect("settings should save");
        let back = settings::load();
        assert_eq!(back.cache_limit_mb, 321);
        assert_eq!(back.cache_keep_days, 12);
        original.save().expect("settings should restore");
    }

    #[test]
    fn the_dialog_reads_and_writes_the_same_setting() {
        // Guards against the save path and the load path drifting apart, which
        // would show as a checkbox that forgets what the user chose.
        let original = settings::load();

        let flipped = AppSettings {
            check_updates_on_startup: !original.check_updates_on_startup,
            ..original.clone()
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
            ("IDC_CACHE_LIMIT", IDC_CACHE_LIMIT),
            ("IDC_CACHE_DAYS", IDC_CACHE_DAYS),
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
        for name in [
            "IDC_CHECK_UPDATES",
            "IDC_AUTOSTART",
            "IDC_CACHE_LIMIT",
            "IDC_CACHE_DAYS",
            "IDOK",
            "IDCANCEL",
        ] {
            let used_in_dialog = rc
                .lines()
                .filter(|l| !l.trim_start().starts_with("#define"))
                .any(|l| l.contains(name));
            assert!(used_in_dialog, "{name} is defined but never placed in a dialog");
        }
    }
}
