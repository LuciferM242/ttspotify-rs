//! Windows GUI module: system tray and config editor using wxDragon.

pub mod autostart;
pub mod manager;
pub mod config_dialog;
pub mod icon;
pub mod progress;
pub mod settings_dialog;
pub mod tray;
pub mod update_dialog;

pub use tray::run;
