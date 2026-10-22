//! One scrollbar, shared by every scrolling surface.
//!
//! Three lists scroll in this console — the field column on the config pages,
//! the session table on the overview and the log stream. They have the same
//! problems (thumb length, page jumps, drag arithmetic) and they used to have
//! none of the same code, so this module owns all of it: geometry, painting and
//! hit testing come from one place, and a page only decides *what* scrolling
//! means for its own data.
//!
//! The bar is drawn only when there is something to scroll, so short pages stay
//! clean. A wheel is an invisible interaction: without a visible bar nobody
//! discovers that a list continues below the fold.

use crate::theme::{self as th, C};

/// Thinnest the thumb is allowed to get, so it still reads as a grab handle
/// when the content is many screens long.
const MIN_THUMB: i32 = 24;
/// Bar width. Wide enough to hit with the pointer, narrow enough not to compete
/// with the card border beside it.
pub const BAR_W: i32 = 4;

/// Where the pointer landed on a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Pressed on the thumb; `grab` is how far below its top edge.
    Thumb { grab: i32 },
    /// Pressed on the track above (negative) or below (positive) the thumb.
    Track { dir: i32 },
}

/// A scrollbar for one list.
#[derive(Debug, Clone, Copy)]
pub struct ScrollBar {
    x: i32,
    top: i32,
    height: i32,
    thumb_top: i32,
    thumb_h: i32,
    max_scroll: i32,
    span: i32,
}

impl ScrollBar {
    /// Lay out the bar for a list at `(x, top, height)`.
    ///
    /// Returns `None` when the content fits: nothing to draw, nothing to drag.
    pub fn new(
        x: i32,
        top: i32,
        height: i32,
        content: i32,
        view: i32,
        scroll: i32,
    ) -> Option<Self> {
        let max_scroll = content - view;
        if max_scroll <= 0 || height <= 0 || content <= 0 {
            return None;
        }
        let thumb_h = ((height as i64 * view as i64) / content.max(1) as i64)
            .max(MIN_THUMB as i64)
            .min(height as i64) as i32;
        let span = height - thumb_h;
        let thumb_top = top + (span as i64 * scroll as i64 / max_scroll as i64) as i32;
        Some(Self {
            x,
            top,
            height,
            thumb_top,
            thumb_h,
            max_scroll,
            span,
        })
    }

    /// Draw track and thumb. `hot` brightens the thumb while it is being
    /// dragged or hovered.
    pub fn paint(&self, hot: bool) {
        th::fill_round(self.x, self.top, BAR_W, self.height, BAR_W / 2, C::RAISED);
        let thumb = if hot { C::TEXT_2 } else { C::TEXT_3 };
        th::fill_round(
            self.x,
            self.thumb_top,
            BAR_W,
            self.thumb_h,
            BAR_W / 2,
            thumb,
        );
    }

    /// What is under the pointer, if it is on the bar at all.
    pub fn hit(&self, px: i32, py: i32) -> Option<Hit> {
        if px < self.x || px > self.x + BAR_W + 4 {
            return None;
        }
        if py < self.top || py > self.top + self.height {
            return None;
        }
        if py >= self.thumb_top && py < self.thumb_top + self.thumb_h {
            Some(Hit::Thumb {
                grab: py - self.thumb_top,
            })
        } else if py < self.thumb_top {
            Some(Hit::Track { dir: -1 })
        } else {
            Some(Hit::Track { dir: 1 })
        }
    }

    /// Scroll offset that puts the thumb's top edge at `thumb_top`, used while
    /// dragging. Clamped to the scrollable range.
    pub fn scroll_at(&self, thumb_top: i32) -> i32 {
        if self.span <= 0 {
            return 0;
        }
        let rel = (thumb_top - self.top).clamp(0, self.span);
        ((rel as i64 * self.max_scroll as i64) / self.span as i64) as i32
    }

    /// Current thumb top, so a drag can be re-anchored after a scroll.
    pub fn thumb_top(&self) -> i32 {
        self.thumb_top
    }

    pub fn max_scroll(&self) -> i32 {
        self.max_scroll
    }
}
