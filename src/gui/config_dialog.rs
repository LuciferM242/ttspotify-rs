//! Config editor dialog with tabbed interface.
//!
//! Opens a Frame with a Notebook containing 3 tabs:
//! - Server: TeamTalk connection settings
//! - Audio: Spotify quality, volume, pipeline settings
//! - Radio & Search: radio mode and search settings
//!
//! Advanced normalisation fields are preserved from the loaded config
//! but not exposed in the GUI.

use std::path::PathBuf;

use wxdragon::prelude::*;

use crate::config::{config_dir, BotConfig};

fn admin_mode_to_index(mode: crate::config::AdminMode) -> u32 {
    use crate::config::AdminMode::*;
    match mode {
        Everyone => 0,
        TtRights => 1,
        List => 2,
        Both => 3,
    }
}

fn index_to_admin_mode(idx: u32) -> crate::config::AdminMode {
    use crate::config::AdminMode::*;
    match idx {
        0 => Everyone,
        1 => TtRights,
        2 => List,
        _ => Both, // index 3 and any unexpected value default to the safe Both
    }
}

/// Open the config editor window.
///
/// - `config`: Current config values (default for new, loaded for edit).
/// - `config_path`: `None` for new config (will prompt for name), `Some` for existing.
/// - `on_save`: Called with the saved file path after a successful save.
pub fn open_config_dialog(
    config: BotConfig,
    config_path: Option<PathBuf>,
    on_save: impl Fn(PathBuf) + 'static,
) {
    let title = if config_path.is_some() {
        "TT Spotify - Edit Configuration"
    } else {
        "TT Spotify - New Configuration"
    };

    let frame = Frame::builder()
        .with_title(title)
        .with_size(Size::new(480, 520))
        .build();

    let panel = Panel::builder(&frame).build();
    let main_sizer = BoxSizer::builder(Orientation::Vertical).build();

    let notebook = Notebook::builder(&panel).build();

    // ---- Tab 1: Server ----
    let server_panel = Panel::builder(&notebook).build();
    let server_sizer = FlexGridSizer::builder(0, 2).with_gap(Size::new(8, 6)).build();

    let host_input = add_text_field(&server_panel, &server_sizer, "Host:", &config.host);
    let tcp_input = add_spin_field(&server_panel, &server_sizer, "TCP Port:", config.tcp_port, 1, 65535);
    let udp_input = add_spin_field(&server_panel, &server_sizer, "UDP Port:", config.udp_port, 1, 65535);
    let encrypted_cb = add_checkbox(&server_panel, &server_sizer, "Encrypted:", config.encrypted);
    let username_input = add_text_field(&server_panel, &server_sizer, "Username:", &config.username);
    let password_input = add_password_field(&server_panel, &server_sizer, "Password:", &config.password);
    let botname_input = add_text_field(&server_panel, &server_sizer, "Bot Nickname:", &config.bot_name);
    let channel_input = add_text_field(&server_panel, &server_sizer, "Channel:", &config.channel_name);
    let chanpass_input = add_password_field(&server_panel, &server_sizer, "Channel Password:", &config.channel_password);
    let gender_input = add_combo_field(
        &server_panel,
        &server_sizer,
        "Bot Gender:",
        &["neutral", "male", "female"],
        &config.bot_gender,
    );
    let default_service_input = add_combo_field(
        &server_panel,
        &server_sizer,
        "Default Service:",
        &["Spotify", "YouTube"],
        config.default_service.name(),
    );
    let license_name_input = add_text_field(
        &server_panel,
        &server_sizer,
        "License Name:",
        config.license_name.as_deref().unwrap_or(""),
    );
    let license_key_input = add_text_field(
        &server_panel,
        &server_sizer,
        "License Key:",
        config.license_key.as_deref().unwrap_or(""),
    );
    let cookies_input = add_text_with_browse(
        &server_panel,
        &server_sizer,
        frame,
        "YT Cookies File:",
        &config.youtube_cookies_file,
    );

    // Default reply language: embedded English plus any <config_dir>/lang/*.lang
    // translation files the owner has installed.
    let language_codes = crate::i18n::installed_language_codes(&config_dir());
    let language_choices: Vec<&str> = language_codes.iter().map(String::as_str).collect();
    let default_lang_input = add_combo_field(
        &server_panel,
        &server_sizer,
        "Default Language:",
        &language_choices,
        &config.default_language,
    );

    // ---- Admin controls ----
    let admin_mode_choice = add_choice_field(
        &server_panel,
        &server_sizer,
        "Admin Mode:",
        &[
            "Everyone - no restrictions, any user can run every command",
            "TeamTalk server admins - admins defined in your TeamTalk server's user accounts",
            "Username list - only the usernames you enter below",
            "Both - TeamTalk server admins or the username list below",
        ],
    );
    let admin_users_input = add_multiline_field(
        &server_panel,
        &server_sizer,
        "Admin usernames (comma or newline separated):",
        &config.admins.join(", "),
    );

    // Load current admin mode and enable the username field only for List/Both.
    admin_mode_choice.set_selection(admin_mode_to_index(config.admin_mode));
    let list_uses_names = |idx: u32| idx == 2 || idx == 3; // List or Both
    admin_users_input.enable(list_uses_names(admin_mode_to_index(config.admin_mode)));
    admin_mode_choice.on_selection_changed(move |_| {
        let idx = admin_mode_choice.get_selection().unwrap_or(3);
        admin_users_input.enable(list_uses_names(idx));
    });

    server_sizer.add_growable_col(1, 1);
    server_panel.set_sizer(server_sizer, true);
    notebook.add_page(&server_panel, "Server", true, None);

    // ---- Tab 2: Audio ----
    let audio_panel = Panel::builder(&notebook).build();
    let audio_sizer = FlexGridSizer::builder(0, 2).with_gap(Size::new(8, 6)).build();

    let quality_input = add_combo_field(
        &audio_panel,
        &audio_sizer,
        "Spotify Quality:",
        &["VERY_HIGH", "HIGH", "NORMAL"],
        &config.spotify_quality,
    );
    let normalization_cb = add_checkbox(
        &audio_panel,
        &audio_sizer,
        "Enable Normalization:",
        config.spotify_enable_normalization,
    );
    let volume_input = add_spin_field(&audio_panel, &audio_sizer, "Default Volume:", config.volume as i32, 0, 100);
    let max_vol_input = add_spin_field(&audio_panel, &audio_sizer, "Max Volume:", config.max_volume as i32, 0, 100);
    let jitter_input = add_spin_field(
        &audio_panel,
        &audio_sizer,
        "Jitter Buffer (ms):",
        config.jitter_buffer_ms as i32,
        0,
        2000,
    );
    let ramp_input = add_text_field(
        &audio_panel,
        &audio_sizer,
        "Volume Ramp Step:",
        &config.volume_ramp_step.to_string(),
    );

    audio_sizer.add_growable_col(1, 1);
    audio_panel.set_sizer(audio_sizer, true);
    notebook.add_page(&audio_panel, "Audio", false, None);

    // ---- Tab 3: Radio & Search ----
    let radio_panel = Panel::builder(&notebook).build();
    let radio_sizer = FlexGridSizer::builder(0, 2).with_gap(Size::new(8, 6)).build();

    let radio_cb = add_checkbox(&radio_panel, &radio_sizer, "Radio Enabled:", config.radio_enabled);
    let batch_input = add_spin_field(
        &radio_panel,
        &radio_sizer,
        "Radio Batch Size:",
        config.radio_batch_size as i32,
        1,
        10,
    );
    let delay_input = add_text_field(
        &radio_panel,
        &radio_sizer,
        "Radio Delay (s):",
        &config.radio_delay.to_string(),
    );
    let search_input = add_spin_field(
        &radio_panel,
        &radio_sizer,
        "Search Limit:",
        config.search_limit as i32,
        1,
        20,
    );

    radio_sizer.add_growable_col(1, 1);
    radio_panel.set_sizer(radio_sizer, true);
    notebook.add_page(&radio_panel, "Radio & Search", false, None);

    // ---- Buttons ----
    let btn_sizer = BoxSizer::builder(Orientation::Horizontal).build();
    let save_btn = Button::builder(&panel).with_label("Save").build();
    let cancel_btn = Button::builder(&panel).with_label("Cancel").build();

    btn_sizer.add_stretch_spacer(1);
    btn_sizer.add(&save_btn, 0, SizerFlag::All, 5);
    btn_sizer.add(&cancel_btn, 0, SizerFlag::All, 5);

    main_sizer.add(&notebook, 1, SizerFlag::Expand | SizerFlag::All, 10);
    main_sizer.add_sizer(&btn_sizer, 0, SizerFlag::Expand | SizerFlag::Bottom | SizerFlag::Right, 10);

    panel.set_sizer(main_sizer, true);

    // ---- Save handler ----
    // Rc so the post-install continuation can own a clone while the click
    // handler keeps its own for any later save.
    let on_save = std::rc::Rc::new(on_save);
    save_btn.on_click(move |_| {
        // Read all values into a new config, starting from the original
        // to preserve advanced normalisation fields not shown in the GUI.
        let mut cfg = config.clone();

        // Server tab
        cfg.host = host_input.get_value();
        cfg.tcp_port = tcp_input.value();
        cfg.udp_port = udp_input.value();
        cfg.encrypted = encrypted_cb.get_value();
        cfg.username = username_input.get_value();
        cfg.password = password_input.get_value();
        cfg.bot_name = botname_input.get_value();
        cfg.channel_name = channel_input.get_value();
        if cfg.channel_name.is_empty() {
            cfg.channel_name = "/".to_string();
        }
        cfg.channel_password = chanpass_input.get_value();
        cfg.bot_gender = gender_input.get_value();
        cfg.default_service = crate::services::Service::parse_or_default(&default_service_input.get_value());
        cfg.youtube_cookies_file = cookies_input.get_value();
        let ln = license_name_input.get_value();
        cfg.license_name = if ln.is_empty() { None } else { Some(ln) };
        let lk = license_key_input.get_value();
        cfg.license_key = if lk.is_empty() { None } else { Some(lk) };
        cfg.admin_mode = index_to_admin_mode(admin_mode_choice.get_selection().unwrap_or(3));
        cfg.admins = crate::bot::auth::parse_admin_list(&admin_users_input.get_value());
        let default_lang = default_lang_input.get_value().trim().to_lowercase();
        cfg.default_language = if default_lang.is_empty() {
            "en".to_string()
        } else {
            default_lang
        };

        // Audio tab
        cfg.spotify_quality = quality_input.get_value();
        cfg.spotify_enable_normalization = normalization_cb.get_value();
        cfg.volume = volume_input.value() as u8;
        cfg.max_volume = max_vol_input.value() as u8;
        cfg.jitter_buffer_ms = jitter_input.value() as u32;
        cfg.volume_ramp_step = ramp_input.get_value().parse::<f32>().unwrap_or(cfg.volume_ramp_step);

        // Radio & Search tab
        cfg.radio_enabled = radio_cb.get_value();
        cfg.radio_batch_size = batch_input.value() as u8;
        cfg.radio_delay = delay_input.get_value().parse::<f32>().unwrap_or(cfg.radio_delay);
        cfg.search_limit = search_input.value() as u8;

        // ---- Validation ----
        let mut errors = Vec::new();
        if cfg.host.is_empty() {
            errors.push("Host is required.");
        }
        if cfg.username.is_empty() {
            errors.push("Username is required.");
        }
        if cfg.volume > cfg.max_volume {
            errors.push("Default volume cannot exceed max volume.");
        }
        if !errors.is_empty() {
            use MessageDialogStyle as MDS;
            MessageDialog::builder(&frame, &errors.join("\n"), "Validation Error")
                .with_style(MDS::OK | MDS::IconError)
                .build()
                .show_modal();
            return;
        }

        // ---- Unchanged edit: nothing to write, no bot restart needed ----
        if config_path.is_some() && cfg == config {
            frame.close(true);
            return;
        }

        // ---- Determine save path ----
        let save_path = if let Some(ref path) = config_path {
            path.clone()
        } else {
            // Prompt for config name
            let dlg = TextEntryDialog::builder(
                &frame,
                "Enter a name for this configuration:",
                "Config Name",
            )
            .with_default_value("config")
            .build();

            if dlg.show_modal() != ID_OK {
                return;
            }
            let name = match dlg.get_value() {
                Some(n) => n.replace(".json", ""),
                None => return,
            };
            if name.is_empty() {
                return;
            }

            let path = config_dir().join(format!("{name}.json"));

            // Check overwrite
            if path.exists() {
                use MessageDialogStyle as MDS;
                let res = MessageDialog::builder(
                    &frame,
                    &format!("{} already exists. Overwrite?", path.display()),
                    "Confirm Overwrite",
                )
                .with_style(MDS::YesNo | MDS::IconQuestion)
                .build()
                .show_modal();
                if res != ID_YES {
                    return;
                }
            }
            path
        };

        // ---- Save ----
        if let Err(e) = cfg.save(&save_path) {
            use MessageDialogStyle as MDS;
            MessageDialog::builder(&frame, &format!("Failed to save: {e}"), "Error")
                .with_style(MDS::OK | MDS::IconError)
                .build()
                .show_modal();
            return;
        }

        // ---- YouTube setup prompt (post-save) ----
        // Run the post-save action (start/restart the bot) only after any
        // YouTube install finishes; otherwise the bot launches before the
        // binaries exist and won't see them until a manual restart.
        // Only offered when creating a new config — edits of an existing
        // config save silently (tray menu has Install tools for later).
        let on_save = on_save.clone();
        if config_path.is_some() {
            on_save(save_path);
        } else {
            prompt_youtube_setup(&frame, cfg.default_service, move || on_save(save_path));
        }
        frame.close(true);
    });

    // ---- Cancel handler ----
    cancel_btn.on_click(move |_| {
        frame.close(true);
    });

    frame.show(true);
    frame.centre();
}

