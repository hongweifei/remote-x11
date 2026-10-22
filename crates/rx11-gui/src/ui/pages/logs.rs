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
use crate::ui::controls::{BtnKind, Button, Field};
use crate::ui::{card, page_header, Page};

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

/// One rendered log record.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Pre-formatted local time, `HH:MM:SS.mmm`.
    pub time: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

/// The page's view state, owned by the app so the filter survives page
/// switches (an operator who filtered to errors does not want to re-filter
/// after glancing at the config).
#[derive(Debug, Clone)]
pub struct LogView {
    pub entries: Vec<LogEntry>,
    /// Minimum level shown.
    pub min_level: LogLevel,
    /// Substring filter, matched case-insensitively against message+target.
    pub filter: String,
    /// Scroll offset in rows from the top of the filtered list.
    pub scroll: usize,
    /// Whether to keep the view pinned to the newest line.
    pub follow: bool,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            min_level: LogLevel::Info,
            filter: String::new(),
            scroll: 0,
            follow: true,
        }
    }
}

impl LogView {
    /// Indices of the entries that pass the level and text filters.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.level <= self.min_level)
            .filter(|(_, e)| {
                needle.is_empty()
                    || e.message.to_lowercase().contains(&needle)
                    || e.target.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Counts per level, shown next to the filter so the operator can see
    /// whether filtering to errors would show anything at all.
    pub fn counts(&self) -> [usize; 5] {
        let mut c = [0usize; 5];
        for e in &self.entries {
            c[e.level as usize] += 1;
        }
        c
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

/// Rows of log text that fit in the list area, derived from the page height.
fn visible_rows(h: i32) -> usize {
    ((h - L::PAD * 2 - 64 - 20 - 78) / ROW_LINE_H).max(1) as usize
}

const ROW_LINE_H: i32 = 20;

impl LogPage {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(LogState {
            view: LogView::default(),
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
                s.view.scroll = 0;
                s.view.follow = true;
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
        page
    }

    pub fn on_action<F: Fn(LogAction) + 'static>(&self, f: F) {
        set_handler(&self.on_action, f);
    }

    /// Replace the tail, keeping the filter, level and follow state.
    pub fn set_view(&self, view: LogView) {
        let level = view.min_level;
        let filter = view.filter.clone();
        let mut s = self.st.borrow_mut();
        s.view = view;
        s.view.min_level = level;
        s.view.filter = filter;
        drop(s);
        self.filter.set_value(&self.st.borrow().view.filter);
        // The list scrolls itself; keep the newest line in view.
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
        let pad = L::PAD;
        let content_x = x + pad;
        let content_w = w - pad * 2;
        let bar_y = y + pad + 64 + 20;

        // Filter row: search box on the left, controls on the right.
        let btn_w = 96;
        let filter_w = (content_w - btn_w * 3 - 24 - 120).max(160);
        self.filter.resize(content_x, bar_y, filter_w, 34);
        let mut bx = content_x + filter_w + 12;
        self.level_min.resize(bx, bar_y, 120, 34);
        bx += 132;
        self.b_export.resize(bx, bar_y, btn_w, 34);
        bx += btn_w + 8;
        self.b_clear.resize(bx, bar_y, btn_w, 34);
        bx += btn_w + 8;
        self.b_copy
            .resize(bx, bar_y, content_w - (bx - content_x), 34);
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn paint(x: i32, y: i32, w: i32, h: i32, s: &LogState) {
    th::fill(x, y, w, h, C::BG);
    let pad = L::PAD;
    let content_x = x + pad;
    let content_w = w - pad * 2;
    page_header(
        content_x,
        y + pad,
        content_w,
        "日志",
        "本次运行的全部日志，按级别着色，可搜索",
    );

    let v = &s.view;
    let counts = v.counts();
    let visible = v.visible();
    let bar_y = y + pad + 64 + 20;

    // Search caption sits on the input's own line, left of the box.
    th::small("搜索", content_x, bar_y, SEARCH_LABEL_W - 8, 34, C::TEXT_2);

    // Counts row: one figure per level, so the operator knows what a filter
    // would reveal before applying it. Right side carries the visible total.
    let counts_y = bar_y + 42;
    let levels = [
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
        LogLevel::Trace,
    ];
    let mut cx = content_x;
    for lv in levels {
        let n = counts[lv as usize];
        let color = if n > 0 { level_color(lv) } else { C::TEXT_3 };
        th::mono(
            &format!("{} {}", lv.code(), n),
            cx,
            counts_y,
            64,
            18,
            T::MICRO,
            color,
        );
        cx += 68;
    }
    // Right-aligned on the counts row: the counts themselves own the left.
    let shown = visible.len();
    th::text(
        &format!("显示 {shown} / 共 {}", v.entries.len()),
        content_x,
        counts_y,
        content_w,
        18,
        th::ui_font(),
        T::MICRO,
        C::TEXT_3,
        fltk::enums::Align::Right,
    );

    let list_y = counts_y + 26;

    // List well.
    let list_h = (y + h - pad - list_y).max(60);
    let (ix, iy, iw) = card(content_x, list_y, content_w, list_h);
    th::fill_round(content_x, list_y, content_w, list_h, L::RADIUS, C::SUNKEN);
    th::stroke_round(content_x, list_y, content_w, list_h, L::RADIUS, C::LINE);

    if visible.is_empty() {
        crate::ui::empty_state(
            ix,
            list_y + 20,
            iw,
            list_h - 40,
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

    let rows = visible_rows(h);
    // `scroll` is an offset from the top; when following, pin to the end.
    let start = if v.follow {
        visible.len().saturating_sub(rows)
    } else {
        v.scroll.min(visible.len().saturating_sub(1))
    };
    let end = (start + rows).min(visible.len());

    let mut ly = iy + 4;
    for &idx in &visible[start..end] {
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

    // Scroll indicator: position within the filtered list.
    if visible.len() > rows {
        let total = visible.len();
        let frac = if total > rows {
            start as f64 / (total - rows) as f64
        } else {
            0.0
        };
        let track_h = list_h - 24;
        let thumb_h = ((track_h as f64) * rows as f64 / total as f64).max(24.0) as i32;
        let thumb_y = list_y + 12 + ((track_h - thumb_h) as f64 * frac) as i32;
        th::fill_round(
            content_x + content_w - 8,
            thumb_y,
            3,
            thumb_h,
            1,
            th::mix(C::TEXT_3, C::SUNKEN, 0.7),
        );
        if !v.follow {
            th::micro(
                "已暂停跟随 · 滚到底部恢复",
                content_x + 16,
                list_y + list_h - 20,
                300,
                14,
                C::AMBER,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(_seq: u64, level: LogLevel, msg: &str) -> LogEntry {
        LogEntry {
            time: "12:00:00.000".into(),
            level,
            target: "rx11_server::relay".into(),
            message: msg.into(),
        }
    }

    #[test]
    fn level_filter_keeps_everything_at_or_above() {
        let mut v = LogView {
            entries: vec![
                entry(1, LogLevel::Error, "boom"),
                entry(2, LogLevel::Warn, "careful"),
                entry(3, LogLevel::Info, "hello"),
                entry(4, LogLevel::Debug, "detail"),
            ],
            min_level: LogLevel::Warn,
            ..LogView::default()
        };
        assert_eq!(v.visible().len(), 2);
        v.min_level = LogLevel::Trace;
        assert_eq!(v.visible().len(), 4);
    }

    #[test]
    fn text_filter_matches_message_and_target_case_insensitively() {
        let mut v = LogView {
            entries: vec![
                entry(1, LogLevel::Info, "Relay listening on 0.0.0.0:7000"),
                entry(2, LogLevel::Info, "session created"),
            ],
            filter: "LISTENING".into(),
            ..LogView::default()
        };
        let vis = v.visible();
        assert_eq!(vis.len(), 1);
        assert_eq!(v.entries[vis[0]].message, "Relay listening on 0.0.0.0:7000");

        v.filter = "rx11_server".into();
        assert_eq!(v.visible().len(), 2);

        v.filter = "  ".into();
        assert_eq!(v.visible().len(), 2, "blank filter must not filter");
    }

    #[test]
    fn counts_report_every_level() {
        let v = LogView {
            entries: vec![
                entry(1, LogLevel::Error, "a"),
                entry(2, LogLevel::Error, "b"),
                entry(3, LogLevel::Info, "c"),
            ],
            ..LogView::default()
        };
        let c = v.counts();
        assert_eq!(c[LogLevel::Error as usize], 2);
        assert_eq!(c[LogLevel::Info as usize], 1);
        assert_eq!(c[LogLevel::Debug as usize], 0);
    }

    #[test]
    fn visible_rows_never_returns_zero() {
        assert!(visible_rows(100) >= 1);
        assert!(visible_rows(2000) > visible_rows(400));
    }
}
