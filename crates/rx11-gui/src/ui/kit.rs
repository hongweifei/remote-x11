//! The interaction kit: one pointer state machine, one coordinate system.
//!
//! Every clickable, draggable and focusable surface in this console used to
//! carry its own hand-written `Enter/Leave/Push/Drag/Released` closure — the
//! button here, the rail there, the same anchor-drag model once per scrolling
//! page. This module owns that machinery once, so a control declares only its
//! paint and its meaning.
//!
//! # Coordinate rule
//!
//! All `Touch` coordinates are **window-relative** — the same space the
//! painters use. Checked against the bundled FLTK 1.4.5 source
//! (`Fl_Group.cxx:95`): the coordinate translation in `send()` applies to
//! child *windows* only, so a plain widget's `handle` sees
//! `Fl::event_x()` untranslated from the window origin. The rectangle drawn
//! and the point hit-tested are therefore already in one space — callers must
//! not add or subtract widget origins. (The old page-relative `PageInput`
//! conversion, undone differently on each page, was the origin of a real
//! off-by-the-page bug in the form drag.)
//!
//! # Keyboard rule
//!
//! FLTK hands `FL_KEYBOARD` to `Fl::focus()` and bubbles up the parent chain
//! (`Fl.cxx:1529`), and `Fl_Group` tab-traversal calls `take_focus()` on each
//! child, which for a plain widget means: accept focus by returning `true`
//! from `handle(Event::Focus)` and keep `visible_focus` (`Fl_Widget.cxx:150`,
//! `Fl.cxx:1060`). The kit implements, per widget:
//!
//! * Tab / Shift-Tab reachability for widgets built focusable;
//! * Enter / Space activation through the *same* `Touch::Click` as the mouse,
//!   synthesised at the widget centre — controls implement one activation
//!   path, not two;
//! * arrow / Page / Home / End scrolling through [`Grip::on_key`], so the
//!   scrolling surfaces work without a pointer;
//! * tooltips on FLTK's own timer (`set_tooltip`), coloured by
//!   [`crate::theme::init`] so they belong to the same language.
//!
//! Re-entrancy note: the event closure runs while the kit holds the widget's
//! state borrow. Controls that must run user callbacks which can touch the
//! same control (a click handler disabling its own button) keep their state
//! in a separate `Rc<RefCell<_>>` and drop *that* borrow before invoking the
//! callback — the `Button` is the reference pattern.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::{Event, Key, Shortcut};
use fltk::prelude::*;
use fltk::widget::Widget;

use super::layout::Rect;
use super::scrollbar::{Hit, ScrollBar};
use super::PageInput;

// ---------------------------------------------------------------------------
// Sense — the shared interaction state every widget carries
// ---------------------------------------------------------------------------

/// Pointer and keyboard state the kit maintains for every interactive
/// widget. A control never tracks hover/press/focus itself; it reads these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sense {
    pub hover: bool,
    pub pressed: bool,
    pub focused: bool,
}

/// What the kit delivers to a control's event closure. All coordinates are
/// window-relative — see the module docs.
#[derive(Debug, Clone)]
pub enum Touch {
    /// Pointer inside; `Enter` and `Move` both map here.
    Move { at: (i32, i32) },
    /// Pointer left the widget; press state is already cleared.
    Exit,
    /// Button pressed inside. Anything but [`Hand::Ignored`] claims the
    /// whole gesture until release.
    Press { at: (i32, i32) },
    /// Pointer moved with the button held (only after a claimed press).
    Drag { at: (i32, i32) },
    /// Button released; delivered even outside the widget so a drag ends
    /// cleanly. [`Touch::Click`] was already delivered when released inside.
    Release { at: (i32, i32) },
    /// A completed press-and-release inside the widget — the activation
    /// event. Keyboard Enter/Space on a focused widget synthesises the same
    /// touch at the widget centre.
    Click { at: (i32, i32) },
    /// Wheel notch; positive scrolls down (`event_dy_value`).
    Wheel(i32),
    /// A keystroke delivered while this widget holds focus.
    Key { key: Key, text: String },
    /// A global key the focus chain did not consume; plain widgets match
    /// their own shortcuts here (key plus modifier flags).
    Shortcut { key: Key, mods: Shortcut },
}

