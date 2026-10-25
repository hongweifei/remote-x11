//! The client's audio outlets: one per open audio channel, and the rule
//! that decides which of them are audible.
//!
//! The relay ships ONE audio channel per X11 connection of an application
//! (the application itself is captured and encoded once; only the
//! transport repeats the sound). Each of those channels carries the tag of
//! the X11 connection whose application produces the sound — the
//! `app` field of the open frame.
//!
//! Which of them are audible is the whole point of the feature: the
//! application the user is looking at is the one that should be heard.
//! The tag is the X11 connection id, so the choice is "the channel whose
//! tag is the foreground connection".
//!
//! Two cases the table must get right:
//!
//! * One application may hold SEVERAL X11 connections, each with its own
//!   audio channel, and the relay sends THE SAME encoded sound on all of
//!   them. The client cannot tell that two tags name the same
//!   application — it only ever sees connection ids — so it must not play
//!   two of them: that would play the same sound twice, at double the
//!   volume. Hence exactly ONE channel per audible application.
//! * The whole-desktop mix channel carries NO tag. In per-application mode
//!   it must stay silent: playing it would mix the whole desktop into
//!   the foreground application's sound, which is exactly what the
//!   per-application mode exists to prevent. It is audible only when the
//!   client has fallen back to the desktop mix.

use std::collections::HashMap;

use rx11_core::types::ConnectionId;

use super::AudioOutlet;

/// The open audio channels and the selection rule.
#[derive(Default)]
pub(crate) struct OutletTable {
    /// The outlets, by their own channel id.
    outlets: HashMap<u32, AudioOutlet>,
    /// The X11 connection each channel's sound belongs to. A channel
    /// without a tag carries the whole-desktop mix.
    app_of: HashMap<u32, ConnectionId>,
    /// The X11 connection whose application is in the foreground. `None`
    /// when the foreground cannot be determined — then every application
    /// is audible (the desktop-mix behaviour).
    foreground: Option<ConnectionId>,
    /// Whether the relay is shipping per-application channels. When it is
    /// not, the channels carry the desktop mix and are all audible.
    per_application: bool,
}

impl OutletTable {
    /// A table for a session that negotiated the per-application tier.
    pub(crate) fn new(per_application: bool) -> Self {
        Self {
            outlets: HashMap::new(),
            app_of: HashMap::new(),
            foreground: None,
            per_application,
        }
    }

    /// Whether the relay ships per-application channels.
    ///
    /// It is decided by the negotiated capability, which is known before
    /// the first channel arrives; the setter exists for the tests, whose
    /// harness changes the negotiated features after the table is built.
    #[cfg(test)]
    pub(crate) fn set_per_application(&mut self, per_application: bool) {
        self.per_application = per_application;
    }

    /// Register an open audio channel.
    ///
    /// A channel that is already known is refused: the relay does not open
    /// the same channel twice, and a duplicate would leak a player
    /// process.
    pub(crate) fn insert(&mut self, outlet: AudioOutlet, app: Option<ConnectionId>) {
        let id = outlet.conn_id.get();
        if self.outlets.contains_key(&id) {
            return;
        }
        if let Some(app) = app {
            self.app_of.insert(id, app);
        }
        self.outlets.insert(id, outlet);
    }

    /// Forget a closed channel.
    pub(crate) fn remove(&mut self, channel_id: ConnectionId) {
        self.outlets.remove(&channel_id.get());
        self.app_of.remove(&channel_id.get());
    }

    /// The number of open channels (diagnostics/tests).
    pub(crate) fn len(&self) -> usize {
        self.outlets.len()
    }

    /// Record the X11 connection whose application is in the foreground.
    ///
    /// `None` means the foreground could not be determined; every
    /// application is audible then.
    ///
    /// The players are re-selected right away: the application that just
    /// came to the front must start being heard, and the one that lost the
    /// focus must stop.
    pub(crate) fn set_foreground(&mut self, connection: Option<ConnectionId>) {
        self.foreground = connection;
        self.refresh_audibility();
    }

    /// The foreground connection, if known.
    pub(crate) fn foreground(&self) -> Option<ConnectionId> {
        self.foreground
    }

