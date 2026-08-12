//! Native Win32 system tray, on winsafe.
//!
//! Ported from `gui/tray.rs`, which used wxWidgets. Behaviour is the same
//! except where the wx version was working around its own toolkit:
//!
//! - The menu is built from `menu::MenuModel`, so a command id names a bot
//!   rather than a list position. See that module for the bug this fixes.
//! - `TrackPopupMenu` with `TPM::RETURNCMD` returns the chosen id directly, so
//!   there is no handler to bind to the menu. wx needed one because its
//!   `popup_menu` did not route through the tray icon's own menu events.
//! - The icon comes from an embedded resource instead of being drawn at
//!   runtime.
//! - `TaskbarCreated` is handled, so the icon comes back when Explorer
//!   restarts. Nothing did that before.
//! - `NOTIFYICON_VERSION_4` is requested, which is what makes the shell report
//!   keyboard invocation of the icon as `WM_CONTEXTMENU` with a usable
//!   position. Without it, reaching the menu from the keyboard is unreliable.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use winsafe::prelude::*;
use winsafe::{self as w, co, gui, msg};

use crate::config::BotConfig;
use crate::gui_native::manager::{BotManager, BotStatus};
use crate::gui_native::facts::FactsCache;
use crate::gui_native::menu::{BotAction, MenuAction, MenuBuilder, MenuEntry, MenuFacts};
use crate::gui_native::tooltip::build_tooltip;

/// Our icon's id within this window. Any constant works; it only has to be
/// stable between the add, the updates and the delete.
const TRAY_ICON_ID: u32 = 1;

/// Icon resource id, matching `IDI_TRAY` in `assets/tray.rc`.
const IDI_TRAY: u16 = 1;

/// Private message the shell sends us about the icon.
const WM_TRAY_CALLBACK: u32 = 0x8000 + 1; // WM_APP + 1

/// Timer that drains bot status updates.
const TIMER_STATUS: usize = 1;
const TIMER_STATUS_MS: u32 = 200;

/// Shell notifications we act on, absent from winsafe's constants.
const NIN_SELECT: u16 = 0x0400; // WM_USER + 0
const NIN_KEYSELECT: u16 = 0x0401; // WM_USER + 1

/// Ask the shell for the modern callback contract.
const NOTIFYICON_VERSION_4: u32 = 4;

/// Everything the event closures share.
struct Tray {
    manager: Rc<RefCell<BotManager>>,
    menus: RefCell<MenuBuilder>,
    /// Spotify/YouTube state for the menu, read from disk at most occasionally.
    facts: RefCell<FactsCache>,
    status_rx: crossbeam_channel::Receiver<(String, BotStatus)>,
    /// Kept alive for as long as the icon refers to it.
    icon: w::guard::DestroyIconGuard,
    /// Result of the startup update check, when one was started.
    update_rx: Option<crossbeam_channel::Receiver<Option<crate::update::UpdateInfo>>>,
    /// The startup update result is acted on once.
    update_done: Cell<bool>,
    /// Set once shutdown begins, so a dialog dismissed during exit cannot
    /// start bots into a closing app.
    exiting: Cell<bool>,
}

