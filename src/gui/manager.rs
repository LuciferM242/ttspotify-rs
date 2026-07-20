//! Bot instance manager for the system tray.
//!
//! Manages multiple bot instances, each running in its own thread with a
//! tokio runtime. Status updates flow back via crossbeam channel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use parking_lot::Mutex;
use std::thread;

use crate::bot::runner::{BotExit, RunnerEvent};

/// Status of a bot instance, displayed in tray menu and tooltip.
#[derive(Debug, Clone)]
pub enum BotStatus {
    Stopped,
    Starting,
    Connecting,
    Authenticating,
    Connected,
    Playing(String),
    Disconnected,
    Error(String),
}

impl std::fmt::Display for BotStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BotStatus::Stopped => write!(f, "Stopped"),
            BotStatus::Starting => write!(f, "Starting..."),
            BotStatus::Connecting => write!(f, "Connecting to server..."),
            BotStatus::Authenticating => write!(f, "Authenticating Spotify..."),
            BotStatus::Connected => write!(f, "Connected, Idle"),
            BotStatus::Playing(track) => write!(f, "Connected, Playing: {track}"),
            BotStatus::Disconnected => write!(f, "Disconnected"),
            BotStatus::Error(msg) => write!(f, "Error: {msg}"),
        }
    }
}

/// A single bot instance with its thread and status.
struct BotInstance {
    name: String,
    config_path: PathBuf,
    status: Arc<Mutex<BotStatus>>,
    thread: Option<thread::JoinHandle<()>>,
    shutdown: Option<Arc<AtomicBool>>,
}

impl BotInstance {
    fn new(name: String, config_path: PathBuf) -> Self {
        Self {
            name,
            config_path,
            status: Arc::new(Mutex::new(BotStatus::Stopped)),
            thread: None,
            shutdown: None,
        }
    }

    fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

/// What `start` should do given the instance's current thread state.
#[derive(Debug, PartialEq, Eq)]
enum StartAction {
    /// Thread alive, no stop signalled: a second start is a no-op.
    AlreadyRunning,
    /// No live thread: start normally.
    Fresh,
    /// Thread alive but stop already signalled (Stop clicked a moment ago):
    /// wait for the old thread to exit, then start — otherwise the quick
    /// Stop-then-Start sequence silently does nothing.
    DeferAfterStop,
}

fn start_disposition(running: bool, stop_signalled: bool) -> StartAction {
    match (running, stop_signalled) {
        (false, _) => StartAction::Fresh,
        (true, false) => StartAction::AlreadyRunning,
        (true, true) => StartAction::DeferAfterStop,
    }
}

/// Manages multiple bot instances from config files.
pub struct BotManager {
    instances: HashMap<String, BotInstance>,
    status_tx: crossbeam_channel::Sender<(String, BotStatus)>,
}

impl BotManager {
    pub fn new(status_tx: crossbeam_channel::Sender<(String, BotStatus)>) -> Self {
        Self {
            instances: HashMap::new(),
            status_tx,
        }
    }

    pub fn load_configs(&mut self) -> Vec<String> {
        let configs = crate::config::list_configs();
        let mut names = Vec::new();
        for (name, path) in configs {
            if !self.instances.contains_key(&name) {
                self.instances
                    .insert(name.clone(), BotInstance::new(name.clone(), path));
                names.push(name);
            }
        }
        names
    }

    pub fn statuses(&self) -> Vec<(String, BotStatus)> {
        let mut result: Vec<_> = self
            .instances
            .iter()
            .map(|(name, inst)| {
                let status = inst.status.lock().clone();
                (name.clone(), status)
            })
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    pub fn start(&mut self, name: &str) -> bool {
        let (running, stop_signalled) = match self.instances.get(name) {
            Some(inst) => (
                inst.is_running(),
                inst.shutdown
                    .as_ref()
                    .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)),
            ),
            None => return false,
        };
        match start_disposition(running, stop_signalled) {
            StartAction::AlreadyRunning => return false,
            StartAction::DeferAfterStop => {
                // The old thread is still winding down from stop_nonblocking;
                // hand off to the restart machinery, which waits for it to
                // exit before starting fresh.
                self.restart_nonblocking(name);
                return true;
            }
            StartAction::Fresh => {}
        }
        let inst = match self.instances.get_mut(name) {
            Some(i) => i,
            None => return false,
        };

        let config_path = inst.config_path.clone();
        let status = inst.status.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown.clone();
        let status_tx = self.status_tx.clone();
        let bot_name = name.to_string();

        *status.lock() = BotStatus::Starting;
        let _ = status_tx.send((bot_name.clone(), BotStatus::Starting));

