//! Visual language for the rx11 console.
//!
//! Direction: a link instrument. The product's core object is a byte stream
//! crossing a wire, so the interface borrows the vocabulary of measurement
//! instruments — graphite surfaces, hairline rules, one amber signal accent,
//! tabular numerals — rather than the generic card-and-shadow dashboard.
//!
//! Three rules keep the language coherent:
//!
//! 1. One hue carries meaning. Amber marks anything live or actionable
//!    (running session, primary button, scope trace, focus). Everything else
//!    is a neutral from the graphite ramp, so a single accent is enough to
//!    tell "this is the thing" from "this is context".
//! 2. Color never carries a state alone. Status is always a dot *plus* a
//!    label ("在线", "重连中", "已停止"), so the console survives a
//!    color-blind reader and a screenshot in grayscale.
//! 3. Density is the feature. This is a tool someone leaves open for hours:
//!    body text sits at 14, labels at 12–13, and numbers get their own
//!    monospaced role so columns line up across rows.
//!
//! # Layering
//!
//! Presentation layer (see the diagram in `src/lib.rs`): `theme / state / ui`.
//! This module is the bottom of it — nothing here knows about pages, state or
//! the data layer. Its three parts are split so each has one job:
//!
//! ```text
//! tokens   palette / type scale / layout grid  — the only source of values
//!    ↓
//! fonts    which faces this machine has
//!    ↓
//! draw     the primitives, and the only caller of fltk::draw
//! ```
//!
//! Everything the rest of the crate uses is re-exported here, so the paths
//! that existed when this was one file (`crate::theme::text`, `C`, `T`, …)
//! all still resolve.

use fltk::app;

mod draw;
mod fonts;
mod tokens;

pub use draw::{
    body, divider, dot, ellipsize, fill, fill_round, hline, is_cjk, line_text, measure, micro,
    mono, mono_right, pop_clip, push_clip, small, stroke_round, text, vline, LineBox,
};
pub use fonts::{mono_font, ui_bold, ui_font, widget_font};
pub use tokens::{mix, rgb, C, L, T};

// ---------------------------------------------------------------------------
// App-level visual setup
// ---------------------------------------------------------------------------

/// Install the console's global look: dark scheme, our font family, and the
/// global colors FLTK uses for menus, selections and tooltips. Called once,
/// before any window is created.
///
/// FLTK's menu popups are native sub-windows; they take their colors from
/// the global scheme, so a dark app needs the scheme set here rather than
/// per widget.
pub fn init() {
    apply_scale();
    // Force the dark scheme: FLTK 1.4+ reads the scheme before first draw.
    app::set_scheme(app::Scheme::Gtk);
    app::set_background_color(0x0E, 0x10, 0x13);
    app::set_background2_color(0x1C, 0x20, 0x26);
    app::set_foreground_color(0xE8, 0xEB, 0xEF);
    app::set_selection_color(0x8A, 0x66, 0x22);

    // Register fonts and install the UI face as the default so every widget
    // we did not paint by hand (text inputs, menus, tooltips) matches.
    app::set_font(ui_font());
    app::set_font_size(T::BODY);
}

/// Match the display's scale factor so text renders at device resolution
/// instead of being stretched by the compositor.
///
/// On a scaled Wayland session X11 clients are drawn at 1× and then magnified
/// by the compositor, which makes every glyph soft. Rendering at the real
/// factor (and sizing type in the same units) keeps text crisp. FLTK has no
/// X11-side way to learn the factor, so it comes from the environment:
///
/// * `RX11_GUI_SCALE` — explicit override, wins over everything;
/// * `GDK_SCALE` / `QT_SCALE_FACTOR` — what the desktop already tells GTK/Qt
///   apps, so the console matches its neighbours;
/// * otherwise 1.0.
fn apply_scale() {
    let explicit = std::env::var("RX11_GUI_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok());
    let detected = std::env::var("GDK_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .or_else(|| {
            std::env::var("QT_SCALE_FACTOR")
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
        });
    let scale = explicit.or(detected).unwrap_or(1.0);
    if (scale - 1.0).abs() > f32::EPSILON && (0.5..=4.0).contains(&scale) {
        app::set_screen_scale(0, scale);
    }
}
