//! Client control plane: signaling session with reconnect, heartbeat, STUN
//! reflexive discovery and peer synchronization into the data-plane engine.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use boringtun::x25519::{PublicKey, StaticSecret};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::candidates::local_candidates;
use crate::config::ResolvedConfig;
use crate::engine::{bind_udp, encode_key, Command, Engine};
use crate::proto::{self, Message, PeerInfo};
use crate::room_state::RoomSnapshot;
use crate::{stun, HEARTBEAT_INTERVAL_SECS};

/// Fallback STUN servers for when our signaling server runs TCP-only.
const PUBLIC_STUN: &[&str] = &["stun.qq.com:3478", "stun.miwifi.com:3478"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_public_key_is_stable() {
        let dir = std::env::temp_dir().join(format!("ara-id-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.key");
        let a = own_public_key(Some(&path));
        let b = own_public_key(Some(&path));
        assert_eq!(a, b);
        assert!(!a.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Persisted device identity so virtual IPs stay stable across restarts.
fn load_or_create_secret(override_path: Option<&std::path::Path>) -> StaticSecret {
    use base64::Engine as _;
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => {
            let base = if cfg!(target_os = "windows") {
                std::env::var("APPDATA").unwrap_or_else(|_| ".".into())
            } else {
                format!("{}/.config", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            };
            std::path::PathBuf::from(base).join("ara").join("identity.key")
        }
    };
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(text.trim()) {
            if let Ok(arr) = <[u8; 32]>::try_from(bytes) {
                return StaticSecret::from(arr);
            }
        }
    }
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &path,
        base64::engine::general_purpose::STANDARD.encode(secret.to_bytes()),
    );
    secret
}

#[derive(Debug)]
struct FatalAuth(String);
impl std::fmt::Display for FatalAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FatalAuth {}

#[derive(Debug)]
struct FatalKicked(String);
impl std::fmt::Display for FatalKicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kicked: {}", self.0)
    }
}
impl std::error::Error for FatalKicked {}

/// Events surfaced to the GUI; the CLI ignores them.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Welcome { virtual_ip: Ipv4Addr, room_code: String },
    Peers(Vec<PeerInfo>),
    Kicked(String),
    Status(String),
}

