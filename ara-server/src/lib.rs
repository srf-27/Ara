//! Control-plane server: pure state plus the WebSocket/STUN runtime.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ara::proto::{self, Message, PeerInfo, RoomInfo};
use ara::{stun, OFFLINE_AFTER_SECS, VIRTUAL_PREFIX, VIRTUAL_SUBNET};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

pub const ROOM_LIMIT: u8 = 8;

#[derive(Default)]
pub struct State {
    rooms: HashMap<String, Room>,
    tokens: HashMap<String, TokenInfo>,
    banned: HashSet<String>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Password {
    #[serde(default)]
    salt: String,
    #[serde(default)]
    hash: String,
}

impl Password {
    fn new(pw: &str) -> Self {
        if pw.is_empty() {
            return Self::default();
        }
        let salt = random_hex(16);
        Self { hash: hash_password(pw, &salt), salt }
    }

    fn verify(&self, pw: &str) -> bool {
        self.hash.is_empty() || hash_password(pw, &self.salt) == self.hash
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct TokenInfo {
    #[serde(default)]
    created: String,
    #[serde(default)]
    revoked: bool,
}

struct Room {
    password: Password,
    creator_id: String,
    creator_name: String,
    peers: HashMap<String, Peer>,
    allocated: HashMap<String, Ipv4Addr>,
    next_host: u16,
}

struct Peer {
    virtual_ip: Ipv4Addr,
    candidates: Vec<SocketAddr>,
    tcp: Vec<SocketAddr>,
    symmetric: bool,
    nickname: String,
    last_seen: Instant,
}

fn hash_password(pw: &str, salt: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(b":");
    h.update(pw.as_bytes());
    format!("{:x}", h.finalize())
}

fn random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn random_code() -> String {
    use rand::Rng;
    const CH: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut r = rand::thread_rng();
    (0..6).map(|_| CH[r.gen_range(0..CH.len())] as char).collect()
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_tokens(&mut self, path: &Path) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(tokens) = serde_json::from_str::<HashMap<String, TokenInfo>>(&text) {
                self.tokens = tokens;
            }
        }
    }

    pub fn save_tokens(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.tokens)?;
        std::fs::write(path, json).with_context(|| format!("write {}", path.display()))
    }

    pub fn gen_token(&mut self) -> String {
        let token = random_hex(24);
        self.tokens.insert(
            token.clone(),
            TokenInfo { created: format!("{:?}", Instant::now()), revoked: false },
        );
        token
    }

    pub fn verify_token(&self, token: &str) -> bool {
        self.tokens.is_empty() || self.tokens.get(token).is_some_and(|t| !t.revoked)
    }

    pub fn is_banned(&self, public_key: &str) -> bool {
        self.banned.contains(public_key)
    }

    /// Create a room with a generated short code; returns the code.
    pub fn create_room(&mut self, password: &str, creator_id: &str, creator_name: &str) -> Result<String, String> {
        let mut code = random_code();
        while self.rooms.contains_key(&code) {
            code = random_code();
        }
        self.rooms.insert(
            code.clone(),
            Room {
                password: Password::new(password),
                creator_id: creator_id.to_string(),
                creator_name: creator_name.to_string(),
                peers: HashMap::new(),
                allocated: HashMap::new(),
                next_host: 1,
            },
        );
        Ok(code)
    }

    /// Join (or implicitly create, legacy behaviour) a room. Returns the VIP.
    #[allow(clippy::too_many_arguments)]
    pub fn join_room(
        &mut self,
        code: &str,
        password: &str,
        public_key: &str,
        nickname: &str,
        candidates: Vec<SocketAddr>,
        tcp: Vec<SocketAddr>,
        symmetric: bool,
    ) -> Result<Ipv4Addr, String> {
        if self.banned.contains(public_key) {
            return Err("id banned by server".into());
        }
        let room = self.rooms.entry(code.to_string()).or_insert_with(|| Room {
            password: Password::new(password),
            creator_id: public_key.to_string(),
            creator_name: nickname.to_string(),
            peers: HashMap::new(),
            allocated: HashMap::new(),
            next_host: 1,
        });
        if !room.password.verify(password) {
            return Err("wrong room password".into());
        }
        if !room.peers.contains_key(public_key) && room.peers.len() >= ROOM_LIMIT as usize {
            return Err(format!("room full ({ROOM_LIMIT} peers)"));
        }
        let vip = match room.allocated.get(public_key) {
            Some(vip) => *vip,
            None => {
                if room.next_host > 254 {
                    return Err("room address space exhausted".into());
                }
                let mut o = VIRTUAL_SUBNET.octets();
                o[3] = room.next_host as u8;
                room.next_host += 1;
                let vip = Ipv4Addr::from(o);
                room.allocated.insert(public_key.to_string(), vip);
                vip
            }
        };
        room.peers.insert(
            public_key.to_string(),
            Peer {
                virtual_ip: vip,
                candidates,
                tcp,
                symmetric,
                nickname: nickname.to_string(),
                last_seen: Instant::now(),
            },
        );
        Ok(vip)
    }

