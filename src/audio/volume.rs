//! Volume, on the curve loudness is actually heard along.
//!
//! A percentage scaled straight onto the samples is linear in amplitude, which
//! is not how hearing works: half the amplitude is nowhere near half as loud,
//! so most of the usable range bunches up at the bottom of the dial and the top
//! half barely changes anything.
//!
//! The mapping is librespot's own, not a copy of it: `CubicMapping` is what the
//! Spotify client applies to its volume slider. Calling it directly means the
//! two cannot drift, and it applies to YouTube as well, since everything is
//! mixed before this point.

use librespot_playback::mixer::mappings::{CubicMapping, VolumeMapping};

/// Decibel range the curve spans. librespot defaults to 60, which is tuned
/// for personal listening and left the middle of the dial well below other
/// sound on a TeamTalk server. 30 keeps the perceptual shape but lifts the
/// middle (50 plays at amplitude ~0.29, 70 at ~0.50); 0 is still silence and
/// 100 is still untouched full scale.
pub const DB_RANGE: f64 = 30.0;

/// Amplitude for a 0-100 setting, along the perceived-loudness curve.
pub fn amplitude_for_percent(percent: u8) -> f32 {
    // Both ends are special-cased, as they are inside librespot, and for the
    // same reason: the curve bottoms out at -60 dB rather than at zero, so
    // without this a volume of 0 would be very quiet but still audible. The
    // top is pinned to defend against rounding drift.
    match percent.min(100) {
        0 => 0.0,
        100 => 1.0,
        p => CubicMapping::linear_to_mapped((p as f64) / 100.0, DB_RANGE) as f32,
    }
}

/// The setting that produces `amplitude`, i.e. the inverse of
/// [`amplitude_for_percent`]. Used once per config to carry a volume saved
/// under the old linear scaling onto the curve without changing how loud it is.
pub fn percent_for_amplitude(amplitude: f32) -> u8 {
    let mapped = (amplitude as f64).clamp(0.0, 1.0);
    // Mirrors the guards in `amplitude_for_percent`, so the two round-trip at
    // the ends instead of the inverse curve pulling 0 up off silence.
    if mapped <= 0.0 {
        return 0;
    }
    if mapped >= 1.0 {
        return 100;
    }
    let linear = CubicMapping::mapped_to_linear(mapped, DB_RANGE);
    (linear * 100.0).round().clamp(0.0, 100.0) as u8
}

/// Volume controller with smooth ramping between volume levels.
pub struct VolumeController {
    target_scale: f32,
    current_scale: f32,
    ramp_step: f32,
}

impl VolumeController {
    /// Start at the configured setting, not at some fixed default: the first
    /// frames of the first track play at `current_scale` before any ramping,
    /// and a controller seeded at 50% made a bot configured to volume 20 play
    /// ~6x too loud for its first ~100ms — audibly so for a muted (volume 0)
    /// bot, which briefly played every track's opening at half volume.
    pub fn new(percent: u8, max_percent: u8, ramp_step: f32) -> Self {
        let start = amplitude_for_percent(percent.min(max_percent));
        Self {
            target_scale: start,
            current_scale: start,
            ramp_step,
        }
    }

    /// Set target volume from user percentage (0-100) capped by max_percent.
    pub fn set_target(&mut self, percent: u8, max_percent: u8) {
        let capped = percent.min(max_percent);
        self.target_scale = amplitude_for_percent(capped);
    }

