pub mod audio;
pub mod bot;
pub mod config;
pub mod error;
#[cfg(windows)]
pub mod gui;
pub mod logging;
#[cfg(target_os = "linux")]
pub mod service;
pub mod spotify;
pub mod tt;
pub mod wizard;

