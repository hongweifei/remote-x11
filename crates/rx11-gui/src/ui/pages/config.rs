//! The four settings screens, defined declaratively.
//!
//! Not to be confused with [`crate::settings`] (`src/settings/`): that is the
//! `config.toml` model — typed load/save/validate. This file only declares
//! which keys the four config pages show, in what order and under what label;
//! the two sides meet through the app's flat-key bridge and never import each
//! other.
//!
//! Each is a `FormSpec`: sections of fields keyed by their TOML path. The
//! engine in [`super::form`] renders them, so this file is pure data — which
//! is what makes the pages cheap to keep in sync with `config.toml`: a new
//! runtime knob is one `FieldSpec` here plus one line in the config layer.
//!
//! Values are strings on the wire between the two layers; the app maps them
//! to the typed config struct. That keeps this file free of schema types and
//! means the hints below can be written for the person setting the tool up
//! rather than for a type checker.

use super::form::{FieldKind, FieldSpec, FormRole, FormSpec, SectionSpec};

// ---------------------------------------------------------------------------
// [server]
// ---------------------------------------------------------------------------

static SERVER_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "server.listen",
        label: "监听地址",
        hint: "形如 0.0.0.0:7000；客户端连的就是这个地址",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "server.x11_port",
        label: "X11 代理起始端口",
        hint: "display :N 映射到 6000+N；默认 6000",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "server.token",
        label: "认证令牌",
        hint: "留空则启动时随机生成并打印；两端必须一致",
        kind: FieldKind::Text,
    },
];

static SERVER_SECTIONS: &[SectionSpec] = &[SectionSpec {
    title: "中继服务",
    note: "远端主机",
    fields: SERVER_FIELDS,
}];

/// `[server]` page.
pub static SERVER: FormSpec = FormSpec {
    title: "服务端",
    subtitle: "远端中继服务：监听地址、X11 端口与认证令牌",
    sections: SERVER_SECTIONS,
    role: Some(FormRole::Server),
    start_label: "启动中继",
};

// ---------------------------------------------------------------------------
// [client]
// ---------------------------------------------------------------------------

static CLIENT_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "client.relay",
        label: "中继地址",
        hint: "远端 rx11 server 的地址，形如 my-server:7000",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "client.token",
        label: "认证令牌",
        hint: "与远端服务端一致；留空则无法连接",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "client.x11",
        label: "本地 X Server",
        hint: "默认 127.0.0.1:6000，即 display :0",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "client.display",
        label: "固定 display",
        hint: "留空 = 由服务端自动分配，可同时开多个会话",
        kind: FieldKind::Number,
    },
];

static CLIENT_SECTIONS: &[SectionSpec] = &[SectionSpec {
    title: "本机客户端",
    note: "直连中继",
    fields: CLIENT_FIELDS,
}];

/// `[client]` page.
pub static CLIENT: FormSpec = FormSpec {
    title: "客户端",
    subtitle: "本机客户端：直连中继并把 X11 流量转发到本地 X Server",
    sections: CLIENT_SECTIONS,
    role: Some(FormRole::Client),
    start_label: "启动客户端",
};

// ---------------------------------------------------------------------------
// [ssh]
// ---------------------------------------------------------------------------

static SSH_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "ssh.host",
        label: "SSH 主机",
        hint: "远端主机名或 IP；协议不需要额外开放端口",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "ssh.port",
        label: "SSH 端口",
        hint: "默认 22",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "ssh.user",
        label: "登录用户",
        hint: "留空则用当前用户名",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "ssh.identity",
        label: "私钥文件",
        hint: "留空则用 ssh 默认的密钥与代理",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "ssh.relay_port",
        label: "远端中继端口",
        hint: "隧道转发的目标端口，默认 7000",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "ssh.token",
        label: "认证令牌",
        hint: "与远端服务端一致",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "ssh.reconnect",
        label: "断线自动重建隧道",
        hint: "开启后隧道意外退出会按退避重建，会话尽量续上",
        kind: FieldKind::Toggle,
    },
    FieldSpec {
        key: "ssh.display",
        label: "固定 display",
        hint: "留空 = 自动分配；固定时本地绑定 17000+display",
        kind: FieldKind::Number,
    },
];