        let handle = match thread::Builder::new()
            .name(format!("bot-{name}"))
            .spawn(move || {
                run_bot_instance(config_path, status, shutdown_flag, status_tx, bot_name);
            }) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::error!("[{}] Failed to spawn bot thread: {e}", inst.name);
                *inst.status.lock() = BotStatus::Error(format!("Thread spawn failed: {e}"));
                let _ = self.status_tx.send((name.to_string(), BotStatus::Error(format!("Thread spawn failed: {e}"))));
                return false;
            }
        };

        inst.thread = handle;
        inst.shutdown = Some(shutdown);
        true
    }

    pub fn stop(&mut self, name: &str) -> bool {
        let inst = match self.instances.get_mut(name) {
            Some(i) => i,
            None => return false,
        };
        if !inst.is_running() {
            return false;
        }
        if let Some(flag) = &inst.shutdown {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(handle) = inst.thread.take() {
            let _ = handle.join();
        }
        *inst.status.lock() = BotStatus::Stopped;
        let _ = self.status_tx.send((name.to_string(), BotStatus::Stopped));
        inst.shutdown = None;
        true
    }

    /// Signal a bot to stop without blocking. The bot thread will exit on its
    /// own and send a status update through the channel. Use this from the GUI
    /// thread to avoid freezing the UI.
    pub fn stop_nonblocking(&mut self, name: &str) -> bool {
        let inst = match self.instances.get_mut(name) {
            Some(i) => i,
            None => return false,
        };
        if !inst.is_running() {
            return false;
        }
        if let Some(flag) = &inst.shutdown {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        true
    }

    /// Signal a bot to stop, then start it again once the old thread finishes.
    /// Runs in a background thread to avoid blocking the GUI.
    pub fn restart_nonblocking(&mut self, name: &str) {
        // Signal stop first
        if !self.stop_nonblocking(name) {
            // Not running, just start
            self.start(name);
            return;
        }
        // Spawn a thread that waits for the old bot to exit, then restarts
        let inst = match self.instances.get_mut(name) {
            Some(i) => i,
            None => return,
        };
        let old_handle = inst.thread.take();
        let config_path = inst.config_path.clone();
        let status = inst.status.clone();
        let status_tx = self.status_tx.clone();
        let bot_name = name.to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown.clone();

        *status.lock() = BotStatus::Starting;
        let _ = status_tx.send((bot_name.clone(), BotStatus::Starting));

        let handle = match thread::Builder::new()
            .name(format!("bot-{name}"))
            .spawn(move || {
                // Wait for old thread to finish
                if let Some(h) = old_handle {
                    let _ = h.join();
                }
                thread::sleep(std::time::Duration::from_millis(500));
                run_bot_instance(config_path, status, shutdown_flag, status_tx, bot_name);
            }) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::error!("[{}] Failed to spawn restart thread: {e}", inst.name);
                *inst.status.lock() = BotStatus::Error(format!("Restart failed: {e}"));
                let _ = self.status_tx.send((name.to_string(), BotStatus::Error(format!("Restart failed: {e}"))));
                return;
            }
        };

        inst.thread = handle;
        inst.shutdown = Some(shutdown);
    }

    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            self.stop(&name);
        }
    }

    /// Signal all bots to stop without blocking. Use this from the GUI
    /// thread (e.g. on_destroy) to avoid freezing the event loop.
    pub fn stop_all_nonblocking(&mut self) {
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            self.stop_nonblocking(&name);
        }
    }

    /// Signal all bots to stop, then wait up to `timeout` (total, across all
    /// instances) for their threads to finish so they can disconnect cleanly
    /// from the TeamTalk server and persist config. Use on app exit: a bounded
    /// wait avoids both an ungraceful drop and a frozen-forever GUI.
    pub fn stop_all_with_timeout(&mut self, timeout: std::time::Duration) {
        // Signal everyone first so they shut down in parallel.
        self.stop_all_nonblocking();
        let deadline = std::time::Instant::now() + timeout;
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            let inst = match self.instances.get_mut(&name) {
                Some(i) => i,
                None => continue,
            };
            if let Some(handle) = inst.thread.take() {
                // Only wait for bots with a live session: they see the shutdown
                // flag within one 100ms poll and disconnect cleanly. A bot still
                // starting/connecting is blocked inside a connect/login wait and
                // can't respond until it times out — waiting for it just delays
                // exit (the process is going away and the half-open connection
                // gets dropped either way), so abandon it immediately.
                let live = matches!(
                    *inst.status.lock(),
                    BotStatus::Connected | BotStatus::Playing(_)
                );
                if live {
                    // Poll for completion until the shared deadline. If the
                    // thread finishes, join it; if the deadline passes first,
                    // abandon it (dropping the handle detaches it) rather than
                    // blocking on join() forever — the process is exiting anyway.
                    loop {
                        if handle.is_finished() {
                            let _ = handle.join();
                            break;
                        }
                        if std::time::Instant::now() >= deadline {
                            tracing::warn!("[{name}] did not shut down within timeout; abandoning thread");
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                } else {
                    tracing::info!("[{name}] no live session; not waiting on exit");
                }
            }
            *inst.status.lock() = BotStatus::Stopped;
            inst.shutdown = None;
        }
    }

    pub fn config_path(&self, name: &str) -> Option<PathBuf> {
        self.instances.get(name).map(|i| i.config_path.clone())
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.instances.get(name).is_some_and(|i| i.is_running())
    }
}

