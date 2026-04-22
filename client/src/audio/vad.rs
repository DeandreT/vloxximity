use anyhow::Result;
use webrtc_vad::{SampleRate, Vad, VadMode};

/// Voice activity detector
pub struct VoiceActivityDetector {
    vad: Vad,
    mode: VadMode,
    /// Number of consecutive frames that must be voice to trigger
    voice_threshold: usize,
    /// Number of consecutive frames that must be silence to stop
    silence_threshold: usize,
    /// Current voice frame count
    voice_count: usize,
    /// Current silence frame count
    silence_count: usize,
    /// Whether we're currently in voice state
    is_voice: bool,
}

impl VoiceActivityDetector {
    pub fn new() -> Result<Self> {
        let vad = Vad::new_with_rate_and_mode(SampleRate::Rate48kHz, VadMode::Quality);

        Ok(Self {
            vad,
            mode: VadMode::Quality,
            voice_threshold: 2,    // 40ms of voice to start
            silence_threshold: 15, // 300ms of silence to stop
            voice_count: 0,
            silence_count: 0,
            is_voice: false,
        })
    }

    /// Set VAD aggressiveness mode
    pub fn set_mode(&mut self, mode: VadMode) -> Result<()> {
        self.mode = match mode {
            VadMode::Quality => VadMode::Quality,
            VadMode::LowBitrate => VadMode::LowBitrate,
            VadMode::Aggressive => VadMode::Aggressive,
            VadMode::VeryAggressive => VadMode::VeryAggressive,
        };
        self.vad = Vad::new_with_rate_and_mode(SampleRate::Rate48kHz, mode);
        Ok(())
    }

    /// Set voice detection threshold (frames of voice needed to trigger)
    pub fn set_voice_threshold(&mut self, frames: usize) {
        self.voice_threshold = frames;
    }

    /// Set silence threshold (frames of silence needed to stop)
    pub fn set_silence_threshold(&mut self, frames: usize) {
        self.silence_threshold = frames;
    }

    /// Process a frame and return whether voice activity is detected
    /// Uses hysteresis to prevent rapid on/off switching
    pub fn process(&mut self, samples: &[f32]) -> bool {
        // Convert f32 to i16 for webrtc-vad
        let samples_i16: Vec<i16> = samples
            .iter()
            .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
            .collect();

        // webrtc-vad expects specific frame sizes (10, 20, or 30ms)
        // At 48kHz, 20ms = 960 samples (our FRAME_SIZE)
        let frame_is_voice = self.vad.is_voice_segment(&samples_i16).unwrap_or(false);

        if frame_is_voice {
            self.voice_count += 1;
            self.silence_count = 0;

            if !self.is_voice && self.voice_count >= self.voice_threshold {
                self.is_voice = true;
            }
        } else {
            self.silence_count += 1;
            self.voice_count = 0;

            if self.is_voice && self.silence_count >= self.silence_threshold {
                self.is_voice = false;
            }
        }

        self.is_voice
    }

    /// Check if currently detecting voice
    pub fn is_voice(&self) -> bool {
        self.is_voice
    }

    /// Reset VAD state
    pub fn reset(&mut self) {
        self.voice_count = 0;
        self.silence_count = 0;
        self.is_voice = false;
    }

    /// Get current mode
    pub fn mode(&self) -> &VadMode {
        &self.mode
    }
}

impl Default for VoiceActivityDetector {
    fn default() -> Self {
        Self::new().expect("Failed to create VAD")
    }
}

/// Simple energy-based VAD as fallback
pub struct EnergyVad {
    threshold: f32,
    smoothing: f32,
    current_energy: f32,
    is_voice: bool,
}

impl EnergyVad {
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            smoothing: 0.95,
            current_energy: 0.0,
            is_voice: false,
        }
    }

    pub fn process(&mut self, samples: &[f32]) -> bool {
        // Calculate RMS energy
        let energy: f32 = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        let rms = energy.sqrt();

        // Smooth the energy
        self.current_energy = self.current_energy * self.smoothing + rms * (1.0 - self.smoothing);

        self.is_voice = self.current_energy > self.threshold;
        self.is_voice
    }

    pub fn is_voice(&self) -> bool {
        self.is_voice
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    pub fn energy(&self) -> f32 {
        self.current_energy
    }
}
