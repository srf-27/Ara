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
    };
    let (to_engine, engine_out) = mpsc::channel(256);
    let (engine_in, from_engine) = mpsc::channel(256);
    let (tx, rx) = oneshot::channel();
    tokio::spawn(ara::client::run(cfg, engine_out, engine_in, tx));
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
    };
    let info_a = PeerInfo {
        public_key: encode_key(&a.public),
        virtual_ip: a.vip,
        candidates: vec![blackhole],
        // B cannot reach A's TCP (A behind NAT): forces B to use the inbound
        // connection A opens, exercising the inbound writer-matching path.
        tcp: vec![blackhole],
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