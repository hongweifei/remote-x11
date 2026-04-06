use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::net::TcpStream;
use tracing::{info, warn};

use rx11_core::compress::{maybe_compress_frame, CompressionAlgo};
use rx11_core::config::{BufferDefaults, ServerDefaults};
use rx11_core::protocol::*;
use rx11_core::transport::Rx11Transport;
use rx11_core::types::DisplayNumber;

use crate::session::{SessionManager, X11ConnToRelay};
use crate::x11_listener::X11Listener;

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

    pub async fn run(&self) -> rx11_core::error::Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.listen_addr).await?;
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
                    if count > ServerDefaults::MAX_CONNECTIONS {
                        self.active_connections.fetch_sub(1, Ordering::Relaxed);
                        warn!("Rejected connection from {}: max connections ({}) reached", addr, ServerDefaults::MAX_CONNECTIONS);
                        continue;
                    }
                    info!("New connection from {} ({}/{})", addr, count, ServerDefaults::MAX_CONNECTIONS);

                    let session_mgr = self.session_mgr.clone();
                    let auth_token = self.auth_token.clone();
                    let conn_counter = self.active_connections.clone();

                    tokio::spawn(async move {
                        let result = handle_client(stream, addr, session_mgr, &auth_token).await;
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
    auth_token: &str,
) -> rx11_core::error::Result<()> {
    info!("Client connected from {}", addr);
    let mut transport = Rx11Transport::new(stream)?;

    let handshake = server_handshake(&mut transport, auth_token, ServerDefaults::HANDSHAKE_TIMEOUT).await?;
    let transport_id = handshake.session_id.clone();
    let compression = handshake.compression;

    info!("Client {} authenticated successfully", transport_id);

    let (mut read_half, write_half) = transport.split();

    let (x11_event_tx, mut x11_event_rx) =
        tokio::sync::mpsc::channel::<X11ConnToRelay>(BufferDefaults::CHANNEL_BUFFER);

    let (outbound_tx, outbound_rx) =
        tokio::sync::mpsc::channel::<Frame>(BufferDefaults::OUTBOUND_CHANNEL);

    let heartbeat_task = spawn_heartbeat(outbound_tx.clone(), ServerDefaults::HEARTBEAT_INTERVAL);
    let sender_task = spawn_sender(outbound_rx, write_half);

    let mut heartbeat_deadline =
        tokio::time::Instant::now() + ServerDefaults::HEARTBEAT_TIMEOUT;

    let tid = transport_id.as_str().to_string();

    let result = relay_loop(
        &mut read_half,
        &outbound_tx,
        &mut x11_event_rx,
        &session_mgr,
        &tid,
        compression,
        &mut heartbeat_deadline,
        &x11_event_tx,
    )
    .await;

    heartbeat_task.abort();
    drop(outbound_tx);
    let _ = sender_task.await;

    session_mgr.release_session(&tid).await;
    info!("Client disconnected from {}", addr);
    result
}

fn spawn_heartbeat(
    outbound: tokio::sync::mpsc::Sender<Frame>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval);
        timer.tick().await;
        loop {
            timer.tick().await;
            if outbound.send(Frame::Heartbeat).await.is_err() {
                break;
            }
        }
    })
}

fn spawn_sender(
    mut rx: tokio::sync::mpsc::Receiver<Frame>,
    mut write_half: rx11_core::transport::Rx11TransportWriteHalf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write_half.send_frame(&frame).await.is_err() {
                break;
            }
        }
        let _ = write_half.flush().await;
    })
}

async fn relay_loop(
    read_half: &mut rx11_core::transport::Rx11TransportReadHalf,
    outbound: &tokio::sync::mpsc::Sender<Frame>,
    x11_events: &mut tokio::sync::mpsc::Receiver<X11ConnToRelay>,
    session_mgr: &Arc<SessionManager>,
    transport_id: &str,
    compression: Option<CompressionAlgo>,
    heartbeat_deadline: &mut tokio::time::Instant,
    x11_event_tx: &tokio::sync::mpsc::Sender<X11ConnToRelay>,
) -> rx11_core::error::Result<()> {
    loop {
        tokio::select! {
            frame_result = read_half.recv_frame() => {
                match frame_result {
                    Ok(frame) => {
                        if handle_inbound_frame(frame, outbound, session_mgr, transport_id, compression, heartbeat_deadline, x11_event_tx).await {
                            break;
                        }
                    }
                    Err(rx11_core::error::Rx11Error::ConnectionClosed) => {
                        info!("Client {} disconnected", transport_id);
                        break;
                    }
                    Err(e) => {
                        warn!("Error on client {}: {}", transport_id, e);
                        break;
                    }
                }
            }
            event = x11_events.recv() => {
                if handle_x11_event(event, outbound, session_mgr, compression).await {
                    break;
                }
            }
            _ = tokio::time::sleep_until(*heartbeat_deadline) => {
                warn!("Client {} heartbeat timeout, disconnecting", transport_id);
                break;
            }
        }
    }
    Ok(())
}

