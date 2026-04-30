//! Audio playback to speakers using cpal.

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat, Stream, StreamConfig};
use crossbeam_channel::{bounded, Sender};
use parking_lot::Mutex;
use std::sync::Arc;

use super::capture::SAMPLE_RATE;

/// Audio playback to speakers
pub struct AudioPlayback {
    host: Host,
    device: Option<Device>,
    stream: Option<Stream>,
    sample_sender: Option<Sender<Vec<f32>>>,
    buffer: Arc<Mutex<Vec<f32>>>,
    is_playing: bool,
    volume: Arc<Mutex<f32>>,
}

impl AudioPlayback {
    pub fn new() -> Self {
        let host = cpal::default_host();
        Self {
            host,
            device: None,
            stream: None,
            sample_sender: None,
            buffer: Arc::new(Mutex::new(Vec::with_capacity(4096))),
            is_playing: false,
            volume: Arc::new(Mutex::new(1.0)),
        }
    }

    /// Get list of available output devices
    pub fn list_devices(&self) -> Vec<String> {
        self.host
            .output_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Select output device by name
    pub fn select_device(&mut self, name: &str) -> Result<()> {
        let device = self
            .host
            .output_devices()?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| anyhow::anyhow!("Device not found: {}", name))?;

        self.device = Some(device);
        log::info!("Selected output device: {}", name);
        Ok(())
    }

    /// Use the default output device
    pub fn use_default_device(&mut self) -> Result<()> {
        let device = self
            .host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No default output device available"))?;

        let name = device.name().unwrap_or_else(|_| "Unknown".to_string());
        log::info!("Using default output device: {}", name);
        self.device = Some(device);
        Ok(())
    }

    /// Start audio playback
    pub fn start(&mut self) -> Result<()> {
        if self.is_playing {
            return Ok(());
        }

        let device = self
            .device
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No output device selected"))?;

        // Get supported config
        let supported_config = device.default_output_config()?;
        log::info!("Output config: {:?}", supported_config);

        // Create channel for samples
        let (sender, receiver) = bounded::<Vec<f32>>(32);
        self.sample_sender = Some(sender);

        let buffer = self.buffer.clone();
        let volume = self.volume.clone();

        // Spawn a thread to receive samples and add to buffer
        let buffer_clone = buffer.clone();
        std::thread::spawn(move || {
            while let Ok(samples) = receiver.recv() {
                let mut buf = buffer_clone.lock();
                buf.extend(samples);
                // Limit buffer size to prevent unbounded growth
                let len = buf.len();
                if len > 48000 {
                    buf.drain(..len - 48000);
                }
            }
        });

        // Build config for stereo 48kHz (we'll output spatial audio)
        let config = StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        // Build the output stream
        let stream = match supported_config.sample_format() {
            SampleFormat::F32 => self.build_stream::<f32>(device, &config, buffer, volume)?,
            SampleFormat::I16 => self.build_stream::<i16>(device, &config, buffer, volume)?,
            SampleFormat::U16 => self.build_stream::<u16>(device, &config, buffer, volume)?,
            format => return Err(anyhow::anyhow!("Unsupported sample format: {:?}", format)),
        };

        stream.play()?;
        self.stream = Some(stream);
        self.is_playing = true;

        log::info!("Audio playback started");
        Ok(())
    }

    fn build_stream<T>(
        &self,
        device: &Device,
        config: &StreamConfig,
        buffer: Arc<Mutex<Vec<f32>>>,
        volume: Arc<Mutex<f32>>,
    ) -> Result<Stream>
    where
        T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
    {
        let err_fn = |err| log::error!("Audio playback error: {}", err);

        let stream = device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut buf = buffer.lock();
                let vol = *volume.lock();

                // The buffer contains interleaved stereo samples
                let samples_needed = data.len();

                for (i, sample) in data.iter_mut().enumerate() {
                    if i < buf.len() {
                        *sample = T::from_sample(buf[i] * vol);
                    } else {
                        *sample = T::from_sample(0.0);
                    }
                }

                // Remove consumed samples
                if buf.len() >= samples_needed {
                    buf.drain(..samples_needed);
                } else {
                    buf.clear();
                }
            },
            err_fn,
            None,
        )?;

        Ok(stream)
    }

    /// Stop audio playback
    pub fn stop(&mut self) {
        self.stream = None;
        self.is_playing = false;
        self.sample_sender = None;
        log::info!("Audio playback stopped");
    }

    /// Get sender for audio samples
    pub fn get_sender(&self) -> Option<Sender<Vec<f32>>> {
        self.sample_sender.clone()
    }

    /// Set playback volume (0.0 to 1.0)
    pub fn set_volume(&self, vol: f32) {
        *self.volume.lock() = vol.clamp(0.0, 1.0);
    }

    /// Get current volume
    pub fn get_volume(&self) -> f32 {
        *self.volume.lock()
    }

    /// Clear any queued playback samples immediately.
    pub fn clear_buffer(&self) {
        self.buffer.lock().clear();
    }

    /// Check if currently playing
    pub fn is_playing(&self) -> bool {
        self.is_playing
    }

    /// Get the current device name
    pub fn device_name(&self) -> Option<String> {
        self.device.as_ref().and_then(|d| d.name().ok())
    }

    /// Queue audio samples for playback
    pub fn queue_samples(&self, samples: Vec<f32>) -> bool {
        if let Some(sender) = &self.sample_sender {
            sender.try_send(samples).is_ok()
        } else {
            false
        }
    }
}

impl Default for AudioPlayback {
    fn default() -> Self {
        Self::new()
    }
}
