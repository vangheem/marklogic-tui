use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::client::ServerConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    pub active_server: Option<String>,
    pub active_database: Option<String>,
}

impl AppConfig {
    pub fn config_path() -> PathBuf {
        let dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("marklogic-tui");
        fs::create_dir_all(&dir).ok();
        dir.join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            Ok(toml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    pub fn active_server_config(&self) -> Option<&ServerConfig> {
        self.active_server
            .as_ref()
            .and_then(|name| self.servers.iter().find(|s| &s.name == name))
    }

    pub fn add_server(&mut self, server: ServerConfig) {
        self.servers.retain(|s| s.name != server.name);
        if self.active_server.is_none() {
            self.active_server = Some(server.name.clone());
        }
        self.servers.push(server);
    }

    pub fn remove_server(&mut self, name: &str) {
        self.servers.retain(|s| s.name != name);
        if self.active_server.as_deref() == Some(name) {
            self.active_server = self.servers.first().map(|s| s.name.clone());
        }
    }
}
