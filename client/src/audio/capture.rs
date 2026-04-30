//! Microphone audio capture using cpal.

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Host, SampleFormat, Stream, StreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use std::sync::Arc;

/// Audio sample rate for voice (Opus standard)
pub const SAMPLE_RATE: u32 = 48000;

/// Frame size in samples (20ms at 48kHz)
pub const FRAME_SIZE: usize = 960;

/// Audio capture from microphone
pub struct AudioCapture {
    host: Host,
    device: Option<Device>,
    stream: Option<Stream>,
    sample_sender: Option<Sender<Vec<f32>>>,
    sample_receiver: Option<Receiver<Vec<f32>>>,
    buffer: Arc<Mutex<Vec<f32>>>,
    is_capturing: bool,
}

impl AudioCapture {
    pub fn new() -> Self {
        let host = cpal::default_host();
        Self {
            host,
            device: None,
            stream: None,
            sample_sender: None,
            sample_receiver: None,
            buffer: Arc::new(Mutex::new(Vec::with_capacity(FRAME_SIZE * 4))),
            is_capturing: false,
        }
    }

    /// Get list of available input devices
    pub fn list_devices(&self) -> Vec<String> {
        self.host
            .input_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Select input device by name
    pub fn select_device(&mut self, name: &str) -> Result<()> {
        let device = self
            .host
            .input_devices()?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| anyhow::anyhow!("Device not found: {}", name))?;

        self.device = Some(device);
        log::info!("Selected input device: {}", name);
        Ok(())
    }

    /// Use the default input device
    pub fn use_default_device(&mut self) -> Result<()> {
        let device = self
            .host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No default input device available"))?;

        let name = device.name().unwrap_or_else(|_| "Unknown".to_string());
        log::info!("Using default input device: {}", name);
        self.device = Some(device);
        Ok(())
    }

    /// Start audio capture
    pub fn start(&mut self) -> Result<()> {
        if self.is_capturing {
            return Ok(());
        }

        let device = self
            .device
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No input device selected"))?;

        // Get supported config
        let supported_config = device.default_input_config()?;
        log::info!("Input config: {:?}", supported_config);

        // Create channel for samples
        let (sender, receiver) = bounded::<Vec<f32>>(32);
        self.sample_sender = Some(sender.clone());
        self.sample_receiver = Some(receiver);

        let buffer = self.buffer.clone();
        let frame_sender = sender;

        // Build config for mono 48kHz
        let config = StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        // Build the input stream based on sample format
        let stream = match supported_config.sample_format() {
            SampleFormat::F32 => self.build_stream::<f32>(device, &config, buffer, frame_sender)?,
            SampleFormat::I16 => self.build_stream::<i16>(device, &config, buffer, frame_sender)?,
            SampleFormat::U16 => self.build_stream::<u16>(device, &config, buffer, frame_sender)?,
            format => return Err(anyhow::anyhow!("Unsupported sample format: {:?}", format)),
        };

        stream.play()?;
        self.stream = Some(stream);
        self.is_capturing = true;

        log::info!("Audio capture started");
        Ok(())
    }

    fn build_stream<T>(
        &self,
        device: &Device,
        config: &StreamConfig,
        buffer: Arc<Mutex<Vec<f32>>>,
        sender: Sender<Vec<f32>>,
    ) -> Result<Stream>
    where
        T: cpal::Sample + cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        let err_fn = |err| log::error!("Audio capture error: {}", err);

        let stream = device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut buf = buffer.lock();

                // Convert samples to f32 and add to buffer
                for sample in data {
                    buf.push(<f32 as FromSample<T>>::from_sample_(*sample));
                }

                // Send complete frames
                while buf.len() >= FRAME_SIZE {
                    let frame: Vec<f32> = buf.drain(..FRAME_SIZE).collect();
                    let _ = sender.try_send(frame);
                }
            },
            err_fn,
            None,
        )?;

        Ok(stream)
    }

    /// Stop audio capture
    pub fn stop(&mut self) {
        self.stream = None;
        self.is_capturing = false;
        self.sample_sender = None;
        log::info!("Audio capture stopped");
    }

    /// Get receiver for audio frames
    pub fn get_receiver(&self) -> Option<Receiver<Vec<f32>>> {
        self.sample_receiver.clone()
    }

    /// Check if currently capturing
    pub fn is_capturing(&self) -> bool {
        self.is_capturing
    }

    /// Get the current device name
    pub fn device_name(&self) -> Option<String> {
        self.device.as_ref().and_then(|d| d.name().ok())
    }
}

impl Default for AudioCapture {
    fn default() -> Self {
        Self::new()
    }
}
