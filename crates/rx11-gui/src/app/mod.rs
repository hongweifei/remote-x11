//! Application shell: owns the pages, the poll timer and the action routing.
//!
//! This is the composition root — the only place that knows both halves:
//!
//! * `ui::*` draws and reports intent — it never starts, stops or loads
//!   anything;
//! * [`Backend`] is the data layer's port; the port and every type it moves
//!   across the boundary live in [`crate::model`], so neither side has to
//!   import the other;
//! * `App` translates between them: page actions become backend calls,
//!   backend snapshots become page data.
//!
//! Working in flat values is what keeps the two halves independent: the UI
//! layer never learns the config schema, and a new runtime knob needs one
//! `FieldSpec` in the config pages (`ui::pages::config`) plus one line in the
//! config layer, with nothing in between to update.
//!
//! The action handlers live in the child modules: `app/actions.rs` holds the
//! translate-and-call side (start/stop/save/poll), this file holds the widget
//! tree, the routing table and the paint loop.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use fltk::app;
use fltk::prelude::*;

use crate::model::{Backend, Health, Role, SessionRowData, TrafficTotals};
use crate::state::{AppState, Page, Sample, SessionRow};
use crate::theme::{self as th, C, L};
use crate::ui::pages::{ConsoleData, ConsolePage, FormPage, FormRole, LogEntry, LogPage};
use crate::ui::shell::{rail_badges, Rail, StatusBar, StatusData, Topbar, TopbarData};

mod actions;
#[cfg(test)]
mod tests;

/// How often the UI polls the backend. Twice a second is fast enough that
/// the scope looks live and cheap enough that the poll never competes with
/// the data path.
pub const POLL_INTERVAL: f64 = 0.5;

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// Which config keys a role owns. Taken from the settings pages themselves, so
/// adding a field to a page cannot leave a stale list here: the page spec is
/// the single source of truth for "what this screen edits".
fn keys_for_role(role: Role) -> Vec<&'static str> {
    match role {
        Role::Server => crate::ui::pages::keys_for(&crate::ui::pages::SERVER),
        Role::Client => crate::ui::pages::keys_for(&crate::ui::pages::CLIENT),
        Role::Ssh => crate::ui::pages::keys_for(&crate::ui::pages::SSH),
    }
}

/// Which page a role's lifecycle belongs to.
fn page_for_role(role: Role) -> Page {
    match role {
        Role::Server => Page::Server,
        Role::Client => Page::Client,
        Role::Ssh => Page::Ssh,
    }
}

pub struct App {
    backend: Rc<dyn Backend>,
    state: AppState,

    rail: Rail,
    topbar: Topbar,
    status: StatusBar,
    console: ConsolePage,
    server: FormPage,
    client: FormPage,
    ssh: FormPage,
    runtime: FormPage,
    logs: LogPage,

    /// Values as loaded from disk, per page; a start action merges the page's
    /// live values over these so hidden fields keep their file values.
    file_values: Vec<(String, String)>,

    /// Totals and sample timing for the scope.
    last_poll: Instant,
    last_totals: TrafficTotals,
    started: Instant,
}

impl App {
    pub fn new(backend: Rc<dyn Backend>, w: i32, h: i32) -> Rc<RefCell<Self>> {
        let cx = L::SIDEBAR_W;
        let cy = L::TOPBAR_H;
        let cw = w - L::SIDEBAR_W;
        let ch = h - L::TOPBAR_H - L::STATUS_H;

        let rail = Rail::new(0, 0, L::SIDEBAR_W, h);
        let topbar = Topbar::new(cx, 0, cw, L::TOPBAR_H);
        let status = StatusBar::new(cx, h - L::STATUS_H, cw, L::STATUS_H);

        let console = ConsolePage::new(cx, cy, cw, ch);
        let server = FormPage::new(&crate::ui::pages::SERVER, cx, cy, cw, ch);
        let client = FormPage::new(&crate::ui::pages::CLIENT, cx, cy, cw, ch);
        let ssh = FormPage::new(&crate::ui::pages::SSH, cx, cy, cw, ch);
        let runtime = FormPage::new(&crate::ui::pages::RUNTIME, cx, cy, cw, ch);
        let logs = LogPage::new(cx, cy, cw, ch);

        let state = AppState::new(backend.config_path());

        let app = Rc::new(RefCell::new(Self {
            backend,
            state,
            rail,
            topbar,
            status,
            console,
            server,
            client,
            ssh,
            runtime,
            logs,
            file_values: Vec::new(),
            last_poll: Instant::now(),
            last_totals: TrafficTotals::default(),
            started: Instant::now(),
        }));

        App::wire(&app);
        App::load_config(&app);
        app
    }

