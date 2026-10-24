//! The window shell: sidebar rail, top bar, status bar.
//!
//! The shell is the fixed frame around every page, so it holds only things
//! that are true everywhere: where you are (rail), what the link is doing
//! right now (top bar), and what the program is doing on your behalf
//! (status bar). Page-specific content never leaks in here.
//!
//! Layout is a three-band grid — top bar, flexible content, status bar — with a
//! rail down the left of the content band. Every band is a leaf of a solved
//! [`crate::ui::layout`] tree, so the rail's rows, the page padding and the
//! status bar all derive from one solve and the metric tokens in
//! [`crate::theme::L`].
//!
//! The shell reports intent through [`crate::ui::action`]: the rail selects a
//! page, the top bar fires a [`TopbarAction`], and both let `app` decide what
//! that means. Nothing here starts or stops anything.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::{Align, Shortcut};
use fltk::prelude::*;
use fltk::widget::Widget;

use crate::model::{Health, Role};
use crate::state::{fmt_rate, fmt_uptime, AppState, Page};
use crate::theme::{self as th, C, L, T};
use crate::ui::action::{emit_action, set_handler, ActionSlot, TopbarAction};
use crate::ui::kit::{hit_index, Hand, Interactive, ListSieve, Touch};
use crate::ui::layout::{Insets, Layout, NodeId, Rect, Size, Solved};

// ---------------------------------------------------------------------------
// Rail
// ---------------------------------------------------------------------------

struct RailState {
    page: Page,
    /// Row hover/press/click — the kit's [`ListSieve`]. `Copy`, taken out
    /// and written back per event like the pages' `Grip`.
    sieve: ListSieve,
    /// Per-page badge: how many sessions of that role are live, 0 = none.
    badges: [u32; 6],
    /// Whether a role has unsaved config changes.
    dirty: [bool; 6],
}

/// The left rail: the product wordmark, then one row per page, then the
/// build footer. Rows are 40px, generous enough to click and quiet enough to
/// stay out of the way; the selected row is marked by an amber bar on the
/// rail edge plus a raised surface, not by a filled block of color.
///
/// The rail is not a Tab stop — one widget owns six rows, so row focus would
/// need a model the kit does not carry. Page switching by keyboard rides
/// FLTK's shortcut event instead: `Alt+1` … `Alt+6`.
#[derive(Clone)]
pub struct Rail {
    it: Interactive<Rc<RefCell<RailState>>>,
    st: Rc<RefCell<RailState>>,
    /// Selection callback: the shared intent channel, carrying the `Page` the
    /// operator picked.
    on_select: ActionSlot<Page>,
}

