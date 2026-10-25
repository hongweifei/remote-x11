//! The client-side relay receive loop: dispatches frames arriving from the
//! relay server to the local X server proxy and housekeeping handlers.
//!
//! All four data-frame shapes share the [`DataPlane`] inbound chain (decode,
//! diff-apply, stats); this module owns routing, the resync handshake and
//! the client side of flow control: downlink gauge edges are advertised on
//! the priority lane, and the relay's uplink pauses flip the per-channel
//! egress gates the proxy pumps await.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Interval;
use tracing::{debug, error, info, warn};

use rx11_core::config::FlowDefaults;
use rx11_core::dataplane::{DataPlane, InboundOutcome};
use rx11_core::flow::{Advertiser, FlowEdge};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{
    Capability, FlowControlAction, Frame, SessionDestroyMessage, StreamResyncRequestMessage,
};
use rx11_core::transport::Rx11TransportReadHalf;
use rx11_core::types::{ConnectionId, DisplayNumber};

use crate::audioout::OutletTable;
use crate::proxy::{send_data_to_local, spawn_local_connection, ProxyDeps};

mod audio;

/// Client side of the flow-control brain. The serve loop is its only owner;
/// the proxy pump reports resume edges through `edges` instead of claiming
/// the advertiser itself, and the ticker reconciles everything the priority
/// lane may have dropped.
pub(crate) struct ServeFlow {
    negotiated: bool,
    advertiser: Advertiser,
    edges: mpsc::Receiver<FlowEdge>,
    ticker: Interval,
}

impl ServeFlow {
    pub(crate) fn new(
        features: &rx11_core::protocol::Features,
        edges: mpsc::Receiver<FlowEdge>,
    ) -> Self {
        let mut ticker = tokio::time::interval(FlowDefaults::ADVERTISE_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            negotiated: features.supports(Capability::FlowControl),
            advertiser: Advertiser::default(),
            edges,
            ticker,
        }
    }
}

/// Everything the receive loop touches, borrowed from the connector scope.
pub(crate) struct ServeCtx<'a> {
    pub read_half: &'a mut Rx11TransportReadHalf,
    pub proxy: ProxyDeps<'a>,
    pub incremental_cache: &'a mut ConnectionDataCache,
    pub display: DisplayNumber,
    pub read_timeout: Duration,
    /// The open audio channels, and the rule that decides which of them
    /// are audible. The relay opens one audio channel per X11 connection
    /// of an application; each carries the tag of the X11 connection whose
    /// application produces the sound. Data for an audio channel bypasses
    /// the dataplane entirely.
    pub audio: &'a mut OutletTable,
    /// The resource ranges of the application connections, and the
    /// authorization the local X server expects: together they let the
    /// client ask the server which window has the focus, and hence which
    /// application the user is looking at.
    pub ranges: &'a Arc<tokio::sync::Mutex<crate::audioout::ResourceIndex>>,
    pub authorization: &'a Arc<tokio::sync::Mutex<crate::audioout::Authorization>>,
    /// The resident monitor connection to the local X server. It is asked
    /// about the focus every tick and kept between the questions.
    pub monitor: &'a mut crate::audioout::ResidentMonitor,
    /// How often the focus of the local X server is checked.
    pub focus_interval: Duration,
    pub flow: &'a mut ServeFlow,
}

impl ServeCtx<'_> {
    fn dataplane(&self) -> &DataPlane {
        self.proxy.dataplane
    }
}

