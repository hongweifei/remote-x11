//! Shared UI state: what page is showing, what the operator has typed, and
//! the rolling sample buffers the scope draws from.
//!
//! The state is owned by the UI thread only. Worker threads (the tokio
//! runtime inside [`crate::launcher::Launcher`]) communicate exclusively
//! through the launcher's snapshot API and the [`crate::launcher::LogRing`],
//! both of which are lock-protected and polled from the UI timer. Nothing in
//! here is shared across threads, which is why it needs no synchronization
//! and why a redraw can never block a network task.
//!
//! Vocabulary shared with the data layer ([`Role`], [`Health`]) lives in
//! [`crate::model`]; this module holds only what the UI thread owns.

use std::collections::VecDeque;

use crate::model::{Health, Role};

/// The three consoles. Each is a different role in the same product, so the
/// sidebar lists them as peers rather than nesting client under server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Link overview: sessions, live scope, traffic totals.
    Console,
    /// Server configuration and lifecycle.
    Server,
    /// Client configuration and lifecycle.
    Client,
    /// SSH-tunnelled client configuration and lifecycle.
    Ssh,
    /// Runtime knobs shared by both ends (`[runtime]`).
    Runtime,
    /// Full log stream.
    Logs,
}

impl Page {
    /// Sidebar order, top to bottom. Overview first because it is the
    /// landing page; config pages next in the order a user sets them up
    /// (server on the remote host, then the local side); logs last.
    pub const ALL: [Page; 6] = [
        Page::Console,
        Page::Server,
        Page::Client,
        Page::Ssh,
        Page::Runtime,
        Page::Logs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Console => "链路总览",
            Page::Server => "服务端",
            Page::Client => "客户端",
            Page::Ssh => "SSH 隧道",
            Page::Runtime => "运行时",
            Page::Logs => "日志",
        }
    }

    /// Two-character mark used in the sidebar rail.
    pub fn mark(self) -> &'static str {
        match self {
            Page::Console => "◉",
            Page::Server => "▤",
            Page::Client => "▣",
            Page::Ssh => "⌁",
            Page::Runtime => "⚙",
            Page::Logs => "≡",
        }
    }
}

/// A rendered row in the session list. Produced by the app from the
/// launcher snapshot each poll; kept free of backend types so the widget
/// stays testable and the layout does not move when the backend does.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: u64,
    pub role: Role,
    pub health: Health,
    /// Primary line: what this session is doing ("监听 0.0.0.0:7000").
    pub headline: String,
    /// Secondary line: address, display, session id — whatever exists.
    pub detail: String,
    /// Wire bytes in / out for this session, if it reports them.
    pub sent: Option<u64>,
    pub received: Option<u64>,
    /// Uptime in seconds.
    pub uptime: Option<u64>,
}

/// One sample of the scope: bytes per second in each direction, plus the
/// savings ratio that the second trace draws.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub sent_bps: f64,
    pub recv_bps: f64,
}

/// Rolling window of samples. 180 samples at the 0.5 s poll interval is a
/// 90-second trace — long enough to see a redraw burst finish, short enough
/// that the newest detail is still wide on screen.
pub const SCOPE_SAMPLES: usize = 180;

/// Everything the UI thread owns.
pub struct AppState {
    pub page: Page,
    /// Sessions as of the last poll.
    pub sessions: Vec<SessionRow>,
    /// Rolling traffic samples for the scope.
    pub scope: VecDeque<Sample>,
    /// Aggregate figures for the summary strip.
    pub total_sent: u64,
    pub total_recv: u64,
    pub savings_ratio: f64,
    /// Active channel count (X11 + audio), as reported by the server stats.
    pub active_channels: u32,
    /// Config path being edited, shown in the status bar.
    pub config_path: String,
    /// Unsaved-changes flag per page, drives the status-bar indicator.
    pub dirty: [bool; 6],
    /// Log lines pulled so far (only the tail is kept in memory; the full
    /// ring lives in the launcher).
    pub log_seq: u64,
    /// Banner shown after a config save/validation failure.
    pub notice: Option<Notice>,
    /// Whether the link is currently up (any running client session).
    pub link_up: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            page: Page::Console,
            sessions: Vec::new(),
            scope: VecDeque::with_capacity(SCOPE_SAMPLES),
            total_sent: 0,
            total_recv: 0,
            savings_ratio: 0.0,
            active_channels: 0,
            config_path: String::new(),
            dirty: [false; 6],
            log_seq: 0,
            notice: None,
            link_up: false,
        }
    }
}

