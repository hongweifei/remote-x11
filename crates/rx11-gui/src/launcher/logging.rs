use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

use super::{lock, GuiDefaults};

// ---------------------------------------------------------------------------
// 日志环形缓冲
// ---------------------------------------------------------------------------

/// 一行日志。`seq` 单调递增且**永不重置**，GUI 只要记住上一次的 `seq`
/// 就能用 [`LogRing::since`] 做增量追加。
#[derive(Debug, Clone)]
pub struct LogLine {
    pub seq: u64,
    pub ts: SystemTime,
    /// 本地时间 `HH:MM:SS.mmm`（push 时算好，界面直接用，不必自己格式化）。
    pub time: String,
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
}

/// 本地时区相对 UTC 的偏移（秒）；进程内只探测一次，探测失败按 UTC（0）处理。
///
/// `std` 不提供本地时区，这里用一次 `date +%z`（coreutils，Linux 必备）取偏移；
/// 不为此引入 chrono/time 依赖。跨 DST 切换的长时间运行会沿用启动时的偏移。
fn local_offset_secs() -> i32 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i32> = OnceLock::new();
    *OFFSET.get_or_init(|| probe_local_offset().unwrap_or(0))
}

fn probe_local_offset() -> Option<i32> {
    let output = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    parse_utc_offset(text.trim())
}

/// 解析 `date +%z` 的输出（`+0800` / `-0530`）为秒数。
fn parse_utc_offset(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    if bytes.len() != 5 {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours: i32 = text[1..3].parse().ok()?;
    let minutes: i32 = text[3..5].parse().ok()?;
    (hours <= 23 && minutes <= 59).then_some(sign * (hours * 3600 + minutes * 60))
}

/// 把时间戳格式化成本地时间 `HH:MM:SS.mmm`（供日志之外的界面显示复用）。
pub fn format_time(ts: SystemTime) -> String {
    let since_epoch = ts.duration_since(SystemTime::UNIX_EPOCH).ok();
    let secs = since_epoch.map(|d| d.as_secs() as i64).unwrap_or(0);
    let millis = since_epoch.map(|d| d.subsec_millis()).unwrap_or(0);
    let local = (secs + local_offset_secs() as i64).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        local / 3600,
        (local % 3600) / 60,
        local % 60,
        millis
    )
}

/// 有界日志环形缓冲（`Arc<LogRing>` 共享，可跨线程）。
pub struct LogRing {
    capacity: usize,
    next_seq: AtomicU64,
    lines: Mutex<VecDeque<LogLine>>,
}

impl LogRing {
    /// 默认容量的环形缓冲。
    pub fn new() -> Arc<Self> {
        Self::with_capacity(GuiDefaults::LOG_RING_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            capacity,
            next_seq: AtomicU64::new(1),
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        lock(&self.lines).len()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.lines).is_empty()
    }

    /// 最后一条日志的序号（没有日志时为 0）。
    pub fn last_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst).saturating_sub(1)
    }

    /// 追加一行；返回它的序号。缓冲满时丢弃最旧的一行。
    pub fn push(&self, level: tracing::Level, target: &str, message: String) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let mut message = message;
        if message.len() > GuiDefaults::LOG_MESSAGE_MAX {
            message = message
                .chars()
                .take(GuiDefaults::LOG_MESSAGE_MAX)
                .collect::<String>();
            message.push('…');
        }
        let now = SystemTime::now();
        let line = LogLine {
            seq,
            ts: now,
            time: format_time(now),
            level,
            target: target.to_string(),
            message,
        };
        let mut lines = lock(&self.lines);
        while lines.len() >= self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
        seq
    }

    /// 取出 `seq` 之后的新日志（增量拉取：GUI 只追加，不重建列表）。
    ///
    /// 若 `seq` 比缓冲区里最旧的一行还旧（已经被挤出），返回当前保留的全部
    /// 行——调用方会丢掉这段缺口，但绝不会重复追加已消费过的行。
    pub fn since(&self, seq: u64) -> Vec<LogLine> {
        let lines = lock(&self.lines);
        lines
            .iter()
            .filter(|line| line.seq > seq)
            .cloned()
            .collect()
    }

    /// 清空缓冲。序号保持单调（`since` 语义不受影响）。
    pub fn clear(&self) {
        lock(&self.lines).clear();
    }

    /// 取一个把事件写入本环的 [`Layer`]，便于自行组装 subscriber。
    pub fn layer(ring: Arc<LogRing>) -> LogRingLayer {
        LogRingLayer { ring }
    }

    /// 见 [`subscriber`]。
    pub fn subscriber(ring: Arc<LogRing>) -> impl Subscriber + Send + Sync + 'static {
        subscriber(ring)
    }
}

