//! 结构测试：把 `src/lib.rs` 里写的分层约定变成会红的断言。
//!
//! 这些规则不是审美偏好，而是这套结构能工作的原因：
//!
//! * `settings`/`launcher` 不依赖表现层，所以领域逻辑可以在没有显示器的机器上跑测试；
//! * `model` 谁都不依赖，所以契约两端都能用它，而它自己不会被 fltk 拖进来；
//! * `ui::*` 不认识 `backend`/`demo`/`app`，所以页面可以在没有网络、没有配置文件
//!   的情况下画出来（`RX11_GUI_DEMO=1` 就是靠这条）；
//! * 适配器不认识界面，所以换一个后端实现不需要动一个像素。
//!
//! 检查方式是**故意做笨的**：读源文件文本、只看 `use` 语句和文件名，不引入任何
//! 依赖。想"更聪明"（解析语法树、上 archunit 那类框架）就会让这个测试本身成为
//! 结构里最重的东西，那不值得。
//!
//! 规则表里出现新模块名时这里会红，这是**有意**的：改依赖方向应该是一次自觉的
//! 决定，而不是顺手 import 一下。

use std::fs;
use std::path::{Path, PathBuf};

/// 一层代码，以及它**不允许**导入的本 crate 模块（不含 `model`：那一层是所有人
/// 共同语言，任何地方都可以用）。
struct Layer {
    label: &'static str,
    /// 相对 `src/` 的路径，文件或目录。
    path: &'static str,
    forbid: &'static [&'static str],
}

/// 自下而上：契约 → 领域 → 表现 → 适配 → 组合根。
/// `app`（组合根）不在表里 —— 它是唯一允许认识所有人的地方。
const LAYERS: &[Layer] = &[
    Layer {
        label: "model（跨层契约）",
        path: "model.rs",
        forbid: &[
            "app", "backend", "demo", "launcher", "settings", "state", "theme", "ui",
        ],
    },
    Layer {
        label: "settings（领域）",
        path: "settings",
        forbid: &["app", "backend", "demo", "state", "theme", "ui"],
    },
    Layer {
        label: "launcher（领域）",
        path: "launcher",
        forbid: &["app", "backend", "demo", "state", "theme", "ui"],
    },
    Layer {
        label: "theme（表现）",
        path: "theme",
        forbid: &[
            "app", "backend", "demo", "launcher", "settings", "state", "ui",
        ],
    },
    Layer {
        label: "state（表现）",
        path: "state.rs",
        forbid: &["app", "backend", "demo", "launcher", "settings", "ui"],
    },
    Layer {
        label: "ui（表现）",
        path: "ui",
        forbid: &["app", "backend", "demo", "launcher", "settings"],
    },
    Layer {
        label: "backend（适配器）",
        path: "backend.rs",
        forbid: &["app", "state", "theme", "ui"],
    },
    Layer {
        label: "demo（适配器）",
        path: "demo.rs",
        forbid: &["app", "state", "theme", "ui"],
    },
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// 目录下全部 `.rs` 文件（递归）；传单个文件时就是它自己。
fn rust_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => panic!("读不到 {}: {e}", dir.display()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// 去掉纯注释行（`//`、`//!`、`///`）之后的代码行。规则检查看代码，不看文档：
/// `model.rs` 的模块文档里写着"不能碰 fltk"，那是说明，不是依赖。
fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|l| !l.trim_start().starts_with("//"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()))
}

/// 收集文件里的 `use` 语句（多行语句拼成一条）。
/// 只认真正的 `use` 语句：行首（忽略缩进）必须是 `use `，所以注释和文档注释里的
/// `use crate::...` 例子不会误伤。
fn use_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        match &mut current {
            None => {
                if trimmed.starts_with("use ") {
                    let mut stmt = trimmed.to_string();
                    if stmt.contains(';') {
                        out.push(stmt);
                    } else {
                        stmt.push(' ');
                        current = Some(stmt);
                    }
                }
            }
            Some(stmt) => {
                stmt.push_str(trimmed);
                stmt.push(' ');
                if trimmed.contains(';') {
                    out.push(std::mem::take(stmt));
                    current = None;
                }
            }
        }
    }
    out
}

/// 一个文件里是否 import 了本 crate 的 `<module>`。
///
/// 必须先确认这条语句真的走 `crate::`：`use fltk::app;` 里的 `app` 是 FLTK 的
/// application 模块，不是本 crate 的 `app`（这一条差点让规则误报 theme 层）。
/// 组导入 `use crate::{model::Health, ui::Page};` 也要能认出来。
fn imports_module(text: &str, module: &str) -> bool {
    let needle = format!("{module}::");
    use_statements(text).iter().any(|stmt| {
        if !stmt.contains("crate::") {
            return false;
        }
        let paths = stmt.replace("crate::", "");
        paths.contains(&needle)
            || paths.contains(&format!("{module};"))
            || paths.contains(&format!("{module},"))
            || paths.contains(&format!("{module}}}"))
    })
}

