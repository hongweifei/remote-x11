//! Relay server: accepts client control connections, performs the handshake
//! and supervises one connection task per client.
//!
//! Two ways in:
//!
//! - [`RelayServer::run`] — the CLI entry point: bind, then block until
//!   `ctrl_c` or a shutdown request, then clean up.
//! - [`RelayServer::start`] — the embeddable entry point (GUI console,
//!   tests): binds synchronously so a port error surfaces immediately,
//!   spawns the accept loop and hands back a [`RelayHandle`] with the bound
//!   address, live stats, the session registry and an explicit
//!   [`RelayHandle::shutdown`].

mod connection;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{watch, Notify};
use tracing::{error, info, warn};

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

    /// Handle to the process-wide connection stats (flow-control counters
    /// and traffic totals) for tests and monitoring.
    pub fn stats(&self) -> Arc<ConnectionStats> {
        self.stats.clone()
    }

    /// Bind, install the X11 listener and spawn the accept loop.
    ///
    /// The bind happens on the calling thread before anything is spawned, so
    /// a port conflict (or a bad address) is returned as `Err` and the caller
    /// never sees a half-started server. The returned handle owns the
    /// lifecycle: dropping it does **not** stop the server — call
    /// [`RelayHandle::shutdown`] (and optionally
    /// [`RelayHandle::wait_stopped`]) explicitly.
    pub async fn start(&self) -> rx11_core::error::Result<RelayHandle> {
        let listener = tokio::net::TcpListener::bind(&self.listen_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("Relay server listening on {}", local_addr);

        let x11_listener = Arc::new(X11Listener::new(
            self.x11_base_port,
            self.session_mgr.clone(),
            self.runtime.clone(),
        ));
        // Installed before the loop is spawned: a client may connect the
        // instant the port is open, and its session needs the binder.
        self.session_mgr.set_x11_listener(x11_listener).await;

        let control = Arc::new(RelayControl::new());
        let handle = RelayHandle {
            local_addr,
            session_mgr: self.session_mgr.clone(),
            stats: self.stats.clone(),
            active_connections: self.active_connections.clone(),
            control: control.clone(),
        };

        let deps = ClientDeps {
            session_mgr: self.session_mgr.clone(),
            stats: self.stats.clone(),
            active_connections: self.active_connections.clone(),
            auth_token: self.auth_token.clone(),
            runtime: self.runtime.clone(),
        };

        tokio::spawn(async move {
            accept_loop(listener, control, deps).await;
        });

        Ok(handle)
    }

    /// Run until `ctrl_c` or until the accept loop stops on its own, then
    /// destroy all sessions. CLI behavior: unchanged from the pre-handle
    /// version, including the shutdown log lines.
    pub async fn run(&self) -> rx11_core::error::Result<()> {
        let handle = self.start().await?;

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal, cleaning up...");
                handle.shutdown();
            }
            _ = handle.wait_stopped() => {}
        }

        handle.wait_stopped().await;
        info!("Server shutdown complete");
        if let Some(err) = handle.control.take_error() {
            return Err(rx11_core::error::Rx11Error::Io(std::io::Error::other(err)));
        }
        Ok(())
    }

    pub fn session_manager(&self) -> Arc<SessionManager> {
        self.session_mgr.clone()
    }
}

/// Shared lifecycle state of one accept loop. Split out of [`RelayHandle`] so
/// the spawned task and every handle clone observe the same flags.
struct RelayControl {
    /// Wakes the accept loop. `notify_one` (not `notify_waiters`) so a
    /// shutdown requested between two `select!` iterations still leaves a
    /// permit for the next `notified()`.
    stop: Notify,
    /// `true` once the accept loop has finished cleaning up.
    stopped_tx: watch::Sender<bool>,
    stopped_rx: watch::Receiver<bool>,
    running: AtomicBool,
    /// First fatal accept-loop error, surfaced by [`RelayServer::run`].
    error: Mutex<Option<String>>,
}

impl RelayControl {
    fn new() -> Self {
        let (stopped_tx, stopped_rx) = watch::channel(false);
        Self {
            stop: Notify::new(),
            stopped_tx,
            stopped_rx,
            running: AtomicBool::new(true),
            error: Mutex::new(None),
        }
    }

    /// Idempotent: only the first request wakes the loop.
    fn request_stop(&self) {
        if self.running.swap(false, Ordering::SeqCst) {
            self.stop.notify_one();
        }
    }

    fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Embeddable control surface of a running relay: clone it freely, it is a
/// cheap handle onto the same server.
#[derive(Clone)]
pub struct RelayHandle {
    local_addr: SocketAddr,
    session_mgr: Arc<SessionManager>,
    stats: Arc<ConnectionStats>,
    active_connections: Arc<AtomicUsize>,
    control: Arc<RelayControl>,
}

impl std::fmt::Debug for RelayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayHandle")
            .field("local_addr", &self.local_addr)
            .field("active_connections", &self.active_connections())
            .field("running", &self.is_running())
            .finish()
    }
}

impl RelayHandle {
    /// The address actually bound (resolves `127.0.0.1:0` to the real port).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn stats(&self) -> Arc<ConnectionStats> {
        self.stats.clone()
    }

    /// Number of client control connections currently being served.
    pub fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }

    pub fn session_manager(&self) -> Arc<SessionManager> {
        self.session_mgr.clone()
    }

    /// Ask the accept loop to stop. Synchronous, non-blocking and idempotent:
    /// sessions are destroyed by the loop itself, so pair this with
    /// [`Self::wait_stopped`] when the caller must know cleanup finished.
    pub fn shutdown(&self) {
        self.control.request_stop();
    }

    /// Resolve once the accept loop has stopped accepting and finished
    /// destroying sessions. Returns immediately if it already stopped.
    pub async fn wait_stopped(&self) {
        let mut rx = self.control.stopped_rx.clone();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.control.running.load(Ordering::SeqCst)
    }
}

