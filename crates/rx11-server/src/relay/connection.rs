//! Per-client connection task: after the handshake, multiplexes frames
//! between the relay socket, the X11 listener events and the heartbeat.
//!
//! This task also owns the session's **flow-control brain**: it advertises
//! uplink pauses from the app-write gauges, enforces the client's downlink
//! pauses on the app-reading gates (with the yield rule that never lets
//! both legs of one channel gate at once), and keeps every advertisement
//! alive on the priority lane against that lane's drop surface.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Interval;
use tracing::{info, warn};

use rx11_core::config::{BufferDefaults, FlowDefaults, RuntimeConfig, ServerDefaults};
use rx11_core::dataplane::{DataPlane, DataPlaneSide, InboundOutcome};
use rx11_core::flow::{Advertiser, FlowEdge};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::*;
use rx11_core::queue::{outbound_channel, OutboundSender};
use rx11_core::stats::ConnectionStats;
use rx11_core::transport::{drain_frames, Rx11Transport, Rx11TransportReadHalf};
use rx11_core::types::{ConnectionId, DisplayNumber};

use crate::session::{Session, SessionManager, X11ConnToRelay};

pub(super) async fn handle_client(
    stream: TcpStream,
    addr: SocketAddr,
    session_mgr: Arc<SessionManager>,
    stats: Arc<ConnectionStats>,
    runtime: RuntimeConfig,
    audio: crate::audio::AudioHub,
    auth_token: &str,
) -> rx11_core::error::Result<()> {
    info!("Client connected from {}", addr);
    let mut transport = Rx11Transport::connect_tcp(stream, runtime.tcp_send_buffer_bytes)?;

    let handshake = server_handshake(
        &mut transport,
        auth_token,
        ServerDefaults::HANDSHAKE_TIMEOUT,
        &runtime,
    )
    .await?;
    let dataplane = DataPlane::new(
        DataPlaneSide::Relay,
        handshake.features,
        handshake.compression,
        &runtime,
    );

    if let Some(ref sid) = handshake.resume_session_id {
        info!(
            "Client {} requests session resume: {}",
            handshake.session_id, sid
        );
    }
    info!("Client {} authenticated successfully", handshake.session_id);

    let transport_id = handshake.session_id.into_inner();
    let (mut read_half, write_half) = transport.split();

    let (x11_event_tx, mut x11_event_rx) =
        mpsc::channel::<X11ConnToRelay>(BufferDefaults::CHANNEL_BUFFER);
    // Byte-budgeted WAN queue: producers block once `outbound_queue_bytes`
    // are in flight, so a slow link's latency stays bounded no matter how
    // fast the X server bursts.
    let (outbound_tx, outbound_rx) = outbound_channel(
        BufferDefaults::OUTBOUND_CHANNEL,
        runtime.outbound_queue_bytes,
    );

    let heartbeat_task = spawn_heartbeat(outbound_tx.clone(), runtime.heartbeat_interval);
    // Coalescing writer: batches already-queued frames into one syscall per
    // batch while keeping idle-queue latency at one frame.
    let sender_task = tokio::spawn(drain_frames(outbound_rx, write_half));

    // Two diff bases, one per direction: `incremental_cache` is what WE diff
    // against when sending the picture down; `uplink_cache` is what inbound
    // client diffs are applied against. They share nothing — on one
    // `connection_id` the request stream and the reply stream are different
    // byte streams at different offsets.
    let mut incremental_cache = ConnectionDataCache::from_runtime(&runtime);
    let mut uplink_cache = ConnectionDataCache::from_runtime(&runtime);
    let mut audio_attached = false;
    let audio_client_id = crate::x11_listener::next_connection_id();
    let mut flow = FlowState {
        negotiated: dataplane.features.supports(Capability::FlowControl),
        ..FlowState::default()
    };
    let mut flow_ticker = tokio::time::interval(FlowDefaults::ADVERTISE_TICK);
    flow_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut ctx = RelayContext {
        read_half: &mut read_half,
        outbound: &outbound_tx,
        x11_events: &mut x11_event_rx,
        session_mgr: &session_mgr,
        transport_id: &transport_id,
        dataplane,
        runtime: &runtime,
        heartbeat_deadline: tokio::time::Instant::now() + runtime.heartbeat_timeout,
        x11_event_tx: &x11_event_tx,
        incremental_cache: &mut incremental_cache,
        uplink_cache: &mut uplink_cache,
        stats: &stats,
        audio: &audio,
        audio_attached: &mut audio_attached,
        audio_client_id: &audio_client_id,
        flow: &mut flow,
        flow_ticker: &mut flow_ticker,
    };

    let result = relay_loop(&mut ctx).await;

    heartbeat_task.abort();
    // The audio subscription (if any) is swept by the shared recorder on
    // the next packet: every lane of this connection reports Closed.
    drop(outbound_tx);
    let _ = sender_task.await;

    session_mgr.release_session(&transport_id).await;
    info!("Client disconnected from {}", addr);
    result
}

