//! Server-side synthetic test peers for solo diagnostic use.
//!
//! Enabled by the `--testpeer[=mode]` CLI flag. A supervisor task watches the
//! room set; whenever a room contains at least one real peer it attaches one
//! or more synthetic peers that emit Opus-encoded tones so positional audio
//! can be exercised end-to-end without a second real client.
//!
//! Modes:
//!   * `orbit` (default) — single peer orbiting a real peer at 500 units over
//!     30s, 440 Hz tone. Good for verifying panning.
//!   * `grid` — five stationary peers at 100/500/1000/2500/5000 units along
//!     world +Z from the real peer, with tones 220/330/440/660/880 Hz. Good
//!     for verifying the distance-attenuation curve at a glance.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mousiki::{Application, Channels, Encoder};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};

use crate::rooms::{Position, RoomManager};

const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_FRAME_SIZE: usize = 960; // 20 ms at 48 kHz
const AUDIO_FRAME_INTERVAL: Duration = Duration::from_millis(20);
const POSITION_INTERVAL: Duration = Duration::from_millis(100);
const SUPERVISOR_POLL: Duration = Duration::from_millis(500);
const OPUS_MAX_PACKET: usize = 4000;
const EVENT_CHANNEL_CAPACITY: usize = 256;
const TONE_AMPLITUDE: f32 = 0.1;
const DITHER_AMPLITUDE: f32 = 0.003;

const ORBIT_PERIOD: Duration = Duration::from_secs(30);
const ORBIT_RADIUS: f32 = 500.0;
const ORBIT_FREQ_HZ: f32 = 440.0;
const ORBIT_NAME: &str = "TestPeer";

const GRID_DISTANCES: &[f32] = &[100.0, 500.0, 1000.0, 2500.0, 5000.0];
const GRID_FREQS: &[f32] = &[220.0, 330.0, 440.0, 660.0, 880.0];

#[derive(Debug, Clone, Copy)]
pub enum TestPeerMode {
    Orbit,
    Grid,
}

impl TestPeerMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "orbit" => Some(Self::Orbit),
            "grid" => Some(Self::Grid),
            _ => None,
        }
    }
}

struct PeerSpec {
    name: String,
    frequency: f32,
    motion: Motion,
}

#[derive(Clone, Copy)]
enum Motion {
    Orbit { radius: f32, period: Duration },
    Anchored { distance: f32 },
}

