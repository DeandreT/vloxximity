//! Wire types and audio-frame codec shared with the server.

use serde::{Deserialize, Serialize};

use crate::position::Position;

/// Leading byte on binary frames identifying them as audio.
pub const AUDIO_FRAME_KIND: u8 = 1;

/// Messages sent from client to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Join a room. *Additive* — a peer can be in many rooms at once;
    /// re-joining a room the peer is already in is a no-op.
    JoinRoom {
        room_id: String,
        player_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    /// Ask the server to validate a GW2 API key without joining a room.
    /// Server replies with `AccountValidated`.
    ValidateApiKey {
        api_key: String,
    },
    /// Leave a single room (when `room_id` is `Some`) or every room the peer
    /// is in (when `None`, used at logout / re-auth / disconnect).
    LeaveRoom {
        #[serde(default)]
        room_id: Option<String>,
    },
    /// Update player position. Position is peer-global (world coordinates),
    /// not per-room.
    UpdatePosition {
        position: Position,
        front: Position,
    },
    /// Tell the server which GW2 group we're in (account names of every
    /// member, including ourselves). The server clusters overlapping
    /// reports and replies with a stable `cluster_id` we can use as a
    /// `squad:` or `party:` room id.
    IdentifyGroup {
        members: Vec<String>,
    },
    /// Heartbeat/keepalive
    Ping,
}

/// Messages received from server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Connection established, assigned peer ID
    Welcome {
        peer_id: String,
    },
    /// Server-validated GW2 account handle for the local peer. `None`
    /// when the user didn't supply an API key or validation failed.
    /// Sent once per JoinRoom, just before the matching `RoomJoined`.
    AccountValidated {
        #[serde(default)]
        account_name: Option<String>,
    },
    /// Successfully joined room
    RoomJoined {
        room_id: String,
        peers: Vec<PeerInfo>,
    },
    /// JoinRoom rejected by the server (missing/invalid API key, etc.).
    /// Carries `room_id` so the client knows which join failed when several
    /// are in flight.
    JoinRejected {
        room_id: String,
        reason: String,
    },
    /// A peer joined a specific room
    PeerJoined {
        room_id: String,
        peer: PeerInfo,
    },
    /// A peer left a specific room (the peer may still be in other rooms
    /// the local client shares with them).
    PeerLeft {
        room_id: String,
        peer_id: String,
    },
    /// Peer position update — peer-global, not per-room.
    PeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    /// Audio data from a peer, tagged with the room it was sent to.
    PeerAudio {
        room_id: String,
        peer_id: String,
        data: Vec<u8>,
    },
    /// Error message
    Error {
        message: String,
    },
    /// Server is closing this connection. Reasons include the dead-connection
    /// sweeper firing on an idle session.
    Kicked {
        reason: String,
    },
    /// Reply to `IdentifyGroup`. The client wraps `cluster_id` as a
    /// `squad:` or `party:` room id based on its local RTAPI group shape.
    GroupIdentified {
        cluster_id: String,
    },
    /// Heartbeat response
    Pong,
}

/// Information about a peer in the room
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub player_name: String,
    /// GW2 account handle as validated by the server (e.g. `Example.1234`).
    /// `None` when the peer joined without an API key or validation failed.
    #[serde(default)]
    pub account_name: Option<String>,
    pub position: Option<Position>,
    pub front: Option<Position>,
}

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

/// Client → server audio frame: `[kind | u16 room_id_len | room_id | opus]`.
pub(crate) fn encode_client_audio_frame(room_id: &str, audio: &[u8]) -> Vec<u8> {
    let room_bytes = room_id.as_bytes();
    let room_len = u16::try_from(room_bytes.len()).unwrap_or(u16::MAX);
    let room_len_usize = room_len as usize;

    let mut frame = Vec::with_capacity(1 + 2 + room_len_usize + audio.len());
    frame.push(AUDIO_FRAME_KIND);
    frame.extend_from_slice(&room_len.to_le_bytes());
    frame.extend_from_slice(&room_bytes[..room_len_usize]);
    frame.extend_from_slice(audio);
    frame
}

/// Server → client audio frame:
/// `[kind | u16 peer_id_len | peer_id | u16 room_id_len | room_id | opus]`.
pub(crate) fn decode_server_audio_frame(data: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let (&kind, rest) = data.split_first()?;
    if kind != AUDIO_FRAME_KIND || rest.len() < 2 {
        return None;
    }

    let peer_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
    let rest = rest.get(2..)?;
    if rest.len() < peer_len {
        return None;
    }
    let peer_id = std::str::from_utf8(&rest[..peer_len]).ok()?.to_string();
    let rest = &rest[peer_len..];

    if rest.len() < 2 {
        return None;
    }
    let room_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
    let rest = &rest[2..];
    if rest.len() < room_len {
        return None;
    }
    let room_id = std::str::from_utf8(&rest[..room_len]).ok()?.to_string();
    let audio = rest[room_len..].to_vec();
    if audio.is_empty() {
        return None;
    }
    Some((peer_id, room_id, audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_codec_round_trip() {
        let opus = b"opus-audio-bytes";
        let room_id = "map:abc:1";
        let client_frame = encode_client_audio_frame(room_id, opus);
        // Re-frame into server format and decode.
        let pid = "peer-xyz";
        let mut server_frame = Vec::new();
        server_frame.push(AUDIO_FRAME_KIND);
        server_frame.extend_from_slice(&(pid.len() as u16).to_le_bytes());
        server_frame.extend_from_slice(pid.as_bytes());
        // Append the client frame body (everything after the kind byte).
        server_frame.extend_from_slice(&client_frame[1..]);
        let (peer, room, audio) = decode_server_audio_frame(&server_frame).expect("parses");
        assert_eq!(peer, pid);
        assert_eq!(room, room_id);
        assert_eq!(audio, opus);
    }

    #[test]
    fn decode_rejects_truncated_peer_id() {
        // kind | peer_len=8 | only 3 bytes follow.
        let bad = vec![AUDIO_FRAME_KIND, 8, 0, b'a', b'b', b'c'];
        assert!(decode_server_audio_frame(&bad).is_none());
    }

    #[test]
    fn decode_rejects_missing_room_id_len() {
        // kind | peer_len=3 | "abc" — no room_len follows.
        let bad = vec![AUDIO_FRAME_KIND, 3, 0, b'a', b'b', b'c'];
        assert!(decode_server_audio_frame(&bad).is_none());
    }

    #[test]
    fn decode_rejects_empty_audio() {
        // Valid header, room id present, no opus payload.
        let pid = "p";
        let room = "r";
        let mut frame = vec![AUDIO_FRAME_KIND];
        frame.extend_from_slice(&(pid.len() as u16).to_le_bytes());
        frame.extend_from_slice(pid.as_bytes());
        frame.extend_from_slice(&(room.len() as u16).to_le_bytes());
        frame.extend_from_slice(room.as_bytes());
        assert!(decode_server_audio_frame(&frame).is_none());
    }
}
