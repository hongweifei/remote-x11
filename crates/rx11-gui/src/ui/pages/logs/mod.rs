//! 日志 — the console's flight recorder.
//!
//! The log view is a scrolling list of lines with a level filter and a search
//! box, and it is deliberately the plainest page in the app: monospaced,
//! one line per record, no cards, no wrapping. When something has gone wrong,
//! the operator wants to read text, not admire layout.
//!
//! Lines arrive incrementally. The page holds the tail it can display and
//! asks the app for more via `LogAction::NeedMore` when scrolled to the top;
//! the ring buffer in the data layer owns the full history, so the UI never
//! has to keep megabytes of text in widget memory.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::prelude::*;

use crate::model::LogLevel;
use crate::theme::{self as th, C, L, T};
use crate::ui::action::{emit_action, set_handler, ActionSlot};
use crate::ui::chrome::{empty_state_in, page_header_in, PAGE_HEADER_H};
use crate::ui::controls::{BtnKind, Button, Field};
use crate::ui::kit::Grip;
use crate::ui::layout::{Axis, Insets, Layout, NodeId, Rect, Size, Solved};
use crate::ui::scrollbar::ScrollBar;
use crate::ui::{Page, PageInput};

/// The filter, the unread watermark and the pixel-scroll bookkeeping — state
/// only, no FLTK, unit-tested on its own.
mod state;
use state::ROW_LINE_H;
pub use state::{LogEntry, LogView};

/// 级别 → 颜色。颜色属于表现层，所以映射留在页面里：`model::LogLevel` 只管语义
/// （严重度顺序 + 中文标签 + 三字母码）。
fn level_color(level: LogLevel) -> u32 {
    match level {
        LogLevel::Error => C::BAD,
        LogLevel::Warn => C::WARN,
        LogLevel::Info => C::TEXT,
        LogLevel::Debug | LogLevel::Trace => C::TEXT_3,
    }
}

/// What the log page asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogAction {
    Clear,
    /// Copy the currently filtered view to the clipboard.
    CopyVisible,
    /// Open the log directory / export to a file.
    Export,
}

struct LogState {
    view: LogView,
    /// The list's scrollbar gesture — anchor-model machine in [`Grip`]. The
    /// painter draws the knob at `grip.painting()` while held; the offset
    /// comes from the same number through `LogView::scroll_to`, the single
    /// write path that keeps follow and the unread watermark in sync.
    grip: Grip,
}

/// The log page.
pub struct LogPage {
    page: Page,
    filter: Field,
    b_clear: Button,
    b_export: Button,
    b_copy: Button,
    level_min: Button,
    st: Rc<RefCell<LogState>>,
    on_action: ActionSlot<LogAction>,
    rect: Rc<RefCell<(i32, i32, i32, i32)>>,
}

/// Width reserved for the "搜索" caption in front of the search box.
const SEARCH_LABEL_W: i32 = 44;

/// Height of the filter row: search box and its buttons.
const FILTER_H: i32 = 34;

/// Height of the per-level counts row.
const COUNTS_H: i32 = 18;

/// Vertical rhythm between the filter, counts and list: tighter than the gap
/// between cards, because they read as one instrument panel.
const SECTION_GAP: i32 = 8;

/// Width of the action buttons on the filter row.
const BUTTON_W: i32 = 96;

/// Width of the level-filter button.
const LEVEL_W: i32 = 120;

/// Inset from the well's edge to the text block (the well is inset like a card).
const WELL_PAD: Insets = Insets::xy(18, 16);

/// Distance from the text block's top to the first line.
const WELL_TOP_GAP: i32 = 4;

/// The page's layout tree: heading, filter row, counts row, list well.
///
/// Built once per paint and solved once; the painter, the widget placement and
/// the pointer handler all read the same solved rectangles, which is what keeps
/// the rows, the scrollbar and the hit testing from drifting apart.
struct LogLayout {
    layout: Layout,
    header: NodeId,
    field: NodeId,
    level: NodeId,
    export: NodeId,
    clear: NodeId,
    copy: NodeId,
    counts: NodeId,
    well: NodeId,
}

impl LogLayout {
    fn solve(&self, page: Rect) -> Solved {
        self.layout.solve(page)
    }

