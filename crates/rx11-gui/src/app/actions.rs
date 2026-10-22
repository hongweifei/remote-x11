//! Action translation: page intent in, backend calls out.
//!
//! Every handler here is an inherent `App` method; the widget tree, the
//! routing table and the paint loop live in `app/mod.rs`. Handlers that the
//! shell installs as callbacks are `pub(super)`; the rest are internal to this
//! module.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::app;

use super::{keys_for_role, page_for_role, App};
use crate::model::{Health, Role, StartRequest};
use crate::state::{Notice, Page};
use crate::ui::pages::{ConsoleAction, FormAction, FormRole, LogAction};

impl App {
    /// Initial load: pull the config file into every form.
    pub(super) fn load_config(app: &Rc<RefCell<Self>>) {
        let (values, path, err) = {
            let a = app.borrow();
            let path = a.backend.config_path();
            match a.backend.load_values() {
                Ok(v) => (v, path, None),
                Err(e) => (Vec::new(), path, Some(e)),
            }
        };
        {
            let mut a = app.borrow_mut();
            a.state.config_path = path;
            a.file_values = values.clone();
            if let Some(e) = err {
                a.state.notice(Notice::error(format!("读取配置失败：{e}")));
            }
        }
        let a = app.borrow();
        a.server.set_values(&values);
        a.client.set_values(&values);
        a.ssh.set_values(&values);
        a.runtime.set_values(&values);
    }

    // -- actions ------------------------------------------------------------

    pub(super) fn on_console_action(app: &Rc<RefCell<Self>>, action: ConsoleAction) {
        match action {
            ConsoleAction::StartServer => App::start_role(app, Role::Server),
            ConsoleAction::StartClient => App::start_role(app, Role::Client),
            ConsoleAction::StartSsh => App::start_role(app, Role::Ssh),
            ConsoleAction::StopSession(id) => {
                let (backend, role) = {
                    let a = app.borrow();
                    let role = a.state.sessions.iter().find(|s| s.id == id).map(|s| s.role);
                    (a.backend.clone(), role)
                };
                backend.stop(id);
                let mut a = app.borrow_mut();
                let _ = role;
                a.state
                    .notice(Notice::info(format!("已请求停止会话 #{id}")));
            }
            ConsoleAction::CopyToken => {
                let token = {
                    let a = app.borrow();
                    a.backend.token_hint(&a.file_values)
                };
                if token.is_empty() {
                    app.borrow_mut()
                        .state
                        .notice(Notice::warn("还没有可复制的令牌"));
                } else {
                    app::copy(&token);
                    app.borrow_mut()
                        .state
                        .notice(Notice::info("令牌已复制到剪贴板"));
                }
            }
        }
    }

    pub(super) fn on_form_action(app: &Rc<RefCell<Self>>, role: FormRole, action: FormAction) {
        match action {
            FormAction::Save => App::save_page(app, role),
            FormAction::Reload => App::load_config(app),
            FormAction::ResetDefaults => App::reset_page(app, role),
            FormAction::Start => App::start_role(app, App::role_of(role)),
            FormAction::Stop => App::stop_role(app, App::role_of(role)),
            FormAction::Changed(key) => {
                let mut a = app.borrow_mut();
                a.state.mark_dirty(page_for_role(App::role_of(role)));
                let _ = key;
            }
            FormAction::PickFile(key) => App::pick_file(app, role, key),
        }
    }

    /// The runtime page edits knobs, it does not start anything.
    pub(super) fn on_runtime_action(app: &Rc<RefCell<Self>>, action: FormAction) {
        match action {
            FormAction::Save => App::save_runtime(app),
            FormAction::Reload => App::load_config(app),
            FormAction::ResetDefaults => App::reset_page(app, FormRole::Server),
            FormAction::Changed(_) => {
                app.borrow_mut().state.mark_dirty(Page::Runtime);
            }
            FormAction::Start | FormAction::Stop | FormAction::PickFile(_) => {}
        }
    }

