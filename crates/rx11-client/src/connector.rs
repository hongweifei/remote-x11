use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use rand::RngExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{error, info, warn, debug};

use rx11_core::auth::generate_display_cookie;
use rx11_core::protocol::*;
use rx11_core::stats::ConnectionStats;
use rx11_core::transport::Rx11Transport;

type X11ConnMap = HashMap<u32, (tokio::sync::mpsc::Sender<bytes::Bytes>, JoinHandle<()>)>;
type SharedX11Conns = Arc<Mutex<X11ConnMap>>;

const CLIENT_READ_TIMEOUT_SECS: u64 = 100;
const INITIAL_BUF_SIZE: usize = 64 * 1024;
const MAX_BUF_SIZE: usize = 256 * 1024;

pub struct LocalConnector {
    relay_addr: String,
    auth_token: String,
    local_x11_addr: String,
    display: Option<u16>,
    auto_display: bool,
    max_retries: u32,
    retry_base_delay: Duration,
    retry_max_delay: Duration,
}

impl LocalConnector {
    pub fn new(
        relay_addr: String,
        auth_token: String,
        local_x11_addr: String,
        display: Option<u16>,
        auto_display: bool,
    ) -> Self {
        Self {
            relay_addr,
            auth_token,
            local_x11_addr,
            display,
            auto_display,
            max_retries: 10,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
        }
    }

    pub async fn connect_and_serve(&self) -> anyhow::Result<()> {
        let mut attempt: u32 = 0;
        let mut last_session_id: Option<String> = None;
        loop {
            let sid = last_session_id.take();
            if let Err(e) = self.connect_and_serve_inner(sid, &mut last_session_id).await {
                let retriable = e
                    .downcast_ref::<rx11_core::error::Rx11Error>()
                    .map(|re| re.is_retriable())
                    .unwrap_or(true);

                if !retriable {
                    return Err(e);
                }

                attempt += 1;
                if attempt > self.max_retries {
                    error!("Max reconnection attempts ({}) reached", self.max_retries);
                    return Err(e);
                }

                let delay = self.calculate_backoff(attempt);
                warn!(
                    "Connection lost ({}), reconnecting in {:?} (attempt {}/{})",
                    e, delay, attempt, self.max_retries
                );
                tokio::time::sleep(delay).await;
            } else {
                return Ok(());
            }
        }
    }

    fn calculate_backoff(&self, attempt: u32) -> Duration {
        let base_ms = self.retry_base_delay.as_millis() as u64;
        let max_ms = self.retry_max_delay.as_millis() as u64;
        let backoff_ms = base_ms.saturating_mul(1u64 << attempt.min(10));
        let capped_ms = backoff_ms.min(max_ms);
        let jitter = rand::rng().random_range(0..=(capped_ms / 4));
        Duration::from_millis((capped_ms + jitter).min(max_ms))
    }

    async fn create_session(
        &self,
        transport: &mut Rx11Transport,
        display: Option<u16>,
    ) -> anyhow::Result<(u16, String)> {
        let cookie = generate_display_cookie();
        if self.auto_display {
            transport
                .send_frame(&Frame::SessionAutoCreate(SessionAutoCreateMessage {
                    auth_name: "MIT-MAGIC-COOKIE-1".to_string(),
                    auth_data: cookie,
                }))
                .await?;
        } else {
            let disp = display.unwrap_or(0);
            if disp > MAX_DISPLAY_NUMBER {
                anyhow::bail!("Display number must be 0-{}, got {}", MAX_DISPLAY_NUMBER, disp);
            }
            transport
                .send_frame(&Frame::SessionCreate(SessionCreateMessage {
                    display: disp,
                    auth_name: "MIT-MAGIC-COOKIE-1".to_string(),
                    auth_data: cookie,
                }))
                .await?;
        }

        let session_ack = transport.recv_frame().await?;
        match session_ack {
            Frame::SessionAck(ack) => {
                if !ack.success {
                    return Err(anyhow::anyhow!(
                        "Session create failed: {}",
                        ack.error_msg.as_deref().unwrap_or("unknown error")
                    ));
                }
                info!("Session created for display :{}", ack.display);
                let sid = ack.session_id.ok_or_else(|| anyhow::anyhow!("Missing session_id"))?;
                Ok((ack.display, sid))
            }
            _ => Err(anyhow::anyhow!("Expected SessionAck")),
        }
    }

