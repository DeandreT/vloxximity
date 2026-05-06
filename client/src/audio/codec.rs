//! Opus codec for voice encoding/decoding.
//! Uses mousiki - a pure Rust Opus implementation.

use anyhow::{anyhow, Result};
use mousiki::{Application, Bitrate, Channels, Decoder, Encoder};

use super::capture::FRAME_SIZE;

/// Sample rate for Opus (48kHz)
const SAMPLE_RATE: u32 = 48000;

/// Opus encoder for voice
pub struct OpusEncoder {
    encoder: Encoder,
    encode_buffer: Vec<u8>,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
            .map_err(|e| anyhow!("Failed to create encoder: {:?}", e))?;

        Ok(Self {
            encoder,
            encode_buffer: vec![0u8; 4000], // Max Opus frame size
        })
    }

    /// Encode a frame of audio samples
    /// Input: FRAME_SIZE f32 samples
    /// Output: Opus-encoded bytes
    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        if samples.len() != FRAME_SIZE {
            return Err(anyhow!(
                "Invalid frame size: {} (expected {})",
                samples.len(),
                FRAME_SIZE
            ));
        }

        let len = self
            .encoder
            .encode_float(samples, &mut self.encode_buffer)
            .map_err(|e| anyhow!("Encode error: {:?}", e))?;
        Ok(self.encode_buffer[..len].to_vec())
    }

    /// Set encoder bitrate
    pub fn set_bitrate(&mut self, bitrate: i32) -> Result<()> {
        self.encoder
            .set_bitrate(Bitrate::Bits(bitrate))
            .map_err(|e| anyhow!("Failed to set bitrate: {:?}", e))?;
        Ok(())
    }

    /// Enable/disable variable bitrate
    pub fn set_vbr(&mut self, enabled: bool) -> Result<()> {
        self.encoder
            .set_vbr(enabled)
            .map_err(|e| anyhow!("Failed to set VBR: {:?}", e))?;
        Ok(())
    }
}

/// Opus decoder for voice
pub struct OpusDecoder {
    decoder: Decoder,
    decode_buffer: Vec<f32>,
    last_good_frame: Vec<f32>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new(SAMPLE_RATE, Channels::Mono)
            .map_err(|e| anyhow!("Failed to create decoder: {:?}", e))?;

        Ok(Self {
            decoder,
            decode_buffer: vec![0.0f32; FRAME_SIZE * 2], // Extra space for safety
            last_good_frame: vec![0.0f32; FRAME_SIZE],
        })
    }

    /// Decode Opus-encoded bytes to audio samples
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<f32>> {
        let len = self
            .decoder
            .decode_float(data, &mut self.decode_buffer, false)
            .map_err(|e| anyhow!("Decode error: {:?}", e))?;

        // Store for PLC
        let result = self.decode_buffer[..len].to_vec();
        if len == FRAME_SIZE {
            self.last_good_frame.copy_from_slice(&result);
        }
        Ok(result)
    }

    /// Packet loss concealment - return faded last good frame
    pub fn decode_plc(&mut self) -> Result<Vec<f32>> {
        // Simple PLC: fade out last good frame
        let mut output = self.last_good_frame.clone();
        for sample in &mut output {
            *sample *= 0.8; // Fade factor
        }
        // Update last_good_frame with faded version for next PLC call
        self.last_good_frame.copy_from_slice(&output);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codec_roundtrip() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut decoder = OpusDecoder::new().unwrap();

        // Create a test signal (sine wave at ~765 Hz, voice band).
        let samples: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| (i as f32 * 0.1).sin() * 0.5)
            .collect();
        let input_rms: f32 =
            (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt();

        for _ in 0..3 {
            let frame = encoder.encode(&samples).unwrap();
            let _ = decoder.decode(&frame).unwrap();
        }

        let encoded = encoder.encode(&samples).unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() < samples.len() * 4); // Should be compressed

        let decoded = decoder.decode(&encoded).unwrap();
        assert_eq!(decoded.len(), FRAME_SIZE);

        let output_rms: f32 =
            (decoded.iter().map(|x| x * x).sum::<f32>() / decoded.len() as f32).sqrt();
        assert!(
            output_rms > 0.5 * input_rms,
            "Output RMS too low: in={input_rms:.3}, out={output_rms:.3}"
        );
        assert!(
            output_rms < 2.0 * input_rms,
            "Output RMS too high: in={input_rms:.3}, out={output_rms:.3}"
        );
        assert!(decoded.iter().all(|s| s.is_finite()));
    }
}
