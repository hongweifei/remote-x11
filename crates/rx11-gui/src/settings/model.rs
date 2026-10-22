//! `config.toml` 的类型化模型：四段配置 + [`GuiConfig`]，以及到运行时配置的转换。

use std::time::Duration;

use serde::Deserialize;

use rx11_core::config::RuntimeConfig;

/// `[client]`：客户端直连/隧道连接的参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClientSection {
    pub relay: Option<String>,
    pub token: Option<String>,
    pub x11: Option<String>,
    pub display: Option<u16>,
}

/// `[server]`：中继服务端参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServerSection {
    pub listen: Option<String>,
    pub x11_port: Option<u16>,
    pub token: Option<String>,
}

/// `[ssh]`：SSH 隧道参数（`reconnect` 默认 true，false 恢复 fail-fast）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SshSection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity: Option<String>,
    pub token: Option<String>,
    pub relay_port: Option<u16>,
    pub x11: Option<String>,
    pub display: Option<u16>,
    pub reconnect: Option<bool>,
}

/// `[runtime]`：运行时可调参数，秒为单位，字段与 CLI 的
/// `RuntimeFileConfig` 一一对应（共 20 项）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RuntimeSection {
    pub heartbeat_interval_secs: Option<u64>,
    pub heartbeat_timeout_secs: Option<u64>,
    pub client_read_timeout_secs: Option<u64>,
    pub compression_threshold: Option<usize>,
    pub diff_chunk_size: Option<usize>,
    pub diff_min_block_size: Option<usize>,
    pub diff_min_savings_ratio: Option<f64>,
    pub outbound_queue_bytes: Option<usize>,
    pub tcp_send_buffer_bytes: Option<usize>,
    pub image_recode: Option<bool>,
    pub image_recode_codec: Option<String>,
    pub image_recode_quality: Option<u8>,
    pub image_recode_min_bytes: Option<usize>,
    pub audio_forward: Option<bool>,
    pub audio_bitrate_kbps: Option<u32>,
    pub audio_record_command: Option<Vec<String>>,
    pub audio_play_command: Option<Vec<String>>,
    pub flow_control: Option<bool>,
    pub flow_pause_bytes: Option<usize>,
    pub flow_resume_bytes: Option<usize>,
}

impl RuntimeSection {
    /// 叠加到 [`RuntimeConfig`] 的默认值上（未填写的键保持默认）。
    pub fn to_runtime(&self) -> RuntimeConfig {
        let defaults = RuntimeConfig::default();
        let secs = |v: Option<u64>, d: Duration| v.map(Duration::from_secs).unwrap_or(d);
        RuntimeConfig {
            heartbeat_interval: secs(self.heartbeat_interval_secs, defaults.heartbeat_interval),
            heartbeat_timeout: secs(self.heartbeat_timeout_secs, defaults.heartbeat_timeout),
            client_read_timeout: secs(self.client_read_timeout_secs, defaults.client_read_timeout),
            compression_threshold: self
                .compression_threshold
                .unwrap_or(defaults.compression_threshold),
            diff_chunk_size: self.diff_chunk_size.unwrap_or(defaults.diff_chunk_size),
            diff_min_block_size: self
                .diff_min_block_size
                .unwrap_or(defaults.diff_min_block_size),
            diff_min_savings_ratio: self
                .diff_min_savings_ratio
                .unwrap_or(defaults.diff_min_savings_ratio),
            outbound_queue_bytes: self
                .outbound_queue_bytes
                .unwrap_or(defaults.outbound_queue_bytes),
            tcp_send_buffer_bytes: self
                .tcp_send_buffer_bytes
                .or(defaults.tcp_send_buffer_bytes),
            image_recode: self.image_recode.unwrap_or(defaults.image_recode),
            image_recode_codec: match self.image_recode_codec.as_deref() {
                Some("png") => rx11_core::x11recode::RecodeCodec::Png,
                Some("jpeg") => rx11_core::x11recode::RecodeCodec::Jpeg,
                Some(other) => {
                    tracing::warn!("未知的 image_recode_codec「{other}」，保持默认值");
                    defaults.image_recode_codec
                }
                None => defaults.image_recode_codec,
            },
            image_recode_quality: self
                .image_recode_quality
                .unwrap_or(defaults.image_recode_quality),
            image_recode_min_bytes: self
                .image_recode_min_bytes
                .unwrap_or(defaults.image_recode_min_bytes),
            audio_forward: self.audio_forward.unwrap_or(defaults.audio_forward),
            audio_bitrate_kbps: self
                .audio_bitrate_kbps
                .unwrap_or(defaults.audio_bitrate_kbps),
            audio_record_command: self
                .audio_record_command
                .clone()
                .or(defaults.audio_record_command),
            audio_play_command: self
                .audio_play_command
                .clone()
                .or(defaults.audio_play_command),
            flow_control: self.flow_control.unwrap_or(defaults.flow_control),
            flow_pause_bytes: self.flow_pause_bytes.unwrap_or(defaults.flow_pause_bytes),
            flow_resume_bytes: self.flow_resume_bytes.unwrap_or(defaults.flow_resume_bytes),
        }
    }
}

