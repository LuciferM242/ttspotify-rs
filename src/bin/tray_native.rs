//! Temporary launcher for the native Win32 tray, so it can be driven before it
//! replaces the wx one in `main.rs`.
//!
//! Delete this once `main.rs` switches over. It exists because wx and winsafe
//! each want to own the thread's message loop, so the two trays cannot both be
//! reachable from one binary.
//!
//! A console is left attached on purpose: warnings go to it while testing.

#[cfg(windows)]
fn main() {
    tt_spotify_bot::logging::install_panic_hook();
    eprintln!("Native tray starting. This starts your configured bots, exactly");
    eprintln!("as the real tray does - do not run it beside another instance.");
    eprintln!("Dialog-backed menu items report that they are not ported yet.");
    tt_spotify_bot::gui_native::tray::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The native tray is Windows-only.");
}
