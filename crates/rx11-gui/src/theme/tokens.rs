//! Design tokens: the console's palette, type scale and layout grid.
//!
//! Position in the layering (see `src/lib.rs`): the bottom of the presentation
//! layer (`theme / state / ui`). Nothing here draws and nothing here knows
//! about pages, so every other presentation module can read the tokens while
//! the tokens depend on nothing.
//!
//! This is the **only** place a colour, a font size or a layout metric is
//! defined. [`rgb`] and [`mix`] live here as well because they convert between
//! the two token forms (hex literal and FLTK colour); a caller doing its own
//! colour arithmetic would be minting a second palette.

use fltk::enums;

/// Hex colors, `0xRRGGBB`. Grouped by role rather than by lightness so the
/// ramp stays consistent when a surface is added.
pub struct C;

impl C {
    // Surfaces, darkest to lightest.
    /// Window base behind everything.
    pub const BG: u32 = 0x0E1013;
    /// Cards, sidebar, log well.
    pub const PANEL: u32 = 0x15181C;
    /// Inputs, hovered rows, raised chips.
    pub const RAISED: u32 = 0x1C2026;
    /// Pressed / selected row.
    pub const PRESSED: u32 = 0x242A31;
    /// Inset wells (scope plot, log).
    pub const SUNKEN: u32 = 0x0A0C0E;

    // Rules.
    /// Hairline between regions.
    pub const LINE: u32 = 0x23282E;
    /// Divider that should read as structural.
    pub const LINE_HI: u32 = 0x333A43;

    // Text.
    /// Primary text and numbers.
    pub const TEXT: u32 = 0xE8EBEF;
    /// Text drawn by FLTK's own widgets (inputs, drop-downs).
    ///
    /// Kept separate from [`C::TEXT`] because FLTK's input path renders a
    /// 0xE8-high colour invisibly on this stack while a full-intensity one
    /// draws: the two are within a few percent of each other visually, and a
    /// field whose value cannot be read is not a trade worth making for that
    /// difference. Painted text keeps using [`C::TEXT`].
    pub const WIDGET_TEXT: u32 = 0xFFFFFF;
    /// Secondary text: labels, descriptions.
    pub const TEXT_2: u32 = 0x99A2AC;
    /// Tertiary text: units, timestamps, disabled.
    pub const TEXT_3: u32 = 0x66707B;

    // The one accent.
    /// Signal amber: live state, primary action, scope trace.
    pub const AMBER: u32 = 0xF2B23C;
    /// Brighter amber for hover/emphasis.
    pub const AMBER_HI: u32 = 0xFFC96B;
    /// Amber reduced to a readable-on-graphite weight for secondary series.
    pub const AMBER_DIM: u32 = 0xA07A2A;
    /// The second series: a desaturated sand, one hue family with amber.
    pub const SAND: u32 = 0x8C7A5E;

    // Semantic status. Always paired with a label.
    pub const OK: u32 = 0x4FBF8F;
    pub const WARN: u32 = 0xE8A33D;
    pub const BAD: u32 = 0xE0604A;
    /// Idle / stopped / never started.
    pub const IDLE: u32 = 0x5A636D;
}

/// Convert a `0xRRGGBB` literal into an FLTK color.
pub fn rgb(hex: u32) -> enums::Color {
    enums::Color::from_rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

/// Blend `fg` over `bg` at `alpha` (0.0 = pure bg, 1.0 = pure fg). Used to
/// derive fills and disabled states from the ramp instead of minting more
/// colors; keeps the palette to the two dozen entries above.
///
/// Returns a hex literal like the palette entries so every drawing helper
/// takes one color type and callers never have to convert.
pub fn mix(fg: u32, bg: u32, alpha: f32) -> u32 {
    let a = alpha.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let f = ((fg >> shift) & 0xFF) as f32;
        let b = ((bg >> shift) & 0xFF) as f32;
        (b + (f - b) * a).round().clamp(0.0, 255.0) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Type scale. Sizes are FLTK units; on a 1.25-scaled desktop one unit is one
/// device pixel, so 14 reads as a comfortable body size in a dense tool.
pub struct T;

impl T {
    /// Window-level page title.
    pub const DISPLAY: i32 = 26;
    /// Card heading.
    pub const H2: i32 = 15;
    /// Body text and control labels.
    pub const BODY: i32 = 14;
    /// Secondary body, list subtitles.
    pub const SMALL: i32 = 13;
    /// Micro labels, column headers, units.
    pub const MICRO: i32 = 12;
    /// Hero statistic.
    pub const STAT: i32 = 30;
    /// Log and tabular figures.
    pub const MONO: i32 = 13;
    /// Brand wordmark.
    pub const BRAND: i32 = 18;
}

/// Layout constants: one source of truth for the shell so every region
/// aligns on the same grid.
pub struct L;

impl L {
    pub const TOPBAR_H: i32 = 58;
    pub const SIDEBAR_W: i32 = 236;
    pub const STATUS_H: i32 = 28;
    /// Outer padding inside the content area.
    pub const PAD: i32 = 22;
    /// Gap between cards.
    pub const GAP: i32 = 14;
    /// Corner radius. Small on purpose: an instrument has machined edges,
    /// not pillowy ones.
    pub const RADIUS: i32 = 6;
}