/// 完整的 config.toml 模型（四段都必然存在，段内字段可为 `None`）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub client: ClientSection,
    pub server: ServerSection,
    pub ssh: SshSection,
    pub runtime: RuntimeSection,
}

impl GuiConfig {
    /// 会话运行参数：`[runtime]` 段叠加到编译期默认值上。
    pub fn to_runtime(&self) -> RuntimeConfig {
        self.runtime.to_runtime()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::test_support::full_config;

    #[test]
    fn to_runtime_maps_every_field() {
        let rt = full_config().to_runtime();
        assert_eq!(rt.heartbeat_interval, Duration::from_secs(30));
        assert_eq!(rt.heartbeat_timeout, Duration::from_secs(90));
        assert_eq!(rt.client_read_timeout, Duration::from_secs(100));
        assert_eq!(rt.compression_threshold, 256);
        assert_eq!(rt.diff_chunk_size, 4096);
        assert_eq!(rt.diff_min_block_size, 1024);
        assert_eq!(rt.diff_min_savings_ratio, 0.25);
        assert_eq!(rt.outbound_queue_bytes, 1_048_576);
        assert_eq!(rt.tcp_send_buffer_bytes, Some(262_144));
        assert!(rt.image_recode);
        assert_eq!(
            rt.image_recode_codec,
            rx11_core::x11recode::RecodeCodec::Jpeg
        );
        assert_eq!(rt.image_recode_quality, 85);
        assert_eq!(rt.image_recode_min_bytes, 16_384);
        assert!(!rt.audio_forward);
        assert_eq!(rt.audio_bitrate_kbps, 64);
        assert_eq!(
            rt.audio_record_command.as_deref(),
            Some(["parec".to_string(), "--raw".to_string()].as_slice())
        );
        assert_eq!(
            rt.audio_play_command.as_deref(),
            Some(["paplay".to_string(), "/dev/stdin".to_string()].as_slice())
        );
        assert!(rt.flow_control);
        assert_eq!(rt.flow_pause_bytes, 2_097_152);
        assert_eq!(rt.flow_resume_bytes, 262_144);
    }

    #[test]
    fn to_runtime_defaults_and_unknown_codec_fallback() {
        assert_eq!(GuiConfig::default().to_runtime(), RuntimeConfig::default());
        let mut cfg = GuiConfig::default();
        cfg.runtime.image_recode_codec = Some("webp".into());
        assert_eq!(
            cfg.to_runtime().image_recode_codec,
            RuntimeConfig::default().image_recode_codec,
            "未知编码格式回退默认值"
        );
        cfg.runtime.image_recode_codec = Some("png".into());
        assert_eq!(
            cfg.to_runtime().image_recode_codec,
            rx11_core::x11recode::RecodeCodec::Png
        );
    }
}
