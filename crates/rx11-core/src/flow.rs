//! Per-channel flow control primitives: the machinery behind the
//! `FlowControl` capability wire frames (`Pause` / `Resume`).
//!
//! Flow control exists because the byte-budgeted outbound queue bounds the
//! SENDING side's latency but not the CONSUMER's stall: when the client's
//! local X server stops draining, pixels keep arriving until the budget is
//! full, then the producer keeps pushing into kernel buffers anyway — the
//! display lags by whatever the whole pipeline holds. An explicit
//! receiver-advertised pause moves the stall back to its root: the pump
//! stops reading the producing socket, so the application itself blocks in
//! `write()`, and nothing new enters the wire. Lossless by construction —
//! pausing only withholds *future* reads, it never discards queued bytes.
//!
//! Three roles, one per paragraph of that story:
//! - [`FlowMeter`] — the receiver's congestion gauge: pending bytes that
//!   have been accepted for delivery but not yet handed to the final
//!   consumer. Charges on enqueue, discharges on completed write, and
//!   emits exactly one edge per hysteresis transition (high → `Pause`,
//!   low → `Resume`).
//! - [`EgressGate`] — the sender's enforcement latch: the bulk pump awaits
//!   `wait_open()` before each read from its producing socket. Closing it
//!   never drops in-flight bytes; the queue ahead of the gate drains
//!   normally.
//! - [`Advertiser`] — the advertisement keepalive. Flow-control frames ride
//!   the real-time priority lane, which is a DROP surface when full; a
//!   lost `Resume` would strand a paused channel forever, so the
//!   advertiser re-states the current action on every tick and repeats a
//!   `Resume` for a bounded while after the edge, until silence is safe.
//!
//! A `Pause` advertises "stop delivering bulk X11 bytes to me". Control
//! frames and the audio real-time lane are out of scope by design — this
//! module only ever gates the x11 data path, and per channel
//! (`connection_id`), never per session.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::config::{FlowDefaults, RuntimeConfig};
use crate::protocol::FlowControlAction;
use crate::types::ConnectionId;

/// Receiver-side congestion gauge with hysteresis.
///
/// `charge`/`discharge` are safe from multiple tasks (the enqueue site and
/// the drain site run on different tasks and race freely); an edge is
/// reported exactly once per latch transition even under that race.
#[derive(Debug)]
pub struct FlowMeter {
    pending: AtomicUsize,
    high: usize,
    low: usize,
    latched: AtomicBool,
}

impl FlowMeter {
    pub fn new(high: usize, low: usize) -> Arc<Self> {
        // A resume point above the pause point would oscillate; clamp to a
        // sane band instead of trusting config authors.
        let low = low.min(high);
        Arc::new(Self {
            pending: AtomicUsize::new(0),
            high: high.max(1),
            low,
            latched: AtomicBool::new(false),
        })
    }

    /// Build from the runtime profile (`flow_pause_bytes` /
    /// `flow_resume_bytes`).
    pub fn from_runtime(runtime: &RuntimeConfig) -> Arc<Self> {
        Self::new(runtime.flow_pause_bytes, runtime.flow_resume_bytes)
    }

    /// Account bytes accepted for delivery. Returns `true` on the edge
    /// where the gauge crossed `high` and was not latched before — that is
    /// the moment a `Pause` must be advertised.
    pub fn charge(&self, n: usize) -> bool {
        let after = self.pending.fetch_add(n, Ordering::Relaxed) + n;
        after >= self.high && self.latch()
    }

    /// Account bytes handed to the final consumer. Returns `true` on the
    /// edge where the drained backlog fell to `low` while latched — the
    /// moment a `Resume` must be advertised.
    pub fn discharge(&self, n: usize) -> bool {
        // Saturating: paired accounting should never underflow, but a
        // mispaired site must not wrap into a permanently high gauge.
        #[allow(deprecated)]
        let before = self
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(n))
            })
            .unwrap_or(0);
        let after = before.saturating_sub(n);
        self.latched.load(Ordering::Relaxed) && after <= self.low && self.unlatch(after)
    }

    /// Whether the gauge currently considers the receiver congested. Used
    /// by the advertisement tick to reconcile a lost edge report: if the
    /// meter is unlatched but we still advertise `Pause`, emit `Resume`.
    pub fn latched(&self) -> bool {
        self.latched.load(Ordering::Relaxed)
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Drop the latch without touching the backlog count: used when a
    /// session re-attaches and a stale `Pause` must not survive, while a
    /// still-full gauge legitimately re-triggers the edge on next charge.
    pub fn reset(&self) {
        self.latched.store(false, Ordering::Relaxed);
    }

    fn latch(&self) -> bool {
        self.latched
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn unlatch(&self, _after: usize) -> bool {
        self.latched
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

/// Sender-side enforcement latch for one bulk direction.
///
/// The pump calls [`EgressGate::wait_open`] before pulling more bytes from
/// its producing socket. Closing the gate stops *future* reads only; bytes
/// already queued keep flowing (lossless, strict-stream compatible).
#[derive(Debug)]
pub struct EgressGate {
    paused: AtomicBool,
    wake: Notify,
}

impl EgressGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            wake: Notify::new(),
        })
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Set the gate; returns whether the value actually changed. Wakes
    /// waiters on any `store` so a racing `wait_open` re-checks.
    pub fn set(&self, paused: bool) -> bool {
        let changed = self.paused.swap(paused, Ordering::Relaxed) != paused;
        if changed {
            self.wake.notify_waiters();
        }
        changed
    }

    /// Resolve as soon as the gate is open. Subscribes before checking, so
    /// a `set(true)` landing between the load and the park cannot be
    /// missed (same pattern as the outbound queue gate).
    pub async fn wait_open(&self) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.paused.load(Ordering::Relaxed) {
                return;
            }
            notified.await;
        }
    }
}

