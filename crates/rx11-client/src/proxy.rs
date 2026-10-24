//! Per-application X11 proxy on the client side: owns the local X server
//! socket and pumps data in both directions against the relay connection.
//!
//! Each connection carries its flow-control pair: a [`FlowMeter`] gauging
//! downlink bytes accepted-but-not-written-to-the-local-X-server (its edges
//! are advertised to the relay as pause/resume), and an [`EgressGate`] the
//! uplink read awaits when the relay has paused this direction. It also
//! carries an `uplink_reset` flag: the serve loop raises it when the relay
//! reports a diff desync on our uplink, and the pump — sole owner of the
//! uplink diff base — drops that base before building its next frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error};

use rx11_core::config::BufferDefaults;
use rx11_core::dataplane::DataPlane;
use rx11_core::flow::{EgressGate, FlowEdge, FlowMeter};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{Capability, ChannelCloseMessage, Frame};
use rx11_core::queue::OutboundSender;
use rx11_core::stats::ConnectionStats;
use rx11_core::types::ConnectionId;
use rx11_core::x11recode::DownstreamExpander;

/// Live local X connections: `connection_id -> (write channel, pump task,
/// downlink gauge, uplink gate, uplink-diff reset flag)`.
pub(crate) type X11ConnMap = HashMap<
    u32,
    (
        mpsc::Sender<bytes::Bytes>,
        JoinHandle<()>,
        Arc<FlowMeter>,
        Arc<EgressGate>,
        Arc<AtomicBool>,
    ),
>;
pub(crate) type SharedX11Conns = Arc<tokio::sync::Mutex<X11ConnMap>>;

pub(crate) fn new_shared_conns() -> SharedX11Conns {
    Arc::new(tokio::sync::Mutex::new(HashMap::new()))
}

/// Shared dependencies for proxying relay frames to local X connections.
pub(crate) struct ProxyDeps<'a> {
    pub outbound: &'a OutboundSender,
    pub x11_connections: &'a SharedX11Conns,
    pub stats: &'a Arc<ConnectionStats>,
    pub dataplane: &'a DataPlane,
    pub seq: &'a Arc<AtomicU32>,
    pub local_x11_addr: &'a str,
    /// Runtime profile: frame dispatch consults it when opening the audio
    /// outlet (player resolution), everything else rides the dataplane.
    pub runtime: &'a rx11_core::config::RuntimeConfig,
    /// Resume edges detected at the write-back site (the pump cannot claim
    /// the advertiser between loop iterations, so it reports and the serve
    /// loop decides).
    pub flow_edges: &'a mpsc::Sender<FlowEdge>,
}

/// Open the local X server socket for a relayed `ChannelOpen` and spawn the
/// pump task. Connection errors are logged and reported to the relay; only
/// socket option failures abort the session (matching previous behavior).
pub(crate) async fn spawn_local_connection(
    conn_id: ConnectionId,
    deps: &ProxyDeps<'_>,
) -> anyhow::Result<()> {
    let local_stream = match TcpStream::connect(deps.local_x11_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to local X Server for {}: {}", conn_id, e);
            deps.stats.dec_x11_connections();
            let _ = deps
                .outbound
                .send(Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: conn_id,
                }))
                .await;
            return Ok(());
        }
    };
    local_stream.set_nodelay(true)?;

    let (mut local_read, mut local_write) = tokio::io::split(local_stream);
    let (write_tx, mut write_rx) = mpsc::channel::<bytes::Bytes>(BufferDefaults::CHANNEL_BUFFER);

    // Flow pair for this channel: the gauge below feeds pause/resume
    // advertisements; the gate is flipped by the serve loop when the relay
    // pauses our uplink. `uplink_reset` is raised by the serve loop when the
    // relay reports a desync on OUR diffs; the pump (its sole owner below)
    // then drops the base and resends whole payloads.
    let meter = FlowMeter::from_runtime(deps.runtime);
    let gate = EgressGate::new();
    let uplink_reset = Arc::new(AtomicBool::new(false));

    let outbound = deps.outbound.clone();
    let stats = Arc::clone(deps.stats);
    let conns = Arc::clone(deps.x11_connections);
    let seq = Arc::clone(deps.seq);
    let dataplane = deps.dataplane.clone(); // Owned by the spawned task.
    let flow_edges = deps.flow_edges.clone();
    let pump_meter = Arc::clone(&meter);
    let pump_gate = Arc::clone(&gate);
    let pump_reset = Arc::clone(&uplink_reset);
    // The pump is the only owner of the uplink diff base, so the base (and
    // the runtime knobs that parameterize it) move into the task. When the
    // direction was not negotiated there is no base to keep at all.
    let diffing = dataplane.may_diff_send();
    let uplink_runtime = diffing.then(|| deps.runtime.clone());

    let handle = tokio::spawn(async move {
        let ctx = ProxyContext {
            conn_id,
            local_read: &mut local_read,
            local_write: &mut local_write,
            relay_rx: &mut write_rx,
            outbound: &outbound,
            stats: &stats,
            dataplane: &dataplane,
            seq: &seq,
            meter: pump_meter,
            gate: pump_gate,
            uplink_reset: pump_reset,
            uplink_runtime,
            flow_edges,
        };
        pump_local_x11_connection(ctx).await;

        let _ = outbound
            .send(Frame::ChannelClose(ChannelCloseMessage {
                connection_id: conn_id,
            }))
            .await;
        conns.lock().await.remove(&conn_id.get());
        stats.dec_x11_connections();
    });

    deps.x11_connections
        .lock()
        .await
        .insert(conn_id.get(), (write_tx, handle, meter, gate, uplink_reset));
    Ok(())
}

