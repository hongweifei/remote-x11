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
use crate::ui::scrollbar::{Hit, ScrollBar};
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
/// Like the log well, both ends are row-aligned: the thumb's bottom detents
/// onto the last whole row.
pub(super) fn table_max_scroll(total: usize, fit: usize) -> i32 {
    (total as i32 - fit as i32).max(0) * Metrics::ROW_H
}

/// The pixel offset → the row window: first row slot, how far it has scrolled
/// past the top of the area, and how many slots to draw (one extra so the row
/// sliding in at the bottom is already on screen mid-drag).
pub(super) fn row_window(scroll_px: i32, total: usize, fit: usize) -> (usize, i32, usize) {
    let first = (scroll_px / Metrics::ROW_H).max(0) as usize;
    let frac = scroll_px.rem_euclid(Metrics::ROW_H);
    let count = (fit + 1).min(total.saturating_sub(first));
    (first, frac, count)
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

/// Move the offset by a wheel/track delta; re-hit-test and repaint on change.
fn scroll_and_repaint(
    st: &RefCell<ConsoleState>,
    page: &Page,
    geometry: &ConsoleLayout,
    solved: &Solved,
    fit: usize,
    delta_px: i32,
) -> bool {
    let cur = st.borrow().scroll_px;
    apply_offset_and_repaint(st, page, geometry, solved, fit, cur + delta_px)
}

/// Store an absolute offset; re-hit-test and repaint on change. The one place
/// a scroll write reaches FLTK: a real move always ends in exactly one
/// `group.redraw()`, so the whole page — canvas and controls — repaints in
/// the same frame the offset moved.
fn apply_offset_and_repaint(
    st: &RefCell<ConsoleState>,
    page: &Page,
    geometry: &ConsoleLayout,
    solved: &Solved,
    fit: usize,
    value: i32,
) -> bool {
    let mut s = st.borrow_mut();
    let changed = apply_table_scroll(&mut s, value, fit);
    drop(s);
    if changed {
        reflow_rows(st, geometry, solved);
        page.redraw();
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
    ScrollBar::new(
        card.right() - 11,
        area.y,
        area.h.max(20),
        total * Metrics::ROW_H,
        fit * Metrics::ROW_H,
        s.scroll_px
            .clamp(0, table_max_scroll(total as usize, fit as usize)),
    )
}

impl ConsolePage {
    /// Wheel and scrollbar input for the session table.
    ///
    /// The table is the only scrolling surface on this page, so the wheel works
    /// anywhere on it; the bar sits at the table's right edge and is draggable.
    /// Every offset write goes through `apply_table_scroll`, the single place
    /// the pixel offset is clamped. After any real move the row hit rectangles
    /// are rebuilt from the same solve and the whole page group is repainted —
    /// so a half-scrolled row never carries a live 「停止」 button, and the
    /// thumb cools on release without waiting for the next poll.
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
            match input {
                PageInput::Wheel(dy) => {
                    if dy == 0 {
                        return false;
                    }
                    // One row per notch — the same rhythm the row-index wheel
                    // had. Pixels are for the thumb, not for the wheel.
                    let step = if dy > 0 {
                        Metrics::ROW_H
                    } else {
                        -Metrics::ROW_H
                    };
                    scroll_and_repaint(&st, &page, &geometry, &solved, fit, step)
                }
                PageInput::Push(lx, ly) => {
                    let Some(bar) = session_bar(card, area, &st.borrow()) else {
                        return false;
                    };
                    match bar.hit(lx + px, ly + py) {
                        Some(Hit::Thumb { grab }) => {
                            // Anchor the knob where it is shown: pressing
                            // must not move it; from there the pointer drags
                            // it and the offset follows.
                            let mut s = st.borrow_mut();
                            s.thumb_grab = Some(grab);
                            s.drag_top = Some(bar.thumb_top());
                            drop(s);
                            page.redraw();
                            true
                        }
                        Some(Hit::Track { dir }) => {
                            // A track click pages by one viewport.
                            scroll_and_repaint(
                                &st,
                                &page,
                                &geometry,
                                &solved,
                                fit,
                                dir * fit as i32 * Metrics::ROW_H,
                            )
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
                    // One number drives both readers: the knob is drawn at
                    // `pointer - grab` (clamped to the track, so it follows
                    // the cursor pixel for pixel), and the content offset is
                    // `scroll_at` of that same position — there is no
                    // division by the row height anywhere in this path.
                    // `ly + py` converts the page-relative pointer to the
                    // bar's absolute space.
                    let (lo, hi) = bar.knob_top_range();
                    let knob = (ly + py - grab).clamp(lo, hi);
                    let target = bar.scroll_at(knob);
                    let changed = {
                        let mut s = st.borrow_mut();
                        s.drag_top = Some(knob);
                        apply_table_scroll(&mut s, target, fit)
                    };
                    if changed {
                        reflow_rows(&st, &geometry, &solved);
                    }
                    // Repaint on every drag frame, not only when the offset
                    // moved: the knob itself is the thing under the cursor.
                    page.redraw();
                    // The drag belongs to the thumb whether or not the offset
                    // moved (at an edge it must not fall through to widgets).
                    true
                }
                PageInput::Release => {
                    let was = {
                        let mut s = st.borrow_mut();
                        s.drag_top = None;
                        s.thumb_grab.take()
                    };
                    if was.is_some() {
                        // The knob settles onto the offset-derived position
                        // and cools from the grabbed colour immediately.
                        page.redraw();
                    }
                    was.is_some()
                }
            }
        });
    }
}
