//! `rx11 client` — direct TCP connection to the relay server.

use rx11_client::connector::LocalConnector;
use rx11_core::config::ClientDefaults;

use crate::settings::load_config;
use crate::util::{detect_x11_server, TOKEN_REQUIRED_MSG};

pub async fn client(
    relay: Option<String>,
    token: Option<String>,
    x11: Option<String>,
    display: Option<u16>,
    auto: Option<bool>,
) -> anyhow::Result<()> {
    let config = load_config();
    let cc = config.client.unwrap_or_default();

    let relay = relay.or(cc.relay).ok_or_else(|| {
        anyhow::anyhow!("Missing relay server address. Use --relay or set it in config file")
    })?;
    let x11 = x11
        .or(cc.x11)
        .unwrap_or_else(|| ClientDefaults::X11_ADDR.to_string());

    let token = token
        .or(cc.token)
        .ok_or_else(|| anyhow::anyhow!(TOKEN_REQUIRED_MSG))?;

    let auto_display = auto.unwrap_or(true) && display.is_none() && cc.display.is_none();

    detect_x11_server(&x11).await;
    LocalConnector::new(relay, token, x11, display, auto_display)
        .connect_and_serve()
        .await
}
