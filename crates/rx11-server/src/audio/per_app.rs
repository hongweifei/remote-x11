//! Per-application capture: one recorder child and one Opus encoder per
//! APPLICATION, not per connection.
//!
//! The route, as measured on a PipeWire host:
//!
//! - The application's own PipeWire stream (the node with
//!   `application.process.id` equal to one of the application's pids) is
//!   found in the graph. It exists only while the application makes a
//!   sound: an application that opens its stream on demand (a browser's
//!   audio service) has no node while it is silent.
//! - The capture is pointed at that stream by its **object serial**:
//!   `pw-cat --record --raw --target=<serial>`. The serial is PipeWire's
//!   stable identity for the stream; the graph's object id is not stable
//!   (it is recycled), and a target given as an id captures the SINK MIX
//!   (measured: with the id the capture carries both applications' sounds,
//!   6752 against 6312; with the serial it carries only the wanted one,
//!   10882 against 3.8).
//!
//! Pointing the capture at the serial means no port pairing and no link
//! step: the capture starts already bound to the application, and it keeps
//! flowing (a port-linked capture stops after about 200 KB).
//!
//! The capture is SHARED: one application, one recorder, one encoder —
//! however many X11 connections it holds and however many clients watch
//! it. Only the transport repeats the encoded sound.
//!
//! When a channel's sound exists depends on the application: an
//! application that opens a stream only while it plays (a browser's audio
//! service) has no PipeWire node while it is silent, so its capture
//! starts when it starts playing. An application that keeps a stream open
//! (a player) keeps its node for its whole life, so its capture lasts as
//! long as the application does.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use rx11_core::audio::{
    capture_command_for_application, ApplicationId, FormatGate, Packetizer, PipeWireTopology,
    ProcessTree,
};
use rx11_core::protocol::{
    ChannelCloseMessage, ChannelKind, ChannelOpenMessage, Frame, StreamDataMessage,
};
use rx11_core::queue::{OutboundSender, PrioritySendError};
use rx11_core::types::ConnectionId;

use super::Channel;

/// Read granularity from the capture child; the packetizer re-slices to
/// frames regardless of how the pipe chunks.
const READ_CHUNK: usize = 8 * 1024;

/// How often the PipeWire graph is re-read while waiting for something to
/// appear in it.
const GRAPH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Wait for the application's output stream to appear in the graph, and
/// return its object serial.
///
/// The application's PipeWire node exists only while the application
/// makes a sound: an application that opens its stream on demand (a
/// browser's audio service) has NO node while it is silent. Waiting for
/// it is what makes "the application starts playing" work, and the wait is
/// unbounded — it ends when the application goes away, not when a
/// deadline passes. A capture that started before the application made a
/// sound would carry the wrong sound (the sink mix) or nothing at all.
///
/// The serial is PipeWire's STABLE identity for the stream. The graph's
/// object id is not stable — it is recycled — and a target given as an id
/// resolves to the sink MIX (measured: with the id the capture carries
/// both applications' sounds, 6752 against 6312; with the serial it
/// carries only the wanted one, 10882 against 3.8).
async fn wait_for_application_serial(
    sound_root: rx11_core::audio::ApplicationId,
    stop: &AtomicBool,
) -> Result<u32, String> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("the application went away before it made a sound".to_string());
        }
        if let Ok(topology) = PipeWireTopology::read() {
            for node in topology.application_output_nodes() {
                let Some(node_pid) = node.process_id else {
                    continue;
                };
                let Some(node_root) =
                    ProcessTree::of(node_pid).and_then(|tree| tree.application_root())
                else {
                    continue;
                };
                if node_root != sound_root {
                    continue;
                }
                let Some(serial) = node.object_serial else {
                    continue;
                };
                info!(
                    "audio: the application's node {} (process {node_pid}, media {:?}) is application {} with serial {serial}",
                    node.id,
                    node.media_name,
                    node_root.get()
                );
                return Ok(serial);
            }
        }
        tokio::time::sleep(GRAPH_POLL_INTERVAL).await;
    }
}

