//! Self-signed TLS for the control plane: `gen-access` creates the cert, the
//! server serves it, and the client trusts only the copy in its access file.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_rustls::TlsAcceptor;

fn san(host: &str) -> Result<rcgen::SanType> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        Ok(rcgen::SanType::IpAddress(ip))
    } else {
        Ok(rcgen::SanType::DnsName(host.to_string().try_into()?))
    }
}

pub fn ensure_cert(cert_path: &Path, key_path: &Path, host: &str) -> Result<(String, String)> {
    if cert_path.exists() && key_path.exists() {
        return Ok((
            std::fs::read_to_string(cert_path)?,
            std::fs::read_to_string(key_path)?,
        ));
    }
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::default();
    params.subject_alt_names = vec![san(host)?];
    let cert = params.self_signed(&key)?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();
    if let Some(dir) = cert_path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(cert_path, &cert_pem)?;
    std::fs::write(key_path, &key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((cert_pem, key_pem))
}

pub fn acceptor(cert_pem: &str, key_pem: &str) -> Result<TlsAcceptor> {
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .context("parse certificate")?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_pem.as_bytes()))
        .context("parse private key")?
        .context("no private key in PEM")?;
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build TLS server config")?;
    Ok(TlsAcceptor::from(Arc::new(cfg)))
}
