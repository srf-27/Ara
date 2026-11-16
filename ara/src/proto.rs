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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Register {
        network: String,
        secret: String,
        public_key: String,
        candidates: Vec<SocketAddr>,
        #[serde(default)]
        tcp: Vec<SocketAddr>,
    },
    Welcome {
        virtual_ip: Ipv4Addr,
        prefix: u8,
    },
    Peers {
        peers: Vec<PeerInfo>,
    },
    Candidates {
        candidates: Vec<SocketAddr>,
        #[serde(default)]
        tcp: Vec<SocketAddr>,
    },
    Error {
        reason: String,
    },
    Heartbeat,
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
            },
            Message::Welcome {
                virtual_ip: Ipv4Addr::new(10, 99, 0, 2),
                prefix: 24,
            },
            Message::Peers { peers: vec![] },
            Message::Candidates { candidates: vec![], tcp: vec![] },
            Message::Error { reason: "bad secret".into() },
            Message::Heartbeat,
        ];
        for m in msgs {
            assert_eq!(decode(&encode(&m)).unwrap(), m);
        }
    }
}