//! Session lifecycle management: display allocation, ownership, grace
//! periods and the registry connecting relay tasks with X11 listener tasks.

mod events;
mod xauth;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{info, warn};

use rx11_core::config::ServerDefaults;
use rx11_core::flow::{EgressGate, FlowMeter};
use rx11_core::types::{ConnectionId, DisplayNumber, SessionId};
use rx11_core::x11recode::RecodeSpec;

use xauth::{xauth_add, xauth_remove};

pub use events::{X11ConnToRelay, X11RelayToConn};

/// Bridges session creation to X11 port binding. Implemented by
/// [`crate::x11_listener::X11Listener`; abstracted to break the dependency
/// cycle between the two modules].
#[async_trait::async_trait]
pub trait X11DisplayBinder: Send + Sync {
    async fn bind_display(&self, disp: u16) -> anyhow::Result<()>;
    async fn unbind_display(&self, disp: u16);
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub display: DisplayNumber,
    pub auth_name: String,
    pub auth_data: Vec<u8>,
    pub client_id: String,
}

/// Read-only view of one live session, for embedders that render their own
/// session list (the GUI console polls [`SessionManager::snapshot`] every
/// frame). Deliberately excludes the xauth cookie: nothing outside the
/// handshake path needs it, and a display list is not a credential store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub display: u16,
    pub session_id: String,
    pub client_id: String,
}

/// Sink + optional protocol-recode policy + the relay's outbound handle
/// (the real-time lane is how pumps report resume edges without waiting
/// for the data path to come unblocked) for one display's relay leg.
type RelayIngress = (
    mpsc::Sender<X11ConnToRelay>,
    Option<RecodeSpec>,
    rx11_core::queue::OutboundSender,
);

/// Per-channel flow-control handles, created by the X11 listener pump and
/// shared with the relay task through the registry:
/// - `upmeter` gauges bytes delivered into the app channel that the pump
///   has not yet written to the application socket (receiver side of the
///   uplink leg);
/// - `egress` is the latch the app-reading pump awaits (enforcement side
///   of the downlink leg, driven by the client's advertised pauses).
pub struct ConnFlow {
    pub upmeter: Arc<FlowMeter>,
    pub egress: Arc<EgressGate>,
}

struct ConnState {
    display: DisplayNumber,
    sender: mpsc::Sender<X11RelayToConn>,
    flow: Arc<ConnFlow>,
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
    /// Per-display relay ingress: the frame sink plus the x11-recode
    /// policy (if negotiated) the listener must apply to app requests.
    conn_to_relay: Arc<RwLock<HashMap<u16, RelayIngress>>>,
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

