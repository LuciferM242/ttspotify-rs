//! Logging setup: stdout + per-instance log file with rotation.
//!
//! Log file path is derived from the config path:
//!   config: ~/.config/ttspotify/myserver.json
//!   log:    ~/.config/ttspotify/logs/myserver.log

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Derive the log file path from a config file path.
/// Returns (log_dir, log_filename).
fn log_path_from_config(config_path: &str) -> (PathBuf, String) {
    let path = Path::new(config_path);
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bot");
    let log_dir = path.parent()
        .unwrap_or(Path::new("."))
        .join("logs");
    (log_dir, format!("{stem}.log"))
}

/// Initialize logging with both stdout and file output.
/// Returns a guard that must be kept alive for the file logger to flush.
pub fn init_logging(config_path: &str) -> WorkerGuard {
    let (log_dir, log_filename) = log_path_from_config(config_path);

    // Create log directory
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("Warning: failed to create log directory {}: {e}", log_dir.display());
    }

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // File appender with daily rotation, keep last 7 days
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_suffix(&log_filename)
        .max_log_files(7)
        .build(&log_dir)
        .expect("failed to create log file appender");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // Stdout layer (warnings and errors only, INFO goes to file)
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::WARN);

    // File layer (no ANSI colors)
    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    tracing::debug!("Logging to {}", log_dir.join(&log_filename).display());

    guard
}

#[allow(dead_code)] // Used by gui/tray.rs (cfg(windows))
/// Initialize file-only logging (no stdout) as the global subscriber. Used by tray app.
/// Logs to {log_dir}/{name}.log with thread names for per-instance identification.
/// Returns a guard that must be kept alive for the file logger to flush.
pub fn init_file_logging(log_dir: &Path, name: &str) -> WorkerGuard {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!("Warning: failed to create log directory {}: {e}", log_dir.display());
    }

    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_suffix(format!("{name}.log"))
        .max_log_files(7)
        .build(log_dir)
        .expect("failed to create log file appender");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_thread_names(true)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .init();

    guard
}

#[allow(dead_code)] // Used by gui/manager.rs (cfg(windows))
/// Create a per-instance file logger without setting it as the global subscriber.
/// Returns a Dispatch and guard. Use `tracing::dispatcher::set_default()` on the
/// target thread to activate it. Threads without a thread-local subscriber fall
/// back to the global one (tray.log).
pub fn create_instance_logging(log_dir: &Path, name: &str) -> (tracing::Dispatch, WorkerGuard) {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!("Warning: failed to create log directory {}: {e}", log_dir.display());
    }

    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_suffix(format!("{name}.log"))
        .max_log_files(7)
        .build(log_dir)
        .expect("failed to create log file appender");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_thread_names(true)
                .with_writer(file_writer),
        );

    (tracing::Dispatch::new(subscriber), guard)
}
