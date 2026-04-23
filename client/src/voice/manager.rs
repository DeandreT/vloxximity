//! Central voice coordination and peer lifecycle management.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::audio::thread::AudioCommand;
use crate::audio::{AudioThread, IncomingAudioCommand, OpusEncoder, VoiceActivityDetector};
use crate::network::signaling::{ConnectionState, PeerInfo, ServerMessage, SignalingClient};
use crate::position::MumbleLink;

use super::peer::VoicePeer;

/// Voice activation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMode {
    /// Push-to-talk
    PushToTalk,
    /// Voice activity detection
    VoiceActivity,
    /// Always transmit
    AlwaysOn,
}

/// Default signaling server URL
pub const DEFAULT_SERVER_URL: &str = "ws://localhost:8080/ws";

/// Voice manager settings
#[derive(Debug, Clone)]
pub struct VoiceSettings {
    pub mode: VoiceMode,
    pub ptt_key: u32,
    pub min_distance: f32,
    pub max_distance: f32,
    pub input_volume: f32,
    pub output_volume: f32,
    pub is_muted: bool,
    pub is_deafened: bool,
    /// Master switch for directional cues. When false, all peers play centered
    /// (mono → both ears) with distance attenuation only.
    pub directional_audio_enabled: bool,
    /// When directional audio is on, selects 3D filter model vs legacy 2D pan.
    pub spatial_3d_enabled: bool,
    pub server_url: String,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            mode: VoiceMode::PushToTalk,
            ptt_key: 0,
            min_distance: 100.0,
            max_distance: 5000.0,
            input_volume: 1.0,
            output_volume: 1.0,
            is_muted: false,
            is_deafened: false,
            directional_audio_enabled: true,
            spatial_3d_enabled: true,
            server_url: DEFAULT_SERVER_URL.to_string(),
        }
    }
}

/// Voice manager state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Disconnected,
    Connecting,
    Connected,
    InRoom,
}

/// Snapshot of a peer for UI display.
#[derive(Debug, Clone)]
pub struct NearbyPeer {
    pub peer_id: String,
    pub player_name: String,
    pub is_speaking: bool,
    pub is_muted: bool,
    pub position: crate::position::Position,
    /// Distance from the local listener. `None` if no listener position is known yet.
    pub distance: Option<f32>,
}

/// Commands to send to the network task
#[derive(Debug)]
pub enum NetworkCommand {
    Connect,
    Disconnect,
    JoinRoom { room_id: String, player_name: String },
    LeaveRoom,
    UpdatePosition { position: crate::position::Position, front: crate::position::Position },
    SendAudio { data: Vec<u8> },
}

/// Events received from the network task
#[derive(Debug)]
pub enum NetworkEvent {
    Connected { peer_id: String },
    Disconnected,
    RoomJoined { room_id: String, peers: Vec<PeerInfo> },
    PeerJoined { peer_id: String, player_name: String },
    PeerLeft { peer_id: String },
    PeerPosition { peer_id: String, position: crate::position::Position, front: crate::position::Position },
    AudioReceived { peer_id: String, data: Vec<u8> },
    Error { message: String },
}

/// Central voice manager
pub struct VoiceManager {
    // State
    state: VoiceState,
    settings: VoiceSettings,
    server_url: String,
    our_peer_id: Option<String>,

    // Position tracking
    mumble_link: MumbleLink,
    current_room: Option<String>,

    // Audio thread (handles cpal on separate thread)
    audio_thread: Option<AudioThread>,

    // Audio processing components
    encoder: Option<OpusEncoder>,
    vad: Option<VoiceActivityDetector>,
    // Peers
    peers: HashMap<String, VoicePeer>,

    // PTT state (atomic for thread-safe access)
    ptt_active: Arc<AtomicBool>,

    // Network communication channels
    network_cmd_tx: Option<mpsc::UnboundedSender<NetworkCommand>>,
    network_event_rx: Option<std::sync::mpsc::Receiver<NetworkEvent>>,

    // Tokio runtime handle
    runtime: Option<tokio::runtime::Runtime>,

    // Position update throttle
    last_position_update: Instant,

    // Last known listener (avatar) position, cached from MumbleLink for UI display.
    last_listener_position: Option<crate::position::Position>,