/// Run the tray. Blocks until the user exits.
pub fn run() {
    // Sort a legacy flat data folder before anything reads from it; the report
    // is logged once logging exists, just below.
    let layout_migration = crate::paths::migrate_data_layout();
    let log_dir = crate::paths::logs_dir();
    let _log_guard = crate::logging::init_file_logging(&log_dir, "tray");
    crate::paths::log_migration(&layout_migration);

    let wnd = gui::WindowMain::new(gui::WindowMainOpts {
        title: "TT Spotify",
        // Never shown. TOOLWINDOW keeps it out of the taskbar and Alt-Tab; a
        // plain hidden window would still appear there. It must be a real
        // top-level window rather than a message-only one, because
        // TaskbarCreated is broadcast and message-only windows do not receive
        // broadcasts.
        ex_style: co::WS_EX::TOOLWINDOW,
        style: co::WS::OVERLAPPED,
        size: (0, 0),
        ..Default::default()
    });

    let hinst = w::HINSTANCE::GetModuleHandle(None).expect("module handle");
    let icon = match hinst.LoadIcon(w::IdIdiStr::Id(IDI_TRAY)) {
        Ok(i) => i,
        Err(e) => {
            // Losing the icon should not stop the bots running, so fall back to
            // the stock application icon rather than giving up.
            tracing::error!("Could not load the tray icon resource: {e}; using the stock icon");
            w::HINSTANCE::NULL
                .LoadIcon(w::IdIdiStr::Idi(co::IDI::APPLICATION))
                .expect("stock icon")
        }
    };

    let (status_tx, status_rx) = crossbeam_channel::unbounded::<(String, BotStatus)>();
    let manager = Rc::new(RefCell::new(BotManager::new(status_tx)));

    // Only gate startup on an update check when there is something to gate:
    // a fresh install has no bots to delay, so it goes straight to the
    // "create a config?" prompt with no network wait.
    let has_configs = !crate::config::list_configs().is_empty();
    let update_rx = if has_configs && crate::settings::load().check_updates_on_startup {
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let _ = tx.send(check_for_update());
        });
        Some(rx)
    } else {
        None
    };

    let tray = Rc::new(Tray {
        manager,
        menus: RefCell::new(MenuBuilder::new()),
        facts: RefCell::new(FactsCache::new()),
        status_rx,
        icon,
        update_rx,
        update_done: Cell::new(false),
        exiting: Cell::new(false),
    });

    {
        // The updater replaces the exe and relaunches with process::exit, which
        // skips wm_destroy, so the bots have to be stopped explicitly first.
        let manager = tray.manager.clone();
        crate::gui_native::update_dialog::set_prepare_relaunch(move || {
            manager
                .borrow_mut()
                .stop_all_with_timeout(std::time::Duration::from_secs(3));
        });
    }

    // TaskbarCreated is broadcast when Explorer restarts; without re-adding the
    // icon it is gone for the rest of the session.
    let taskbar_created = w::RegisterWindowMessage("TaskbarCreated").unwrap_or(0);

    register_events(&wnd, &tray, taskbar_created, has_configs);

    if let Err(e) = wnd.run_main(Some(co::SW::HIDE)) {
        tracing::error!("Tray message loop ended with an error: {e}");
    }
}

/// Wire up every window message the tray reacts to. Must happen before the
/// window is created, which is why it is separate from `run`.
fn register_events(
    wnd: &gui::WindowMain,
    tray: &Rc<Tray>,
    taskbar_created: u32,
    has_configs: bool,
) {
    // --- Created: show the icon, start the status timer, start the bots ---
    {
        let tray = tray.clone();
        let wnd2 = wnd.clone();
        wnd.on().wm_create(move |_| {
            add_icon(wnd2.hwnd(), &tray.icon);
            update_tooltip(wnd2.hwnd(), &tray);
            let _ = wnd2.hwnd().SetTimer(TIMER_STATUS, TIMER_STATUS_MS, None);
            // With an update check in flight, the timer starts the bots once
            // the user has answered; otherwise there is nothing to wait for.
            if !has_configs || tray.update_rx.is_none() {
                start_bots(&wnd2, &tray);
            }
            Ok(0)
        });
    }

    // --- The shell talking to us about the icon ---
    {
        let tray = tray.clone();
        let wnd2 = wnd.clone();
        wnd.on().wm(unsafe { co::WM::from_raw(WM_TRAY_CALLBACK) }, move |p: msg::Wm| {
            // Under version 4 the low word of lParam is the notification and
            // wParam carries the screen position the shell wants a menu at.
            // Taking the position from the shell rather than the cursor is what
            // makes keyboard invocation land in the right place.
            let notification = (p.lparam as u32 & 0xffff) as u16;
            let x = (p.wparam & 0xffff) as i16 as i32;
            let y = ((p.wparam >> 16) & 0xffff) as i16 as i32;
            const WM_CONTEXTMENU: u16 = 0x007b;
            match notification {
                WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => {
                    // Left click and Enter both open the menu: with no main
                    // window to show, there is no more useful primary action,
                    // and a dead click reads as a broken icon.
                    show_menu(wnd2.hwnd(), &wnd2, &tray, w::POINT::with(x, y));
                }
                _ => {}
            }
            Ok(0)
        });
    }

    // --- Explorer restarted: put the icon back ---
    if taskbar_created != 0 {
        let tray = tray.clone();
        let wnd2 = wnd.clone();
        wnd.on()
            .wm(unsafe { co::WM::from_raw(taskbar_created) }, move |_| {
                tracing::info!("Explorer restarted; restoring the tray icon");
                add_icon(wnd2.hwnd(), &tray.icon);
                update_tooltip(wnd2.hwnd(), &tray);
                Ok(0)
            });
    }

    // --- Status updates and the one-shot startup update result ---
    {
        let tray = tray.clone();
        let wnd2 = wnd.clone();
        wnd.on().wm_timer(TIMER_STATUS, move || {
            let mut changed = false;
            while tray.status_rx.try_recv().is_ok() {
                changed = true;
            }
            if changed {
                update_tooltip(wnd2.hwnd(), &tray);
            }
            poll_startup_update(&wnd2, &tray);
            Ok(())
        });
    }

    // --- Exit: take the icon down and let the bots disconnect cleanly ---
    {
        let tray = tray.clone();
        let wnd2 = wnd.clone();
        wnd.on().wm_destroy(move || {
            tray.exiting.set(true);
            let _ = wnd2.hwnd().KillTimer(TIMER_STATUS);
            remove_icon(wnd2.hwnd());
            // Bounded: a bot with a live session sees the flag within one poll
            // and leaves the server tidily, but exit must never hang on one
            // that does not.
            tray.manager
                .borrow_mut()
                .stop_all_with_timeout(std::time::Duration::from_secs(3));
            w::PostQuitMessage(0);
            Ok(())
        });
    }
}

