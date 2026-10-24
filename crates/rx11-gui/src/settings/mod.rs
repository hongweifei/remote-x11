//! rx11-gui 配置层：`~/.config/rx11/config.toml` 的类型化双向模型。
//!
//! 键名与语义和 `crates/rx11-cli/src/settings.rs` 完全一致（同一个文件 CLI 与
//! GUI 都能读写）：`[client]` / `[server]` / `[ssh]` / `[runtime]` 四段，每段
//! 字段都是 `Option<T>`，`None` 表示「用默认值」。
//!
//! 三条设计约束：
//! 1. **保存不破坏用户文件**：`save` 读回磁盘上的文档，只增删改它认识的那 37
//!    个键，注释、键序、未识别键全部原样保留；写盘走同目录临时文件 + rename，
//!    并自动创建父目录。
//! 2. **校验只报「已填但不合法」**：未填写的字段不算错误（GUI 允许留空用默认
//!    值），填写了就必须合法，错误信息是中文并带 TOML 键名。
//! 3. **不写真实 `$HOME`**：所有测试都用临时目录；路径解析规则与 CLI 相同
//!    （`HOME` → `USERPROFILE`，空值视为未设置）。
//!
//! # 子模块分层
//!
//! | 文件 | 职责 |
//! |---|---|
//! | `paths.rs` | `HOME`/`USERPROFILE` 解析（纯环境查询） |
//! | `error.rs` | `ConfigError` 与字节偏移 → 行号 |
//! | `model.rs` | 四段类型 + `GuiConfig`，以及 `[runtime]` → `RuntimeConfig` |
//! | `validate.rs` | `ValidationIssue` 与逐字段校验 |
//! | `flat.rs` | 扁平键值视图（表单引擎 ↔ TOML 路径）、`Section` 键名唯一来源 |
//! | `io.rs` | `load` / `save`（toml_edit：保注释、保键序、原子写） |
//!
//! 对外路径不变：原来的全部 pub 项都在这里重导出，crate 内其他文件无需改动。

mod error;
mod flat;
mod io;
mod model;
mod paths;
mod validate;

#[cfg(test)]
mod test_support;

pub use error::ConfigError;
pub use flat::{all_field_keys, apply_flat, apply_flat_checked, flat_values};
pub use model::{ClientSection, GuiConfig, RuntimeSection, ServerSection, SshSection};
pub use paths::{default_config_path, expand_tilde};
pub use validate::ValidationIssue;