#[test]
fn dependencies_point_downwards() {
    let src = src_dir();
    let mut violations = Vec::new();
    for layer in LAYERS {
        let root = src.join(layer.path);
        // 路径写错会让这条规则变成"扫了个空目录然后通过"，所以先确认它存在。
        assert!(
            root.exists(),
            "规则表里的 {} 不存在（改了模块名就要同步这张表）",
            root.display()
        );
        for file in rust_files(&root) {
            let text = read(&file);
            for module in layer.forbid {
                if imports_module(&text, module) {
                    violations.push(format!(
                        "{} 导入了 crate::{}（{}）",
                        file.display(),
                        module,
                        layer.label
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "分层依赖被打破：\n  {}\n\n依赖只允许自上而下（见 src/lib.rs 的分层图）。",
        violations.join("\n  ")
    );
}

/// `theme::text` 是文字绘制的唯一入口，而它自己按字体度量算基线。
///
/// FLTK 的框形式 `fl_draw(s, x, y, w, h, align)` 用的是整行高而不是上行高，会让
/// 每段文字偏低一个 descent（用户报过「有些文字的显示位置总是向下了一些」，见
/// DESIGN.md 第 5 条）。所以代码里一旦重新出现 `draw_text2` 就是回归。
/// （只看代码行：文档里可以正当地提到这个函数名。）
#[test]
fn text_is_drawn_only_with_an_explicit_baseline() {
    let mut offenders = Vec::new();
    for file in rust_files(&src_dir()) {
        let text = read(&file);
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains("draw_text2") {
                offenders.push(format!("{}:{}", file.display(), n + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "出现了 FLTK 框形式画字（基线会低一个 descent）：\n  {}",
        offenders.join("\n  ")
    );
}

/// 模型层不认识界面，也不知道 fltk 的存在。
#[test]
fn model_layer_stays_dependency_free() {
    let text = read(&src_dir().join("model.rs"));
    let fltk_uses: Vec<&str> = code_lines(&text).filter(|l| l.contains("fltk")).collect();
    assert!(
        fltk_uses.is_empty(),
        "model.rs 不能出现 fltk：契约层不是画布层，但它有：{fltk_uses:?}"
    );
    let imports: Vec<String> = use_statements(&text)
        .into_iter()
        .filter(|s| s.contains("crate::"))
        .collect();
    assert!(
        imports.is_empty(),
        "model.rs 不能依赖本 crate 其他模块，但它 import 了：{imports:?}"
    );
}

/// 入口只有一行：模块树归库所有。历史上 `lib.rs` 两个模块、`main.rs` 六个模块，
/// 从哪个入口都看不出架构，测试和 example 也够不到真正的模块。
#[test]
fn the_binary_entry_point_owns_no_modules() {
    let text = read(&src_dir().join("main.rs"));
    let declares: Vec<&str> = text
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("mod ") || l.starts_with("pub mod "))
        .collect();
    assert!(
        declares.is_empty(),
        "main.rs 只应该调用 rx11_gui::run()，不该声明模块：{declares:?}"
    );
}

/// 拆分是为了让每个文件只有一个话题；1800 行的文件回归之前先在这里红一次。
#[test]
fn no_source_file_is_a_dumping_ground() {
    const LIMIT: usize = 1000;
    let mut big = Vec::new();
    for file in rust_files(&src_dir()) {
        let n = read(&file).lines().count();
        if n > LIMIT {
            big.push(format!("{}: {n} 行", file.display(), n = n));
        }
    }
    assert!(
        big.is_empty(),
        "单文件超过 {LIMIT} 行，先按职责拆开：\n  {}",
        big.join("\n  ")
    );
}

/// 每个 `.rs` 文件都必须被它的父模块声明，否则就是重构留下的孤儿文件
/// （不会被编译、也不会被发现）。
#[test]
fn every_file_is_declared_by_its_parent_module() {
    let src = src_dir();
    let mut orphans = Vec::new();
    for file in rust_files(&src) {
        let name = file.file_stem().unwrap().to_string_lossy().to_string();
        if matches!(name.as_str(), "main" | "lib" | "mod") {
            continue;
        }
        let dir = file.parent().unwrap();
        // 根模块：`src/foo.rs` 由 `src/lib.rs` 声明；其余由同目录的 `mod.rs`
        // 或兄弟文件 `ui.rs` 声明。
        let parent = if dir == src {
            src.join("lib.rs")
        } else {
            let candidate = dir.join("mod.rs");
            if candidate.is_file() {
                candidate
            } else {
                dir.with_extension("rs")
            }
        };
        if !parent.is_file() {
            orphans.push(format!("{}: 找不到父模块文件", file.display()));
            continue;
        }
        let declared = read(&parent);
        let needle_mod = format!("mod {name};");
        if !declared.contains(&needle_mod) {
            orphans.push(format!(
                "{}: {} 里没有 `{needle_mod}`",
                file.display(),
                parent.display()
            ));
        }
    }
    assert!(
        orphans.is_empty(),
        "有文件没被任何模块声明（编译不到）：\n  {}",
        orphans.join("\n  ")
    );
}