impl Rail {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(RailState {
            page: Page::Console,
            sieve: ListSieve::default(),
            badges: [0; 6],
            dirty: [false; 6],
        }));
        let on_select: ActionSlot<Page> = Rc::new(RefCell::new(None));

        let st_draw = st.clone();
        let sel = on_select.clone();
        let it = Interactive::build(x, y, w, h, st.clone())
            .focus(false)
            .draw(move |w, _, _| paint_rail(w.x(), w.y(), w.w(), w.h(), &st_draw.borrow()))
            .event(move |wid, cell, _, touch| {
                let rects = row_rects(wid);
                let idx = touch.point().and_then(|(tx, ty)| hit_index(&rects, tx, ty));
                match touch {
                    Touch::Move { .. } | Touch::Drag { .. } => {
                        let mut s = cell.borrow_mut();
                        if s.sieve.move_to(idx) {
                            Hand::Repaint
                        } else {
                            Hand::Claimed
                        }
                    }
                    Touch::Exit => {
                        let mut s = cell.borrow_mut();
                        if s.sieve.exit() {
                            Hand::Repaint
                        } else {
                            Hand::Claimed
                        }
                    }
                    Touch::Press { .. } => {
                        let mut s = cell.borrow_mut();
                        if s.sieve.press(idx) {
                            Hand::Claimed
                        } else {
                            Hand::Ignored
                        }
                    }
                    Touch::Click { at } => {
                        let fired = {
                            let mut s = cell.borrow_mut();
                            s.sieve.release(hit_index(&rects, at.0, at.1))
                        };
                        match fired {
                            Some(i) => select(i, cell, &sel),
                            None => Hand::Claimed,
                        }
                    }
                    Touch::Release { .. } => Hand::Claimed,
                    Touch::Shortcut { key, mods } => {
                        // Alt+1 … Alt+6 pick a page; any other key is not ours.
                        let bit = key.bits();
                        let idx = ('1' as i32..'7' as i32)
                            .position(|d| d == bit)
                            .filter(|_| mods.contains(Shortcut::Alt));
                        match idx {
                            Some(i) => select(i, cell, &sel),
                            None => Hand::Ignored,
                        }
                    }
                    _ => Hand::Ignored,
                }
            })
            .install();

        Self { it, st, on_select }
    }

    pub fn on_select<F: Fn(Page) + 'static>(&self, f: F) {
        set_handler(&self.on_select, f);
    }

    pub fn set_page(&self, page: Page) {
        self.st.borrow_mut().page = page;
        self.it.redraw();
    }

    /// Update the live-session count and dirty markers shown per row.
    pub fn set_badges(&self, counts: [u32; 6], dirty: [bool; 6]) {
        let mut s = self.st.borrow_mut();
        if s.badges != counts || s.dirty != dirty {
            s.badges = counts;
            s.dirty = dirty;
            drop(s);
            self.it.redraw();
        }
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.it.resize(x, y, w, h);
    }
}

/// Rail row geometry: brand block, one row per page, then the status band.
const BRAND_H: i32 = 74;
const ROW_H: i32 = 40;
const ROW_GAP: i32 = 2;

/// The rail's layout tree: brand block, page rows, then the status band.
///
/// Solving this once is what keeps a row's hit target, its hover highlight and
/// its label on the same rectangle — they used to be recomputed from
/// `brand + index * (row + gap)` in three places.
struct RailLayout {
    layout: Layout,
    brand: NodeId,
    rows: Vec<NodeId>,
    footer: NodeId,
}

impl RailLayout {
    fn solve(&self, area: Rect) -> Solved {
        self.layout.solve(area)
    }

    /// The status band at the bottom of the rail, shared with the status bar.
    fn footer_rect(&self, solved: &Solved) -> Rect {
        solved.get(self.footer)
    }
}

fn build_rail_layout() -> RailLayout {
    let mut layout = Layout::column();
    let brand = layout.leaf(0, Size::Fixed(BRAND_H));
    // The gap sits *between* rows, not after the brand block, so every row
    // except the first carries its own lead.
    let mut rows = Vec::with_capacity(Page::ALL.len());
    for i in 0..Page::ALL.len() {
        let id = layout.leaf(0, Size::Fixed(ROW_H));
        layout = layout.lead(id, if i == 0 { 0 } else { ROW_GAP });
        rows.push(id);
    }
    // The trailing status band is pinned to the bottom by the fill spacer
    // between the rows and itself.
    let _spacer = layout.leaf(0, Size::Fill(1));
    let footer = layout.leaf(0, Size::Fixed(L::STATUS_H));
    RailLayout {
        layout,
        brand,
        rows,
        footer,
    }
}

/// Which rail row rects the solved tree produces — the same rects the
/// highlight is drawn into, the hit-testing reads, and the labels ride in.
fn row_rects(wid: &Widget) -> Vec<Rect> {
    let geometry = build_rail_layout();
    let solved = geometry.solve(Rect::new(wid.x(), wid.y(), wid.w(), wid.h()));
    geometry.rows.iter().map(|&id| solved.get(id)).collect()
}

