//! UI primitives and pages.
//!
//! Layout is deliberately manual: every region computes its own rectangles
//! from the constants in [`crate::theme::L`] and paints onto one background
//! canvas per page. FLTK's stock container widgets would each impose their
//! own border and padding, and fighting them costs more than the arithmetic.
//! Interactive elements (text fields, drop-downs, buttons) are real widgets
//! positioned on that canvas.
//!
//! # Parts
//!
//! * [`action`] — the intent channel every control reports through;
//! * [`chrome`] — the page skeleton (heading, card, statistic, empty state);
//! * [`controls`] — the hand-drawn widgets;
//! * [`scope`] — the link oscilloscope;
//! * [`shell`] — the fixed frame: rail, top bar, status bar;
//! * [`pages`] — one module per console page.
//!
//! [`Page`] itself stays here: it is the surface every page is created on, so
//! it belongs to the layer rather than to any one page. All of it depends only
//! downwards on [`crate::theme`] and [`crate::state`] — never on `app`,
//! `backend` or `demo`.

pub mod action;
pub mod chrome;
pub mod controls;
pub mod pages;
pub mod scope;
pub mod scrollbar;
pub mod shell;

use fltk::prelude::*;
use fltk::widget::Widget;

use crate::theme as th;

pub use chrome::{card, card_title, empty_state, page_header, stat};

/// Pointer input a scrollable page receives from its background widget.
#[derive(Debug, Clone, Copy)]
pub enum PageInput {
    /// Wheel delta; positive scrolls down.
    Wheel(i32),
    /// Button pressed at page-relative `(x, y)` — add the page origin to get
    /// the same coordinate space the painters use.
    Push(i32, i32),
    /// Pointer moved with the button held, page-relative.
    Drag(i32, i32),
    /// Button released.
    Release,
}

/// A page surface: an FLTK group whose first child paints the background.
///
/// Two FLTK facts shape this design, both checked against the bundled source
/// and a scratch probe:
///
/// 1. a group draws its children **in creation order**, so the background
///    widget is created first and the controls paint over it;
/// 2. repainting one child does not redraw its siblings, so a background
///    repaint on its own would erase the controls. [`Page::redraw`] therefore
///    repaints the whole group, and the app repaints the visible page on every
///    poll tick — at two frames a second the cost is irrelevant and it removes
///    an entire class of "widget vanished" bugs.
#[derive(Clone)]
pub struct Page {
    group: fltk::group::Group,
    bg: Widget,
}

impl Page {
    /// Create the surface and its background painter. Controls created after
    /// this call, while the group is open, sit on top of the background; call
    /// [`Page::finish`] once they are all in place.
    pub fn new<F>(x: i32, y: i32, w: i32, h: i32, bg_color: u32, f: F) -> Self
    where
        F: FnMut(i32, i32, i32, i32) + 'static,
    {
        let mut group = fltk::group::Group::new(x, y, w, h, None);
        group.set_frame(fltk::enums::FrameType::NoBox);
        group.begin();

        let mut bg = Widget::new(x, y, w, h, None);
        bg.set_frame(fltk::enums::FrameType::NoBox);
        bg.set_color(th::rgb(bg_color));
        let mut f = f;
        bg.draw(move |w| f(w.x(), w.y(), w.w(), w.h()));

        Self { group, bg }
    }

    /// Close the group: later widgets belong to the window again.
    pub fn finish(&self) {
        let g = self.group.clone();
        g.end();
    }

    /// Install the pointer input handler for a scrollable page.
    ///
    /// The background widget covers the whole page and is the bottom-most
    /// child, so it receives input that no control above it consumed. Only one
    /// handler can live on a widget, so wheel, drag and release are delivered
    /// through one closure — see [`PageInput`].
    pub fn on_input<F>(&self, f: F)
    where
        F: Fn(PageInput) -> bool + 'static,
    {
        let mut bg = self.bg.clone();
        bg.handle(move |w, ev| {
            let (bx, by) = (w.x(), w.y());
            let at = || (fltk::app::event_x() - bx, fltk::app::event_y() - by);
            match ev {
                fltk::enums::Event::MouseWheel => f(PageInput::Wheel(fltk::app::event_dy_value())),
                fltk::enums::Event::Push => {
                    let (x, y) = at();
                    f(PageInput::Push(x, y))
                }
                fltk::enums::Event::Drag => {
                    let (x, y) = at();
                    f(PageInput::Drag(x, y))
                }
                fltk::enums::Event::Released => f(PageInput::Release),
                _ => false,
            }
        });
    }

    pub fn widget(&self) -> fltk::group::Group {
        self.group.clone()
    }

    /// Repaint the page: background and every control, in one pass.
    pub fn redraw(&self) {
        let mut g = self.group.clone();
        g.redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        let mut g = self.group.clone();
        g.resize(x, y, w, h);
        let mut bg = self.bg.clone();
        bg.resize(x, y, w, h);
    }
}