impl Touch {
    /// The pointer location a pointer touch carries, if any.
    pub fn point(&self) -> Option<(i32, i32)> {
        match self {
            Touch::Move { at }
            | Touch::Press { at }
            | Touch::Drag { at }
            | Touch::Release { at }
            | Touch::Click { at } => Some(*at),
            _ => None,
        }
    }
}

/// The control's verdict on a touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    /// Not ours: the event falls through to what is underneath. A press
    /// that is not claimed means no Drag/Release follows.
    Ignored,
    /// Ours; no repaint requested.
    Claimed,
    /// Ours; the widget repaints.
    Repaint,
}

impl Hand {
    fn claimed(self) -> bool {
        self != Hand::Ignored
    }
}

// ---------------------------------------------------------------------------
// Interactive — a widget whose state machine comes from the kit
// ---------------------------------------------------------------------------

struct Inner<S> {
    sense: Sense,
    data: S,
    paint: PaintFn<S>,
    event: EventFn<S>,
    focusable: bool,
}

/// The control's paint closure: draw the widget box from its data and the
/// kit-maintained sense flags.
type PaintFn<S> = Box<dyn FnMut(&Widget, &mut S, &Sense)>;

/// The control's event closure: interpret a touch, return a verdict.
type EventFn<S> = Box<dyn FnMut(&Widget, &mut S, &mut Sense, &Touch) -> Hand>;

/// A plain `Widget` with the kit's canonical draw+handle closures installed.
/// `S` is the control's own data; the kit owns hover/press/focus. Cloning is
/// cheap and flat: every clone sees the one state cell, like `Widget` itself.
pub struct Interactive<S> {
    w: Widget,
    inner: Rc<RefCell<Inner<S>>>,
}

impl<S> Clone for Interactive<S> {
    fn clone(&self) -> Self {
        Self {
            w: self.w.clone(),
            inner: self.inner.clone(),
        }
    }
}

/// Deferred construction: paint and event closures are installed together,
/// because they share one state cell.
pub struct InteractiveBuilder<S> {
    w: Widget,
    data: S,
    tip: Option<String>,
    focusable: bool,
    paint: Option<PaintFn<S>>,
    event: Option<EventFn<S>>,
}