/// Select a rail row and announce the page. The state cell is borrowed only
/// briefly: the intent handler re-enters `Rail::set_page` (show_page calls
/// it), and that must not find a live borrow.
fn select(i: usize, cell: &Rc<RefCell<RailState>>, sel: &ActionSlot<Page>) -> Hand {
    let page = Page::ALL[i];
    cell.borrow_mut().page = page;
    emit_action(sel, page);
    Hand::Repaint
}

fn paint_rail(x: i32, y: i32, w: i32, h: i32, s: &RailState) {
    th::fill(x, y, w, h, C::PANEL);
    th::vline(x + w - 1, y, h, C::LINE);

    let geometry = build_rail_layout();
    let solved = geometry.solve(Rect::new(x, y, w, h));

    // Wordmark: the product name in the display face, with a one-line
    // descriptor. The amber dot is the only ornament in the shell, and it is
    // placed from the same line box the wordmark is drawn in — that is the
    // whole point of `LineBox`: the dot and the text cannot drift apart.
    let brand = solved.get(geometry.brand);
    let word = th::LineBox::new(
        th::mono_font(),
        brand.x + 36,
        brand.y + 18,
        brand.w - 50,
        26,
        T::BRAND,
    );
    th::dot(brand.x + 24, word.center_y(), 4, C::AMBER);
    th::line_text("rx11", &word, th::mono_font(), C::TEXT);
    th::micro(
        "远程 X11 中继控制台",
        brand.x + 24,
        brand.y + 46,
        brand.w - 40,
        16,
        C::TEXT_3,
    );

    for (i, page) in Page::ALL.iter().enumerate() {
        let ry = solved.get(geometry.rows[i]).y;
        let selected = *page == s.page;
        let hovered = s.sieve.hover == Some(i);

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
    let footer = geometry.footer_rect(&solved);
    th::hline(footer.x, footer.y, footer.w, C::LINE);
    th::micro(
        "版本 0.1.0 · 协议 v1",
        footer.x + 24,
        footer.y,
        footer.w - 40,
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
    /// Action hover/press/click — the kit's [`ListSieve`], same machine as
    /// the rail rows. (The hand-rolled version before it hit-tested with
    /// `r.contains(mx, r.y)` — the pointer's own y was ignored, so a click
    /// anywhere in the bar's column fired the action under it.)
    sieve: ListSieve,
}

/// The top bar. Its center is the product's own topology drawn as a chain —
/// `中继 ⇄ 客户端` — with the live state under each node and a moving pulse
/// on the wire between them when traffic is flowing. That is the one place
/// the interface states, rather than measures, what the product does.
#[derive(Clone)]
pub struct Topbar {
    it: Interactive<Rc<RefCell<TopbarState>>>,
    st: Rc<RefCell<TopbarState>>,
}

impl Topbar {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(TopbarState {
            data: TopbarData::default(),
            actions: Vec::new(),
            sieve: ListSieve::default(),
        }));
        let st_draw = st.clone();
        let it = Interactive::build(x, y, w, h, st.clone())
            .focus(false)
            .draw(move |w, _, _| paint_topbar(w.x(), w.y(), w.w(), w.h(), &st_draw.borrow()))
            .event(move |wid, cell, _, touch| {
                let idx = touch.point().and_then(|(tx, ty)| {
                    let s = cell.borrow();
                    let rects =
                        action_rects(Rect::new(wid.x(), wid.y(), wid.w(), wid.h()), &s.actions);
                    hit_index(&rects, tx, ty)
                });
                match touch {
                    Touch::Move { .. } => {
                        let mut s = cell.borrow_mut();
                        if s.sieve.move_to(idx) {
                            Hand::Repaint
                        } else {
                            Hand::Claimed
                        }
                    }
                    Touch::Exit => {
                        let mut s = cell.borrow_mut();
                        if s.sieve.exit() {
                            Hand::Repaint
                        } else {
                            Hand::Claimed
                        }
                    }
                    Touch::Press { .. } => {
                        let mut s = cell.borrow_mut();
                        if s.sieve.press(idx) {
                            Hand::Claimed
                        } else {
                            Hand::Ignored
                        }
                    }
                    Touch::Click { at } => {
                        let tx = at.0;
                        let ty = at.1;
                        let fired = {
                            let mut s = cell.borrow_mut();
                            let rects = action_rects(
                                Rect::new(wid.x(), wid.y(), wid.w(), wid.h()),
                                &s.actions,
                            );
                            s.sieve.release(hit_index(&rects, tx, ty))
                        };
                        if let Some(i) = fired {
                            let cb = cell.borrow().actions[i].2.clone();
                            cb();
                        }
                        Hand::Repaint
                    }
                    Touch::Release { .. } => Hand::Claimed,
                    _ => Hand::Ignored,
                }
            })
            .install();

        Self { it, st }
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
            self.it.redraw();
        }
    }

    pub fn set_actions(&self, actions: Vec<TopbarAction>) {
        self.st.borrow_mut().actions = actions;
        self.it.redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.it.resize(x, y, w, h);
    }
}

