//! Audio thread management.
//!
//! Handles audio capture and playback on a dedicated thread since cpal::Stream
//! requires staying on the thread where it was created.

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::position::{Position, Transform};
use crate::voice::mixer::AudioMixer;

use super::capture::{AudioCapture, FRAME_SIZE, SAMPLE_RATE};
use super::codec::OpusDecoder;
use super::playback::AudioPlayback;
use super::spatial::{SpatialConfig, SpatialMode, SpatialState};

/// Commands for the audio thread.
#[derive(Debug)]
pub enum AudioCommand {
    /// Start audio capture and playback
    Start,
    /// Stop audio capture and playback
    Stop,
    /// Shutdown the thread
    Shutdown,
    /// Change input device
    SetInputDevice(String),
    /// Change output device
    SetOutputDevice(String),
}

/// Incoming playback updates sent from the main thread.
#[derive(Debug, Clone)]
pub enum IncomingAudioCommand {
    ResetIncoming,
    SetListenerTransform(Transform),
    SetPlaybackSettings {
        min_distance: f32,
        max_distance: f32,
        output_volume: f32,
        is_deafened: bool,
        directional_audio_enabled: bool,
        spatial_3d_enabled: bool,
    },
    UpsertPeer {
        peer_id: String,
        player_name: String,
    },
    RemovePeer {
        peer_id: String,
    },
    SetPeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    SetPeerLocalPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    PushPeerOpus {
        peer_id: String,
        data: Vec<u8>,
    },
    PushPeerPcm {
        peer_id: String,
        data: Vec<f32>,
    },
    SetPeerMuted {
        peer_id: String,
        muted: bool,
    },
    SetPeerVolume {
        peer_id: String,
        volume: f32,
    },
}

/// Audio thread handle.
pub struct AudioThread {
    /// Command sender
    cmd_tx: Sender<AudioCommand>,
    /// Captured audio frames receiver
    capture_rx: Receiver<Vec<f32>>,
    /// Incoming playback command sender
    incoming_tx: Sender<IncomingAudioCommand>,
    /// Thread handle
    handle: Option<JoinHandle<()>>,
    /// Running flag
    running: Arc<AtomicBool>,
}

impl AudioThread {
    /// Spawn a new audio thread.
    pub fn spawn() -> Result<Self> {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(32);
        let (capture_tx, capture_rx) = bounded::<Vec<f32>>(64);
        let (incoming_tx, incoming_rx) = unbounded::<IncomingAudioCommand>();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let handle = thread::Builder::new()
            .name("vloxximity-audio".to_string())
            .spawn(move || {
                audio_thread_main(cmd_rx, incoming_rx, capture_tx, running_clone);
            })?;

        Ok(Self {
            cmd_tx,
            capture_rx,
            incoming_tx,
            handle: Some(handle),
            running,
        })
    }

    /// Send a command to the audio thread.
    pub fn send_command(&self, cmd: AudioCommand) -> Result<()> {
        self.cmd_tx.send(cmd)?;
        Ok(())
    }

    /// Send an incoming playback update to the audio thread.
    pub fn send_incoming_command(&self, cmd: IncomingAudioCommand) -> Result<()> {
        self.incoming_tx.send(cmd)?;
        Ok(())
    }

    /// Clone the incoming playback command sender.
    pub fn clone_incoming_sender(&self) -> Sender<IncomingAudioCommand> {
        self.incoming_tx.clone()
    }

    /// Start audio capture and playback.
    pub fn start(&self) -> Result<()> {
        self.send_command(AudioCommand::Start)
    }

    /// Stop audio capture and playback.
    pub fn stop(&self) -> Result<()> {
        self.send_command(AudioCommand::Stop)
    }

    /// Get receiver for captured audio frames.
    pub fn capture_receiver(&self) -> &Receiver<Vec<f32>> {
        &self.capture_rx
    }

    /// Shutdown the audio thread.
    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.cmd_tx.send(AudioCommand::Shutdown);

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Check if thread is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct JitterBuffer {
    buffer: VecDeque<Vec<f32>>,
    target_size: usize,
    max_size: usize,
}

impl JitterBuffer {
    fn new(target_size: usize, max_size: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(max_size),
            target_size,
            max_size,
        }
    }

    fn push(&mut self, frame: Vec<f32>) {
        if self.buffer.len() >= self.max_size {
            self.buffer.pop_front();
        }
        self.buffer.push_back(frame);
    }

