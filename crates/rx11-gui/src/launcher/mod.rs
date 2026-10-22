//! rx11-gui 进程编排层：在 GUI 进程内**直接嵌入** relay 服务端 / 客户端 /
//! SSH 隧道（不 fork `rx11` 二进制），并把生命周期与统计暴露成随时可读的快照。
//!
//! 线程模型（关键约束）：
//! - `Launcher` 自持一个多线程 tokio runtime；GUI 线程只做「提交任务 + 读快照」。
//! - [`Launcher::snapshot`] 是**纯读锁**：只读进程内的 `Mutex` 与原子计数，
//!   不 `await`、不 `block_on`，GUI 可以每个重绘周期调用。
//! - 长任务（连接、重连退避、隧道重建、等待停止）全部在 runtime 上跑；
//!   唯一在 GUI 线程上短暂阻塞的是服务端本地 `bind`（微秒级，见
//!   [`Launcher::start`]）。
//! - 停止是优雅的：先请求停止，超过 [`GuiDefaults::STOP_GRACE`] 才放弃
//!   （SSH 会话会强制丢弃 `supervise` future，隧道由 ssh 子进程的
//!   `kill_on_drop` + 健康检查兜底回收）。
//!
//! 日志：`tracing` 事件同时进 stderr（`fmt` layer，终端调试用）和一个有界
//! 环形缓冲 [`LogRing`]；GUI 用 `since(seq)` 增量拉取，只追加不重建。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use rx11_client::connector::LocalConnector;
use rx11_client::ssh::{self, RebuildPolicy, TunnelHandle};

use record::{ServerExtras, SessionBody, SessionRecord, SessionState};
use session::{
    describe_ssh, open_tunnel, pick_local_bind_port, require_non_empty, ssh_conclusion, TunnelSpec,
};

use crate::settings::{default_config_path, GuiConfig};

mod logging;
mod record;
mod session;

pub use logging::{
    format_time, init_logging, subscriber, subscriber_with_filter, LogLine, LogRing, LogRingLayer,
};
pub use session::{
    path_exists, SessionId, SessionKind, SessionPhase, SessionSnapshot, SessionSpec,
};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 数据层常量（与 core 的 `*Defaults` 同风格，放在这里而不是散落在调用点）。
pub struct GuiDefaults;

impl GuiDefaults {
    /// 日志环形缓冲容量（行）。约 4000 行足够覆盖一次排障窗口。
    pub const LOG_RING_CAPACITY: usize = 4000;
    /// 单条日志的最大字节数，超出截断（防止一条巨型日志独占缓冲）。
    pub const LOG_MESSAGE_MAX: usize = 4096;
    /// 默认日志过滤级别（`RUST_LOG` 可覆盖）。
    pub const LOG_FILTER: &'static str = "info";
    /// 服务端会话/连接数快照的刷新间隔。
    pub const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
    /// 停止会话后等待其真正结束的宽限期，超过则放弃。
    pub const STOP_GRACE: Duration = Duration::from_secs(3);
    /// 等待 SSH 隧道本地端口就绪的轮询间隔（上限，指数退避）。
    pub const WAIT_PORT_MAX_DELAY: Duration = Duration::from_millis(500);
    /// tokio worker 线程数：会话编排是 IO 密集，2 个足够且不抢 GUI 的 CPU。
    pub const WORKER_THREADS: usize = 2;
}

pub(super) fn lock<'a, T>(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Launcher
// ---------------------------------------------------------------------------

/// 会话编排器：持有 tokio runtime、会话表和日志环。
pub struct Launcher {
    rt: tokio::runtime::Runtime,
    inner: Mutex<Vec<SessionRecord>>,
    next_id: AtomicU64,
    log: Arc<LogRing>,
    config_path: PathBuf,
}

impl Launcher {
    /// 新建编排器（自持多线程 runtime + 默认容量的日志环）。
    pub fn new() -> Launcher {
        Launcher::with_log_ring(LogRing::new())
    }