    /// Legacy M1/M2 registration: network name acts as the room code and the
    /// shared secret as its password.
    pub fn register(
        &mut self,
        network: &str,
        secret: &str,
        public_key: &str,
        candidates: Vec<SocketAddr>,
        tcp: Vec<SocketAddr>,
        symmetric: bool,
    ) -> Result<Ipv4Addr, String> {
        self.join_room(network, secret, public_key, "", candidates, tcp, symmetric)
    }

    pub fn room_list(&self) -> Vec<RoomInfo> {
        let mut out: Vec<RoomInfo> = self
            .rooms
            .iter()
            .map(|(code, r)| RoomInfo {
                code: code.clone(),
                creator: r.creator_name.clone(),
                creator_id: r.creator_id.clone(),
                members: r.peers.len().min(u8::MAX as usize) as u8,
                limit: ROOM_LIMIT,
            })
            .collect();
        out.sort_by(|a, b| a.code.cmp(&b.code));
        out
    }

    pub fn room_of(&self, public_key: &str) -> Option<String> {
        self.rooms
            .iter()
            .find(|(_, r)| r.peers.contains_key(public_key))
            .map(|(code, _)| code.clone())
    }

    pub fn kick_room(&mut self, code: &str) -> Vec<String> {
        self.rooms
            .remove(code)
            .map(|r| r.peers.into_keys().collect())
            .unwrap_or_default()
    }

    pub fn kick_peer(&mut self, public_key: &str) -> Vec<String> {
        self.banned.insert(public_key.to_string());
        let mut hit = Vec::new();
        for (code, room) in &mut self.rooms {
            if room.peers.remove(public_key).is_some() {
                hit.push(code.clone());
            }
        }
        hit
    }

    pub fn update_candidates(
        &mut self,
        code: &str,
        public_key: &str,
        candidates: Vec<SocketAddr>,
        tcp: Vec<SocketAddr>,
        symmetric: bool,
    ) {
        if let Some(room) = self.rooms.get_mut(code) {
            if let Some(p) = room.peers.get_mut(public_key) {
                p.candidates = candidates;
                p.tcp = tcp;
                p.symmetric = symmetric;
                p.last_seen = Instant::now();
            }
        }
    }

    pub fn heartbeat(&mut self, code: &str, public_key: &str) {
        if let Some(room) = self.rooms.get_mut(code) {
            if let Some(p) = room.peers.get_mut(public_key) {
                p.last_seen = Instant::now();
            }
        }
    }

    pub fn remove(&mut self, code: &str, public_key: &str) {
        if let Some(room) = self.rooms.get_mut(code) {
            room.peers.remove(public_key);
        }
    }

    /// Drop peers idle longer than OFFLINE_AFTER_SECS. Returns affected rooms.
    pub fn sweep(&mut self) -> Vec<String> {
        let cutoff = Duration::from_secs(OFFLINE_AFTER_SECS);
        let mut changed = Vec::new();
        for (code, room) in &mut self.rooms {
            let before = room.peers.len();
            room.peers.retain(|_, p| p.last_seen.elapsed() < cutoff);
            if room.peers.len() != before {
                changed.push(code.clone());
            }
        }
        changed
    }

