//! The client-side relay receive loop: dispatches frames arriving from the
//! relay server to the local X server proxy and housekeeping handlers.

use tracing::{debug, error, info, warn};

use rx11_core::compress::decompress_frame_data;
use rx11_core::config::ClientDefaults;
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{
    BinaryMessageCodec, CompressedIncrementalX11DataMessage, CompressedX11DataMessage, Frame,
    IncrementalX11DataMessage, SessionDestroyMessage, X11DataMessage,
};
use rx11_core::transport::Rx11TransportReadHalf;
use rx11_core::types::DisplayNumber;

use crate::proxy::{send_data_to_local, spawn_local_connection, ProxyDeps};

/// Everything the receive loop touches, borrowed from the connector scope.
pub(crate) struct ServeCtx<'a> {
    pub read_half: &'a mut Rx11TransportReadHalf,
    pub proxy: ProxyDeps<'a>,
    pub incremental_cache: &'a mut ConnectionDataCache,
    pub display: DisplayNumber,
}

pub(crate) async fn serve(ctx: &mut ServeCtx<'_>) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            frame_result = tokio::time::timeout(
                ClientDefaults::READ_TIMEOUT,
                ctx.read_half.recv_frame()
            ) => {
                match frame_result {
                    Ok(Ok(frame)) => dispatch_frame(ctx, frame).await?,
                    Ok(Err(e)) => {
                        error!("Connection error: {}", e);
                        return Err(e.into());
                    }
                    Err(_) => {
                        error!(
                            "Read timeout ({:?}), no data from relay",
                            ClientDefaults::READ_TIMEOUT
                        );
                        return Err(rx11_core::error::Rx11Error::Timeout.into());
                    }
                }
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

async fn dispatch_frame(ctx: &mut ServeCtx<'_>, frame: Frame) -> anyhow::Result<()> {
    match frame {
        Frame::X11Connect(msg) => {
            info!("X11 client connected ({})", msg.connection_id);
            ctx.proxy.stats.inc_x11_connections();
            spawn_local_connection(msg.connection_id, msg.display, &ctx.proxy).await?;
        }
        Frame::DataX11(msg) => on_plain_data(ctx, msg).await,
        Frame::CompressedDataX11(msg) => on_compressed_data(ctx, msg).await,
        Frame::IncrementalDataX11(msg) => on_incremental_data(ctx, msg).await,
        Frame::CompressedIncrementalDataX11(msg) => on_compressed_incremental(ctx, msg).await,
        frame => on_other_frame(ctx, frame).await,
    }
    Ok(())
}

async fn on_plain_data(ctx: &mut ServeCtx<'_>, msg: X11DataMessage) {
    ctx.proxy.stats.add_bytes_received(msg.data.len() as u64);
    ctx.incremental_cache
        .update_cache(msg.connection_id, msg.sequence_id, &msg.data);
    send_data_to_local(ctx.proxy.x11_connections, msg.connection_id, msg.data).await;
}

async fn on_compressed_data(ctx: &mut ServeCtx<'_>, msg: CompressedX11DataMessage) {
    let Some(algo) = ctx.proxy.compression else {
        return;
    };
    let original_len = msg.original_len;
    let compressed_len = msg.data.len();
    let Some(decompressed) = decompress_frame_data(&msg, algo) else {
        warn!(
            "Decompression failed for {}, dropping frame",
            msg.connection_id
        );
        return;
    };
    ctx.proxy
        .stats
        .add_compression_saved(original_len.saturating_sub(compressed_len) as u64);
    deliver(ctx, msg.connection_id, msg.sequence_id, decompressed).await;
}

async fn on_incremental_data(ctx: &mut ServeCtx<'_>, msg: IncrementalX11DataMessage) {
    let conn_id = msg.connection_id;
    let total_len = msg.total_len;
    let Some(decompressed) = ctx.incremental_cache.apply_incremental(&msg) else {
        warn!(
            "Incremental data apply failed for {}, will request full data",
            conn_id
        );
        return;
    };
    let saved = total_len.saturating_sub(msg.chunks.iter().map(|c| c.data.len()).sum::<usize>());
    ctx.proxy.stats.add_incremental_saved(saved as u64);
    // apply_incremental already refreshed the cache with the result.
    deliver_without_cache(ctx, conn_id, decompressed).await;
}

async fn on_compressed_incremental(
    ctx: &mut ServeCtx<'_>,
    msg: CompressedIncrementalX11DataMessage,
) {
    let Some(algo) = ctx.proxy.compression else {
        return;
    };
    let original_len = msg.original_len;
    let compressed_len = msg.data.len();
    let conn_id = msg.connection_id;

    // Decompress the binary IncrementalDataX11 payload first…
    let Some(incremental_bytes) = decompress_frame_data(&msg, algo) else {
        warn!(
            "Incremental decompression failed for {}, dropping frame",
            conn_id
        );
        return;
    };
    ctx.proxy
        .stats
        .add_incremental_compression_saved(original_len.saturating_sub(compressed_len) as u64);

    // …then decode and apply the diff itself.
    let incremental_msg = match IncrementalX11DataMessage::decode_payload(&incremental_bytes) {
        Ok(m) => m,
        Err(e) => {
            warn!("Incremental decode failed for {}: {}", conn_id, e);
            return;
        }
    };
    on_incremental_data(ctx, incremental_msg).await;
}

/// Statistics for the received bytes, cache refresh and delivery to the
/// local socket — the common tail of the full-payload paths.
async fn deliver(
    ctx: &mut ServeCtx<'_>,
    conn_id: rx11_core::types::ConnectionId,
    seq_id: u32,
    decompressed: Vec<u8>,
) {
    ctx.proxy
        .stats
        .add_bytes_received(decompressed.len() as u64);
    ctx.incremental_cache
        .update_cache(conn_id, seq_id, &decompressed);
    send_data_to_local(
        ctx.proxy.x11_connections,
        conn_id,
        bytes::Bytes::from(decompressed),
    )
    .await;
}

/// Same tail without touching the diff cache (already updated by
/// `apply_incremental`).
async fn deliver_without_cache(
    ctx: &mut ServeCtx<'_>,
    conn_id: rx11_core::types::ConnectionId,
    decompressed: Vec<u8>,
) {
    ctx.proxy
        .stats
        .add_bytes_received(decompressed.len() as u64);
    send_data_to_local(
        ctx.proxy.x11_connections,
        conn_id,
        bytes::Bytes::from(decompressed),
    )
    .await;
}

async fn on_other_frame(ctx: &mut ServeCtx<'_>, frame: Frame) {
    match frame {
        Frame::Heartbeat => {
            let _ = ctx.proxy.outbound.send(Frame::HeartbeatAck).await;
        }
        Frame::SessionDestroy(msg) => {
            info!("Session destroyed for display {}", msg.display);
        }
        Frame::X11Disconnect(msg) => {
            debug!("Remote X11 client disconnected ({})", msg.connection_id);
            let mut conns = ctx.proxy.x11_connections.lock().await;
            conns.remove(&msg.connection_id.get());
            ctx.incremental_cache.clear_connection(msg.connection_id);
        }
        Frame::Error(msg) => {
            error!("Error from relay (code={}): {}", msg.code, msg.message);
        }
        Frame::FlowControl(msg) => {
            debug!(
                "FlowControl from relay: action={:?} conn={:?}",
                msg.action, msg.connection_id
            );
        }
        _ => {
            warn!("Unexpected frame from relay: {:?}", frame.msg_type());
        }
    }
}
