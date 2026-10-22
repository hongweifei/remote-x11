//! Drawing primitives: the only code in the crate that calls `fltk::draw`.
//!
//! Position in the layering (see `src/lib.rs`): presentation layer, on top of
//! [`super::tokens`] and [`super::fonts`]. Every primitive takes a hex token
//! and converts it at this boundary, so call sites never hold an FLTK colour
//! and the palette stays in one file.
//!
//! [`text`] is the single entry point for painted text. It computes the
//! baseline itself instead of delegating to FLTK's box form; the reason is
//! documented on the function and is a fix, not a style choice.

use fltk::{draw, enums};

use super::fonts::{mono_font, ui_font};
use super::tokens::{rgb, C, T};

/// Filled rectangle in a palette color.
pub fn fill(x: i32, y: i32, w: i32, h: i32, color: u32) {
    draw::draw_rect_fill(x, y, w, h, rgb(color));
}

/// Filled rounded rectangle (card surface).
pub fn fill_round(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    draw::set_draw_color(rgb(color));
    draw::draw_rounded_rectf(x, y, w, h, r);
}

/// 1px outline of a rounded rectangle.
pub fn stroke_round(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    if w <= 1 || h <= 1 {
        return;
    }
    draw::set_draw_color(rgb(color));
    draw::draw_rounded_rect(x, y, w - 1, h - 1, r);
}

/// Horizontal hairline. One rule does one job: never double a border.
pub fn hline(x: i32, y: i32, w: i32, color: u32) {
    draw::set_draw_color(rgb(color));
    draw::set_line_style(draw::LineStyle::Solid, 0);
    draw::draw_line(x, y, x + w, y);
}

/// Vertical hairline.
pub fn vline(x: i32, y: i32, h: i32, color: u32) {
    draw::set_draw_color(rgb(color));
    draw::set_line_style(draw::LineStyle::Solid, 0);
    draw::draw_line(x, y, x, y + h);
}

/// A 1px rule that spans a whole region and sits on the pixel grid.
pub fn divider(x: i32, y: i32, w: i32) {
    hline(x, y, w, C::LINE);
}

/// Height of a label's ink centre above its baseline, in logical units.
///
/// **Calibrated against rendered frames, not derived.** FLTK's metrics do not
/// predict the drawn baseline under `set_screen_scale` — it shifts scaled text
/// by an internal `-1` (`Fl_Scalable_Graphics_Driver::draw`, "for issue #1308")
/// — and three attempts to model it analytically each produced a different
/// wrong answer. `examples/calibrate.rs` renders real labels beside candidate
/// dots; these are what it measured:
///
/// | label | ink centre above baseline |
/// |---|---|
/// | Latin (`rx11`, digits, addresses): caps/digits sit on the baseline | 0.355 em ≈ half the cap height |
/// | CJK (`中继`, `保存`): glyphs fill the em box and dip below the baseline | 0.115 em |
///
/// The distinction matters: centring the *line box* (ascent+descent) leaves CJK
/// ink 3–7 units low, because a Chinese glyph fills the em square while the line
/// box reserves a descender strip under it. That is the "文字总是向下一些"
/// report, and it is why [`text`] now centres the ink instead.
///
/// Re-run the probe if the type scale or the font family changes.
pub fn ink_above_baseline(text: &str, size: i32) -> i32 {
    let k = if is_cjk(text) { 115 } else { 355 };
    (size * k + 500) / 1000
}

/// Whether a label is written in CJK, which changes where its ink centre sits.
///
/// Covers the CJK unified ideographs and the common CJK punctuation/width
/// blocks. Anything else is treated as Latin for the purposes of
/// [`LineBox::center_y`].
pub fn is_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        let u = c as u32;
        (0x2E80..=0x9FFF).contains(&u)      // radicals, kana, CJK unified
            || (0xF900..=0xFAFF).contains(&u) // compatibility ideographs
            || (0xFF00..=0xFF60).contains(&u) // full-width forms
    })
}

