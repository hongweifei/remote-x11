//! Scrolling for the session table.
//!
//! Split out of `console/mod.rs` when the 1000-line rule fired — the same move
//! `form/scroll.rs` made. This file owns "which part of the row list is
//! visible and how does the operator move it": the pixel offset, the single
//! write path that clamps it, the re-hit-testing after a move, and the
//! scrollbar's geometry. The painter stays on the page and reads the same
//! `row_window` / `compute_row_hits`, so nothing about the visible window has
//! a second source.
//!
//! The rule that matters: the clip band the painter uses, the 「停止」 hit
//! rule, and the bar's viewport all measure the same `fit` rows from the top
//! of the row area. A half-scrolled row paints (clipped) so a drag reads
//! continuously, but never carries a live button.

use super::*;
use crate::ui::scrollbar::{list_max_scroll, list_row_window, ScrollBar};
use crate::ui::PageInput;

/// One row's hit geometry. Recomputed whenever the visible window moves, so
/// drawing and hit testing can never disagree about where a control is.
#[derive(Clone, Copy)]
pub(super) struct RowHit {
    pub(super) session_id: u64,
    /// The 「停止」 button, but only when the whole row is inside the visible
    /// band. A row half-scrolled past an edge is still painted (clipped) so a
    /// mid-drag frame is continuous, yet its button is neither drawn nor
    /// clickable — you cannot stop a session by clicking pixels that were cut
    /// away.
    pub(super) stop: Option<(i32, i32, i32, i32)>,
}

/// Scrollable range in pixels for `total` sessions in a window of `fit` rows.
/// Delegates to the shared [`list_max_scroll`] with the table's row height —
/// same arithmetic as the log well, so both lists hide and show the bar under
/// exactly the same condition.
pub(super) fn table_max_scroll(total: usize, fit: usize) -> i32 {
    list_max_scroll(total, fit, Metrics::ROW_H)
}

/// The pixel offset → the row window: first row slot, how far it has scrolled
/// past the top of the area, and how many slots to draw (one extra so the row
/// sliding in at the bottom is already on screen mid-drag). The shared
/// [`list_row_window`], parameterised by the table's row height.
pub(super) fn row_window(scroll_px: i32, total: usize, fit: usize) -> (usize, i32, usize) {
    list_row_window(scroll_px, total, fit, Metrics::ROW_H)
}

/// Clamp a pixel offset into the table's range and store it.
///
/// The single write path for `scroll_px`: wheel, track click and thumb drag all
/// go through it, so clamping cannot be forgotten in one of them (a scroll past
/// the end used to blank the table until the next poll).
pub(super) fn apply_table_scroll(s: &mut ConsoleState, value: i32, fit: usize) -> bool {
    let max = table_max_scroll(s.data.sessions.len(), fit);
    let next = value.clamp(0, max);
    let changed = next != s.scroll_px;
    s.scroll_px = next;
    changed
}

/// The drawn row slots with their 「停止」 hit rectangles, from one solve.
///
/// Painting and hit testing consume the same `Vec`, so they cannot disagree
/// about which rows exist. A button is included only when its whole 24 units
/// sit inside the row band — a half-scrolled row shows clipped text (so the
/// drag reads continuously) but no clickable control.
pub(super) fn compute_row_hits(
    geometry: &ConsoleLayout,
    solved: &Solved,
    sessions: &[SessionRow],
    scroll_px: i32,
) -> Vec<RowHit> {
    let area = geometry.row_area(solved);
    let fit = list_rows(area);
    let scroll = scroll_px.clamp(0, table_max_scroll(sessions.len(), fit));
    let (first, frac, count) = row_window(scroll, sessions.len(), fit);
    // The band that shows rows: `fit` whole rows from the top of the area.
    // Whatever slack the solved height carries below it is card padding, not
    // viewport — clipping and hit testing stop at the band, so a row at rest
    // never leaks into the padding and a half-scrolled button is never live.
    let band_bottom = area.y + fit as i32 * Metrics::ROW_H;
    let mut out = Vec::with_capacity(count);
    for slot in 0..count {
        let ry = area.y + slot as i32 * Metrics::ROW_H - frac;
        let action = geometry.column_rects(solved, slot)[4];
        let rect = (
            action.right() - SESSION_ACTION_W,
            ry + (Metrics::ROW_H - 24) / 2,
            SESSION_ACTION_W,
            24,
        );
        let whole = rect.1 >= area.y && rect.1 + rect.3 <= band_bottom;
        out.push(RowHit {
            session_id: sessions[first + slot].id,
            stop: whole.then_some(rect),
        });
    }
    out
}