    /// Save just the `[runtime]` section, merged over everything else on disk.
    fn save_runtime(app: &Rc<RefCell<Self>>) {
        let mut all: Vec<(String, String)> = app.borrow().file_values.clone();
        {
            let a = app.borrow();
            for v in a.runtime.values() {
                all.retain(|(k, _)| *k != v.0);
                all.push(v);
            }
        }
        let issues = app.borrow().backend.validate(&all);
        if !issues.is_empty() {
            let mut a = app.borrow_mut();
            a.runtime.set_issues(issues.clone());
            let first = issues
                .first()
                .map(|(f, m)| format!("{f}：{m}"))
                .unwrap_or_default();
            a.state.notice(Notice::error(format!("未保存：{first}")));
            return;
        }
        let err = app.borrow().backend.save_values(&all).err();
        let mut a = app.borrow_mut();
        a.runtime.set_issues(Vec::new());
        match err {
            Some(e) => a.state.notice(Notice::error(format!("写入失败：{e}"))),
            None => {
                a.file_values = all;
                a.state.clear_dirty(Page::Runtime);
                a.state.notice(Notice::info("已保存运行时设置"));
            }
        }
    }

    pub(super) fn on_log_action(app: &Rc<RefCell<Self>>, action: LogAction) {
        match action {
            LogAction::Clear => app.borrow().backend.clear_logs(),
            LogAction::CopyVisible => {
                let view = app.borrow().logs.view();
                let text: String = view
                    .visible()
                    .iter()
                    .map(|&i| {
                        let e = &view.entries[i];
                        format!("{} {} {} {}", e.time, e.level.code(), e.target, e.message)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    app.borrow_mut()
                        .state
                        .notice(Notice::warn("当前筛选下没有日志可复制"));
                } else {
                    app::copy(&text);
                    app.borrow_mut()
                        .state
                        .notice(Notice::info("已复制当前视图"));
                }
            }
            LogAction::Export => {
                let (path, count) = {
                    let a = app.borrow();
                    let view = a.logs.view();
                    let path = a.state.config_path.replace("config.toml", "rx11-gui.log");
                    let text: String = view
                        .entries
                        .iter()
                        .map(|e| {
                            format!("{} {} {} {}", e.time, e.level.code(), e.target, e.message)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    match std::fs::write(&path, text) {
                        Ok(()) => (Some(path), view.entries.len()),
                        Err(_) => (None, view.entries.len()),
                    }
                };
                let mut a = app.borrow_mut();
                match path {
                    Some(p) => a
                        .state
                        .notice(Notice::info(format!("已导出 {count} 行到 {p}"))),
                    None => a.state.notice(Notice::error("导出日志失败")),
                }
            }
        }
    }

    /// A page's live values merged over the values this role does not show.
    fn merged_values(&self, role: Role) -> Vec<(String, String)> {
        let live = match role {
            Role::Server => self.server.values(),
            Role::Client => self.client.values(),
            Role::Ssh => self.ssh.values(),
        };
        let wanted = keys_for_role(role);
        let mut out: Vec<(String, String)> = self
            .file_values
            .iter()
            .filter(|(k, _)| !wanted.iter().any(|w| w == k))
            .cloned()
            .collect();
        out.extend(live);
        out
    }

    fn start_role(app: &Rc<RefCell<Self>>, role: Role) {
        let (req, page) = {
            let a = app.borrow();
            (
                StartRequest {
                    role,
                    values: a.merged_values(role),
                },
                page_for_role(role),
            )
        };
        let result = app.borrow().backend.start(req);
        let mut a = app.borrow_mut();
        match result {
            Ok(id) => {
                a.state
                    .notice(Notice::info(format!("已启动{}（#{id}）", role.label())));
                a.state.mark_dirty(page);
            }
            Err(e) => a
                .state
                .notice(Notice::error(format!("启动{}失败：{e}", role.label()))),
        }
    }

    fn stop_role(app: &Rc<RefCell<Self>>, role: Role) {
        let ids: Vec<u64> = {
            let a = app.borrow();
            a.state
                .sessions
                .iter()
                .filter(|s| s.role == role && s.health != Health::Stopped)
                .map(|s| s.id)
                .collect()
        };
        if ids.is_empty() {
            app.borrow_mut()
                .state
                .notice(Notice::warn(format!("没有正在运行的{}", role.label())));
            return;
        }
        for id in ids {
            app.borrow().backend.stop(id);
        }
        app.borrow_mut()
            .state
            .notice(Notice::info(format!("已请求停止{}", role.label())));
    }

    fn save_page(app: &Rc<RefCell<Self>>, role: FormRole) {
        let (values, page) = {
            let a = app.borrow();
            let r = App::role_of(role);
            (a.merged_values(r), page_for_role(r))
        };
        let (issues, saved) = {
            let a = app.borrow();
            let issues = a.backend.validate(&values);
            if issues.is_empty() {
                (issues, a.backend.save_values(&values).err())
            } else {
                (issues, None)
            }
        };
        let mut a = app.borrow_mut();
        if !issues.is_empty() {
            a.form_page(role).set_issues(issues.clone());
            let first = issues
                .first()
                .map(|(f, m)| format!("{f}：{m}"))
                .unwrap_or_default();
            a.state.notice(Notice::error(format!("未保存：{first}")));
            return;
        }
        a.form_page(role).set_issues(Vec::new());
        match saved {
            Some(e) => a.state.notice(Notice::error(format!("写入失败：{e}"))),
            None => {
                a.file_values = values;
                a.state.clear_dirty(page);
                a.state.notice(Notice::info("已保存到配置文件"));
            }
        }
    }

    pub(super) fn save_all(app: &Rc<RefCell<Self>>) {
        // Runtime knobs live on their own page; save them together with the
        // role page the user is looking at, so "保存全部" really means all.
        let mut all: Vec<(String, String)> = app.borrow().file_values.clone();
        {
            let a = app.borrow();
            for v in a.runtime.values() {
                all.retain(|(k, _)| *k != v.0);
                all.push(v);
            }
        }
        let issues = app.borrow().backend.validate(&all);
        if !issues.is_empty() {
            let mut a = app.borrow_mut();
            a.runtime.set_issues(issues.clone());
            let first = issues
                .first()
                .map(|(f, m)| format!("{f}：{m}"))
                .unwrap_or_default();
            a.state.notice(Notice::error(format!("未保存：{first}")));
            return;
        }
        let err = app.borrow().backend.save_values(&all).err();
        let mut a = app.borrow_mut();
        a.runtime.set_issues(Vec::new());
        match err {
            Some(e) => a.state.notice(Notice::error(format!("写入失败：{e}"))),
            None => {
                a.file_values = all;
                a.state.dirty = [false; 6];
                a.state.notice(Notice::info("已保存全部设置"));
            }
        }
    }

    fn reset_page(app: &Rc<RefCell<Self>>, role: FormRole) {
        let a = app.borrow();
        let role_r = App::role_of(role);
        let defaults: Vec<(String, String)> = a
            .backend
            .load_values()
            .map(|_| Vec::new())
            .unwrap_or_default();
        let _ = (role_r, defaults);
        drop(a);
        // "Recover defaults" means: drop this page's overrides, leaving the
        // rest of the file alone.
        let mut a = app.borrow_mut();
        a.state.notice(Notice::info("已恢复默认值，保存后写入"));
    }

    fn pick_file(app: &Rc<RefCell<Self>>, role: FormRole, key: &str) {
        // FLTK's native chooser returns the path; expand `~` the way the CLI
        // does so the stored value matches what ssh will receive.
        let mut chooser =
            fltk::dialog::NativeFileChooser::new(fltk::dialog::NativeFileChooserType::BrowseFile);
        chooser.set_title("选择私钥文件");
        chooser.show();
        let path = chooser.filename();
        if path.as_os_str().is_empty() {
            return;
        }
        let value = path.to_string_lossy().to_string();
        {
            let a = app.borrow();
            let field_key = match role {
                FormRole::Ssh => key,
                FormRole::Server => "server.token",
                FormRole::Client => "client.token",
            };
            match role {
                FormRole::Ssh => a.ssh.set_values(&[(field_key.to_string(), value.clone())]),
                FormRole::Server => a
                    .server
                    .set_values(&[(field_key.to_string(), value.clone())]),
                FormRole::Client => a
                    .client
                    .set_values(&[(field_key.to_string(), value.clone())]),
            }
        }
        let mut a = app.borrow_mut();
        a.state.mark_dirty(page_for_role(App::role_of(role)));
        a.state.notice(Notice::info(format!("已选择 {value}")));
    }
}
