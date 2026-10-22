//! 链路总览 — the landing page.
//!
//! Structure follows what an operator arrives to do: see whether the link is
//! alive, then act. The page reads top-down as three questions:
//!
//! 1. *Is anything moving?* — five figures in one strip, then the scope, which
//!    is the page's one large element.
//! 2. *What is running?* — one row per session, each with its own stop
//!    control.
//! 3. *What can I start?* — the three roles as primary actions next to the
//!    token they need, plus the capabilities this link negotiated.
//!
//! Everything except the scope is quiet on purpose: the trace is what the eye
//! should land on, and the rest is the instrument's bezel.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::{Align, Event};
use fltk::prelude::*;

use crate::model::Health;
use crate::state::{fmt_bytes, fmt_pct, fmt_rate, fmt_uptime, Sample, SessionRow};
use crate::theme::{self as th, C, L, T};
use crate::ui::action::{emit_action, set_handler, ActionSlot};
use crate::ui::chrome::{
    card_in, card_title_in, empty_state_in, page_header_in, stat_in, CARD_TITLE_H, PAGE_HEADER_H,
};
use crate::ui::controls::{BtnKind, Button};
use crate::ui::layout::{Axis, Insets, Layout, NodeId, Rect, Size, Solved};
use crate::ui::scope::{Scope, ScopeData};
use crate::ui::scrollbar::{Hit, ScrollBar};
use crate::ui::{Page, PageInput};

/// The background painter: page rect in, pixels out.
type Painter = Box<dyn FnMut(i32, i32, i32, i32)>;

/// Vertical rhythm of the page, in one place: `paint` and `layout` both read
/// these, so a height change cannot desynchronise drawing from hit testing.
struct Metrics;

impl Metrics {
    const SUMMARY_H: i32 = 76;
    const BAND_H: i32 = 300;
    const RIGHT_W: i32 = 296;
    /// Start card: title, three role buttons, then the token block. Sized so
    /// the token hint never collides with the button above it.
    const START_CARD_H: i32 = 178;
    const CAP_CARD_GAP: i32 = 12;
    const ROW_H: i32 = 46;
}

/// What the page can ask the app to do. Pages never touch the launcher
/// directly: the app owns the lifecycle, the page only states intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleAction {
    StartServer,
    StartClient,
    StartSsh,
    StopSession(u64),
    CopyToken,
}

/// Everything the console renders, assembled by the app from the launcher
/// snapshot. Plain data: no handles, no locks, no backend types.
#[derive(Clone, Default)]
pub struct ConsoleData {
    pub sessions: Vec<SessionRow>,
    pub scope: Vec<Sample>,
    pub total_sent: u64,
    pub total_recv: u64,
    pub savings: f64,
    pub channels: u32,
    pub live: bool,
    /// Negotiated capabilities as (name, enabled-by-both).
    pub capabilities: Vec<(String, bool)>,
    /// Token in use, shown truncated with a copy action.
    pub token_hint: String,
}

impl ConsoleData {
    fn rates(&self) -> (f64, f64) {
        self.scope
            .last()
            .map(|s| (s.sent_bps, s.recv_bps))
            .unwrap_or((0.0, 0.0))
    }

    fn running(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| {
                matches!(
                    s.health,
                    Health::Running | Health::Starting | Health::Reconnecting
                )
            })
            .count()
    }
}

/// One row's hit geometry. Recomputed in `layout`, so drawing and hit testing
/// can never disagree about where a control is.
#[derive(Clone, Copy)]
struct RowHit {
    session_id: u64,
    stop: (i32, i32, i32, i32),
}

struct ConsoleState {
    data: ConsoleData,
    rows: Vec<RowHit>,
    hover_stop: Option<usize>,
    pressed_stop: Option<usize>,
    /// First visible session row. The launcher keeps every session; the table
    /// is windowed onto them, so this is a row index, not a pixel offset.
    scroll: usize,
    /// While dragging the table's scrollbar: pointer offset below the thumb's
    /// top edge. `None` when not dragging.
    thumb_grab: Option<i32>,
}

