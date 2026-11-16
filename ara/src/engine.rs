//! Data plane: one async task owning the shared STUN/WG UDP socket and one
//! boringtun tunnel per peer. IP packets flow to/from the TUN via channels.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::proto::PeerInfo;
use crate::{stun, KEEPALIVE_SECS};

const BUF: usize = 2048;
const TICK: Duration = Duration::from_millis(250);
const PUNCH_INTERVAL: Duration = Duration::from_secs(2);
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

pub fn encode_key(key: &PublicKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.as_bytes())
}

pub fn decode_key(s: &str) -> Result<PublicKey> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("base64 public key")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("key not 32 bytes"))?;
    Ok(PublicKey::from(arr))
}

pub enum Command {
    AddPeer(PeerInfo),
    /// Reconcile the peer table to exactly this list (drops stale peers).
    SetPeers(Vec<PeerInfo>),
    RemovePeer(String),
    UpdateCandidates { public_key: String, candidates: Vec<SocketAddr> },
    Stun { server: SocketAddr, reply: oneshot::Sender<Option<SocketAddr>> },
    /// WG datagram arrived via the server relay (TCP), not UDP.
    RelayIn(Vec<u8>),
}

/// Owned copy of a TunnResult so we can release the peer borrow before doing IO.
enum Action {
    Send(Vec<u8>),
    ToTun(Vec<u8>),
    None,
}

fn action(res: TunnResult<'_>) -> Action {
    match res {
        TunnResult::WriteToNetwork(b) => Action::Send(b.to_vec()),
        TunnResult::WriteToTunnelV4(b, _) | TunnResult::WriteToTunnelV6(b, _) => {
            Action::ToTun(b.to_vec())
        }
        _ => Action::None,
    }
}

/// Our socket is dual-stack IPv6; Windows requires IPv4 targets to be
/// v4-mapped or sendto fails with WSAEFAULT.
fn target(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port()),
        v6 => v6,
    }
}

async fn apply(
    socket: &UdpSocket,
    ip_in: &mpsc::Sender<Vec<u8>>,
    relay_out: &mpsc::Sender<Vec<u8>>,
    peer: &Peer,
    act: Action,
) {
    match act {
        Action::Send(bytes) => {
            if peer.use_relay {
                let mut frame = peer.key_bytes.to_vec();
                frame.extend_from_slice(&bytes);
                let _ = relay_out.send(frame).await;
                return;
            }
            let targets: Vec<SocketAddr> = match peer.endpoint {
                Some(ep) => vec![ep],
                None => peer.candidates.clone(),
            };
            for t in targets {
                let _ = socket.send_to(&bytes, target(t)).await;
            }
        }
        Action::ToTun(bytes) => {
            let _ = ip_in.send(bytes).await;
        }
        Action::None => {}
    }
}

async fn punch(socket: &UdpSocket, relay_out: &mpsc::Sender<Vec<u8>>, peer: &mut Peer) {
    peer.last_punch = Instant::now();
    let mut dst_buf = [0u8; BUF];
    let res = peer.tunn.format_handshake_initiation(&mut dst_buf, false);
    if let TunnResult::WriteToNetwork(bytes) = res {
        if peer.use_relay {
            let mut frame = peer.key_bytes.to_vec();
            frame.extend_from_slice(bytes);
            let _ = relay_out.send(frame).await;
        }
        for c in &peer.candidates {
            let _ = socket.send_to(bytes, target(*c)).await;
        }
    }
}

struct Peer {
    tunn: Tunn,
    key_bytes: [u8; 32],
    virtual_ip: Ipv4Addr,
    candidates: Vec<SocketAddr>,
    endpoint: Option<SocketAddr>,
    last_punch: Instant,
    created: Instant,
    last_direct: Instant,
    use_relay: bool,
}

pub struct Engine {
    socket: Arc<UdpSocket>,
    secret: StaticSecret,
    peers: HashMap<String, Peer>,
    vip_to_key: HashMap<Ipv4Addr, String>,
    ip_out: mpsc::Receiver<Vec<u8>>,
    ip_in: mpsc::Sender<Vec<u8>>,
    relay_out: mpsc::Sender<Vec<u8>>,
    cmd: mpsc::Receiver<Command>,
    next_index: u32,
    pending_stun: Option<([u8; 12], Instant, oneshot::Sender<Option<SocketAddr>>)>,
}

