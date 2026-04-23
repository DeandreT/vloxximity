pub mod capture;
pub mod codec;
pub mod playback;
pub mod spatial;
pub mod thread;
pub mod vad;

pub use capture::{AudioCapture, FRAME_SIZE, SAMPLE_RATE};
pub use codec::{OpusDecoder, OpusEncoder};
pub use playback::AudioPlayback;
pub use spatial::{SpatialConfig, SpatialMode, SpatialState};
pub use thread::{AudioThread, IncomingAudioCommand};
pub use vad::VoiceActivityDetector;

use cpal::traits::{DeviceTrait, HostTrait};

/// Enumerate available input device names via the default cpal host.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Enumerate available output device names via the default cpal host.
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Name of the default input device, if one exists.
pub fn default_input_device_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()
        .and_then(|d| d.name().ok())
}

/// Name of the default output device, if one exists.
pub fn default_output_device_name() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}
