//! On-demand throughput probes for the four data-plane paths. Run alone:
//! cargo test --test throughput -- --ignored --nocapture

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ara::engine::{bind_udp, encode_key, Command, Engine};
use ara::proto::PeerInfo;
use boringtun::x25519::{PublicKey, StaticSecret};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::Instant;

const PKT_LEN: usize = 1280;
const WINDOW: Duration = Duration::from_secs(3);
const WARMUP: Duration = Duration::from_secs(30);

fn ip_packet(src: Ipv4Addr, dst: Ipv4Addr) -> Vec<u8> {
    let mut p = vec![0u8; 20 + PKT_LEN];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&((20 + PKT_LEN) as u16).to_be_bytes());
    p[8] = 64;
    p[9] = 17;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    p
}

struct Node {
    vip: Ipv4Addr,
    public: PublicKey,
    addr: SocketAddr,
    to_engine: mpsc::Sender<Vec<u8>>,
    from_engine: mpsc::Receiver<Vec<u8>>,
    cmd: mpsc::Sender<Command>,
}

async fn node(vip: Ipv4Addr, tcp: bool) -> (Node, mpsc::Receiver<Vec<u8>>, SocketAddr) {
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public = PublicKey::from(&secret);
    let socket = Arc::new(bind_udp(0).unwrap());
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), socket.local_addr().unwrap().port());
    let (to_engine, engine_out) = mpsc::channel(4096);
    let (engine_in, from_engine) = mpsc::channel(4096);
    let (relay_tx, relay_rx) = mpsc::channel::<Vec<u8>>(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(1024);
    let engine = Engine::new(socket, secret, engine_out, engine_in, relay_tx, cmd_rx, cmd_tx.clone());
    tokio::spawn(async move { let _ = engine.run().await; });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = listener.local_addr().unwrap();
    if tcp {
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if tx.send(Command::TcpAccepted(stream)).await.is_err() { break; }
            }
        });
    }
    (Node { vip, public, addr, to_engine, from_engine, cmd: cmd_tx }, relay_rx, tcp_addr)
}

fn info(n: &Node, candidates: Vec<SocketAddr>, tcp: Vec<SocketAddr>) -> PeerInfo {
    PeerInfo {
        public_key: encode_key(&n.public),
        virtual_ip: n.vip,
        candidates,
        tcp,
        symmetric: false,
    }
}

fn forward_relay(mut rx: mpsc::Receiver<Vec<u8>>, cmd: mpsc::Sender<Command>) {
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if bytes.len() <= 32 { continue; }
            if cmd.send(Command::RelayIn(bytes[32..].to_vec())).await.is_err() { break; }
        }
    });
}

async fn measure(label: &str, tx: mpsc::Sender<Vec<u8>>, rx: &mut mpsc::Receiver<Vec<u8>>, pkt: Vec<u8>) {
    let stop = Arc::new(AtomicBool::new(false));
    let pumper = tokio::spawn({
        let tx = tx.clone();
        let pkt = pkt.clone();
        let stop = stop.clone();
        async move {
            while !stop.load(Ordering::Relaxed) {
                if tx.send(pkt.clone()).await.is_err() { break; }
                tokio::task::yield_now().await;
            }
        }
    });
    let first = tokio::time::timeout(WARMUP, rx.recv())
        .await
        .expect("path warmup timeout")
        .expect("channel closed");
    assert_eq!(first, pkt);
    let start = Instant::now();
    let mut bytes = 0u64;
    let mut count = 0u64;
    while start.elapsed() < WINDOW {
        let remaining = WINDOW - start.elapsed();
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(p)) => { bytes += p.len() as u64; count += 1; }
            _ => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    pumper.abort();
    let mbps = bytes as f64 / WINDOW.as_secs_f64() / 1e6;
    eprintln!("[{label}] {count} pkts / {bytes} B in {WINDOW:?} = {mbps:.2} MB/s");
    assert!(count > 0, "{label} delivered nothing");
}

fn blackhole() -> SocketAddr {
    "127.0.0.1:1".parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "on-demand throughput probe; run alone"]
async fn udp_direct_throughput() {
    let (a, _, _) = node(Ipv4Addr::new(10, 99, 0, 1), false).await;
    let (mut b, _, _) = node(Ipv4Addr::new(10, 99, 0, 2), false).await;
    a.cmd.send(Command::AddPeer(info(&b, vec![b.addr], vec![]))).await.unwrap();
    b.cmd.send(Command::AddPeer(info(&a, vec![a.addr], vec![]))).await.unwrap();
    measure("udp-direct", a.to_engine.clone(), &mut b.from_engine, ip_packet(a.vip, b.vip)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "on-demand throughput probe; run alone"]
async fn tcp_direct_throughput() {
    let (a, _, _) = node(Ipv4Addr::new(10, 99, 0, 1), true).await;
    let (mut b, _, b_tcp) = node(Ipv4Addr::new(10, 99, 0, 2), true).await;
    a.cmd.send(Command::AddPeer(info(&b, vec![blackhole()], vec![b_tcp]))).await.unwrap();
    b.cmd.send(Command::AddPeer(info(&a, vec![blackhole()], vec![blackhole()]))).await.unwrap();
    measure("tcp-direct", a.to_engine.clone(), &mut b.from_engine, ip_packet(a.vip, b.vip)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "on-demand throughput probe; run alone"]
async fn server_relay_throughput() {
    let (a, a_relay, _) = node(Ipv4Addr::new(10, 99, 0, 1), false).await;
    let (mut b, b_relay, _) = node(Ipv4Addr::new(10, 99, 0, 2), false).await;
    a.cmd.send(Command::AddPeer(info(&b, vec![blackhole()], vec![]))).await.unwrap();
    b.cmd.send(Command::AddPeer(info(&a, vec![blackhole()], vec![]))).await.unwrap();
    forward_relay(a_relay, b.cmd.clone());
    forward_relay(b_relay, a.cmd.clone());
    measure("server-relay", a.to_engine.clone(), &mut b.from_engine, ip_packet(a.vip, b.vip)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "on-demand throughput probe; run alone"]
async fn peer_relay_throughput() {
    let (a, _, _) = node(Ipv4Addr::new(10, 99, 0, 1), false).await;
    let (b, _, _) = node(Ipv4Addr::new(10, 99, 0, 2), false).await;
    let (mut c, _, _) = node(Ipv4Addr::new(10, 99, 0, 3), false).await;
    a.cmd.send(Command::AddPeer(info(&b, vec![b.addr], vec![]))).await.unwrap();
    a.cmd.send(Command::AddPeer(info(&c, vec![blackhole()], vec![]))).await.unwrap();
    b.cmd.send(Command::AddPeer(info(&a, vec![a.addr], vec![]))).await.unwrap();
    b.cmd.send(Command::AddPeer(info(&c, vec![c.addr], vec![]))).await.unwrap();
    c.cmd.send(Command::AddPeer(info(&b, vec![b.addr], vec![]))).await.unwrap();
    c.cmd.send(Command::AddPeer(info(&a, vec![blackhole()], vec![]))).await.unwrap();
    measure("peer-relay", a.to_engine.clone(), &mut c.from_engine, ip_packet(a.vip, c.vip)).await;
}
