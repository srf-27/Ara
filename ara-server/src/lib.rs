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
    allocated: HashMap<String, Ipv4Addr>,
    next_host: u16,
}

struct Peer {
    virtual_ip: Ipv4Addr,
    candidates: Vec<SocketAddr>,
    tcp: Vec<SocketAddr>,
    symmetric: bool,
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
        tcp: Vec<SocketAddr>,
        symmetric: bool,
    ) -> Result<Ipv4Addr, String> {
        let net = self
            .networks
            .entry(network.to_string())
            .or_insert_with(|| Network {
                secret: secret.to_string(),
                peers: HashMap::new(),
                allocated: HashMap::new(),
                next_host: 1,
            });
        if net.secret != secret {
            return Err("wrong network secret".into());
        }
        let vip = match net.allocated.get(public_key) {
            Some(vip) => *vip,
            None => {
                if net.next_host > 254 {
                    return Err("network full (254 peers)".into());
                }
                let mut o = VIRTUAL_SUBNET.octets();
                o[3] = net.next_host as u8;
                net.next_host += 1;
                let vip = Ipv4Addr::from(o);
                net.allocated.insert(public_key.to_string(), vip);
                vip
            }
        };
        net.peers.insert(
            public_key.to_string(),
            Peer { virtual_ip: vip, candidates, tcp, symmetric, last_seen: Instant::now() },
        );
        Ok(vip)
    }

    pub fn update_candidates(&mut self, network: &str, public_key: &str, candidates: Vec<SocketAddr>, tcp: Vec<SocketAddr>, symmetric: bool) {
        if let Some(net) = self.networks.get_mut(network) {
            if let Some(p) = net.peers.get_mut(public_key) {
                p.candidates = candidates;
                p.tcp = tcp;
                p.symmetric = symmetric;
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
                        tcp: p.tcp.clone(),
                        symmetric: p.symmetric,
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

    /// Re-insert a persisted allocation (survives server restarts).
    pub fn restore(&mut self, network: &str, secret: &str, public_key: &str, vip: Ipv4Addr) {
        let net = self
            .networks
            .entry(network.to_string())
            .or_insert_with(|| Network {
                secret: secret.to_string(),
                peers: HashMap::new(),
                allocated: HashMap::new(),
                next_host: 1,
            });
        let host = vip.octets()[3] as u16;
        if host >= net.next_host {
            net.next_host = host + 1;
        }
        net.allocated.insert(public_key.to_string(), vip);
    }

    pub fn allocations(&self) -> Vec<Alloc> {
        let mut out = Vec::new();
        for (name, net) in &self.networks {
            for (key, vip) in &net.allocated {
                out.push(Alloc {
                    network: name.clone(),
                    secret: net.secret.clone(),
                    public_key: key.clone(),
                    virtual_ip: *vip,
                });
            }
        }
        out
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Alloc {
    pub network: String,
    pub secret: String,
    pub public_key: String,
    pub virtual_ip: Ipv4Addr,
}

type Bcast = broadcast::Sender<(String, String)>;

/// Run the control plane on an already-bound TCP listener and optional UDP
/// socket (None = TCP-only signaling, clients use public STUN).
/// `state_path` persists virtual-IP allocations across restarts.
pub async fn serve(
    listener: TcpListener,
    udp: Option<Arc<UdpSocket>>,
    state_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let state = Arc::new(Mutex::new(State::new()));
    if let Some(path) = &state_path {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(allocs) = serde_json::from_str::<Vec<Alloc>>(&text) {
                let mut st = state.lock().await;
                for a in allocs {
                    st.restore(&a.network, &a.secret, &a.public_key, a.virtual_ip);
                }
            }
        }
    }
    let (bcast, _) = broadcast::channel::<(String, String)>(256);
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));

    if let Some(udp) = udp {
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
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let bcast = bcast.clone();
        let registry = registry.clone();
        let state_path = state_path.clone();
        tokio::spawn(async move {
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => {
                    if let Err(e) = handle_conn(ws, state, bcast, registry, state_path).await {
                        tracing::debug!("conn {addr} ended: {e}");
                    }
                }
                Err(e) => tracing::debug!("ws handshake {addr}: {e}"),
            }
        });
    }
}

type Registry = Arc<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>;