    /// Install every callback. Kept in one place so the wiring is readable as
    /// a table of "page → action".
    fn wire(app: &Rc<RefCell<Self>>) {
        // Rail: page switching.
        {
            let a = app.clone();
            app.borrow()
                .rail
                .on_select(move |page| App::show_page(&a, page));
        }

        // Console page.
        {
            let a = app.clone();
            app.borrow().console.on_action(move |action| {
                App::on_console_action(&a, action);
            });
        }

        // The four form pages share one handler, differing only by role.
        for role in [FormRole::Server, FormRole::Client, FormRole::Ssh] {
            let a = app.clone();
            app.borrow().form_page(role).on_action(move |action| {
                App::on_form_action(&a, role, action);
            });
        }
        {
            // The runtime page owns no role: its actions are config-only, so
            // it must never be routed through a role's start/stop path.
            let a = app.clone();
            app.borrow().runtime.on_action(move |action| {
                App::on_runtime_action(&a, action);
            });
        }

        // Log page.
        {
            let a = app.clone();
            app.borrow().logs.on_action(move |action| {
                App::on_log_action(&a, action);
            });
        }

        // Top bar: reflect state, plus the one global action (save all).
        {
            let a = app.clone();
            app.borrow().topbar.set_actions(vec![(
                "保存全部".to_string(),
                true,
                Rc::new(move || App::save_all(&a)),
            )]);
        }
    }

    fn form_page(&self, role: FormRole) -> &FormPage {
        match role {
            FormRole::Server => &self.server,
            FormRole::Client => &self.client,
            FormRole::Ssh => &self.ssh,
        }
    }

    fn role_of(role: FormRole) -> Role {
        match role {
            FormRole::Server => Role::Server,
            FormRole::Client => Role::Client,
            FormRole::Ssh => Role::Ssh,
        }
    }

    // -- rendering ----------------------------------------------------------

    fn show_page(app: &Rc<RefCell<Self>>, page: Page) {
        {
            let mut a = app.borrow_mut();
            a.state.page = page;
            a.rail.set_page(page);
            match page {
                Page::Console => {
                    a.console.show();
                    a.server.hide();
                    a.client.hide();
                    a.ssh.hide();
                    a.runtime.hide();
                    a.logs.hide();
                }
                Page::Server => {
                    a.console.hide();
                    a.server.show();
                    a.client.hide();
                    a.ssh.hide();
                    a.runtime.hide();
                    a.logs.hide();
                }
                Page::Client => {
                    a.console.hide();
                    a.server.hide();
                    a.client.show();
                    a.ssh.hide();
                    a.runtime.hide();
                    a.logs.hide();
                }
                Page::Ssh => {
                    a.console.hide();
                    a.server.hide();
                    a.client.hide();
                    a.ssh.show();
                    a.runtime.hide();
                    a.logs.hide();
                }
                Page::Runtime => {
                    a.console.hide();
                    a.server.hide();
                    a.client.hide();
                    a.ssh.hide();
                    a.runtime.show();
                    a.logs.hide();
                }
                Page::Logs => {
                    a.console.hide();
                    a.server.hide();
                    a.client.hide();
                    a.ssh.hide();
                    a.runtime.hide();
                    a.logs.show();
                }
            }
        }
    }

