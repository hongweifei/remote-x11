//! 扁平键值视图（给声明式表单引擎用）：TOML 路径 ↔ 字符串。
//!
//! 所有键名的唯一来源：`flat_values` / `apply_flat` / `save` 都走 [`Section`] 这一层。

use toml_edit::{Table, Value};

use super::io::{set_argv, set_bool, set_float, set_int, set_str, set_usize};
use super::model::{ClientSection, GuiConfig, RuntimeSection, ServerSection, SshSection};
use super::validate::ValidationIssue;

/// 每个配置段：字段名列表 + 字符串化读写 + 写回 TOML。
///
/// 所有键名的唯一来源；`flat_values` / `apply_flat` / `save` 都走这一层，
/// 键名不会在别处再写一遍。
pub(super) trait Section: Default {
    const NAME: &'static str;
    const FIELDS: &'static [&'static str];

    /// 已填写字段的字符串形式（`None` = 未填写）。
    fn get(&self, field: &str) -> Option<String>;
    /// 写入一个字段；`raw` 为空串表示清空（置 `None`）。
    /// 返回 `false` 表示非空但解析失败，字段保持不变。
    fn set(&mut self, field: &str, raw: &str) -> bool;
    /// 把已填写的字段写进 TOML 表（`None` 的键删除）。
    fn write_into(&self, table: &mut Table);

    fn is_unset(&self) -> bool {
        Self::FIELDS.iter().all(|f| self.get(f).is_none())
    }
}

/// 全部配置键（TOML 路径形式），顺序固定：client → server → ssh → runtime。
///
/// 表单引擎可按这个顺序布局；键名与 `flat_values` / `apply_flat` 完全一致。
pub fn all_field_keys() -> Vec<String> {
    let mut out = Vec::new();
    collect_keys::<ClientSection>(&mut out);
    collect_keys::<ServerSection>(&mut out);
    collect_keys::<SshSection>(&mut out);
    collect_keys::<RuntimeSection>(&mut out);
    out
}

fn collect_keys<S: Section>(out: &mut Vec<String>) {
    for f in S::FIELDS {
        out.push(format!("{}.{}", S::NAME, f));
    }
}

fn section_values<S: Section>(out: &mut Vec<(String, String)>, section: &S) {
    for f in S::FIELDS {
        if let Some(v) = section.get(f) {
            out.push((format!("{}.{}", S::NAME, f), v));
        }
    }
}

/// 只输出「已填写」字段的 `(TOML 键路径, 字符串值)` 列表。
///
/// 值的字符串形式就是 TOML 字面量的形式：布尔为 `true`/`false`，整数为十进制，
/// `diff_min_savings_ratio` 为浮点，`audio_*_command` 为 TOML 数组字面量
/// （如 `["parec", "--raw"]`）。
pub fn flat_values(cfg: &GuiConfig) -> Vec<(String, String)> {
    let mut out = Vec::new();
    section_values(&mut out, &cfg.client);
    section_values(&mut out, &cfg.server);
    section_values(&mut out, &cfg.ssh);
    section_values(&mut out, &cfg.runtime);
    out
}

/// 用扁平键值回填配置：空字符串 = 该字段置 `None`；未识别的键忽略；
/// 非空但无法解析的值**保持不变**（返回值见 [`apply_flat_checked`]）。
pub fn apply_flat(cfg: &mut GuiConfig, values: &[(String, String)]) {
    let _ = apply_flat_checked(cfg, values);
}

/// 同 [`apply_flat`]，但把「非空却解析失败」的键作为校验问题返回，供 UI 标红。
pub fn apply_flat_checked(
    cfg: &mut GuiConfig,
    values: &[(String, String)],
) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    for (key, raw) in values {
        let Some((section, field)) = key.split_once('.') else {
            continue;
        };
        let ok = match section {
            ClientSection::NAME => apply_one::<ClientSection>(&mut cfg.client, field, raw),
            ServerSection::NAME => apply_one::<ServerSection>(&mut cfg.server, field, raw),
            SshSection::NAME => apply_one::<SshSection>(&mut cfg.ssh, field, raw),
            RuntimeSection::NAME => apply_one::<RuntimeSection>(&mut cfg.runtime, field, raw),
            _ => continue,
        };
        if !ok {
            issues.push(ValidationIssue::new(
                key,
                format!("无法解析为合法取值（当前 \"{raw}\"）"),
            ));
        }
    }
    issues
}

