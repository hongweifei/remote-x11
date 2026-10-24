//! Interactive controls, painted by hand, driven by the kit.
//!
//! Only the controls the console actually uses live here: a button, a text
//! field shell, a drop-down and a switch. Anything a page draws once and never
//! interacts with belongs in the page's own paint routine, so this module
//! stays a control kit rather than a half-used component library.
//!
//! FLTK's stock widgets carry a 1990s chrome that would fight the instrument
//! language, so the drawn controls here are [`Interactive`] widgets: the
//! paint is ours, while hover/press/focus, Tab reachability and Enter/Space
//! activation all come from [`crate::ui::kit`]. Text entry still rides
//! `fltk::input::Input` (a real text editor is not worth reimplementing) and
//! menus still ride `fltk::menu::Choice` (a menu popup is a real window), but
//! both are framed to read as part of the same grammar.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::Align;
use fltk::input::Input;
use fltk::prelude::*;
use fltk::widget::Widget;

use crate::theme::{self as th, C, L, T};
use crate::ui::kit::{Hand, Interactive, Sense, Touch};

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
    on_click: Option<Box<dyn FnMut()>>,
}

/// A hand-painted button. Hover, press, focus and keyboard activation are
/// the kit's; the click callback runs with the state cell *unborrowed*, so a
/// handler may rebuild the UI (including this button) without tripping the
/// `RefCell`.
#[derive(Clone)]
pub struct Button {
    it: Interactive<Rc<RefCell<BtnState>>>,
    st: Rc<RefCell<BtnState>>,
}

impl Button {
    pub fn new(x: i32, y: i32, width: i32, height: i32, label: &str, kind: BtnKind) -> Self {
        let st = Rc::new(RefCell::new(BtnState {
            label: label.to_string(),
            kind,
            enabled: true,
            on_click: None,
        }));
        let st_draw = st.clone();
        let it = Interactive::build(x, y, width, height, st.clone())
            .tip(label)
            .draw(move |w, _, sense| {
                let s = st_draw.borrow();
                paint_button(w.x(), w.y(), w.w(), w.h(), &s, sense);
            })
            .event(move |_w, cell, _, touch| match touch {
                // A disabled button claims nothing: the event falls through
                // to whatever is underneath, exactly as an absent button.
                Touch::Click { .. } => {
                    if !cell.borrow().enabled {
                        return Hand::Ignored;
                    }
                    // Take the callback out, run it unborrowed (it may rebuild
                    // the page), then put it back.
                    let mut cb = cell.borrow_mut().on_click.take();
                    if let Some(f) = cb.as_mut() {
                        f();
                    }
                    cell.borrow_mut().on_click = cb;
                    Hand::Claimed
                }
                Touch::Press { .. } if cell.borrow().enabled => Hand::Claimed,
                Touch::Move { .. } | Touch::Exit | Touch::Release { .. } | Touch::Drag { .. } => {
                    Hand::Claimed
                }
                Touch::Press { .. }
                | Touch::Wheel(_)
                | Touch::Key { .. }
                | Touch::Shortcut { .. } => Hand::Ignored,
            })
            .install();
        Self { it, st }
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
        self.it.set_tip(label);
        self.redraw();
    }

    pub fn redraw(&self) {
        self.it.redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.it.resize(x, y, w, h);
    }

    /// The underlying widget, for callers that must reach FLTK directly.
    pub fn widget(&self) -> Widget {
        self.it.widget()
    }
}

fn paint_button(x: i32, y: i32, w: i32, h: i32, s: &BtnState, sense: &Sense) {
    let (fill, text, border) = match (s.kind, s.enabled, sense.hover) {
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

    let dy = if sense.pressed { 1 } else { 0 };
    th::fill_round(x, y + dy, w, h, L::RADIUS, fill);
    match s.kind {
        BtnKind::Primary => {}
        _ => th::stroke_round(x, y + dy, w, h, L::RADIUS, border),
    }
    if sense.focused {
        // The amber accent doubles as the keyboard focus ring: one hairline
        // outside the button makes a Tab-cycled control unmistakable.
        th::stroke_round(x - 2, y - 2, w + 4, h + 4, L::RADIUS + 1, C::AMBER);
    }
    th::text(
        &s.label,
        x,
        y + dy,
        w,
        h,
        th::ui_font(),
        T::BODY,
        text,
        Align::Center,
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
    enabled: bool,
    on_change: Option<Box<dyn FnMut(bool)>>,
}

/// A boolean toggle: a short track with a travelling knob. Used for the
/// capability kill switches, where the binary nature is the point. Like the
/// button its state machine is the kit's; it toggles on a *completed* click —
/// mouse or Enter/Space — and the callback runs unborrowed.
#[derive(Clone)]
pub struct Switch {
    it: Interactive<Rc<RefCell<SwitchState>>>,
    st: Rc<RefCell<SwitchState>>,
}

impl Switch {
    pub fn new(x: i32, y: i32, on: bool) -> Self {
        let st = Rc::new(RefCell::new(SwitchState {
            on,
            enabled: true,
            on_change: None,
        }));
        let st_draw = st.clone();
        let it = Interactive::build(x, y, 38, 20, st.clone())
            .draw(move |w, _, sense| {
                let s = st_draw.borrow();
                paint_switch(w.x(), w.y(), w.w(), w.h(), &s, sense);
            })
            .event(move |_w, cell, _, touch| match touch {
                Touch::Click { .. } => {
                    if !cell.borrow().enabled {
                        return Hand::Ignored;
                    }
                    let v = {
                        let mut b = cell.borrow_mut();
                        b.on = !b.on;
                        b.on
                    };
                    let mut cb = cell.borrow_mut().on_change.take();
                    if let Some(f) = cb.as_mut() {
                        f(v);
                    }
                    cell.borrow_mut().on_change = cb;
                    Hand::Claimed
                }
                Touch::Press { .. } if cell.borrow().enabled => Hand::Claimed,
                Touch::Move { .. } | Touch::Exit | Touch::Release { .. } | Touch::Drag { .. } => {
                    Hand::Claimed
                }
                Touch::Press { .. }
                | Touch::Wheel(_)
                | Touch::Key { .. }
                | Touch::Shortcut { .. } => Hand::Ignored,
            })
            .install();
        Self { it, st }
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
        self.it.redraw();
    }

    pub fn widget(&self) -> Widget {
        self.it.widget()
    }

    pub fn resize(&self, x: i32, y: i32) {
        self.it.resize(x, y, 38, 20);
    }
}

fn paint_switch(x: i32, y: i32, w: i32, h: i32, s: &SwitchState, sense: &Sense) {
    let track = if !s.enabled {
        th::mix(C::RAISED, C::BG, 0.5)
    } else if s.on {
        th::mix(C::AMBER, C::BG, 0.30)
    } else if sense.hover {
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
    if sense.focused {
        // Same amber hairline focus ring as the buttons.
        th::stroke_round(x - 2, y - 2, w + 4, h + 4, (h + 4) / 2, C::AMBER);
    }
}

// ---------------------------------------------------------------------------
// Painted text helpers used by pages
// ---------------------------------------------------------------------------
