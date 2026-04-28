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
    /// Additive: a peer can be in multiple rooms at once. Re-joining a room
    /// the peer is already in is a no-op.
    JoinRoom {
        room_id: String,
        player_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    ValidateApiKey {
        api_key: String,
    },
    /// Leave a single room when `room_id` is `Some`; leave every room the
    /// peer is in when `None` (used by the client at logout / re-auth).
    LeaveRoom {
        #[serde(default)]
        room_id: Option<String>,
    },
    UpdatePosition {
        position: Position,
        front: Position,
    },
    /// Report the local player's GW2 group membership (account names of
    /// every member, including the sender). The server clusters
    /// overlapping reports and replies with a stable `cluster_id` that
    /// every member of the same group converges on, even across commander
    /// churn or late joiners. See `squad.rs`.
    IdentifyGroup {
        members: Vec<String>,
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
        room_id: String,
        reason: String,
    },
    PeerJoined {
        room_id: String,
        peer: PeerInfo,
    },
    PeerLeft {
        room_id: String,
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
    Kicked {
        reason: String,
    },
    /// Reply to `IdentifyGroup`. The client wraps `cluster_id` as a
    /// `squad:` or `party:` room id based on its local RTAPI group shape.
    GroupIdentified {
        cluster_id: String,
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

/// Client → server audio frame: `[kind | u16 room_id_len | room_id_utf8 | opus]`.
pub(crate) fn decode_client_audio_frame(data: &[u8]) -> Option<(&str, &[u8])> {
    let (&kind, rest) = data.split_first()?;
    if kind != AUDIO_FRAME_KIND {
        return None;
    }
    if rest.len() < 2 {
        return None;
    }
    let (len_bytes, rest) = rest.split_at(2);
    let room_id_len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
    if rest.len() < room_id_len {
        return None;
    }
    let (room_id_bytes, opus) = rest.split_at(room_id_len);
    if opus.is_empty() {
        return None;
    }
    let room_id = std::str::from_utf8(room_id_bytes).ok()?;
    Some((room_id, opus))
}

/// Server → client audio frame:
/// `[kind | u16 peer_id_len | peer_id_utf8 | u16 room_id_len | room_id_utf8 | opus]`.
pub(crate) fn encode_server_audio_frame(peer_id: &str, room_id: &str, audio: &[u8]) -> Vec<u8> {
    let peer_id_bytes = peer_id.as_bytes();
    let peer_id_len = u16::try_from(peer_id_bytes.len()).unwrap_or(u16::MAX);
    let peer_id_len_usize = peer_id_len as usize;

    let room_id_bytes = room_id.as_bytes();
    let room_id_len = u16::try_from(room_id_bytes.len()).unwrap_or(u16::MAX);
    let room_id_len_usize = room_id_len as usize;

    let mut frame =
        Vec::with_capacity(1 + 2 + peer_id_len_usize + 2 + room_id_len_usize + audio.len());
    frame.push(AUDIO_FRAME_KIND);
    frame.extend_from_slice(&peer_id_len.to_le_bytes());
    frame.extend_from_slice(&peer_id_bytes[..peer_id_len_usize]);
    frame.extend_from_slice(&room_id_len.to_le_bytes());
    frame.extend_from_slice(&room_id_bytes[..room_id_len_usize]);
    frame.extend_from_slice(audio);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_round_trip_through_server_format() {
        // Client sends [kind | u16 room_id_len | room_id | opus]; server
        // re-emits with [kind | u16 peer_id_len | peer_id | u16 room_id_len | room_id | opus].
        let opus = b"opus-bytes";
        let room_id = "map:abcd:1";
        let client_frame = {
            let room_bytes = room_id.as_bytes();
            let room_len = room_bytes.len() as u16;
            let mut v = Vec::with_capacity(1 + 2 + room_bytes.len() + opus.len());
            v.push(AUDIO_FRAME_KIND);
            v.extend_from_slice(&room_len.to_le_bytes());
            v.extend_from_slice(room_bytes);
            v.extend_from_slice(opus);
            v
        };
        let (decoded_room, decoded_opus) =
            decode_client_audio_frame(&client_frame).expect("client frame parses");
        assert_eq!(decoded_room, room_id);
        assert_eq!(decoded_opus, opus);

        let server_frame = encode_server_audio_frame("peer-abc", decoded_room, decoded_opus);
        // Layout: 0x01 | u16le 8 | "peer-abc" | u16le 10 | "map:abcd:1" | opus
        assert_eq!(server_frame[0], AUDIO_FRAME_KIND);
        assert_eq!(u16::from_le_bytes([server_frame[1], server_frame[2]]), 8);
        assert_eq!(&server_frame[3..11], b"peer-abc");
        assert_eq!(
            u16::from_le_bytes([server_frame[11], server_frame[12]]),
            room_id.len() as u16
        );
        assert_eq!(&server_frame[13..13 + room_id.len()], room_id.as_bytes());
        assert_eq!(&server_frame[13 + room_id.len()..], opus);
    }

    #[test]
    fn decode_rejects_wrong_kind() {
        assert!(decode_client_audio_frame(&[0xff, 1, 0, b'a', 1, 2, 3]).is_none());
    }

    #[test]
    fn decode_rejects_empty_opus() {
        // kind | len=1 | "a"   — no opus payload after room_id
        let frame = [AUDIO_FRAME_KIND, 1, 0, b'a'];
        assert!(decode_client_audio_frame(&frame).is_none());
    }

    #[test]
    fn decode_rejects_truncated_room_id() {
        // kind | len=5 | only 2 bytes follow
        let frame = [AUDIO_FRAME_KIND, 5, 0, b'a', b'b'];
        assert!(decode_client_audio_frame(&frame).is_none());
    }

    #[test]
    fn decode_rejects_missing_len() {
        assert!(decode_client_audio_frame(&[AUDIO_FRAME_KIND]).is_none());
    }
}