fn spawn_heartbeat(
    outbound: OutboundSender,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval);
        timer.tick().await; // first tick completes immediately
        loop {
            timer.tick().await;
            if outbound.send(Frame::Heartbeat).await.is_err() {
                break;
            }
        }
    })
}

/// Everything the relay loop touches, borrowed from [`handle_client`]'s scope.
struct RelayContext<'a> {
    read_half: &'a mut Rx11TransportReadHalf,
    outbound: &'a OutboundSender,
    x11_events: &'a mut mpsc::Receiver<X11ConnToRelay>,
    session_mgr: &'a Arc<SessionManager>,
    transport_id: &'a str,
    dataplane: DataPlane,
    runtime: &'a RuntimeConfig,
    heartbeat_deadline: tokio::time::Instant,
    x11_event_tx: &'a mpsc::Sender<X11ConnToRelay>,
    incremental_cache: &'a mut ConnectionDataCache,
    /// Receive-side diff base for the CLIENT uplink (application requests →
    /// relay); kept separate from [`Self::incremental_cache`] because the
    /// two directions of one channel are different byte streams.
    uplink_cache: &'a mut ConnectionDataCache,
    stats: &'a Arc<ConnectionStats>,
    /// Shared capture hub; `audio_attached` pins at most one subscription
    /// per connection task (attached with the first successful session
    /// ack, so its ChannelOpen can never interleave before SessionAck).
    audio: &'a crate::audio::AudioHub,
    audio_attached: &'a mut bool,
    /// The id this connection task's audio subscription was made under:
    /// it identifies the client inside the hub.
    audio_client_id: &'a ConnectionId,
    /// Flow-control brain of this session (see the module header).
    flow: &'a mut FlowState,
    /// Re-advertisement period for held flow state on the priority lane.
    flow_ticker: &'a mut Interval,
}