/// The overview page.
pub struct ConsolePage {
    page: Page,
    scope: Scope,
    start_server: Button,
    start_client: Button,
    start_ssh: Button,
    copy_token: Button,
    st: Rc<RefCell<ConsoleState>>,
    on_action: ActionSlot<ConsoleAction>,
    rect: Rc<RefCell<(i32, i32, i32, i32)>>,
}

impl ConsolePage {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        // The page surface is a group: its draw paints the background, then
        // FLTK draws the controls created inside `begin()`/`end()` on top.
        let st = Rc::new(RefCell::new(ConsoleState {
            data: ConsoleData::default(),
            rows: Vec::new(),
            hover_stop: None,
            pressed_stop: None,
            scroll: 0,
            thumb_grab: None,
        }));
        let on_action: ActionSlot<ConsoleAction> = Rc::new(RefCell::new(None));
        let rect = Rc::new(RefCell::new((x, y, w, h)));

        // The background is the group's first child, so FLTK draws it before
        // the controls created below.
        let page = {
            let st_draw = st.clone();
            let painter: Painter = Box::new(move |px, py, pw, ph| {
                let s = st_draw.borrow();
                paint(px, py, pw, ph, &s);
            });
            Page::new(x, y, w, h, C::BG, painter)
        };

        let scope = Scope::new(0, 0, 10, 10);
        let start_server = Button::new(0, 0, 10, 10, "启动中继", BtnKind::Primary);
        let start_client = Button::new(0, 0, 10, 10, "启动客户端", BtnKind::Primary);
        let start_ssh = Button::new(0, 0, 10, 10, "SSH 隧道", BtnKind::Secondary);
        let copy_token = Button::new(0, 0, 10, 10, "复制", BtnKind::Ghost);

        {
            let act = on_action.clone();
            start_server.on_click(move || emit_action(&act, ConsoleAction::StartServer));
        }
        {
            let act = on_action.clone();
            start_client.on_click(move || emit_action(&act, ConsoleAction::StartClient));
        }
        {
            let act = on_action.clone();
            start_ssh.on_click(move || emit_action(&act, ConsoleAction::StartSsh));
        }
        {
            let act = on_action.clone();
            copy_token.on_click(move || emit_action(&act, ConsoleAction::CopyToken));
        }

        page.finish();

        // Stop-control hit testing: the group sees events its children did
        // not consume.
        {
            let st_ev = st.clone();
            let act = on_action.clone();
            let mut wid = page.widget();
            wid.handle(move |w, ev| {
                let (mx, my) = fltk::app::event_coords();
                let hit = st_ev.borrow().rows.iter().position(|r| {
                    let (sx, sy, sw, sh) = r.stop;
                    mx >= sx && mx <= sx + sw && my >= sy && my <= sy + sh
                });
                match ev {
                    Event::Move | Event::Enter => {
                        let mut s = st_ev.borrow_mut();
                        if s.hover_stop != hit {
                            s.hover_stop = hit;
                            w.redraw();
                        }
                        true
                    }
                    Event::Leave => {
                        let mut s = st_ev.borrow_mut();
                        s.hover_stop = None;
                        s.pressed_stop = None;
                        w.redraw();
                        true
                    }
                    Event::Push => {
                        st_ev.borrow_mut().pressed_stop = hit;
                        w.redraw();
                        hit.is_some()
                    }
                    Event::Released => {
                        let mut s = st_ev.borrow_mut();
                        let fire = s.pressed_stop.is_some() && s.pressed_stop == hit;
                        s.pressed_stop = None;
                        w.redraw();
                        drop(s);
                        if fire {
                            let id = st_ev.borrow().rows[hit.unwrap()].session_id;
                            emit_action(&act, ConsoleAction::StopSession(id));
                        }
                        true
                    }
                    _ => false,
                }
            });
        }

