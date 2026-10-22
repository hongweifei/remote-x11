//! 跨层契约：GUI 外壳与数据层之间交换的东西。
//!
//! 这一层是**最底层的共同语言**，不允许依赖本 crate 的任何其他模块（尤其不
//! 能碰 fltk、`theme`、`ui`）：
//!
//! * [`Role`] / [`Health`] / [`LogLevel`]：两端都在用的词汇。颜色属于表现层，
//!   所以 `color()` 不在这里，而在使用它们的页面里（见 `ui/pages/*`）。
//! * [`SessionRowData`] / [`TrafficTotals`] / [`LogRecord`]：数据层交给外壳的
//!   快照。
//! * [`StartRequest`]：外壳交给数据层的请求。
//! * [`Backend`]：端口。实现有两个 —— 真实数据层 `crate::backend::LauncherBackend`
//!   和样例数据 `crate::demo::DemoBackend`（`RX11_GUI_DEMO=1`）。
//!
//! 端口两侧都只认扁平 `key → string` 值，所以界面不需要知道配置文件的 schema，
//! 数据层也不需要知道界面长什么样。新增一个运行时开关的代价是：一个
//! `FieldSpec`（`ui/pages/config.rs`）+ 一行配置层代码，中间没有任何东西要同步改。

/// Which end of the link a session represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Server,
    Client,
    Ssh,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Server => "服务端",
            Role::Client => "客户端",
            Role::Ssh => "SSH 隧道",
        }
    }
}

/// Health of one session as rendered by the status chip.
///
/// Deliberately not the launcher's own phase enum: the widget layer should not
/// move every time a backend state is renamed. The adapter maps
/// `SessionPhase → Health` (see `crate::backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Health {
    Running,
    Starting,
    Reconnecting,
    #[default]
    Stopped,
    Failed,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::Running => "运行中",
            Health::Starting => "启动中",
            Health::Reconnecting => "重连中",
            Health::Stopped => "已停止",
            Health::Failed => "失败",
        }
    }
}

/// Severity of one displayed log line.
///
/// Mapped from `tracing::Level` by the adapter so the UI layer stays
/// independent of the logging facade. The variant order is load-bearing:
/// `LogView::counts` indexes a `[usize; 5]` with `level as usize` and the level
/// filter compares with `<=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Error => "错误",
            LogLevel::Warn => "警告",
            LogLevel::Info => "信息",
            LogLevel::Debug => "调试",
            LogLevel::Trace => "跟踪",
        }
    }

    /// Three-letter code for the left gutter. Kept as letters rather than a
    /// colored dot because a log line is read left-to-right and the level is
    /// the first thing that has to be legible in a dense column.
    pub fn code(self) -> &'static str {
        match self {
            LogLevel::Error => "ERR",
            LogLevel::Warn => "WRN",
            LogLevel::Info => "INF",
            LogLevel::Debug => "DBG",
            LogLevel::Trace => "TRC",
        }
    }
}

/// One session, as the UI needs to see it.
#[derive(Debug, Clone, Default)]
pub struct SessionRowData {
    pub id: u64,
    pub role: Role,
    pub health: Health,
    pub headline: String,
    pub detail: String,
    pub sent: Option<u64>,
    pub received: Option<u64>,
    pub uptime: Option<u64>,
}

/// Aggregate traffic figures for the summary strip and the scope.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrafficTotals {
    pub sent: u64,
    pub received: u64,
    /// Bytes saved by compression and diffing.
    pub saved: u64,
    /// Bytes that would have crossed the wire without those savings.
    pub raw: u64,
    pub channels: u32,
    pub uptime: u64,
}

impl TrafficTotals {
    pub fn savings_ratio(&self) -> f64 {
        let total = self.saved + self.raw;
        if total == 0 {
            0.0
        } else {
            self.saved as f64 / total as f64
        }
    }
}

/// One log line, already formatted for display.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub seq: u64,
    pub time: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

/// What a page wants started. Flat fields, because the UI already has them
/// as strings and the config layer knows how to interpret them.
#[derive(Debug, Clone, Default)]
pub struct StartRequest {
    pub role: Role,
    /// Field values from the page that asked, plus whatever the config file
    /// supplies for fields the page does not show.
    pub values: Vec<(String, String)>,
}

/// The data layer. Two implementations: the real one in `crate::backend`,
/// and a fixture one used for UI previews (`RX11_GUI_DEMO=1`).
pub trait Backend {
    fn config_path(&self) -> String;
    fn load_values(&self) -> Result<Vec<(String, String)>, String>;
    fn save_values(&self, values: &[(String, String)]) -> Result<(), String>;
    fn validate(&self, values: &[(String, String)]) -> Vec<(String, String)>;
    fn start(&self, req: StartRequest) -> Result<u64, String>;
    fn stop(&self, id: u64);
    fn sessions(&self) -> Vec<SessionRowData>;
    fn totals(&self) -> TrafficTotals;
    fn logs_since(&self, seq: u64) -> Vec<LogRecord>;
    fn clear_logs(&self);
    /// Default token for a role, so the console can show what will be used.
    fn token_hint(&self, values: &[(String, String)]) -> String;
    /// Release everything (called before the window closes).
    fn shutdown(&self);
}
