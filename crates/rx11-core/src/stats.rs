use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

pub struct ConnectionStats {
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub x11_connections_active: AtomicU32,
    start_time: Instant,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self {
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            x11_connections_active: AtomicU32::new(0),
            start_time: Instant::now(),
        }
    }

    pub fn add_bytes_sent(&self, n: u64) {
        self.bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_bytes_received(&self, n: u64) {
        self.bytes_received.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_x11_connections(&self) {
        self.x11_connections_active.fetch_add(1, Ordering::Release);
    }

    pub fn dec_x11_connections(&self) {
        let prev =
            self.x11_connections_active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1));
        if prev.is_err() {
            tracing::warn!("x11_connections_active underflow detected");
        }
    }

    pub fn summary(&self) -> String {
        let sent = self.bytes_sent.load(Ordering::Relaxed);
        let recv = self.bytes_received.load(Ordering::Relaxed);
        let active = self.x11_connections_active.load(Ordering::Acquire);
        let uptime = self.start_time.elapsed();

        let conn_str = if active == 1 {
            "connection".to_string()
        } else {
            "connections".to_string()
        };

        format!(
            "{} active X11 {} | Sent: {} | Recv: {} | Uptime: {}",
            active,
            conn_str,
            format_bytes(sent),
            format_bytes(recv),
            format_duration(uptime),
        )
    }
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
    } else {
        format!("{}m{}s", mins, secs)
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
        assert_eq!(stats.bytes_sent.load(Ordering::Relaxed), 0);
        assert_eq!(stats.bytes_received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.x11_connections_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_stats_add_bytes() {
        let stats = ConnectionStats::new();
        stats.add_bytes_sent(100);
        stats.add_bytes_received(200);
        assert_eq!(stats.bytes_sent.load(Ordering::Relaxed), 100);
        assert_eq!(stats.bytes_received.load(Ordering::Relaxed), 200);
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
    fn test_stats_summary_contains_active() {
        let stats = ConnectionStats::new();
        stats.inc_x11_connections();
        let summary = stats.summary();
        assert!(summary.contains("1 active X11 connection"));
    }

    #[test]
    fn test_stats_summary_plural() {
        let stats = ConnectionStats::new();
        stats.inc_x11_connections();
        stats.inc_x11_connections();
        let summary = stats.summary();
        assert!(summary.contains("2 active X11 connections"));
    }

    #[test]
    fn test_default() {
        let stats = ConnectionStats::default();
        assert_eq!(stats.bytes_sent.load(Ordering::Relaxed), 0);
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