fn apply_one<S: Section>(section: &mut S, field: &str, raw: &str) -> bool {
    if !S::FIELDS.contains(&field) {
        return true; // 未识别的键：忽略，不算失败
    }
    section.set(field, raw)
}

// ---------------------------------------------------------------------------
// 各段的字符串化读写（键名的唯一来源）
// ---------------------------------------------------------------------------

fn parse_string(raw: &str) -> Option<String> {
    Some(raw.to_string())
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

macro_rules! parse_num {
    ($name:ident, $ty:ty) => {
        fn $name(raw: &str) -> Option<$ty> {
            raw.trim().parse::<$ty>().ok()
        }
    };
}

parse_num!(parse_u8, u8);
parse_num!(parse_u16, u16);
parse_num!(parse_u32, u32);
parse_num!(parse_u64, u64);
parse_num!(parse_usize, usize);

fn parse_f64(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok()
}

/// 命令行参数数组：`["parec", "--raw"]` 形式按 TOML 数组解析；
/// 不带方括号时按空白切分（方便手输）。
fn parse_argv(raw: &str) -> Option<Vec<String>> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') {
        match trimmed.parse::<Value>() {
            Ok(Value::Array(arr)) => arr
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>(),
            _ => None,
        }
    } else {
        Some(trimmed.split_whitespace().map(str::to_string).collect())
    }
}

/// 与 [`parse_argv`] 互逆：输出规范的 TOML 数组字面量。
fn format_argv(argv: &[String]) -> String {
    let mut out = String::from("[");
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        for c in arg.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

/// 统一的「非空才写入、空串清空」入口。
fn with_parsed<T>(slot: &mut Option<T>, raw: &str, parse: fn(&str) -> Option<T>) -> bool {
    if raw.trim().is_empty() {
        *slot = None;
        return true;
    }
    match parse(raw) {
        Some(v) => {
            *slot = Some(v);
            true
        }
        None => false,
    }
}

macro_rules! opt_field {
    ($self:ident, $field:ident, $raw:ident, $parse:ident) => {
        with_parsed(&mut $self.$field, $raw, $parse)
    };
}

impl Section for ClientSection {
    const NAME: &'static str = "client";
    const FIELDS: &'static [&'static str] = &["relay", "token", "x11", "display"];

    fn get(&self, field: &str) -> Option<String> {
        match field {
            "relay" => self.relay.clone(),
            "token" => self.token.clone(),
            "x11" => self.x11.clone(),
            "display" => self.display.map(|v| v.to_string()),
            _ => None,
        }
    }

    fn set(&mut self, field: &str, raw: &str) -> bool {
        match field {
            "relay" => opt_field!(self, relay, raw, parse_string),
            "token" => opt_field!(self, token, raw, parse_string),
            "x11" => opt_field!(self, x11, raw, parse_string),
            "display" => opt_field!(self, display, raw, parse_u16),
            _ => true,
        }
    }

    fn write_into(&self, table: &mut Table) {
        set_str(table, "relay", self.relay.as_deref());
        set_str(table, "token", self.token.as_deref());
        set_str(table, "x11", self.x11.as_deref());
        set_int(table, "display", self.display.map(u64::from));
    }
}

impl Section for ServerSection {
    const NAME: &'static str = "server";
    const FIELDS: &'static [&'static str] = &["listen", "x11_port", "token"];

    fn get(&self, field: &str) -> Option<String> {
        match field {
            "listen" => self.listen.clone(),
            "x11_port" => self.x11_port.map(|v| v.to_string()),
            "token" => self.token.clone(),
            _ => None,
        }
    }

    fn set(&mut self, field: &str, raw: &str) -> bool {
        match field {
            "listen" => opt_field!(self, listen, raw, parse_string),
            "x11_port" => opt_field!(self, x11_port, raw, parse_u16),
            "token" => opt_field!(self, token, raw, parse_string),
            _ => true,
        }
    }

    fn write_into(&self, table: &mut Table) {
        set_str(table, "listen", self.listen.as_deref());
        set_int(table, "x11_port", self.x11_port.map(u64::from));
        set_str(table, "token", self.token.as_deref());
    }
}

