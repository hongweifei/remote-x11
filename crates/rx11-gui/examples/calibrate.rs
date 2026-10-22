//! Calibration probe: render the console's real labels, and next to each one a
//! column of candidate dots at known offsets. Measuring the screenshot says
//! which offset puts the dot on the label's optical centre — no font-metric
//! derivation involved.
//!
//! Run: `RX11_GUI_SCALE=1.25 cargo run -p rx11-gui --example calibrate`
//!
//! Why: under `set_screen_scale` FLTK draws text with an internal `-1` offset
//! (`Fl_Scalable_Graphics_Driver::draw`, "for issue #1308"), so `fl_height()`
//! and `fl_descent()` do not predict the rendered baseline exactly. Measuring is
//! cheaper and more honest than modelling that.
//!
//! The amber column is the candidate offsets; the red dot is what
//! `LineBox::center_y()` currently returns. If the red dot is not level with the
//! ink of the label beside it, retune `ink_above_baseline` in `theme/draw.rs`.

use fltk::prelude::*;

use rx11_gui::theme::C as c;

const W: i32 = 760;
const H: i32 = 320;
/// Candidate offsets, in logical units, above the line box baseline.
const CANDIDATES: [i32; 10] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

fn main() {
    let _app = fltk::app::App::default();
    rx11_gui::theme::init();

    let mut win = fltk::window::DoubleWindow::new(40, 40, W, H, "calibrate");
    win.set_color(fltk::enums::Color::from_rgb(14, 16, 19));

    let mut canvas = fltk::widget::Widget::new(0, 0, W, H, None);
    canvas.set_frame(fltk::enums::FrameType::NoBox);
    canvas.draw(|_| {
        let c = rx11_gui::theme::C;
        // Two cases: the CJK node label and the Latin wordmark.
        let cases: [(&str, fltk::enums::Font, i32, i32, i32); 2] = [
            ("中继", rx11_gui::theme::ui_font(), 20, 32, 13),
            ("rx11", rx11_gui::theme::mono_font(), 18, 26, 18),
        ];
        for (ci, (text, font, region_y, region_h, size)) in cases.into_iter().enumerate() {
            let base_y = 40 + ci as i32 * 150;
            let line =
                rx11_gui::theme::LineBox::new(font, 150, base_y + region_y, 200, region_h, size);
            rx11_gui::theme::line_text(text, &line, font, c::TEXT);
            // Baseline rule: the reference the eye uses.
            rx11_gui::theme::fill(150, line.baseline_for(text), 200, 1, c::LINE_HI);
            // Candidate dots, each labelled by its offset above the baseline.
            for off in CANDIDATES {
                let y = line.baseline_for(text) - off;
                rx11_gui::theme::dot(120, y, 3, c::AMBER);
                rx11_gui::theme::micro(&format!("-{off}"), 128, y - 5, 24, 12, c::TEXT_3);
            }
            // What the current formula returns, for comparison.
            rx11_gui::theme::dot(60, line.center_y(), 3, c::BAD);
        }
    });

    win.end();
    win.show();
    let _ = fltk::app::run();
}
