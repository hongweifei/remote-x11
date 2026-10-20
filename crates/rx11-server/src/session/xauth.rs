//! Thin wrappers around the `xauth` CLI used to register and revoke
//! MIT-MAGIC-COOKIE entries for proxied displays.
//!
//! These shell out to `xauth`; in unit tests they are stubbed so session
//! management can be exercised without an X environment.

#[cfg(not(test))]
use tokio::process::Command;
#[cfg(not(test))]
use tracing::{info, warn};

/// Add a cookie for `:<disp>`, so local X clients can authenticate.
#[cfg(not(test))]
pub(super) async fn xauth_add(disp: u16, auth_data: &[u8], auth_name: &str) -> anyhow::Result<()> {
    let cookie_hex = hex::encode(auth_data);
    let display_str = format!(":{}", disp);
    let output = Command::new("xauth")
        .args(["add", &display_str, auth_name, &cookie_hex])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to execute xauth: {}", e))?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "xauth add {} failed: {}",
            display_str,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    info!("xauth add {} succeeded", display_str);
    Ok(())
}

/// Remove the cookie for `:<disp>`, logging but never failing.
#[cfg(not(test))]
pub(super) async fn xauth_remove(disp: u16) {
    let display_str = format!(":{}", disp);
    match Command::new("xauth")
        .args(["remove", &display_str])
        .output()
        .await
    {
        Ok(_) => info!("xauth remove {} done", display_str),
        Err(e) => warn!("Failed to execute xauth remove: {}", e),
    }
}

#[cfg(test)]
pub(super) async fn xauth_add(_disp: u16, _auth_data: &[u8], _auth_name: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
pub(super) async fn xauth_remove(_disp: u16) {}
