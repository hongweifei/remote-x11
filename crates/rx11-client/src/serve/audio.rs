//! The client's audio dispatch: opening the outlets the relay's audio
//! channels ask for, feeding them their packets, and deciding which of
//! them are audible.
//!
//! The relay opens ONE audio channel per X11 connection of an
//! application, and the sound of that application is captured and encoded
//! once — only the transport repeats it. So the client holds several
//! channels at once, and each channel carries the tag of the X11
//! connection whose application produced the sound.
//!
//! Which of them are audible is the point of the whole feature: the
//! application the user is looking at is the one that should be heard.
//! The [`OutletTable`] holds that rule.

use tracing::{info, warn};

use rx11_core::protocol::Capability;
use rx11_core::types::ConnectionId;

use crate::audioout::start_audio_outlet;

use super::ServeCtx;

/// Open an audio channel the relay asked for.
///
/// The player is best-effort: a channel without a local player is
/// refused, and a failure never touches the X11 session.
pub(crate) fn open_audio_channel(
    ctx: &mut ServeCtx<'_>,
    connection_id: ConnectionId,
    app: Option<ConnectionId>,
) {
    if !ctx
        .proxy
        .dataplane
        .features
        .supports(Capability::AudioForward)
    {
        warn!("Relay opened audio without negotiated capability; ignoring");
        return;
    }
    match start_audio_outlet(connection_id, ctx.proxy.runtime) {
        Some(outlet) => {
            info!(
                "audio: playing to local sink ({}) for application {:?}",
                connection_id, app
            );
            ctx.audio.insert(outlet, app);
            // A new channel must obey the selection rule from the
            // start: only the foreground application's sound is heard.
            ctx.audio.refresh_audibility();
        }
        None => {
            warn!("audio: no local player available; channel ignored")
        }
    }
}

/// Feed one packet to the audio channel it belongs to.
///
/// Every audio channel is fed: the Opus decoder carries state across
/// packets, so a channel that skipped packets would sound wrong once it
/// became audible again. Which PLAYER is fed is decided by the
/// selection rule — the foreground application's sound only.
pub(crate) fn deliver_audio_packet(
    ctx: &mut ServeCtx<'_>,
    channel_id: ConnectionId,
    data: bytes::Bytes,
) {
    let alive = ctx.audio.deliver(channel_id, data);
    if !alive {
        warn!("audio: player gone; outlet removed");
        ctx.audio.remove(channel_id);
    }
}
