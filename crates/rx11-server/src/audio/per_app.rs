//! Per-application capture: one recorder child and one Opus encoder per
//! APPLICATION, not per connection.
//!
//! The route, as measured on a PipeWire host:
//!
//! - `pw-cat --record --raw --target=0` starts a capture stream that is
//!   linked to nothing (`--target=0`). It carries NO
//!   `application.process.id`, so it is recognised by the unique
//!   `node.name` this module gives it.
//! - The application's own PipeWire stream (the node with
//!   `application.process.id` equal to one of the application's pids) is
//!   found in the graph, and its output ports are linked into the capture
//!   stream's input ports with `pw-link`.
//! - The links survive the `pw-link` process exiting, so linking is a
//!   one-shot action.
//!
//! `--target=0` matters: pointing `pw-cat --target` at an application
//! node captures the SINK MIX, not that application — the measured result
//! of `--target` and of a port-linked capture differ completely.
//!
//! Port NUMBERS are used, not port names: every `paplay` instance names
//! its node "paplay", so "paplay:output_FL" does not say WHICH
//! application. The ports are paired by channel suffix (`output_FL` to
//! `input_FL`) so the capture stays stereo.
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
    capture_stream_command, link_command, ApplicationId, FormatGate, Packetizer, PipeWireTopology,
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

/// One application being captured.
///
/// The capture child and the Opus encoder belong to the APPLICATION: every
/// X11 connection of that application listens to the same encoded sound,
/// and the sound is captured and encoded exactly once.
pub(crate) struct ApplicationCapture {
    /// The application's identity (its application root).
    pub(crate) app: ApplicationId,
    /// The unique name of this application's capture stream. The capture
    /// node carries no process id, so this is how it is found in the
    /// graph.
    node_name: String,
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
    pub(crate) fn new(app: ApplicationId, sequence: u64) -> Self {
        let node_name = format!("rx11-audio-app-{}-{}", app.get(), sequence);
        Self {
            app,
            node_name,
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
        let node_name = self.node_name.clone();
        let stop = Arc::clone(&self.stop);
        let channels = Arc::clone(&self.channels);
        let bitrate = runtime.audio_bitrate_kbps;
        let sound_pid = app.get();
        self.task = Some(tokio::spawn(async move {
            let outcome = capture_loop(&node_name, bitrate, sound_pid, channels, &stop).await;
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

/// The capture body: start the child, link the application into it,
/// encode, and fan out until the child ends or the application is stopped.
async fn capture_loop(
    node_name: &str,
    bitrate_kbps: u32,
    sound_pid: i32,
    channels: Arc<Mutex<Vec<Channel>>>,
    stop: &AtomicBool,
) -> Result<u64, String> {
    let mut child = Command::new("pw-cat")
        .args(capture_stream_command(node_name))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("capture child failed to start: {e}"))?;

    let topology =
        PipeWireTopology::read().map_err(|e| format!("reading the audio graph failed: {e}"))?;
    let capture_node = topology
        .node_named(node_name)
        .map(|node| node.id)
        .ok_or_else(|| format!("capture stream '{node_name}' never appeared in the graph"))?;

    // The PipeWire node's process id is the process that PLAYS the sound
    // (a browser's audio service), not the process that holds the X11
    // connection (the browser's main process). They belong to the same
    // application, so the APPLICATION ROOT is what compares them.
    let sound_root = ProcessTree::of(sound_pid)
        .and_then(|tree| tree.application_root())
        .ok_or_else(|| format!("process {sound_pid} has no application"))?;
    let mut pairs = Vec::new();
    for node in topology.application_output_nodes() {
        let Some(node_pid) = node.process_id else {
            continue;
        };
        let node_root = match ProcessTree::of(node_pid).and_then(|tree| tree.application_root()) {
            Some(root) => root,
            None => continue,
        };
        if node_root != sound_root {
            continue;
        }
        pairs.extend(topology.pair_channels(node.id, capture_node));
    }
    if pairs.is_empty() {
        return Err(format!(
            "application {sound_pid} has no output port to capture"
        ));
    }
    for (output_port, input_port) in pairs {
        let argv = link_command(output_port, input_port);
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| format!("linking port {output_port} failed to start: {e}"))?;
        if !status.success() {
            return Err(format!(
                "linking application port {output_port} into the capture failed: {status}"
            ));
        }
    }

    let mut packetizer =
        Packetizer::new(bitrate_kbps).map_err(|e| format!("opus encoder init failed: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture child has no stdout".to_string())?;
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
            break;
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
