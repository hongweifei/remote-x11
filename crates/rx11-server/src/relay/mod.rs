//! Relay server: accepts client control connections, performs the handshake
//! and supervises one connection task per client.

mod connection;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tracing::{info, warn};

use rx11_core::config::{RuntimeConfig, ServerDefaults};
use rx11_core::stats::ConnectionStats;

use crate::session::SessionManager;
use crate::x11_listener::X11Listener;

use connection::handle_client;

pub struct RelayServer {
    listen_addr: String,
    auth_token: String,
    session_mgr: Arc<SessionManager>,
    x11_base_port: u16,
    active_connections: Arc<AtomicUsize>,
    stats: Arc<ConnectionStats>,
    runtime: RuntimeConfig,
}

impl RelayServer {
    /// Relay server with the default runtime profile.
    pub fn new(listen_addr: String, auth_token: String, x11_base_port: u16) -> Self {
        Self::with_config(
            listen_addr,
            auth_token,
            x11_base_port,
            RuntimeConfig::default(),
        )
    }

    pub fn with_config(
        listen_addr: String,
        auth_token: String,
        x11_base_port: u16,
        runtime: RuntimeConfig,
    ) -> Self {
        Self {
            listen_addr,
            auth_token,
            session_mgr: Arc::new(SessionManager::new()),
            x11_base_port,
            active_connections: Arc::new(AtomicUsize::new(0)),
            stats: Arc::new(ConnectionStats::new()),
            runtime,
        }
    }

    pub async fn run(&self) -> rx11_core::error::Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.listen_addr).await?;
        info!("Relay server listening on {}", self.listen_addr);

        let x11_listener = Arc::new(X11Listener::new(
            self.x11_base_port,
            self.session_mgr.clone(),
        ));
        self.session_mgr.set_x11_listener(x11_listener).await;

        let stats_task = spawn_stats_reporter(self.stats.clone(), ServerDefaults::STATUS_INTERVAL);
        // One shared audio capture for the whole relay: recorder + encoder,
        // fan-out per attached client. Lives as long as this loop.
        let audio_hub = crate::audio::AudioHub::new(self.runtime.clone());

        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, addr) = result?;
                    self.spawn_client(stream, addr, audio_hub.clone());
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received shutdown signal, cleaning up...");
                    self.session_mgr.destroy_all_sessions().await;
                    info!("Server shutdown complete");
                    break;
                }
            }
        }

        stats_task.abort();
        Ok(())
    }

    fn spawn_client(
        &self,
        stream: tokio::net::TcpStream,
        addr: std::net::SocketAddr,
        audio: crate::audio::AudioHub,
    ) {
        let count = self.active_connections.fetch_add(1, Ordering::Relaxed) + 1;
        if count > ServerDefaults::MAX_CONNECTIONS {
            self.active_connections.fetch_sub(1, Ordering::Relaxed);
            warn!(
                "Rejected connection from {}: max connections ({}) reached",
                addr,
                ServerDefaults::MAX_CONNECTIONS
            );
            return;
        }
        info!(
            "New connection from {} ({}/{})",
            addr,
            count,
            ServerDefaults::MAX_CONNECTIONS
        );

        let session_mgr = self.session_mgr.clone();
        let stats = self.stats.clone();
        let auth_token = self.auth_token.clone();
        let conn_counter = self.active_connections.clone();
        let runtime = self.runtime.clone();

        tokio::spawn(async move {
            let result = handle_client(
                stream,
                addr,
                session_mgr,
                stats,
                runtime,
                audio,
                &auth_token,
            )
            .await;
            conn_counter.fetch_sub(1, Ordering::Relaxed);
            if let Err(e) = result {
                warn!("Client handler error: {}", e);
            }
        });
    }

    pub fn session_manager(&self) -> Arc<SessionManager> {
        self.session_mgr.clone()
    }
}

/// Periodically logs a server-wide activity summary; skips idle servers to
/// keep the log quiet when nothing is flowing.
fn spawn_stats_reporter(
    stats: Arc<ConnectionStats>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval);
        timer.tick().await; // first tick completes immediately
        loop {
            timer.tick().await;
            if !stats.is_idle() {
                info!("[rx11] Relay status | {}", stats.summary());
                stats.reset_period();
            }
        }
    })
}
