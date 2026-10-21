//! Shared audio capture for the whole relay: ONE recorder child and ONE
//! Opus encoder per server, fan-out of every packet to all clients that
//! negotiated the `audio` channel.
//!
//! The per-connection pump this replaced recorded the same desktop sink N
//! times for N clients — N `parec` children, N encoders, N identical
//! streams. Real-time semantics make the fan-out cheap: every subscriber
//! rides its own real-time lane (a slow client sheds packets on its own
//! and never couples to others), and late joiners start from the next
//! packet — no keyframe concept, no state handoff. Byte equality ACROSS
//! clients is deliberately NOT a property: each client's Opus decoder
//! carries its own state (a joiner's first frame decodes low-energy and
//! converges by frame two), so everyone hears a valid stream of their own.
//!
//! Lifecycle: attach (first session ack of an audio-capable client) lazily
//! spawns the recorder; detach is implicit — a dead client's lane reports
//! `Closed` on the next packet and its subscription is swept. The recorder
//! stops when the last listener leaves and restarts on demand; if the
//! recorder stream ends on its own (override commands like `cat file`),
//! every opened subscriber receives `ChannelClose` so client outlets tear
//! down instead of hanging open.
//!
//! Failures stay non-fatal everywhere: a missing audio stack warns per
//! attach attempt and the X11 session never notices.

use std::sync::{Arc, Mutex};

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

/// Read granularity from the recorder child; the packetizer re-slices to
/// frames regardless of how the pipe chunks.
const READ_CHUNK: usize = 8 * 1024;
/// Rate-limit the "this subscriber's link is behind" warning.
const DROP_LOG_EVERY: u64 = 250;

struct Subscriber {
    conn: ConnectionId,
    outbound: OutboundSender,
    /// `ChannelOpen` was delivered on the real-time lane: only opened
    /// subscribers may see data, and only opened subscribers need a
    /// `ChannelClose` on teardown.
    opened: bool,
    shed: u64,
}

#[derive(Default)]
struct HubState {
    subscribers: Vec<Subscriber>,
    /// A recorder task owns the capture right now. The flag flips inside
    /// the same lock section that reads/extends `subscribers`, so an
    /// attach can never push into a list that a finishing recorder already
    /// decided was empty.
    running: bool,
    task: Option<JoinHandle<()>>,
}

struct Inner {
    runtime: RuntimeConfig,
    state: Mutex<HubState>,
}

/// Cheap-clone handle: one per `RelayServer::run` shared into every
/// connection task. The recorder is aborted when the last clone drops.
#[derive(Clone)]
pub(crate) struct AudioHub(Arc<Inner>);

impl AudioHub {
    pub(crate) fn new(runtime: RuntimeConfig) -> Self {
        Self(Arc::new(Inner {
            runtime,
            state: Mutex::new(HubState::default()),
        }))
    }