    /// 复用调用方已有的日志环（`init_logging(ring)` 与 GUI 共用一个环）。
    pub fn with_log_ring(log: Arc<LogRing>) -> Launcher {
        Launcher::with_parts(log, default_config_path())
    }

    /// 指定配置文件路径（状态栏显示 + [`Launcher::config_path`]），测试用。
    pub fn with_config_path(path: impl Into<PathBuf>) -> Launcher {
        Launcher::with_parts(LogRing::new(), path.into())
    }

    fn with_parts(log: Arc<LogRing>, config_path: PathBuf) -> Launcher {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(GuiDefaults::WORKER_THREADS)
            .thread_name("rx11-gui-rt")
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败");
        Launcher {
            rt,
            inner: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            log,
            config_path,
        }
    }

    /// GUI 与日志共用的日志环。
    pub fn log_ring(&self) -> Arc<LogRing> {
        self.log.clone()
    }

    /// 配置文件路径（状态栏显示）。
    pub fn config_path(&self) -> PathBuf {
        self.config_path.clone()
    }

    /// 读取配置文件（文件不存在时为全默认）。
    pub fn load_config(&self) -> Result<GuiConfig, crate::settings::ConfigError> {
        GuiConfig::load(&self.config_path)
    }

    /// 写回配置文件（保注释/保键序/保留未识别键）。
    pub fn save_config(&self, cfg: &GuiConfig) -> Result<(), crate::settings::ConfigError> {
        cfg.save(&self.config_path)
    }

    /// 启动一个会话，返回它的 id。
    ///
    /// 服务端会话会**短暂**阻塞当前线程直到本地 `bind` 完成（微秒级），
    /// 这样端口占用之类的错误能立刻以 `Err` 返回；其余角色只提交任务。
    /// 客户端/隧道会话的失败（连不上、ssh 起不来、隧道重建耗尽）不在这里
    /// 返回，而是体现在 [`SessionSnapshot::phase`] / `last_error` 上。
    ///
    /// 注意：不要从 tokio 任务里调用（服务端分支会 `block_on`）。
    pub fn start(&self, spec: SessionSpec) -> Result<SessionId> {
        let id = SessionId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let started_at = SystemTime::now();
        let kind = spec.kind;
        let record = match kind {
            SessionKind::Server => self.start_server(id, started_at, spec)?,
            SessionKind::Client => self.start_client(id, started_at, spec)?,
            SessionKind::Ssh => self.start_ssh(id, started_at, spec)?,
        };
        let detail = record.detail();
        lock(&self.inner).push(record);
        tracing::info!("启动{}会话 #{}：{}", kind.label(), id.0, detail);
        Ok(id)
    }

    /// 停止一个会话（幂等；未知 id 无操作）。记录保留到 [`Launcher::remove`]。
    pub fn stop(&self, id: SessionId) {
        let force = {
            let inner = lock(&self.inner);
            let Some(record) = inner.iter().find(|r| r.id == id) else {
                return;
            };
            record.request_stop()
        };
        if let Some(force) = force {
            // 宽限期后再放弃：先让 handle.stop() 走优雅路径。
            std::thread::spawn(move || {
                std::thread::sleep(GuiDefaults::STOP_GRACE);
                let _ = force.send(());
            });
        }
        tracing::info!("停止会话 #{}", id.0);
    }

