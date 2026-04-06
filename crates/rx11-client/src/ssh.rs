use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

pub struct SshClient;

impl SshClient {
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

        info!("Starting SSH tunnel: ssh {}", args.join(" "));

        let child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .spawn()?;

        Ok(child)
    }
}
