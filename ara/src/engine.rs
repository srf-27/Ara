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
/// Direct-TCP transport: used once proven by a received WG packet; relay stays
/// active until then so a black-holed TCP connect never cuts connectivity.
const TCP_DIRECT: bool = true;
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
    Stun { server: SocketAddr, reply: oneshot::Sender<Option<SocketAddr>> },
    /// WG datagram arrived via the server relay (TCP), not UDP.
    RelayIn(Vec<u8>),
    /// Outbound peer TCP connection established.
    TcpUp { key: String, stream: tokio::net::TcpStream },
    /// Inbound peer TCP connection accepted (peer identified on first packet).
    TcpAccepted(tokio::net::TcpStream),
    /// WG datagram arrived over a peer TCP connection (conn id for writer matching).
    TcpIn { conn: u64, key: Option<String>, data: Vec<u8> },
    /// A peer TCP connection closed.
    TcpClosed(u64),
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

/// Non-blocking TCP send. The engine is a single loop; awaiting a TCP writer
/// that stalls on a black-holed connection (NAT SYN proxy) would freeze it.
/// Returns true if the sender accepted the bytes. A dead sender is dropped and
/// the peer reverts to relay; a merely full (busy/stalled) sender is kept — the
/// writer's write timeout reaps it if it never drains.
fn send_tcp(peer: &mut Peer, bytes: &[u8]) -> bool {
    let Some(tx) = &peer.tcp_tx else { return false };
    match tx.try_send(bytes.to_vec()) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => {
            peer.tcp_tx = None;
            peer.tcp_proven = false;
            peer.use_relay = true;
            false
        }
        Err(mpsc::error::TrySendError::Full(_)) => false,
    }
}