    /// 从列表里移除一个会话（仍在运行的会先被停止）。
    pub fn remove(&self, id: SessionId) {
        let live = {
            let inner = lock(&self.inner);
            inner
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.is_live())
                .unwrap_or(false)
        };
        if live {
            self.stop(id);
        }
        lock(&self.inner).retain(|r| r.id != id);
    }

    /// 停止并移除全部会话（窗口关闭时调用）。
    pub fn stop_all(&self) {
        let ids: Vec<SessionId> = lock(&self.inner).iter().map(|r| r.id).collect();
        for id in ids {
            self.stop(id);
        }
    }

    /// 当前全部会话的快照，按 id 升序。
    ///
    /// 纯读锁 + 原子读，不阻塞 GUI 线程。
    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        let inner = lock(&self.inner);
        let mut out: Vec<SessionSnapshot> = inner.iter().map(SessionRecord::snapshot).collect();
        out.sort_by_key(|s| s.id);
        out
    }

    /// 会话数量。
    pub fn session_count(&self) -> usize {
        lock(&self.inner).len()
    }

    /// 服务端会话**实际绑定**的地址（`127.0.0.1:0` 会解析成真实端口）；
    /// 非服务端会话或未知 id 返回 `None`。GUI 用它显示/复制真实监听地址。
    pub fn local_addr(&self, id: SessionId) -> Option<std::net::SocketAddr> {
        let inner = lock(&self.inner);
        let record = inner.iter().find(|r| r.id == id)?;
        match &record.body {
            SessionBody::Server { handle, .. } => Some(handle.local_addr()),
            _ => None,
        }
    }

    // ---- 各角色的启动实现 ----

    fn start_server(
        &self,
        id: SessionId,
        started_at: SystemTime,
        spec: SessionSpec,
    ) -> Result<SessionRecord> {
        let params = spec.server.clone().expect("server spec 必带参数");
        require_non_empty(&params.listen, "server.listen", "监听地址")?;
        require_non_empty(&params.token, "server.token", "服务端 token")?;

        let server = rx11_server::relay::RelayServer::with_config(
            params.listen.clone(),
            params.token.clone(),
            params.x11_port,
            spec.runtime.clone(),
        );
        // bind 在 start() 里同步完成，所以这里的短暂 block_on 能拿到真实错误
        // （端口占用等），而不是留下一个「后台才失败」的会话。
        let handle = self
            .rt
            .block_on(server.start())
            .map_err(|e| anyhow!("服务端启动失败：{e}"))?;

        let extras = Arc::new(Mutex::new(ServerExtras::default()));
        let state = Arc::new(Mutex::new(SessionState::default()));
        // 后台刷新会话列表（SessionManager::snapshot 是 async，不能在
        // snapshot() 里 await）；停止由 wait_stopped() 单独标记终态。
        let refresh_handle = handle.clone();
        let mgr = handle.session_manager();
        let refresh_extras = extras.clone();
        self.rt.spawn(async move {
            while refresh_handle.is_running() {
                let sessions = mgr.snapshot().await;
                lock(&refresh_extras).sessions = sessions;
                tokio::time::sleep(GuiDefaults::REFRESH_INTERVAL).await;
            }
        });
        let stopped_handle = handle.clone();
        let stopped_state = state.clone();
        self.rt.spawn(async move {
            stopped_handle.wait_stopped().await;
            lock(&stopped_state).terminal = Some((SessionPhase::Stopped, "已停止".to_string()));
        });

        Ok(SessionRecord {
            id,
            kind: SessionKind::Server,
            started_at,
            state,
            body: SessionBody::Server { handle, extras },
        })
    }

    fn start_client(
        &self,
        id: SessionId,
        started_at: SystemTime,
        spec: SessionSpec,
    ) -> Result<SessionRecord> {
        let params = spec.client.clone().expect("client spec 必带参数");
        require_non_empty(&params.relay, "client.relay", "中继服务器地址")?;
        require_non_empty(&params.token, "client.token", "客户端 token")?;

        let connector = Arc::new(
            LocalConnector::new(
                params.relay.clone(),
                params.token.clone(),
                params.x11.clone(),
                params.display,
                params.auto,
            )
            .with_runtime(spec.runtime.clone()),
        );
        // spawn() 需要 runtime 上下文；enter() 不阻塞、不启动任务。
        let handle = {
            let _guard = self.rt.enter();
            connector.clone().spawn()
        };
        Ok(SessionRecord {
            id,
            kind: SessionKind::Client,
            started_at,
            state: Arc::new(Mutex::new(SessionState::default())),
            body: SessionBody::Client {
                handle,
                connector,
                relay: params.relay,
            },
        })
    }

    fn start_ssh(
        &self,
        id: SessionId,
        started_at: SystemTime,
        spec: SessionSpec,
    ) -> Result<SessionRecord> {
        let params = spec.ssh.clone().expect("ssh spec 必带参数");
        require_non_empty(&params.host, "ssh.host", "SSH 主机")?;
        require_non_empty(&params.token, "ssh.token", "隧道 token")?;

        let local_bind_port = pick_local_bind_port(params.auto, params.display.unwrap_or(0))
            .map_err(|e| anyhow!("分配 SSH 隧道本地端口失败：{e}"))?;
        let tunnel_spec = TunnelSpec {
            host: params.host.clone(),
            port: params.port,
            user: params.user.clone(),
            identity: params.identity.clone(),
            relay_port: params.relay_port,
            local_bind_port,
        };
        let endpoint = describe_ssh(&params);
        let local_addr = format!("127.0.0.1:{local_bind_port}");

        let connector = Arc::new(
            LocalConnector::new(
                local_addr.clone(),
                params.token.clone(),
                params.x11.clone(),
                params.display,
                params.auto,
            )
            .with_runtime(spec.runtime.clone()),
        );
        let handle = connector.handle();

        let state = Arc::new(Mutex::new(SessionState {
            detail: format!("{endpoint} → {local_addr} · 建立隧道中…"),
            ..SessionState::default()
        }));

        let (force_tx, force_rx) = tokio::sync::oneshot::channel::<()>();
        let task_state = state.clone();
        let task_endpoint = endpoint.clone();
        let task_local_addr = local_addr.clone();
        let auto_reconnect = params.reconnect;
        let task_connector = connector.clone();
        self.rt.spawn(async move {
            // 首个隧道严格打开（配置级错误立刻暴露），与 CLI 的 open_tunnel 一致。
            let tunnel = match open_tunnel(&tunnel_spec).await {
                Ok(t) => t,
                Err(e) => {
                    let msg = format!("SSH 隧道建立失败：{e:#}");
                    tracing::error!("{msg}");
                    let mut st = lock(&task_state);
                    st.terminal = Some((SessionPhase::Failed, msg));
                    return;
                }
            };
            tracing::info!("SSH 隧道就绪：{task_endpoint} → {task_local_addr}");
            {
                let mut st = lock(&task_state);
                st.detail = format!("{task_endpoint} → {task_local_addr}");
            }

            let rebuild_spec = tunnel_spec.clone();
            let rebuild = move || {
                let spec = rebuild_spec.clone();
                async move {
                    let tunnel = open_tunnel(&spec).await?;
                    Ok(Box::new(tunnel) as Box<dyn TunnelHandle>)
                }
            };
            let supervised = ssh::supervise(
                Box::new(tunnel),
                task_connector.connect_and_serve(),
                rebuild,
                RebuildPolicy::default(),
                auto_reconnect,
            );
            tokio::pin!(supervised);
            let mut force_rx = force_rx;
            let conclusion = tokio::select! {
                end = &mut supervised => ssh_conclusion(end),
                _ = &mut force_rx => (
                    SessionPhase::Failed,
                    format!("停止超时（{:?}），已放弃 SSH 隧道", GuiDefaults::STOP_GRACE),
                ),
            };
            let mut st = lock(&task_state);
            st.terminal = Some(conclusion);
        });

        Ok(SessionRecord {
            id,
            kind: SessionKind::Ssh,
            started_at,
            state,
            body: SessionBody::Ssh {
                handle,
                connector,
                endpoint,
                local_addr,
                force: Mutex::new(Some(force_tx)),
            },
        })
    }
}