    /// Whether the channel with this id must be audible right now.
    ///
    /// The rule:
    ///
    /// * not per-application mode ⇒ every channel is audible (the relay
    ///   ships the desktop mix);
    /// * a channel without an application tag ⇒ the desktop mix: audible
    ///   only when the foreground is unknown, because in per-application
    ///   mode it would be the whole desktop playing over the foreground
    ///   application;
    /// * the foreground is known ⇒ the channel's application must BE the
    ///   foreground;
    /// * the foreground is unknown ⇒ the channel is audible only if it is
    ///   the FIRST channel carrying its application tag. The relay sends
    ///   the same sound on every channel of an application, and the client
    ///   cannot tell that two tags name one application, so playing more
    ///   than one would play the same sound twice at double the volume.
    ///   The first channel of an application is the lowest channel id
    ///   carrying that tag — a deterministic choice that needs no extra
    ///   information.
    pub(crate) fn is_audible(&self, channel_id: ConnectionId) -> bool {
        if !self.per_application {
            return true;
        }
        // A channel that is not open is not audible — its player is gone.
        if !self.outlets.contains_key(&channel_id.get()) {
            return false;
        }
        let Some(app) = self.app_of.get(&channel_id.get()) else {
            // The desktop mix: only when the foreground is unknown.
            return self.foreground.is_none();
        };
        // The foreground decides WHICH APPLICATION is heard; the first
        // channel of that application decides WHICH of its channels is.
        // The relay sends the same sound on every channel of an
        // application, and the client cannot tell that two tags name one
        // application — so playing two of them would play the same sound
        // twice, at double the volume.
        match self.foreground {
            Some(foreground) => {
                *app == foreground && self.is_first_channel_of_app(*app, channel_id)
            }
            None => self.is_first_channel_of_app(*app, channel_id),
        }
    }

    /// Whether this channel is the lowest-numbered channel carrying the
    /// given application tag.
    fn is_first_channel_of_app(&self, app: ConnectionId, channel_id: ConnectionId) -> bool {
        self.app_of
            .iter()
            .filter(|(_, tagged)| **tagged == app)
            .map(|(id, _)| *id)
            .min()
            .is_some_and(|lowest| lowest == channel_id.get())
    }

    /// The channels that must be audible right now.
    pub(crate) fn audible(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .outlets
            .keys()
            .copied()
            .filter(|id| self.is_audible(ConnectionId::new(*id)))
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Whether the table holds a channel with this id.
    pub(crate) fn owns(&self, channel_id: ConnectionId) -> bool {
        self.outlets.contains_key(&channel_id.get())
    }

    /// Deliver one packet to its channel. Returns false when the channel
    /// is gone (the caller removes it).
    pub(crate) fn deliver(&mut self, channel_id: ConnectionId, packet: bytes::Bytes) -> bool {
        let Some(outlet) = self.outlets.get(&channel_id.get()) else {
            return false;
        };
        outlet.deliver(packet)
    }

    /// Apply the selection rule to the players: the audible channels are
    /// fed, the others are muted.
    ///
    /// Muting the PLAYER rather than dropping the packets is deliberate:
    /// the Opus decoder carries state across packets, so a channel that
    /// skipped packets would sound wrong once it became audible again.
    pub(crate) fn refresh_audibility(&self) {
        for (id, outlet) in self.outlets.iter() {
            outlet.set_audible(self.is_audible(ConnectionId::new(*id)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rx11_core::audio::Packetizer;
    use rx11_core::audio::DEFAULT_BITRATE_KBPS;

    async fn outlet(id: u32) -> AudioOutlet {
        let (outlet, _rx) = AudioOutlet::for_test(ConnectionId::new(id));
        outlet
    }

    #[tokio::test]
    async fn a_closed_channel_is_forgotten() {
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        assert_eq!(table.len(), 1);
        table.remove(ConnectionId::new(10));
        assert_eq!(table.len(), 0);
        assert!(!table.is_audible(ConnectionId::new(10)));
    }

    #[tokio::test]
    async fn the_same_channel_is_not_opened_twice() {
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(10).await, Some(ConnectionId::new(2)));
        assert_eq!(table.len(), 1, "a duplicate open is refused");
    }

    #[tokio::test]
    async fn only_the_foreground_application_is_audible() {
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(11).await, Some(ConnectionId::new(2)));
        table.set_foreground(Some(ConnectionId::new(2)));
        assert!(!table.is_audible(ConnectionId::new(10)));
        assert!(table.is_audible(ConnectionId::new(11)));
        assert_eq!(table.audible(), vec![11]);
    }

    #[tokio::test]
    async fn one_application_with_several_connections_plays_on_one_of_them_only() {
        // The relay sends THE SAME sound on every channel of an application,
        // and the client cannot tell that two tags name one application.
        // Playing two of them would play the same sound twice, at double
        // the volume.
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(11).await, Some(ConnectionId::new(1)));
        table.insert(outlet(12).await, Some(ConnectionId::new(2)));
        table.set_foreground(Some(ConnectionId::new(1)));
        assert_eq!(
            table.audible(),
            vec![10],
            "the foreground application is heard once, not twice"
        );
    }

    #[tokio::test]
    async fn an_unknown_foreground_plays_each_application_once() {
        // The fallback: the foreground could not be determined. Every
        // application must still be heard — but once each, so the volume
        // stays right.
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(11).await, Some(ConnectionId::new(1)));
        table.insert(outlet(12).await, Some(ConnectionId::new(2)));
        table.set_foreground(None);
        assert_eq!(
            table.audible(),
            vec![10, 12],
            "one channel per application, never two of the same"
        );
    }

    #[tokio::test]
    async fn the_desktop_mix_stays_silent_in_per_application_mode() {
        // The mix channel carries the whole desktop. Playing it over the
        // foreground application would be exactly what the per-application
        // mode exists to prevent.
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(99).await, None); // the desktop mix
        table.set_foreground(Some(ConnectionId::new(1)));
        assert!(table.is_audible(ConnectionId::new(10)));
        assert!(
            !table.is_audible(ConnectionId::new(99)),
            "the desktop mix must not play over an application"
        );
    }