    pub fn peers(&self, code: &str) -> Vec<PeerInfo> {
        let mut out: Vec<PeerInfo> = self
            .rooms
            .get(code)
            .map(|room| {
                room.peers
                    .iter()
                    .map(|(k, p)| PeerInfo {
                        public_key: k.clone(),
                        virtual_ip: p.virtual_ip,
                        candidates: p.candidates.clone(),
                        tcp: p.tcp.clone(),
                        symmetric: p.symmetric,
                        nickname: p.nickname.clone(),
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
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        &mut self,
        code: &str,
        password_salt: &str,
        password_hash: &str,
        creator_id: &str,
        creator_name: &str,
        public_key: &str,
        vip: Ipv4Addr,
    ) {
        let room = self.rooms.entry(code.to_string()).or_insert_with(|| Room {
            password: Password {
                salt: password_salt.to_string(),
                hash: password_hash.to_string(),
            },
            creator_id: creator_id.to_string(),
            creator_name: creator_name.to_string(),
            peers: HashMap::new(),
            allocated: HashMap::new(),
            next_host: 1,
        });
        let host = vip.octets()[3] as u16;
        if host >= room.next_host {
            room.next_host = host + 1;
        }
        room.allocated.insert(public_key.to_string(), vip);
    }

    pub fn allocations(&self) -> Vec<Alloc> {
        let mut out = Vec::new();
        for (code, room) in &self.rooms {
            for (key, vip) in &room.allocated {
                out.push(Alloc {
                    network: code.clone(),
                    password_salt: room.password.salt.clone(),
                    password_hash: room.password.hash.clone(),
                    creator_id: room.creator_id.clone(),
                    creator_name: room.creator_name.clone(),
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
    #[serde(default)]
    pub password_salt: String,
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub creator_id: String,
    #[serde(default)]
    pub creator_name: String,
    pub public_key: String,
    pub virtual_ip: Ipv4Addr,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct KickRequest {
    pub room: Option<String>,
    pub id: Option<String>,
}

type Bcast = broadcast::Sender<(String, String)>;
type KickTx = broadcast::Sender<(String, String)>;

/// Run the control plane on an already-bound TCP listener and optional UDP
/// socket (None = TCP-only signaling, clients use public STUN).
/// `state_path` persists virtual-IP allocations; `access.json` and
/// `kick.json` live next to it.
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
                    st.restore(
                        &a.network,
                        &a.password_salt,
                        &a.password_hash,
                        &a.creator_id,
                        &a.creator_name,
                        &a.public_key,
                        a.virtual_ip,
                    );
                }
            }
        }
        let access = path.with_file_name("access.json");
        state.lock().await.load_tokens(&access);
    }
    let (bcast, _) = broadcast::channel::<(String, String)>(256);
    let (kick_tx, _) = broadcast::channel::<(String, String)>(64);
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
        let kick_tx = kick_tx.clone();
        let state_path = state_path.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                for room in state.lock().await.sweep() {
                    broadcast_peers(&state, &bcast, &room).await;
                }
                if let Some(path) = &state_path {
                    let kick_path = path.with_file_name("kick.json");
                    if let Ok(text) = std::fs::read_to_string(&kick_path) {
                        if let Ok(req) = serde_json::from_str::<KickRequest>(&text) {
                            let mut st = state.lock().await;
                            let mut hit = Vec::new();
                            if let Some(room) = req.room {
                                hit.extend(st.kick_room(&room));
                            }
                            if let Some(id) = req.id {
                                hit.extend(st.kick_peer(&id));
                            }
                            drop(st);
                            for key in hit {
                                let _ = kick_tx.send((key, "kicked by server".into()));
                            }
                            let _ = std::fs::remove_file(&kick_path);
                        }
                    }
                }
            }
        });
    }

    loop {
        let (stream, addr) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let bcast = bcast.clone();
        let kick_tx = kick_tx.clone();
        let registry = registry.clone();
        let state_path = state_path.clone();
        tokio::spawn(async move {
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => {
                    if let Err(e) =
                        handle_conn(ws, state, bcast, kick_tx, registry, state_path).await
                    {
                        tracing::debug!("conn {addr} ended: {e}");
                    }
                }
                Err(e) => tracing::debug!("ws handshake {addr}: {e}"),
            }
        });
    }
}

type Registry = Arc<Mutex<HashMap<String, (String, tokio::sync::mpsc::UnboundedSender<Vec<u8>>)>>>;

async fn save_state(state: &Arc<Mutex<State>>, path: &Option<std::path::PathBuf>) {
    if let Some(path) = path {
        let allocs = state.lock().await.allocations();
        if let Ok(json) = serde_json::to_string(&allocs) {
            let _ = std::fs::write(path, json);
        }
    }
}

async fn broadcast_peers(state: &Arc<Mutex<State>>, bcast: &Bcast, code: &str) {
    let peers = state.lock().await.peers(code);
    let msg = proto::encode(&Message::Peers { peers });
    let _ = bcast.send((code.to_string(), msg));
}