    fn pop(&mut self) -> Option<Vec<f32>> {
        if self.buffer.len() >= self.target_size {
            self.buffer.pop_front()
        } else {
            None
        }
    }

    fn clear(&mut self) {
        self.buffer.clear();
    }
}

struct PlaybackPeer {
    player_name: String,
    position: Position,
    front: Position,
    volume: f32,
    is_muted: bool,
    decoder: OpusDecoder,
    jitter_buffer: JitterBuffer,
    last_audio_time: Instant,
    last_distance_log: Instant,
    spatial: SpatialState,
}

impl PlaybackPeer {
    fn new(_peer_id: String, player_name: String) -> Result<Self> {
        Ok(Self {
            player_name,
            position: Position::default(),
            front: Position::new(0.0, 0.0, 1.0),
            volume: 1.0,
            is_muted: false,
            decoder: OpusDecoder::new()?,
            jitter_buffer: JitterBuffer::new(3, 10),
            last_audio_time: Instant::now(),
            last_distance_log: Instant::now() - Duration::from_secs(10),
            spatial: SpatialState::new(),
        })
    }

    fn update_position(&mut self, position: Position, front: Position) {
        self.position = position;
        self.front = front;
    }

    fn push_opus(&mut self, data: &[u8]) -> Result<()> {
        let samples = self.decoder.decode(data)?;
        self.push_pcm(samples);
        Ok(())
    }

    fn push_pcm(&mut self, samples: Vec<f32>) {
        self.jitter_buffer.push(samples);
        self.last_audio_time = Instant::now();
    }

    fn clear_buffer(&mut self) {
        self.jitter_buffer.clear();
    }

    fn next_spatial_audio(
        &mut self,
        listener: &Transform,
        cfg: &SpatialConfig,
        stereo_out: &mut [f32],
    ) -> bool {
        if self.is_muted {
            let _ = self.jitter_buffer.pop();
            return false;
        }

        let Some(mono) = self.jitter_buffer.pop() else {
            return false;
        };

        if self.last_distance_log.elapsed() > Duration::from_secs(2) {
            let distance = listener.position.distance_to(&self.position);
            let (right_c, up_c, front_c) = listener.relative_position(&self.position);
            let azimuth_deg = listener.azimuth_to(&self.position).to_degrees();
            log::info!(
                "peer '{}' distance={:.1} listener=({:.1},{:.1},{:.1}) front=({:.2},{:.2},{:.2}) top=({:.2},{:.2},{:.2}) source=({:.1},{:.1},{:.1}) local=(right={:.1},up={:.1},front={:.1}) azimuth={:.0}° min={:.0} max={:.0} mode={:?}",
                self.player_name,
                distance,
                listener.position.x, listener.position.y, listener.position.z,
                listener.front.x, listener.front.y, listener.front.z,
                listener.top.x, listener.top.y, listener.top.z,
                self.position.x, self.position.y, self.position.z,
                right_c, up_c, front_c,
                azimuth_deg,
                cfg.min_distance, cfg.max_distance, cfg.mode,
            );
            self.last_distance_log = Instant::now();
        }

        self.spatial
            .process_frame(&mono, listener, &self.position, cfg, stereo_out);
        for sample in stereo_out.iter_mut() {
            *sample *= self.volume;
        }
        true
    }
}

struct IncomingPlaybackEngine {
    peers: HashMap<String, PlaybackPeer>,
    listener_transform: Transform,
    spatial_cfg: SpatialConfig,
    peer_scratch: Vec<f32>,
    mixer: AudioMixer,
    output_volume: f32,
    is_deafened: bool,
    frame_duration: Duration,
    target_lead: Duration,
    next_mix_at: Instant,
    playback_buffered_until: Instant,
}

impl IncomingPlaybackEngine {
    fn new() -> Self {
        let now = Instant::now();
        let frame_duration = Duration::from_secs_f32(FRAME_SIZE as f32 / SAMPLE_RATE as f32);

        Self {
            peers: HashMap::new(),
            listener_transform: Transform::default(),
            spatial_cfg: SpatialConfig::default(),
            peer_scratch: vec![0.0; FRAME_SIZE * 2],
            mixer: AudioMixer::new(),
            output_volume: 1.0,
            is_deafened: false,
            frame_duration,
            target_lead: frame_duration * 3,
            next_mix_at: now,
            playback_buffered_until: now,
        }
    }

    fn reset_timing(&mut self) {
        let now = Instant::now();
        self.next_mix_at = now;
        self.playback_buffered_until = now;
    }