/// A line box: the one thing text and the markers beside it are laid out from.
///
/// This struct exists to kill a whole class of bug. Before it, every label was
/// drawn into a hand-written box (`h = 20`, `26`, `32`, …) while every status dot
/// next to it was placed with its own arithmetic, and the two only agreed by
/// luck. Two rules now hold:
///
/// 1. a label's box is exactly one line tall — [`LineBox::height`] comes from
///    FLTK's own metrics, never from a literal;
/// 2. a marker beside a label is placed with [`LineBox::center_y`], i.e. the
///    same box, so text and marker are two readings of one number.
///
/// One rule ties them together: the box's **optical centre** is `y + h/2`, the
/// label's ink is centred there by [`text`], and a marker is placed there by
/// [`LineBox::center_y`]. Nothing derives a centre from font metrics, because
/// FLTK's metrics do not predict the rendered baseline under a screen scale —
/// see [`ink_above_baseline`].
#[derive(Debug, Clone, Copy)]
pub struct LineBox {
    /// Left edge.
    pub x: i32,
    /// Top edge of the box the label is drawn into.
    pub y: i32,
    /// Width.
    pub w: i32,
    /// Height of that box.
    pub h: i32,
    /// Size the metrics were taken at.
    pub size: i32,
}

impl LineBox {
    /// The box a label of `size` will be drawn into.
    ///
    /// `font` is kept for callers that measure the face; the vertical maths below
    /// works from the box and the calibrated ink ratios, so it is not otherwise
    /// consulted.
    pub fn new(font: enums::Font, x: i32, y: i32, w: i32, h: i32, size: i32) -> Self {
        let _ = font;
        Self { x, y, w, h, size }
    }

    /// [`LineBox::new`] with the UI face — what most labels want.
    pub fn ui(x: i32, y: i32, w: i32, h: i32, size: i32) -> Self {
        Self::new(ui_font(), x, y, w, h, size)
    }

    /// Optical centre of the box: where a marker that must look level with the
    /// label goes. Identical to where [`text`] puts the label's ink.
    pub fn center_y(&self) -> i32 {
        self.y + self.h / 2
    }

    /// Baseline [`text`] will draw this label at, so callers (and the
    /// calibration probe) can reason about it.
    pub fn baseline_for(&self, text: &str) -> i32 {
        self.center_y() + ink_above_baseline(text, self.size)
    }
}

/// Draw a label inside a measured line box, left-aligned.
///
/// Prefer this over [`text`] when a marker sits next to the label: both then
/// come from the same [`LineBox`] and cannot drift apart.
pub fn line_text(s: &str, b: &LineBox, font: enums::Font, color: u32) {
    text(
        s,
        b.x,
        b.y,
        b.w,
        b.h,
        font,
        b.size,
        color,
        enums::Align::Left,
    );
}