impl Engine {
    pub fn new(
        socket: Arc<UdpSocket>,
        secret: StaticSecret,
        ip_out: mpsc::Receiver<Vec<u8>>,
        ip_in: mpsc::Sender<Vec<u8>>,
        relay_out: mpsc::Sender<Vec<u8>>,
        cmd: mpsc::Receiver<Command>,
    ) -> Self {
        Self {
            socket,
            secret,
            peers: HashMap::new(),
            vip_to_key: HashMap::new(),
            ip_out,
            ip_in,
            relay_out,
            cmd,
            next_index: 1,
            pending_stun: None,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let mut udp_buf = vec![0u8; BUF];
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                r = self.socket.recv_from(&mut udp_buf) => {
                    match r {
                        Ok((n, src)) => self.on_udp(&udp_buf[..n], src).await,
                        // Windows surfaces ICMP port-unreachable as a recv error
                        // (WSAECONNRESET); normal during punching, not fatal.
                        Err(e) => {
                            tracing::debug!("udp recv: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
                Some(packet) = self.ip_out.recv() => {
                    self.on_ip_out(&packet).await;
                }
                Some(cmd) = self.cmd.recv() => {
                    self.on_command(cmd).await;
                }
                _ = tick.tick() => {
                    self.on_tick().await;
                }
            }
        }
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::AddPeer(info) => {
                self.upsert_peer(info).await;
            }
            Command::SetPeers(list) => {
                let keep: std::collections::HashSet<String> =
                    list.iter().map(|p| p.public_key.clone()).collect();
                let stale: Vec<String> = self
                    .peers
                    .keys()
                    .filter(|k| !keep.contains(*k))
                    .cloned()
                    .collect();
                for k in stale {
                    if let Some(p) = self.peers.remove(&k) {
                        self.vip_to_key.remove(&p.virtual_ip);
                    }
                }
                for info in list {
                    self.upsert_peer(info).await;
                }
            }
            Command::RemovePeer(key) => {
                if let Some(p) = self.peers.remove(&key) {
                    self.vip_to_key.remove(&p.virtual_ip);
                }
            }
            Command::UpdateCandidates { public_key, candidates } => {
                self.update_candidates(&public_key, candidates);
            }
            Command::Stun { server, reply } => {
                let mut txid = [0u8; 12];
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut txid);
                let req = stun::encode_request(&txid);
                tracing::debug!("stun request to {server}");
                if self.socket.send_to(&req, target(server)).await.is_ok() {
                    self.pending_stun = Some((txid, Instant::now(), reply));
                } else {
                    let _ = reply.send(None);
                }
            }
            Command::RelayIn(data) => {
                self.ingest(&data, None).await;
            }
        }
    }

    async fn upsert_peer(&mut self, info: PeerInfo) {
        if self.peers.contains_key(&info.public_key) {
            self.update_candidates(&info.public_key, info.candidates);
            return;
        }
        let Ok(key) = decode_key(&info.public_key) else {
            tracing::warn!("bad peer key {}", info.public_key);
            return;
        };
        let tunn = match Tunn::new(
            self.secret.clone(),
            key,
            None,
            Some(KEEPALIVE_SECS),
            self.next_index,
            None,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("tunnel init: {e}");
                return;
            }
        };
        self.next_index += 1;
        self.vip_to_key.insert(info.virtual_ip, info.public_key.clone());
        tracing::debug!("add peer {} vip {} candidates {:?}", info.public_key, info.virtual_ip, info.candidates);
        let mut peer = Peer {
            tunn,
            key_bytes: *key.as_bytes(),
            virtual_ip: info.virtual_ip,
            candidates: info.candidates,
            endpoint: None,
            last_punch: Instant::now() - PUNCH_INTERVAL,
            created: Instant::now(),
            last_direct: Instant::now() - Duration::from_secs(10),
            use_relay: false,
        };
        punch(&self.socket, &self.relay_out, &mut peer).await;
        self.peers.insert(info.public_key, peer);
    }

    fn update_candidates(&mut self, key: &str, candidates: Vec<SocketAddr>) {
        if let Some(p) = self.peers.get_mut(key) {
            p.candidates = candidates;
            p.last_punch = Instant::now() - PUNCH_INTERVAL;
        }
    }

    async fn on_ip_out(&mut self, packet: &[u8]) {
        let Some(dst) = Tunn::dst_address(packet) else {
            return;
        };
        let IpAddr::V4(dst4) = dst else { return };
        let Some(key) = self.vip_to_key.get(&dst4).cloned() else {
            return;
        };
        let act = {
            let Some(peer) = self.peers.get_mut(&key) else { return };
            let mut dst_buf = [0u8; BUF];
            let res = peer.tunn.encapsulate(packet, &mut dst_buf);
            action(res)
        };
        if let Some(peer) = self.peers.get(&key) {
            apply(&self.socket, &self.ip_in, &self.relay_out, peer, act).await;
        }
    }

