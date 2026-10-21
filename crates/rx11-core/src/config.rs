//! Centralized default configuration values: timeouts, retry policies,
//! ports and buffer sizes shared across server, client and CLI.

use std::time::Duration;

pub struct ServerDefaults;
pub struct ClientDefaults;
pub struct SshDefaults;
pub struct BufferDefaults;
pub struct IncrementalDefaults;

impl ServerDefaults {
    /// Default relay listen address (`IP:PORT`).
    pub const LISTEN_ADDR: &'static str = "0.0.0.0:7000";
    pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
    pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);
    pub const MAX_CONNECTIONS: usize = 256;
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
    pub const SESSION_GRACE_PERIOD: Duration = Duration::from_secs(60);
    pub const MAX_X11_CONNECTIONS_PER_DISPLAY: usize = 64;
    /// Interval between periodic relay status summary log lines.
    pub const STATUS_INTERVAL: Duration = Duration::from_secs(30);
}

impl ClientDefaults {
    /// Default local X Server address (Display :0).
    pub const X11_ADDR: &'static str = "127.0.0.1:6000";
    /// Interval between periodic status log lines.
    pub const STATUS_INTERVAL: Duration = Duration::from_secs(30);
    /// Timeout for probing whether a local X Server accepts connections.
    pub const X11_DETECT_TIMEOUT: Duration = Duration::from_secs(2);
    pub const READ_TIMEOUT: Duration = Duration::from_secs(100);
    pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    pub const MAX_RETRIES: u32 = 10;
    pub const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
    pub const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
}

impl SshDefaults {
    pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
    pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
    pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;
    pub const DEFAULT_PORT: u16 = 22;
    /// Base for local bind ports in fixed-display mode: `17000 + display`.
    pub const LOCAL_BIND_BASE: u16 = 17000;
    /// How long to wait for the forwarded local port to accept connections.
    pub const READY_TIMEOUT: Duration = Duration::from_secs(10);
}

impl BufferDefaults {
    /// Channel capacity between the X11 socket pump and the relay task.
    pub const CHANNEL_BUFFER: usize = 2048;
    /// Channel capacity of the outbound frame queue (frame COUNT fairness
    /// bound; the latency bound is [`BufferDefaults::OUTBOUND_QUEUE_BYTES`]).
    pub const OUTBOUND_CHANNEL: usize = 2048;
    /// In-flight BYTES budget for the WAN-bound outbound queue. Sized for
    /// interactivity on slow links: at 3 Mbit/s (≈375 KB/s) this caps the
    /// queueing delay contributed by our own buffer at ~2.8 s even while a
    /// huge redraw burst saturates the link; on localhost the queue drains
    /// in microseconds, so throughput is unaffected.
    pub const OUTBOUND_QUEUE_BYTES: usize = 1024 * 1024;
    /// Initial size of raw TCP read buffers (transport and X11 proxy loops).
    pub const INITIAL_READ_BUF: usize = 64 * 1024;
    /// Growth ceiling for raw TCP read buffers in the X11 proxy loops.
    pub const PROXY_READ_BUF_MAX: usize = 256 * 1024;
    /// Max frames one coalesced outbound write may carry.
    pub const OUTBOUND_BATCH_FRAMES: usize = 64;
    /// Max bytes one coalesced outbound write may carry.
    pub const OUTBOUND_BATCH_BYTES: usize = 256 * 1024;
}

impl IncrementalDefaults {
    pub const DEFAULT_CHUNK_SIZE: usize = 4096;
    pub const MIN_BLOCK_SIZE_FOR_DIFF: usize = 1024;
    /// Minimum fraction of the base frame that must be saved for a diff
    /// frame to be worth sending.
    pub const MIN_SAVINGS_RATIO: f64 = 0.1;
}

/// Runtime-tunable knobs, threaded from the CLI config file (`[runtime]`
/// section) into server and client. Defaults match the compile-time
/// constants above; new tunables are added here rather than at call sites.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub client_read_timeout: Duration,
    pub compression_threshold: usize,
    pub diff_chunk_size: usize,
    pub diff_min_block_size: usize,
    /// Minimum fraction of the base frame a diff must save to be sent.
    pub diff_min_savings_ratio: f64,
    /// In-flight byte budget of the WAN-bound outbound queue (see
    /// [`BufferDefaults::OUTBOUND_QUEUE_BYTES`]).
    pub outbound_queue_bytes: usize,
    /// Optional cap for the TCP `SO_SNDBUF` on the relay socket. Slow-WAN
    /// tip: pair a small queue with a small kernel buffer so the total
    /// in-flight bytes (queue + socket) stay near the link's patience
    /// budget. `None` leaves the OS autotuning in charge.
    pub tcp_send_buffer_bytes: Option<usize>,
    /// Opt in to the `x11_recode` capability (re-encode large PutImage
    /// payloads as JPEG / lossless PNG in transit). Off by default:
    /// JPEG mode is lossy in pixels, so it must be a deliberate choice.
    pub image_recode: bool,
    pub image_recode_codec: crate::x11recode::RecodeCodec,
    pub image_recode_quality: u8,
    pub image_recode_min_bytes: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: ServerDefaults::HEARTBEAT_INTERVAL,
            heartbeat_timeout: ServerDefaults::HEARTBEAT_TIMEOUT,
            client_read_timeout: ClientDefaults::READ_TIMEOUT,
            compression_threshold: crate::compress::COMPRESSION_THRESHOLD,
            diff_chunk_size: IncrementalDefaults::DEFAULT_CHUNK_SIZE,
            diff_min_block_size: IncrementalDefaults::MIN_BLOCK_SIZE_FOR_DIFF,
            diff_min_savings_ratio: IncrementalDefaults::MIN_SAVINGS_RATIO,
            outbound_queue_bytes: BufferDefaults::OUTBOUND_QUEUE_BYTES,
            tcp_send_buffer_bytes: None,
            image_recode: false,
            image_recode_codec: crate::x11recode::RecodeCodec::Jpeg,
            image_recode_quality: 80,
            image_recode_min_bytes: 16 * 1024,
        }
    }
}