/// Gap between the action chips.
const CHIP_GAP: i32 = 8;

/// Inset of the action group from the top bar's right edge.
const CHIP_MARGIN: i32 = 20;

/// Height of an action chip, vertically centred in the bar.
const CHIP_H: i32 = 28;

/// The top bar's action chips, right-aligned.
///
/// Painting and hit testing both call this, so a chip's frame, its label and
/// the area that fires it are the same rectangle. They used to be recomputed
/// from `measure(label) + 28` in two places.
fn action_rects(area: Rect, actions: &[TopbarAction]) -> Vec<Rect> {
    let mut layout = Layout::row();
    layout = layout.pad(
        0,
        Insets {
            top: 0,
            right: CHIP_MARGIN,
            bottom: 0,
            left: 0,
        },
    );
    // A leading spacer pushes the chips against the right edge.
    let _spacer = layout.leaf(0, Size::Fill(1));
    let mut ids = Vec::with_capacity(actions.len());
    for (i, (label, _, _)) in actions.iter().enumerate() {
        let id = layout.leaf(
            0,
            Size::Fixed(th::measure(label, th::ui_font(), T::SMALL) + 28),
        );
        if i > 0 {
            layout = layout.lead(id, CHIP_GAP);
        }
        ids.push(id);
    }
    let solved = layout.solve(area);
    ids.iter().map(|&id| solved.get(id)).collect()
}

fn paint_topbar(x: i32, y: i32, w: i32, h: i32, s: &TopbarState) {
    th::fill(x, y, w, h, C::BG);
    th::hline(x, y + h - 1, w, C::LINE);

    // Center: the chain. Fixed width block, centered in the available space.
    let chain_w = 330;
    let cx = x + (w - chain_w) / 2;
    paint_chain(cx, y, chain_w, h, &s.data);

    // Right: action chips from the solved row, so the frame you click and the
    // frame you see come from the same rectangles.
    let bar = Rect::new(x, y, w, h);
    for (i, rect) in action_rects(bar, &s.actions).iter().enumerate() {
        let (label, enabled, _) = &s.actions[i];
        let hovered = s.sieve.hover == Some(i);
        let pressed = s.sieve.pressed == Some(i);
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
        let chip_y = rect.center_y() - CHIP_H / 2 + dy;
        th::fill_round(rect.x, chip_y, rect.w, CHIP_H, L::RADIUS, fill);
        th::stroke_round(rect.x, chip_y, rect.w, CHIP_H, L::RADIUS, border);
        th::text(
            label,
            rect.x,
            chip_y,
            rect.w,
            CHIP_H,
            th::ui_font(),
            T::SMALL,
            text,
            Align::Center,
        );
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
