//! Client connector: establishes the relay connection (handshake, session
//! creation or resume), runs the proxy receive loop and reconnects with
//! exponential backoff on transient failures.
//!
//! Lifecycle/observability seam for embedders: [`LocalConnector::handle`]
//! hands out a [`ConnectorHandle`] over the same state, so a GUI can render
//! the phase/display/session and call [`ConnectorHandle::stop`] without
//! owning the future returned by [`LocalConnector::connect_and_serve`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::RngExt;
use tokio::net::TcpStream;
use tokio::sync::{watch, Notify};
use tracing::{error, info, warn};

use rx11_core::auth::generate_display_cookie;
use rx11_core::config::{BufferDefaults, ClientDefaults, RuntimeConfig};
use rx11_core::dataplane::{DataPlane, DataPlaneSide};
use rx11_core::error::{is_retriable_io_error, Rx11Error};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{
    client_handshake, Frame, SessionAckMessage, SessionAutoCreateMessage, SessionCreateMessage,
    SessionResumeMessage,
};
use rx11_core::stats::ConnectionStats;
use rx11_core::transport::Rx11Transport;
use rx11_core::types::{DisplayNumber, SessionId, Token};

use crate::proxy::{cleanup_connections, new_shared_conns, ProxyDeps};
use crate::serve::{serve, ServeCtx, ServeFlow};

/// X11 authorization protocol the client always requests.
const X11_AUTH_NAME: &str = "MIT-MAGIC-COOKIE-1";
/// Ceiling of the exponential backoff doubling (`base * 2^cap`).
const MAX_BACKOFF_EXPONENT: u32 = 10;

/// Where a connector currently is in its connect → serve → retry cycle.
///
/// `Default` is [`ConnectorPhase::Stopped`]: a connector that was never
/// started has not failed, it simply is not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectorPhase {
    /// Opening the TCP connection to the relay.
    Connecting,
    /// TCP is up; negotiating the handshake and creating/resuming a session.
    Handshaking,
    /// Session established, relaying X11 traffic.
    Serving,
    /// An attempt failed with a retriable error; waiting out the backoff.
    Reconnecting,
    /// Finished cleanly (normal end of session or an explicit stop).
    #[default]
    Stopped,
    /// Finished with a fatal error (non-retriable, or retries exhausted).
    Failed,
}

/// Point-in-time snapshot of a connector, cheap to clone and to render.
#[derive(Debug, Clone, Default)]
pub struct ConnectorState {
    pub phase: ConnectorPhase,
    /// Display of the current (or last) session.
    pub display: Option<u16>,
    pub session_id: Option<String>,
    /// 1-based retry counter of the current backoff cycle (0 while the first
    /// attempt is in flight).
    pub reconnect_attempt: u32,
    /// Negotiated compression algorithm, `None` when the peer disabled it.
    pub compression: Option<String>,
    /// Negotiated capability names, sorted (`Features::to_list`).
    pub features: Vec<String>,
    /// Message of the most recent failure; cleared once an attempt reaches
    /// [`ConnectorPhase::Serving`] again.
    pub last_error: Option<String>,
}

impl ConnectorState {
    /// True while the connector is doing work (i.e. not stopped/failed).
    pub fn is_running(&self) -> bool {
        matches!(
            self.phase,
            ConnectorPhase::Connecting
                | ConnectorPhase::Handshaking
                | ConnectorPhase::Serving
                | ConnectorPhase::Reconnecting
        )
    }
}

/// Shared connector lifecycle state: one instance per [`LocalConnector`],
/// created in [`LocalConnector::new`] and shared by every handle clone.
struct ConnectorControl {
    state: Mutex<ConnectorState>,
    /// Stop request; checked at every loop boundary and awaited during
    /// backoff so a sleeping retry wakes up immediately.
    stop_requested: AtomicBool,
    stop: Notify,
    /// Flipped once `connect_and_serve` has returned (for any reason).
    done_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
}

impl ConnectorControl {
    fn new() -> Self {
        let (done_tx, done_rx) = watch::channel(false);
        Self {
            state: Mutex::new(ConnectorState::default()),
            stop_requested: AtomicBool::new(false),
            stop: Notify::new(),
            done_tx,
            done_rx,
        }
    }