impl Section for SshSection {
    const NAME: &'static str = "ssh";
    const FIELDS: &'static [&'static str] = &[
        "host",
        "port",
        "user",
        "identity",
        "token",
        "relay_port",
        "x11",
        "display",
        "reconnect",
    ];

    fn get(&self, field: &str) -> Option<String> {
        match field {
            "host" => self.host.clone(),
            "port" => self.port.map(|v| v.to_string()),
            "user" => self.user.clone(),
            "identity" => self.identity.clone(),
            "token" => self.token.clone(),
            "relay_port" => self.relay_port.map(|v| v.to_string()),
            "x11" => self.x11.clone(),
            "display" => self.display.map(|v| v.to_string()),
            "reconnect" => self.reconnect.map(|v| v.to_string()),
            _ => None,
        }
    }

    fn set(&mut self, field: &str, raw: &str) -> bool {
        match field {
            "host" => opt_field!(self, host, raw, parse_string),
            "port" => opt_field!(self, port, raw, parse_u16),
            "user" => opt_field!(self, user, raw, parse_string),
            "identity" => opt_field!(self, identity, raw, parse_string),
            "token" => opt_field!(self, token, raw, parse_string),
            "relay_port" => opt_field!(self, relay_port, raw, parse_u16),
            "x11" => opt_field!(self, x11, raw, parse_string),
            "display" => opt_field!(self, display, raw, parse_u16),
            "reconnect" => opt_field!(self, reconnect, raw, parse_bool),
            _ => true,
        }
    }

    fn write_into(&self, table: &mut Table) {
        set_str(table, "host", self.host.as_deref());
        set_int(table, "port", self.port.map(u64::from));
        set_str(table, "user", self.user.as_deref());
        set_str(table, "identity", self.identity.as_deref());
        set_str(table, "token", self.token.as_deref());
        set_int(table, "relay_port", self.relay_port.map(u64::from));
        set_str(table, "x11", self.x11.as_deref());
        set_int(table, "display", self.display.map(u64::from));
        set_bool(table, "reconnect", self.reconnect);
    }
}

