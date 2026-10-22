use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rx11_client::connector::{ConnectorHandle, LocalConnector};
use rx11_server::relay::RelayHandle;
use rx11_server::session::SessionInfo;

use super::lock;
use super::session::{
    client_detail, map_connector_phase, ssh_detail, SessionId, SessionKind, SessionPhase,
    SessionSnapshot,
};

// ---------------------------------------------------------------------------
// 会话内部状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(super) struct SessionState {
    /// 终态结论（由后台任务写一次）；一旦写入就不再被连接器状态覆盖。
    pub(super) terminal: Option<(SessionPhase, String)>,
    /// 静态摘要（SSH 的「端点 → 本地端口」），会话不变时用它即可。
    pub(super) detail: String,
}

#[derive(Default)]
pub(super) struct ServerExtras {
    pub(super) sessions: Vec<SessionInfo>,
}

pub(super) enum SessionBody {
    Server {
        handle: RelayHandle,
        extras: Arc<Mutex<ServerExtras>>,
    },
    Client {
        handle: ConnectorHandle,
        connector: Arc<LocalConnector>,
        relay: String,
    },
    Ssh {
        handle: ConnectorHandle,
        connector: Arc<LocalConnector>,
        endpoint: String,
        local_addr: String,
        /// 优雅停止超时后的强制放弃信号。
        force: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    },
}

pub(super) struct SessionRecord {
    pub(super) id: SessionId,
    pub(super) kind: SessionKind,
    pub(super) started_at: SystemTime,
    pub(super) state: Arc<Mutex<SessionState>>,
    pub(super) body: SessionBody,
}

impl SessionRecord {
    pub(super) fn is_live(&self) -> bool {
        self.snapshot().phase.is_live()
    }

    /// 发送停止请求；返回 SSH 会话的「强制放弃」信号（非 SSH 为 `None`）。
    pub(super) fn request_stop(&self) -> Option<tokio::sync::oneshot::Sender<()>> {
        match &self.body {
            SessionBody::Server { handle, .. } => {
                handle.shutdown();
                None
            }
            SessionBody::Client { handle, .. } => {
                handle.stop();
                None
            }
            SessionBody::Ssh { handle, force, .. } => {
                handle.stop();
                lock(force).take()
            }
        }
    }

    /// 供日志用的一句话摘要（读当前状态）。
    pub(super) fn detail(&self) -> String {
        self.snapshot().detail
    }

    /// 纯读快照：只碰 `Mutex` 与原子量，不 `await`。
    pub(super) fn snapshot(&self) -> SessionSnapshot {
        let st = lock(&self.state).clone();
        let base = |phase: SessionPhase, detail: String| SessionSnapshot {
            id: self.id,
            kind: self.kind,
            phase,
            detail,
            display: None,
            session_id: None,
            stats: None,
            started_at: self.started_at,
            last_error: None,
        };
        match &self.body {
            SessionBody::Server { handle, extras } => {
                let sessions = lock(extras).sessions.clone();
                // 终态由 wait_stopped() 的后台任务写入；在此之前用 is_running()
                // 判定，避免 shutdown() 到 wait_stopped() 返回之间的空窗显示错阶段。
                let phase = match &st.terminal {
                    Some((phase, _)) => *phase,
                    None if handle.is_running() => SessionPhase::Running,
                    None => SessionPhase::Stopped,
                };
                let detail = if phase == SessionPhase::Running {
                    format!(
                        "监听 {} · {} 个客户端连接 · {} 个会话",
                        handle.local_addr(),
                        handle.active_connections(),
                        sessions.len()
                    )
                } else {
                    let why = st
                        .terminal
                        .as_ref()
                        .map(|(_, message)| message.as_str())
                        .unwrap_or("已停止");
                    format!("监听 {} · {why}", handle.local_addr())
                };
                let (display, session_id) = match sessions.as_slice() {
                    [only] => (Some(only.display), Some(only.session_id.clone())),
                    _ => (None, None),
                };
                SessionSnapshot {
                    phase,
                    detail,
                    display,
                    session_id,
                    stats: Some(handle.stats().totals()),
                    last_error: st.terminal.as_ref().and_then(|(phase, message)| {
                        (*phase == SessionPhase::Failed).then(|| message.clone())
                    }),
                    ..base(phase, String::new())
                }
            }
            SessionBody::Client {
                handle,
                connector,
                relay,
            } => {
                let cs = handle.state();
                let phase = map_connector_phase(cs.phase);
                let detail = client_detail(relay, &cs, phase);
                SessionSnapshot {
                    phase,
                    detail,
                    display: cs.display,
                    session_id: cs.session_id.clone(),
                    stats: Some(connector.stats().totals()),
                    last_error: cs.last_error.clone(),
                    ..base(phase, String::new())
                }
            }
            SessionBody::Ssh {
                handle,
                connector,
                endpoint,
                local_addr,
                ..
            } => {
                let cs = handle.state();
                let mapped = map_connector_phase(cs.phase);
                let connector_detail = ssh_detail(endpoint, local_addr, &cs, mapped);
                let (phase, detail, last_error) = match &st.terminal {
                    Some((phase, message)) => (
                        *phase,
                        if st.detail.is_empty() {
                            connector_detail
                        } else {
                            format!("{} · {}", st.detail, message)
                        },
                        Some(message.clone()),
                    ),
                    None => (mapped, connector_detail, cs.last_error.clone()),
                };
                SessionSnapshot {
                    phase,
                    detail,
                    display: cs.display,
                    session_id: cs.session_id.clone(),
                    stats: Some(connector.stats().totals()),
                    last_error,
                    ..base(phase, String::new())
                }
            }
        }
    }
}
