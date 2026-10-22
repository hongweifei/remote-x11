//! The window shell: sidebar rail, top bar, status bar.
//!
//! The shell is the fixed frame around every page, so it holds only things
//! that are true everywhere: where you are (rail), what the link is doing
//! right now (top bar), and what the program is doing on your behalf
//! (status bar). Page-specific content never leaks in here.
//!
//! Layout is a three-band grid — 58px top bar, flexible content, 28px status
//! bar — with a 236px rail down the left of the content band. Every value
//! comes from [`crate::theme::L`] so the rail, the page padding and the
//! status bar share one grid.
//!
//! The shell reports intent through [`crate::ui::action`]: the rail selects a
//! page, the top bar fires a [`TopbarAction`], and both let `app` decide what
//! that means. Nothing here starts or stops anything.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::{Align, Event};
use fltk::prelude::*;
use fltk::widget::Widget;

use crate::model::{Health, Role};
use crate::state::{fmt_rate, fmt_uptime, AppState, Page};
use crate::theme::{self as th, C, L, T};
use crate::ui::action::{emit_action, set_handler, ActionSlot, TopbarAction};

// ---------------------------------------------------------------------------
// Rail
// ---------------------------------------------------------------------------

struct RailState {
    page: Page,
    hover: Option<usize>,
    /// Per-page badge: how many sessions of that role are live, 0 = none.
    badges: [u32; 6],
    /// Whether a role has unsaved config changes.
    dirty: [bool; 6],
}

/// The left rail: the product wordmark, then one row per page, then the
/// build footer. Rows are 40px, generous enough to click and quiet enough to
/// stay out of the way; the selected row is marked by an amber bar on the
/// rail edge plus a raised surface, not by a filled block of color.
#[derive(Clone)]
pub struct Rail {
    w: Widget,
    st: Rc<RefCell<RailState>>,
    /// Selection callback: the shared intent channel, carrying the `Page` the
    /// operator picked.
    on_select: ActionSlot<Page>,
}

impl Rail {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(RailState {
            page: Page::Console,
            hover: None,
            badges: [0; 6],
            dirty: [false; 6],
        }));
        let on_select: ActionSlot<Page> = Rc::new(RefCell::new(None));

        let mut wid = Widget::new(x, y, w, h, None);
        wid.set_frame(fltk::enums::FrameType::NoBox);

        let st_draw = st.clone();
        wid.draw(move |w| paint_rail(w.x(), w.y(), w.w(), w.h(), &st_draw.borrow()));

        let st_ev = st.clone();
        let sel = on_select.clone();
        wid.handle(move |w, ev| {
            let (_, my) = fltk::app::event_coords();
            let idx = row_at(w.y(), my);
            match ev {
                Event::Move | Event::Enter | Event::Drag => {
                    let mut s = st_ev.borrow_mut();
                    if s.hover != idx {
                        s.hover = idx;
                        w.redraw();
                    }
                    true
                }
                Event::Leave => {
                    st_ev.borrow_mut().hover = None;
                    w.redraw();
                    true
                }
                Event::Push => {
                    if let Some(i) = idx {
                        let page = Page::ALL[i];
                        st_ev.borrow_mut().page = page;
                        w.redraw();
                        // The shared channel clones the handler out of the
                        // RefCell, so user code can freely mutate the rail
                        // while it runs.
                        emit_action(&sel, page);
                        return true;
                    }
                    false
                }
                _ => false,
            }
        });

        Self {
            w: wid,
            st,
            on_select,
        }
    }

    pub fn on_select<F: Fn(Page) + 'static>(&self, f: F) {
        set_handler(&self.on_select, f);
    }

    pub fn set_page(&self, page: Page) {
        self.st.borrow_mut().page = page;
        self.w.clone().redraw();
    }

    /// Update the live-session count and dirty markers shown per row.
    pub fn set_badges(&self, counts: [u32; 6], dirty: [bool; 6]) {
        let mut s = self.st.borrow_mut();
        if s.badges != counts || s.dirty != dirty {
            s.badges = counts;
            s.dirty = dirty;
            drop(s);
            self.w.clone().redraw();
        }
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.w.clone().resize(x, y, w, h);
    }
}

