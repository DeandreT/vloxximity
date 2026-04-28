//! Central voice coordination and peer lifecycle management.

use anyhow::Result;
use parking_lot::RwLock as PlRwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::audio::thread::AudioCommand;
use crate::audio::{AudioThread, IncomingAudioCommand, OpusEncoder, VoiceActivityDetector};
use crate::network::{ConnectionState, PeerInfo, ServerMessage, SignalingClient};
use crate::position::MumbleLink;

use super::active_speak::ActiveSpeak;
use super::group::{GroupKind, GroupMemberEvent, GroupState};
use super::peer::VoicePeer;
use super::persist;
use super::room_type::{RoomType, RoomTypeVolumes};

/// Result of a local GW2 API key validation. Lives in a shared slot on
/// `VoiceManager` so a background tokio task can write the result and the
/// UI can read it on the next frame.
#[derive(Debug, Clone)]
pub enum ApiKeyStatus {
    /// No key entered, or the current key hasn't been validated yet.
    Unknown,
    /// A validation request is in flight.
    Validating,
    /// GW2 returned an account handle for this key. `checked_at` is roughly
    /// when validation completed (used to suppress stale UI messages).
    Valid { account_name: String },
    /// GW2 rejected the key or the request failed. `message` is a short,
    /// user-visible reason.
    Invalid { message: String },
}

/// Voice activation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Voice manager settings. `#[serde(default)]` on the struct keeps older
/// on-disk configs loadable as we add new fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    pub mode: VoiceMode,
    pub ptt_key: u32,
    pub min_distance: f32,
    pub max_distance: f32,
    pub input_volume: f32,
    pub output_volume: f32,
    /// Session-only
    pub is_muted: bool,
    /// Session-only
    #[serde(skip)]
    pub is_deafened: bool,
    /// Master switch for directional cues. When false, all peers play centered
    /// (mono → both ears) with distance attenuation only.
    pub directional_audio_enabled: bool,
    /// When directional audio is on, selects 3D filter model vs legacy 2D pan.
    pub spatial_3d_enabled: bool,
    pub show_peer_markers: bool,
    pub server_url: String,
    #[serde(skip)]
    pub gw2_api_key: String,
    /// Per-room-type playback gain. Map rooms keep the spatial pipeline;
    /// squad/party play centered with this gain on top of the
    /// per-peer and master output volumes.
    #[serde(default)]
    pub room_type_volumes: RoomTypeVolumes,
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
            show_peer_markers: false,
            server_url: DEFAULT_SERVER_URL.to_string(),
            gw2_api_key: String::new(),
            room_type_volumes: RoomTypeVolumes::default(),
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

/// Suggested rooms derived from RTAPI group state + the server's
/// clustering reply. Surfaced in the settings UI as click-to-join rows.
#[derive(Debug, Clone)]
pub struct GroupSuggestions {
    pub room_id: String,
    pub member_count: usize,
    pub commander_account_name: Option<String>,
    pub kind: GroupKind,
}

/// Snapshot of a peer for UI display.
#[derive(Debug, Clone)]
pub struct NearbyPeer {
    pub peer_id: String,
    pub player_name: String,
    /// Server-validated GW2 account handle, when known.
    pub account_name: Option<String>,
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
    JoinRoom {
        room_id: String,
        player_name: String,
        api_key: Option<String>,
    },
    ValidateApiKey { api_key: String },
    /// Leave a single room (`Some`) or every joined room (`None`).
    LeaveRoom { room_id: Option<String> },
    UpdatePosition { position: crate::position::Position, front: crate::position::Position },
    SendAudio { room_id: String, data: Vec<u8> },
    /// Forward a debounced group snapshot to the server for clustering.
    IdentifyGroup { members: Vec<String> },
}

/// Events received from the network task
#[derive(Debug)]
pub enum NetworkEvent {
    Connected { peer_id: String },
    Disconnected,
    AccountValidated { account_name: Option<String> },
    RoomJoined { room_id: String, peers: Vec<PeerInfo> },
    JoinRejected { room_id: String, reason: String },
    PeerJoined { room_id: String, peer_id: String, player_name: String, account_name: Option<String> },
    PeerLeft { room_id: String, peer_id: String },
    PeerPosition { peer_id: String, position: crate::position::Position, front: crate::position::Position },
    AudioReceived { room_id: String, peer_id: String, data: Vec<u8> },
    GroupIdentified { cluster_id: String },
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
    /// Every room the local client is joined to, keyed by room id with
    /// the join timestamp as value. Timestamps power the per-type
    /// fallback tie-break in `ActiveSpeak::resolve`. The MumbleLink tick
    /// owns the `map:` entry; manually-joined rooms live alongside it.
    joined_rooms: HashMap<String, Instant>,
    /// The `map:` room the MumbleLink-driven flow currently owns. Tracked
    /// separately from `joined_rooms` so a map change leaves only the old
    /// map room and not, e.g., a squad room the user manually joined.
    current_map_room: Option<String>,

    // Audio thread (handles cpal on separate thread)
    audio_thread: Option<AudioThread>,

    // Audio processing components
    encoder: Option<OpusEncoder>,
    vad: Option<VoiceActivityDetector>,
    // Peers
    peers: HashMap<String, VoicePeer>,

