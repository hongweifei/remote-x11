//! Byte-budgeted outbound frame queue with a real-time priority lane.
//!
//! The WAN writer runs ahead of the link: while a frame is being written,
//! everything produced afterwards piles up. Count-bounded mpsc channels
//! (2048 frames) say nothing about *bytes* — with frames up to 256 KB that
//! is a worst-case 512 MB of in-flight queue, i.e. minutes of stale drawing
//! ahead of a mouse move on a 3 Mbit/s link (classic bufferbloat, and the
//! exact "Xfce over a slow VPS feels laggy" complaint).
//!
//! `OutboundQueue` wraps an mpsc channel with an in-flight BYTES budget:
//! senders block once `budget` bytes are queued and are woken as the writer
//! drains. A zero-inflight escape hatch guarantees progress for a frame
//! larger than the budget itself. Latency is now bounded by
//! `budget / link_rate` regardless of frame sizes.
//!
//! The **priority lane** exists for real-time traffic (audio): small,
//! short-lived packets that must not queue behind a redraw burst. It is a
//! bounded, NON-blocking channel outside the byte budget — when it is full
//! new packets are dropped (stale audio is worthless), and the writer task
//! takes from it between bulk frames and between socket commit chunks, so
//! real-time latency is bounded by one commit chunk, never one batch.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, Notify};

use crate::config::BufferDefaults;
use crate::protocol::Frame;

/// Budget value meaning "no byte limit" (still capped by frame capacity).
pub const UNLIMITED: usize = usize::MAX / 2;

#[derive(Debug)]
struct QueueState {
    inflight: AtomicUsize,
    budget: usize,
    gate: Notify,
    /// Packets parked in the priority lane (maintained by both sides of the
    /// pair; a cheap peek so the coalescer can yield mid-batch without
    /// consuming a frame).
    priority_backlog: AtomicUsize,
}

pub struct OutboundSender {
    tx: mpsc::Sender<(Frame, usize)>,
    ptx: mpsc::Sender<Frame>,
    state: Arc<QueueState>,
}

impl Clone for OutboundSender {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            ptx: self.ptx.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

pub struct OutboundReceiver {
    rx: mpsc::Receiver<(Frame, usize)>,
    prx: mpsc::Receiver<Frame>,
    state: Arc<QueueState>,
    /// Sticky once the priority lane reports closed: the fused waiter then
    /// stops polling it and never spins on a permanently-ready `None`.
    priority_done: bool,
}

/// Create the (sender, receiver) pair. `frame_capacity` bounds queued frame
/// COUNT (fairness/pinning), `budget_bytes` bounds queued BYTES (latency).
pub fn outbound_channel(
    frame_capacity: usize,
    budget_bytes: usize,
) -> (OutboundSender, OutboundReceiver) {
    let (tx, rx) = mpsc::channel(frame_capacity);
    let (ptx, prx) = mpsc::channel(BufferDefaults::PRIORITY_LANE_CAPACITY);
    let state = Arc::new(QueueState {
        inflight: AtomicUsize::new(0),
        budget: budget_bytes.max(1),
        gate: Notify::new(),
        priority_backlog: AtomicUsize::new(0),
    });
    (
        OutboundSender {
            tx,
            ptx,
            state: Arc::clone(&state),
        },
        OutboundReceiver {
            rx,
            prx,
            state,
            priority_done: false,
        },
    )
}

impl OutboundSender {
    /// Queue a frame on the bulk lane, awaiting byte budget AND channel
    /// capacity. Returns the frame back on a closed queue (matching
    /// `mpsc::Sender::send` ergonomics at our call sites, which only check
    /// `is_err()`).
    pub async fn send(&self, frame: Frame) -> Result<(), Frame> {
        let size = frame.encoded_size();
        let mut pending = Some(frame);
        loop {
            // Subscribe before checking state so no release can slip through
            // the gap between load and await.
            let notified = self.state.gate.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let inflight = self.state.inflight.load(Ordering::Relaxed);
            let room = inflight == 0 || inflight.saturating_add(size) <= self.state.budget;
            if room {
                let frame = pending.take().expect("frame held across loop");
                match self.tx.try_send((frame, size)) {
                    Ok(()) => {
                        self.state.inflight.fetch_add(size, Ordering::Relaxed);
                        return Ok(());
                    }
                    Err(TrySendError::Full((frame, _))) => pending = Some(frame),
                    Err(TrySendError::Closed((frame, _))) => return Err(frame),
                }
            }
            // No room (budget or capacity): every release notifies the gate,
            // so this wakes on whichever freed first.
            notified.await;
        }
    }