    // Shutdown flag
    shutdown: bool,
}

// Safety: VoiceManager is Sync because:
// - Audio streams are handled on a separate thread via AudioThread
// - Network operations are on a separate tokio runtime
// - All other mutable state is protected by the outer RwLock
// - AtomicBool and channels are Send+Sync
unsafe impl Sync for VoiceManager {}
unsafe impl Send for VoiceManager {}

impl VoiceManager {
    pub fn new(server_url: &str) -> Self {
        Self {
            state: VoiceState::Disconnected,
            settings: VoiceSettings::default(),
            server_url: server_url.to_string(),
            our_peer_id: None,
            mumble_link: MumbleLink::new(),
            current_room: None,
            audio_thread: None,
            encoder: None,
            vad: None,
            peers: HashMap::new(),
            ptt_active: Arc::new(AtomicBool::new(false)),
            network_cmd_tx: None,
            network_event_rx: None,
            runtime: None,
            last_position_update: Instant::now(),
            last_listener_position: None,
            shutdown: false,
        }
    }

    /// Initialize all components
    pub fn init(&mut self) -> Result<()> {
        // Clear any previous shutdown flag so we can restart after a shutdown.
        self.shutdown = false;
        // Initialize MumbleLink
        self.mumble_link.init()?;

        // Spawn audio thread
        let audio_thread = AudioThread::spawn()?;
        let incoming_audio_tx = audio_thread.clone_incoming_sender();
        self.audio_thread = Some(audio_thread);

        // Initialize Opus encoder
        let encoder = OpusEncoder::new()?;
        self.encoder = Some(encoder);

        // Initialize VAD
        let vad = VoiceActivityDetector::new()?;
        self.vad = Some(vad);

        // Create tokio runtime for async networking
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("vloxximity-network")
            .build()?;

        // Create channels for network communication
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<NetworkCommand>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<NetworkEvent>();

        self.network_cmd_tx = Some(cmd_tx);
        self.network_event_rx = Some(event_rx);

        // Spawn network task
        let server_url = self.server_url.clone();
        runtime.spawn(async move {
            network_task(server_url, cmd_rx, event_tx, incoming_audio_tx).await;
        });

        self.sync_playback_settings();

        self.runtime = Some(runtime);

        // Connect to signaling server
        if let Some(tx) = &self.network_cmd_tx {
            let _ = tx.send(NetworkCommand::Connect);
        }
        self.state = VoiceState::Connecting;

        log::info!("VoiceManager initialized");
        Ok(())
    }

    /// Start voice processing
    pub fn start(&mut self) -> Result<()> {
        if let Some(ref audio_thread) = self.audio_thread {
            audio_thread.start()?;
        }
        Ok(())
    }

    /// Stop voice processing
    pub fn stop(&mut self) {
        if let Some(ref audio_thread) = self.audio_thread {
            let _ = audio_thread.stop();
        }
    }

    /// Update voice manager (call each frame)
    pub fn update(&mut self) -> Result<()> {
        if self.shutdown {
            return Ok(());
        }

        // Process network events
        self.process_network_events()?;

        // Read MumbleLink and handle room changes
        if let Some(state) = self.mumble_link.read() {
            // Cache the camera position so the UI distance readout matches
            // what the audio pipeline hears from (camera, not avatar).
            self.last_listener_position = Some(state.camera_transform.position);
            let room_key = state.room_key.clone();

            // Check if room changed
            if self.current_room.as_ref() != Some(&room_key) {
                // Leave old room
                if self.current_room.is_some() {
                    self.leave_room();
                }

                // Join new room if in game
                if state.is_in_game() {
                    let player_name = state
                        .identity
                        .as_ref()
                        .and_then(|i| {
                            let n = i.name.trim();
                            if n.is_empty() {
                                None
                            } else {
                                Some(i.name.clone())
                            }
                        })
                        .unwrap_or_else(|| "Unknown".to_string());

                    self.join_room(&room_key, &player_name)?;
                }
            }

            // Send position updates (throttled)
            if self.state == VoiceState::InRoom
                && self.last_position_update.elapsed() > std::time::Duration::from_millis(100)
            {
                if let Some(tx) = &self.network_cmd_tx {
                    let position = crate::position::Position {
                        x: state.transform.position.x,
                        y: state.transform.position.y,
                        z: state.transform.position.z,
                    };
                    let front = crate::position::Position {
                        x: state.transform.front.x,
                        y: state.transform.front.y,
                        z: state.transform.front.z,
                    };
                    let _ = tx.send(NetworkCommand::UpdatePosition { position, front });
                    self.last_position_update = std::time::Instant::now();
                }
            }
        }

        // Process outgoing audio
        self.process_outgoing_audio()?;

        Ok(())
    }

