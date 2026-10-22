//! The UI's intent channel: one callback-slot shape shared by every control.
//!
//! Position in the layering (see `src/lib.rs`): `ui`, the presentation layer.
//! Pages draw and state intent; they never start, stop or save anything. A slot
//! is how that intent leaves a page and reaches `app` — the composition root,
//! and the only layer that knows the backend. One generic slot instead of a
//! private copy per page is what keeps `app` the single place where actions are
//! translated.
//!
//! [`emit_action`] deliberately clones the handler out of the `RefCell` before
//! calling it: a handler may re-enter the page that fired it and mutate the
//! page's own state, and calling while the borrow is held would panic. The
//! clone is the fix, not an oversight.
//!
//! [`TopbarAction`] lives here too: the shell's action tuple is the same
//! "state intent, let `app` act" contract, just with the payload baked in.

use std::cell::RefCell;
use std::rc::Rc;

/// A page's action callback slot: at most one handler, installed by the app.
pub type ActionSlot<A> = Rc<RefCell<Option<Rc<dyn Fn(A)>>>>;

/// A top-bar action: label, enabled flag, and the action itself.
pub type TopbarAction = (String, bool, Rc<dyn Fn()>);

/// Install the handler for `slot`, replacing any previous one. Equivalent to
/// `*slot.borrow_mut() = Some(Rc::new(f))`, kept as a function so the pattern
/// exists once instead of once per page.
pub fn set_handler<A, F>(slot: &ActionSlot<A>, f: F)
where
    F: Fn(A) + 'static,
{
    *slot.borrow_mut() = Some(Rc::new(f));
}

/// Fire the slot's handler, if one is installed. See the module docs for why
/// the handler is cloned out before it is called.
pub fn emit_action<A>(slot: &ActionSlot<A>, action: A) {
    let cb = slot.borrow().clone();
    if let Some(f) = cb {
        f(action);
    }
}