/// Rail row geometry: brand block, then rows, then a gap before the footer.
const BRAND_H: i32 = 74;
const ROW_H: i32 = 40;
const ROW_GAP: i32 = 2;

fn row_y(top: i32, idx: usize) -> i32 {
    top + BRAND_H + idx as i32 * (ROW_H + ROW_GAP)
}

fn row_at(top: i32, my: i32) -> Option<usize> {
    for i in 0..Page::ALL.len() {
        let y = row_y(top, i);
        if my >= y && my < y + ROW_H {
            return Some(i);
        }
    }
    None
}

fn paint_rail(x: i32, y: i32, w: i32, h: i32, s: &RailState) {
    th::fill(x, y, w, h, C::PANEL);
    th::vline(x + w - 1, y, h, C::LINE);

    // Wordmark: the product name in the display face, with a one-line
    // descriptor. The amber dot is the only ornament in the shell, and it is
    // placed from the same line box the wordmark is drawn in — that is the
    // whole point of `LineBox`: the dot and the text cannot drift apart.
    let word = th::LineBox::new(th::mono_font(), x + 36, y + 18, w - 50, 26, T::BRAND);
    th::dot(x + 24, word.center_y(), 4, C::AMBER);
    th::line_text("rx11", &word, th::mono_font(), C::TEXT);
    th::micro("远程 X11 中继控制台", x + 24, y + 46, w - 40, 16, C::TEXT_3);

    for (i, page) in Page::ALL.iter().enumerate() {
        let ry = row_y(y, i);
        let selected = *page == s.page;
        let hovered = s.hover == Some(i);

        if selected {
            th::fill_round(x + 12, ry, w - 24, ROW_H, L::RADIUS, C::RAISED);
            // Amber edge bar: the selection marker, one shape, no fill.
            th::fill_round(x + 12, ry + 10, 2, ROW_H - 20, 1, C::AMBER);
        } else if hovered {
            th::fill_round(
                x + 12,
                ry,
                w - 24,
                ROW_H,
                L::RADIUS,
                th::mix(C::RAISED, C::PANEL, 0.5),
            );
        }

        let mark_color = if selected {
            C::AMBER
        } else if hovered {
            C::TEXT_2
        } else {
            C::TEXT_3
        };
        th::text(
            page.mark(),
            x + 26,
            ry,
            20,
            ROW_H,
            th::ui_font(),
            T::BODY,
            mark_color,
            Align::Center,
        );
        let title_color = if selected { C::TEXT } else { C::TEXT_2 };
        let font = if selected {
            th::ui_bold()
        } else {
            th::ui_font()
        };
        th::text(
            page.title(),
            x + 50,
            ry,
            w - 90,
            ROW_H,
            font,
            T::BODY,
            title_color,
            Align::Left,
        );

        // Trailing marker: a live-session count when the page has sessions,
        // otherwise a dot when the page has unsaved edits.
        let badge = s.badges[i];
        let dirty = s.dirty[i];
        if badge > 0 {
            let label = badge.to_string();
            let bw = th::measure(&label, th::mono_font(), T::MICRO) + 14;
            th::fill_round(
                x + w - 24 - bw,
                ry + 11,
                bw,
                18,
                9,
                th::mix(C::AMBER, C::PANEL, 0.22),
            );
            th::text(
                &label,
                x + w - 24 - bw,
                ry + 11,
                bw,
                18,
                th::mono_font(),
                T::MICRO,
                C::AMBER,
                Align::Center,
            );
        } else if dirty {
            th::dot(x + w - 30, ry + ROW_H / 2, 3, C::AMBER);
        }
    }

    // Footer: the version stamp. It sits on the **status bar's band** and
    // continues the same rule, so the bottom of the window reads as one row.
    // It used to sit at `y + h - 54` with an inset divider, which left the
    // caption floating ~40 device px above the status bar text with a dead
    // strip of panel underneath it.
    let fy = y + h - L::STATUS_H;
    th::hline(x, fy, w, C::LINE);
    th::micro(
        "版本 0.1.0 · 协议 v1",
        x + 24,
        fy,
        w - 40,
        L::STATUS_H,
        C::TEXT_3,
    );
}