impl<S: 'static> InteractiveBuilder<S> {
    /// Paint callback — mandatory: the kit never guesses how a control looks.
    pub fn draw<F>(mut self, f: F) -> Self
    where
        F: FnMut(&Widget, &mut S, &Sense) + 'static,
    {
        self.paint = Some(Box::new(f));
        self
    }

    /// Event callback; the default swallows everything the kit delivers.
    pub fn event<F>(mut self, f: F) -> Self
    where
        F: FnMut(&Widget, &mut S, &mut Sense, &Touch) -> Hand + 'static,
    {
        self.event = Some(Box::new(f));
        self
    }

    /// FLTK tooltip text.
    pub fn tip(mut self, text: impl Into<String>) -> Self {
        self.tip = Some(text.into());
        self
    }

    /// Whether Tab can land here. Default `true` for interactive controls;
    /// pure display surfaces (the scope canvas) must opt out or they steal
    /// focus from the controls behind them.
    pub fn focus(mut self, on: bool) -> Self {
        self.focusable = on;
        self
    }

    /// Install the closures and return the handle.
    pub fn install(self) -> Interactive<S> {
        let paint = self
            .paint
            .expect("kit: an interactive widget needs a draw closure");
        let event = self
            .event
            .unwrap_or_else(|| Box::new(|_, _, _, _| Hand::Ignored));
        let inner = Rc::new(RefCell::new(Inner {
            sense: Sense::default(),
            data: self.data,
            paint,
            event,
            focusable: self.focusable,
        }));

        let mut w = self.w;
        w.set_frame(fltk::enums::FrameType::NoBox);
        if let Some(text) = self.tip {
            w.set_tooltip(&text);
        }
        // `visible_focus` stays on for focusable widgets (FLTK's `Fl::focus`
        // ignores widgets without it); non-focusable ones must not be
        // reachable by Tab at all.
        if !self.focusable {
            w.clear_visible_focus();
        }

        let st_draw = inner.clone();
        w.draw(move |w| {
            let mut b = st_draw.borrow_mut();
            let Inner {
                sense, data, paint, ..
            } = &mut *b;
            paint(w, data, sense);
        });

        let st_ev = inner.clone();
        w.handle(move |w, ev| {
            // Window-relative, same space as the painters (see module docs).
            let (x, y) = fltk::app::event_coords();
            let mut b = st_ev.borrow_mut();
            let Inner {
                sense,
                data,
                event,
                focusable,
                ..
            } = &mut *b;
            let inside = x >= w.x() && x < w.x() + w.w() && y >= w.y() && y < w.y() + w.h();
            // Sense flips repaint *by construction*: whenever the kit changes
            // hover/press, the widget redraws even if the behaviour returns
            // only `Claimed`. A control that paints a hover or pressed state
            // therefore cannot forget to refresh it; `Hand::Repaint` stays
            // available for the control's own data-driven repaints.
            let hand = match ev {
                Event::Enter => {
                    let fresh = !std::mem::replace(&mut sense.hover, true);
                    if fresh {
                        w.redraw();
                    }
                    event(w, data, sense, &Touch::Move { at: (x, y) })
                }
                Event::Move => event(w, data, sense, &Touch::Move { at: (x, y) }),
                Event::Leave => {
                    if sense.hover || sense.pressed {
                        sense.hover = false;
                        sense.pressed = false;
                        w.redraw();
                    }
                    event(w, data, sense, &Touch::Exit)
                }
                Event::Push => {
                    let h = event(w, data, sense, &Touch::Press { at: (x, y) });
                    sense.pressed = h.claimed();
                    if sense.pressed {
                        w.redraw();
                    }
                    h
                }
                Event::Drag => {
                    if !sense.pressed {
                        return false;
                    }
                    event(w, data, sense, &Touch::Drag { at: (x, y) })
                }
                Event::Released => {
                    if !sense.pressed {
                        return false;
                    }
                    if std::mem::take(&mut sense.pressed) {
                        w.redraw();
                    }
                    let mut final_hand = Hand::Ignored;
                    if inside {
                        let h = event(w, data, sense, &Touch::Click { at: (x, y) });
                        if !h.claimed() {
                            // A press claimed but a click refused: the widget
                            // does not want this gesture at all.
                            return false;
                        }
                        final_hand = h;
                    }
                    let h = event(w, data, sense, &Touch::Release { at: (x, y) });
                    if h.claimed() {
                        final_hand = h;
                    }
                    final_hand
                }
                Event::MouseWheel => {
                    event(w, data, sense, &Touch::Wheel(fltk::app::event_dy_value()))
                }
                Event::Focus => {
                    if !*focusable {
                        return false;
                    }
                    sense.focused = true;
                    w.redraw();
                    return true;
                }
                Event::Unfocus => {
                    sense.focused = false;
                    w.redraw();
                    return *focusable;
                }
                Event::KeyDown => {
                    if !*focusable {
                        return false;
                    }
                    let key = fltk::app::event_key();
                    let text = fltk::app::event_text();
                    if key == Key::Enter || key == Key::KPEnter || text == " " {
                        // Keyboard activation is the mouse activation: the
                        // control implements one path.
                        let at = (w.x() + w.w() / 2, w.y() + w.h() / 2);
                        event(w, data, sense, &Touch::Click { at })
                    } else {
                        event(w, data, sense, &Touch::Key { key, text })
                    }
                }
                Event::Shortcut => event(
                    w,
                    data,
                    sense,
                    &Touch::Shortcut {
                        key: fltk::app::event_key(),
                        mods: fltk::app::event_state(),
                    },
                ),
                _ => return false,
            };
            if hand == Hand::Repaint {
                w.redraw();
            }
            hand.claimed()
        });

        Interactive { w, inner }
    }
}

