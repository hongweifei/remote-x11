//! The client-side relay receive loop: dispatches frames arriving from the
//! relay server to the local X server proxy and housekeeping handlers.
//!
//! All four data-frame shapes share the [`DataPlane`] inbound chain (decode,
//! diff-apply, stats); this module owns routing and the resync handshake.

use std::time::Duration;

use tracing::{debug, error, info, warn};

use rx11_core::dataplane::{DataPlane, InboundOutcome};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{Capability, Frame, SessionDestroyMessage, StreamResyncRequestMessage};
use rx11_core::transport::Rx11TransportReadHalf;
use rx11_core::types::{ConnectionId, DisplayNumber};

use crate::audioout::{start_audio_outlet, AudioOutlet};
use crate::proxy::{send_data_to_local, spawn_local_connection, ProxyDeps};

/// Everything the receive loop touches, borrowed from the connector scope.
pub(crate) struct ServeCtx<'a> {
    pub read_half: &'a mut Rx11TransportReadHalf,
    pub proxy: ProxyDeps<'a>,
    pub incremental_cache: &'a mut ConnectionDataCache,
    pub display: DisplayNumber,
    pub read_timeout: Duration,
    /// The one open audio channel, if any (the server opens at most one
    /// per connection). Data for its id bypasses the dataplane entirely.
    pub audio: &'a mut Option<AudioOutlet>,
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
                // Audio channel: gated by the negotiated capability, one
                // outlet per connection. Starting the player is best-effort;
                // audio failures never touch the X11 session.
                if !ctx
                    .proxy
                    .dataplane
                    .features
                    .supports(Capability::AudioForward)
                {
                    warn!("Relay opened audio without negotiated capability; ignoring");
                } else if ctx.audio.is_some() {
                    warn!("Duplicate audio channel ({}); ignoring", msg.connection_id);
                } else {
                    match start_audio_outlet(msg.connection_id, ctx.proxy.runtime) {
                        Some(outlet) => {
                            info!("audio: playing to local sink ({})", msg.connection_id);
                            *ctx.audio = Some(outlet);
                        }
                        None => {
                            warn!("audio: no local player available; channel ignored")
                        }
                    }
                }
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
            // Real-time bypass: frames on the open audio channel go
            // straight to the local player — the diff cache, compression
            // chain and X connection routing never see them.
            if let Frame::StreamData(msg) = &frame {
                if ctx
                    .audio
                    .as_ref()
                    .is_some_and(|a| a.conn_id == msg.connection_id)
                {
                    let alive = ctx.audio.as_ref().unwrap().deliver(msg.data.clone());
                    if !alive {
                        warn!("audio: player gone; outlet removed");
                        *ctx.audio = None;
                    }
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
                    send_data_to_local(ctx.proxy.x11_connections, connection_id, data).await;
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
    let _ = ctx
        .proxy
        .outbound
        .send(Frame::StreamResyncRequest(StreamResyncRequestMessage {
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
        Frame::ChannelClose(msg) => {
            if ctx
                .audio
                .as_ref()
                .is_some_and(|a| a.conn_id == msg.connection_id)
            {
                info!("audio channel ({}) closed by relay", msg.connection_id);
                // Dropping the outlet closes its buffer: the player task
                // sees EOF, flushes and reaps the child (kill_on_drop).
                *ctx.audio = None;
            } else {
                debug!("Remote X11 client disconnected ({})", msg.connection_id);
                let mut conns = ctx.proxy.x11_connections.lock().await;
                conns.remove(&msg.connection_id.get());
                ctx.incremental_cache.clear_connection(msg.connection_id);
            }
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
        BinaryMessageCodec, ChannelCloseMessage, CompressedIncrementalStreamDataMessage,
        CompressedStreamDataMessage, Features, HelloAckMessage, IncrementalChunk,
        IncrementalStreamDataMessage, StreamDataMessage, PROTOCOL_VERSION,
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
        outbound_tx: rx11_core::queue::OutboundSender,
        outbound_rx: rx11_core::queue::OutboundReceiver,
        conns: SharedX11Conns,
        stats: Arc<ConnectionStats>,
        seq: Arc<AtomicU32>,
        cache: ConnectionDataCache,
        dataplane: DataPlane,
        rt: RuntimeConfig,
        audio: Option<crate::audioout::AudioOutlet>,
    }

    impl Harness {
        async fn new(compression: Option<CompressionAlgo>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (_peer, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = Rx11Transport::new(client).split();
            let (tx, rx) = rx11_core::queue::outbound_channel(16, rx11_core::queue::UNLIMITED);
            let features = Features::from_list(&["incremental".into(), "request_resync".into()]);
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
                rt: RuntimeConfig::default(),
                audio: None,
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
                    runtime: &self.rt,
                },
                incremental_cache: &mut self.cache,
                display: DisplayNumber::new(0).unwrap(),
                read_timeout: Duration::from_secs(1),
                audio: &mut self.audio,
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
                Frame::StreamData(StreamDataMessage {
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
                Frame::CompressedStreamData(CompressedStreamDataMessage {
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
                Frame::CompressedStreamData(CompressedStreamDataMessage {
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
        let msg = IncrementalStreamDataMessage {
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
            dispatch_frame(&mut ctx, Frame::IncrementalStreamData(msg))
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

        let msg = IncrementalStreamDataMessage {
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
            dispatch_frame(&mut ctx, Frame::IncrementalStreamData(msg))
                .await
                .unwrap();
        }

        match h.outbound_rx.recv().await {
            Some((Frame::StreamResyncRequest(m), _)) => assert_eq!(m.connection_id, conn(7)),
            other => panic!("expected StreamResyncRequest, got {other:?}"),
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

        let msg = IncrementalStreamDataMessage {
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
                Frame::CompressedIncrementalStreamData(CompressedIncrementalStreamDataMessage {
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
        h.dataplane.features = Features::from_list(&["request_resync".to_string()]);
        let _rx = h.register_local(conn(11)).await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::IncrementalStreamData(IncrementalStreamDataMessage {
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
            Some((Frame::StreamResyncRequest(_), _))
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
            Some((Frame::HeartbeatAck, _))
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
                Frame::ChannelClose(ChannelCloseMessage {
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

    fn audio_open(conn: ConnectionId) -> Frame {
        Frame::ChannelOpen(rx11_core::protocol::ChannelOpenMessage {
            connection_id: conn,
            kind: rx11_core::protocol::ChannelKind::audio(),
            display: None,
        })
    }

    #[tokio::test]
    async fn audio_channel_open_ignored_without_capability() {
        let mut h = Harness::new(None).await;
        h.dataplane.features = Features::default();
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, audio_open(conn(20)))
                .await
                .unwrap();
            assert!(ctx.audio.is_none(), "ungated audio channel must not start");
        }
        h.assert_nothing_emitted().await;
    }

    #[tokio::test]
    async fn audio_channel_open_spawns_outlet_when_gated() {
        let mut h = Harness::new(None).await;
        h.dataplane.features = Features::from_list(&["audio_forward".to_string()]);
        // A real player child we never wait on: cat drains stdin to null.
        h.rt.audio_play_command = Some(vec!["cat".to_string()]);
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, audio_open(conn(21)))
                .await
                .unwrap();
            assert!(ctx.audio.is_some(), "gated audio channel starts player");
            assert_eq!(ctx.audio.as_ref().unwrap().conn_id, conn(21));
        }
    }

    #[tokio::test]
    async fn audio_frames_bypass_the_dataplane_entirely() {
        let mut h = Harness::new(Some(CompressionAlgo::Zstd)).await;
        let (outlet, mut packets) = crate::audioout::AudioOutlet::for_test(conn(22));
        h.audio = Some(outlet);
        let mut local_rx = h.register_local(conn(22)).await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::StreamData(StreamDataMessage {
                    connection_id: conn(22),
                    sequence_id: 5,
                    data: Bytes::from_static(&[0xDE, 0xAD]),
                }),
            )
            .await
            .unwrap();
        }
        assert_eq!(&packets.recv().await.unwrap()[..], &[0xDE, 0xAD]);
        assert!(local_rx
            .try_recv()
            .unwrap_err()
            .to_string()
            .contains("empty"));
        assert!(
            h.cache.get_cached(conn(22)).is_none(),
            "no diff-cache touch"
        );
        assert!(
            !h.stats.summary().contains("Recv:"),
            "no dataplane byte accounting: {}",
            h.stats.summary()
        );
    }

    #[tokio::test]
    async fn dead_player_clears_the_outlet() {
        let mut h = Harness::new(None).await;
        let (outlet, rx) = crate::audioout::AudioOutlet::for_test(conn(23));
        drop(rx); // player task gone
        h.audio = Some(outlet);
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::StreamData(StreamDataMessage {
                    connection_id: conn(23),
                    sequence_id: 1,
                    data: Bytes::from_static(&[1]),
                }),
            )
            .await
            .unwrap();
            assert!(
                ctx.audio.is_none(),
                "closed outlet is removed after delivery"
            );
        }
    }

    #[tokio::test]
    async fn channel_close_clears_audio_but_keeps_x11_routed() {
        let mut h = Harness::new(None).await;
        h.cache.update_cache(conn(24), 1, &[0u8; 64]);
        let (outlet, _rx) = crate::audioout::AudioOutlet::for_test(conn(24));
        h.audio = Some(outlet);
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: conn(24),
                }),
            )
            .await
            .unwrap();
            assert!(ctx.audio.is_none());
        }
        // An x11 conn close still uses the registry path untouched.
        let _rx2 = h.register_local(conn(25)).await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: conn(25),
                }),
            )
            .await
            .unwrap();
        }
        assert!(h.conns.lock().await.is_empty());
    }
}
