use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use rx11_client::connector::{ConnectorPhase, ConnectorState};
use rx11_client::ssh::{SshTunnel, SupervisionEnd};
use rx11_core::config::{ClientDefaults, RuntimeConfig, ServerDefaults, SshDefaults};
use rx11_core::protocol::{DEFAULT_RELAY_PORT, DEFAULT_X11_PORT};
use rx11_core::stats::StatsTotals;

use super::GuiDefaults;
use crate::settings::GuiConfig;

// ---------------------------------------------------------------------------
// 会话角色 / 阶段
// ---------------------------------------------------------------------------

/// 会话角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKind {
    Server,
    Client,
    Ssh,
}

impl SessionKind {
    pub fn label(self) -> &'static str {
        match self {
            SessionKind::Server => "服务端",
            SessionKind::Client => "客户端",
            SessionKind::Ssh => "SSH 隧道",
        }
    }
}

/// GUI 视角的会话阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Starting,
    Running,
    Reconnecting,
    Stopped,
    Failed,
}

impl SessionPhase {
    pub fn label(self) -> &'static str {
        match self {
            SessionPhase::Starting => "启动中",
            SessionPhase::Running => "运行中",
            SessionPhase::Reconnecting => "重连中",
            SessionPhase::Stopped => "已停止",
            SessionPhase::Failed => "失败",
        }
    }

    /// 终态（不会再变化）。
    pub fn is_terminal(self) -> bool {
        matches!(self, SessionPhase::Stopped | SessionPhase::Failed)
    }

    /// 是否仍在工作。
    pub fn is_live(self) -> bool {
        !self.is_terminal()
    }
}

/// 会话 id（进程内单调递增，从 1 开始）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// 一次会话的启动参数：三种角色各一份（构造后由 `kind` 决定用哪份）。
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub(super) kind: SessionKind,
    pub(super) runtime: RuntimeConfig,
    pub(super) server: Option<ServerParams>,
    pub(super) client: Option<ClientParams>,
    pub(super) ssh: Option<SshParams>,
}

#[derive(Debug, Clone)]
pub(super) struct ServerParams {
    pub(super) listen: String,
    pub(super) token: String,
    pub(super) x11_port: u16,
}

#[derive(Debug, Clone)]
pub(super) struct ClientParams {
    pub(super) relay: String,
    pub(super) token: String,
    pub(super) x11: String,
    pub(super) display: Option<u16>,
    pub(super) auto: bool,
}

#[derive(Debug, Clone)]
pub(super) struct SshParams {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) user: Option<String>,
    pub(super) identity: Option<String>,
    pub(super) token: String,
    pub(super) relay_port: u16,
    pub(super) x11: String,
    pub(super) display: Option<u16>,
    pub(super) auto: bool,
    pub(super) reconnect: bool,
}

impl SessionSpec {
    /// relay 服务端会话。
    pub fn server(
        listen: impl Into<String>,
        token: impl Into<String>,
        x11_port: u16,
    ) -> SessionSpec {
        SessionSpec {
            kind: SessionKind::Server,
            runtime: RuntimeConfig::default(),
            server: Some(ServerParams {
                listen: listen.into(),
                token: token.into(),
                x11_port,
            }),
            client: None,
            ssh: None,
        }
    }

    /// 直连 relay 的客户端会话；`auto` 为真时由服务端自动分配 display。
    pub fn client(
        relay: impl Into<String>,
        token: impl Into<String>,
        x11: impl Into<String>,
        display: Option<u16>,
        auto: bool,
    ) -> SessionSpec {
        SessionSpec {
            kind: SessionKind::Client,
            runtime: RuntimeConfig::default(),
            server: None,
            client: Some(ClientParams {
                relay: relay.into(),
                token: token.into(),
                x11: x11.into(),
                display,
                auto,
            }),
            ssh: None,
        }
    }

    /// SSH 隧道会话（隧道内跑 relay 连接）。`reconnect` 见
    /// [`SessionSpec::with_ssh_reconnect`]，默认 true。
    #[allow(clippy::too_many_arguments)]
    pub fn ssh(
        host: impl Into<String>,
        port: u16,
        user: Option<String>,
        identity: Option<String>,
        token: impl Into<String>,
        relay_port: u16,
        x11: impl Into<String>,
        display: Option<u16>,
        auto: bool,
    ) -> SessionSpec {
        SessionSpec {
            kind: SessionKind::Ssh,
            runtime: RuntimeConfig::default(),
            server: None,
            client: None,
            ssh: Some(SshParams {
                host: host.into(),
                port,
                user,
                identity,
                token: token.into(),
                relay_port,
                x11: x11.into(),
                display,
                auto,
                reconnect: true,
            }),
        }
    }