        let page = Self {
            page,
            scope,
            start_server,
            start_client,
            start_ssh,
            copy_token,
            st,
            on_action,
            rect,
        };
        page.layout(x, y, w, h);
        page.install_input();
        page
    }

    /// Wheel and scrollbar input for the session table.
    ///
    /// The table is the only scrolling surface on this page, so the wheel works
    /// anywhere on it; the bar sits at the table's right edge and is draggable.
    fn install_input(&self) {
        let st = self.st.clone();
        let rect = self.rect.clone();
        let page = self.page.clone();
        self.page.on_input(move |input| {
            let (px, py, pw, ph) = *rect.borrow();
            // One solve, reused by every branch: the bar the pointer grabs is
            // the bar that was painted.
            let geometry = build_layout(st.borrow().data.sessions.len());
            let solved = geometry.solve(Rect::new(px, py, pw, ph));
            let card = geometry.session_card(&solved);
            let area = geometry.row_area(&solved);
            let fit = list_rows(area);
            let scrollable = st.borrow().data.sessions.len() > fit;
            let max = (st.borrow().data.sessions.len() as i32 - fit as i32).max(0);
            match input {
                PageInput::Wheel(dy) => {
                    if dy == 0 || !scrollable {
                        return false;
                    }
                    let step = if dy > 0 { 1 } else { -1 };
                    let cur = st.borrow().scroll as i32;
                    st.borrow_mut().scroll = (cur + step).clamp(0, max) as usize;
                    page.redraw();
                    true
                }
                PageInput::Push(lx, ly) => {
                    let Some(bar) = session_bar(card, area, &st.borrow()) else {
                        return false;
                    };
                    match bar.hit(lx + px, ly + py) {
                        Some(Hit::Thumb { grab }) => {
                            st.borrow_mut().thumb_grab = Some(grab);
                            page.redraw();
                            true
                        }
                        Some(Hit::Track { dir }) => {
                            let cur = st.borrow().scroll as i32;
                            st.borrow_mut().scroll = (cur + dir * 3).clamp(0, max) as usize;
                            page.redraw();
                            true
                        }
                        None => false,
                    }
                }
                PageInput::Drag(_lx, ly) => {
                    let Some(grab) = st.borrow().thumb_grab else {
                        return false;
                    };
                    let Some(bar) = session_bar(card, area, &st.borrow()) else {
                        return false;
                    };
                    let next = rows_for(bar.scroll_at(ly + py - grab));
                    st.borrow_mut().scroll = next.min(max.max(0) as usize);
                    page.redraw();
                    true
                }
                PageInput::Release => st.borrow_mut().thumb_grab.take().is_some(),
            }
        });
    }

    pub fn on_action<F: Fn(ConsoleAction) + 'static>(&self, f: F) {
        set_handler(&self.on_action, f);
    }

    /// Feed the page a new snapshot. Values are cheap to clone and the whole
    /// page repaints from them, so there is no partial-update path to keep in
    /// sync with the layout.
    pub fn set_data(&self, data: ConsoleData) {
        self.st.borrow_mut().data = data.clone();
        self.scope.set_data(ScopeData {
            samples: data.scope,
            live: data.live,
        });
        let (x, y, w, h) = *self.rect.borrow();
        self.layout(x, y, w, h);
        self.page.redraw();
    }

    /// Showing a group shows everything inside it, so a page switch is one
    /// call rather than a per-widget bookkeeping exercise.
    pub fn show(&self) {
        let mut g = self.page.widget();
        g.show();
    }

    /// Repaint the page surface and everything on it.
    pub fn redraw(&self) {
        self.page.redraw();
    }

    pub fn hide(&self) {
        let mut g = self.page.widget();
        g.hide();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        *self.rect.borrow_mut() = (x, y, w, h);
        self.layout(x, y, w, h);
    }

    fn layout(&self, x: i32, y: i32, w: i32, h: i32) {
        self.page.resize(x, y, w, h);

        let geometry = build_layout(self.st.borrow().data.sessions.len());
        let solved = geometry.solve(Rect::new(x, y, w, h));

        // The scope fills the plot leaf: the card's inner area below its title.
        let plot = solved.get(geometry.scope_plot);
        self.scope.resize(plot.x, plot.y, plot.w, plot.h);

        let [a, b, c] = geometry.start_buttons.map(|id| solved.get(id));
        self.start_server.resize(a.x, a.y, a.w, a.h);
        self.start_client.resize(b.x, b.y, b.w, b.h);
        self.start_ssh.resize(c.x, c.y, c.w, c.h);
        let copy = solved.get(geometry.copy_token);
        self.copy_token.resize(copy.x, copy.y, copy.w, copy.h);

        // Session rows: hit testing from the same solve that paints them, so a
        // scrolled row's 「停止」 button can never belong to another session.
        let area = geometry.row_area(&solved);
        let fit = list_rows(area);
        let stop_w = SESSION_ACTION_W;
        let mut rows = Vec::new();
        let (start, end, list) = {
            let st = self.st.borrow();
            (
                st.scroll,
                st.data.sessions.len().min(st.scroll + fit),
                st.data.sessions.clone(),
            )
        };
        for slot in 0..(end.saturating_sub(start)) {
            let ry = area.y + slot as i32 * Metrics::ROW_H;
            let action = geometry.column_rects(&solved, slot)[4];
            rows.push(RowHit {
                session_id: list[start + slot].id,
                stop: (
                    action.right() - stop_w,
                    ry + (Metrics::ROW_H - 24) / 2,
                    stop_w,
                    24,
                ),
            });
        }
        self.st.borrow_mut().rows = rows;
    }
}