/// Base icon data. `uFlags` is set by each caller for what it is changing.
fn icon_data(hwnd: &w::HWND) -> w::NOTIFYICONDATA {
    let mut nid = w::NOTIFYICONDATA::default();
    nid.hWnd = unsafe { hwnd.raw_copy() };
    nid.uID = TRAY_ICON_ID;
    nid
}

fn add_icon(hwnd: &w::HWND, icon: &w::guard::DestroyIconGuard) {
    let mut nid = icon_data(hwnd);
    // SHOWTIP is required, not optional: asking for NOTIFYICON_VERSION_4
    // below suppresses the standard tooltip unless it is set, and a screen
    // reader takes the icon's accessible name from that tooltip. Without it
    // the icon reads as a bare "TT Spotify" with no status.
    nid.uFlags = co::NIF::ICON | co::NIF::MESSAGE | co::NIF::TIP | co::NIF::SHOWTIP;
    nid.uCallbackMessage = unsafe { co::WM::from_raw(WM_TRAY_CALLBACK) };
    nid.hIcon = unsafe { icon.raw_copy() };
    nid.set_szTip("TT Spotify");
    if let Err(e) = w::Shell_NotifyIcon(co::NIM::ADD, &nid) {
        tracing::error!("Could not add the tray icon: {e}");
        return;
    }
    // Opt into the modern callback contract. Only meaningful after ADD.
    let mut ver = icon_data(hwnd);
    ver.uVersion = NOTIFYICON_VERSION_4;
    if let Err(e) = w::Shell_NotifyIcon(co::NIM::SETVERSION, &ver) {
        tracing::warn!("Tray icon is using the legacy callback contract: {e}");
    }
}

fn remove_icon(hwnd: &w::HWND) {
    let nid = icon_data(hwnd);
    if let Err(e) = w::Shell_NotifyIcon(co::NIM::DELETE, &nid) {
        tracing::warn!("Could not remove the tray icon: {e}");
    }
}

/// Push the current bot statuses into the icon's tooltip.
fn update_tooltip(hwnd: &w::HWND, tray: &Tray) {
    let text = build_tooltip(&tray.manager.borrow().statuses());
    let mut nid = icon_data(hwnd);
    // SHOWTIP again: it is a property of the notification, so leaving it off
    // here would re-suppress the tooltip on the first status change.
    nid.uFlags = co::NIF::TIP | co::NIF::SHOWTIP;
    nid.set_szTip(&text);
    if let Err(e) = w::Shell_NotifyIcon(co::NIM::MODIFY, &nid) {
        // Warn, not debug: the tooltip is the only status a tray-only app
        // shows, and the default filter would have hidden a debug line.
        tracing::warn!("Could not update the tray tooltip: {e}");
    }
}