impl<S: 'static> Interactive<S> {
    /// Begin building: a plain `NoBox` widget at `(x, y, w, h)` in the
    /// painter's space, owned by whatever group is current. Named `build`,
    /// not `new`: it hands back the deferred-construction builder, because
    /// paint and event closures share one state cell and can only be
    /// installed together.
    pub fn build(x: i32, y: i32, w: i32, h: i32, data: S) -> InteractiveBuilder<S> {
        InteractiveBuilder {
            w: Widget::new(x, y, w, h, None),
            data,
            tip: None,
            focusable: true,
            paint: None,
            event: None,
        }
    }

    pub fn widget(&self) -> Widget {
        self.w.clone()
    }

    /// The current sense, for callers outside the paint pass.
    pub fn sense(&self) -> Sense {
        self.inner.borrow().sense
    }

    /// Mutate the data without redrawing.
    pub fn edit<F: FnOnce(&mut S)>(&self, f: F) {
        f(&mut self.inner.borrow_mut().data);
    }

    /// Mutate the data and repaint.
    pub fn update<F: FnOnce(&mut S)>(&self, f: F) {
        f(&mut self.inner.borrow_mut().data);
        self.w.clone().redraw();
    }

    /// Move the widget; the paint closure reads the new box itself.
    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        let mut wid = self.w.clone();
        wid.resize(x, y, w, h);
    }

    /// Repaint without touching state.
    pub fn redraw(&self) {
        self.w.clone().redraw();
    }

    /// The tooltip needs updating after construction (a dynamic hint).
    pub fn set_tip(&self, text: impl Into<String>) {
        let mut w = self.w.clone();
        w.set_tooltip(&text.into());
    }
}

// ---------------------------------------------------------------------------
// ListSieve — hover/press/click over a dynamic list of rectangles
// ---------------------------------------------------------------------------

/// Which index of a hit list the pointer is on, tracked press-correctly:
/// press and release must land on the *same* index for a click to fire.
/// This is the machine the rail, the topbar actions and the session-table
/// 「停止」 buttons each used to hand-roll. `Copy`: pages keep it in their
/// state cell and take it out to run an event, writing it back after.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListSieve {
    pub hover: Option<usize>,
    pub pressed: Option<usize>,
}

/// Index of the first rectangle containing the point (painter space).
pub fn hit_index(rects: &[Rect], x: i32, y: i32) -> Option<usize> {
    rects.iter().position(|r| r.contains(x, y))
}

impl ListSieve {
    /// Pointer moved: returns whether the hover index changed.
    pub fn move_to(&mut self, idx: Option<usize>) -> bool {
        if self.hover == idx {
            return false;
        }
        self.hover = idx;
        true
    }

    /// Pointer left the surface: clears hover and the dangling press (a
    /// release outside never fires). Returns whether repaint is due.
    pub fn exit(&mut self) -> bool {
        let dirty = self.hover.is_some() || self.pressed.is_some();
        self.hover = None;
        self.pressed = None;
        dirty
    }

    /// Press: the gesture is claimed iff the point hit something.
    pub fn press(&mut self, idx: Option<usize>) -> bool {
        self.pressed = idx;
        idx.is_some()
    }

    /// Release: fire when the same index is still under the pointer.
    pub fn release(&mut self, idx: Option<usize>) -> Option<usize> {
        let fired = match (self.pressed, idx) {
            (Some(p), Some(r)) if p == r => Some(p),
            _ => None,
        };
        self.pressed = None;
        fired
    }
}

