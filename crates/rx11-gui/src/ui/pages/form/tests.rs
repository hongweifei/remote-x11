//! Geometry tests for the form engine's layout tree.
//!
//! Split out of `form/mod.rs` so the engine file stays under the 1000-line rule,
//! the same split `console/` and `app/` used.
//!
//! These pin the numbers the hand-written version produced: a section is as tall
//! as its head plus its rows plus the card's bottom air, rows are exactly
//! `ROW_H` apart, scrolling is a pure translation, and the scrollable content
//! height includes the gap that follows the last section.

use super::*;

static FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "server.listen",
        label: "监听",
        hint: "IP:PORT",
        kind: FieldKind::Text,
    },
    FieldSpec {
        key: "server.x11_port",
        label: "X11 端口",
        hint: "默认 6000",
        kind: FieldKind::Number,
    },
];
static SECTIONS: &[SectionSpec] = &[SectionSpec {
    title: "服务端",
    note: "默认",
    fields: FIELDS,
}];
static SPEC: FormSpec = FormSpec {
    title: "服务端",
    subtitle: "测试用",
    sections: SECTIONS,
    role: Some(FormRole::Server),
    start_label: "启动中继",
};

#[test]
fn geometry_stacks_sections_without_gaps() {
    let (sections, rows) = geometry(&SPEC, 0, 0, 1000, 700, 0);
    assert_eq!(sections.len(), 1);
    assert_eq!(rows.len(), 2);
    // Second row is exactly one row height below the first.
    assert_eq!(rows[1].row_y - rows[0].row_y, ROW_H);
    // Section is tall enough for its head plus every row.
    assert_eq!(sections[0].h, SECTION_HEAD + 2 * ROW_H + 12);
}

#[test]
fn geometry_offsets_follow_scroll() {
    let (_, a) = geometry(&SPEC, 0, 0, 1000, 700, 0);
    let (_, b) = geometry(&SPEC, 0, 0, 1000, 700, 40);
    assert_eq!(a[0].row_y - b[0].row_y, 40);
}

/// The scrollable content height must equal the shipped sum: every card plus the
/// gap that follows it. The first version of the tree omitted the trailing gap,
/// which silently shrank the scroll range by `SECTION_GAP`.
#[test]
fn content_height_matches_the_shipped_formula() {
    let shipped: i32 = SPEC
        .sections
        .iter()
        .map(|sec| SECTION_HEAD + sec.fields.len() as i32 * ROW_H + 12 + SECTION_GAP)
        .sum();
    let solved = form_layout(&SPEC, 0, 0, 1000, 0);
    assert_eq!(
        solved.content_bottom - page_y_start(0, L::PAD),
        shipped,
        "content height is the sum of the cards plus their gaps"
    );
}

/// Every row and card must sit inside the column the form scrolls, and the rows
/// must be exactly `ROW_H` apart.
#[test]
fn rows_tile_the_section_without_drift() {
    let solved = form_layout(&SPEC, 0, 0, 1000, 0);
    let card = solved.solved.get(solved.cards[0]);
    let tops: Vec<i32> = solved
        .rows
        .iter()
        .map(|&(_, _, id)| solved.solved.get(id).y)
        .collect();
    assert_eq!(tops[1] - tops[0], ROW_H);
    assert_eq!(tops[0], card.y + SECTION_HEAD, "rows start under the head");
    assert_eq!(
        solved.solved.get(solved.rows.last().unwrap().2).bottom(),
        card.bottom() - 12,
        "the last row leaves the card's bottom air"
    );
}

/// The side column is a sibling of the field column, so it has its own rect:
/// solving with `scroll = 0` gives the page-stable geometry the buttons and the
/// validation card both align to. (`Solved::shift` moves every rectangle, which
/// is why those callers pass zero for the side column.)
#[test]
fn side_column_keeps_its_own_rect() {
    let side = form_layout(&SPEC, 0, 0, 1000, 0).side_rect();
    assert_eq!(side.w, SIDE_W);
    assert_eq!(
        side.x,
        1000 - L::PAD - SIDE_W,
        "the side column sits at the content's right edge"
    );
    assert_eq!(
        side.y,
        page_y_start(0, L::PAD),
        "its top is the content origin, shared with the field column"
    );
    assert_eq!(
        form_layout(&SPEC, 0, 0, 1000, 500).side_rect().x,
        side.x,
        "and its x does not depend on the scroll offset"
    );
}