/// Run the client forever (reconnecting), until a fatal error.
pub async fn run(
    cfg: ResolvedConfig,
    ip_out: mpsc::Receiver<Vec<u8>>,
    ip_in: mpsc::Sender<Vec<u8>>,
    assigned: oneshot::Sender<Ipv4Addr>,
    events: Option<mpsc::UnboundedSender<UiEvent>>,
) -> Result<()> {
    let socket = Arc::new(bind_udp(cfg.local_port).context("bind local UDP")?);
    let local_port = socket.local_addr()?.port();
    let secret = load_or_create_secret(cfg.identity_path.as_deref());
    let public_key = encode_key(&PublicKey::from(&secret));
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (relay_tx, mut relay_rx) = mpsc::channel::<Vec<u8>>(256);
    let engine = Engine::new(socket.clone(), secret.clone(), ip_out, ip_in, relay_tx, cmd_rx, cmd_tx.clone());
    tokio::spawn(async move {
        if let Err(e) = engine.run().await {
            tracing::error!("data plane stopped: {e}");
        }
    });

    // Direct-TCP fallback listener.
    let tcp_port = cfg.tcp_port;
    match tokio::net::TcpListener::bind(("0.0.0.0", tcp_port)).await {
        Ok(listener) => {
            let tx = cmd_tx.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    if tx.send(Command::TcpAccepted(stream)).await.is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => tracing::warn!("TCP fallback listener on {tcp_port}: {e}"),
    }

    let mut assigned = Some(assigned);
    let mut snapshot = crate::room_state::load().unwrap_or_default();
    let mut backoff = Duration::from_secs(1);
    loop {
        let outcome = session(
            &cfg,
            local_port,
            tcp_port,
            &public_key,
            &secret,
            &cmd_tx,
            &mut relay_rx,
            &mut assigned,
            &mut snapshot,
            &events,
        )
        .await;
        // Persist the last known room/VIP/member table on every disconnect or
        // explicit exit, before the reconnect backoff.
        let _ = crate::room_state::save(&snapshot);
        match outcome {
            Ok(()) => tracing::info!("signaling closed, reconnecting"),
            Err(e) if e.is::<FatalAuth>() => {
                tracing::error!("{e}");
                return Err(e);
            }
            Err(e) if e.is::<FatalKicked>() => {
                tracing::error!("{e}");
                return Err(e);
            }
            Err(e) => tracing::warn!("signaling error: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Public key (device ID) for the current identity, without starting a session.
pub fn own_public_key(identity_path: Option<&std::path::Path>) -> String {
    let secret = load_or_create_secret(identity_path);
    encode_key(&PublicKey::from(&secret))
}

/// Query the server's room list without joining a room. Shared by the CLI and GUI.
pub async fn fetch_rooms(
    server: &str,
    cert_pem: &str,
    token: &str,
    server_pub: &str,
) -> Result<Vec<crate::proto::RoomInfo>> {
    use tokio_tungstenite::{connect_async, connect_async_tls_with_config, Connector};

    let url = if cert_pem.is_empty() {
        format!("ws://{server}")
    } else {
        format!("wss://{server}")
    };
    let (ws, _) = if cert_pem.is_empty() {
        connect_async(&url).await
    } else {
        let tls = crate::tls::client_config(cert_pem)?;
        connect_async_tls_with_config(&url, None, false, Some(Connector::Rustls(tls))).await
    }
    .with_context(|| format!("connect {url}"))?;
    let (mut sink, mut stream) = ws.split();
    let secret = load_or_create_secret(None);
    let public_key = encode_key(&PublicKey::from(&secret));
    let register = Message::Register {
        network: String::new(),
        secret: String::new(),
        public_key,
        candidates: vec![],
        tcp: vec![],
        symmetric: false,
        room_code: String::new(),
        room_password: String::new(),
        access_token: token.to_string(),
        nickname: String::new(),
        create_room: false,
    };
    sink.send(WsMessage::Text(proto::encode(&register)))
        .await
        .context("send room list request")?;
    let mut reply = stream
        .next()
        .await
        .context("closed before room list")?
        .context("ws read")?;
    if let WsMessage::Text(t) = &reply {
        if let Ok(Message::Challenge { nonce }) = proto::decode(t) {
            if server_pub.is_empty() {
                anyhow::bail!("server requires a DH challenge but access file has no server_pub");
            }
            let server_key = crate::engine::decode_key(server_pub)?;
            let proof = crate::engine::dh_proof(&secret, &server_key, &nonce);
            sink.send(WsMessage::Text(proto::encode(&Message::ChallengeReply { proof })))
                .await?;
            reply = stream
                .next()
                .await
                .context("closed after challenge")?
                .context("ws read")?;
        }
    }
    let WsMessage::Text(text) = reply else {
        anyhow::bail!("expected text room list");
    };
    match proto::decode(&text)? {
        Message::RoomList { rooms } => Ok(rooms),
        Message::Error { reason } => anyhow::bail!("{reason}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

/// Offline direct mode: no server, no room; one manually configured peer.
pub async fn run_direct(
    cfg: ResolvedConfig,
    ip_out: mpsc::Receiver<Vec<u8>>,
    ip_in: mpsc::Sender<Vec<u8>>,
    assigned: oneshot::Sender<Ipv4Addr>,
    events: Option<mpsc::UnboundedSender<UiEvent>>,
) -> Result<()> {
    let vip = cfg.direct_vip.context("missing own VIP for direct mode")?;
    let peer_addr = cfg.direct_peer_addr.context("missing --peer address")?;
    let peer_vip = cfg.direct_peer_vip.context("missing --peer-vip")?;
    if cfg.direct_peer_key.is_empty() {
        anyhow::bail!("missing --peer-key (peer public key)");
    }
    let socket = Arc::new(bind_udp(cfg.local_port).context("bind local UDP")?);
    let secret = load_or_create_secret(cfg.identity_path.as_deref());
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (relay_tx, mut relay_rx) = mpsc::channel::<Vec<u8>>(256);
    let engine = Engine::new(socket, secret, ip_out, ip_in, relay_tx, cmd_rx, cmd_tx.clone());
    tokio::spawn(async move {
        let _ = engine.run().await;
    });
    tokio::spawn(async move {
        while relay_rx.recv().await.is_some() {}
    });
    cmd_tx
        .send(Command::AddPeer(PeerInfo {
            public_key: cfg.direct_peer_key.clone(),
            virtual_ip: peer_vip,
            candidates: vec![peer_addr],
            tcp: vec![],
            symmetric: false,
            nickname: String::new(),
        }))
        .await?;
    let _ = assigned.send(vip);
    if let Some(tx) = events {
        let _ = tx.send(UiEvent::Welcome {
            virtual_ip: vip,
            room_code: "direct".into(),
        });
    }
    tracing::info!("direct mode: {vip} <-> {peer_vip} at {peer_addr}");
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    cfg: &ResolvedConfig,
    local_port: u16,
    tcp_port: u16,
    public_key: &str,
    secret: &StaticSecret,
    cmd_tx: &mpsc::Sender<Command>,
    relay_rx: &mut mpsc::Receiver<Vec<u8>>,
    assigned: &mut Option<oneshot::Sender<Ipv4Addr>>,
    snapshot: &mut RoomSnapshot,
    events: &Option<mpsc::UnboundedSender<UiEvent>>,
) -> Result<()> {
    let server: SocketAddr = tokio::net::lookup_host((cfg.server_host.as_str(), cfg.server_port))
        .await?
        .next()
        .context("server address resolved to nothing")?;
    let url = if cfg.server_cert_pem.is_empty() {
        format!("ws://{server}")
    } else {
        format!("wss://{server}")
    };
    let (ws, _) = if cfg.server_cert_pem.is_empty() {
        tokio_tungstenite::connect_async(&url).await
    } else {
        let tls = crate::tls::client_config(&cfg.server_cert_pem)?;
        tokio_tungstenite::connect_async_tls_with_config(
            &url,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls)),
        )
        .await
    }
    .with_context(|| format!("connect {url}"))?;
    if let tokio_tungstenite::MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_nodelay(true);
    }
    let (mut sink, mut stream) = ws.split();
    tracing::info!("connected to server {server}");

    let register = Message::Register {
        network: cfg.network.clone(),
        secret: cfg.secret.clone(),
        public_key: public_key.to_string(),
        candidates: local_candidates(local_port),
        tcp: tcp_candidates(tcp_port, None),
        symmetric: false,
        room_code: cfg.room_code.clone(),
        room_password: cfg.room_password.clone(),
        access_token: cfg.access_token.clone(),
        nickname: cfg.nickname.clone(),
        create_room: cfg.create_room,
    };
    sink.send(WsMessage::Text(proto::encode(&register))).await?;

    // First reply may be a DH challenge (access-enabled server) or Welcome.
    let mut first = stream.next().await.context("closed before welcome")??;
    if let WsMessage::Text(t) = &first {
        if let Ok(Message::Challenge { nonce }) = proto::decode(t) {
            if cfg.server_pub.is_empty() {
                return Err(FatalAuth(
                    "server requires a DH challenge but access file has no server_pub".into(),
                )
                .into());
            }
            let server_pub = crate::engine::decode_key(&cfg.server_pub)?;
            let proof = crate::engine::dh_proof(secret, &server_pub, &nonce);
            sink.send(WsMessage::Text(proto::encode(&Message::ChallengeReply { proof })))
                .await?;
            first = stream.next().await.context("closed after challenge")??;
        }
    }
    let vip = match first {
        WsMessage::Text(t) => match proto::decode(&t)? {
            Message::Welcome { virtual_ip, room_code, .. } => {
                snapshot.room_code = room_code;
                virtual_ip
            }
            Message::Error { reason } => return Err(FatalAuth(reason).into()),
            other => anyhow::bail!("unexpected first message: {other:?}"),
        },
        _ => anyhow::bail!("expected text welcome"),
    };
    snapshot.virtual_ip = Some(vip);
    if let Some(tx) = events {
        let _ = tx.send(UiEvent::Welcome {
            virtual_ip: vip,
            room_code: snapshot.room_code.clone(),
        });
    }
    tracing::info!("assigned virtual IP {vip}");
    if let Some(tx) = assigned.take() {
        let _ = tx.send(vip);
    }

    // STUN reflexive discovery over the shared socket, then publish candidates.
    // Fall back to public STUN when our server runs TCP-only (--no-stun).
    let mut reflexive = None;
    let mut probes: Vec<SocketAddr> = Vec::new();
    let mut stun_targets = vec![server];
    for host in PUBLIC_STUN {
        if let Ok(mut addrs) = tokio::net::lookup_host(*host).await {
            if let Some(a) = addrs.next() {
                stun_targets.push(a);
            }
        }
    }
    for target in stun_targets {
        let (stun_tx, stun_rx) = oneshot::channel();
        let _ = cmd_tx.try_send(Command::Stun { server: target, reply: stun_tx });
        if let Ok(Some(r)) = stun_rx.await {
            if reflexive.is_none() {
                reflexive = Some(r);
            }
            probes.push(r);
            if probes.len() >= 2 {
                break;
            }
        }
    }
    let symmetric = stun::classify(&probes) == stun::NatType::Symmetric;
    if symmetric {
        tracing::info!("symmetric NAT detected: {probes:?}");
    }
    let mut candidates = local_candidates(local_port);
    if let Some(reflexive) = reflexive {
        if !candidates.contains(&reflexive) {
            candidates.push(reflexive);
        }
        tracing::info!("STUN reflexive address {reflexive}");
    }
    sink.send(WsMessage::Text(proto::encode(&Message::Candidates {
        candidates: candidates.clone(),
        tcp: tcp_candidates(tcp_port, reflexive),
        symmetric,
    })))
    .await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    // Interfaces appear/disappear (Wi-Fi roam, VM adapters); re-publish on change.
    let mut refresh = tokio::time::interval(Duration::from_secs(30));
    refresh.tick().await;
    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(t))) => {
                        match proto::decode(&t) {
                            Ok(Message::Peers { peers }) => {
                                tracing::debug!("peers update: {} entries", peers.len());
                                let others: Vec<PeerInfo> = peers
                                    .into_iter()
                                    .filter(|p| p.public_key != public_key)
                                    .collect();
                                let code = snapshot.room_code.clone();
                                let vip = snapshot.virtual_ip;
                                *snapshot = RoomSnapshot::from_peers(&code, vip, &others);
                                if let Some(tx) = events {
                                    let _ = tx.send(UiEvent::Peers(others.clone()));
                                }
                                let _ = cmd_tx.try_send(Command::SetPeers(others));
                            }
                            Ok(Message::RoomList { rooms }) => {
                                tracing::info!("room list: {rooms:?}");
                            }
                            Ok(Message::Kicked { reason }) => {
                                if let Some(tx) = events {
                                    let _ = tx.send(UiEvent::Kicked(reason.clone()));
                                }
                                return Err(FatalKicked(reason).into());
                            }
                            Ok(_) => {}
                            Err(e) => tracing::debug!("bad signaling message: {e}"),
                        }
                    }
                    Some(Ok(WsMessage::Binary(b))) => {
                        let _ = cmd_tx.send(Command::RelayIn(b)).await;
                    }
                    Some(Ok(WsMessage::Ping(p))) => { let _ = sink.send(WsMessage::Pong(p)).await; }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            Some(bytes) = relay_rx.recv() => {
                sink.send(WsMessage::Binary(bytes)).await?;
            }
            _ = heartbeat.tick() => {
                sink.send(WsMessage::Text(proto::encode(&Message::Heartbeat))).await?;
            }
            _ = refresh.tick() => {
                let mut now = local_candidates(local_port);
                if let Some(r) = reflexive {
                    if !now.contains(&r) {
                        now.push(r);
                    }
                }
                if now != candidates {
                    tracing::info!("candidates changed, re-publishing");
                    candidates = now;
                    sink.send(WsMessage::Text(proto::encode(&Message::Candidates {
                        candidates: candidates.clone(),
                        tcp: tcp_candidates(tcp_port, reflexive),
                        symmetric,
                    }))).await?;
                }
            }
        }
    }
}

/// TCP endpoints for the direct-TCP fallback: local interfaces plus the
/// STUN-reflexive IP (cloud NAT usually preserves the TCP port).
fn tcp_candidates(tcp_port: u16, reflexive: Option<SocketAddr>) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = local_candidates(0)
        .into_iter()
        .map(|a| SocketAddr::new(a.ip(), tcp_port))
        .collect();
    if let Some(r) = reflexive {
        let a = SocketAddr::new(r.ip(), tcp_port);
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}
