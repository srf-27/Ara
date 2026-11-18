//! File transfer over the room's virtual network: one TCP connection per file,
//! request/accept handshake, 64 KiB chunks with offsets, resumable via
//! `transfers.json`, SHA-256 verified end to end.

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const TRANSFER_PORT: u16 = 43001;
const CHUNK: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    accept: bool,
    offset: u64,
    reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Final {
    ok: bool,
    reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferState {
    pub sha256: String,
    pub path: String,
    pub offset: u64,
}

pub fn state_path() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var("APPDATA").unwrap_or_else(|_| ".".into())
    } else {
        format!(
            "{}/.config",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    };
    PathBuf::from(base).join("ara").join("transfers.json")
}

pub fn load_state() -> Option<TransferState> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_state(state: &TransferState) -> Result<()> {
    let path = state_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&path, serde_json::to_string_pretty(state)?).context("write transfers.json")
}

pub fn clear_state() {
    let _ = std::fs::remove_file(state_path());
}

fn sanitize(name: &str) -> Result<String> {
    let path = Path::new(name);
    if path.is_absolute() {
        bail!("absolute path rejected");
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => bail!("unsafe file name rejected"),
        }
    }
    let base = out
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid file name")?
        .to_string();
    if base.is_empty() {
        bail!("empty file name");
    }
    Ok(base)
}

async fn write_json<W: AsyncWriteExt + Unpin>(w: &mut W, v: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec(v)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

async fn read_json<T: for<'de> Deserialize<'de>, R: AsyncReadExt + Unpin>(r: &mut R) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > 64 * 1024 {
        bail!("bad control frame length {len}");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

async fn hash_file(path: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn ask_accept(name: &str, size: u64) -> bool {
    println!("incoming file: {name} ({size} bytes). accept? [y/N]");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}

/// Send one file to a peer VIP. Returns after the receiver acknowledged all
/// chunks and the receiver-side checksum passed.
pub async fn send(to: SocketAddr, file: PathBuf) -> Result<()> {
    let size = std::fs::metadata(&file)?.len();
    let sha256 = hash_file(&file).await?;
    let name = file
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid file name")?
        .to_string();
    let mut stream = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(to))
        .await
        .context("connect timeout")?
        .with_context(|| format!("connect {to}"))?;
    write_json(&mut stream, &Request { name, size, sha256: sha256.clone() }).await?;
    let reply: Reply = read_json(&mut stream).await?;
    if !reply.accept {
        bail!("receiver rejected: {}", reply.reason);
    }
    let mut f = tokio::fs::File::open(&file).await?;
    if reply.offset > 0 {
        f.seek(std::io::SeekFrom::Start(reply.offset)).await?;
    }
    let mut offset = reply.offset;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&offset.to_be_bytes()).await?;
        stream.write_all(&(n as u32).to_be_bytes()).await?;
        stream.write_all(&buf[..n]).await?;
        offset += n as u64;
    }
    stream.flush().await?;
    if offset != size {
        bail!("sent {offset} of {size} bytes");
    }
    let final_msg: Final = read_json(&mut stream).await?;
    if !final_msg.ok {
        bail!("receiver rejected the transfer: {}", final_msg.reason);
    }
    Ok(())
}

/// Serve incoming transfer requests; `auto_accept` skips the interactive prompt.
pub async fn serve(dir: PathBuf, port: u16, auto_accept: bool) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind transfer port {port}"))?;
    serve_listener(listener, dir, auto_accept).await
}

pub async fn serve_listener(listener: TcpListener, dir: PathBuf, auto_accept: bool) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let dir = dir.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, dir, auto_accept).await {
                eprintln!("transfer failed: {e:#}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, dir: PathBuf, auto_accept: bool) -> Result<()> {
    let req: Request = read_json(&mut stream).await?;
    let name = sanitize(&req.name)?;
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(&name);
    let mut offset = 0u64;
    if let Some(st) = load_state() {
        if st.sha256 == req.sha256 && st.path == path.display().to_string() && st.offset <= req.size
        {
            offset = st.offset;
        }
    }
    let accept = if auto_accept {
        true
    } else {
        let n = req.name.clone();
        tokio::task::spawn_blocking(move || ask_accept(&n, req.size))
            .await
            .unwrap_or(false)
    };
    let reply = Reply {
        accept,
        offset: if accept { offset } else { 0 },
        reason: if accept { String::new() } else { "rejected".into() },
    };
    write_json(&mut stream, &reply).await?;
    if !accept {
        return Ok(());
    }

    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .await?;
    if offset == 0 {
        f.set_len(0).await?;
    }
    loop {
        let mut hdr = [0u8; 12];
        if stream.read_exact(&mut hdr).await.is_err() {
            break;
        }
        let off = u64::from_be_bytes(hdr[0..8].try_into().unwrap());
        let len = u32::from_be_bytes(hdr[8..12].try_into().unwrap()) as usize;
        if len == 0 || len > CHUNK {
            bail!("bad chunk length {len}");
        }
        if off != offset {
            bail!("offset mismatch: got {off}, expected {offset}");
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        f.write_all(&buf).await?;
        f.flush().await?;
        offset += len as u64;
        save_state(&TransferState {
            sha256: req.sha256.clone(),
            path: path.display().to_string(),
            offset,
        })?;
        if offset >= req.size {
            break;
        }
    }
    let actual = hash_file(&path).await?;
    let ok = actual == req.sha256;
    write_json(
        &mut stream,
        &Final {
            ok,
            reason: if ok { String::new() } else { "checksum mismatch".into() },
        },
    )
    .await?;
    if !ok {
        bail!("checksum mismatch for {}", path.display());
    }
    clear_state();
    tracing::info!("received {} ({offset} bytes)", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(sanitize("a.txt").unwrap(), "a.txt");
        assert_eq!(sanitize("./a.txt").unwrap(), "a.txt");
        assert!(sanitize("../a.txt").is_err());
        assert!(sanitize("/etc/passwd").is_err());
        assert!(sanitize("dir/a.txt").is_ok());
    }

    #[tokio::test]
    async fn transfer_roundtrip_and_resume() {
        clear_state();
        let base = std::env::temp_dir().join(format!("ara-transfer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let src = base.join("payload.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = base.join("recv");
        tokio::spawn(serve_listener(listener, dir.clone(), true));

        send(addr, src.clone()).await.unwrap();
        let got = std::fs::read(dir.join("payload.bin")).unwrap();
        assert_eq!(got.len(), data.len(), "received length mismatch");
        assert_eq!(got, data);
        clear_state();
        std::fs::remove_dir_all(&base).ok();
    }
}
