//! M1 loopback integration: server + two logical clients (no TUN), verifying
//! register -> virtual IP -> hole punch -> encrypted IP delivery both ways.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ara::config::ResolvedConfig;
use ara::engine::bind_udp;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

fn ip_packet(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let total = (20 + payload.len()) as u16;
    let mut p = vec![0u8; 20];
    p[0] = 0x45; // version 4, IHL 5
    p[2..4].copy_from_slice(&total.to_be_bytes());
    p[8] = 64; // TTL
    p[9] = 17; // UDP
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    p.extend_from_slice(payload);
    p
}

struct Client {
    vip: Ipv4Addr,
    to_engine: mpsc::Sender<Vec<u8>>,
    from_engine: mpsc::Receiver<Vec<u8>>,
}

async fn start_client(server_port: u16, tag: &str) -> Client {
    let cfg = ResolvedConfig {
        network: "test-net".into(),
        secret: "test-secret".into(),
        server_host: "127.0.0.1".into(),
        server_port,
        local_port: 0,
        tcp_port: 0,
        identity_path: Some(std::env::temp_dir().join(format!("ara-test-{tag}.key"))),
        room_code: String::new(),
        room_password: String::new(),
        access_token: String::new(),
        nickname: String::new(),
        create_room: false,
    };
    let (to_engine, engine_out) = mpsc::channel(256);
    let (engine_in, from_engine) = mpsc::channel(256);
    let (tx, rx) = oneshot::channel();
    tokio::spawn(ara::client::run(cfg, engine_out, engine_in, tx, None));
    let vip = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("assigned IP timeout")
        .expect("assigned channel");
    Client { vip, to_engine, from_engine }
}

async fn expect_packet(rx: &mut mpsc::Receiver<Vec<u8>>, want: &[u8]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for packet");
        }
        match tokio::time::timeout(remaining.min(Duration::from_millis(500)), rx.recv()).await {
            Ok(Some(got)) => {
                if got == want {
                    return;
                }
            }
            Ok(None) => panic!("channel closed"),
            Err(_) => {} // retry window; sender keeps retransmitting
        }
    }
}

