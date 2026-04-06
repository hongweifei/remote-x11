use std::time::Duration;

pub struct ServerDefaults;
pub struct ClientDefaults;
pub struct SshDefaults;
pub struct BufferDefaults;

impl ServerDefaults {
    pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
    pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);
    pub const MAX_CONNECTIONS: usize = 256;
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
    pub const SESSION_GRACE_PERIOD: Duration = Duration::from_secs(60);
    pub const MAX_X11_CONNECTIONS_PER_DISPLAY: usize = 64;
}

impl ClientDefaults {
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
}

impl BufferDefaults {
    pub const CHANNEL_BUFFER: usize = 2048;
    pub const OUTBOUND_CHANNEL: usize = 2048;
    pub const INITIAL_READ_BUF: usize = 64 * 1024;
    pub const MAX_READ_BUF: usize = 256 * 1024;
}
