use std::sync::Arc;

use anyhow::{Context, Result};
use ara::engine::bind_udp;
use ara::DEFAULT_PORT;
use clap::Parser;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(name = "ara-server", about = "Ara control-plane server (signaling + STUN)")]
struct Args {
    /// TCP (WebSocket signaling) and UDP (STUN) port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Skip the UDP STUN responder (signaling only); clients fall back to
    /// public STUN. Use when another process needs the UDP port.
    #[arg(long)]
    no_stun: bool,
    /// File persisting virtual-IP allocations across restarts.
    #[arg(long, default_value = "ara-server-state.json")]
    state: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let udp = if args.no_stun {
        None
    } else {
        Some(Arc::new(bind_udp(args.port).context("bind STUN UDP")?))
    };
    let listener = TcpListener::bind(("0.0.0.0", args.port))
        .await
        .context("bind signaling TCP")?;
    tracing::info!("ara-server listening on port {} (stun: {})", args.port, !args.no_stun);
    ara_server::serve(listener, udp, Some(args.state)).await
}