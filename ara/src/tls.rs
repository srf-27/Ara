//! TLS helpers: the client trusts only the certificate embedded in the access
//! file (pin-by-root); the server generates a self-signed cert on demand.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::CertificateDer;
use rustls::RootCertStore;

pub fn client_config(cert_pem: &str) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = RootCertStore::empty();
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    for cert in rustls_pemfile::certs(&mut reader) {
        roots
            .add(cert.context("parse certificate")?)
            .context("add trusted certificate")?;
    }
    if roots.is_empty() {
        anyhow::bail!("access file has no certificate");
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

pub fn cert_der_sha256(cert_pem: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    let cert: CertificateDer = rustls_pemfile::certs(&mut reader)
        .next()
        .context("no certificate")?
        .context("parse certificate")?;
    let mut h = Sha256::new();
    h.update(cert.as_ref());
    Ok(format!("{:x}", h.finalize()))
}
