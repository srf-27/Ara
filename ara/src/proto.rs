use std::net::{Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub public_key: String,
    pub virtual_ip: Ipv4Addr,
    pub candidates: Vec<SocketAddr>,
    /// Reachable TCP endpoints for the direct-TCP fallback transport.
    #[serde(default)]
    pub tcp: Vec<SocketAddr>,
    /// Peer detected a symmetric NAT: its reflexive port is not reusable.
    #[serde(default)]
    pub symmetric: bool,
    /// Display name chosen by the user; never used for authorization.
    #[serde(default)]
    pub nickname: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomInfo {
    pub code: String,
    pub creator: String,
    pub creator_id: String,
    pub members: u8,
    pub limit: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Register {
        /// Legacy network name (M1/M2). Ignored when `room_code` is set.
        #[serde(default)]
        network: String,
        /// Legacy shared secret (M1/M2). Ignored when `room_code` is set.
        #[serde(default)]
        secret: String,
        public_key: String,
        candidates: Vec<SocketAddr>,
        #[serde(default)]
        tcp: Vec<SocketAddr>,
        #[serde(default)]
        symmetric: bool,
        /// M3 room code; empty means "just list rooms, do not join".
        #[serde(default)]
        room_code: String,
        /// Room password (verified server-side against its salted hash).
        #[serde(default)]
        room_password: String,
        /// Access token from the server's validation file.
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        nickname: String,
        /// Ask the server to create a new room with `room_password`.
        #[serde(default)]
        create_room: bool,
    },
    Welcome {
        virtual_ip: Ipv4Addr,
        prefix: u8,
        #[serde(default)]
        room_code: String,
    },
    Peers {
        peers: Vec<PeerInfo>,
    },
    Candidates {
        candidates: Vec<SocketAddr>,
        #[serde(default)]
        tcp: Vec<SocketAddr>,
        #[serde(default)]
        symmetric: bool,
    },
    /// Server-wide room list (sent when the client registers without joining).
    RoomList {
        rooms: Vec<RoomInfo>,
    },
    /// Server-initiated disconnect with a human readable reason.
    Kicked {
        reason: String,
    },
    /// M3 security placeholder: server nonce, client proves key possession.
    Challenge {
        nonce: String,
    },
    ChallengeReply {
        proof: String,
    },
    Error {
        reason: String,
    },
    Heartbeat,
}

impl Message {
    pub fn room_list(rooms: Vec<RoomInfo>) -> Self {
        Message::RoomList { rooms }
    }
}

pub fn encode(msg: &Message) -> String {
    serde_json::to_string(msg).expect("message is always serializable")
}

pub fn decode(text: &str) -> Result<Message, serde_json::Error> {
    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_messages() {
        let msgs = vec![
            Message::Register {
                network: "n".into(),
                secret: "s".into(),
                public_key: "k".into(),
                candidates: vec!["1.2.3.4:5".parse().unwrap()],
                tcp: vec![],
                symmetric: true,
                room_code: "ABC123".into(),
                room_password: "pw".into(),
                access_token: "tok".into(),
                nickname: "tester".into(),
                create_room: true,
            },
            Message::Welcome {
                virtual_ip: Ipv4Addr::new(10, 99, 0, 2),
                prefix: 24,
                room_code: "ABC123".into(),
            },
            Message::Peers { peers: vec![] },
            Message::Candidates { candidates: vec![], tcp: vec![], symmetric: true },
            Message::RoomList {
                rooms: vec![RoomInfo {
                    code: "ABC123".into(),
                    creator: "tester".into(),
                    creator_id: "k".into(),
                    members: 2,
                    limit: 8,
                }],
            },
            Message::Kicked { reason: "room closed".into() },
            Message::Challenge { nonce: "n".into() },
            Message::ChallengeReply { proof: "p".into() },
            Message::Error { reason: "bad secret".into() },
            Message::Heartbeat,
        ];
        for m in msgs {
            assert_eq!(decode(&encode(&m)).unwrap(), m);
        }
    }

    #[test]
    fn old_messages_without_new_fields_decode() {
        let old = r#"{"type":"candidates","candidates":[],"tcp":[]}"#;
        assert_eq!(
            decode(old).unwrap(),
            Message::Candidates { candidates: vec![], tcp: vec![], symmetric: false }
        );
        let old_peers = r#"{"type":"peers","peers":[{"public_key":"k","virtual_ip":"10.99.0.2","candidates":[],"tcp":[]}]}"#;
        let Message::Peers { peers } = decode(old_peers).unwrap() else { panic!("expected peers") };
        assert!(!peers[0].symmetric);
        assert!(peers[0].nickname.is_empty());
        let old_welcome = r#"{"type":"welcome","virtual_ip":"10.99.0.2","prefix":24}"#;
        assert_eq!(
            decode(old_welcome).unwrap(),
            Message::Welcome {
                virtual_ip: Ipv4Addr::new(10, 99, 0, 2),
                prefix: 24,
                room_code: String::new()
            }
        );
    }

    #[test]
    fn room_list_helper() {
        let m = Message::room_list(vec![]);
        assert_eq!(decode(&encode(&m)).unwrap(), m);
    }
}