struct ProxyContext<'a> {
    conn_id: ConnectionId,
    local_read: &'a mut tokio::io::ReadHalf<TcpStream>,
    local_write: &'a mut tokio::io::WriteHalf<TcpStream>,
    relay_rx: &'a mut mpsc::Receiver<bytes::Bytes>,
    outbound: &'a OutboundSender,
    stats: &'a ConnectionStats,
    dataplane: &'a DataPlane,
    seq: &'a Arc<AtomicU32>,
    /// Downlink gauge: discharged here after the local write completes; the
    /// resume edge is reported to the serve loop (never sent from here —
    /// advertisement state is single-owner).
    meter: Arc<FlowMeter>,
    /// Uplink latch: while the relay holds it closed we do not read the
    /// local X server; its output stalls in kernel buffers, losslessly.
    gate: Arc<EgressGate>,
    /// Raised by the serve loop when the relay could not apply one of our
    /// diffs. Checked (and cleared) by the uplink arm before building the
    /// next frame — a plain atomic, never a lock held across an await.
    uplink_reset: Arc<AtomicBool>,
    /// Some iff the uplink diff direction was negotiated; the pump builds
    /// its per-connection base cache from it.
    uplink_runtime: Option<rx11_core::config::RuntimeConfig>,
    flow_edges: mpsc::Sender<FlowEdge>,
}

