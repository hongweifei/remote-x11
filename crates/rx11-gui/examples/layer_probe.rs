//! Layering probe: does a custom-painted background plus custom child widgets
//! actually render inside one FLTK group?
//!
//! Run with `cargo run -p rx11-gui --example layer_probe` (a display is
//! required). It is a scratch diagnostic kept in the tree because the group
//! draw-order rules it checks are non-obvious and cost real debugging time.

use fltk::enums::FrameType;
use fltk::prelude::*;
use fltk::widget::Widget;

fn main() {
    let app = fltk::app::App::default();
    let mut win = fltk::window::Window::new(60, 60, 520, 320, "layer probe");
    win.set_color(fltk::enums::Color::from_rgb(20, 22, 26));

    let mut group = fltk::group::Group::new(0, 0, 520, 320, None);
    group.set_frame(FrameType::NoBox);
    group.begin();

    // 1. Background painter first: FLTK draws children in creation order, so
    //    the first child is the bottom-most.
    let mut bg = Widget::new(0, 0, 520, 320, None);
    bg.set_frame(FrameType::NoBox);
    bg.draw(|w| {
        fltk::draw::draw_rect_fill(
            w.x(),
            w.y(),
            w.w(),
            w.h(),
            fltk::enums::Color::from_rgb(30, 34, 40),
        );
        fltk::draw::set_draw_color(fltk::enums::Color::from_rgb(200, 200, 200));
        fltk::draw::set_font(fltk::enums::Font::Helvetica, 14);
        fltk::draw::draw_text2(
            "background",
            w.x() + 40,
            w.y() + 200,
            300,
            20,
            fltk::enums::Align::Left,
        );
    });

    // 2. A control created second — should end up on top of the background.
    let mut top = Widget::new(40, 40, 200, 60, None);
    top.set_frame(FrameType::NoBox);
    top.draw(|w| {
        fltk::draw::draw_rect_fill(
            w.x(),
            w.y(),
            w.w(),
            w.h(),
            fltk::enums::Color::from_rgb(240, 180, 60),
        );
        fltk::draw::set_draw_color(fltk::enums::Color::from_rgb(10, 10, 10));
        fltk::draw::set_font(fltk::enums::Font::Helvetica, 18);
        fltk::draw::draw_text2(
            "child on top",
            w.x() + 10,
            w.y() + 20,
            w.w(),
            20,
            fltk::enums::Align::Left,
        );
    });

    // 3. A real FLTK input in the same group.
    let mut input = fltk::input::Input::new(40, 140, 300, 32, None);
    input.set_value("0.0.0.0:7000");
    input.set_text_size(14);
    input.set_text_color(fltk::enums::Color::from_rgb(232, 235, 239));
    input.set_color(fltk::enums::Color::from_rgb(28, 32, 38));
    input.set_frame(FrameType::FlatBox);

    group.end();
    win.end();
    win.show();

    println!("group children = {}", group.children());
    println!(
        "top visible = {}  bg visible = {}",
        top.visible(),
        bg.visible()
    );

    // Repaint the group repeatedly, the way a live page would.
    fltk::app::add_timeout3(0.5, |h| {
        println!("tick");
        fltk::app::repeat_timeout3(0.5, h);
    });

    app.run().unwrap();
}