#[allow(clippy::too_many_arguments)]
async fn handle_conn(
    ws: WebSocketStream<tokio::net::TcpStream>,
    state: Arc<Mutex<State>>,
    bcast: Bcast,
    kick_tx: KickTx,
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
    let Message::Register {
        network,
        secret,
        public_key,
        candidates,
        tcp,
        symmetric,
        room_code,
        room_password,
        access_token,
        nickname,
        create_room,
    } = proto::decode(&text)?
    else {
        let _ = sink
            .send(WsMessage::Text(proto::encode(&Message::Error {
                reason: "expected register".into(),
            })))
            .await;
        anyhow::bail!("expected register");
    };

    if !state.lock().await.verify_token(&access_token) {
        let _ = sink
            .send(WsMessage::Text(proto::encode(&Message::Error {
                reason: "invalid access token".into(),
            })))
            .await;
        anyhow::bail!("invalid access token");
    }

    if room_code.is_empty() && !create_room && !network.is_empty() {
        // Legacy M1/M2 clients: network/secret map onto room code/password.
        let code = network.clone();
        let vip = {
            let mut st = state.lock().await;
            match st.join_room(&code, &secret, &public_key, "", candidates, tcp, symmetric) {
                Ok(v) => v,
                Err(reason) => {
                    let _ = sink
                        .send(WsMessage::Text(proto::encode(&Message::Error { reason })))
                        .await;
                    anyhow::bail!("register rejected");
                }
            }
        };
        return finish_conn(
            sink, stream, state, bcast, kick_tx, registry, state_path, code, public_key, vip,
        )
        .await;
    }

    if room_code.is_empty() && !create_room {
        let rooms = state.lock().await.room_list();
        sink.send(WsMessage::Text(proto::encode(&Message::RoomList { rooms }))).await?;
        return Ok(());
    }

    let code = if create_room {
        let mut st = state.lock().await;
        match st.create_room(&room_password, &public_key, &nickname) {
            Ok(code) => code,
            Err(reason) => {
                let _ = sink
                    .send(WsMessage::Text(proto::encode(&Message::Error { reason })))
                    .await;
                anyhow::bail!("create room rejected");
            }
        }
    } else {
        room_code.clone()
    };

    let vip = {
        let mut st = state.lock().await;
        match st.join_room(
            &code,
            &room_password,
            &public_key,
            &nickname,
            candidates,
            tcp,
            symmetric,
        ) {
            Ok(v) => v,
            Err(reason) => {
                let _ = sink
                    .send(WsMessage::Text(proto::encode(&Message::Error { reason })))
                    .await;
                anyhow::bail!("join rejected");
            }
        }
    };

    finish_conn(
        sink, stream, state, bcast, kick_tx, registry, state_path, code, public_key, vip,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_conn(
    mut sink: futures_util::stream::SplitSink<
        WebSocketStream<tokio::net::TcpStream>,
        WsMessage,
    >,
    mut stream: futures_util::stream::SplitStream<WebSocketStream<tokio::net::TcpStream>>,
    state: Arc<Mutex<State>>,
    bcast: Bcast,
    kick_tx: KickTx,
    registry: Registry,
    state_path: Option<std::path::PathBuf>,
    code: String,
    public_key: String,
    vip: Ipv4Addr,
) -> Result<()> {
    let mut rx = bcast.subscribe();
    let mut kick_rx = kick_tx.subscribe();
    let prefix = state.lock().await.prefix();
    sink.send(WsMessage::Text(proto::encode(&Message::Welcome {
        virtual_ip: vip,
        prefix,
        room_code: code.clone(),
    })))
    .await?;
    broadcast_peers(&state, &bcast, &code).await;
    save_state(&state, &state_path).await;
    let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    registry
        .lock()
        .await
        .insert(public_key.clone(), (code.clone(), relay_tx));
    tracing::info!("peer {public_key} joined room {code} as {vip}");

    let result = loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => match proto::decode(&text) {
                        Ok(Message::Candidates { candidates, tcp, symmetric }) => {
                            state.lock().await.update_candidates(&code, &public_key, candidates, tcp, symmetric);
                            broadcast_peers(&state, &bcast, &code).await;
                        }
                        Ok(Message::Heartbeat) => {
                            state.lock().await.heartbeat(&code, &public_key);
                        }
                        _ => {}
                    },
                    Some(Ok(WsMessage::Binary(b))) => {
                        if b.len() > 32 {
                            use base64::Engine as _;
                            let dest = base64::engine::general_purpose::STANDARD.encode(&b[..32]);
                            let reg = registry.lock().await;
                            if let Some((dest_room, tx)) = reg.get(&dest) {
                                if dest_room == &code {
                                    let _ = tx.send(b[32..].to_vec());
                                }
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
                    Ok((room, text)) if room == code => {
                        if sink.send(WsMessage::Text(text)).await.is_err() { break Ok(()); }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
            k = kick_rx.recv() => {
                match k {
                    Ok((key, reason)) if key == public_key => {
                        let _ = sink.send(WsMessage::Text(proto::encode(&Message::Kicked { reason }))).await;
                        break Ok(());
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
        }
    };

    registry.lock().await.remove(&public_key);
    state.lock().await.remove(&code, &public_key);
    broadcast_peers(&state, &bcast, &code).await;
    save_state(&state, &state_path).await;
    tracing::info!("peer {public_key} left room {code}");
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
        let a2 = s.register("n", "sec", "keyA", cands(), vec![], false).unwrap();
        assert_eq!(a, a2);
        assert_eq!(s.peers("n").len(), 2);
    }

    #[test]
    fn wrong_password_rejected() {
        let mut s = State::new();
        s.create_room("pw", "keyA", "alice").unwrap();
        let code = s.room_list()[0].code.clone();
        assert!(s.join_room(&code, "wrong", "keyB", "", cands(), vec![], false).is_err());
        assert!(s.join_room(&code, "pw", "keyB", "", cands(), vec![], false).is_ok());
    }

    #[test]
    fn room_limit_is_eight() {
        let mut s = State::new();
        let code = s.create_room("pw", "creator", "alice").unwrap();
        for i in 0..ROOM_LIMIT {
            s.join_room(&code, "pw", &format!("k{i}"), "", cands(), vec![], false)
                .unwrap();
        }
        assert!(s.join_room(&code, "pw", "k8", "", cands(), vec![], false).is_err());
    }

    #[test]
    fn rooms_are_isolated() {
        let mut s = State::new();
        let a = s.create_room("pw", "ka", "alice").unwrap();
        let b = s.create_room("pw", "kb", "bob").unwrap();
        s.join_room(&a, "pw", "ka", "", cands(), vec![], false).unwrap();
        s.join_room(&b, "pw", "kb", "", cands(), vec![], false).unwrap();
        assert_eq!(s.peers(&a).len(), 1);
        assert_eq!(s.peers(&b).len(), 1);
        assert_eq!(s.room_list().len(), 2);
    }

    #[test]
    fn kick_room_and_peer() {
        let mut s = State::new();
        let code = s.create_room("pw", "ka", "alice").unwrap();
        s.join_room(&code, "pw", "ka", "", cands(), vec![], false).unwrap();
        s.join_room(&code, "pw", "kb", "", cands(), vec![], false).unwrap();
        let hit = s.kick_room(&code);
        assert_eq!(hit.len(), 2);
        assert!(s.peers(&code).is_empty());

        let code2 = s.create_room("pw", "ka", "alice").unwrap();
        s.join_room(&code2, "pw", "ka", "", cands(), vec![], false).unwrap();
        assert!(s.kick_peer("ka").contains(&code2));
        assert!(s.is_banned("ka"));
        assert!(s.join_room(&code2, "pw", "ka", "", cands(), vec![], false).is_err());
    }

    #[test]
    fn access_tokens_gate_registration() {
        let mut s = State::new();
        assert!(s.verify_token("anything")); // no tokens configured -> legacy open
        let token = s.gen_token();
        assert!(s.verify_token(&token));
        assert!(!s.verify_token("bogus"));
        s.tokens.get_mut(&token).unwrap().revoked = true;
        assert!(!s.verify_token(&token));
    }

    #[test]
    fn merge_from_restore_keeps_password() {
        let mut s = State::new();
        s.restore("AB12CD", "salt", &hash_password("pw", "salt"), "ka", "alice", "ka", Ipv4Addr::new(10, 99, 0, 1));
        assert!(s.join_room("AB12CD", "pw", "kb", "", cands(), vec![], false).is_ok());
        assert!(s.join_room("AB12CD", "bad", "kc", "", cands(), vec![], false).is_err());
    }

    #[test]
    fn nickname_roundtrips() {
        let mut s = State::new();
        let code = s.create_room("pw", "ka", "alice").unwrap();
        s.join_room(&code, "pw", "ka", "Alice", cands(), vec![], false).unwrap();
        assert_eq!(s.peers(&code)[0].nickname, "Alice");
    }
}
