//! Platform font detection: which faces this machine actually has.
//!
//! Position in the layering (see `src/lib.rs`): presentation layer, beside
//! [`super::tokens`] and below [`super::draw`]. FLTK only exposes families that
//! fontconfig has already told it about, so the faces are probed once — the
//! registry is global and enumerating it is expensive — and the callers
//! ([`super::draw`] and the widget setup in [`super::init`]) ask for a role
//! (`ui`, `ui_bold`, `mono`, `widget`) rather than a family name.
//!
//! [`super::init`]: crate::theme::init

use std::sync::OnceLock;

use fltk::{app, enums};

/// Resolved once: FLTK's font registry is global and expensive to enumerate.
pub(super) struct Fonts {
    ui: enums::Font,
    ui_bold: enums::Font,
    mono: enums::Font,
}

pub(super) fn fonts() -> &'static Fonts {
    static F: OnceLock<Fonts> = OnceLock::new();
    F.get_or_init(|| {
        // Register the system families we care about; FLTK only exposes what
        // fontconfig has already told it about. Broad patterns are harmless.
        app::set_fonts("Noto Sans*");
        app::set_fonts("Noto Serif*");
        app::set_fonts("JetBrainsMono*");
        app::set_fonts("Adwaita*");
        let names = app::get_font_names();
        let has = |n: &str| names.iter().any(|f| f == n);

        let ui = pick(
            &names,
            &[
                "Noto Sans CJK SC",
                "Noto Sans CJK SC Regular",
                "Noto Sans SC",
                "Source Han Sans SC",
                "WenQuanYi Micro Hei",
                "Adwaita Sans",
                "Sans",
            ],
        );
        let ui_bold = pick(
            &names,
            &[
                "Noto Sans CJK SC Bold",
                "Noto Sans SC Bold",
                "Adwaita Sans Bold",
                "Sans Bold",
            ],
        );
        let mono = pick(
            &names,
            &[
                "JetBrainsMono NF",
                "JetBrainsMono Nerd Font",
                "Adwaita Mono",
                "Monospace",
            ],
        );
        let _ = has;
        Fonts {
            ui: enums::Font::by_name(&ui),
            ui_bold: enums::Font::by_name(&ui_bold),
            mono: enums::Font::by_name(&mono),
        }
    })
}

pub(super) fn pick(names: &[String], candidates: &[&str]) -> String {
    for c in candidates {
        if names.iter().any(|n| n == c) {
            return (*c).to_string();
        }
    }
    candidates.last().copied().unwrap_or("Sans").to_string()
}

/// UI face (Chinese-capable sans).
pub fn ui_font() -> enums::Font {
    fonts().ui
}

/// UI face, heavier weight. Falls back to the regular face where the system
/// has no bold CJK cut.
pub fn ui_bold() -> enums::Font {
    fonts().ui_bold
}

/// Tabular face for numbers, addresses and log lines.
pub fn mono_font() -> enums::Font {
    fonts().mono
}

/// Face to hand to FLTK *widgets* (text inputs, drop-downs) rather than to
/// our own drawing routines.
///
/// `init()` maps FLTK's default font slot (index 0) onto [`ui_font`], and it
/// is that slot a widget should use: passing a font *index* obtained from
/// `Font::by_name` into `set_text_font` renders nothing on this system, while
/// the same index drawn directly through `draw::set_font` works fine. Using
/// the default slot keeps widget text identical to the painted text.
///
/// [`ui_font`]: ui_font
pub fn widget_font() -> enums::Font {
    enums::Font::by_index(0)
}
