//! The config editor: a host dialog with a tab control over three child
//! dialogs.
//!
//! Child dialogs are how Windows builds tabbed pages, and each keeps its own
//! tab order, so a screen reader walks a page normally instead of through one
//! flat list of every control in the editor.
//!
//! All the reading and writing lives in `config_form`, which is tested on its
//! own. This file moves strings between controls and that module, and owns the
//! parts that need a window: browsing for a file, naming a new config, and the
//! offer to install the YouTube tools afterwards.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use winsafe::prelude::*;
use winsafe::{self as w, co, gui};

use crate::config::BotConfig;
use crate::gui_native::config_form::{
    mode_uses_username_list, ConfigForm, ADMIN_MODE_LABELS,
};

const IDD_CONFIG: u16 = 600;
const IDC_CFG_TAB: u16 = 601;

const IDD_CFG_SERVER: u16 = 610;
const IDC_HOST: u16 = 611;
const IDC_TCP_PORT: u16 = 612;
const IDC_UDP_PORT: u16 = 613;
const IDC_ENCRYPTED: u16 = 614;
const IDC_USERNAME: u16 = 615;
const IDC_PASSWORD: u16 = 616;
const IDC_BOTNAME: u16 = 617;
const IDC_CHANNEL: u16 = 618;
const IDC_CHANPASS: u16 = 619;
const IDC_GENDER: u16 = 620;
const IDC_SERVICE: u16 = 621;
const IDC_LICNAME: u16 = 622;
const IDC_LICKEY: u16 = 623;
const IDC_COOKIES: u16 = 624;
const IDC_COOKIES_BROWSE: u16 = 625;
const IDC_LANGUAGE: u16 = 626;
const IDC_ADMIN_MODE: u16 = 627;
const IDC_ADMIN_USERS: u16 = 628;
const IDC_SP_ENABLED: u16 = 629;
const IDC_YT_ENABLED: u16 = 630;
const IDC_SERVICE_LABEL: u16 = 631;

const IDD_CFG_AUDIO: u16 = 640;
const IDC_QUALITY: u16 = 641;
const IDC_NORMALIZE: u16 = 642;
const IDC_VOLUME: u16 = 643;
const IDC_MAX_VOLUME: u16 = 644;
const IDC_JITTER: u16 = 645;
const IDC_RAMP: u16 = 646;

const IDD_CFG_RADIO: u16 = 660;
const IDC_RADIO_ENABLED: u16 = 661;
const IDC_RADIO_BATCH: u16 = 662;
const IDC_RADIO_DELAY: u16 = 663;
const IDC_SEARCH_LIMIT: u16 = 664;

const IDD_NAME: u16 = 500;
const IDC_NAME_EDIT: u16 = 501;
const IDOK: u16 = 1;
const IDCANCEL: u16 = 2;

const GENDERS: [&str; 3] = ["neutral", "male", "female"];
const SERVICES: [&str; 2] = ["Spotify", "YouTube"];
const QUALITIES: [&str; 3] = ["VERY_HIGH", "HIGH", "NORMAL"];

/// Controls on the Server page.
struct ServerPage {
    host: gui::Edit,
    tcp_port: gui::Edit,
    udp_port: gui::Edit,
    encrypted: gui::CheckBox,
    username: gui::Edit,
    password: gui::Edit,
    bot_name: gui::Edit,
    channel: gui::Edit,
    channel_password: gui::Edit,
    gender: gui::ComboBox,
    spotify_enabled: gui::CheckBox,
    youtube_enabled: gui::CheckBox,
    service: gui::ComboBox,
    language: gui::ComboBox,
    license_name: gui::Edit,
    license_key: gui::Edit,
    cookies: gui::Edit,
    admin_mode: gui::ComboBox,
    admin_users: gui::Edit,
}