// ---- Helper functions for building form fields ----

fn add_text_field(parent: &Panel, sizer: &FlexGridSizer, label: &str, value: &str) -> TextCtrl {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = TextCtrl::builder(parent).build();
    input.set_value(value);
    // Programmatic accessible name (not just the adjacent StaticText's proximity)
    // so MSAA/UIA screen readers reliably announce the field's purpose.
    input.set_name(label);
    sizer.add(&lbl, 0, SizerFlag::AlignCenterVertical | SizerFlag::AlignRight, 0);
    sizer.add(&input, 1, SizerFlag::Expand, 0);
    input
}

fn add_password_field(parent: &Panel, sizer: &FlexGridSizer, label: &str, value: &str) -> TextCtrl {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = TextCtrl::builder(parent)
        .with_style(TextCtrlStyle::Password)
        .build();
    input.set_value(value);
    input.set_name(label);
    sizer.add(&lbl, 0, SizerFlag::AlignCenterVertical | SizerFlag::AlignRight, 0);
    sizer.add(&input, 1, SizerFlag::Expand, 0);
    input
}

fn add_spin_field(parent: &Panel, sizer: &FlexGridSizer, label: &str, value: i32, min: i32, max: i32) -> SpinCtrl {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = SpinCtrl::builder(parent)
        .with_range(min, max)
        .with_initial_value(value)
        .build();
    input.set_name(label);
    sizer.add(&lbl, 0, SizerFlag::AlignCenterVertical | SizerFlag::AlignRight, 0);
    sizer.add(&input, 1, SizerFlag::Expand, 0);
    input
}