pub(crate) async fn serve(ctx: &mut ServeCtx<'_>) -> anyhow::Result<()> {
    let mut focus_ticker = tokio::time::interval(ctx.focus_interval);
    focus_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately: the foreground is worth
    // knowing before the first audio packet arrives.
    focus_ticker.tick().await;
    loop {
        tokio::select! {
            _ = focus_ticker.tick() => {
                update_focus(ctx).await;
            }
            frame_result = tokio::time::timeout(
                ctx.read_timeout,
                ctx.read_half.recv_frame()
            ) => {
                match frame_result {
                    Ok(Ok(frame)) => dispatch_frame(ctx, frame).await?,
                    Ok(Err(e)) => {
                        error!("Connection error: {}", e);
                        return Err(e.into());
                    }
                    Err(_) => {
                        error!("Read timeout ({:?}), no data from relay", ctx.read_timeout);
                        return Err(rx11_core::error::Rx11Error::Timeout.into());
                    }
                }
            }
            edge = ctx.flow.edges.recv() => {
                // Edge senders live as long as the pumps; a closed
                // channel means teardown, nothing to drain.
                let Some(edge) = edge else { continue };
                if ctx.flow.negotiated {
                    advertise_edge(ctx, edge).await;
                }
            }
            _ = ctx.flow.ticker.tick() => {
                re_advertise(ctx).await;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal, sending SessionDestroy...");
                let _ = ctx
                    .proxy
                    .outbound
                    .send(Frame::SessionDestroy(SessionDestroyMessage {
                        display: ctx.display,
                    }))
                    .await;
                break;
            }
        }
    }
    Ok(())
}

/// Ask the local X server which window has the focus, and tell the audio
/// table which application that is.
///
/// The focus is the server's global state, so it is asked on a monitor
/// connection of the client's own — never on an application connection,
/// whose request counter would be thrown off. When the question cannot be
/// answered, the foreground is unknown and every application is audible:
/// the desktop-mix behaviour, which is always better than silence.
async fn update_focus(ctx: &mut ServeCtx<'_>) {
    // The authorization the local X server wants is learned from the
    // application's setup request; the monitor presents the same one.
    let authorization = ctx.authorization.lock().await.clone();
    ctx.monitor.set_authorization(authorization);
    let focus = ctx.monitor.focus_window().await;
    let foreground = match focus {
        Some(focus) => {
            let ranges = ctx.ranges.lock().await;
            crate::audioout::foreground_connection(focus, &ranges)
        }
        None => None,
    };
    if foreground != ctx.audio.foreground() {
        info!(
            "audio: foreground application is now {:?} ({} of {} channels audible)",
            foreground.map(|c| c.get()),
            ctx.audio.audible().len(),
            ctx.audio.len(),
        );
        ctx.audio.set_foreground(foreground);
    }
}

/// Put one flow edge on the wire via the real-time lane: pauses must not
/// queue behind a redraw burst (the bulk budget would defeat the purpose
/// of pausing at all), and the lane's drop surface is covered by the
/// ticker's keep-alive.
fn send_flow_frame(ctx: &ServeCtx<'_>, edge: FlowEdge) {
    let frame = Frame::FlowControl(rx11_core::protocol::FlowControlMessage {
        action: edge.action,
        connection_id: Some(edge.connection_id),
    });
    let _ = ctx.proxy.outbound.send_priority(frame);
}

async fn advertise_edge(ctx: &mut ServeCtx<'_>, edge: FlowEdge) {
    if ctx.flow.advertiser.record(edge) {
        if edge.action == FlowControlAction::Pause {
            ctx.proxy.stats.add_flow_advertised();
        }
        info!(
            "flow: advertising {:?} for {}",
            edge.action, edge.connection_id
        );
        send_flow_frame(ctx, edge);
    }
}

/// Re-state held advertisements (drops are survivable) and reconcile: a
/// resume edge reported by the pump can be lost on the (bounded, small)
/// report channel; the gauge itself is ground truth.
async fn re_advertise(ctx: &mut ServeCtx<'_>) {
    if !ctx.flow.negotiated {
        return;
    }
    let stale: Vec<ConnectionId> = {
        let conns = ctx.proxy.x11_connections.lock().await;
        ctx.flow
            .advertiser
            .paused_ids()
            .filter(|id| {
                conns
                    .get(&id.get())
                    .is_some_and(|(_, _, meter, _, _)| !meter.latched())
            })
            .collect()
    };
    for id in stale {
        advertise_edge(ctx, FlowEdge::resume(id)).await;
    }
    for edge in ctx.flow.advertiser.due() {
        send_flow_frame(ctx, edge);
    }
}

