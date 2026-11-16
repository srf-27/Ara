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
    ) -> Self {
        self.network = network.or(self.network);
        self.secret = secret.or(self.secret);
        self.server = server.or(self.server);
        self.port = port.or(self.port);
        self
    }

    pub fn validate(self) -> Result<ResolvedConfig> {
        let network = self.network.context("missing 'network'")?;
        let secret = self.secret.context("missing 'secret'")?;
        let server = self.server.context("missing 'server'")?;
        if network.is_empty() || secret.is_empty() {
            bail!("network and secret must not be empty");
        }
        Ok(ResolvedConfig {
            network,
            secret,
            server,
            port: self.port.unwrap_or(DEFAULT_PORT),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub network: String,
    pub secret: String,
    /// Server host or IP, without scheme/port.
    pub server: String,
    pub port: u16,
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
        };
        let merged = file.merge_cli(Some("cli-net".into()), None, None, Some(2));
        let r = merged.validate().unwrap();
        assert_eq!(r.network, "cli-net");
        assert_eq!(r.secret, "file-secret");
        assert_eq!(r.port, 2);
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
        };
        assert_eq!(c.validate().unwrap().port, DEFAULT_PORT);
    }
}