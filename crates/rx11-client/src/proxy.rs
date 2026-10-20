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

use rx11_core::compress::{maybe_compress_frame, CompressionAlgo};
use rx11_core::config::BufferDefaults;
use rx11_core::protocol::{Frame, X11DisconnectMessage};
use rx11_core::stats::ConnectionStats;
use rx11_core::types::{ConnectionId, DisplayNumber};

/// Live local X connections: `connection_id -> (write channel, pump task)`.
pub(crate) type X11ConnMap = HashMap<u32, (mpsc::Sender<bytes::Bytes>, JoinHandle<()>)>;
pub(crate) type SharedX11Conns = Arc<tokio::sync::Mutex<X11ConnMap>>;

pub(crate) fn new_shared_conns() -> SharedX11Conns {
    Arc::new(tokio::sync::Mutex::new(HashMap::new()))
}

/// Shared dependencies for proxying relay frames to local X connections.
pub(crate) struct ProxyDeps<'a> {
    pub outbound: &'a mpsc::Sender<Frame>,
    pub x11_connections: &'a SharedX11Conns,
    pub stats: &'a Arc<ConnectionStats>,
    pub compression: Option<CompressionAlgo>,
    pub seq: &'a Arc<AtomicU32>,
    pub local_x11_addr: &'a str,
}

/// Open the local X server socket for a relayed `X11Connect` and spawn the
/// pump task. Connection errors are logged and reported to the relay; only
/// socket option failures abort the session (matching previous behavior).
pub(crate) async fn spawn_local_connection(
    conn_id: ConnectionId,
    display: DisplayNumber,
    deps: &ProxyDeps<'_>,
) -> anyhow::Result<()> {
    let local_stream = match TcpStream::connect(deps.local_x11_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to local X Server for {}: {}", conn_id, e);
            deps.stats.dec_x11_connections();
            let _ = deps
                .outbound
                .send(Frame::X11Disconnect(X11DisconnectMessage {
                    display,
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
    let compression = deps.compression;

    let handle = tokio::spawn(async move {
        let ctx = ProxyContext {
            conn_id,
            local_read: &mut local_read,
            local_write: &mut local_write,
            relay_rx: &mut write_rx,
            outbound: &outbound,
            stats: &stats,
            compression,
            seq: &seq,
        };
        pump_local_x11_connection(ctx).await;

        let _ = outbound
            .send(Frame::X11Disconnect(X11DisconnectMessage {
                display,
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
    outbound: &'a mpsc::Sender<Frame>,
    stats: &'a ConnectionStats,
    compression: Option<CompressionAlgo>,
    seq: &'a Arc<AtomicU32>,
}

/// Relay both directions until either side closes; buffers grow geometrically
/// up to the proxy read limit.
async fn pump_local_x11_connection(ctx: ProxyContext<'_>) {
    let mut buf = vec![0u8; BufferDefaults::INITIAL_READ_BUF];
    loop {
        tokio::select! {
            result = ctx.local_read.read(&mut buf) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                        let seq_id = ctx.seq.fetch_add(1, Ordering::Relaxed);

                        let frame = maybe_compress_frame(
                            ctx.conn_id,
                            seq_id,
                            data.clone(),
                            ctx.compression,
                        );
                        if ctx.outbound.send(frame).await.is_err() {
                            break;
                        }
                        if ctx.compression.is_some()
                            && data.len() >= rx11_core::compress::COMPRESSION_THRESHOLD
                        {
                            ctx.stats
                                .add_compression_saved(data.len().saturating_sub(n) as u64);
                        }
                        ctx.stats.add_bytes_sent(n as u64);

                        if buf.len() < BufferDefaults::PROXY_READ_BUF_MAX {
                            let new_size = (buf.len() * 2).min(BufferDefaults::PROXY_READ_BUF_MAX);
                            buf.resize(new_size, 0);
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
                        if ctx.local_write.write_all(&data).await.is_err()
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