impl Default for EgressGate {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }
}

/// A flow-control edge for one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowEdge {
    pub connection_id: ConnectionId,
    pub action: FlowControlAction,
}

impl FlowEdge {
    pub fn pause(connection_id: ConnectionId) -> Self {
        Self {
            connection_id,
            action: FlowControlAction::Pause,
        }
    }

    pub fn resume(connection_id: ConnectionId) -> Self {
        Self {
            connection_id,
            action: FlowControlAction::Resume,
        }
    }
}

/// Keeps the PEER informed of our current flow state, tolerating the
/// priority lane's drop surface: every `Pause` we hold is re-advertised on
/// each tick, and every `Resume` is repeated for
/// [`FlowDefaults::RESUME_RETRANSMITS`] ticks after the edge (then the
/// peer's gate is provably as stale as the wire can make it — and the
/// meter's own hysteresis re-edges if congestion persists).
#[derive(Debug, Default)]
pub struct Advertiser {
    /// Channels we currently advertise as paused.
    paused: HashSet<ConnectionId>,
    /// Channels whose `Resume` is still being repeated; the value is the
    /// remaining retransmission budget.
    resuming: HashMap<ConnectionId, u8>,
}

impl Advertiser {
    /// Record a local edge. Returns `true` when a frame must go out right
    /// now (a real state transition), `false` for redundant edges.
    pub fn record(&mut self, edge: FlowEdge) -> bool {
        match edge.action {
            FlowControlAction::Pause => {
                self.resuming.remove(&edge.connection_id);
                self.paused.insert(edge.connection_id)
            }
            FlowControlAction::Resume => {
                if self.paused.remove(&edge.connection_id) {
                    self.resuming
                        .insert(edge.connection_id, FlowDefaults::RESUME_RETRANSMITS);
                    true
                } else {
                    // Nothing advertised as paused: a Resume for a channel
                    // we never gated. Redundant — but the peer's gate may
                    // disagree with our view after a lost Pause re-send,
                    // so pass a single frame through without arming the
                    // budget (set-idempotency keeps it harmless).
                    self.resuming.remove(&edge.connection_id);
                    false
                }
            }
        }
    }

    /// Whether we currently advertise `Pause` for this channel. The server
    /// uses it for the yield rule: a channel we hold paused on the uplink
    /// must not have its downlink gate enforced.
    pub fn is_paused(&self, connection_id: ConnectionId) -> bool {
        self.paused.contains(&connection_id)
    }