/// One application being captured.
///
/// The capture child and the Opus encoder belong to the APPLICATION: every
/// X11 connection of that application listens to the same encoded sound,
/// and the sound is captured and encoded exactly once.
pub(crate) struct ApplicationCapture {
    /// The application's identity (its application root).
    pub(crate) app: ApplicationId,
    /// The audio channels this application's sound is shipped on: one per
    /// X11 connection of the application, each tagged with its own X11
    /// connection.
    ///
    /// Shared with the capture task: a connection that appears while the
    /// capture is already running joins the sound that is flowing.
    channels: Arc<Mutex<Vec<Channel>>>,
    /// The capture task: it owns the child's lifetime and the encoder.
    task: Option<JoinHandle<()>>,
    /// Raised when the capture must stop.
    stop: Arc<AtomicBool>,
}

impl ApplicationCapture {
    pub(crate) fn new(app: ApplicationId, _sequence: u64) -> Self {
        Self {
            app,
            channels: Arc::new(Mutex::new(Vec::new())),
            task: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Add an X11 connection's channel to this application's sound.
    pub(crate) fn add_channel(
        &mut self,
        app_connection_id: ConnectionId,
        outbound: OutboundSender,
    ) {
        self.channels.lock().expect("audio channels").push(Channel {
            audio_channel_id: crate::x11_listener::next_connection_id(),
            app_connection_id,
            outbound,
            opened: false,
            shed: 0,
        });
    }

    /// Remove an X11 connection's channel.
    pub(crate) fn remove_channel(&mut self, app_connection_id: ConnectionId) {
        let mut channels = self.channels.lock().expect("audio channels");
        let Some(index) = channels
            .iter()
            .position(|c| c.app_connection_id == app_connection_id)
        else {
            return;
        };
        let channel = channels.remove(index);
        if channel.opened {
            let _ = channel
                .outbound
                .send_priority(Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: channel.audio_channel_id,
                }));
        }
    }

    /// Whether any channel is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.channels.lock().expect("audio channels").is_empty()
    }

    /// The channels this application speaks on: one per X11 connection.
    #[cfg(test)]
    pub(crate) fn channel_ids(&self) -> Vec<Channel> {
        self.channels.lock().expect("audio channels").clone()
    }

    /// Ship one packet on every channel of this application.
    #[cfg(test)]
    pub(crate) fn fan_out(&mut self, seq: u32, packet: &[u8]) {
        let mut channels = self.channels.lock().expect("audio channels");
        fan_out(&mut channels, seq, packet);
    }

    /// Start capturing this application.
    ///
    /// The capture is started in the background: a failure is reported and
    /// leaves the application silent, never the X11 session broken.
    pub(crate) fn start(&mut self, runtime: &rx11_core::config::RuntimeConfig) {
        if self.task.is_some() {
            return;
        }
        let app = self.app;
        let stop = Arc::clone(&self.stop);
        let channels = Arc::clone(&self.channels);
        let bitrate = runtime.audio_bitrate_kbps;
        let sound_pid = app.get();
        self.task = Some(tokio::spawn(async move {
            let outcome = capture_loop(bitrate, sound_pid, channels, &stop).await;
            match outcome {
                Ok(packets) => {
                    info!(
                        "audio: per-application capture for app {} ended ({packets} packets)",
                        app.get()
                    );
                }
                Err(e) => report_failure(&app, &e),
            }
        }));
    }

    /// Stop capturing this application.
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// The capture body: wait for the application to make a sound, start the
/// capture child pointed at it, encode, and fan out until the child ends
/// or the application is stopped.
async fn capture_loop(
    bitrate_kbps: u32,
    sound_pid: i32,
    channels: Arc<Mutex<Vec<Channel>>>,
    stop: &AtomicBool,
) -> Result<u64, String> {
    // The application root of the process that holds the X11 connection.
    // The sound comes from a DESCENDANT of it (a browser's audio
    // service), so the root is what identifies the application on both
    // sides — PipeWire reports the playing process, the X11 connection
    // reports the holding process, and both resolve to the same root.
    let sound_root = ProcessTree::of(sound_pid)
        .and_then(|tree| tree.application_root())
        .ok_or_else(|| format!("process {sound_pid} has no application"))?;

    // The application opens its PipeWire stream when it makes a sound, and
    // closes it when it stops. Waiting for that node is what makes "the
    // application starts playing" work; the wait ends when the
    // application goes away, not when a deadline passes.
    let serial = wait_for_application_serial(sound_root, stop).await?;

    // The capture is pointed at the application's stream by its serial, so
    // it starts already bound to it: no port pairing, no link step, and
    // no window in which the two ends have to be matched up.
    let mut child = Command::new("pw-cat")
        .args(capture_command_for_application(serial))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // The child's stderr is kept, not discarded: it is the only place
        // the reason for a failed capture shows up, and "the capture ended
        // with no packets" is otherwise undiagnosable.
        .stderr(Stdio::piped())
        // The capture child inherits the server's working directory, and
        // PipeWire's client writes a state file into it. Running the
        // server from a source tree would litter that tree with `pw-cat`
        // files. The child's directory is the system temporary directory,
        // where such files belong.
        .current_dir(std::env::temp_dir())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("capture child failed to start: {e}"))?;

    // The pipe is taken at once, BEFORE the waits below: the capture
    // writes continuously, and a pipe nobody reads fills up and blocks
    // the child — which would stop it from registering its node at all.
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture child has no stdout".to_string())?;
    // The child's stderr is read alongside the audio: a capture that ends
    // without a byte says nothing on its own, and the reason is in the
    // child's own diagnostics.
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture child has no stderr".to_string())?;

    info!("audio: the capture of serial {serial} is reading");

    // The link exists, but the audio needs a moment to travel into the
    // capture stream. Reading straight away would report an empty
    // capture for a link that is in fact good.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut packetizer =
        Packetizer::new(bitrate_kbps).map_err(|e| format!("opus encoder init failed: {e}"))?;
    let mut gate = FormatGate::default();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut sent: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = stdout
            .read(&mut buf)
            .await
            .map_err(|e| format!("capture read failed: {e}"))?;
        if n == 0 {
            // The child ended. Its stderr says why — without it, "the
            // capture ended with no packets" has no cause.
            let mut detail = String::new();
            let _ = child_stderr.read_to_string(&mut detail).await;
            let status = child.wait().await;
            return Err(format!(
                "the capture child ended after {sent} packets (status {status:?}): {}",
                detail.trim()
            ));
        }
        let pcm = gate.feed(&buf[..n]);
        packetizer.push(&pcm, |seq, packet| {
            let mut channels = channels.lock().expect("audio channels");
            fan_out(&mut channels, seq, packet);
            sent += 1;
        });
    }
    let channels = channels.lock().expect("audio channels");
    for channel in channels.iter() {
        if channel.opened {
            let _ = channel
                .outbound
                .send_priority(Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: channel.audio_channel_id,
                }));
        }
    }
    Ok(sent)
}