    // PTT state (atomic for thread-safe access). Kept for the legacy
    // single-PTT identifier the user may already have bound; the new
    // multi-room speak resolver lives in `active_speak`.
    ptt_active: Arc<AtomicBool>,
    /// Per-type PTT key state + speak-room resolver. Cloned into the
    /// keybind handlers in `lib.rs`.
    active_speak: ActiveSpeak,

    // Network communication channels
    network_cmd_tx: Option<mpsc::UnboundedSender<NetworkCommand>>,
    network_event_rx: Option<std::sync::mpsc::Receiver<NetworkEvent>>,

    // Tokio runtime handle
    runtime: Option<tokio::runtime::Runtime>,

    // Position update throttle
    last_position_update: Instant,

    // Last known listener (avatar) position, cached from MumbleLink for UI display.
    last_listener_position: Option<crate::position::Position>,

    // Last known camera transform + vertical FOV, cached for the world-space
    // peer marker overlay.
    last_camera_transform: Option<crate::position::Transform>,
    last_fov: Option<f32>,

    // Our own GW2 account handle, read from RTAPI when available. Shown in
    // the settings UI so the user can sanity-check the API key they pasted.
    own_account_name: Option<String>,

    // Persisted account-keyed mutes. Mirrors `<addon_dir>/mutes.json`.
    muted_accounts: HashSet<String>,

    // Result of the most recent server-side validation of the saved API
    // key. Populated when the server responds to our JoinRoom with an
    // AccountValidated message.
    api_key_status: Arc<PlRwLock<ApiKeyStatus>>,

    // Which API key the current `api_key_status` actually applies to. The
    // UI uses this to invalidate stale "Valid/Invalid" indicators the
    // moment the user starts editing a new key.
    api_key_status_for: Arc<PlRwLock<Option<String>>>,

    // Reason the last JoinRoom was rejected, if any. Cleared on successful
    // join. Surfaced in the settings UI so the user knows why they're not
    // hearing anyone.
    last_join_rejection: Option<String>,

    // Local mirror of the GW2 RTAPI group, fed by `handle_group_member_event`
    // from the keybind/event-callback path in `lib.rs`.
    group: GroupState,

    // Coalesces a flurry of GROUP_MEMBER_JOINED/LEFT events into a single
    // `IdentifyGroup` round-trip. `Some(t)` = we have an outstanding change
    // that should be reported once `t` has elapsed.
    pending_identify_at: Option<Instant>,

