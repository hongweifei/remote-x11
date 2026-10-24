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
    /// The CLIENT end may diff its uplink (local X server → relay) too.
    /// `Incremental` alone is the downlink direction (relay → client, the
    /// picture): the client's uplink carries the application's REQUEST
    /// stream, whose position-based chunks have a different redundancy
    /// profile, so the direction is negotiated separately — a peer that
    /// survives from before this variant keeps the old downlink-only policy
    /// instead of receiving diff frames it was never asked to apply.
    /// Advertised by default; either peer opts OUT with
    /// `[runtime] incremental_uplink = false`.
    IncrementalUplink,
    /// Per-channel bulk pause/resume (`FlowControl` frames): a stalled
    /// consumer asks the sender to stop delivering x11 data for one
    /// `connection_id`, and the sender's pump stops reading the producing
    /// socket. Control frames and the audio real-time lane are exempt.
    /// Advertised by default; either peer opts OUT with
    /// `[runtime] flow_control = false`, falling back to pure
    /// transport-level backpressure.
    FlowControl,
    /// The `x11` channel kind may be opened via `ChannelOpen`. Channel kinds
    /// are capabilities: unknown names are ignored, so a future clipboard or
    /// file channel ships as one variant + one name without a version bump.
    X11,
    /// The x11 channel may re-encode large PutImage payloads in transit
    /// (PNG default tier — lossless; JPEG via explicit codec choice).
    /// Advertised by default; either peer opts OUT with
    /// `[runtime] image_recode = false`. The lossy JPEG tier is never the
    /// default, so default-on keeps the pixel-exactness promise.
    X11Recode,
    /// The `audio` channel kind may be opened: server-side capture of the
    /// default output sink, Opus-encoded, delivered on the real-time lane.
    /// Advertised by default; either peer opts OUT with
    /// `[runtime] audio_forward = false`. Endpoints without a capture or
    /// playback stack stay silent instead of failing.
    AudioForward,
}

impl Capability {
    /// What this build can actually do. Every capability here is fully
    /// implemented; the gated ones ride default-ON with per-end kill
    /// switches in `advertised_for`.
    pub const SUPPORTED: [Capability; 7] = [
        Capability::RequestResync,
        Capability::Incremental,
        Capability::IncrementalUplink,
        Capability::FlowControl,
        Capability::X11,
        Capability::X11Recode,
        Capability::AudioForward,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Capability::RequestResync => "request_resync",
            Capability::Incremental => "incremental",
            Capability::IncrementalUplink => "incremental_uplink",
            Capability::FlowControl => "flow_control",
            Capability::X11 => "x11",
            Capability::X11Recode => "x11_recode",
            Capability::AudioForward => "audio_forward",
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

    /// Advertised list honoring the per-endpoint kill switches: recode,
    /// audio, uplink diffing and flow control ride by default (their default
    /// tiers are lossless / harmless), and an explicit `false` in `[runtime]`
    /// removes the name from the wire — a peer that disabled a capability can
    /// never be talked into it.
    pub fn advertised_for(runtime: &crate::config::RuntimeConfig) -> Vec<String> {
        Capability::SUPPORTED
            .iter()
            .filter(|c| match c {
                Capability::X11Recode => runtime.image_recode,
                Capability::AudioForward => runtime.audio_forward,
                Capability::FlowControl => runtime.flow_control,
                Capability::IncrementalUplink => runtime.incremental_uplink,
                _ => true,
            })
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
    fn recode_and_audio_advertise_by_default_and_hide_on_explicit_off() {
        use crate::config::RuntimeConfig;
        let def = Capability::advertised_for(&RuntimeConfig::default());
        assert!(def.iter().any(|c| c == "x11_recode"), "recode default-on");
        assert!(def.iter().any(|c| c == "audio_forward"), "audio default-on");
        assert!(
            def.iter().any(|c| c == "flow_control"),
            "flow control default-on"
        );
        // Kill switches are independent, and a peer that disabled a
        // capability can never be talked into it by the other side:
        let recode_off = Capability::advertised_for(&RuntimeConfig {
            image_recode: false,
            ..RuntimeConfig::default()
        });
        assert!(!recode_off.iter().any(|c| c == "x11_recode"));
        assert!(recode_off.iter().any(|c| c == "audio_forward"));
        let audio_off = Capability::advertised_for(&RuntimeConfig {
            audio_forward: false,
            ..RuntimeConfig::default()
        });
        assert!(audio_off.iter().any(|c| c == "x11_recode"));
        assert!(!audio_off.iter().any(|c| c == "audio_forward"));
        let flow_off = Capability::advertised_for(&RuntimeConfig {
            flow_control: false,
            ..RuntimeConfig::default()
        });
        assert!(!flow_off.iter().any(|c| c == "flow_control"));
        assert!(flow_off.iter().any(|c| c == "x11_recode"));
        let f = Features::negotiate(&def, &recode_off);
        assert!(!f.supports(Capability::X11Recode));
        assert!(f.supports(Capability::AudioForward));
        let f = Features::negotiate(&def, &flow_off);
        assert!(!f.supports(Capability::FlowControl));
        assert!(f.supports(Capability::X11Recode));
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
