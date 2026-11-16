//! Client control plane: signaling session with reconnect, heartbeat, STUN
//! reflexive discovery and peer synchronization into the data-plane engine.

use std::collections::HashMap;
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
use crate::HEARTBEAT_INTERVAL_SECS;

#[derive(Debug)]
struct FatalAuth(String);
impl std::fmt::Display for FatalAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FatalAuth {}

/// Run the client forever (reconnecting), until a fatal error.
pub async fn run(
    cfg: ResolvedConfig,
    ip_out: mpsc::Receiver<Vec<u8>>,
    ip_in: mpsc::Sender<Vec<u8>>,
    assigned: oneshot::Sender<Ipv4Addr>,
) -> Result<()> {
    let socket = Arc::new(bind_udp(cfg.local_port).context("bind local UDP")?);
    let local_port = socket.local_addr()?.port();
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public_key = encode_key(&PublicKey::from(&secret));
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let engine = Engine::new(socket.clone(), secret, ip_out, ip_in, cmd_rx);
    tokio::spawn(async move {
        if let Err(e) = engine.run().await {
            tracing::error!("data plane stopped: {e}");
        }
    });

    let mut assigned = Some(assigned);
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&cfg, local_port, &public_key, &cmd_tx, &mut assigned).await {
            Ok(()) => tracing::info!("signaling closed, reconnecting"),
            Err(e) if e.is::<FatalAuth>() => {
                tracing::error!("{e}");
                return Err(e);
            }
            Err(e) => tracing::warn!("signaling error: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn session(
    cfg: &ResolvedConfig,
    local_port: u16,
    public_key: &str,
    cmd_tx: &mpsc::Sender<Command>,
    assigned: &mut Option<oneshot::Sender<Ipv4Addr>>,
) -> Result<()> {
    let server: SocketAddr = tokio::net::lookup_host((cfg.server_host.as_str(), cfg.server_port))
        .await?
        .next()
        .context("server address resolved to nothing")?;
    let url = format!("ws://{server}");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connect {url}"))?;
    let (mut sink, mut stream) = ws.split();
    tracing::info!("connected to server {server}");

    let register = Message::Register {
        network: cfg.network.clone(),
        secret: cfg.secret.clone(),
        public_key: public_key.to_string(),
        candidates: local_candidates(local_port),
    };
    sink.send(WsMessage::Text(proto::encode(&register))).await?;

    // First reply: Welcome or Error.
    let vip = match stream.next().await.context("closed before welcome")?? {
        WsMessage::Text(t) => match proto::decode(&t)? {
            Message::Welcome { virtual_ip, .. } => virtual_ip,
            Message::Error { reason } => return Err(FatalAuth(reason).into()),
            other => anyhow::bail!("unexpected first message: {other:?}"),
        },
        _ => anyhow::bail!("expected text welcome"),
    };
    tracing::info!("assigned virtual IP {vip}");
    if let Some(tx) = assigned.take() {
        let _ = tx.send(vip);
    }

    // STUN reflexive discovery over the shared socket, then publish candidates.
    let (stun_tx, stun_rx) = oneshot::channel();
    let _ = cmd_tx.try_send(Command::Stun { server, reply: stun_tx });
    let mut candidates = local_candidates(local_port);
    if let Ok(Some(reflexive)) = stun_rx.await {
        if !candidates.contains(&reflexive) {
            candidates.push(reflexive);
        }
        tracing::info!("STUN reflexive address {reflexive}");
    }
    sink.send(WsMessage::Text(proto::encode(&Message::Candidates { candidates })))
        .await?;

    let mut known: HashMap<String, PeerInfo> = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(t))) => {
                        if let Ok(Message::Peers { peers }) = proto::decode(&t) {
                            tracing::debug!("peers update: {} entries", peers.len());
                            sync_peers(cmd_tx, &mut known, peers, public_key);
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => { let _ = sink.send(WsMessage::Pong(p)).await; }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            _ = heartbeat.tick() => {
                sink.send(WsMessage::Text(proto::encode(&Message::Heartbeat))).await?;
            }
        }
    }
}

fn sync_peers(
    cmd_tx: &mpsc::Sender<Command>,
    known: &mut HashMap<String, PeerInfo>,
    peers: Vec<PeerInfo>,
    own_key: &str,
) {
    let mut seen = std::collections::HashSet::new();
    for p in peers {
        if p.public_key == own_key {
            continue;
        }
        seen.insert(p.public_key.clone());
        if known.get(&p.public_key) != Some(&p) {
            let _ = cmd_tx.try_send(Command::AddPeer(p.clone()));
            known.insert(p.public_key.clone(), p);
        }
    }
    let gone: Vec<String> = known.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    for k in gone {
        known.remove(&k);
        let _ = cmd_tx.try_send(Command::RemovePeer(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(key: &str, ip: u8) -> PeerInfo {
        PeerInfo {
            public_key: key.into(),
            virtual_ip: Ipv4Addr::new(10, 99, 0, ip),
            candidates: vec![],
        }
    }

    #[test]
    fn sync_adds_updates_removes_and_skips_self() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut known = HashMap::new();
        sync_peers(&tx, &mut known, vec![peer("self", 2), peer("a", 3)], "self");
        assert!(matches!(rx.try_recv(), Ok(Command::AddPeer(p)) if p.public_key == "a"));
        assert!(rx.try_recv().is_err());
        // unchanged peer produces no command
        sync_peers(&tx, &mut known, vec![peer("a", 3)], "self");
        assert!(rx.try_recv().is_err());
        // removal
        sync_peers(&tx, &mut known, vec![], "self");
        assert!(matches!(rx.try_recv(), Ok(Command::RemovePeer(k)) if k == "a"));
    }
}