/// Run a single bot instance in its own tokio runtime.
fn run_bot_instance(
    config_path: PathBuf,
    status: Arc<Mutex<BotStatus>>,
    shutdown: Arc<AtomicBool>,
    status_tx: crossbeam_channel::Sender<(String, BotStatus)>,
    name: String,
) {
    // Per-instance log file (e.g. logs/myserver.log)
    let log_dir = crate::config::config_dir().join("logs");
    let (dispatch, _log_guard) = crate::logging::create_instance_logging(&log_dir, &name);
    let _dispatch_guard = tracing::dispatcher::set_default(&dispatch);

    let update_status = |new_status: BotStatus| {
        *status.lock() = new_status.clone();
        let _ = status_tx.send((name.clone(), new_status));
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            update_status(BotStatus::Error(format!("Runtime: {e}")));
            return;
        }
    };

    // Bridge runner events to tray BotStatus
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<RunnerEvent>();
    let bridge_status = status.clone();
    let bridge_tx = status_tx.clone();
    let bridge_name = name.clone();
    std::thread::spawn(move || {
        while let Ok(evt) = event_rx.recv() {
            let new_status = match evt {
                RunnerEvent::Connecting => BotStatus::Connecting,
                RunnerEvent::Authenticating => BotStatus::Authenticating,
                RunnerEvent::Connected | RunnerEvent::Idle => BotStatus::Connected,
                RunnerEvent::Playing(track) => BotStatus::Playing(track),
                RunnerEvent::Disconnected => BotStatus::Disconnected,
                RunnerEvent::Error(msg) => BotStatus::Error(msg),
            };
            *bridge_status.lock() = new_status.clone();
            let _ = bridge_tx.send((bridge_name.clone(), new_status));
        }
    });

    let config_path_str = config_path.to_str().unwrap_or("").to_string();
    // Auto-recover from run_bot errors (e.g. reconnect exhausted, server down
    // at startup) with capped exponential backoff before giving up.
    const MAX_ERROR_RETRIES: u32 = 5;
    let mut error_retries: u32 = 0;
    // Carries the current channel across restarts (in memory); config default
    // is used on a fresh start.
    let last_channel = Arc::new(Mutex::new(None));
    rt.block_on(async {
        loop {
            // Reload config each iteration so edits take effect on restart
            let cfg = match crate::config::BotConfig::load(&config_path_str) {
                Ok(c) => c,
                Err(e) => {
                    update_status(BotStatus::Error(format!("Config: {e}")));
                    return;
                }
            };

            let shutdown_clone = shutdown.clone();
            let event_tx_clone = event_tx.clone();
            match crate::bot::runner::run_bot(
                cfg,
                config_path_str.clone(),
                shutdown_clone,
                Some(event_tx_clone),
                last_channel.clone(),
            )
            .await
            {
                Ok(BotExit::Restart) => {
                    // Bot requested restart (user sent "rs" command)
                    tracing::info!("[{name}] Restart requested, restarting...");
                    update_status(BotStatus::Starting);
                    shutdown.store(false, std::sync::atomic::Ordering::Relaxed);
                    // Verify event bridge is still alive before restarting
                    if event_tx.send(RunnerEvent::Idle).is_err() {
                        tracing::warn!("[{name}] Event bridge dead, cannot restart");
                        update_status(BotStatus::Stopped);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    continue;
                }
                Ok(_) => {
                    update_status(BotStatus::Stopped);
                }
                Err(e) => {
                    // Don't retry if the user asked the bot to stop.
                    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        update_status(BotStatus::Error(e.to_string()));
                        break;
                    }
                    error_retries += 1;
                    if error_retries > MAX_ERROR_RETRIES {
                        tracing::error!("[{name}] Giving up after {MAX_ERROR_RETRIES} restart attempts: {e}");
                        update_status(BotStatus::Error(e.to_string()));
                        break;
                    }
                    let backoff = std::cmp::min(60, 5u64 * (1 << (error_retries - 1)));
                    tracing::warn!("[{name}] Bot error: {e}; retry {error_retries}/{MAX_ERROR_RETRIES} in {backoff}s");
                    update_status(BotStatus::Error(format!("{e} (retrying in {backoff}s)")));
                    // Wait, but wake early if the user requests shutdown.
                    for _ in 0..(backoff * 10) {
                        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        update_status(BotStatus::Stopped);
                        break;
                    }
                    update_status(BotStatus::Starting);
                    continue;
                }
            }
            // Reset the error counter after any clean (Ok) outcome.
            error_retries = 0;
            break;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{start_disposition, StartAction};

    #[test]
    fn fresh_start_when_no_live_thread() {
        assert_eq!(start_disposition(false, false), StartAction::Fresh);
        // Stale stop flag from a finished thread doesn't matter.
        assert_eq!(start_disposition(false, true), StartAction::Fresh);
    }

    #[test]
    fn running_without_stop_is_a_noop() {
        assert_eq!(start_disposition(true, false), StartAction::AlreadyRunning);
    }

    #[test]
    fn start_after_stop_signal_defers_instead_of_noop() {
        // Quick Stop-then-Start from the tray: thread alive but already told
        // to stop. Must queue the start, not silently drop it.
        assert_eq!(start_disposition(true, true), StartAction::DeferAfterStop);
    }
}
