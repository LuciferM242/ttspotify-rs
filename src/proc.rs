//! Helpers for spawning child processes.

/// Keep a child console program from flashing a command window when the
/// parent is a GUI process with no console (the Windows tray). No-op on other
/// platforms.
///
/// This covers the spawned program itself and no further: a console is
/// inherited, and this flag denies the child one, so anything *it* spawns is
/// handed a fresh console by Windows. See `spawn_ytdlp_with_client` for where
/// that limit actually bites.
pub fn hide_console_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
