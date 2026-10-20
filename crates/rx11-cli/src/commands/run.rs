//! `rx11 run` — launch a GUI program with DISPLAY preset, forwarding
//! Ctrl+C to the child process on shutdown.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::info;

pub async fn run(display: u16, command: Vec<String>) -> anyhow::Result<()> {
    let display_str = format!(":{}", display);
    info!(
        "Setting DISPLAY={} and running: {}",
        display_str,
        command.join(" ")
    );

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .env("DISPLAY", &display_str)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let status = tokio::select! {
        status = child.wait() => status.map_err(anyhow::Error::from),
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, waiting for child process to exit...");
            terminate_child(&mut child).await
        }
    }?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Graceful-then-forced shutdown of the child, platform-specific.
#[cfg(unix)]
async fn terminate_child(
    child: &mut tokio::process::Child,
) -> anyhow::Result<std::process::ExitStatus> {
    const GRACE_PERIOD: Duration = Duration::from_secs(3);

    if let Some(id) = child.id() {
        let _ = unsafe { libc::kill(id as libc::pid_t, libc::SIGTERM) };
    }
    tokio::time::sleep(GRACE_PERIOD).await;
    match child.try_wait()? {
        Some(status) => Ok(status),
        None => {
            info!("Child did not exit, sending SIGKILL...");
            if let Some(id) = child.id() {
                let _ = unsafe { libc::kill(id as libc::pid_t, libc::SIGKILL) };
            }
            child.wait().await.map_err(Into::into)
        }
    }
}

#[cfg(windows)]
async fn terminate_child(
    child: &mut tokio::process::Child,
) -> anyhow::Result<std::process::ExitStatus> {
    tokio::time::sleep(Duration::from_secs(2)).await;
    match child.try_wait()? {
        Some(status) => Ok(status),
        None => {
            child.kill().await?;
            child.wait().await.map_err(Into::into)
        }
    }
}