/// Relay both directions until either side closes; buffers grow
/// geometrically up to the proxy read limit.
///
/// The two directions run as INDEPENDENT futures polled concurrently (the
/// server-side listener has the same shape). This is load-bearing, not
/// cosmetic: with arm bodies, a blocked downlink write (`write_all` to the
/// local X server) stops polling the uplink read — and a local X server
/// whose response write waits on that read stops draining the downlink.
/// That three-party wait (our write ← its read ← its write ← our read) is
/// a buffer-exhaustion deadlock, reachable whenever both directions run
/// bulk at once (an echoing fake X server in tests found it at ~10%
/// frequency). Independent legs break the cycle: reading keeps flowing
/// while writing waits.
async fn pump_local_x11_connection(ctx: ProxyContext<'_>) {
    let ProxyContext {
        conn_id,
        local_read,
        local_write,
        relay_rx,
        outbound,
        stats,
        dataplane,
        seq,
        meter,
        gate,
        uplink_reset,
        uplink_runtime,
        flow_edges,
    } = ctx;
    let dp_up = dataplane;
    let dp_down = dataplane;

    let uplink = async move {
        // The pump owns the uplink diff base outright: one cache per channel,
        // never shared, so the hot path takes no lock. The serve loop's only
        // say in it is the `uplink_reset` flag below.
        //
        // Sequence numbers come from the base cache when diffing (per
        // connection, the same rule the relay's downlink uses) because a diff
        // names the base sequence it applies to; without diffs the channel
        // keeps the session-wide counter, byte-identical to before.
        let mut base = uplink_runtime.map(|rt| ConnectionDataCache::from_runtime(&rt));
        // BytesMut accumulation: kernel read lands directly in the buffer
        // that becomes the frame payload (split().freeze() is a refcount,
        // not a memcpy), saving one full payload copy per read.
        let mut buf = bytes::BytesMut::with_capacity(BufferDefaults::INITIAL_READ_BUF);
        loop {
            let want = (buf.capacity() * 2).min(BufferDefaults::PROXY_READ_BUF_MAX);
            if buf.capacity() < want {
                buf.reserve(want - buf.capacity());
            }
            // Flow-control enforcement: while the relay holds this
            // channel's uplink paused, stop reading the local X server.
            // Responses and events wait in kernel buffers (the lossless
            // place to stall); overshoot is at most one in-flight chunk.
            gate.wait_open().await;
            match local_read.read_buf(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let data = buf.split().freeze();
                    // The relay could not apply one of our previous diffs:
                    // forget the base so this payload ships whole and future
                    // diffs re-apply against it. The swap consumes the flag,
                    // so a reset raised mid-frame is honored next iteration.
                    if base.is_some() && uplink_reset.swap(false, Ordering::AcqRel) {
                        debug!("Uplink diff base reset for {conn_id} by relay resync");
                        if let Some(cache) = base.as_mut() {
                            cache.clear_connection(conn_id);
                        }
                    }
                    let seq_id = match base.as_mut() {
                        Some(cache) => cache.next_sequence_for(conn_id),
                        None => seq.fetch_add(1, Ordering::Relaxed),
                    };

                    // build_frame owns outbound stats (wire bytes plus
                    // compression and incremental savings for the shape
                    // actually emitted).
                    let frame =
                        dp_up.build_frame(conn_id, seq_id, data, base.as_mut(), Some(stats));
                    if outbound.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    debug!("Read error from local X Server ({conn_id}): {e}");
                    break;
                }
            }
        }
    };

    let downlink = async move {
        while let Some(data) = relay_rx.recv().await {
            let len = data.len();
            // With `x11_recode` the downstream payloads are framed units
            // (raw spans / image containers); the local X server must
            // receive the ORIGINAL request bytes. An unexpandable
            // container means bytes would be lost: fail the connection
            // (reconnect-with-resume rebuilds cleanly).
            let out = if dp_down.features.supports(Capability::X11Recode) {
                match DownstreamExpander::expand(data) {
                    Some(b) => b,
                    None => {
                        error!("Recode container unusable for {conn_id}; failing session");
                        break;
                    }
                }
            } else {
                data
            };
            if local_write.write_all(&out).await.is_err() || local_write.flush().await.is_err() {
                break;
            }
            // Bytes are now the local X server's problem: the downlink
            // backlog shrank. A resume edge is reported (never sent from
            // here — advertisement state is single-owner); losing a
            // report against the bounded channel would strand the pause,
            // so the serve loop also reconciles from the meter itself.
            if meter.discharge(len) {
                let _ = flow_edges.try_send(FlowEdge::resume(conn_id));
            }
        }
    };

    tokio::select! {
        _ = uplink => {}
        _ = downlink => {}
    }
}

/// Route decompressed relay payload to the matching local connection,
/// charging its downlink flow gauge. `None` when the channel is gone or
/// stale (the caller prunes); `Some((bytes, pause_edge))` on acceptance.
pub(crate) async fn send_data_to_local(
    x11_connections: &SharedX11Conns,
    conn_id: ConnectionId,
    data: bytes::Bytes,
) -> Option<(usize, bool)> {
    let route = {
        let conns = x11_connections.lock().await;
        conns
            .get(&conn_id.get())
            .map(|(tx, _, meter, _, _)| (tx.clone(), Arc::clone(meter)))
    };

    let (tx, meter) = route?;
    let len = data.len();
    if tx.send(data).await.is_err() {
        debug!("Local X11 connection gone for {}", conn_id);
        x11_connections.lock().await.remove(&conn_id.get());
        return None;
    }
    Some((len, meter.charge(len)))
}