#[tokio::test]
async fn two_clients_exchange_ip_packets() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ara=debug,ara_server=debug")
        .with_test_writer()
        .try_init();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let udp = Arc::new(bind_udp(port).unwrap());
    tokio::spawn(ara_server::serve(listener, Some(udp), None));

    let mut a = start_client(port, "a").await;
    let mut b = start_client(port, "b").await;
    assert_ne!(a.vip, b.vip);
    assert_eq!(a.vip, Ipv4Addr::new(10, 99, 0, 1));
    assert_eq!(b.vip, Ipv4Addr::new(10, 99, 0, 2));

    let payload_ab = b"hello-from-a";
    let pkt_ab = ip_packet(a.vip, b.vip, payload_ab);
    let payload_ba = b"hello-from-b";
    let pkt_ba = ip_packet(b.vip, a.vip, payload_ba);

    // Keep retransmitting until the punched path delivers (handshake may still
    // be completing on the first sends).
    let send_ab = tokio::spawn({
        let tx = a.to_engine.clone();
        let pkt = pkt_ab.clone();
        async move {
            loop {
                if tx.send(pkt.clone()).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    });
    let send_ba = tokio::spawn({
        let tx = b.to_engine.clone();
        let pkt = pkt_ba.clone();
        async move {
            loop {
                if tx.send(pkt.clone()).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    });

    expect_packet(&mut b.from_engine, &pkt_ab).await;
    expect_packet(&mut a.from_engine, &pkt_ba).await;

    send_ab.abort();
    send_ba.abort();
}

/// Force the direct-TCP transport: two engines whose UDP candidates are a
/// black hole (nothing listens) but whose TCP candidates point at each other's
/// real listener. Verifies handshake + bidirectional IP delivery over TCP only.
#[tokio::test]
async fn two_clients_exchange_ip_packets_over_tcp() {
    use ara::engine::{encode_key, Command, Engine};
    use ara::proto::PeerInfo;
    use boringtun::x25519::{PublicKey, StaticSecret};

    let _ = tracing_subscriber::fmt()
        .with_env_filter("ara=debug")
        .with_test_writer()
        .try_init();

    struct Node {
        vip: Ipv4Addr,
        public: PublicKey,
        tcp_addr: std::net::SocketAddr,
        to_engine: mpsc::Sender<Vec<u8>>,
        from_engine: mpsc::Receiver<Vec<u8>>,
        cmd_tx: mpsc::Sender<Command>,
    }

    async fn node(vip: Ipv4Addr) -> Node {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        let socket = Arc::new(bind_udp(0).unwrap());
        let (to_engine, engine_out) = mpsc::channel(256);
        let (engine_in, from_engine) = mpsc::channel(256);
        let (relay_tx, mut relay_rx) = mpsc::channel::<Vec<u8>>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let engine = Engine::new(
            socket, secret, engine_out, engine_in, relay_tx, cmd_rx, cmd_tx.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = engine.run().await {
                tracing::error!("engine stopped: {e}");
            }
        });
        tokio::spawn(async move { while relay_rx.recv().await.is_some() {} });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if tx.send(Command::TcpAccepted(stream)).await.is_err() {
                    break;
                }
            }
        });
        Node { vip, public, tcp_addr, to_engine, from_engine, cmd_tx }
    }

    let mut a = node(Ipv4Addr::new(10, 99, 0, 1)).await;
    let mut b = node(Ipv4Addr::new(10, 99, 0, 2)).await;
    let blackhole: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

    let info_b = PeerInfo {
        public_key: encode_key(&b.public),
        virtual_ip: b.vip,
        candidates: vec![blackhole],
        tcp: vec![b.tcp_addr],
        symmetric: false, nickname: String::new(),
    };
    let info_a = PeerInfo {
        public_key: encode_key(&a.public),
        virtual_ip: a.vip,
        candidates: vec![blackhole],
        // B cannot reach A's TCP (A behind NAT): forces B to use the inbound
        // connection A opens, exercising the inbound writer-matching path.
        tcp: vec![blackhole],
        symmetric: false, nickname: String::new(),
    };
    a.cmd_tx.send(Command::AddPeer(info_b)).await.unwrap();
    b.cmd_tx.send(Command::AddPeer(info_a)).await.unwrap();

    let pkt_ab = ip_packet(a.vip, b.vip, b"tcp-from-a");
    let pkt_ba = ip_packet(b.vip, a.vip, b"tcp-from-b");
    let send_ab = tokio::spawn({
        let tx = a.to_engine.clone();
        let pkt = pkt_ab.clone();
        async move {
            loop {
                if tx.send(pkt.clone()).await.is_err() { break; }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    });
    let send_ba = tokio::spawn({
        let tx = b.to_engine.clone();
        let pkt = pkt_ba.clone();
        async move {
            loop {
                if tx.send(pkt.clone()).await.is_err() { break; }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    });

    expect_packet(&mut b.from_engine, &pkt_ab).await;
    expect_packet(&mut a.from_engine, &pkt_ba).await;
    send_ab.abort();
    send_ba.abort();
}

#[tokio::test]
async fn relay_routes_binary_frames() {
    use base64::Engine as _;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Ws;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(ara_server::serve(listener, None, None));
    let url = format!("ws://127.0.0.1:{port}");

    let key_a = [1u8; 32];
    let key_b = [2u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD;
    let reg = |key: [u8; 32]| {
        ara::proto::encode(&ara::proto::Message::Register {
            network: "relay-net".into(),
            secret: "s".into(),
            public_key: b64.encode(key),
            candidates: vec![],
            tcp: vec![],
            symmetric: false,
            room_code: String::new(),
            room_password: String::new(),
            access_token: String::new(),
            nickname: String::new(),
            create_room: false,
        })
    };

    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    a.send(Ws::Text(reg(key_a))).await.unwrap();
    let _ = a.next().await; // welcome
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    b.send(Ws::Text(reg(key_b))).await.unwrap();
    let _ = b.next().await; // welcome
    let _ = b.next().await; // peers broadcast

    let mut frame = key_b.to_vec();
    frame.extend_from_slice(b"relay-payload");
    a.send(Ws::Binary(frame)).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Ok(Ws::Binary(data))) = b.next().await {
                return data;
            }
        }
    })
    .await
    .expect("relay frame timeout");
    assert_eq!(got, b"relay-payload");
}

