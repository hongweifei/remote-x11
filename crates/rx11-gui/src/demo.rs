//! Fixture backend: lets the console be opened, inspected and screenshotted
//! without a relay, a config file or a network — enabled by `RX11_GUI_DEMO=1`.
//!
//! It exists for two honest reasons: the UI needs to be verified in every
//! state (running, reconnecting, failed, empty) without waiting for real
//! failures to happen, and anyone reviewing the console before configuring it
//! should be able to see what it looks like populated. The numbers below are
//! a coherent fictional session, not claims about the product — the real
//! backend fills the same fields from live counters.
//!
//! Compiled only with the `demo` feature (crate default). It is a
//! [`Backend`] implementation like any other: no page, no widget and no
//! launcher appears here, which is why the whole console can be exercised
//! without touching the network.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::launcher::GuiDefaults;
use crate::model::{
    Backend, Health, LogLevel, LogRecord, Role, SessionRowData, StartRequest, TrafficTotals,
};

struct FakeSession {
    id: u64,
    role: Role,
    started: Instant,
    values: Vec<(String, String)>,
}

pub struct DemoBackend {
    sessions: Mutex<Vec<FakeSession>>,
    next_id: AtomicU64,
    started: Instant,
    log_seq: AtomicU64,
    cleared_at: AtomicU64,
    /// Growing byte counters, so the scope has something to draw.
    bytes: Mutex<(u64, u64)>,
}