struct ActivePeer {
    peer_id: String,
    stop_tx: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

/// Spawn the test-peer supervisor onto the current tokio runtime.
pub fn spawn_supervisor(rooms: Arc<RoomManager>, mode: TestPeerMode) {
    tokio::spawn(supervisor(rooms, mode));
}

fn specs_for(mode: TestPeerMode) -> Vec<PeerSpec> {
    match mode {
        TestPeerMode::Orbit => vec![PeerSpec {
            name: ORBIT_NAME.to_string(),
            frequency: ORBIT_FREQ_HZ,
            motion: Motion::Orbit {
                radius: ORBIT_RADIUS,
                period: ORBIT_PERIOD,
            },
        }],
        TestPeerMode::Grid => GRID_DISTANCES
            .iter()
            .zip(GRID_FREQS.iter())
            .map(|(&distance, &frequency)| PeerSpec {
                name: format!("TestPeer@{}", distance as u32),
                frequency,
                motion: Motion::Anchored { distance },
            })
            .collect(),
    }
}

async fn supervisor(rooms: Arc<RoomManager>, mode: TestPeerMode) {
    tracing::info!("Test peer supervisor started (mode={:?})", mode);
    let mut active: HashMap<String, Vec<ActivePeer>> = HashMap::new();
    let mut ticker = time::interval(SUPERVISOR_POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut ticks_since_log: u32 = 0;
    loop {
        ticker.tick().await;
        let snapshots = rooms.rooms_with_peers();
        ticks_since_log += 1;
        if ticks_since_log >= 4 {
            ticks_since_log = 0;
            tracing::info!(
                "Test peer supervisor tick: {} rooms, active_rooms={}",
                snapshots.len(),
                active.len()
            );
            for (room_id, peer_ids) in &snapshots {
                tracing::info!("  room {} has {} peers", room_id, peer_ids.len());
            }
        }

        for (room_id, peer_ids) in &snapshots {
            if active.contains_key(room_id) {
                continue;
            }
            if peer_ids.is_empty() {
                continue;
            }
            let specs = specs_for(mode);
            tracing::info!(
                "Test peer supervisor: spawning {} peer(s) for room {} ({} real peers)",
                specs.len(),
                room_id,
                peer_ids.len()
            );
            let spawned: Vec<ActivePeer> = specs
                .into_iter()
                .filter_map(|spec| start_peer(rooms.clone(), room_id.clone(), spec))
                .collect();
            if !spawned.is_empty() {
                active.insert(room_id.clone(), spawned);
            }
        }

        let to_remove: Vec<String> = active
            .iter()
            .filter(|(room_id, peers)| {
                match snapshots.iter().find(|(r, _)| r == *room_id) {
                    None => true,
                    Some((_, occupants)) => {
                        let fake_ids: Vec<&str> =
                            peers.iter().map(|p| p.peer_id.as_str()).collect();
                        occupants
                            .iter()
                            .all(|occ| fake_ids.contains(&occ.as_str()))
                    }
                }
            })
            .map(|(r, _)| r.clone())
            .collect();

        for room_id in to_remove {
            if let Some(peers) = active.remove(&room_id) {
                for mut peer in peers {
                    if let Some(tx) = peer.stop_tx.take() {
                        let _ = tx.send(());
                    }
                    let _ = peer.handle.await;
                }
            }
        }
    }
}

fn start_peer(rooms: Arc<RoomManager>, room_id: String, spec: PeerSpec) -> Option<ActivePeer> {
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
    let registered = rooms.register_peer(event_tx);
    let peer_id = registered.peer_id.clone();

    let (stop_tx, stop_rx) = oneshot::channel();
    let peer_id_task = peer_id.clone();
    let room_id_task = room_id.clone();
    let handle = tokio::spawn(async move {
        run_peer(rooms, room_id_task, peer_id_task, spec, stop_rx).await;
    });

    Some(ActivePeer {
        peer_id,
        stop_tx: Some(stop_tx),
        handle,
    })
}

async fn run_peer(
    rooms: Arc<RoomManager>,
    room_id: String,
    peer_id: String,
    spec: PeerSpec,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut encoder = match Encoder::new(AUDIO_SAMPLE_RATE, Channels::Mono, Application::Voip) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!("Test peer encoder init failed: {:?}", err);
            rooms.unregister_peer(&peer_id);
            return;
        }
    };

    let mut target: Option<String> = rooms.first_other_peer_in(&room_id, &peer_id);
    let start = Instant::now();
    let mut last_position_update = Instant::now()
        .checked_sub(POSITION_INTERVAL)
        .unwrap_or_else(Instant::now);
    // For Anchored peers: snapshot the target's position on first sighting and
    // keep broadcasting that fixed location so grid peers stay put when the
    // player moves.
    let mut anchor: Option<Position> = None;
    // Room membership is deferred until we can compute a valid spawn position,
    // because `PeerJoined` broadcasts don't carry position and any peer that
    // sees us join before we've stamped a position would cache us at (0,0,0).
    let mut joined = false;

    let phase_increment = spec.frequency * std::f32::consts::TAU / AUDIO_SAMPLE_RATE as f32;
    let mut phase: f32 = 0.0;
    let mut pcm = vec![0i16; AUDIO_FRAME_SIZE];
    // Simple LCG for dither. SILK's pitch/predictor math panics on perfectly
    // periodic signals (div-by-zero in stereo_find_predictor), so we mix a tiny
    // amount of noise under the tone to break the degeneracy.
    let mut rng_state: u32 = 0x1234_5678;