/// Column widths of the session table: role and health are fixed, the object
/// column takes the slack, traffic and the action button close the row.
const SESSION_ROLE_W: i32 = 86;
const SESSION_HEALTH_W: i32 = 96;
const SESSION_TRAFFIC_W: i32 = 132;
const SESSION_ACTION_W: i32 = 54;

/// Everything above the session table's first row, plus the card's bottom air:
/// padding 16 + title 20 + column headers 34 + gap 8 + bottom 8.
///
/// The card is sized as `SESSION_CHROME_H + rows * ROW_H`, and the layout tree
/// subtracts the same amounts by padding, so the two cannot disagree.
const SESSION_CHROME_H: i32 = 106;

/// The session card's inner padding. The bottom is tighter than the sides so the
/// last row keeps the same 8 units of air the card height budgets for.
const SESSION_PAD: Insets = Insets {
    top: 16,
    right: 18,
    bottom: 8,
    left: 18,
};

/// Offset of the column-header text inside its 34-unit band. The band exists to
/// separate the title row from the first session row; the text sits near its top.
const SESSION_HEAD_TEXT_INSET: i32 = 14;

/// The scope card's inner padding. Its bottom is 18 rather than the usual 16: the
/// plot ends where `BAND_H - 54` says it does.
const SCOPE_PAD: Insets = Insets {
    top: 16,
    right: 18,
    bottom: 18,
    left: 18,
};

/// The overview's layout tree.
///
/// Solved once per frame; painting, widget placement, hit testing and the
/// scrollbar all read the same rectangles out of it. Before this, each of those
/// walked the page recomputing `header + summary + band` offsets, which is how a
/// session row once ended up drawn past the bottom of its card.
struct ConsoleLayout {
    layout: Layout,
    header: NodeId,
    summary: NodeId,
    scope_card: NodeId,
    scope_plot: NodeId,
    start_card: NodeId,
    start_buttons: [NodeId; 3],
    copy_token: NodeId,
    cap_card: NodeId,
    sessions: NodeId,
    session_head: NodeId,
    session_cols: [NodeId; 5],
    session_rows: NodeId,
}

impl ConsoleLayout {
    fn solve(&self, page: Rect) -> Solved {
        self.layout.solve(page)
    }

    /// The session card's rect.
    fn session_card(&self, solved: &Solved) -> Rect {
        solved.get(self.sessions)
    }

    /// The rect the session rows are listed in.
    fn row_area(&self, solved: &Solved) -> Rect {
        solved.get(self.session_rows)
    }

    /// One row's rect, `slot` rows below the first visible one.
    fn row_rect(&self, solved: &Solved, slot: usize) -> Rect {
        let area = self.row_area(solved);
        Rect {
            y: area.y + slot as i32 * Metrics::ROW_H,
            ..area
        }
    }

    /// The five column rects of one session row.
    fn column_rects(&self, solved: &Solved, slot: usize) -> [Rect; 5] {
        let row = self.row_rect(solved, slot);
        let mut out = [row; 5];
        for (i, &id) in self.session_cols.iter().enumerate() {
            let col = solved.get(id);
            out[i] = Rect { y: row.y, ..col };
        }
        out
    }
}

