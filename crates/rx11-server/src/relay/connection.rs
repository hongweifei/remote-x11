//! Per-client connection task: after the handshake, multiplexes frames
//! between the relay socket, the X11 listener events and the heartbeat.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use rx11_core::compress::{
    decompress_frame_data, maybe_incremental_or_compress_frame, CompressionAlgo,
};
use rx11_core::config::{BufferDefaults, ServerDefaults};
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
    auth_token: &str,
) -> rx11_core::error::Result<()> {
    info!("Client connected from {}", addr);
    let mut transport = Rx11Transport::new(stream)?;

    let handshake = server_handshake(
        &mut transport,
        auth_token,
        ServerDefaults::HANDSHAKE_TIMEOUT,
    )
    .await?;
    let compression = handshake.compression;

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

    let heartbeat_task = spawn_heartbeat(outbound_tx.clone(), ServerDefaults::HEARTBEAT_INTERVAL);
    let sender_task = spawn_sender(outbound_rx, write_half);

    let mut incremental_cache = ConnectionDataCache::new();

    let mut ctx = RelayContext {
        read_half: &mut read_half,
        outbound: &outbound_tx,
        x11_events: &mut x11_event_rx,
        session_mgr: &session_mgr,
        transport_id: &transport_id,
        compression,
        heartbeat_deadline: tokio::time::Instant::now() + ServerDefaults::HEARTBEAT_TIMEOUT,
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
    compression: Option<CompressionAlgo>,
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
        Frame::DataX11(msg) => {
            ctx.stats.add_bytes_received(msg.data.len() as u64);
            forward_to_x11(ctx, msg.connection_id, msg.data.to_vec()).await
        }
        Frame::IncrementalRequestFull(msg) => {
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
                "Client {} requested full resend for {} (diff cache dropped a frame); clearing send base",
                ctx.transport_id, msg.connection_id
            );
            ctx.incremental_cache.clear_connection(msg.connection_id);
            RelayAction::Continue
        }
        Frame::CompressedDataX11(msg) => {
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
            let Some(algo) = ctx.compression else {
                warn!("CompressedDataX11 received but no compression negotiated, dropping");
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
            ctx.heartbeat_deadline =
                tokio::time::Instant::now() + ServerDefaults::HEARTBEAT_TIMEOUT;
            RelayAction::Continue
        }
        Frame::FlowControl(msg) => {
            warn!(
                "Client {} FlowControl {:?} for {:?} (not implemented)",
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

/// Ownership-checked plain-data forwarding for the DataX11 path.
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

/// Attribute one outbound X11 data frame to the relay stats: wire bytes
/// plus compression / incremental savings relative to the original payload.
fn record_outbound_frame(stats: &ConnectionStats, frame: &Frame, original_len: usize) {
    match frame {
        Frame::DataX11(m) => stats.add_bytes_sent(m.data.len() as u64),
        Frame::CompressedDataX11(m) => {
            stats.add_bytes_sent(m.data.len() as u64);
            stats.add_compression_saved(m.original_len.saturating_sub(m.data.len()) as u64);
        }
        Frame::IncrementalDataX11(m) => {
            let chunks_len: usize = m.chunks.iter().map(|c| c.data.len()).sum();
            stats.add_bytes_sent(chunks_len as u64);
            stats.add_incremental_saved(original_len.saturating_sub(chunks_len) as u64);
        }
        Frame::CompressedIncrementalDataX11(m) => {
            stats.add_bytes_sent(m.data.len() as u64);
            stats.add_incremental_compression_saved(
                m.original_len.saturating_sub(m.data.len()) as u64
            );
        }
        _ => {}
    }
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
                Frame::X11Connect(X11ConnectMessage {
                    display,
                    connection_id,
                }),
            )
            .await
        }
        Some(X11ConnToRelay::Data {
            connection_id,
            data,
            ..
        }) => {
            let original_len = data.len();
            let sequence_id = ctx.incremental_cache.next_sequence_for(connection_id);
            let frame = maybe_incremental_or_compress_frame(
                connection_id,
                sequence_id,
                data,
                ctx.compression,
                Some(ctx.incremental_cache),
            );
            record_outbound_frame(ctx.stats, &frame, original_len);
            send_outbound(ctx.outbound, frame).await
        }
        Some(X11ConnToRelay::Disconnected {
            display,
            connection_id,
        }) => {
            ctx.session_mgr
                .unregister_x11_connection(connection_id)
                .await;
            ctx.incremental_cache.clear_connection(connection_id);
            ctx.stats.dec_x11_connections();
            send_outbound(
                ctx.outbound,
                Frame::X11Disconnect(X11DisconnectMessage {
                    display,
                    connection_id,
                }),
            )
            .await
        }
        None => RelayAction::Disconnect,
    }
}