static SSH_SECTIONS: &[SectionSpec] = &[SectionSpec {
    title: "SSH 隧道",
    note: "加密传输",
    fields: SSH_FIELDS,
}];

/// `[ssh]` page.
pub static SSH: FormSpec = FormSpec {
    title: "SSH 隧道",
    subtitle: "经由 SSH 隧道连接中继，无需在远端额外开放端口",
    sections: SSH_SECTIONS,
    role: Some(FormRole::Ssh),
    start_label: "建立隧道并连接",
};

// ---------------------------------------------------------------------------
// [runtime]
// ---------------------------------------------------------------------------

static RT_TIMING: &[FieldSpec] = &[
    FieldSpec {
        key: "runtime.heartbeat_interval_secs",
        label: "心跳间隔",
        hint: "秒；服务端探测客户端的周期",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.heartbeat_timeout_secs",
        label: "心跳超时",
        hint: "秒；超过此时长没心跳即判定客户端掉线",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.client_read_timeout_secs",
        label: "客户端读超时",
        hint: "秒；客户端多久没收到数据即判定链路死亡",
        kind: FieldKind::Number,
    },
];

static RT_COMPRESSION: &[FieldSpec] = &[
    FieldSpec {
        key: "runtime.compression_threshold",
        label: "压缩阈值",
        hint: "小于该字节数的负载不压缩，省下无收益的 CPU",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.diff_chunk_size",
        label: "增量块大小",
        hint: "字节；差分比对的粒度",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.diff_min_block_size",
        label: "差分最小帧",
        hint: "字节；小于该值的帧不做差分",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.diff_min_savings_ratio",
        label: "差分最小收益",
        hint: "0~1；省不到这个比例就直接发整帧",
        kind: FieldKind::Float,
    },
    FieldSpec {
        key: "runtime.incremental_uplink",
        label: "上行增量差分",
        hint: "本地程序发往中继的请求流也做差分（默认开启）；关闭后上行始终发整帧",
        kind: FieldKind::Toggle,
    },
    FieldSpec {
        key: "runtime.outbound_queue_bytes",
        label: "出站排队上限",
        hint: "字节；限住慢链路上的排队延迟（1 MB ≈ 3 Mbps 下 2.8 秒）",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.tcp_send_buffer_bytes",
        label: "内核发送缓冲上限",
        hint: "字节；留空交给系统自适应。小队列配小内核缓冲才压得住延迟",
        kind: FieldKind::Number,
    },
];

static RT_X11: &[FieldSpec] = &[
    FieldSpec {
        key: "runtime.image_recode",
        label: "图像重编码",
        hint: "把大的 PutImage 像素在传输中改编码；两端任一关闭即逐字节透传",
        kind: FieldKind::Toggle,
    },
    FieldSpec {
        key: "runtime.image_recode_codec",
        label: "重编码格式",
        hint: "png 无损（默认）；jpeg 有损，只有显式选择才启用",
        kind: FieldKind::Select(&["png", "jpeg"]),
    },
    FieldSpec {
        key: "runtime.image_recode_quality",
        label: "JPEG 质量",
        hint: "1~100；仅在格式为 jpeg 时生效",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.image_recode_min_bytes",
        label: "重编码最小尺寸",
        hint: "字节；更小的图像保持逐字节原样",
        kind: FieldKind::Number,
    },
];

static RT_AUDIO: &[FieldSpec] = &[
    FieldSpec {
        key: "runtime.audio_forward",
        label: "转发桌面音频",
        hint: "服务端采集输出设备，客户端播放；任一关闭即静音",
        kind: FieldKind::Toggle,
    },
    FieldSpec {
        key: "runtime.audio_bitrate_kbps",
        label: "音频码率",
        hint: "kbps；32 适合人声，64 接近透明",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.audio_record_command",
        label: "采集命令",
        hint: "留空自动探测 parec；自定义时按空格拆分整条命令",
        kind: FieldKind::Argv,
    },
    FieldSpec {
        key: "runtime.audio_play_command",
        label: "播放命令",
        hint: "留空自动探测 paplay；自定义时按空格拆分整条命令",
        kind: FieldKind::Argv,
    },
];