    async fn connect_and_serve_inner(
        &self,
        resume_session_id: Option<String>,
        saved_session_id: &mut Option<String>,
    ) -> anyhow::Result<()> {
        info!("Connecting to relay at {}", self.relay_addr);
        let stream = tokio::time::timeout(
            Duration::from_secs(30),
            TcpStream::connect(&self.relay_addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("TCP connect to {} timed out", self.relay_addr))??;
        let mut transport = Rx11Transport::new(stream)?;

        transport
            .send_frame(&Frame::Hello(HelloMessage {
                version: PROTOCOL_VERSION,
                mode: ConnectionMode::Client,
                resume_session_id: resume_session_id.clone(),
                compression_algos: rx11_core::compress::CompressionAlgo::ALL.to_vec(),
            }))
            .await?;

        let ack = transport.recv_frame().await?;
        let compression_algo: Option<rx11_core::compress::CompressionAlgo>;
        match ack {
            Frame::HelloAck(hello_ack) => {
                if !hello_ack.success {
                    return Err(anyhow::anyhow!(
                        "Handshake failed: {}",
                        hello_ack.error_msg.as_deref().unwrap_or("unknown error")
                    ));
                }
                if hello_ack.version != PROTOCOL_VERSION {
                    return Err(anyhow::anyhow!(
                        "Protocol version mismatch: server {} client {}",
                        hello_ack.version,
                        PROTOCOL_VERSION
                    ));
                }
                compression_algo = hello_ack.compression;
                info!(
                    "Connected to relay, transport_id={}, compression={}",
                    hello_ack.session_id,
                    compression_algo.map(|a| a.as_str()).unwrap_or("disabled")
                );
            }
            _ => return Err(anyhow::anyhow!("Expected HelloAck")),
        }

        transport
            .send_frame(&Frame::AuthRequest(AuthRequestMessage {
                token: self.auth_token.clone(),
            }))
            .await?;

        let auth_resp = transport.recv_frame().await?;
        match auth_resp {
            Frame::AuthResponse(resp) => {
                if !resp.success {
                    return Err(anyhow::anyhow!(
                        "Auth failed: {}",
                        resp.error_msg.as_deref().unwrap_or("unknown error")
                    ));
                }
                info!("Authenticated successfully");
            }
            _ => return Err(anyhow::anyhow!("Expected AuthResponse")),
        }

        rx11_core::protocol::validate_token_len(&self.auth_token)?;

        let actual_display: u16;

        if let Some(ref sid) = resume_session_id {
            transport
                .send_frame(&Frame::SessionResume(SessionResumeMessage {
                    session_id: sid.clone(),
                }))
                .await?;

            let resume_ack = transport.recv_frame().await?;
            match resume_ack {
                Frame::SessionAck(ack) => {
                    if !ack.success {
                        warn!(
                            "Session resume failed: {}, creating new session",
                            ack.error_msg.as_deref().unwrap_or("unknown error")
                        );
                        let (disp, new_sid) = self.create_session(&mut transport, self.display).await?;
                        actual_display = disp;
                        *saved_session_id = Some(new_sid);
                    } else {
                        actual_display = ack.display;
                        info!("Session resumed for display :{}", ack.display);
                        *saved_session_id = ack.session_id.clone();
                    }
                }
                _ => return Err(anyhow::anyhow!("Expected SessionAck for resume")),
            }
        } else {
            let (disp, sid) = self.create_session(&mut transport, self.display).await?;
            actual_display = disp;
            *saved_session_id = Some(sid);
        }

        let (mut read_half, mut write_half) = transport.split();

        let x11_connections: SharedX11Conns = Arc::new(Mutex::new(HashMap::new()));
        let stats = Arc::new(ConnectionStats::new());
        let seq_counter = Arc::new(AtomicU32::new(1));

        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<Frame>(512);

        let stats_clone = stats.clone();
        let stats_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            loop {
                interval.tick().await;
                info!("[rx11] Status: connected | {}", stats_clone.summary());
            }
        });

        let sender_task = tokio::spawn(async move {
            while let Some(frame) = outbound_rx.recv().await {
                if write_half.send_frame(&frame).await.is_err() {
                    break;
                }
            }
        });

        info!(
            "Proxying X11 data: relay <-> local X server at {}",
            self.local_x11_addr
        );