/// Text drawing with an explicit role.
///
/// Nine parameters is a lot, but they are exactly the inputs a text draw needs
/// (string, box, face, size, colour, alignment); bundling them into a struct
/// at every one of the ~30 call sites would add noise without removing a
/// decision.
///
/// The baseline is computed here rather than delegating to FLTK's box form.
/// `fl_draw(str, x, y, w, h, align)` places the baseline at
/// `y + (h - height)/2 + height` when no vertical flag is set — it adds a whole
/// line height instead of the ascent, so every label sits `descent` pixels
/// lower than optical centring (3–5 px at size 14, and visibly so in a dense
/// Chinese UI). Computing `y + (h - height)/2 + ascent` is both correct and
/// independent of which alignment flags happen to be set.
#[allow(clippy::too_many_arguments)]
pub fn text(
    s: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: enums::Font,
    size: i32,
    color: u32,
    align: enums::Align,
) {
    if s.is_empty() {
        return;
    }
    draw::set_font(font, size);
    draw::set_draw_color(rgb(color));

    // Centre the *ink*, not the line box.
    //
    // The box's optical centre is `y + h/2`. To put the ink's middle there the
    // baseline goes `ink_above_baseline` below it — a per-script constant, since
    // a CJK glyph fills the em box while Latin lower case sits in the x-height
    // band. Centring the line box instead (the original `y + (h-line)/2 +
    // ascent`) leaves every Chinese label 3–7 units low, which is what the
    // "文字向下了一些" report was.
    //
    // Markers use the same centre through `LineBox::center_y`, so a dot and its
    // label cannot disagree.
    let baseline = y + h / 2 + ink_above_baseline(s, size);

    // Horizontal anchor: `Align::Center` is the absence of Left/Right, so the
    // flags have to be tested in that order.
    let text_w = draw::measure(s, false).0;
    let left = align.bits() & enums::Align::Left.bits() != 0;
    let right = align.bits() & enums::Align::Right.bits() != 0;
    let x0 = if right {
        x + w - text_w
    } else if left {
        x
    } else {
        x + (w - text_w) / 2
    };

    draw::draw_text(s, x0, baseline);
}

/// Body text, single line, vertically centered in the given box.
pub fn body(s: &str, x: i32, y: i32, w: i32, h: i32, color: u32) {
    text(s, x, y, w, h, ui_font(), T::BODY, color, enums::Align::Left);
}

/// Small secondary text.
pub fn small(s: &str, x: i32, y: i32, w: i32, h: i32, color: u32) {
    text(
        s,
        x,
        y,
        w,
        h,
        ui_font(),
        T::SMALL,
        color,
        enums::Align::Left,
    );
}

/// Micro label: column headers and units. Always paired with a real value.
pub fn micro(s: &str, x: i32, y: i32, w: i32, h: i32, color: u32) {
    text(
        s,
        x,
        y,
        w,
        h,
        ui_font(),
        T::MICRO,
        color,
        enums::Align::Left,
    );
}

/// Monospaced text (numbers, addresses, log lines).
pub fn mono(s: &str, x: i32, y: i32, w: i32, h: i32, size: i32, color: u32) {
    text(s, x, y, w, h, mono_font(), size, color, enums::Align::Left);
}

/// Monospaced text, right-aligned — the alignment used for figures that
/// need to line up column-wise.
pub fn mono_right(s: &str, x: i32, y: i32, w: i32, h: i32, size: i32, color: u32) {
    text(s, x, y, w, h, mono_font(), size, color, enums::Align::Right);
}

/// Width of `s` in the given face and size, for manual layout of chips and
/// right-aligned values.
pub fn measure(s: &str, font: enums::Font, size: i32) -> i32 {
    draw::set_font(font, size);
    draw::measure(s, false).0
}

/// A status dot: filled circle, no outline. Radius 3 keeps it legible at
/// 100% without turning into a badge.
pub fn dot(cx: i32, cy: i32, r: i32, color: u32) {
    draw::set_draw_color(rgb(color));
    draw::draw_pie(cx - r, cy - r, r * 2, r * 2, 0.0, 360.0);
}

/// Truncate `s` with an ellipsis so it fits `max_w` at the given face/size.
/// Keeps list rows from colliding without silently clipping a name in half.
pub fn ellipsize(s: &str, max_w: i32, font: enums::Font, size: i32) -> String {
    if measure(s, font, size) <= max_w {
        return s.to_string();
    }
    let ell = "…";
    let ell_w = measure(ell, font, size);
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = measure(&ch.to_string(), font, size);
        if w + cw + ell_w > max_w {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(ell);
    out
}

/// Push a clip region; pair with [`pop_clip`]. FLTK clipping is a stack.
pub fn push_clip(x: i32, y: i32, w: i32, h: i32) {
    draw::push_clip(x, y, w, h);
}

/// Pop the most recent clip region.
pub fn pop_clip() {
    draw::pop_clip();
}
