//! Capability negotiation: feature flags advertised in `Hello` and echoed
//! (as the intersection) in `HelloAck`.
//!
//! Wire form is a list of strings so that unknown capabilities are ignored
//! rather than breaking decode: adding a feature later means minting a new
//! name, not a protocol version bump. Behavior gates on `Features`, never
//! on "did a frame arrive" guessing.

use std::fmt;

/// Features implemented by this codebase. Extend by adding variants and
/// wiring their `name()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// Receiver of incremental diffs can ask the sender to fall back to
    /// full frames (`IncrementalRequestFull`).
    RequestResync,
    /// Sender may emit `IncrementalDataX11` / `CompressedIncrementalDataX11`.
    Incremental,
    /// Reserved: `FlowControl` semantics are not implemented yet; the bit
    /// exists so enabling it later does not require a wire change.
    FlowControl,
}

impl Capability {
    /// What this build can actually do. `FlowControl` is advertised but
    /// both sides must ignore its effect until semantics are decided.
    pub const SUPPORTED: [Capability; 3] = [
        Capability::RequestResync,
        Capability::Incremental,
        Capability::FlowControl,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Capability::RequestResync => "request_resync",
            Capability::Incremental => "incremental",
            Capability::FlowControl => "flow_control",
        }
    }

    pub fn parse(name: &str) -> Option<Capability> {
        Capability::SUPPORTED.iter().copied().find(|c| c.name() == name)
    }

    /// Wire list advertised by this build.
    pub fn advertised() -> Vec<String> {
        Capability::SUPPORTED.iter().map(|c| c.name().to_string()).collect()
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Negotiated intersection of the two peers' capability lists, resolved once
/// during the handshake and carried through the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    pub request_resync: bool,
    pub incremental: bool,
    pub flow_control: bool,
}

impl Features {
    /// Intersection of two advertised lists; unknown names are ignored.
    pub fn negotiate(client: &[String], server: &[String]) -> Features {
        let declared = |list: &[String], cap: Capability| {
            list.iter().any(|name| name == cap.name())
        };
        let both = |cap: Capability| declared(client, cap) && declared(server, cap);
        Features {
            request_resync: both(Capability::RequestResync),
            incremental: both(Capability::Incremental),
            flow_control: both(Capability::FlowControl),
        }
    }

    pub fn from_list(list: &[String]) -> Features {
        let has = |cap: Capability| list.iter().any(|name| name == cap.name());
        Features {
            request_resync: has(Capability::RequestResync),
            incremental: has(Capability::Incremental),
            flow_control: has(Capability::FlowControl),
        }
    }

    pub fn to_list(&self) -> Vec<String> {
        let mut out = Vec::new();
        for cap in Capability::SUPPORTED {
            if self.supports(cap) {
                out.push(cap.name().to_string());
            }
        }
        out
    }

    pub fn supports(self, cap: Capability) -> bool {
        match cap {
            Capability::RequestResync => self.request_resync,
            Capability::Incremental => self.incremental,
            Capability::FlowControl => self.flow_control,
        }
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
        assert!(f.incremental);
        assert!(!f.request_resync);
        assert!(!f.flow_control);
    }

    #[test]
    fn unknown_capabilities_are_ignored_not_rejected() {
        let f = Features::negotiate(&list(&["made_up_v9"]), &list(&["made_up_v9"]));
        assert_eq!(f, Features::default());
    }

    #[test]
    fn advertised_round_trips_through_parse() {
        let adv = Capability::advertised();
        for name in &adv {
            assert!(Capability::parse(name).is_some(), "unparseable: {name}");
        }
        let f = Features::from_list(&adv);
        assert!(f.incremental && f.request_resync && f.flow_control);
        assert_eq!(f.to_list().len(), 3);
    }

    #[test]
    fn empty_side_negotiates_nothing() {
        let f = Features::negotiate(&[], &Capability::advertised());
        assert_eq!(f, Features::default());
    }
}
