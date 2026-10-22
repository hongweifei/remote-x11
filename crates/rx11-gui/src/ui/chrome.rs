//! Page chrome: the shared skeleton every page is built from.
//!
//! Position in the layering (see `src/lib.rs`): `ui`, one step above
//! [`crate::theme`] and one below the pages. These helpers are the skeleton a
//! page assembles itself from — heading block, card, card title, statistic,
//! empty state — so no page invents its own padding, its own idea of a card, or
//! its own empty-state voice.
//!
//! The `*_in` helpers take a rectangle solved by [`crate::ui::layout`] and paint
//! into it. A page lists its sections as layout leaves and hands the resulting
//! rectangles here; it never derives one coordinate from another. The card
//! returns the rectangle its content occupies, so a card's children are listed
//! against that rectangle rather than against the card's edges.

use fltk::enums::Align;

use crate::theme::{self as th, C, L, T};
use crate::ui::layout::{Insets, Rect};

/// Inner padding of a card.
pub const CARD_PAD: Insets = Insets::xy(18, 16);

/// Height of a card's title row.
pub const CARD_TITLE_H: i32 = 20;

/// Height of the page heading block: title, purpose line, hairline, and the air
/// below the hairline before content starts.
pub const PAGE_HEADER_H: i32 = 84;

/// Draw the page heading block into `area`.
pub fn page_header_in(area: Rect, title: &str, subtitle: &str) {
    th::text(
        title,
        area.x,
        area.y,
        area.w,
        34,
        th::ui_bold(),
        T::DISPLAY,
        C::TEXT,
        Align::Left,
    );
    th::small(subtitle, area.x, area.y + 36, area.w - 20, 20, C::TEXT_2);
    th::divider(area.x, area.y + 64, area.w);
}

/// Paint a card into `area`; returns the rectangle its content occupies.
pub fn card_in(area: Rect) -> Rect {
    th::fill_round(area.x, area.y, area.w, area.h, L::RADIUS, C::PANEL);
    th::stroke_round(area.x, area.y, area.w, area.h, L::RADIUS, C::LINE);
    area.inset(CARD_PAD)
}

/// A section title inside a card, with an optional trailing note.
pub fn card_title_in(area: Rect, title: &str, note: Option<&str>) {
    th::text(
        title,
        area.x,
        area.y,
        area.w,
        CARD_TITLE_H,
        th::ui_bold(),
        T::H2,
        C::TEXT,
        Align::Left,
    );
    if let Some(n) = note {
        th::text(
            n,
            area.x,
            area.y,
            area.w,
            CARD_TITLE_H,
            th::ui_font(),
            T::MICRO,
            C::TEXT_3,
            Align::Right,
        );
    }
}

/// A statistic: caption above, big figure below, unit suffix inline.
pub fn stat_in(area: Rect, caption: &str, value: &str, unit: &str, color: u32) {
    th::micro(caption, area.x, area.y, area.w, 14, C::TEXT_3);
    let vw = th::measure(value, th::mono_font(), T::STAT);
    th::text(
        value,
        area.x,
        area.y + 16,
        area.w,
        38,
        th::mono_font(),
        T::STAT,
        color,
        Align::Left,
    );
    if !unit.is_empty() {
        th::micro(
            unit,
            area.x + vw + 4,
            area.y + 32,
            area.w - vw - 4,
            14,
            C::TEXT_3,
        );
    }
}

/// Empty state: a short line plus one actionable hint. Deliberately quiet — an
/// empty console should look like an instrument with nothing on the wire, not
/// like a broken page.
pub fn empty_state_in(area: Rect, title: &str, hint: &str) {
    let cy = area.center_y() - 16;
    th::text(
        title,
        area.x,
        cy,
        area.w,
        20,
        th::ui_font(),
        T::BODY,
        C::TEXT_2,
        Align::Center,
    );
    th::text(
        hint,
        area.x,
        cy + 22,
        area.w,
        18,
        th::ui_font(),
        T::SMALL,
        C::TEXT_3,
        Align::Center,
    );
}

// ---------------------------------------------------------------------------
// Tuple-based forms, kept only until the six pages are on the layout tree.
// ---------------------------------------------------------------------------

pub fn page_header(x: i32, y: i32, w: i32, title: &str, subtitle: &str) -> i32 {
    let area = Rect::new(x, y, w, PAGE_HEADER_H);
    page_header_in(area, title, subtitle);
    y + 64 + 20
}

pub fn card(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32) {
    let inner = card_in(Rect::new(x, y, w, h));
    (inner.x, inner.y, inner.w)
}

pub fn card_title(x: i32, y: i32, w: i32, title: &str, note: Option<&str>) {
    card_title_in(Rect::new(x, y, w, CARD_TITLE_H), title, note);
}

pub fn stat(x: i32, y: i32, w: i32, caption: &str, value: &str, unit: &str, color: u32) {
    stat_in(Rect::new(x, y, w, 54), caption, value, unit, color);
}

pub fn empty_state(x: i32, y: i32, w: i32, h: i32, title: &str, hint: &str) {
    empty_state_in(Rect::new(x, y, w, h), title, hint);
}