fn build_layout(sessions: usize) -> ConsoleLayout {
    let mut layout = Layout::column().pad(0, Insets::all(L::PAD));
    let header = layout.leaf(0, Size::Fixed(PAGE_HEADER_H));
    // The heading block already carries its own trailing air; the summary
    // follows it flush, and only the cards are separated by `L::GAP`.
    let summary = layout.leaf(0, Size::Fixed(Metrics::SUMMARY_H));

    // Middle band: the scope card takes the slack, the right column is fixed.
    let band = layout.stack(0, Axis::Row, Size::Fixed(Metrics::BAND_H));
    layout = layout.lead(band, L::GAP);
    layout = layout.gap(band, L::GAP);
    let scope_card = layout.leaf(band, Size::Fill(1));
    let right = layout.stack(band, Axis::Column, Size::Fixed(Metrics::RIGHT_W));
    layout = layout.gap(right, Metrics::CAP_CARD_GAP);

    // Scope card: title row, then the plot.
    let scope_inner = layout.stack(scope_card, Axis::Column, Size::Fill(1));
    layout = layout.pad(scope_inner, SCOPE_PAD);
    let _scope_title = layout.leaf(scope_inner, Size::Fixed(CARD_TITLE_H));
    let scope_plot = layout.leaf(scope_inner, Size::Fill(1));

    // Start card: title, the three role buttons, then the copy control on the
    // token value's row. The leads reproduce the documented rhythm: buttons at
    // +46, the SSH button at +84, copy at +129.
    let start_card = layout.leaf(right, Size::Fixed(Metrics::START_CARD_H));
    let start_inner = layout.stack(start_card, Axis::Column, Size::Fill(1));
    layout = layout.pad(start_inner, Insets::xy(18, 16));
    let _start_title = layout.leaf(start_inner, Size::Fixed(CARD_TITLE_H));
    let start_buttons = {
        let pair = layout.stack(start_inner, Axis::Row, Size::Fixed(30));
        layout = layout.lead(pair, 10);
        layout = layout.gap(pair, 8);
        let a = layout.leaf(pair, Size::Fill(1));
        let b = layout.leaf(pair, Size::Fill(1));
        let c = layout.leaf(start_inner, Size::Fixed(28));
        layout = layout.lead(c, 8);
        [a, b, c]
    };
    let copy_token = layout.leaf(start_inner, Size::Fixed(22));
    layout = layout.lead(copy_token, 17);
    let cap_card = layout.leaf(right, Size::Fill(1));

    // Session card: exactly as tall as its rows need, never taller than the
    // space left under the band (`Hug` yields when the page is short).
    let needed = SESSION_CHROME_H + sessions.max(2) as i32 * Metrics::ROW_H;
    let sessions = layout.leaf(0, Size::Hug(needed));
    layout = layout.lead(sessions, L::GAP);
    let session_inner = layout.stack(sessions, Axis::Column, Size::Fill(1));
    layout = layout.pad(session_inner, SESSION_PAD);
    let _session_title = layout.leaf(session_inner, Size::Fixed(CARD_TITLE_H));
    let session_head = layout.leaf(session_inner, Size::Fixed(34));
    // The row area is the columns' container: a row rect is a sub-rect of it, so
    // the columns and the rows can never be laid out against different boxes.
    let session_rows = layout.stack(session_inner, Axis::Row, Size::Fill(1));
    layout = layout.lead(session_rows, 8);
    let mut session_cols = [0usize; 5];
    for (i, size) in [
        Size::Fixed(SESSION_ROLE_W),
        Size::Fixed(SESSION_HEALTH_W),
        Size::Fill(1),
        Size::Fixed(SESSION_TRAFFIC_W),
        Size::Fixed(SESSION_ACTION_W),
    ]
    .into_iter()
    .enumerate()
    {
        session_cols[i] = layout.leaf(session_rows, size);
    }

    ConsoleLayout {
        layout,
        header,
        summary,
        scope_card,
        scope_plot,
        start_card,
        start_buttons,
        copy_token,
        cap_card,
        sessions,
        session_head,
        session_cols,
        session_rows,
    }
}

/// Gap between the traffic numbers and the action column. Without it the
/// right-aligned numbers end exactly where the 停止 button starts, and the two
/// read as one crowded blob.
const COL_GAP: i32 = 16;

