//! Interactive controls, painted by hand.
//!
//! Only the controls the console actually uses live here: a button, a text
//! field shell, a drop-down and a switch. Anything a page draws once and never
//! interacts with belongs in the page's own paint routine, so this module
//! stays a control kit rather than a half-used component library.
//!
//! FLTK's stock widgets carry a 1990s chrome that would fight the instrument
//! language, so every control here is a `Widget` with its own draw and
//! handle closures: fill, hairline, label and state all come from
//! [`crate::theme`]. Text entry still rides `fltk::input::Input` (a real text
//! editor is not worth reimplementing), but it is framed by our own field
//! shell so it reads as part of the same grammar.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::{Align, Event};
use fltk::input::Input;
use fltk::prelude::*;
use fltk::widget::Widget;

use crate::theme::{self as th, C, L, T};

// ---------------------------------------------------------------------------
// Button
// ---------------------------------------------------------------------------

/// How much weight a button carries. Only one primary button should be
/// visible per region: the action the operator came to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    /// Filled amber. The main action.
    Primary,
    /// Outlined, neutral fill. Secondary actions.
    Secondary,
    /// Text only. Tertiary actions and inline affordances.
    Ghost,
    /// Destructive: stop, discard.
    Danger,
}

struct BtnState {
    label: String,
    kind: BtnKind,
    enabled: bool,
    hover: bool,
    pressed: bool,
    on_click: Option<Box<dyn FnMut()>>,
}

/// A hand-painted button.
#[derive(Clone)]
pub struct Button {
    w: Widget,
    st: Rc<RefCell<BtnState>>,
}

impl Button {
    pub fn new(x: i32, y: i32, width: i32, height: i32, label: &str, kind: BtnKind) -> Self {
        let st = Rc::new(RefCell::new(BtnState {
            label: label.to_string(),
            kind,
            enabled: true,
            hover: false,
            pressed: false,
            on_click: None,
        }));

        let mut w = Widget::new(x, y, width, height, None);
        w.set_frame(fltk::enums::FrameType::NoBox);
        w.clear_visible_focus();

        let st_draw = st.clone();
        w.draw(move |w| {
            let s = st_draw.borrow();
            paint_button(w.x(), w.y(), w.w(), w.h(), &s);
        });

        let st_ev = st.clone();
        w.handle(move |w, ev| {
            let mut s = st_ev.borrow_mut();
            if !s.enabled {
                return false;
            }
            match ev {
                Event::Enter => {
                    s.hover = true;
                    w.redraw();
                    true
                }
                Event::Leave => {
                    s.hover = false;
                    s.pressed = false;
                    w.redraw();
                    true
                }
                Event::Push => {
                    s.pressed = true;
                    w.redraw();
                    true
                }
                Event::Released => {
                    let fire = s.pressed && s.hover;
                    s.pressed = false;
                    w.redraw();
                    if fire {
                        // Release the borrow before running user code: the
                        // callback is allowed to rebuild the UI.
                        drop(s);
                        let mut cb = st_ev.borrow_mut().on_click.take();
                        if let Some(f) = cb.as_mut() {
                            f();
                        }
                        st_ev.borrow_mut().on_click = cb;
                    }
                    true
                }
                _ => false,
            }
        });

        Self { w, st }
    }

    /// Install the click action. Replacing it is fine — pages rebuild their
    /// handlers when the session list changes.
    pub fn on_click<F: FnMut() + 'static>(&self, f: F) {
        self.st.borrow_mut().on_click = Some(Box::new(f));
    }

    /// Enable or disable the button. Disabled buttons keep their geometry so
    /// the row does not shift when a session starts or stops.
    pub fn set_enabled(&self, on: bool) {
        self.st.borrow_mut().enabled = on;
        self.redraw();
    }

    pub fn set_label(&self, label: &str) {
        self.st.borrow_mut().label = label.to_string();
        self.redraw();
    }

    pub fn redraw(&self) {
        self.w.clone().redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.w.clone().resize(x, y, w, h);
    }
}

