/// Volume controller with smooth ramping between volume levels.
pub struct VolumeController {
    target_scale: f32,
    current_scale: f32,
    ramp_step: f32,
}

impl VolumeController {
    pub fn new(ramp_step: f32) -> Self {
        Self {
            target_scale: 0.5,
            current_scale: 0.5,
            ramp_step,
        }
    }

    /// Set target volume from user percentage (0-100) capped by max_percent.
    pub fn set_target(&mut self, percent: u8, max_percent: u8) {
        let capped = percent.min(max_percent);
        self.target_scale = capped as f32 / 100.0;
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

    // -- set_target --

    #[test]
    fn set_target_caps_at_max_percent() {
        let mut v = VolumeController::new(0.5);
        v.set_target(80, 70);
        // target = 70/100 = 0.7. Force snap by using large ramp_step.
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 700);
    }

    #[test]
    fn set_target_zero_silences_audio() {
        let mut v = VolumeController::new(0.5);
        v.set_target(0, 100);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 0);
    }

    #[test]
    fn set_target_full_passes_audio_through() {
        let mut v = VolumeController::new(1.0);
        v.set_target(100, 100);
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 1000);
    }

    // -- apply: steady state --

    #[test]
    fn apply_at_steady_state_scales_by_current() {
        // ctor sets target=current=0.5. Applying immediately should scale by 0.5.
        let mut v = VolumeController::new(0.5);
        let mut samples = [1000i16, -1000];
        v.apply(&mut samples);
        assert_eq!(samples, [500, -500]);
    }

    // -- apply: ramping --

    #[test]
    fn apply_ramps_up_by_one_step_per_frame() {
        let mut v = VolumeController::new(0.1); // step
        v.set_target(100, 100); // target=1.0, current=0.5 → diff=0.5 > 0.1 → ramp up
        let mut samples = [1000i16];
        v.apply(&mut samples);
        // After one frame: current = 0.5 + 0.1 = 0.6.
        assert_eq!(samples[0], 600);
    }

    #[test]
    fn apply_ramps_down_by_one_step_per_frame() {
        let mut v = VolumeController::new(0.1);
        v.set_target(0, 100); // target=0.0, current=0.5
        let mut samples = [1000i16];
        v.apply(&mut samples);
        // After one frame: current = 0.5 - 0.1 = 0.4.
        assert_eq!(samples[0], 400);
    }

    #[test]
    fn apply_snaps_to_target_when_within_one_step() {
        // step=0.5 means any diff <= 0.5 triggers snap.
        let mut v = VolumeController::new(0.5);
        v.set_target(60, 100); // target=0.6, current=0.5 → diff=0.1, snaps to 0.6
        let mut samples = [1000i16];
        v.apply(&mut samples);
        assert_eq!(samples[0], 600);
    }

    #[test]
    fn apply_settles_at_target_after_enough_frames() {
        let mut v = VolumeController::new(0.1);
        v.set_target(100, 100); // 0.5 → 1.0 in 5 steps
        let mut samples = [1000i16];
        for _ in 0..10 {
            v.apply(&mut samples);
            samples[0] = 1000; // reset for next frame
        }
        v.apply(&mut samples);
        assert_eq!(samples[0], 1000); // fully ramped to 1.0
    }

    // -- apply: empty slice --

    #[test]
    fn apply_with_empty_slice_does_not_panic_and_advances_ramp() {
        let mut v = VolumeController::new(0.1);
        v.set_target(100, 100); // target=1.0, current=0.5
        v.apply(&mut []); // should advance current to 0.6 without panic
        let mut samples = [1000i16];
        v.apply(&mut samples);
        // Second frame: current = 0.6 + 0.1 = 0.7.
        assert_eq!(samples[0], 700);
    }
}