async fn save_state(state: &Arc<Mutex<State>>, path: &Option<std::path::PathBuf>) {
    if let Some(path) = path {
        let allocs = state.lock().await.allocations();
        if let Ok(json) = serde_json::to_string(&allocs) {
            let _ = std::fs::write(path, json);
        }
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
    registry: Registry,
    state_path: Option<std::path::PathBuf>,
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
    let Message::Register { network, secret, public_key, candidates, tcp, symmetric } = proto::decode(&text)?
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
        match st.register(&network, &secret, &public_key, candidates, tcp, symmetric) {
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
    save_state(&state, &state_path).await;
    let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    registry.lock().await.insert(public_key.clone(), relay_tx);
    tracing::info!("peer {public_key} joined {network} as {vip}");

    let result = loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => match proto::decode(&text) {
                        Ok(Message::Candidates { candidates, tcp, symmetric }) => {
                            state.lock().await.update_candidates(&network, &public_key, candidates, tcp, symmetric);
                            broadcast_peers(&state, &bcast, &network).await;
                        }
                        Ok(Message::Heartbeat) => {
                            state.lock().await.heartbeat(&network, &public_key);
                        }
                        _ => {}
                    },
                    Some(Ok(WsMessage::Binary(b))) => {
                        // Relay frame: [32-byte dest public key][WG packet].
                        if b.len() > 32 {
                            use base64::Engine as _;
                            let dest = base64::engine::general_purpose::STANDARD.encode(&b[..32]);
                            if let Some(tx) = registry.lock().await.get(&dest) {
                                let _ = tx.send(b[32..].to_vec());
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => { let _ = sink.send(WsMessage::Pong(p)).await; }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(anyhow::anyhow!(e)),
                    None => break Ok(()),
                }
            }
            Some(bytes) = relay_rx.recv() => {
                if sink.send(WsMessage::Binary(bytes)).await.is_err() { break Ok(()); }
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

    registry.lock().await.remove(&public_key);
    state.lock().await.remove(&network, &public_key);
    broadcast_peers(&state, &bcast, &network).await;
    save_state(&state, &state_path).await;
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
        let a = s.register("n", "sec", "keyA", cands(), vec![], false).unwrap();
        let b = s.register("n", "sec", "keyB", cands(), vec![], false).unwrap();
        assert_eq!(a, Ipv4Addr::new(10, 99, 0, 1));
        assert_eq!(b, Ipv4Addr::new(10, 99, 0, 2));
        // re-register A keeps its IP
        let a2 = s.register("n", "sec", "keyA", cands(), vec![], false).unwrap();
        assert_eq!(a, a2);
        assert_eq!(s.peers("n").len(), 2);
    }

    #[test]
    fn wrong_secret_rejected() {
        let mut s = State::new();
        s.register("n", "sec", "keyA", cands(), vec![], false).unwrap();
        assert!(s.register("n", "other", "keyB", cands(), vec![], false).is_err());
    }

    #[test]
    fn separate_networks_isolated() {
        let mut s = State::new();
        let a = s.register("n1", "s1", "k", cands(), vec![], false).unwrap();
        let b = s.register("n2", "s2", "k", cands(), vec![], false).unwrap();
        assert_eq!(a, b); // each network starts at .2
        assert_eq!(s.peers("n1").len(), 1);
        assert_eq!(s.peers("n2").len(), 1);
    }

    #[test]
    fn remove_and_sweep() {
        let mut s = State::new();
        s.register("n", "sec", "keyA", cands(), vec![], false).unwrap();
        s.remove("n", "keyA");
        assert_eq!(s.peers("n").len(), 0);
        s.register("n", "sec", "keyB", cands(), vec![], false).unwrap();
        // fresh peer survives sweep
        assert!(s.sweep().is_empty());
        assert_eq!(s.peers("n").len(), 1);
    }

    #[test]
    fn candidates_update() {
        let mut s = State::new();
        s.register("n", "sec", "k", cands(), vec![], false).unwrap();
        let new: Vec<SocketAddr> = vec!["9.9.9.9:1".parse().unwrap()];
        s.update_candidates("n", "k", new.clone(), vec![], false);
        assert_eq!(s.peers("n")[0].candidates, new);
    }

    #[test]
    fn symmetric_flag_roundtrips() {
        let mut s = State::new();
        s.register("n", "sec", "k", cands(), vec![], true).unwrap();
        assert!(s.peers("n")[0].symmetric);
        s.update_candidates("n", "k", cands(), vec![], false);
        assert!(!s.peers("n")[0].symmetric);
    }
}