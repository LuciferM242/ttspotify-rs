//! System tray integration using wxDragon's TaskBarIcon.
//!
//! Creates a tray icon with a popup menu for managing multiple bot instances.
//! Uses on_right_up + popup_menu() for dynamic menu with submenus (set_popup_menu
//! doesn't reliably support submenus).

use std::cell::RefCell;
use std::rc::Rc;

use wxdragon::prelude::*;
use wxdragon::timer::Timer;

use crate::config::{config_dir, BotConfig};
use crate::gui::config_dialog;
use crate::gui::icon::create_icon;
use crate::gui::manager::{BotManager, BotStatus};

// Fixed menu IDs
const ID_EXIT: i32 = 1;
const ID_ADD_SERVER: i32 = 2;
const ID_SPOTIFY_AUTH: i32 = 3;
const ID_YT_INSTALL: i32 = 4;
const ID_YT_UPDATE: i32 = 5;
const ID_CHECK_UPDATES: i32 = 6;
const ID_SETTINGS: i32 = 7;

// Per-bot menu IDs: base + (bot_index * 10) + action
const ID_BOT_BASE: i32 = 1000;
const ACTION_START: i32 = 0;
const ACTION_STOP: i32 = 1;
const ACTION_RESTART: i32 = 2;
const ACTION_LOGS: i32 = 3;
const ACTION_CONFIG: i32 = 4;

/// Run the tray application. This blocks until the user exits.
pub fn run() {
    // Init tray-level logging (file only, no console)
    let log_dir = config_dir().join("logs");
    let _log_guard = crate::logging::init_file_logging(&log_dir, "tray");

    let _ = wxdragon::main(|_| {
        // Hidden frame keeps the wxDragon event loop alive.
        let hidden_frame = Frame::builder()
            .with_title("TT Spotify")
            .with_size(Size::new(1, 1))
            .build();

        let (status_tx, status_rx) = crossbeam_channel::unbounded::<(String, BotStatus)>();
        let manager = Rc::new(RefCell::new(BotManager::new(status_tx)));

        // Create tray icon
        let taskbar = TaskBarIcon::builder()
            .with_icon_type(TaskBarIconType::Default)
            .build();

        let icon = create_icon();
        taskbar.set_icon(&icon, "TT Spotify");

        // Initial tooltip (no bots started yet).
        let tooltip = build_tooltip(&manager.borrow().statuses());
        taskbar.set_icon(&icon, &tooltip);

        // Set on Exit. Guards the gated-startup path: if the update dialog is
        // still open when the app exits, its dismiss callback must not start
        // bots into a shutting-down app.
        let exiting = Rc::new(std::cell::Cell::new(false));

        // Before a successful update relaunches (process::exit skips
        // on_destroy), stop all bots with the same bounded wait as app exit so
        // they disconnect cleanly and persist config.
        let mgr_relaunch = manager.clone();
        crate::gui::update_dialog::set_prepare_relaunch(move || {
            mgr_relaunch
                .borrow_mut()
                .stop_all_with_timeout(std::time::Duration::from_secs(3));
        });

        // Startup update check gates bot startup, but only when bots are actually
        // configured — there's nothing to gate on a fresh install, so skip the
        // check and go straight to start_bots (which prompts to create the first
        // config) with no network wait. When configured and enabled, run the
        // check on a worker thread; the status timer below acts on the result
        // (show the dialog and hold off starting bots, or start them if none).
        let has_configs = !crate::config::list_configs().is_empty();
        let update_rx = if has_configs && crate::settings::load().check_updates_on_startup {
            let (tx, rx) = crossbeam_channel::unbounded::<Option<crate::update::UpdateInfo>>();
            std::thread::spawn(move || {
                let _ = tx.send(check_for_update());
            });
            Some(rx)
        } else {
            start_bots(&manager, &taskbar, &icon, hidden_frame);
            None
        };

        // --- Right-click: build fresh menu, bind handler ON THE MENU, show it ---
        // popup_menu() is synchronous (blocks until dismissed). Events from
        // popup_menu don't route through TaskBarIcon's on_menu, so we bind
        // the handler directly on the Menu via on_selected. The handler and
        // menu live on the stack during the blocking popup_menu call.
        let mgr_popup = manager.clone();
        let taskbar_popup = taskbar.clone();
        let icon_popup = icon.clone();
        taskbar.on_right_up(move |_| {
            let mut menu = build_menu(&mgr_popup.borrow());

            // Bind menu event handler directly on the menu
            let mgr = mgr_popup.clone();
            let tb = taskbar_popup.clone();
            let ic = icon_popup.clone();
            menu.on_selected(move |event| {
                let id = event.get_id();
                handle_menu_action(id, &mgr, &tb, &ic, hidden_frame);
            });

            taskbar_popup.popup_menu(&mut menu);

            // Update tooltip after menu dismissed
            let tooltip = build_tooltip(&mgr_popup.borrow().statuses());
            taskbar_popup.set_icon(&icon_popup, &tooltip);
        });

        // --- Timer: poll status channel + one-shot startup update result ---
        let mgr_timer = manager.clone();
        let taskbar_timer = taskbar.clone();
        let icon_timer = icon.clone();
        let exiting_timer = exiting.clone();
        let timer = Timer::new(&hidden_frame);
        let update_done = std::cell::Cell::new(false);
        timer.on_tick(move |_| {
            let mut changed = false;
            while status_rx.try_recv().is_ok() {
                changed = true;
            }
            if changed {
                let tooltip = build_tooltip(&mgr_timer.borrow().statuses());
                taskbar_timer.set_icon(&icon_timer, &tooltip);
            }

            // Startup update result (handled once): show the dialog and gate bot
            // startup on the user's choice; start bots outright if no update.
            if let Some(rx) = &update_rx {
                if !update_done.get() {
                    if let Ok(result) = rx.try_recv() {
                        update_done.set(true);
                        match result {
                            Some(info) => {
                                let m = mgr_timer.clone();
                                let tb = taskbar_timer.clone();
                                let ic = icon_timer.clone();
                                let exiting = exiting_timer.clone();
                                crate::gui::update_dialog::show_update_available(info, move || {
                                    if !exiting.get() {
                                        start_bots(&m, &tb, &ic, hidden_frame);
                                    }
                                });
                            }
                            None => {
                                start_bots(&mgr_timer, &taskbar_timer, &icon_timer, hidden_frame)
                            }
                        }
                    }
                }
            }
        });
        timer.start(200, false);

        // Cleanup on exit
        let taskbar_destroy = taskbar.clone();
        hidden_frame.on_destroy(move |evt| {
            exiting.set(true);
            timer.stop();
            // Wait (bounded) for bots to disconnect cleanly from the server and
            // persist config before the process exits, instead of dropping them
            // mid-shutdown. Capped so the GUI can never hang on exit.
            manager
                .borrow_mut()
                .stop_all_with_timeout(std::time::Duration::from_secs(3));
            taskbar_destroy.destroy();
            evt.skip(true);
        });
    });
}