    fn reset(&mut self, playback: Option<&AudioPlayback>) {
        self.peers.clear();
        self.reset_timing();
        if let Some(playback) = playback {
            playback.clear_buffer();
        }
    }

    fn clear_peer_buffers(&mut self) {
        for peer in self.peers.values_mut() {
            peer.clear_buffer();
        }
    }

    fn set_deafened(&mut self, playback: Option<&AudioPlayback>, is_deafened: bool) {
        self.is_deafened = is_deafened;
        if is_deafened {
            self.clear_peer_buffers();
            self.reset_timing();
            if let Some(playback) = playback {
                playback.clear_buffer();
            }
        }
    }

    fn handle_command(&mut self, cmd: IncomingAudioCommand, playback: Option<&AudioPlayback>) {
        match cmd {
            IncomingAudioCommand::ResetIncoming => self.reset(playback),
            IncomingAudioCommand::SetListenerTransform(transform) => {
                self.listener_transform = transform;
            }
            IncomingAudioCommand::SetPlaybackSettings {
                min_distance,
                max_distance,
                output_volume,
                is_deafened,
                directional_audio_enabled,
                spatial_3d_enabled,
            } => {
                self.spatial_cfg.min_distance = min_distance;
                self.spatial_cfg.max_distance = max_distance;
                self.spatial_cfg.mode = match (directional_audio_enabled, spatial_3d_enabled) {
                    (false, _) => SpatialMode::Off,
                    (true, false) => SpatialMode::Pan2D,
                    (true, true) => SpatialMode::Full3D,
                };
                self.output_volume = output_volume;
                self.mixer.set_master_volume(output_volume);
                self.set_deafened(playback, is_deafened);
            }
            IncomingAudioCommand::UpsertPeer {
                peer_id,
                player_name,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.player_name = player_name;
                } else if let Ok(peer) = PlaybackPeer::new(peer_id.clone(), player_name) {
                    self.peers.insert(peer_id, peer);
                }
            }
            IncomingAudioCommand::RemovePeer { peer_id } => {
                self.peers.remove(&peer_id);
            }
            IncomingAudioCommand::SetPeerPosition {
                peer_id,
                position,
                front,
            } => {
                if let Some(peer) = self.ensure_peer(&peer_id) {
                    peer.update_position(position, front);
                }
            }
            IncomingAudioCommand::SetPeerLocalPosition {
                peer_id,
                position,
                front,
            } => {
                let world_position = self
                    .listener_transform
                    .local_offset_to_world(position.x, position.y, position.z);
                if let Some(peer) = self.ensure_peer(&peer_id) {
                    peer.update_position(world_position, front);
                }
            }
            IncomingAudioCommand::PushPeerOpus { peer_id, data } => {
                if let Some(peer) = self.ensure_peer(&peer_id) {
                    if let Err(e) = peer.push_opus(&data) {
                        log::warn!("Failed to decode audio for {}: {}", peer_id, e);
                    }
                }
            }
            IncomingAudioCommand::PushPeerPcm { peer_id, data } => {
                if let Some(peer) = self.ensure_peer(&peer_id) {
                    peer.push_pcm(data);
                }
            }
            IncomingAudioCommand::SetPeerMuted { peer_id, muted } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.is_muted = muted;
                }
            }
            IncomingAudioCommand::SetPeerVolume { peer_id, volume } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.volume = volume.clamp(0.0, 2.0);
                }
            }
        }
    }

    fn ensure_peer(&mut self, peer_id: &str) -> Option<&mut PlaybackPeer> {
        if !self.peers.contains_key(peer_id) {
            match PlaybackPeer::new(peer_id.to_string(), peer_id.to_string()) {
                Ok(peer) => {
                    self.peers.insert(peer_id.to_string(), peer);
                }
                Err(e) => {
                    log::warn!("Failed to create playback peer {}: {}", peer_id, e);
                    return None;
                }
            }
        }
        self.peers.get_mut(peer_id)
    }

    fn process(&mut self, playback: &AudioPlayback) {
        let now = Instant::now();
        let max_lag = self.frame_duration * 8;
        let oldest_mix_time = now.checked_sub(max_lag).unwrap_or(now);
        if self.next_mix_at < oldest_mix_time {
            self.next_mix_at = oldest_mix_time;
        }
        if self.playback_buffered_until < now {
            self.playback_buffered_until = now;
        }

        let mut frames_to_process = 0usize;
        while self.next_mix_at <= now || self.playback_buffered_until < now + self.target_lead {
            frames_to_process += 1;
            self.next_mix_at += self.frame_duration;
            if frames_to_process >= 8 {
                break;
            }
        }

        if frames_to_process == 0 {
            return;
        }

        if self.is_deafened {
            self.clear_peer_buffers();
            self.reset_timing();
            playback.clear_buffer();
            return;
        }

        self.mixer.set_master_volume(self.output_volume);

        for _ in 0..frames_to_process {
            let mut peer_audio = Vec::new();

            for peer in self.peers.values_mut() {
                if peer.next_spatial_audio(
                    &self.listener_transform,
                    &self.spatial_cfg,
                    &mut self.peer_scratch,
                ) {
                    peer_audio.push(self.peer_scratch.clone());
                }
            }

            let mixed = self.mixer.mix(peer_audio);
            if playback.queue_samples(mixed) {
                self.playback_buffered_until += self.frame_duration;
            } else {
                self.playback_buffered_until = now;
                break;
            }
        }
    }
}

