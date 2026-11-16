//! Control-plane server: pure state plus the WebSocket/STUN runtime.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ara::proto::{self, Message, PeerInfo};
use ara::{stun, OFFLINE_AFTER_SECS, VIRTUAL_PREFIX, VIRTUAL_SUBNET};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

#[derive(Default)]
pub struct State {
    networks: HashMap<String, Network>,
}

struct Network {
    secret: String,
    peers: HashMap<String, Peer>,
    next_host: u16,
}

struct Peer {
    virtual_ip: Ipv4Addr,
    candidates: Vec<SocketAddr>,
    last_seen: Instant,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or re-register) a peer. Returns its virtual IP.
    pub fn register(
        &mut self,
        network: &str,
        secret: &str,
        public_key: &str,
        candidates: Vec<SocketAddr>,
    ) -> Result<Ipv4Addr, String> {
        let net = self
            .networks
            .entry(network.to_string())
            .or_insert_with(|| Network {
                secret: secret.to_string(),
                peers: HashMap::new(),
                next_host: 2,
            });
        if net.secret != secret {
            return Err("wrong network secret".into());
        }
        let vip = if let Some(p) = net.peers.get_mut(public_key) {
            p.candidates = candidates;
            p.last_seen = Instant::now();
            p.virtual_ip
        } else {
            if net.next_host > 254 {
                return Err("network full (253 peers)".into());
            }
            let mut o = VIRTUAL_SUBNET.octets();
            o[3] = net.next_host as u8;
            net.next_host += 1;
            let vip = Ipv4Addr::from(o);
            net.peers.insert(
                public_key.to_string(),
                Peer { virtual_ip: vip, candidates, last_seen: Instant::now() },
            );
            vip
        };
        Ok(vip)
    }

    pub fn update_candidates(&mut self, network: &str, public_key: &str, candidates: Vec<SocketAddr>) {
        if let Some(net) = self.networks.get_mut(network) {
            if let Some(p) = net.peers.get_mut(public_key) {
                p.candidates = candidates;
                p.last_seen = Instant::now();
            }
        }
    }

    pub fn heartbeat(&mut self, network: &str, public_key: &str) {
        if let Some(net) = self.networks.get_mut(network) {
            if let Some(p) = net.peers.get_mut(public_key) {
                p.last_seen = Instant::now();
            }
        }
    }

    pub fn remove(&mut self, network: &str, public_key: &str) {
        if let Some(net) = self.networks.get_mut(network) {
            net.peers.remove(public_key);
        }
    }

    /// Drop peers idle longer than OFFLINE_AFTER_SECS. Returns affected networks.
    pub fn sweep(&mut self) -> Vec<String> {
        let cutoff = Duration::from_secs(OFFLINE_AFTER_SECS);
        let mut changed = Vec::new();
        for (name, net) in &mut self.networks {
            let before = net.peers.len();
            net.peers.retain(|_, p| p.last_seen.elapsed() < cutoff);
            if net.peers.len() != before {
                changed.push(name.clone());
            }
        }
        changed
    }

    pub fn peers(&self, network: &str) -> Vec<PeerInfo> {
        let mut out: Vec<PeerInfo> = self
            .networks
            .get(network)
            .map(|net| {
                net.peers
                    .iter()
                    .map(|(k, p)| PeerInfo {
                        public_key: k.clone(),
                        virtual_ip: p.virtual_ip,
                        candidates: p.candidates.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|p| p.virtual_ip);
        out
    }

    pub fn prefix(&self) -> u8 {
        VIRTUAL_PREFIX
    }
}

type Bcast = broadcast::Sender<(String, String)>;

/// Run the control plane on an already-bound TCP listener and UDP socket.
pub async fn serve(listener: TcpListener, udp: Arc<UdpSocket>) -> Result<()> {
    let state = Arc::new(Mutex::new(State::new()));
    let (bcast, _) = broadcast::channel::<(String, String)>(256);

    {
        let udp = udp.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                match udp.recv_from(&mut buf).await {
                    Ok((n, src)) => {
                        if let Some(txid) = stun::parse_request(&buf[..n]) {
                            tracing::debug!("stun request from {src}");
                            let resp = stun::encode_response(&txid, src);
                            let _ = udp.send_to(&resp, src).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("stun recv: {e}");
                        break;
                    }
                }
            }
        });
    }

    {
        let state = state.clone();
        let bcast = bcast.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let changed = state.lock().await.sweep();
                for net in changed {
                    broadcast_peers(&state, &bcast, &net).await;
                }
            }
        });
    }

    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();
        let bcast = bcast.clone();
        tokio::spawn(async move {
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => {
                    if let Err(e) = handle_conn(ws, state, bcast).await {
                        tracing::debug!("conn {addr} ended: {e}");
                    }
                }
                Err(e) => tracing::debug!("ws handshake {addr}: {e}"),
            }
        });
    }
}

