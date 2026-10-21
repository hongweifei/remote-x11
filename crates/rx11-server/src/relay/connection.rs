//! Per-client connection task: after the handshake, multiplexes frames
//! between the relay socket, the X11 listener events and the heartbeat.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use rx11_core::compress::decompress_frame_data;
use rx11_core::config::{BufferDefaults, RuntimeConfig, ServerDefaults};
use rx11_core::dataplane::DataPlane;
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::*;
use rx11_core::stats::ConnectionStats;
use rx11_core::transport::{Rx11Transport, Rx11TransportReadHalf, Rx11TransportWriteHalf};
use rx11_core::types::{ConnectionId, DisplayNumber};

use crate::session::{Session, SessionManager, X11ConnToRelay};

pub(super) async fn handle_client(
    stream: TcpStream,
    addr: SocketAddr,
    session_mgr: Arc<SessionManager>,
    stats: Arc<ConnectionStats>,
    runtime: RuntimeConfig,
    auth_token: &str,
) -> rx11_core::error::Result<()> {
    info!("Client connected from {}", addr);
    let mut transport = Rx11Transport::connect_tcp(stream)?;

    let handshake = server_handshake(
        &mut transport,
        auth_token,
        ServerDefaults::HANDSHAKE_TIMEOUT,
    )
    .await?;
    let dataplane = DataPlane::new(handshake.features, handshake.compression, &runtime);

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
    let (outbound_tx, outbound_rx) = mpsc::channel::<Frame>(BufferDefaults::OUTBOUND_CHANNEL);

    let heartbeat_task = spawn_heartbeat(outbound_tx.clone(), runtime.heartbeat_interval);
    let sender_task = spawn_sender(outbound_rx, write_half);

    let mut incremental_cache = ConnectionDataCache::from_runtime(&runtime);

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
        stats: &stats,
    };

    let result = relay_loop(&mut ctx).await;

    heartbeat_task.abort();
    drop(outbound_tx);
    let _ = sender_task.await;

    session_mgr.release_session(&transport_id).await;
    info!("Client disconnected from {}", addr);
    result
}

fn spawn_heartbeat(
    outbound: mpsc::Sender<Frame>,
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

fn spawn_sender(
    mut rx: mpsc::Receiver<Frame>,
    mut write_half: Rx11TransportWriteHalf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write_half.send_frame(&frame).await.is_err() {
                break;
            }
        }
        let _ = write_half.flush().await;
    })
}

/// Everything the relay loop touches, borrowed from [`handle_client`]'s scope.
struct RelayContext<'a> {
    read_half: &'a mut Rx11TransportReadHalf,
    outbound: &'a mpsc::Sender<Frame>,
    x11_events: &'a mut mpsc::Receiver<X11ConnToRelay>,
    session_mgr: &'a Arc<SessionManager>,
    transport_id: &'a str,
    dataplane: DataPlane,
    runtime: &'a RuntimeConfig,
    heartbeat_deadline: tokio::time::Instant,
    x11_event_tx: &'a mpsc::Sender<X11ConnToRelay>,
    incremental_cache: &'a mut ConnectionDataCache,
    stats: &'a Arc<ConnectionStats>,
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
async fn send_outbound(outbound: &mpsc::Sender<Frame>, frame: Frame) -> RelayAction {
    if outbound.send(frame).await.is_err() {
        RelayAction::Disconnect
    } else {
        RelayAction::Continue
    }
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
        Frame::StreamData(msg) => {
            ctx.stats.add_bytes_received(msg.data.len() as u64);
            forward_to_x11(ctx, msg.connection_id, msg.data.to_vec()).await
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
        Frame::CompressedStreamData(msg) => {
            if !ctx
                .session_mgr
                .owns_connection(msg.connection_id, ctx.transport_id)
                .await
            {
                warn!(
                    "Client {} sent compressed data for unowned {}",
                    ctx.transport_id, msg.connection_id
                );
                return RelayAction::Continue;
            }
            let Some(algo) = ctx.dataplane.compression else {
                warn!("CompressedStreamData received but no compression negotiated, dropping");
                return RelayAction::Continue;
            };
            ctx.stats.add_bytes_received(msg.data.len() as u64);
            match decompress_frame_data(&msg, algo) {
                Some(decompressed) => {
                    ctx.session_mgr
                        .send_to_x11_connection(msg.connection_id, decompressed)
                        .await;
                }
                None => {
                    warn!(
                        "Decompression failed for {}, dropping frame",
                        msg.connection_id
                    );
                }
            }
            RelayAction::Continue
        }
        Frame::HeartbeatAck => {
            ctx.heartbeat_deadline = tokio::time::Instant::now() + ctx.runtime.heartbeat_timeout;
            RelayAction::Continue
        }
        Frame::FlowControl(msg) => {
            warn!(
                "Client {} FlowControl {:?} for {:?} (capability registered, behavior not implemented)",
                ctx.transport_id, msg.action, msg.connection_id
            );
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
    data: Vec<u8>,
) -> RelayAction {
    if !ctx
        .session_mgr
        .owns_connection(connection_id, ctx.transport_id)
        .await
    {
        warn!(
            "Client {} sent data for unowned {}",
            ctx.transport_id, connection_id
        );
        return RelayAction::Continue;
    }
    ctx.session_mgr
        .send_to_x11_connection(connection_id, data)
        .await;
    RelayAction::Continue
}

async fn send_session_ack(
    ctx: &RelayContext<'_>,
    result: rx11_core::error::Result<Session>,
) -> RelayAction {
    let ack = match result {
        Ok(session) => {
            let disp = session.display;
            info!(
                "Session created: DISPLAY={} (client: {})",
                disp, ctx.transport_id
            );
            ctx.session_mgr
                .register_x11_relay(disp, ctx.x11_event_tx.clone())
                .await;
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
            send_outbound(
                ctx.outbound,
                Frame::ChannelOpen(ChannelOpenMessage {
                    connection_id,
                    kind: ChannelKind::x11(),
                    display: Some(display),
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
        Some(X11ConnToRelay::Disconnected { connection_id, .. }) => {
            ctx.session_mgr
                .unregister_x11_connection(connection_id)
                .await;
            ctx.incremental_cache.clear_connection(connection_id);
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
