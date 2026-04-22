//! Multi-peer audio mixing.

use crate::audio::capture::FRAME_SIZE;

/// Audio mixer for combining multiple peer audio streams
pub struct AudioMixer {
    /// Master output volume
    pub master_volume: f32,
    /// Maximum number of simultaneous speakers to mix
    pub max_voices: usize,
    /// Mixing buffer
    mix_buffer: Vec<f32>,
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            master_volume: 1.0,
            max_voices: 16,
            // Stereo buffer
            mix_buffer: vec![0.0; FRAME_SIZE * 2],
        }
    }

    /// Mix multiple stereo audio streams into one
    /// Each input should be interleaved stereo [L, R, L, R, ...]
    pub fn mix(&mut self, inputs: Vec<Vec<f32>>) -> Vec<f32> {
        // Clear mix buffer
        self.mix_buffer.fill(0.0);

        // Limit number of voices
        let inputs: Vec<_> = inputs.into_iter().take(self.max_voices).collect();
        let voice_count = inputs.len();

        if voice_count == 0 {
            return self.mix_buffer.clone();
        }

        // Sum all inputs. Don't pre-divide by sqrt(voice_count) — that makes
        // single-peer playback unnecessarily quiet because the sqrt(2) case is
        // common (e.g. fake-player + one friend). The soft clipper below
        // absorbs occasional overshoot when multiple peers speak at once.
        let _ = voice_count;
        for input in &inputs {
            for (i, sample) in input.iter().enumerate() {
                if i < self.mix_buffer.len() {
                    self.mix_buffer[i] += sample;
                }
            }
        }

        for sample in &mut self.mix_buffer {
            *sample *= self.master_volume;
            *sample = soft_clip(*sample);
        }

        self.mix_buffer.clone()
    }

    /// Set master volume
    pub fn set_master_volume(&mut self, volume: f32) {
        self.master_volume = volume.clamp(0.0, 1.0);
    }

    /// Get master volume
    pub fn master_volume(&self) -> f32 {
        self.master_volume
    }

    /// Set max simultaneous voices
    pub fn set_max_voices(&mut self, max: usize) {
        self.max_voices = max.max(1);
    }
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new()
    }
}

/// Soft clipping function to prevent harsh digital distortion
fn soft_clip(x: f32) -> f32 {
    if x.abs() < 0.5 {
        x
    } else if x > 0.0 {
        0.5 + (1.0 - (-2.0 * (x - 0.5)).exp()) * 0.5
    } else {
        -0.5 - (1.0 - (-2.0 * (-x - 0.5)).exp()) * 0.5
    }
}

/// Hard clipping (fallback)
#[allow(dead_code)]
fn hard_clip(x: f32) -> f32 {
    x.clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mix_single() {
        let mut mixer = AudioMixer::new();
        let input = vec![0.5f32; FRAME_SIZE * 2];
        let output = mixer.mix(vec![input]);

        // Single voice, no normalization needed
        assert!((output[0] - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_mix_multiple() {
        let mut mixer = AudioMixer::new();
        let input1 = vec![0.5f32; FRAME_SIZE * 2];
        let input2 = vec![0.5f32; FRAME_SIZE * 2];
        let output = mixer.mix(vec![input1, input2]);

        // Two voices: 0.5 + 0.5 = 1.0, normalized by 1/sqrt(2) ≈ 0.707
        assert!(output[0] < 1.0);
        assert!(output[0] > 0.5);
    }

    #[test]
    fn test_soft_clip() {
        assert!((soft_clip(0.3) - 0.3).abs() < 0.001);
        assert!(soft_clip(2.0) < 1.0);
        assert!(soft_clip(-2.0) > -1.0);
    }
}
