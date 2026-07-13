//! Windows per-user autostart via the HKCU Run key. We touch exactly one value
//! (VALUE_NAME) under one key — never the key itself, never HKLM, never other
//! values — so we can't orphan entries or need admin. Shows up in Task Manager
//! and Settings > Apps > Startup under VALUE_NAME.

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "ttspotify-rs";

fn quoted_exe_path() -> std::io::Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("\"{}\"", exe.display()))
}

/// True if our autostart value exists under HKCU Run.
pub fn is_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) {
        Ok(key) => key.get_value::<String, _>(VALUE_NAME).is_ok(),
        Err(_) => false,
    }
}

/// Create (on) or delete (off) our single autostart value. On writes/overwrites
/// with the current exe path (self-heals a stale path); off deletes only our
/// value, leaving the key and any other values untouched.
pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)?;
    if enabled {
        key.set_value(VALUE_NAME, &quoted_exe_path()?)
    } else {
        match key.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Round-trips through the real (per-user) registry. Safe: uses our own value
    // name, cleans up after itself, requires no admin. Restores prior state.
    #[test]
    fn toggle_round_trip() {
        let prior = is_enabled();

        set_enabled(true).unwrap();
        assert!(is_enabled());

        set_enabled(false).unwrap();
        assert!(!is_enabled());

        // idempotent off
        set_enabled(false).unwrap();
        assert!(!is_enabled());

        // restore prior state
        set_enabled(prior).unwrap();
    }
}
