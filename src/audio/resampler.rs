use rubato::{SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction, Resampler as _};

const CHUNK_SIZE: usize = 480;

/// Resamples audio from one sample rate to another (e.g. 44100 → 48000 Hz).
/// Internally buffers input to feed the resampler in fixed-size chunks.
pub struct Resampler {
    inner: SincFixedIn<f64>,
    channels: usize,
    input_buffer: Vec<Vec<f64>>,  // per-channel accumulator
    output_buffer: Vec<i16>,      // resampled output waiting to be consumed
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32, channels: usize) -> Self {
        let ratio = output_rate as f64 / input_rate as f64;
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.925,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let inner = SincFixedIn::new(
            ratio,
            2.0,
            params,
            CHUNK_SIZE,
            channels,
        ).expect("Failed to create resampler");

        let input_buffer = (0..channels).map(|_| Vec::with_capacity(CHUNK_SIZE * 2)).collect();

        Self {
            inner,
            channels,
            input_buffer,
            output_buffer: Vec::with_capacity(CHUNK_SIZE * 4),
        }
    }

    /// Feed interleaved i16 samples into the resampler.
    /// Call `drain_output` to get resampled samples.
    pub fn push(&mut self, samples: &[i16]) {
        let frames = samples.len() / self.channels;

        // De-interleave into per-channel buffers
        for frame in 0..frames {
            for ch in 0..self.channels {
                let sample = samples[frame * self.channels + ch] as f64 / 32768.0;
                self.input_buffer[ch].push(sample);
            }
        }

        // Process full chunks
        while self.input_buffer[0].len() >= CHUNK_SIZE {
            // Extract exactly CHUNK_SIZE frames per channel
            let chunk: Vec<Vec<f64>> = self.input_buffer.iter_mut()
                .map(|buf| buf.drain(..CHUNK_SIZE).collect())
                .collect();

            match self.inner.process(&chunk, None) {
                Ok(output) => {
                    // Re-interleave to output buffer
                    let out_frames = output[0].len();
                    for frame in 0..out_frames {
                        for ch in 0..self.channels {
                            let sample = (output[ch][frame] * 32768.0).clamp(-32768.0, 32767.0) as i16;
                            self.output_buffer.push(sample);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Resampler error: {e}");
                }
            }
        }
    }

    /// Drain up to `max_samples` resampled interleaved i16 samples.
    pub fn drain(&mut self, max_samples: usize) -> Vec<i16> {
        let take = max_samples.min(self.output_buffer.len());
        self.output_buffer.drain(..take).collect()
    }

    /// How many resampled samples are available.
    pub fn available(&self) -> usize {
        self.output_buffer.len()
    }

    pub fn clear(&mut self) {
        for buf in &mut self.input_buffer {
            buf.clear();
        }
        self.output_buffer.clear();
    }
}