    /// Bind the X11 port, install the xauth cookie and register the session,
    /// unwinding each step if a later one fails.
    async fn create_session_at(
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
            if let Some(listener) = self.x11_listener.read().await.as_ref() {
                listener.unbind_display(disp_val).await;
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
            xauth_remove(disp_val).await;
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
        Ok((*session).clone())
    }

    pub async fn create_session(
        &self,
        disp: DisplayNumber,
        auth_name: String,
        auth_data: Vec<u8>,
        client_id: String,
    ) -> rx11_core::error::Result<Session> {
        self.create_session_at(disp, auth_name, auth_data, client_id)
            .await
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
                    rx11_core::error::Rx11Error::Protocol("No available display number".into())
                })?
        };
        self.create_session_at(DisplayNumber::new(disp)?, auth_name, auth_data, client_id)
            .await
    }

    pub async fn try_resume_session(
        &self,
        session_id: &SessionId,
        new_client_id: String,
    ) -> rx11_core::error::Result<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.values().find(|s| s.id == *session_id).cloned();
        match session {
            Some(session) => {
                let old_client_id = session.client_id.clone();
                let mut updated = (*session).clone();
                updated.client_id = new_client_id.clone();
                let updated = Arc::new(updated);
                sessions.insert(session.display.get(), updated.clone());
                drop(sessions);

                if let Some(flag) = self.grace_tasks.lock().await.remove(&session.display.get()) {
                    flag.store(true, Ordering::Relaxed);
                }

                warn!(
                    "Session {} resumed for display :{} (old client: {}, new client: {})",
                    session_id, session.display, old_client_id, new_client_id
                );
                Ok((*updated).clone())
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

    /// Every live session, sorted by display number. A read-lock snapshot for
    /// embedders (the GUI session list); never holds the lock while the caller
    /// renders. Ordering is the caller-facing contract: `Vec` is cheap to sort
    /// here because the map is small and the ordering must not depend on hash
    /// iteration order.
    pub async fn snapshot(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        let mut out: Vec<SessionInfo> = sessions
            .values()
            .map(|s| SessionInfo {
                display: s.display.get(),
                session_id: s.id.as_str().to_string(),
                client_id: s.client_id.clone(),
            })
            .collect();
        drop(sessions);
        out.sort_by_key(|s| s.display);
        out
    }

    pub async fn owns_connection(&self, conn_id: ConnectionId, client_id: &str) -> bool {
        let display = {
            let conns = self.connections.lock().await;
            conns.get(&conn_id.get()).map(|state| state.display)
        };
        match display {
            Some(disp) => self.owns_session(disp, client_id).await,
            None => false,
        }
    }

    /// Start the grace period for every session owned by `client_id`. If no
    /// resume happens before the deadline, the sessions are destroyed.
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
                client_id,
                session_id,
                disp,
                ServerDefaults::SESSION_GRACE_PERIOD.as_secs()
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
                warn!(
                    "Grace period expired for display :{}, destroying session",
                    disp
                );
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

    pub async fn register_x11_relay(
        &self,
        disp: DisplayNumber,
        sender: mpsc::Sender<X11ConnToRelay>,
        recode: Option<RecodeSpec>,
        outbound: rx11_core::queue::OutboundSender,
    ) {
        self.conn_to_relay
            .write()
            .await
            .insert(disp.get(), (sender, recode, outbound));
    }

    pub async fn unregister_x11_relay(&self, disp: DisplayNumber) {
        self.conn_to_relay.write().await.remove(&disp.get());
    }

    pub async fn get_x11_relay_ingress(&self, disp: DisplayNumber) -> Option<RelayIngress> {
        self.conn_to_relay.read().await.get(&disp.get()).cloned()
    }

    pub async fn register_x11_connection(
        &self,
        conn_id: ConnectionId,
        disp: DisplayNumber,
        sender: mpsc::Sender<X11RelayToConn>,
        flow: ConnFlow,
    ) -> rx11_core::error::Result<()> {
        let conn_val = conn_id.get();
        let disp_val = disp.get();
        {
            let display_conns = self.display_conns.lock().await;
            let count = display_conns.get(&disp_val).map(|s| s.len()).unwrap_or(0);
            if count >= ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY {
                return Err(rx11_core::error::Rx11Error::Protocol(format!(
                    "Too many X11 connections for display :{} (max {})",
                    disp_val,
                    ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY
                )));
            }
        }
        self.connections.lock().await.insert(
            conn_val,
            ConnState {
                display: disp,
                sender,
                flow: Arc::new(flow),
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

    /// Route payload toward an X11 connection WITHOUT awaiting its channel:
    /// the caller charges the uplink gauge and advertises any pause edge on
    /// the real-time lane BEFORE awaiting the enqueue. Awaiting first would
    /// hide exactly the congestion the advertisement exists to cure — and
    /// can deadlock full-duplex bulk, because the only drain of this
    /// channel is the very pump the advertisement is meant to unblock.
    /// `None` for unknown or foreign-owned connections (the caller logs);
    /// ownership is settled before anything is charged, so denied traffic
    /// never touches the receiver's gauge.
    pub async fn route_to_x11(
        &self,
        conn_id: ConnectionId,
        client_id: &str,
    ) -> Option<(mpsc::Sender<X11RelayToConn>, Arc<ConnFlow>)> {
        let route = {
            let conns = self.connections.lock().await;
            conns
                .get(&conn_id.get())
                .map(|state| (state.display, state.sender.clone(), state.flow.clone()))
        };
        let (display, sender, flow) = route?;
        if !self.owns_session(display, client_id).await {
            return None;
        }
        Some((sender, flow))
    }

    /// The downlink enforcement latch for one channel (the app-reading pump
    /// awaits it). `None` when the channel is unknown.
    pub async fn egress_gate(&self, conn_id: ConnectionId) -> Option<Arc<EgressGate>> {
        self.connections
            .lock()
            .await
            .get(&conn_id.get())
            .map(|s| s.flow.egress.clone())
    }

    /// Ground truth for the advertisement tick: is this channel's uplink
    /// gauge still actually congested? A resume edge reported on the
    /// priority lane can be dropped when that lane is full; the ticker
    /// reconciles stale advertisements against this.
    pub async fn upmeter_latched(&self, conn_id: ConnectionId) -> bool {
        self.connections
            .lock()
            .await
            .get(&conn_id.get())
            .is_some_and(|s| s.flow.upmeter.latched())
    }

    /// Every channel currently multiplexed for `client_id` — the fan-out
    /// surface for a scope-less (`connection_id: None`) pause.
    pub async fn connection_ids_for_client(&self, client_id: &str) -> Vec<ConnectionId> {
        let conns = self.connections.lock().await;
        let sessions = self.sessions.read().await;
        conns
            .iter()
            .filter(|(_, state)| {
                sessions
                    .get(&state.display.get())
                    .map(|s| s.client_id == client_id)
                    .unwrap_or(false)
            })
            .map(|(id, _)| ConnectionId::new(*id))
            .collect()
    }

    /// Clear stale flow state on display (re-)attach: a fresh relay task
    /// must not inherit gates from a dead transport, and a still-full
    /// uplink gauge re-edges on its next charge. (Lock order: display_conns
    /// before connections, per the table above.)
    pub async fn reset_flow_for_display(&self, disp: DisplayNumber) {
        let ids: Vec<u32> = {
            let display_conns = self.display_conns.lock().await;
            display_conns
                .get(&disp.get())
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default()
        };
        let conns = self.connections.lock().await;
        for id in ids {
            if let Some(state) = conns.get(&id) {
                state.flow.egress.set(false);
                state.flow.upmeter.reset();
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
}

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

    /// Flow handles for registry tests: watermarks so large a functional
    /// test can never trip the gauge (flow behavior has its own tests).
    fn test_flow() -> ConnFlow {
        ConnFlow {
            upmeter: FlowMeter::new(1 << 30, 1 << 20),
            egress: EgressGate::new(),
        }
    }

    async fn seeded_manager(disp: u16, client: &str) -> (SessionManager, Session) {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;
        let session = mgr
            .create_session(
                DisplayNumber::new(disp).unwrap(),
                "MIT-MAGIC-COOKIE-1".into(),
                vec![1, 2, 3, 4],
                client.into(),
            )
            .await
            .unwrap();
        (mgr, session)
    }

    #[tokio::test]
    async fn test_create_and_destroy_session() {
        let (mgr, session) = seeded_manager(10, "client-1").await;

        assert_eq!(session.display.get(), 10);
        assert!(
            mgr.owns_session(DisplayNumber::new(10).unwrap(), "client-1")
                .await
        );
        assert!(
            !mgr.owns_session(DisplayNumber::new(10).unwrap(), "client-2")
                .await
        );

        mgr.destroy_session(session.display).await;
        assert!(
            !mgr.owns_session(DisplayNumber::new(10).unwrap(), "client-1")
                .await
        );
    }

    #[tokio::test]
    async fn test_create_auto_session() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        let session = mgr
            .create_session_auto(
                "MIT-MAGIC-COOKIE-1".into(),
                vec![1, 2, 3],
                "client-1".into(),
            )
            .await
            .unwrap();

        assert_eq!(session.display.get(), 0);
    }

    #[tokio::test]
    async fn test_duplicate_display_rejected() {
        let (mgr, _) = seeded_manager(5, "client-1").await;

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
        let (mgr, session) = seeded_manager(20, "client-1").await;

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

        mgr.register_x11_connection(conn_id, disp, tx, test_flow())
            .await
            .unwrap();

        {
            let conns = mgr.connections.lock().await;
            assert!(conns.contains_key(&conn_id.get()));
        }

        mgr.unregister_x11_connection(conn_id).await;

        {
            let conns = mgr.connections.lock().await;
            assert!(!conns.contains_key(&conn_id.get()));
        }
    }

    #[tokio::test]
    async fn test_max_x11_connections_per_display() {
        let mgr = test_manager();
        let disp = DisplayNumber::new(1).unwrap();

        for i in 0..ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY {
            let (tx, _rx) = tokio::sync::mpsc::channel(16);
            mgr.register_x11_connection(ConnectionId::new(i as u32 + 1), disp, tx, test_flow())
                .await
                .unwrap();
        }

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let result = mgr
            .register_x11_connection(
                ConnectionId::new(ServerDefaults::MAX_X11_CONNECTIONS_PER_DISPLAY as u32 + 1),
                disp,
                tx,
                test_flow(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_relay_registration() {
        let mgr = test_manager();

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let (out_tx, _out_rx) = rx11_core::queue::outbound_channel(4, 1024);
        let disp = DisplayNumber::new(7).unwrap();

        assert!(mgr.get_x11_relay_ingress(disp).await.is_none());

        mgr.register_x11_relay(disp, tx, None, out_tx).await;
        assert!(mgr.get_x11_relay_ingress(disp).await.is_some());

        mgr.unregister_x11_relay(disp).await;
        assert!(mgr.get_x11_relay_ingress(disp).await.is_none());
    }

    #[tokio::test]
    async fn test_release_then_resume_cancels_grace() {
        let (mgr, session) = seeded_manager(15, "client-1").await;

        mgr.release_session("client-1").await;
        // Session survives the grace period and a cancel flag is armed.
        assert!(
            mgr.owns_session(DisplayNumber::new(15).unwrap(), "client-1")
                .await
        );
        assert!(mgr.grace_tasks.lock().await.contains_key(&15));

        let resumed = mgr
            .try_resume_session(&session.id, "client-2".into())
            .await
            .unwrap();
        assert_eq!(resumed.client_id, "client-2");
        // Resuming must cancel and remove the pending grace task.
        assert!(!mgr.grace_tasks.lock().await.contains_key(&15));
    }

    #[tokio::test]
    async fn snapshot_lists_sessions_sorted_by_display() {
        let mgr = test_manager();
        mgr.set_x11_listener(Arc::new(MockBinder)).await;

        // Insert out of order: the snapshot contract is display-ascending.
        for (disp, client) in [(12u16, "client-c"), (3, "client-a"), (7, "client-b")] {
            mgr.create_session(
                DisplayNumber::new(disp).unwrap(),
                "auth".into(),
                vec![1],
                client.into(),
            )
            .await
            .unwrap();
        }

        let snapshot = mgr.snapshot().await;
        let displays: Vec<u16> = snapshot.iter().map(|s| s.display).collect();
        assert_eq!(displays, vec![3, 7, 12]);
        assert_eq!(snapshot[0].client_id, "client-a");
        assert_eq!(snapshot[1].client_id, "client-b");
        assert_eq!(snapshot[2].client_id, "client-c");
        // The id must be the real session id, not a display placeholder.
        assert!(!snapshot[0].session_id.is_empty());
        assert_ne!(snapshot[0].session_id, snapshot[1].session_id);
    }

    #[tokio::test]
    async fn snapshot_is_empty_without_sessions_and_tracks_destroy() {
        let (mgr, session) = seeded_manager(31, "client-1").await;
        assert_eq!(mgr.snapshot().await.len(), 1);

        mgr.destroy_session(session.display).await;
        assert!(mgr.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn snapshot_reflects_resume_owner_change() {
        let (mgr, session) = seeded_manager(32, "old-client").await;

        mgr.try_resume_session(&session.id, "new-client".into())
            .await
            .unwrap();

        let snapshot = mgr.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].client_id, "new-client");
        assert_eq!(snapshot[0].session_id, session.id.as_str());
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
            assert!(
                !mgr.owns_session(DisplayNumber::new(i).unwrap(), &format!("client-{}", i))
                    .await
            );
        }
    }

    #[tokio::test]
    async fn route_resolves_owned_channels_only_and_meter_is_shared_truth() {
        let (mgr, session) = seeded_manager(21, "client-flow").await;
        let disp = session.display;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<X11RelayToConn>(8);
        let flow = ConnFlow {
            upmeter: FlowMeter::new(100, 40),
            egress: EgressGate::new(),
        };
        let meter = Arc::clone(&flow.upmeter);
        let conn = ConnectionId::new(77);
        mgr.register_x11_connection(conn, disp, tx, flow)
            .await
            .unwrap();

        // The relay's deliver path: route, charge, advertise, then await.
        let (sender, flow) = mgr
            .route_to_x11(conn, "client-flow")
            .await
            .expect("owned channel routes");
        assert!(sender
            .send(X11RelayToConn::Data(bytes::Bytes::from(vec![9u8; 60])))
            .await
            .is_ok());
        assert!(!flow.upmeter.charge(60), "below high: no pause edge");
        assert!(flow.upmeter.charge(60), "crossing high reports the edge");
        assert!(!flow.upmeter.charge(10), "latched: exactly one edge");
        assert!(matches!(rx.recv().await, Some(X11RelayToConn::Data(_))));
        assert_eq!(meter.pending(), 130);
        // Reconcile ground truth sees the latch through the registry:
        assert!(mgr.upmeter_latched(conn).await);

        // Foreign/unknown clients are denied before anything is touched.
        assert!(mgr.route_to_x11(conn, "intruder").await.is_none());
        assert!(mgr
            .route_to_x11(ConnectionId::new(999), "client-flow")
            .await
            .is_none());
        assert_eq!(meter.pending(), 130, "denied routes never charge");
    }

    #[tokio::test]
    async fn re_attach_resets_stale_gates_and_latches() {
        let (mgr, session) = seeded_manager(22, "client-flow").await;
        let disp = session.display;
        let (tx, _rx) = tokio::sync::mpsc::channel::<X11RelayToConn>(8);
        let flow = ConnFlow {
            upmeter: FlowMeter::new(10, 2),
            egress: EgressGate::new(),
        };
        let gate = Arc::clone(&flow.egress);
        let meter = Arc::clone(&flow.upmeter);
        let conn = ConnectionId::new(78);
        mgr.register_x11_connection(conn, disp, tx, flow)
            .await
            .unwrap();

        gate.set(true);
        meter.charge(50);
        assert!(meter.latched());
        assert!(mgr.upmeter_latched(conn).await, "registry sees the latch");

        mgr.reset_flow_for_display(disp).await;
        assert!(
            !gate.paused(),
            "stale peer pause must not survive re-attach"
        );
        assert!(!meter.latched(), "stale latch cleared");
        assert!(!mgr.upmeter_latched(conn).await);
        assert_eq!(meter.pending(), 50, "real backlog accounting survives");
        // A still-full gauge re-edges when new traffic arrives.
        let (_sender, flow) = mgr
            .route_to_x11(conn, "client-flow")
            .await
            .expect("owned after re-attach");
        assert!(
            flow.upmeter.charge(3),
            "congested receiver re-advertises on next charge"
        );
    }

    #[tokio::test]
    async fn scopeless_fan_out_lists_only_owned_channels() {
        let (mgr, _) = seeded_manager(23, "mine").await;
        // Second display owned by another client.
        mgr.create_session(
            DisplayNumber::new(24).unwrap(),
            "a".into(),
            vec![1],
            "theirs".into(),
        )
        .await
        .unwrap();
        for id in 1..=2u32 {
            let (tx, _rx) = tokio::sync::mpsc::channel::<X11RelayToConn>(4);
            mgr.register_x11_connection(
                ConnectionId::new(id),
                DisplayNumber::new(23).unwrap(),
                tx,
                test_flow(),
            )
            .await
            .unwrap();
        }
        let (tx, _rx) = tokio::sync::mpsc::channel::<X11RelayToConn>(4);
        mgr.register_x11_connection(
            ConnectionId::new(3),
            DisplayNumber::new(24).unwrap(),
            tx,
            test_flow(),
        )
        .await
        .unwrap();

        let ids = mgr.connection_ids_for_client("mine").await;
        assert_eq!(ids.len(), 2, "only own-display channels");
        assert!(ids.iter().all(|c| c.get() <= 2));
        assert!(mgr.egress_gate(ConnectionId::new(99)).await.is_none());
    }
}
