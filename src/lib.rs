pub mod audio;
pub mod bot;
pub mod config;
pub mod error;
#[cfg(windows)]
pub mod gui;
pub mod logging;
pub mod player;
#[cfg(target_os = "linux")]
pub mod service;
pub mod services;
pub mod spotify;
pub mod track;
pub mod tt;
pub mod wizard;