static RT_FLOW: &[FieldSpec] = &[
    FieldSpec {
        key: "runtime.flow_control",
        label: "逐通道流控",
        hint: "积压时让发送端停读源套接字，而不是把旧像素灌进管道",
        kind: FieldKind::Toggle,
    },
    FieldSpec {
        key: "runtime.flow_pause_bytes",
        label: "暂停水位",
        hint: "字节；单通道积压超过此值即发 Pause",
        kind: FieldKind::Number,
    },
    FieldSpec {
        key: "runtime.flow_resume_bytes",
        label: "恢复水位",
        hint: "字节；必须小于暂停水位，两者之差即迟滞区间",
        kind: FieldKind::Number,
    },
];

static RT_SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        title: "超时与心跳",
        note: "两端各自生效",
        fields: RT_TIMING,
    },
    SectionSpec {
        title: "压缩与增量",
        note: "带宽",
        fields: RT_COMPRESSION,
    },
    SectionSpec {
        title: "X11 图像",
        note: "x11_recode",
        fields: RT_X11,
    },
    SectionSpec {
        title: "音频",
        note: "audio_forward",
        fields: RT_AUDIO,
    },
    SectionSpec {
        title: "流控",
        note: "flow_control",
        fields: RT_FLOW,
    },
];

/// `[runtime]` page.
pub static RUNTIME: FormSpec = FormSpec {
    title: "运行时",
    subtitle: "压缩、增量、图像重编码、音频与流控的调参项",
    sections: RT_SECTIONS,
    role: None,
    start_label: "",
};

/// Every field key a page manages, in layout order. Used by the tests that
/// pin the runtime page against the config layer's key list.
pub fn keys_for(spec: &FormSpec) -> Vec<&'static str> {
    spec.sections
        .iter()
        .flat_map(|s| s.fields.iter().map(|f| f.key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_key_is_unique_across_pages() {
        let mut all: Vec<&str> = Vec::new();
        for spec in [&SERVER, &CLIENT, &SSH, &RUNTIME] {
            all.extend(keys_for(spec));
        }
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "duplicate field key in the specs");
    }

    #[test]
    fn keys_are_toml_paths() {
        for spec in [&SERVER, &CLIENT, &SSH, &RUNTIME] {
            for key in keys_for(spec) {
                assert!(
                    key.contains('.') && !key.contains(' '),
                    "key {key} is not a TOML path"
                );
            }
        }
    }

    #[test]
    fn runtime_page_covers_every_runtime_knob() {
        // The runtime page is the one place a user can see all knobs; a new
        // knob that never appears here is a knob nobody can set.
        let keys = keys_for(&RUNTIME);
        for expected in [
            "runtime.heartbeat_interval_secs",
            "runtime.heartbeat_timeout_secs",
            "runtime.client_read_timeout_secs",
            "runtime.compression_threshold",
            "runtime.diff_chunk_size",
            "runtime.diff_min_block_size",
            "runtime.diff_min_savings_ratio",
            "runtime.incremental_uplink",
            "runtime.outbound_queue_bytes",
            "runtime.tcp_send_buffer_bytes",
            "runtime.image_recode",
            "runtime.image_recode_codec",
            "runtime.image_recode_quality",
            "runtime.image_recode_min_bytes",
            "runtime.audio_forward",
            "runtime.audio_bitrate_kbps",
            "runtime.audio_record_command",
            "runtime.audio_play_command",
            "runtime.flow_control",
            "runtime.flow_pause_bytes",
            "runtime.flow_resume_bytes",
        ] {
            assert!(
                keys.contains(&expected),
                "runtime page is missing {expected}"
            );
        }
    }

    #[test]
    fn only_lifecycle_pages_carry_a_role() {
        assert_eq!(SERVER.role, Some(FormRole::Server));
        assert_eq!(CLIENT.role, Some(FormRole::Client));
        assert_eq!(SSH.role, Some(FormRole::Ssh));
        assert_eq!(RUNTIME.role, None);
    }
}