    /// Register one audio-capable client connection. Idempotent per
    /// `connection_id`; spawns the shared recorder lazily if none runs.
    pub(crate) fn attach(&self, outbound: OutboundSender, connection_id: ConnectionId) {
        let spawn = {
            let mut st = self.state();
            if st.subscribers.iter().any(|s| s.conn == connection_id) {
                return;
            }
            st.subscribers.push(Subscriber {
                conn: connection_id,
                outbound,
                opened: false,
                shed: 0,
            });
            if st.running {
                false
            } else {
                st.running = true; // claim before unlocking: attaches serialize
                true
            }
        };
        if spawn {
            let hub = self.clone();
            let handle = tokio::spawn(async move { hub.run_recorder().await });
            self.state().task = Some(handle);
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.0.state.lock().expect("audio hub state")
    }

    /// Recorder body: runs until the last subscriber is gone, the child
    /// EOFs or dies, or setup fails. `finish` always releases the running
    /// claim inside the same lock section that reads the subscriber list,
    /// keeping attach race-free.
    async fn run_recorder(&self) {
        let rt = &self.0.runtime;
        let Some(argv) = resolve_command(&rt.audio_record_command, record_command_candidates())
        else {
            warn!(
                "audio: no recorder available (need parec; or set [runtime] audio_record_command); fan-out disabled"
            );
            self.finish(false);
            return;
        };
        let Some((program, args)) = argv.split_first() else {
            self.finish(false);
            return;
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
                warn!("audio: recorder '{program}' failed to start: {e}; fan-out disabled");
                self.finish(false);
                return;
            }
        };
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut packetizer = match Packetizer::new(rt.audio_bitrate_kbps) {
            Ok(p) => p,
            Err(e) => {
                warn!("audio: opus encoder init failed: {e}; fan-out disabled");
                self.finish(false);
                let _ = child.kill().await;
                return;
            }
        };

        let listeners = self.state().subscribers.len();
        if listeners == 0 {
            // Everyone attached-then-detached before the spawn completed.
            self.finish(false);
            let _ = child.kill().await;
            return;
        }
        info!(
            "audio: shared capture via {program} at {} kbit/s ({listeners} listener(s))",
            rt.audio_bitrate_kbps
        );

        let mut gate = FormatGate::default();
        let mut buf = vec![0u8; READ_CHUNK];
        let mut sent: u64 = 0;
        let mut stop = false;

        while !stop {
            let n = match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break, // recorder exited or pipe broke
                Ok(n) => n,
            };
            let pcm = gate.feed(&buf[..n]);
            packetizer.push(&pcm, |seq, pkt| {
                let mut st = self.state();
                let mut dead: Vec<ConnectionId> = Vec::new();
                for sub in st.subscribers.iter_mut() {
                    if !sub.opened {
                        if sub
                            .outbound
                            .send_priority(Frame::ChannelOpen(ChannelOpenMessage {
                                connection_id: sub.conn,
                                kind: ChannelKind::audio(),
                                display: None,
                            }))
                            .is_err()
                        {
                            dead.push(sub.conn);
                            continue;
                        }
                        sub.opened = true;
                    }
                    let frame = Frame::StreamData(StreamDataMessage {
                        connection_id: sub.conn,
                        sequence_id: seq,
                        data: Bytes::copy_from_slice(pkt),
                    });
                    match sub.outbound.send_priority(frame) {
                        Ok(()) => {}
                        Err(PrioritySendError::Full) => {
                            sub.shed += 1;
                            if sub.shed % DROP_LOG_EVERY == 1 {
                                warn!(
                                    "audio: {} packets shed for {} — its link is behind",
                                    sub.shed, sub.conn
                                );
                            }
                        }
                        Err(PrioritySendError::Closed) => dead.push(sub.conn),
                    }
                }
                if !dead.is_empty() {
                    st.subscribers.retain(|s| !dead.contains(&s.conn));
                }
                sent += 1;
                if st.subscribers.is_empty() {
                    stop = true; // last listener gone: stop recording
                }
            });
        }

        // Natural end (EOF, error, or last listener gone): opened
        // subscribers get an orderly ChannelClose.
        self.finish(true);
        let _ = child.kill().await;
        info!("audio: shared capture ended ({sent} packets fanned out)");
    }

    /// Release the running claim; with `close`, send `ChannelClose` to
    /// opened subscribers and empty the list (the next attach starts a
    /// fresh recorder). Lock-held, synchronous lane writes only — no await
    /// inside.
    fn finish(&self, close: bool) {
        let mut st = self.state();
        st.running = false;
        st.task = None;
        if close {
            for sub in st.subscribers.iter() {
                if sub.opened {
                    let _ = sub
                        .outbound
                        .send_priority(Frame::ChannelClose(ChannelCloseMessage {
                            connection_id: sub.conn,
                        }));
                }
            }
            st.subscribers.clear();
        } else {
            // Setup never succeeded: nobody was opened, so drop the
            // pending subscriptions — a later attach retries fresh.
            st.subscribers.retain(|s| s.opened);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Last hub clone gone (server returned, connections dropped): stop
        // the detached recorder task; kill_on_drop reaps the child.
        if let Some(task) = self.state.get_mut().expect("audio hub state").task.take() {
            task.abort();
        }
    }
}