    /// Apply volume scaling with smooth ramping to the given samples.
    /// Ramp step is applied once per frame (not per sample) for smooth transitions.
    pub fn apply(&mut self, samples: &mut [i16]) {
        // Ramp current_scale toward target_scale once per frame
        if (self.current_scale - self.target_scale).abs() > self.ramp_step {
            if self.current_scale < self.target_scale {
                self.current_scale += self.ramp_step;
            } else {
                self.current_scale -= self.ramp_step;
            }
        } else {
            self.current_scale = self.target_scale;
        }

        // Apply the same scale to all samples in this frame
        for sample in samples.iter_mut() {
            let scaled = (*sample as f32 * self.current_scale).clamp(-32768.0, 32767.0);
            *sample = scaled as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the curve itself --

    #[test]
    fn zero_is_silence_and_full_is_untouched() {
        // The curve bottoms out at -60 dB rather than at zero, so both ends are
        // pinned. Without that, "volume 0" would be quiet but still audible.
        assert_eq!(amplitude_for_percent(0), 0.0);
        assert_eq!(amplitude_for_percent(100), 1.0);
    }

    #[test]
    fn a_setting_maps_along_the_curve_not_straight_to_a_fraction() {
        // The whole point of the change: half the dial is not half the
        // amplitude, because half the amplitude is nowhere near half as loud.
        let half = amplitude_for_percent(50);
        assert!((half - 0.285_038).abs() < 1e-5, "got {half}");
        assert!(half < 0.5, "50% must be quieter than the old linear scaling");
    }

    #[test]
    fn the_curve_never_goes_backwards() {
        // A dial that dips as it is turned up would be worse than a wrong curve.
        let mut previous = -1.0f32;
        for percent in 0..=100u8 {
            let amplitude = amplitude_for_percent(percent);
            assert!(
                amplitude >= previous,
                "{percent}% is quieter than {}%",
                percent - 1
            );
            previous = amplitude;
        }
    }

    #[test]
    fn a_setting_above_the_scale_is_treated_as_full() {
        assert_eq!(amplitude_for_percent(200), 1.0);
    }

    // -- the inverse, used once to carry old settings across --

    #[test]
    fn the_inverse_returns_the_setting_it_came_from() {
        for percent in 0..=100u8 {
            let round_tripped = percent_for_amplitude(amplitude_for_percent(percent));
            assert!(
                round_tripped.abs_diff(percent) <= 1,
                "{percent}% came back as {round_tripped}%"
            );
        }
    }

    #[test]
    fn a_volume_saved_under_the_old_scaling_keeps_its_loudness() {
        // Someone on 50% before the change was hearing amplitude 0.5. To sound
        // the same on the curve they need 70%, and that is what the migration
        // has to write.
        assert_eq!(percent_for_amplitude(0.5), 70);
        let after = amplitude_for_percent(70);
        assert!((after - 0.5).abs() < 0.005, "got {after}, wanted about 0.5");
    }

    #[test]
    fn the_ends_survive_the_inverse_too() {
        assert_eq!(percent_for_amplitude(0.0), 0);
        assert_eq!(percent_for_amplitude(1.0), 100);
        // Out of range rather than panicking.
        assert_eq!(percent_for_amplitude(-1.0), 0);
        assert_eq!(percent_for_amplitude(2.0), 100);
    }

    // -- the controller --

    #[test]
    fn the_maximum_caps_the_setting() {
        let mut v = VolumeController::new(50, 100, 1.0); // large step: snaps immediately
        v.set_target(80, 70);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        // Capped to 70%, which is amplitude 0.502.
        assert_eq!(samples[0], 502);
    }

    #[test]
    fn zero_silences_and_full_passes_through() {
        let mut v = VolumeController::new(50, 100, 1.0);
        v.set_target(0, 100);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 0);

        let mut v = VolumeController::new(50, 100, 1.0);
        v.set_target(100, 100);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 1000);
    }

    #[test]
    fn a_new_controller_starts_at_its_configured_setting() {
        // The first frames play at the seed gain before any ramp: it must be
        // the configured volume, not a fixed default. (A 50%-seeded controller
        // made a volume-20 bot ~6x too loud for its first frames.)
        let mut v = VolumeController::new(20, 100, 1.0);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], (1000.0 * amplitude_for_percent(20)) as i16);
    }

    #[test]
    fn a_muted_controller_starts_silent() {
        // The sharpest case of the seed bug: a bot the operator muted played
        // ~100ms of every track's opening before the ramp reached zero.
        let mut v = VolumeController::new(0, 100, 0.03);
        let mut samples = [1000i16, -1000];
        v.apply(&mut samples);
        assert_eq!(samples, [0, 0]);
    }

    #[test]
    fn the_seed_respects_the_maximum() {
        let mut v = VolumeController::new(100, 70, 1.0);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], (1000.0 * amplitude_for_percent(70)) as i16);
    }

    #[test]
    fn a_change_is_ramped_rather_than_jumping() {
        // A step change in gain is audible as a click, which is why the ramp
        // exists at all.
        let mut v = VolumeController::new(50, 100, 0.1);
        v.set_target(100, 100); // 0.285 -> 1.0, further than one step
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 385, "one step of 0.1 above the starting 0.285");
    }

    #[test]
    fn ramping_down_moves_by_one_step_too() {
        let mut v = VolumeController::new(50, 100, 0.05);
        v.set_target(0, 100); // target 0.0 from 0.285
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 235, "0.285 - 0.05");
    }

    #[test]
    fn a_ramp_settles_exactly_on_the_target() {
        let mut v = VolumeController::new(50, 100, 0.1);
        v.set_target(100, 100);
        for _ in 0..20 {
            v.apply(&mut []);
        }
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 1000, "should have reached full, not overshot");
    }

    #[test]
    fn an_empty_frame_still_advances_the_ramp() {
        let mut v = VolumeController::new(50, 100, 0.1);
        v.set_target(100, 100);
        v.apply(&mut []);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 485, "two steps above the starting 0.285");
    }
}