    /// Queue a frame on the real-time lane: never blocks, never charges the
    /// byte budget. `Err` means closed (connection gone) or full — the
    /// caller should drop the frame (stale real-time data is worthless;
    /// blocking here would defeat the lane's purpose).
    pub fn send_priority(&self, frame: Frame) -> Result<(), PrioritySendError> {
        match self.ptx.try_send(frame) {
            Ok(()) => {
                self.state.priority_backlog.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(PrioritySendError::Full),
            Err(TrySendError::Closed(_)) => Err(PrioritySendError::Closed),
        }
    }

    /// Current queued bytes (monitoring/tests).
    pub fn inflight_bytes(&self) -> usize {
        self.state.inflight.load(Ordering::Relaxed)
    }
}

/// Rejection reason for [`OutboundSender::send_priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrioritySendError {
    /// Lane is full: the packet was too late to matter.
    Full,
    /// Writer is gone: the connection is ending.
    Closed,
}

/// Outcome of [`OutboundReceiver::recv_any`], the fused two-lane wait.
#[derive(Debug)]
pub enum LaneItem {
    /// Real-time frame; always wins when both lanes are ready.
    Priority(Frame),
    /// Bulk frame; its share of the byte budget was already released.
    Bulk(Frame),
    /// Bulk lane closed: every sender clone is gone, the writer may finish.
    BulkClosed,
}

impl OutboundReceiver {
    pub async fn recv(&mut self) -> Option<(Frame, usize)> {
        let item = self.rx.recv().await?;
        self.release(item.1);
        Some(item)
    }

    pub fn try_recv(&mut self) -> Result<(Frame, usize), TryRecvError> {
        let item = self.rx.try_recv()?;
        self.release(item.1);
        Ok(item)
    }

    /// Next real-time frame, waiting if the lane is empty. `None` once all
    /// priority senders are gone.
    pub async fn recv_priority(&mut self) -> Option<Frame> {
        let frame = self.prx.recv().await?;
        self.release_priority();
        Some(frame)
    }

    pub fn try_recv_priority(&mut self) -> Option<Frame> {
        let frame = self.prx.try_recv().ok()?;
        self.release_priority();
        Some(frame)
    }

    /// Whether a real-time frame is parked without consuming it: the
    /// coalescer breaks its batch on `true`, the writer drains the lane
    /// between commit chunks.
    pub fn priority_pending(&self) -> bool {
        self.state.priority_backlog.load(Ordering::Relaxed) > 0
    }

    /// Fused wait across both lanes: one poll site, so a writer never has to
    /// hold two mutable borrows of this receiver (which `tokio::select!`
    /// over `recv()` + `recv_priority()` would require). The priority lane
    /// is polled first, so a ready real-time packet always wins the wakeup.
    pub async fn recv_any(&mut self) -> LaneItem {
        use std::task::Poll;
        std::future::poll_fn(|cx| {
            if !self.priority_done {
                match self.prx.poll_recv(cx) {
                    Poll::Ready(Some(frame)) => {
                        self.release_priority();
                        return Poll::Ready(LaneItem::Priority(frame));
                    }
                    Poll::Ready(None) => self.priority_done = true,
                    Poll::Pending => {}
                }
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some((frame, size))) => {
                    self.release(size);
                    Poll::Ready(LaneItem::Bulk(frame))
                }
                Poll::Ready(None) => Poll::Ready(LaneItem::BulkClosed),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
    }

    fn release_priority(&self) {
        // See `release` for the deprecated-spelling note.
        #[allow(deprecated)]
        self.state
            .priority_backlog
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }

    fn release(&self, size: usize) {
        // Saturating: release must never underflow even if an odd path
        // popped more than was charged.
        // `fetch_update` was renamed to `try_update` in Rust 1.99; keep the
        // long-stable spelling for compatibility with earlier toolchains
        // (same treatment as `stats::dec_x11_connections`).
        #[allow(deprecated)]
        self.state
            .inflight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(size))
            })
            .ok();
        self.state.gate.notify_waiters();
    }
}

impl Drop for OutboundReceiver {
    fn drop(&mut self) {
        // Unblock any senders parked on the gate: they will observe the
        // closed channel via `try_send` on their next wake-up.
        self.state.gate.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::StreamDataMessage;
    use crate::types::ConnectionId;
    use bytes::Bytes;

    fn frame(len: usize) -> Frame {
        Frame::StreamData(StreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 0,
            data: Bytes::from(vec![7u8; len]),
        })
    }

