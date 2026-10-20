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

/// Test helper: serialize every test that touches HOME/USERPROFILE, since
/// env is process-global and cargo runs tests in parallel threads.
#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn non_empty_var(key: &str) -> Option<PathBuf> {
    // An empty-but-set HOME (e.g. `HOME= cmd`) must not degrade config paths
    // to relative paths; treat it as unset.
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn config_home() -> Option<PathBuf> {
    non_empty_var("HOME").or_else(|| non_empty_var("USERPROFILE"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_template_parses_with_all_sections() {
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).expect("template must be valid TOML");
        assert!(cfg.client.is_some());
        assert!(cfg.server.is_some());
        assert!(cfg.ssh.is_some());
    }

    #[tokio::test]
    async fn config_path_and_load_follow_env_precedence() {
        let _g = ENV_LOCK.lock().await;
        let saved_home = std::env::var("HOME").ok();
        let saved_profile = std::env::var("USERPROFILE").ok();

        let dir = std::env::temp_dir().join(format!("rx11-cfg-{}", std::process::id()));
        let cfg_dir = dir.join(".config").join("rx11");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_file = cfg_dir.join("config.toml");

        std::env::set_var("HOME", &dir);
        assert!(config_path().unwrap().starts_with(&dir));

        std::fs::write(
            &cfg_file,
            "[client]\nrelay = \"srv:7000\"\ntoken = \"tk\"\n",
        )
        .unwrap();
        let cfg = load_config();
        assert_eq!(cfg.client.unwrap().relay.as_deref(), Some("srv:7000"));

        // broken TOML degrades to defaults instead of failing startup
        std::fs::write(&cfg_file, "this is ]=[ not toml").unwrap();
        let cfg = load_config();
        assert!(cfg.client.is_none());

        // empty HOME is ignored; falls back to USERPROFILE
        std::env::set_var("HOME", "");
        std::env::set_var("USERPROFILE", &dir);
        std::fs::write(&cfg_file, "[server]\nlisten = \"0.0.0.0:9\"\n").unwrap();
        let cfg = load_config();
        assert_eq!(cfg.server.unwrap().listen.as_deref(), Some("0.0.0.0:9"));

        let _ = std::fs::remove_dir_all(&dir);
        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match saved_profile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
    }
}
