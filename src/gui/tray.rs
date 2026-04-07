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

        // Load configs and auto-start bots, or prompt to create one
        {
            let mut mgr = manager.borrow_mut();
            let names = mgr.load_configs();

            if names.is_empty() {
                drop(mgr);

                use MessageDialogStyle as MDS;
                let res = MessageDialog::builder(
                    &hidden_frame,
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
            } else {
                for name in &names {
                    mgr.start(name);
                }
            }
        }

        // Update tooltip with initial status
        let tooltip = build_tooltip(&manager.borrow().statuses());
        taskbar.set_icon(&icon, &tooltip);

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

        // --- Timer: poll status channel and update tooltip ---
        let mgr_timer = manager.clone();
        let taskbar_timer = taskbar.clone();
        let icon_timer = icon.clone();
        let timer = Timer::new(&hidden_frame);
        timer.on_tick(move |_| {
            let mut changed = false;
            while status_rx.try_recv().is_ok() {
                changed = true;
            }
            if changed {
                let tooltip = build_tooltip(&mgr_timer.borrow().statuses());
                taskbar_timer.set_icon(&icon_timer, &tooltip);
            }
        });
        timer.start(200, false);

        // Cleanup on exit
        let taskbar_destroy = taskbar.clone();
        hidden_frame.on_destroy(move |evt| {
            timer.stop();
            manager.borrow_mut().stop_all_nonblocking();
            taskbar_destroy.destroy();
            evt.skip(true);
        });
    });
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
                            config_dialog::open_config_dialog(
                                cfg,
                                Some(path),
                                move |_| {
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

    menu.append(ID_ADD_SERVER, "Add Server", "", ItemKind::Normal);
    menu.append_separator();
    menu.append(ID_EXIT, "Exit", "", ItemKind::Normal);

    menu
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
        matches.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        match matches.first() {
            Some(entry) => entry.path(),
            None => return,
        }
    } else {
        return;
    };
    let abs_path = std::fs::canonicalize(&target).unwrap_or(target);
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &abs_path.display().to_string()])
        .spawn();
}