    // Most recent server-issued cluster id for the local group.
    last_cluster_id: Option<String>,

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
        Self::with_persistence(server_url, VoiceSettings::default(), HashSet::new())
    }

    /// Construct a `VoiceManager` seeded from disk. Call sites load the
    /// settings and mutes on addon startup and hand them in here; the
    /// manager uses the provided `settings.server_url` as its connection
    /// target.
    pub fn with_persistence(
        server_url: &str,
        settings: VoiceSettings,
        muted_accounts: HashSet<String>,
    ) -> Self {
        Self {
            state: VoiceState::Disconnected,
            settings,
            server_url: server_url.to_string(),
            our_peer_id: None,
            mumble_link: MumbleLink::new(),
            joined_rooms: HashMap::new(),
            current_map_room: None,
            audio_thread: None,
            encoder: None,
            vad: None,
            peers: HashMap::new(),
            ptt_active: Arc::new(AtomicBool::new(false)),
            active_speak: ActiveSpeak::new(),
            network_cmd_tx: None,
            network_event_rx: None,
            runtime: None,
            last_position_update: Instant::now(),
            last_listener_position: None,
            last_camera_transform: None,
            last_fov: None,
            own_account_name: None,
            muted_accounts,
            api_key_status: Arc::new(PlRwLock::new(ApiKeyStatus::Unknown)),
            api_key_status_for: Arc::new(PlRwLock::new(None)),
            last_join_rejection: None,
            group: GroupState::new(),
            pending_identify_at: None,
            last_cluster_id: None,
            shutdown: false,
        }
    }

    /// Initialize all components
    pub fn init(&mut self) -> Result<()> {
        // Clear any previous shutdown flag so we can restart after a shutdown.
        self.shutdown = false;
        // Initialize MumbleLink
        self.mumble_link.init()?;
        self.refresh_own_account_name();

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
            self.last_camera_transform = Some(state.camera_transform);
            self.last_fov = state.identity.as_ref().map(|i| i.fov).filter(|f| *f > 0.0);
            self.send_incoming_audio_command(IncomingAudioCommand::SetListenerTransform(
                state.camera_transform,
            ));
            // Encode the MumbleLink room key as `map:<hash>` so the server
            // can route it without knowing about room types. Only the map
            // room is auto-managed here; other rooms (squad/party/etc.) are
            // joined via the UI and persist across map changes.
            let map_room_id = if state.is_in_game() {
                Some(format!("map:{}", state.room_key))
            } else {
                None
            };

            if self.current_map_room != map_room_id {
                // Leave the old map room (if any) without touching other rooms.
                if let Some(old) = self.current_map_room.take() {
                    self.leave_room(&old);
                }

                // Join the new map room if we're in game.
                if let Some(new_room) = map_room_id.as_deref() {
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

                    self.join_room(new_room, &player_name)?;
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

        // Flush any pending RTAPI-driven group identification.
        self.flush_pending_identify();

        // Process outgoing audio
        self.process_outgoing_audio()?;

        Ok(())
    }

    /// Apply a Nexus RTAPI group-member event. Called from the FFI
    /// callbacks registered in `lib.rs`. Updates the local cache and
    /// schedules a debounced `IdentifyGroup` send if anything relevant
    /// changed.
    pub fn handle_group_member_event(&mut self, event: GroupMemberEvent) {
        let changed = self.group.apply(event);
        if !changed {
            return;
        }

        if self.group.member_count() <= 1 {
            // Solo / disbanded. Drop the suggestion entirely.
            self.last_cluster_id = None;
            self.pending_identify_at = None;
            return;
        }

        // 300 ms debounce on top of the per-bucket rate limit. The flush
        // path in `update()` will fire the actual `IdentifyGroup`.
        self.pending_identify_at =
            Some(Instant::now() + std::time::Duration::from_millis(300));
    }

    /// Send an `IdentifyGroup` if the debounce timer has expired and the
    /// local group still has multiple members.
    fn flush_pending_identify(&mut self) {
        let Some(deadline) = self.pending_identify_at else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.pending_identify_at = None;

        if self.group.member_count() <= 1 {
            return;
        }
        if self.state == VoiceState::Disconnected {
            // Manager hasn't connected yet; retry on the next change.
            return;
        }

        let members = self.group.member_account_names();
        if let Some(tx) = &self.network_cmd_tx {
            let _ = tx.send(NetworkCommand::IdentifyGroup { members });
        }
    }

    /// Snapshot of the suggested squad / party room based on the most
    /// recent server clustering reply. None when we're solo or the server
    /// hasn't replied yet.
    pub fn group_suggestions(&self) -> Option<GroupSuggestions> {
        let cluster = self.last_cluster_id.as_ref()?;
        let kind = self.group.classify();
        if matches!(kind, GroupKind::None) {
            return None;
        }
        let room_id = match kind {
            GroupKind::Squad => format!("squad:{}", cluster),
            GroupKind::Party => format!("party:{}", cluster),
            GroupKind::None => return None,
        };
        Some(GroupSuggestions {
            room_id,
            member_count: self.group.member_count(),
            commander_account_name: self.group.commander_name().map(str::to_string),
            kind,
        })
    }

    /// Process network events from the async task
    fn process_network_events(&mut self) -> Result<()> {
        // Collect events first to avoid borrow conflict
        let events: Vec<NetworkEvent> = if let Some(rx) = &self.network_event_rx {
            let collected: Vec<NetworkEvent> = rx.try_iter().collect();
            for ev in collected.iter() {
                match ev {
                    NetworkEvent::PeerJoined { room_id, peer_id, player_name, account_name } => log::info!("  event: PeerJoined {} ({}) room={} account={:?}", player_name, peer_id, room_id, account_name),
                    NetworkEvent::PeerPosition { peer_id, .. } => log::info!("  event: PeerPosition ({})", peer_id),
                    NetworkEvent::AudioReceived { room_id, peer_id, .. } => log::info!("  event: AudioReceived ({} in {})", peer_id, room_id),
                    NetworkEvent::RoomJoined { room_id, peers } => log::info!("  event: RoomJoined {} ({} peers)", room_id, peers.len()),
                    NetworkEvent::Connected { peer_id } => log::info!("  event: Connected ({})", peer_id),
                    NetworkEvent::Disconnected => log::info!("  event: Disconnected"),
                    NetworkEvent::PeerLeft { room_id, peer_id } => log::info!("  event: PeerLeft ({} from {})", peer_id, room_id),
                    NetworkEvent::Error { message } => log::info!("  event: Error: {}", message),
                    NetworkEvent::AccountValidated { account_name } => log::info!("  event: AccountValidated account={:?}", account_name),
                    NetworkEvent::JoinRejected { room_id, reason } => log::info!("  event: JoinRejected (room={}): {}", room_id, reason),
                    NetworkEvent::GroupIdentified { cluster_id } => log::info!("  event: GroupIdentified cluster={}", cluster_id),
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

                    // Re-issue every joined room after reconnect. We snapshot
                    // and clear the local set first, then route each through
                    // the normal join path so server acks repopulate it.
                    let to_rejoin: Vec<String> = self.joined_rooms.drain().map(|(k, _)| k).collect();
                    if !to_rejoin.is_empty() {
                        let player_name = self
                            .mumble_link
                            .read()
                            .and_then(|s| s.identity.as_ref().map(|i| i.name.clone()))
                            .filter(|n| !n.trim().is_empty())
                            .unwrap_or_else(|| "Unknown".to_string());
                        let api_key = self.api_key_to_send();
                        if api_key.is_none() {
                            let reason = "GW2 API key required to join rooms (set one in Vloxximity settings)";
                            log::warn!("Skipping reconnect JoinRooms ({}): {}", to_rejoin.len(), reason);
                            self.current_map_room = None;
                            self.last_join_rejection = Some(reason.to_string());
                        } else {
                            if let Some(key) = api_key.as_deref() {
                                self.mark_api_key_validating(key);
                            }
                            for room_id in to_rejoin {
                                log::info!("Re-joining room {} after reconnect", room_id);
                                if let Some(tx) = &self.network_cmd_tx {
                                    let _ = tx.send(NetworkCommand::JoinRoom {
                                        room_id,
                                        player_name: player_name.clone(),
                                        api_key: api_key.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                NetworkEvent::Disconnected => {
                    log::info!("Disconnected from signaling server");
                    self.state = VoiceState::Disconnected;
                    // Keep `joined_rooms` so we can rejoin on reconnect.
                    self.peers.clear();
                    self.send_incoming_audio_command(IncomingAudioCommand::ResetIncoming);
                }
                NetworkEvent::RoomJoined { room_id, peers } => {
                    log::info!("Joined room {} with {} peers", room_id, peers.len());
                    self.joined_rooms.insert(room_id.clone(), Instant::now());
                    self.state = VoiceState::InRoom;
                    self.last_join_rejection = None;

                    // Add existing peers / update their per-room membership.
                    for peer in peers {
                        let account = peer.account_name.as_deref();
                        if let Err(e) = self.add_peer_to_room(
                            &peer.peer_id,
                            &peer.player_name,
                            account,
                            &room_id,
                        ) {
                            log::error!("Failed to add peer {}: {}", peer.peer_id, e);
                        }
                        // Update position if available
                        if let (Some(pos), Some(front)) = (peer.position, peer.front) {
                            self.update_peer_position(&peer.peer_id, pos, front);
                        }
                    }
                }
                NetworkEvent::PeerJoined { room_id, peer_id, player_name, account_name } => {
                    log::info!(
                        "Peer joined: {} ({}) in room {} account={:?}",
                        player_name, peer_id, room_id, account_name
                    );
                    if let Err(e) = self.add_peer_to_room(
                        &peer_id,
                        &player_name,
                        account_name.as_deref(),
                        &room_id,
                    ) {
                        log::error!("Failed to add peer {}: {}", peer_id, e);
                    }
                }
                NetworkEvent::PeerLeft { room_id, peer_id } => {
                    log::info!("Peer left: {} from room {}", peer_id, room_id);
                    self.remove_peer_from_room(&peer_id, &room_id);
                }
                NetworkEvent::PeerPosition { peer_id, position, front } => {
                    self.update_peer_position(&peer_id, position, front);
                }
                NetworkEvent::AudioReceived { room_id, peer_id, data } => {
                    if let Err(e) = self.receive_peer_audio(&peer_id, &room_id, &data) {
                        log::trace!("Failed to receive audio from {}: {}", peer_id, e);
                    }
                }
                NetworkEvent::Error { message } => {
                    log::error!("Network error: {}", message);
                }
                NetworkEvent::GroupIdentified { cluster_id } => {
                    log::info!("Server clustered our group: cluster={}", cluster_id);
                    self.last_cluster_id = Some(cluster_id);
                }
                NetworkEvent::AccountValidated { account_name } => {
                    self.apply_api_key_validation_result(account_name);
                }
                NetworkEvent::JoinRejected { room_id, reason } => {
                    log::warn!("Server rejected JoinRoom for {}: {}", room_id, reason);
                    // Drop the rejected room from local state so the next
                    // MumbleLink tick (or a manual retry) can redo the join.
                    self.joined_rooms.remove(&room_id);
                    if self.current_map_room.as_deref() == Some(room_id.as_str()) {
                        self.current_map_room = None;
                    }
                    if self.joined_rooms.is_empty() {
                        self.state = VoiceState::Connected;
                        self.peers.clear();
                        self.send_incoming_audio_command(IncomingAudioCommand::ResetIncoming);
                    }
                    self.last_join_rejection = Some(reason);
                }
            }
        }
        Ok(())
    }

    /// Join a voice room. Records the room as the local-side map room when
    /// the id starts with `map:`, so a future map change can leave only that
    /// one. The server returns the actual roster via `RoomJoined`.
    fn join_room(&mut self, room_id: &str, player_name: &str) -> Result<()> {
        log::info!("Joining room: {}", room_id);

        let api_key = self.api_key_to_send();
        if api_key.is_none() {
            // Mirror the server's refusal locally so the UI can show the
            // same guidance without a round-trip. We still record the
            // map-room id locally so the MumbleLink-driven tick doesn't
            // retry the join every frame; `revalidate_saved_api_key`
            // clears it when the user adds a key.
            let reason = "GW2 API key required to join rooms (set one in Vloxximity settings)";
            log::warn!("Skipping JoinRoom for room {}: {}", room_id, reason);
            self.last_join_rejection = Some(reason.to_string());
            if room_id.starts_with("map:") {
                self.current_map_room = Some(room_id.to_string());
            }
            return Ok(());
        }
        if let Some(key) = api_key.as_deref() {
            self.mark_api_key_validating(key);
        }
        if let Some(tx) = &self.network_cmd_tx {
            tx.send(NetworkCommand::JoinRoom {
                room_id: room_id.to_string(),
                player_name: player_name.to_string(),
                api_key,
            })?;
        }

        if room_id.starts_with("map:") {
            self.current_map_room = Some(room_id.to_string());
        }
        Ok(())
    }

    /// Return the current API key to send on JoinRoom, if the user has
    /// configured one.
    fn api_key_to_send(&self) -> Option<String> {
        let key = self.settings.gw2_api_key.trim();
        if key.is_empty() {
            None
        } else {
            Some(key.to_string())
        }
    }

    /// Read the local GW2 account handle from Nexus RTAPI if present.
    /// Called once on `init` and on demand from the settings UI.
    pub fn refresh_own_account_name(&mut self) {
        self.own_account_name = nexus::rtapi::RealTimeApi::get()
            .and_then(|api| api.read_player())
            .map(|player| player.account_name)
            .filter(|name| !name.is_empty());
        if let Some(name) = &self.own_account_name {
            log::info!("RTAPI account name: {}", name);
        } else {
            log::info!("RTAPI not active — no local account name available");
        }
    }

    /// Local GW2 account handle if we read it from RTAPI.
    pub fn own_account_name(&self) -> Option<&str> {
        self.own_account_name.as_deref()
    }

    /// Reason the server last rejected a JoinRoom, if still relevant.
    pub fn last_join_rejection(&self) -> Option<&str> {
        self.last_join_rejection.as_deref()
    }

    /// Current client-side API key validation status. The returned value is
    /// a snapshot — the underlying state may change on the next frame.
    pub fn api_key_status(&self) -> ApiKeyStatus {
        self.api_key_status.read().clone()
    }

    /// Returns `true` if the last recorded validation result matches the
    /// current saved API key. Used by the UI to avoid showing a stale
    /// "Valid" badge after the user edits the key.
    pub fn api_key_status_matches_current(&self) -> bool {
        match &*self.api_key_status_for.read() {
            Some(key) => key == &self.settings.gw2_api_key,
            None => false,
        }
    }

    /// Mark the currently-saved API key as "in flight" for validation and
    /// record which key the status applies to. Called right before we send
    /// a JoinRoom that includes the key.
    fn mark_api_key_validating(&self, key: &str) {
        *self.api_key_status.write() = ApiKeyStatus::Validating;
        *self.api_key_status_for.write() = Some(key.to_string());
    }

    /// Ask the server to re-validate the saved API key immediately. Used
    /// when the user edits the key in settings — avoids waiting for the
    /// next room rejoin. Safe to call without an active room.
    pub fn revalidate_saved_api_key(&mut self) {
        let key = self.settings.gw2_api_key.trim().to_string();

        // Invalidate any "already joined" / "locally refused" state tied to
        // the previous key, so the next MumbleLink tick is free to retry
        // the join with the new key.
        if self.state != VoiceState::InRoom {
            self.current_map_room = None;
        }
        self.last_join_rejection = None;

        if key.is_empty() {
            // Empty key is a terminal state, not a pending validation.
            *self.api_key_status.write() = ApiKeyStatus::Unknown;
            *self.api_key_status_for.write() = Some(String::new());
            return;
        }
        self.mark_api_key_validating(&key);
        if let Some(tx) = &self.network_cmd_tx {
            if let Err(e) = tx.send(NetworkCommand::ValidateApiKey {
                api_key: key,
            }) {
                log::warn!("Failed to queue ValidateApiKey command: {}", e);
                *self.api_key_status.write() = ApiKeyStatus::Invalid {
                    message: "Not connected to signaling server".to_string(),
                };
            }
        } else {
            *self.api_key_status.write() = ApiKeyStatus::Invalid {
                message: "Not connected to signaling server".to_string(),
            };
        }
    }

    /// Update the API key status from a server-reported result. Dropped
    /// silently if the user has since moved on to a different key
    fn apply_api_key_validation_result(&self, account_name: Option<String>) {
        let current = self.settings.gw2_api_key.trim().to_string();
        let recorded = self.api_key_status_for.read().clone();
        if recorded.as_deref() != Some(current.as_str()) {
            return;
        }
        *self.api_key_status.write() = match account_name {
            Some(name) if !name.is_empty() => ApiKeyStatus::Valid { account_name: name },
            Some(_) => ApiKeyStatus::Invalid {
                message: "GW2 returned an empty account name".to_string(),
            },
            None => ApiKeyStatus::Invalid {
                message: "Server rejected key or GW2 API unreachable".to_string(),
            },
        };
    }

    /// Leave a single room and update local state. Drops only those peers
    /// whose membership in *this* client's room set becomes empty.
    fn leave_room(&mut self, room_id: &str) {
        if self.joined_rooms.remove(room_id).is_none() {
            // Pre-confirmation join (no RoomJoined yet) — still tell the
            // server in case it accepted but the ack hasn't arrived. No
            // local peer state to clean up.
            if let Some(tx) = &self.network_cmd_tx {
                let _ = tx.send(NetworkCommand::LeaveRoom {
                    room_id: Some(room_id.to_string()),
                });
            }
            return;
        }

        log::info!("Leaving room: {}", room_id);
        if let Some(tx) = &self.network_cmd_tx {
            let _ = tx.send(NetworkCommand::LeaveRoom {
                room_id: Some(room_id.to_string()),
            });
        }

        // Drop this room from every peer's membership. Peers whose set
        // goes empty are dropped entirely; survivors keep their other
        // rooms but lose the (peer, this-room) playback stream.
        let mut to_remove = Vec::new();
        let mut to_drop_stream = Vec::new();
        for (id, peer) in self.peers.iter_mut() {
            if peer.room_ids.remove(room_id) {
                if peer.room_ids.is_empty() {
                    to_remove.push(id.clone());
                } else {
                    to_drop_stream.push(id.clone());
                }
            }
        }
        for id in to_remove {
            self.peers.remove(&id);
            self.send_incoming_audio_command(IncomingAudioCommand::RemovePeer {
                peer_id: id,
            });
        }
        for id in to_drop_stream {
            self.send_incoming_audio_command(IncomingAudioCommand::RemovePeerFromRoom {
                peer_id: id,
                room_id: room_id.to_string(),
            });
        }

        if self.joined_rooms.is_empty() {
            self.state = VoiceState::Connected;
            self.send_incoming_audio_command(IncomingAudioCommand::ResetIncoming);
        }
    }

    /// Process outgoing audio
    fn process_outgoing_audio(&mut self) -> Result<()> {
        // Check if we should transmit
        if self.settings.is_muted || self.state != VoiceState::InRoom {
            return Ok(());
        }

        // Resolve once per tick: PTT-mode + no key held → None → drop;
        // any other mode falls through to VAD gating below.
        let target = match self.resolve_speak_room() {
            Some(r) => r,
            None => return Ok(()),
        };

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
                                if let Some(tx) = &self.network_cmd_tx {
                                    let _ = tx.send(NetworkCommand::SendAudio {
                                        room_id: target.clone(),
                                        data: encoded,
                                    });
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

    /// Resolve which joined room to send the outgoing audio frame into.
    /// Delegates the per-type / fallback-chain logic to `ActiveSpeak`.
    fn resolve_speak_room(&self) -> Option<String> {
        let ptt_required = matches!(self.settings.mode, VoiceMode::PushToTalk);
        // The legacy single-PTT atomic still drives the default key — bridge
        // it into ActiveSpeak so users who haven't bound the new per-type
        // keys still get the fallback chain behavior.
        self.active_speak
            .set_default(self.ptt_active.load(Ordering::Relaxed));
        self.active_speak.resolve(&self.joined_rooms, ptt_required)
    }

    /// Record that `peer_id` is in `room_id` from the local client's
    /// perspective. Adds the peer entry on first sighting (skipping self),
    /// otherwise just unions the room into the existing entry's set.
    fn add_peer_to_room(
        &mut self,
        peer_id: &str,
        player_name: &str,
        account_name: Option<&str>,
        room_id: &str,
    ) -> Result<()> {
        // Don't track ourselves.
        if Some(peer_id.to_string()) == self.our_peer_id {
            return Ok(());
        }

        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.room_ids.insert(room_id.to_string());
            return Ok(());
        }

        self.add_peer(peer_id, player_name, account_name)?;
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.room_ids.insert(room_id.to_string());
        }
        Ok(())
    }

    /// Drop `peer_id`'s membership in `room_id`. Tells the audio thread
    /// to drop just that (peer, room) playback stream; if the peer's last
    /// shared room with us is gone, removes the peer entirely.
    fn remove_peer_from_room(&mut self, peer_id: &str, room_id: &str) {
        let now_empty = match self.peers.get_mut(peer_id) {
            Some(peer) => {
                peer.room_ids.remove(room_id);
                peer.room_ids.is_empty()
            }
            None => return,
        };
        if now_empty {
            self.remove_peer(peer_id);
        } else {
            self.send_incoming_audio_command(IncomingAudioCommand::RemovePeerFromRoom {
                peer_id: peer_id.to_string(),
                room_id: room_id.to_string(),
            });
        }
    }

    /// Add a peer (called when a new player joins the room)
    pub fn add_peer(
        &mut self,
        peer_id: &str,
        player_name: &str,
        account_name: Option<&str>,
    ) -> Result<()> {
        log::info!(
            "add_peer called for {} (name={}, account={:?}), our_peer_id={:?}",
            peer_id, player_name, account_name, self.our_peer_id
        );

        // Don't add ourselves
        if Some(peer_id.to_string()) == self.our_peer_id {
            log::info!("Skipping add_peer for {} because it matches our_peer_id", peer_id);
            return Ok(());
        }

        log::info!("Adding peer: {} ({})", player_name, peer_id);

        // Apply a persistent mute if this account is on the muted list.
        let should_mute = account_name
            .map(|name| self.muted_accounts.contains(name))
            .unwrap_or(false);

        let mut peer = VoicePeer::new(
            peer_id.to_string(),
            player_name.to_string(),
            account_name.map(|s| s.to_string()),
        )?;
        if should_mute {
            peer.is_muted = true;
        }
        self.peers.insert(peer_id.to_string(), peer);
        self.send_incoming_audio_command(IncomingAudioCommand::UpsertPeer {
            peer_id: peer_id.to_string(),
            player_name: player_name.to_string(),
        });
        if should_mute {
            self.send_incoming_audio_command(IncomingAudioCommand::SetPeerMuted {
                peer_id: peer_id.to_string(),
                muted: true,
            });
            log::info!(
                "Auto-muted peer {} ({}): account on persistent mute list",
                peer_id,
                account_name.unwrap_or("?"),
            );
        }

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

    /// Receive audio data for a peer in a specific room. The actual decode
    /// + playback happens on the audio thread (already dispatched by the
    /// network task); this just refreshes the speaking-indicator timer.
    pub fn receive_peer_audio(
        &mut self,
        peer_id: &str,
        room_id: &str,
        opus_data: &[u8],
    ) -> Result<()> {
        if let Some(peer) = self.peers.get(peer_id) {
            peer.mark_audio_received();
        }
        let _ = (room_id, opus_data);
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

    /// Get the last known camera transform (position, front, top), if any.
    pub fn last_camera_transform(&self) -> Option<crate::position::Transform> {
        self.last_camera_transform
    }

    /// Get the last known vertical field of view in radians, if any.
    pub fn last_fov(&self) -> Option<f32> {
        self.last_fov
    }

    /// Get peer info for the UI, including position and distance from the listener.
    pub fn get_peers(&self) -> Vec<NearbyPeer> {
        let listener = self.last_listener_position;
        self.peers
            .values()
            .map(|p| NearbyPeer {
                peer_id: p.peer_id.clone(),
                player_name: p.player_name.clone(),
                account_name: p.account_name.clone(),
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

    /// Mute a specific peer. If the peer has a server-validated account
    /// handle, the mute is persisted to `mutes.json` and re-applied when
    /// the peer reconnects in a future session. Peers without an account
    /// (no API key / validation failed) can still be muted but only for
    /// the current session.
    pub fn mute_peer(&mut self, peer_id: &str, muted: bool) {
        let Some(peer) = self.peers.get_mut(peer_id) else {
            log::warn!("mute_peer: peer not found: {}", peer_id);
            return;
        };
        peer.set_muted(muted);
        let account = peer.account_name.clone();
        self.send_incoming_audio_command(IncomingAudioCommand::SetPeerMuted {
            peer_id: peer_id.to_string(),
            muted,
        });
        log::info!("Set mute={} for peer {}", muted, peer_id);

        match account {
            Some(name) => {
                let changed = if muted {
                    self.muted_accounts.insert(name.clone())
                } else {
                    self.muted_accounts.remove(&name)
                };
                if changed {
                    persist::save_muted_accounts(&self.muted_accounts);
                    log::info!(
                        "Persisted mute set update: {} -> muted={}",
                        name, muted
                    );
                }
            }
            None => {
                log::debug!(
                    "Peer {} has no account handle; mute is session-only",
                    peer_id
                );
            }
        }
    }

    /// Unmute a specific peer (convenience wrapper)
    pub fn unmute_peer(&mut self, peer_id: &str) {
        self.mute_peer(peer_id, false);
    }

    /// Toggle mute state for a peer and return the new state if present
    pub fn toggle_mute_peer(&mut self, peer_id: &str) -> Option<bool> {
        let new_state = {
            let peer = self.peers.get(peer_id)?;
            !peer.is_muted
        };
        self.mute_peer(peer_id, new_state);
        Some(new_state)
    }

    /// Snapshot of currently persisted muted account handles.
    pub fn muted_accounts(&self) -> &HashSet<String> {
        &self.muted_accounts
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
        self.send_incoming_audio_command(IncomingAudioCommand::SetRoomTypeVolumes(
            self.settings.room_type_volumes,
        ));
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
        self.joined_rooms.clear();
        self.current_map_room = None;
        self.state = VoiceState::Disconnected;
    }

    /// Check if shutdown requested
    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Clone the active-speak handle so the keybind dispatcher in
    /// `lib.rs` can update per-type held flags from the input thread
    /// without holding the manager's write lock.
    pub fn active_speak_handle(&self) -> ActiveSpeak {
        self.active_speak.clone()
    }

    /// All rooms the local client is currently joined to (room_id only).
    pub fn joined_room_ids(&self) -> Vec<String> {
        self.joined_rooms.keys().cloned().collect()
    }

    /// Manual join from the settings UI. Validates the prefix scheme
    /// locally so an unknown room type is rejected before round-tripping
    /// to the server. The actual roster comes back via `RoomJoined`.
    pub fn join_room_manual(&mut self, room_id: &str) -> Result<()> {
        let id = room_id.trim();
        if id.is_empty() {
            anyhow::bail!("Room id is empty");
        }
        if RoomType::from_room_id(id).is_none() {
            anyhow::bail!(
                "Room id must start with map:, squad:, or party:"
            );
        }
        if self.joined_rooms.contains_key(id) {
            return Ok(());
        }
        let player_name = self
            .mumble_link
            .read()
            .and_then(|s| s.identity.as_ref().map(|i| i.name.clone()))
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Unknown".to_string());
        self.join_room(id, &player_name)
    }

    /// Manual leave from the settings UI.
    pub fn leave_room_manual(&mut self, room_id: &str) {
        if self.current_map_room.as_deref() == Some(room_id) {
            self.current_map_room = None;
        }
        self.leave_room(room_id);
    }

    /// Per-room peer counts for the "Active rooms" UI section.
    pub fn rooms_with_peer_counts(&self) -> Vec<(String, usize)> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for room_id in self.joined_rooms.keys() {
            counts.insert(room_id.clone(), 0);
        }
        for peer in self.peers.values() {
            for room_id in &peer.room_ids {
                if let Some(c) = counts.get_mut(room_id) {
                    *c += 1;
                }
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
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
                    NetworkCommand::JoinRoom { room_id, player_name, api_key } => {
                        if let Err(e) = signaling_client.join_room(
                            &room_id,
                            &player_name,
                            api_key.as_deref(),
                        ) {
                            log::error!("Failed to join room: {}", e);
                        }
                    }
                    NetworkCommand::ValidateApiKey { api_key } => {
                        if let Err(e) = signaling_client.validate_api_key(&api_key) {
                            log::error!("Failed to send ValidateApiKey: {}", e);
                        }
                    }
                    NetworkCommand::LeaveRoom { room_id } => {
                        if let Err(e) = signaling_client.leave_room(room_id.as_deref()) {
                            log::error!("Failed to leave room: {}", e);
                        }
                    }
                    NetworkCommand::UpdatePosition { position, front } => {
                        let _ = signaling_client.update_position(position, front);
                    }
                    NetworkCommand::SendAudio { room_id, data } => {
                        let _ = signaling_client.send_audio(&room_id, &data);
                    }
                    NetworkCommand::IdentifyGroup { members } => {
                        if let Err(e) = signaling_client.identify_group(members) {
                            log::warn!("Failed to send IdentifyGroup: {}", e);
                        }
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
                    Some(ServerMessage::AccountValidated { account_name }) => {
                        let _ = event_tx.send(NetworkEvent::AccountValidated { account_name });
                    }
                    Some(ServerMessage::JoinRejected { room_id, reason }) => {
                        let _ = event_tx.send(NetworkEvent::JoinRejected { room_id, reason });
                    }
                    Some(ServerMessage::RoomJoined { room_id, peers }) => {
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
                    Some(ServerMessage::PeerJoined { room_id, peer }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::UpsertPeer {
                            peer_id: peer.peer_id.clone(),
                            player_name: peer.player_name.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::PeerJoined {
                            room_id,
                            peer_id: peer.peer_id,
                            player_name: peer.player_name,
                            account_name: peer.account_name,
                        });
                    }
                    Some(ServerMessage::PeerLeft { room_id, peer_id }) => {
                        // Phase 2: audio thread is peer-keyed, so we can't
                        // drop "(peer, this room)" alone — let the manager
                        // decide when the peer's last shared room ended and
                        // emit RemovePeer then.
                        let _ = event_tx.send(NetworkEvent::PeerLeft { room_id, peer_id });
                    }
                    Some(ServerMessage::PeerPosition { peer_id, position, front }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::SetPeerPosition {
                            peer_id: peer_id.clone(),
                            position,
                            front,
                        });
                        let _ = event_tx.send(NetworkEvent::PeerPosition { peer_id, position, front });
                    }
                    Some(ServerMessage::PeerAudio { room_id, peer_id, data }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::PushPeerOpus {
                            peer_id: peer_id.clone(),
                            room_id: room_id.clone(),
                            data: data.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::AudioReceived {
                            room_id,
                            peer_id,
                            data,
                        });
                    }
                    Some(ServerMessage::Error { message }) => {
                        log::error!("Server error: {}", message);
                        let _ = event_tx.send(NetworkEvent::Error { message });
                    }
                    Some(ServerMessage::Kicked { reason }) => {
                        log::warn!("Server kicked us: {}", reason);
                        let _ = event_tx.send(NetworkEvent::Error {
                            message: format!("Disconnected by server: {}", reason),
                        });
                    }
                    Some(ServerMessage::GroupIdentified { cluster_id }) => {
                        let _ = event_tx.send(NetworkEvent::GroupIdentified { cluster_id });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-seeding `muted_accounts` and then adding a peer whose account
    /// matches should leave the peer muted immediately — no fresh UI
    /// interaction required. This is the cross-session behaviour users rely
    /// on after reloading the addon.
    #[test]
    fn auto_mute_on_join_by_account() {
        let mut muted = HashSet::new();
        muted.insert("Jerk.1234".to_string());

        let settings = VoiceSettings::default();
        let mut vm = VoiceManager::with_persistence(DEFAULT_SERVER_URL, settings, muted);

        vm.add_peer("peer-id-a", "Character A", Some("Jerk.1234"))
            .expect("add_peer");
        vm.add_peer("peer-id-b", "Character B", Some("Nice.9999"))
            .expect("add_peer");
        vm.add_peer("peer-id-c", "Character C", None)
            .expect("add_peer");

        let peers = vm.get_peers();
        let by_id: HashMap<String, bool> = peers
            .into_iter()
            .map(|p| (p.peer_id, p.is_muted))
            .collect();
        assert_eq!(by_id.get("peer-id-a"), Some(&true), "matched account auto-mutes");
        assert_eq!(by_id.get("peer-id-b"), Some(&false), "unmatched account stays unmuted");
        assert_eq!(by_id.get("peer-id-c"), Some(&false), "no account stays unmuted");
    }

    /// `NearbyPeer.account_name` should carry the handle through to the UI.
    #[test]
    fn nearby_peer_surfaces_account_name() {
        let mut vm = VoiceManager::new(DEFAULT_SERVER_URL);
        vm.add_peer("peer-id", "Char Name", Some("Acc.4242"))
            .expect("add_peer");
        let peers = vm.get_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].account_name.as_deref(), Some("Acc.4242"));
    }
}
