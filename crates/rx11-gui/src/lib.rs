//! rx11 控制台 —— FLTK 原生 GUI，叠在 protocol-v1 协议栈上。
//!
//! 整个应用都在这个库里，`main.rs` 只是一行入口。这样切是为了让 `tests/` 和
//! `examples/` 能拿到真正的模块，而不是把全部代码关在 bin target 里（历史上
//! 就是那样，`lib.rs` 只有两个模块、`main.rs` 有六个，看哪个入口都看不出架
//! 构）。
//!
//! # 分层
//!
//! 依赖只允许自上而下、单向：
//!
//! ```text
//! settings / launcher      领域与数据：config.toml 模型、进程内会话编排、日志环
//!         ↓
//!       model              跨层契约：Backend 端口 + 两端交换的数据类型
//!         ↓
//!  theme / state / ui      表现层：视觉规范、视图状态、页面与自绘控件
//!         ↓
//!  backend / demo          端口的两种实现：真实数据层 / 演示样例数据
//!         ↓
//!        app               组合根：窗口、页面路由、轮询、动作翻译
//! ```
//!
//! 几条硬约束（由 `tests/architecture.rs` 断言，改坏会让它变红）：
//!
//! * `model` 不依赖本 crate 任何其他模块，也不碰 fltk —— 契约层不是画布层；
//! * `ui::*` 不认识 `backend` / `demo` / `app`，只负责画和抛意图（见 `ui::pages`）；
//! * `backend` / `demo` 只依赖 `model` 和领域层，不知道界面长什么样；
//! * `settings` / `launcher` 不依赖表现层，也就能在没有显示器的环境里跑测试。
//!
//! # 模块
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`settings`] | `config.toml` 的类型化双向模型（读写/校验/扁平键值视图） |
//! | [`launcher`] | 进程内嵌入 relay/client/SSH 隧道的会话编排与日志环 |
//! | [`model`] | [`Backend`] 端口与它交换的数据类型 |
//! | [`theme`] | 调色板/字阶/布局常量的唯一来源，以及全部绘制原语 |
//! | [`state`] | UI 线程独占的视图状态（当前页、会话行、示波器采样、提示） |
//! | [`ui`] | 外壳（侧栏/顶栏/状态栏）、自绘控件、示波器、六个页面 |
//! | [`backend`] | [`Backend`] 的真实实现：`settings` + `launcher` + 日志环 |
//! | [`demo`] | [`Backend`] 的样例数据实现（feature `demo`，默认开） |
//! | [`app`] | 组合根：窗口、路由、0.5 s 轮询、动作 → 后端调用 |
//!
//! # 运行
//!
//! 二进制只调用 [`run`]。环境变量见 `main.rs` 顶部：`RX11_GUI_DEMO`、
//! `RX11_GUI_PAGE`、`RX11_GUI_SCALE`。

pub mod app;
pub mod backend;
#[cfg(feature = "demo")]
pub mod demo;
pub mod launcher;
pub mod model;
pub mod settings;
pub mod state;
pub mod theme;
pub mod ui;

use std::rc::Rc;

use model::Backend;

/// 默认窗口尺寸。控制台按 1180×800 排版，再小就靠页面内部滚动，不缩放字号。
const WINDOW: (i32, i32) = (1180, 800);

/// 演示模式开关：`RX11_GUI_DEMO=1`（也接受任何非空、非 `0` 的值）。
fn demo_requested() -> bool {
    std::env::var("RX11_GUI_DEMO")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
}

/// 启动控制台（阻塞到窗口关闭）。
pub fn run() {
    // FLTK 必须先初始化：配色方案、全局字体和屏幕缩放都要在任何 widget
    // 存在之前定下来（`theme::init` 依赖已初始化的 FLTK）。
    let _fltk = fltk::app::App::default();
    theme::init();

    let is_demo = demo_requested();
    let title = if is_demo {
        "rx11 控制台（演示数据）"
    } else {
        "rx11 控制台"
    };
    app::App::run(build_backend(is_demo), title, WINDOW.0, WINDOW.1);
}

/// 选一个 [`Backend`] 实现。
///
/// 关掉 `demo` feature 后 `RX11_GUI_DEMO=1` 没有对应实现：明确报错退出，而不是
/// 悄悄退回真实数据层（那会让取证脚本拍到一份"看起来正常"的真实空数据）。
fn build_backend(is_demo: bool) -> Rc<dyn Backend> {
    if is_demo {
        #[cfg(feature = "demo")]
        return Rc::new(demo::seeded());
        #[cfg(not(feature = "demo"))]
        {
            eprintln!(
                "rx11-gui: RX11_GUI_DEMO 已设置，但这个构建没编进演示数据\
                 （feature \"demo\" 关闭）。去掉该变量，或用默认 feature 重新编译。"
            );
            std::process::exit(2);
        }
    }
    Rc::new(backend::LauncherBackend::new())
}
