//! 逐字段校验：只报「已填写但不合法」的取值（错误信息带 TOML 键名）。

use std::fmt;

use super::model::GuiConfig;

impl GuiConfig {
    /// 逐字段校验，返回全部问题（中文提示 + TOML 键名）。
    ///
    /// 未填写的字段不报错（留空 = 用默认值）；填写了但非法才报错。
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        let c = &self.client;
        check_host_port(&mut issues, "client.relay", c.relay.as_deref());
        check_non_empty(&mut issues, "client.token", c.token.as_deref(), "token");
        check_host_port(&mut issues, "client.x11", c.x11.as_deref());
        check_display(&mut issues, "client.display", c.display);

        let s = &self.server;
        check_host_port(&mut issues, "server.listen", s.listen.as_deref());
        if let Some(port) = s.x11_port {
            if port == 0 {
                issues.push(ValidationIssue::new(
                    "server.x11_port",
                    "X11 端口基数必须是 1-65280 之间的整数",
                ));
            } else if port > u16::MAX - 255 {
                issues.push(ValidationIssue::new(
                    "server.x11_port",
                    "X11 端口基数不能超过 65280（每个 display 都要占用一个端口，需预留 256 个）",
                ));
            }
        }
        check_non_empty(&mut issues, "server.token", s.token.as_deref(), "token");

        let h = &self.ssh;
        check_non_empty(&mut issues, "ssh.host", h.host.as_deref(), "主机名");
        check_port(&mut issues, "ssh.port", h.port);
        check_non_empty(&mut issues, "ssh.user", h.user.as_deref(), "用户名");
        check_non_empty(
            &mut issues,
            "ssh.identity",
            h.identity.as_deref(),
            "身份文件路径",
        );
        check_non_empty(&mut issues, "ssh.token", h.token.as_deref(), "token");
        check_port(&mut issues, "ssh.relay_port", h.relay_port);
        check_host_port(&mut issues, "ssh.x11", h.x11.as_deref());
        check_display(&mut issues, "ssh.display", h.display);

        let r = &self.runtime;
        check_min_u64(
            &mut issues,
            "runtime.heartbeat_interval_secs",
            r.heartbeat_interval_secs,
            1,
        );
        check_min_u64(
            &mut issues,
            "runtime.heartbeat_timeout_secs",
            r.heartbeat_timeout_secs,
            1,
        );
        if let (Some(interval), Some(timeout)) =
            (r.heartbeat_interval_secs, r.heartbeat_timeout_secs)
        {
            if timeout <= interval {
                issues.push(ValidationIssue::new(
                    "runtime.heartbeat_timeout_secs",
                    format!("心跳超时（{timeout}s）必须大于心跳间隔（{interval}s）"),
                ));
            }
        }
        check_min_u64(
            &mut issues,
            "runtime.client_read_timeout_secs",
            r.client_read_timeout_secs,
            1,
        );
        check_min_usize(&mut issues, "runtime.diff_chunk_size", r.diff_chunk_size, 1);
        check_min_usize(
            &mut issues,
            "runtime.diff_min_block_size",
            r.diff_min_block_size,
            1,
        );
        if let (Some(chunk), Some(block)) = (r.diff_chunk_size, r.diff_min_block_size) {
            if block > chunk {
                issues.push(ValidationIssue::new(
                    "runtime.diff_min_block_size",
                    format!("差分最小块（{block}）不能大于差分块大小（{chunk}），否则永远不会发送差分帧"),
                ));
            }
        }
        if let Some(ratio) = r.diff_min_savings_ratio {
            if !(0.0..=1.0).contains(&ratio) {
                issues.push(ValidationIssue::new(
                    "runtime.diff_min_savings_ratio",
                    format!("最小节省比例必须在 0.0-1.0 之间（当前 {ratio}）"),
                ));
            }
        }
        check_min_usize(
            &mut issues,
            "runtime.outbound_queue_bytes",
            r.outbound_queue_bytes,
            1,
        );
        check_min_usize(
            &mut issues,
            "runtime.tcp_send_buffer_bytes",
            r.tcp_send_buffer_bytes,
            1,
        );
        if let Some(codec) = r.image_recode_codec.as_deref() {
            if codec != "png" && codec != "jpeg" {
                issues.push(ValidationIssue::new(
                    "runtime.image_recode_codec",
                    format!(
                        "图像重编码格式只能是 \"png\"（无损）或 \"jpeg\"（有损），当前 \"{codec}\""
                    ),
                ));
            }
        }
        if let Some(q) = r.image_recode_quality {
            if q == 0 || q > 100 {
                issues.push(ValidationIssue::new(
                    "runtime.image_recode_quality",
                    format!("JPEG 质量必须在 1-100 之间（当前 {q}）"),
                ));
            }
        }
        check_min_usize(
            &mut issues,
            "runtime.image_recode_min_bytes",
            r.image_recode_min_bytes,
            1,
        );
        if let Some(kbps) = r.audio_bitrate_kbps {
            if !(6..=510).contains(&kbps) {
                issues.push(ValidationIssue::new(
                    "runtime.audio_bitrate_kbps",
                    format!("Opus 码率必须在 6-510 kbps 之间（当前 {kbps}）"),
                ));
            }
        }
        check_argv(
            &mut issues,
            "runtime.audio_record_command",
            r.audio_record_command.as_deref(),
        );
        check_argv(
            &mut issues,
            "runtime.audio_play_command",
            r.audio_play_command.as_deref(),
        );
        check_min_usize(
            &mut issues,
            "runtime.flow_pause_bytes",
            r.flow_pause_bytes,
            1,
        );
        check_min_usize(
            &mut issues,
            "runtime.flow_resume_bytes",
            r.flow_resume_bytes,
            1,
        );
        if let (Some(pause), Some(resume)) = (r.flow_pause_bytes, r.flow_resume_bytes) {
            if resume >= pause {
                issues.push(ValidationIssue::new(
                    "runtime.flow_resume_bytes",
                    format!("恢复水位（{resume}）必须小于暂停水位（{pause}）"),
                ));
            }
        }