/// Ship one Opus packet on every channel of the application.
///
/// Each channel opens lazily on its first packet (so a subscriber that
/// never receives audio never sees a `ChannelOpen` it cannot use), and a
/// channel whose lane is gone is swept.
fn fan_out(channels: &mut Vec<Channel>, seq: u32, packet: &[u8]) {
    let mut dead = Vec::new();
    for channel in channels.iter_mut() {
        if !channel.opened {
            if channel
                .outbound
                .send_priority(Frame::ChannelOpen(ChannelOpenMessage {
                    connection_id: channel.audio_channel_id,
                    kind: ChannelKind::audio(),
                    display: None,
                    // The application label: the X11 connection whose
                    // application produced this sound.
                    app: Some(channel.app_connection_id),
                }))
                .is_err()
            {
                dead.push(channel.audio_channel_id);
                continue;
            }
            channel.opened = true;
        }
        let frame = Frame::StreamData(StreamDataMessage {
            connection_id: channel.audio_channel_id,
            sequence_id: seq,
            data: Bytes::copy_from_slice(packet),
        });
        match channel.outbound.send_priority(frame) {
            Ok(()) => {}
            Err(PrioritySendError::Full) => {
                channel.shed += 1;
            }
            Err(PrioritySendError::Closed) => {
                dead.push(channel.audio_channel_id);
            }
        }
    }
    if !dead.is_empty() {
        channels.retain(|c| !dead.contains(&c.audio_channel_id));
    }
}

/// Report a capture failure. Audio failures never take an X11 session
/// down: they leave the channel silent and say so.
pub(crate) fn report_failure(app: &ApplicationId, reason: &str) {
    warn!(
        "audio: per-application capture failed for app {}: {reason}",
        app.get()
    );
    info!("audio: this application's audio stays silent");
}