impl Default for DemoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoBackend {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            started: Instant::now(),
            log_seq: AtomicU64::new(0),
            cleared_at: AtomicU64::new(0),
            bytes: Mutex::new((0, 0)),
        }
    }

    /// Default values a fresh config file would carry.
    fn defaults() -> Vec<(String, String)> {
        [
            ("server.listen", "0.0.0.0:7000"),
            ("server.x11_port", "6000"),
            ("server.token", ""),
            ("client.relay", "127.0.0.1:7000"),
            ("client.token", ""),
            ("client.x11", "127.0.0.1:6000"),
            ("client.display", ""),
            ("ssh.host", ""),
            ("ssh.port", "22"),
            ("ssh.user", ""),
            ("ssh.identity", "~/.ssh/id_ed25519"),
            ("ssh.relay_port", "7000"),
            ("ssh.token", ""),
            ("ssh.reconnect", "true"),
            ("ssh.display", ""),
            ("runtime.heartbeat_interval_secs", "30"),
            ("runtime.heartbeat_timeout_secs", "90"),
            ("runtime.client_read_timeout_secs", "100"),
            ("runtime.compression_threshold", "256"),
            ("runtime.diff_chunk_size", "4096"),
            ("runtime.diff_min_block_size", "1024"),
            ("runtime.diff_min_savings_ratio", "0.1"),
            ("runtime.outbound_queue_bytes", "1048576"),
            ("runtime.tcp_send_buffer_bytes", ""),
            ("runtime.image_recode", "true"),
            ("runtime.image_recode_codec", "png"),
            ("runtime.image_recode_quality", "80"),
            ("runtime.image_recode_min_bytes", "16384"),
            ("runtime.audio_forward", "true"),
            ("runtime.audio_bitrate_kbps", "64"),
            ("runtime.audio_record_command", ""),
            ("runtime.audio_play_command", ""),
            ("runtime.flow_control", "true"),
            ("runtime.flow_pause_bytes", "2097152"),
            ("runtime.flow_resume_bytes", "262144"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
    }
}

impl Backend for DemoBackend {
    fn config_path(&self) -> String {
        r"~/.config/rx11/config.toml（演示模式：不会写入）".to_string()
    }

    fn load_values(&self) -> Result<Vec<(String, String)>, String> {
        Ok(Self::defaults())
    }

    fn save_values(&self, _values: &[(String, String)]) -> Result<(), String> {
        // Demo mode never touches the disk; the UI's saved/dirty state is
        // exercised, the file is not.
        Ok(())
    }

    fn validate(&self, values: &[(String, String)]) -> Vec<(String, String)> {
        let get = |k: &str| {
            values
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        let mut issues = Vec::new();
        let pause = get("runtime.flow_pause_bytes").parse::<u64>().ok();
        let resume = get("runtime.flow_resume_bytes").parse::<u64>().ok();
        if let (Some(p), Some(r)) = (pause, resume) {
            if p <= r {
                issues.push((
                    "runtime.flow_pause_bytes".to_string(),
                    "暂停水位必须大于恢复水位，否则通道会在两个阈值之间抖动".to_string(),
                ));
            }
        }
        issues
    }

    fn start(&self, req: StartRequest) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut s = self.sessions.lock().unwrap();
        s.push(FakeSession {
            id,
            role: req.role,
            started: Instant::now(),
            values: req.values,
        });
        Ok(id)
    }

    fn stop(&self, id: u64) {
        let mut s = self.sessions.lock().unwrap();
        s.retain(|f| f.id != id);
    }

    fn sessions(&self) -> Vec<SessionRowData> {
        let mut out = Vec::new();
        for f in self.sessions.lock().unwrap().iter() {
            let age = f.started.elapsed();
            // The first seconds read as "starting", so the transitional state
            // is visible in the UI rather than only in a race.
            let (health, headline, detail) = if age < Duration::from_millis(900) {
                (Health::Starting, "正在建立连接…".to_string(), String::new())
            } else {
                match f.role {
                    Role::Server => {
                        let listen = f
                            .values
                            .iter()
                            .find(|(k, _)| k == "server.listen")
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| "0.0.0.0:7000".into());
                        (
                            Health::Running,
                            format!("监听 {listen}"),
                            "x11 代理已就绪 · 共享音频采集已启动".to_string(),
                        )
                    }
                    Role::Client => {
                        let relay = f
                            .values
                            .iter()
                            .find(|(k, _)| k == "client.relay")
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| "127.0.0.1:7000".into());
                        (
                            Health::Running,
                            "display :0 · 已协商 6 项能力".to_string(),
                            format!("relay {relay} · zstd_stream"),
                        )
                    }
                    Role::Ssh => {
                        let host = f
                            .values
                            .iter()
                            .find(|(k, _)| k == "ssh.host")
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| "example-host".into());
                        (
                            Health::Running,
                            "隧道已建立，会话运行中".to_string(),
                            format!("{host}:22 → 127.0.0.1:17000"),
                        )
                    }
                }
            };
            // Direction matters to the operator: this product streams the
            // remote screen *down* to the local client, so the bulk is the
            // client's received bytes and the relay's sent bytes. The fixture
            // used to have it backwards (client "uploading" 9 MB), which also
            // contradicted the summary strip — `totals()` already advances
            // `received` fast and `sent` slowly.
            let (sent, recv) = match f.role {
                // Mirrored exactly: what the relay sends is what the client
                // receives, so the two rows can be read against each other.
                Role::Server => (9_120_553, 1_284_913),
                Role::Client => (1_284_913, 9_120_553),
                Role::Ssh => (0, 0),
            };
            out.push(SessionRowData {
                id: f.id,
                role: f.role,
                health,
                headline,
                detail,
                sent: Some(sent),
                received: Some(recv),
                uptime: Some(age.as_secs()),
            });
        }
        out
    }

    fn totals(&self) -> TrafficTotals {
        // Advance the counters at a believable rate so the scope has a shape.
        let mut b = self.bytes.lock().unwrap();
        let up = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f.role, Role::Client));
        let down = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f.role, Role::Server));
        if down {
            b.1 += 9_000 + (b.1 % 7000);
        }
        if up {
            b.0 += 700 + (b.0 % 900);
        }
        let channels = self.sessions.lock().unwrap().len() as u32;
        TrafficTotals {
            sent: b.0,
            received: b.1,
            saved: (b.0 + b.1) * 6 / 10,
            raw: (b.0 + b.1) * 4 / 10,
            channels,
            uptime: self.started.elapsed().as_secs(),
        }
    }

    fn logs_since(&self, seq: u64) -> Vec<LogRecord> {
        if seq <= self.cleared_at.load(Ordering::Relaxed) {
            // First call (or after a clear): emit the opening lines.
            self.cleared_at.store(0, Ordering::Relaxed);
        }
        let next = self.log_seq.load(Ordering::Relaxed);
        if seq >= next && seq != 0 {
            return Vec::new();
        }
        let start = if seq == 0 { 1 } else { seq + 1 };
        let mut out = Vec::new();
        let script: [(LogLevel, &str, &str); 8] = [
            (
                LogLevel::Info,
                "rx11_core::config",
                "Loaded config from ~/.config/rx11/config.toml",
            ),
            (
                LogLevel::Info,
                "rx11_server::relay",
                "Relay server listening on 0.0.0.0:7000",
            ),
            (
                LogLevel::Info,
                "rx11_server::audio",
                "audio: capturing output sink via parec",
            ),
            (
                LogLevel::Info,
                "rx11_client::connector",
                "Connected to relay, compression=zstd_stream",
            ),
            (
                LogLevel::Info,
                "rx11_client::serve",
                "X11 client connected (1) display=Some(0)",
            ),
            (
                LogLevel::Debug,
                "rx11_core::dataplane",
                "incremental diff accepted: 12 frames, saved 184.2 KB",
            ),
            (
                LogLevel::Warn,
                "rx11_client::proxy",
                "No X Server detected at 127.0.0.1:6000",
            ),
            (
                LogLevel::Info,
                "rx11_server::session",
                "Session resumed for display :0",
            ),
        ];
        for n in start..=next {
            let (level, target, message) = script[((n - 1) as usize) % script.len()];
            out.push(LogRecord {
                seq: n,
                time: stamp(n),
                level,
                target: target.to_string(),
                message: message.to_string(),
            });
        }
        out
    }

    fn clear_logs(&self) {
        let next = self.log_seq.load(Ordering::Relaxed);
        self.cleared_at.store(next, Ordering::Relaxed);
    }

    fn token_hint(&self, values: &[(String, String)]) -> String {
        values
            .iter()
            .find(|(k, _)| k == "server.token" || k == "client.token")
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "7f3c9a1e5b0d4c82a6f0".to_string())
    }

    fn shutdown(&self) {}
}