async fn apply(
    socket: &UdpSocket,
    ip_in: &mpsc::Sender<Vec<u8>>,
    relay_out: &mpsc::Sender<Vec<u8>>,
    peer: &mut Peer,
    act: Action,
) {
    match act {
        Action::Send(bytes) => {
            let direct_recent =
                Instant::now().duration_since(peer.last_direct) < Duration::from_secs(10);
            if !direct_recent {
                let tcp_ok = send_tcp(peer, &bytes);
                if tcp_ok && peer.tcp_proven {
                    return;
                }
                if peer.use_relay {
                    let mut frame = peer.key_bytes.to_vec();
                    frame.extend_from_slice(&bytes);
                    // Non-blocking: a saturated relay must not stall the loop
                    // (it still needs to drain inbound ACKs). Drop on full.
                    let _ = relay_out.try_send(frame);
                    return;
                }
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
            let _ = ip_in.try_send(bytes);
        }
        Action::None => {}
    }
}

async fn punch(socket: &UdpSocket, relay_out: &mpsc::Sender<Vec<u8>>, peer: &mut Peer) {
    peer.last_punch = Instant::now();
    let mut dst_buf = [0u8; BUF];
    let res = peer.tunn.format_handshake_initiation(&mut dst_buf, false);
    if let TunnResult::WriteToNetwork(bytes) = res {
        send_tcp(peer, bytes);
        if peer.use_relay {
            let mut frame = peer.key_bytes.to_vec();
            frame.extend_from_slice(bytes);
            let _ = relay_out.try_send(frame);
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
    tcp: Vec<SocketAddr>,
    tcp_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// True only after a WG packet was successfully received over TCP. Until
    /// then the relay stays active so a black-holed TCP connect (NAT SYN proxy)
    /// never cuts connectivity.
    tcp_proven: bool,
    last_tcp_try: Instant,
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
    cmd_tx: mpsc::Sender<Command>,
    tcp_writers: HashMap<u64, mpsc::Sender<Vec<u8>>>,
    next_conn: u64,
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
        cmd_tx: mpsc::Sender<Command>,
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
            cmd_tx,
            tcp_writers: HashMap::new(),
            next_conn: 1,
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
                        // Do NOT sleep here: it would stall the whole event loop.
                        Err(e) => {
                            tracing::debug!("udp recv: {e}");
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
            Command::TcpUp { key, stream } => {
                self.attach_tcp(&key, stream, true).await;
            }
            Command::TcpAccepted(stream) => {
                self.attach_tcp("", stream, false).await;
            }
            Command::TcpIn { conn, key, data } => {
                self.ingest_tcp(&data, key, conn).await;
            }
            Command::TcpClosed(conn) => {
                tracing::debug!("tcp conn {conn} closed");
                self.tcp_writers.remove(&conn);
                for p in self.peers.values_mut() {
                    if p.tcp_tx.as_ref().is_some_and(|tx| tx.is_closed()) {
                        p.tcp_tx = None;
                        p.tcp_proven = false;
                        p.use_relay = true;
                    }
                }
            }
        }
    }

    async fn upsert_peer(&mut self, info: PeerInfo) {
        if self.peers.contains_key(&info.public_key) {
            self.update_candidates(&info.public_key, info.candidates, info.tcp);
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
            tcp: info.tcp,
            tcp_tx: None,
            tcp_proven: false,
            last_tcp_try: Instant::now() - Duration::from_secs(10),
            endpoint: None,
            last_punch: Instant::now() - PUNCH_INTERVAL,
            created: Instant::now(),
            last_direct: Instant::now() - Duration::from_secs(10),
            use_relay: false,
        };
        punch(&self.socket, &self.relay_out, &mut peer).await;
        self.peers.insert(info.public_key, peer);
    }

    fn update_candidates(&mut self, key: &str, candidates: Vec<SocketAddr>, tcp: Vec<SocketAddr>) {
        if let Some(p) = self.peers.get_mut(key) {
            p.candidates = candidates;
            p.tcp = tcp;
            p.last_punch = Instant::now() - PUNCH_INTERVAL;
        }
    }

    /// Wire a TCP stream as a transport: reader feeds TcpIn, writer drains a channel.
    async fn attach_tcp(&mut self, key: &str, stream: tokio::net::TcpStream, outbound: bool) {
        let _ = stream.set_nodelay(true);
        let conn = self.next_conn;
        self.next_conn += 1;
        tracing::debug!("attach tcp conn {conn} outbound={outbound} key={key}");
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let cmd_tx = self.cmd_sender();
        let cmd_tx_wr = cmd_tx.clone();
        let hint = if outbound { Some(key.to_string()) } else { None };
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut lenbuf = [0u8; 2];
            loop {
                // Idle timeout reaps a black-holed read half (healthy peers
                // send WG keepalives well within 60s).
                match tokio::time::timeout(Duration::from_secs(60), rd.read_exact(&mut lenbuf)).await {
                    Ok(Ok(_)) => {}
                    _ => {
                        tracing::debug!("tcp reader conn {conn}: read len eof/err/idle");
                        break;
                    }
                }
                let n = u16::from_be_bytes(lenbuf) as usize;
                if n == 0 || n > BUF {
                    tracing::debug!("tcp reader conn {conn}: bad len {n}");
                    break;
                }
                let mut data = vec![0u8; n];
                if rd.read_exact(&mut data).await.is_err() {
                    tracing::debug!("tcp reader conn {conn}: read body eof/err");
                    break;
                }
                if cmd_tx.send(Command::TcpIn { conn, key: hint.clone(), data }).await.is_err() {
                    break;
                }
            }
            let _ = cmd_tx.send(Command::TcpClosed(conn)).await;
        });
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(data) = rx.recv().await {
                let len = (data.len() as u16).to_be_bytes();
                // A black-holed TCP (NAT SYN proxy) never errors but stalls
                // write_all forever once the kernel buffer fills; bound it so
                // the task exits, closes the socket and triggers relay fallback.
                let res = tokio::time::timeout(Duration::from_secs(5), async {
                    wr.write_all(&len).await?;
                    wr.write_all(&data).await
                })
                .await;
                if !matches!(res, Ok(Ok(()))) {
                    tracing::debug!("tcp writer conn {conn}: write err/stalled, exiting");
                    break;
                }
            }
            // Notify the engine promptly so a dead sender is cleared and the
            // peer falls back to relay without waiting for the reader's EOF.
            let _ = cmd_tx_wr.send(Command::TcpClosed(conn)).await;
        });
        if outbound {
            if let Some(p) = self.peers.get_mut(key) {
                p.tcp_tx = Some(tx);
                // Keep relay active until TCP is proven by a received packet.
                tracing::debug!("peer {} outbound TCP attached (probing)", p.virtual_ip);
            }
        } else {
            self.tcp_writers.insert(conn, tx);
        }
    }

    fn cmd_sender(&self) -> mpsc::Sender<Command> {
        self.cmd_tx.clone()
    }

    async fn ingest_tcp(&mut self, data: &[u8], hint: Option<String>, conn: u64) {
        let keys: Vec<String> = match hint {
            Some(k) => vec![k],
            None => self.peers.keys().cloned().collect(),
        };
        for key in keys {
            let act = {
                let Some(peer) = self.peers.get_mut(&key) else { continue };
                let mut dst_buf = [0u8; BUF];
                let res = peer.tunn.decapsulate(None, data, &mut dst_buf);
                match res {
                    TunnResult::Err(_) => continue,
                    other => action(other),
                }
            };
            if let Some(peer) = self.peers.get_mut(&key) {
                // A valid WG packet over TCP proves this direct path works; only
                // now disable the relay fallback.
                peer.tcp_proven = true;
                peer.use_relay = false;
                if peer.tcp_tx.as_ref().is_none_or(|tx| tx.is_closed()) {
                    if let Some(tx) = self.tcp_writers.remove(&conn) {
                        peer.tcp_tx = Some(tx);
                        tracing::info!("peer {} direct TCP (inbound) established", peer.virtual_ip);
                    }
                }
                apply(&self.socket, &self.ip_in, &self.relay_out, peer, act).await;
            }
            self.drain(&key).await;
            return;
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
        if let Some(peer) = self.peers.get_mut(&key) {
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
            if let Some(peer) = self.peers.get_mut(key) {
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
                // No proven direct path (UDP recent or TCP proven) for a while
                // -> activate relay. A still-probing TCP sender must not block
                // this, otherwise a black-holed TCP connect cuts connectivity.
                if !peer.use_relay
                    && !peer.tcp_proven
                    && now.duration_since(peer.created) > Duration::from_secs(8)
                    && now.duration_since(peer.last_direct) > Duration::from_secs(10)
                {
                    peer.use_relay = true;
                    tracing::info!("peer {} via server relay (no direct path)", peer.virtual_ip);
                }
                // Attempt direct TCP when UDP is dead and we have TCP endpoints.
                if TCP_DIRECT
                    && peer.tcp_tx.is_none()
                    && !peer.tcp.is_empty()
                    && now.duration_since(peer.last_direct) > Duration::from_secs(5)
                    && now.duration_since(peer.last_tcp_try) > Duration::from_secs(5)
                {
                    peer.last_tcp_try = now;
                    let tcp = peer.tcp.clone();
                    let key = key.clone();
                    let tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        for addr in tcp {
                            if let Ok(Ok(stream)) =
                                tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr)).await
                            {
                                let _ = tx.send(Command::TcpUp { key, stream }).await;
                                return;
                            }
                        }
                    });
                }
                let mut dst_buf = [0u8; BUF];
                let res = peer.tunn.update_timers(&mut dst_buf);
                let stale = peer
                    .tunn
                    .time_since_last_handshake()
                    .is_none_or(|d| d > Duration::from_secs(30));
                (action(res), stale && now.duration_since(peer.last_punch) > PUNCH_INTERVAL)
            };
            if let Some(peer) = self.peers.get_mut(&key) {
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