    /// 会话运行时参数（`[runtime]` 段）。
    pub fn with_runtime(mut self, runtime: RuntimeConfig) -> SessionSpec {
        self.runtime = runtime;
        self
    }

    /// SSH 隧道中途断开后是否自动重建（对应 `[ssh] reconnect`，默认 true；
    /// false = 首次断开即失败退出，与 CLI `ssh.reconnect = false` 一致）。
    pub fn with_ssh_reconnect(mut self, reconnect: bool) -> SessionSpec {
        if let Some(ssh) = self.ssh.as_mut() {
            ssh.reconnect = reconnect;
        }
        self
    }

    pub fn kind(&self) -> SessionKind {
        self.kind
    }

    // ---- 从 GuiConfig 构造（默认值与 CLI 同源） ----

    /// 用 `[server]` 段构造，缺省值与 `rx11 server` 一致。
    pub fn server_from_config(cfg: &GuiConfig) -> Result<SessionSpec> {
        let sc = &cfg.server;
        let listen = sc
            .listen
            .clone()
            .unwrap_or_else(|| ServerDefaults::LISTEN_ADDR.to_string());
        let x11_port = sc.x11_port.unwrap_or(DEFAULT_X11_PORT);
        let token = required(&sc.token, "server.token", "服务端 token")?;
        Ok(SessionSpec::server(listen, token, x11_port).with_runtime(cfg.to_runtime()))
    }

    /// 用 `[client]` 段构造，缺省值与 `rx11 client` 一致。
    ///
    /// 与 CLI 的差别只有一处：CLI 的命令行 `--display` 与配置文件各有一份
    /// display，arg 优先；GUI 只有配置文件这一份，所以「填了 display 就固定、
    /// 留空就自动分配」就是这里的等价语义。
    pub fn client_from_config(cfg: &GuiConfig) -> Result<SessionSpec> {
        let cc = &cfg.client;
        let relay = required(&cc.relay, "client.relay", "中继服务器地址")?;
        let token = required(&cc.token, "client.token", "客户端 token")?;
        let x11 = cc
            .x11
            .clone()
            .unwrap_or_else(|| ClientDefaults::X11_ADDR.to_string());
        let display = cc.display;
        Ok(
            SessionSpec::client(relay, token, x11, display, display.is_none())
                .with_runtime(cfg.to_runtime()),
        )
    }

    /// 用 `[ssh]` 段构造，缺省值与 `rx11 ssh` 一致（含 `reconnect`）。
    pub fn ssh_from_config(cfg: &GuiConfig) -> Result<SessionSpec> {
        let sc = &cfg.ssh;
        let host = required(&sc.host, "ssh.host", "SSH 主机")?;
        let token = required(&sc.token, "ssh.token", "隧道 token")?;
        let x11 = sc
            .x11
            .clone()
            .unwrap_or_else(|| ClientDefaults::X11_ADDR.to_string());
        let display = sc.display;
        let spec = SessionSpec::ssh(
            host,
            sc.port.unwrap_or(SshDefaults::DEFAULT_PORT),
            sc.user.clone(),
            sc.identity.clone(),
            token,
            sc.relay_port.unwrap_or(DEFAULT_RELAY_PORT),
            x11,
            display,
            display.is_none(),
        )
        .with_runtime(cfg.to_runtime());
        Ok(spec.with_ssh_reconnect(sc.reconnect.unwrap_or(true)))
    }
}

fn required(value: &Option<String>, field: &str, what: &str) -> Result<String> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v.clone()),
        _ => Err(anyhow!("未配置 {field}（{what}）")),
    }
}

