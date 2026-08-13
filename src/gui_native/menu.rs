//! The tray popup menu, as data.
//!
//! The menu is described here and rendered by the Win32 layer, so what a click
//! means is decided once, when the menu is built, and looked up by command id
//! afterwards. Deriving the bot from the clicked item's position instead would
//! misfire whenever the bot list changed while the menu was open: "Stop" would
//! reach whichever bot had slid into that slot.

use std::collections::HashMap;

/// A per-bot menu command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotAction {
    Start,
    Stop,
    Restart,
    Logs,
    Config,
}

/// What activating a menu item does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Bot { name: String, action: BotAction },
    AddServer,
    SpotifyAuth,
    YoutubeInstall,
    YoutubeUpdate,
    CheckUpdates,
    Settings,
    Exit,
}

/// One rendered entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item {
        id: u16,
        label: String,
        enabled: bool,
    },
    Separator,
    Submenu {
        label: String,
        items: Vec<MenuEntry>,
    },
}

/// The Spotify line in the menu. Naming the account matters when several bots
/// share a machine: "signed in" alone does not say whose account is playing.
pub fn spotify_label(signed_in: bool, user: Option<&str>) -> String {
    match (signed_in, user) {
        (true, Some(name)) => format!("Spotify: signed in as {name}"),
        // Spotify does not always return a username with the stored
        // credentials, so a sign-in without a name is normal, not an error.
        (true, None) => "Spotify: signed in".to_string(),
        (false, _) => "Spotify: not signed in".to_string(),
    }
}

/// Facts the menu needs that don't come from the bot list.
#[derive(Debug, Clone)]
pub struct MenuFacts {
    pub spotify_signed_in: bool,
    pub youtube_installed: bool,
    /// The signed-in Spotify account, when it is known.
    pub spotify_user: Option<String>,
}

/// A built menu: what to draw, and what each command ID means.
#[derive(Debug, Default)]
pub struct MenuModel {
    pub entries: Vec<MenuEntry>,
    actions: HashMap<u16, MenuAction>,
}

impl MenuModel {
    /// The action for a command ID, or `None` if the ID isn't ours.
    pub fn action(&self, id: u16) -> Option<&MenuAction> {
        self.actions.get(&id)
    }
}

// Fixed IDs for the commands that are always the same. Zero is avoided:
// TrackPopupMenu returns 0 to mean "dismissed without choosing".
const ID_ADD_SERVER: u16 = 1;
const ID_SPOTIFY_AUTH: u16 = 2;
const ID_YT_INSTALL: u16 = 3;
const ID_YT_UPDATE: u16 = 4;
const ID_CHECK_UPDATES: u16 = 5;
const ID_SETTINGS: u16 = 6;
const ID_EXIT: u16 = 7;

/// First ID handed out to a bot. Above the fixed commands, with room to add
/// more of them without disturbing bots.
const BOT_ID_BASE: u16 = 100;
/// IDs reserved per bot: one per `BotAction`, rounded up for future ones.
const IDS_PER_BOT: u16 = 8;

/// Builds tray menus, remembering which ID belongs to which bot.
///
/// The mapping has to outlive a single build. Handing out IDs in list order
/// would let a removed bot's IDs be reused by whichever bot took its place, so
/// a click on an already-open menu could reach the wrong bot. A name keeps its
/// slot for the lifetime of the process instead.
#[derive(Debug, Default)]
pub struct MenuBuilder {
    slots: HashMap<String, u16>,
    next_slot: u16,
}