    async fn on_udp(&mut self, datagram: &[u8], src: SocketAddr) {
        if let Some((txid, _, _)) = &self.pending_stun {
            if datagram.len() >= 20 && datagram[8..20] == *txid {
                if let Some(addr) = stun::parse_response(datagram) {
                    tracing::debug!("stun reflexive {addr}");
                    if let Some((_, _, reply)) = self.pending_stun.take() {
                        let _ = reply.send(Some(addr));
                    }
                    return;
                }
            }
        }
        self.ingest(datagram, Some(src)).await;
    }

    /// Feed a WG datagram (from UDP or relay) to whichever peer accepts it.
    async fn ingest(&mut self, datagram: &[u8], src: Option<SocketAddr>) {
        let keys: Vec<String> = self.peers.keys().cloned().collect();
        for key in keys {
            let act = {
                let peer = self.peers.get_mut(&key).unwrap();
                let mut dst_buf = [0u8; BUF];
                let res = peer.tunn.decapsulate(src.map(|s| s.ip()), datagram, &mut dst_buf);
                match res {
                    TunnResult::Err(_) => continue,
                    other => action(other),
                }
            };
            if let Some(peer) = self.peers.get_mut(&key) {
                if let Some(src) = src {
                    peer.endpoint = Some(src);
                    peer.last_direct = Instant::now();
                    if peer.use_relay {
                        tracing::info!("peer {} direct path restored", peer.virtual_ip);
                    }
                    peer.use_relay = false;
                }
                apply(&self.socket, &self.ip_in, &self.relay_out, peer, act).await;
            }
            self.drain(&key).await;
            return;
        }
    }

    async fn drain(&mut self, key: &str) {
        loop {
            let act = {
                let peer = self.peers.get_mut(key).unwrap();
                let mut dst_buf = [0u8; BUF];
                let res = peer.tunn.decapsulate(None, &[], &mut dst_buf);
                match res {
                    TunnResult::Done | TunnResult::Err(_) => return,
                    other => action(other),
                }
            };
            if let Some(peer) = self.peers.get(key) {
                apply(&self.socket, &self.ip_in, &self.relay_out, peer, act).await;
            }
        }
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();
        if let Some((_, started, _)) = &self.pending_stun {
            if now.duration_since(*started) > STUN_TIMEOUT {
                if let Some((_, _, reply)) = self.pending_stun.take() {
                    let _ = reply.send(None);
                }
            }
        }
        let keys: Vec<String> = self.peers.keys().cloned().collect();
        for key in keys {
            let (act, punch_due) = {
                let peer = self.peers.get_mut(&key).unwrap();
                // No direct UDP path for a while -> fall back to server relay.
                if !peer.use_relay
                    && now.duration_since(peer.created) > Duration::from_secs(8)
                    && now.duration_since(peer.last_direct) > Duration::from_secs(10)
                {
                    peer.use_relay = true;
                    tracing::info!("peer {} via server relay (no direct path)", peer.virtual_ip);
                }
                let mut dst_buf = [0u8; BUF];
                let res = peer.tunn.update_timers(&mut dst_buf);
                let stale = peer
                    .tunn
                    .time_since_last_handshake()
                    .is_none_or(|d| d > Duration::from_secs(30));
                (action(res), stale && now.duration_since(peer.last_punch) > PUNCH_INTERVAL)
            };
            if let Some(peer) = self.peers.get(&key) {
                apply(&self.socket, &self.ip_in, &self.relay_out, peer, act).await;
            }
            if punch_due {
                let peer = self.peers.get_mut(&key).unwrap();
                punch(&self.socket, &self.relay_out, peer).await;
            }
        }
    }
}

/// Bind the shared dual-stack UDP socket used for both STUN and WireGuard.
pub fn bind_udp(port: u16) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_only_v6(false)?;
    sock.bind(&SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], port)).into())?;
    sock.set_nonblocking(true)?;
    UdpSocket::from_std(sock.into()).context("tokio udp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        let s = encode_key(&public);
        assert_eq!(decode_key(&s).unwrap().as_bytes(), public.as_bytes());
        assert!(decode_key("not-base64!!").is_err());
        assert!(decode_key("aGVsbG8=").is_err());
    }

    #[test]
    fn dst_address_v4() {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x45;
        pkt[16..20].copy_from_slice(&[10, 99, 0, 7]);
        assert_eq!(
            Tunn::dst_address(&pkt),
            Some(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 7)))
        );
    }
}