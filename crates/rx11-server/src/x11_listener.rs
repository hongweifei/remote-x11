use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use rx11_core::config::BufferDefaults;
use rx11_core::types::{ConnectionId, DisplayNumber};

use crate::session::{SessionManager, X11ConnToRelay, X11DisplayBinder, X11RelayToConn};

static NEXT_CONNECTION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_connection_id() -> ConnectionId {
    loop {
        let raw = NEXT_CONNECTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = (raw & 0xFFFFFFFF) as u32;
        if id != 0 {
            return ConnectionId::new(id);
        }
    }
}

pub struct X11Listener {
    base_port: u16,
    session_mgr: Arc<SessionManager>,
    listeners: Arc<RwLock<HashMap<u16, JoinHandle<()>>>>,
}

impl X11Listener {
    pub fn new(base_port: u16, session_mgr: Arc<SessionManager>) -> Self {
        Self {
            base_port,
            session_mgr,
            listeners: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn bind_display(&self, disp: u16) -> anyhow::Result<()> {
        let port = self
            .base_port
            .checked_add(disp)
            .ok_or_else(|| anyhow::anyhow!(
                "X11 port overflow: base {} + display {}",
                self.base_port,
                disp
            ))?;
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        info!("X11 listening on port {} (display :{})", port, disp);

        let session_mgr = self.session_mgr.clone();
        let disp_num = DisplayNumber::new(disp).unwrap_or(DisplayNumber::UNSPECIFIED);

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        info!("X11 connection on display {} from {}", disp_num, addr);
                        let mgr = session_mgr.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_x11_connection(stream, disp_num, mgr).await {
                                warn!("X11 connection error on display {}: {}", disp_num, e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("X11 accept error on display {}: {}", disp_num, e);
                    }
                }
            }
        });

        self.listeners.write().await.insert(disp, handle);
        Ok(())
    }

    pub async fn unbind_display(&self, disp: u16) {
        if let Some(handle) = self.listeners.write().await.remove(&disp) {
            handle.abort();
            info!("X11 listener for display :{} stopped", disp);
        }
    }

    pub async fn unbind_all(&self) {
        let mut listeners = self.listeners.write().await;
        for (disp, handle) in listeners.drain() {
            handle.abort();
            info!("X11 listener for display :{} stopped", disp);
        }
    }
}

#[async_trait::async_trait]
impl X11DisplayBinder for X11Listener {
    async fn bind_display(&self, disp: u16) -> anyhow::Result<()> {
        Self::bind_display(self, disp).await
    }

    async fn unbind_display(&self, disp: u16) {
        Self::unbind_display(self, disp).await
    }
}

async fn handle_x11_connection(
    x11_stream: TcpStream,
    disp: DisplayNumber,
    session_mgr: Arc<SessionManager>,
) -> anyhow::Result<()> {
    x11_stream.set_nodelay(true)?;

    let connection_id = next_connection_id();

    let event_tx = session_mgr
        .get_x11_event_sender(disp)
        .await
        .ok_or_else(|| anyhow::anyhow!("No relay registered for display {}", disp))?;

    let (relay_tx, mut relay_rx) = tokio::sync::mpsc::channel::<X11RelayToConn>(BufferDefaults::CHANNEL_BUFFER);
    session_mgr
        .register_x11_connection(connection_id, disp, relay_tx)
        .await?;

    if event_tx
        .send(X11ConnToRelay::Connected {
            display: disp,
            connection_id,
        })
        .await
        .is_err()
    {
        session_mgr.unregister_x11_connection(connection_id).await;
        return Err(anyhow::anyhow!("Relay gone for display {}", disp));
    }

    let (mut read_half, write_half) = tokio::io::split(x11_stream);
    let event_tx_clone = event_tx.clone();

    let socket_to_relay = async move {
        let mut buf = vec![0u8; BufferDefaults::INITIAL_READ_BUF];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                    if event_tx_clone
                        .send(X11ConnToRelay::Data {
                            display: disp,
                            connection_id,
                            data,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if buf.len() < BufferDefaults::MAX_READ_BUF {
                        let new_size = (buf.len() * 2).min(BufferDefaults::MAX_READ_BUF);
                        buf.resize(new_size, 0);
                    }
                }
                Err(_) => break,
            }
        }
    };

    let relay_to_socket = async move {
        let mut write_half = write_half;
        while let Some(cmd) = relay_rx.recv().await {
            match cmd {
                X11RelayToConn::Data(data) => {
                    if write_half.write_all(&data).await.is_err()
                        || write_half.flush().await.is_err()
                    {
                        break;
                    }
                }
                X11RelayToConn::Close => break,
            }
        }
    };

    tokio::select! {
        _ = socket_to_relay => {},
        _ = relay_to_socket => {},
    }

    let _ = event_tx
        .send(X11ConnToRelay::Disconnected {
            display: disp,
            connection_id,
        })
        .await;

    session_mgr.unregister_x11_connection(connection_id).await;

    Ok(())
}