// ---------------------------------------------------------------------------
// Top bar
// ---------------------------------------------------------------------------

/// What the top bar needs to know each frame.
#[derive(Clone, Default)]
pub struct TopbarData {
    pub link_up: bool,
    /// The chain readout: server side, then client side.
    pub server_state: Option<String>,
    pub client_state: Option<String>,
    /// Current throughput shown at the right edge.
    pub rate: f64,
    /// Total wire bytes for the session.
    pub total: u64,
}

struct TopbarState {
    data: TopbarData,
    actions: Vec<TopbarAction>,
    hover: Option<usize>,
    pressed: Option<usize>,
}

/// The top bar. Its center is the product's own topology drawn as a chain —
/// `中继 ⇄ 客户端` — with the live state under each node and a moving pulse
/// on the wire between them when traffic is flowing. That is the one place
/// the interface states, rather than measures, what the product does.
#[derive(Clone)]
pub struct Topbar {
    w: Widget,
    st: Rc<RefCell<TopbarState>>,
}

impl Topbar {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(TopbarState {
            data: TopbarData::default(),
            actions: Vec::new(),
            hover: None,
            pressed: None,
        }));
        let mut wid = Widget::new(x, y, w, h, None);
        wid.set_frame(fltk::enums::FrameType::NoBox);

        let st_draw = st.clone();
        wid.draw(move |w| paint_topbar(w.x(), w.y(), w.w(), w.h(), &st_draw.borrow()));

        let st_ev = st.clone();
        wid.handle(move |w, ev| {
            let (mx, _) = fltk::app::event_coords();
            let hit = {
                let s = st_ev.borrow();
                hit_action(w.x(), w.w(), mx, &s.actions)
            };
            match ev {
                Event::Move | Event::Enter => {
                    let mut s = st_ev.borrow_mut();
                    if s.hover != hit {
                        s.hover = hit;
                        w.redraw();
                    }
                    true
                }
                Event::Leave => {
                    st_ev.borrow_mut().hover = None;
                    w.redraw();
                    true
                }
                Event::Push => {
                    st_ev.borrow_mut().pressed = hit;
                    w.redraw();
                    hit.is_some()
                }
                Event::Released => {
                    let mut s = st_ev.borrow_mut();
                    let fire = s.pressed.is_some() && s.pressed == hit;
                    s.pressed = None;
                    w.redraw();
                    if fire {
                        let cb = s.actions[hit.unwrap()].2.clone();
                        drop(s);
                        cb();
                    }
                    true
                }
                _ => false,
            }
        });

        Self { w: wid, st }
    }

    pub fn set_data(&self, data: TopbarData) {
        let mut s = self.st.borrow_mut();
        let changed = s.data.link_up != data.link_up
            || s.data.rate != data.rate
            || s.data.total != data.total
            || s.data.server_state != data.server_state
            || s.data.client_state != data.client_state;
        s.data = data;
        drop(s);
        if changed {
            self.w.clone().redraw();
        }
    }

    pub fn set_actions(&self, actions: Vec<TopbarAction>) {
        self.st.borrow_mut().actions = actions;
        self.w.clone().redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.w.clone().resize(x, y, w, h);
    }
}

fn hit_action(_x: i32, w: i32, mx: i32, actions: &[TopbarAction]) -> Option<usize> {
    // Actions are right-aligned, laid out from the right edge inward.
    let mut right = w - 20;
    for (i, (label, _enabled, _)) in actions.iter().enumerate().rev() {
        let bw = th::measure(label, th::ui_font(), T::SMALL) + 28;
        let left = right - bw;
        if mx >= left && mx <= right {
            return Some(i);
        }
        right = left - 8;
    }
    None
}