/// Everything one client task needs, cloned once per accepted connection so
/// the accept loop's own signature stays readable.
#[derive(Clone)]
struct ClientDeps {
    session_mgr: Arc<SessionManager>,
    stats: Arc<ConnectionStats>,
    active_connections: Arc<AtomicUsize>,
    auth_token: String,
    runtime: RuntimeConfig,
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    control: Arc<RelayControl>,
    deps: ClientDeps,
) {
    let stats_task = spawn_stats_reporter(deps.stats.clone(), ServerDefaults::STATUS_INTERVAL);
    // One shared audio capture for the whole relay: recorder + encoder,
    // fan-out per attached client. Lives as long as this loop.
    let audio_hub = crate::audio::AudioHub::new(deps.runtime.clone());

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        spawn_client(stream, addr, &deps, audio_hub.clone());
                    }
                    Err(e) => {
                        // The pre-handle server aborted the whole run on an
                        // accept error; keep that decision, but record it so
                        // `run()` can still report it.
                        *control
                            .error
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(format!("accept failed: {}", e));
                        error!("Relay accept loop failed: {}", e);
                        break;
                    }
                }
            }
            _ = control.stop.notified() => break,
        }
    }

    // Cleanup happens here, not in the handle: the accept loop owns the
    // listener and the session registry's lifetime. `run()` prints the
    // operator-facing shutdown lines, so embedders stopping through
    // `RelayHandle::shutdown` stay quiet by default.
    deps.session_mgr.destroy_all_sessions().await;
    stats_task.abort();
    control.running.store(false, Ordering::SeqCst);
    let _ = control.stopped_tx.send(true);
}

fn spawn_client(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    deps: &ClientDeps,
    audio: crate::audio::AudioHub,
) {
    let count = deps.active_connections.fetch_add(1, Ordering::Relaxed) + 1;
    if count > ServerDefaults::MAX_CONNECTIONS {
        deps.active_connections.fetch_sub(1, Ordering::Relaxed);
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

    let deps = deps.clone();
    tokio::spawn(async move {
        let result = handle_client(
            stream,
            addr,
            deps.session_mgr,
            deps.stats,
            deps.runtime,
            audio,
            &deps.auth_token,
        )
        .await;
        deps.active_connections.fetch_sub(1, Ordering::Relaxed);
        if let Err(e) = result {
            warn!("Client handler error: {}", e);
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TOKEN: &str = "relay-handle-token";

    /// A server bound to an ephemeral port; no X11 session is ever created,
    /// so no `xauth` child is spawned and no HOME is touched.
    async fn started() -> (RelayHandle, RelayServer) {
        let server = RelayServer::with_config(
            "127.0.0.1:0".to_string(),
            TOKEN.to_string(),
            17000,
            RuntimeConfig::default(),
        );
        let handle = server.start().await.expect("ephemeral bind must succeed");
        (handle, server)
    }

    #[tokio::test]
    async fn start_reports_the_bound_address_and_runs() {
        let (handle, _server) = started().await;
        assert_ne!(handle.local_addr().port(), 0, "resolved from :0");
        assert!(handle.is_running());
        assert_eq!(handle.active_connections(), 0);

        // The port is live: a plain TCP connect succeeds.
        tokio::net::TcpStream::connect(handle.local_addr())
            .await
            .expect("accept loop is listening");

        handle.shutdown();
        handle.wait_stopped().await;
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn start_fails_when_the_port_is_taken() {
        let first = RelayServer::with_config(
            "127.0.0.1:0".to_string(),
            TOKEN.to_string(),
            17000,
            RuntimeConfig::default(),
        );
        let handle = first.start().await.unwrap();
        let addr = handle.local_addr().to_string();

        let second = RelayServer::with_config(
            addr.clone(),
            TOKEN.to_string(),
            17000,
            RuntimeConfig::default(),
        );
        let err = second
            .start()
            .await
            .expect_err("binding a taken port must fail synchronously");
        assert!(matches!(err, rx11_core::error::Rx11Error::Io(_)), "{err}");

        handle.shutdown();
        handle.wait_stopped().await;
    }

    #[tokio::test]
    async fn shutdown_stops_accepting_new_connections() {
        let (handle, _server) = started().await;
        let addr = handle.local_addr();

        handle.shutdown();
        handle.wait_stopped().await;

        // The listener is dropped with the loop: connecting must fail. Retry
        // briefly because the OS may accept into the backlog for a moment.
        let mut refused = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_err() {
                refused = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(refused, "connections must be refused after shutdown");
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_and_wait_stopped_is_reusable() {
        let (handle, _server) = started().await;
        let clone = handle.clone();

        handle.shutdown();
        handle.shutdown();
        clone.wait_stopped().await;
        handle.wait_stopped().await;
        clone.shutdown();
        assert!(!clone.is_running());
    }

    #[tokio::test]
    async fn run_surfaces_a_bind_error_without_starting() {
        let first = RelayServer::with_config(
            "127.0.0.1:0".to_string(),
            TOKEN.to_string(),
            17000,
            RuntimeConfig::default(),
        );
        let handle = first.start().await.unwrap();
        let addr = handle.local_addr().to_string();

        let second =
            RelayServer::with_config(addr, TOKEN.to_string(), 17000, RuntimeConfig::default());
        let result = tokio::time::timeout(Duration::from_secs(5), second.run())
            .await
            .expect("run must not block when the bind fails");
        assert!(result.is_err(), "port conflict must surface from run()");

        handle.shutdown();
        handle.wait_stopped().await;
    }
}