/// Load configs and start every bot, or prompt to create the first config if
/// none exist, then refresh the tray tooltip. Called at startup — directly when
/// the update check is off, or after the user declines an available update.
fn start_bots(manager: &Rc<RefCell<BotManager>>, taskbar: &TaskBarIcon, icon: &Bitmap, parent: Frame) {
    let names = { manager.borrow_mut().load_configs() };
    if names.is_empty() {
        use MessageDialogStyle as MDS;
        let res = MessageDialog::builder(
            &parent,
            "No config files found.\nWould you like to create one now?\n\nYou can also create one later from the tray menu (Add Server).",
            "TT Spotify - No Configurations",
        )
        .with_style(MDS::YesNo | MDS::IconQuestion)
        .build()
        .show_modal();
        if res == ID_YES {
            let mgr_save = manager.clone();
            let tb = taskbar.clone();
            let ic = icon.clone();
            config_dialog::open_config_dialog(BotConfig::default(), None, move |_path| {
                let mut m = mgr_save.borrow_mut();
                let new_names = m.load_configs();
                for name in &new_names {
                    m.start(name);
                }
                drop(m);
                let tooltip = build_tooltip(&mgr_save.borrow().statuses());
                tb.set_icon(&ic, &tooltip);
            });
        }
    } else {
        let mut m = manager.borrow_mut();
        for name in &names {
            m.start(name);
        }
    }
    let tooltip = build_tooltip(&manager.borrow().statuses());
    taskbar.set_icon(icon, &tooltip);
}

/// Blocking startup update check: runs `update::check()` on a fresh runtime with
/// an 8s cap so a slow or unreachable network can't stall bot startup. Returns
/// `Some` only when a newer release is definitively available.
fn check_for_update() -> Option<crate::update::UpdateInfo> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        match tokio::time::timeout(std::time::Duration::from_secs(8), crate::update::check()).await {
            Ok(Ok(info)) => info,
            _ => None,
        }
    })
}