/// Per-session flow-control state, owned by the relay task (the only frame
/// sender); pumps see the effects through shared [`rx11_core::flow`] handles.
#[derive(Debug, Default)]
struct FlowState {
    /// `FlowControl` capability negotiated with this client.
    negotiated: bool,
    /// Our uplink pauses (advertised to the client, enforced there).
    advertiser: Advertiser,
    /// Downlink pauses the client advertised at us, enforced on the
    /// listener pumps' egress gates (subject to the yield rule).
    peer_paused: HashSet<ConnectionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayAction {
    Continue,
    Disconnect,
}

async fn relay_loop(ctx: &mut RelayContext<'_>) -> rx11_core::error::Result<()> {
    loop {
        tokio::select! {
            frame_result = ctx.read_half.recv_frame() => {
                match frame_result {
                    Ok(frame) => {
                        if handle_inbound_frame(frame, ctx).await == RelayAction::Disconnect {
                            break;
                        }
                    }
                    Err(rx11_core::error::Rx11Error::ConnectionClosed) => {
                        info!("Client {} disconnected", ctx.transport_id);
                        break;
                    }
                    Err(e) => {
                        warn!("Error on client {}: {}", ctx.transport_id, e);
                        break;
                    }
                }
            }
            event = ctx.x11_events.recv() => {
                if handle_x11_event(event, ctx).await == RelayAction::Disconnect {
                    break;
                }
            }
            _ = ctx.flow_ticker.tick() => {
                re_advertise(ctx).await;
            }
            _ = tokio::time::sleep_until(ctx.heartbeat_deadline) => {
                warn!("Client {} heartbeat timeout, disconnecting", ctx.transport_id);
                break;
            }
        }
    }
    Ok(())
}

/// Queue a frame for the writer task; a closed queue means the connection is
/// gone and the loop must stop.
async fn send_outbound(outbound: &OutboundSender, frame: Frame) -> RelayAction {
    if outbound.send(frame).await.is_err() {
        RelayAction::Disconnect
    } else {
        RelayAction::Continue
    }
}

/// The flow-control frame for one edge, kept small on purpose (control
/// frames must stay cheap enough for the priority lane's 512-slot drop
/// surface alongside audio).
fn flow_frame(edge: FlowEdge) -> Frame {
    Frame::FlowControl(FlowControlMessage {
        action: edge.action,
        connection_id: Some(edge.connection_id),
    })
}

/// Record a locally-detected uplink edge and, on a real transition, put the
/// frame on the priority lane (never queued behind a redraw burst). Then
/// re-derive the downlink gate for the channel: the yield rule keys on our
/// own advertisement state.
async fn advertise_edge(ctx: &mut RelayContext<'_>, edge: FlowEdge) {
    if !ctx.flow.negotiated {
        return;
    }
    if ctx.flow.advertiser.record(edge) {
        let _ = ctx.outbound.send_priority(flow_frame(edge));
        if edge.action == FlowControlAction::Pause {
            ctx.stats.add_flow_advertised();
        }
        info!("flow: {:?} uplink for {}", edge.action, edge.connection_id);
    }
    sync_egress(ctx, edge.connection_id).await;
}

/// Yield rule, as a pure decision (unit-testable): enforce the peer's
/// downlink `Pause` only while we do NOT hold the same channel paused on
/// the uplink. Gating both legs of one X11 conversation at once is the
/// classic stop-and-wait deadlock — the uplink gate starves the reply path
/// that would unstick every buffer, while the downlink gate only defers
/// fresh requests. When they conflict, the downlink gate yields and lets
/// the application finish the blocking request.
fn egress_desired(peer_paused: bool, we_pause_up_advertised: bool) -> bool {
    peer_paused && !we_pause_up_advertised
}

/// Re-evaluate what the client's advertised pauses mean for one channel's
/// egress gate.
async fn sync_egress(ctx: &mut RelayContext<'_>, connection_id: ConnectionId) {
    let gated = egress_desired(
        ctx.flow.peer_paused.contains(&connection_id),
        ctx.flow.advertiser.is_paused(connection_id),
    );
    if let Some(gate) = ctx.session_mgr.egress_gate(connection_id).await {
        if gate.set(gated) {
            if gated {
                ctx.stats.add_flow_enforced();
                info!(
                    "flow: downlink paused for {} (client congestion)",
                    connection_id
                );
            } else {
                info!("flow: downlink resumed for {}", connection_id);
            }
        }
    }
}

/// Keep-alive of our advertised state: the priority lane drops when full,
/// and a lost `Resume` would strand a paused channel forever, so every
/// held state is re-stated each tick — first reconciled against the
/// gauges themselves: the pumps' state-machine reports ride a `try_send`
/// (they must never await the shared ingress, they ARE its drain), so a
/// dropped report must not leave us re-advertising a `Pause` the meter
/// has long since drained away. The meter is the ground truth.
async fn re_advertise(ctx: &mut RelayContext<'_>) {
    if !ctx.flow.negotiated {
        return;
    }
    let paused: Vec<ConnectionId> = ctx.flow.advertiser.paused_ids().collect();
    for id in paused {
        if !ctx.session_mgr.upmeter_latched(id).await {
            advertise_edge(ctx, FlowEdge::resume(id)).await;
        }
    }
    for edge in ctx.flow.advertiser.due() {
        let _ = ctx.outbound.send_priority(flow_frame(edge));
        sync_egress(ctx, edge.connection_id).await;
    }
}

/// Apply a peer pause/resume to one channel: membership already decided by
/// the caller; this records it and updates the enforcement gate.
async fn set_peer_pause(ctx: &mut RelayContext<'_>, connection_id: ConnectionId, paused: bool) {
    if paused {
        ctx.flow.peer_paused.insert(connection_id);
    } else {
        ctx.flow.peer_paused.remove(&connection_id);
    }
    sync_egress(ctx, connection_id).await;
}
async fn handle_inbound_frame(frame: Frame, ctx: &mut RelayContext<'_>) -> RelayAction {
    match frame {
        Frame::SessionCreate(msg) => {
            let result = ctx.session_mgr.create_session(
                msg.display,
                msg.auth_name,
                msg.auth_data,
                ctx.transport_id.to_string(),
            );
            send_session_ack(ctx, result.await).await
        }
        Frame::SessionResume(msg) => {
            let result = ctx
                .session_mgr
                .try_resume_session(&msg.session_id, ctx.transport_id.to_string());
            send_session_ack(ctx, result.await).await
        }
        Frame::SessionAutoCreate(msg) => {
            let result = ctx.session_mgr.create_session_auto(
                msg.auth_name,
                msg.auth_data,
                ctx.transport_id.to_string(),
            );
            send_session_ack(ctx, result.await).await
        }
        Frame::SessionDestroy(msg) => {
            let disp = msg.display;
            if !ctx.session_mgr.owns_session(disp, ctx.transport_id).await {
                warn!(
                    "Client {} tried to destroy unowned session for display {}",
                    ctx.transport_id, disp
                );
                return RelayAction::Continue;
            }
            ctx.session_mgr.unregister_x11_relay(disp).await;
            ctx.session_mgr.destroy_session(disp).await;
            info!("Session destroyed for display {}", disp);
            RelayAction::Continue
        }
        frame @ (Frame::StreamData(_)
        | Frame::CompressedStreamData(_)
        | Frame::IncrementalStreamData(_)
        | Frame::CompressedIncrementalStreamData(_)) => {
            // The client's uplink (application requests → relay). All four
            // shapes ride the SAME policy chain that builds and parses the
            // downlink — decode, diff-apply, decompress, savings accounting
            // — so a diff frame is handled exactly once, here, and a shape
            // this session never negotiated is refused INSIDE the chain
            // (`Dropped`) instead of touching the cache.
            //
            // Ownership is checked against the channel the frame names
            // BEFORE the frame may touch any per-connection state: a peer
            // must not be able to poison another client's diff base or
            // compression stream.
            let Some(connection_id) = frame.connection_id() else {
                warn!(
                    "Client {} sent a data frame without a channel id; ignoring",
                    ctx.transport_id
                );
                return RelayAction::Continue;
            };
            if !ctx
                .session_mgr
                .owns_connection(connection_id, ctx.transport_id)
                .await
            {
                warn!(
                    "Client {} sent {} for unowned {}",
                    ctx.transport_id,
                    frame.msg_type(),
                    connection_id
                );
                return RelayAction::Continue;
            }
            match ctx
                .dataplane
                .apply_inbound(frame, ctx.uplink_cache, ctx.stats)
            {
                InboundOutcome::Payload {
                    connection_id,
                    data,
                } => forward_to_x11(ctx, connection_id, data).await,
                InboundOutcome::ResyncRequired { connection_id } => {
                    // Our receive base is out of sync: tell the client to
                    // stop diffing this channel and resend whole payloads.
                    // The bytes already delivered to the X server are
                    // untouched — the hole self-heals forward, same promise
                    // the client's downlink resync makes.
                    if !ctx.dataplane.features.supports(Capability::RequestResync) {
                        warn!(
                            "Uplink diff desync on {} but resync was not negotiated; \
                             the frame was dropped",
                            connection_id
                        );
                        return RelayAction::Continue;
                    }
                    ctx.stats.add_incremental_full_fallback();
                    info!(
                        "Uplink diff desync on {}; asking client {} for full resend",
                        connection_id, ctx.transport_id
                    );
                    // A control signal, so it rides the priority lane — never
                    // queued behind the redraw burst on the bulk byte budget
                    // (the flow-control structural law). A full-lane drop is
                    // harmless: the next failed diff re-asks.
                    if ctx
                        .outbound
                        .send_priority(Frame::StreamResyncRequest(StreamResyncRequestMessage {
                            connection_id,
                        }))
                        .is_err()
                    {
                        warn!(
                            "Resync request for {} lost on a full priority lane; \
                             relying on the next diff to re-ask",
                            connection_id
                        );
                    }
                    RelayAction::Continue
                }
                InboundOutcome::StreamBroken => {
                    // Those bytes are gone and the X server stream is
                    // positional: keeping the session alive would corrupt
                    // it. Drop the connection so the client reconnects
                    // (resume rebuilds transports and the compression
                    // stream).
                    warn!(
                        "Uplink decompression failed for {} — failing the client connection",
                        connection_id
                    );
                    RelayAction::Disconnect
                }
                InboundOutcome::Dropped => RelayAction::Continue,
            }
        }
        Frame::StreamResyncRequest(msg) => {
            if !ctx.dataplane.features.supports(Capability::RequestResync) {
                warn!(
                    "Client {} sent StreamResyncRequest but resync capability was not negotiated; ignoring",
                    ctx.transport_id
                );
                return RelayAction::Continue;
            }
            if !ctx
                .session_mgr
                .owns_connection(msg.connection_id, ctx.transport_id)
                .await
            {
                warn!(
                    "Client {} requested full resend for unowned {}",
                    ctx.transport_id, msg.connection_id
                );
                return RelayAction::Continue;
            }
            info!(
                "Client {} requested full resend for {}; dropping send diff base",
                ctx.transport_id, msg.connection_id
            );
            ctx.incremental_cache.clear_connection(msg.connection_id);
            RelayAction::Continue
        }
        Frame::HeartbeatAck => {
            ctx.heartbeat_deadline = tokio::time::Instant::now() + ctx.runtime.heartbeat_timeout;
            RelayAction::Continue
        }
        Frame::FlowControl(msg) => {
            if !ctx.flow.negotiated {
                warn!(
                    "Client {} sent FlowControl without negotiated capability; ignoring",
                    ctx.transport_id
                );
                return RelayAction::Continue;
            }
            // Scope resolution: per-channel must be ours; a scope-less form
            // (never emitted by this build, accepted defensively) fans out
            // to every channel the client currently owns.
            let ids: Vec<ConnectionId> = match msg.connection_id {
                Some(id) => {
                    if !ctx.session_mgr.owns_connection(id, ctx.transport_id).await {
                        warn!(
                            "Client {} sent FlowControl for unowned {}",
                            ctx.transport_id, id
                        );
                        return RelayAction::Continue;
                    }
                    vec![id]
                }
                None => {
                    ctx.session_mgr
                        .connection_ids_for_client(ctx.transport_id)
                        .await
                }
            };
            let paused = matches!(msg.action, FlowControlAction::Pause);
            for id in ids {
                set_peer_pause(ctx, id, paused).await;
            }
            RelayAction::Continue
        }
        frame => {
            warn!(
                "Unexpected frame from client {}: {:?}",
                ctx.transport_id,
                frame.msg_type()
            );
            RelayAction::Continue
        }
    }
}

/// Ownership-checked plain-data forwarding for the StreamData path.
async fn forward_to_x11(
    ctx: &mut RelayContext<'_>,
    connection_id: ConnectionId,
    data: bytes::Bytes,
) -> RelayAction {
    match ctx
        .session_mgr
        .route_to_x11(connection_id, ctx.transport_id)
        .await
    {
        Some((sender, flow)) => deliver_charged(ctx, sender, flow, connection_id, data).await,
        None => {
            warn!(
                "Client {} sent data for unowned {}",
                ctx.transport_id, connection_id
            );
        }
    }
    RelayAction::Continue
}

/// Charge the uplink gauge, advertise any pause edge on the real-time
/// lane, and only THEN await the channel enqueue — the order is
/// load-bearing. When the gauge crosses, the congestion must be announced
/// before this task parks on the very channel that is full: the
/// advertisement is what triggers the client to stop feeding us, and the
/// app-write pump drains that channel — parking first means the frame
/// that would relieve the stall is the one never sent (full-duplex bulk
/// deadlock, caught by the perf suite). A gone channel retracts the
/// charge.
async fn deliver_charged(
    ctx: &mut RelayContext<'_>,
    sender: mpsc::Sender<crate::session::X11RelayToConn>,
    flow: Arc<crate::session::ConnFlow>,
    connection_id: ConnectionId,
    data: bytes::Bytes,
) {
    let len = data.len();
    let edge = flow.upmeter.charge(len);
    if edge {
        advertise_edge(ctx, FlowEdge::pause(connection_id)).await;
    }
    if sender
        .send(crate::session::X11RelayToConn::Data(data))
        .await
        .is_err()
    {
        flow.upmeter.discharge(len);
        warn!(
            "Failed to send data to x11 connection {}, channel gone",
            connection_id
        );
    }
}

async fn send_session_ack(
    ctx: &mut RelayContext<'_>,
    result: rx11_core::error::Result<Session>,
) -> RelayAction {
    let ack = match result {
        Ok(session) => {
            let disp = session.display;
            info!(
                "Session created: DISPLAY={} (client: {})",
                disp, ctx.transport_id
            );
            let recode = if ctx.dataplane.features.supports(Capability::X11Recode) {
                Some(rx11_core::x11recode::RecodeSpec {
                    codec: ctx.runtime.image_recode_codec,
                    quality: ctx.runtime.image_recode_quality,
                    min_bytes: ctx.runtime.image_recode_min_bytes,
                })
            } else {
                None
            };
            ctx.session_mgr
                .register_x11_relay(disp, ctx.x11_event_tx.clone(), recode, ctx.outbound.clone())
                .await;
            // Flow state is per-transport: gates and latches inherited from
            // a dead relay task must not strand the re-attached one (an
            // app still connected across the reconnect keeps pumping into
            // the gates this task alone will evaluate from now on).
            ctx.session_mgr.reset_flow_for_display(disp).await;
            // Audio rides the connection, not the display: attach with the
            // first successful ack. Capture/encode are shared process-wide;
            // attaching only registers this client's real-time lane.
            //
            // The whole-desktop mix is the fallback. The per-application
            // tier needs the client to have negotiated `audio_per_app`:
            // a per-application stream is a DIFFERENT channel per X11
            // connection, and a client that only understands the single
            // legacy mix channel would drop all but one application's
            // sound.
            if ctx.dataplane.features.supports(Capability::AudioForward) && !*ctx.audio_attached {
                *ctx.audio_attached = true;
                if ctx.dataplane.features.supports(Capability::AudioPerApp) && !ctx.audio.mix_only()
                {
                    ctx.audio
                        .attach_per_app(ctx.outbound.clone(), *ctx.audio_client_id);
                } else {
                    let conn = crate::x11_listener::next_connection_id();
                    ctx.audio.attach_mix(ctx.outbound.clone(), conn);
                }
            }
            SessionAckMessage {
                display: disp,
                success: true,
                error_msg: None,
                session_id: Some(session.id),
            }
        }
        Err(e) => SessionAckMessage {
            display: DisplayNumber::UNSPECIFIED,
            success: false,
            error_msg: Some(format!("{}", e)),
            session_id: None,
        },
    };
    send_outbound(ctx.outbound, Frame::SessionAck(ack)).await
}

async fn handle_x11_event(
    event: Option<X11ConnToRelay>,
    ctx: &mut RelayContext<'_>,
) -> RelayAction {
    match event {
        Some(X11ConnToRelay::Connected {
            display,
            connection_id,
        }) => {
            ctx.stats.inc_x11_connections();
            // The per-application audio tier: this X11 connection belongs
            // to an application, and that application's sound gets its own
            // audio channel, tagged with THIS connection. The channel is
            // opened lazily, on the application's first packet, so a
            // connection whose application never makes a sound never sees
            // an audio channel.
            if ctx.dataplane.features.supports(Capability::AudioPerApp)
                && ctx.dataplane.features.supports(Capability::AudioForward)
                && !ctx.audio.mix_only()
            {
                let app = ctx
                    .session_mgr
                    .application_of_connection(connection_id)
                    .await;
                let lane = ctx.audio.client_lane(*ctx.audio_client_id);
                if let (Some(app), Some(lane)) = (app, lane) {
                    info!(
                        "audio: X11 connection {connection_id} joins application {}",
                        app.get()
                    );
                    ctx.audio.attach_connection(app, connection_id, lane);
                }
            }
            send_outbound(
                ctx.outbound,
                Frame::ChannelOpen(ChannelOpenMessage {
                    connection_id,
                    kind: ChannelKind::x11(),
                    display: Some(display),
                    // An X11 channel belongs to no single application tag:
                    // the application label is the audio channel's field.
                    app: None,
                }),
            )
            .await
        }
        Some(X11ConnToRelay::Data {
            connection_id,
            data,
            ..
        }) => {
            let sequence_id = ctx.incremental_cache.next_sequence_for(connection_id);
            let frame = ctx.dataplane.build_frame(
                connection_id,
                sequence_id,
                data,
                Some(ctx.incremental_cache),
                Some(ctx.stats),
            );
            send_outbound(ctx.outbound, frame).await
        }
        Some(X11ConnToRelay::Congested {
            connection_id,
            action,
            ..
        }) => {
            // The app-write pump drained the uplink backlog across its
            // resume edge: re-state our advertisement to the client.
            advertise_edge(
                ctx,
                FlowEdge {
                    connection_id,
                    action,
                },
            )
            .await;
            RelayAction::Continue
        }
        Some(X11ConnToRelay::Disconnected { connection_id, .. }) => {
            // The per-application audio channel of this X11 connection
            // closes with it: the application loses one of its channels.
            // When it was the last one, the application's capture stops.
            if ctx.dataplane.features.supports(Capability::AudioPerApp)
                && ctx.dataplane.features.supports(Capability::AudioForward)
                && !ctx.audio.mix_only()
            {
                if let Some(app) = ctx
                    .session_mgr
                    .application_of_connection(connection_id)
                    .await
                {
                    ctx.audio.detach_connection(app, connection_id);
                }
            }
            ctx.session_mgr
                .unregister_x11_connection(connection_id)
                .await;
            // Both bases for this channel die with it: our downlink send
            // base and the uplink receive base we keep for the client's
            // diffs. Freed memory, and a recycled ConnectionId can never
            // apply a stale base.
            ctx.incremental_cache.clear_connection(connection_id);
            ctx.uplink_cache.clear_connection(connection_id);
            ctx.flow.advertiser.forget(connection_id);
            ctx.flow.peer_paused.remove(&connection_id);
            ctx.stats.dec_x11_connections();
            send_outbound(
                ctx.outbound,
                Frame::ChannelClose(ChannelCloseMessage { connection_id }),
            )
            .await
        }
        None => RelayAction::Disconnect,
    }
}

#[cfg(test)]
mod tests {
    use super::egress_desired;

    #[test]
    fn yield_rule_never_gates_both_legs_of_one_channel() {
        // No advertisement, no enforcement: normal flow.
        assert!(!egress_desired(false, false));
        // Client congested, our uplink free: enforce the pause.
        assert!(egress_desired(true, false));
        // Our uplink held paused, client says nothing: stay open.
        assert!(!egress_desired(false, true));
        // CONFLICT: both sides want the channel frozen — the downlink
        // gate yields; the uplink starvation would deadlock the whole
        // X11 conversation, a deferred request never does.
        assert!(!egress_desired(true, true));
    }
}
