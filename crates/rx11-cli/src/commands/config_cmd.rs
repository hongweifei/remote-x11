//! `rx11 config` — manage `~/.config/rx11/config.toml`.

use crate::settings::{config_path, CONFIG_TEMPLATE};

pub async fn config_init() -> anyhow::Result<()> {
    let cfg_path = config_path()?;

    if cfg_path.exists() {
        anyhow::bail!("Config file already exists at {}", cfg_path.display());
    }

    if let Some(parent) = cfg_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&cfg_path, CONFIG_TEMPLATE).await?;
    println!("Config file created at {}", cfg_path.display());
    Ok(())
}

pub fn config_path_cmd() -> anyhow::Result<()> {
    println!("{}", config_path()?.display());
    Ok(())
}
