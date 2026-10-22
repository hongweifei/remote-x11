//! 仅 `cfg(test)` 的共用夹具：临时目录、环境变量串行锁、全字段样例配置。

use std::path::PathBuf;
use std::sync::Mutex;

use super::model::{ClientSection, GuiConfig, RuntimeSection, ServerSection, SshSection};

/// 串行化所有会改 HOME/USERPROFILE 的测试（env 是进程级的）。
pub(super) static ENV_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rx11-gui-cfg-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(super) fn full_config() -> GuiConfig {
    GuiConfig {
        client: ClientSection {
            relay: Some("relay.example:7000".into()),
            token: Some("tk".into()),
            x11: Some("127.0.0.1:6000".into()),
            display: Some(3),
        },
        server: ServerSection {
            listen: Some("0.0.0.0:7000".into()),
            x11_port: Some(6100),
            token: Some("tk".into()),
        },
        ssh: SshSection {
            host: Some("jump.example".into()),
            port: Some(2222),
            user: Some("weifei".into()),
            identity: Some("~/.ssh/id_ed25519".into()),
            token: Some("tk".into()),
            relay_port: Some(7000),
            x11: Some("127.0.0.1:6000".into()),
            display: Some(1),
            reconnect: Some(false),
        },
        runtime: RuntimeSection {
            heartbeat_interval_secs: Some(30),
            heartbeat_timeout_secs: Some(90),
            client_read_timeout_secs: Some(100),
            compression_threshold: Some(256),
            diff_chunk_size: Some(4096),
            diff_min_block_size: Some(1024),
            diff_min_savings_ratio: Some(0.25),
            outbound_queue_bytes: Some(1_048_576),
            tcp_send_buffer_bytes: Some(262_144),
            image_recode: Some(true),
            image_recode_codec: Some("jpeg".into()),
            image_recode_quality: Some(85),
            image_recode_min_bytes: Some(16_384),
            audio_forward: Some(false),
            audio_bitrate_kbps: Some(64),
            audio_record_command: Some(vec!["parec".into(), "--raw".into()]),
            audio_play_command: Some(vec!["paplay".into(), "/dev/stdin".into()]),
            flow_control: Some(true),
            flow_pause_bytes: Some(2_097_152),
            flow_resume_bytes: Some(262_144),
        },
    }
}