    #[tokio::test]
    async fn an_unknown_foreground_plays_the_mix_too() {
        // With no foreground to single out, the desktop mix is what the
        // user gets.
        let mut table = OutletTable::new(true);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(11).await, Some(ConnectionId::new(2)));
        table.set_foreground(None);
        assert!(table.is_audible(ConnectionId::new(10)));
        assert!(table.is_audible(ConnectionId::new(11)));
        // The mix channel joins them: with no foreground, nothing is
        // being singled out, so the mix is what the user gets.
        table.insert(outlet(99).await, None);
        assert!(table.is_audible(ConnectionId::new(99)));
    }

    #[tokio::test]
    async fn without_the_per_application_tier_every_channel_plays() {
        let mut table = OutletTable::new(false);
        table.insert(outlet(10).await, Some(ConnectionId::new(1)));
        table.insert(outlet(99).await, None);
        table.set_foreground(Some(ConnectionId::new(1)));
        assert!(table.is_audible(ConnectionId::new(10)));
        assert!(
            table.is_audible(ConnectionId::new(99)),
            "without per-application channels the mix is the only sound"
        );
    }

    #[tokio::test]
    async fn delivering_reaches_the_channel_and_reports_a_gone_one() {
        let mut table = OutletTable::new(true);
        let (outlet, mut rx) = AudioOutlet::for_test(ConnectionId::new(10));
        table.insert(outlet, Some(ConnectionId::new(1)));
        let packet = bytes::Bytes::from_static(b"p");
        assert!(table.deliver(ConnectionId::new(10), packet.clone()));
        assert_eq!(rx.try_recv().ok(), Some(packet));
        assert!(!table.deliver(ConnectionId::new(77), bytes::Bytes::new()));
    }

    #[tokio::test]
    async fn an_opus_packet_survives_the_outlet() {
        // The outlet's path: a packet produced by the server's encoder
        // reaches the player process unharmed. The test outlet stops at
        // the packet boundary — decoding to PCM happens in the drain
        // task, which needs a real player child.
        let mut encoder = Packetizer::new(DEFAULT_BITRATE_KBPS).unwrap();
        let mut packet = None;
        encoder.push(&[0u8; 3840], |_: u32, pkt: &[u8]| {
            packet = Some(pkt.to_vec())
        });
        let packet = packet.expect("the encoder produced a packet");
        let (outlet, mut rx) = AudioOutlet::for_test(ConnectionId::new(10));
        assert!(outlet.deliver(bytes::Bytes::from(packet.clone())));
        let received = rx.try_recv().expect("a packet reached the outlet");
        assert_eq!(received.as_ref(), packet.as_slice());
    }

    #[tokio::test]
    async fn a_muted_channel_is_still_fed_but_its_player_is_silent() {
        // The decoder carries state across packets, so a muted channel
        // must keep receiving them; only its PLAYER is silent. Otherwise
        // the sound would be wrong once the channel became audible again.
        let (outlet, mut rx) = AudioOutlet::for_test(ConnectionId::new(10));
        outlet.set_audible(false);
        assert!(outlet.deliver(bytes::Bytes::from_static(b"p")));
        assert_eq!(
            rx.try_recv().ok(),
            Some(bytes::Bytes::from_static(b"p")),
            "a muted channel still receives its packets"
        );
    }
}