    fn state(&self) -> ConnectorState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Apply `update` under the state lock. Callers must not hold the guard
    /// across an await, so this is the only mutation entry point.
    fn update(&self, update: impl FnOnce(&mut ConnectorState)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        update(&mut state);
    }

    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }

    fn is_stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    async fn notified(&self) {
        self.stop.notified().await;
    }

    fn mark_done(&self) {
        let _ = self.done_tx.send(true);
    }
}

/// Cheap, cloneable control/observation handle of one [`LocalConnector`].
#[derive(Clone)]
pub struct ConnectorHandle {
    control: Arc<ConnectorControl>,
}

impl ConnectorHandle {
    /// Snapshot of the current phase, display, session and negotiated
    /// settings. Synchronous and lock-scoped (never held across an await).
    pub fn state(&self) -> ConnectorState {
        self.control.state()
    }

    /// Ask the connector to stop. Synchronous, non-blocking and idempotent.
    ///
    /// A backoff sleep wakes up immediately; a live session is torn down at
    /// its next loop boundary, so the phase becomes [`ConnectorPhase::Stopped`]
    /// shortly after — poll [`Self::state`] / [`Self::is_running`] (or await
    /// [`Self::wait_stopped`]) instead of assuming it already is.
    pub fn stop(&self) {
        self.control.request_stop();
    }

    pub fn is_running(&self) -> bool {
        self.control.state().is_running()
    }

    /// Resolve once `connect_and_serve` has returned (cleanly or not).
    pub async fn wait_stopped(&self) {
        let mut rx = self.control.done_rx.clone();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

pub struct LocalConnector {
    relay_addr: String,
    auth_token: String,
    local_x11_addr: String,
    display: Option<u16>,
    auto_display: bool,
    max_retries: u32,
    retry_base_delay: Duration,
    retry_max_delay: Duration,
    runtime: RuntimeConfig,
    /// One stats instance for the whole connector lifetime: counters
    /// accumulate across reconnects instead of resetting with every session.
    stats: Arc<ConnectionStats>,
    control: Arc<ConnectorControl>,
}

impl LocalConnector {
    pub fn new(
        relay_addr: String,
        auth_token: String,
        local_x11_addr: String,
        display: Option<u16>,
        auto_display: bool,
    ) -> Self {
        Self {
            relay_addr,
            auth_token,
            local_x11_addr,
            display,
            auto_display,
            max_retries: ClientDefaults::MAX_RETRIES,
            retry_base_delay: ClientDefaults::RETRY_BASE_DELAY,
            retry_max_delay: ClientDefaults::RETRY_MAX_DELAY,
            runtime: RuntimeConfig::default(),
            stats: Arc::new(ConnectionStats::new()),
            control: Arc::new(ConnectorControl::new()),
        }
    }

    /// Apply a runtime profile (timeouts, compression threshold, diffing).
    pub fn with_runtime(mut self, runtime: RuntimeConfig) -> Self {
        self.runtime = runtime;
        self
    }

    /// The connector's lifetime statistics, shared by every session it runs.
    pub fn stats(&self) -> Arc<ConnectionStats> {
        self.stats.clone()
    }

    /// Observation/control handle over this connector's state. Cheap to
    /// clone; safe to move to another thread.
    pub fn handle(&self) -> ConnectorHandle {
        ConnectorHandle {
            control: self.control.clone(),
        }
    }

    /// Start `connect_and_serve` on the current Tokio runtime and return its
    /// handle immediately.
    ///
    /// Requires a runtime context; without one the connector is left in
    /// [`ConnectorPhase::Failed`] with `last_error` set instead of panicking.
    pub fn spawn(self: Arc<Self>) -> ConnectorHandle {
        let handle = self.handle();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.control.update(|state| {
                state.phase = ConnectorPhase::Failed;
                state.last_error = Some("no Tokio runtime context for spawn()".to_string());
            });
            self.control.mark_done();
            return handle;
        };
        rt.spawn(async move {
            if let Err(e) = self.connect_and_serve().await {
                error!("Connector stopped with error: {}", e);
            }
        });
        handle
    }

    /// Run connect/serve cycles, retrying retriable failures while carrying
    /// the session id over for resume attempts.
    ///
    /// Signature and error semantics are unchanged; the only addition is that
    /// the final phase is published on the handle ([`ConnectorPhase::Stopped`]
    /// on success, [`ConnectorPhase::Failed`] on error).
    pub async fn connect_and_serve(&self) -> anyhow::Result<()> {
        let result = self.run_cycles().await;
        match &result {
            Ok(()) => self.control.update(|state| {
                state.phase = ConnectorPhase::Stopped;
            }),
            Err(e) => self.control.update(|state| {
                state.phase = ConnectorPhase::Failed;
                state.last_error = Some(e.to_string());
            }),
        }
        self.control.mark_done();
        result
    }

    async fn run_cycles(&self) -> anyhow::Result<()> {
        let mut attempt: u32 = 0;
        let mut last_session_id: Option<String> = None;
        loop {
            if self.control.is_stop_requested() {
                info!("Connector stop requested, exiting");
                return Ok(());
            }

            self.control.update(|state| {
                state.phase = ConnectorPhase::Connecting;
                state.reconnect_attempt = attempt;
            });

            // Clone, not take: a failed establish (transport never came up)
            // must not consume the id — every later attempt keeps trying to
            // resume the same session until one succeeds and overwrites it.
            let sid = last_session_id.clone();
            let result = self
                .connect_and_serve_once(sid.as_deref(), &mut last_session_id)
                .await;

            let Err(e) = result else {
                return Ok(());
            };

            if !is_retriable(&e) {
                self.control.update(|state| {
                    state.last_error = Some(e.to_string());
                });
                return Err(e);
            }

            attempt += 1;
            if attempt > self.max_retries {
                error!("Max reconnection attempts ({}) reached", self.max_retries);
                self.control.update(|state| {
                    state.last_error = Some(e.to_string());
                });
                return Err(e);
            }

            // Publish the failure before sleeping so a UI never shows
            // "Serving" while the connector is actually backing off.
            self.control.update(|state| {
                state.phase = ConnectorPhase::Reconnecting;
                state.reconnect_attempt = attempt;
                state.last_error = Some(e.to_string());
            });

            let delay = self.calculate_backoff(attempt);
            warn!(
                "Connection lost ({}), reconnecting in {:?} (attempt {}/{})",
                e, delay, attempt, self.max_retries
            );
            if self.sleep_or_stop(delay).await {
                info!("Connector stop requested during backoff, exiting");
                return Ok(());
            }
        }
    }

    /// Sleep for `delay`, returning early with `true` when a stop was
    /// requested. A pre-armed stop permit resolves immediately.
    async fn sleep_or_stop(&self, delay: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => self.control.is_stop_requested(),
            _ = self.stop_signal() => true,
        }
    }