pub(crate) async fn cleanup_connections(
    x11_connections: &SharedX11Conns,
    stats: &Arc<ConnectionStats>,
) {
    let mut conns = x11_connections.lock().await;
    let count = conns.len() as u32;
    for _ in 0..count {
        stats.dec_x11_connections();
    }
    for (_, entry) in conns.drain() {
        entry.1.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future;

    async fn stub_handle() -> JoinHandle<()> {
        tokio::spawn(future::pending::<()>())
    }

    #[tokio::test]
    async fn send_data_to_local_delivers_to_live_channel() {
        let conns = new_shared_conns();
        let (tx, mut rx) = mpsc::channel::<bytes::Bytes>(4);
        conns.lock().await.insert(
            12,
            (
                tx,
                stub_handle().await,
                idle_flow(),
                open_gate(),
                no_reset(),
            ),
        );
        let (n, edge) = send_data_to_local(
            &conns,
            ConnectionId::new(12),
            bytes::Bytes::from_static(&[7, 7]),
        )
        .await
        .expect("delivered");
        assert_eq!((n, edge), (2, false));
        assert_eq!(&rx.recv().await.unwrap()[..], &[7, 7]);
    }

    /// Flow gauge too high to ever trip a real edge in functional tests.
    fn idle_flow() -> Arc<FlowMeter> {
        FlowMeter::new(1 << 30, 1 << 20)
    }
    fn open_gate() -> Arc<EgressGate> {
        EgressGate::new()
    }
    /// Uplink-diff reset flag in its resting state.
    fn no_reset() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[tokio::test]
    async fn send_data_to_local_prunes_gone_receiver() {
        let conns = new_shared_conns();
        let (tx, rx) = mpsc::channel::<bytes::Bytes>(4);
        drop(rx); // pump task vanished
        conns.lock().await.insert(
            11,
            (
                tx,
                stub_handle().await,
                idle_flow(),
                open_gate(),
                no_reset(),
            ),
        );

        let out = send_data_to_local(
            &conns,
            ConnectionId::new(11),
            bytes::Bytes::from_static(&[1]),
        )
        .await;
        assert!(out.is_none(), "gone channel reports None");
        assert!(conns.lock().await.is_empty(), "stale entry must be pruned");
    }

    #[tokio::test]
    async fn send_data_to_local_charges_the_meter_and_reports_the_pause_edge() {
        let conns = new_shared_conns();
        let (tx, _rx) = mpsc::channel::<bytes::Bytes>(8);
        let meter = FlowMeter::new(10, 4);
        conns.lock().await.insert(
            13,
            (
                tx,
                stub_handle().await,
                Arc::clone(&meter),
                open_gate(),
                no_reset(),
            ),
        );

        let (_, edge) = send_data_to_local(
            &conns,
            ConnectionId::new(13),
            bytes::Bytes::from(vec![0u8; 6]),
        )
        .await
        .unwrap();
        assert!(!edge, "below high: no edge");
        let (n, edge) = send_data_to_local(
            &conns,
            ConnectionId::new(13),
            bytes::Bytes::from(vec![0u8; 6]),
        )
        .await
        .unwrap();
        assert_eq!(n, 6);
        assert!(edge, "crossing high reports the pause edge exactly once");
        assert_eq!(meter.pending(), 12);
    }

    #[tokio::test]
    async fn send_data_to_local_unknown_connection_is_noop() {
        let conns = new_shared_conns();
        let out = send_data_to_local(
            &conns,
            ConnectionId::new(99),
            bytes::Bytes::from_static(&[1]),
        )
        .await;
        assert!(out.is_none());
        assert!(conns.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cleanup_connections_aborts_tasks_and_decrements_stats() {
        let conns = new_shared_conns();
        let stats = Arc::new(ConnectionStats::new());
        for id in 1..=2u32 {
            let (tx, _rx) = mpsc::channel::<bytes::Bytes>(1);
            conns.lock().await.insert(
                id,
                (
                    tx,
                    stub_handle().await,
                    idle_flow(),
                    open_gate(),
                    no_reset(),
                ),
            );
            stats.inc_x11_connections();
        }

        cleanup_connections(&conns, &stats).await;

        assert!(conns.lock().await.is_empty());
        assert!(stats.summary().contains("0 active X11 connections"));
    }
}