fn paint_button(x: i32, y: i32, w: i32, h: i32, s: &BtnState) {
    let (fill, text, border) = match (s.kind, s.enabled, s.hover) {
        (_, false, _) => (th::mix(C::RAISED, C::BG, 0.45), C::TEXT_3, C::LINE),
        (BtnKind::Primary, true, false) => (C::AMBER, C::BG, C::AMBER),
        (BtnKind::Primary, true, true) => (C::AMBER_HI, C::BG, C::AMBER_HI),
        (BtnKind::Secondary, true, false) => (C::RAISED, C::TEXT, C::LINE_HI),
        (BtnKind::Secondary, true, true) => (C::PRESSED, C::TEXT, C::TEXT_3),
        (BtnKind::Ghost, true, false) => (C::PANEL, C::TEXT_2, C::PANEL),
        (BtnKind::Ghost, true, true) => (C::RAISED, C::TEXT, C::LINE),
        (BtnKind::Danger, true, false) => (th::mix(C::BAD, C::BG, 0.18), C::BAD, C::BAD),
        (BtnKind::Danger, true, true) => (th::mix(C::BAD, C::BG, 0.30), C::BAD, C::BAD),
    };

    let dy = if s.pressed { 1 } else { 0 };
    th::fill_round(x, y + dy, w, h, L::RADIUS, fill);
    match s.kind {
        BtnKind::Primary => {}
        _ => th::stroke_round(x, y + dy, w, h, L::RADIUS, border),
    }
    let align = Align::Center;
    th::text(
        &s.label,
        x,
        y + dy,
        w,
        h,
        th::ui_font(),
        T::BODY,
        text,
        align,
    );
}

// ---------------------------------------------------------------------------
// Field
// ---------------------------------------------------------------------------

/// A labelled text input: caption above, entry below, optional hint under it.
/// The caption is drawn on the shared background canvas, so a field owns only
/// its input widget and the geometry the caller laid out.
pub struct Field {
    input: Input,
}

impl Field {
    /// Create the input at `(x, y, w, h)`. The caller draws the caption at
    /// `label_y` and the hint at `hint_y` (if it wants one).
    pub fn new(x: i32, y: i32, w: i32, h: i32, value: &str) -> Self {
        let mut input = Input::new(x + 1, y + 1, w - 2, h - 2, None);
        input.set_frame(fltk::enums::FrameType::FlatBox);
        // Colours are applied *after* the value: FLTK's input keeps its own
        // text colour slot and a value assignment made before it can leave the
        // widget drawing with the default (black) glyphs on a dark fill.
        input.set_value(value);
        input.set_color(th::rgb(C::RAISED));
        input.set_text_color(th::rgb(C::WIDGET_TEXT));
        input.set_text_font(th::widget_font());
        input.set_text_size(T::BODY);
        input.set_selection_color(th::rgb(th::mix(C::AMBER, C::RAISED, 0.45)));
        input.set_cursor_color(th::rgb(C::AMBER));
        let _ = (y, h);
        Self { input }
    }

    pub fn value(&self) -> String {
        self.input.value()
    }

    pub fn set_value(&self, v: &str) {
        let mut i = self.input.clone();
        i.set_value(v);
        // An input shown after the window was mapped has no damage of its own,
        // so FLTK skips it on a partial repaint and the value stays invisible
        // until something else touches it. Mark it explicitly.
        i.redraw();
    }

    pub fn input(&self) -> Input {
        self.input.clone()
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        let mut i = self.input.clone();
        i.resize(x + 1, y + 1, w - 2, h - 2);
    }
}

// ---------------------------------------------------------------------------
// Select
// ---------------------------------------------------------------------------

/// A styled drop-down. Wraps FLTK's `Choice` because a menu popup is a real
/// window and reimplementing it would be worse than skinning it.
pub struct Select {
    choice: fltk::menu::Choice,
    st: Rc<RefCell<Vec<String>>>,
}

impl Select {
    pub fn new(x: i32, y: i32, w: i32, h: i32, items: &[&str], selected: usize) -> Self {
        let mut choice = fltk::menu::Choice::new(x, y, w, h, None);
        choice.set_frame(fltk::enums::FrameType::FlatBox);
        choice.set_color(th::rgb(C::RAISED));
        choice.set_text_color(th::rgb(C::WIDGET_TEXT));
        choice.set_text_font(th::widget_font());
        choice.set_text_size(T::BODY);
        choice.set_selection_color(th::rgb(th::mix(C::AMBER, C::RAISED, 0.5)));
        // Populate through the menu API so the popup is owned by the widget.
        for it in items {
            choice.add_choice(it);
        }
        choice.set_value(selected as i32);
        let st = Rc::new(RefCell::new(
            items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
        ));
        Self { choice, st }
    }