    /// Process network events from the async task
    fn process_network_events(&mut self) -> Result<()> {
        // Collect events first to avoid borrow conflict
        let events: Vec<NetworkEvent> = if let Some(rx) = &self.network_event_rx {
            let collected: Vec<NetworkEvent> = rx.try_iter().collect();
            for ev in collected.iter() {
                match ev {
                    NetworkEvent::PeerJoined { peer_id, player_name } => log::info!("  event: PeerJoined {} ({})", player_name, peer_id),
                    NetworkEvent::PeerPosition { peer_id, .. } => log::info!("  event: PeerPosition ({})", peer_id),
                    NetworkEvent::AudioReceived { peer_id, .. } => log::info!("  event: AudioReceived ({})", peer_id),
                    NetworkEvent::RoomJoined { room_id, peers } => log::info!("  event: RoomJoined {} ({} peers)", room_id, peers.len()),
                    NetworkEvent::Connected { peer_id } => log::info!("  event: Connected ({})", peer_id),
                    NetworkEvent::Disconnected => log::info!("  event: Disconnected"),
                    NetworkEvent::PeerLeft { peer_id } => log::info!("  event: PeerLeft ({})", peer_id),
                    NetworkEvent::Error { message } => log::info!("  event: Error: {}", message),
                }
            }
            collected
        } else {
            return Ok(());
        };

        for event in events {
            match event {
                NetworkEvent::Connected { peer_id } => {
                    log::info!("Connected to signaling server with peer ID: {}", peer_id);
                    self.our_peer_id = Some(peer_id);
                    self.state = VoiceState::Connected;

                    // Start audio if we're connected
                    let _ = self.start();

                    // If we were in a room before the disconnect, rejoin it.
                    if let Some(room_id) = self.current_room.clone() {
                        let player_name = self
                            .mumble_link
                            .read()
                            .and_then(|s| s.identity.as_ref().map(|i| i.name.clone()))
                            .filter(|n| !n.trim().is_empty())
                            .unwrap_or_else(|| "Unknown".to_string());
                        log::info!("Re-joining room {} after reconnect", room_id);
                        if let Some(tx) = &self.network_cmd_tx {
                            let _ = tx.send(NetworkCommand::JoinRoom {
                                room_id,
                                player_name,
                            });
                        }
                    }
                }
                NetworkEvent::Disconnected => {
                    log::info!("Disconnected from signaling server");
                    self.state = VoiceState::Disconnected;
                    // Keep `current_room` so we can rejoin on reconnect.
                    self.peers.clear();
                    self.send_incoming_audio_command(IncomingAudioCommand::ResetIncoming);
                }
                NetworkEvent::RoomJoined { room_id, peers } => {
                    log::info!("Joined room {} with {} peers", room_id, peers.len());
                    self.current_room = Some(room_id);
                    self.state = VoiceState::InRoom;

                    self.send_incoming_audio_command(IncomingAudioCommand::ResetIncoming);

                    // Add existing peers
                    for peer in peers {
                        if let Err(e) = self.add_peer(&peer.peer_id, &peer.player_name) {
                            log::error!("Failed to add peer {}: {}", peer.peer_id, e);
                        }
                        // Update position if available
                        if let (Some(pos), Some(front)) = (peer.position, peer.front) {
                            self.update_peer_position(&peer.peer_id, pos, front);
                        }
                    }
                }
                NetworkEvent::PeerJoined { peer_id, player_name } => {
                    log::info!("Peer joined: {} ({})", player_name, peer_id);
                    if let Err(e) = self.add_peer(&peer_id, &player_name) {
                        log::error!("Failed to add peer {}: {}", peer_id, e);
                    }
                }
                NetworkEvent::PeerLeft { peer_id } => {
                    log::info!("Peer left: {}", peer_id);
                    self.remove_peer(&peer_id);
                }
                NetworkEvent::PeerPosition { peer_id, position, front } => {
                    self.update_peer_position(&peer_id, position, front);
                }
                NetworkEvent::AudioReceived { peer_id, data } => {
                    if let Err(e) = self.receive_peer_audio(&peer_id, &data) {
                        log::trace!("Failed to receive audio from {}: {}", peer_id, e);
                    }
                }
                NetworkEvent::Error { message } => {
                    log::error!("Network error: {}", message);
                }
            }
        }
        Ok(())
    }