    /// The block log lines are drawn into: the well minus its card inset.
    fn text_area(&self, solved: &Solved) -> Rect {
        solved.get(self.well).inset(WELL_PAD)
    }
}

fn build_layout() -> LogLayout {
    let mut layout = Layout::column().pad(0, Insets::all(L::PAD));
    let header = layout.leaf(0, Size::Fixed(PAGE_HEADER_H));
    let body = layout.stack(0, Axis::Column, Size::Fill(1));
    layout = layout.gap(body, SECTION_GAP);

    // Filter row: search box, level button, then a tighter group of actions.
    let filter = layout.stack(body, Axis::Row, Size::Fixed(FILTER_H));
    layout = layout.gap(filter, 12);
    let field = layout.leaf(filter, Size::Fill(1));
    let level = layout.leaf(filter, Size::Fixed(LEVEL_W));
    let actions = layout.stack(filter, Axis::Row, Size::Fixed(BUTTON_W * 3));
    layout = layout.gap(actions, 8);
    let export = layout.leaf(actions, Size::Fixed(BUTTON_W));
    let clear = layout.leaf(actions, Size::Fixed(BUTTON_W));
    let copy = layout.leaf(actions, Size::Fill(1));

    let counts = layout.leaf(body, Size::Fixed(COUNTS_H));
    let well = layout.leaf(body, Size::Fill(1));

    LogLayout {
        layout,
        header,
        field,
        level,
        export,
        clear,
        copy,
        counts,
        well,
    }
}

/// How many log lines fit in a well of this outer height.
///
/// Derived from the well's own height so a tall or short page cannot overshoot:
/// the row count and the scroll track length both come from here.
fn list_rows(well_h: i32) -> usize {
    // card padding 16 + top gap 4 + card padding 16
    ((well_h - 36) / ROW_LINE_H).max(1) as usize
}

