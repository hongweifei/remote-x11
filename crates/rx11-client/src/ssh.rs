use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Notify;
use tracing::{info, warn};

const HEALTH_CHECK_INTERVAL_SECS: u64 = 10;
const HEALTH_CHECK_TIMEOUT_SECS: u64 = 5;
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

pub struct SshTunnel {
    child: tokio::process::Child,
    local_addr: String,
    cancel: Arc<Notify>,
    health_task: tokio::task::JoinHandle<()>,
}

impl SshTunnel {
    pub async fn create(
        ssh_host: &str,
        ssh_port: u16,
        ssh_user: Option<&str>,
        remote_relay_port: u16,
        local_bind_port: u16,
        identity_file: Option<&str>,
    ) -> anyhow::Result<Self> {
        let child = create_forward_tunnel(
            ssh_host,
            ssh_port,
            ssh_user,
            remote_relay_port,
            local_bind_port,
            identity_file,
        )
        .await?;

        let local_addr = format!("127.0.0.1:{}", local_bind_port);
        let cancel = Arc::new(Notify::new());
        let cancel_clone = cancel.clone();
        let local_addr_clone = local_addr.clone();

        let health_task = tokio::spawn(async move {
            health_check_loop(&local_addr_clone, cancel_clone).await;
        });

        Ok(Self {
            child,
            local_addr,
            cancel,
            health_task,
        })
    }

    pub async fn wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        self.child.wait().await.map_err(Into::into)
    }

    pub async fn kill(&mut self) -> anyhow::Result<()> {
        self.cancel.notify_one();
        self.health_task.abort();
        self.child.kill().await.map_err(Into::into)
    }

    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }
}

async fn health_check_loop(local_addr: &str, cancel: Arc<Notify>) {
    let mut interval = tokio::time::interval(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS));
    interval.tick().await;

    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.notified() => break,
            _ = interval.tick() => {
                let healthy = check_tunnel_health(local_addr).await;
                if healthy {
                    if consecutive_failures > 0 {
                        info!("SSH tunnel health check recovered");
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    warn!(
                        "SSH tunnel health check failed ({}/{})",
                        consecutive_failures, MAX_CONSECUTIVE_FAILURES
                    );
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        warn!(
                            "SSH tunnel health check failed {} consecutive times, tunnel appears dead",
                            MAX_CONSECUTIVE_FAILURES
                        );
                        break;
                    }
                }
            }
        }
    }
}

async fn check_tunnel_health(addr: &str) -> bool {
    let result = tokio::time::timeout(
        Duration::from_secs(HEALTH_CHECK_TIMEOUT_SECS),
        async {
            let mut stream = TcpStream::connect(addr).await?;
            stream.shutdown().await?;
            Ok::<(), std::io::Error>(())
        },
    )
    .await;

    match result {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

pub async fn create_forward_tunnel(
    ssh_host: &str,
    ssh_port: u16,
    ssh_user: Option<&str>,
    remote_relay_port: u16,
    local_bind_port: u16,
    identity_file: Option<&str>,
) -> anyhow::Result<tokio::process::Child> {
    let mut args = Vec::new();

    args.push("-N".to_string());
    args.push("-T".to_string());
    args.push("-o".to_string());
    args.push("ExitOnForwardFailure=yes".to_string());
    args.push("-o".to_string());
    args.push("BatchMode=yes".to_string());
    args.push("-o".to_string());
    args.push("StrictHostKeyChecking=accept-new".to_string());
    args.push("-o".to_string());
    args.push("ServerAliveInterval=15".to_string());
    args.push("-o".to_string());
    args.push("ServerAliveCountMax=3".to_string());

    args.push("-L".to_string());
    args.push(format!(
        "127.0.0.1:{}:127.0.0.1:{}",
        local_bind_port, remote_relay_port
    ));

    if let Some(id) = identity_file {
        args.push("-i".to_string());
        args.push(id.to_string());
    }

    args.push("-p".to_string());
    args.push(ssh_port.to_string());

    let target = match ssh_user {
        Some(u) => format!("{}@{}", u, ssh_host),
        None => ssh_host.to_string(),
    };
    args.push(target);

    info!(
        "Starting SSH tunnel: {}@{}:{} -> 127.0.0.1:{}",
        ssh_user.unwrap_or("<default>"),
        ssh_host,
        ssh_port,
        local_bind_port
    );

    let child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .spawn()?;

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_check_tunnel_health_unreachable() {
        assert!(!check_tunnel_health("127.0.0.1:1").await);
    }

    #[tokio::test]
    async fn test_check_tunnel_health_with_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let listener_task = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });

        assert!(check_tunnel_health(&format!("127.0.0.1:{}", port)).await);
        listener_task.abort();
    }
}