    /// Join a voice room
    fn join_room(&mut self, room_id: &str, player_name: &str) -> Result<()> {
        log::info!("Joining room: {}", room_id);

        if let Some(tx) = &self.network_cmd_tx {
            tx.send(NetworkCommand::JoinRoom {
                room_id: room_id.to_string(),
                player_name: player_name.to_string(),
            })?;
        }

        self.current_room = Some(room_id.to_string());
        Ok(())
    }

    /// Leave current room
    fn leave_room(&mut self) {
        if let Some(room_id) = self.current_room.take() {
            log::info!("Leaving room: {}", room_id);

            if let Some(tx) = &self.network_cmd_tx {
                let _ = tx.send(NetworkCommand::LeaveRoom);
            }

            // Clear all peers
            self.peers.clear();
            self.send_incoming_audio_command(IncomingAudioCommand::ResetIncoming);
            self.state = VoiceState::Connected;
        }
    }

    /// Process outgoing audio
    fn process_outgoing_audio(&mut self) -> Result<()> {
        // Check if we should transmit
        if self.settings.is_muted || self.state != VoiceState::InRoom {
            return Ok(());
        }

        let should_transmit = match self.settings.mode {
            VoiceMode::PushToTalk => self.ptt_active.load(Ordering::Relaxed),
            VoiceMode::VoiceActivity => true, // VAD check happens below
            VoiceMode::AlwaysOn => true,
        };

        if !should_transmit {
            return Ok(());
        }

        // Get captured audio from audio thread
        if let Some(ref audio_thread) = self.audio_thread {
            let capture_rx = audio_thread.capture_receiver();

            while let Ok(samples) = capture_rx.try_recv() {
                // Apply input volume
                let samples: Vec<f32> = samples
                    .into_iter()
                    .map(|s| s * self.settings.input_volume)
                    .collect();

                // Check VAD if in voice activity mode
                if self.settings.mode == VoiceMode::VoiceActivity {
                    if let Some(ref mut vad) = self.vad {
                        if !vad.process(&samples) {
                            continue;
                        }
                    }
                }

                // Skip near-silent frames: mousiki's SILK encoder has div-by-zero
                // panics on all-zero input (pathological pitch/predictor cases),
                // and there's no value in spending bandwidth on silence anyway.
                let peak = samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
                if peak < 1e-4 {
                    continue;
                }

                // Encode with Opus
                if let Some(ref mut encoder) = self.encoder {
                    match encoder.encode(&samples) {
                        Ok(encoded) => {
                            if !encoded.is_empty() {
                                // Send to network task for broadcast
                                if let Some(tx) = &self.network_cmd_tx {
                                    let _ = tx.send(NetworkCommand::SendAudio { data: encoded });
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to encode audio: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Add a peer (called when a new player joins the room)
    pub fn add_peer(&mut self, peer_id: &str, player_name: &str) -> Result<()> {
        // Debug: show current our_peer_id
        log::info!("add_peer called for {} (name={}), our_peer_id={:?}", peer_id, player_name, self.our_peer_id);

        // Don't add ourselves
        if Some(peer_id.to_string()) == self.our_peer_id {
            log::info!("Skipping add_peer for {} because it matches our_peer_id", peer_id);
            return Ok(());
        }

        log::info!("Adding peer: {} ({})", player_name, peer_id);

        let peer = VoicePeer::new(peer_id.to_string(), player_name.to_string())?;
        self.peers.insert(peer_id.to_string(), peer);
        self.send_incoming_audio_command(IncomingAudioCommand::UpsertPeer {
            peer_id: peer_id.to_string(),
            player_name: player_name.to_string(),
        });

        // Log peers after insertion
        let ids: Vec<String> = self.peers.keys().cloned().collect();
        log::info!("Peer added. now {} peers: [{}]", self.peers.len(), ids.join(", "));

        Ok(())
    }

    /// Remove a peer (called when a player leaves the room)
    pub fn remove_peer(&mut self, peer_id: &str) {
        log::info!("Removing peer: {}", peer_id);
        self.peers.remove(peer_id);
        self.send_incoming_audio_command(IncomingAudioCommand::RemovePeer {
            peer_id: peer_id.to_string(),
        });
    }

    /// Receive audio data for a peer
    pub fn receive_peer_audio(&mut self, peer_id: &str, opus_data: &[u8]) -> Result<()> {
        if let Some(peer) = self.peers.get(peer_id) {
            peer.mark_audio_received();
        }
        let _ = opus_data;
        Ok(())
    }

    /// Update peer position
    pub fn update_peer_position(
        &mut self,
        peer_id: &str,
        position: crate::position::Position,
        front: crate::position::Position,
    ) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.update_position(position, front);
            self.send_incoming_audio_command(IncomingAudioCommand::SetPeerPosition {
                peer_id: peer_id.to_string(),
                position,
                front,
            });
        }
    }

    /// Set PTT state (thread-safe, can be called from keybind handler)
    pub fn set_ptt(&self, active: bool) {
        self.ptt_active.store(active, Ordering::Relaxed);
    }

    /// Get PTT state
    pub fn is_ptt_active(&self) -> bool {
        self.ptt_active.load(Ordering::Relaxed)
    }

    /// Get settings
    pub fn settings(&self) -> VoiceSettings {
        self.settings.clone()
    }

    /// Update settings
    pub fn update_settings<F>(&mut self, f: F)
    where
        F: FnOnce(&mut VoiceSettings),
    {
        f(&mut self.settings);
        self.sync_playback_settings();
    }

    /// Get current state
    pub fn state(&self) -> VoiceState {
        self.state
    }

    /// Get peer count
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get peer info for the UI, including position and distance from the listener.
    pub fn get_peers(&self) -> Vec<NearbyPeer> {
        let listener = self.last_listener_position;
        self.peers
            .values()
            .map(|p| NearbyPeer {
                peer_id: p.peer_id.clone(),
                player_name: p.player_name.clone(),
                is_speaking: p.is_speaking(),
                is_muted: p.is_muted,
                position: p.position,
                distance: listener.map(|l| l.distance_to(&p.position)),
            })
            .collect()
    }

    /// Switch the audio capture device by name.
    pub fn set_input_device(&mut self, name: &str) {
        if let Some(ref audio_thread) = self.audio_thread {
            if let Err(e) = audio_thread.send_command(AudioCommand::SetInputDevice(name.to_string())) {
                log::warn!("Failed to send SetInputDevice to audio thread: {}", e);
            } else {
                log::info!("Requested input device change: {}", name);
            }
        }
    }

    /// Switch the audio playback device by name.
    pub fn set_output_device(&mut self, name: &str) {
        if let Some(ref audio_thread) = self.audio_thread {
            if let Err(e) = audio_thread.send_command(AudioCommand::SetOutputDevice(name.to_string())) {
                log::warn!("Failed to send SetOutputDevice to audio thread: {}", e);
            } else {
                log::info!("Requested output device change: {}", name);
            }
        }
    }

    /// Mute a specific peer
    pub fn mute_peer(&mut self, peer_id: &str, muted: bool) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.set_muted(muted);
            self.send_incoming_audio_command(IncomingAudioCommand::SetPeerMuted {
                peer_id: peer_id.to_string(),
                muted,
            });
            log::info!("Set mute={} for peer {}", muted, peer_id);
        } else {
            log::warn!("mute_peer: peer not found: {}", peer_id);
        }
    }

    /// Unmute a specific peer (convenience wrapper)
    pub fn unmute_peer(&mut self, peer_id: &str) {
        self.mute_peer(peer_id, false);
    }

    /// Toggle mute state for a peer and return the new state if present
    pub fn toggle_mute_peer(&mut self, peer_id: &str) -> Option<bool> {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            let new_state = !peer.is_muted;
            peer.set_muted(new_state);
            self.send_incoming_audio_command(IncomingAudioCommand::SetPeerMuted {
                peer_id: peer_id.to_string(),
                muted: new_state,
            });
            log::info!("Toggled mute -> {} for peer {}", new_state, peer_id);
            Some(new_state)
        } else {
            log::warn!("toggle_mute_peer: peer not found: {}", peer_id);
            None
        }
    }

    /// Set peer volume
    pub fn set_peer_volume(&mut self, peer_id: &str, volume: f32) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.set_volume(volume);
            let applied_volume = peer.volume;
            let peer_id = peer_id.to_string();
            let _ = peer;
            self.send_incoming_audio_command(IncomingAudioCommand::SetPeerVolume {
                peer_id,
                volume: applied_volume,
            });
        }
    }

    /// Get available input devices
    pub fn get_input_devices(&self) -> Vec<String> {
        vec!["Default".to_string()]
    }

    /// Get available output devices
    pub fn get_output_devices(&self) -> Vec<String> {
        vec!["Default".to_string()]
    }

    fn send_incoming_audio_command(&self, cmd: IncomingAudioCommand) {
        if let Some(audio_thread) = &self.audio_thread {
            if let Err(e) = audio_thread.send_incoming_command(cmd) {
                log::trace!("Failed to send incoming audio command: {}", e);
            }
        }
    }

    fn sync_playback_settings(&self) {
        self.send_incoming_audio_command(IncomingAudioCommand::SetPlaybackSettings {
            min_distance: self.settings.min_distance,
            max_distance: self.settings.max_distance,
            output_volume: self.settings.output_volume,
            is_deafened: self.settings.is_deafened,
            directional_audio_enabled: self.settings.directional_audio_enabled,
            spatial_3d_enabled: self.settings.spatial_3d_enabled,
        });
    }

    /// Shutdown voice manager
    pub fn shutdown(&mut self) {
        self.shutdown = true;

        // Send disconnect command
        if let Some(tx) = &self.network_cmd_tx {
            let _ = tx.send(NetworkCommand::Disconnect);
        }

        // Stop and shutdown audio thread
        if let Some(mut audio_thread) = self.audio_thread.take() {
            audio_thread.shutdown();
        }

        // Shutdown runtime
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }

        // Clear state
        self.peers.clear();
        self.current_room = None;
        self.state = VoiceState::Disconnected;
    }

