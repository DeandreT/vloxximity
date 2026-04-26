//! Wire types and audio-frame codec shared with the server.

use serde::{Deserialize, Serialize};

use crate::position::Position;

/// Leading byte on binary frames identifying them as audio.
pub const AUDIO_FRAME_KIND: u8 = 1;

/// Messages sent from client to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Join a room with instance hash
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
    /// Leave current room
    LeaveRoom,
    /// Update player position
    UpdatePosition {
        position: Position,
        front: Position,
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
    JoinRejected {
        reason: String,
    },
    /// A peer joined the room
    PeerJoined {
        peer: PeerInfo,
    },
    /// A peer left the room
    PeerLeft {
        peer_id: String,
    },
    /// Peer position update
    PeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    /// Audio data from a peer
    PeerAudio {
        peer_id: String,
        data: Vec<u8>,
    },
    /// Error message
    Error {
        message: String,
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

pub(crate) fn encode_client_audio_frame(audio: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + audio.len());
    frame.push(AUDIO_FRAME_KIND);
    frame.extend_from_slice(audio);
    frame
}

pub(crate) fn decode_server_audio_frame(data: &[u8]) -> Option<(String, Vec<u8>)> {
    let (&kind, rest) = data.split_first()?;
    if kind != AUDIO_FRAME_KIND || rest.len() < 2 {
        return None;
    }

    let peer_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
    if rest.len() < 2 + peer_len {
        return None;
    }

    let peer_id = std::str::from_utf8(&rest[2..2 + peer_len]).ok()?.to_string();
    let audio = rest[2 + peer_len..].to_vec();
    Some((peer_id, audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_codec_round_trip() {
        let opus = b"opus-audio-bytes";
        let client_frame = encode_client_audio_frame(opus);
        // Wrap in server format and decode.
        let mut server_frame = Vec::new();
        server_frame.push(AUDIO_FRAME_KIND);
        let pid = "peer-xyz";
        server_frame.extend_from_slice(&(pid.len() as u16).to_le_bytes());
        server_frame.extend_from_slice(pid.as_bytes());
        server_frame.extend_from_slice(&client_frame[1..]); // strip leading kind byte
        let (peer, audio) = decode_server_audio_frame(&server_frame).expect("parses");
        assert_eq!(peer, pid);
        assert_eq!(audio, opus);
    }

    #[test]
    fn decode_rejects_truncated_frame() {
        // Just kind + length bytes, no peer_id payload.
        let bad = vec![AUDIO_FRAME_KIND, 8, 0];
        assert!(decode_server_audio_frame(&bad).is_none());
    }
}
