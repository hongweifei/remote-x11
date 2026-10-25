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

use rx11_core::audio::ApplicationId;
use rx11_core::config::RuntimeConfig;
use rx11_core::queue::OutboundSender;
use rx11_core::types::ConnectionId;

/// One audio channel: a subscriber's real-time lane, plus the X11
/// connection whose application this sound belongs to.
#[derive(Clone)]
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

/// The audio hub: the relay's single entry point for audio.
///
/// It owns both capture modes. Which mode is in force depends on the
/// runtime profile and on what the host supports; the decision happens
/// once, at attach time, and never changes for a running session.
#[derive(Clone)]
pub(crate) struct AudioHub {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: RuntimeConfig,
    /// The whole-desktop mix: the fallback and the explicit opt-out.
    mix: mix::MixHub,
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    /// The applications being captured. One capture and one encoder per
    /// application, shared by every X11 connection of that application.
    applications: Vec<per_app::ApplicationCapture>,
    /// Counts the capture streams started, so each capture stream gets a
    /// distinct name.
    sequences: u64,
    /// The clients subscribed to the per-application tier, each with the
    /// real-time lane their channels ride.
    clients: Vec<PerAppClient>,
}

/// A client subscribed to the per-application tier.
struct PerAppClient {
    /// The client's audio subscription id: the id of the channel the
    /// client first attached with. It identifies the client inside the
    /// hub.
    client_connection_id: ConnectionId,
    /// The client's real-time lane.
    outbound: OutboundSender,
}

impl AudioHub {
    pub(crate) fn new(runtime: RuntimeConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                mix: mix::MixHub::new(runtime.clone()),
                runtime,
                state: Mutex::new(HubState::default()),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.inner.state.lock().expect("audio hub state")
    }

    /// Whether the whole-desktop mix is in force.
    ///
    /// Three reasons send the session to the mix:
    ///
    /// - the runtime profile turned per-application audio off;
    /// - the host cannot capture per application (no `pw-cat` / `pw-link`
    ///   / `pw-dump`);
    /// - the operator set an explicit recorder command. That command is
    ///   the whole-desktop recorder — per-application capture needs a
    ///   graph lookup and a port link, which one argv cannot express — so
    ///   an override pins the session to the mix it names.
    pub(crate) fn mix_only(&self) -> bool {
        !self.inner.runtime.audio_per_app
            || self.inner.runtime.audio_record_command.is_some()
            || !rx11_core::audio::per_app_capture_available()
    }

    /// Subscribe a client to the whole-desktop mix.
    pub(crate) fn attach_mix(&self, outbound: OutboundSender, connection_id: ConnectionId) {
        self.inner.mix.attach(outbound, connection_id);
    }

    /// Subscribe a client to the per-application tier.
    ///
    /// The client's real-time lane is registered. The application whose
    /// sound it will carry is decided when an X11 connection of that
    /// application appears: the audio channel's id and its application
    /// label both come from that X11 connection.
    pub(crate) fn attach_per_app(&self, outbound: OutboundSender, connection_id: ConnectionId) {
        let mut st = self.state();
        st.clients.push(PerAppClient {
            client_connection_id: connection_id,
            outbound,
        });
    }

    /// The client's real-time lane, for the per-application tier.
    ///
    /// `None` when the client never subscribed (it did not negotiate the
    /// audio capability).
    pub(crate) fn client_lane(&self, client_connection_id: ConnectionId) -> Option<OutboundSender> {
        let st = self.state();
        st.clients
            .iter()
            .find(|c| c.client_connection_id == client_connection_id)
            .map(|c| c.outbound.clone())
    }

    /// Give an X11 connection an audio channel on its application's sound.
    ///
    /// The application's capture starts if it is not running yet. The
    /// channel's id is the AUDIO channel's own id — a different namespace
    /// from the X11 connection ids, because the client routes audio frames
    /// by the channel id and matches the foreground application by the X11
    /// connection id.
    pub(crate) fn attach_connection(
        &self,
        app: ApplicationId,
        app_connection_id: ConnectionId,
        outbound: OutboundSender,
    ) {
        let mut st = self.state();
        if let Some(capture) = st.applications.iter_mut().find(|c| c.app == app) {
            // The application is already being captured: this connection
            // just joins the sound that is already flowing.
            capture.add_channel(app_connection_id, outbound);
            return;
        }
        let sequence = st.sequences;
        st.sequences += 1;
        let mut capture = per_app::ApplicationCapture::new(app, sequence);
        capture.add_channel(app_connection_id, outbound);
        capture.start(&self.inner.runtime);
        st.applications.push(capture);
    }