    /// Resolve when a stop is requested, including the case where the request
    /// arrived before this future was created (the `Notify` permit is either
    /// still armed or the flag is already set).
    async fn stop_signal(&self) {
        if self.control.is_stop_requested() {
            return;
        }
        self.control.notified().await;
    }

    fn calculate_backoff(&self, attempt: u32) -> Duration {
        let base_ms = self.retry_base_delay.as_millis() as u64;
        let max_ms = self.retry_max_delay.as_millis() as u64;
        let backoff_ms = base_ms.saturating_mul(1u64 << attempt.min(MAX_BACKOFF_EXPONENT));
        let capped_ms = backoff_ms.min(max_ms);
        let jitter = rand::rng().random_range(0..=(capped_ms / 4));
        Duration::from_millis((capped_ms + jitter).min(max_ms))
    }

    /// Connect the TCP stream, perform the handshake and obtain a session
    /// (resuming `resume_session_id` when the server still has it).
    async fn establish(
        &self,
        resume_session_id: Option<&str>,
        saved_session_id: &mut Option<String>,
    ) -> anyhow::Result<(Rx11Transport, DisplayNumber, DataPlane)> {
        info!("Connecting to relay at {}", self.relay_addr);
        let stream = tokio::time::timeout(
            ClientDefaults::TCP_CONNECT_TIMEOUT,
            TcpStream::connect(&self.relay_addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("TCP connect to {} timed out", self.relay_addr))??;
        let mut transport = Rx11Transport::connect_tcp(stream, self.runtime.tcp_send_buffer_bytes)?;
        self.control.update(|state| {
            state.phase = ConnectorPhase::Handshaking;
        });

        let resume_sid = resume_session_id
            .map(|s| SessionId::new(s.to_string()))
            .transpose()?;

        let token = Token::new(self.auth_token.clone())?;
        let handshake = client_handshake(
            &mut transport,
            &token,
            resume_sid.as_ref(),
            ClientDefaults::TCP_CONNECT_TIMEOUT,
            &self.runtime,
        )
        .await?;
        let compression = handshake.compression;

        info!(
            "Connected to relay, transport_id={}, compression={}",
            handshake.session_id,
            compression.map(|a| a.as_str()).unwrap_or("disabled")
        );

        let display = if let Some(ref sid) = resume_sid {
            self.try_resume_session(&mut transport, sid, saved_session_id)
                .await?
        } else {
            let (disp, sid) = self.request_new_session(&mut transport).await?;
            *saved_session_id = Some(sid.into_inner());
            disp
        };

        let dataplane = DataPlane::new(
            DataPlaneSide::Client,
            handshake.features,
            compression,
            &self.runtime,
        );
        Ok((transport, display, dataplane))
    }

    async fn try_resume_session(
        &self,
        transport: &mut Rx11Transport,
        sid: &SessionId,
        saved_session_id: &mut Option<String>,
    ) -> anyhow::Result<DisplayNumber> {
        transport
            .send_frame(&Frame::SessionResume(SessionResumeMessage {
                session_id: sid.clone(),
            }))
            .await?;

        let ack = recv_session_ack(transport).await?;
        if !ack.success {
            warn!(
                "Session resume failed: {}, creating new session",
                ack.error_msg.as_deref().unwrap_or("unknown error")
            );
            let (disp, new_sid) = self.request_new_session(transport).await?;
            *saved_session_id = Some(new_sid.into_inner());
            return Ok(disp);
        }

        info!("Session resumed for display {}", ack.display);
        *saved_session_id = ack.session_id.clone().map(SessionId::into_inner);
        Ok(ack.display)
    }

    async fn request_new_session(
        &self,
        transport: &mut Rx11Transport,
    ) -> anyhow::Result<(DisplayNumber, SessionId)> {
        let cookie = generate_display_cookie();
        let frame = if self.auto_display {
            Frame::SessionAutoCreate(SessionAutoCreateMessage {
                auth_name: X11_AUTH_NAME.to_string(),
                auth_data: cookie,
            })
        } else {
            Frame::SessionCreate(SessionCreateMessage {
                display: DisplayNumber::new(self.display.unwrap_or(0))?,
                auth_name: X11_AUTH_NAME.to_string(),
                auth_data: cookie,
            })
        };
        transport.send_frame(&frame).await?;

        let ack = recv_session_ack(transport).await?;
        if !ack.success {
            return Err(anyhow::anyhow!(
                "Session create failed: {}",
                ack.error_msg.as_deref().unwrap_or("unknown error")
            ));
        }
        info!("Session created for display {}", ack.display);
        let sid = ack
            .session_id
            .ok_or_else(|| anyhow::anyhow!("Missing session_id"))?;
        Ok((ack.display, sid))
    }

    async fn connect_and_serve_once(
        &self,
        resume_session_id: Option<&str>,
        saved_session_id: &mut Option<String>,
    ) -> anyhow::Result<()> {
        let (transport, actual_display, dataplane) =
            self.establish(resume_session_id, saved_session_id).await?;
        let (mut read_half, write_half) = transport.split();

        // Publish the established session before the first byte moves: this
        // is what a dashboard shows as "serving :N".
        self.control.update(|state| {
            state.phase = ConnectorPhase::Serving;
            state.display = Some(actual_display.get());
            state.session_id = saved_session_id.clone();
            state.compression = dataplane.compression.map(|a| a.as_str().to_string());
            state.features = dataplane.features.to_list();
            state.last_error = None;
        });

        let x11_connections = new_shared_conns();
        // Lifetime stats: shared across reconnect cycles by design.
        let stats = self.stats.clone();
        let seq_counter = Arc::new(AtomicU32::new(1));
        let mut incremental_cache = ConnectionDataCache::from_runtime(&self.runtime);
        // The open audio channels: one per X11 connection of an
        // application, and the rule that decides which of them are heard.
        let mut audio_outlets = crate::audioout::OutletTable::new(
            dataplane
                .features
                .supports(rx11_core::protocol::Capability::AudioPerApp),
        );
        // The resource ranges of the application connections: the local X
        // server gives each connection a disjoint range of ids, and a
        // window id inside a range belongs to the connection that owns it.
        let ranges = Arc::new(tokio::sync::Mutex::new(
            crate::audioout::ResourceIndex::default(),
        ));
        // The authorization the local X server expects, learned from the
        // application's setup request.
        let authorization = Arc::new(tokio::sync::Mutex::new(
            crate::audioout::Authorization::default(),
        ));
        // The resident monitor connection: one connection to the local X
        // server, asked about the focus every tick and kept between the
        // questions. A connection per question would take a resource
        // range from the server every time.
        let mut monitor = crate::audioout::ResidentMonitor::new(
            &self.local_x11_addr,
            crate::audioout::Authorization::default(),
        );
        // Pump → serve resume-edge reports: small and rare (hysteresis
        // bounded); drops are reconciled from the meters by the tick.
        let (flow_edge_tx, flow_edge_rx) =
            tokio::sync::mpsc::channel::<rx11_core::flow::FlowEdge>(64);
        let mut flow = ServeFlow::new(&dataplane.features, flow_edge_rx);

        let (outbound_tx, outbound_rx) = rx11_core::queue::outbound_channel(
            BufferDefaults::OUTBOUND_CHANNEL,
            self.runtime.outbound_queue_bytes,
        );

        let stats_logger = {
            let stats = stats.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(ClientDefaults::STATUS_INTERVAL);
                interval.tick().await; // first tick completes immediately
                loop {
                    interval.tick().await;
                    info!("[rx11] Status: connected | {}", stats.summary());
                    stats.reset_period();
                }
            })
        };

