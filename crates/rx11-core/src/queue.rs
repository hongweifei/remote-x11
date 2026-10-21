//! Byte-budgeted outbound frame queue.
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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, Notify};

use crate::protocol::Frame;

/// Budget value meaning "no byte limit" (still capped by frame capacity).
pub const UNLIMITED: usize = usize::MAX / 2;

#[derive(Debug)]
struct QueueState {
    inflight: AtomicUsize,
    budget: usize,
    gate: Notify,
}

pub struct OutboundSender {
    tx: mpsc::Sender<(Frame, usize)>,
    state: Arc<QueueState>,
}

impl Clone for OutboundSender {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

pub struct OutboundReceiver {
    rx: mpsc::Receiver<(Frame, usize)>,
    state: Arc<QueueState>,
}

/// Create the (sender, receiver) pair. `frame_capacity` bounds queued frame
/// COUNT (fairness/pinning), `budget_bytes` bounds queued BYTES (latency).
pub fn outbound_channel(
    frame_capacity: usize,
    budget_bytes: usize,
) -> (OutboundSender, OutboundReceiver) {
    let (tx, rx) = mpsc::channel(frame_capacity);
    let state = Arc::new(QueueState {
        inflight: AtomicUsize::new(0),
        budget: budget_bytes.max(1),
        gate: Notify::new(),
    });
    (
        OutboundSender {
            tx,
            state: Arc::clone(&state),
        },
        OutboundReceiver { rx, state },
    )
}

impl OutboundSender {
    /// Queue a frame, awaiting byte budget AND channel capacity. Returns the
    /// frame back on a closed queue (matching `mpsc::Sender::send` ergonomics
    /// at our call sites, which only check `is_err()`).
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

    /// Current queued bytes (monitoring/tests).
    pub fn inflight_bytes(&self) -> usize {
        self.state.inflight.load(Ordering::Relaxed)
    }
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
}
