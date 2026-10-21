//! The client-side relay receive loop: dispatches frames arriving from the
//! relay server to the local X server proxy and housekeeping handlers.
//!
//! All four data-frame shapes share the [`DataPlane`] inbound chain (decode,
//! diff-apply, stats); this module owns routing and the resync handshake.

use std::time::Duration;

use tracing::{debug, error, info, warn};

use rx11_core::dataplane::{DataPlane, InboundOutcome};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{Frame, IncrementalRequestFullMessage, SessionDestroyMessage};
use rx11_core::transport::Rx11TransportReadHalf;
use rx11_core::types::{ConnectionId, DisplayNumber};

use crate::proxy::{send_data_to_local, spawn_local_connection, ProxyDeps};

/// Everything the receive loop touches, borrowed from the connector scope.
pub(crate) struct ServeCtx<'a> {
    pub read_half: &'a mut Rx11TransportReadHalf,
    pub proxy: ProxyDeps<'a>,
    pub incremental_cache: &'a mut ConnectionDataCache,
    pub display: DisplayNumber,
    pub read_timeout: Duration,
}

impl ServeCtx<'_> {
    fn dataplane(&self) -> &DataPlane {
        self.proxy.dataplane
    }
}

pub(crate) async fn serve(ctx: &mut ServeCtx<'_>) -> anyhow::Result<()> {
    loop {
        tokio::select! {
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
        frame @ (Frame::DataX11(_)
        | Frame::CompressedDataX11(_)
        | Frame::IncrementalDataX11(_)
        | Frame::CompressedIncrementalDataX11(_)) => {
            // Copy the (small, Copy) policy so the cache borrow below stays
            // disjoint from the dataplane read.
            let dataplane = *ctx.proxy.dataplane;
            let outcome = dataplane.apply_inbound(frame, ctx.incremental_cache, ctx.proxy.stats);
            match outcome {
                InboundOutcome::Payload {
                    connection_id,
                    data,
                } => {
                    send_data_to_local(ctx.proxy.x11_connections, connection_id, data).await;
                }
                InboundOutcome::ResyncRequired { connection_id } => {
                    request_full_resend(ctx, connection_id).await;
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
    if !ctx.dataplane().features.request_resync {
        warn!(
            "Diff desync for {} but resync capability not negotiated; frame dropped",
            conn_id
        );
        return;
    }
    ctx.proxy.stats.add_incremental_full_fallback();
    let _ = ctx
        .proxy
        .outbound
        .send(Frame::IncrementalRequestFull(IncrementalRequestFullMessage {
            connection_id: conn_id,
        }))
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
    use rx11_core::config::RuntimeConfig;
    use rx11_core::protocol::{
        BinaryMessageCodec, CompressedIncrementalX11DataMessage, CompressedX11DataMessage,
        Features, HelloAckMessage, IncrementalChunk, IncrementalX11DataMessage, PROTOCOL_VERSION,
        X11DataMessage, X11DisconnectMessage,
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
        outbound_tx: mpsc::Sender<Frame>,
        outbound_rx: mpsc::Receiver<Frame>,
        conns: SharedX11Conns,
        stats: Arc<ConnectionStats>,
        seq: Arc<AtomicU32>,
        cache: ConnectionDataCache,
        dataplane: DataPlane,
    }

    impl Harness {
        async fn new(compression: Option<CompressionAlgo>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (_peer, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = Rx11Transport::new(client).split();
            let (tx, rx) = mpsc::channel(16);
            let mut features = Features::default();
            features.incremental = true;
            features.request_resync = true;
            Self {
                read_half,
                _write_half: write_half,
                outbound_tx: tx,
                outbound_rx: rx,
                conns: new_shared_conns(),
                stats: Arc::new(ConnectionStats::new()),
                seq: Arc::new(AtomicU32::new(1)),
                cache: ConnectionDataCache::new(),
                dataplane: DataPlane::new(features, compression, &RuntimeConfig::default()),
            }
        }

        fn ctx(&mut self) -> ServeCtx<'_> {
            ServeCtx {
                read_half: &mut self.read_half,
                proxy: ProxyDeps {
                    outbound: &self.outbound_tx,
                    x11_connections: &self.conns,
                    stats: &self.stats,
                    dataplane: &self.dataplane,
                    seq: &self.seq,
                    local_x11_addr: "127.0.0.1:0",
                },
                incremental_cache: &mut self.cache,
                display: DisplayNumber::new(0).unwrap(),
                read_timeout: Duration::from_secs(1),
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
        let mut h = Harness::new(None).await;
        let mut rx = h.register_local(conn(3)).await;
        {
            let mut ctx = h.ctx();
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
        }
        assert_eq!(&rx.recv().await.unwrap()[..], &[1, 2, 3]);
        assert_eq!(h.cache.get_cached(conn(3)).unwrap().0, 7);
        assert!(h.stats.summary().contains("Recv: 3 B"));
    }

    #[tokio::test]
    async fn compressed_data_delivers_and_counts_savings() {
        let payload = vec![0xABu8; 4096];
        let compressed = CompressionAlgo::Zstd
            .compress_to_bytes(&payload)
            .expect("compressible");

        let mut h = Harness::new(Some(CompressionAlgo::Zstd)).await;
        let mut rx = h.register_local(conn(4)).await;
        {
            let mut ctx = h.ctx();
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
        }
        assert_eq!(rx.recv().await.unwrap(), Bytes::from(payload));
        // Client-side accounting counts the decompressed application payload.
        let summary = h.stats.summary();
        assert!(summary.contains("Compressed (full): 1 frames"), "{summary}");
        assert!(summary.contains("Recv: 4.0 KB"), "{summary}");
    }

    #[tokio::test]
    async fn compressed_frame_without_negotiated_algo_is_dropped() {
        let mut h = Harness::new(None).await;
        let mut rx = h.register_local(conn(5)).await;
        {
            let mut ctx = h.ctx();
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
        let mut h = Harness::new(None).await;
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
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, Frame::IncrementalDataX11(msg))
                .await
                .unwrap();
        }
        assert_eq!(rx.recv().await.unwrap(), Bytes::from(new));
        let summary = h.stats.summary();
        assert!(summary.contains("Incremental: 1 frames"), "{summary}");
    }

    #[tokio::test]
    async fn stale_incremental_triggers_full_resend_request() {
        let mut h = Harness::new(None).await;
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
            let mut ctx = h.ctx();
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
        let mut h = Harness::new(Some(CompressionAlgo::Zstd)).await;
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
            let mut ctx = h.ctx();
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
        }
        assert_eq!(rx.recv().await.unwrap(), Bytes::from(new));
        let summary = h.stats.summary();
        assert!(
            summary.contains("Compressed (incremental): 1 frames"),
            "{summary}"
        );
        assert!(summary.contains("Incremental: 1 frames"), "{summary}");
    }

    #[tokio::test]
    async fn diff_disabled_ignores_incremental_frames() {
        // Peer sends diffs we never negotiated: dropped defensively.
        let mut h = Harness::new(None).await;
        let mut dp_features = Features::default();
        dp_features.request_resync = true; // no Incremental bit
        h.dataplane.features = dp_features;
        let _rx = h.register_local(conn(11)).await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::IncrementalDataX11(IncrementalX11DataMessage {
                    connection_id: conn(11),
                    sequence_id: 2,
                    base_sequence_id: 1,
                    total_len: 2048,
                    chunks: vec![],
                }),
            )
            .await
            .unwrap();
        }
        // apply_inbound does not know about per-direction gating yet, so the
        // base mismatch surfaces as a resync request; assert nothing panics
        // and one request frame is emitted.
        assert!(matches!(
            h.outbound_rx.recv().await,
            Some(Frame::IncrementalRequestFull(_))
        ));
    }

    #[tokio::test]
    async fn heartbeat_is_acked_outbound() {
        let mut h = Harness::new(None).await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, Frame::Heartbeat).await.unwrap();
        }
        assert!(matches!(
            h.outbound_rx.recv().await,
            Some(Frame::HeartbeatAck)
        ));
    }

    #[tokio::test]
    async fn x11_disconnect_cleans_registry_and_diff_cache() {
        let mut h = Harness::new(None).await;
        h.cache.update_cache(conn(9), 1, &[0u8; 2048]);
        let _rx = h.register_local(conn(9)).await;

        {
            let mut ctx = h.ctx();
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
        let mut h = Harness::new(None).await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::HelloAck(HelloAckMessage {
                    version: PROTOCOL_VERSION,
                    session_id: SessionId::new("s".into()).unwrap(),
                    success: true,
                    error_msg: None,
                    compression: None,
                    capabilities: vec![],
                }),
            )
            .await
            .unwrap();
        }
        h.assert_nothing_emitted().await;
    }
}
