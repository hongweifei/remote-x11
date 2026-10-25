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
//! application. Ports are matched by channel suffix (`output_FL` to
//! `input_FL`) so the capture stays stereo.
//!
//! The capture is SHARED: one application, one recorder, one encoder —
//! however many X11 connections it holds and however many clients watch
//! it. Only the transport repeats the encoded sound.

use std::collections::HashMap;
use std::process::Stdio;

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{info, warn};

use rx11_core::audio::{
    capture_stream_command, link_command, FormatGate, Packetizer, PipeWireTopology,
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
    pub(crate) channels: Vec<Channel>,
}

impl ApplicationCapture {
    pub(crate) fn new(app: ApplicationId, sequence: u64) -> Self {
        let node_name = format!("rx11-audio-app-{}-{}", app.get(), sequence);
        Self {
            app,
            node_name,
            channels: Vec::new(),
        }
    }
}

/// The identity of a captured application: the application root process.
pub(crate) type ApplicationId = rx11_core::audio::ApplicationId;

/// Attach one X11 connection to an application: the connection's client
/// gets a channel on the application's sound.
///
/// The channel id is the AUDIO channel's own id — a different namespace
/// from the X11 connection ids, because the client routes audio frames by
/// the channel id and matches the foreground application by the X11
/// connection id.
pub(crate) fn add_channel(
    capture: &mut ApplicationCapture,
    app_connection_id: ConnectionId,
    audio_channel_id: ConnectionId,
    outbound: OutboundSender,
) {
    capture.channels.push(Channel {
        audio_channel_id,
        app_connection_id,
        outbound,
        opened: false,
        shed: 0,
    });
}

/// Ship one Opus packet on every channel of the application.
///
/// Each channel opens lazily on its first packet (so a subscriber that
/// never receives audio never sees a `ChannelOpen` it cannot use), and a
/// channel whose lane is gone is swept.
pub(crate) fn fan_out(capture: &mut ApplicationCapture, seq: u32, packet: &[u8]) {
    let mut dead = Vec::new();
    for channel in capture.channels.iter_mut() {
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
        capture.channels.retain(|c| !dead.contains(&c.audio_channel_id));
    }
}

/// Close every open channel of the application: the application is gone
/// (its last X11 connection disconnected) or its capture ended.
pub(crate) fn close_channels(capture: &mut ApplicationCapture) {
    for channel in capture.channels.iter() {
        if channel.opened {
            let _ = channel
                .outbound
                .send_priority(Frame::ChannelClose(ChannelCloseMessage {
                    connection_id: channel.audio_channel_id,
                }));
        }
    }
    capture.channels.clear();
}

/// The outcome of linking one application's sound into its capture.
pub(crate) struct CaptureLink {
    /// The capture child: the reader of the application's sound.
    pub(crate) child: tokio::process::Child,
    /// The encoder: ONE per application.
    pub(crate) packetizer: Packetizer,
}

/// Start the capture for one application and link the application's sound
/// into it.
///
/// `application_pid` is the process whose PipeWire output stream carries
/// the sound (the application's own process, or the descendant that
/// actually plays the audio).
pub(crate) async fn start_capture(
    capture: &ApplicationCapture,
    application_pid: i32,
    bitrate_kbps: u32,
) -> anyhow::Result<CaptureLink> {
    let topology = PipeWireTopology::read()?;
    let mut child = Command::new("pw-cat")
        .args(capture_stream_command(&capture.node_name))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    // The capture node appears in the graph once the child is up. Reading
    // the graph again is how its input ports are found — the capture
    // carries no process id, so its unique name is the only handle.
    let capture_ports = wait_for_capture_ports(&capture.node_name).await?;
    let application_ports = link_application_ports(&topology, application_pid, &capture_ports)?;

    let packetizer = Packetizer::new(bitrate_kbps)
        .map_err(|e| anyhow::anyhow!("opus encoder init failed: {e}"))?;
    let _ = application_ports;
    Ok(CaptureLink { child, packetizer })
}

/// Wait for the capture stream to appear in the graph and return its input
/// ports, by number.
///
/// The capture child needs a moment to register its node; the graph is
/// read again until the node is there. The wait is bounded: a capture
/// that never registers is a failure, reported as such — never a silent
/// empty capture.
async fn wait_for_capture_ports(node_name: &str) -> anyhow::Result<Vec<u32>> {
    const ATTEMPTS: u32 = 20;
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    for _ in 0..ATTEMPTS {
        if let Ok(topology) = PipeWireTopology::read() {
            if let Some(node) = topology.node_named(node_name) {
                let ports: Vec<u32> = topology
                    .input_ports_of(node.id)
                    .iter()
                    .map(|p| p.id)
                    .collect();
                if !ports.is_empty() {
                    return Ok(ports);
                }
            }
        }
        tokio::time::sleep(INTERVAL).await;
    }
    Err(anyhow::anyhow!(
        "capture stream '{node_name}' never appeared in the PipeWire graph"
    ))
}

/// Link the application's output ports into the capture's input ports.
///
/// The ports are paired by CHANNEL SUFFIX (`output_FL` to `input_FL`), so
/// the capture keeps both channels. Pairing by position alone would put
/// the left channel of the application into the right channel of the
/// capture.
fn link_application_ports(
    topology: &PipeWireTopology,
    application_pid: i32,
    capture_ports: &[u32],
) -> anyhow::Result<Vec<u32>> {
    let capture_suffixes: HashMap<String, u32> = capture_ports
        .iter()
        .filter_map(|id| {
            topology
                .ports
                .iter()
                .find(|p| p.id == *id)
                .map(|p| (channel_suffix(&p.name), p.id))
        })
        .collect();

    let mut linked = Vec::new();
    for node in topology.application_outputs(application_pid) {
        for port in topology.output_ports_of(node.id) {
            let suffix = channel_suffix(&port.name);
            let Some(capture_port) = capture_suffixes.get(&suffix) else {
                warn!(
                    "audio: capture has no '{suffix}' input for application port {}; skipping",
                    port.id
                );
                continue;
            };
            let argv = link_command(port.id, *capture_port);
            let Some((program, args)) = argv.split_first() else {
                continue;
            };
            let status = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            if !status.success() {
                return Err(anyhow::anyhow!(
                    "linking application port {} into capture port {capture_port} failed: {status}",
                    port.id
                ));
            }
            linked.push(port.id);
        }
    }
    if linked.is_empty() {
        return Err(anyhow::anyhow!(
            "application {application_pid} has no output port to capture"
        ));
    }
    Ok(linked)
}

/// The channel part of a port name: `output_FL` and `input_FL` both end
/// in `FL`.
fn channel_suffix(name: &str) -> String {
    name.rsplit('_').next().unwrap_or(name).to_string()
}

/// Read the capture child's bytes and ship them as Opus packets.
///
/// One encoder per application: the packets are shared by every channel
/// of the application.
pub(crate) async fn pump(
    mut link: CaptureLink,
    mut capture: ApplicationCapture,
) -> std::io::Result<u64> {
    let mut stdout = link
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("capture child has no stdout"))?;
    let mut gate = FormatGate::default();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut sent: u64 = 0;
    loop {
        let n = stdout.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let pcm = gate.feed(&buf[..n]);
        link.packetizer.push(&pcm, |seq, packet| {
            fan_out(&mut capture, seq, packet);
            sent += 1;
        });
        if capture.channels.is_empty() {
            break;
        }
    }
    close_channels(&mut capture);
    Ok(sent)
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