    #[tokio::test]
    async fn budget_blocks_sender_until_receiver_drains() {
        let (tx, mut rx) = outbound_channel(16, 1000);
        // 400-byte frames: two fit under 1000, the third must wait.
        tx.send(frame(400)).await.unwrap();
        tx.send(frame(400)).await.unwrap();
        assert_eq!(tx.inflight_bytes(), 2 * (400 + 19));
        let third =
            tokio::time::timeout(std::time::Duration::from_millis(50), tx.send(frame(400))).await;
        assert!(third.is_err(), "third send must gate on byte budget");

        // Draining one frame frees exactly its budget share and admits it.
        let (got, size) = rx.recv().await.unwrap();
        assert!(matches!(got, Frame::StreamData(_)));
        assert_eq!(size, 400 + 19);
        tokio::time::timeout(std::time::Duration::from_millis(200), tx.send(frame(400)))
            .await
            .expect("gated send wakes on release")
            .unwrap();
    }

    #[tokio::test]
    async fn oversize_frame_passes_when_queue_empty() {
        let (tx, mut rx) = outbound_channel(4, 10);
        // One 500-byte frame exceeds the budget but the queue is empty:
        // the zero-inflight escape hatch must admit it (no deadlock).
        tx.send(frame(500)).await.unwrap();
        let (f, _) = rx.recv().await.unwrap();
        assert!(matches!(f, Frame::StreamData(_)));
    }

    #[tokio::test]
    async fn order_is_preserved_through_the_gate() {
        let (tx, mut rx) = outbound_channel(16, 1200);
        let sizes = [100usize, 700, 600, 200];
        let writer = tokio::spawn(async move {
            for (i, s) in sizes.iter().enumerate() {
                let mut f = frame(*s);
                if let Frame::StreamData(m) = &mut f {
                    m.sequence_id = i as u32;
                }
                tx.send(f).await.unwrap();
            }
        });
        for i in 0..sizes.len() {
            let (f, _) = rx.recv().await.unwrap();
            let Frame::StreamData(m) = f else {
                panic!("shape")
            };
            assert_eq!(m.sequence_id as usize, i, "FIFO preserved");
        }
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn closed_receiver_returns_frame_to_sender() {
        let (tx, rx) = outbound_channel(4, 100);
        drop(rx);
        let err = tx.send(frame(10)).await.unwrap_err();
        assert!(matches!(err, Frame::StreamData(_)));
    }

    #[tokio::test]
    async fn priority_lane_bypasses_the_byte_gate() {
        // Bulk budget fully saturated by an oversize frame; a real-time
        // packet must still be accepted instantly.
        let (tx, _rx) = outbound_channel(4, 100);
        tx.send(frame(500)).await.unwrap(); // uses the zero-inflight hatch
        assert!(tx.inflight_bytes() > 100, "budget is over");
        let gated =
            tokio::time::timeout(std::time::Duration::from_millis(50), tx.send(frame(10))).await;
        assert!(gated.is_err(), "bulk lane must still be gated");
        tx.send_priority(frame(80)).expect("priority must not gate");
    }

    #[tokio::test]
    async fn priority_full_is_reported_not_blocked() {
        let (tx, _rx) = outbound_channel(4, 100);
        // Fill the lane to capacity, then one past it: Err(Full), never park.
        for _ in 0..BufferDefaults::PRIORITY_LANE_CAPACITY {
            tx.send_priority(frame(8)).unwrap();
        }
        let err = tx.send_priority(frame(8)).unwrap_err();
        assert!(matches!(err, PrioritySendError::Full));
    }

    #[tokio::test]
    async fn priority_pending_tracks_backlog() {
        let (tx, mut rx) = outbound_channel(4, 1000);
        assert!(!rx.priority_pending());
        tx.send_priority(frame(8)).unwrap();
        assert!(rx.priority_pending());
        let f = rx.try_recv_priority().expect("frame");
        assert!(matches!(f, Frame::StreamData(_)));
        assert!(!rx.priority_pending());
    }

    #[tokio::test]
    async fn closed_priority_lane_reports_closed() {
        let (tx, rx) = outbound_channel(4, 100);
        drop(rx);
        let err = tx.send_priority(frame(8)).unwrap_err();
        assert_eq!(err, PrioritySendError::Closed);
    }
}
