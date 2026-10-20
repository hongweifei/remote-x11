//! Client connector: establishes the relay connection (handshake, session
//! creation or resume), runs the proxy receive loop and reconnects with
//! exponential backoff on transient failures.

use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use rand::RngExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use rx11_core::auth::generate_display_cookie;
use rx11_core::compress::CompressionAlgo;
use rx11_core::config::{BufferDefaults, ClientDefaults};
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
use crate::serve::{serve, ServeCtx};

/// X11 authorization protocol the client always requests.
const X11_AUTH_NAME: &str = "MIT-MAGIC-COOKIE-1";
/// Ceiling of the exponential backoff doubling (`base * 2^cap`).
const MAX_BACKOFF_EXPONENT: u32 = 10;

pub struct LocalConnector {
    relay_addr: String,
    auth_token: String,
    local_x11_addr: String,
    display: Option<u16>,
    auto_display: bool,
    max_retries: u32,
    retry_base_delay: Duration,
    retry_max_delay: Duration,
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
        }
    }

    /// Run connect/serve cycles, retrying retriable failures while carrying
    /// the session id over for resume attempts.
    pub async fn connect_and_serve(&self) -> anyhow::Result<()> {
        let mut attempt: u32 = 0;
        let mut last_session_id: Option<String> = None;
        loop {
            let sid = last_session_id.take();
            let result = self
                .connect_and_serve_once(sid.as_deref(), &mut last_session_id)
                .await;

            let Err(e) = result else {
                return Ok(());
            };

            if !is_retriable(&e) {
                return Err(e);
            }

            attempt += 1;
            if attempt > self.max_retries {
                error!("Max reconnection attempts ({}) reached", self.max_retries);
                return Err(e);
            }

            let delay = self.calculate_backoff(attempt);
            warn!(
                "Connection lost ({}), reconnecting in {:?} (attempt {}/{})",
                e, delay, attempt, self.max_retries
            );
            tokio::time::sleep(delay).await;
        }
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
    ) -> anyhow::Result<(Rx11Transport, DisplayNumber, Option<CompressionAlgo>)> {
        info!("Connecting to relay at {}", self.relay_addr);
        let stream = tokio::time::timeout(
            ClientDefaults::TCP_CONNECT_TIMEOUT,
            TcpStream::connect(&self.relay_addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("TCP connect to {} timed out", self.relay_addr))??;
        let mut transport = Rx11Transport::new(stream)?;

        let resume_sid = resume_session_id
            .map(|s| SessionId::new(s.to_string()))
            .transpose()?;

        let token = Token::new(self.auth_token.clone())?;
        let handshake = client_handshake(
            &mut transport,
            &token,
            resume_sid.as_ref(),
            ClientDefaults::TCP_CONNECT_TIMEOUT,
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

        Ok((transport, display, compression))
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
        let (transport, actual_display, compression) =
            self.establish(resume_session_id, saved_session_id).await?;
        let (mut read_half, write_half) = transport.split();

        let x11_connections = new_shared_conns();
        let stats = Arc::new(ConnectionStats::new());
        let seq_counter = Arc::new(AtomicU32::new(1));
        let mut incremental_cache = ConnectionDataCache::new();

        let (outbound_tx, mut outbound_rx) =
            mpsc::channel::<Frame>(BufferDefaults::OUTBOUND_CHANNEL);

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

        let sender_task = tokio::spawn(async move {
            let mut write_half = write_half;
            while let Some(frame) = outbound_rx.recv().await {
                if write_half.send_frame(&frame).await.is_err() {
                    break;
                }
            }
            let _ = write_half.flush().await;
        });

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
                    compression,
                    seq: &seq_counter,
                    local_x11_addr: &self.local_x11_addr,
                },
                incremental_cache: &mut incremental_cache,
                display: actual_display,
            };
            serve(&mut ctx).await
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
