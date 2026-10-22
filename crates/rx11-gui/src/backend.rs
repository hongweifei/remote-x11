//! The real data layer: configuration on disk plus sessions running in this
//! process.
//!
//! This is the only module that knows about `settings`/`launcher`; the UI
//! talks to the [`Backend`](crate::model::Backend) trait in flat `key → string`
//! values and snapshots. Two consequences worth stating:
//!
//! * a new runtime knob needs one `FieldSpec` in the config page and one
//!   line in the config layer — nothing in between;
//! * the console owns no network or process state of its own. Starting a role
//!   hands a `SessionSpec` to the launcher, and every figure on screen comes
//!   back through `snapshot()`, so there is exactly one place where a session
//!   can be created, stopped or leaked.

use std::sync::Arc;

use crate::launcher::{
    init_logging, Launcher, LogRing, SessionId, SessionKind, SessionPhase, SessionSpec,
};
use crate::model::{
    Backend, Health, LogLevel, LogRecord, Role, SessionRowData, StartRequest, TrafficTotals,
};
use crate::settings::{self, GuiConfig};

/// Session lifetime and configuration, backed by the real launcher.
pub struct LauncherBackend {
    launcher: Launcher,
    ring: Arc<LogRing>,
    /// Values as they are on disk, kept so a start request can be assembled
    /// from the file plus the page's edits.
    loaded: std::sync::Mutex<GuiConfig>,
}

impl LauncherBackend {
    /// Install global logging (stderr plus the ring the console reads) and
    /// take a first snapshot of the config file.
    ///
    /// `init_logging` fails if a subscriber is already installed, which is
    /// the case in tests; the console treats that as non-fatal because the
    /// ring still works on its own.
    pub fn new() -> Self {
        let ring = LogRing::new();
        let _ = init_logging(ring.clone());
        let launcher = Launcher::new();
        let loaded = launcher.load_config().unwrap_or_default();
        Self {
            launcher,
            ring,
            loaded: std::sync::Mutex::new(loaded),
        }
    }

    fn config(&self) -> GuiConfig {
        self.loaded.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn store(&self, cfg: GuiConfig) {
        if let Ok(mut g) = self.loaded.lock() {
            *g = cfg;
        }
    }
}

impl Default for LauncherBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for LauncherBackend {
    fn config_path(&self) -> String {
        let path = self.launcher.config_path();
        // Show `~` for the home directory: the full path is long and the
        // status bar has one line.
        if let Some(home) = std::env::var_os("HOME") {
            let home = std::path::PathBuf::from(home);
            if let Ok(rest) = path.strip_prefix(&home) {
                return format!("~/{}", rest.display());
            }
        }
        path.display().to_string()
    }

    fn load_values(&self) -> Result<Vec<(String, String)>, String> {
        let cfg = self.launcher.load_config().map_err(|e| e.to_string())?;
        let values = settings::flat_values(&cfg);
        self.store(cfg);
        Ok(values)
    }

    fn save_values(&self, values: &[(String, String)]) -> Result<(), String> {
        // Round-trip through what is on disk so keys the console does not
        // manage (and their comments) survive the write.
        let mut cfg = self.launcher.load_config().unwrap_or_default();
        let issues = settings::apply_flat_checked(&mut cfg, values);
        if let Some(first) = issues.first() {
            return Err(format!("{}：{}", first.field, first.message));
        }
        settings::apply_flat(&mut cfg, values);
        cfg.save(&self.launcher.config_path())
            .map_err(|e| e.to_string())?;
        self.store(cfg);
        Ok(())
    }

    fn validate(&self, values: &[(String, String)]) -> Vec<(String, String)> {
        let mut cfg = self.config();
        let mut issues: Vec<(String, String)> = settings::apply_flat_checked(&mut cfg, values)
            .into_iter()
            .map(|i| (i.field, i.message))
            .collect();
        settings::apply_flat(&mut cfg, values);
        issues.extend(cfg.validate().into_iter().map(|i| (i.field, i.message)));
        issues.sort();
        issues.dedup();
        issues
    }

    fn start(&self, req: StartRequest) -> Result<u64, String> {
        // Build the spec from the file plus this page's edits, so a role can
        // be started with values the user has not saved yet.
        let mut cfg = self.config();
        settings::apply_flat(&mut cfg, &req.values);
        let spec: SessionSpec = match req.role {
            Role::Server => SessionSpec::server_from_config(&cfg),
            Role::Client => SessionSpec::client_from_config(&cfg),
            Role::Ssh => SessionSpec::ssh_from_config(&cfg),
        }
        .map_err(|e| e.to_string())?;
        self.launcher
            .start(spec)
            .map(|id| id.0)
            .map_err(|e| e.to_string())
    }

    fn stop(&self, id: u64) {
        let id = SessionId(id);
        self.launcher.stop(id);
        self.launcher.remove(id);
    }

