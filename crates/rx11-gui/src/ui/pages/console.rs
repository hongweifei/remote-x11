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
use crate::ui::controls::{BtnKind, Button};
use crate::ui::scope::{Scope, ScopeData};
use crate::ui::scrollbar::{Hit, ScrollBar};
use crate::ui::{card, card_title, page_header, Page, PageInput};

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
            let scrollable = {
                let s = st.borrow();
                s.data.sessions.len() > visible_session_rows(px, py, pw, ph, s.data.sessions.len())
            };
            match input {
                PageInput::Wheel(dy) => {
                    if dy == 0 || !scrollable {
                        return false;
                    }
                    let step = if dy > 0 { 1 } else { -1 };
                    let cur = st.borrow().scroll as i32;
                    let max = max_session_scroll(px, py, pw, ph, &st.borrow());
                    st.borrow_mut().scroll = (cur + step).clamp(0, max) as usize;
                    page.redraw();
                    true
                }
                PageInput::Push(lx, ly) => {
                    let (card_x, card_y, card_w, card_h) =
                        sessions_rect(px, py, pw, ph, st.borrow().data.sessions.len());
                    let Some(bar) = session_bar(card_x, card_y, card_w, card_h, &st.borrow())
                    else {
                        return false;
                    };
                    match bar.hit(lx + px, ly + py) {
                        Some(Hit::Thumb { grab }) => {
                            st.borrow_mut().thumb_grab = Some(grab);
                            page.redraw();
                            true
                        }
                        Some(Hit::Track { dir }) => {
                            let max = max_session_scroll(px, py, pw, ph, &st.borrow());
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
                    let (card_x, card_y, card_w, card_h) =
                        sessions_rect(px, py, pw, ph, st.borrow().data.sessions.len());
                    let Some(bar) = session_bar(card_x, card_y, card_w, card_h, &st.borrow())
                    else {
                        return false;
                    };
                    let next = rows_for(bar.scroll_at(ly + py - grab));
                    let max = max_session_scroll(px, py, pw, ph, &st.borrow());
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

        let pad = L::PAD;
        let content_x = x + pad;
        let content_w = w - pad * 2;
        let mut cy = header_bottom(y, pad);
        cy += Metrics::SUMMARY_H + L::GAP;

        let left_w = content_w - Metrics::RIGHT_W - L::GAP;
        self.scope
            .resize(content_x + 18, cy + 36, left_w - 36, Metrics::BAND_H - 54);

        let rx = content_x + left_w + L::GAP;
        let (bx, by, bw) = (rx + 18, cy + 46, Metrics::RIGHT_W - 36);
        let half = (bw - 8) / 2;
        self.start_server.resize(bx, by, half, 30);
        self.start_client.resize(bx + half + 8, by, half, 30);
        self.start_ssh.resize(bx, by + 38, bw, 28);

        // Copy sits on the token row's right edge, inside the start card.
        // On the token value's row: `by + 83` is `y + 129`, i.e. a 22-unit
        // button centred on the value at y+140 (see `paint_right_column`).
        self.copy_token.resize(bx + bw - 42, by + 83, 42, 22);

        // Session rows: geometry only. `sessions_rect` is the single source for
        // where the card sits and how tall it is, so hit testing agrees with
        // paint even as the card shrinks to fit its rows.
        cy += Metrics::BAND_H + L::GAP;
        let _ = cy;
        let (_, sy, _, sh) = sessions_rect(x, y, w, h, self.st.borrow().data.sessions.len());
        let (first_row_y, fit) = session_rows(sy, sh);
        let ix = content_x + 18;
        let iw = content_w - 36;
        let stop_w = 54;
        let mut rows = Vec::new();
        let (start, end, list) = {
            let st = self.st.borrow();
            (
                st.scroll,
                st.data.sessions.len().min(st.scroll + fit),
                st.data.sessions.clone(),
            )
        };
        for (slot, i) in (start..end).enumerate() {
            let ry = first_row_y + slot as i32 * Metrics::ROW_H;
            rows.push(RowHit {
                session_id: list[i].id,
                stop: (ix + iw - stop_w, ry + (Metrics::ROW_H - 24) / 2, stop_w, 24),
            });
        }
        self.st.borrow_mut().rows = rows;
    }
}

/// Where the session card sits on a page of this size.
///
/// The only place this geometry is decided: paint, layout, the scrollbar and
/// the pointer handler all call it, so they cannot drift apart.
fn sessions_rect(x: i32, y: i32, w: i32, h: i32, rows: usize) -> (i32, i32, i32, i32) {
    let pad = L::PAD;
    let content_x = x + pad;
    let content_w = w - pad * 2;
    let mut cy = header_bottom(y, pad) + Metrics::SUMMARY_H + L::GAP;
    cy += Metrics::BAND_H + L::GAP;
    let available = (h - (cy - y) - pad).max(120);
    // The card is as tall as its rows need, and only grows to the available
    // height when the list overflows (then it scrolls). Stretching it to the
    // page bottom always left a big empty well under two rows.
    // At least two rows tall: the empty state needs room to breathe, and it
    // keeps the card from jumping around as sessions start and stop.
    let needed = card_chrome_h() + rows.max(2) as i32 * Metrics::ROW_H;
    let list_h = available.min(needed);
    (content_x, cy, content_w, list_h)
}

/// Everything in the session card above the first row, plus the bottom padding:
/// card padding + title row + column headers + header gap + 8.
fn card_chrome_h() -> i32 {
    // padding 16 + title 20 + headers 34 + gap 28 + bottom padding 8
    16 + 20 + 34 + 28 + 8
}

/// Gap between the traffic numbers and the action column. Without it the
/// right-aligned numbers end exactly where the 停止 button starts, and the two
/// read as one crowded blob.
const COL_GAP: i32 = 16;

/// Height of a row's two text lines (headline + detail). The row itself is
/// `Metrics::ROW_H`; the difference is split above and below so the block is
/// centred in the row instead of hugging its top edge.
const ROW_CONTENT_H: i32 = 34;

/// First row's y and how many rows fit, given the session **card** rect.
///
/// Takes the card rect (not the header line) so the arithmetic can subtract the
/// card's padding, the title row and the column headers. Getting this wrong
/// does not fail to compile — it draws the last row past the card's bottom edge,
/// over the status bar.
fn session_rows(card_y: i32, card_h: i32) -> (i32, usize) {
    // First row starts right below the card's chrome; the same constant is what
    // `sessions_rect` adds when it sizes the card, so the two agree.
    let first_row_y = card_y + 78;
    let room = (card_y + card_h - 8 - first_row_y).max(Metrics::ROW_H);
    (first_row_y, (room / Metrics::ROW_H).max(1) as usize)
}

/// Number of session rows the table can show at this page size and row count.
fn visible_session_rows(x: i32, y: i32, w: i32, h: i32, rows: usize) -> usize {
    let (_, cy, _, list_h) = sessions_rect(x, y, w, h, rows);
    session_rows(cy, list_h).1
}

/// Highest first-row index that still fills the table.
fn max_session_scroll(x: i32, y: i32, w: i32, h: i32, s: &ConsoleState) -> i32 {
    let fit = visible_session_rows(x, y, w, h, s.data.sessions.len()) as i32;
    (s.data.sessions.len() as i32 - fit).max(0)
}

/// Pixel scroll from a bar drag → first visible row.
fn rows_for(scroll_px: i32) -> usize {
    (scroll_px / Metrics::ROW_H).max(0) as usize
}

/// Scroll bar for the session table, or `None` when every row fits.
fn session_bar(
    card_x: i32,
    card_y: i32,
    card_w: i32,
    card_h: i32,
    s: &ConsoleState,
) -> Option<ScrollBar> {
    // Takes the session CARD rect — the same rectangle `paint_sessions` draws
    // into. It used to be handed that rect but interpret it as the page rect,
    // which put the bar ~100 units below the window.
    //
    // Not `card()`: that function paints the card, erasing the rows above.
    let (ix, iw) = (card_x + 18, card_w - 36);
    let (first_row_y, fit) = session_rows(card_y, card_h);
    let total = s.data.sessions.len() as i32;
    // Sit in the card's 18-unit right padding, not on top of the content: the
    // last column here is the 停止 button, and an inside-the-content bar lands
    // on it.
    let _ = (ix, iw);
    ScrollBar::new(
        card_x + card_w - 11,
        first_row_y,
        (card_y + card_h - 8 - first_row_y).max(20),
        total * Metrics::ROW_H,
        fit as i32 * Metrics::ROW_H,
        s.scroll as i32 * Metrics::ROW_H,
    )
}

fn header_bottom(page_y: i32, pad: i32) -> i32 {
    // `page_header` draws a 64px heading block, then a rule, then 20px of air.
    page_y + pad + 64 + 20
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn paint(x: i32, y: i32, w: i32, h: i32, s: &ConsoleState) {
    th::fill(x, y, w, h, C::BG);
    let d = &s.data;
    let pad = L::PAD;
    let content_x = x + pad;
    let content_w = w - pad * 2;
    let mut cy = page_header(
        content_x,
        y + pad,
        content_w,
        "链路总览",
        "实时链路、流量与会话；从这里启动和停止每一端",
    );

    paint_summary(content_x, cy, content_w, d);
    cy += Metrics::SUMMARY_H + L::GAP;

    let left_w = content_w - Metrics::RIGHT_W - L::GAP;
    paint_scope_card(content_x, cy, left_w, d);
    paint_right_column(content_x + left_w + L::GAP, cy, d);

    // Card rect from the same helper the hit testing and the scrollbar use.
    let (sx, sy, sw, sh) = sessions_rect(x, y, w, h, d.sessions.len());
    paint_sessions(sx, sy, sw, sh, s);
}

fn paint_summary(x: i32, y: i32, w: i32, d: &ConsoleData) {
    let h = Metrics::SUMMARY_H;
    th::fill_round(x, y, w, h, L::RADIUS, C::PANEL);
    th::stroke_round(x, y, w, h, L::RADIUS, C::LINE);

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
    let cell_w = w / cells.len() as i32;
    for (i, (cap, value, color)) in cells.iter().enumerate() {
        let cx = x + 20 + cell_w * i as i32;
        crate::ui::stat(cx, y + 12, cell_w - 24, cap, value, "", *color);
        if i > 0 {
            th::vline(x + cell_w * i as i32, y + 14, h - 28, C::LINE);
        }
    }
}

fn paint_scope_card(x: i32, y: i32, w: i32, d: &ConsoleData) {
    th::fill_round(x, y, w, Metrics::BAND_H, L::RADIUS, C::PANEL);
    th::stroke_round(x, y, w, Metrics::BAND_H, L::RADIUS, C::LINE);
    let (ix, iy, iw) = card(x, y, w, Metrics::BAND_H);
    card_title(ix, iy, iw, "链路示波器", None);
    // Live mark: a dot plus a word. The dot alone would not survive a
    // grayscale screenshot; the word alone would not read as state.
    if d.live {
        let note = "实时";
        let nw = th::measure(note, th::ui_font(), T::MICRO);
        // Centred in the same 20-unit row the label was always drawn in, so
        // only the dot's position changes.
        let line = th::LineBox::ui(ix + iw - nw - 6, iy, nw + 6, 20, T::MICRO);
        th::dot(ix + iw - nw - 14, line.center_y(), 3, C::OK);
        th::line_text(note, &line, th::ui_font(), C::TEXT_2);
    } else {
        th::micro("无活动链路", ix + iw - 76, iy, 76, 20, C::TEXT_3);
    }
}

fn paint_right_column(x: i32, y: i32, d: &ConsoleData) {
    let w = Metrics::RIGHT_W;
    let h = Metrics::START_CARD_H;
    th::fill_round(x, y, w, h, L::RADIUS, C::PANEL);
    th::stroke_round(x, y, w, h, L::RADIUS, C::LINE);
    let (ix, iy, iw) = card(x, y, w, h);
    card_title(ix, iy, iw, "启动", Some("三选一"));

    // Token block: caption, value, then the rule that explains both. Three
    // separate boxes with explicit gaps — the block used to be positioned as
    // `ty`, `ty + 10`, `ty + 30`, which overlapped the caption with the value
    // and left the copy control straddling two lines. See DESIGN.md §3.
    let token_label_y = y + 114;
    let token_value_y = y + 130;
    let token_hint_y = y + 152;
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

    // Capability card directly under it.
    let cy = y + h + Metrics::CAP_CARD_GAP;
    let ch = Metrics::BAND_H - h - Metrics::CAP_CARD_GAP;
    th::fill_round(x, cy, w, ch, L::RADIUS, C::PANEL);
    th::stroke_round(x, cy, w, ch, L::RADIUS, C::LINE);
    let (cx, cyi, cw) = card(x, cy, w, ch);
    card_title(cx, cyi, cw, "本次协商", None);

    // Capability tags wrap onto as many rows as needed; enabled ones are
    // amber-tinted, unavailable ones stay neutral.
    let mut tx = cx;
    let mut tag_y = cyi + 26;
    for (name, on) in &d.capabilities {
        let tw = th::measure(name, th::mono_font(), T::MICRO) + 14;
        if tx + tw > cx + cw {
            tx = cx;
            tag_y += 24;
        }
        if tag_y + 18 > cy + ch - 12 {
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

fn paint_sessions(x: i32, y: i32, w: i32, h: i32, s: &ConsoleState) {
    let d = &s.data;
    th::fill_round(x, y, w, h, L::RADIUS, C::PANEL);
    th::stroke_round(x, y, w, h, L::RADIUS, C::LINE);
    let (ix, iy, iw) = card(x, y, w, h);
    card_title(ix, iy, iw, "会话", Some(&format!("{} 个活动", d.running())));

    // Column headers. Widths are fixed except the object column, which takes
    // the slack; the same arithmetic positions every row.
    let header_y = iy + 34;
    let (role_w, health_w, traffic_w, action_w) = (86, 96, 132, 54);
    let obj_w = iw - role_w - health_w - traffic_w - action_w;
    th::micro("角色", ix, header_y, role_w, 16, C::TEXT_3);
    th::micro("状态", ix + role_w, header_y, health_w, 16, C::TEXT_3);
    th::micro(
        "对象",
        ix + role_w + health_w,
        header_y,
        obj_w,
        16,
        C::TEXT_3,
    );
    th::micro(
        "流量",
        ix + role_w + health_w + obj_w,
        header_y,
        traffic_w,
        16,
        C::TEXT_3,
    );
    th::micro(
        "操作",
        ix + role_w + health_w + obj_w + traffic_w,
        header_y,
        action_w,
        16,
        C::TEXT_3,
    );
    th::divider(ix, header_y + 20, iw);

    let (first_row_y, fit) = session_rows(y, h);
    if d.sessions.is_empty() {
        crate::ui::empty_state(
            ix,
            first_row_y,
            iw,
            (h - 70).max(50),
            "还没有会话",
            "用上方按钮启动中继或客户端；运行中的会话会出现在这里",
        );
        return;
    }

    let end = d.sessions.len().min(s.scroll + fit);
    for (slot, i) in (s.scroll..end).enumerate() {
        let row = &d.sessions[i];
        let ry = first_row_y + slot as i32 * Metrics::ROW_H;
        paint_session_row(ix, ry, row, role_w, health_w, obj_w, traffic_w);
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

    if let Some(bar) = session_bar(x, y, w, h, s) {
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

fn paint_session_row(
    x: i32,
    y: i32,
    row: &SessionRow,
    role_w: i32,
    health_w: i32,
    obj_w: i32,
    traffic_w: i32,
) {
    // Centre the two-line block in the row. It used to hang from the row's top
    // edge, so the 停止 button (centred in ROW_H) sat visibly low against it.
    let y = y + (Metrics::ROW_H - ROW_CONTENT_H) / 2;
    th::small(row.role.label(), x, y, role_w, 20, C::TEXT);

    let health_x = x + role_w;
    // Label and dot from one line box, both centred in the 20-unit row.
    let health = th::LineBox::ui(health_x + 12, y, health_w - 12, 20, T::SMALL);
    th::dot(health_x + 3, health.center_y(), 3, health_color(row.health));
    th::line_text(
        row.health.label(),
        &health,
        th::ui_font(),
        health_color(row.health),
    );

    let obj_x = health_x + health_w;
    let head = th::ellipsize(&row.headline, obj_w, th::ui_font(), T::BODY);
    th::body(&head, obj_x, y, obj_w, 20, C::TEXT);
    if !row.detail.is_empty() {
        let det = th::ellipsize(&row.detail, obj_w, th::mono_font(), T::MICRO);
        th::mono(&det, obj_x, y + 18, obj_w, 16, T::MICRO, C::TEXT_3);
    }

    let tr_x = obj_x + obj_w;
    // Keep the numbers clear of the action column.
    let tr_w = (traffic_w - COL_GAP).max(40);
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
mod tests {
    use super::*;
    use crate::model::Role;

    fn sample(id: u64, role: Role) -> SessionRow {
        SessionRow {
            id,
            role,
            health: Health::Running,
            headline: "x".into(),
            detail: String::new(),
            sent: Some(1),
            received: Some(2),
            uptime: Some(3),
        }
    }

    #[test]
    fn rates_come_from_the_newest_sample() {
        assert_eq!(ConsoleData::default().rates(), (0.0, 0.0));
        let d = ConsoleData {
            scope: vec![
                Sample {
                    sent_bps: 1.0,
                    recv_bps: 2.0,
                },
                Sample {
                    sent_bps: 30.0,
                    recv_bps: 40.0,
                },
            ],
            ..ConsoleData::default()
        };
        assert_eq!(d.rates(), (30.0, 40.0));
    }

    #[test]
    fn running_counts_all_live_phases() {
        let mut d = ConsoleData {
            sessions: vec![
                sample(1, Role::Server),
                sample(2, Role::Client),
                sample(3, Role::Ssh),
            ],
            ..ConsoleData::default()
        };
        d.sessions[2].health = Health::Reconnecting;
        assert_eq!(d.running(), 3);
        d.sessions[0].health = Health::Stopped;
        assert_eq!(d.running(), 2);
    }
}
