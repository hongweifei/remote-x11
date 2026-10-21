//! `rx11 ssh` — connect through an SSH tunnel to the remote relay server.
//!
//! The tunnel runs under [`ssh::supervise`]: if the ssh child dies or the
//! health check gives up, a replacement tunnel is spawned on the same local
//! port with exponential backoff while the connector keeps retrying — so a
//! link blip resumes the session instead of ending the command. Disable via
//! `ssh.reconnect = false` in the config file to get the old fail-fast shape
//! back.

use rx11_client::connector::LocalConnector;
use rx11_client::ssh::{self, SshTunnel, SupervisionEnd, TunnelDeath, TunnelHandle};
use rx11_core::config::{ClientDefaults, SshDefaults};
use rx11_core::protocol::DEFAULT_RELAY_PORT;
use tracing::info;

use crate::cli::SshArgs;
use crate::settings::load_config;
use crate::util::{detect_x11_server, wait_for_port, TOKEN_REQUIRED_MSG};

/// Everything needed to (re)open the forward; owned so the rebuild closure
/// is self-contained across tunnel deaths.
#[derive(Clone)]
struct TunnelSpec {
    host: String,
    port: u16,
    user: Option<String>,
    identity: Option<String>,
    relay_port: u16,
    local_bind_port: u16,
}

/// Spawn the tunnel and wait until the forwarded port accepts connections.
/// A child that exits before the port opens (bad key/host, port clash) is
/// reported immediately instead of burning the full READY_TIMEOUT.
async fn open_tunnel(spec: &TunnelSpec) -> anyhow::Result<SshTunnel> {
    let mut tunnel = SshTunnel::create(
        &spec.host,
        spec.port,
        spec.user.as_deref(),
        spec.relay_port,
        spec.local_bind_port,
        spec.identity.as_deref(),
    )
    .await?;
    let local = tunnel.local_addr().to_owned();
    let ready = tokio::select! {
        r = wait_for_port(&local, SshDefaults::READY_TIMEOUT) => r,
        status = tunnel.wait() => {
            let _ = tunnel.kill().await;
            let status = status?;
            Err(anyhow::anyhow!(
                "ssh exited before the tunnel came up: {status} (check host, port, and key auth)"
            ))
        }
    };
    if let Err(e) = ready {
        let _ = tunnel.kill().await;
        return Err(e.context("SSH tunnel failed to become ready"));
    }
    Ok(tunnel)
}

pub async fn ssh(args: SshArgs) -> anyhow::Result<()> {
    let SshArgs {
        host,
        port,
        user,
        identity,
        token,
        relay_port,
        x11,
        display,
        auto,
    } = args;
    let config = load_config();
    let sc = config.ssh.unwrap_or_default();

    let host = host.or(sc.host).ok_or_else(|| {
        anyhow::anyhow!("Missing remote server address. Use --host or set it in config file")
    })?;

    let port = port.or(sc.port).unwrap_or(SshDefaults::DEFAULT_PORT);
    let relay_port = relay_port.or(sc.relay_port).unwrap_or(DEFAULT_RELAY_PORT);
    let x11 = x11
        .or(sc.x11)
        .unwrap_or_else(|| ClientDefaults::X11_ADDR.to_string());

    let auto_display = auto.unwrap_or(true) && display.is_none() && sc.display.is_none();

    let token = token
        .or(sc.token)
        .ok_or_else(|| anyhow::anyhow!(TOKEN_REQUIRED_MSG))?;

    let user = user.or(sc.user);
    let identity = identity.or(sc.identity);

    let local_bind_port =
        pick_local_bind_port(auto_display, display.or(sc.display).unwrap_or(0)).await?;

    if !auto_display {
        let check_addr = format!("127.0.0.1:{}", local_bind_port);
        if tokio::net::TcpStream::connect(&check_addr).await.is_ok() {
            anyhow::bail!(
                "Port {} is already in use. Another rx11 ssh instance may be running, or use a different display (-d) to pick another port",
                local_bind_port
            );
        }
    }

    let spec = TunnelSpec {
        host,
        port,
        user,
        identity,
        relay_port,
        local_bind_port,
    };

    // The first tunnel comes up strictly: config-level failures (bad host,
    // missing key) should surface immediately, not burn the rebuild budget.
    let tunnel = open_tunnel(&spec).await?;
    let local_addr = tunnel.local_addr().to_string();

    detect_x11_server(&x11).await;

    let auto_reconnect = sc.reconnect.unwrap_or(true);
    if auto_reconnect {
        info!(
            "SSH tunnel auto-rebuild enabled (up to {} attempts per drop, backoff {:?}..{:?})",
            SshDefaults::REBUILD_MAX_ATTEMPTS,
            SshDefaults::REBUILD_BASE_DELAY,
            SshDefaults::REBUILD_MAX_DELAY
        );
    }

    let runtime = config.runtime.unwrap_or_default().to_runtime();
    let connector =
        LocalConnector::new(local_addr, token, x11, display, auto_display).with_runtime(runtime);

    let rebuild_spec = spec.clone();
    let end = ssh::supervise(
        Box::new(tunnel),
        connector.connect_and_serve(),
        move || {
            let spec = rebuild_spec.clone();
            async move {
                let t = open_tunnel(&spec).await?;
                Ok(Box::new(t) as Box<dyn TunnelHandle>)
            }
        },
        ssh::RebuildPolicy::default(),
        auto_reconnect,
    )
    .await;

    match end {
        SupervisionEnd::Session(r) => r,
        SupervisionEnd::TunnelDown(death) => match death {
            TunnelDeath::Exit(status) => {
                anyhow::bail!("SSH tunnel exited with status: {:?}", status?)
            }
            TunnelDeath::Unhealthy => {
                anyhow::bail!("SSH tunnel health check failed; aborting session")
            }
        },
        SupervisionEnd::RebuildExhausted {
            attempts,
            last_error,
        } => anyhow::bail!(
            "SSH tunnel could not be rebuilt after {} attempts: {} (check the network, or set ssh.reconnect = false to fail fast)",
            attempts,
            last_error
        ),
    }
}

/// Fixed-display mode uses `17000 + display` so multiple sessions get stable
/// ports; auto mode grabs an ephemeral port from the OS.
async fn pick_local_bind_port(auto_display: bool, display: u16) -> anyhow::Result<u16> {
    if auto_display {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    } else {
        Ok(SshDefaults::LOCAL_BIND_BASE.saturating_add(display))
    }
}