        issues
    }
}

/// 一条校验问题：`field` 是 TOML 键路径（如 `runtime.flow_resume_bytes`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

impl ValidationIssue {
    pub(super) fn new(field: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_string(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.field, self.message)
    }
}

fn check_port(issues: &mut Vec<ValidationIssue>, field: &str, port: Option<u16>) {
    if port == Some(0) {
        issues.push(ValidationIssue::new(field, "端口必须是 1-65535 之间的整数"));
    }
}

fn check_display(issues: &mut Vec<ValidationIssue>, field: &str, display: Option<u16>) {
    if let Some(d) = display {
        if d > 255 {
            issues.push(ValidationIssue::new(
                field,
                format!("display 必须在 0-255 之间（当前 {d}）"),
            ));
        }
    }
}

fn check_non_empty(
    issues: &mut Vec<ValidationIssue>,
    field: &str,
    value: Option<&str>,
    what: &str,
) {
    if let Some(v) = value {
        if v.trim().is_empty() {
            issues.push(ValidationIssue::new(field, format!("{what}不能为空")));
        }
    }
}

/// `host:port` 形式校验（主机名可以是域名或 IP，端口 1-65535）。
fn check_host_port(issues: &mut Vec<ValidationIssue>, field: &str, value: Option<&str>) {
    let Some(v) = value else { return };
    if v.trim().is_empty() {
        issues.push(ValidationIssue::new(
            field,
            "地址不能为空，格式为 host:port",
        ));
        return;
    }
    match v.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
            Ok(0) | Err(_) => issues.push(ValidationIssue::new(
                field,
                format!("端口必须是 1-65535 之间的整数（当前 \"{v}\"）"),
            )),
            Ok(_) => {}
        },
        _ => issues.push(ValidationIssue::new(
            field,
            format!("地址格式应为 host:port（当前 \"{v}\"）"),
        )),
    }
}

fn check_min_u64(issues: &mut Vec<ValidationIssue>, field: &str, value: Option<u64>, min: u64) {
    if let Some(v) = value {
        if v < min {
            issues.push(ValidationIssue::new(
                field,
                format!("值必须不小于 {min}（当前 {v}）"),
            ));
        }
    }
}

fn check_min_usize(
    issues: &mut Vec<ValidationIssue>,
    field: &str,
    value: Option<usize>,
    min: usize,
) {
    if let Some(v) = value {
        if v < min {
            issues.push(ValidationIssue::new(
                field,
                format!("值必须不小于 {min}（当前 {v}）"),
            ));
        }
    }
}

