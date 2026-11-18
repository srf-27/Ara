use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ara::engine::bind_udp;
use ara::DEFAULT_PORT;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(name = "ara-server", about = "Ara control-plane server (signaling + STUN + rooms)")]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// TCP (WebSocket signaling) and UDP (STUN) port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Skip the UDP STUN responder (signaling only); clients fall back to
    /// public STUN.
    #[arg(long)]
    no_stun: bool,
    /// File persisting virtual-IP allocations across restarts.
    #[arg(long, default_value = "ara-server-state.json")]
    state: PathBuf,
    /// Directory for rotating server logs (default: alongside the state file).
    #[arg(long)]
    log_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate an access token; writes access.json and prints the validation file.
    GenAccess {
        /// Address clients should dial, e.g. vpn.example.com:23947.
        #[arg(long, default_value = "127.0.0.1:23947")]
        server: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Kick a room or a peer id at runtime (server applies it within 5s).
    Kick {
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        id: Option<String>,
    },
    /// Print rooms and allocations from the state file.
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_dir = args.log_dir.clone().unwrap_or_else(|| {
        args.state
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    });
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "ara-server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match args.cmd {
        Some(Cmd::GenAccess { server, name }) => {
            let mut state = ara_server::State::new();
            let access = args.state.with_file_name("access.json");
            state.load_tokens(&access);
            let token = state.gen_token();
            state.save_tokens(&access)?;
            let file = serde_json::json!({
                "server": server,
                "token": token,
                "name": name.unwrap_or_default(),
            });
            let path = args.state.with_file_name("access-file.json");
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            println!("{}", serde_json::to_string_pretty(&file)?);
            eprintln!("wrote {}", path.display());
        }
        Some(Cmd::Kick { room, id }) => {
            if room.is_none() && id.is_none() {
                anyhow::bail!("give --room and/or --id");
            }
            let req = ara_server::KickRequest { room, id };
            let path = args.state.with_file_name("kick.json");
            std::fs::write(&path, serde_json::to_string(&req)?)?;
            eprintln!("queued kick in {} (server applies within 5s)", path.display());
        }
        Some(Cmd::List) => {
            let text = std::fs::read_to_string(&args.state).unwrap_or_else(|_| "[]".into());
            let allocs: Vec<ara_server::Alloc> = serde_json::from_str(&text)?;
            let mut rooms: std::collections::BTreeMap<&str, Vec<&ara_server::Alloc>> =
                std::collections::BTreeMap::new();
            for a in &allocs {
                rooms.entry(a.network.as_str()).or_default().push(a);
            }
            for (code, members) in rooms {
                println!("room {code}: {} member(s)", members.len());
                for m in members {
                    println!("  {} -> {}", m.public_key, m.virtual_ip);
                }
            }
        }
        None => {
            let udp = if args.no_stun {
                None
            } else {
                Some(Arc::new(bind_udp(args.port).context("bind STUN UDP")?))
            };
            let listener = TcpListener::bind(("0.0.0.0", args.port))
                .await
                .context("bind signaling TCP")?;
            tracing::info!(
                "ara-server listening on port {} (stun: {}, log: {})",
                args.port,
                !args.no_stun,
                log_dir.display()
            );
            eprintln!(
                "ara-server listening on {} (stun: {}); log: {}",
                args.port,
                !args.no_stun,
                log_dir.display()
            );
            return ara_server::serve(listener, udp, Some(args.state)).await;
        }
    };
    Ok(())
}
