use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

pub struct ConnectionStats {
    total_bytes_sent: AtomicU64,
    total_bytes_received: AtomicU64,
    x11_connections_active: AtomicU32,

    total_compression_saved: AtomicU64,
    total_compression_frames: AtomicU64,

    total_incremental_saved: AtomicU64,
    total_incremental_frames: AtomicU64,
    total_incremental_fallback: AtomicU64,

    period_bytes_sent: AtomicU64,
    period_bytes_received: AtomicU64,
    period_compression_saved: AtomicU64,
    period_compression_frames: AtomicU64,
    period_incremental_saved: AtomicU64,
    period_incremental_frames: AtomicU64,
    period_incremental_fallback: AtomicU64,

    start_time: Instant,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self {
            total_bytes_sent: AtomicU64::new(0),
            total_bytes_received: AtomicU64::new(0),
            x11_connections_active: AtomicU32::new(0),
            total_compression_saved: AtomicU64::new(0),
            total_compression_frames: AtomicU64::new(0),
            total_incremental_saved: AtomicU64::new(0),
            total_incremental_frames: AtomicU64::new(0),
            total_incremental_fallback: AtomicU64::new(0),
            period_bytes_sent: AtomicU64::new(0),
            period_bytes_received: AtomicU64::new(0),
            period_compression_saved: AtomicU64::new(0),
            period_compression_frames: AtomicU64::new(0),
            period_incremental_saved: AtomicU64::new(0),
            period_incremental_frames: AtomicU64::new(0),
            period_incremental_fallback: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    pub fn add_bytes_sent(&self, n: u64) {
        self.total_bytes_sent.fetch_add(n, Ordering::Relaxed);
        self.period_bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_bytes_received(&self, n: u64) {
        self.total_bytes_received.fetch_add(n, Ordering::Relaxed);
        self.period_bytes_received.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_compression_saved(&self, saved: u64) {
        if saved > 0 {
            self.total_compression_saved
                .fetch_add(saved, Ordering::Relaxed);
            self.total_compression_frames
                .fetch_add(1, Ordering::Relaxed);
            self.period_compression_saved
                .fetch_add(saved, Ordering::Relaxed);
            self.period_compression_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_incremental_saved(&self, saved: u64) {
        if saved > 0 {
            self.total_incremental_saved
                .fetch_add(saved, Ordering::Relaxed);
            self.total_incremental_frames
                .fetch_add(1, Ordering::Relaxed);
            self.period_incremental_saved
                .fetch_add(saved, Ordering::Relaxed);
            self.period_incremental_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_incremental_full_fallback(&self) {
        self.total_incremental_fallback
            .fetch_add(1, Ordering::Relaxed);
        self.period_incremental_fallback
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_x11_connections(&self) {
        self.x11_connections_active.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_x11_connections(&self) {
        let prev =
            self.x11_connections_active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1));
        if prev.is_err() {
            tracing::warn!("x11_connections_active underflow detected");
        }
    }

    pub fn reset_period(&self) {
        self.period_bytes_sent.store(0, Ordering::Relaxed);
        self.period_bytes_received.store(0, Ordering::Relaxed);
        self.period_compression_saved.store(0, Ordering::Relaxed);
        self.period_compression_frames.store(0, Ordering::Relaxed);
        self.period_incremental_saved.store(0, Ordering::Relaxed);
        self.period_incremental_frames.store(0, Ordering::Relaxed);
        self.period_incremental_fallback.store(0, Ordering::Relaxed);
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

        self.append_transfer_stats_lines(&mut lines, "Total", self.get_total_stats());
        self.append_transfer_stats_lines(&mut lines, "Period", self.get_period_stats());

        lines.push(format!("  Uptime: {}", format_duration(uptime)));

        lines.join("\n")
    }

    fn get_total_stats(&self) -> StatsSnapshot {
        StatsSnapshot {
            bytes_sent: self.total_bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.total_bytes_received.load(Ordering::Relaxed),
            compression_saved: self.total_compression_saved.load(Ordering::Relaxed),
            compression_frames: self.total_compression_frames.load(Ordering::Relaxed),
            incremental_saved: self.total_incremental_saved.load(Ordering::Relaxed),
            incremental_frames: self.total_incremental_frames.load(Ordering::Relaxed),
            incremental_fallback: self.total_incremental_fallback.load(Ordering::Relaxed),
        }
    }

    fn get_period_stats(&self) -> StatsSnapshot {
        StatsSnapshot {
            bytes_sent: self.period_bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.period_bytes_received.load(Ordering::Relaxed),
            compression_saved: self.period_compression_saved.load(Ordering::Relaxed),
            compression_frames: self.period_compression_frames.load(Ordering::Relaxed),
            incremental_saved: self.period_incremental_saved.load(Ordering::Relaxed),
            incremental_frames: self.period_incremental_frames.load(Ordering::Relaxed),
            incremental_fallback: self.period_incremental_fallback.load(Ordering::Relaxed),
        }
    }

    fn append_transfer_stats_lines(
        &self,
        lines: &mut Vec<String>,
        prefix: &str,
        stats: StatsSnapshot,
    ) {
        let has_transfer = stats.bytes_sent > 0 || stats.bytes_received > 0;
        let has_compression = stats.compression_saved > 0 || stats.compression_frames > 0;
        let has_incremental = stats.incremental_saved > 0
            || stats.incremental_frames > 0
            || stats.incremental_fallback > 0;

        if has_transfer || has_compression || has_incremental {
            lines.push(format!("  {}:", prefix));

            if stats.bytes_sent > 0 || stats.bytes_received > 0 {
                lines.push(format!("    Sent: {}", format_bytes(stats.bytes_sent)));
                lines.push(format!("    Recv: {}", format_bytes(stats.bytes_received)));
            }

            if has_compression {
                let mut comp_parts = vec![format!("{} frames", stats.compression_frames)];
                if stats.compression_saved > 0 {
                    comp_parts.push(format!("saved {}", format_bytes(stats.compression_saved)));
                }
                lines.push(format!("    Compressed: {}", comp_parts.join(", ")));
            }

            if has_incremental {
                let mut inc_parts = vec![format!("{} frames", stats.incremental_frames)];
                if stats.incremental_saved > 0 {
                    inc_parts.push(format!("saved {}", format_bytes(stats.incremental_saved)));
                }
                if stats.incremental_fallback > 0 {
                    inc_parts.push(format!("{} fallbacks", stats.incremental_fallback));
                }
                lines.push(format!("    Incremental: {}", inc_parts.join(", ")));
            }
        }
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
}

impl Default for ConnectionStats {
    fn default() -> Self {
        Self::new()
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

fn format_duration(d: std::time::Duration) -> String {
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
    fn test_stats_initial_values() {
        let stats = ConnectionStats::new();
        assert_eq!(stats.total_bytes_sent.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total_bytes_received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.x11_connections_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_stats_add_bytes() {
        let stats = ConnectionStats::new();
        stats.add_bytes_sent(100);
        stats.add_bytes_received(200);
        assert_eq!(stats.total_bytes_sent.load(Ordering::Relaxed), 100);
        assert_eq!(stats.total_bytes_received.load(Ordering::Relaxed), 200);
        assert_eq!(stats.period_bytes_sent.load(Ordering::Relaxed), 100);
        assert_eq!(stats.period_bytes_received.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn test_stats_reset_period() {
        let stats = ConnectionStats::new();
        stats.add_bytes_sent(100);
        stats.add_bytes_received(200);
        stats.add_compression_saved(50);
        stats.add_incremental_saved(30);

        stats.reset_period();

        assert_eq!(stats.total_bytes_sent.load(Ordering::Relaxed), 100);
        assert_eq!(stats.total_bytes_received.load(Ordering::Relaxed), 200);
        assert_eq!(stats.period_bytes_sent.load(Ordering::Relaxed), 0);
        assert_eq!(stats.period_bytes_received.load(Ordering::Relaxed), 0);
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
        assert_eq!(stats.total_bytes_sent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_format_duration_seconds() {
        let d = std::time::Duration::from_secs(45);
        assert_eq!(format_duration(d), "45s");
    }

    #[test]
    fn test_format_duration_minutes() {
        let d = std::time::Duration::from_secs(90);
        assert_eq!(format_duration(d), "1m30s");
    }

    #[test]
    fn test_format_duration_hours() {
        let d = std::time::Duration::from_secs(3661);
        assert_eq!(format_duration(d), "1h1m1s");
    }

    #[test]
    fn test_format_duration_days() {
        let d = std::time::Duration::from_secs(90061);
        assert_eq!(format_duration(d), "1d1h1m1s");
    }
}