pub(super) fn require_non_empty(value: &str, field: &str, what: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(anyhow!("{field} 不能为空（{what}）"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 快照
// ---------------------------------------------------------------------------

/// 一次会话的只读快照，GUI 直接渲染。
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub kind: SessionKind,
    pub phase: SessionPhase,
    /// 一句话摘要（中文），如 `监听 127.0.0.1:7000 · 1 个客户端连接`。
    pub detail: String,
    pub display: Option<u16>,
    pub session_id: Option<String>,
    pub stats: Option<StatsTotals>,
    pub started_at: SystemTime,
    pub last_error: Option<String>,
}

impl SessionSnapshot {
    /// 是否仍在工作。
    pub fn is_live(&self) -> bool {
        self.phase.is_live()
    }
}

/// 把 supervise 的结束原因翻译成 GUI 的阶段 + 中文说明。
pub(super) fn ssh_conclusion(end: SupervisionEnd) -> (SessionPhase, String) {
    match end {
        SupervisionEnd::Session(Ok(())) => (SessionPhase::Stopped, "隧道已停止".to_string()),
        SupervisionEnd::Session(Err(e)) => (SessionPhase::Failed, format!("会话结束：{e}")),
        SupervisionEnd::TunnelDown(death) => (
            SessionPhase::Failed,
            format!("隧道断开（未启用自动重建）：{death}"),
        ),
        SupervisionEnd::RebuildExhausted {
            attempts,
            last_error,
        } => (
            SessionPhase::Failed,
            format!("隧道重建失败（已尝试 {attempts} 次）：{last_error}"),
        ),
    }
}

pub(super) fn map_connector_phase(phase: ConnectorPhase) -> SessionPhase {
    match phase {
        ConnectorPhase::Connecting | ConnectorPhase::Handshaking => SessionPhase::Starting,
        ConnectorPhase::Serving => SessionPhase::Running,
        ConnectorPhase::Reconnecting => SessionPhase::Reconnecting,
        ConnectorPhase::Stopped => SessionPhase::Stopped,
        ConnectorPhase::Failed => SessionPhase::Failed,
    }
}

pub(super) fn client_detail(relay: &str, cs: &ConnectorState, phase: SessionPhase) -> String {
    match phase {
        SessionPhase::Running => {
            let display = cs
                .display
                .map(|d| format!("display :{d}"))
                .unwrap_or_else(|| "display 未知".to_string());
            let compression = cs.compression.as_deref().unwrap_or("未压缩");
            format!("relay {relay} · {display} · {compression}")
        }
        SessionPhase::Reconnecting => {
            format!(
                "relay {relay} · 第 {} 次重连中…",
                cs.reconnect_attempt.max(1)
            )
        }
        SessionPhase::Failed => format!(
            "relay {relay} · {}",
            cs.last_error.as_deref().unwrap_or("连接失败")
        ),
        SessionPhase::Stopped => format!("relay {relay} · 已停止"),
        SessionPhase::Starting => format!("relay {relay} · 连接中…"),
    }
}

pub(super) fn ssh_detail(
    endpoint: &str,
    local_addr: &str,
    cs: &ConnectorState,
    phase: SessionPhase,
) -> String {
    let base = format!("{endpoint} → {local_addr}");
    match phase {
        SessionPhase::Running => {
            format!("{base} · {}", cs.compression.as_deref().unwrap_or("已连接"))
        }
        SessionPhase::Reconnecting => {
            format!(
                "{base} · 隧道重建中（第 {} 次连接尝试）",
                cs.reconnect_attempt.max(1)
            )
        }
        SessionPhase::Starting => format!("{base} · 连接中…"),
        SessionPhase::Stopped => format!("{base} · 已停止"),
        SessionPhase::Failed => format!(
            "{base} · {}",
            cs.last_error.as_deref().unwrap_or("隧道失败")
        ),
    }
}

pub(super) fn describe_ssh(params: &SshParams) -> String {
    match &params.user {
        Some(user) if !user.is_empty() => format!("{user}@{}:{}", params.host, params.port),
        _ => format!("{}:{}", params.host, params.port),
    }
}

// ---------------------------------------------------------------------------
// SSH 隧道辅助（与 rx11-cli 的 commands/ssh.rs 同语义）
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(super) struct TunnelSpec {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) user: Option<String>,
    pub(super) identity: Option<String>,
    pub(super) relay_port: u16,
    pub(super) local_bind_port: u16,
}

/// 固定 display 用 `17000 + display`，自动模式抓一个临时端口
/// （与 CLI 的 `pick_local_bind_port` 同规则）。
pub(super) fn pick_local_bind_port(auto_display: bool, display: u16) -> Result<u16> {
    if auto_display {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    } else {
        Ok(SshDefaults::LOCAL_BIND_BASE.saturating_add(display))
    }
}

/// 打开隧道并等本地端口就绪；ssh 在就绪前退出立即报错，不空等 READY_TIMEOUT。
pub(super) async fn open_tunnel(spec: &TunnelSpec) -> Result<SshTunnel> {
    let mut tunnel = SshTunnel::create(
        &spec.host,
        spec.port,
        spec.user.as_deref(),
        spec.relay_port,
        spec.local_bind_port,
        spec.identity.as_deref(),
    )
    .await?;
    let local = tunnel.local_addr().to_owned();
    let ready = tokio::select! {
        r = wait_for_port(&local, SshDefaults::READY_TIMEOUT) => r,
        status = tunnel.wait() => {
            let _ = tunnel.kill().await;
            let status = status?;
            Err(anyhow!(
                "ssh 在隧道就绪前退出：{status}（请检查主机、端口与密钥认证）"
            ))
        }
    };
    if let Err(e) = ready {
        let _ = tunnel.kill().await;
        return Err(e.context("SSH 隧道未能就绪"));
    }
    Ok(tunnel)
}

/// 轮询直到 `addr` 可连接（指数退避 50ms → 500ms，与 CLI 同规则）。
async fn wait_for_port(addr: &str, timeout: Duration) -> Result<()> {
    const INITIAL_DELAY: Duration = Duration::from_millis(50);
    let start = tokio::time::Instant::now();
    let mut delay = INITIAL_DELAY;
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(anyhow!("等待 {addr} 就绪超时（{timeout:?}）"));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(GuiDefaults::WAIT_PORT_MAX_DELAY);
    }
}