/// Height of a row's two text lines (headline + detail). The row itself is
/// `Metrics::ROW_H`; the difference is split above and below so the block is
/// centred in the row instead of hugging its top edge.
const ROW_CONTENT_H: i32 = 34;

/// Rows that fit in a session row area.
///
/// The area comes from the layout tree, so the count, the first row's y and the
/// scroll track all derive from one rectangle. This module used to compute the
/// first row's y a second time from the card rect; that duplicate is what let a
/// painter and a hit test disagree.
fn list_rows(area: Rect) -> usize {
    (area.h / Metrics::ROW_H).max(1) as usize
}

/// Pixel scroll from a bar drag → first visible row.
fn rows_for(scroll_px: i32) -> usize {
    (scroll_px / Metrics::ROW_H).max(0) as usize
}

/// Scroll bar for the session table, or `None` when every row fits.
///
/// `area` is the solved row area: the track spans exactly the rows that are
/// drawn, so it cannot leave the card.
fn session_bar(card: Rect, area: Rect, s: &ConsoleState) -> Option<ScrollBar> {
    let total = s.data.sessions.len() as i32;
    // Sit in the card's right padding, not on top of the content: the last
    // column here is the 停止 button, and an inside-the-content bar lands on it.
    ScrollBar::new(
        card.right() - 11,
        area.y,
        area.h.max(20),
        total * Metrics::ROW_H,
        list_rows(area) as i32 * Metrics::ROW_H,
        s.scroll as i32 * Metrics::ROW_H,
    )
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn paint(x: i32, y: i32, w: i32, h: i32, s: &ConsoleState) {
    th::fill(x, y, w, h, C::BG);
    let d = &s.data;
    let geometry = build_layout(d.sessions.len());
    let solved = geometry.solve(Rect::new(x, y, w, h));
    page_header_in(
        solved.get(geometry.header),
        "链路总览",
        "实时链路、流量与会话；从这里启动和停止每一端",
    );

    paint_summary(solved.get(geometry.summary), d);
    paint_scope_card(solved.get(geometry.scope_card), d);
    paint_right_column(
        solved.get(geometry.start_card),
        solved.get(geometry.cap_card),
        d,
    );
    paint_sessions(&geometry, &solved, s);
}

fn paint_summary(area: Rect, d: &ConsoleData) {
    th::fill_round(area.x, area.y, area.w, area.h, L::RADIUS, C::PANEL);
    th::stroke_round(area.x, area.y, area.w, area.h, L::RADIUS, C::LINE);

    let (sent_bps, recv_bps) = d.rates();
    let cells = [
        (
            "下行",
            fmt_rate(recv_bps),
            if recv_bps > 0.0 { C::AMBER } else { C::TEXT },
        ),
        (
            "上行",
            fmt_rate(sent_bps),
            if sent_bps > 0.0 { C::SAND } else { C::TEXT },
        ),
        (
            "压缩节省",
            fmt_pct(d.savings),
            if d.savings > 0.0 { C::OK } else { C::TEXT },
        ),
        ("累计流量", fmt_bytes(d.total_recv + d.total_sent), C::TEXT),
        (
            "活动通道",
            d.channels.to_string(),
            if d.channels > 0 { C::AMBER } else { C::TEXT },
        ),
    ];
    let cell_w = area.w / cells.len() as i32;
    for (i, (cap, value, color)) in cells.iter().enumerate() {
        let cx = area.x + 20 + cell_w * i as i32;
        stat_in(
            Rect::new(cx, area.y + 12, cell_w - 24, Metrics::SUMMARY_H - 12),
            cap,
            value,
            "",
            *color,
        );
        if i > 0 {
            th::vline(
                area.x + cell_w * i as i32,
                area.y + 14,
                area.h - 28,
                C::LINE,
            );
        }
    }
}

fn paint_scope_card(area: Rect, d: &ConsoleData) {
    let inner = card_in(area);
    card_title_in(inner, "链路示波器", None);
    // Live mark: a dot plus a word. The dot alone would not survive a
    // grayscale screenshot; the word alone would not read as state.
    let (ix, iy, iw) = (inner.x, inner.y, inner.w);
    if d.live {
        let note = "实时";
        let nw = th::measure(note, th::ui_font(), T::MICRO);
        // Centred in the same 20-unit row the label was always drawn in, so
        // only the dot's position changes.
        let line = th::LineBox::ui(ix + iw - nw - 6, iy, nw + 6, CARD_TITLE_H, T::MICRO);
        th::dot(ix + iw - nw - 14, line.center_y(), 3, C::OK);
        th::line_text(note, &line, th::ui_font(), C::TEXT_2);
    } else {
        th::micro("无活动链路", ix + iw - 76, iy, 76, CARD_TITLE_H, C::TEXT_3);
    }
}

fn paint_right_column(start: Rect, cap: Rect, d: &ConsoleData) {
    let inner = card_in(start);
    card_title_in(inner, "启动", Some("三选一"));

    // Token block: caption, value, then the rule that explains both. Three
    // separate boxes with explicit gaps — the block used to be positioned as
    // `ty`, `ty + 10`, `ty + 30`, which overlapped the caption with the value
    // and left the copy control straddling two lines. See DESIGN.md §3.
    let (ix, iw) = (inner.x, inner.w);
    let token_label_y = start.y + 114;
    let token_value_y = start.y + 130;
    let token_hint_y = start.y + 152;
    th::micro("令牌", ix, token_label_y, 40, 18, C::TEXT_3);
    let token = if d.token_hint.is_empty() {
        "尚未生成令牌".to_string()
    } else {
        d.token_hint.clone()
    };
    let token_color = if d.token_hint.is_empty() {
        C::TEXT_3
    } else {
        C::TEXT
    };
    // The value stops short of the copy control on this row.
    let shown = th::ellipsize(&token, iw - 58, th::mono_font(), T::SMALL);
    th::mono(
        &shown,
        ix,
        token_value_y,
        iw - 58,
        20,
        T::SMALL,
        token_color,
    );
    th::micro(
        "两端必须同一令牌，留空则随机生成",
        ix,
        token_hint_y,
        iw,
        16,
        C::TEXT_3,
    );

    // Capability card directly under it: its rect comes from the layout tree,
    // so the two cards cannot overlap however tall the start card becomes.
    let cap_inner = card_in(cap);
    card_title_in(cap_inner, "本次协商", None);

    // Capability tags wrap onto as many rows as needed; enabled ones are
    // amber-tinted, unavailable ones stay neutral.
    let mut tx = cap_inner.x;
    let mut tag_y = cap_inner.y + 26;
    for (name, on) in &d.capabilities {
        let tw = th::measure(name, th::mono_font(), T::MICRO) + 14;
        if tx + tw > cap_inner.right() {
            tx = cap_inner.x;
            tag_y += 24;
        }
        if tag_y + 18 > cap.bottom() - 12 {
            break;
        }
        let fill = if *on {
            th::mix(C::AMBER, C::PANEL, 0.16)
        } else {
            C::RAISED
        };
        th::fill_round(tx, tag_y, tw, 18, 4, fill);
        th::mono(
            name,
            tx + 7,
            tag_y,
            tw - 14,
            18,
            T::MICRO,
            if *on { C::AMBER } else { C::TEXT_3 },
        );
        tx += tw + 6;
    }
}

fn paint_sessions(geometry: &ConsoleLayout, solved: &Solved, s: &ConsoleState) {
    let d = &s.data;
    let card = geometry.session_card(solved);
    let inner = card_in(card);
    let area = geometry.row_area(solved);
    let cols = geometry.column_rects(solved, 0);
    let [role, health, obj, traffic, action] = cols;
    card_title_in(inner, "会话", Some(&format!("{} 个活动", d.running())));

    // Column headers on the same column rects the rows use, and their band on
    // the solved head leaf, so a header can never drift from its column.
    let head = solved.get(geometry.session_head);
    let header_y = head.y + SESSION_HEAD_TEXT_INSET;
    th::micro("角色", role.x, header_y, role.w, 16, C::TEXT_3);
    th::micro("状态", health.x, header_y, health.w, 16, C::TEXT_3);
    th::micro("对象", obj.x, header_y, obj.w, 16, C::TEXT_3);
    th::micro("流量", traffic.x, header_y, traffic.w, 16, C::TEXT_3);
    th::micro("操作", action.x, header_y, action.w, 16, C::TEXT_3);
    th::divider(inner.x, header_y + 20, inner.w);

    let fit = list_rows(area);
    if d.sessions.is_empty() {
        empty_state_in(
            area,
            "还没有会话",
            "用上方按钮启动中继或客户端；运行中的会话会出现在这里",
        );
        return;
    }

    let end = d.sessions.len().min(s.scroll + fit);
    for slot in 0..(end.saturating_sub(s.scroll)) {
        let i = s.scroll + slot;
        let row = &d.sessions[i];
        let ry = area.y + slot as i32 * Metrics::ROW_H;
        let row_cols = geometry.column_rects(solved, slot);
        paint_session_row(&row_cols, ry, row);
        if let Some(r) = s.rows.get(slot) {
            let (sx, sy, sw, sh) = r.stop;
            let hovered = s.hover_stop == Some(slot);
            let pressed = s.pressed_stop == Some(slot);
            let (fill, text) = if pressed {
                (th::mix(C::BAD, C::PANEL, 0.34), C::BAD)
            } else if hovered {
                (th::mix(C::BAD, C::PANEL, 0.24), C::BAD)
            } else {
                (C::RAISED, C::TEXT_2)
            };
            th::fill_round(
                sx,
                sy + if pressed { 1 } else { 0 },
                sw,
                sh,
                L::RADIUS,
                fill,
            );
            th::text(
                "停止",
                sx,
                sy,
                sw,
                sh,
                th::ui_font(),
                T::MICRO,
                text,
                Align::Center,
            );
        }
    }

    if let Some(bar) = session_bar(card, area, s) {
        bar.paint(s.thumb_grab.is_some());
    }
}

/// 健康状态 → 颜色。颜色属于表现层，所以映射留在页面里（`model::Health` 只管
/// 语义与中文标签）。
fn health_color(health: Health) -> u32 {
    match health {
        Health::Running => C::OK,
        Health::Starting => C::AMBER,
        Health::Reconnecting => C::WARN,
        Health::Stopped => C::IDLE,
        Health::Failed => C::BAD,
    }
}

fn paint_session_row(cols: &[Rect; 5], y: i32, row: &SessionRow) {
    // Centre the two-line block in the row. It used to hang from the row's top
    // edge, so the 停止 button (centred in ROW_H) sat visibly low against it.
    let y = y + (Metrics::ROW_H - ROW_CONTENT_H) / 2;
    let [role, health, obj, traffic, _action] = *cols;
    th::small(row.role.label(), role.x, y, role.w, 20, C::TEXT);

    // Label and dot from one line box, both centred in the 20-unit row.
    let health_line = th::LineBox::ui(health.x + 12, y, health.w - 12, 20, T::SMALL);
    th::dot(
        health.x + 3,
        health_line.center_y(),
        3,
        health_color(row.health),
    );
    th::line_text(
        row.health.label(),
        &health_line,
        th::ui_font(),
        health_color(row.health),
    );

    let head = th::ellipsize(&row.headline, obj.w, th::ui_font(), T::BODY);
    th::body(&head, obj.x, y, obj.w, 20, C::TEXT);
    if !row.detail.is_empty() {
        let det = th::ellipsize(&row.detail, obj.w, th::mono_font(), T::MICRO);
        th::mono(&det, obj.x, y + 18, obj.w, 16, T::MICRO, C::TEXT_3);
    }

    // Keep the numbers clear of the action column.
    let tr_x = traffic.x;
    let tr_w = (traffic.w - COL_GAP).max(40);
    let (sent, recv) = (row.sent.unwrap_or(0), row.received.unwrap_or(0));
    if sent > 0 || recv > 0 {
        th::mono_right(
            &format!("↓ {}", fmt_bytes(recv)),
            tr_x,
            y,
            tr_w,
            18,
            T::MICRO,
            C::TEXT_2,
        );
        th::mono_right(
            &format!("↑ {}", fmt_bytes(sent)),
            tr_x,
            y + 17,
            tr_w,
            18,
            T::MICRO,
            C::TEXT_3,
        );
    } else if let Some(up) = row.uptime {
        th::mono_right(&fmt_uptime(up), tr_x, y + 8, tr_w, 18, T::MICRO, C::TEXT_3);
    }
}

#[cfg(test)]
mod geometry;
#[cfg(test)]
mod tests;
