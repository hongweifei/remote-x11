//! Lock-free connection statistics: throughput, compression/incremental
//! savings and active X11 connections, tracked as lifetime totals plus a
//! resettable reporting period.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A counter maintained for both the whole session and the current period.
#[derive(Default)]
struct Counter {
    total: AtomicU64,
    period: AtomicU64,
}

impl Counter {
    fn add(&self, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
        self.period.fetch_add(n, Ordering::Relaxed);
    }

    fn incr(&self) {
        self.add(1);
    }

    fn load_total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    fn load_period(&self) -> u64 {
        self.period.load(Ordering::Relaxed)
    }

    fn reset_period(&self) {
        self.period.store(0, Ordering::Relaxed);
    }
}

/// Savings plus the number of frames that produced them.
#[derive(Default)]
struct Savings {
    saved: Counter,
    frames: Counter,
}

impl Savings {
    fn record(&self, saved: u64) {
        if saved > 0 {
            self.saved.add(saved);
            self.frames.incr();
        }
    }
}

pub struct ConnectionStats {
    bytes_sent: Counter,
    bytes_received: Counter,
    x11_connections_active: AtomicU32,

    compression: Savings,
    incremental: Savings,
    incremental_fallback: Counter,
    incremental_compression: Savings,

    start_time: Instant,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self {
            bytes_sent: Counter::default(),
            bytes_received: Counter::default(),
            x11_connections_active: AtomicU32::new(0),
            compression: Savings::default(),
            incremental: Savings::default(),
            incremental_fallback: Counter::default(),
            incremental_compression: Savings::default(),
            start_time: Instant::now(),
        }
    }

    pub fn add_bytes_sent(&self, n: u64) {
        self.bytes_sent.add(n);
    }

    pub fn add_bytes_received(&self, n: u64) {
        self.bytes_received.add(n);
    }

    pub fn add_compression_saved(&self, saved: u64) {
        self.compression.record(saved);
    }

    pub fn add_incremental_saved(&self, saved: u64) {
        self.incremental.record(saved);
    }

    pub fn add_incremental_full_fallback(&self) {
        self.incremental_fallback.incr();
    }

    pub fn add_incremental_compression_saved(&self, saved: u64) {
        self.incremental_compression.record(saved);
    }

    pub fn inc_x11_connections(&self) {
        self.x11_connections_active.fetch_add(1, Ordering::Relaxed);
    }

    // `fetch_update` was renamed to `try_update` in Rust 1.99; keep the
    // long-stable spelling for compatibility with earlier toolchains.
    #[allow(deprecated)]
    pub fn dec_x11_connections(&self) {
        let prev =
            self.x11_connections_active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1));
        if prev.is_err() {
            tracing::warn!("x11_connections_active underflow detected");
        }
    }

    pub fn reset_period(&self) {
        self.bytes_sent.reset_period();
        self.bytes_received.reset_period();
        self.compression.saved.reset_period();
        self.compression.frames.reset_period();
        self.incremental.saved.reset_period();
        self.incremental.frames.reset_period();
        self.incremental_fallback.reset_period();
        self.incremental_compression.saved.reset_period();
        self.incremental_compression.frames.reset_period();
    }

    pub fn summary(&self) -> String {
        let active = self.x11_connections_active.load(Ordering::Relaxed);
        let uptime = self.start_time.elapsed();

        let conn_str = if active == 1 {
            "connection"
        } else {
            "connections"
        };

        let mut lines = vec![format!(" {} active X11 {}", active, conn_str)];
        self.snapshot(false).push_lines(&mut lines, "Total");
        self.snapshot(true).push_lines(&mut lines, "Period");
        lines.push(format!("  Uptime: {}", format_duration(uptime)));

        lines.join("\n")
    }

    fn snapshot(&self, period: bool) -> StatsSnapshot {
        let g = |c: &Counter| {
            if period {
                c.load_period()
            } else {
                c.load_total()
            }
        };
        StatsSnapshot {
            bytes_sent: g(&self.bytes_sent),
            bytes_received: g(&self.bytes_received),
            compression_saved: g(&self.compression.saved),
            compression_frames: g(&self.compression.frames),
            incremental_saved: g(&self.incremental.saved),
            incremental_frames: g(&self.incremental.frames),
            incremental_fallback: g(&self.incremental_fallback),
            incremental_compression_saved: g(&self.incremental_compression.saved),
            incremental_compression_frames: g(&self.incremental_compression.frames),
        }
    }
}

impl Default for ConnectionStats {
    fn default() -> Self {
        Self::new()
    }
}

struct StatsSnapshot {
    bytes_sent: u64,
    bytes_received: u64,
    compression_saved: u64,
    compression_frames: u64,
    incremental_saved: u64,
    incremental_frames: u64,
    incremental_fallback: u64,
    incremental_compression_saved: u64,
    incremental_compression_frames: u64,
}