impl Default for LogRing {
    fn default() -> Self {
        Self {
            capacity: GuiDefaults::LOG_RING_CAPACITY,
            next_seq: AtomicU64::new(1),
            lines: Mutex::new(VecDeque::with_capacity(256)),
        }
    }
}

/// 把 `tracing` 事件写进 [`LogRing`] 的 layer。
pub struct LogRingLayer {
    ring: Arc<LogRing>,
}

impl<S> Layer<S> for LogRingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.ring
            .push(*metadata.level(), metadata.target(), visitor.finish());
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            return self.message;
        }
        let extra: Vec<String> = self
            .fields
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if self.message.is_empty() {
            extra.join(" ")
        } else {
            format!("{} ({})", self.message, extra.join(", "))
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }
}

/// `tracing` subscriber：stderr 的 `fmt` layer + [`LogRing`] layer。
///
/// 过滤用 `RUST_LOG`（默认 `info`），两侧共用同一套过滤结果。
pub fn subscriber(ring: Arc<LogRing>) -> impl Subscriber + Send + Sync + 'static {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(GuiDefaults::LOG_FILTER));
    subscriber_with_filter(ring, filter)
}

/// 同 [`subscriber`]，但由调用方给定过滤器（测试用）。
pub fn subscriber_with_filter(
    ring: Arc<LogRing>,
    filter: EnvFilter,
) -> impl Subscriber + Send + Sync + 'static {
    Registry::default()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(LogRingLayer { ring })
}