fn paint_topbar(x: i32, y: i32, w: i32, h: i32, s: &TopbarState) {
    th::fill(x, y, w, h, C::BG);
    th::hline(x, y + h - 1, w, C::LINE);

    // Center: the chain. Fixed width block, centered in the available space.
    let chain_w = 330;
    let cx = x + (w - chain_w) / 2;
    paint_chain(cx, y, chain_w, h, &s.data);

    // Right: action chips, laid out from the right edge inward.
    let mut right = x + w - 20;
    for (i, (label, enabled, _)) in s.actions.iter().enumerate().rev() {
        let bw = th::measure(label, th::ui_font(), T::SMALL) + 28;
        let left = right - bw;
        let hovered = s.hover == Some(i);
        let pressed = s.pressed == Some(i);
        let (fill, text, border) = if !*enabled {
            (C::BG, C::TEXT_3, C::LINE)
        } else if pressed {
            (C::PRESSED, C::TEXT, C::TEXT_3)
        } else if hovered {
            (C::RAISED, C::TEXT, C::LINE_HI)
        } else {
            (C::PANEL, C::TEXT_2, C::LINE)
        };
        let dy = if pressed { 1 } else { 0 };
        th::fill_round(left, y + 15 + dy, bw, 28, L::RADIUS, fill);
        th::stroke_round(left, y + 15 + dy, bw, 28, L::RADIUS, border);
        th::text(
            label,
            left,
            y + 15 + dy,
            bw,
            28,
            th::ui_font(),
            T::SMALL,
            text,
            Align::Center,
        );
        right = left - 8;
    }
}

/// The chain: `中继 ⇄ 客户端`. Nodes are square marks; the wire between them
/// carries a pulse whose position is derived from the current rate, so a busy
/// link visibly moves and an idle one sits still. This is the shell's one
/// piece of motion, and it encodes the thing the product does.
fn paint_chain(x: i32, y: i32, w: i32, h: i32, d: &TopbarData) {
    let cy = y + h / 2;
    let node_w = 84;
    let left_x = x;
    let right_x = x + w - node_w;
    let wire_x1 = left_x + node_w + 12;
    let wire_x2 = right_x - 12;

    // Wire first, so nodes sit on top of its ends.
    let wire_color = if d.link_up { C::AMBER_DIM } else { C::LINE_HI };
    th::hline(wire_x1, cy, wire_x2 - wire_x1, wire_color);

    // Direction arrows on the wire: downlink solid, uplink thinner, both
    // small enough to read as annotation rather than decoration.
    let mid = (wire_x1 + wire_x2) / 2;
    th::text(
        "⇄",
        mid - 14,
        cy - 11,
        28,
        20,
        th::ui_font(),
        T::BODY,
        if d.link_up { C::AMBER } else { C::TEXT_3 },
        Align::Center,
    );

    // Pulse: a short bright segment travelling along the wire. Speed follows
    // the rate so it reads as flow, not as a spinner.
    if d.link_up && d.rate > 0.5 {
        let span = (wire_x2 - wire_x1 - 24).max(1);
        let speed = (d.rate / 4096.0).clamp(0.15, 3.0);
        let t = (now_ms() as f64 / 1000.0 * speed) % 1.0;
        let px = wire_x1 + 8 + (span as f64 * t) as i32;
        th::fill_round(px, cy - 1, 16, 3, 1, C::AMBER);
    }

    paint_node(
        left_x,
        cy,
        node_w,
        "中继",
        d.server_state.as_deref(),
        d.link_up,
    );
    paint_node(
        right_x,
        cy,
        node_w,
        "客户端",
        d.client_state.as_deref(),
        d.link_up,
    );

    // Readout under the chain: throughput and total moved this session.
    let label = format!(
        "{} · 累计 {}",
        fmt_rate(d.rate),
        crate::state::fmt_bytes(d.total)
    );
    th::text(
        &label,
        x,
        y + h - 18,
        w,
        14,
        th::mono_font(),
        T::MICRO,
        C::TEXT_3,
        Align::Center,
    );
}

