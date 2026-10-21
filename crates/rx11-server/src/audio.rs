//! Server-side audio capture pump: records the machine's default output
//! sink (whatever the desktop session plays), packetizes it to Opus and
//! streams it downlink on the real-time priority lane of the client's
//! outbound queue.
//!
//! Lifecycle mirrors the relay connection: started once the first session
//! ack succeeds, stopped when the writer tears down (priority lane closed)
//! or the recorder child exits. Audio is real-time traffic: a full lane
//! sheds packets (audible clicks, logged), a stall is never acceptable —
//! and a failure here must NEVER take the X11 session down with it. The
//! strict-stream rules of the x11 channel deliberately do not apply.

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use rx11_core::audio::{record_command_candidates, resolve_command, FormatGate, Packetizer};
use rx11_core::config::RuntimeConfig;
use rx11_core::protocol::{
    ChannelCloseMessage, ChannelKind, ChannelOpenMessage, Frame, StreamDataMessage,
};
use rx11_core::queue::{OutboundSender, PrioritySendError};
use rx11_core::types::ConnectionId;

/// Read granularity from the recorder child; frames are re-sliced by
/// [`Packetizer`] regardless of how the pipe chunks.
const READ_CHUNK: usize = 8 * 1024;
/// Rate-limit the "link is behind" warning: one line per this many sheds.
const DROP_LOG_EVERY: u64 = 250;

/// Spawn the capture pump for one client connection. Returns the handle so
/// the connection task can abort it (and reap the child via `kill_on_drop`)
/// when the session ends.
pub(crate) fn spawn_audio_pump(
    outbound: OutboundSender,
    connection_id: ConnectionId,
    runtime: RuntimeConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(argv) =
            resolve_command(&runtime.audio_record_command, record_command_candidates())
        else {
            warn!("audio: no recorder available (need parec; or set [runtime] audio_record_command); forward disabled");
            return;
        };
        run_pump(outbound, connection_id, &argv, runtime.audio_bitrate_kbps).await;
    })
}

async fn run_pump(
    outbound: OutboundSender,
    connection_id: ConnectionId,
    argv: &[String],
    bitrate_kbps: u32,
) {
    let Some((program, args)) = argv.split_first() else {
        return; // resolve_command guarantees non-empty
    };
    let mut child = match Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("audio: recorder '{program}' failed to start: {e}; forward disabled");
            return;
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut packetizer = match Packetizer::new(bitrate_kbps) {
        Ok(p) => p,
        Err(e) => {
            warn!("audio: opus encoder init failed: {e}; forward disabled");
            let _ = child.kill().await;
            return;
        }
    };

    info!("audio: capturing output sink via {program} at {bitrate_kbps} kbit/s ({connection_id})");
    if outbound
        .send_priority(Frame::ChannelOpen(ChannelOpenMessage {
            connection_id,
            kind: ChannelKind::audio(),
            display: None,
        }))
        .is_err()
    {
        let _ = child.kill().await;
        return;
    }

    let mut gate = FormatGate::default();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut sent: u64 = 0;
    let mut dropped: u64 = 0;
    let gone = AtomicBool::new(false);

    while !gone.load(Ordering::Relaxed) {
        let n = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => break, // recorder exited or pipe broke
            Ok(n) => n,
        };
        let pcm = gate.feed(&buf[..n]);
        packetizer.push(&pcm, |seq, pkt| {
            let frame = Frame::StreamData(StreamDataMessage {
                connection_id,
                sequence_id: seq,
                data: Bytes::copy_from_slice(pkt),
            });
            match outbound.send_priority(frame) {
                Ok(()) => sent += 1,
                Err(PrioritySendError::Full) => {
                    dropped += 1;
                    if dropped % DROP_LOG_EVERY == 1 {
                        warn!("audio: {dropped} packets shed — link is behind");
                    }
                }
                Err(PrioritySendError::Closed) => gone.store(true, Ordering::Relaxed),
            }
        });
    }

    let _ = outbound.send_priority(Frame::ChannelClose(ChannelCloseMessage { connection_id }));
    let _ = child.kill().await;
    info!("audio: capture ended ({sent} packets sent, {dropped} shed)");
}