impl LogPage {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(LogState {
            view: LogView::default(),
            grip: Grip::default(),
        }));
        let on_action: ActionSlot<LogAction> = Rc::new(RefCell::new(None));
        let rect = Rc::new(RefCell::new((x, y, w, h)));

        // Background first: FLTK draws a group's children in creation order.
        let page = {
            let st_draw = st.clone();
            Page::new(x, y, w, h, C::BG, move |px, py, pw, ph| {
                let s = st_draw.borrow();
                paint(px, py, pw, ph, &s);
            })
        };

        let filter = Field::new(0, 0, 10, 10, "");
        {
            let st_c = st.clone();
            let mut input = filter.input();
            input.set_callback(move |i| {
                let mut s = st_c.borrow_mut();
                s.view.filter = i.value();
                s.view.scroll_px = 0;
                // A filter change re-pins to the newest match; everything the
                // new view can show is by definition already seen.
                s.view.follow = true;
                s.view.seen_at_bottom = s.view.entries.len();
            });
        }

        let b_clear = Button::new(0, 0, 10, 10, "清空", BtnKind::Ghost);
        let b_export = Button::new(0, 0, 10, 10, "导出", BtnKind::Secondary);
        let b_copy = Button::new(0, 0, 10, 10, "复制当前视图", BtnKind::Ghost);
        let level_min = Button::new(0, 0, 10, 10, "级别：信息", BtnKind::Secondary);

        {
            let act = on_action.clone();
            b_clear.on_click(move || emit_action(&act, LogAction::Clear));
        }
        {
            let act = on_action.clone();
            b_export.on_click(move || emit_action(&act, LogAction::Export));
        }
        {
            let act = on_action.clone();
            b_copy.on_click(move || emit_action(&act, LogAction::CopyVisible));
        }
        {
            // Cycles 信息 → 调试 → 跟踪 → 警告 → 错误 → 信息, so the whole
            // level filter is reachable with one control and no menu.
            let st_c = st.clone();
            let lm = level_min.clone();
            level_min.on_click(move || {
                let next = {
                    let mut s = st_c.borrow_mut();
                    s.view.min_level = match s.view.min_level {
                        LogLevel::Info => LogLevel::Debug,
                        LogLevel::Debug => LogLevel::Trace,
                        LogLevel::Trace => LogLevel::Warn,
                        LogLevel::Warn => LogLevel::Error,
                        LogLevel::Error => LogLevel::Info,
                    };
                    s.view.follow = true;
                    s.view.seen_at_bottom = s.view.entries.len();
                    s.view.min_level
                };
                lm.set_label(&format!("级别：{}", next.label()));
            });
        }

        page.finish();

        let page = Self {
            page,
            filter,
            b_clear,
            b_export,
            b_copy,
            level_min,
            st,
            on_action,
            rect,
        };
        page.layout(x, y, w, h);
        page.install_input();
        page
    }

    /// Wheel, scrollbar-drag and keyboard input for the log list.
    ///
    /// Scrolling up leaves "follow" mode (the list stops jumping to the newest
    /// line); scrolling back to the bottom re-arms it, which is the behaviour a
    /// tail -f reader expects. Both moves go through `LogView::scroll_to`, the
    /// only writer of the offset, so the follow flag, the unread watermark and
    /// the whole-group repaint cannot desynchronise from the offset itself.
    /// The gesture machine is the shared [`Grip`]: this page contributes the
    /// bar (rebuilt from the same solve the painter used), the offset writes,
    /// the wheel rhythm (`dy` rows per notch) and the repaint.
    fn install_input(&self) {
        let st = self.st.clone();
        let rect = self.rect.clone();
        let page = self.page.clone();
        self.page.on_input(move |input| {
            let (px, py, pw, ph) = *rect.borrow();
            let geometry = build_layout();
            let solved = geometry.solve(Rect::new(px, py, pw, ph));
            let rows = list_rows(solved.get(geometry.well).h);
            let mut grip = st.borrow().grip;
            let mut bar = || log_bar(&geometry, &solved, &st.borrow());
            let mut scroll_to = |v: i32| {
                let mut s = st.borrow_mut();
                let total = s.view.visible().len();
                s.view.scroll_to(v, total, rows)
            };
            let mut scroll_by = |d: i32| {
                let mut s = st.borrow_mut();
                let total = s.view.visible().len();
                let cur = s.view.effective_scroll(total, rows);
                s.view.scroll_to(cur + d, total, rows)
            };
            let mut wheel = |dy: i32| {
                let mut s = st.borrow_mut();
                let total = s.view.visible().len();
                if total <= rows {
                    return false;
                }
                let cur = s.view.effective_scroll(total, rows);
                let changed = s.view.scroll_to(cur + dy * ROW_LINE_H, total, rows);
                drop(s);
                if changed {
                    page.redraw();
                }
                true
            };
            let mut repaint = || page.redraw();
            let consumed = match input {
                PageInput::Key(key) => grip.on_key(
                    key,
                    ROW_LINE_H,
                    &mut bar,
                    &mut scroll_by,
                    &mut scroll_to,
                    &mut repaint,
                ),
                _ => grip.on_input(
                    input,
                    &mut wheel,
                    &mut bar,
                    &mut scroll_by,
                    &mut scroll_to,
                    &mut repaint,
                ),
            };
            st.borrow_mut().grip = grip;
            consumed
        });
    }

    pub fn on_action<F: Fn(LogAction) + 'static>(&self, f: F) {
        set_handler(&self.on_action, f);
    }

    /// Replace the tail, keeping the filter, scroll and follow state.
    pub fn set_view(&self, view: LogView) {
        // Only the entries come from the app each tick. Level, filter, scroll
        // and follow are the operator's, not the poller's: overwriting them made
        // a wheel scroll snap back half a second later. The cap and the unread
        // watermark move with the entries — `adopt_entries` owns both.
        let mut s = self.st.borrow_mut();
        s.view.adopt_entries(view.entries);
        drop(s);
        self.filter.set_value(&self.st.borrow().view.filter);
        // New lines under follow must land in view: repaint the whole group,
        // not just the canvas.
        self.page.redraw();
    }

    pub fn view(&self) -> LogView {
        self.st.borrow().view.clone()
    }

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
        let geometry = build_layout();
        let solved = geometry.solve(Rect::new(x, y, w, h));
        let r = solved.get(geometry.field);
        self.filter.resize(r.x, r.y, r.w, r.h);
        let r = solved.get(geometry.level);
        self.level_min.resize(r.x, r.y, r.w, r.h);
        let r = solved.get(geometry.export);
        self.b_export.resize(r.x, r.y, r.w, r.h);
        let r = solved.get(geometry.clear);
        self.b_clear.resize(r.x, r.y, r.w, r.h);
        let r = solved.get(geometry.copy);
        self.b_copy.resize(r.x, r.y, r.w, r.h);
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Scroll bar for the log list, or `None` when the whole filtered view fits.
///
/// Its track spans exactly the drawn rows, so it cannot poke past the well.
/// Every argument is in pixels and comes from the same geometry the painter
/// uses: the thumb's `scroll_at` inverse is the drag's offset, which is what
/// makes the thumb stay under the pointer one pixel at a time.
fn log_bar(geometry: &LogLayout, solved: &Solved, s: &LogState) -> Option<ScrollBar> {
    let text = geometry.text_area(solved);
    let rows = list_rows(solved.get(geometry.well).h);
    let total = s.view.visible().len();
    ScrollBar::new(
        // In the well's right padding, clear of the text columns.
        text.right() + 7,
        text.y + WELL_TOP_GAP,
        (rows as i32 * ROW_LINE_H).max(20),
        total as i32 * ROW_LINE_H,
        rows as i32 * ROW_LINE_H,
        s.view.effective_scroll(total, rows),
    )
}

