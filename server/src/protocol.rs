//! Wire types and audio-frame codec for the WebSocket protocol.
//!
//! The protocol is JSON for control messages (tagged enum), and a small
//! binary frame for Opus audio. Splitting these out from the session
//! handler lets us test the frame layout independently of socket I/O.

use serde::{Deserialize, Serialize};

use crate::rooms::Position;

/// Leading byte on binary frames identifying them as audio. Reserves room
/// for future binary frame kinds without breaking existing parsers.
pub const AUDIO_FRAME_KIND: u8 = 1;

/// Messages from client to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    JoinRoom {
        room_id: String,
        player_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    ValidateApiKey {
        api_key: String,
    },
    LeaveRoom,
    UpdatePosition {
        position: Position,
        front: Position,
    },
    Ping,
}

/// Messages from server to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        peer_id: String,
    },
    AccountValidated {
        #[serde(default)]
        account_name: Option<String>,
    },
    RoomJoined {
        room_id: String,
        peers: Vec<PeerInfo>,
    },
    JoinRejected {
        reason: String,
    },
    PeerJoined {
        peer: PeerInfo,
    },
    PeerLeft {
        peer_id: String,
    },
    PeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    Error {
        message: String,
    },
    Pong,
}

/// Peer info for room listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub player_name: String,
    #[serde(default)]
    pub account_name: Option<String>,
    pub position: Option<Position>,
    pub front: Option<Position>,
}

impl From<crate::rooms::PeerInfo> for PeerInfo {
    fn from(p: crate::rooms::PeerInfo) -> Self {
        Self {
            peer_id: p.peer_id,
            player_name: p.player_name,
            account_name: p.account_name,
            position: p.position,
            front: p.front,
        }
    }
}

pub(crate) fn decode_client_audio_frame(data: &[u8]) -> Option<&[u8]> {
    let (&kind, payload) = data.split_first()?;
    if kind != AUDIO_FRAME_KIND || payload.is_empty() {
        return None;
    }
    Some(payload)
}

pub(crate) fn encode_server_audio_frame(peer_id: &str, audio: &[u8]) -> Vec<u8> {
    let peer_id_bytes = peer_id.as_bytes();
    let peer_id_len = u16::try_from(peer_id_bytes.len()).unwrap_or(u16::MAX);
    let peer_id_len_usize = peer_id_len as usize;

    let mut frame = Vec::with_capacity(1 + 2 + peer_id_len_usize + audio.len());
    frame.push(AUDIO_FRAME_KIND);
    frame.extend_from_slice(&peer_id_len.to_le_bytes());
    frame.extend_from_slice(&peer_id_bytes[..peer_id_len_usize]);
    frame.extend_from_slice(audio);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_round_trip_through_server_format() {
        // Client sends [kind | opus]; server prepends sender peer_id.
        let opus = b"opus-bytes";
        let client_frame = {
            let mut v = Vec::with_capacity(1 + opus.len());
            v.push(AUDIO_FRAME_KIND);
            v.extend_from_slice(opus);
            v
        };
        let decoded = decode_client_audio_frame(&client_frame).expect("client frame parses");
        assert_eq!(decoded, opus);

        let server_frame = encode_server_audio_frame("peer-abc", decoded);
        // Layout: 0x01 | u16le len=8 | "peer-abc" | opus
        assert_eq!(server_frame[0], AUDIO_FRAME_KIND);
        assert_eq!(u16::from_le_bytes([server_frame[1], server_frame[2]]), 8);
        assert_eq!(&server_frame[3..11], b"peer-abc");
        assert_eq!(&server_frame[11..], opus);
    }

    #[test]
    fn decode_rejects_wrong_kind() {
        assert!(decode_client_audio_frame(&[0xff, 1, 2, 3]).is_none());
    }

    #[test]
    fn decode_rejects_empty_payload() {
        assert!(decode_client_audio_frame(&[AUDIO_FRAME_KIND]).is_none());
    }
}
