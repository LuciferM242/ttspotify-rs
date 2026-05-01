//! Service-agnostic media player trait.
//!
//! Both Spotify (librespot-backed) and YouTube (rustypipe-backed in Phase 3)
//! implement this so the runner can drive either through one interface.
//! The audio sink is shared — both implementations push 44.1k stereo PCM
//! into the same `crossbeam_channel<Vec<i16>>` consumed by the audio
//! pipeline.

pub trait MediaPlayer: Send + Sync {
    /// Load and start playing the given service-specific URI.
    fn load(&self, uri: &str);

    /// Resume playback.
    fn play(&self);

    /// Pause playback (preserve position).
    fn pause(&self);

    /// Stop playback and release any held resources.
    fn stop(&self);

    /// Seek to absolute position in milliseconds.
    fn seek(&self, position_ms: u32);

    /// Hint the player to begin fetching the given URI in the background.
    /// Implementations may treat this as a no-op.
    fn preload(&self, uri: &str);
}