        let sender_task = tokio::spawn(rx11_core::transport::drain_frames(outbound_rx, write_half));

        info!(
            "Proxying X11 data: relay <-> local X server at {}",
            self.local_x11_addr
        );

        let result = {
            let mut ctx = ServeCtx {
                read_half: &mut read_half,
                proxy: ProxyDeps {
                    outbound: &outbound_tx,
                    x11_connections: &x11_connections,
                    stats: &stats,
                    dataplane: &dataplane,
                    seq: &seq_counter,
                    local_x11_addr: &self.local_x11_addr,
                    runtime: &self.runtime,
                    flow_edges: &flow_edge_tx,
                    ranges: ranges.clone(),
                    authorization: authorization.clone(),
                },
                incremental_cache: &mut incremental_cache,
                display: actual_display,
                read_timeout: self.runtime.client_read_timeout,
                audio: &mut audio_outlets,
                ranges: &ranges,
                authorization: &authorization,
                monitor: &mut monitor,
                focus_interval: Duration::from_millis(500),
                flow: &mut flow,
            };
            tokio::select! {
                result = serve(&mut ctx) => result,
                // A stop request tears the live session down at this boundary;
                // the transport halves and pumps are dropped/aborted below.
                _ = self.stop_signal() => Ok(()),
            }
        };

