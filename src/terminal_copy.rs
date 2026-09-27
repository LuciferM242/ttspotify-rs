//! Copying a pair-code link from a terminal, including one reached over SSH.
//!
//! The text goes to the terminal as an OSC 52 sequence, and the terminal on
//! the person's own machine puts it on the clipboard. Nothing reports whether
//! it did: a terminal without support ignores the sequence silently.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The OSC 52 sequence that puts `text` on the clipboard.
pub fn osc52(text: &str) -> String {
    format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

/// Ask the terminal to copy `text`. False when stdout is not a terminal.
pub fn copy(text: &str) -> bool {
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return false;
    }
    out.write_all(osc52(text).as_bytes()).and_then(|()| out.flush()).is_ok()
}

/// The key that copies again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKey {
    /// c, read key by key.
    C,
    /// Enter, where the terminal only hands over whole lines.
    Enter,
}

#[derive(Debug, PartialEq, Eq)]
enum Press {
    Copy,
    Cancel,
    Nothing,
}

fn press(key: CopyKey, event: &KeyEvent) -> Press {
    // Windows reports releases too; one press is one copy.
    if event.kind != KeyEventKind::Press {
        return Press::Nothing;
    }
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    match (key, event.code) {
        // Key by key, the terminal no longer turns Ctrl+C into a signal.
        (CopyKey::C, KeyCode::Char('c' | 'C')) if ctrl => Press::Cancel,
        (CopyKey::C, KeyCode::Char('c' | 'C')) => Press::Copy,
        (CopyKey::Enter, KeyCode::Enter) => Press::Copy,
        _ => Press::Nothing,
    }
}

/// Listens for the copy key until dropped.
pub struct CopyKeys {
    pub key: CopyKey,
    raw: bool,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CopyKeys {
    /// Copy `text` again whenever the key is pressed. A Ctrl+C sends on
    /// `cancel`. `None` when stdin is not a terminal: nobody is there to press
    /// anything.
    pub fn start(text: String, cancel: tokio::sync::oneshot::Sender<()>) -> Option<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return None;
        }
        let raw = crossterm::terminal::enable_raw_mode().is_ok();
        let key = if raw { CopyKey::C } else { CopyKey::Enter };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut cancel = Some(cancel);
            // Polled rather than blocking on a read, so dropping the listener
            // stops it and the next prompt gets its own input.
            while !stop2.load(Ordering::Relaxed) {
                match crossterm::event::poll(Duration::from_millis(100)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break,
                }
                let Ok(Event::Key(event)) = crossterm::event::read() else {
                    continue;
                };
                match press(key, &event) {
                    Press::Copy => {
                        copy(&text);
                        say("Link copied again.", raw);
                    }
                    Press::Cancel => {
                        if let Some(cancel) = cancel.take() {
                            let _ = cancel.send(());
                        }
                        break;
                    }
                    Press::Nothing => {}
                }
            }
        });
        Some(Self { key, raw, stop, thread: Some(thread) })
    }
}

impl Drop for CopyKeys {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Print a line. Key by key, a newline no longer returns the cursor.
fn say(line: &str, raw: bool) {
    let mut out = std::io::stdout();
    let _ = write!(out, "  {line}{}", if raw { "\r\n" } else { "\n" });
    let _ = out.flush();
}

/// What to say about copying, if anything.
pub fn copy_hint(copied: bool, key: Option<CopyKey>) -> Option<String> {
    if !copied {
        return None;
    }
    let first = "The link is on your clipboard too, if your terminal allows it.";
    Some(match key {
        Some(CopyKey::C) => format!("{first} Press c to copy it again."),
        Some(CopyKey::Enter) => format!("{first} Press Enter to copy it again."),
        None => first.to_string(),
    })
}

/// Print `line` while the listener may be reading key by key.
pub fn say_while(keys: Option<&CopyKeys>, line: &str) {
    say(line, keys.is_some_and(|k| k.raw));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        let mut event = KeyEvent::new(code, modifiers);
        event.kind = kind;
        event
    }

    fn down(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    #[test]
    fn the_sequence_carries_the_text_in_base64() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn c_copies_and_ctrl_c_still_cancels() {
        assert_eq!(press(CopyKey::C, &down(KeyCode::Char('c'))), Press::Copy);
        assert_eq!(press(CopyKey::C, &down(KeyCode::Char('C'))), Press::Copy);
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL, KeyEventKind::Press);
        assert_eq!(press(CopyKey::C, &ctrl_c), Press::Cancel);
    }

    #[test]
    fn enter_copies_only_where_enter_is_the_key() {
        assert_eq!(press(CopyKey::Enter, &down(KeyCode::Enter)), Press::Copy);
        assert_eq!(press(CopyKey::C, &down(KeyCode::Enter)), Press::Nothing);
        // A typed c arrives with its Enter in line mode; only the Enter copies.
        assert_eq!(press(CopyKey::Enter, &down(KeyCode::Char('c'))), Press::Nothing);
    }

    #[test]
    fn a_release_is_not_a_second_press() {
        let up = key(KeyCode::Char('c'), KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(press(CopyKey::C, &up), Press::Nothing);
    }

    #[test]
    fn the_hint_names_the_key_in_use() {
        assert_eq!(copy_hint(false, Some(CopyKey::C)), None, "nothing was copied");
        assert!(copy_hint(true, Some(CopyKey::C)).unwrap().ends_with("Press c to copy it again."));
        assert!(copy_hint(true, Some(CopyKey::Enter)).unwrap().ends_with("Press Enter to copy it again."));
        assert!(!copy_hint(true, None).unwrap().contains("Press"));
    }
}
