use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use rx11_core::auth;
use rx11_core::compress::CompressionAlgo;
use rx11_core::protocol::*;
use rx11_core::transport::Rx11Transport;

use crate::session::{SessionManager, X11ConnToRelay};
use crate::x11_listener::X11Listener;

const SERVER_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const SERVER_HEARTBEAT_TIMEOUT_SECS: u64 = 90;
const MAX_CONNECTIONS: usize = 256;
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;
const CHANNEL_BUFFER_SIZE: usize = 2048;
const OUTBOUND_CHANNEL_SIZE: usize = 2048;

pub struct RelayServer {
    listen_addr: String,
    auth_token: String,
    session_mgr: Arc<SessionManager>,
    x11_base_port: u16,
    active_connections: Arc<AtomicUsize>,
}

impl RelayServer {
    pub fn new(listen_addr: String, auth_token: String, x11_base_port: u16) -> Self {
        Self {
            listen_addr,
            auth_token,
            session_mgr: Arc::new(SessionManager::new()),
            x11_base_port,
            active_connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        info!("Relay server listening on {}", self.listen_addr);

        let x11_listener = Arc::new(X11Listener::new(
            self.x11_base_port,
            self.session_mgr.clone(),
        ));
        self.session_mgr.set_x11_listener(x11_listener).await;

        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, addr) = result?;
                    let count = self.active_connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if count > MAX_CONNECTIONS {
                        self.active_connections.fetch_sub(1, Ordering::Relaxed);
                        warn!("Rejected connection from {}: max connections ({}) reached", addr, MAX_CONNECTIONS);
                        continue;
                    }
                    info!("New connection from {} ({}/{})", addr, count, MAX_CONNECTIONS);

                    let session_mgr = self.session_mgr.clone();
                    let auth_token = self.auth_token.clone();
                    let conn_counter = self.active_connections.clone();

                    tokio::spawn(async move {
                        let result = handle_client(stream, addr, session_mgr, auth_token).await;
                        conn_counter.fetch_sub(1, Ordering::Relaxed);
                        if let Err(e) = result {
                            warn!("Client handler error: {}", e);
                        }
                    });
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received shutdown signal, cleaning up...");
                    self.session_mgr.destroy_all_sessions().await;
                    info!("Server shutdown complete");
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn session_manager(&self) -> Arc<SessionManager> {
        self.session_mgr.clone()
    }
}

async fn handle_client(
    stream: TcpStream,
    addr: SocketAddr,
    session_mgr: Arc<SessionManager>,
    auth_token: String,
) -> anyhow::Result<()> {
    info!("Client connected from {}", addr);
    let mut transport = Rx11Transport::new(stream)?;
    let transport_id = uuid::Uuid::new_v4().to_string();

    let hello_frame = tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        transport.recv_frame(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Handshake timeout: no Hello received within {}s",
            HANDSHAKE_TIMEOUT_SECS
        )
    })??;
    let compression_algo: Option<CompressionAlgo>;
    match hello_frame {
        Frame::Hello(hello) => {
            if hello.version != PROTOCOL_VERSION {
                transport
                    .send_frame(&Frame::HelloAck(HelloAckMessage {
                        version: PROTOCOL_VERSION,
                        session_id: rx11_core::types::SessionId::new(String::new())?,
                        success: false,
                        error_msg: Some(format!(
                            "Version mismatch: got {} expected {}",
                            hello.version, PROTOCOL_VERSION
                        )),
                        compression: None,
                    }))
                    .await?;
                return Ok(());
            }

            if !matches!(hello.mode, ConnectionMode::Client) {
                transport
                    .send_frame(&Frame::HelloAck(HelloAckMessage {
                        version: PROTOCOL_VERSION,
                        session_id: rx11_core::types::SessionId::new(String::new())?,
                        success: false,
                        error_msg: Some("Expected Client mode".into()),
                        compression: None,
                    }))
                    .await?;
                return Ok(());
            }

            let server_algos = &CompressionAlgo::ALL;
            compression_algo = CompressionAlgo::negotiate(
                &hello.compression_algos,
                server_algos,
            );
            if let Some(algo) = compression_algo {
                info!("Client {} compression: {}", transport_id, algo.as_str());
            } else {
                info!("Client {} compression: disabled", transport_id);
            }

            let resume_session_id = hello.resume_session_id;
            if let Some(ref sid) = resume_session_id {
                info!("Client {} requests session resume: {}", transport_id, sid);
            }
            transport
                .send_frame(&Frame::HelloAck(HelloAckMessage {
                    version: PROTOCOL_VERSION,
                    session_id: rx11_core::types::SessionId::new(transport_id.clone())?,
                    success: true,
                    error_msg: None,
                    compression: compression_algo,
                }))
                .await?;
        }
        _ => {
            return Err(anyhow::anyhow!("Expected Hello frame"));
        }
    }

    let auth_frame = tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        transport.recv_frame(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Handshake timeout: no AuthRequest received within {}s",
            HANDSHAKE_TIMEOUT_SECS
        )
    })??;
    match auth_frame {
        Frame::AuthRequest(auth_req) => {
            if let Err(e) = rx11_core::types::Token::new(auth_req.token.0.clone()) {
                transport
                    .send_frame(&Frame::AuthResponse(AuthResponseMessage {
                        success: false,
                        error_msg: Some(format!("Invalid token: {}", e)),
                    }))
                    .await?;
                return Ok(());
            }
            if !auth::verify_token(auth_req.token.as_str(), &auth_token) {
                transport
                    .send_frame(&Frame::AuthResponse(AuthResponseMessage {
                        success: false,
                        error_msg: Some("Invalid token".into()),
                    }))
                    .await?;
                return Ok(());
            }
            transport
                .send_frame(&Frame::AuthResponse(AuthResponseMessage {
                    success: true,
                    error_msg: None,
                }))
                .await?;
        }
        _ => {
            return Err(anyhow::anyhow!("Expected AuthRequest frame"));
        }
    }

    info!("Client {} authenticated successfully", transport_id);

    let (mut read_half, mut write_half) = transport.split();

    let mgr = session_mgr.clone();
    let tid = transport_id.clone();

    let (x11_event_tx, mut x11_event_rx) =
        tokio::sync::mpsc::channel::<X11ConnToRelay>(CHANNEL_BUFFER_SIZE);

    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<Frame>(OUTBOUND_CHANNEL_SIZE);

    let outbound_tx_clone = outbound_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            SERVER_HEARTBEAT_INTERVAL_SECS,
        ));
        interval.tick().await;
        loop {
            interval.tick().await;
            if outbound_tx_clone.send(Frame::Heartbeat).await.is_err() {
                break;
            }
        }
    });

    let sender_task = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            if write_half.send_frame(&frame).await.is_err() {
                break;
            }
        }
        let _ = write_half.flush().await;
    });

    let mut heartbeat_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(SERVER_HEARTBEAT_TIMEOUT_SECS);

    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                frame_result = read_half.recv_frame() => {
                    match frame_result {
                        Ok(Frame::SessionCreate(msg)) => {
                            let disp = msg.display;
                            match session_mgr
                                .create_session(
                                    disp,
                                    msg.auth_name,
                                    msg.auth_data,
                                    transport_id.clone(),
                                )
                                .await
                            {
                                Ok(session) => {
                                    let disp = session.display;
                                    let sid = session.id.clone();
                                    info!("Session created for display {}", disp);
                                    eprintln!("[rx11] Session created: DISPLAY={} (client: {})", disp, tid);
                                    session_mgr.register_x11_relay(disp, x11_event_tx.clone()).await;
                                    if outbound_tx.send(Frame::SessionAck(SessionAckMessage {
                                        display: disp,
                                        success: true,
                                        error_msg: None,
                                        session_id: Some(sid),
                                    })).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let err_msg = format!("{}", e);
                                    if outbound_tx.send(Frame::SessionAck(SessionAckMessage {
                                        display: rx11_core::types::DisplayNumber::new(u16::MAX).unwrap_or_else(|_| rx11_core::types::DisplayNumber::new(0).unwrap()),
                                        success: false,
                                        error_msg: Some(err_msg),
                                        session_id: None,
                                    })).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(Frame::SessionResume(msg)) => {
                            let sid = msg.session_id;
                            match session_mgr
                                .try_resume_session(&sid, transport_id.clone())
                                .await
                            {
                                Ok(session) => {
                                    let disp = session.display;
                                    let sid = session.id.clone();
                                    info!("Session resumed for display {}", disp);
                                    eprintln!("[rx11] Session resumed: DISPLAY={} (client: {})", disp, tid);
                                    session_mgr.register_x11_relay(disp, x11_event_tx.clone()).await;
                                    if outbound_tx.send(Frame::SessionAck(SessionAckMessage {
                                        display: disp,
                                        success: true,
                                        error_msg: None,
                                        session_id: Some(sid),
                                    })).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let err_msg = format!("{}", e);
                                    if outbound_tx.send(Frame::SessionAck(SessionAckMessage {
                                        display: rx11_core::types::DisplayNumber::new(u16::MAX).unwrap_or_else(|_| rx11_core::types::DisplayNumber::new(0).unwrap()),
                                        success: false,
                                        error_msg: Some(err_msg),
                                        session_id: None,
                                    })).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(Frame::SessionAutoCreate(msg)) => {
                            match session_mgr
                                .create_session_auto(
                                    msg.auth_name,
                                    msg.auth_data,
                                    transport_id.clone(),
                                )
                                .await
                            {
                                Ok(session) => {
                                    let disp = session.display;
                                    let sid = session.id.clone();
                                    info!("Session auto-created for display {}", disp);
                                    eprintln!("[rx11] Session created: DISPLAY={} (client: {})", disp, tid);
                                    session_mgr.register_x11_relay(disp, x11_event_tx.clone()).await;
                                    if outbound_tx.send(Frame::SessionAck(SessionAckMessage {
                                        display: disp,
                                        success: true,
                                        error_msg: None,
                                        session_id: Some(sid),
                                    })).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let err_msg = format!("{}", e);
                                    if outbound_tx.send(Frame::SessionAck(SessionAckMessage {
                                        display: rx11_core::types::DisplayNumber::new(u16::MAX).unwrap_or_else(|_| rx11_core::types::DisplayNumber::new(0).unwrap()),
                                        success: false,
                                        error_msg: Some(err_msg),
                                        session_id: None,
                                    })).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(Frame::SessionDestroy(msg)) => {
                            let disp = msg.display;
                            if !session_mgr.owns_session(disp, &tid).await {
                                warn!("Client {} tried to destroy unowned session for display {}", tid, disp);
                                continue;
                            }
                            session_mgr.unregister_x11_relay(disp).await;
                            session_mgr.destroy_session(disp).await;
                            info!("Session destroyed for display {}", disp);
                            eprintln!("[rx11] Session destroyed: DISPLAY={}", disp);
                        }
                        Ok(Frame::DataX11(msg)) => {
                            let conn_id = msg.connection_id;
                            if !session_mgr.owns_connection(conn_id, &tid).await {
                                warn!("Client {} sent data for unowned {}", tid, conn_id);
                                continue;
                            }
                            session_mgr.send_to_x11_connection(conn_id, msg.data.to_vec()).await;
                        }
                        Ok(Frame::CompressedDataX11(msg)) => {
                            let conn_id = msg.connection_id;
                            if !session_mgr.owns_connection(conn_id, &tid).await {
                                warn!("Client {} sent compressed data for unowned {}", tid, conn_id);
                                continue;
                            }
                            let algo = match compression_algo {
                                Some(a) => a,
                                None => {
                                    warn!("CompressedDataX11 received but no compression negotiated, dropping");
                                    continue;
                                }
                            };
                            match algo.decompress(&msg.data, msg.original_len) {
                                Some(decompressed) if decompressed.len() == msg.original_len => {
                                    session_mgr.send_to_x11_connection(conn_id, decompressed).await;
                                }
                                _ => {
                                    warn!("Decompression failed for {}, dropping frame", conn_id);
                                }
                            }
                        }
                        Ok(Frame::HeartbeatAck) => {
                            heartbeat_deadline = tokio::time::Instant::now()
                                + std::time::Duration::from_secs(SERVER_HEARTBEAT_TIMEOUT_SECS);
                        }
                        Ok(Frame::FlowControl(msg)) => {
                            let target_conn = msg.connection_id;
                            match msg.action {
                                FlowControlAction::Pause => {
                                    warn!("Client {} requests pause for {:?}", tid, target_conn);
                                }
                                FlowControlAction::Resume => {
                                    warn!("Client {} requests resume for {:?}", tid, target_conn);
                                }
                            }
                        }
                        Ok(frame) => {
                            warn!("Unexpected frame from client {}: {:?}", tid, frame.msg_type());
                        }
                        Err(rx11_core::error::Rx11Error::ConnectionClosed) => {
                            info!("Client {} disconnected", tid);
                            break;
                        }
                        Err(e) => {
                            warn!("Error on client {}: {}", tid, e);
                            break;
                        }
                    }
                }
                event = x11_event_rx.recv() => {
                    match event {
                        Some(X11ConnToRelay::Connected { display, connection_id }) => {
                            if outbound_tx.send(Frame::X11Connect(X11ConnectMessage {
                                display,
                                connection_id,
                            })).await.is_err() {
                                break;
                            }
                        }
                        Some(X11ConnToRelay::Data { display: _, connection_id, data }) => {
                            let frame = maybe_compress_x11_data(
                                connection_id,
                                0,
                                data,
                                compression_algo,
                            );
                            if outbound_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        Some(X11ConnToRelay::Disconnected { display, connection_id }) => {
                            if outbound_tx.send(Frame::X11Disconnect(X11DisconnectMessage {
                                display,
                                connection_id,
                            })).await.is_err() {
                                break;
                            }
                            session_mgr.unregister_x11_connection(connection_id).await;
                        }
                        None => {
                            warn!("X11 event channel closed for client {}", tid);
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(heartbeat_deadline) => {
                    warn!("Client {} heartbeat timeout, disconnecting", tid);
                    break;
                }
            }
        }
        Ok(())
    }
    .await;

    heartbeat_task.abort();
    drop(outbound_tx);
    let _ = sender_task.await;

    mgr.release_session(&tid).await;
    info!("Client disconnected from {}", addr);
    result
}

fn maybe_compress_x11_data(
    connection_id: rx11_core::types::ConnectionId,
    sequence_id: u32,
    data: bytes::Bytes,
    compression_algo: Option<CompressionAlgo>,
) -> Frame {
    if let Some(algo) = compression_algo {
        if data.len() >= rx11_core::compress::COMPRESSION_THRESHOLD {
            if let Some(compressed) = algo.compress_to_bytes(&data) {
                return Frame::CompressedDataX11(CompressedX11DataMessage {
                    connection_id,
                    sequence_id,
                    original_len: data.len(),
                    data: compressed,
                });
            }
        }
    }
    Frame::DataX11(X11DataMessage {
        connection_id,
        sequence_id,
        data,
    })
}
