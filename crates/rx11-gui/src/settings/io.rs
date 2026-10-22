//! 读写 `config.toml`：保注释、保键序、保留未识别键，原子写入。
//!
//! 认识的键以 [`GuiConfig`] 模型为准；写盘走同目录临时文件 + rename。

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::error::{line_of, ConfigError};
use super::flat::Section;
use super::model::GuiConfig;

impl GuiConfig {
    /// 读取配置文件。
    ///
    /// - 文件不存在 → 返回全默认配置（GUI 首次启动就是这种情形）；
    /// - TOML 语法/类型错误 → [`ConfigError::Parse`]（带行号）；
    /// - 其它 IO 错误 → [`ConfigError::Io`]。
    ///
    /// 未识别的键被忽略（但 `save` 时会原样保留）。
    pub fn load(path: &Path) -> Result<GuiConfig, ConfigError> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GuiConfig::default()),
            Err(e) => {
                return Err(ConfigError::Io(format!("{}：{e}", path.display())));
            }
        };
        toml_edit::de::from_str::<GuiConfig>(&content).map_err(|e| ConfigError::Parse {
            line: e.span().map(|s| line_of(&content, s.start)).unwrap_or(0),
            message: e.message().to_string(),
        })
    }

    /// 写回配置文件：保注释、保键序、保留未识别键，原子写入并自动建父目录。
    ///
    /// 认识的键以本模型为准：`Some(v)` 写入（保留该键原有的行内注释），
    /// `None` 删除该键；四段之外的内容一律不动。模型全空且文件不存在时
    /// 写入空文件（等价于全默认）。
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let existing = match std::fs::read_to_string(path) {
            Ok(c) => Some(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ConfigError::Io(format!("{}：{e}", path.display()))),
        };
        let mut doc = match existing {
            Some(text) => text
                .parse::<DocumentMut>()
                .map_err(|e| ConfigError::Parse {
                    line: e.span().map(|s| line_of(&text, s.start)).unwrap_or(0),
                    message: e.message().to_string(),
                })?,
            None => DocumentMut::new(),
        };
        self.apply_to(&mut doc);
        let rendered = doc.to_string();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ConfigError::Io(format!("{}：{e}", parent.display())))?;
            }
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_string());
        let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, rendered.as_bytes())
            .map_err(|e| ConfigError::Io(format!("{}：{e}", tmp.display())))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigError::Io(format!("{}：{e}", path.display())));
        }
        Ok(())
    }

    fn apply_to(&self, doc: &mut DocumentMut) {
        write_section(doc, &self.client);
        write_section(doc, &self.server);
        write_section(doc, &self.ssh);
        write_section(doc, &self.runtime);
    }
}

// ---------------------------------------------------------------------------
// toml_edit 写入：保注释、保键序
// ---------------------------------------------------------------------------

fn ensure_table<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    let needs_table = !doc.as_table().contains_key(name) || !doc.as_table()[name].is_table();
    if needs_table {
        doc.as_table_mut().insert(name, Item::Table(Table::new()));
    }
    doc.as_table_mut()[name]
        .as_table_mut()
        .expect("刚刚保证过是 Table")
}

/// 写入一个键；`None` 表示删除该键。
fn assign(table: &mut Table, key: &str, item: Option<Item>) {
    let Some(new) = item else {
        table.remove(key);
        return;
    };
    match (table.get_mut(key), new) {
        // 已存在且都是值：**原地替换**，键的注释（Key 的 decor）与键序都不动。
        (Some(slot), Item::Value(nv)) if matches!(slot, Item::Value(_)) => {
            let decor = slot.as_value().map(|v| v.decor().clone());
            let mut nv = nv;
            if let Some(decor) = decor {
                *nv.decor_mut() = decor;
            }
            *slot.as_value_mut().expect("已确认是 Value") = nv;
        }
        // 类型变了（例如原来是表）：整体替换。
        (Some(slot), new) => *slot = new,
        (None, new) => {
            table.insert(key, new);
        }
    }
}

