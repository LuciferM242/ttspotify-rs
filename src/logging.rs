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
    let stem = Path::new(config_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bot");
    // Each instance gets its own folder: the rotating appender prunes by
    // counting the files beside it, so sharing one directory made every
    // instance's retention depend on how noisy its neighbours were. Deriving
    // the folder from the config file's parent would also follow configs into
    // config/ and bury the logs there.
    (instance_log_dir(stem), "log".to_string())
}

/// Where one instance's logs live: `logs/<name>/`.
pub fn instance_log_dir(name: &str) -> PathBuf {
    crate::paths::logs_dir().join(name)
}

/// The most recent log file for an instance, for "open my log" menu items.
/// Files are named `<date>.log`, so the newest sorts last.
pub fn newest_log_file(log_dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<PathBuf> = None;
    for entry in std::fs::read_dir(log_dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match &newest {
            Some(best) if path.file_name() <= best.file_name() => {}
            _ => newest = Some(path),
        }
    }
    newest
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

/// Default log filter.
///
/// symphonia is held at `error` because a track that fails to decrypt reaches
/// it as noise, and it reports every byte of that: one failed track produced
/// 8,626 "skipping junk at N bytes" and 1,430 "invalid mpeg audio header"
/// lines, 781 KB of log for three tracks. The failure itself is still reported
/// by librespot at error level, which is the line worth keeping.
///
/// `RUST_LOG` overrides this wholesale, so debugging a decode problem is still
/// possible with `RUST_LOG=symphonia_core=debug`.
const DEFAULT_FILTER: &str = "info,\
    symphonia_core=error,\
    symphonia_bundle_mp3=error,\
    symphonia_format_isomp4=error,\
    symphonia_metadata=error";

fn default_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn the_default_filter_parses() {
        // EnvFilter::new panics on a malformed directive, and this one is only
        // built when RUST_LOG is unset, so a typo would crash every ordinary
        // startup while a developer with RUST_LOG set saw nothing wrong.
        let filter = EnvFilter::new(DEFAULT_FILTER);
        let rendered = filter.to_string();
        assert!(rendered.contains("symphonia_core"), "got: {rendered}");
    }

    #[test]
    fn symphonia_is_quieter_than_everything_else() {
        // The point of the filter: the decode spam is suppressed while normal
        // logging stays at info.
        assert!(DEFAULT_FILTER.starts_with("info"));
        for target in [
            "symphonia_core",
            "symphonia_bundle_mp3",
            "symphonia_format_isomp4",
            "symphonia_metadata",
        ] {
            assert!(
                DEFAULT_FILTER.contains(&format!("{target}=error")),
                "{target} is not held at error, so its decode spam returns"
            );
        }
    }
}