/// Process a menu item click by ID.
fn handle_menu_action(
    id: i32,
    mgr: &Rc<RefCell<BotManager>>,
    taskbar: &TaskBarIcon,
    icon: &Bitmap,
    hidden_frame: Frame,
) {
    match id {
        ID_EXIT => {
            // close triggers on_destroy which does non-blocking stop
            hidden_frame.close(true);
        }
        ID_ADD_SERVER => {
            let mgr_save = mgr.clone();
            let tb = taskbar.clone();
            let ic = icon.clone();
            config_dialog::open_config_dialog(
                BotConfig::default(),
                None,
                move |_path| {
                    let mut m = mgr_save.borrow_mut();
                    let new_names = m.load_configs();
                    for name in &new_names {
                        m.start(name);
                    }
                    drop(m);
                    let tooltip = build_tooltip(&mgr_save.borrow().statuses());
                    tb.set_icon(&ic, &tooltip);
                },
            );
        }
        ID_SPOTIFY_AUTH => {
            // The browser drives the login UI, so run silently on a worker
            // thread and let the result land in the log.
            std::thread::spawn(|| {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("Spotify auth: tokio runtime failed: {e}");
                        return;
                    }
                };
                let mut auth = crate::spotify::auth::SpotifyAuth::new();
                match rt.block_on(auth.reauthenticate()) {
                    Ok(_) => tracing::info!("Spotify re-authentication successful"),
                    Err(e) => tracing::error!("Spotify re-authentication failed: {e}"),
                }
            });
        }
        ID_YT_INSTALL => {
            crate::gui::progress::run_progress_dialog(
                "Install YouTube tools",
                |p| crate::gui::progress::youtube_install(p),
                |_| {},
            );
        }
        ID_YT_UPDATE => {
            crate::gui::progress::run_progress_dialog(
                "Update YouTube tools",
                |p| crate::gui::progress::youtube_update(p),
                |_| {},
            );
        }
        ID_CHECK_UPDATES => {
            // Manual check announces "up to date" / errors.
            spawn_update_check(true);
        }
        ID_SETTINGS => {
            crate::gui::settings_dialog::open_settings_dialog();
        }
        _ if id >= ID_BOT_BASE => {
            let bot_idx = ((id - ID_BOT_BASE) / 10) as usize;
            let action = (id - ID_BOT_BASE) % 10;
            let statuses = mgr.borrow().statuses();
            if let Some((name, _)) = statuses.get(bot_idx) {
                let name = name.clone();
                match action {
                    ACTION_START => {
                        mgr.borrow_mut().start(&name);
                    }
                    ACTION_STOP => {
                        mgr.borrow_mut().stop_nonblocking(&name);
                    }
                    ACTION_RESTART => {
                        mgr.borrow_mut().restart_nonblocking(&name);
                    }
                    ACTION_LOGS => {
                        let log_path = config_dir().join("logs").join(format!("{name}.log"));
                        open_file(&log_path);
                    }
                    ACTION_CONFIG => {
                        if let Some(path) = mgr.borrow().config_path(&name) {
                            let cfg = BotConfig::load(path.to_str().unwrap_or(""))
                                .unwrap_or_default();
                            let mgr_save = mgr.clone();
                            let tb = taskbar.clone();
                            let ic = icon.clone();
                            let name_cb = name.clone();
                            config_dialog::open_config_dialog(
                                cfg,
                                Some(path),
                                move |_| {
                                    // Apply the edit (and any freshly installed
                                    // tools) by restarting a running bot.
                                    {
                                        let mut m = mgr_save.borrow_mut();
                                        if m.is_running(&name_cb) {
                                            m.restart_nonblocking(&name_cb);
                                        }
                                    }
                                    let tooltip =
                                        build_tooltip(&mgr_save.borrow().statuses());
                                    tb.set_icon(&ic, &tooltip);
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Build tooltip text from bot statuses.
fn build_tooltip(statuses: &[(String, BotStatus)]) -> String {
    if statuses.is_empty() {
        return "TT Spotify - no bots configured".to_string();
    }

    // Single bot: show its name and status directly
    if statuses.len() == 1 {
        let (name, status) = &statuses[0];
        return format!("TT Spotify - {name}: {status}");
    }

    // Multiple bots: show summary counts
    let mut connected = 0u32;
    let mut playing = 0u32;
    let mut failed = 0u32;
    let mut stopped = 0u32;
    let mut starting = 0u32;

    for (_, status) in statuses {
        match status {
            BotStatus::Connected => connected += 1,
            BotStatus::Playing(_) => {
                connected += 1;
                playing += 1;
            }
            BotStatus::Error(_) | BotStatus::Disconnected => failed += 1,
            BotStatus::Stopped => stopped += 1,
            BotStatus::Starting | BotStatus::Connecting | BotStatus::Authenticating => starting += 1,
        }
    }

    let total = statuses.len();
    let mut parts = Vec::new();
    if connected > 0 {
        parts.push(format!("{connected} connected"));
    }
    if playing > 0 {
        parts.push(format!("{playing} playing"));
    }
    if starting > 0 {
        parts.push(format!("{starting} starting"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if stopped > 0 {
        parts.push(format!("{stopped} stopped"));
    }

    if parts.is_empty() {
        format!("TT Spotify - {total} bots")
    } else {
        format!("TT Spotify - {}", parts.join(", "))
    }
}

/// Build the tray popup menu with per-bot submenus.
fn build_menu(manager: &BotManager) -> Menu {
    let statuses = manager.statuses();
    let menu = Menu::builder().build();

    for (idx, (name, status)) in statuses.iter().enumerate() {
        let running = manager.is_running(name);
        let base_id = ID_BOT_BASE + idx as i32 * 10;

        let submenu = Menu::builder()
            .append_item(base_id + ACTION_START, "Start", "Start this bot")
            .append_item(base_id + ACTION_STOP, "Stop", "Stop this bot")
            .append_item(base_id + ACTION_RESTART, "Restart", "Restart this bot")
            .append_separator()
            .append_item(base_id + ACTION_LOGS, "View Logs", "Open log file")
            .append_item(base_id + ACTION_CONFIG, "Edit Config", "Open config editor")
            .build();

        submenu.enable_item(base_id + ACTION_START, !running);
        submenu.enable_item(base_id + ACTION_STOP, running);

        let label = format!("{name} - {status}");
        menu.append_submenu(submenu, &label, "");
    }

    if !statuses.is_empty() {
        menu.append_separator();
    }

    let spotify_signed_in = crate::spotify::auth::SpotifyAuth::new().has_cached_credentials();
    let spotify_label = if spotify_signed_in {
        "Spotify: signed in"
    } else {
        "Spotify: not signed in"
    };
    let spotify_menu = Menu::builder()
        .append_item(ID_SPOTIFY_AUTH, "Sign in / re-authenticate", "Open the browser to log in to Spotify")
        .build();
    menu.append_submenu(spotify_menu, spotify_label, "");

    let yt_installed = crate::youtube::setup::resolve_paths()
        .map(|p| crate::youtube::setup::is_installed(&p))
        .unwrap_or(false);
    let yt_label = if yt_installed {
        "YouTube tools: installed"
    } else {
        "YouTube tools: not installed"
    };
    let yt_menu = Menu::builder()
        .append_item(ID_YT_INSTALL, "Install tools", "Download yt-dlp and bgutil-pot")
        .append_item(ID_YT_UPDATE, "Update tools", "Update yt-dlp and bgutil-pot")
        .build();
    yt_menu.enable_item(ID_YT_INSTALL, !yt_installed);
    yt_menu.enable_item(ID_YT_UPDATE, yt_installed);
    menu.append_submenu(yt_menu, yt_label, "");

    menu.append_separator();
    menu.append(ID_ADD_SERVER, "Add Server", "", ItemKind::Normal);
    menu.append(ID_CHECK_UPDATES, "Check for updates", "", ItemKind::Normal);
    menu.append(ID_SETTINGS, "Settings", "", ItemKind::Normal);
    menu.append_separator();
    menu.append(ID_EXIT, "Exit", "", ItemKind::Normal);

    menu
}

/// Run a GitHub update check on a worker thread and marshal the result back to
/// the GUI thread via `call_after`. `announce_up_to_date` controls whether the
/// "you're up to date" / error boxes appear (manual check) or stay silent
/// (startup check).
fn spawn_update_check(announce_up_to_date: bool) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("Update check: tokio runtime failed: {e}");
                return;
            }
        };
        let result = rt.block_on(crate::update::check());
        wxdragon::call_after(Box::new(move || match result {
            Ok(Some(info)) => crate::gui::update_dialog::show_update_available(info, || {}),
            Ok(None) => {
                if announce_up_to_date {
                    crate::gui::update_dialog::show_up_to_date();
                }
            }
            Err(e) => {
                if announce_up_to_date {
                    crate::gui::update_dialog::show_check_error(&e.to_string());
                } else {
                    tracing::warn!("Startup update check failed: {e}");
                }
            }
        }));
    });
}

/// Open a file with the default Windows application.
/// Log files use daily rotation with a date prefix (e.g. `2026-04-07.config.log`),
/// so we find the most recent file matching the suffix.
fn open_file(path: &std::path::Path) {
    let target = if path.exists() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent() {
        let suffix = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut matches: Vec<_> = std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(suffix) && n != suffix)
            })
            .collect();
        matches.sort_by_key(|b| std::cmp::Reverse(b.file_name()));
        match matches.first() {
            Some(entry) => entry.path(),
            None => return,
        }
    } else {
        return;
    };
    let abs_path = std::fs::canonicalize(&target).unwrap_or(target);
    // `start` (via cmd) keeps the shell "open" verb: default app, or the
    // "Open with" picker when no association is set. CREATE_NO_WINDOW hides the
    // cmd console that would otherwise flash before the target app appears.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &abs_path.display().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}