fn add_checkbox(parent: &Panel, sizer: &FlexGridSizer, label: &str, value: bool) -> CheckBox {
    // Empty spacer for the label column to keep the grid aligned
    let spacer = StaticText::builder(parent).with_label("").build();
    let input = CheckBox::builder(parent).with_label(label).with_value(value).build();
    // Explicitly set the window name so screen readers announce the label.
    // wxCheckBox defaults the window name to "check" which is what gets
    // read by MSAA/UIA instead of the label text.
    input.set_name(label);
    sizer.add(&spacer, 0, SizerFlag::AlignCenterVertical, 0);
    sizer.add(&input, 0, SizerFlag::AlignCenterVertical, 0);
    input
}

fn add_choice_field(parent: &Panel, sizer: &FlexGridSizer, label: &str, choices: &[&str]) -> Choice {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = Choice::builder(parent).build();
    for c in choices {
        input.append(c);
    }
    input.set_name(label);
    sizer.add(&lbl, 0, SizerFlag::AlignCenterVertical | SizerFlag::AlignRight, 0);
    sizer.add(&input, 1, SizerFlag::Expand, 0);
    input
}

fn add_multiline_field(parent: &Panel, sizer: &FlexGridSizer, label: &str, value: &str) -> TextCtrl {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = TextCtrl::builder(parent)
        .with_style(TextCtrlStyle::MultiLine)
        .with_size(Size::new(-1, 60))
        .build();
    input.set_value(value);
    input.set_name(label);
    sizer.add(&lbl, 0, SizerFlag::Top | SizerFlag::AlignRight, 0);
    sizer.add(&input, 1, SizerFlag::Expand, 0);
    input
}