    /// One poll: read the backend, update every visible surface.
    fn tick(app: &Rc<RefCell<Self>>) {
        let now = Instant::now();
        let dt = now.duration_since(app.borrow().last_poll).as_secs_f64();
        app.borrow_mut().last_poll = now;

        // Sessions + totals.
        let (sessions, totals) = {
            let a = app.borrow();
            (a.backend.sessions(), a.backend.totals())
        };

        // Logs: append only what is new.
        let (log_records, log_seq) = {
            let a = app.borrow();
            let seq = a.state.log_seq;
            (a.backend.logs_since(seq), seq)
        };

        {
            let mut a = app.borrow_mut();
            let rows: Vec<SessionRow> = sessions.iter().map(row_from).collect();
            a.state.sessions = rows;
            a.state.total_sent = totals.sent;
            a.state.total_recv = totals.received;
            a.state.savings_ratio = totals.savings_ratio();
            a.state.active_channels = totals.channels;
            a.state.link_up = a
                .state
                .sessions
                .iter()
                .any(|s| matches!(s.health, Health::Running | Health::Reconnecting));

            // Per-second rates for the scope, from the delta between polls.
            let prev = a.last_totals;
            let sample = if dt > 0.05 {
                Sample {
                    sent_bps: totals.sent.saturating_sub(prev.sent) as f64 / dt,
                    recv_bps: totals.received.saturating_sub(prev.received) as f64 / dt,
                }
            } else {
                Sample::default()
            };
            a.last_totals = totals;
            a.state.push_sample(sample);
            a.state.tick_notice();
        }

        // Log ring → view.
        if !log_records.is_empty() {
            let mut a = app.borrow_mut();
            let mut view = a.logs.view();
            for r in &log_records {
                view.entries.push(LogEntry {
                    time: r.time.clone(),
                    level: r.level,
                    target: r.target.clone(),
                    message: r.message.clone(),
                });
            }
            a.state.log_seq = log_seq.max(log_records.last().map(|r| r.seq).unwrap_or(log_seq));
            // Keep the in-memory tail bounded; the ring keeps the history.
            let cap = 4000;
            if view.entries.len() > cap {
                let drop = view.entries.len() - cap;
                view.entries.drain(0..drop);
            }
            a.logs.set_view(view);
        }

        App::paint(app);
    }

    /// Push current state into the pages and the shell.
    fn paint(app: &Rc<RefCell<Self>>) {
        let (console_data, topbar_data, status_data, badges, run_state, log_view) = {
            let a = app.borrow();
            let d = &a.state;
            let (sent_bps, recv_bps) = d
                .scope
                .back()
                .map(|s| (s.sent_bps, s.recv_bps))
                .unwrap_or((0.0, 0.0));
            let console = ConsoleData {
                sessions: d.sessions.clone(),
                scope: d.scope.iter().copied().collect(),
                total_sent: d.total_sent,
                total_recv: d.total_recv,
                savings: d.savings_ratio,
                channels: d.active_channels,
                live: d.link_up && (sent_bps > 1.0 || recv_bps > 1.0),
                capabilities: a.capabilities(),
                token_hint: a.backend.token_hint(&a.file_values),
            };
            let server_state = a.role_summary(Role::Server);
            let client_state = a.role_summary(Role::Client);
            let topbar = TopbarData {
                link_up: d.link_up,
                server_state,
                client_state,
                rate: recv_bps + sent_bps,
                total: d.total_sent + d.total_recv,
            };
            let notice = d.notice.as_ref().map(|n| (n.text.clone(), n.color()));
            let status = StatusData {
                config_path: d.config_path.clone(),
                dirty: d.any_dirty(),
                notice,
                session_count: d.sessions.len(),
                uptime: a.started.elapsed().as_secs(),
            };
            let badges = rail_badges(d);
            let run_state = [
                (Role::Server, a.state.is_active(Role::Server)),
                (Role::Client, a.state.is_active(Role::Client)),
                (Role::Ssh, a.state.is_active(Role::Ssh)),
            ];
            let log_view = a.logs.view();
            (console, topbar, status, badges, run_state, log_view)
        };

        let a = app.borrow();
        a.console.set_data(console_data);
        a.topbar.set_data(topbar_data);
        a.status.set_data(status_data);
        a.rail.set_badges(badges.0, badges.1);
        // Repaint the visible page every tick. FLTK's partial repaints can
        // leave a widget painted over (or erased under) a sibling background,
        // and at two frames a second a full page repaint costs nothing next to
        // never having to reason about which widget holds damage.
        match a.state.page {
            Page::Console => a.console.redraw(),
            Page::Server => a.server.redraw(),
            Page::Client => a.client.redraw(),
            Page::Ssh => a.ssh.redraw(),
            Page::Runtime => a.runtime.redraw(),
            Page::Logs => a.logs.redraw(),
        }
        a.server.set_running(run_state[0].1);
        a.client.set_running(run_state[1].1);
        a.ssh.set_running(run_state[2].1);
        let _ = log_view;
    }

