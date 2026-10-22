//! 配置读写错误：中文 `Display` + 解析错误定位到行号。

use std::fmt;

/// 配置读写错误。`Display` 为中文，可直接显示在 GUI 的错误条上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// 读写失败（含路径）。
    Io(String),
    /// TOML 解析失败；`line` 为 1 起始的行号，0 表示无法定位。
    Parse { line: usize, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(msg) => write!(f, "配置读写失败：{msg}"),
            ConfigError::Parse { line: 0, message } => write!(f, "配置解析失败：{message}"),
            ConfigError::Parse { line, message } => {
                write!(f, "配置第 {line} 行解析失败：{message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// 字节偏移 → 1 起始的行号。
pub(super) fn line_of(content: &str, byte: usize) -> usize {
    let mut line = 1usize;
    for (i, c) in content.char_indices() {
        if i >= byte {
            break;
        }
        if c == '\n' {
            line += 1;
        }
    }
    line
}