/// Controls on the Audio page.
struct AudioPage {
    quality: gui::ComboBox,
    normalize: gui::CheckBox,
    volume: gui::Edit,
    max_volume: gui::Edit,
    jitter: gui::Edit,
    ramp: gui::Edit,
}

/// Controls on the Radio and admin page.
struct RadioPage {
    radio_enabled: gui::CheckBox,
    batch: gui::Edit,
    delay: gui::Edit,
    search_limit: gui::Edit,
}

/// Open the config editor.
///
/// `config_path` is `None` for a new config, in which case a name is asked for
/// on save. Returns the path written, or `None` if the user cancelled.
pub fn show(
    parent: &(impl GuiParent + 'static),
    config: BotConfig,
    config_path: Option<PathBuf>,
) -> Option<PathBuf> {
    let dlg = gui::WindowModal::new_dlg(IDD_CONFIG);

    let server_pg = gui::TabPage::new_dlg(&dlg, IDD_CFG_SERVER);
    let audio_pg = gui::TabPage::new_dlg(&dlg, IDD_CFG_AUDIO);
    let radio_pg = gui::TabPage::new_dlg(&dlg, IDD_CFG_RADIO);

    let server = Rc::new(ServerPage {
        host: gui::Edit::new_dlg(&server_pg, IDC_HOST, (gui::Horz::Resize, gui::Vert::None)),
        tcp_port: gui::Edit::new_dlg(&server_pg, IDC_TCP_PORT, (gui::Horz::None, gui::Vert::None)),
        udp_port: gui::Edit::new_dlg(&server_pg, IDC_UDP_PORT, (gui::Horz::None, gui::Vert::None)),
        encrypted: gui::CheckBox::new_dlg(&server_pg, IDC_ENCRYPTED, (gui::Horz::None, gui::Vert::None)),
        username: gui::Edit::new_dlg(&server_pg, IDC_USERNAME, (gui::Horz::Resize, gui::Vert::None)),
        password: gui::Edit::new_dlg(&server_pg, IDC_PASSWORD, (gui::Horz::Resize, gui::Vert::None)),
        bot_name: gui::Edit::new_dlg(&server_pg, IDC_BOTNAME, (gui::Horz::Resize, gui::Vert::None)),
        channel: gui::Edit::new_dlg(&server_pg, IDC_CHANNEL, (gui::Horz::Resize, gui::Vert::None)),
        channel_password: gui::Edit::new_dlg(&server_pg, IDC_CHANPASS, (gui::Horz::Resize, gui::Vert::None)),
        gender: gui::ComboBox::new_dlg(&server_pg, IDC_GENDER, (gui::Horz::None, gui::Vert::None)),
        spotify_enabled: gui::CheckBox::new_dlg(&server_pg, IDC_SP_ENABLED, (gui::Horz::None, gui::Vert::None)),
        youtube_enabled: gui::CheckBox::new_dlg(&server_pg, IDC_YT_ENABLED, (gui::Horz::None, gui::Vert::None)),
        service: gui::ComboBox::new_dlg(&server_pg, IDC_SERVICE, (gui::Horz::None, gui::Vert::None)),
        language: gui::ComboBox::new_dlg(&server_pg, IDC_LANGUAGE, (gui::Horz::None, gui::Vert::None)),
        license_name: gui::Edit::new_dlg(&server_pg, IDC_LICNAME, (gui::Horz::Resize, gui::Vert::None)),
        license_key: gui::Edit::new_dlg(&server_pg, IDC_LICKEY, (gui::Horz::Resize, gui::Vert::None)),
        cookies: gui::Edit::new_dlg(&server_pg, IDC_COOKIES, (gui::Horz::Resize, gui::Vert::None)),
        admin_mode: gui::ComboBox::new_dlg(&server_pg, IDC_ADMIN_MODE, (gui::Horz::Resize, gui::Vert::None)),
        admin_users: gui::Edit::new_dlg(&server_pg, IDC_ADMIN_USERS, (gui::Horz::Resize, gui::Vert::Resize)),
    });

    let audio = Rc::new(AudioPage {
        quality: gui::ComboBox::new_dlg(&audio_pg, IDC_QUALITY, (gui::Horz::None, gui::Vert::None)),
        normalize: gui::CheckBox::new_dlg(&audio_pg, IDC_NORMALIZE, (gui::Horz::None, gui::Vert::None)),
        volume: gui::Edit::new_dlg(&audio_pg, IDC_VOLUME, (gui::Horz::None, gui::Vert::None)),
        max_volume: gui::Edit::new_dlg(&audio_pg, IDC_MAX_VOLUME, (gui::Horz::None, gui::Vert::None)),
        jitter: gui::Edit::new_dlg(&audio_pg, IDC_JITTER, (gui::Horz::None, gui::Vert::None)),
        ramp: gui::Edit::new_dlg(&audio_pg, IDC_RAMP, (gui::Horz::None, gui::Vert::None)),
    });
    let browse = gui::Button::new_dlg(&server_pg, IDC_COOKIES_BROWSE, (gui::Horz::Repos, gui::Vert::None));

    let radio = Rc::new(RadioPage {
        radio_enabled: gui::CheckBox::new_dlg(&radio_pg, IDC_RADIO_ENABLED, (gui::Horz::None, gui::Vert::None)),
        batch: gui::Edit::new_dlg(&radio_pg, IDC_RADIO_BATCH, (gui::Horz::None, gui::Vert::None)),
        delay: gui::Edit::new_dlg(&radio_pg, IDC_RADIO_DELAY, (gui::Horz::None, gui::Vert::None)),
        search_limit: gui::Edit::new_dlg(&radio_pg, IDC_SEARCH_LIMIT, (gui::Horz::None, gui::Vert::None)),
    });

    let _tab = gui::Tab::new_dlg(
        &dlg,
        IDC_CFG_TAB,
        (gui::Horz::Resize, gui::Vert::Resize),
        &[
            ("Server", server_pg.clone()),
            ("Audio", audio_pg.clone()),
            ("Radio && search", radio_pg.clone()),
        ],
    );

    let saved: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let original = Rc::new(config);
    let path = Rc::new(config_path);

    // --- Fill the form ---
    {
        let dlg2 = dlg.clone();
        let (server, audio, radio) = (server.clone(), audio.clone(), radio.clone());
        let server_pg2 = server_pg.clone();
        let original = original.clone();
        let path = path.clone();
        dlg.on().wm_init_dialog(move |_| {
            let _ = dlg2.hwnd().SetWindowText(if path.is_some() {
                "TT Spotify - Edit configuration"
            } else {
                "TT Spotify - New configuration"
            });

            let form = ConfigForm::from_config(&original);

            let _ = server.gender.items().add(&GENDERS);
            let _ = server.service.items().add(&SERVICES);
            // Embedded English plus whatever translations are installed.
            let languages = crate::i18n::installed_language_codes(&crate::config::config_dir());
            let lang_refs: Vec<&str> = languages.iter().map(String::as_str).collect();
            let _ = server.language.items().add(&lang_refs);
            let _ = audio.quality.items().add(&QUALITIES);
            let _ = server.admin_mode.items().add(&ADMIN_MODE_LABELS);

            let _ = server.host.set_text(&form.host);
            let _ = server.tcp_port.set_text(&form.tcp_port.to_string());
            let _ = server.udp_port.set_text(&form.udp_port.to_string());
            server.encrypted.set_check(form.encrypted);
            let _ = server.username.set_text(&form.username);
            let _ = server.password.set_text(&form.password);
            let _ = server.bot_name.set_text(&form.bot_name);
            let _ = server.channel.set_text(&form.channel_name);
            let _ = server.channel_password.set_text(&form.channel_password);
            select_or_first(&server.gender, &GENDERS, &form.bot_gender);
            server.spotify_enabled.set_check(form.spotify_enabled);
            server.youtube_enabled.set_check(form.youtube_enabled);
            update_service_row(&server_pg2, &server);
            select_or_first(&server.service, &SERVICES, &form.default_service);
            select_or_first(&server.language, &lang_refs, &form.default_language);
            let _ = server.license_name.set_text(&form.license_name);

            let _ = server.license_key.set_text(&form.license_key);
            let _ = server.cookies.set_text(&form.youtube_cookies_file);
            select_or_first(&audio.quality, &QUALITIES, &form.spotify_quality);
            audio.normalize.set_check(form.spotify_enable_normalization);
            let _ = audio.volume.set_text(&form.volume.to_string());
            let _ = audio.max_volume.set_text(&form.max_volume.to_string());
            let _ = audio.jitter.set_text(&form.jitter_buffer_ms.to_string());
            let _ = audio.ramp.set_text(&form.volume_ramp_step);

            radio.radio_enabled.set_check(form.radio_enabled);
            let _ = radio.batch.set_text(&form.radio_batch_size.to_string());
            let _ = radio.delay.set_text(&form.radio_delay);
            let _ = radio.search_limit.set_text(&form.search_limit.to_string());
            server.admin_mode.items().select(Some(form.admin_mode_index));
            let _ = server.admin_users.set_text(&form.admin_users);
            // The username list is meaningless in the modes that ignore it.
            server
                .admin_users
                .hwnd()
                .EnableWindow(mode_uses_username_list(form.admin_mode_index));

            // Tab order follows z-order among siblings. The tab pages are
            // created at run time and land below Save and Cancel, which were
            // declared in the template, so tabbing went straight from the tab
            // strip to the buttons and only then into the fields. Moving the
            // buttons to the bottom of the z-order puts them last, where they
            // belong: you should reach what you are saving before you reach
            // Save.
            for id in [IDOK, IDCANCEL] {
                if let Ok(btn) = dlg2.hwnd().GetDlgItem(id) {
                    let _ = btn.SetWindowPos(
                        w::HwndPlace::Place(co::HWND_PLACE::BOTTOM),
                        w::POINT::default(),
                        w::SIZE::default(),
                        co::SWP::NOMOVE | co::SWP::NOSIZE | co::SWP::NOACTIVATE,
                    );
                }
            }

            Ok(true)
        });
    }

    // --- Service checkboxes decide whether the default-service row shows ---
    for checkbox in [&server.spotify_enabled, &server.youtube_enabled] {
        let server2 = server.clone();
        let server_pg2 = server_pg.clone();
        checkbox.on().bn_clicked(move || {
            update_service_row(&server_pg2, &server2);
            Ok(())
        });
    }

    // --- Admin mode changes what the username list is for ---
    {
        let server2 = server.clone();
        server.admin_mode.on().cbn_sel_change(move || {
            let idx = server2.admin_mode.items().selected_index().unwrap_or(3);
            server2
                .admin_users
                .hwnd()
                .EnableWindow(mode_uses_username_list(idx));
            Ok(())
        });
    }

    // --- Browse for a cookies file ---
    {
        let server2 = server.clone();
        let dlg2 = dlg.clone();
        browse.on().bn_clicked(move || {
            if let Some(picked) = pick_cookies_file(dlg2.hwnd(), &server2.cookies.text().unwrap_or_default()) {
                let _ = server2.cookies.set_text(&picked);
            }
            Ok(())
        });
    }

    // --- Save ---
    {
        let dlg2 = dlg.clone();
        let (server, audio, radio) = (server.clone(), audio.clone(), radio.clone());
        let original = original.clone();
        let path = path.clone();
        let saved = saved.clone();
        dlg.on().wm_command_acc_menu(IDOK, move || {
            let hwnd = dlg2.hwnd();
            let form = read_form(&server, &audio, &radio);

            let errors = form.validate();
            if !errors.is_empty() {
                let _ = hwnd.MessageBox(
                    &errors.join("\n"),
                    "Please check these",
                    co::MB::OK | co::MB::ICONERROR,
                );
                return Ok(());
            }

            let mut cfg = form.apply(&original);
            // The same clamps every config load applies. Without them here the
            // editor persisted out-of-range values (port 0, ramp step 0) that
            // runtime silently corrected on each start while the file — and
            // the editor on reopen — kept showing the bogus number.
            let corrections = cfg.validate();
            if !corrections.is_empty() {
                let _ = hwnd.MessageBox(
                    &format!(
                        "Some values were out of range and have been corrected:\n\n{}",
                        corrections.join("\n")
                    ),
                    "TT Spotify",
                    co::MB::OK | co::MB::ICONWARNING,
                );
            }

            // Editing and changing nothing writes nothing, so the bot is not
            // restarted for a no-op.
            if path.is_some() && cfg == *original {
                let _ = hwnd.EndDialog(IDCANCEL as isize);
                return Ok(());
            }

            let save_path = match path.as_ref() {
                Some(p) => p.clone(),
                None => match ask_for_path(&dlg2, hwnd) {
                    Some(p) => p,
                    None => return Ok(()),
                },
            };

            if let Err(e) = cfg.save(&save_path) {
                let _ = hwnd.MessageBox(
                    &format!("Could not save: {e}"),
                    "Error",
                    co::MB::OK | co::MB::ICONERROR,
                );
                return Ok(());
            }

            // Offer the YouTube tools only for a new config; an edit saves
            // quietly, and the tray menu can install them later.
            if path.is_none() {
                offer_youtube_setup(&dlg2, &cfg);
            }

            *saved.borrow_mut() = Some(save_path);
            let _ = hwnd.EndDialog(IDOK as isize);
            Ok(())
        });
    }

    {
        let dlg2 = dlg.clone();
        dlg.on().wm_command_acc_menu(IDCANCEL, move || {
            let _ = dlg2.hwnd().EndDialog(IDCANCEL as isize);
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Config dialog failed: {e}");
        return None;
    }
    let result = saved.borrow().clone();
    result
}

/// Show or hide the default-service row to match the service checkboxes.
/// With one service enabled the default is obvious, so the row disappears and
/// the (hidden) dropdown is snapped to the surviving service — read_form still
/// reads it, and a stale value must not be what gets saved.
fn update_service_row(server_pg: &gui::TabPage, server: &ServerPage) {
    let spotify = server.spotify_enabled.is_checked();
    let youtube = server.youtube_enabled.is_checked();
    let both = spotify && youtube;
    let mode = if both { co::SW::SHOW } else { co::SW::HIDE };
    if let Ok(label) = server_pg.hwnd().GetDlgItem(IDC_SERVICE_LABEL) {
        label.ShowWindow(mode);
    }
    server.service.hwnd().ShowWindow(mode);
    if !both {
        let only = if spotify { "Spotify" } else { "YouTube" };
        select_or_first(&server.service, &SERVICES, only);
    }
}

/// Read every control into a form.
fn read_form(server: &ServerPage, audio: &AudioPage, radio: &RadioPage) -> ConfigForm {
    ConfigForm {
        host: text(&server.host),
        tcp_port: number(&server.tcp_port, 10333),
        udp_port: number(&server.udp_port, 10333),
        encrypted: server.encrypted.is_checked(),
        username: text(&server.username),
        password: text(&server.password),
        bot_name: text(&server.bot_name),
        channel_name: text(&server.channel),
        channel_password: text(&server.channel_password),
        bot_gender: combo_text(&server.gender),
        spotify_enabled: server.spotify_enabled.is_checked(),
        youtube_enabled: server.youtube_enabled.is_checked(),
        default_service: combo_text(&server.service),
        license_name: text(&server.license_name),
        license_key: text(&server.license_key),
        youtube_cookies_file: text(&server.cookies),
        default_language: combo_text(&server.language),
        admin_mode_index: server.admin_mode.items().selected_index().unwrap_or(3),
        admin_users: text(&server.admin_users),
        spotify_quality: combo_text(&audio.quality),
        spotify_enable_normalization: audio.normalize.is_checked(),
        volume: number(&audio.volume, 50),
        max_volume: number(&audio.max_volume, 100),
        jitter_buffer_ms: number(&audio.jitter, 0),
        volume_ramp_step: text(&audio.ramp),
        radio_enabled: radio.radio_enabled.is_checked(),
        radio_batch_size: number(&radio.batch, 1),
        radio_delay: text(&radio.delay),
        search_limit: number(&radio.search_limit, 5),
    }
}

fn text(edit: &gui::Edit) -> String {
    edit.text().unwrap_or_default()
}

/// A number from an edit box, falling back rather than becoming zero: an
/// unreadable port or volume silently set to 0 would be worse than unchanged.
fn number(edit: &gui::Edit, fallback: i32) -> i32 {
    text(edit).trim().parse().unwrap_or(fallback)
}

fn combo_text(combo: &gui::ComboBox) -> String {
    combo.items().selected_text().ok().flatten().unwrap_or_default()
}

/// Select `value` if the list has it, otherwise the first entry, so a combo is
/// never left with nothing selected.
fn select_or_first(combo: &gui::ComboBox, items: &[&str], value: &str) {
    let idx = items
        .iter()
        .position(|i| i.eq_ignore_ascii_case(value))
        .unwrap_or(0);
    combo.items().select(Some(idx as u32));
}

/// Ask for a name and turn it into a path inside the configs folder.
fn ask_for_path(parent: &(impl GuiParent + 'static), hwnd: &w::HWND) -> Option<PathBuf> {
    let name = ask_for_name(parent)?;
    let path = crate::paths::config_file(&name);
    if path.exists() {
        let answer = hwnd.MessageBox(
            &format!("{} already exists.\n\nOverwrite it?", path.display()),
            "Confirm overwrite",
            co::MB::YESNO | co::MB::ICONQUESTION,
        );
        if !matches!(answer, Ok(co::DLGID::YES)) {
            return None;
        }
    }
    Some(path)
}

/// The name prompt for a new config.
fn ask_for_name(parent: &(impl GuiParent + 'static)) -> Option<String> {
    let result: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let dlg = gui::WindowModal::new_dlg(IDD_NAME);
    let edit = gui::Edit::new_dlg(&dlg, IDC_NAME_EDIT, (gui::Horz::Resize, gui::Vert::None));

    {
        let edit = edit.clone();
        dlg.on().wm_init_dialog(move |_| {
            let _ = edit.set_text("config");
            let _ = edit.hwnd().SetFocus();
            edit.set_selection(0, -1);
            // Focus was set here, so tell Windows not to override it.
            Ok(false)
        });
    }
    {
        let dlg2 = dlg.clone();
        let edit = edit.clone();
        let result = result.clone();
        dlg.on().wm_command_acc_menu(IDOK, move || {
            let typed = edit.text().unwrap_or_default();
            match crate::config::sanitise_config_name(&typed) {
                Some(name) => {
                    *result.borrow_mut() = Some(name);
                    let _ = dlg2.hwnd().EndDialog(IDOK as isize);
                }
                None => {
                    let _ = dlg2.hwnd().MessageBox(
                        "Please enter a name.",
                        "Config name",
                        co::MB::OK | co::MB::ICONERROR,
                    );
                }
            }
            Ok(())
        });
    }
    {
        let dlg2 = dlg.clone();
        dlg.on().wm_command_acc_menu(IDCANCEL, move || {
            let _ = dlg2.hwnd().EndDialog(IDCANCEL as isize);
            Ok(())
        });
    }

    if let Err(e) = dlg.show_modal(parent) {
        tracing::error!("Name prompt failed: {e}");
        return None;
    }
    let name = result.borrow().clone();
    name
}

/// Pick a cookies file with the standard Windows file dialog.
///
/// `cookies.txt` is normally produced by a browser extension and lands in the
/// Downloads folder, so browsing to it is the usual way to set this, not typing
/// the path.
fn pick_cookies_file(parent: &w::HWND, current: &str) -> Option<String> {
    // winsafe's GUI setup does not initialise COM, and the shell dialog is a
    // COM object. Apartment-threaded is what shell UI requires. The guard
    // uninitialises when it drops, so this leaves the process as it found it.
    let _com = match w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE) {
        Ok(guard) => guard,
        Err(e) => {
            tracing::error!("Could not initialise COM for the file dialog: {e}");
            return None;
        }
    };

    let picked = (|| -> w::HrResult<Option<String>> {
        let dlg = w::CoCreateInstance::<w::IFileOpenDialog>(
            &co::CLSID::FileOpenDialog,
            None::<&w::IUnknown>,
            co::CLSCTX::INPROC_SERVER,
        )?;

        dlg.SetOptions(
            dlg.GetOptions()?
                // A path the bot cannot open would be worse than none, so only
                // real files that exist can be chosen.
                | co::FOS::FORCEFILESYSTEM
                | co::FOS::FILEMUSTEXIST
                | co::FOS::PATHMUSTEXIST,
        )?;
        dlg.SetTitle("Select your YouTube cookies file")?;
        dlg.SetFileTypes(&[("Cookie files", "*.txt"), ("All files", "*.*")])?;
        dlg.SetFileTypeIndex(1)?;

        // Start where the current value points, so re-picking opens next to the
        // old choice instead of somewhere arbitrary.
        if !current.trim().is_empty() {
            let path = std::path::Path::new(current.trim());
            if let Some(dir) = path.parent().filter(|d| d.is_dir()) {
                if let Ok(item) = w::SHCreateItemFromParsingName::<w::IShellItem>(
                    &dir.to_string_lossy(),
                    None::<&w::IBindCtx>,
                ) {
                    let _ = dlg.SetFolder(&item);
                }
            }
            if let Some(name) = path.file_name() {
                let _ = dlg.SetFileName(&name.to_string_lossy());
            }
        }

        if dlg.Show(parent)? {
            Ok(Some(dlg.GetResult()?.GetDisplayName(co::SIGDN::FILESYSPATH)?))
        } else {
            Ok(None) // cancelled
        }
    })();

    match picked {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("File dialog failed: {e}");
            None
        }
    }
}

/// Offer to install the YouTube tools after a new config is created.
fn offer_youtube_setup(parent: &(impl GuiParent + 'static), cfg: &BotConfig) {
    use crate::services::Service;
    use crate::youtube::setup;

    // A bot that may not use YouTube must not end its setup with a YouTube
    // download offer — the wizard skips this step for the same reason.
    if !cfg.enabled_services.youtube {
        return;
    }
    let paths = match setup::resolve_paths() {
        Ok(p) => p,
        Err(_) => return, // not user-actionable
    };
    if setup::is_installed(&paths) {
        return;
    }

    let prompt = if cfg.default_service == Service::YouTube {
        "YouTube support needs extra programs (about 50 MB: yt-dlp, bgutil-pot and a JavaScript runtime).\n\nDownload them now?"
    } else {
        "You can also enable YouTube support. This downloads about 50 MB of programs (yt-dlp, bgutil-pot and a JavaScript runtime).\n\nSkip this if you only need Spotify.\n\nInstall YouTube support?"
    };
    let answer = parent.hwnd().MessageBox(
        prompt,
        "YouTube support",
        co::MB::YESNO | co::MB::ICONQUESTION,
    );
    if matches!(answer, Ok(co::DLGID::YES)) {
        crate::gui_native::progress_dialog::run(parent, "Install YouTube tools", |p| {
            crate::gui_native::progress_dialog::youtube_install(p)
        });
    }
}
