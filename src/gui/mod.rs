//! Windows GUI module: system tray and config editor using wxDragon.

pub mod manager;
pub mod config_dialog;
pub mod icon;
pub mod tray;

pub use tray::run;