// ---------------------------------------------------------------------------
// Grip — the anchor-model scrollbar drag, once
// ---------------------------------------------------------------------------

/// The scrollbar gesture state: how many pixels below the knob top was
/// grabbed, and where the knob is *drawn* while held. The model, measured on
/// the probe rig (DESIGN §6.7): while held the knob is painted at
/// `pointer − grab` (clamped to the track) and the content offset is the one
/// inverse division `scroll_at(knob)` — deriving the knob from the offset
/// instead composes two truncating divisions and measures as a period-5
/// staircase. `Copy`: same take-out-and-write-back discipline as ListSieve.
#[derive(Debug, Clone, Copy, Default)]
pub struct Grip {
    grab: Option<i32>,
    knob: Option<i32>,
}

/// What a navigation key means for a scrolling surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glide {
    /// Move by a pixel delta (arrows: one row).
    By(i32),
    /// Move by signed viewport heights (Page Up/Down).
    Page(i32),
    /// Jump to the first row (Home); the write path clamps.
    Start,
    /// Jump past the end (End); the write path clamps.
    End,
}

/// Key → scroll intent; unknown keys are not consumed.
pub fn glide(key: Key, row_px: i32) -> Option<Glide> {
    Some(match key {
        k if k == Key::Up => Glide::By(-row_px),
        k if k == Key::Down => Glide::By(row_px),
        k if k == Key::PageUp => Glide::Page(-1),
        k if k == Key::PageDown => Glide::Page(1),
        k if k == Key::Home => Glide::Start,
        k if k == Key::End => Glide::End,
        _ => return None,
    })
}

impl Grip {
    /// Is a knob currently held (the bar paints hot)?
    pub fn held(&self) -> bool {
        self.grab.is_some()
    }

    /// Where the knob is painted while held — `ScrollBar::paint`'s `drag_top`.
    pub fn knob(&self) -> Option<i32> {
        self.knob
    }

    /// Paint arguments for the shared bar: `(hot, drag_top)`.
    pub fn painting(&self) -> (bool, Option<i32>) {
        (self.held(), self.knob)
    }

    /// Drive one pointer event through the anchor model. Keyboard scrolling
    /// is [`Grip::on_key`]; a page that receives `PageInput::Key` calls it
    /// here as `on_key` after letting the pointer paths run.
    ///
    /// * `bar` — the bar as painted, rebuilt from current state (one solve:
    ///   the thing you grab is the thing you see);
    /// * `scroll_by` / `scroll_to` — the page's single offset write paths,
    ///   clamping and storing; return `true` when the offset really moved;
    /// * `repaint` — the page's whole-group redraw. Called on every real
    ///   move and on every drag frame (the knob under the cursor is itself a
    ///   change); the kit never invents partial damage.
    ///
    /// `Wheel` goes to the page first: each surface picks its own rhythm —
    /// one row per notch on the table, `dy` rows on the log well.
    pub fn on_input(
        &mut self,
        input: PageInput,
        wheel: &mut dyn FnMut(i32) -> bool,
        bar: &mut dyn FnMut() -> Option<ScrollBar>,
        scroll_by: &mut dyn FnMut(i32) -> bool,
        scroll_to: &mut dyn FnMut(i32) -> bool,
        repaint: &mut dyn FnMut(),
    ) -> bool {
        match input {
            PageInput::Wheel(dy) => {
                if dy == 0 {
                    return false;
                }
                wheel(dy)
            }
            PageInput::Push(x, y) => {
                let Some(b) = bar() else { return false };
                match b.hit(x, y) {
                    Some(Hit::Thumb { grab }) => {
                        // The press anchors the knob at the *visible*
                        // position: pressing must not move anything.
                        self.grab = Some(grab);
                        self.knob = Some(b.thumb_top());
                        repaint();
                        true
                    }
                    Some(Hit::Track { dir }) => {
                        if scroll_by(dir * b.viewport()) {
                            repaint();
                        }
                        true
                    }
                    None => false,
                }
            }
            PageInput::Drag(_x, y) => {
                let Some(grab) = self.grab else { return false };
                let Some(b) = bar() else { return false };
                let (lo, hi) = b.knob_top_range();
                let knob = (y - grab).clamp(lo, hi);
                self.knob = Some(knob);
                // The knob is the thing under the cursor: repaint every
                // frame, even when the clamped offset did not move.
                scroll_to(b.scroll_at(knob));
                repaint();
                true
            }
            PageInput::Release => {
                let was = self.grab.take();
                self.knob = None;
                if was.is_some() {
                    repaint();
                }
                was.is_some()
            }
            PageInput::Key(_) => false,
        }
    }

