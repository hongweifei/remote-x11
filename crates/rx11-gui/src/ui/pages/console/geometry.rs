//! Geometry tests for the overview page's layout tree.
//!
//! The reference values were measured on the shipped layout before this page
//! moved onto the tree; the tree must reproduce them, so a migration that shifts
//! anything on screen fails here instead of only in a screenshot.

use super::scroll::{apply_table_scroll, compute_row_hits};
use super::*;

/// Page 944x714 logical is what a 1180x800 window leaves the content area after
/// the rail, the top bar and the status bar.
const PAGE: Rect = Rect::new(0, 0, 944, 714);

#[test]
fn card_stack_matches_the_shipped_layout() {
    let geometry = build_layout(2);
    let solved = geometry.solve(PAGE);
    assert_eq!(solved.get(geometry.header), Rect::new(22, 22, 900, 84));
    assert_eq!(solved.get(geometry.summary), Rect::new(22, 106, 900, 76));
    assert_eq!(
        solved.get(geometry.scope_card),
        Rect::new(22, 196, 590, 300)
    );
    assert_eq!(
        solved.get(geometry.start_card),
        Rect::new(626, 196, 296, 178)
    );
    assert_eq!(solved.get(geometry.cap_card), Rect::new(626, 386, 296, 110));
    assert_eq!(solved.get(geometry.sessions), Rect::new(22, 510, 900, 182));
    assert!(!solved.overflow());
}

/// The scope plot fills the scope card's inner area below its title row.
#[test]
fn scope_plot_fills_the_card_inner_area() {
    let geometry = build_layout(2);
    let solved = geometry.solve(PAGE);
    let plot = solved.get(geometry.scope_plot);
    let card = solved.get(geometry.scope_card);
    assert_eq!(plot.x, card.x + SCOPE_PAD.left);
    assert_eq!(plot.w, card.w - SCOPE_PAD.left - SCOPE_PAD.right);
    assert_eq!(plot.y, card.y + SCOPE_PAD.top + CARD_TITLE_H);
    assert_eq!(
        plot.bottom(),
        card.bottom() - SCOPE_PAD.bottom,
        "BAND_H - 54 is what the plot span used to be"
    );
}

/// Two rows fit, and the first row sits where the hand-written version put it:
/// 78 below the card top.
#[test]
fn session_rows_start_below_the_card_chrome() {
    let geometry = build_layout(2);
    let solved = geometry.solve(PAGE);
    let area = geometry.row_area(&solved);
    assert_eq!(area.y, 588, "first row sits 78 below the card top");
    assert_eq!(list_rows(area), 2);
}

/// A short page squeezes the card (`Hug` yields) instead of drawing past the
/// bottom; a tall page still stops at the height the rows need.
#[test]
fn session_card_is_capped_by_the_page() {
    let geometry = build_layout(12);
    let short = geometry.solve(Rect::new(0, 0, 944, 600));
    let card = short.get(geometry.sessions);
    assert!(card.h <= 120, "a short page squeezes the card: {card:?}");
    assert!(card.bottom() <= 600, "the card stays on the page: {card:?}");
    assert!(short.overflow(), "the fixed cards above it did not fit");

    let tall = build_layout(2).solve(Rect::new(0, 0, 944, 1400));
    let geometry = build_layout(2);
    assert_eq!(
        tall.get(geometry.sessions).h,
        SESSION_CHROME_H + 2 * Metrics::ROW_H
    );
}

/// Session rows and their columns come from one solve, so a row never receives a
/// column rect belonging to another row.
#[test]
fn row_columns_track_the_row() {
    let geometry = build_layout(12);
    let solved = geometry.solve(PAGE);
    let first = geometry.column_rects(&solved, 0);
    let second = geometry.column_rects(&solved, 1);
    assert_eq!(second[0].y, first[0].y + Metrics::ROW_H);
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.x, b.x, "columns share a left edge across rows");
        assert_eq!(a.w, b.w, "columns share a width across rows");
    }
    // The five columns tile the row exactly.
    assert_eq!(first[0].x, geometry.row_area(&solved).x);
    assert_eq!(
        first[4].right(),
        geometry.row_area(&solved).right(),
        "the action column closes the row"
    );
}

// ---------------------------------------------------------------------------
// Pixel scrolling: the window, the ends, and the button rule
// ---------------------------------------------------------------------------

fn sessions(n: usize) -> Vec<SessionRow> {
    (0..n)
        .map(|i| SessionRow {
            id: i as u64 + 1,
            role: crate::model::Role::Server,
            health: Health::Running,
            headline: "x".into(),
            detail: String::new(),
            sent: None,
            received: None,
            uptime: None,
        })
        .collect()
}

/// Both detents of the table's scroll range are row-aligned: resting the thumb
/// at top or bottom shows whole rows, never a sliver. Only mid-drag frames are
/// fractional.
#[test]
fn table_scroll_extends_are_row_aligned() {
    for (total, fit) in [(12usize, 2usize), (12, 5), (2, 2), (400, 24)] {
        let max = table_max_scroll(total, fit);
        assert_eq!(max % Metrics::ROW_H, 0, "max {max} 必须落在整行上");
        let (first, frac, count) = row_window(max, total, fit);
        assert_eq!((first, frac), (total.saturating_sub(fit), 0));
        assert!(first + count <= total);
        let (first, frac, _) = row_window(0, total, fit);
        assert_eq!((first, frac), (0, 0));
    }
}