/// Build and show the popup menu, then act on what was chosen.
fn show_menu(hwnd: &w::HWND, wnd: &gui::WindowMain, tray: &Rc<Tray>, at: w::POINT) {
    let statuses = tray.manager.borrow().statuses();
    let bots: Vec<(String, String, bool)> = statuses
        .iter()
        .map(|(name, status)| {
            let running = tray.manager.borrow().is_running(name);
            (name.clone(), status.to_string(), running)
        })
        .collect();

    // Read from the cache: both of these answers come from disk, and this runs
    // on the message loop every time the menu opens.
    let facts = tray.facts.borrow_mut().get(|| MenuFacts {
        spotify_signed_in: crate::spotify::auth::SpotifyAuth::new().has_cached_credentials(),
        youtube_installed: crate::youtube::setup::resolve_paths()
            .map(|p| crate::youtube::setup::is_installed(&p))
            .unwrap_or(false),
    });
    let model = tray.menus.borrow_mut().build(&bots, facts);

    let mut hmenu = match build_hmenu(&model.entries) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("Could not build the tray menu: {e}");
            return;
        }
    };

    // Required, and easy to miss: without it the menu stays up when the user
    // clicks elsewhere, because the menu's owner is not the foreground window.
    hwnd.SetForegroundWindow();

    let picked = hmenu.TrackPopupMenu(
        co::TPM::RETURNCMD | co::TPM::RIGHTBUTTON,
        at,
        hwnd,
    );
    let _ = hmenu.DestroyMenu();

    match picked {
        Ok(Some(id)) => {
            if let Some(action) = model.action(id as u16) {
                handle_action(hwnd, wnd, tray, action.clone());
            }
        }
        Ok(None) => {} // dismissed
        Err(e) => tracing::error!("Could not show the tray menu: {e}"),
    }
    update_tooltip(hwnd, tray);
}

/// Turn the menu model into a real HMENU. Submenus are attached with MF_POPUP,
/// so destroying the returned menu destroys them too.
fn build_hmenu(entries: &[MenuEntry]) -> w::SysResult<w::HMENU> {
    let menu = w::HMENU::CreatePopupMenu()?;
    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                menu.AppendMenu(co::MF::SEPARATOR, w::IdMenu::None, w::BmpPtrStr::None)?;
            }
            MenuEntry::Item { id, label, enabled } => {
                let flags = if *enabled {
                    co::MF::STRING
                } else {
                    co::MF::STRING | co::MF::GRAYED
                };
                menu.AppendMenu(flags, w::IdMenu::Id(*id), w::BmpPtrStr::from_str(label))?;
            }
            MenuEntry::Submenu { label, items } => {
                let sub = build_hmenu(items)?;
                menu.AppendMenu(
                    co::MF::POPUP,
                    w::IdMenu::Menu(&sub),
                    w::BmpPtrStr::from_str(label),
                )?;
            }
        }
    }
    Ok(menu)
}

/// Carry out a chosen menu command.
fn handle_action(
    hwnd: &w::HWND,
    wnd: &gui::WindowMain,
    tray: &Rc<Tray>,
    action: MenuAction,
) {
    match action {
        MenuAction::Exit => {
            let _ = hwnd.DestroyWindow();
        }
        MenuAction::Bot { name, action } => match action {
            BotAction::Start => {
                tray.manager.borrow_mut().start(&name);
            }
            BotAction::Stop => {
                tray.manager.borrow_mut().stop_nonblocking(&name);
            }
            BotAction::Restart => {
                tray.manager.borrow_mut().restart_nonblocking(&name);
            }
            BotAction::Logs => open_logs(hwnd, &name),
            BotAction::Config => edit_config(wnd, tray, &name),
        },
        MenuAction::SpotifyAuth => spawn_spotify_auth(tray.facts.borrow().staleness_flag()),
        MenuAction::CheckUpdates => check_for_updates_now(hwnd, wnd),
        MenuAction::AddServer => add_server(wnd, tray),
        MenuAction::YoutubeInstall => {
            crate::gui_native::progress_dialog::run(wnd, "Install YouTube tools", |p| {
                crate::gui_native::progress_dialog::youtube_install(p)
            });
            // Whether it worked or not, what is on disk may have changed.
            tray.facts.borrow().mark_stale();
        }
        MenuAction::YoutubeUpdate => {
            crate::gui_native::progress_dialog::run(wnd, "Update YouTube tools", |p| {
                crate::gui_native::progress_dialog::youtube_update(p)
            });
            tray.facts.borrow().mark_stale();
        }
        MenuAction::Settings => crate::gui_native::settings_dialog::show(wnd),
    }
}

