//! Per-application X11 proxy on the client side: owns the local X server
//! socket and pumps data in both directions against the relay connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error};

use rx11_core::config::BufferDefaults;
use rx11_core::dataplane::DataPlane;
use rx11_core::protocol::{Capability, ChannelCloseMessage, Frame};
use rx11_core::queue::OutboundSender;
use rx11_core::stats::ConnectionStats;
use rx11_core::types::ConnectionId;
use rx11_core::x11recode::DownstreamExpander;

/// Live local X connections: `connection_id -> (write channel, pump task)`.
pub(crate) type X11ConnMap = HashMap<u32, (mpsc::Sender<bytes::Bytes>, JoinHandle<()>)>;
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

    let outbound = deps.outbound.clone();
    let stats = Arc::clone(deps.stats);
    let conns = Arc::clone(deps.x11_connections);
    let seq = Arc::clone(deps.seq);
    let dataplane = deps.dataplane.clone(); // Owned by the spawned task.

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
        .insert(conn_id.get(), (write_tx, handle));
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
}

/// Relay both directions until either side closes; buffers grow geometrically
/// up to the proxy read limit.
async fn pump_local_x11_connection(ctx: ProxyContext<'_>) {
    // BytesMut accumulation: kernel read lands directly in the buffer that
    // becomes the frame payload (split().freeze() is a refcount, not a
    // memcpy), saving one full payload copy per read.
    let mut buf = bytes::BytesMut::with_capacity(BufferDefaults::INITIAL_READ_BUF);
    loop {
        let want = (buf.capacity() * 2).min(BufferDefaults::PROXY_READ_BUF_MAX);
        if buf.capacity() < want {
            buf.reserve(want - buf.capacity());
        }
        tokio::select! {
            result = ctx.local_read.read_buf(&mut buf) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {
                        let data = buf.split().freeze();
                        let seq_id = ctx.seq.fetch_add(1, Ordering::Relaxed);

                        // build_frame owns outbound stats (wire bytes plus
                        // compression savings for the emitted shape).
                        let frame = ctx.dataplane.build_frame(
                            ctx.conn_id,
                            seq_id,
                            data,
                            None,
                            Some(ctx.stats),
                        );
                        if ctx.outbound.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!("Read error from local X Server ({}): {}", ctx.conn_id, e);
                        break;
                    }
                }
            }
            data = ctx.relay_rx.recv() => {
                match data {
                    Some(data) => {
                        // With `x11_recode` the downstream payloads are
                        // framed units (raw spans / image containers);
                        // the local X server must receive the ORIGINAL
                        // request bytes. An unexpandable container means
                        // bytes would be lost: fail the connection
                        // (reconnect-with-resume rebuilds cleanly).
                        let out = if ctx.dataplane.features.supports(Capability::X11Recode) {
                            match DownstreamExpander::expand(data) {
                                Some(b) => b,
                                None => {
                                    error!("Recode container unusable for {}; failing session", ctx.conn_id);
                                    break;
                                }
                            }
                        } else {
                            data
                        };
                        if ctx.local_write.write_all(&out).await.is_err()
                            || ctx.local_write.flush().await.is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

/// Route decompressed relay payload to the matching local connection,
/// dropping stale map entries when the pump task is gone.
pub(crate) async fn send_data_to_local(
    x11_connections: &SharedX11Conns,
    conn_id: ConnectionId,
    data: bytes::Bytes,
) {
    let tx = {
        let conns = x11_connections.lock().await;
        conns.get(&conn_id.get()).map(|(tx, _)| tx.clone())
    };

    if let Some(tx) = tx {
        if tx.send(data).await.is_err() {
            debug!("Local X11 connection gone for {}", conn_id);
            x11_connections.lock().await.remove(&conn_id.get());
        }
    } else {
        debug!("No local connection for {}", conn_id);
    }
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
    for (_, (_, handle)) in conns.drain() {
        handle.abort();
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
        conns.lock().await.insert(12, (tx, stub_handle().await));
        send_data_to_local(
            &conns,
            ConnectionId::new(12),
            bytes::Bytes::from_static(&[7, 7]),
        )
        .await;
        assert_eq!(&rx.recv().await.unwrap()[..], &[7, 7]);
    }

    #[tokio::test]
    async fn send_data_to_local_prunes_gone_receiver() {
        let conns = new_shared_conns();
        let (tx, rx) = mpsc::channel::<bytes::Bytes>(4);
        drop(rx); // pump task vanished
        conns.lock().await.insert(11, (tx, stub_handle().await));

        send_data_to_local(
            &conns,
            ConnectionId::new(11),
            bytes::Bytes::from_static(&[1]),
        )
        .await;
        assert!(conns.lock().await.is_empty(), "stale entry must be pruned");
    }

    #[tokio::test]
    async fn send_data_to_local_unknown_connection_is_noop() {
        let conns = new_shared_conns();
        send_data_to_local(
            &conns,
            ConnectionId::new(99),
            bytes::Bytes::from_static(&[1]),
        )
        .await;
        assert!(conns.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cleanup_connections_aborts_tasks_and_decrements_stats() {
        let conns = new_shared_conns();
        let stats = Arc::new(ConnectionStats::new());
        for id in 1..=2u32 {
            let (tx, _rx) = mpsc::channel::<bytes::Bytes>(1);
            conns.lock().await.insert(id, (tx, stub_handle().await));
            stats.inc_x11_connections();
        }

        cleanup_connections(&conns, &stats).await;

        assert!(conns.lock().await.is_empty());
        assert!(stats.summary().contains("0 active X11 connections"));
    }
}
