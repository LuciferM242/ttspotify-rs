//! Windows GUI: system tray, config editor and dialogs, on winsafe.
//!
//! Native Win32 throughout. Dialogs are built from resource templates in
//! assets/tray.rc, so tab order, mnemonics, Enter/Escape and DPI scaling come
//! from Windows rather than being reimplemented, and standard controls are what
//! screen readers are built to read.

/// Dialog and control ids, generated from `assets/tray.rc` by build.rs so
/// the resource templates and the code can never disagree about a number.
pub mod resource_ids {
    include!(concat!(env!("OUT_DIR"), "/resource_ids.rs"));
}

pub mod autostart;
pub mod config_dialog;
pub mod config_form;
pub mod facts;
pub mod manager;
pub mod menu;
pub mod progress_dialog;
pub mod remove_dialog;
pub mod settings_dialog;
pub mod tooltip;
pub mod tray;
pub mod update_dialog;
pub mod youtube_tools;

pub use tray::run;