/// Create a new config, then start whatever it produced.
fn add_server(wnd: &gui::WindowMain, tray: &Rc<Tray>) {
    let created = crate::gui_native::config_dialog::show(wnd, BotConfig::default(), None);
    if created.is_none() {
        return;
    }
    let names = { tray.manager.borrow_mut().load_configs() };
    let mut m = tray.manager.borrow_mut();
    for name in &names {
        m.start(name);
    }
    drop(m);
    // A config may have signed in or installed tools.
    tray.facts.borrow().mark_stale();
    update_tooltip(wnd.hwnd(), tray);
}

/// Edit an existing config. A running bot is restarted so the edit takes
/// effect; a stopped one is left stopped.
fn edit_config(wnd: &gui::WindowMain, tray: &Rc<Tray>, name: &str) {
    let Some(path) = tray.manager.borrow().config_path(name) else {
        return;
    };
    let cfg = match BotConfig::load(path.to_str().unwrap_or("")) {
        Ok(c) => c,
        Err(e) => {
            let _ = wnd.hwnd().MessageBox(
                &format!("Could not read {}: {e}", path.display()),
                "TT Spotify",
                co::MB::OK | co::MB::ICONERROR,
            );
            return;
        }
    };
    if crate::gui_native::config_dialog::show(wnd, cfg, Some(path)).is_some() {
        let mut m = tray.manager.borrow_mut();
        if m.is_running(name) {
            m.restart_nonblocking(name);
        }
        drop(m);
        tray.facts.borrow().mark_stale();
        update_tooltip(wnd.hwnd(), tray);
    }
}

/// The manual "check for updates". Runs the network call on a worker thread and
/// waits for it without freezing: the message loop keeps running, so the tray
/// stays responsive and its status keeps updating.
fn check_for_updates_now(hwnd: &w::HWND, wnd: &gui::WindowMain) {
    let (tx, rx) = crossbeam_channel::unbounded::<Result<Option<crate::update::UpdateInfo>, String>>();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))
            .and_then(|rt| rt.block_on(crate::update::check()).map_err(|e| e.to_string()));
        let _ = tx.send(result);
    });
    match pump_until_ready(hwnd, &rx) {
        Some(Ok(Some(info))) => {
            let _ = crate::gui_native::update_dialog::show_update_available(wnd, info);
        }
        Some(Ok(None)) => crate::gui_native::update_dialog::show_up_to_date(hwnd),
        Some(Err(e)) => crate::gui_native::update_dialog::show_check_error(hwnd, &e),
        None => {}
    }
}

