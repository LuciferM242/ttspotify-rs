//! Bot instance manager for the system tray.
//!
//! Manages multiple bot instances, each running in its own thread with a
//! tokio runtime. Status updates flow back via crossbeam channel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::bot::runner::{BotExit, RunnerEvent};

/// Status of a bot instance, displayed in tray menu and tooltip.
#[derive(Debug, Clone)]
pub enum BotStatus {
    Stopped,
    Starting,
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
                let status = inst.status.lock().unwrap().clone();
                (name.clone(), status)
            })
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    pub fn start(&mut self, name: &str) -> bool {
        let inst = match self.instances.get_mut(name) {
            Some(i) => i,
            None => return false,
        };
        if inst.is_running() {
            return false;
        }

        let config_path = inst.config_path.clone();
        let status = inst.status.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown.clone();
        let status_tx = self.status_tx.clone();
        let bot_name = name.to_string();

        *status.lock().unwrap() = BotStatus::Starting;
        let _ = status_tx.send((bot_name.clone(), BotStatus::Starting));

        let handle = thread::Builder::new()
            .name(format!("bot-{name}"))
            .spawn(move || {
                run_bot_instance(config_path, status, shutdown_flag, status_tx, bot_name);
            })
            .ok();

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
        *inst.status.lock().unwrap() = BotStatus::Stopped;
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

        *status.lock().unwrap() = BotStatus::Starting;
        let _ = status_tx.send((bot_name.clone(), BotStatus::Starting));

        let handle = thread::Builder::new()
            .name(format!("bot-{name}"))
            .spawn(move || {
                // Wait for old thread to finish
                if let Some(h) = old_handle {
                    let _ = h.join();
                }
                thread::sleep(std::time::Duration::from_millis(500));
                run_bot_instance(config_path, status, shutdown_flag, status_tx, bot_name);
            })
            .ok();

        inst.thread = handle;
        inst.shutdown = Some(shutdown);
    }

    pub fn restart(&mut self, name: &str) -> bool {
        self.stop(name);
        thread::sleep(std::time::Duration::from_millis(500));
        self.start(name)
    }

    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            self.stop(&name);
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
    let update_status = |new_status: BotStatus| {
        *status.lock().unwrap() = new_status.clone();
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
                RunnerEvent::Connected | RunnerEvent::Idle => BotStatus::Connected,
                RunnerEvent::Playing(track) => BotStatus::Playing(track),
                RunnerEvent::Disconnected => BotStatus::Disconnected,
                RunnerEvent::Error(msg) => BotStatus::Error(msg),
            };
            *bridge_status.lock().unwrap() = new_status.clone();
            let _ = bridge_tx.send((bridge_name.clone(), new_status));
        }
    });

    let config_path_str = config_path.to_str().unwrap_or("").to_string();
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
            )
            .await
            {
                Ok(BotExit::Restart) => {
                    // Bot requested restart (user sent "rs" command)
                    tracing::info!("[{name}] Restart requested, restarting...");
                    update_status(BotStatus::Starting);
                    shutdown.store(false, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    continue;
                }
                Ok(_) => {
                    update_status(BotStatus::Stopped);
                }
                Err(e) => {
                    update_status(BotStatus::Error(e.to_string()));
                }
            }
            break;
        }
    });
}