        let result: anyhow::Result<()> = async {
            loop {
                tokio::select! {
                    frame_result = tokio::time::timeout(
                        std::time::Duration::from_secs(CLIENT_READ_TIMEOUT_SECS),
                        read_half.recv_frame()
                    ) => {
                        match frame_result {
                            Ok(Ok(Frame::X11Connect(msg))) => {
                                let conn_id = msg.connection_id;
                                let display = msg.display;
                                info!("X11 client connected (connection_id={})", conn_id);
                                stats.inc_x11_connections();

                                match TcpStream::connect(&self.local_x11_addr).await {
                                    Ok(local_stream) => {
                                        local_stream.set_nodelay(true)?;
                                        let (mut local_read, mut local_write) = tokio::io::split(local_stream);

                                        let (write_tx, mut write_rx) =
                                            tokio::sync::mpsc::channel::<bytes::Bytes>(512);

                                        let outbound = outbound_tx.clone();
                                        let stats_clone = stats.clone();
                                        let x11_conns_clone = x11_connections.clone();
                                        let compress = compression_algo;
                                        let seq = seq_counter.clone();

                                        let handle = tokio::spawn(async move {
                                            let mut buf = vec![0u8; INITIAL_BUF_SIZE];
                                            loop {
                                                tokio::select! {
                                                    result = local_read.read(&mut buf) => {
                                                        match result {
                                                            Ok(0) => break,
                                                            Ok(n) => {
                                                                let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                                                                let seq_id = seq.fetch_add(1, Ordering::Relaxed);
                                                                let frame = if let Some(algo) = compress {
                                                                    if let Some(compressed) = algo.compress_to_bytes(&data) {
                                                                        stats_clone.add_compression_saved((data.len() - compressed.len()) as u64);
                                                                        Frame::CompressedDataX11 {
                                                                            connection_id: conn_id,
                                                                            sequence_id: seq_id,
                                                                            original_len: data.len(),
                                                                            data: compressed,
                                                                        }
                                                                    } else {
                                                                        Frame::DataX11(X11DataMessage {
                                                                            connection_id: conn_id,
                                                                            sequence_id: seq_id,
                                                                            data,
                                                                        })
                                                                    }
                                                                } else {
                                                                    Frame::DataX11(X11DataMessage {
                                                                        connection_id: conn_id,
                                                                        sequence_id: seq_id,
                                                                        data,
                                                                    })
                                                                };
                                                                if outbound.send(frame).await.is_err() {
                                                                    break;
                                                                }
                                                                stats_clone.add_bytes_sent(n as u64);
                                                                if buf.len() < MAX_BUF_SIZE {
                                                                    let new_size = (buf.len() * 2).min(MAX_BUF_SIZE);
                                                                    buf.resize(new_size, 0);
                                                                }
                                                            }
                                                            Err(e) => {
                                                                debug!("Read error from local X Server (connection_id={}): {}", conn_id, e);
                                                                break;
                                                            }
                                                        }
                                                    }
                                                    data = write_rx.recv() => {
                                                        match data {
                                                            Some(data) => {
                                                                if local_write.write_all(&data).await.is_err()
                                                                    || local_write.flush().await.is_err()
                                                                {
                                                                    break;
                                                                }
                                                            }
                                                            None => break,
                                                        }
                                                    }
                                                }
                                            }

                                            let _ = outbound
                                                .send(Frame::X11Disconnect(X11DisconnectMessage {
                                                    display,
                                                    connection_id: conn_id,
                                                }))
                                                .await;
                                            x11_conns_clone.lock().await.remove(&conn_id);
                                            stats_clone.dec_x11_connections();
                                        });

                                        x11_connections.lock().await.insert(conn_id, (write_tx, handle));
                                    }
                                    Err(e) => {
                                        error!("Failed to connect to local X Server for connection_id={}: {}", conn_id, e);
                                        stats.dec_x11_connections();
                                        let _ = outbound_tx
                                            .send(Frame::X11Disconnect(X11DisconnectMessage {
                                                display: msg.display,
                                                connection_id: conn_id,
                                            }))
                                            .await;
                                    }
                                }
                            }
                            Ok(Ok(frame)) => {
                                match frame {
                                    Frame::DataX11(msg) => {
                                        let conn_id = msg.connection_id;
                                        stats.add_bytes_received(msg.data.len() as u64);

                                        let tx = {
                                            let conns = x11_connections.lock().await;
                                            conns.get(&conn_id).map(|(tx, _)| tx.clone())
                                        };

                                        if let Some(tx) = tx {
                                            if tx.send(msg.data).await.is_err() {
                                                debug!("Local X11 connection gone for connection_id={}", conn_id);
                                                x11_connections.lock().await.remove(&conn_id);
                                            }
                                        } else {
                                            debug!("No local connection for connection_id={}", conn_id);
                                            x11_connections.lock().await.remove(&conn_id);
                                        }
                                    }
                                    Frame::CompressedDataX11 { connection_id, sequence_id: _, original_len, data } => {
                                        let conn_id = connection_id;
                                        let algo = match compression_algo {
                                            Some(a) => a,
                                            None => continue,
                                        };
                                        let decompressed = match algo.decompress(&data, original_len) {
                                            Some(d) if d.len() == original_len => d,
                                            _ => {
                                                warn!("Decompression failed for connection_id={}, dropping frame", conn_id);
                                                continue;
                                            }
                                        };
                                        stats.add_bytes_received(decompressed.len() as u64);

                                        let tx = {
                                            let conns = x11_connections.lock().await;
                                            conns.get(&conn_id).map(|(tx, _)| tx.clone())
                                        };

                                        if let Some(tx) = tx {
                                            if tx.send(bytes::Bytes::from(decompressed)).await.is_err() {
                                                debug!("Local X11 connection gone for connection_id={}", conn_id);
                                                x11_connections.lock().await.remove(&conn_id);
                                            }
                                        } else {
                                            debug!("No local connection for connection_id={}", conn_id);
                                            x11_connections.lock().await.remove(&conn_id);
                                        }
                                    }
                                     _ => {
                                         match frame {
                                             Frame::Heartbeat => {
                                                 let _ = outbound_tx.send(Frame::HeartbeatAck).await;
                                             }
                                              Frame::SessionDestroy(msg) => {
                                                  info!("Session destroyed for display :{}", msg.display);
                                                  break;
                                              }
                                              Frame::X11Disconnect(msg) => {
                                                  let conn_id = msg.connection_id;
                                                  debug!("Remote X11 client disconnected (connection_id={})", conn_id);
                                                  let mut conns = x11_connections.lock().await;
                                                  conns.remove(&conn_id);
                                              }
                                              Frame::Error(msg) => {
                                                  error!("Error from relay (code={}): {}", msg.code, msg.message);
                                                  break;
                                              }
                                              Frame::FlowControl(msg) => {
                                                  debug!("FlowControl from relay: action={:?} conn={:?}", msg.action, msg.connection_id);
                                              }
                                              _ => {
                                                 warn!("Unexpected frame from relay: {:?}", frame.msg_type());
                                             }
                                         }
                                     }
                                 }
                              }
                             Ok(Err(e)) => {
                                 error!("Connection error: {}", e);
                                 return Err(e.into());
                             }
                             Err(_) => {
                                 error!(
                                     "Read timeout ({:?}), no data from relay",
                                     Duration::from_secs(CLIENT_READ_TIMEOUT_SECS)
                                 );
                                 return Err(rx11_core::error::Rx11Error::Timeout.into());
                             }
                         }
                     }
                    _ = tokio::signal::ctrl_c() => {
                        info!("Received shutdown signal, sending SessionDestroy...");
                        let _ = outbound_tx
                            .send(Frame::SessionDestroy(SessionDestroyMessage {
                                display: actual_display,
                            }))
                            .await;
                        break;
                    }
                }
            }
            Ok(())
        }
        .await;

        stats_task.abort();
        cleanup_connections(&x11_connections, &stats).await;
        drop(outbound_tx);
        if let Err(e) = sender_task.await {
            if !e.is_cancelled() {
                error!("Sender task panicked: {}", e);
            }
        }

        if let Err(e) = &result {
            error!("Session ended with error: {}", e);
        } else {
            info!("Session ended gracefully");
        }

        result
    }
}

async fn cleanup_connections(x11_connections: &SharedX11Conns, stats: &Arc<ConnectionStats>) {
    let mut conns = x11_connections.lock().await;
    let count = conns.len() as u32;
    for _ in 0..count {
        stats.dec_x11_connections();
    }
    for (_, (_, handle)) in conns.drain() {
        handle.abort();
    }
}