/// A transient message in the status bar. Not a modal: a config problem is
/// reported in place, next to the control that caused it, and disappears on
/// the next successful action.
#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
    pub at: std::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warn,
    Error,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoticeKind::Info,
            at: std::time::Instant::now(),
        }
    }

    pub fn warn(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoticeKind::Warn,
            at: std::time::Instant::now(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoticeKind::Error,
            at: std::time::Instant::now(),
        }
    }

    pub fn color(&self) -> u32 {
        use crate::theme::C;
        match self.kind {
            NoticeKind::Info => C::TEXT_2,
            NoticeKind::Warn => C::WARN,
            NoticeKind::Error => C::BAD,
        }
    }

    /// Notices fade after a while so a stale message never looks current.
    pub fn expired(&self) -> bool {
        let ttl = match self.kind {
            NoticeKind::Error => std::time::Duration::from_secs(12),
            _ => std::time::Duration::from_secs(6),
        };
        self.at.elapsed() > ttl
    }
}

impl AppState {
    pub fn new(config_path: String) -> Self {
        Self {
            config_path,
            ..Self::default()
        }
    }

    /// Push a fresh sample, dropping the oldest once the window is full.
    pub fn push_sample(&mut self, sample: Sample) {
        if self.scope.len() >= SCOPE_SAMPLES {
            self.scope.pop_front();
        }
        self.scope.push_back(sample);
    }

    /// Sessions of a given role.
    pub fn sessions_of(&self, role: Role) -> impl Iterator<Item = &SessionRow> {
        self.sessions.iter().filter(move |s| s.role == role)
    }

    /// Whether a role currently has anything running.
    pub fn is_active(&self, role: Role) -> bool {
        self.sessions_of(role).any(|s| {
            matches!(
                s.health,
                Health::Running | Health::Starting | Health::Reconnecting
            )
        })
    }

    pub fn notice(&mut self, n: Notice) {
        self.notice = Some(n);
    }

    /// Drop expired notices; called from the poll timer.
    pub fn tick_notice(&mut self) {
        if self.notice.as_ref().is_some_and(Notice::expired) {
            self.notice = None;
        }
    }

    pub fn mark_dirty(&mut self, page: Page) {
        self.dirty[page as usize] = true;
    }

    pub fn clear_dirty(&mut self, page: Page) {
        self.dirty[page as usize] = false;
    }

    pub fn any_dirty(&self) -> bool {
        self.dirty.iter().any(|d| *d)
    }
}

/// Human-readable byte size. Two significant decimals up to GB; the console
/// shows sizes far more often than it shows raw counts, so this is the
/// single formatting authority for every figure in the UI.
pub fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let f = n as f64;
    if f < KB {
        format!("{n} B")
    } else if f < KB * KB {
        format!("{:.1} KB", f / KB)
    } else if f < KB * KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else {
        format!("{:.2} GB", f / (KB * KB * KB))
    }
}

/// Bytes per second, for the scope's axis labels and the summary strip.
pub fn fmt_rate(bps: f64) -> String {
    if bps < 1.0 {
        "0 B/s".to_string()
    } else if bps < 1024.0 {
        format!("{bps:.0} B/s")
    } else if bps < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else {
        format!("{:.2} MB/s", bps / (1024.0 * 1024.0))
    }
}

/// Elapsed time as `1h 02m`, `3m 12s`, or `8s`.
pub fn fmt_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Percent with one decimal, used for the savings ratio.
pub fn fmt_pct(ratio: f64) -> String {
    format!("{:.1}%", (ratio * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting_is_stable() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn rate_formatting_handles_zero_and_units() {
        assert_eq!(fmt_rate(0.0), "0 B/s");
        assert_eq!(fmt_rate(512.0), "512 B/s");
        assert_eq!(fmt_rate(2048.0), "2.0 KB/s");
    }

    #[test]
    fn uptime_formatting_covers_all_ranges() {
        assert_eq!(fmt_uptime(8), "8s");
        assert_eq!(fmt_uptime(192), "3m 12s");
        assert_eq!(fmt_uptime(3720), "1h 02m");
    }

    #[test]
    fn scope_window_is_bounded() {
        let mut s = AppState::default();
        for i in 0..SCOPE_SAMPLES + 25 {
            s.push_sample(Sample {
                sent_bps: i as f64,
                ..Sample::default()
            });
        }
        assert_eq!(s.scope.len(), SCOPE_SAMPLES);
        // Newest sample is retained; oldest are dropped.
        assert_eq!(
            s.scope.back().unwrap().sent_bps,
            (SCOPE_SAMPLES + 24) as f64
        );
    }

    #[test]
    fn dirty_flags_are_per_page() {
        let mut s = AppState::default();
        assert!(!s.any_dirty());
        s.mark_dirty(Page::Server);
        assert!(s.dirty[Page::Server as usize]);
        assert!(!s.dirty[Page::Client as usize]);
        assert!(s.any_dirty());
        s.clear_dirty(Page::Server);
        assert!(!s.any_dirty());
    }

    #[test]
    fn expired_notices_are_dropped() {
        let mut s = AppState::default();
        s.notice(Notice::error("boom"));
        assert!(s.notice.is_some());
        // Force expiry rather than sleeping.
        if let Some(n) = s.notice.as_mut() {
            n.at = std::time::Instant::now() - std::time::Duration::from_secs(30);
        }
        s.tick_notice();
        assert!(s.notice.is_none());
    }
}
