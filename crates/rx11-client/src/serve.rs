//! The client-side relay receive loop: dispatches frames arriving from the
//! relay server to the local X server proxy and housekeeping handlers.

use tracing::{debug, error, info, warn};

use rx11_core::compress::decompress_frame_data;
use rx11_core::config::ClientDefaults;
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{
    BinaryMessageCodec, CompressedIncrementalX11DataMessage, CompressedX11DataMessage, Frame,
    IncrementalRequestFullMessage, IncrementalX11DataMessage, SessionDestroyMessage,
    X11DataMessage,
};
use rx11_core::transport::Rx11TransportReadHalf;
use rx11_core::types::ConnectionId;
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

/// Ask the relay to stop diffing this connection and resend full frames:
/// our receive cache is out of sync, so every further diff would fail.
async fn request_full_resend(ctx: &mut ServeCtx<'_>, conn_id: ConnectionId) {
    ctx.proxy.stats.add_incremental_full_fallback();
    let _ = ctx
        .proxy
        .outbound
        .send(Frame::IncrementalRequestFull(
            IncrementalRequestFullMessage {
                connection_id: conn_id,
            },
        ))
        .await;
}

async fn on_incremental_data(ctx: &mut ServeCtx<'_>, msg: IncrementalX11DataMessage) {
    let conn_id = msg.connection_id;
    let total_len = msg.total_len;
    let Some(decompressed) = ctx.incremental_cache.apply_incremental(&msg) else {
        warn!(
            "Incremental data apply failed for {}, requesting full data",
            conn_id
        );
        request_full_resend(ctx, conn_id).await;
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
        request_full_resend(ctx, conn_id).await;
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
            request_full_resend(ctx, conn_id).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{new_shared_conns, SharedX11Conns};
    use bytes::Bytes;
    use rx11_core::compress::CompressionAlgo;
    use rx11_core::protocol::{
        HelloAckMessage, IncrementalChunk, X11DisconnectMessage, PROTOCOL_VERSION,
    };
    use rx11_core::stats::ConnectionStats;
    use rx11_core::transport::{Rx11Transport, Rx11TransportWriteHalf};
    use rx11_core::types::SessionId;
    use std::future;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TryRecvError;

    /// Drives `dispatch_frame` without a real relay: the read half is a
    /// loopback socket nobody feeds, all assertions go through the outbound
    /// channel, local conn channels and stats.
    struct Harness {
        read_half: Rx11TransportReadHalf,
        _write_half: Rx11TransportWriteHalf,
        _peer: tokio::net::TcpStream,
        outbound_tx: mpsc::Sender<Frame>,
        outbound_rx: mpsc::Receiver<Frame>,
        conns: SharedX11Conns,
        stats: Arc<ConnectionStats>,
        seq: Arc<AtomicU32>,
        cache: ConnectionDataCache,
        local_addr: String,
    }

    impl Harness {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (peer, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = Rx11Transport::new(client).unwrap().split();
            let (tx, rx) = mpsc::channel(16);
            Self {
                read_half,
                _write_half: write_half,
                _peer: peer,
                outbound_tx: tx,
                outbound_rx: rx,
                conns: new_shared_conns(),
                stats: Arc::new(ConnectionStats::new()),
                seq: Arc::new(AtomicU32::new(1)),
                cache: ConnectionDataCache::new(),
                local_addr: "unused".to_string(),
            }
        }

        fn ctx(&mut self, compression: Option<CompressionAlgo>) -> ServeCtx<'_> {
            ServeCtx {
                read_half: &mut self.read_half,
                proxy: ProxyDeps {
                    outbound: &self.outbound_tx,
                    x11_connections: &self.conns,
                    stats: &self.stats,
                    compression,
                    seq: &self.seq,
                    local_x11_addr: &self.local_addr,
                },
                incremental_cache: &mut self.cache,
                display: DisplayNumber::new(0).unwrap(),
            }
        }

        async fn register_local(&mut self, conn: ConnectionId) -> mpsc::Receiver<Bytes> {
            let (tx, rx) = mpsc::channel(16);
            let handle = tokio::spawn(future::pending::<()>());
            self.conns.lock().await.insert(conn.get(), (tx, handle));
            rx
        }

        async fn assert_nothing_emitted(&mut self) {
            assert!(matches!(
                self.outbound_rx.try_recv().unwrap_err(),
                TryRecvError::Empty
            ));
        }
    }

    fn conn(id: u32) -> ConnectionId {
        ConnectionId::new(id)
    }

    #[tokio::test]
    async fn plain_data_delivers_and_updates_receive_cache() {
        let mut h = Harness::new().await;
        let mut rx = h.register_local(conn(3)).await;
        {
            let mut ctx = h.ctx(None);
            dispatch_frame(
                &mut ctx,
                Frame::DataX11(X11DataMessage {
                    connection_id: conn(3),
                    sequence_id: 7,
                    data: Bytes::from_static(&[1, 2, 3]),
                }),
            )
            .await
            .unwrap();
            assert_eq!(&rx.recv().await.unwrap()[..], &[1, 2, 3]);
        }
        assert_eq!(h.cache.get_cached(conn(3)).unwrap().0, 7);
        assert!(h.stats.summary().contains("Recv: 3 B"));
    }

    #[tokio::test]
    async fn compressed_data_delivers_and_counts_savings() {
        let payload = vec![0xABu8; 4096];
        let compressed = CompressionAlgo::Zstd
            .compress_to_bytes(&payload)
            .expect("compressible");

        let mut h = Harness::new().await;
        let mut rx = h.register_local(conn(4)).await;
        {
            let mut ctx = h.ctx(Some(CompressionAlgo::Zstd));
            dispatch_frame(
                &mut ctx,
                Frame::CompressedDataX11(CompressedX11DataMessage {
                    connection_id: conn(4),
                    sequence_id: 2,
                    original_len: payload.len(),
                    data: compressed,
                }),
            )
            .await
            .unwrap();
            assert_eq!(rx.recv().await.unwrap(), Bytes::from(payload));
        }
        // Client-side accounting counts the decompressed application payload.
        let summary = h.stats.summary();
        assert!(summary.contains("Compressed (full): 1 frames"), "{summary}");
        assert!(summary.contains("Recv: 4.0 KB"), "{summary}");
    }

    #[tokio::test]
    async fn compressed_frame_without_negotiated_algo_is_dropped() {
        let mut h = Harness::new().await;
        let mut rx = h.register_local(conn(5)).await;
        {
            let mut ctx = h.ctx(None);
            dispatch_frame(
                &mut ctx,
                Frame::CompressedDataX11(CompressedX11DataMessage {
                    connection_id: conn(5),
                    sequence_id: 1,
                    original_len: 8,
                    data: Bytes::from_static(&[1, 2, 3, 4]),
                }),
            )
            .await
            .unwrap();
        }
        assert!(matches!(rx.try_recv().unwrap_err(), TryRecvError::Empty));
        h.assert_nothing_emitted().await;
    }

    #[tokio::test]
    async fn incremental_applies_against_matching_base() {
        let mut h = Harness::new().await;
        let old = vec![0u8; 2048];
        let mut new = vec![0u8; 2048];
        new[1500..1600].fill(1);
        h.cache.update_cache(conn(6), 1, &old);

        let mut rx = h.register_local(conn(6)).await;
        let msg = IncrementalX11DataMessage {
            connection_id: conn(6),
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: new.len(),
            chunks: vec![IncrementalChunk {
                offset: 1500,
                length: 100,
                data: Bytes::from(vec![1u8; 100]),
            }],
        };
        {
            let mut ctx = h.ctx(None);
            dispatch_frame(&mut ctx, Frame::IncrementalDataX11(msg))
                .await
                .unwrap();
            assert_eq!(rx.recv().await.unwrap(), Bytes::from(new));
        }
        let summary = h.stats.summary();
        assert!(summary.contains("Incremental: 1 frames"), "{summary}");
    }

    #[tokio::test]
    async fn stale_incremental_triggers_full_resend_request() {
        let mut h = Harness::new().await;
        let _rx = h.register_local(conn(7)).await;

        let msg = IncrementalX11DataMessage {
            connection_id: conn(7),
            sequence_id: 2,
            base_sequence_id: 99, // receiver has nothing cached at base 99
            total_len: 2048,
            chunks: vec![IncrementalChunk {
                offset: 0,
                length: 4,
                data: Bytes::from_static(&[1, 2, 3, 4]),
            }],
        };
        {
            let mut ctx = h.ctx(None);
            dispatch_frame(&mut ctx, Frame::IncrementalDataX11(msg))
                .await
                .unwrap();
        }

        match h.outbound_rx.recv().await {
            Some(Frame::IncrementalRequestFull(m)) => assert_eq!(m.connection_id, conn(7)),
            other => panic!("expected IncrementalRequestFull, got {other:?}"),
        }
        let summary = h.stats.summary();
        assert!(summary.contains("1 fallbacks"), "{summary}");
    }

    #[tokio::test]
    async fn compressed_incremental_roundtrips_and_reapplies() {
        let mut h = Harness::new().await;
        let old = vec![0u8; 2048];
        let mut new = vec![0u8; 2048];
        new[1000..1900].fill(0x5A);
        h.cache.update_cache(conn(8), 5, &old);

        let msg = IncrementalX11DataMessage {
            connection_id: conn(8),
            sequence_id: 6,
            base_sequence_id: 5,
            total_len: new.len(),
            chunks: vec![IncrementalChunk {
                offset: 1000,
                length: 900,
                data: Bytes::from(vec![0x5Au8; 900]),
            }],
        };
        let encoded = msg.encode_payload().unwrap();
        let original_len = encoded.len();
        let compressed = CompressionAlgo::Zstd
            .compress_to_bytes(&encoded)
            .expect("diff encodable+compressible");

        let mut rx = h.register_local(conn(8)).await;
        {
            let mut ctx = h.ctx(Some(CompressionAlgo::Zstd));
            dispatch_frame(
                &mut ctx,
                Frame::CompressedIncrementalDataX11(CompressedIncrementalX11DataMessage {
                    connection_id: conn(8),
                    sequence_id: 6,
                    original_len,
                    data: compressed,
                }),
            )
            .await
            .unwrap();
            assert_eq!(rx.recv().await.unwrap(), Bytes::from(new));
        }
        let summary = h.stats.summary();
        assert!(
            summary.contains("Compressed (incremental): 1 frames"),
            "{summary}"
        );
        assert!(summary.contains("Incremental: 1 frames"), "{summary}");
    }

    #[tokio::test]
    async fn heartbeat_is_acked_outbound() {
        let mut h = Harness::new().await;
        {
            let mut ctx = h.ctx(None);
            dispatch_frame(&mut ctx, Frame::Heartbeat).await.unwrap();
        }
        assert!(matches!(
            h.outbound_rx.recv().await,
            Some(Frame::HeartbeatAck)
        ));
    }

    #[tokio::test]
    async fn x11_disconnect_cleans_registry_and_diff_cache() {
        let mut h = Harness::new().await;
        h.cache.update_cache(conn(9), 1, &[0u8; 2048]);
        let _rx = h.register_local(conn(9)).await;

        {
            let mut ctx = h.ctx(None);
            dispatch_frame(
                &mut ctx,
                Frame::X11Disconnect(X11DisconnectMessage {
                    display: DisplayNumber::new(0).unwrap(),
                    connection_id: conn(9),
                }),
            )
            .await
            .unwrap();
        }

        assert!(h.conns.lock().await.is_empty());
        assert!(h.cache.get_cached(conn(9)).is_none());
    }

    #[tokio::test]
    async fn session_ack_like_unexpected_frame_is_ignored() {
        let mut h = Harness::new().await;
        {
            let mut ctx = h.ctx(None);
            dispatch_frame(
                &mut ctx,
                Frame::HelloAck(HelloAckMessage {
                    version: PROTOCOL_VERSION,
                    session_id: SessionId::new("s".into()).unwrap(),
                    success: true,
                    error_msg: None,
                    compression: None,
                }),
            )
            .await
            .unwrap();
        }
        h.assert_nothing_emitted().await;
    }
}
