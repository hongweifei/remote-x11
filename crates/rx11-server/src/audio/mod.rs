//! Audio capture and fan-out for the whole relay.
//!
//! Two capture modes, one fan-out surface:
//!
//! - **Whole-desktop mix** ([`mix`]): the legacy mode. ONE recorder child
//!   and ONE Opus encoder for the whole server; every subscriber gets the
//!   same stream. Used when the host cannot attribute sound to
//!   applications, and whenever `audio_per_app` is off.
//! - **Per application** ([`per_app`]): ONE recorder child and ONE Opus
//!   encoder PER APPLICATION, not per connection. An application's sound is
//!   captured from that application's own PipeWire stream, so a client can
//!   play the application it renders instead of the whole desktop.
//!
//! The channel topology is deliberately finer than the capture topology:
//! an application may hold several X11 connections (a browser's renderer
//!   processes each open one), and the client decides which application is
//!   in the foreground by X11 connection. So an application's captured and
//!   encoded sound is shipped on ONE audio channel PER X11 connection of
//!   that application, each channel tagged with the X11 connection it
//!   belongs to. The sound is encoded once and shared by all of them; only
//!   the transport repeats it.
//!
//! Capture is shared, never per client: N clients cost ONE recorder and
//! ONE encoder, exactly as the mix mode always did. A slow client sheds
//! packets on its own lane and never couples to the others.
//!
//! Failure policy: audio never takes an X11 session down. A host without
//! the per-application tools degrades to the whole-desktop mix with a
//! warning; a recorder that fails to start leaves the channel silent and
//! says so. Nothing here ever fabricates audio.

pub mod mix;
pub mod per_app;

use std::sync::{Arc, Mutex};

use rx11_core::config::RuntimeConfig;
use rx11_core::queue::OutboundSender;
use rx11_core::types::ConnectionId;

pub(crate) use mix::MixHub;

/// One application's captured sound, shared by all of its channels.
pub(crate) struct ApplicationStream {
    /// The application's identity (its process tree, once the core
    /// attribution API lands). Two X11 connections of the same application
    /// resolve to the same identity.
    pub(crate) app: AppIdentity,
    /// The audio channel each of the application's X11 connections listens
    /// on. One entry per X11 connection; every entry carries the same
    /// sound.
    pub(crate) channels: Vec<Channel>,
}

/// The identity of an application, used to decide which X11 connections
/// belong together.
///
/// The shape is deliberately opaque to this module: the attribution layer
/// ([`per_app`]) owns what an identity IS (today: a process tree), and
/// only asks "are these two the same application?" through it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AppIdentity(u64);

impl AppIdentity {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn raw(&self) -> u64 {
        self.0
    }
}

/// One audio channel: a subscriber's real-time lane, plus the X11
/// connection whose application this sound belongs to.
pub(crate) struct Channel {
    /// The audio channel's own id. A DIFFERENT namespace from the X11
    /// connection ids: the client routes audio frames by this id.
    pub(crate) audio_channel_id: ConnectionId,
    /// The X11 connection whose application this sound comes from. This
    /// is what the client matches against its foreground application.
    pub(crate) app_connection_id: ConnectionId,
    /// The subscriber's real-time lane.
    pub(crate) outbound: OutboundSender,
    /// Whether `ChannelOpen` already went out on this lane: only opened
    /// channels may see data, and only opened channels need a
    /// `ChannelClose` on teardown.
    pub(crate) opened: bool,
    /// Packets shed because this subscriber's link fell behind.
    pub(crate) shed: u64,
}

/// The subscriber set of one application: the X11 connections that want
/// this application's sound.
///
/// A subscriber is a RELAY CONNECTION (a client), not an X11 connection:
/// the X11 connections live on the relay side, the clients live on the
/// wire. One client may hold several X11 connections (it renders several
/// remote applications), so one client holds one entry per application it
/// renders.
pub(crate) struct Subscriber {
    /// The relay connection this subscriber belongs to. A client that
    /// renders two applications gets two entries, one per application.
    pub(crate) client: ClientId,
    /// The client's real-time lane.
    pub(crate) outbound: OutboundSender,
    /// The audio channels this subscriber listens on, one per X11
    /// connection of the application.
    pub(crate) channels: Vec<Channel>,
}

/// Identifies one relay connection (one connected client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ClientId(pub(crate) u64);

/// The audio hub: the relay's single entry point for audio.
///
/// It owns both capture modes and hands out subscriptions. Which mode is
/// in force depends on the runtime profile and on what the host supports;
/// the mode decision happens once, at attach time, and never changes for
/// a running session.
#[derive(Clone)]
pub(crate) struct AudioHub(Arc<Inner>);

struct Inner {
    runtime: RuntimeConfig,
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    /// The per-application streams currently captured. Keyed by the
    /// application's identity: one capture and one encoder per
    /// application, shared by every X11 connection of that application.
    pub(crate) applications: Vec<ApplicationStream>,
    /// The whole-desktop mix subscribers. Empty in per-application mode.
    pub(crate) mix_subscribers: Vec<Subscriber>,
}

impl AudioHub {
    pub(crate) fn new(runtime: RuntimeConfig) -> Self {
        Self(Arc::new(Inner {
            runtime,
            state: Mutex::new(HubState::default()),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.0.state.lock().expect("audio hub state")
    }
}
