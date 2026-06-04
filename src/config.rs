use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::client::ServerConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub active_server: Option<String>,
    #[serde(default)]
    pub active_database: Option<String>,
    #[serde(default)]
    pub active_modules_database: Option<String>,
    #[serde(default)]
    pub active_app_server: Option<String>,
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

        // Validate: try to parse what we just serialized
        if let Err(e) = toml::from_str::<AppConfig>(&content) {
            bail!("Generated invalid TOML, refusing to write: {}", e);
        }

        // Write to temp file first, then rename atomically
        let tmp_path = path.with_extension("toml.tmp");
        fs::write(&tmp_path, &content)?;
        fs::rename(&tmp_path, &path)?;

        Ok(())
    }

    pub fn active_server_config(&self) -> Option<&ServerConfig> {
        self.active_server
            .as_ref()
            .and_then(|name| self.servers.iter().find(|s| &s.name == name))
    }

    pub fn add_server(&mut self, server: ServerConfig) {
        self.servers.retain(|s| s.name != server.name);
        self.active_server = Some(server.name.clone());
        self.servers.push(server);
    }

    pub fn remove_server(&mut self, name: &str) {
        self.servers.retain(|s| s.name != name);
        if self.active_server.as_deref() == Some(name) {
            self.active_server = self.servers.first().map(|s| s.name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;
    use crate::client::ServerConfig;

    fn server(name: &str, uri: &str) -> ServerConfig {
        use crate::client::AuthType;
        ServerConfig {
            name: name.to_string(),
            uri: uri.to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8003,
            auth_type: AuthType::Digest,
        }
    }

    #[test]
    fn add_server_makes_new_server_active() {
        let mut config = AppConfig {
            servers: vec![server("old", "http://old.example")],
            active_server: Some("old".to_string()),
            active_database: Some("Documents".to_string()),
            active_modules_database: Some("Modules".to_string()),
            active_app_server: Some("App-Services".to_string()),
        };

        config.add_server(server("new", "http://new.example"));

        assert_eq!(config.active_server.as_deref(), Some("new"));
        assert_eq!(config.servers.len(), 2);
    }

    #[test]
    fn replacing_server_keeps_replacement_active() {
        let mut config = AppConfig {
            servers: vec![server("local", "http://old.example")],
            active_server: Some("local".to_string()),
            active_database: None,
            active_modules_database: None,
            active_app_server: None,
        };

        config.add_server(server("local", "http://new.example"));

        assert_eq!(config.active_server.as_deref(), Some("local"));
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].uri, "http://new.example");
    }
}