/// Diagnostic: raw ws relay throughput (no engine/WG), isolating the
/// ws-frame + server-forwarding segment. Sends N frames one-per-message
/// (as the client session loop does) and times full delivery on loopback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "throughput diagnostic; run on demand (CPU contention with other async tests)"]
async fn relay_throughput_loopback() {
    use base64::Engine as _;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Ws;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(ara_server::serve(listener, None, None));
    let url = format!("ws://127.0.0.1:{port}");
    let b64 = base64::engine::general_purpose::STANDARD;
    let reg = |key: [u8; 32]| {
        ara::proto::encode(&ara::proto::Message::Register {
            network: "tput-net".into(),
            secret: "s".into(),
            public_key: b64.encode(key),
            candidates: vec![],
            tcp: vec![],
            symmetric: false,
            room_code: String::new(),
            room_password: String::new(),
            access_token: String::new(),
            nickname: String::new(),
            create_room: false,
        })
    };
    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    a.send(Ws::Text(reg([1u8; 32]))).await.unwrap();
    let _ = a.next().await;
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    b.send(Ws::Text(reg([2u8; 32]))).await.unwrap();
    let _ = b.next().await;
    let _ = b.next().await;

    const N: u64 = 2000;
    const SZ: usize = 1300;
    let mut frame = [2u8; 32].to_vec();
    frame.extend_from_slice(&vec![0u8; SZ]);

    let start = tokio::time::Instant::now();
    let sender = tokio::spawn(async move {
        for _ in 0..N {
            if a.send(Ws::Binary(frame.clone())).await.is_err() {
                break;
            }
        }
    });
    let mut got = 0u64;
    let recv = async {
        while got < N {
            match tokio::time::timeout(Duration::from_secs(30), b.next()).await {
                Ok(Some(Ok(Ws::Binary(_)))) => got += 1,
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
    };
    recv.await;
    sender.await.unwrap();
    let elapsed = start.elapsed();
    let mbps = (got * SZ as u64) as f64 / elapsed.as_secs_f64() / 1e6;
    eprintln!("relay raw-ws loopback: {got}/{N} frames in {elapsed:?} = {mbps:.2} MB/s");
    assert!(got > 0, "relay must deliver frames");
}

/// Diagnostic: full engine+WG relay throughput on loopback. Two engines wired
/// to a real server via ws, all candidates black-holed so only relay works.
/// Isolates the engine single-loop segment from the network.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow (~13s, 8s relay warmup); on-demand relay throughput diagnostic"]
async fn engine_relay_throughput_loopback() {
    use ara::engine::{encode_key, Command, Engine};
    use ara::proto::PeerInfo;
    use boringtun::x25519::{PublicKey, StaticSecret};
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_tungstenite::tungstenite::Message as Ws;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(ara_server::serve(listener, None, None));
    let url = format!("ws://127.0.0.1:{port}");
    let blackhole: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

    struct Node {
        vip: Ipv4Addr,
        public: PublicKey,
        to_engine: mpsc::Sender<Vec<u8>>,
        from_engine: mpsc::Receiver<Vec<u8>>,
        cmd_tx: mpsc::Sender<Command>,
    }
    async fn node(vip: Ipv4Addr, url: &str, blackhole: std::net::SocketAddr) -> Node {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        let socket = Arc::new(bind_udp(0).unwrap());
        let (to_engine, engine_out) = mpsc::channel(256);
        let (engine_in, from_engine) = mpsc::channel(4096);
        let (relay_tx, mut relay_rx) = mpsc::channel::<Vec<u8>>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let engine = Engine::new(socket, secret, engine_out, engine_in, relay_tx, cmd_rx, cmd_tx.clone());
        tokio::spawn(async move {
            if let Err(e) = engine.run().await {
                tracing::error!("engine stopped: {e}");
            }
        });
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let reg = ara::proto::encode(&ara::proto::Message::Register {
            network: "tput".into(),
            secret: "s".into(),
            public_key: encode_key(&public),
            candidates: vec![blackhole],
            tcp: vec![blackhole],
            symmetric: false,
            room_code: String::new(),
            room_password: String::new(),
            access_token: String::new(),
            nickname: String::new(),
            create_room: false,
        });
        ws.send(Ws::Text(reg)).await.unwrap();
        let _ = ws.next().await; // welcome
        let (mut sink, mut stream) = ws.split();
        tokio::spawn(async move {
            while let Some(b) = relay_rx.recv().await {
                if sink.send(Ws::Binary(b)).await.is_err() {
                    break;
                }
            }
        });
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Ws::Binary(b) = msg {
                    if tx.send(Command::RelayIn(b)).await.is_err() {
                        break;
                    }
                }
            }
        });
        Node { vip, public, to_engine, from_engine, cmd_tx }
    }

    let a = node(Ipv4Addr::new(10, 99, 0, 1), &url, blackhole).await;
    let mut b = node(Ipv4Addr::new(10, 99, 0, 2), &url, blackhole).await;
    a.cmd_tx.send(Command::AddPeer(PeerInfo {
        public_key: encode_key(&b.public), virtual_ip: b.vip,
        candidates: vec![blackhole], tcp: vec![blackhole], symmetric: false, nickname: String::new(),
    })).await.unwrap();
    b.cmd_tx.send(Command::AddPeer(PeerInfo {
        public_key: encode_key(&a.public), virtual_ip: a.vip,
        candidates: vec![blackhole], tcp: vec![blackhole], symmetric: false, nickname: String::new(),
    })).await.unwrap();

    let pkt = ip_packet(a.vip, b.vip, &vec![0u8; 1280]);
    let stop = Arc::new(AtomicBool::new(false));
    let pumper = tokio::spawn({
        let tx = a.to_engine.clone();
        let pkt = pkt.clone();
        let stop = stop.clone();
        async move {
            while !stop.load(Ordering::Relaxed) {
                if tx.send(pkt.clone()).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    });

    // Warmup: relay activates ~8s in, then WG handshake completes over relay.
    let first = tokio::time::timeout(Duration::from_secs(25), b.from_engine.recv())
        .await.expect("relay warmup timeout").expect("channel closed");
    assert_eq!(first, pkt);

    let window = Duration::from_secs(3);
    let start = tokio::time::Instant::now();
    let mut bytes = 0u64;
    let mut count = 0u64;
    while start.elapsed() < window {
        let remaining = window - start.elapsed();
        match tokio::time::timeout(remaining, b.from_engine.recv()).await {
            Ok(Some(p)) => { bytes += p.len() as u64; count += 1; }
            _ => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    pumper.await.unwrap();
    let mbps = bytes as f64 / window.as_secs_f64() / 1e6;
    eprintln!("relay engine+wg loopback: {count} pkts / {bytes} B in {window:?} = {mbps:.2} MB/s");
    assert!(count > 0, "relay must deliver in steady state");
}

/// Regression: a saturated relay egress must not freeze the engine's single
/// loop. Two engines relay to each other; once the tunnel is up we pause
/// draining A's relay_rx (channel kept alive) and keep flooding A->B so the
/// bounded relay_out fills. The engine must still answer a Stun command and
/// resume delivery once drained. Before the try_send fix the loop blocked in
/// relay_out.send().await and never processed another event (the WAN deadlock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_backpressure_keeps_engine_responsive() {
    use ara::engine::{encode_key, Command, Engine};
    use ara::proto::PeerInfo;
    use boringtun::x25519::{PublicKey, StaticSecret};
    use std::sync::atomic::{AtomicBool, Ordering};

    let blackhole: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

    struct Node {
        vip: Ipv4Addr,
        public: PublicKey,
        to_engine: mpsc::Sender<Vec<u8>>,
        from_engine: mpsc::Receiver<Vec<u8>>,
        cmd_tx: mpsc::Sender<Command>,
    }
    async fn node(vip: Ipv4Addr) -> (Node, mpsc::Receiver<Vec<u8>>) {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        let socket = Arc::new(bind_udp(0).unwrap());
        let (to_engine, engine_out) = mpsc::channel(4096);
        let (engine_in, from_engine) = mpsc::channel(4096);
        let (relay_tx, relay_rx) = mpsc::channel::<Vec<u8>>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let engine = Engine::new(socket, secret, engine_out, engine_in, relay_tx, cmd_rx, cmd_tx.clone());
        tokio::spawn(async move { let _ = engine.run().await; });
        (Node { vip, public, to_engine, from_engine, cmd_tx }, relay_rx)
    }

    let (a, mut a_relay_rx) = node(Ipv4Addr::new(10, 99, 0, 1)).await;
    let (mut b, mut b_relay_rx) = node(Ipv4Addr::new(10, 99, 0, 2)).await;

    // B->A relay forward: always drains. Frames are [32-byte dest key][WG];
    // strip the key prefix exactly as the server does before RelayIn.
    let a_cmd = a.cmd_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_relay_rx.recv().await {
            if bytes.len() <= 32 { continue; }
            if a_cmd.send(Command::RelayIn(bytes[32..].to_vec())).await.is_err() { break; }
        }
    });
    // A->B relay forward with a pause gate; keeps relay_rx alive when paused so
    // the channel fills (Full) rather than closes.
    let pause = Arc::new(AtomicBool::new(false));
    let b_cmd = b.cmd_tx.clone();
    let gate = pause.clone();
    tokio::spawn(async move {
        loop {
            if gate.load(Ordering::Relaxed) { tokio::task::yield_now().await; continue; }
            match a_relay_rx.recv().await {
                Some(bytes) => {
                    if bytes.len() <= 32 { continue; }
                    if b_cmd.send(Command::RelayIn(bytes[32..].to_vec())).await.is_err() { break; }
                }
                None => break,
            }
        }
    });

    a.cmd_tx.send(Command::AddPeer(PeerInfo {
        public_key: encode_key(&b.public), virtual_ip: b.vip,
        candidates: vec![blackhole], tcp: vec![], symmetric: false, nickname: String::new(),
    })).await.unwrap();
    b.cmd_tx.send(Command::AddPeer(PeerInfo {
        public_key: encode_key(&a.public), virtual_ip: a.vip,
        candidates: vec![blackhole], tcp: vec![], symmetric: false, nickname: String::new(),
    })).await.unwrap();

    let pkt = ip_packet(a.vip, b.vip, &vec![0u8; 1280]);
    let stop = Arc::new(AtomicBool::new(false));
    let pumper = tokio::spawn({
        let tx = a.to_engine.clone();
        let pkt = pkt.clone();
        let stop = stop.clone();
        async move {
            while !stop.load(Ordering::Relaxed) {
                if tx.send(pkt.clone()).await.is_err() { break; }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });

    // Warmup: relay activates ~8s in, then the WG handshake completes over it.
    let first = tokio::time::timeout(Duration::from_secs(25), b.from_engine.recv())
        .await.expect("relay warmup timeout").expect("channel closed");
    assert_eq!(first, pkt);

    // Saturate A's relay egress, then probe engine responsiveness.
    pause.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (reply_tx, reply_rx) = oneshot::channel();
    a.cmd_tx.send(Command::Stun { server: blackhole, reply: reply_tx }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(6), reply_rx)
        .await
        .expect("engine frozen: Stun not processed under relay backpressure")
        .expect("stun reply channel");

    // Draining again must resume delivery (loop survived, tunnel intact).
    pause.store(false, Ordering::Relaxed);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "delivery did not resume after backpressure");
        match tokio::time::timeout(remaining.min(Duration::from_millis(500)), b.from_engine.recv()).await {
            Ok(Some(got)) if got == pkt => break,
            Ok(Some(_)) => {}
            Ok(None) => panic!("channel closed"),
            Err(_) => {}
        }
    }

    stop.store(true, Ordering::Relaxed);
    pumper.await.unwrap();
}

