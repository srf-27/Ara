//! Local room snapshot: the last known room, VIP and member table. Written on
//! room exit and before reconnect; the server always stays authoritative.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub public_key: String,
    pub virtual_ip: Ipv4Addr,
    #[serde(default)]
    pub nickname: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomSnapshot {
    #[serde(default)]
    pub room_code: String,
    #[serde(default)]
    pub virtual_ip: Option<Ipv4Addr>,
    #[serde(default)]
    pub members: Vec<Member>,
}

impl RoomSnapshot {
    pub fn from_peers(
        room_code: &str,
        virtual_ip: Option<Ipv4Addr>,
        peers: &[crate::proto::PeerInfo],
    ) -> Self {
        Self {
            room_code: room_code.to_string(),
            virtual_ip,
            members: peers
                .iter()
                .map(|p| Member {
                    public_key: p.public_key.clone(),
                    virtual_ip: p.virtual_ip,
                    nickname: p.nickname.clone(),
                })
                .collect(),
        }
    }
}

pub fn default_path() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var("APPDATA").unwrap_or_else(|_| ".".into())
    } else {
        format!(
            "{}/.config",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    };
    PathBuf::from(base).join("ara").join("room.json")
}

pub fn save_to(snapshot: &RoomSnapshot, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let json = serde_json::to_string_pretty(snapshot).context("encode room snapshot")?;
    std::fs::write(path, json).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn load_from(path: &Path) -> Option<RoomSnapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(snapshot: &RoomSnapshot) -> Result<()> {
    save_to(snapshot, &default_path())
}

pub fn load() -> Option<RoomSnapshot> {
    load_from(&default_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RoomSnapshot {
        RoomSnapshot {
            room_code: "ABC123".into(),
            virtual_ip: Some(Ipv4Addr::new(10, 99, 0, 2)),
            members: vec![Member {
                public_key: "keyA".into(),
                virtual_ip: Ipv4Addr::new(10, 99, 0, 2),
                nickname: "alice".into(),
            }],
        }
    }

    #[test]
    fn snapshot_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ara-room-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("room.json");
        let snap = sample();
        save_to(&snap, &path).unwrap();
        assert_eq!(load_from(&path), Some(snap));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_peers_maps_members() {
        let peers = vec![crate::proto::PeerInfo {
            public_key: "keyA".into(),
            virtual_ip: Ipv4Addr::new(10, 99, 0, 3),
            candidates: vec![],
            tcp: vec![],
            symmetric: false,
            nickname: "alice".into(),
        }];
        let snap = RoomSnapshot::from_peers("ABC123", Some(Ipv4Addr::new(10, 99, 0, 2)), &peers);
        assert_eq!(snap.room_code, "ABC123");
        assert_eq!(snap.members[0].nickname, "alice");
    }
}
