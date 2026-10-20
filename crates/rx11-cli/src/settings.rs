//! TOML configuration file support: location, schema, loading and the
//! template written by `rx11 config init`.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::{info, warn};

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub client: Option<ClientConfig>,
    pub server: Option<ServerConfig>,
    pub ssh: Option<SshConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ClientConfig {
    pub relay: Option<String>,
    pub token: Option<String>,
    pub x11: Option<String>,
    pub display: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServerConfig {
    pub listen: Option<String>,
    pub x11_port: Option<u16>,
    pub token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SshConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity: Option<String>,
    pub token: Option<String>,
    pub relay_port: Option<u16>,
    pub x11: Option<String>,
    pub display: Option<u16>,
}

/// Written by `rx11 config init`.
pub const CONFIG_TEMPLATE: &str = r#"# rx11 configuration file
# Priority: command-line args > environment variables > this config > defaults

[client]
# relay = "my-server:7000"
# token = "your-token-here"
# x11 = "127.0.0.1:6000"
# display = 0

[server]
# listen = "0.0.0.0:7000"
# x11_port = 6000
# token = "your-token-here"

[ssh]
# host = "my-server"
# port = 22
# user = "myuser"
# identity = "~/.ssh/id_rsa"
"#;

fn config_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from))
}

/// `~/.config/rx11/config.toml`
pub fn config_path() -> anyhow::Result<PathBuf> {
    config_home()
        .map(|h| h.join(".config").join("rx11").join("config.toml"))
        .ok_or_else(|| {
            anyhow::anyhow!("Cannot determine config directory (HOME/USERPROFILE not set)")
        })
}

/// Load the config file; a missing or broken file silently yields defaults
/// (with a warning), keeping CLI startup resilient.
pub fn load_config() -> Config {
    let Ok(path) = config_path() else {
        return Config::default();
    };
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
                Ok(config) => {
                    info!("Loaded config from {}", path.display());
                    return config;
                }
                Err(e) => {
                    warn!("Failed to parse config file {}: {}", path.display(), e);
                    eprintln!(
                        "WARNING: Failed to parse config file {}: {}. Using default config.",
                        path.display(),
                        e
                    );
                }
            },
            Err(e) => {
                warn!("Failed to read config file {}: {}", path.display(), e);
            }
        }
    }
    Config::default()
}
