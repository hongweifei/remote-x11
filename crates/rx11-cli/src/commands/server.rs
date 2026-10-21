//! `rx11 server` — run the relay server.

use rx11_core::auth::generate_token;
use rx11_core::config::{RuntimeConfig, ServerDefaults};
use rx11_core::protocol::DEFAULT_X11_PORT;

use crate::settings::load_config;

pub async fn server(
    listen: Option<String>,
    x11_port: Option<u16>,
    token: Option<String>,
) -> anyhow::Result<()> {
    let config = load_config();
    let sc = config.server.unwrap_or_default();
    let runtime = config
        .runtime
        .unwrap_or_default()
        .to_runtime();

    let listen = listen
        .or(sc.listen)
        .unwrap_or_else(|| ServerDefaults::LISTEN_ADDR.to_string());
    let x11_port = x11_port.or(sc.x11_port).unwrap_or(DEFAULT_X11_PORT);
    let token = token.or(sc.token).unwrap_or_else(|| {
        let t = generate_token();
        eprintln!("Generated token: {}", t);
        eprintln!("Save this token, it is required for client connections");
        t
    });

    rx11_server::relay::RelayServer::with_config(listen, token, x11_port, runtime)
        .run()
        .await
        .map_err(Into::into)
}
