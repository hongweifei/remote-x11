//! Page chrome: the shared skeleton every page is built from.
//!
//! Position in the layering (see `src/lib.rs`): `ui`, one step above
//! [`crate::theme`] and one below the pages. These five helpers are the
//! skeleton a page assembles itself from — heading block, card, card title,
//! statistic, empty state — so no page invents its own padding, its own idea
//! of a card, or its own empty-state voice. They take geometry as arguments
//! and keep no state; a page that needs more than this should draw it itself.
//!
//! Re-exported from [`crate::ui`] so the historical call paths
//! (`crate::ui::{card, page_header, …}`) keep working.

use fltk::enums::Align;

use crate::theme::{self as th, C, L, T};

/// Draw the page heading block: title, one-line purpose, and a hairline that
/// closes the header. Returns the y coordinate where content starts.
pub fn page_header(x: i32, y: i32, w: i32, title: &str, subtitle: &str) -> i32 {
    th::text(
        title,
        x,
        y,
        w,
        34,
        th::ui_bold(),
        T::DISPLAY,
        C::TEXT,
        Align::Left,
    );
    th::small(subtitle, x, y + 36, w - 20, 20, C::TEXT_2);
    let rule_y = y + 64;
    th::divider(x, rule_y, w);
    rule_y + 20
}

/// A card: raised surface, hairline border, internal padding. Cards group
/// one subject; they are not used to decorate every block. Returns the inner
/// content rect `(x, y, w)` and the card's bottom edge.
pub fn card(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32) {
    th::fill_round(x, y, w, h, L::RADIUS, C::PANEL);
    th::stroke_round(x, y, w, h, L::RADIUS, C::LINE);
    (x + 18, y + 16, w - 36)
}

/// A section title inside a card, with an optional trailing note.
pub fn card_title(x: i32, y: i32, w: i32, title: &str, note: Option<&str>) {
    th::text(
        title,
        x,
        y,
        w,
        20,
        th::ui_bold(),
        T::H2,
        C::TEXT,
        Align::Left,
    );
    if let Some(n) = note {
        th::text(
            n,
            x,
            y,
            w,
            20,
            th::ui_font(),
            T::MICRO,
            C::TEXT_3,
            Align::Right,
        );
    }
}

/// A statistic: caption above, big figure below, unit suffix inline. The
/// figure is monospaced so a row of statistics keeps its columns.
pub fn stat(x: i32, y: i32, w: i32, caption: &str, value: &str, unit: &str, color: u32) {
    th::micro(caption, x, y, w, 14, C::TEXT_3);
    let vw = th::measure(value, th::mono_font(), T::STAT);
    th::text(
        value,
        x,
        y + 16,
        w,
        38,
        th::mono_font(),
        T::STAT,
        color,
        Align::Left,
    );
    if !unit.is_empty() {
        th::micro(unit, x + vw + 4, y + 32, w - vw - 4, 14, C::TEXT_3);
    }
}

/// Empty state: a short line plus one actionable hint. Deliberately quiet —
/// an empty console should look like an instrument with nothing on the wire,
/// not like a broken page.
pub fn empty_state(x: i32, y: i32, w: i32, h: i32, title: &str, hint: &str) {
    let cy = y + h / 2 - 16;
    th::text(
        title,
        x,
        cy,
        w,
        20,
        th::ui_font(),
        T::BODY,
        C::TEXT_2,
        Align::Center,
    );
    th::text(
        hint,
        x,
        cy + 22,
        w,
        18,
        th::ui_font(),
        T::SMALL,
        C::TEXT_3,
        Align::Center,
    );
}
