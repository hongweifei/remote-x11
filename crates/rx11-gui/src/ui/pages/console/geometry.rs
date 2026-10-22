//! Geometry tests for the overview page's layout tree.
//!
//! The reference values were measured on the shipped layout before this page
//! moved onto the tree; the tree must reproduce them, so a migration that shifts
//! anything on screen fails here instead of only in a screenshot.

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