    /// Close the audio channel of an X11 connection that went away.
    ///
    /// When the application's last channel goes, its capture stops — the
    /// same "the last listener left" rule the mix mode follows.
    pub(crate) fn detach_connection(&self, app: ApplicationId, app_connection_id: ConnectionId) {
        let mut st = self.state();
        let Some(index) = st.applications.iter().position(|c| c.app == app) else {
            return;
        };
        st.applications[index].remove_channel(app_connection_id);
        if st.applications[index].is_empty() {
            let capture = st.applications.remove(index);
            capture.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rx11_core::audio::ProcessTree;
    use rx11_core::protocol::Frame;
    use rx11_core::queue::outbound_channel;

    /// This process's application root: a real, readable application, so
    /// the tests use the same identity the production path produces.
    fn own_app() -> ApplicationId {
        ProcessTree::of_self()
            .and_then(|tree| tree.application_root())
            .expect("this process has an application")
    }

    /// A real priority lane and its receiver: the tests read the frames
    /// the hub actually puts on the wire.
    fn lane() -> (OutboundSender, rx11_core::queue::OutboundReceiver) {
        outbound_channel(16, 64 * 1024)
    }

    #[tokio::test]
    async fn a_clients_lane_is_found_by_its_subscription_id() {
        let hub = AudioHub::new(RuntimeConfig::default());
        let (tx, _rx) = lane();
        let id = ConnectionId::new(77);
        hub.attach_per_app(tx.clone(), id);
        assert!(
            hub.client_lane(id).is_some(),
            "the subscription id finds the lane"
        );
        assert!(
            hub.client_lane(ConnectionId::new(78)).is_none(),
            "another id finds nothing"
        );
    }

    #[tokio::test]
    async fn one_application_serves_several_connections_on_one_capture() {
        let hub = AudioHub::new(RuntimeConfig::default());
        let (tx, _rx) = lane();
        let client = ConnectionId::new(77);
        hub.attach_per_app(tx.clone(), client);

        let application = own_app();
        // Two X11 connections of the same application.
        let first = ConnectionId::new(11);
        let second = ConnectionId::new(12);
        hub.attach_connection(application, first, tx.clone());
        hub.attach_connection(application, second, tx.clone());

        // ONE capture for the application, shared by both connections.
        let captures = hub.state().applications.len();
        assert_eq!(captures, 1, "one capture per application, not per client");

        // The channels are per X11 connection, each with its own channel
        // id and its own application label.
        let channels = hub.state().applications[0].channel_ids();
        assert_eq!(channels.len(), 2, "one channel per X11 connection");
        assert_ne!(
            channels[0].audio_channel_id, channels[1].audio_channel_id,
            "the audio channels have their own ids"
        );
        assert_ne!(
            channels[0].audio_channel_id, first,
            "the audio channel id is not the X11 connection id"
        );
        assert_eq!(channels[0].app_connection_id, first);
        assert_eq!(channels[1].app_connection_id, second);
    }

    #[tokio::test]
    async fn the_first_packet_opens_every_channel_with_its_application_label() {
        let hub = AudioHub::new(RuntimeConfig::default());
        let (tx, mut rx) = lane();
        hub.attach_per_app(tx.clone(), ConnectionId::new(77));

        let application = own_app();
        let x11_connection = ConnectionId::new(11);
        hub.attach_connection(application, x11_connection, tx.clone());

        // The fan-out with a real packet: the channel opens with the
        // application label, then the data follows on the same channel.
        let capture = &mut hub.state().applications[0];
        capture.fan_out(7, &[1, 2, 3]);

        let opened = rx.try_recv_priority().expect("the channel opens");
        let Frame::ChannelOpen(open) = opened else {
            panic!("the first frame opens the channel");
        };
        assert_eq!(open.kind, rx11_core::protocol::ChannelKind::audio());
        assert_eq!(
            open.app,
            Some(x11_connection),
            "the channel carries the X11 connection it speaks for"
        );
        assert_eq!(
            open.connection_id,
            capture.channel_ids()[0].audio_channel_id,
            "the data follows the channel the open named"
        );

        let data = rx.try_recv_priority().expect("the packet follows");
        let Frame::StreamData(payload) = data else {
            panic!("the second frame carries the packet");
        };
        assert_eq!(payload.connection_id, open.connection_id);
        assert_eq!(payload.sequence_id, 7);
        assert_eq!(payload.data.as_ref(), &[1, 2, 3]);
    }

    #[tokio::test]
    async fn detaching_the_last_connection_stops_the_capture() {
        let hub = AudioHub::new(RuntimeConfig::default());
        let (tx, _rx) = lane();
        hub.attach_per_app(tx.clone(), ConnectionId::new(77));

        let application = own_app();
        let first = ConnectionId::new(11);
        let second = ConnectionId::new(12);
        hub.attach_connection(application, first, tx.clone());
        hub.attach_connection(application, second, tx.clone());
        assert_eq!(hub.state().applications.len(), 1);

        // One connection gone: the application keeps its capture.
        hub.detach_connection(application, first);
        assert_eq!(
            hub.state().applications.len(),
            1,
            "the application still has a connection"
        );

        // The last one gone: the capture stops.
        hub.detach_connection(application, second);
        assert!(
            hub.state().applications.is_empty(),
            "the application's capture stopped with its last connection"
        );
    }

    #[tokio::test]
    async fn the_mix_is_in_force_when_per_application_audio_is_off() {
        let runtime = RuntimeConfig {
            audio_per_app: false,
            ..RuntimeConfig::default()
        };
        let hub = AudioHub::new(runtime);
        assert!(
            hub.mix_only(),
            "the runtime profile turned per-application audio off"
        );

        let runtime = RuntimeConfig::default();
        let hub = AudioHub::new(runtime);
        assert!(
            !hub.mix_only(),
            "on a host with the per-application tools, the tier is available"
        );
    }

    #[tokio::test]
    async fn an_explicit_recorder_command_pins_the_session_to_the_mix() {
        let runtime = RuntimeConfig {
            audio_record_command: Some(vec!["cat".into(), "pcm".into()]),
            ..RuntimeConfig::default()
        };
        let hub = AudioHub::new(runtime);
        assert!(
            hub.mix_only(),
            "an explicit recorder names the whole-desktop recorder"
        );
    }
}