/// Main function for the audio thread.
fn audio_thread_main(
    cmd_rx: Receiver<AudioCommand>,
    incoming_rx: Receiver<IncomingAudioCommand>,
    capture_tx: Sender<Vec<f32>>,
    running: Arc<AtomicBool>,
) {
    log::info!("Audio thread started");

    let mut capture: Option<AudioCapture> = None;
    let mut playback: Option<AudioPlayback> = None;
    let mut incoming = IncomingPlaybackEngine::new();
    let mut is_active = false;

    let mut cap = AudioCapture::new();
    if let Err(e) = cap.use_default_device() {
        log::error!("Failed to initialize capture device: {}", e);
    } else {
        capture = Some(cap);
    }

    let mut play = AudioPlayback::new();
    if let Err(e) = play.use_default_device() {
        log::error!("Failed to initialize playback device: {}", e);
    } else {
        playback = Some(play);
    }

    while running.load(Ordering::SeqCst) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::Start => {
                    if !is_active {
                        log::info!("Starting audio");

                        if let Some(ref mut cap) = capture {
                            if let Err(e) = cap.start() {
                                log::error!("Failed to start capture: {}", e);
                            }
                        }

                        if let Some(ref mut play) = playback {
                            if let Err(e) = play.start() {
                                log::error!("Failed to start playback: {}", e);
                            }
                        }

                        incoming.reset_timing();
                        is_active = true;
                    }
                }
                AudioCommand::Stop => {
                    if is_active {
                        log::info!("Stopping audio");

                        if let Some(ref mut cap) = capture {
                            cap.stop();
                        }

                        if let Some(ref mut play) = playback {
                            play.clear_buffer();
                            play.stop();
                        }

                        incoming.reset_timing();
                        is_active = false;
                    }
                }
                AudioCommand::Shutdown => {
                    log::info!("Audio thread shutting down");
                    running.store(false, Ordering::SeqCst);
                }
                AudioCommand::SetInputDevice(name) => {
                    if let Some(ref mut cap) = capture {
                        if is_active {
                            cap.stop();
                        }
                        if let Err(e) = cap.select_device(&name) {
                            log::error!("Failed to select input device: {}", e);
                        }
                        if is_active {
                            let _ = cap.start();
                        }
                    }
                }
                AudioCommand::SetOutputDevice(name) => {
                    if let Some(ref mut play) = playback {
                        if is_active {
                            play.clear_buffer();
                            play.stop();
                        }
                        if let Err(e) = play.select_device(&name) {
                            log::error!("Failed to select output device: {}", e);
                        }
                        if is_active {
                            let _ = play.start();
                            incoming.reset_timing();
                        }
                    }
                }
            }
        }

        while let Ok(cmd) = incoming_rx.try_recv() {
            incoming.handle_command(cmd, playback.as_ref());
        }

        if is_active {
            if let Some(ref cap) = capture {
                if let Some(rx) = cap.get_receiver() {
                    while let Ok(frame) = rx.try_recv() {
                        let _ = capture_tx.try_send(frame);
                    }
                }
            }
        }

        if is_active {
            if let Some(ref play) = playback {
                incoming.process(play);
            }
        }

        thread::sleep(Duration::from_millis(5));
    }

    if let Some(mut cap) = capture {
        cap.stop();
    }
    if let Some(mut play) = playback {
        play.stop();
    }

    log::info!("Audio thread stopped");
}