    /// Iterate channels we currently advertise paused. The receiver-side
    /// ticker uses it to reconcile stale advertisements against its own
    /// gauges (a lost edge report must not strand a pause).
    pub fn paused_ids(&self) -> impl Iterator<Item = ConnectionId> + '_ {
        self.paused.iter().copied()
    }

    /// Tick: re-advertisement frames owed right now.
    pub fn due(&mut self) -> Vec<FlowEdge> {
        let mut out = Vec::with_capacity(self.paused.len() + self.resuming.len());
        out.extend(self.paused.iter().copied().map(FlowEdge::pause));
        let mut keep = HashMap::with_capacity(self.resuming.len());
        for (id, budget) in self.resuming.drain() {
            out.push(FlowEdge::resume(id));
            if budget > 1 {
                keep.insert(id, budget - 1);
            }
        }
        self.resuming = keep;
        out
    }

    /// Forget a channel entirely (channel closed / connection gone).
    pub fn forget(&mut self, connection_id: ConnectionId) {
        self.paused.remove(&connection_id);
        self.resuming.remove(&connection_id);
    }

    #[cfg(test)]
    fn paused_len(&self) -> usize {
        self.paused.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ConnectionId {
        ConnectionId::new(n)
    }

    #[test]
    fn meter_emits_one_pause_edge_per_latch_cycle() {
        let m = FlowMeter::new(1000, 400);
        assert!(!m.charge(600), "below high: no edge");
        assert!(m.charge(500), "crossing high reports the edge exactly once");
        assert!(!m.charge(4000), "already latched: no second edge");
        assert!(!m.discharge(4200), "still above low: no resume edge");
        assert!(m.discharge(600), "drained to low: resume edge");
        assert!(!m.discharge(1), "unlatched: no repeat edge");
        // A fresh cycle latches again.
        assert!(m.charge(1000), "second cycle pauses again");
    }

    #[test]
    fn meter_discharge_underflow_cannot_wrap_the_gauge() {
        let m = FlowMeter::new(100, 10);
        m.charge(100);
        assert!(m.latched());
        m.discharge(1_000_000);
        assert_eq!(m.pending(), 0, "saturating: never wraps to huge");
        assert!(!m.latched(), "drained reports resume");
    }

    #[test]
    fn meter_clamps_resume_above_pause() {
        let m = FlowMeter::new(100, 500);
        m.charge(100);
        assert!(m.discharge(100), "low was clamped to high: drains resume");
    }

    #[test]
    fn meter_reset_clears_latch_but_keeps_backlog() {
        let m = FlowMeter::new(100, 10);
        m.charge(500);
        m.reset();
        assert!(!m.latched(), "stale latch must not survive re-attach");
        assert_eq!(m.pending(), 500);
        // A still-full gauge re-triggers on the very next charge: resetting
        // never silences a genuinely congested receiver.
        assert!(m.charge(1), "unlatched full gauge re-edges on next charge");
    }

    #[tokio::test]
    async fn gate_wakes_parked_pump_on_open() {
        let g = EgressGate::new();
        assert!(g.set(true), "closed reports change");
        assert!(!g.set(true), "idempotent close reports none");
        let waiter = {
            let g = Arc::clone(&g);
            tokio::spawn(async move {
                g.wait_open().await;
            })
        };
        // The pump is parked: still pending after a moment…
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        assert!(g.set(false), "open reports change");
        tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("wait_open resolves after resume")
            .unwrap();
    }

    #[tokio::test]
    async fn gate_set_between_check_and_park_is_not_missed() {
        // The subscribe-before-check order in wait_open guarantees this:
        // flip the gate while a waiter is between its load and its park.
        let g = EgressGate::new();
        let a = {
            let g = Arc::clone(&g);
            tokio::spawn(async move { g.wait_open().await })
        };
        g.set(true);
        g.set(false);
        tokio::time::timeout(std::time::Duration::from_millis(200), a)
            .await
            .expect("no lost wakeup")
            .unwrap();
    }

    #[test]
    fn advertiser_reports_real_transitions_only() {
        let mut a = Advertiser::default();
        assert!(a.record(FlowEdge::pause(id(1))), "new pause is an edge");
        assert!(
            !a.record(FlowEdge::pause(id(1))),
            "duplicate pause is not an edge"
        );
        assert!(a.record(FlowEdge::resume(id(1))), "resume ends the hold");
        assert!(
            !a.record(FlowEdge::resume(id(1))),
            "resume of an unheld channel is not an edge"
        );
        assert!(!a.is_paused(id(2)));
    }

    #[test]
    fn advertiser_keepalive_repeats_pause_and_ages_out_resume() {
        let mut a = Advertiser::default();
        a.record(FlowEdge::pause(id(7)));
        // While held, every tick re-states the Pause (lane drops are survivable).
        for _ in 0..3 {
            let due = a.due();
            assert_eq!(due.as_slice(), &[FlowEdge::pause(id(7))]);
        }
        a.record(FlowEdge::resume(id(7)));
        // Resume repeats for its full budget, then falls silent.
        for _ in 0..FlowDefaults::RESUME_RETRANSMITS {
            let due = a.due();
            assert_eq!(due.as_slice(), &[FlowEdge::resume(id(7))]);
        }
        assert!(a.due().is_empty(), "aged-out resume stops re-sending");
    }

    #[test]
    fn resume_for_never_paused_channel_is_not_an_edge() {
        let mut a = Advertiser::default();
        assert!(!a.record(FlowEdge::resume(id(3))), "not an edge");
        assert!(a.due().is_empty(), "no retransmit budget armed");
    }

    #[test]
    fn forget_drops_both_tables() {
        let mut a = Advertiser::default();
        a.record(FlowEdge::pause(id(9)));
        a.record(FlowEdge::resume(id(9))); // armed resuming
        a.forget(id(9));
        assert_eq!(a.paused_len(), 0);
        assert!(a.due().is_empty());
    }
}
