//! Centralized default configuration values: timeouts, retry policies,
//! ports and buffer sizes shared across server, client and CLI.

use std::time::Duration;

pub struct ServerDefaults;
pub struct ClientDefaults;
pub struct SshDefaults;
pub struct BufferDefaults;
pub struct IncrementalDefaults;
pub struct FlowDefaults;

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
    /// First wait between tunnel rebuild attempts (doubling up to the cap).
    /// Sized so a rebuild can still succeed inside the server's session-grace
    /// window while the connector's own retry loop is running.
    pub const REBUILD_BASE_DELAY: Duration = Duration::from_secs(2);
    /// Ceiling of the rebuild backoff doubling.
    pub const REBUILD_MAX_DELAY: Duration = Duration::from_secs(30);
    /// Consecutive rebuild attempts (per tunnel death) before giving up;
    /// a successful rebuild resets the budget for the next death.
    pub const REBUILD_MAX_ATTEMPTS: u32 = 8;
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
    /// Capacity of the real-time outbound lane (audio packets at 50/s ≈
    /// 10 s of buffering). The lane is a DROP-OLDEST-VALUE surface: a full
    /// lane sheds NEW packets, because stale real-time audio is worthless
    /// — unlike the X11 byte stream, losing a late audio frame is a click,
    /// not corruption.
    pub const PRIORITY_LANE_CAPACITY: usize = 512;
    /// Max bytes one blocking socket commit may carry while the priority
    /// lane is live. Real-time latency is bounded by this chunk (64 KiB ≈
    /// 0.17 s on a 3 Mbit/s link), never by a whole coalesced batch.
    pub const OUTBOUND_WRITE_CHUNK: usize = 64 * 1024;
}

impl IncrementalDefaults {
    pub const DEFAULT_CHUNK_SIZE: usize = 4096;
    pub const MIN_BLOCK_SIZE_FOR_DIFF: usize = 1024;
    /// Minimum fraction of the base frame that must be saved for a diff
    /// frame to be worth sending.
    pub const MIN_SAVINGS_RATIO: f64 = 0.1;
    /// Fixed payload of a reference frame — the chunk-diff header with zero
    /// chunks (`conn + seq + base_seq + total_len + chunk_count` =
    /// 4+4+4+4+2 bytes). A repeat of a payload longer than this ships as a
    /// reference instead of whole even below the diff floor: the frame is
    /// mandatory on a positional stream, but its CONTENT is free.
    pub const REFERENCE_OVERHEAD: usize = 18;
}

