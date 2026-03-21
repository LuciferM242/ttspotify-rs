use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::spotify::types::SpotifyTrack;

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub track: SpotifyTrack,
    #[allow(dead_code)] // stored for future "who queued this" display
    pub requester: String,
    /// Only allow radio recommendations for single-track plays (not playlists/albums)
    pub allow_recommend: bool,
}

#[derive(Debug)]
pub struct PlayerState {
    pub queue: Vec<QueueEntry>,
    pub current_index: Option<usize>,
    pub is_playing: bool,
    pub is_paused: bool,
    pub is_loading: bool,

    // Modes
    pub repeat_track: bool,
    pub repeat_queue: bool,
    pub shuffle: bool,

    // Radio
    pub radio_enabled: bool,

    // Search session (user_id → results)
    pub search_results: HashMap<i32, Vec<SpotifyTrack>>,

    // Track position tracking
    pub position_ms: u32,
}

pub type SharedState = Arc<Mutex<PlayerState>>;

impl PlayerState {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            current_index: None,
            is_playing: false,
            is_paused: false,
            is_loading: false,
            repeat_track: false,
            repeat_queue: false,
            shuffle: false,
            radio_enabled: false,
            search_results: HashMap::new(),
            position_ms: 0,
        }
    }

    pub fn current(&self) -> Option<&QueueEntry> {
        self.current_index.and_then(|i| self.queue.get(i))
    }

    pub fn enqueue(&mut self, track: SpotifyTrack, requester: String, allow_recommend: bool) {
        self.queue.push(QueueEntry { track, requester, allow_recommend });
        if self.current_index.is_none() {
            self.current_index = Some(0);
        }
    }

    pub fn enqueue_all(&mut self, tracks: Vec<SpotifyTrack>, requester: String, allow_recommend: bool) {
        let was_empty = self.queue.is_empty();
        for track in tracks {
            self.queue.push(QueueEntry {
                track,
                requester: requester.clone(),
                allow_recommend,
            });
        }
        if was_empty && !self.queue.is_empty() {
            self.current_index = Some(0);
        }
    }

    /// Advance to the next track. Returns the next entry if available.
    pub fn advance(&mut self) -> Option<&QueueEntry> {
        if self.queue.is_empty() {
            self.current_index = None;
            return None;
        }

        if self.repeat_track {
            return self.current();
        }

        if self.shuffle {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            let current = self.current_index.unwrap_or(0);
            // Only shuffle among upcoming tracks (after current), excluding current
            let remaining: Vec<usize> = ((current + 1)..self.queue.len())
                .filter(|&i| i != current)
                .collect();
            if !remaining.is_empty() {
                let idx = remaining[rng.gen_range(0..remaining.len())];
                self.current_index = Some(idx);
                return self.queue.get(idx);
            } else if self.repeat_queue && self.queue.len() > 1 {
                // All tracks played, re-shuffle from start (excluding the one that just played)
                let others: Vec<usize> = (0..self.queue.len()).filter(|&i| i != current).collect();
                if !others.is_empty() {
                    let idx = others[rng.gen_range(0..others.len())];
                    self.current_index = Some(idx);
                    return self.queue.get(idx);
                }
            }
            // Fallthrough: no more tracks
            self.current_index = None;
            return None;
        }

        if let Some(idx) = self.current_index {
            let next = idx + 1;
            if next < self.queue.len() {
                self.current_index = Some(next);
                return self.queue.get(next);
            } else if self.repeat_queue {
                self.current_index = Some(0);
                return self.queue.first();
            } else {
                self.current_index = None;
                return None;
            }
        }

        None
    }

    /// Go to previous track.
    pub fn go_prev(&mut self) -> Option<&QueueEntry> {
        if self.queue.is_empty() {
            return None;
        }

        if let Some(idx) = self.current_index {
            if idx > 0 {
                self.current_index = Some(idx - 1);
            } else if self.repeat_queue {
                self.current_index = Some(self.queue.len() - 1);
            }
        } else {
            self.current_index = Some(self.queue.len() - 1);
        }

        self.current()
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.current_index = None;
        self.is_playing = false;
        self.is_paused = false;
        self.is_loading = false;
        self.position_ms = 0;
    }

    pub fn remove(&mut self, index: usize) -> Option<QueueEntry> {
        if index >= self.queue.len() {
            return None;
        }
        let entry = self.queue.remove(index);

        // Adjust current index
        if let Some(ref mut cur) = self.current_index {
            if index < *cur {
                *cur -= 1;
            } else if index == *cur {
                if self.queue.is_empty() {
                    self.current_index = None;
                } else if *cur >= self.queue.len() {
                    *cur = self.queue.len() - 1;
                }
            }
        }

        Some(entry)
    }

    pub fn queue_display(&self) -> String {
        if self.queue.is_empty() {
            return "Queue is empty".to_string();
        }

        let mut lines = Vec::new();
        for (i, entry) in self.queue.iter().enumerate() {
            let marker = if self.current_index == Some(i) { "> " } else { "  " };
            lines.push(format!(
                "{}{}: {} [{}]",
                marker,
                i + 1,
                entry.track.display_name(),
                entry.track.duration_display()
            ));
        }
        lines.join("\n")
    }

    pub fn mode_display(&self) -> String {
        let mut modes = Vec::new();
        if self.repeat_track {
            modes.push("Repeat Track");
        }
        if self.repeat_queue {
            modes.push("Repeat Queue");
        }
        if self.shuffle {
            modes.push("Shuffle");
        }
        if modes.is_empty() {
            "No modes active".to_string()
        } else {
            modes.join(", ")
        }
    }
}
