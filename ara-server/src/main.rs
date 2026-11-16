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
    let udp = Arc::new(bind_udp(args.port).context("bind STUN UDP")?);
    let listener = TcpListener::bind(("0.0.0.0", args.port))
        .await
        .context("bind signaling TCP")?;
    tracing::info!("ara-server listening on port {}", args.port);
    ara_server::serve(listener, udp).await
}