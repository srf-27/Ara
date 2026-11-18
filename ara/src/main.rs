use std::path::PathBuf;

use anyhow::{Context, Result};
use ara::config::ClientConfig;
use ara::{TUN_MTU, VIRTUAL_PREFIX};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tun::{create_as_async, TunPacket};

#[derive(Parser)]
#[command(name = "ara", about = "Ara P2P virtual LAN")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join a virtual network.
    Up {
        /// TOML config file (default: ./ara.toml if present).
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        network: Option<String>,
        #[arg(long)]
        secret: Option<String>,
        /// Server host, optionally host:port.
        #[arg(long)]
        server: Option<String>,
        /// Local UDP port shared by STUN and WireGuard (0 = ephemeral).
        #[arg(long)]
        port: Option<u16>,
        /// Local TCP port for direct-TCP fallback (default: same as --port).
        #[arg(long)]
        tcp_port: Option<u16>,
        /// Identity key file override (default: per-user platform path).
        #[arg(long)]
        identity: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wintun::log=off".into()),
        )
        .init();
    let cli = Cli::parse();
    let Command::Up { config, network, secret, server, port, tcp_port, identity } = cli.command;

    let file_cfg = match config {
        Some(path) => ClientConfig::from_file(&path)?,
        None => {
            let default = PathBuf::from("ara.toml");
            if default.exists() {
                ClientConfig::from_file(&default)?
            } else {
                ClientConfig::default()
            }
        }
    };
    let cfg = file_cfg.merge_cli(network, secret, server, port, tcp_port, identity).validate()?;

    let (from_tun_tx, from_tun_rx) = mpsc::channel::<Vec<u8>>(256);
    let (to_tun_tx, mut to_tun_rx) = mpsc::channel::<Vec<u8>>(256);
    let (assigned_tx, assigned_rx) = oneshot::channel();

    let client = tokio::spawn(ara::client::run(cfg, from_tun_rx, to_tun_tx, assigned_tx));

    let vip = assigned_rx
        .await
        .context("failed to join network (wrong secret, or client exited; see logs)")?;

    #[cfg(target_os = "windows")]
    ensure_wintun().context("prepare wintun.dll")?;

    let mut config = tun::configure();
    config
        .name("ara")
        .address(vip)
        .netmask(prefix_mask(VIRTUAL_PREFIX))
        .mtu(TUN_MTU as i32)
        .layer(tun::Layer::L3)
        .up();
    let device = create_as_async(&config).context(
        "create TUN device (Windows: run as administrator; Linux: run as root)",
    )?;
    tracing::info!("TUN device up with {vip}/{VIRTUAL_PREFIX}, MTU {TUN_MTU}");
    let (mut tun_sink, mut tun_stream) = device.into_framed().split();

    // TUN -> engine
    tokio::spawn(async move {
        while let Some(pkt) = tun_stream.next().await {
            match pkt {
                Ok(p) => {
                    if from_tun_tx.send(p.get_bytes().to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("TUN read: {e}");
                    break;
                }
            }
        }
    });
    // engine -> TUN
    tokio::spawn(async move {
        while let Some(bytes) = to_tun_rx.recv().await {
            if tun_sink.send(TunPacket::new(bytes)).await.is_err() {
                break;
            }
        }
    });

    tracing::info!("ara is running; press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    client.abort();
    tracing::info!("shutting down");
    Ok(())
}

fn prefix_mask(prefix: u8) -> std::net::Ipv4Addr {
    let mask: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    std::net::Ipv4Addr::from(mask)
}

/// The wintun crate loads wintun.dll from the exe directory; embed the official
/// signed DLL and extract it on first run so the binary is self-contained.
#[cfg(target_os = "windows")]
fn ensure_wintun() -> Result<()> {
    const DLL: &[u8] = include_bytes!("../vendor/wintun.dll");
    let exe = std::env::current_exe().context("locate exe")?;
    let dll = exe.parent().context("exe dir")?.join("wintun.dll");
    if !dll.exists() {
        std::fs::write(&dll, DLL).context("extract wintun.dll next to ara.exe")?;
    }
    Ok(())
}