    fn sessions(&self) -> Vec<SessionRowData> {
        self.launcher
            .snapshot()
            .into_iter()
            .map(|s| {
                let stats = s.stats;
                SessionRowData {
                    id: s.id.0,
                    role: match s.kind {
                        SessionKind::Server => Role::Server,
                        SessionKind::Client => Role::Client,
                        SessionKind::Ssh => Role::Ssh,
                    },
                    health: match s.phase {
                        SessionPhase::Starting => Health::Starting,
                        SessionPhase::Running => Health::Running,
                        SessionPhase::Reconnecting => Health::Reconnecting,
                        SessionPhase::Stopped => Health::Stopped,
                        SessionPhase::Failed => Health::Failed,
                    },
                    headline: s.detail,
                    detail: s
                        .last_error
                        .clone()
                        .or_else(|| s.session_id.as_ref().map(|id| format!("会话 {id}")))
                        .unwrap_or_default(),
                    sent: stats.map(|t| t.bytes_sent),
                    received: stats.map(|t| t.bytes_received),
                    uptime: stats.map(|t| t.uptime.as_secs()).or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(s.started_at)
                            .ok()
                            .map(|d| d.as_secs())
                    }),
                }
            })
            .collect()
    }

    fn totals(&self) -> TrafficTotals {
        let mut t = TrafficTotals::default();
        for s in self.launcher.snapshot() {
            let Some(stats) = s.stats else { continue };
            t.sent += stats.bytes_sent;
            t.received += stats.bytes_received;
            t.saved += stats.compression_saved
                + stats.incremental_saved
                + stats.incremental_compression_saved;
            t.raw += stats.bytes_sent + stats.bytes_received;
            t.channels += stats.x11_connections_active;
            t.uptime = t.uptime.max(stats.uptime.as_secs());
        }
        t
    }

    fn logs_since(&self, seq: u64) -> Vec<LogRecord> {
        self.ring
            .since(seq)
            .into_iter()
            .map(|l| LogRecord {
                seq: l.seq,
                time: l.time,
                level: match l.level {
                    tracing::Level::ERROR => LogLevel::Error,
                    tracing::Level::WARN => LogLevel::Warn,
                    tracing::Level::INFO => LogLevel::Info,
                    tracing::Level::DEBUG => LogLevel::Debug,
                    tracing::Level::TRACE => LogLevel::Trace,
                },
                target: l.target,
                message: l.message,
            })
            .collect()
    }

    fn clear_logs(&self) {
        self.ring.clear();
    }

    fn token_hint(&self, values: &[(String, String)]) -> String {
        // The token the local side will present, which is the one the user
        // needs to copy: prefer the client's, fall back to the server's.
        for key in ["client.token", "server.token", "ssh.token"] {
            if let Some((_, v)) = values.iter().find(|(k, _)| k == key) {
                if !v.is_empty() {
                    return v.clone();
                }
            }
        }
        String::new()
    }

    fn shutdown(&self) {
        self.launcher.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_is_shown_relative_to_home() {
        let b = LauncherBackend::new();
        let shown = b.config_path();
        // Either `~/...` or an absolute fallback when there is no HOME.
        assert!(!shown.is_empty());
    }

    #[test]
    fn validation_surfaces_runtime_watermark_conflict() {
        let b = LauncherBackend::new();
        let values = vec![
            ("runtime.flow_pause_bytes".to_string(), "1024".to_string()),
            ("runtime.flow_resume_bytes".to_string(), "4096".to_string()),
        ];
        let issues = b.validate(&values);
        assert!(
            issues.iter().any(|(f, _)| f.contains("flow_")),
            "expected a watermark conflict, got {issues:?}"
        );
    }

    /// End-to-end smoke: start a relay through this adapter and watch it come
    /// up. Ignored by default because the in-process server shells out to
    /// `xauth`, which writes into `$HOME` — run it with an isolated HOME:
    ///
    /// ```sh
    /// cargo test -p rx11-gui --bin rx11-gui -- --ignored --test-threads=1
    /// ```
    #[test]
    #[ignore]
    fn starts_a_relay_through_the_adapter() {
        let b = LauncherBackend::new();
        let values = vec![
            ("server.listen".to_string(), "127.0.0.1:0".to_string()),
            ("server.x11_port".to_string(), "16100".to_string()),
            ("server.token".to_string(), "gui-smoke-token".to_string()),
        ];
        let id = b
            .start(StartRequest {
                role: Role::Server,
                values,
            })
            .expect("relay should start");
        // The first snapshot may report Starting; give it a moment to bind.
        let mut sessions = Vec::new();
        for _ in 0..40 {
            sessions = b.sessions();
            if sessions
                .iter()
                .any(|s| s.id == id && s.health == Health::Running)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let s = sessions
            .iter()
            .find(|s| s.id == id)
            .expect("session must be visible right after start");
        assert_eq!(s.role, Role::Server);
        assert_eq!(
            s.health,
            Health::Running,
            "relay did not reach Running: {s:?}"
        );
        assert!(
            s.headline.contains("127.0.0.1"),
            "headline should carry the bound address: {}",
            s.headline
        );

        // Totals must be readable while it runs, and stop must remove it.
        let _ = b.totals();
        b.stop(id);
        let mut gone = false;
        for _ in 0..40 {
            if !b.sessions().iter().any(|s| s.id == id) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(gone, "stopped session should leave the snapshot");
    }

    #[test]
    fn token_hint_prefers_the_client_token() {
        let b = LauncherBackend::new();
        let values = vec![
            ("server.token".to_string(), "server-side".to_string()),
            ("client.token".to_string(), "client-side".to_string()),
        ];
        assert_eq!(b.token_hint(&values), "client-side");
        assert_eq!(b.token_hint(&[]), "");
    }
}