/// Re-run hit testing after a scroll, from the same solve the pointer handler
/// used. Kept free so the wheel/drag closure — which cannot borrow `self` —
/// takes the same path as `layout`.
pub(super) fn reflow_rows(st: &RefCell<ConsoleState>, geometry: &ConsoleLayout, solved: &Solved) {
    let (list, scroll_px) = {
        let s = st.borrow();
        (s.data.sessions.clone(), s.scroll_px)
    };
    let hits = compute_row_hits(geometry, solved, &list, scroll_px);
    st.borrow_mut().rows = hits;
}

/// Move the offset: clamp and store, re-run hit testing, report the move.
///
/// The one offset write the pointer path and the keyboard path share. It
/// repaints nothing itself — the kit's `Grip` owns when a repaint is due
/// (every real move, every drag frame), and the wheel closure repaints on
/// its own change, so exactly one `group.redraw()` lands per frame that
/// actually differs.
fn move_offset(
    st: &RefCell<ConsoleState>,
    geometry: &ConsoleLayout,
    solved: &Solved,
    fit: usize,
    value: i32,
) -> bool {
    let changed = {
        let mut s = st.borrow_mut();
        apply_table_scroll(&mut s, value, fit)
    };
    if changed {
        reflow_rows(st, geometry, solved);
    }
    changed
}

/// Scroll bar for the session table, or `None` when every row fits.
///
/// `area` is the solved row area: the track spans exactly the rows that are
/// drawn, so it cannot leave the card. All numbers are pixels — the drag's
/// inverse `scroll_at` lands in the same space as `scroll_px`.
pub(super) fn session_bar(card: Rect, area: Rect, s: &ConsoleState) -> Option<ScrollBar> {
    let total = s.data.sessions.len() as i32;
    let fit = list_rows(area) as i32;
    // Sit in the card's right padding, not on top of the content: the last
    // column here is the 停止 button, and an inside-the-content bar lands on it.
    // The track spans exactly the drawn band — `fit` rows, not the solved
    // area's slack — so the clip, the button rule, the bar and the viewport
    // step are one rectangle (the third layout baseline, DESIGN §3).
    ScrollBar::new(
        card.right() - 11,
        area.y,
        (fit * Metrics::ROW_H).max(20),
        total * Metrics::ROW_H,
        fit * Metrics::ROW_H,
        s.scroll_px
            .clamp(0, table_max_scroll(total as usize, fit as usize)),
    )
}

impl ConsolePage {
    /// Wheel, scrollbar and keyboard input for the session table.
    ///
    /// The table is the only scrolling surface on this page, so the wheel works
    /// anywhere on it; the bar sits at the table's right edge and is draggable;
    /// with the page surface focused, arrows/Page/Home/End scroll it too. All
    /// of it is the kit's [`Grip`] — the anchor model, the per-frame knob, the
    /// release cooldown — over `move_offset`, the one write that clamps the
    /// pixel offset and re-runs hit testing from the same solve the pointer
    /// handler (and the painter) used. So a half-scrolled row never carries a
    /// live 「停止」 button, and the thumb cools on release without waiting
    /// for the next poll.
    pub(super) fn install_input(&self) {
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
            let mut grip = st.borrow().grip;
            let mut bar = || session_bar(card, area, &st.borrow());
            let mut scroll_to = |v: i32| move_offset(&st, &geometry, &solved, fit, v);
            let mut scroll_by = |d: i32| {
                let cur = st.borrow().scroll_px;
                move_offset(&st, &geometry, &solved, fit, cur + d)
            };
            let mut wheel = |dy: i32| {
                let cur = st.borrow().scroll_px;
                // One row per notch — the same rhythm the row-index wheel
                // had. Pixels are for the thumb, not for the wheel.
                let step = if dy > 0 {
                    Metrics::ROW_H
                } else {
                    -Metrics::ROW_H
                };
                let changed = move_offset(&st, &geometry, &solved, fit, cur + step);
                if changed {
                    page.redraw();
                }
                changed
            };
            let mut repaint = || page.redraw();
            let consumed = match input {
                PageInput::Key(key) => grip.on_key(
                    key,
                    Metrics::ROW_H,
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
}