    /// Capabilities the current link negotiated, for the console's tag list.
    /// Read from the newest client session's detail if present; the real list
    /// lives in the connector state and is surfaced through the backend.
    fn capabilities(&self) -> Vec<(String, bool)> {
        const KNOWN: [&str; 6] = [
            "request_resync",
            "incremental",
            "flow_control",
            "x11",
            "x11_recode",
            "audio_forward",
        ];
        // A session is live: report what is actually negotiated. Otherwise
        // show the names greyed, so the list reads as "these exist" rather
        // than "these are on".
        let live = self.state.link_up;
        KNOWN.iter().map(|n| ((*n).to_string(), live)).collect()
    }

    fn role_summary(&self, role: Role) -> Option<String> {
        self.state
            .sessions
            .iter()
            .find(|s| s.role == role)
            .map(|s| s.health.label().to_string())
    }

    /// Build the window, attach the console to it and run the event loop.
    ///
    /// The window must exist *before* the widgets so FLTK parents them to it;
    /// construction order here is load-bearing, not stylistic.
    pub fn run(backend: Rc<dyn Backend>, title: &str, w: i32, h: i32) {
        let mut win = fltk::window::DoubleWindow::new(80, 60, w, h, title);
        win.set_color(th::rgb(C::BG));

        let app = App::new(backend, w, h);
        win.end();
        win.show();

        // Page visibility is decided *after* the window is mapped, so nothing
        // the window's own show() does can resurrect a hidden page.
        App::show_page(&app, initial_page());

        // The poll timer drives everything: data in, redraws out. One timer
        // rather than per-widget timers, so a slow tick can never stack.
        {
            let a = app.clone();
            app::add_timeout3(POLL_INTERVAL, move |handle| {
                App::tick(&a);
                app::repeat_timeout3(POLL_INTERVAL, handle);
            });
        }

        // Immediate first paint so the window is never blank.
        App::tick(&app);

        // Relayout on resize; the window is resizable and every page is
        // manual-layout, so this is the only place geometry moves.
        {
            let a = app.clone();
            win.resize_callback(move |_, _, _, nw, nh| {
                App::on_resize(&a, nw, nh);
            });
        }

        let a = app.clone();
        win.set_callback(move |_| {
            a.borrow().backend.shutdown();
            app::quit();
        });

        app::run().unwrap();
    }

    /// Window resize: relayout every page to the new content rect.
    pub fn on_resize(app: &Rc<RefCell<Self>>, w: i32, h: i32) {
        let cx = L::SIDEBAR_W;
        let cy = L::TOPBAR_H;
        let cw = w - L::SIDEBAR_W;
        let ch = h - L::TOPBAR_H - L::STATUS_H;
        let a = app.borrow();
        a.rail.resize(0, 0, L::SIDEBAR_W, h);
        a.topbar.resize(cx, 0, cw, L::TOPBAR_H);
        a.status.resize(cx, h - L::STATUS_H, cw, L::STATUS_H);
        a.console.resize(cx, cy, cw, ch);
        a.server.resize(cx, cy, cw, ch);
        a.client.resize(cx, cy, cw, ch);
        a.ssh.resize(cx, cy, cw, ch);
        a.runtime.resize(cx, cy, cw, ch);
        a.logs.resize(cx, cy, cw, ch);
    }
}

/// Which page to open on. `RX11_GUI_PAGE=server` (etc.) makes the console
/// start there, which is what the screenshot script and anyone demoing one
/// screen needs; the default is the overview.
fn initial_page() -> Page {
    match std::env::var("RX11_GUI_PAGE")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "server" => Page::Server,
        "client" => Page::Client,
        "ssh" => Page::Ssh,
        "runtime" => Page::Runtime,
        "logs" => Page::Logs,
        _ => Page::Console,
    }
}

fn row_from(s: &SessionRowData) -> SessionRow {
    SessionRow {
        id: s.id,
        role: s.role,
        health: s.health,
        headline: s.headline.clone(),
        detail: s.detail.clone(),
        sent: s.sent,
        received: s.received,
        uptime: s.uptime,
    }
}