    /// Check if shutdown requested
    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Get current room ID
    pub fn current_room(&self) -> Option<&str> {
        self.current_room.as_deref()
    }

    /// Get server URL
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// Set server URL (will disconnect and reconnect if connected)
    pub fn set_server_url(&mut self, url: &str) {
        if self.server_url == url {
            return;
        }

        log::info!("Changing server URL to: {}", url);
        self.server_url = url.to_string();
        self.settings.server_url = url.to_string();

        // If we're connected, we need to restart the network task
        if self.state != VoiceState::Disconnected {
            self.shutdown();
            // Re-initialize after a brief moment
            let _ = self.init();
        }
    }
}

impl Default for VoiceManager {
    fn default() -> Self {
        Self::new(DEFAULT_SERVER_URL)
    }
}

impl Drop for VoiceManager {
    fn drop(&mut self) {
        if !self.shutdown {
            self.shutdown();
        }
    }
}

/// Async network task that handles signaling and audio relay
async fn network_task(
    server_url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<NetworkCommand>,
    event_tx: std::sync::mpsc::Sender<NetworkEvent>,
    incoming_audio_tx: crossbeam_channel::Sender<IncomingAudioCommand>,
) {
    log::info!("Network task started");

    let mut signaling_client = SignalingClient::new(&server_url);
    let mut signaling_event_rx: Option<mpsc::UnboundedReceiver<ServerMessage>> = None;
    let mut was_connected = false;

    let mut reconnect_timer = tokio::time::interval(std::time::Duration::from_secs(2));
    reconnect_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so we don't race the initial Connect command.
    reconnect_timer.tick().await;

    loop {
        tokio::select! {
            // Handle commands from VoiceManager
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    NetworkCommand::Connect => {
                        log::info!("Connecting to signaling server...");
                        match signaling_client.connect().await {
                            Ok(()) => {
                                signaling_event_rx = signaling_client.take_event_receiver();
                            }
                            Err(e) => {
                                log::error!("Failed to connect: {}", e);
                                let _ = event_tx.send(NetworkEvent::Error {
                                    message: format!("Connection failed: {}", e),
                                });
                            }
                        }
                    }
                    NetworkCommand::Disconnect => {
                        log::info!("Disconnecting...");
                        signaling_client.disconnect();
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::ResetIncoming);
                        let _ = event_tx.send(NetworkEvent::Disconnected);
                        break;
                    }
                    NetworkCommand::JoinRoom { room_id, player_name } => {
                        if let Err(e) = signaling_client.join_room(&room_id, &player_name) {
                            log::error!("Failed to join room: {}", e);
                        }
                    }
                    NetworkCommand::LeaveRoom => {
                        if let Err(e) = signaling_client.leave_room() {
                            log::error!("Failed to leave room: {}", e);
                        }
                    }
                    NetworkCommand::UpdatePosition { position, front } => {
                        let _ = signaling_client.update_position(position, front);
                    }
                    NetworkCommand::SendAudio { data } => {
                        let _ = signaling_client.send_audio(&data);
                    }
                }
            }

            // Handle signaling events
            msg = async {
                if let Some(ref mut rx) = signaling_event_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match msg {
                    Some(ServerMessage::Welcome { peer_id }) => {
                        let _ = event_tx.send(NetworkEvent::Connected { peer_id });
                    }
                    Some(ServerMessage::RoomJoined { room_id, peers }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::ResetIncoming);
                        for peer in &peers {
                            let _ = incoming_audio_tx.send(IncomingAudioCommand::UpsertPeer {
                                peer_id: peer.peer_id.clone(),
                                player_name: peer.player_name.clone(),
                            });
                            if let (Some(position), Some(front)) = (peer.position, peer.front) {
                                let _ = incoming_audio_tx.send(IncomingAudioCommand::SetPeerPosition {
                                    peer_id: peer.peer_id.clone(),
                                    position,
                                    front,
                                });
                            }
                        }
                        let _ = event_tx.send(NetworkEvent::RoomJoined { room_id, peers });
                    }
                    Some(ServerMessage::PeerJoined { peer }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::UpsertPeer {
                            peer_id: peer.peer_id.clone(),
                            player_name: peer.player_name.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::PeerJoined {
                            peer_id: peer.peer_id,
                            player_name: peer.player_name,
                        });
                    }
                    Some(ServerMessage::PeerLeft { peer_id }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::RemovePeer {
                            peer_id: peer_id.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::PeerLeft { peer_id });
                    }
                    Some(ServerMessage::PeerPosition { peer_id, position, front }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::SetPeerPosition {
                            peer_id: peer_id.clone(),
                            position,
                            front,
                        });
                        let _ = event_tx.send(NetworkEvent::PeerPosition { peer_id, position, front });
                    }
                    Some(ServerMessage::PeerAudio { peer_id, data }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::PushPeerOpus {
                            peer_id: peer_id.clone(),
                            data: data.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::AudioReceived {
                            peer_id,
                            data,
                        });
                    }
                    Some(ServerMessage::Error { message }) => {
                        log::error!("Server error: {}", message);
                        let _ = event_tx.send(NetworkEvent::Error { message });
                    }
                    Some(ServerMessage::Pong) => {
                        // Keepalive response
                    }
                    None => {
                        // Signaling read task ended — the server dropped us.
                        signaling_event_rx = None;
                    }
                }
            }

            _ = reconnect_timer.tick() => {
                // Periodic wake-up to evaluate reconnection below.
            }
        }

        let state = signaling_client.state();
        if state == ConnectionState::Connected {
            was_connected = true;
        } else if state == ConnectionState::Disconnected {
            if was_connected {
                was_connected = false;
                let _ = incoming_audio_tx.send(IncomingAudioCommand::ResetIncoming);
                let _ = event_tx.send(NetworkEvent::Disconnected);
            }
            log::info!("Signaling disconnected; attempting reconnect...");
            match signaling_client.connect().await {
                Ok(()) => {
                    signaling_event_rx = signaling_client.take_event_receiver();
                    log::info!("Signaling reconnect succeeded");
                }
                Err(e) => {
                    log::warn!("Signaling reconnect failed: {}", e);
                }
            }
        }
    }

    log::info!("Network task stopped");
}