fn paint_node(x: i32, cy: i32, w: i32, name: &str, state: Option<&str>, live: bool) {
    let top = cy - 16;
    th::fill_round(x, top, w, 32, L::RADIUS, C::PANEL);
    th::stroke_round(
        x,
        top,
        w,
        32,
        L::RADIUS,
        if live { C::LINE_HI } else { C::LINE },
    );
    let dot_color = match state {
        Some(s) if s.contains("运行") => C::OK,
        Some(s) if s.contains("重连") => C::WARN,
        Some(s) if s.contains("失败") => C::BAD,
        _ => C::IDLE,
    };
    // Label and dot come from one line box, centred in the 32-unit pill.
    let line = th::LineBox::ui(x + 22, top, w - 30, 32, T::SMALL);
    th::dot(x + 12, line.center_y(), 3, dot_color);
    th::line_text(
        name,
        &line,
        th::ui_font(),
        if live { C::TEXT } else { C::TEXT_2 },
    );
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// Bottom strip: config path, unsaved marker, notice text, and a right-hand
/// clock. It is the console's "what the program is doing" line.
#[derive(Clone)]
pub struct StatusBar {
    w: Widget,
    st: Rc<RefCell<StatusData>>,
}

#[derive(Clone, Default)]
pub struct StatusData {
    pub config_path: String,
    pub dirty: bool,
    pub notice: Option<(String, u32)>,
    pub session_count: usize,
    pub uptime: u64,
}

impl StatusBar {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(StatusData::default()));
        let mut wid = Widget::new(x, y, w, h, None);
        wid.set_frame(fltk::enums::FrameType::NoBox);
        let st_draw = st.clone();
        wid.draw(move |w| paint_status(w.x(), w.y(), w.w(), w.h(), &st_draw.borrow()));
        Self { w: wid, st }
    }

    pub fn set_data(&self, d: StatusData) {
        *self.st.borrow_mut() = d;
        self.w.clone().redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.w.clone().resize(x, y, w, h);
    }
}

fn paint_status(x: i32, y: i32, w: i32, h: i32, d: &StatusData) {
    th::fill(x, y, w, h, C::PANEL);
    th::hline(x, y, w, C::LINE);
    let ty = y + (h - 16) / 2;

    // Left: config path, with an unsaved dot.
    let path_color = if d.dirty { C::AMBER } else { C::TEXT_3 };
    let label = th::ellipsize(&d.config_path, 380, th::mono_font(), T::MICRO);
    th::mono(&label, x + 16, ty, 380, 16, T::MICRO, path_color);
    if d.dirty {
        th::micro("· 未保存", x + 16 + 384, ty, 80, 16, C::AMBER);
    }

    // Center-left: notice, the only text that can change color here.
    if let Some((text, color)) = &d.notice {
        let start = x + 16 + 470;
        let avail = (w - 520).max(120);
        let shown = th::ellipsize(text, avail, th::ui_font(), T::MICRO);
        th::small(&shown, start, ty, avail, 16, *color);
    }

    // Right: session count and uptime, both figures.
    let right = format!("{} 个会话 · 运行 {}", d.session_count, fmt_uptime(d.uptime));
    let rw = th::measure(&right, th::mono_font(), T::MICRO) + 20;
    th::mono(&right, x + w - rw - 12, ty, rw, 16, T::MICRO, C::TEXT_3);
}

/// Compose the rail badges from app state: live session count per page and
/// the dirty flags. Kept next to the shell because both widgets consume it.
pub fn rail_badges(state: &AppState) -> ([u32; 6], [bool; 6]) {
    let mut counts = [0u32; 6];
    for s in &state.sessions {
        let page = match s.role {
            Role::Server => Page::Server,
            Role::Client => Page::Client,
            Role::Ssh => Page::Ssh,
        };
        if matches!(
            s.health,
            Health::Running | Health::Starting | Health::Reconnecting
        ) {
            counts[page as usize] += 1;
        }
    }
    let mut dirty = [false; 6];
    for (i, d) in state.dirty.iter().enumerate() {
        dirty[i] = *d;
    }
    (counts, dirty)
}