    /// Keyboard scrolling with the page's row metric: arrows move whole
    /// rows, Page Up/Down viewports, Home/End the ends. A surface with no
    /// bar (everything fits) consumes nothing, so plain pages keep typing.
    pub fn on_key(
        &mut self,
        key: Key,
        row_px: i32,
        bar: &mut dyn FnMut() -> Option<ScrollBar>,
        scroll_by: &mut dyn FnMut(i32) -> bool,
        scroll_to: &mut dyn FnMut(i32) -> bool,
        repaint: &mut dyn FnMut(),
    ) -> bool {
        let Some(g) = glide(key, row_px) else {
            return false;
        };
        let Some(b) = bar() else {
            return false;
        };
        let changed = match g {
            Glide::By(d) => scroll_by(d),
            Glide::Page(dir) => scroll_by(dir * b.viewport()),
            Glide::Start => scroll_to(0),
            Glide::End => scroll_to(i32::MAX),
        };
        if changed {
            repaint();
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// Tests — the pure state machines, no FLTK window needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn click_fires_only_when_press_and_release_share_the_row() {
        let rects = vec![Rect::new(0, 0, 100, 20), Rect::new(0, 20, 100, 20)];
        let mut sieve = ListSieve::default();
        assert_eq!(hit_index(&rects, 5, 25), Some(1));
        assert!(sieve.press(Some(1)));
        // Drag off and release on another row: nothing fires.
        assert_eq!(sieve.release(Some(0)), None);
        assert!(sieve.press(Some(1)));
        assert_eq!(sieve.release(Some(1)), Some(1));
        // A release without a claimed press is not a click.
        assert!(!sieve.press(None));
        assert_eq!(sieve.release(Some(0)), None);
    }

    #[test]
    fn hover_and_exit_keep_the_sieve_clean() {
        let mut sieve = ListSieve::default();
        assert!(sieve.move_to(Some(2)));
        assert!(!sieve.move_to(Some(2)));
        sieve.press(Some(2));
        assert!(sieve.exit());
        assert_eq!(sieve.hover, None);
        assert_eq!(sieve.pressed, None);
        assert!(!sieve.exit());
    }

    /// The anchor model end to end: the press does not move content, a drag
    /// writes `scroll_at(pointer − grab)` every frame, the knob stays
    /// clamped to the track, release cools without a further offset write.
    #[test]
    fn grip_anchors_the_knob_and_writes_only_through_the_page_paths() {
        let b0 = ScrollBar::new(1400, 316, 478, 8000, 480, 0).unwrap();
        let press_at = b0.thumb_top() + 12;
        let mut grip = Grip::default();
        let offset = Cell::new(0i32);
        let paints = Cell::new(0u32);
        let mut scroll_to = |v: i32| {
            let next = v.clamp(0, b0.max_scroll());
            let changed = next != offset.get();
            offset.set(next);
            changed
        };
        let mut scroll_by = |d: i32| {
            let next = (offset.get() + d).clamp(0, b0.max_scroll());
            let changed = next != offset.get();
            offset.set(next);
            changed
        };
        let mut repaint = || paints.set(paints.get() + 1);
        let mut wheel = |_: i32| false;
        let mut mk_bar = || Some(b0);

        // Press on the knob: held, painted hot, content untouched.
        assert!(grip.on_input(
            PageInput::Push(1402, press_at),
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert_eq!(offset.get(), 0, "按下本身不许移动内容");
        assert_eq!(grip.knob(), Some(b0.thumb_top()));
        assert!(grip.held());

        // Drag 200px down: knob = pointer − grab, offset from one division.
        assert!(grip.on_input(
            PageInput::Drag(1402, press_at + 200),
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert_eq!(grip.knob(), Some(b0.thumb_top() + 200));
        assert_eq!(offset.get(), b0.scroll_at(b0.thumb_top() + 200));

        // Past the bottom: the knob clamps, the offset sits at max, and the
        // frame still repaints (the knob is under the cursor).
        let before = paints.get();
        assert!(grip.on_input(
            PageInput::Drag(1402, 99999),
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert_eq!(grip.knob(), Some(b0.knob_top_range().1));
        assert_eq!(offset.get(), b0.max_scroll());
        assert_eq!(paints.get(), before + 1);

        // Release cools the knob once; a second release is not consumed.
        assert!(grip.on_input(
            PageInput::Release,
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert!(!grip.held());
        assert!(!grip.on_input(
            PageInput::Release,
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));

        // An off-bar press falls through untouched.
        assert!(!grip.on_input(
            PageInput::Push(20, 400),
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));

        // A track click pages by exactly one viewport — the track spans it.
        // Click well below the (top-anchored) knob, on a reset list.
        offset.set(0);
        let track_px = b0.knob_top_range().0 + 60;
        assert!(grip.on_input(
            PageInput::Push(1402, track_px),
            &mut wheel,
            &mut mk_bar,
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert_eq!(offset.get(), b0.viewport());
    }

    #[test]
    fn glide_maps_the_navigation_keys() {
        assert_eq!(glide(Key::Up, 20), Some(Glide::By(-20)));
        assert_eq!(glide(Key::Down, 20), Some(Glide::By(20)));
        assert_eq!(glide(Key::PageUp, 20), Some(Glide::Page(-1)));
        assert_eq!(glide(Key::PageDown, 20), Some(Glide::Page(1)));
        assert_eq!(glide(Key::Home, 20), Some(Glide::Start));
        assert_eq!(glide(Key::End, 20), Some(Glide::End));
        assert_eq!(glide(Key::None, 20), None);
    }

    #[test]
    fn keyboard_scrolling_needs_a_bar_and_moves_whole_rows() {
        let mut grip = Grip::default();
        let offset = Cell::new(0i32);
        let max = ScrollBar::new(0, 0, 100, 8000, 480, 0)
            .unwrap()
            .max_scroll();
        // Independent of each other, exactly as the pages' closures are: both
        // touch only the shared cell, never one another.
        let mut scroll_to = |v: i32| {
            let next = v.clamp(0, max);
            let changed = next != offset.get();
            offset.set(next);
            changed
        };
        let mut scroll_by = |d: i32| {
            let next = (offset.get() + d).clamp(0, max);
            let changed = next != offset.get();
            offset.set(next);
            changed
        };
        let repaints = Cell::new(0u32);
        let mut repaint = || repaints.set(repaints.get() + 1);
        assert!(grip.on_key(
            Key::Down,
            20,
            &mut || Some(ScrollBar::new(0, 0, 100, 8000, 480, offset.get()).unwrap()),
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert_eq!(offset.get(), 20);
        // Everything fits → no bar → the page keeps its arrows unclaimed.
        assert!(!grip.on_key(
            Key::Down,
            20,
            &mut || ScrollBar::new(0, 0, 100, 400, 480, offset.get()),
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert!(grip.on_key(
            Key::End,
            20,
            &mut || Some(ScrollBar::new(0, 0, 100, 8000, 480, offset.get()).unwrap()),
            &mut scroll_by,
            &mut scroll_to,
            &mut repaint,
        ));
        assert_eq!(offset.get(), 8000 - 480);
    }
}
