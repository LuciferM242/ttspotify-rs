//! Logging setup: stdout + per-instance log file with rotation.
//!
//! Log file path is derived from the config path:
//!   config: ~/.config/ttspotify/myserver.json
//!   log:    ~/.config/ttspotify/logs/myserver.log

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Directory where the panic hook writes `panics.log`. Set once during logging
/// init so the hook can dump panics synchronously to disk — the non-blocking
/// file writer may not flush on `panic = "abort"`, and the tray has no console
/// for the hook's `eprintln`, so without this a tray panic leaves no trace.
static PANIC_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

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

/// Create a daily-rotating file appender and non-blocking writer. If the file
/// appender can't be created (e.g. an unwritable log directory), fall back to
/// stderr instead of panicking at startup — a logging problem should never
/// prevent the bot from running.
fn create_file_writer(log_dir: &Path, log_filename: &str) -> (tracing_appender::non_blocking::NonBlocking, WorkerGuard) {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!("Warning: failed to create log directory {}: {e}", log_dir.display());
    }
    // Remember the dir so the panic hook can write crashes here synchronously.
    let _ = PANIC_LOG_DIR.set(log_dir.to_path_buf());
    match tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_suffix(log_filename)
        .max_log_files(7)
        .build(log_dir)
    {
        Ok(appender) => tracing_appender::non_blocking(appender),
        Err(e) => {
            eprintln!(
                "Warning: failed to create log file appender in {} ({e}); logging to stderr",
                log_dir.display()
            );
            tracing_appender::non_blocking(std::io::stderr())
        }
    }
}

/// Install a panic hook that records the panic (with a backtrace) via tracing
/// and to stderr before the process aborts. With `panic = "abort"` set in the
/// release profile, a panic in any thread takes down the whole process (in the
/// tray, every bot at once) — without this hook that happens with no trace of
/// where or why. Call once, as early as possible in each entry point.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        tracing::error!("PANIC at {location}: {payload}\n{backtrace}");
        eprintln!("PANIC at {location}: {payload}\n{backtrace}");
        // Write the panic to a dedicated crash file synchronously — the only
        // path that reliably reaches disk under panic=abort with no console.
        if let Some(dir) = PANIC_LOG_DIR.get() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panics.log"))
            {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let _ = writeln!(f, "[unix:{ts}] PANIC at {location}: {payload}\n{backtrace}\n");
                let _ = f.flush();
            }
        }
        // Preserve any prior hook behavior (e.g. default abort message).
        default_hook(info);
    }));
}

fn default_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Initialize logging with both stdout and file output.
/// Returns a guard that must be kept alive for the file logger to flush.
pub fn init_logging(config_path: &str) -> WorkerGuard {
    let (log_dir, log_filename) = log_path_from_config(config_path);
    let (file_writer, guard) = create_file_writer(&log_dir, &log_filename);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::WARN);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(default_env_filter())
        .with(stdout_layer)
        .with(file_layer)
        .init();

    tracing::debug!("Logging to {}", log_dir.join(&log_filename).display());

    guard
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Initialize file-only logging (no stdout) as the global subscriber. Used by tray app.
/// Logs to {log_dir}/{name}.log with thread names for per-instance identification.
/// Returns a guard that must be kept alive for the file logger to flush.
pub fn init_file_logging(log_dir: &Path, name: &str) -> WorkerGuard {
    let (file_writer, guard) = create_file_writer(log_dir, &format!("{name}.log"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_thread_names(true)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(default_env_filter())
        .with(file_layer)
        .init();

    guard
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Create a per-instance file logger without setting it as the global subscriber.
/// Returns a Dispatch and guard. Use `tracing::dispatcher::set_default()` on the
/// target thread to activate it. Threads without a thread-local subscriber fall
/// back to the global one (tray.log).
pub fn create_instance_logging(log_dir: &Path, name: &str) -> (tracing::Dispatch, WorkerGuard) {
    let (file_writer, guard) = create_file_writer(log_dir, &format!("{name}.log"));

    let subscriber = tracing_subscriber::registry()
        .with(default_env_filter())
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_thread_names(true)
                .with_writer(file_writer),
        );

    (tracing::Dispatch::new(subscriber), guard)
}