#[cfg(test)]
mod audio_key_watch_tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// The watched failure state is process-wide, so these tests would
    /// otherwise arm each other's flag while running in parallel.
    static SERIALISE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Run `body` with only the watch layer installed, from a clean state and
    /// with no other test in this module running.
    fn with_watch(body: impl FnOnce()) -> parking_lot::MutexGuard<'static, ()> {
        let guard = SERIALISE.lock();
        crate::spotify::audio_key::reset_for_test();
        let subscriber = tracing_subscriber::registry().with(AudioKeyWatch);
        tracing::subscriber::with_default(subscriber, body);
        guard
    }

    #[test]
    fn a_librespot_key_failure_is_seen_through_the_layer() {
        // The whole mechanism end to end: the layer must actually extract the
        // message text from an event. If tracing ever changes how the message
        // field is recorded, this fails rather than silently never matching.
        let _guard = with_watch(|| {
            tracing::error!("error audio key 0 1");
        });
        assert!(
            crate::spotify::audio_key::failed_recently(),
            "the layer did not recognise librespot's key failure"
        );
    }

    #[test]
    fn the_warning_form_is_seen_too() {
        let _guard = with_watch(|| {
            tracing::warn!(
                "Unable to load key, continuing without decryption: Service unavailable {{ audio key error }}"
            );
        });
        assert!(crate::spotify::audio_key::failed_recently());
    }

    #[test]
    fn a_key_failure_logged_through_the_log_crate_is_seen() {
        // The path that actually matters in production: librespot uses the
        // `log` crate, not tracing, so its records reach the layer through
        // tracing-log's bridge. Testing only native tracing events would leave
        // the real path unverified, and a bridge that named the message field
        // differently would make this fix silently do nothing.
        // The real subscribers are built with `.init()`, which installs this
        // bridge; `with_default` alone does not, so the test must install it.
        let _ = tracing_log::LogTracer::init();
        let _guard = with_watch(|| {
            log::error!("error audio key 0 1");
        });
        assert!(
            crate::spotify::audio_key::failed_recently(),
            "a librespot log record did not reach the watcher"
        );
    }

    #[test]
    fn ordinary_logging_does_not_arm_the_blame() {
        let _guard = with_watch(|| {
            tracing::warn!("skipping junk at 4096 bytes");
            tracing::error!("Unable to read audio file: end of stream");
            tracing::info!("error audio key 0 1"); // below warn: ignored
        });
        assert!(
            !crate::spotify::audio_key::failed_recently(),
            "a skip was blamed on the audio key with no key failure"
        );
    }
}

/// Watches librespot's own log for a refused audio key.
///
/// librespot reports this only as a log line: it warns, then streams the
/// undecryptable bytes anyway, so the reason never reaches us as a player
/// event. Catching it here lets the skip message name the real cause instead
/// of blaming the track.
struct AudioKeyWatch;

impl<S: tracing::Subscriber> Layer<S> for AudioKeyWatch {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        use tracing::field::{Field, Visit};

        // Key failures are logged at warn or error; skip the rest cheaply.
        if !matches!(
            *event.metadata().level(),
            tracing::Level::WARN | tracing::Level::ERROR
        ) {
            return;
        }

        struct MessageVisitor(String);
        impl Visit for MessageVisitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }

        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        if crate::spotify::audio_key::is_audio_key_failure(&visitor.0) {
            crate::spotify::audio_key::note_failure();
        }
    }
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
        .with(AudioKeyWatch)
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
    let (file_writer, guard) = create_file_writer(&log_dir.join(name), "log");

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_thread_names(true)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(default_env_filter())
        .with(AudioKeyWatch)
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
    let (file_writer, guard) = create_file_writer(&log_dir.join(name), "log");

    let subscriber = tracing_subscriber::registry()
        .with(default_env_filter())
        .with(AudioKeyWatch)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_thread_names(true)
                .with_writer(file_writer),
        );

    (tracing::Dispatch::new(subscriber), guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("ttspotify_logging_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn newest_log_file_picks_the_latest_date() {
        let dir = scratch("newest");
        for day in ["2026-08-01.log", "2026-08-09.log", "2026-07-30.log"] {
            std::fs::write(dir.join(day), "x").unwrap();
        }
        let newest = newest_log_file(&dir).expect("a log should be found");
        assert_eq!(newest.file_name().unwrap(), "2026-08-09.log");
    }

    #[test]
    fn newest_log_file_is_none_for_an_empty_or_missing_folder() {
        let dir = scratch("empty");
        assert!(newest_log_file(&dir).is_none());
        assert!(newest_log_file(&dir.join("nope")).is_none());
    }

    #[test]
    fn each_instance_logs_into_its_own_folder() {
        // Shared directories made one instance's retention depend on how noisy
        // its neighbours were.
        let (dir_a, name_a) = log_path_from_config("/anywhere/config/alpha.json");
        let (dir_b, _) = log_path_from_config("/anywhere/config/beta.json");
        assert_ne!(dir_a, dir_b);
        assert!(dir_a.ends_with("alpha"), "got {}", dir_a.display());
        assert_eq!(name_a, "log");
    }
}
