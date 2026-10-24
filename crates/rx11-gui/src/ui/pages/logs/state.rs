//! Pure view state of the log stream: filters, the unread watermark and the
//! pixel-scroll bookkeeping.
//!
//! No drawing, no widgets, no FLTK. Two of this page's real bugs lived in
//! exactly this arithmetic — the row quantisation that made the thumb lag the
//! pointer, and a watermark that would silently drift when the tail was
//! truncated — and both were catchable, and now pinned, as plain unit tests
//! long before a screenshot could say anything about them.

use crate::model::LogLevel;
use crate::ui::scrollbar::{list_max_scroll, list_row_window};

/// One rendered log record.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Pre-formatted local time, `HH:MM:SS.mmm`.
    pub time: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

/// Line height of one log row. Rows are a fixed height so the row window can
/// be pure division on the pixel offset.
pub(super) const ROW_LINE_H: i32 = 20;

/// In-memory tail cap. The full history lives in the launcher's ring; the
/// page keeps only what a scrolling view can reach, so a long run cannot grow
/// the UI's copy without bound. Truncation shifts the unread watermark here,
/// which is why it belongs to the page and not to the poller.
pub(super) const TAIL_CAP: usize = 4000;

/// Scrollable range in pixels for a list of `total` rows in a window of
/// `rows`. Delegates to the shared [`list_max_scroll`] with this page's line
/// height, so the log well and the session table cannot disagree about the
/// arithmetic that decides when the bar appears.
pub(super) fn max_scroll(total: usize, rows: usize) -> i32 {
    list_max_scroll(total, rows, ROW_LINE_H)
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
    /// Scroll offset in **pixels** from the top of the filtered list.
    ///
    /// Pixels, not rows: a thumb drag maps the pointer to a pixel offset, and
    /// quantising that to a row index is what made dragging lag the pointer by
    /// up to a whole line. The row window is derived instead: `scroll_px /
    /// ROW_LINE_H` is the first drawn row and `scroll_px % ROW_LINE_H` is how
    /// far that row has moved past the top of the track, clipped, below the
    /// header. `0` and the maximum are both row-aligned, so resting the thumb
    /// at either end shows no sliver.
    pub scroll_px: i32,
    /// Whether to keep the view pinned to the newest line. Equivalent to the
    /// offset sitting at `max_scroll` — reaching the bottom re-arms it, which
    /// is the tail -f behaviour.
    pub follow: bool,
    /// `entries.len()` at the moment the view last sat at the bottom. Lines
    /// after that mark are "unread": they kept arriving while the operator
    /// was reading history, and the notice counts them.
    pub seen_at_bottom: usize,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            min_level: LogLevel::Info,
            filter: String::new(),
            scroll_px: 0,
            follow: true,
            seen_at_bottom: 0,
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

    /// The pixel offset the view paints at: following pins to the bottom, and
    /// a stored offset from before the newest arrivals can never exceed the
    /// current range.
    pub(super) fn effective_scroll(&self, total: usize, rows: usize) -> i32 {
        if self.follow {
            max_scroll(total, rows)
        } else {
            self.scroll_px.min(max_scroll(total, rows)).max(0)
        }
    }

    /// The row window for a pixel offset: first visible row, how far it has
    /// scrolled past the top of the track, and how many slots to draw (one
    /// extra so the partially scrolled-in row at the bottom shows). The
    /// arithmetic itself is the shared [`list_row_window`].
    pub(super) fn row_window(scroll_px: i32, total: usize, rows: usize) -> (usize, i32, usize) {
        list_row_window(scroll_px, total, rows, ROW_LINE_H)
    }

    /// Move the viewport to a pixel offset. Returns whether anything changed.
    ///
    /// The single write path for `scroll_px`: wheel, track click and thumb drag
    /// all go through it, so the follow re-arming and the unread watermark
    /// cannot be forgotten in one of them — and the caller repaints the whole
    /// group when the view actually moved.
    pub(super) fn scroll_to(&mut self, value: i32, total: usize, rows: usize) -> bool {
        let max = max_scroll(total, rows);
        let next = value.clamp(0, max);
        let now_follow = next >= max;
        let changed = next != self.scroll_px || now_follow != self.follow;
        self.scroll_px = next;
        if now_follow != self.follow {
            self.follow = now_follow;
            // Crossing either edge re-marks everything up to the newest line
            // as seen: arriving at the bottom clears the backlog; leaving it
            // freezes the watermark at the line the operator was looking at.
            self.seen_at_bottom = self.entries.len();
        }
        changed
    }

    /// New lines that arrived after the view last sat at the bottom, counting
    /// only those the current filter would actually show. Zero while following.
    pub(super) fn unread(&self, visible: &[usize]) -> usize {
        if self.follow {
            0
        } else {
            visible
                .iter()
                .filter(|&&i| i >= self.seen_at_bottom)
                .count()
        }
    }

    /// Install the poller's entry list (the old tail plus what arrived):
    /// cap it, keep the watermark aligned through the truncation, and hold a
    /// following view at the newest line.
    pub(super) fn adopt_entries(&mut self, mut entries: Vec<LogEntry>) {
        let drop = entries.len().saturating_sub(TAIL_CAP);
        if drop > 0 {
            entries.drain(..drop);
        }
        self.entries = entries;
        self.seen_at_bottom = self.seen_at_bottom.saturating_sub(drop);
        if self.follow {
            self.seen_at_bottom = self.entries.len();
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

    /// The two ends of the scroll range are row-aligned: `max_scroll` is a
    /// whole number of rows, so resting the thumb at top or bottom shows
    /// whole rows — only mid-drag frames are fractional.
    #[test]
    fn scroll_extends_are_row_aligned() {
        for (total, rows) in [(200usize, 20usize), (400, 24), (30, 7), (20, 20)] {
            let max = max_scroll(total, rows);
            assert_eq!(max % ROW_LINE_H, 0, "max {max} 必须落在整行上");
            let (first, frac, count) = LogView::row_window(max, total, rows);
            assert_eq!(frac, 0);
            assert_eq!(first, total.saturating_sub(rows));
            assert!(first + count <= total);
        }
    }

    /// The pixel offset is a continuous dial, and the window derives from it:
    /// the first row only advances every `ROW_LINE_H` pixels, and the
    /// remainder says how far that row has moved past the top. One extra slot
    /// is drawn so the row sliding in at the bottom is already there mid-drag.
    #[test]
    fn row_window_moves_one_row_per_line_height() {
        let (f0, r0, _) = LogView::row_window(0, 200, 20);
        assert_eq!((f0, r0), (0, 0));
        let (f1, r1, c1) = LogView::row_window(ROW_LINE_H * 3 + 7, 200, 20);
        assert_eq!((f1, r1), (3, 7));
        assert_eq!(c1, 21, "多画一行给滑进来的那行");
        // Near the end the count stops overshooting the list.
        let (f2, _, c2) = LogView::row_window(ROW_LINE_H * 18, 20, 20);
        assert_eq!((f2, c2), (18, 2));
    }

    /// Leaving the bottom freezes the watermark where the operator was
    /// reading; whatever arrives after that is unread. Reaching the bottom
    /// re-arms follow and clears the backlog — one rule inside `scroll_to`,
    /// so no input path can forget either half.
    #[test]
    fn unread_counts_arrivals_since_leaving_the_bottom() {
        let rows = 20usize;
        let mut v = LogView {
            entries: (0..100).map(|i| entry(i, LogLevel::Info, "line")).collect(),
            ..LogView::default()
        };
        v.scroll_px = max_scroll(100, rows);
        assert!(v.follow);
        // Scroll up: follow clears, the watermark sits at the newest line.
        assert!(v.scroll_to(v.scroll_px - 25, 100, rows));
        assert!(!v.follow);
        assert_eq!(v.seen_at_bottom, 100);
        assert_eq!(v.unread(&v.visible()), 0, "刚离开底部时还没有新行");
        // Twelve lines arrive while history is being read.
        for i in 100..112 {
            v.entries.push(entry(i, LogLevel::Info, "new"));
        }
        assert_eq!(v.unread(&v.visible()), 12);
        // Dragging within the view does not re-mark anything.
        v.scroll_to(v.scroll_px - 10, 112, rows);
        assert_eq!(v.unread(&v.visible()), 12);
        // Back to the bottom: follow re-arms, backlog clears.
        let max = max_scroll(112, rows);
        assert!(v.scroll_to(max, 112, rows));
        assert!(v.follow);
        assert_eq!(v.unread(&v.visible()), 0);
        assert_eq!(v.seen_at_bottom, 112);
    }

    /// Unread means "lines the current filter would show". Lines that the
    /// level filter hides are not waiting for anybody.
    #[test]
    fn unread_respects_the_filter() {
        let mut v = LogView {
            entries: (0..50).map(|i| entry(i, LogLevel::Info, "old")).collect(),
            min_level: LogLevel::Info,
            ..LogView::default()
        };
        v.follow = false;
        v.seen_at_bottom = 50;
        v.entries.push(entry(50, LogLevel::Warn, "boom"));
        v.entries.push(entry(51, LogLevel::Debug, "noise"));
        v.entries.push(entry(52, LogLevel::Debug, "more noise"));
        assert_eq!(v.unread(&v.visible()), 1, "只显示会被看到的未读");
    }

    /// The in-memory tail cap and the watermark move together: truncating
    /// old lines shifts every surviving index, and an unread count that
    /// silently grew would start blaming lines nobody ever skipped.
    #[test]
    fn adopt_entries_shifts_watermark_through_truncation() {
        let mut v = LogView {
            follow: false,
            ..LogView::default()
        };
        let mut entries: Vec<LogEntry> = (0..(TAIL_CAP as u64 + 30))
            .map(|i| entry(i, LogLevel::Info, "x"))
            .collect();
        v.seen_at_bottom = 35; // 水位在旧尾端之前 35 行处
        entries.truncate(TAIL_CAP + 10);
        v.adopt_entries(entries.clone());
        assert_eq!(v.entries.len(), TAIL_CAP);
        assert_eq!(v.seen_at_bottom, 35 - 10, "水位跟着被剪掉的行号前移");
        assert_eq!(v.unread(&v.visible()), v.entries.len() - v.seen_at_bottom);
        // Under follow the watermark just tracks the end.
        v.follow = true;
        v.adopt_entries(entries);
        assert_eq!(v.seen_at_bottom, v.entries.len());
    }
}