async fn handle_inbound_frame(
    frame: Frame,
    outbound: &tokio::sync::mpsc::Sender<Frame>,
    session_mgr: &Arc<SessionManager>,
    transport_id: &str,
    compression: Option<CompressionAlgo>,
    heartbeat_deadline: &mut tokio::time::Instant,
    x11_event_tx: &tokio::sync::mpsc::Sender<X11ConnToRelay>,
) -> bool {
    match frame {
        Frame::SessionCreate(msg) => {
            let result = session_mgr
                .create_session(msg.display, msg.auth_name, msg.auth_data, transport_id.to_string())
                .await;
            send_session_ack(result, outbound, session_mgr, transport_id, x11_event_tx).await
        }
        Frame::SessionResume(msg) => {
            let result = session_mgr
                .try_resume_session(&msg.session_id, transport_id.to_string())
                .await;
            send_session_ack(result, outbound, session_mgr, transport_id, x11_event_tx).await
        }
        Frame::SessionAutoCreate(msg) => {
            let result = session_mgr
                .create_session_auto(msg.auth_name, msg.auth_data, transport_id.to_string())
                .await;
            send_session_ack(result, outbound, session_mgr, transport_id, x11_event_tx).await
        }
        Frame::SessionDestroy(msg) => {
            let disp = msg.display;
            if !session_mgr.owns_session(disp, transport_id).await {
                warn!("Client {} tried to destroy unowned session for display {}", transport_id, disp);
                return false;
            }
            session_mgr.unregister_x11_relay(disp).await;
            session_mgr.destroy_session(disp).await;
            info!("Session destroyed for display {}", disp);
            false
        }
        Frame::DataX11(msg) => {
            if !session_mgr.owns_connection(msg.connection_id, transport_id).await {
                warn!("Client {} sent data for unowned {}", transport_id, msg.connection_id);
                return false;
            }
            session_mgr.send_to_x11_connection(msg.connection_id, msg.data.to_vec()).await;
            false
        }
        Frame::CompressedDataX11(msg) => {
            if !session_mgr.owns_connection(msg.connection_id, transport_id).await {
                warn!("Client {} sent compressed data for unowned {}", transport_id, msg.connection_id);
                return false;
            }
            let algo = match compression {
                Some(a) => a,
                None => {
                    warn!("CompressedDataX11 received but no compression negotiated, dropping");
                    return false;
                }
            };
            match rx11_core::compress::decompress_frame_data(&msg, algo) {
                Some(decompressed) => {
                    session_mgr.send_to_x11_connection(msg.connection_id, decompressed).await;
                }
                None => {
                    warn!("Decompression failed for {}, dropping frame", msg.connection_id);
                }
            }
            false
        }
        Frame::HeartbeatAck => {
            *heartbeat_deadline = tokio::time::Instant::now() + ServerDefaults::HEARTBEAT_TIMEOUT;
            false
        }
        Frame::FlowControl(msg) => {
            match msg.action {
                FlowControlAction::Pause => {
                    warn!("Client {} requests pause for {:?}", transport_id, msg.connection_id);
                }
                FlowControlAction::Resume => {
                    warn!("Client {} requests resume for {:?}", transport_id, msg.connection_id);
                }
            }
            false
        }
        frame => {
            warn!("Unexpected frame from client {}: {:?}", transport_id, frame.msg_type());
            false
        }
    }
}

async fn send_session_ack(
    result: rx11_core::error::Result<crate::session::Session>,
    outbound: &tokio::sync::mpsc::Sender<Frame>,
    session_mgr: &Arc<SessionManager>,
    transport_id: &str,
    x11_event_tx: &tokio::sync::mpsc::Sender<X11ConnToRelay>,
) -> bool {
    match result {
        Ok(session) => {
            let disp = session.display;
            let sid = session.id.clone();
            info!("Session created for display {}", disp);
            info!("[rx11] Session created: DISPLAY={} (client: {})", disp, transport_id);
            session_mgr.register_x11_relay(disp, x11_event_tx.clone()).await;
            outbound
                .send(Frame::SessionAck(SessionAckMessage {
                    display: disp,
                    success: true,
                    error_msg: None,
                    session_id: Some(sid),
                }))
                .await
                .is_err()
        }
        Err(e) => outbound
            .send(Frame::SessionAck(SessionAckMessage {
                display: DisplayNumber::UNSPECIFIED,
                success: false,
                error_msg: Some(format!("{}", e)),
                session_id: None,
            }))
            .await
            .is_err(),
    }
}

async fn handle_x11_event(
    event: Option<X11ConnToRelay>,
    outbound: &tokio::sync::mpsc::Sender<Frame>,
    session_mgr: &Arc<SessionManager>,
    compression: Option<CompressionAlgo>,
) -> bool {
    match event {
        Some(X11ConnToRelay::Connected { display, connection_id }) => outbound
            .send(Frame::X11Connect(X11ConnectMessage {
                display,
                connection_id,
            }))
            .await
            .is_err(),
        Some(X11ConnToRelay::Data { connection_id, data, .. }) => {
            let frame = maybe_compress_frame(connection_id, 0, data, compression);
            outbound.send(frame).await.is_err()
        }
        Some(X11ConnToRelay::Disconnected { display, connection_id }) => {
            session_mgr.unregister_x11_connection(connection_id).await;
            outbound
                .send(Frame::X11Disconnect(X11DisconnectMessage {
                    display,
                    connection_id,
                }))
                .await
                .is_err()
        }
        None => true,
    }
}
