//! Scrolling machinery for the field column.
//!
//! Split out of `form/mod.rs` so that file stays about layout, painting and the
//! widget tree; this one is only about "which part of the list is visible and
//! how does the operator move it".
//!
//! The rule that matters: the band used to *clip* the canvas and the band used
//! to *hide* widgets must be the same rectangle, or a row's label gets clipped
//! while its input widget still pokes out over the page header. `scroll_band` is
//! that rectangle, `row_fits` is the single predicate both sides ask.

use super::*;

/// Clamp `value`, store it, then relayout and repaint if it moved.
///
/// The single write path for `scroll`: wheel, track click and drag all go
/// through it, so clamping and relayout cannot be forgotten in one of them.
pub(super) fn apply_scroll(
    spec: &'static FormSpec,
    widgets: &WidgetList,
    st: &Rc<RefCell<FormState>>,
    rect: &Rc<RefCell<(i32, i32, i32, i32)>>,
    page: &Page,
    value: i32,
) {
    let changed = {
        let mut s = st.borrow_mut();
        let max = (s.content_h - s.view_h).max(0);
        let next = value.clamp(0, max);
        if next == s.scroll {
            false
        } else {
            s.scroll = next;
            true
        }
    };
    if changed {
        place_fields(spec, widgets, st, rect);
        page.redraw();
    }
}

/// Reposition the field widgets for the current scroll offset.
///
/// A free function rather than a method so the wheel handler (which cannot
/// borrow `self`) can call the same code.
pub(super) fn place_fields(
    spec: &'static FormSpec,
    widgets: &WidgetList,
    st: &Rc<RefCell<FormState>>,
    rect: &Rc<RefCell<(i32, i32, i32, i32)>>,
) {
    let (x0, y0, ww, hh) = *rect.borrow();
    let scroll = st.borrow().scroll;
    let (_, rows) = geometry(spec, x0, y0, ww, hh, scroll);
    let input_x = x0 + L::PAD + 18 + LABEL_COL;
    let input_w = (ww - L::PAD * 2 - SIDE_W - L::GAP) - 36 - LABEL_COL;
    // Same band paint uses: only a row whose text *and* input fit is shown.
    let (_, band_top, _, band_h) = scroll_band(x0, y0, ww, hh);
    let band_bottom = band_top + band_h;
    for (key, widget) in widgets.borrow().iter() {
        let Some(geom) = rows.iter().find(|r| r.key == *key) else {
            continue;
        };
        // Widgets outside the viewport are hidden rather than moved, so
        // FLTK never draws them over the header or the side column.
        let on_screen = row_fits(geom, band_top, band_bottom);
        match widget {
            FieldWidget::Text(f) => {
                f.resize(input_x, geom.input_y, input_w, geom.input_h);
                set_widget_visible(f.input(), on_screen);
            }
            FieldWidget::Toggle(sw) => {
                sw.resize(input_x, geom.input_y + 2);
                set_widget_visible(sw.widget(), on_screen);
            }
            FieldWidget::Select(sel) => {
                sel.resize(input_x, geom.input_y, input_w.min(320), geom.input_h);
                set_widget_visible(sel.widget(), on_screen);
            }
        }
    }
}

pub(super) fn set_widget_visible<W: WidgetExt>(mut w: W, on: bool) {
    if on {
        w.show();
        // A widget that was hidden (scrolled out of view, or the page was not
        // on screen yet) carries no damage when it comes back, so FLTK skips
        // it during a partial repaint and its contents stay invisible.
        w.redraw();
    } else {
        w.hide();
    }
}

/// The band of the page that scrolls, in absolute coordinates.
///
/// Paint clips to it and `place_fields` only shows a widget whose whole row
/// fits inside it — the two must agree, or a row's text is clipped away while
/// its input widget still pokes out over the page header.
pub(super) fn scroll_band(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    let pad = L::PAD;
    let top = y + pad + 84;
    let bottom = y + h - pad;
    let left = x + pad;
    let width = (w - pad * 2 - SIDE_W - L::GAP + 16).max(40);
    (left, top, width, bottom - top)
}

/// Whether a whole row (input through hint) sits inside the scroll band.
pub(super) fn row_fits(g: &RowGeom, top: i32, bottom: i32) -> bool {
    g.input_y >= top && g.hint_y + 16 <= bottom
}

/// Lay out the scroll bar for the field column.
///
/// The one place the bar's geometry is decided: painting uses it, and so does
/// the pointer handler, so the thing you see and the thing you can grab are the
/// same rectangle.
pub(super) fn scroll_bar(rect: Rect, s: &FormState) -> Option<ScrollBar> {
    let (_, top, _, height) = scroll_band(rect.x, rect.y, rect.w, rect.h);
    let pad = L::PAD;
    let content_x = rect.x + pad;
    let content_w = rect.w - pad * 2;
    let main_w = content_w - SIDE_W - L::GAP;
    ScrollBar::new(
        content_x + main_w + 5,
        top,
        height,
        s.content_h,
        height,
        s.scroll,
    )
}