fn int_value(v: u64) -> Item {
    Item::Value(Value::from(i64::try_from(v).unwrap_or(i64::MAX)))
}

pub(super) fn set_str(table: &mut Table, key: &str, value: Option<&str>) {
    assign(table, key, value.map(|v| Item::Value(Value::from(v))));
}

pub(super) fn set_bool(table: &mut Table, key: &str, value: Option<bool>) {
    assign(table, key, value.map(|v| Item::Value(Value::from(v))));
}

pub(super) fn set_int(table: &mut Table, key: &str, value: Option<u64>) {
    assign(table, key, value.map(int_value));
}

pub(super) fn set_usize(table: &mut Table, key: &str, value: Option<usize>) {
    assign(table, key, value.map(|v| int_value(v as u64)));
}

pub(super) fn set_float(table: &mut Table, key: &str, value: Option<f64>) {
    assign(table, key, value.map(|v| Item::Value(Value::from(v))));
}

pub(super) fn set_argv(table: &mut Table, key: &str, value: Option<&[String]>) {
    assign(
        table,
        key,
        value.map(|argv| {
            let mut arr = Array::new();
            for a in argv {
                arr.push(a.as_str());
            }
            Item::Value(Value::Array(arr))
        }),
    );
}

fn write_section<S: Section>(doc: &mut DocumentMut, section: &S) {
    if !doc.as_table().contains_key(S::NAME) && section.is_unset() {
        return;
    }
    let table = ensure_table(doc, S::NAME);
    section.write_into(table);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::flat::all_field_keys;
    use crate::settings::test_support::{full_config, temp_dir};

    #[test]
    fn missing_file_loads_as_default() {
        let dir = temp_dir("missing");
        let cfg = GuiConfig::load(&dir.join("nope.toml")).unwrap();
        assert_eq!(cfg, GuiConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_loads_as_default() {
        let dir = temp_dir("empty");
        let path = dir.join("config.toml");
        std::fs::write(&path, "").unwrap();
        assert_eq!(GuiConfig::load(&path).unwrap(), GuiConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_all_fields() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.toml");
        let cfg = full_config();
        cfg.save(&path).unwrap();
        let loaded = GuiConfig::load(&path).unwrap();
        assert_eq!(loaded, cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_section_key_is_written_and_read_back() {
        let dir = temp_dir("keys");
        let path = dir.join("config.toml");
        full_config().save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        let mut keys = Vec::new();
        for (name, item) in doc.as_table().iter() {
            let table = item.as_table().expect("section must be a table");
            for (field, _) in table.iter() {
                keys.push(format!("{name}.{field}"));
            }
        }
        let expected = all_field_keys();
        assert_eq!(keys, expected, "写出的键必须与 all_field_keys() 完全一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_preserves_comments_order_and_unknown_keys() {
        let dir = temp_dir("preserve");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# 顶部注释\n\
             [client]\n\
             # relay 的说明\n\
             relay = \"old:7000\"   # 行尾注释\n\
             token = \"old-token\"\n\
             \n\
             [unknown]\n\
             future_key = 42\n\
             \n\
             [server]\n\
             listen = \"0.0.0.0:7000\"\n\
             custom = \"keep me\"\n",
        )
        .unwrap();

        let mut cfg = GuiConfig::load(&path).unwrap();
        assert_eq!(cfg.client.relay.as_deref(), Some("old:7000"));
        cfg.client.relay = Some("new:8000".into());
        cfg.client.token = None;
        cfg.server.listen = Some("127.0.0.1:9000".into());
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 顶部注释"), "顶部注释必须保留：{text}");
        assert!(text.contains("# relay 的说明"), "键前注释必须保留：{text}");
        assert!(
            text.contains("relay = \"new:8000\"   # 行尾注释"),
            "行尾注释必须保留：{text}"
        );
        assert!(text.contains("[unknown]"), "未识别段必须保留：{text}");
        assert!(text.contains("future_key = 42"), "未识别键必须保留：{text}");
        assert!(
            text.contains("custom = \"keep me\""),
            "段内未知键必须保留：{text}"
        );
        assert!(
            !text.contains("token = \"old-token\""),
            "置 None 的键必须删除：{text}"
        );

        // 键序：relay 仍在 token 之前（token 被删），[unknown] 仍在 [server] 之前。
        let relay_at = text.find("relay =").unwrap();
        let unknown_at = text.find("[unknown]").unwrap();
        let server_at = text.find("[server]").unwrap();
        assert!(
            relay_at < unknown_at && unknown_at < server_at,
            "键序必须保持：{text}"
        );

        // 值确实写进去了，且重新加载后模型一致。
        let reloaded = GuiConfig::load(&path).unwrap();
        assert_eq!(reloaded.client.relay.as_deref(), Some("new:8000"));
        assert_eq!(reloaded.client.token, None);
        assert_eq!(reloaded.server.listen.as_deref(), Some("127.0.0.1:9000"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_parent_dirs_and_leaves_no_temp_file() {
        let dir = temp_dir("mkdir");
        let path = dir.join("deep").join("nested").join("config.toml");
        full_config().save(&path).unwrap();
        assert!(path.exists());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "原子写的临时文件必须已被 rename 掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_error_carries_line_number() {
        let dir = temp_dir("parse");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[client]\nrelay = \"a:1\"\ntoken = \n").unwrap();
        match GuiConfig::load(&path) {
            Err(ConfigError::Parse { line, .. }) => assert_eq!(line, 3, "错误应定位到第 3 行"),
            other => panic!("期望 Parse 错误，实际 {other:?}"),
        }
        // 类型不匹配同样是 Parse 错误，且带行号。
        std::fs::write(&path, "[server]\nx11_port = \"abc\"\n").unwrap();
        match GuiConfig::load(&path) {
            Err(ConfigError::Parse { line, .. }) => assert_eq!(line, 2),
            other => panic!("期望 Parse 错误，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_display_is_chinese() {
        let e = ConfigError::Parse {
            line: 7,
            message: "bad".into(),
        };
        assert!(e.to_string().contains("第 7 行"));
        assert!(e.to_string().contains("配置"));
        assert!(ConfigError::Io("x".into())
            .to_string()
            .contains("配置读写失败"));
    }

    #[test]
    fn saved_file_has_cli_compatible_toml_types() {
        let dir = temp_dir("types");
        let path = dir.join("config.toml");
        full_config().save(&path).unwrap();
        let doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let get = |section: &str, key: &str| doc[section][key].clone();
        assert!(get("client", "display").is_integer(), "display 必须是整数");
        assert!(get("client", "relay").is_str(), "relay 必须是字符串");
        assert!(get("server", "x11_port").is_integer());
        assert!(get("ssh", "reconnect").is_bool(), "reconnect 必须是布尔");
        assert!(
            get("runtime", "diff_min_savings_ratio").is_float(),
            "比例必须是浮点"
        );
        assert!(get("runtime", "heartbeat_interval_secs").is_integer());
        assert!(get("runtime", "flow_control").is_bool());
        assert!(
            get("runtime", "audio_play_command").is_array(),
            "命令必须是数组"
        );
        assert!(get("runtime", "image_recode_codec").is_str());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_every_field_removes_all_keys() {
        let dir = temp_dir("clear");
        let path = dir.join("config.toml");
        full_config().save(&path).unwrap();
        GuiConfig::default().save(&path).unwrap();
        let doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        for (name, item) in doc.as_table().iter() {
            let table = item.as_table().expect("section must stay a table");
            assert!(table.is_empty(), "[{name}] 应已清空");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
