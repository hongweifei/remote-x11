use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{info, warn};

use rx11_core::config::ServerDefaults;
use rx11_core::types::{ConnectionId, DisplayNumber, SessionId};

#[async_trait::async_trait]
pub trait X11DisplayBinder: Send + Sync {
    async fn bind_display(&self, disp: u16) -> anyhow::Result<()>;
    async fn unbind_display(&self, disp: u16);
}

#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub display: DisplayNumber,
    pub auth_name: String,
    pub auth_data: Vec<u8>,
    pub client_id: String,
}

pub enum X11ConnToRelay {
    Connected {
        display: DisplayNumber,
        connection_id: ConnectionId,
    },
    Data {
        display: DisplayNumber,
        connection_id: ConnectionId,
        data: bytes::Bytes,
    },
    Disconnected {
        display: DisplayNumber,
        connection_id: ConnectionId,
    },
}

pub enum X11RelayToConn {
    Data(bytes::Bytes),
    Close,
}

struct ConnState {
    display: DisplayNumber,
    sender: mpsc::Sender<X11RelayToConn>,
}

/// Lock ordering (always acquire in this order to prevent deadlock):
/// 1. grace_tasks (Mutex)
/// 2. x11_listener (RwLock)
/// 3. display_conns (Mutex)
/// 4. connections (Mutex)
/// 5. conn_to_relay (RwLock)
/// 6. sessions (RwLock)
#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<u16, Arc<Session>>>>,
    x11_listener: Arc<RwLock<Option<Arc<dyn X11DisplayBinder>>>>,
    conn_to_relay: Arc<RwLock<HashMap<u16, mpsc::Sender<X11ConnToRelay>>>>,
    connections: Arc<Mutex<HashMap<u32, ConnState>>>,
    display_conns: Arc<Mutex<HashMap<u16, HashSet<u32>>>>,
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
            connections: Arc::new(Mutex::new(HashMap::new())),
            display_conns: Arc::new(Mutex::new(HashMap::new())),
            grace_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn set_x11_listener(&self, listener: Arc<dyn X11DisplayBinder>) {
        let mut x11 = self.x11_listener.write().await;
        *x11 = Some(listener);
    }

    async fn create_session_inner(
        &self,
        disp: DisplayNumber,
        auth_name: String,
        auth_data: Vec<u8>,
        client_id: String,
    ) -> rx11_core::error::Result<Session> {
        let disp_val = disp.get();

        let listener = self.x11_listener.read().await;
        let listener_ref = listener.as_ref().ok_or_else(|| {
            rx11_core::error::Rx11Error::Protocol("X11Listener not initialized".into())
        })?;
        if let Err(e) = listener_ref.bind_display(disp_val).await {
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Failed to bind X11 port for display :{}: {}",
                disp_val, e
            )));
        }
        drop(listener);

        if let Err(e) = xauth_add(disp_val, &auth_data, &auth_name).await {
            let listeners = self.x11_listener.read().await;
            if let Some(ref l) = *listeners {
                l.unbind_display(disp_val).await;
            }
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "xauth setup failed for display :{}: {}",
                disp_val, e
            )));
        }

        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&disp_val) {
            drop(sessions);
            if let Some(listener) = self.x11_listener.read().await.as_ref() {
                listener.unbind_display(disp_val).await;
            }
            self.xauth_remove_quiet(disp_val).await;
            return Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Display :{} already in use",
                disp_val
            )));
        }
        let session = Arc::new(Session {
            id: SessionId::new(uuid::Uuid::new_v4().to_string())?,
            display: disp,
            auth_name,
            auth_data,
            client_id,
        });
        sessions.insert(disp_val, session.clone());
        Ok(Session {
            id: session.id.clone(),
            display: session.display,
            auth_name: session.auth_name.clone(),
            auth_data: session.auth_data.clone(),
            client_id: session.client_id.clone(),
        })
    }

    pub async fn create_session(
        &self,
        disp: DisplayNumber,
        auth_name: String,
        auth_data: Vec<u8>,
        client_id: String,
    ) -> rx11_core::error::Result<Session> {
        self.create_session_inner(disp, auth_name, auth_data, client_id).await
    }

    pub async fn create_session_auto(
        &self,
        auth_name: String,
        auth_data: Vec<u8>,
        client_id: String,
    ) -> rx11_core::error::Result<Session> {
        let disp = {
            let sessions = self.sessions.read().await;
            (0..=rx11_core::types::MAX_DISPLAY_NUMBER)
                .find(|d| !sessions.contains_key(d))
                .ok_or_else(|| {
                    rx11_core::error::Rx11Error::Protocol(
                        "No available display number".into(),
                    )
                })?
        };
        self.create_session_inner(DisplayNumber::new(disp)?, auth_name, auth_data, client_id)
            .await
    }

    pub async fn try_resume_session(
        &self,
        session_id: &SessionId,
        new_client_id: String,
    ) -> rx11_core::error::Result<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .values()
            .find(|s| s.id == *session_id)
            .cloned();
        match session {
            Some(session) => {
                let old_client_id = session.client_id.clone();
                let updated = Arc::new(Session {
                    id: session.id.clone(),
                    display: session.display,
                    auth_name: session.auth_name.clone(),
                    auth_data: session.auth_data.clone(),
                    client_id: new_client_id.clone(),
                });
                sessions.insert(session.display.get(), updated.clone());

                drop(sessions);

                if let Some(flag) = self.grace_tasks.lock().await.remove(&session.display.get()) {
                    flag.store(true, Ordering::Relaxed);
                }

                warn!(
                    "Session {} resumed for display :{} (old client: {}, new client: {})",
                    session_id, session.display, old_client_id, new_client_id
                );
                Ok(Session {
                    id: updated.id.clone(),
                    display: updated.display,
                    auth_name: updated.auth_name.clone(),
                    auth_data: updated.auth_data.clone(),
                    client_id: updated.client_id.clone(),
                })
            }
            None => Err(rx11_core::error::Rx11Error::Protocol(format!(
                "Session {} not found or expired",
                session_id
            ))),
        }
    }

    pub async fn owns_session(&self, disp: DisplayNumber, client_id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(&disp.get())
            .map(|s| s.client_id == client_id)
            .unwrap_or(false)
    }

    pub async fn owns_connection(&self, conn_id: ConnectionId, client_id: &str) -> bool {
        let conns = self.connections.lock().await;
        match conns.get(&conn_id.get()) {
            Some(state) => self.owns_session(state.display, client_id).await,
            None => false,
        }
    }

    pub async fn release_session(&self, client_id: &str) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let to_release: Vec<(u16, SessionId)> = {
            let mut tasks = self.grace_tasks.lock().await;
            let sessions = self.sessions.read().await;
            let matching: Vec<(u16, SessionId)> = sessions
                .values()
                .filter(|s| s.client_id == client_id)
                .map(|s| (s.display.get(), s.id.clone()))
                .collect();

            for (disp, _) in &matching {
                if let Some(old_flag) = tasks.insert(*disp, cancelled.clone()) {
                    old_flag.store(true, Ordering::Relaxed);
                }
            }
            drop(sessions);
            matching
        };

        for (disp, session_id) in &to_release {
            info!(
                "Client {} disconnected, session {} for display :{} enters grace period ({}s)",
                client_id, session_id, disp, ServerDefaults::SESSION_GRACE_PERIOD.as_secs()
            );
        }

        if to_release.is_empty() {
            return;
        }

        let mgr = self.clone();
        let client_id_owned = client_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(ServerDefaults::SESSION_GRACE_PERIOD).await;
            if cancelled.load(Ordering::Relaxed) {
                return;
            }

            // Re-check ownership before destroying: if try_resume_session ran
            // after our snapshot, it updated client_id and the grace task was
            // cancelled via the AtomicBool. This filter is a safety net.
            let to_destroy: Vec<u16> = {
                let sessions = mgr.sessions.read().await;
                sessions
                    .values()
                    .filter(|s| s.client_id == client_id_owned)
                    .map(|s| s.display.get())
                    .collect()
            };

            for disp in &to_destroy {
                warn!("Grace period expired for display :{}, destroying session", disp);
                if let Ok(disp_num) = DisplayNumber::new(*disp) {
                    mgr.destroy_session(disp_num).await;
                }
            }

            let mut tasks = mgr.grace_tasks.lock().await;
            for disp in &to_destroy {
                tasks.remove(disp);
            }
        });
    }

    pub async fn destroy_session(&self, disp: DisplayNumber) {
        let disp_val = disp.get();

        if let Some(flag) = self.grace_tasks.lock().await.remove(&disp_val) {
            flag.store(true, Ordering::Relaxed);
        }

        if let Some(listener) = self.x11_listener.read().await.as_ref() {
            listener.unbind_display(disp_val).await;
        }

        let conn_ids: Option<HashSet<u32>> = self.display_conns.lock().await.remove(&disp_val);
        if let Some(ids) = conn_ids {
            let mut connections = self.connections.lock().await;
            for conn_id in ids {
                if let Some(state) = connections.remove(&conn_id) {
                    let _ = state.sender.send(X11RelayToConn::Close).await;
                }
            }
        }

        xauth_remove(disp_val).await;

        self.conn_to_relay.write().await.remove(&disp_val);
        self.sessions.write().await.remove(&disp_val);
    }

    pub async fn register_x11_relay(&self, disp: DisplayNumber, sender: mpsc::Sender<X11ConnToRelay>) {
        self.conn_to_relay.write().await.insert(disp.get(), sender);
    }

    pub async fn unregister_x11_relay(&self, disp: DisplayNumber) {
        self.conn_to_relay.write().await.remove(&disp.get());
    }

    pub async fn get_x11_event_sender(&self, disp: DisplayNumber) -> Option<mpsc::Sender<X11ConnToRelay>> {
        self.conn_to_relay.read().await.get(&disp.get()).cloned()
    }

    pub async fn register_x11_connection(
        &self,
        conn_id: ConnectionId,
        disp: DisplayNumber,
        sender: mpsc::Sender<X11RelayToConn>,
    ) -> rx11_core::error::Result<()> {
        let conn_val = conn_id.get();
        let disp_val = disp.get();
        {
            let display_conns = self.display_conns.lock().await;
            let count = display_conns.get(&disp_val).map(|s| s.len()).unwrap_or(0);
            if count >= ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY {
                return Err(rx11_core::error::Rx11Error::Protocol(format!(
                    "Too many X11 connections for display :{} (max {})",
                    disp_val, ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY
                )));
            }
        }
        self.connections.lock().await.insert(
            conn_val,
            ConnState {
                display: disp,
                sender,
            },
        );
        self.display_conns
            .lock()
            .await
            .entry(disp_val)
            .or_default()
            .insert(conn_val);
        Ok(())
    }

    pub async fn unregister_x11_connection(&self, conn_id: ConnectionId) {
        if let Some(state) = self.connections.lock().await.remove(&conn_id.get()) {
            let disp = state.display;
            if let Some(set) = self.display_conns.lock().await.get_mut(&disp.get()) {
                set.remove(&conn_id.get());
            }
        }
    }

    pub async fn send_to_x11_connection(&self, conn_id: ConnectionId, data: Vec<u8>) {
        if let Some(state) = self.connections.lock().await.get(&conn_id.get()) {
            if state.sender.send(X11RelayToConn::Data(bytes::Bytes::from(data))).await.is_err() {
                warn!("Failed to send data to x11 connection {}, channel full or closed", conn_id);
            }
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
            if let Ok(disp_num) = DisplayNumber::new(disp) {
                self.destroy_session(disp_num).await;
            }
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

#[cfg(not(test))]
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

#[cfg(test)]
async fn xauth_add(_disp: u16, _auth_data: &[u8], _auth_name: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(test))]
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

#[cfg(test)]
async fn xauth_remove(_disp: u16) {}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBinder;

    #[async_trait::async_trait]
    impl X11DisplayBinder for MockBinder {
        async fn bind_display(&self, _disp: u16) -> anyhow::Result<()> {
            Ok(())
        }
        async fn unbind_display(&self, _disp: u16) {}
    }

    fn test_manager() -> SessionManager {
        SessionManager::new()
    }

    #[tokio::test]
    async fn test_create_and_destroy_session() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        let session = mgr
            .create_session(
                DisplayNumber::new(10).unwrap(),
                "MIT-MAGIC-COOKIE-1".into(),
                vec![1, 2, 3, 4],
                "client-1".into(),
            )
            .await
            .unwrap();

        assert_eq!(session.display.get(), 10);
        assert!(mgr.owns_session(DisplayNumber::new(10).unwrap(), "client-1").await);
        assert!(!mgr.owns_session(DisplayNumber::new(10).unwrap(), "client-2").await);

        mgr.destroy_session(session.display).await;
        assert!(!mgr.owns_session(DisplayNumber::new(10).unwrap(), "client-1").await);
    }

    #[tokio::test]
    async fn test_create_auto_session() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        let session = mgr
            .create_session_auto("MIT-MAGIC-COOKIE-1".into(), vec![1, 2, 3], "client-1".into())
            .await
            .unwrap();

        assert_eq!(session.display.get(), 0);
    }

    #[tokio::test]
    async fn test_duplicate_display_rejected() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        mgr.create_session(
            DisplayNumber::new(5).unwrap(),
            "auth".into(),
            vec![1],
            "client-1".into(),
        )
        .await
        .unwrap();

        let result = mgr
            .create_session(
                DisplayNumber::new(5).unwrap(),
                "auth".into(),
                vec![1],
                "client-2".into(),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_resume_session() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        let session = mgr
            .create_session(
                DisplayNumber::new(20).unwrap(),
                "auth".into(),
                vec![1],
                "client-1".into(),
            )
            .await
            .unwrap();

        let resumed = mgr
            .try_resume_session(&session.id, "client-2".into())
            .await
            .unwrap();

        assert_eq!(resumed.display.get(), 20);
        assert_eq!(resumed.client_id, "client-2");
    }

    #[tokio::test]
    async fn test_resume_nonexistent_session() {
        let mgr = test_manager();
        let fake_id = SessionId::new("nonexistent".into()).unwrap();

        let result = mgr.try_resume_session(&fake_id, "client-2".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_register_unregister_x11_connection() {
        let mgr = test_manager();

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let conn_id = ConnectionId::new(42);
        let disp = DisplayNumber::new(1).unwrap();

        mgr.register_x11_connection(conn_id, disp, tx).await.unwrap();

        let conn_state = {
            let conns = mgr.connections.lock().await;
            conns.get(&conn_id.get()).is_some()
        };
        assert!(conn_state);

        mgr.unregister_x11_connection(conn_id).await;

        let conn_state = {
            let conns = mgr.connections.lock().await;
            conns.get(&conn_id.get()).is_some()
        };
        assert!(!conn_state);
    }

    #[tokio::test]
    async fn test_max_x11_connections_per_display() {
        let mgr = test_manager();
        let disp = DisplayNumber::new(1).unwrap();

        for i in 0..ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY {
            let (tx, _rx) = tokio::sync::mpsc::channel(16);
            mgr.register_x11_connection(ConnectionId::new(i as u32 + 1), disp, tx)
                .await
                .unwrap();
        }

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let result = mgr
            .register_x11_connection(
                ConnectionId::new(ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY as u32 + 1),
                disp,
                tx,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_relay_registration() {
        let mgr = test_manager();

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let disp = DisplayNumber::new(7).unwrap();

        assert!(mgr.get_x11_event_sender(disp).await.is_none());

        mgr.register_x11_relay(disp, tx).await;
        assert!(mgr.get_x11_event_sender(disp).await.is_some());

        mgr.unregister_x11_relay(disp).await;
        assert!(mgr.get_x11_event_sender(disp).await.is_none());
    }

    #[tokio::test]
    async fn test_destroy_all_sessions() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        for i in 0..3 {
            mgr.create_session(
                DisplayNumber::new(i).unwrap(),
                "auth".into(),
                vec![1],
                format!("client-{}", i),
            )
            .await
            .unwrap();
        }

        mgr.destroy_all_sessions().await;

        for i in 0..3 {
            assert!(!mgr.owns_session(DisplayNumber::new(i).unwrap(), &format!("client-{}", i)).await);
        }
    }
}
