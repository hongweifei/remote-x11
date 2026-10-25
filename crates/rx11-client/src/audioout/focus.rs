//! Which remote application is in the foreground.
//!
//! The client must hear the application the user is looking at. The
//! local X server knows which window has the input focus, and each remote
//! application's windows live in a resource range that belongs to the
//! connection that created them — so the focused window identifies the
//! X11 connection, and that connection identifies the application.
//!
//! The judgement is made by asking the local X server, on a connection
//! of its own. That is deliberate:
//!
//! * The application connections carry the X11 request sequence numbers;
//!   asking the server on one of them would advance that counter and the
//!   application would see a sequence error. A separate monitor connection
//!   asks the question without touching the application's byte stream.
//! * The question is about the server's GLOBAL state, so any connection
//!   may ask it.
//!
//! The resource ranges come from the setup reply of each application
//! connection — bytes that are already in the stream, read and never
//! written.

use std::collections::HashMap;

use rx11_core::types::ConnectionId;

/// The resource range a connection owns in the X server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResourceRange {
    /// The first resource id of the range.
    pub base: u32,
    /// The last resource id of the range.
    pub top: u32,
}

impl ResourceRange {
    /// Build a range from the setup reply's base and mask.
    pub(crate) fn new(base: u32, mask: u32) -> Self {
        Self {
            base,
            top: base | mask,
        }
    }

    /// Whether the id belongs to this range.
    pub(crate) fn contains(&self, id: u32) -> bool {
        (self.base..=self.top).contains(&id)
    }
}

/// The resource ranges of the application connections, by X11 connection id.
#[derive(Debug, Default)]
pub(crate) struct ResourceIndex {
    ranges: HashMap<u32, ResourceRange>,
}

impl ResourceIndex {
    /// Register the range of one application connection.
    pub(crate) fn register(&mut self, connection_id: ConnectionId, range: ResourceRange) {
        self.ranges.insert(connection_id.get(), range);
    }

    /// Forget a connection's range.
    pub(crate) fn forget(&mut self, connection_id: ConnectionId) {
        self.ranges.remove(&connection_id.get());
    }

    /// The connection whose range holds this resource id.
    ///
    /// `None` when no registered range holds it — the id belongs to a
    /// window the client does not know, or to the monitor connection.
    pub(crate) fn connection_of(&self, resource_id: u32) -> Option<ConnectionId> {
        self.ranges
            .iter()
            .find(|(_, range)| range.contains(resource_id))
            .map(|(id, _)| ConnectionId::new(*id))
    }
}

/// The focus of the local X server: which window has the input focus.
///
/// The window id is `PointerRoot` (1) when no window holds the focus.
pub(crate) const POINTER_ROOT: u32 = 1;

/// Whether the focus id names a real window.
pub(crate) fn focus_names_a_window(focus: u32) -> bool {
    focus != POINTER_ROOT && focus != 0
}

/// The foreground connection, from the focused window and the ranges.
///
/// `None` when the focus is not on a window, or on a window whose
/// connection is not registered — the caller then plays every
/// application.
pub(crate) fn foreground_connection(focus: u32, index: &ResourceIndex) -> Option<ConnectionId> {
    if !focus_names_a_window(focus) {
        return None;
    }
    index.connection_of(focus)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: u32) -> ConnectionId {
        ConnectionId::new(id)
    }

    #[test]
    fn a_range_holds_the_ids_between_its_base_and_its_top() {
        let range = ResourceRange::new(0x2000000, 0x1fffff);
        assert!(range.contains(0x2000000));
        assert!(range.contains(0x2000001));
        assert!(range.contains(0x21fffff));
        assert!(!range.contains(0x1fffff), "below the range");
        assert!(!range.contains(0x2200000), "above the range");
    }

    #[test]
    fn the_window_id_names_the_connection_that_created_it() {
        // Measured on a live server: three connections keep disjoint
        // ranges, and each window belongs to the connection that made it.
        let mut index = ResourceIndex::default();
        index.register(conn(1), ResourceRange::new(0x2000000, 0x1fffff));
        index.register(conn(2), ResourceRange::new(0x2200000, 0x1fffff));
        index.register(conn(3), ResourceRange::new(0x2400000, 0x1fffff));

        assert_eq!(index.connection_of(0x2000001), Some(conn(1)));
        assert_eq!(index.connection_of(0x2200002), Some(conn(2)));
        assert_eq!(index.connection_of(0x2400003), Some(conn(3)));
        // A window of the monitor connection is in nobody's range.
        assert_eq!(index.connection_of(0x2600000), None);
    }

    #[test]
    fn a_focus_without_a_window_yields_no_foreground() {
        let mut index = ResourceIndex::default();
        index.register(conn(1), ResourceRange::new(0x2000000, 0x1fffff));
        assert_eq!(foreground_connection(POINTER_ROOT, &index), None);
        assert_eq!(foreground_connection(0, &index), None);
    }

    #[test]
    fn a_focus_on_an_unknown_window_yields_no_foreground() {
        let mut index = ResourceIndex::default();
        index.register(conn(1), ResourceRange::new(0x2000000, 0x1fffff));
        assert_eq!(foreground_connection(0x2200001, &index), None);
    }

    #[test]
    fn a_focus_on_a_known_window_yields_its_connection() {
        let mut index = ResourceIndex::default();
        index.register(conn(7), ResourceRange::new(0x2000000, 0x1fffff));
        assert_eq!(foreground_connection(0x2000001, &index), Some(conn(7)));
    }

    #[test]
    fn a_forgotten_connection_no_longer_claims_windows() {
        let mut index = ResourceIndex::default();
        index.register(conn(7), ResourceRange::new(0x2000000, 0x1fffff));
        index.forget(conn(7));
        assert_eq!(foreground_connection(0x2000001, &index), None);
    }
}
