//! Client-side audio outlet: decodes Opus packets arriving on the `audio`
//! channel and feeds raw s16le PCM to a local player process
//! (`paplay`).
//!
//! Real-time semantics on this end too: the outlet never backpressures the
//! serve loop. A full buffer means the player is already behind; extra
//! packets are shed (stale audio is worthless), and a dead player takes the
//! channel down — never the session.

use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use rx11_core::audio::{play_command_candidates, resolve_command, Depacketizer};
use rx11_core::config::RuntimeConfig;
use rx11_core::types::ConnectionId;

/// Packets buffered in front of the player (~1.2 s at 50 packets/s). Past
/// that the link is behind and shedding is correct.
const OUTLET_BUFFER: usize = 64;
/// Grace for the player to drain its buffer after the stream ends.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct AudioOutlet {
    pub conn_id: ConnectionId,
    tx: tokio::sync::mpsc::Sender<Bytes>,
    /// Detached on drop; `kill_on_drop` reaps the player regardless.
    _task: JoinHandle<()>,
}

impl AudioOutlet {
    /// Queue one Opus packet. Returns false when the player is gone (caller
    /// tears the outlet down) or when the buffer is full (packet shed —
    /// never block the serve loop for real-time audio).
    pub fn deliver(&self, packet: Bytes) -> bool {
        match self.tx.try_send(packet) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                debug!("audio: player behind; packet shed");
                true // still alive; shedding is the RT policy
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Test seam: an outlet wired to an external receiver (no player child).
    #[cfg(test)]
    pub(crate) fn for_test(conn_id: ConnectionId) -> (Self, tokio::sync::mpsc::Receiver<Bytes>) {
        let (tx, rx) = tokio::sync::mpsc::channel(OUTLET_BUFFER);
        let task = tokio::spawn(std::future::pending::<()>());
        (
            Self {
                conn_id,
                tx,
                _task: task,
            },
            rx,
        )
    }
}

/// Resolve the player and spawn the drain task. `None` means no player is
/// available — the channel is refused and the session continues silently.
pub(crate) fn start_audio_outlet(
    conn_id: ConnectionId,
    runtime: &RuntimeConfig,
) -> Option<AudioOutlet> {
    let argv = resolve_command(&runtime.audio_play_command, play_command_candidates())?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(OUTLET_BUFFER);
    let task = tokio::spawn(async move {
        let Some((program, args)) = argv.split_first() else {
            return; // resolve_command guarantees non-empty
        };
        let mut child = match Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("audio: player '{program}' failed to start: {e}");
                return;
            }
        };
        let Some(mut stdin) = child.stdin.take() else {
            warn!("audio: player stdin unavailable");
            let _ = child.kill().await;
            return;
        };
        let mut depacketizer = match Depacketizer::new() {
            Ok(d) => d,
            Err(e) => {
                warn!("audio: opus decoder init failed: {e}");
                let _ = child.kill().await;
                return;
            }
        };

        let mut played: u64 = 0;
        while let Some(packet) = rx.recv().await {
            let Some(pcm) = depacketizer.push(&packet) else {
                continue; // corrupt packet: RT drop, keep stream
            };
            if stdin.write_all(&pcm).await.is_err() {
                warn!("audio: player process died; outlet closing");
                break;
            }
            played += 1;
        }
        // Stream over: close the write end so the player sees EOF and
        // drains, then reap (kill after a grace period if it lingers).
        let _ = stdin.flush().await;
        drop(stdin);
        if timeout(DRAIN_GRACE, child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
        debug!("audio: played {played} packets");
    });
    Some(AudioOutlet {
        conn_id,
        tx,
        _task: task,
    })
}