impl Section for RuntimeSection {
    const NAME: &'static str = "runtime";
    const FIELDS: &'static [&'static str] = &[
        "heartbeat_interval_secs",
        "heartbeat_timeout_secs",
        "client_read_timeout_secs",
        "compression_threshold",
        "diff_chunk_size",
        "diff_min_block_size",
        "diff_min_savings_ratio",
        "outbound_queue_bytes",
        "tcp_send_buffer_bytes",
        "image_recode",
        "image_recode_codec",
        "image_recode_quality",
        "image_recode_min_bytes",
        "audio_forward",
        "audio_bitrate_kbps",
        "audio_record_command",
        "audio_play_command",
        "flow_control",
        "flow_pause_bytes",
        "flow_resume_bytes",
    ];

    fn get(&self, field: &str) -> Option<String> {
        match field {
            "heartbeat_interval_secs" => self.heartbeat_interval_secs.map(|v| v.to_string()),
            "heartbeat_timeout_secs" => self.heartbeat_timeout_secs.map(|v| v.to_string()),
            "client_read_timeout_secs" => self.client_read_timeout_secs.map(|v| v.to_string()),
            "compression_threshold" => self.compression_threshold.map(|v| v.to_string()),
            "diff_chunk_size" => self.diff_chunk_size.map(|v| v.to_string()),
            "diff_min_block_size" => self.diff_min_block_size.map(|v| v.to_string()),
            "diff_min_savings_ratio" => self.diff_min_savings_ratio.map(|v| v.to_string()),
            "outbound_queue_bytes" => self.outbound_queue_bytes.map(|v| v.to_string()),
            "tcp_send_buffer_bytes" => self.tcp_send_buffer_bytes.map(|v| v.to_string()),
            "image_recode" => self.image_recode.map(|v| v.to_string()),
            "image_recode_codec" => self.image_recode_codec.clone(),
            "image_recode_quality" => self.image_recode_quality.map(|v| v.to_string()),
            "image_recode_min_bytes" => self.image_recode_min_bytes.map(|v| v.to_string()),
            "audio_forward" => self.audio_forward.map(|v| v.to_string()),
            "audio_bitrate_kbps" => self.audio_bitrate_kbps.map(|v| v.to_string()),
            "audio_record_command" => self.audio_record_command.as_deref().map(format_argv),
            "audio_play_command" => self.audio_play_command.as_deref().map(format_argv),
            "flow_control" => self.flow_control.map(|v| v.to_string()),
            "flow_pause_bytes" => self.flow_pause_bytes.map(|v| v.to_string()),
            "flow_resume_bytes" => self.flow_resume_bytes.map(|v| v.to_string()),
            _ => None,
        }
    }

    fn set(&mut self, field: &str, raw: &str) -> bool {
        match field {
            "heartbeat_interval_secs" => {
                opt_field!(self, heartbeat_interval_secs, raw, parse_u64)
            }
            "heartbeat_timeout_secs" => opt_field!(self, heartbeat_timeout_secs, raw, parse_u64),
            "client_read_timeout_secs" => {
                opt_field!(self, client_read_timeout_secs, raw, parse_u64)
            }
            "compression_threshold" => opt_field!(self, compression_threshold, raw, parse_usize),
            "diff_chunk_size" => opt_field!(self, diff_chunk_size, raw, parse_usize),
            "diff_min_block_size" => opt_field!(self, diff_min_block_size, raw, parse_usize),
            "diff_min_savings_ratio" => opt_field!(self, diff_min_savings_ratio, raw, parse_f64),
            "outbound_queue_bytes" => opt_field!(self, outbound_queue_bytes, raw, parse_usize),
            "tcp_send_buffer_bytes" => opt_field!(self, tcp_send_buffer_bytes, raw, parse_usize),
            "image_recode" => opt_field!(self, image_recode, raw, parse_bool),
            "image_recode_codec" => opt_field!(self, image_recode_codec, raw, parse_string),
            "image_recode_quality" => opt_field!(self, image_recode_quality, raw, parse_u8),
            "image_recode_min_bytes" => opt_field!(self, image_recode_min_bytes, raw, parse_usize),
            "audio_forward" => opt_field!(self, audio_forward, raw, parse_bool),
            "audio_bitrate_kbps" => opt_field!(self, audio_bitrate_kbps, raw, parse_u32),
            "audio_record_command" => opt_field!(self, audio_record_command, raw, parse_argv),
            "audio_play_command" => opt_field!(self, audio_play_command, raw, parse_argv),
            "flow_control" => opt_field!(self, flow_control, raw, parse_bool),
            "flow_pause_bytes" => opt_field!(self, flow_pause_bytes, raw, parse_usize),
            "flow_resume_bytes" => opt_field!(self, flow_resume_bytes, raw, parse_usize),
            _ => true,
        }
    }

    fn write_into(&self, table: &mut Table) {
        set_int(
            table,
            "heartbeat_interval_secs",
            self.heartbeat_interval_secs,
        );
        set_int(table, "heartbeat_timeout_secs", self.heartbeat_timeout_secs);
        set_int(
            table,
            "client_read_timeout_secs",
            self.client_read_timeout_secs,
        );
        set_usize(table, "compression_threshold", self.compression_threshold);
        set_usize(table, "diff_chunk_size", self.diff_chunk_size);
        set_usize(table, "diff_min_block_size", self.diff_min_block_size);
        set_float(table, "diff_min_savings_ratio", self.diff_min_savings_ratio);
        set_usize(table, "outbound_queue_bytes", self.outbound_queue_bytes);
        set_usize(table, "tcp_send_buffer_bytes", self.tcp_send_buffer_bytes);
        set_bool(table, "image_recode", self.image_recode);
        set_str(
            table,
            "image_recode_codec",
            self.image_recode_codec.as_deref(),
        );
        set_int(
            table,
            "image_recode_quality",
            self.image_recode_quality.map(u64::from),
        );
        set_usize(table, "image_recode_min_bytes", self.image_recode_min_bytes);
        set_bool(table, "audio_forward", self.audio_forward);
        set_int(
            table,
            "audio_bitrate_kbps",
            self.audio_bitrate_kbps.map(u64::from),
        );
        set_argv(
            table,
            "audio_record_command",
            self.audio_record_command.as_deref(),
        );
        set_argv(
            table,
            "audio_play_command",
            self.audio_play_command.as_deref(),
        );
        set_bool(table, "flow_control", self.flow_control);
        set_usize(table, "flow_pause_bytes", self.flow_pause_bytes);
        set_usize(table, "flow_resume_bytes", self.flow_resume_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::test_support::full_config;

    #[test]
    fn flat_values_only_lists_filled_fields() {
        let cfg = GuiConfig {
            client: ClientSection {
                relay: Some("a:1".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let flat = flat_values(&cfg);
        assert_eq!(flat, vec![("client.relay".to_string(), "a:1".to_string())]);
        assert!(flat_values(&GuiConfig::default()).is_empty());
    }

    #[test]
    fn flat_values_round_trips_through_apply_flat() {
        let cfg = full_config();
        let flat = flat_values(&cfg);
        assert_eq!(flat.len(), all_field_keys().len());
        let mut rebuilt = GuiConfig::default();
        let issues = apply_flat_checked(&mut rebuilt, &flat);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(rebuilt, cfg);
    }

    #[test]
    fn apply_flat_empty_string_clears_and_unknown_keys_are_ignored() {
        let mut cfg = full_config();
        apply_flat(
            &mut cfg,
            &[
                ("client.relay".into(), String::new()),
                ("runtime.flow_control".into(), "   ".into()),
                ("nope.nothing".into(), "x".into()),
                ("client".into(), "x".into()),
            ],
        );
        assert_eq!(cfg.client.relay, None);
        assert_eq!(cfg.runtime.flow_control, None);
        assert_eq!(
            cfg.runtime.flow_pause_bytes,
            Some(2_097_152),
            "其它字段不受影响"
        );
    }

    #[test]
    fn apply_flat_reports_unparsable_values_and_keeps_old_value() {
        let mut cfg = GuiConfig::default();
        cfg.client.display = Some(1);
        let issues = apply_flat_checked(
            &mut cfg,
            &[
                ("client.display".into(), "不是数字".into()),
                ("ssh.reconnect".into(), "也许".into()),
                ("runtime.diff_min_savings_ratio".into(), "0.5".into()),
            ],
        );
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().any(|i| i.field == "client.display"));
        assert!(issues.iter().any(|i| i.field == "ssh.reconnect"));
        assert_eq!(cfg.client.display, Some(1), "解析失败时不改动原值");
        assert_eq!(cfg.runtime.diff_min_savings_ratio, Some(0.5));
    }

    #[test]
    fn apply_flat_accepts_bool_spellings_and_argv_shapes() {
        let mut cfg = GuiConfig::default();
        apply_flat(
            &mut cfg,
            &[
                ("runtime.flow_control".into(), "false".into()),
                ("ssh.reconnect".into(), "true".into()),
                (
                    "runtime.audio_play_command".into(),
                    "[\"paplay\", \"/dev/stdin\"]".into(),
                ),
                ("runtime.audio_record_command".into(), "parec --raw".into()),
            ],
        );
        assert_eq!(cfg.runtime.flow_control, Some(false));
        assert_eq!(cfg.ssh.reconnect, Some(true));
        assert_eq!(
            cfg.runtime.audio_play_command.as_deref(),
            Some(["paplay".to_string(), "/dev/stdin".to_string()].as_slice())
        );
        assert_eq!(
            cfg.runtime.audio_record_command.as_deref(),
            Some(["parec".to_string(), "--raw".to_string()].as_slice())
        );
        // 数组经过 flat_values 再回来必须等价
        let flat = flat_values(&cfg);
        let mut again = GuiConfig::default();
        apply_flat(&mut again, &flat);
        assert_eq!(again, cfg);
    }

    #[test]
    fn all_field_keys_are_unique_and_dotted() {
        let keys = all_field_keys();
        assert_eq!(keys.len(), 4 + 3 + 9 + 20, "四段字段总数");
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "键名不得重复");
        for k in &keys {
            assert!(k.contains('.'), "键名必须是 TOML 路径：{k}");
        }
    }
}
