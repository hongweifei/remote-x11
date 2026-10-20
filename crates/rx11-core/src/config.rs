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
    /// Channel capacity of the outbound frame queue.
    pub const OUTBOUND_CHANNEL: usize = 2048;
    /// Initial size of raw TCP read buffers (transport and X11 proxy loops).
    pub const INITIAL_READ_BUF: usize = 64 * 1024;
    /// Growth ceiling for raw TCP read buffers in the X11 proxy loops.
    pub const PROXY_READ_BUF_MAX: usize = 256 * 1024;
}

impl IncrementalDefaults {
    pub const DEFAULT_CHUNK_SIZE: usize = 4096;
    pub const MIN_BLOCK_SIZE_FOR_DIFF: usize = 1024;
    /// Minimum fraction of the base frame that must be saved for a diff
    /// frame to be worth sending.
    pub const MIN_SAVINGS_RATIO: f64 = 0.1;
}