        stats_logger.abort();
        cleanup_connections(&x11_connections, &stats).await;
        drop(outbound_tx);
        if let Err(e) = sender_task.await {
            if !e.is_cancelled() {
                error!("Sender task panicked: {}", e);
            }
        }

        if let Err(e) = &result {
            error!("Session ended with error: {}", e);
        } else {
            info!("Session ended gracefully");
        }

        result
    }
}

/// Receive the next frame and require it to be a SessionAck.
async fn recv_session_ack(transport: &mut Rx11Transport) -> anyhow::Result<SessionAckMessage> {
    match transport.recv_frame().await? {
        Frame::SessionAck(ack) => Ok(ack),
        _ => Err(anyhow::anyhow!("Expected SessionAck")),
    }
}

/// Classify an anyhow error as retriable, checking both our own error type
/// and raw IO errors.
fn is_retriable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Rx11Error>()
        .map(Rx11Error::is_retriable)
        .or_else(|| {
            err.downcast_ref::<std::io::Error>()
                .map(is_retriable_io_error)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn connector() -> LocalConnector {
        LocalConnector::new(
            "relay:7000".into(),
            "token".into(),
            "127.0.0.1:6000".into(),
            None,
            true,
        )
    }

    /// Connector aimed at a port that is bound then released, so connects
    /// are refused immediately (the `rx11-cli` util-test pattern) with fast
    /// backoff, keeping the retry loop observable without real network waits.
    async fn fast_retry_connector() -> LocalConnector {
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let mut c = LocalConnector::new(
            addr.to_string(),
            "token".into(),
            "127.0.0.1:6000".into(),
            None,
            true,
        );
        c.max_retries = 1000;
        c.retry_base_delay = Duration::from_millis(20);
        c.retry_max_delay = Duration::from_millis(40);
        c
    }

    /// Poll `pred` until it holds or `timeout` elapses.
    async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if pred() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn backoff_first_retry_is_base_doubled_plus_jitter() {
        let c = connector();
        let d = c.calculate_backoff(1);
        assert!(
            d >= Duration::from_millis(2000) && d <= Duration::from_millis(2500),
            "unexpected first delay {d:?}"
        );
    }

    #[test]
    fn backoff_saturates_at_max_delay() {
        let c = connector();
        for attempt in [10, 32, 1000] {
            assert_eq!(
                c.calculate_backoff(attempt),
                ClientDefaults::RETRY_MAX_DELAY,
                "attempt {attempt} should clamp to max delay"
            );
        }
    }

    #[test]
    fn retriable_classification_covers_all_error_sources() {
        assert!(is_retriable(&anyhow::Error::from(Rx11Error::Timeout)));
        assert!(is_retriable(&anyhow::Error::from(
            Rx11Error::ConnectionClosed
        )));
        assert!(is_retriable(&anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "no relay"
        ))));
        assert!(!is_retriable(&anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "firewall"
        ))));
        assert!(!is_retriable(&anyhow::Error::from(Rx11Error::Protocol(
            "bad frame".into()
        ))));
        assert!(!is_retriable(&anyhow::anyhow!("unrelated error")));
    }

    #[test]
    fn fresh_connector_reports_stopped_and_not_running() {
        let c = connector();
        let handle = c.handle();
        let state = handle.state();
        assert_eq!(state.phase, ConnectorPhase::Stopped);
        assert!(!handle.is_running());
        assert_eq!(state.reconnect_attempt, 0);
        assert!(state.display.is_none() && state.session_id.is_none());
        assert!(state.last_error.is_none());
    }

    #[test]
    fn handle_shares_state_with_the_connector() {
        let c = connector();
        let a = c.handle();
        let b = c.handle();
        c.control.update(|state| {
            state.phase = ConnectorPhase::Serving;
            state.display = Some(7);
        });
        assert_eq!(a.state().phase, ConnectorPhase::Serving);
        assert_eq!(b.state().display, Some(7));
        assert!(a.is_running() && b.is_running());
    }

    #[test]
    fn stats_arc_is_created_once_per_connector() {
        let c = connector();
        let first = c.stats();
        let second = c.stats();
        assert!(
            Arc::ptr_eq(&first, &second),
            "stats must survive reconnects as one instance"
        );
        first.add_bytes_sent(1234);
        assert_eq!(second.totals().bytes_sent, 1234);
    }

    #[tokio::test]
    async fn stop_makes_a_retrying_connector_exit() {
        let connector = Arc::new(fast_retry_connector().await);
        let handle = connector.clone().spawn();

        // Reach the backoff phase: connects are refused, retries begin.
        assert!(
            wait_until(Duration::from_secs(5), || {
                handle.state().phase == ConnectorPhase::Reconnecting
            })
            .await,
            "connector should be retrying, got {:?}",
            handle.state()
        );
        let retrying = handle.state();
        assert!(retrying.reconnect_attempt >= 1);
        assert!(retrying.last_error.is_some(), "failure must be published");

        handle.stop();
        tokio::time::timeout(Duration::from_secs(5), handle.wait_stopped())
            .await
            .expect("stop must interrupt the backoff sleep");
        assert_eq!(handle.state().phase, ConnectorPhase::Stopped);
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn stop_before_start_is_honored() {
        let connector = Arc::new(fast_retry_connector().await);
        let handle = connector.handle();
        handle.stop();
        let result = connector.connect_and_serve().await;
        assert!(result.is_ok(), "a pre-stopped connector exits cleanly");
        assert_eq!(handle.state().phase, ConnectorPhase::Stopped);
    }

    #[tokio::test]
    async fn non_retriable_failure_lands_in_failed_phase() {
        // An invalid token fails validation before any network I/O, which is
        // a non-retriable protocol error.
        let mut c = fast_retry_connector().await;
        c.auth_token = String::new();
        let result = c.connect_and_serve().await;
        assert!(result.is_err(), "empty token must fail");
        let state = c.handle().state();
        assert_eq!(state.phase, ConnectorPhase::Failed);
        assert!(state.last_error.is_some());
        assert!(!c.handle().is_running());
    }

    #[test]
    fn spawn_without_runtime_fails_instead_of_panicking() {
        let c = Arc::new(connector());
        let handle = c.clone().spawn();
        let state = handle.state();
        assert_eq!(state.phase, ConnectorPhase::Failed);
        assert!(state
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("runtime"));
    }
}