/// 判断一个路径是否是已存在的文件（GUI 里校验 `ssh.identity` 用；
/// `~` 先经 [`crate::settings::expand_tilde`] 展开）。
pub fn path_exists(path: &str) -> bool {
    Path::new(&crate::settings::expand_tilde(path)).is_file()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::RuntimeSection;

    #[test]
    fn spec_from_config_uses_cli_defaults() {
        let cfg = GuiConfig::default();
        assert!(SessionSpec::server_from_config(&cfg).is_err(), "缺 token");
        assert!(SessionSpec::client_from_config(&cfg).is_err());
        assert!(SessionSpec::ssh_from_config(&cfg).is_err());

        let mut cfg = GuiConfig::default();
        cfg.server.token = Some("tk".into());
        cfg.client.relay = Some("relay:7000".into());
        cfg.client.token = Some("tk".into());
        cfg.ssh.host = Some("jump".into());
        cfg.ssh.token = Some("tk".into());
        cfg.ssh.reconnect = Some(false);

        let server = SessionSpec::server_from_config(&cfg).unwrap();
        assert_eq!(server.kind(), SessionKind::Server);
        assert_eq!(cfg.runtime, RuntimeSection::default());
        let client = SessionSpec::client_from_config(&cfg).unwrap();
        assert_eq!(
            client.client.as_ref().unwrap().x11,
            ClientDefaults::X11_ADDR
        );
        assert!(
            client.client.as_ref().unwrap().auto,
            "没填 display 就自动分配"
        );
        let ssh = SessionSpec::ssh_from_config(&cfg).unwrap();
        assert_eq!(ssh.ssh.as_ref().unwrap().port, SshDefaults::DEFAULT_PORT);
        assert_eq!(ssh.ssh.as_ref().unwrap().relay_port, DEFAULT_RELAY_PORT);
        assert!(
            !ssh.ssh.as_ref().unwrap().reconnect,
            "reconnect=false 必须传下去"
        );

        let mut cfg = GuiConfig::default();
        cfg.client.relay = Some("relay:7000".into());
        cfg.client.token = Some("tk".into());
        cfg.client.display = Some(3);
        let client = SessionSpec::client_from_config(&cfg).unwrap();
        assert_eq!(client.client.as_ref().unwrap().display, Some(3));
        assert!(!client.client.as_ref().unwrap().auto, "填了 display 就固定");
    }

    #[test]
    fn ssh_local_bind_port_follows_display() {
        assert_eq!(
            pick_local_bind_port(false, 5).unwrap(),
            SshDefaults::LOCAL_BIND_BASE + 5
        );
        let auto = pick_local_bind_port(true, 5).unwrap();
        assert_ne!(auto, SshDefaults::LOCAL_BIND_BASE + 5);
        assert!(auto > 0);
    }

    #[test]
    fn phase_helpers_are_consistent() {
        assert!(SessionPhase::Stopped.is_terminal());
        assert!(SessionPhase::Failed.is_terminal());
        assert!(SessionPhase::Running.is_live());
        assert_eq!(SessionPhase::Reconnecting.label(), "重连中");
        assert_eq!(SessionKind::Ssh.label(), "SSH 隧道");
        assert_eq!(
            map_connector_phase(ConnectorPhase::Handshaking),
            SessionPhase::Starting
        );
        assert_eq!(
            map_connector_phase(ConnectorPhase::Serving),
            SessionPhase::Running
        );
        assert_eq!(
            map_connector_phase(ConnectorPhase::Failed),
            SessionPhase::Failed
        );
    }

    #[test]
    fn ssh_conclusion_maps_every_outcome() {
        assert_eq!(
            ssh_conclusion(SupervisionEnd::Session(Ok(()))).0,
            SessionPhase::Stopped
        );
        assert_eq!(
            ssh_conclusion(SupervisionEnd::Session(Err(anyhow!("x")))).0,
            SessionPhase::Failed
        );
        assert_eq!(
            ssh_conclusion(SupervisionEnd::TunnelDown(
                rx11_client::ssh::TunnelDeath::Unhealthy
            ))
            .0,
            SessionPhase::Failed
        );
        assert_eq!(
            ssh_conclusion(SupervisionEnd::RebuildExhausted {
                attempts: 3,
                last_error: "boom".into()
            })
            .0,
            SessionPhase::Failed
        );
    }
}