    let mut audio_ticker = time::interval(AUDIO_FRAME_INTERVAL);
    audio_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = audio_ticker.tick() => {}
        }

        // Test peers have no inbound socket; keep their liveness fresh so
        // the sweeper doesn't flag them as idle.
        rooms.touch_peer(&peer_id);

        let target_pos_opt: Option<Position> = target
            .as_deref()
            .and_then(|id| rooms.get_peer_snapshot(id))
            .filter(|s| s.room_ids.contains(room_id.as_str()))
            .map(|s| s.position)
            .or_else(|| {
                target = rooms.first_other_peer_in(&room_id, &peer_id);
                target
                    .as_deref()
                    .and_then(|id| rooms.get_peer_snapshot(id))
                    .filter(|s| s.room_ids.contains(room_id.as_str()))
                    .map(|s| s.position)
            });

        let target_valid = target_pos_opt
            .filter(|p| p.x != 0.0 || p.y != 0.0 || p.z != 0.0);

        let computed: Option<(Position, Position)> = match spec.motion {
            Motion::Orbit { radius, period } => target_valid.map(|center| {
                let t = start.elapsed().as_secs_f32();
                let angle = (t / period.as_secs_f32()) * std::f32::consts::TAU;
                let pos = Position::new(
                    center.x + angle.cos() * radius,
                    center.y,
                    center.z + angle.sin() * radius,
                );
                let dx = center.x - pos.x;
                let dz = center.z - pos.z;
                let len = (dx * dx + dz * dz).sqrt().max(1e-6);
                let front = Position::new(dx / len, 0.0, dz / len);
                (pos, front)
            }),
            Motion::Anchored { distance } => {
                if anchor.is_none() {
                    anchor = target_valid;
                }
                anchor.map(|base| {
                    let pos = Position::new(base.x, base.y, base.z + distance);
                    let front = Position::new(0.0, 0.0, -1.0);
                    (pos, front)
                })
            }
        };

        // Lazy room join: only once we have a position to publish.
        if !joined {
            let Some((pos, front)) = computed else { continue };
            // Stamp position before joining so it's on record the moment the
            // room broadcasts PeerJoined.
            rooms.update_position(&peer_id, pos, front);
            if rooms.join_room(&peer_id, &room_id, &spec.name).is_none() {
                tracing::warn!("Test peer failed to join room {}", room_id);
                rooms.unregister_peer(&peer_id);
                return;
            }
            tracing::info!(
                "Test peer {} ({}) joined room {} at ({:.1}, {:.1}, {:.1})",
                peer_id,
                spec.name,
                room_id,
                pos.x,
                pos.y,
                pos.z
            );
            joined = true;
            last_position_update = Instant::now();
            // Still need to emit an explicit PeerPosition so existing peers
            // that already listed us via RoomJoined get a fresh broadcast.
            rooms.update_position(&peer_id, pos, front);
        } else if last_position_update.elapsed() >= POSITION_INTERVAL {
            if let Some((pos, front)) = computed {
                rooms.update_position(&peer_id, pos, front);
                last_position_update = Instant::now();
            }
        }

        for sample in pcm.iter_mut() {
            rng_state = rng_state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let dither = ((rng_state >> 16) as i16 as f32) / i16::MAX as f32 * DITHER_AMPLITUDE;
            let s = (phase.sin() * TONE_AMPLITUDE) + dither;
            *sample = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            phase += phase_increment;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
        }

        match encoder.encode_vec(&pcm, OPUS_MAX_PACKET) {
            Ok(packet) => rooms.broadcast_audio(&peer_id, &room_id, packet),
            Err(err) => tracing::warn!("Test peer encode error: {:?}", err),
        }
    }

    tracing::info!("Test peer {} leaving room {}", peer_id, room_id);
    if joined {
        rooms.leave_room(&peer_id, &room_id);
    }
    rooms.unregister_peer(&peer_id);
}