/// Regression: both peers only know each other's stale symmetric-NAT port
/// (real port = stale + delta). Handshake initiations must be sprayed over
/// predicted ports; without the spray neither side can ever punch through.
/// Relay is drained (not forwarded), so UDP prediction is the only path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symmetric_nat_port_prediction_punches_through() {
    use ara::engine::{encode_key, Command, Engine};
    use ara::proto::PeerInfo;
    use boringtun::x25519::{PublicKey, StaticSecret};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    struct Node {
        vip: Ipv4Addr,
        public: PublicKey,
        addr: SocketAddr,
        to_engine: mpsc::Sender<Vec<u8>>,
        from_engine: mpsc::Receiver<Vec<u8>>,
        cmd_tx: mpsc::Sender<Command>,
    }
    async fn node(vip: Ipv4Addr) -> Node {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        let socket = Arc::new(bind_udp(0).unwrap());
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), socket.local_addr().unwrap().port());
        let (to_engine, engine_out) = mpsc::channel(256);
        let (engine_in, from_engine) = mpsc::channel(256);
        let (relay_tx, mut relay_rx) = mpsc::channel::<Vec<u8>>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let engine = Engine::new(socket, secret, engine_out, engine_in, relay_tx, cmd_rx, cmd_tx.clone());
        tokio::spawn(async move { let _ = engine.run().await; });
        tokio::spawn(async move { while relay_rx.recv().await.is_some() {} });
        Node { vip, public, addr, to_engine, from_engine, cmd_tx }
    }

    let a = node(Ipv4Addr::new(10, 99, 0, 1)).await;
    let mut b = node(Ipv4Addr::new(10, 99, 0, 2)).await;
    assert!(a.addr.port() > 20 && b.addr.port() > 20);
    let stale_a = SocketAddr::new(a.addr.ip(), a.addr.port() - 3);
    let stale_b = SocketAddr::new(b.addr.ip(), b.addr.port() - 5);

    a.cmd_tx.send(Command::AddPeer(PeerInfo {
        public_key: encode_key(&b.public), virtual_ip: b.vip,
        candidates: vec![stale_b], tcp: vec![], symmetric: true, nickname: String::new(),
    })).await.unwrap();
    b.cmd_tx.send(Command::AddPeer(PeerInfo {
        public_key: encode_key(&a.public), virtual_ip: a.vip,
        candidates: vec![stale_a], tcp: vec![], symmetric: true, nickname: String::new(),
    })).await.unwrap();

    let pkt = ip_packet(a.vip, b.vip, b"sym-nat-prediction");
    let pumper = tokio::spawn({
        let tx = a.to_engine.clone();
        let pkt = pkt.clone();
        async move {
            loop {
                if tx.send(pkt.clone()).await.is_err() { break; }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    });
    expect_packet(&mut b.from_engine, &pkt).await;
    pumper.abort();
}

/// Regression: A and C have no direct path and the server relay is drained.
/// All WG packets must travel one hop through the directly-connected peer B,
/// in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_relay_delivers_without_server() {
    use ara::engine::{encode_key, Command, Engine};
    use ara::proto::PeerInfo;
    use boringtun::x25519::{PublicKey, StaticSecret};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    struct Node {
        vip: Ipv4Addr,
        public: PublicKey,
        addr: SocketAddr,
        to_engine: mpsc::Sender<Vec<u8>>,
        from_engine: mpsc::Receiver<Vec<u8>>,
        cmd_tx: mpsc::Sender<Command>,
    }
    async fn node(vip: Ipv4Addr) -> Node {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        let socket = Arc::new(bind_udp(0).unwrap());
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), socket.local_addr().unwrap().port());
        let (to_engine, engine_out) = mpsc::channel(256);
        let (engine_in, from_engine) = mpsc::channel(256);
        let (relay_tx, mut relay_rx) = mpsc::channel::<Vec<u8>>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let engine = Engine::new(socket, secret, engine_out, engine_in, relay_tx, cmd_rx, cmd_tx.clone());
        tokio::spawn(async move { let _ = engine.run().await; });
        tokio::spawn(async move { while relay_rx.recv().await.is_some() {} });
        Node { vip, public, addr, to_engine, from_engine, cmd_tx }
    }

    let mut a = node(Ipv4Addr::new(10, 99, 0, 1)).await;
    let b = node(Ipv4Addr::new(10, 99, 0, 2)).await;
    let mut c = node(Ipv4Addr::new(10, 99, 0, 3)).await;
    let blackhole: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let info = |n: &Node, candidates: Vec<SocketAddr>| PeerInfo {
        public_key: encode_key(&n.public),
        virtual_ip: n.vip,
        candidates,
        tcp: vec![],
        symmetric: false, nickname: String::new(),
    };
    a.cmd_tx.send(Command::AddPeer(info(&b, vec![b.addr]))).await.unwrap();
    a.cmd_tx.send(Command::AddPeer(info(&c, vec![blackhole]))).await.unwrap();
    b.cmd_tx.send(Command::AddPeer(info(&a, vec![a.addr]))).await.unwrap();
    b.cmd_tx.send(Command::AddPeer(info(&c, vec![c.addr]))).await.unwrap();
    c.cmd_tx.send(Command::AddPeer(info(&b, vec![b.addr]))).await.unwrap();
    c.cmd_tx.send(Command::AddPeer(info(&a, vec![blackhole]))).await.unwrap();

    let pkt_ac = ip_packet(a.vip, c.vip, b"peer-relay-a-c");
    let pkt_ca = ip_packet(c.vip, a.vip, b"peer-relay-c-a");
    let pump = |tx: mpsc::Sender<Vec<u8>>, pkt: Vec<u8>| tokio::spawn(async move {
        loop {
            if tx.send(pkt.clone()).await.is_err() { break; }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
    let pa = pump(a.to_engine.clone(), pkt_ac.clone());
    let pc = pump(c.to_engine.clone(), pkt_ca.clone());
    expect_packet(&mut c.from_engine, &pkt_ac).await;
    expect_packet(&mut a.from_engine, &pkt_ca).await;
    pa.abort();
    pc.abort();
}