async fn dispatch_frame(ctx: &mut ServeCtx<'_>, frame: Frame) -> anyhow::Result<()> {
    match frame {
        Frame::ChannelOpen(msg) => {
            // Channel-kind routing: each openable kind gets a branch here.
            // Unknown/unimplemented kinds are refused explicitly so the peer
            // never waits on a channel that will stay silent.
            if msg.kind.is_x11() {
                info!(
                    "X11 client connected ({}) display={:?}",
                    msg.connection_id, msg.display
                );
                ctx.proxy.stats.inc_x11_connections();
                spawn_local_connection(msg.connection_id, &ctx.proxy).await?;
            } else if msg.kind.is_audio() {
                // Audio channel: gated by the negotiated capability. The
                // relay opens one channel per X11 connection of an
                // application, each tagged with the connection whose
                // application produces the sound. Starting the player is
                // best-effort; audio failures never touch the X11 session.
                audio::open_audio_channel(ctx, msg.connection_id, msg.app);
            } else {
                warn!(
                    "Relay opened unsupported channel kind '{}' ({}); ignoring",
                    msg.kind, msg.connection_id
                );
            }
        }
        frame @ (Frame::StreamData(_)
        | Frame::CompressedStreamData(_)
        | Frame::IncrementalStreamData(_)
        | Frame::CompressedIncrementalStreamData(_)) => {
            // Real-time bypass: frames on an open audio channel go
            // straight to the local player — the diff cache, compression
            // chain and X connection routing never see them.
            if let Frame::StreamData(msg) = &frame {
                if ctx.audio.owns(msg.connection_id) {
                    // EVERY audio channel is fed: the Opus decoder carries
                    // state across packets, so a channel that skipped
                    // packets would sound wrong once it became audible
                    // again. Which PLAYER is fed is decided by the
                    // selection rule — the foreground application's sound
                    // only.
                    audio::deliver_audio_packet(ctx, msg.connection_id, msg.data.clone());
                    return Ok(());
                }
            }
            // Copy the (small, Copy) policy so the cache borrow below stays
            // disjoint from the dataplane read.
            let dataplane = ctx.proxy.dataplane.clone();
            let outcome = dataplane.apply_inbound(frame, ctx.incremental_cache, ctx.proxy.stats);
            match outcome {
                InboundOutcome::Payload {
                    connection_id,
                    data,
                } => {
                    // The local write-back gauge charges on acceptance; the
                    // pause edge goes to the relay immediately on the
                    // priority lane. Enforcement (stopping the app-reading
                    // pump) happens there, losslessly.
                    if let Some((_, pause_edge)) =
                        send_data_to_local(ctx.proxy.x11_connections, connection_id, data).await
                    {
                        if pause_edge && ctx.flow.negotiated {
                            advertise_edge(ctx, FlowEdge::pause(connection_id)).await;
                        }
                    }
                }
                InboundOutcome::ResyncRequired { connection_id } => {
                    request_full_resend(ctx, connection_id).await;
                }
                InboundOutcome::StreamBroken => {
                    // Bytes were lost on the way in (or the compression
                    // stream desynced): the local X server would receive a
                    // corrupted byte stream. Fail the session so the
                    // reconnect-with-resume path rebuilds clean state.
                    return Err(rx11_core::error::Rx11Error::Protocol(
                        "compression stream broken; forcing reconnect".into(),
                    )
                    .into());
                }
                InboundOutcome::Dropped => {}
            }
        }
        frame => on_other_frame(ctx, frame).await,
    }
    Ok(())
}

/// Ask the relay to stop diffing this connection and resend full frames:
/// our receive cache is out of sync, so every further diff would fail.
async fn request_full_resend(ctx: &mut ServeCtx<'_>, conn_id: ConnectionId) {
    if !ctx.dataplane().features.supports(Capability::RequestResync) {
        warn!(
            "Diff desync for {} but resync capability not negotiated; frame dropped",
            conn_id
        );
        return;
    }
    ctx.proxy.stats.add_incremental_full_fallback();
    // Control signal: rides the priority lane so it never queues behind the
    // redraw burst it is trying to stop. A full-lane drop is harmless — the
    // next failed diff re-asks.
    if ctx
        .proxy
        .outbound
        .send_priority(Frame::StreamResyncRequest(StreamResyncRequestMessage {
            connection_id: conn_id,
        }))
        .is_err()
    {
        warn!(
            "Resync request for {conn_id} lost on a full priority lane; relying on the next diff"
        );
    }
}