impl Default for Launcher {
    fn default() -> Launcher {
        Launcher::new()
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        // 尽力停止：runtime 随后被 drop 会取消任务，这里只负责发停止信号。
        for record in lock(&self.inner).iter() {
            record.request_stop();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    fn poll_until<F: Fn(&[SessionSnapshot]) -> bool>(
        launcher: &Launcher,
        what: &str,
        pred: F,
    ) -> Vec<SessionSnapshot> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let snap = launcher.snapshot();
            if pred(&snap) {
                return snap;
            }
            if std::time::Instant::now() > deadline {
                panic!("等待「{what}」超时，最后快照：{snap:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn server_session_runs_then_stops() {
        let dir = std::env::temp_dir().join(format!("rx11-gui-launch-{}", std::process::id()));
        let launcher = Launcher::with_config_path(dir.join("config.toml"));
        let id = launcher
            .start(SessionSpec::server("127.0.0.1:0", "tk", 6100))
            .unwrap();
        let snap = poll_until(&launcher, "服务端运行", |s| {
            s.first().is_some_and(|s| s.phase == SessionPhase::Running)
        });
        assert_eq!(snap[0].kind, SessionKind::Server);
        assert!(
            snap[0].detail.contains("监听 127.0.0.1:"),
            "{}",
            snap[0].detail
        );
        assert!(
            !snap[0].detail.contains(":0 "),
            "必须显示真实绑定的端口：{}",
            snap[0].detail
        );
        assert!(snap[0].stats.is_some());
        assert_eq!(snap[0].display, None);

        launcher.stop(id);
        poll_until(&launcher, "服务端停止", |s| {
            s.first().is_some_and(|s| s.phase == SessionPhase::Stopped)
        });
        launcher.remove(id);
        assert_eq!(launcher.session_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_bind_conflict_is_reported_synchronously() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let launcher = Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-test.toml"));
        let err = launcher
            .start(SessionSpec::server(addr.to_string(), "tk", 6100))
            .expect_err("端口被占用时必须立刻返回错误");
        assert!(err.to_string().contains("服务端启动失败"), "{err}");
        assert_eq!(launcher.session_count(), 0, "失败的启动不留下会话记录");
    }

    #[test]
    fn client_reconnects_then_stops() {
        let launcher = Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-test.toml"));
        let port = closed_port();
        let id = launcher
            .start(SessionSpec::client(
                format!("127.0.0.1:{port}"),
                "tk",
                "127.0.0.1:6000",
                None,
                true,
            ))
            .unwrap();
        let snap = poll_until(&launcher, "客户端进入重连或失败", |s| {
            s.first().is_some_and(|s| {
                s.phase == SessionPhase::Reconnecting || s.phase == SessionPhase::Failed
            })
        });
        assert_eq!(snap[0].kind, SessionKind::Client);
        assert!(snap[0].detail.contains("127.0.0.1"), "{}", snap[0].detail);
        assert!(snap[0].stats.is_some(), "客户端也要有统计（即使为 0）");

        launcher.stop(id);
        poll_until(&launcher, "客户端停止", |s| {
            s.first().is_some_and(|s| s.phase == SessionPhase::Stopped)
        });
        launcher.stop(id); // 幂等
    }

    #[test]
    fn start_rejects_incomplete_specs() {
        let launcher = Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-test.toml"));
        assert!(launcher
            .start(SessionSpec::server("127.0.0.1:0", "  ", 6100))
            .is_err());
        assert!(launcher
            .start(SessionSpec::client(
                "127.0.0.1:1",
                "",
                "127.0.0.1:6000",
                None,
                true
            ))
            .is_err());
        assert!(launcher
            .start(SessionSpec::ssh(
                "  ",
                22,
                None,
                None,
                "tk",
                7000,
                "127.0.0.1:6000",
                None,
                true
            ))
            .is_err());
        assert_eq!(launcher.session_count(), 0);
    }

    #[test]
    fn stop_and_remove_unknown_ids_are_noops() {
        let launcher = Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-test.toml"));
        launcher.stop(SessionId(999));
        launcher.remove(SessionId(999));
        assert_eq!(launcher.session_count(), 0);
    }

    #[test]
    fn snapshots_track_ids_and_sort_order() {
        let launcher = Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-test.toml"));
        let a = launcher
            .start(SessionSpec::server("127.0.0.1:0", "tk", 6100))
            .unwrap();
        let b = launcher
            .start(SessionSpec::server("127.0.0.1:0", "tk", 6100))
            .unwrap();
        assert!(b.0 > a.0);
        let snap = launcher.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap[0].id <= snap[1].id);
        assert_eq!(
            launcher.config_path().file_name().unwrap(),
            "rx11-gui-test.toml"
        );
        let with_dir =
            Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-x/config.toml"));
        assert!(with_dir.config_path().ends_with("rx11-gui-x/config.toml"));
        launcher.stop_all();
        let _ = launcher.snapshot();
    }

    /// 端到端冒烟：Launcher 起的服务端 + 客户端真正握手到「运行中」，
    /// 服务端快照能看到 1 个客户端连接与 1 个会话。
    ///
    /// **默认 ignore**：建会话时服务端会调用 `xauth`（写
    /// `$XAUTHORITY` / `~/.Xauthority`），必须用隔离 HOME 跑，绝不碰真实 home：
    ///
    /// ```text
    /// HB=$(mktemp -d)
    /// BIN=$(find target/debug/deps -maxdepth 1 -type f -executable -name 'rx11_gui-*' | head -1)
    /// HOME=$HB "$BIN" --ignored --nocapture launcher_end_to_end_server_and_client
    /// ```
    #[test]
    #[ignore = "会调用 xauth 写 $HOME/.Xauthority，需要隔离 HOME 后才能跑"]
    fn launcher_end_to_end_server_and_client() {
        let launcher = Launcher::with_config_path(std::env::temp_dir().join("rx11-gui-e2e.toml"));
        let server_id = launcher
            .start(SessionSpec::server("127.0.0.1:0", "tk", 61500))
            .unwrap();
        let addr = launcher
            .local_addr(server_id)
            .expect("服务端必须给出真实绑定地址");
        assert_ne!(addr.port(), 0, "127.0.0.1:0 必须被解析成真实端口");

        let client_id = launcher
            .start(SessionSpec::client(
                addr.to_string(),
                "tk",
                "127.0.0.1:6000",
                None,
                true,
            ))
            .unwrap();
        let snap = poll_until(&launcher, "客户端进入运行中", |s| {
            s.iter()
                .any(|x| x.id == client_id && x.phase == SessionPhase::Running)
        });
        let client = snap.iter().find(|x| x.id == client_id).unwrap();
        assert!(client.detail.contains("display :"), "{}", client.detail);
        assert!(client.stats.is_some());

        poll_until(&launcher, "服务端看到 1 个客户端连接", |s| {
            s.iter()
                .any(|x| x.id == server_id && x.detail.contains("1 个客户端连接"))
        });
        poll_until(&launcher, "服务端看到 1 个会话", |s| {
            s.iter()
                .any(|x| x.id == server_id && x.detail.contains("1 个会话"))
        });
        let snap = launcher.snapshot();
        let server = snap.iter().find(|x| x.id == server_id).unwrap();
        assert_eq!(server.display, Some(0), "单会话时服务端快照应带 display");
        assert!(server.session_id.is_some());

        launcher.stop(client_id);
        poll_until(&launcher, "客户端停止", |s| {
            s.iter()
                .any(|x| x.id == client_id && x.phase == SessionPhase::Stopped)
        });
        launcher.stop(server_id);
        poll_until(&launcher, "服务端停止", |s| {
            s.iter()
                .any(|x| x.id == server_id && x.phase == SessionPhase::Stopped)
        });
        launcher.stop_all();
    }
}