async fn broadcast_peers(state: &Arc<Mutex<State>>, bcast: &Bcast, network: &str) {
    let peers = state.lock().await.peers(network);
    let msg = proto::encode(&Message::Peers { peers });
    let _ = bcast.send((network.to_string(), msg));
}

async fn handle_conn(
    ws: WebSocketStream<tokio::net::TcpStream>,
    state: Arc<Mutex<State>>,
    bcast: Bcast,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();

    let first = stream
        .next()
        .await
        .context("closed before register")?
        .context("ws read")?;
    let WsMessage::Text(text) = first else {
        anyhow::bail!("expected text register");
    };
    let Message::Register { network, secret, public_key, candidates } = proto::decode(&text)?
    else {
        let _ = sink
            .send(WsMessage::Text(proto::encode(&Message::Error {
                reason: "expected register".into(),
            })))
            .await;
        anyhow::bail!("expected register");
    };

    let mut rx = bcast.subscribe();
    let vip = {
        let mut st = state.lock().await;
        match st.register(&network, &secret, &public_key, candidates) {
            Ok(vip) => vip,
            Err(reason) => {
                let _ = sink
                    .send(WsMessage::Text(proto::encode(&Message::Error { reason })))
                    .await;
                anyhow::bail!("register rejected");
            }
        }
    };
    let prefix = state.lock().await.prefix();
    sink.send(WsMessage::Text(proto::encode(&Message::Welcome {
        virtual_ip: vip,
        prefix,
    })))
    .await?;
    broadcast_peers(&state, &bcast, &network).await;
    tracing::info!("peer {public_key} joined {network} as {vip}");

    let result = loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => match proto::decode(&text) {
                        Ok(Message::Candidates { candidates }) => {
                            state.lock().await.update_candidates(&network, &public_key, candidates);
                            broadcast_peers(&state, &bcast, &network).await;
                        }
                        Ok(Message::Heartbeat) => {
                            state.lock().await.heartbeat(&network, &public_key);
                        }
                        _ => {}
                    },
                    Some(Ok(WsMessage::Ping(p))) => { let _ = sink.send(WsMessage::Pong(p)).await; }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(anyhow::anyhow!(e)),
                    None => break Ok(()),
                }
            }
            b = rx.recv() => {
                match b {
                    Ok((net, text)) if net == network => {
                        if sink.send(WsMessage::Text(text)).await.is_err() { break Ok(()); }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
        }
    };

    state.lock().await.remove(&network, &public_key);
    broadcast_peers(&state, &bcast, &network).await;
    tracing::info!("peer {public_key} left {network}");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands() -> Vec<SocketAddr> {
        vec!["1.2.3.4:5".parse().unwrap()]
    }

    #[test]
    fn sequential_allocation_and_reclaim() {
        let mut s = State::new();
        let a = s.register("n", "sec", "keyA", cands()).unwrap();
        let b = s.register("n", "sec", "keyB", cands()).unwrap();
        assert_eq!(a, Ipv4Addr::new(10, 99, 0, 2));
        assert_eq!(b, Ipv4Addr::new(10, 99, 0, 3));
        // re-register A keeps its IP
        let a2 = s.register("n", "sec", "keyA", cands()).unwrap();
        assert_eq!(a, a2);
        assert_eq!(s.peers("n").len(), 2);
    }

    #[test]
    fn wrong_secret_rejected() {
        let mut s = State::new();
        s.register("n", "sec", "keyA", cands()).unwrap();
        assert!(s.register("n", "other", "keyB", cands()).is_err());
    }

    #[test]
    fn separate_networks_isolated() {
        let mut s = State::new();
        let a = s.register("n1", "s1", "k", cands()).unwrap();
        let b = s.register("n2", "s2", "k", cands()).unwrap();
        assert_eq!(a, b); // each network starts at .2
        assert_eq!(s.peers("n1").len(), 1);
        assert_eq!(s.peers("n2").len(), 1);
    }

    #[test]
    fn remove_and_sweep() {
        let mut s = State::new();
        s.register("n", "sec", "keyA", cands()).unwrap();
        s.remove("n", "keyA");
        assert_eq!(s.peers("n").len(), 0);
        s.register("n", "sec", "keyB", cands()).unwrap();
        // fresh peer survives sweep
        assert!(s.sweep().is_empty());
        assert_eq!(s.peers("n").len(), 1);
    }

    #[test]
    fn candidates_update() {
        let mut s = State::new();
        s.register("n", "sec", "k", cands()).unwrap();
        let new: Vec<SocketAddr> = vec!["9.9.9.9:1".parse().unwrap()];
        s.update_candidates("n", "k", new.clone());
        assert_eq!(s.peers("n")[0].candidates, new);
    }
}