/// 安装全局 subscriber（stderr + 日志环）。重复安装会返回错误而不是 panic。
pub fn init_logging(ring: Arc<LogRing>) -> Result<(), tracing_subscriber::util::TryInitError> {
    subscriber(ring).try_init()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_ring_is_bounded_and_drops_oldest() {
        let ring = LogRing::with_capacity(3);
        for i in 1..=5 {
            ring.push(tracing::Level::INFO, "t", format!("第 {i} 行"));
        }
        assert_eq!(ring.len(), 3);
        let lines = ring.since(0);
        assert_eq!(
            lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
            vec![3, 4, 5],
            "最旧的先被挤掉，序号保持单调"
        );
        assert_eq!(lines[2].message, "第 5 行");
    }

    #[test]
    fn log_ring_since_is_incremental() {
        let ring = LogRing::new();
        assert!(ring.since(0).is_empty());
        let first = ring.push(tracing::Level::WARN, "rx11", "一".to_string());
        ring.push(tracing::Level::ERROR, "rx11", "二".to_string());
        assert_eq!(ring.since(0).len(), 2);
        assert_eq!(ring.since(first).len(), 1);
        assert_eq!(ring.since(ring.last_seq()).len(), 0);
        // 缺口：请求的 seq 已被挤出时，返回当前保留的全部行
        let small = LogRing::with_capacity(1);
        small.push(tracing::Level::INFO, "t", "a".into());
        small.push(tracing::Level::INFO, "t", "b".into());
        assert_eq!(small.since(0).len(), 1);
    }

    #[test]
    fn log_ring_clear_keeps_sequence_monotonic() {
        let ring = LogRing::new();
        let seq = ring.push(tracing::Level::INFO, "t", "x".into());
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.since(0).len(), 0, "清空后 since(0) 不再回放旧行");
        let next = ring.push(tracing::Level::INFO, "t", "y".into());
        assert!(next > seq, "序号不得重置，否则 GUI 会重复追加");
    }

    #[test]
    fn log_ring_truncates_huge_messages() {
        let ring = LogRing::new();
        ring.push(tracing::Level::INFO, "t", "x".repeat(10_000));
        let line = &ring.since(0)[0];
        assert!(line.message.chars().count() <= GuiDefaults::LOG_MESSAGE_MAX + 1);
    }

    #[test]
    fn log_lines_carry_local_hms_time() {
        let ring = LogRing::new();
        ring.push(tracing::Level::INFO, "t", "带时间".into());
        let line = &ring.since(0)[0];
        assert_eq!(line.time, format_time(line.ts), "time 必须与 ts 一致");
        assert_eq!(line.time.len(), 12, "HH:MM:SS.mmm：{}", line.time);
        assert!(
            line.time.chars().enumerate().all(|(i, c)| match i {
                2 | 5 => c == ':',
                8 => c == '.',
                _ => c.is_ascii_digit(),
            }),
            "时间格式必须是 HH:MM:SS.mmm：{}",
            line.time
        );
    }

    #[test]
    fn utc_offset_parsing_handles_signs_and_garbage() {
        assert_eq!(parse_utc_offset("+0800"), Some(8 * 3600));
        assert_eq!(parse_utc_offset("-0530"), Some(-(5 * 3600 + 1800)));
        assert_eq!(parse_utc_offset("+0000"), Some(0));
        assert_eq!(parse_utc_offset("UTC"), None);
        assert_eq!(parse_utc_offset("+08"), None);
        assert_eq!(parse_utc_offset("+9900"), None, "小时数越界必须拒绝");
    }

    /// 验证 `init_logging` 真的能把事件装进环。
    ///
    /// 它会安装**全局** subscriber（只允许一次，且会设置全局 max-level hint），
    /// 所以必须单独跑，不能和其它基于 `with_default` 的日志测试同进程：
    ///
    /// ```text
    /// cargo test -p rx11-gui --lib -- --ignored --exact --test-threads=1 \
    ///     launcher::logging::tests::init_logging_installs_global_subscriber
    /// ```
    ///
    /// 别用 `find target/debug/deps -name 'rx11_gui-*' | head -1` 挑二进制：每次
    /// 构建都会留下旧的测试二进制，挑错了会报"没有这个测试"。
    #[test]
    #[ignore = "会安装全局 tracing subscriber，必须单独运行"]
    fn init_logging_installs_global_subscriber() {
        let ring = LogRing::new();
        init_logging(ring.clone()).expect("首次安装必须成功");
        tracing::info!(target: "rx11_gui::boot", "全局订阅者已安装");
        let lines = ring.since(0);
        assert!(
            lines.iter().any(|l| l.message.contains("全局订阅者已安装")),
            "{lines:?}"
        );
    }

    #[test]
    fn log_ring_layer_captures_events_with_fields() {
        let ring = LogRing::new();
        let sub = subscriber_with_filter(ring.clone(), EnvFilter::new("trace"));
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "rx11_gui::test", answer = 42, "你好 {}", "世界");
            tracing::debug!(target: "rx11_gui::test", "只有消息");
        });
        let lines = ring.since(0);
        assert_eq!(lines.len(), 2, "过滤为 trace 时两条都应进入环");
        assert_eq!(lines[0].level, tracing::Level::INFO);
        assert_eq!(lines[0].target, "rx11_gui::test");
        assert_eq!(lines[0].message, "你好 世界 (answer=42)");
        assert_eq!(lines[1].message, "只有消息");
    }
}
