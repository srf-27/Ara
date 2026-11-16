use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::DEFAULT_PORT;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub network: Option<String>,
    pub secret: Option<String>,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<std::path::PathBuf>,
}

impl ClientConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        toml::from_str(&text).context("parse config TOML")
    }

    /// CLI values override file values.
    pub fn merge_cli(
        mut self,
        network: Option<String>,
        secret: Option<String>,
        server: Option<String>,
        port: Option<u16>,
        identity: Option<std::path::PathBuf>,
    ) -> Self {
        self.network = network.or(self.network);
        self.secret = secret.or(self.secret);
        self.server = server.or(self.server);
        self.port = port.or(self.port);
        self.identity = identity.or(self.identity);
        self
    }

    pub fn validate(self) -> Result<ResolvedConfig> {
        let network = self.network.context("missing 'network'")?;
        let secret = self.secret.context("missing 'secret'")?;
        let server = self.server.context("missing 'server'")?;
        if network.is_empty() || secret.is_empty() {
            bail!("network and secret must not be empty");
        }
        let (server_host, server_port) = match server.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().context("server port")?),
            None => (server.clone(), DEFAULT_PORT),
        };
        Ok(ResolvedConfig {
            network,
            secret,
            server_host,
            server_port,
            local_port: self.port.unwrap_or(DEFAULT_PORT),
            identity_path: self.identity,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub network: String,
    pub secret: String,
    pub server_host: String,
    pub server_port: u16,
    /// Local UDP port shared by STUN and WireGuard; 0 = ephemeral.
    pub local_port: u16,
    /// Override identity key file (default: per-user platform path).
    pub identity_path: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_file() {
        let file = ClientConfig {
            network: Some("file-net".into()),
            secret: Some("file-secret".into()),
            server: Some("1.1.1.1".into()),
            port: Some(1),
            identity: None,
        };
        let merged = file.merge_cli(Some("cli-net".into()), None, None, Some(2), None);
        let r = merged.validate().unwrap();
        assert_eq!(r.network, "cli-net");
        assert_eq!(r.secret, "file-secret");
        assert_eq!(r.local_port, 2);
        assert_eq!(r.server_host, "1.1.1.1");
        assert_eq!(r.server_port, DEFAULT_PORT);
    }

    #[test]
    fn server_host_port_split() {
        let c = ClientConfig {
            network: Some("n".into()),
            secret: Some("s".into()),
            server: Some("example.com:9999".into()),
            port: None,
            identity: None,
        };
        let r = c.validate().unwrap();
        assert_eq!(r.server_host, "example.com");
        assert_eq!(r.server_port, 9999);
    }

    #[test]
    fn missing_fields_fail() {
        assert!(ClientConfig::default().validate().is_err());
    }

    #[test]
    fn default_port() {
        let c = ClientConfig {
            network: Some("n".into()),
            secret: Some("s".into()),
            server: Some("h".into()),
            port: None,
            identity: None,
        };
        assert_eq!(c.validate().unwrap().local_port, DEFAULT_PORT);
    }
}