/// Wait for a channel while keeping the message loop alive, so the window keeps
/// painting and the tray keeps updating instead of appearing hung.
fn pump_until_ready<T>(hwnd: &w::HWND, rx: &crossbeam_channel::Receiver<T>) -> Option<T> {
    loop {
        match rx.try_recv() {
            Ok(v) => return Some(v),
            Err(crossbeam_channel::TryRecvError::Disconnected) => return None,
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }
        // Give Windows a slice so the window keeps painting; the tray's own
        // timer keeps running because this is the same message loop.
        let _ = hwnd;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Open an instance's newest log, or its folder when it has none yet.
fn open_logs(hwnd: &w::HWND, name: &str) {
    let dir = crate::logging::instance_log_dir(name);
    // Deliberately not the wx version's filename-suffix search: that dated
    // from when every instance shared one log folder. Logs now live in
    // logs/<name>/<date>.log, so the newest file in the folder is the answer.
    let target = match crate::logging::newest_log_file(&dir) {
        Some(path) => path,
        None if dir.is_dir() => dir,
        None => {
            let _ = hwnd.MessageBox(
                &format!("{name} has not written any logs yet."),
                "TT Spotify",
                co::MB::OK | co::MB::ICONINFORMATION,
            );
            return;
        }
    };
    open_path(&target);
}

/// Hand a path to the shell's default handler.
fn open_path(path: &std::path::Path) {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // `start` via cmd keeps the shell "open" verb, so an unassociated file
    // offers the "Open with" picker instead of failing. CREATE_NO_WINDOW hides
    // the console that would otherwise flash.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Err(e) = std::process::Command::new("cmd")
        .args(["/c", "start", "", &abs.display().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        tracing::error!("Could not open {}: {e}", abs.display());
    }
}

/// Load configs and start every bot, prompting to create one if none exist.
fn start_bots(wnd: &gui::WindowMain, tray: &Rc<Tray>) {
    let hwnd = wnd.hwnd();
    if tray.exiting.get() {
        return;
    }
    let names = { tray.manager.borrow_mut().load_configs() };
    if names.is_empty() {
        let answer = hwnd.MessageBox(
            "No config files found.\n\nWould you like to create one now?\n\n\
             You can also create one later from the tray menu (Add Server).",
            "TT Spotify - No Configurations",
            co::MB::YESNO | co::MB::ICONQUESTION,
        );
        if matches!(answer, Ok(co::DLGID::YES)) {
            add_server(wnd, tray);
            return;
        }
    } else {
        let mut m = tray.manager.borrow_mut();
        for name in &names {
            m.start(name);
        }
    }
    update_tooltip(hwnd, tray);
}

/// Act on the startup update check, once.
fn poll_startup_update(wnd: &gui::WindowMain, tray: &Rc<Tray>) {
    let Some(rx) = &tray.update_rx else { return };
    if tray.update_done.get() {
        return;
    }
    let Ok(result) = rx.try_recv() else { return };
    tray.update_done.set(true);
    match result {
        Some(info) => {
            // Offering the update gates bot startup on the user's answer. The
            // dialog returns false only when the process is about to relaunch,
            // in which case starting bots here would be pointless work in a
            // dying process.
            if crate::gui_native::update_dialog::show_update_available(wnd, info) {
                start_bots(wnd, tray);
            }
        }
        None => start_bots(wnd, tray),
    }
}

/// Blocking startup update check on a fresh runtime, capped so a slow network
/// cannot stall bot startup.
fn check_for_update() -> Option<crate::update::UpdateInfo> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        match tokio::time::timeout(std::time::Duration::from_secs(8), crate::update::check()).await
        {
            Ok(Ok(info)) => info,
            _ => None,
        }
    })
}

/// Re-authenticate with Spotify. The browser drives the UI, so this runs
/// silently and the result lands in the log.
fn spawn_spotify_auth(facts_stale: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
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
        // Signed-in state just changed; the next menu must read it again
        // rather than showing the stale label for up to 15 seconds.
        facts_stale.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}



#[cfg(test)]
mod hmenu_tests {
    use super::*;

    fn facts() -> MenuFacts {
        MenuFacts {
            spotify_signed_in: false,
            youtube_installed: false,
        }
    }

    fn bot(name: &str, running: bool) -> (String, String, bool) {
        (name.to_string(), "Connected, Idle".to_string(), running)
    }

