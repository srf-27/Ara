pub mod candidates;
pub mod client;
pub mod config;
pub mod engine;
pub mod proto;
pub mod room_state;
pub mod stun;
pub mod transfer;

use std::net::Ipv4Addr;

pub const DEFAULT_PORT: u16 = 23947;
pub const VIRTUAL_SUBNET: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 0);
pub const VIRTUAL_PREFIX: u8 = 24;
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;
pub const OFFLINE_AFTER_SECS: u64 = 45;
pub const KEEPALIVE_SECS: u16 = 25;
pub const TUN_MTU: u16 = 1280;