impl FlowDefaults {
    /// Pause the bulk pump once this many bytes sit accepted-but-not-consumed
    /// on one channel. The byte-budgeted outbound queue already holds
    /// `outbound_queue_bytes` ahead of this gauge, so the total display lag
    /// stays bounded by (queue + this) / link rate — 2 MiB on top of 1 MiB
    /// is ~8 s at 3 Mbit/s, past which waiting longer only replays stale
    /// pixels while the producing app freezes anyway.
    pub const PAUSE_BYTES: usize = 2 * 1024 * 1024;
    /// Resume once the backlog drains below this. The band between the two
    /// watermarks is the hysteresis that keeps a redraw burst from
    /// oscillating the wire state.
    pub const RESUME_BYTES: usize = 256 * 1024;
    /// How often held flow state is re-advertised on the priority lane (see
    /// [`crate::flow::Advertiser`]): the lane drops when full, and a lost
    /// `Resume` must not strand a paused channel.
    pub const ADVERTISE_TICK: Duration = Duration::from_secs(2);
    /// How many ticks a `Resume` is repeated after its edge, then silence.
    pub const RESUME_RETRANSMITS: u8 = 8;
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
    /// Diff the client uplink (application requests → relay) too, not only
    /// the picture on the downlink. ON by default: the same chunk-diff +
    /// compression chain keyed per channel, self-healing through
    /// `StreamResyncRequest`. Set `false` on either end to keep the
    /// downlink-only policy and ship uplink payloads whole.
    pub incremental_uplink: bool,
    /// In-flight byte budget of the WAN-bound outbound queue (see
    /// [`BufferDefaults::OUTBOUND_QUEUE_BYTES`]).
    pub outbound_queue_bytes: usize,
    /// Optional cap for the TCP `SO_SNDBUF` on the relay socket. Slow-WAN
    /// tip: pair a small queue with a small kernel buffer so the total
    /// in-flight bytes (queue + socket) stay near the link's patience
    /// budget. `None` leaves the OS autotuning in charge.
    pub tcp_send_buffer_bytes: Option<usize>,
    /// Re-encode large PutImage payloads as PNG/JPEG in transit. ON by
    /// default: the default tier (PNG) is lossless, so image traffic
    /// shrinks while staying pixel exact. Set `false` on either end to
    /// force byte-for-byte passthrough; the lossy JPEG tier is only
    /// reached through an explicit `image_recode_codec = "jpeg"` choice.
    pub image_recode: bool,
    pub image_recode_codec: crate::x11recode::RecodeCodec,
    pub image_recode_quality: u8,
    pub image_recode_min_bytes: usize,
    /// ON by default: the server records its default output sink (monitor),
    /// ships Opus packets down a real-time lane, and the client plays them
    /// locally. Set `false` on either end to disable audio entirely. Where
    /// no Pulse/PipeWire capture exists the probe warns and audio stays
    /// inactive — it never fails the session.
    pub audio_forward: bool,
    /// ON by default: the server attributes each captured sound stream to
    /// the application producing it (by process id) and ships it on its own
    /// `audio` channel, tagged with that application's X11 connection. The
    /// client then plays the sound of the applications it renders instead
    /// of the server's entire desktop mix.
    ///
    /// Set `false` on either end to fall back to the single whole-desktop
    /// mix channel. The server also falls back on its own when the host
    /// cannot attribute sound to applications (no per-stream capture
    /// support), so a mixed or unsupported host still gets audio.
    pub audio_per_app: bool,
    /// Opus target bitrate in kbit/s (VBR). 64 ≈ transparent stereo music;
    /// 32 is fine for speech-heavy desktop sound.
    pub audio_bitrate_kbps: u32,
    /// Override the recorder argv (e.g. `["parec", "--raw", ...]`). `None`
    /// auto-detects `parec` (PulseAudio and PipeWire both ship it).
    pub audio_record_command: Option<Vec<String>>,
    /// Override the player argv (e.g. `["paplay", "--raw", ...]`). `None`
    /// auto-detects `paplay` (PulseAudio and PipeWire both ship it).
    pub audio_play_command: Option<Vec<String>>,
    /// Per-channel bulk flow control (`FlowControl` capability). ON by
    /// default: a stalled consumer advertises `Pause` and the producing
    /// pump stops reading, freezing the application in `write()` instead of
    /// parking stale pixels down the pipe. Control frames and the audio
    /// real-time lane are never gated. Set `false` on either end to fall
    /// back to pure transport-level backpressure (byte-budgeted queue).
    pub flow_control: bool,
    /// Pause watermark: accepted-but-not-consumed bytes on one channel at
    /// which a `Pause` is advertised (see [`FlowDefaults::PAUSE_BYTES`]).
    pub flow_pause_bytes: usize,
    /// Resume watermark, the hysteresis floor below
    /// [`flow_pause_bytes`][Self::flow_pause_bytes].
    pub flow_resume_bytes: usize,
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
            incremental_uplink: true,
            outbound_queue_bytes: BufferDefaults::OUTBOUND_QUEUE_BYTES,
            tcp_send_buffer_bytes: None,
            image_recode: true,
            image_recode_codec: crate::x11recode::RecodeCodec::Png,
            image_recode_quality: 80,
            image_recode_min_bytes: 16 * 1024,
            audio_forward: true,
            audio_per_app: true,
            audio_bitrate_kbps: crate::audio::DEFAULT_BITRATE_KBPS,
            audio_record_command: None,
            audio_play_command: None,
            flow_control: true,
            flow_pause_bytes: FlowDefaults::PAUSE_BYTES,
            flow_resume_bytes: FlowDefaults::RESUME_BYTES,
        }
    }
}