/// Fake log timestamps that advance a second per line, so the log page shows
/// a plausible sequence instead of a wall of identical times.
fn stamp(n: u64) -> String {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = base.saturating_sub(8) + n;
    let (h, m, s) = ((t / 3600) % 24, (t / 60) % 60, t % 60);
    format!("{h:02}:{m:02}:{s:02}.{:03}", (n * 137) % 1000)
}

/// How many sessions / log lines the demo should show.
///
/// The default fixtures are deliberately small — a screenshot of a two-session
/// console is honest about what the demo is. But the scrollbars only appear once
/// the lists overflow (the session table fits ~7 rows, the log well ~25), so
/// exercising them by hand needs a bigger fixture:
///
/// ```text
/// RX11_GUI_DEMO=1 RX11_GUI_DEMO_SESSIONS=12 RX11_GUI_DEMO_LOGS=200 rx11-gui
/// ```
///
/// Values are clamped so a typo cannot spin the fixture generator.
fn demo_count(key: &str, fallback: u64, max: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|n| n.clamp(1, max))
        .unwrap_or(fallback)
}

/// Seed the demo with a running server + client, so the console opens on a
/// populated state rather than an empty one.
///
/// `RX11_GUI_DEMO_SESSIONS` / `RX11_GUI_DEMO_LOGS` enlarge the fixture for
/// testing the scrolling lists; the defaults are the delivery screenshots.
pub fn seeded() -> DemoBackend {
    let b = DemoBackend::new();
    let defaults = DemoBackend::defaults();
    let sessions = demo_count("RX11_GUI_DEMO_SESSIONS", 2, 200);
    let logs = demo_count(
        "RX11_GUI_DEMO_LOGS",
        6,
        GuiDefaults::LOG_RING_CAPACITY as u64,
    );
    b.log_seq.store(logs, Ordering::Relaxed);

    // Roles cycle so extra rows are visually distinguishable — otherwise a
    // scrolled session table looks identical to an unscrolled one.
    for i in 0..sessions {
        let role = match i % 3 {
            0 => Role::Server,
            1 => Role::Client,
            _ => Role::Ssh,
        };
        if let Ok(id) = b.start(StartRequest {
            role,
            values: defaults.clone(),
        }) {
            // Stagger the uptimes so the table does not read as one moment.
            if let Ok(mut s) = b.sessions.lock() {
                if let Some(f) = s.iter_mut().find(|f| f.id == id) {
                    let age = 192u64.saturating_sub(i * 4);
                    f.started = Instant::now() - Duration::from_secs(age);
                }
            }
        }
    }
    b
}