impl StatsSnapshot {
    fn push_lines(&self, lines: &mut Vec<String>, prefix: &str) {
        let has_transfer = self.bytes_sent > 0 || self.bytes_received > 0;
        let has_compression = self.compression_saved > 0 || self.compression_frames > 0;
        let has_incremental = self.incremental_saved > 0
            || self.incremental_frames > 0
            || self.incremental_fallback > 0;
        let has_incremental_compression =
            self.incremental_compression_saved > 0 || self.incremental_compression_frames > 0;

        if !(has_transfer || has_compression || has_incremental || has_incremental_compression) {
            return;
        }

        lines.push(format!("  {}:", prefix));
        if has_transfer {
            lines.push(format!("    Sent: {}", format_bytes(self.bytes_sent)));
            lines.push(format!("    Recv: {}", format_bytes(self.bytes_received)));
        }
        if has_compression {
            lines.push(format!(
                "    Compressed (full): {}",
                Self::parts(self.compression_frames, self.compression_saved, "")
            ));
        }
        if has_incremental {
            lines.push(format!(
                "    Incremental: {}",
                Self::parts(
                    self.incremental_frames,
                    self.incremental_saved,
                    &if self.incremental_fallback > 0 {
                        format!(", {} fallbacks", self.incremental_fallback)
                    } else {
                        String::new()
                    }
                )
            ));
        }
        if has_incremental_compression {
            lines.push(format!(
                "    Compressed (incremental): {}",
                Self::parts(
                    self.incremental_compression_frames,
                    self.incremental_compression_saved,
                    ""
                )
            ));
        }
    }

    /// `"<frames> frames[, saved <bytes>][<suffix>]"`
    fn parts(frames: u64, saved: u64, suffix: &str) -> String {
        let mut parts = vec![format!("{} frames", frames)];
        if saved > 0 {
            parts.push(format!("saved {}", format_bytes(saved)));
        }
        format!("{}{}", parts.join(", "), suffix)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if days > 0 {
        format!("{}d{}h{}m{}s", days, hours, mins, secs)
    } else if hours > 0 {
        format!("{}h{}m{}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m{}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn test_format_bytes_kb() {
        let result = format_bytes(1536);
        assert!(result.contains("KB"));
    }

    #[test]
    fn test_format_bytes_mb() {
        let result = format_bytes(1024 * 1024 * 5);
        assert!(result.contains("MB"));
    }

    #[test]
    fn test_format_bytes_gb() {
        let result = format_bytes(1024 * 1024 * 1024 * 3);
        assert!(result.contains("GB"));
    }

    #[test]
    fn test_counter_tracks_total_and_period() {
        let c = Counter::default();
        c.add(10);
        c.incr();
        assert_eq!(c.load_total(), 11);
        assert_eq!(c.load_period(), 11);
        c.reset_period();
        assert_eq!(c.load_total(), 11);
        assert_eq!(c.load_period(), 0);
    }

    #[test]
    fn test_savings_ignores_zero() {
        let s = Savings::default();
        s.record(0);
        assert_eq!(s.frames.load_total(), 0);
        s.record(5);
        assert_eq!(s.saved.load_total(), 5);
        assert_eq!(s.frames.load_total(), 1);
    }

    #[test]
    fn test_stats_initial_values() {
        let stats = ConnectionStats::new();
        assert_eq!(stats.bytes_sent.load_total(), 0);
        assert_eq!(stats.bytes_received.load_total(), 0);
        assert_eq!(stats.x11_connections_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_stats_add_bytes() {
        let stats = ConnectionStats::new();
        stats.add_bytes_sent(100);
        stats.add_bytes_received(200);
        assert_eq!(stats.bytes_sent.load_total(), 100);
        assert_eq!(stats.bytes_received.load_total(), 200);
        assert_eq!(stats.bytes_sent.load_period(), 100);
        assert_eq!(stats.bytes_received.load_period(), 200);
    }

    #[test]
    fn test_stats_reset_period() {
        let stats = ConnectionStats::new();
        stats.add_bytes_sent(100);
        stats.add_bytes_received(200);
        stats.add_compression_saved(50);
        stats.add_incremental_saved(30);

        stats.reset_period();

        assert_eq!(stats.bytes_sent.load_total(), 100);
        assert_eq!(stats.bytes_received.load_total(), 200);
        assert_eq!(stats.bytes_sent.load_period(), 0);
        assert_eq!(stats.bytes_received.load_period(), 0);
        assert_eq!(stats.compression.saved.load_total(), 50);
        assert_eq!(stats.compression.saved.load_period(), 0);
    }

    #[test]
    fn test_stats_x11_connections() {
        let stats = ConnectionStats::new();
        stats.inc_x11_connections();
        stats.inc_x11_connections();
        assert_eq!(stats.x11_connections_active.load(Ordering::Relaxed), 2);
        stats.dec_x11_connections();
        assert_eq!(stats.x11_connections_active.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_stats_dec_does_not_underflow() {
        let stats = ConnectionStats::new();
        stats.dec_x11_connections();
        assert_eq!(stats.x11_connections_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_default() {
        let stats = ConnectionStats::default();
        assert_eq!(stats.bytes_sent.load_total(), 0);
    }

    #[test]
    fn test_summary_contains_active_section() {
        let stats = ConnectionStats::new();
        stats.add_bytes_sent(4096);
        stats.add_compression_saved(100);
        let summary = stats.summary();
        assert!(
            summary.contains("1 active X11 connection")
                || summary.contains("0 active X11 connections")
        );
        assert!(summary.contains("Total:"));
        assert!(summary.contains("Sent:"));
        assert!(summary.contains("Compressed (full): 1 frames, saved 100 B"));
        assert!(summary.contains("Uptime:"));
    }

    #[test]
    fn test_summary_empty_stats_omits_sections() {
        let stats = ConnectionStats::new();
        let summary = stats.summary();
        assert!(!summary.contains("Total:"));
        assert!(!summary.contains("Period:"));
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h1m1s");
    }

    #[test]
    fn test_format_duration_days() {
        assert_eq!(format_duration(Duration::from_secs(90061)), "1d1h1m1s");
    }
}
