//! Small CLI helpers shared by the client and ssh commands.

use std::time::{Duration, Instant};

use tracing::{info, warn};

use rx11_core::config::ClientDefaults;

/// Missing-token error shared by `client` and `ssh`.
pub const TOKEN_REQUIRED_MSG: &str =
    "Missing auth token. Use --token, RX11_TOKEN env var, or set it in config file";

/// Probe the local X Server so users get an early, actionable warning.
pub async fn detect_x11_server(addr: &str) {
    match tokio::time::timeout(
        ClientDefaults::X11_DETECT_TIMEOUT,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    {
        Ok(Ok(_)) => {
            info!("Local X Server detected at {}", addr);
        }
        _ => {
            warn!("No X Server detected at {}", addr);
            eprintln!(
                "WARNING: No X Server detected at {}. Start an X Server before connecting (VcXsrv/XQuartz/Xorg).",
                addr
            );
        }
    }
}

/// Poll `addr` with a capped exponential delay until it accepts connections.
pub async fn wait_for_port(addr: &str, timeout_duration: Duration) -> anyhow::Result<()> {
    const INITIAL_DELAY: Duration = Duration::from_millis(50);
    const MAX_DELAY: Duration = Duration::from_millis(500);

    let start = Instant::now();
    let mut delay = INITIAL_DELAY;
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if start.elapsed() > timeout_duration {
                    return Err(anyhow::anyhow!("Timeout waiting for {}", addr));
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_DELAY);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn wait_for_port_returns_once_port_accepts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        wait_for_port(&addr, Duration::from_millis(1500))
            .await
            .expect("live port must be detected");
    }

    #[tokio::test]
    async fn wait_for_port_times_out_on_dead_port() {
        // Bind then drop so the port is guaranteed closed (nothing listens).
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().to_string()
        };
        let start = Instant::now();
        let err = wait_for_port(&addr, Duration::from_millis(250))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Timeout waiting for"), "{err}");
        assert!(start.elapsed() >= Duration::from_millis(250));
    }
}