/// A fractional offset slides the window: the first slot is the row being cut
/// by the top edge, and one extra slot is drawn so the row sliding in at the
/// bottom is already on screen mid-drag. The count never runs past the list.
#[test]
fn row_window_tracks_the_fraction() {
    let (first, frac, count) = row_window(46 * 3 + 12, 12, 2);
    assert_eq!((first, frac), (3, 12));
    assert_eq!(count, 3, "两行视口 + 滑进来的第三行");
    // Near the end the window closes without overshooting the list.
    let (first, frac, count) = row_window(table_max_scroll(12, 2), 12, 2);
    assert_eq!((first, frac, count), (10, 0, 2));
    // Nothing to scroll: exactly the fitting rows, aligned.
    let (first, frac, count) = row_window(0, 2, 2);
    assert_eq!((first, frac, count), (0, 0, 2));
}

/// `apply_table_scroll` is the only writer: clamped at both ends, and it only
/// reports "changed" for real moves — the caller repaints on `true` alone
/// (drag frames repaint unconditionally for the knob itself, wheel and track
/// moves repaint on change).
#[test]
fn apply_table_scroll_clamps_and_only_reports_real_moves() {
    let mut s = ConsoleState {
        data: ConsoleData {
            sessions: sessions(12),
            ..ConsoleData::default()
        },
        rows: Vec::new(),
        hover_stop: None,
        pressed_stop: None,
        scroll_px: 0,
        thumb_grab: None,
        drag_top: None,
    };
    assert!(!apply_table_scroll(&mut s, 0, 2), "不动不算变化");
    assert!(apply_table_scroll(&mut s, 40, 2));
    assert_eq!(s.scroll_px, 40);
    let max = table_max_scroll(12, 2);
    assert!(apply_table_scroll(&mut s, max + 9_999, 2));
    assert_eq!(s.scroll_px, max, "越界必须夹到底");
    assert!(
        apply_table_scroll(&mut s, -50, 2),
        "从底部一把推过头也是一次真实移动"
    );
    assert_eq!(s.scroll_px, 0, "负方向同样夹住");
    assert!(
        !apply_table_scroll(&mut s, -1, 2),
        "已经贴着 0 再推不算移动"
    );
}

/// The button rule the pointer handler depends on: a row half-scrolled past an
/// edge still paints (clipped) but carries no 「停止」 hit rect — you cannot
/// stop a session by clicking pixels that were cut away. And the hit rects
/// slide with the rows, from the same solve that paints them.
#[test]
fn half_scrolled_rows_keep_the_row_but_lose_the_button() {
    let geometry = build_layout(12);
    let solved = geometry.solve(PAGE);
    let area = geometry.row_area(&solved);
    let fit = list_rows(area);
    let list = sessions(12);

    // At rest every fully visible row has a live button…
    let hits = compute_row_hits(&geometry, &solved, &list, 0);
    assert_eq!(hits.len(), fit + 1, "静止时多画的那一行整体裁掉");
    let band_bottom = area.y + fit as i32 * Metrics::ROW_H;
    for (slot, h) in hits.iter().enumerate().take(fit) {
        let (_, sy, _, sh) = h
            .stop
            .unwrap_or_else(|| panic!("第 {slot} 行静止时应整行可见"));
        assert!(sy >= area.y && sy + sh <= band_bottom, "按钮在行带内");
        assert_eq!(h.session_id, slot as u64 + 1);
    }
    assert!(hits[fit].stop.is_none(), "视口外的那一行没有命中框");

    // …dragged into a fraction, the top row's button has left the area.
    let frac = 20;
    let hits = compute_row_hits(&geometry, &solved, &list, frac);
    assert_eq!(hits.len(), fit + 1);
    assert!(hits[0].stop.is_none(), "被上边缘切掉的按钮不可点");
    assert_eq!(hits[0].session_id, 1, "行内容还在，只是裁了");
    for h in hits.iter().skip(1).take(fit - 1) {
        assert!(h.stop.is_some(), "中间整行仍然可点");
    }
    if let Some((_, sy, _, sh)) = hits[1].stop {
        assert_eq!(
            sy,
            area.y + Metrics::ROW_H - frac + (Metrics::ROW_H - 24) / 2,
            "命中框随行一起平移"
        );
        assert!(sy >= area.y && sy + sh <= area.y + fit as i32 * Metrics::ROW_H);
    } else {
        panic!("拖动后的第二行仍是整行，按钮必须还在");
    }

    // At the bottom detent the last row is fully visible with its button —
    // nothing dead-ends the table just because the offset moved.
    let max = table_max_scroll(12, fit);
    let hits = compute_row_hits(&geometry, &solved, &list, max);
    assert!(hits.iter().take(fit).all(|h| h.stop.is_some()));
}
