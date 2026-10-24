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
    ///
    /// `drag_top` replaces the computed thumb position while the operator
    /// holds the knob: the knob is drawn under the pointer, and the content
    /// offset is derived from that position by the inverse map. Deriving the
    /// knob from the offset instead composes two truncating divisions — on
    /// the log well that measured as a period-5 staircase (1,1,0,1,2 device
    /// px per pointer px), every fifth pointer pixel leaving the knob frozen.
    /// A knob that never lags the cursor *is* what 跟手 means; the offset may
    /// quantise, the knob may not.
    pub fn paint(&self, hot: bool, drag_top: Option<i32>) {
        th::fill_round(self.x, self.top, BAR_W, self.height, BAR_W / 2, C::RAISED);
        let thumb = if hot { C::TEXT_2 } else { C::TEXT_3 };
        let top = drag_top
            .unwrap_or(self.thumb_top)
            .clamp(self.top, self.top + self.span);
        th::fill_round(self.x, top, BAR_W, self.thumb_h, BAR_W / 2, thumb);
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

    /// Current thumb top, so a grab can anchor the dragged knob there —
    /// pressing must not move it.
    pub fn thumb_top(&self) -> i32 {
        self.thumb_top
    }

    /// The closed range the knob's top edge may cover. While dragging, the
    /// handler sets the knob to `pointer - grab` clamped into this range, and
    /// the offset to `scroll_at` of the same value — one number, two readers.
    pub fn knob_top_range(&self) -> (i32, i32) {
        (self.top, self.top + self.span)
    }

    pub fn max_scroll(&self) -> i32 {
        self.max_scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log well at shipping geometry: 400 lines of 20 logical px, a 24-row
    /// viewport — the exact shape the 2026-10-23 step-sweep measured on the
    /// old row-quantised build.
    fn bar() -> ScrollBar {
        ScrollBar::new(1400, 316, 478, 400 * 20, 24 * 20, 0).unwrap()
    }

    /// The algebra behind "跟手": the drag path is `scroll_at(pointer)` and the
    /// painted thumb is `new().thumb_top`. With a row index in between, the
    /// composition had plateaus — pointer pixels that moved the offset not at
    /// all (measured: 16 of 40 one-pixel steps). Without it, every pointer
    /// pixel inside the track must move the offset, and the ends must land
    /// exactly on 0 and `max_scroll`.
    #[test]
    fn pointer_to_offset_has_no_dead_steps() {
        let b = bar();
        let mut prev = b.scroll_at(b.top);
        assert_eq!(prev, 0, "the very top is the very start");
        assert_eq!(
            b.scroll_at(b.top + b.span),
            b.max_scroll,
            "the very bottom is the very end"
        );
        for p in (b.top + 1)..=(b.top + b.span) {
            let v = b.scroll_at(p);
            assert!(
                v > prev,
                "pointer pixel {p} moved the offset not at all (dead step)"
            );
            // At most one extra offset unit per pixel: the map is a smooth
            // line, never a lurch.
            assert!(v - prev <= (b.max_scroll / b.span) + 2);
            prev = v;
        }
    }

    /// Grab, move, release: recomputing the bar from the stored offset must
    /// put the thumb back under the pointer where it was dragged to — one
    /// pixel of integer rounding is the whole tolerance. The knob itself is
    /// *drawn* at the pointer while held (the pages pass `drag_top`), so this
    /// only pins how far the settled position can differ from the cursor.
    #[test]
    fn drag_round_trip_returns_the_thumb_under_the_pointer() {
        let b = bar();
        for p in (b.top + 4)..=(b.top + b.span - 4) {
            let offset = b.scroll_at(p);
            let after = ScrollBar::new(1400, 316, 478, 8000, 480, offset).unwrap();
            assert!(
                (after.thumb_top - p).abs() <= 1,
                "pointer {p} → offset {offset} → thumb {}",
                after.thumb_top
            );
        }
    }

    /// The anchor model: pressing anchors at the *visible* knob (no jump),
    /// and while held the knob is `pointer − grab` clamped to
    /// `knob_top_range` — inside the track that is one knob pixel per pointer
    /// pixel, exactly, with no double-floor staircase left to measure. The
    /// offset comes from `scroll_at` of the same value.
    #[test]
    fn dragged_knob_tracks_the_pointer_pixel_for_pixel_between_the_ends() {
        let b = bar();
        let grab = 12; // pressed 12 units below the visible top
        let press = b.thumb_top() + grab;
        assert_eq!(press - grab, b.thumb_top(), "按下不许让滑块跳");
        let (lo, hi) = b.knob_top_range();
        let knob = |d: i32| (press + d - grab).clamp(lo, hi);
        let mut prev = knob(-120);
        for d in -119..=120 {
            let now = knob(d);
            if prev < hi && now > lo {
                assert_eq!(now - prev, 1, "指针 1px = 滑块 1px（第 {d} 步）");
            }
            prev = now;
        }
        // Past either end both the knob and the offset sit clamped.
        assert_eq!(knob(10_000), hi);
        assert_eq!(b.scroll_at(knob(10_000)), b.max_scroll());
        assert_eq!(knob(-10_000), lo);
        assert_eq!(b.scroll_at(knob(-10_000)), 0);
    }

    /// Short content must still not produce a bar, and one pixel of
    /// overflow must.
    #[test]
    fn bar_appears_exactly_when_content_overflows() {
        assert!(ScrollBar::new(0, 0, 100, 200, 200, 0).is_none());
        assert!(ScrollBar::new(0, 0, 100, 201, 200, 0).is_some());
    }
}