fn check_argv(issues: &mut Vec<ValidationIssue>, field: &str, argv: Option<&[String]>) {
    if let Some(argv) = argv {
        if argv.is_empty() || argv.iter().all(|a| a.trim().is_empty()) {
            issues.push(ValidationIssue::new(
                field,
                "命令参数不能为空（留空表示自动探测；填写时第一项必须是可执行文件名）",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::test_support::full_config;

    #[test]
    fn validate_clean_config_has_no_issue() {
        assert!(full_config().validate().is_empty());
        assert!(GuiConfig::default().validate().is_empty(), "留空不算错误");
    }

    #[test]
    fn validate_reports_each_rule_with_field_name() {
        let cases: Vec<(&str, GuiConfig)> = vec![
            ("client.relay", {
                let mut c = GuiConfig::default();
                c.client.relay = Some("no-port".into());
                c
            }),
            ("client.token", {
                let mut c = GuiConfig::default();
                c.client.token = Some("  ".into());
                c
            }),
            ("client.x11", {
                let mut c = GuiConfig::default();
                c.client.x11 = Some(":0".into());
                c
            }),
            ("client.display", {
                let mut c = GuiConfig::default();
                c.client.display = Some(300);
                c
            }),
            ("server.listen", {
                let mut c = GuiConfig::default();
                c.server.listen = Some("127.0.0.1".into());
                c
            }),
            ("server.x11_port", {
                let mut c = GuiConfig::default();
                c.server.x11_port = Some(0);
                c
            }),
            ("server.x11_port", {
                let mut c = GuiConfig::default();
                c.server.x11_port = Some(65535);
                c
            }),
            ("ssh.host", {
                let mut c = GuiConfig::default();
                c.ssh.host = Some(String::new());
                c
            }),
            ("ssh.port", {
                let mut c = GuiConfig::default();
                c.ssh.port = Some(0);
                c
            }),
            ("ssh.user", {
                let mut c = GuiConfig::default();
                c.ssh.user = Some(" ".into());
                c
            }),
            ("ssh.identity", {
                let mut c = GuiConfig::default();
                c.ssh.identity = Some(String::new());
                c
            }),
            ("ssh.relay_port", {
                let mut c = GuiConfig::default();
                c.ssh.relay_port = Some(0);
                c
            }),
            ("ssh.display", {
                let mut c = GuiConfig::default();
                c.ssh.display = Some(256);
                c
            }),
            ("runtime.heartbeat_interval_secs", {
                let mut c = GuiConfig::default();
                c.runtime.heartbeat_interval_secs = Some(0);
                c
            }),
            ("runtime.heartbeat_timeout_secs", {
                let mut c = GuiConfig::default();
                c.runtime.heartbeat_interval_secs = Some(60);
                c.runtime.heartbeat_timeout_secs = Some(30);
                c
            }),
            ("runtime.client_read_timeout_secs", {
                let mut c = GuiConfig::default();
                c.runtime.client_read_timeout_secs = Some(0);
                c
            }),
            ("runtime.diff_chunk_size", {
                let mut c = GuiConfig::default();
                c.runtime.diff_chunk_size = Some(0);
                c
            }),
            ("runtime.diff_min_block_size", {
                let mut c = GuiConfig::default();
                c.runtime.diff_chunk_size = Some(512);
                c.runtime.diff_min_block_size = Some(1024);
                c
            }),
            ("runtime.diff_min_savings_ratio", {
                let mut c = GuiConfig::default();
                c.runtime.diff_min_savings_ratio = Some(1.5);
                c
            }),
            ("runtime.outbound_queue_bytes", {
                let mut c = GuiConfig::default();
                c.runtime.outbound_queue_bytes = Some(0);
                c
            }),
            ("runtime.tcp_send_buffer_bytes", {
                let mut c = GuiConfig::default();
                c.runtime.tcp_send_buffer_bytes = Some(0);
                c
            }),
            ("runtime.image_recode_codec", {
                let mut c = GuiConfig::default();
                c.runtime.image_recode_codec = Some("webp".into());
                c
            }),
            ("runtime.image_recode_quality", {
                let mut c = GuiConfig::default();
                c.runtime.image_recode_quality = Some(0);
                c
            }),
            ("runtime.image_recode_min_bytes", {
                let mut c = GuiConfig::default();
                c.runtime.image_recode_min_bytes = Some(0);
                c
            }),
            ("runtime.audio_bitrate_kbps", {
                let mut c = GuiConfig::default();
                c.runtime.audio_bitrate_kbps = Some(1000);
                c
            }),
            ("runtime.audio_record_command", {
                let mut c = GuiConfig::default();
                c.runtime.audio_record_command = Some(Vec::new());
                c
            }),
            ("runtime.audio_play_command", {
                let mut c = GuiConfig::default();
                c.runtime.audio_play_command = Some(vec![" ".into()]);
                c
            }),
            ("runtime.flow_pause_bytes", {
                let mut c = GuiConfig::default();
                c.runtime.flow_pause_bytes = Some(0);
                c
            }),
            ("runtime.flow_resume_bytes", {
                let mut c = GuiConfig::default();
                c.runtime.flow_pause_bytes = Some(1024);
                c.runtime.flow_resume_bytes = Some(1024);
                c
            }),
        ];
        for (field, cfg) in cases {
            let issues = cfg.validate();
            assert!(
                issues.iter().any(|i| i.field == field),
                "配置 {cfg:?} 应报出 {field}，实际 {issues:?}"
            );
            for i in &issues {
                assert!(!i.message.is_empty());
            }
        }
    }
}