    pub fn value(&self) -> usize {
        self.choice.value().max(0) as usize
    }

    pub fn set_value(&self, idx: usize) {
        let mut c = self.choice.clone();
        c.set_value(idx as i32);
    }

    pub fn items(&self) -> Vec<String> {
        self.st.borrow().clone()
    }

    pub fn on_change<F: FnMut() + 'static>(&self, mut f: F) {
        let mut c = self.choice.clone();
        c.set_callback(move |_| f());
    }

    pub fn widget(&self) -> fltk::menu::Choice {
        self.choice.clone()
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        let mut c = self.choice.clone();
        c.resize(x, y, w, h);
    }
}

// ---------------------------------------------------------------------------
// Switch
// ---------------------------------------------------------------------------

struct SwitchState {
    on: bool,
    hover: bool,
    enabled: bool,
    on_change: Option<Box<dyn FnMut(bool)>>,
}

/// A boolean toggle: a short track with a travelling knob. Used for the
/// capability kill switches, where the binary nature is the point.
#[derive(Clone)]
pub struct Switch {
    w: Widget,
    st: Rc<RefCell<SwitchState>>,
}

impl Switch {
    pub fn new(x: i32, y: i32, on: bool) -> Self {
        let st = Rc::new(RefCell::new(SwitchState {
            on,
            hover: false,
            enabled: true,
            on_change: None,
        }));
        let mut w = Widget::new(x, y, 38, 20, None);
        w.set_frame(fltk::enums::FrameType::NoBox);

        let st_draw = st.clone();
        w.draw(move |w| {
            let s = st_draw.borrow();
            paint_switch(w.x(), w.y(), w.w(), w.h(), &s);
        });

        let st_ev = st.clone();
        w.handle(move |w, ev| {
            let mut s = st_ev.borrow_mut();
            if !s.enabled {
                return false;
            }
            match ev {
                Event::Enter => {
                    s.hover = true;
                    w.redraw();
                    true
                }
                Event::Leave => {
                    s.hover = false;
                    w.redraw();
                    true
                }
                Event::Push => {
                    s.on = !s.on;
                    w.redraw();
                    drop(s);
                    let mut cb = st_ev.borrow_mut().on_change.take();
                    if let Some(f) = cb.as_mut() {
                        f(st_ev.borrow().on);
                    }
                    st_ev.borrow_mut().on_change = cb;
                    true
                }
                _ => false,
            }
        });

        Self { w, st }
    }

    pub fn on_change<F: FnMut(bool) + 'static>(&self, f: F) {
        self.st.borrow_mut().on_change = Some(Box::new(f));
    }

    pub fn value(&self) -> bool {
        self.st.borrow().on
    }

    pub fn set_value(&self, on: bool) {
        self.st.borrow_mut().on = on;
        self.redraw();
    }

    pub fn redraw(&self) {
        self.w.clone().redraw();
    }

    pub fn widget(&self) -> Widget {
        self.w.clone()
    }

    pub fn resize(&self, x: i32, y: i32) {
        self.w.clone().resize(x, y, 38, 20);
    }
}

fn paint_switch(x: i32, y: i32, w: i32, h: i32, s: &SwitchState) {
    let track = if !s.enabled {
        th::mix(C::RAISED, C::BG, 0.5)
    } else if s.on {
        th::mix(C::AMBER, C::BG, 0.30)
    } else if s.hover {
        C::PRESSED
    } else {
        C::RAISED
    };
    th::fill_round(x, y, w, h, h / 2, track);
    th::stroke_round(
        x,
        y,
        w,
        h,
        h / 2,
        if s.on { C::AMBER_DIM } else { C::LINE_HI },
    );
    let knob_d = h - 6;
    let knob_x = if s.on { x + w - 3 - knob_d } else { x + 3 };
    let knob = if !s.enabled {
        C::TEXT_3
    } else if s.on {
        C::AMBER
    } else {
        C::TEXT_2
    };
    th::dot(knob_x + knob_d / 2, y + h / 2, knob_d / 2, knob);
}

// ---------------------------------------------------------------------------
// Painted text helpers used by pages
// ---------------------------------------------------------------------------