async fn on_other_frame(ctx: &mut ServeCtx<'_>, frame: Frame) {
    match frame {
        Frame::Heartbeat => {
            let _ = ctx.proxy.outbound.send(Frame::HeartbeatAck).await;
        }
        Frame::SessionDestroy(msg) => {
            info!("Session destroyed for display {}", msg.display);
        }
        Frame::ChannelClose(msg) => {
            if ctx.audio.owns(msg.connection_id) {
                info!("audio channel ({}) closed by relay", msg.connection_id);
                // Dropping the outlet closes its buffer: the player task
                // sees EOF, flushes and reaps the child (kill_on_drop).
                ctx.audio.remove(msg.connection_id);
            } else {
                debug!("Remote X11 client disconnected ({})", msg.connection_id);
                let mut conns = ctx.proxy.x11_connections.lock().await;
                conns.remove(&msg.connection_id.get());
                ctx.incremental_cache.clear_connection(msg.connection_id);
                ctx.flow.advertiser.forget(msg.connection_id);
            }
        }
        Frame::StreamResyncRequest(msg) => {
            // The relay could not apply one of OUR uplink diffs. The send
            // base belongs to this channel's pump task, so we raise its
            // reset flag instead of reaching into a shared cache: the pump
            // drops the base before building the next frame, and this
            // payload ships whole from there on.
            if !ctx.dataplane().features.supports(Capability::RequestResync) {
                warn!(
                    "Relay sent StreamResyncRequest but resync capability was not negotiated; ignoring"
                );
                return;
            }
            let raised = {
                let conns = ctx.proxy.x11_connections.lock().await;
                conns
                    .get(&msg.connection_id.get())
                    .map(|(_, _, _, _, uplink_reset)| {
                        uplink_reset.store(true, std::sync::atomic::Ordering::Release);
                    })
                    .is_some()
            };
            if raised {
                info!(
                    "Relay requested full resend for {}; uplink diff base will reset",
                    msg.connection_id
                );
            } else {
                debug!(
                    "Resync request for unknown channel {}; nothing to reset",
                    msg.connection_id
                );
            }
        }
        Frame::Error(msg) => {
            error!("Error from relay (code={}): {}", msg.code, msg.message);
        }
        Frame::FlowControl(msg) => {
            if !ctx.flow.negotiated {
                warn!("FlowControl from relay without negotiated capability; ignoring");
                return;
            }
            let paused = matches!(msg.action, FlowControlAction::Pause);
            let ids: Vec<ConnectionId> = match msg.connection_id {
                Some(id) => vec![id],
                None => {
                    // Scope-less form: every live x11 channel. Audio is
                    // exempt by design — it rides the real-time lane.
                    ctx.proxy
                        .x11_connections
                        .lock()
                        .await
                        .keys()
                        .copied()
                        .map(ConnectionId::new)
                        .collect()
                }
            };
            for id in ids {
                let gate = {
                    let conns = ctx.proxy.x11_connections.lock().await;
                    conns.get(&id.get()).map(|(_, _, _, gate, _)| gate.clone())
                };
                match gate {
                    Some(gate) => {
                        if gate.set(paused) {
                            if paused {
                                ctx.proxy.stats.add_flow_enforced();
                            }
                            info!("flow: relay {:?} uplink for {}", msg.action, id);
                        }
                        // A `Pause` racing ahead of its `ChannelOpen` (the
                        // priority lane overtakes bulk) is not lost: our
                        // advertiser keeps re-stating held pauses every
                        // tick, so the gate flips once the channel exists.
                    }
                    None => {
                        debug!("FlowControl for unknown channel {}; ignored", id);
                    }
                }
            }
        }
        _ => {
            warn!("Unexpected frame from relay: {:?}", frame.msg_type());
        }
    }
}

#[cfg(test)]
mod tests;
