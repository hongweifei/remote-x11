//! rx11 控制台 —— FLTK 原生 GUI 的入口。
//!
//! 所有模块都在库里（分层说明见 [`rx11_gui`] 的 crate 文档），这里只做一件事：
//! 调 [`rx11_gui::run`]。
//!
//! 用法：
//!   rx11-gui                  正常启动：读 `$HOME/.config/rx11/config.toml`，
//!                             在进程内嵌入 server/client
//!   RX11_GUI_DEMO=1 rx11-gui  演示模式：固定样例数据，不读写配置、不连网络
//!   RX11_GUI_PAGE=<页>        直开某一页：console|server|client|ssh|runtime|logs
//!   RX11_GUI_SCALE=1.25 …     覆盖界面缩放（默认跟随 GDK_SCALE/QT_SCALE_FACTOR）

fn main() {
    rx11_gui::run();
}
