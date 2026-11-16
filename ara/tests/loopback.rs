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

async fn start_client(server_port: u16) -> Client {
    let cfg = ResolvedConfig {
        network: "test-net".into(),
        secret: "test-secret".into(),
        server_host: "127.0.0.1".into(),
        server_port,
        local_port: 0,
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
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
    tokio::spawn(ara_server::serve(listener, udp));

    let mut a = start_client(port).await;
    let mut b = start_client(port).await;
    assert_ne!(a.vip, b.vip);
    assert_eq!(a.vip, Ipv4Addr::new(10, 99, 0, 2));
    assert_eq!(b.vip, Ipv4Addr::new(10, 99, 0, 3));

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