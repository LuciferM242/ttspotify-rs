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