fn paint(x: i32, y: i32, w: i32, h: i32, s: &LogState) {
    th::fill(x, y, w, h, C::BG);
    let geometry = build_layout();
    let solved = geometry.solve(Rect::new(x, y, w, h));
    page_header_in(
        solved.get(geometry.header),
        "日志",
        "本次运行的全部日志，按级别着色，可搜索",
    );

    let v = &s.view;
    let counts = v.counts();
    let visible = v.visible();

    // Search caption sits on the input's own line, left of the box.
    let field = solved.get(geometry.field);
    th::small(
        "搜索",
        field.x,
        field.y,
        SEARCH_LABEL_W - 8,
        FILTER_H,
        C::TEXT_2,
    );

    // Counts row: one figure per level, so the operator knows what a filter
    // would reveal before applying it. Right side carries the visible total.
    let counts_row = solved.get(geometry.counts);
    let levels = [
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
        LogLevel::Trace,
    ];
    let mut cx = counts_row.x;
    for lv in levels {
        let n = counts[lv as usize];
        let color = if n > 0 { level_color(lv) } else { C::TEXT_3 };
        th::mono(
            &format!("{} {}", lv.code(), n),
            cx,
            counts_row.y,
            64,
            COUNTS_H,
            T::MICRO,
            color,
        );
        cx += 68;
    }
    // Right-aligned on the counts row: the counts themselves own the left.
    let shown = visible.len();
    th::text(
        &format!("显示 {shown} / 共 {}", v.entries.len()),
        counts_row.x,
        counts_row.y,
        counts_row.w,
        COUNTS_H,
        th::ui_font(),
        T::MICRO,
        C::TEXT_3,
        fltk::enums::Align::Right,
    );

    // The well and its text block both come from the solved tree.
    let well = solved.get(geometry.well);
    th::fill_round(well.x, well.y, well.w, well.h, L::RADIUS, C::SUNKEN);
    th::stroke_round(well.x, well.y, well.w, well.h, L::RADIUS, C::LINE);
    let text = geometry.text_area(&solved);

    if visible.is_empty() {
        empty_state_in(
            text,
            if v.entries.is_empty() {
                "还没有日志"
            } else {
                "当前筛选下没有日志"
            },
            if v.entries.is_empty() {
                "启动任意一端后，运行日志会实时出现在这里"
            } else {
                "换一个级别或清空搜索词"
            },
        );
        return;
    }

    let rows = list_rows(well.h);
    // One pixel offset drives everything below: the first row, its overshoot,
    // the clipped draw window and the scrollbar all come from the same pair.
    let scroll = v.effective_scroll(visible.len(), rows);
    let (start, frac, count) = LogView::row_window(scroll, visible.len(), rows);

    let (ix, iw) = (text.x, text.w);
    let track_top = text.y + WELL_TOP_GAP;
    let track_h = rows as i32 * ROW_LINE_H;
    // Clip to the track the scrollbar spans: a mid-drag frame shows the rows
    // cut by the track edge, never over the well border. Whole rows are never
    // cut (`max_scroll` ends are row-aligned), only intermediate frames are.
    th::push_clip(ix, track_top, iw, track_h);
    let mut ly = track_top - frac;
    for &idx in &visible[start..start + count] {
        let e = &v.entries[idx];
        // Gutter: level code, then time, then target, then the message. The
        // columns are fixed so a long target never pushes text out of line.
        th::mono(
            e.level.code(),
            ix,
            ly,
            40,
            ROW_LINE_H,
            T::MONO,
            level_color(e.level),
        );
        th::mono(&e.time, ix + 44, ly, 84, ROW_LINE_H, T::MONO, C::TEXT_3);
        let target = th::ellipsize(&e.target, 150, th::mono_font(), T::MONO);
        th::mono(&target, ix + 134, ly, 150, ROW_LINE_H, T::MONO, C::TEXT_3);
        // The message gets the remaining width; a pathological line is
        // ellipsized rather than wrapped, so rows stay a fixed height and the
        // scroll arithmetic stays honest.
        let msg_x = ix + 292;
        let msg_w = (iw - 292).max(80);
        let msg = th::ellipsize(&e.message, msg_w, th::mono_font(), T::MONO);
        th::mono(
            &msg,
            msg_x,
            ly,
            msg_w,
            ROW_LINE_H,
            T::MONO,
            level_color(e.level),
        );
        ly += ROW_LINE_H;
    }
    th::pop_clip();

    // Scrollbar for the list well. While held, the knob shows the drag
    // position, not the quantised offset's position.
    if let Some(bar) = log_bar(&geometry, &solved, s) {
        let (hot, knob) = s.grip.painting();
        bar.paint(hot, knob);
    }

    // The scroll position is shown by the shared scrollbar above; only the
    // paused-follow notice lives here. While history is being read the line
    // count that piled up behind the view is the point: "已暂停" says what
    // stopped, "还有 N 条未读" says what is waiting.
    if visible.len() > rows && !v.follow {
        let unread = v.unread(&visible);
        let notice = if unread > 0 {
            format!("还有 {unread} 条未读 · 滚到底部恢复")
        } else {
            "已暂停跟随 · 滚到底部恢复".to_string()
        };
        th::micro(&notice, well.x + 16, well.bottom() - 20, 300, 14, C::AMBER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_rows_never_returns_zero() {
        assert!(list_rows(100) >= 1);
        assert!(list_rows(2000) > list_rows(400));
    }

    /// The drawn rows must fit inside the well: overshooting here draws the last
    /// line over the well's bottom border (which is exactly what happened).
    #[test]
    fn rows_always_fit_the_well() {
        for h in [60, 120, 200, 400, 518, 900, 1400] {
            let rows = list_rows(h) as i32;
            assert!(rows >= 1, "h={h}: 至少一行");
            assert!(
                rows * ROW_LINE_H <= (h - 36).max(ROW_LINE_H),
                "h={h}: {rows} 行放不进井里"
            );
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// The filter row must keep its three columns: a search box that fills the
    /// slack, the level button, and the action group pinned to the right.
    #[test]
    fn filter_row_gives_the_slack_to_the_search_box() {
        let geometry = build_layout();
        let solved = geometry.solve(Rect::new(0, 0, 1180, 714));
        let field = solved.get(geometry.field);
        let level = solved.get(geometry.level);
        let export = solved.get(geometry.export);
        let clear = solved.get(geometry.clear);
        let copy = solved.get(geometry.copy);
        assert_eq!(level.w, LEVEL_W);
        assert_eq!(export.w, BUTTON_W);
        assert_eq!(clear.w, BUTTON_W);
        // The last action takes whatever is left of its group, so the group
        // ends flush instead of leaving a sliver.
        assert!(
            copy.w > 0 && copy.w <= BUTTON_W,
            "copy fills the group's remainder: {copy:?}"
        );
        assert!(field.w > 0);
        assert!(
            field.right() < level.x,
            "search box must not overlap the level button: {field:?} {level:?}"
        );
        assert!(
            level.right() < export.x,
            "level button must sit left of the action group"
        );
        assert_eq!(
            copy.right(),
            solved.get(geometry.header).right(),
            "the action group ends flush with the content edge"
        );
    }
}
