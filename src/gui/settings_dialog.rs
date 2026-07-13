//! App-global Settings window (Windows tray). Two checkboxes:
//!   1. Check for updates on startup  -> settings.json
//!   2. Launch on Windows startup     -> HKCU Run registry (gui::autostart)

use wxdragon::prelude::*;

use super::autostart;
use crate::settings::{self, AppSettings};

pub fn open_settings_dialog() {
    let current = settings::load();
    let autostart_on = autostart::is_enabled();

    let frame = Frame::builder()
        .with_title("Settings")
        .with_size(Size::new(400, 200))
        .build();
    let panel = Panel::builder(&frame).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let update_cb = CheckBox::builder(&panel)
        .with_label("Check for updates on startup")
        .with_value(current.check_updates_on_startup)
        .build();
    let autostart_cb = CheckBox::builder(&panel)
        .with_label("Launch on Windows startup")
        .with_value(autostart_on)
        .build();
    // Screen readers announce a wxCheckBox's window name, which defaults to
    // "check" rather than the label; set it explicitly (matches config_dialog).
    update_cb.set_name("Check for updates on startup");
    autostart_cb.set_name("Launch on Windows startup");

    let btn_row = BoxSizer::builder(Orientation::Horizontal).build();
    let save_btn = Button::builder(&panel).with_label("Save").build();
    let cancel_btn = Button::builder(&panel).with_label("Cancel").build();
    btn_row.add(&save_btn, 0, SizerFlag::All, 5);
    btn_row.add(&cancel_btn, 0, SizerFlag::All, 5);

    sizer.add(&update_cb, 0, SizerFlag::All, 10);
    sizer.add(&autostart_cb, 0, SizerFlag::All, 10);
    sizer.add_sizer(&btn_row, 0, SizerFlag::AlignRight | SizerFlag::All, 6);
    panel.set_sizer(sizer, true);

    cancel_btn.on_click(move |_| {
        frame.close(true);
    });

    save_btn.on_click(move |_| {
        use MessageDialogStyle as MDS;
        let new = AppSettings {
            check_updates_on_startup: update_cb.get_value(),
        };
        if let Err(e) = new.save() {
            MessageDialog::builder(&frame, &format!("Failed to save settings: {e}"), "Error")
                .with_style(MDS::OK | MDS::IconError)
                .build()
                .show_modal();
            return;
        }
        if let Err(e) = autostart::set_enabled(autostart_cb.get_value()) {
            MessageDialog::builder(&frame, &format!("Failed to update autostart: {e}"), "Error")
                .with_style(MDS::OK | MDS::IconError)
                .build()
                .show_modal();
            return;
        }
        frame.close(true);
    });

    frame.centre();
    frame.show(true);
}
