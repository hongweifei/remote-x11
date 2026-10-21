//! `rx11 ssh` — connect through an SSH tunnel to the remote relay server.

use rx11_client::connector::LocalConnector;
use rx11_client::ssh::SshTunnel;
use rx11_core::config::{ClientDefaults, SshDefaults};
use rx11_core::protocol::DEFAULT_RELAY_PORT;

use crate::cli::SshArgs;
use crate::settings::load_config;
use crate::util::{detect_x11_server, wait_for_port, TOKEN_REQUIRED_MSG};

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

    let mut ssh_tunnel = SshTunnel::create(
        &host,
        port,
        user.as_deref(),
        relay_port,
        local_bind_port,
        identity.as_deref(),
    )
    .await?;

    let local_addr = ssh_tunnel.local_addr().to_string();
    if let Err(e) = wait_for_port(&local_addr, SshDefaults::READY_TIMEOUT).await {
        let _ = ssh_tunnel.kill().await;
        return Err(e.context("SSH tunnel failed to become ready"));
    }

    detect_x11_server(&x11).await;

    let runtime = config
        .runtime
        .unwrap_or_default()
        .to_runtime();
    let connector = LocalConnector::new(local_addr, token, x11, display, auto_display)
        .with_runtime(runtime);

    /// Which branch ended the session.
    enum SessionEnd {
        Connector(anyhow::Result<()>),
        TunnelExit(anyhow::Result<std::process::ExitStatus>),
        Unhealthy,
    }

    let dead = ssh_tunnel.dead_notify();
    let outcome = tokio::select! {
        r = connector.connect_and_serve() => SessionEnd::Connector(r),
        status = ssh_tunnel.wait() => SessionEnd::TunnelExit(status),
        _ = dead.notified() => SessionEnd::Unhealthy,
    };

    // Idempotent cleanup for every exit path (the health task's abort and
    // child kill live inside SshTunnel::kill).
    let _ = ssh_tunnel.kill().await;

    match outcome {
        SessionEnd::Connector(r) => r,
        SessionEnd::TunnelExit(status) => {
            anyhow::bail!("SSH tunnel exited with status: {:?}", status?)
        }
        SessionEnd::Unhealthy => {
            anyhow::bail!("SSH tunnel health check failed; aborting session")
        }
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