    /// Build a real Win32 menu from the model and hand it to `check`.
    /// The menu is destroyed afterwards even if `check` fails.
    fn with_menu(bots: &[(String, String, bool)], check: impl FnOnce(&w::HMENU, &super::super::menu::MenuModel)) {
        let model = MenuBuilder::new().build(bots, facts());
        let mut hmenu = build_hmenu(&model.entries).expect("menu should build");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&hmenu, &model)));
        let _ = hmenu.DestroyMenu();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn the_model_becomes_a_real_menu_of_the_right_shape() {
        // Guards the translation from model to Win32: a separator appended as
        // a string, or a submenu appended as an item, would still "work"
        // enough to compile and then look wrong on screen.
        with_menu(&[bot("alpha", true)], |hmenu, model| {
            let count = hmenu.GetMenuItemCount().expect("count");
            assert_eq!(
                count as usize,
                model.entries.len(),
                "every top-level entry should appear exactly once"
            );
        });
    }

    #[test]
    fn a_bot_becomes_a_submenu_holding_its_commands() {
        with_menu(&[bot("alpha", true)], |hmenu, _| {
            let sub = hmenu.GetSubMenu(0).expect("the first entry should be a submenu");
            // Start, Stop, Restart, separator, View Logs, Edit Config.
            assert_eq!(sub.GetMenuItemCount().expect("sub count"), 6);
        });
    }

    #[test]
    fn command_ids_survive_into_the_menu() {
        // If ids were dropped or renumbered on the way into Win32, clicks would
        // resolve to the wrong action or to nothing.
        with_menu(&[bot("alpha", false)], |hmenu, model| {
            let sub = hmenu.GetSubMenu(0).expect("submenu");
            let start_id = sub.GetMenuItemID(0).expect("first command should have an id");
            assert_eq!(
                model.action(start_id),
                Some(&MenuAction::Bot {
                    name: "alpha".to_string(),
                    action: BotAction::Start
                }),
                "the id Win32 reports must be the one the model assigned"
            );
        });
    }

    #[test]
    fn a_running_bot_has_start_greyed_and_stop_available() {
        // Queried by command id, not by position: winsafe's GetMenuState sends
        // the by-position flag as 1 instead of MF_BYPOSITION (0x400), so
        // IdPos::Pos does not address what it claims to. By-command passes
        // MF_BYCOMMAND (0) and is unaffected.
        with_menu(&[bot("alpha", true)], |hmenu, model| {
            let sub = hmenu.GetSubMenu(0).expect("submenu");
            let id_of = |want: BotAction| -> u16 {
                (0..sub.GetMenuItemCount().expect("count") as i32)
                    .filter_map(|i| sub.GetMenuItemID(i))
                    .find(|id| {
                        matches!(model.action(*id), Some(MenuAction::Bot { action, .. }) if *action == want)
                    })
                    .unwrap_or_else(|| panic!("no menu item for {want:?}"))
            };

            let start = sub
                .GetMenuState(w::IdPos::Id(id_of(BotAction::Start)))
                .expect("start state");
            let stop = sub
                .GetMenuState(w::IdPos::Id(id_of(BotAction::Stop)))
                .expect("stop state");
            assert!(start.has(co::MF::GRAYED), "Start should be greyed while running");
            assert!(!stop.has(co::MF::GRAYED), "Stop should be available while running");
        });
    }

    #[test]
    fn a_stopped_bot_has_stop_greyed_and_start_available() {
        with_menu(&[bot("idle", false)], |hmenu, model| {
            let sub = hmenu.GetSubMenu(0).expect("submenu");
            let id_of = |want: BotAction| -> u16 {
                (0..sub.GetMenuItemCount().expect("count") as i32)
                    .filter_map(|i| sub.GetMenuItemID(i))
                    .find(|id| {
                        matches!(model.action(*id), Some(MenuAction::Bot { action, .. }) if *action == want)
                    })
                    .unwrap_or_else(|| panic!("no menu item for {want:?}"))
            };
            let start = sub
                .GetMenuState(w::IdPos::Id(id_of(BotAction::Start)))
                .expect("start state");
            let stop = sub
                .GetMenuState(w::IdPos::Id(id_of(BotAction::Stop)))
                .expect("stop state");
            assert!(!start.has(co::MF::GRAYED), "Start should be available when stopped");
            assert!(stop.has(co::MF::GRAYED), "Stop should be greyed when stopped");
        });
    }

    #[test]
    fn labels_reach_the_menu_intact() {
        with_menu(&[bot("alpha", true)], |hmenu, _| {
            let label = hmenu.GetMenuString(w::IdPos::Pos(0)).expect("submenu label");
            assert!(label.contains("alpha"), "got: {label}");
        });
    }

    #[test]
    fn a_menu_with_no_bots_still_has_the_global_commands() {
        with_menu(&[], |hmenu, model| {
            assert_eq!(
                hmenu.GetMenuItemCount().expect("count") as usize,
                model.entries.len()
            );
            assert!(model.entries.len() >= 5, "expected the global commands");
        });
    }
}