impl MenuBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// This bot's reserved ID block, assigning one if it is new.
    fn slot_for(&mut self, name: &str) -> u16 {
        if let Some(base) = self.slots.get(name) {
            return *base;
        }
        let base = BOT_ID_BASE + self.next_slot * IDS_PER_BOT;
        self.next_slot += 1;
        self.slots.insert(name.to_string(), base);
        base
    }

    /// Build the menu for the given bots (name, status text, running).
    pub fn build(&mut self, bots: &[(String, String, bool)], facts: MenuFacts) -> MenuModel {
        let mut model = MenuModel::default();

        for (name, status, running) in bots {
            let base = self.slot_for(name);
            let mut items = Vec::new();
            for (offset, action, label, enabled) in [
                (0u16, BotAction::Start, "Start", !*running),
                (1, BotAction::Stop, "Stop", *running),
                (2, BotAction::Restart, "Restart", true),
            ] {
                let id = base + offset;
                items.push(MenuEntry::Item {
                    id,
                    label: label.to_string(),
                    enabled,
                });
                model.actions.insert(
                    id,
                    MenuAction::Bot {
                        name: name.clone(),
                        action,
                    },
                );
            }
            items.push(MenuEntry::Separator);
            for (offset, action, label) in [
                (3u16, BotAction::Logs, "View Logs"),
                (4, BotAction::Config, "Edit Config"),
            ] {
                let id = base + offset;
                items.push(MenuEntry::Item {
                    id,
                    label: label.to_string(),
                    enabled: true,
                });
                model.actions.insert(
                    id,
                    MenuAction::Bot {
                        name: name.clone(),
                        action,
                    },
                );
            }
            model.entries.push(MenuEntry::Submenu {
                label: format!("{name} - {status}"),
                items,
            });
        }

        if !bots.is_empty() {
            model.entries.push(MenuEntry::Separator);
        }

        model.entries.push(MenuEntry::Submenu {
            label: spotify_label(facts.spotify_signed_in, facts.spotify_user.as_deref()),
            items: vec![MenuEntry::Item {
                id: ID_SPOTIFY_AUTH,
                label: "Sign in / re-authenticate".to_string(),
                enabled: true,
            }],
        });
        model
            .actions
            .insert(ID_SPOTIFY_AUTH, MenuAction::SpotifyAuth);

        let yt_label = if facts.youtube_installed {
            "YouTube tools: installed"
        } else {
            "YouTube tools: not installed"
        };
        model.entries.push(MenuEntry::Submenu {
            label: yt_label.to_string(),
            items: vec![
                MenuEntry::Item {
                    id: ID_YT_INSTALL,
                    label: "Install tools".to_string(),
                    enabled: !facts.youtube_installed,
                },
                MenuEntry::Item {
                    id: ID_YT_UPDATE,
                    label: "Update tools".to_string(),
                    enabled: facts.youtube_installed,
                },
            ],
        });
        model
            .actions
            .insert(ID_YT_INSTALL, MenuAction::YoutubeInstall);
        model.actions.insert(ID_YT_UPDATE, MenuAction::YoutubeUpdate);

        model.entries.push(MenuEntry::Separator);
        for (id, label, action) in [
            (ID_ADD_SERVER, "Add Server", MenuAction::AddServer),
            (
                ID_CHECK_UPDATES,
                "Check for updates",
                MenuAction::CheckUpdates,
            ),
            (ID_SETTINGS, "Settings", MenuAction::Settings),
        ] {
            model.entries.push(MenuEntry::Item {
                id,
                label: label.to_string(),
                enabled: true,
            });
            model.actions.insert(id, action);
        }

        model.entries.push(MenuEntry::Separator);
        model.entries.push(MenuEntry::Item {
            id: ID_EXIT,
            label: "Exit".to_string(),
            enabled: true,
        });
        model.actions.insert(ID_EXIT, MenuAction::Exit);

        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> MenuFacts {
        MenuFacts {
            spotify_signed_in: false,
            youtube_installed: false,
            spotify_user: None,
        }
    }

    fn bot(name: &str, status: &str, running: bool) -> (String, String, bool) {
        (name.to_string(), status.to_string(), running)
    }

    /// Every command ID appearing anywhere in the entry tree.
    fn ids(entries: &[MenuEntry]) -> Vec<u16> {
        let mut out = Vec::new();
        for e in entries {
            match e {
                MenuEntry::Item { id, .. } => out.push(*id),
                MenuEntry::Submenu { items, .. } => out.extend(ids(items)),
                MenuEntry::Separator => {}
            }
        }
        out
    }

    fn find(entries: &[MenuEntry], id: u16) -> Option<&MenuEntry> {
        for e in entries {
            match e {
                MenuEntry::Item { id: i, .. } if *i == id => return Some(e),
                MenuEntry::Submenu { items, .. } => {
                    if let Some(f) = find(items, id) {
                        return Some(f);
                    }
                }
                _ => {}
            }
        }
        None
    }

    #[test]
    fn the_spotify_line_names_the_account_when_it_is_known() {
        // With several bots on one machine, "signed in" alone does not say
        // whose account is playing.
        assert_eq!(
            spotify_label(true, Some("aloys")),
            "Spotify: signed in as aloys"
        );
    }

    #[test]
    fn a_sign_in_without_a_name_still_reads_as_signed_in() {
        // Spotify does not always return a username with the stored
        // credentials, and that is normal - it must not read as an error or
        // as being signed out.
        assert_eq!(spotify_label(true, None), "Spotify: signed in");
    }

    #[test]
    fn being_signed_out_never_shows_a_name() {
        // A stale name beside "not signed in" would be worse than no name.
        assert_eq!(spotify_label(false, None), "Spotify: not signed in");
        assert_eq!(spotify_label(false, Some("aloys")), "Spotify: not signed in");
    }

    #[test]
    fn the_menu_shows_the_account_name() {
        let menu = MenuBuilder::new().build(
            &[],
            MenuFacts {
                spotify_signed_in: true,
                youtube_installed: false,
                spotify_user: Some("aloys".to_string()),
            },
        );
        let labels: Vec<&str> = menu
            .entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Submenu { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            labels.iter().any(|l| l.contains("aloys")),
            "got: {labels:?}"
        );
    }

    #[test]
    fn every_command_id_resolves_to_an_action() {
        let bots = [bot("alpha", "Connected, Idle", true)];
        let menu = MenuBuilder::new().build(&bots, facts());
        let all = ids(&menu.entries);
        assert!(!all.is_empty(), "menu produced no commands");
        for id in all {
            assert!(menu.action(id).is_some(), "id {id} maps to nothing");
        }
    }

    #[test]
    fn command_ids_are_unique() {
        // A duplicate ID means one menu item silently triggers another's action.
        let bots = [
            bot("alpha", "Connected, Idle", true),
            bot("beta", "Stopped", false),
        ];
        let menu = MenuBuilder::new().build(&bots, facts());
        let all = ids(&menu.entries);
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "duplicate command ids: {all:?}");
    }

    #[test]
    fn each_bot_gets_its_own_five_commands() {
        let bots = [
            bot("alpha", "Connected, Idle", true),
            bot("beta", "Stopped", false),
        ];
        let menu = MenuBuilder::new().build(&bots, facts());
        for name in ["alpha", "beta"] {
            let mut actions: Vec<BotAction> = ids(&menu.entries)
                .iter()
                .filter_map(|id| match menu.action(*id) {
                    Some(MenuAction::Bot { name: n, action }) if n == name => Some(*action),
                    _ => None,
                })
                .collect();
            actions.sort_by_key(|a| format!("{a:?}"));
            assert_eq!(
                actions,
                vec![
                    BotAction::Config,
                    BotAction::Logs,
                    BotAction::Restart,
                    BotAction::Start,
                    BotAction::Stop
                ],
                "wrong commands for {name}"
            );
        }
    }

    #[test]
    fn an_id_from_a_stale_menu_does_not_resolve_to_the_wrong_bot() {
        // The bug this module exists to prevent. Build a menu with two bots,
        // note the ID that means "stop beta", then rebuild after alpha is
        // removed. Position-based lookup would now read that ID as bot 0.
        let two = [
            bot("alpha", "Connected, Idle", true),
            bot("beta", "Connected, Idle", true),
        ];
        let mut builder = MenuBuilder::new();
        let menu = builder.build(&two, facts());
        let stop_beta = ids(&menu.entries)
            .into_iter()
            .find(|id| {
                matches!(menu.action(*id), Some(MenuAction::Bot { name, action })
                    if name == "beta" && *action == BotAction::Stop)
            })
            .expect("no stop command for beta");

        let one = [bot("beta", "Connected, Idle", true)];
        let rebuilt = builder.build(&one, facts());
        // A name keeps its ID block for the life of the process, so the click
        // still reaches beta. Merely resolving to nothing would be safe but is
        // not what we promise: it would silently drop the user's click.
        assert_eq!(
            rebuilt.action(stop_beta),
            Some(&MenuAction::Bot {
                name: "beta".to_string(),
                action: BotAction::Stop
            }),
            "an id must keep meaning the same bot across rebuilds"
        );
    }

    #[test]
    fn start_is_disabled_while_running_and_stop_while_stopped() {
        let bots = [
            bot("running", "Connected, Idle", true),
            bot("idle", "Stopped", false),
        ];
        let menu = MenuBuilder::new().build(&bots, facts());
        for (bot_name, action, expect_enabled) in [
            ("running", BotAction::Start, false),
            ("running", BotAction::Stop, true),
            ("idle", BotAction::Start, true),
            ("idle", BotAction::Stop, false),
        ] {
            let id = ids(&menu.entries)
                .into_iter()
                .find(|id| {
                    matches!(menu.action(*id), Some(MenuAction::Bot { name, action: a })
                        if name == bot_name && *a == action)
                })
                .unwrap_or_else(|| panic!("no {action:?} for {bot_name}"));
            match find(&menu.entries, id) {
                Some(MenuEntry::Item { enabled, .. }) => assert_eq!(
                    *enabled, expect_enabled,
                    "{bot_name} {action:?} enabled should be {expect_enabled}"
                ),
                other => panic!("expected an item, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_global_commands_are_always_present() {
        let menu = MenuBuilder::new().build(&[], facts());
        let present: Vec<&MenuAction> = ids(&menu.entries)
            .iter()
            .filter_map(|id| menu.action(*id))
            .collect();
        for wanted in [
            MenuAction::AddServer,
            MenuAction::SpotifyAuth,
            MenuAction::CheckUpdates,
            MenuAction::Settings,
            MenuAction::Exit,
        ] {
            assert!(present.contains(&&wanted), "{wanted:?} missing from the menu");
        }
    }

    #[test]
    fn install_and_update_are_offered_according_to_what_is_installed() {
        for installed in [false, true] {
            let facts = MenuFacts {
                spotify_signed_in: false,
                youtube_installed: installed,
                spotify_user: None,
            };
            let menu = MenuBuilder::new().build(&[], facts);
            for (action, expect_enabled) in [
                (MenuAction::YoutubeInstall, !installed),
                (MenuAction::YoutubeUpdate, installed),
            ] {
                let id = ids(&menu.entries)
                    .into_iter()
                    .find(|id| menu.action(*id) == Some(&action))
                    .unwrap_or_else(|| panic!("{action:?} missing"));
                match find(&menu.entries, id) {
                    Some(MenuEntry::Item { enabled, .. }) => assert_eq!(
                        *enabled, expect_enabled,
                        "{action:?} with installed={installed}"
                    ),
                    other => panic!("expected an item, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_bot_submenu_is_labelled_with_its_name_and_status() {
        let bots = [bot("alpha", "Connected, Playing: Song", true)];
        let menu = MenuBuilder::new().build(&bots, facts());
        let labels: Vec<&str> = menu
            .entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Submenu { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            labels.contains(&"alpha - Connected, Playing: Song"),
            "got: {labels:?}"
        );
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        let menu = MenuBuilder::new().build(&[bot("alpha", "Stopped", false)], facts());
        assert_eq!(menu.action(u16::MAX), None);
    }
}
