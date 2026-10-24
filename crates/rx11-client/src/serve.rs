//! The client-side relay receive loop: dispatches frames arriving from the
//! relay server to the local X server proxy and housekeeping handlers.
//!
//! All four data-frame shapes share the [`DataPlane`] inbound chain (decode,
//! diff-apply, stats); this module owns routing, the resync handshake and
//! the client side of flow control: downlink gauge edges are advertised on
//! the priority lane, and the relay's uplink pauses flip the per-channel
//! egress gates the proxy pumps await.

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

use crate::audioout::{start_audio_outlet, AudioOutlet};
use crate::proxy::{send_data_to_local, spawn_local_connection, ProxyDeps};

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
    /// The one open audio channel, if any (the server opens at most one
    /// per connection). Data for its id bypasses the dataplane entirely.
    pub audio: &'a mut Option<AudioOutlet>,
    pub flow: &'a mut ServeFlow,
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
mod tests {
    use super::*;
    use crate::proxy::{new_shared_conns, SharedX11Conns};
    use bytes::Bytes;
    use rx11_core::compress::CompressionAlgo;
    use rx11_core::config::RuntimeConfig;
    use rx11_core::dataplane::DataPlaneSide;
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
        flow: ServeFlow,
        // Kept alive so `flow.edges` never sees a close; tests may also
        // report edges through it like the pump does.
        flow_edge_tx: mpsc::Sender<FlowEdge>,
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
            let (flow_edge_tx, flow_edge_rx) = mpsc::channel(64);
            Self {
                read_half,
                _write_half: write_half,
                outbound_tx: tx,
                outbound_rx: rx,
                conns: new_shared_conns(),
                stats: Arc::new(ConnectionStats::new()),
                seq: Arc::new(AtomicU32::new(1)),
                cache: ConnectionDataCache::new(),
                dataplane: DataPlane::new(
                    DataPlaneSide::Client,
                    features.clone(),
                    compression,
                    &RuntimeConfig::default(),
                ),
                rt: RuntimeConfig::default(),
                audio: None,
                flow: ServeFlow::new(&features, flow_edge_rx),
                flow_edge_tx,
            }
        }

        /// Negotiate `flow_control` for tests that exercise it.
        fn enable_flow(&mut self) {
            self.flow.negotiated = true;
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
                    flow_edges: &self.flow_edge_tx,
                },
                incremental_cache: &mut self.cache,
                display: DisplayNumber::new(0).unwrap(),
                read_timeout: Duration::from_secs(1),
                audio: &mut self.audio,
                flow: &mut self.flow,
            }
        }

        async fn register_local(&mut self, conn: ConnectionId) -> mpsc::Receiver<Bytes> {
            self.register_local_with(
                conn,
                rx11_core::flow::FlowMeter::new(1 << 30, 1 << 20),
                rx11_core::flow::EgressGate::new(),
            )
            .await
        }

        async fn register_local_with(
            &mut self,
            conn: ConnectionId,
            meter: Arc<rx11_core::flow::FlowMeter>,
            gate: Arc<rx11_core::flow::EgressGate>,
        ) -> mpsc::Receiver<Bytes> {
            let (tx, rx) = mpsc::channel(16);
            let handle = tokio::spawn(future::pending::<()>());
            self.conns.lock().await.insert(
                conn.get(),
                (
                    tx,
                    handle,
                    meter,
                    gate,
                    Arc::new(std::sync::atomic::AtomicBool::new(false)),
                ),
            );
            rx
        }

        async fn assert_nothing_emitted(&mut self) {
            assert!(matches!(
                self.outbound_rx.try_recv().unwrap_err(),
                TryRecvError::Empty
            ));
        }

        fn take_priority_frame(&mut self) -> Option<Frame> {
            self.outbound_rx.try_recv_priority()
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

        // The resync request rides the priority lane: it is a control signal
        // that must not queue behind the redraw burst it is trying to stop.
        match h.outbound_rx.recv_priority().await {
            Some(Frame::StreamResyncRequest(m)) => assert_eq!(m.connection_id, conn(7)),
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
        // The direction gate lives inside `apply_inbound`: a diff frame the
        // session never negotiated is refused outright (Dropped, with a
        // warning), so no resync request is emitted and the cache is never
        // touched. A peer that can't agree on the shape must not be able to
        // make us spam control frames.
        h.assert_nothing_emitted().await;
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

    fn flow_frame(action: FlowControlAction, id: ConnectionId) -> Frame {
        Frame::FlowControl(rx11_core::protocol::FlowControlMessage {
            action,
            connection_id: Some(id),
        })
    }

    fn data_frame(id: ConnectionId, len: usize) -> Frame {
        Frame::StreamData(StreamDataMessage {
            connection_id: id,
            sequence_id: 1,
            data: Bytes::from(vec![3u8; len]),
        })
    }

    #[tokio::test]
    async fn downlink_gauge_edge_advertises_pause_on_the_priority_lane() {
        let mut h = Harness::new(None).await;
        h.enable_flow();
        let meter = rx11_core::flow::FlowMeter::new(10, 4);
        let _rx = h
            .register_local_with(conn(30), Arc::clone(&meter), Default::default())
            .await;

        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, data_frame(conn(30), 6))
                .await
                .unwrap();
        }
        assert!(h.take_priority_frame().is_none(), "below high: silent");
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, data_frame(conn(30), 6))
                .await
                .unwrap();
        }
        match h.take_priority_frame() {
            Some(Frame::FlowControl(m)) => {
                assert_eq!(m.action, FlowControlAction::Pause);
                assert_eq!(m.connection_id, Some(conn(30)));
            }
            other => panic!("expected priority FlowControl(Pause), got {other:?}"),
        }
        // Third push while latched: no second advertisement.
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, data_frame(conn(30), 6))
                .await
                .unwrap();
        }
        assert!(h.take_priority_frame().is_none());
        // The advertisement skipped the byte budget entirely — it went out
        // on the real-time lane, never as a bulk frame.
        assert!(h.outbound_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn pump_reported_resume_edge_is_re_advertised() {
        let mut h = Harness::new(None).await;
        h.enable_flow();
        let meter = rx11_core::flow::FlowMeter::new(10, 4);
        let _rx = h
            .register_local_with(conn(31), Arc::clone(&meter), Default::default())
            .await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, data_frame(conn(31), 12))
                .await
                .unwrap();
        }
        assert!(matches!(
            h.take_priority_frame(),
            Some(Frame::FlowControl(_))
        ));
        // The pump drains fully and reports the edge on the report channel.
        meter.discharge(12);
        h.flow_edge_tx
            .try_send(FlowEdge::resume(conn(31)))
            .expect("report channel live");
        {
            let mut ctx = h.ctx();
            // Drive the same path the serve loop's arm uses.
            advertise_edge(&mut ctx, FlowEdge::resume(conn(31))).await;
        }
        match h.take_priority_frame() {
            Some(Frame::FlowControl(m)) => {
                assert_eq!(m.action, FlowControlAction::Resume);
                assert_eq!(m.connection_id, Some(conn(31)));
            }
            other => panic!("expected priority FlowControl(Resume), got {other:?}"),
        }
        assert!(!h.flow.advertiser.is_paused(conn(31)));
    }

    #[tokio::test]
    async fn relay_pause_flips_the_uplink_gate_and_resume_opens_it() {
        let mut h = Harness::new(None).await;
        h.enable_flow();
        let gate: Arc<rx11_core::flow::EgressGate> = Default::default();
        let _rx = h
            .register_local_with(
                conn(32),
                rx11_core::flow::FlowMeter::new(1 << 30, 1 << 20),
                Arc::clone(&gate),
            )
            .await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, flow_frame(FlowControlAction::Pause, conn(32)))
                .await
                .unwrap();
            assert!(gate.paused(), "relay pause enforced locally");
            dispatch_frame(&mut ctx, flow_frame(FlowControlAction::Resume, conn(32)))
                .await
                .unwrap();
            assert!(!gate.paused(), "relay resume releases the gate");
        }
        // Enforcement never advertises back: the lane stays quiet.
        h.assert_nothing_emitted().await;
        assert!(h.take_priority_frame().is_none());
    }

    #[tokio::test]
    async fn ungate_and_unknown_channel_flow_frames_are_inert() {
        // Not negotiated: nothing is enforced or advertised.
        let mut h = Harness::new(None).await;
        let gate: Arc<rx11_core::flow::EgressGate> = Default::default();
        let _rx = h
            .register_local_with(
                conn(33),
                rx11_core::flow::FlowMeter::new(1 << 30, 1 << 20),
                Arc::clone(&gate),
            )
            .await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, flow_frame(FlowControlAction::Pause, conn(33)))
                .await
                .unwrap();
            assert!(!gate.paused(), "ungated flow control ignored");
        }
        // Negotiated, but for a channel this side does not have.
        h.flow.negotiated = true;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, flow_frame(FlowControlAction::Pause, conn(999)))
                .await
                .unwrap();
        }
        assert!(!gate.paused(), "unknown id must not disturb live gates");
    }

    #[tokio::test]
    async fn scope_less_pause_fans_out_to_every_live_channel() {
        let mut h = Harness::new(None).await;
        h.enable_flow();
        let g1: Arc<rx11_core::flow::EgressGate> = Default::default();
        let g2: Arc<rx11_core::flow::EgressGate> = Default::default();
        let big = || rx11_core::flow::FlowMeter::new(1 << 30, 1 << 20);
        let _a = h
            .register_local_with(conn(34), big(), Arc::clone(&g1))
            .await;
        let _b = h
            .register_local_with(conn(35), big(), Arc::clone(&g2))
            .await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::FlowControl(rx11_core::protocol::FlowControlMessage {
                    action: FlowControlAction::Pause,
                    connection_id: None,
                }),
            )
            .await
            .unwrap();
        }
        assert!(g1.paused() && g2.paused(), "both gates closed");
    }

    #[tokio::test]
    async fn tick_reconciles_a_lost_resume_report() {
        let mut h = Harness::new(None).await;
        h.enable_flow();
        let meter = rx11_core::flow::FlowMeter::new(10, 4);
        let _rx = h
            .register_local_with(conn(36), Arc::clone(&meter), Default::default())
            .await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, data_frame(conn(36), 12))
                .await
                .unwrap();
        }
        assert!(matches!(
            h.take_priority_frame(),
            Some(Frame::FlowControl(_))
        ));
        // The pump drained everything but its report channel DROPPED the
        // edge (simulated by never sending it): the gauge is the ground
        // truth, so the next tick must resume on its own.
        meter.discharge(12);
        {
            let mut ctx = h.ctx();
            re_advertise(&mut ctx).await;
        }
        let mut saw_resume = false;
        while let Some(frame) = h.take_priority_frame() {
            if let Frame::FlowControl(m) = frame {
                if m.action == FlowControlAction::Resume {
                    saw_resume = true;
                }
            }
        }
        assert!(saw_resume, "tick re-advertises a lost resume edge");
        assert!(!h.flow.advertiser.is_paused(conn(36)));
    }

    #[tokio::test]
    async fn channel_close_forgets_advertisement_state() {
        let mut h = Harness::new(None).await;
        h.enable_flow();
        let meter = rx11_core::flow::FlowMeter::new(10, 4);
        let _rx = h
            .register_local_with(conn(37), Arc::clone(&meter), Default::default())
            .await;
        {
            let mut ctx = h.ctx();
            dispatch_frame(&mut ctx, data_frame(conn(37), 12))
                .await
                .unwrap();
        }
        assert!(h.flow.advertiser.is_paused(conn(37)));
        assert!(matches!(
            h.take_priority_frame(),
            Some(Frame::FlowControl(_))
        ));
        {
            let mut ctx = h.ctx();
            dispatch_frame(
                &mut ctx,
                Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: conn(37),
                }),
            )
            .await
            .unwrap();
            re_advertise(&mut ctx).await;
        }
        assert!(!h.flow.advertiser.is_paused(conn(37)));
        // Forget means the keepalive tick goes silent for it.
        assert!(h.take_priority_frame().is_none());
    }
}
