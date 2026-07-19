use std::collections::HashSet;

use crate::config::{AdminMode, BotConfig};

/// TeamTalk `USERTYPE_ADMIN` (see TeamTalk.h). A server admin's `user_type`
/// has this bit set. Kept local so this module has no FFI dependency.
const USERTYPE_ADMIN: u32 = 0x02;

/// Commands that require admin. Append here to gate more.
pub const ADMIN_COMMANDS: &[&str] = &["q", "quit", "rs", "restart", "jc", "glang"];

/// True if `cmd` (already lowercased by the dispatcher) is admin-gated.
pub fn is_admin_command(cmd: &str) -> bool {
    ADMIN_COMMANDS.contains(&cmd)
}

/// Split a raw admin-list string (comma- and/or newline-separated) into
/// trimmed, non-empty usernames. Shared by the GUI dialog and the CLI wizard.
pub fn parse_admin_list(raw: &str) -> Vec<String> {
    raw.split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolved admin policy, built once from config at startup.
pub struct AdminAuth {
    mode: AdminMode,
    users: HashSet<String>, // lowercased usernames
}

impl AdminAuth {
    pub fn from_config(cfg: &BotConfig) -> AdminAuth {
        let users = cfg
            .admins
            .iter()
            .map(|u| u.trim().to_lowercase())
            .filter(|u| !u.is_empty())
            .collect();
        AdminAuth {
            mode: cfg.admin_mode,
            users,
        }
    }

    /// Whether the sender may run an admin-gated command.
    /// `user_type` is the sender's TeamTalk USERTYPE bitmask.
    pub fn is_admin(&self, username: &str, user_type: u32) -> bool {
        let by_rights = user_type & USERTYPE_ADMIN != 0;
        let by_list = self.users.contains(&username.to_lowercase());
        match self.mode {
            AdminMode::Everyone => true,
            AdminMode::TtRights => by_rights,
            AdminMode::List => by_list,
            AdminMode::Both => by_rights || by_list,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdminMode, BotConfig};

    fn auth(mode: AdminMode, names: &[&str]) -> AdminAuth {
        let cfg = BotConfig {
            admin_mode: mode,
            admins: names.iter().map(|s| s.to_string()).collect(),
            ..BotConfig::default()
        };
        AdminAuth::from_config(&cfg)
    }

    const ADMIN_TYPE: u32 = 0x02;
    const DEFAULT_TYPE: u32 = 0x01;

    #[test]
    fn everyone_is_always_admin() {
        let a = auth(AdminMode::Everyone, &[]);
        assert!(a.is_admin("", DEFAULT_TYPE));
        assert!(a.is_admin("nobody", 0));
    }

    #[test]
    fn ttrights_checks_user_type_only() {
        let a = auth(AdminMode::TtRights, &["alice"]);
        assert!(a.is_admin("stranger", ADMIN_TYPE));
        assert!(!a.is_admin("alice", DEFAULT_TYPE)); // list ignored in this mode
    }

    #[test]
    fn list_checks_username_case_insensitively() {
        let a = auth(AdminMode::List, &["Alice", "bob"]);
        assert!(a.is_admin("alice", DEFAULT_TYPE));
        assert!(a.is_admin("BOB", DEFAULT_TYPE));
        assert!(!a.is_admin("carol", ADMIN_TYPE)); // user_type ignored in this mode
    }

    #[test]
    fn both_accepts_either_signal() {
        let a = auth(AdminMode::Both, &["alice"]);
        assert!(a.is_admin("alice", DEFAULT_TYPE)); // via list
        assert!(a.is_admin("stranger", ADMIN_TYPE)); // via rights
        assert!(!a.is_admin("stranger", DEFAULT_TYPE));
    }

    #[test]
    fn admin_commands_are_recognized() {
        for c in ["q", "quit", "rs", "restart", "jc", "glang"] {
            assert!(is_admin_command(c), "{c} should be gated");
        }
        for c in ["p", "n", "search", "h", "queue", "v", "lang"] {
            assert!(!is_admin_command(c), "{c} should be open");
        }
    }

    #[test]
    fn parse_admin_list_splits_and_trims() {
        assert_eq!(
            parse_admin_list("alice, bob\n  carol \n\n,"),
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()]
        );
        assert!(parse_admin_list("   ").is_empty());
    }
}