fn add_combo_field(
    parent: &Panel,
    sizer: &FlexGridSizer,
    label: &str,
    choices: &[&str],
    selected: &str,
) -> ComboBox {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = ComboBox::builder(parent)
        .with_string_choices(choices)
        .with_style(ComboBoxStyle::ReadOnly)
        .build();
    input.set_value(selected);
    input.set_name(label);
    sizer.add(&lbl, 0, SizerFlag::AlignCenterVertical | SizerFlag::AlignRight, 0);
    sizer.add(&input, 1, SizerFlag::Expand, 0);
    input
}

// ---- YouTube setup post-save flow ----

/// Offer to install the YouTube tools after a save, then run `on_complete`.
///
/// `on_complete` fires exactly once: immediately when no install is needed (or
/// declined), or when the install finishes. Callers use it to start/restart the
/// bot only after the binaries exist, so the running bot can see them.
fn prompt_youtube_setup(
    parent: &Frame,
    default_service: crate::services::Service,
    on_complete: impl FnOnce() + 'static,
) {
    use crate::services::Service;
    use crate::youtube::setup;
    use MessageDialogStyle as MDS;

    let paths = match setup::resolve_paths() {
        Ok(p) => p,
        Err(_) => {
            on_complete(); // can't resolve — skip install; not user-actionable
            return;
        }
    };

    // Skip the entire prompt if binaries are already on disk.
    if setup::is_installed(&paths) {
        on_complete();
        return;
    }

    let prompt = if default_service == Service::YouTube {
        "YouTube support requires extra binaries (~50 MB: yt-dlp, bgutil-pot, plugin). Download now?"
    } else {
        "You can also enable YouTube support. This downloads ~50 MB of binaries (yt-dlp, bgutil-pot, plugin). Skip if you only need Spotify. Install YouTube support?"
    };
    let res = MessageDialog::builder(parent, prompt, "YouTube Support")
        .with_style(MDS::YesNo | MDS::IconQuestion)
        .build()
        .show_modal();
    if res != ID_YES {
        on_complete();
        return;
    }

    crate::gui::progress::run_progress_dialog(
        "Install YouTube tools",
        |p| crate::gui::progress::youtube_install(p),
        move |_success| on_complete(),
    );
}

/// Text field with a Browse... button next to it, suitable for picking a file path.
fn add_text_with_browse(
    parent: &Panel,
    sizer: &FlexGridSizer,
    frame: Frame,
    label: &str,
    value: &str,
) -> TextCtrl {
    let lbl = StaticText::builder(parent).with_label(label).build();
    let input = TextCtrl::builder(parent).build();
    input.set_value(value);
    input.set_name(label);
    let browse = Button::builder(parent).with_label("Browse...").build();

    let row = BoxSizer::builder(Orientation::Horizontal).build();
    row.add(&input, 1, SizerFlag::Expand, 0);
    row.add(&browse, 0, SizerFlag::AlignCenterVertical | SizerFlag::Left, 4);

    sizer.add(&lbl, 0, SizerFlag::AlignCenterVertical | SizerFlag::AlignRight, 0);
    sizer.add_sizer(&row, 1, SizerFlag::Expand, 0);

    let input_for_handler = input;
    browse.on_click(move |_| {
        let dlg = FileDialog::builder(&frame)
            .with_message("Select cookies file")
            .build();
        if dlg.show_modal() == ID_OK {
            if let Some(path) = dlg.get_path() {
                input_for_handler.set_value(&path);
            }
        }
    });
    input
}
