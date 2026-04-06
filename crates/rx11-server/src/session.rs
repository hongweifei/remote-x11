use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{info, warn};

use crate::x11_listener::X11Listener;

const SESSION_GRACE_PERIOD_SECS: u64 = 60;
const MAX_X11_CONNECTIONS_PER_DISPLAY: usize = 64;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub display: u16,
    pub auth_name: String,
    pub auth_data: Vec<u8>,
    pub client_id: String,
}

pub enum X11ConnToRelay {
    Connected { display: u16, connection_id: u32 },
    Data { display: u16, connection_id: u32, data: Vec<u8> },
    Disconnected { display: u16, connection_id: u32 },
}

pub enum X11RelayToConn {
    Data(Vec<u8>),
    Close,
}

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<u16, Session>>>,
    x11_listener: Arc<RwLock<Option<Arc<X11Listener>>>>,
    conn_to_relay: Arc<RwLock<HashMap<u16, mpsc::Sender<X11ConnToRelay>>>>,
    relay_to_conn: Arc<Mutex<HashMap<u32, mpsc::Sender<X11RelayToConn>>>>,
    display_conns: Arc<Mutex<HashMap<u16, HashSet<u32>>>>,
    conn_display_map: Arc<Mutex<HashMap<u32, u16>>>,
    grace_tasks: Arc<Mutex<HashMap<u16, Arc<AtomicBool>>>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            x11_listener: Arc::new(RwLock::new(None)),
            conn_to_relay: Arc::new(RwLock::new(HashMap::new())),
            relay_to_conn: Arc::new(Mutex::new(HashMap::new())),
            display_conns: Arc::new(Mutex::new(HashMap::new())),
            conn_display_map: Arc::new(Mutex::new(HashMap::new())),
            grace_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn set_x11_listener(&self, listener: Arc<X11Listener>) {
        let mut x11 = self.x11_listener.write().await;
        *x11 = Some(listener);
    }

    pub async fn create_session(
        &self,
        disp: u16,
        auth_name: String,
        auth_data: Vec<u8>,
        client_id: String,
    ) -> rx11_core::error::Result<Session> {
        rx11_core::protocol::validate_display(disp)?;

        let listener = self.x11_listener.read().await;
        let listener_ref = listener.as_ref().ok_or_else(|| {
            rx11_core::error::Rx11Error::Protocol("X11Listener not initialized".into())
        })?;
        if let Err(e) = listener_ref.bind_display(disp).await {
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Failed to bind X11 port for display :{}: {}",
                disp, e
            )));
        }
        drop(listener);

        if let Err(e) = xauth_add(disp, &auth_data, &auth_name).await {
            let listeners = self.x11_listener.read().await;
            if let Some(ref l) = *listeners {
                l.unbind_display(disp).await;
            }
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "xauth setup failed for display :{}: {}",
                disp, e
            )));
        }

        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&disp) {
            drop(sessions);
            if let Some(listener) = self.x11_listener.read().await.as_ref() {
                listener.unbind_display(disp).await;
            }
            self.xauth_remove_quiet(disp).await;
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Display :{} already in use",
                disp
            )));
        }
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            display: disp,
            auth_name,
            auth_data,
            client_id,
        };
        sessions.insert(disp, session.clone());

        Ok(session)
    }

    pub async fn create_session_auto(
        &self,
        auth_name: String,
        auth_data: Vec<u8>,
        client_id: String,
    ) -> rx11_core::error::Result<Session> {
        let disp = {
            let sessions = self.sessions.read().await;
            (1..=rx11_core::protocol::MAX_DISPLAY_NUMBER)
                .find(|d| !sessions.contains_key(d))
                .ok_or_else(|| {
                    rx11_core::error::Rx11Error::Protocol("No available display number".into())
                })?
        };
        rx11_core::protocol::validate_display(disp)?;

        let listener = self.x11_listener.read().await;
        let listener_ref = listener.as_ref().ok_or_else(|| {
            rx11_core::error::Rx11Error::Protocol("X11Listener not initialized".into())
        })?;
        if let Err(e) = listener_ref.bind_display(disp).await {
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Failed to bind X11 port for display :{}: {}",
                disp, e
            )));
        }
        drop(listener);

        if let Err(e) = xauth_add(disp, &auth_data, &auth_name).await {
            let listeners = self.x11_listener.read().await;
            if let Some(ref l) = *listeners {
                l.unbind_display(disp).await;
            }
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "xauth setup failed for display :{}: {}",
                disp, e
            )));
        }

        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&disp) {
            drop(sessions);
            if let Some(listener) = self.x11_listener.read().await.as_ref() {
                listener.unbind_display(disp).await;
            }
            self.xauth_remove_quiet(disp).await;
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Display :{} already in use",
                disp
            )));
        }
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            display: disp,
            auth_name,
            auth_data,
            client_id,
        };
        sessions.insert(disp, session.clone());
        Ok(session)
    }

    pub async fn try_resume_session(
        &self,
        session_id: &str,
        new_client_id: String,
    ) -> rx11_core::error::Result<Session> {
        rx11_core::protocol::validate_session_id(session_id)?;

        let mut sessions = self.sessions.write().await;
        let session = sessions
            .values_mut()
            .find(|s| s.id == session_id)
            .cloned();
        match session {
            Some(mut session) => {
                let old_client_id = session.client_id.clone();
                session.client_id = new_client_id.clone();
                sessions.insert(session.display, session.clone());

                drop(sessions);

                if let Some(flag) = self.grace_tasks.lock().await.remove(&session.display) {
                    flag.store(true, Ordering::Relaxed);
                }

                warn!(
                    "Session {} resumed for display :{} (old client: {}, new client: {})",
                    session_id, session.display, old_client_id, new_client_id
                );
                Ok(session)
            }
            None => Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Session {} not found or expired",
                session_id
            ))),
        }
    }

    pub async fn owns_session(&self, disp: u16, client_id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(&disp)
            .map(|s| s.client_id == client_id)
            .unwrap_or(false)
    }

    pub async fn owns_connection(&self, conn_id: u32, client_id: &str) -> bool {
        let disp = self.conn_display_map.lock().await.get(&conn_id).copied();
        match disp {
            Some(d) => self.owns_session(d, client_id).await,
            None => false,
        }
    }

    pub async fn release_session(&self, client_id: &str) {
        let to_release: Vec<(u16, String)> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|s| s.client_id == client_id)
                .map(|s| (s.display, s.id.clone()))
                .collect()
        };

        for (disp, session_id) in &to_release {
            info!(
                "Client {} disconnected, session {} for display :{} enters grace period ({}s)",
                client_id, session_id, disp, SESSION_GRACE_PERIOD_SECS
            );
        }

        if to_release.is_empty() {
            return;
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut tasks = self.grace_tasks.lock().await;
            for (disp, _) in &to_release {
                if let Some(old_flag) = tasks.insert(*disp, cancelled.clone()) {
                    old_flag.store(true, Ordering::Relaxed);
                }
            }
        }

        let mgr = self.clone();
        let client_id_owned = client_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(SESSION_GRACE_PERIOD_SECS)).await;
            if cancelled.load(Ordering::Relaxed) {
                return;
            }

            let to_destroy: Vec<u16> = {
                let sessions = mgr.sessions.read().await;
                sessions
                    .values()
                    .filter(|s| s.client_id == client_id_owned)
                    .map(|s| s.display)
                    .collect()
            };

            for disp in &to_destroy {
                warn!("Grace period expired for display :{}, destroying session", disp);
                mgr.destroy_session(*disp).await;
            }

            let mut tasks = mgr.grace_tasks.lock().await;
            for disp in &to_destroy {
                tasks.remove(disp);
            }
        });
    }

    pub async fn destroy_session(&self, disp: u16) {
        if let Some(flag) = self.grace_tasks.lock().await.remove(&disp) {
            flag.store(true, Ordering::Relaxed);
        }

        if let Some(listener) = self.x11_listener.read().await.as_ref() {
            listener.unbind_display(disp).await;
        }

        let conn_ids: Option<HashSet<u32>> = self.display_conns.lock().await.remove(&disp);
        if let Some(ids) = conn_ids {
            let mut relay_to_conn = self.relay_to_conn.lock().await;
            for conn_id in ids {
                if let Some(tx) = relay_to_conn.remove(&conn_id) {
                    let _ = tx.send(X11RelayToConn::Close).await;
                }
            }
        }

        xauth_remove(disp).await;

        self.conn_to_relay.write().await.remove(&disp);
        self.sessions.write().await.remove(&disp);
    }

    pub async fn register_x11_relay(&self, disp: u16, sender: mpsc::Sender<X11ConnToRelay>) {
        self.conn_to_relay.write().await.insert(disp, sender);
    }

    pub async fn unregister_x11_relay(&self, disp: u16) {
        self.conn_to_relay.write().await.remove(&disp);
    }

    pub async fn get_x11_event_sender(&self, disp: u16) -> Option<mpsc::Sender<X11ConnToRelay>> {
        self.conn_to_relay.read().await.get(&disp).cloned()
    }

    pub async fn register_x11_connection(&self, conn_id: u32, disp: u16, sender: mpsc::Sender<X11RelayToConn>) -> rx11_core::error::Result<()> {
        {
            let display_conns = self.display_conns.lock().await;
            let count = display_conns.get(&disp).map(|s| s.len()).unwrap_or(0);
            if count >= MAX_X11_CONNECTIONS_PER_DISPLAY {
                return Err(rx11_core::error::Rx11Error::Protocol(format!(
                    "Too many X11 connections for display :{} (max {})",
                    disp, MAX_X11_CONNECTIONS_PER_DISPLAY
                )));
            }
        }
        self.relay_to_conn.lock().await.insert(conn_id, sender);
        self.display_conns.lock().await.entry(disp).or_default().insert(conn_id);
        self.conn_display_map.lock().await.insert(conn_id, disp);
        Ok(())
    }

    pub async fn unregister_x11_connection(&self, conn_id: u32) {
        self.relay_to_conn.lock().await.remove(&conn_id);
        if let Some(disp) = self.conn_display_map.lock().await.remove(&conn_id) {
            if let Some(set) = self.display_conns.lock().await.get_mut(&disp) {
                set.remove(&conn_id);
            }
        }
    }

    pub async fn send_to_x11_connection(&self, conn_id: u32, data: Vec<u8>) {
        if let Some(tx) = self.relay_to_conn.lock().await.get(&conn_id) {
            let _ = tx.send(X11RelayToConn::Data(data)).await;
        }
    }

    pub async fn destroy_all_sessions(&self) {
        let mut tasks = self.grace_tasks.lock().await;
        for (_, flag) in tasks.drain() {
            flag.store(true, Ordering::Relaxed);
        }
        drop(tasks);

        let displays: Vec<u16> = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect()
        };
        for disp in displays {
            self.destroy_session(disp).await;
        }
        info!("All sessions destroyed");
    }

    async fn xauth_remove_quiet(&self, disp: u16) {
        let display_str = format!(":{}", disp);
        let _ = Command::new("xauth")
            .args(["remove", &display_str])
            .output()
            .await;
    }
}

async fn xauth_add(disp: u16, auth_data: &[u8], auth_name: &str) -> anyhow::Result<()> {
    let cookie_hex = hex::encode(auth_data);
    let display_str = format!(":{}", disp);
    let output = Command::new("xauth")
        .args(["add", &display_str, auth_name, &cookie_hex])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to execute xauth: {}", e))?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "xauth add {} failed: {}",
            display_str,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    info!("xauth add {} succeeded", display_str);
    Ok(())
}

async fn xauth_remove(disp: u16) {
    let display_str = format!(":{}", disp);
    match Command::new("xauth")
        .args(["remove", &display_str])
        .output()
        .await
    {
        Ok(_) => info!("xauth remove {} done", display_str),
        Err(e) => warn!("Failed to execute xauth remove: {}", e),
    }
}
