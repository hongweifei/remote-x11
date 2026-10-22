//! 配置路径解析：`HOME` → `USERPROFILE`，都没有时退回临时目录。

use std::path::PathBuf;

fn non_empty_var(key: &str) -> Option<String> {
    // 设置了但为空的 HOME（例如 `HOME= cmd`）不能让配置路径退化成相对路径。
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn config_home() -> Option<PathBuf> {
    non_empty_var("HOME")
        .or_else(|| non_empty_var("USERPROFILE"))
        .map(PathBuf::from)
}

/// `$HOME/.config/rx11/config.toml`；空 `HOME` 回退 `USERPROFILE`（与 CLI 同规则）。
///
/// 两个环境变量都没有时（GUI 场景下几乎不可能）回退到临时目录下的
/// `rx11/config.toml`，**不会**写到当前工作目录。
pub fn default_config_path() -> PathBuf {
    match config_home() {
        Some(home) => home.join(".config").join("rx11").join("config.toml"),
        None => std::env::temp_dir().join("rx11").join("config.toml"),
    }
}

/// 展开路径开头的 `~` / `~/`（用 `HOME` → `USERPROFILE`）。
///
/// 只用于 GUI 侧的存在性检查、文件对话框初始目录等；**写回配置时保持原样**。
/// CLI 就是把 `ssh.identity` 原样交给 `ssh -i`（见 `rx11_client::ssh::
/// build_tunnel_args`），`~` 由 ssh 自己展开，所以这里不改变存储语义。
/// `~user/...` 形式不在本函数处理范围内，原样返回。
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return config_home()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    match path.strip_prefix("~/") {
        Some(rest) => match config_home() {
            Some(home) => home.join(rest).to_string_lossy().into_owned(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::test_support::{temp_dir, ENV_LOCK};

    #[test]
    fn default_config_path_follows_home_then_userprofile() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_home = std::env::var("HOME").ok();
        let saved_profile = std::env::var("USERPROFILE").ok();
        let dir = temp_dir("path");

        std::env::set_var("HOME", &dir);
        assert_eq!(
            default_config_path(),
            dir.join(".config").join("rx11").join("config.toml")
        );

        // 空 HOME 视为未设置，回退 USERPROFILE
        std::env::set_var("HOME", "");
        std::env::set_var("USERPROFILE", &dir);
        assert!(default_config_path().starts_with(&dir));

        // 两者都没有 → 临时目录兜底，绝不落到当前工作目录
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");
        let fallback = default_config_path();
        assert!(fallback.starts_with(std::env::temp_dir()));
        assert!(fallback.is_absolute());

        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match saved_profile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_tilde_only_touches_leading_tilde() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_home = std::env::var("HOME").ok();
        let dir = temp_dir("tilde");
        std::env::set_var("HOME", &dir);

        assert_eq!(
            expand_tilde("~/.ssh/id_ed25519"),
            dir.join(".ssh").join("id_ed25519").to_string_lossy()
        );
        assert_eq!(expand_tilde("~"), dir.to_string_lossy());
        assert_eq!(expand_tilde("/abs/key"), "/abs/key");
        assert_eq!(expand_tilde("~other/key"), "~other/key");
        assert_eq!(expand_tilde("rel/key"), "rel/key");

        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
