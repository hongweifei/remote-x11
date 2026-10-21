//! Capability negotiation: feature flags advertised in `Hello` and echoed
//! (as the intersection) in `HelloAck`.
//!
//! Wire form is a list of strings so that unknown capabilities are ignored
//! rather than breaking decode: adding a feature later means minting a new
//! name, not a protocol version bump. Behavior gates on `Features`, never
//! on "did a frame arrive" guessing.

use std::collections::BTreeSet;
use std::fmt;

/// Features implemented by this codebase. EXTENDING: add a variant here,
/// register its name, and consume it via `Features::supports` — no other
/// plumbing is required (the negotiation, wire form and gating all derive
/// from this enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// Receiver of incremental diffs can ask the sender to fall back to
    /// full frames (`StreamResyncRequest`).
    RequestResync,
    /// Sender may emit `IncrementalStreamData` / `CompressedIncrementalStreamData`.
    Incremental,
    /// Reserved: `FlowControl` semantics are not implemented yet; the bit
    /// exists so enabling it later does not require a wire change.
    FlowControl,
    /// The `x11` channel kind may be opened via `ChannelOpen`. Channel kinds
    /// are capabilities: unknown names are ignored, so a future clipboard or
    /// file channel ships as one variant + one name without a version bump.
    X11,
}

impl Capability {
    /// What this build can actually do. `FlowControl` is advertised but
    /// both sides must ignore its effect until semantics are decided.
    pub const SUPPORTED: [Capability; 4] = [
        Capability::RequestResync,
        Capability::Incremental,
        Capability::FlowControl,
        Capability::X11,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Capability::RequestResync => "request_resync",
            Capability::Incremental => "incremental",
            Capability::FlowControl => "flow_control",
            Capability::X11 => "x11",
        }
    }

    pub fn parse(name: &str) -> Option<Capability> {
        Capability::SUPPORTED
            .iter()
            .copied()
            .find(|c| c.name() == name)
    }

    /// Wire list advertised by this build.
    pub fn advertised() -> Vec<String> {
        Capability::SUPPORTED
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Negotiated intersection of the two peers' capability lists, resolved once
/// during the handshake and carried through the session. A set, not a struct
/// of bools: new capabilities join without touching this type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Features {
    caps: BTreeSet<Capability>,
}

impl Features {
    /// Intersection of two advertised lists; unknown names are ignored.
    pub fn negotiate(client: &[String], server: &[String]) -> Features {
        let declared = |list: &[String], cap: Capability| list.iter().any(|n| n == cap.name());
        Features {
            caps: Capability::SUPPORTED
                .iter()
                .copied()
                .filter(|c| declared(client, *c) && declared(server, *c))
                .collect(),
        }
    }

    pub fn from_list(list: &[String]) -> Features {
        Features {
            caps: list
                .iter()
                .filter_map(|name| Capability::parse(name))
                .collect(),
        }
    }

    pub fn to_list(&self) -> Vec<String> {
        self.caps.iter().map(|c| c.name().to_string()).collect()
    }

    pub fn supports(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn negotiate_takes_intersection() {
        let client = list(&["incremental", "flow_control", "unknown_future"]);
        let server = list(&["incremental", "request_resync"]);
        let f = Features::negotiate(&client, &server);
        assert!(f.supports(Capability::Incremental));
        assert!(!f.supports(Capability::RequestResync));
        assert!(!f.supports(Capability::FlowControl));
    }

    #[test]
    fn unknown_capabilities_are_ignored_not_rejected() {
        let f = Features::negotiate(&list(&["made_up_v9"]), &list(&["made_up_v9"]));
        assert_eq!(f, Features::default());
    }

    #[test]
    fn channel_kinds_are_capabilities() {
        // Opening a channel kind must ride on a capability name.
        assert_eq!(Capability::X11.name(), "x11");
        let f = Features::from_list(&list(&["x11", "incremental"]));
        assert!(f.supports(Capability::X11) && f.supports(Capability::Incremental));
    }

    #[test]
    fn advertised_round_trips_through_parse() {
        let adv = Capability::advertised();
        for name in &adv {
            assert!(Capability::parse(name).is_some(), "unparseable: {name}");
        }
        let f = Features::from_list(&adv);
        assert_eq!(f.to_list().len(), Capability::SUPPORTED.len());
        assert!(f.supports(Capability::X11));
    }

    #[test]
    fn empty_side_negotiates_nothing() {
        let f = Features::negotiate(&[], &Capability::advertised());
        assert_eq!(f, Features::default());
    }
}
