//! End-to-end test of the PER-APPLICATION capture tier: the real
//! PipeWire route, not a stand-in recorder.
//!
//! What it exercises, end to end, on a host with PipeWire:
//!
//! * a real `Stream/Output/Audio` node appears (a `paplay` child playing
//!   a real PCM file);
//! * the server resolves the X11 connection's peer process and attributes
//!   it to an application;
//! * the server starts a real `pw-cat` capture stream and links the
//!   application's output ports into it with `pw-link`;
//! * the captured sound is Opus-encoded and shipped on an audio channel
//!   tagged with the X11 connection;
//! * the client receives that sound — asserted by its ENERGY, not merely
//!   by "frames arrived": the captured PCM must carry the tone that was
//!   played.
//!
//! The test plays the tone and holds the X11 connection from ONE child
//! process, so the server sees one application owning both. The child is
//! this test's own helper binary: it plays the file AND connects to the
//! X11 port, so both belong to the same application.
//!
//! Ignored by default (it drives the session machinery, which shells out
//! to `xauth`); run with an isolated HOME so nothing touches the real one:
//!
//! ```sh
//! HB=$(mktemp -d); HOME=$HB cargo test -p rx11-server --test audio_per_app \
//!     -- --ignored --test-threads=1; rm -rf "$HB"
//! ```
//!
//! `HOME` must be on the SAME command line as the test: `xauth` writes
//! `$HOME/.Xauthority`.

use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use rx11_core::audio::{PCM_FRAME_BYTES, SAMPLE_RATE};
use rx11_core::config::RuntimeConfig;
use rx11_core::protocol::{Capability, Frame};
use rx11_core::transport::Rx11Transport;
use rx11_server::relay::RelayServer;

const TOKEN: &str = "audio-per-app-e2e-token";

/// The tone the test plays. Its frequency is asserted against the
/// captured sound, so a capture of the WRONG application (or of silence)
/// cannot pass.
const TONE_HZ: f64 = 440.0;

/// The helper: it plays the PCM file and holds the X11 connection.
///
/// It is a shell script the test writes and runs: `paplay` is the player
/// (a real PipeWire output stream) and the script's own shell holds the
/// X11 connection. Both live in the SAME process tree with the SAME
/// application root — measured: a shell and its child share the root,
/// while a Rust test process and its child do not (the rule keys on the
/// program the session leader runs).
///
/// The ORDER matters, and it is the opposite of what one would write
/// first: the player is started BEFORE the X11 connection is made.
/// Opening the connection redirects a file descriptor in the shell, and
/// PipeWire's client — which the player inherits — then fails to
/// register its node at all: the player runs, but never appears in the
/// graph, so the server finds nothing to capture. Measured: with the
/// player started first the node appears; with the connection first it
/// never does.
const HELPER_REPORT: &str = r#"#!/bin/bash
# $1: the WAV file, $2: the X11 port, $3: the report file
{
    echo "shell=$$"
    echo "wav=$1"
} > "$3"

# The player starts FIRST and the X11 connection SECOND: opening the
# connection redirects a file descriptor in the shell, and PipeWire's
# client — which the player inherits — then fails to register its node
# at all. Measured: with the player started first the node appears; with
# the connection first it never does.
#
# ONE player for the whole run. A player per connection cycle would
# leave a stream node behind each time and make the node disappear and
# reappear — and a capture whose source disappears ends at once.
#
# The player stays a CHILD of this shell: the server attributes the
# sound to the application of the X11 connection's peer process, and that
# attribution walks the process ancestry. A player detached into its own
# session would have a different application root than the connection, and
# the sound would never be attributed.
paplay "$1" 2>> "$3" &
PLAYER=$!
echo "player=$PLAYER" >> "$3"

# The player dies with this shell, however this shell ends. A player left
# behind would keep playing (and keep its PipeWire node) into the next
# run, where the server would link to THAT stale player instead of the
# live one.
trap 'kill $PLAYER 2>/dev/null' EXIT
trap 'kill $PLAYER 2>/dev/null' TERM INT

# The player needs a moment to register its PipeWire node, and the server
# starts capturing as soon as the connection arrives — it links whatever
# output node it finds THEN. Connecting before the node exists would make
# the server wait for a node that never comes.
sleep 1.5

# Hold the X11 connection open until the server closes it. The
# connection is retried: the server binds the X11 port when the session
# is created, which may be a moment after the helper starts.
#
# The connection is NOT drained: an X11 application sends nothing until
# it has a request to make, so a read would see the end of the stream at
# once and the loop would end — killing the player with it.
while true; do
    if exec 3<>/dev/tcp/127.0.0.1/"$2" 2>/dev/null; then
        # A read with a timeout: it returns immediately on EOF, so the
        # connection is only watched, never drained.
        read -t 3600 -r line <&3 || true
        break
    fi
    sleep 0.1
done
kill $PLAYER 2>/dev/null
wait $PLAYER 2>/dev/null
"#;

fn workdir() -> std::path::PathBuf {
    // The system's temporary directory, NOT the repository: PipeWire's
    // client writes a state file into the working directory of whatever
    // starts it, and the server's capture child runs in the working
    // directory of the server. A test that runs the server from the
    // source tree would litter that tree.
    //
    // The path is absolute: the helper process resolves the WAV against
    // its own working directory.
    let dir = std::env::temp_dir().join(format!(
        "rx11-audio-per-app-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_micros()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// 440 Hz sine at the wire format (s16le, stereo, 48 kHz), as a WAV file
/// the player can read.
fn write_tone_wav(path: &std::path::Path, seconds: usize) {
    let frames = seconds * SAMPLE_RATE as usize;
    let mut pcm = Vec::with_capacity(frames * PCM_FRAME_BYTES);
    for i in 0..frames {
        let t = i as f64 / SAMPLE_RATE as f64;
        let s = ((2.0 * std::f64::consts::PI * TONE_HZ * t).sin() * 12_000.0) as i16;
        for _ in 0..2 {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
    }
    let data_len = pcm.len() as u32;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes()); // byte rate
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&pcm);
    std::fs::write(path, wav).unwrap();
}

/// The RMS of a byte span read as s16le stereo.
///
/// The two channels are interleaved. Measuring them as one stream would
/// make the left channel appear at twice the rate, so the samples are
/// taken per channel and the louder one decides — a mono tone is present
/// in both channels anyway.
fn rms(bytes: &[u8]) -> f64 {
    let mut acc = 0f64;
    let mut count = 0f64;
    for pair in bytes.as_chunks::<4>().0 {
        // s16le stereo: left, right.
        let left = i16::from_le_bytes([pair[0], pair[1]]) as f64;
        let right = i16::from_le_bytes([pair[2], pair[3]]) as f64;
        for sample in [left, right] {
            acc += sample * sample;
            count += 1.0;
        }
    }
    if count == 0.0 {
        return 0.0;
    }
    (acc / count).sqrt()
}

/// The energy of a tone at this frequency in a byte span, by the Goertzel
/// method: the captured sound must carry the tone that was played, not
/// merely any sound.
///
/// The span is s16le STEREO, so the two channels are measured separately
/// and the louder one counts. Treating the interleaved bytes as one
/// stream would sample at twice the rate and halve every frequency.
fn tone_energy(bytes: &[u8], hz: f64) -> f64 {
    let mut best = 0.0f64;
    for channel in 0..2 {
        let samples: Vec<f64> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|quad| {
                let raw = match channel {
                    0 => [quad[0], quad[1]],
                    _ => [quad[2], quad[3]],
                };
                i16::from_le_bytes(raw) as f64
            })
            .collect();
        if samples.is_empty() {
            continue;
        }
        let rate = SAMPLE_RATE as f64;
        let w = 2.0 * std::f64::consts::PI * hz / rate;
        let coeff = 2.0 * w.cos();
        let mut s1 = 0.0;
        let mut s2 = 0.0;
        for s in &samples {
            let s0 = s + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let energy = (s1 * s1 + s2 * s2 - coeff * s1 * s2).abs().sqrt() / samples.len() as f64;
        best = best.max(energy);
    }
    best
}

/// Spawn the helper as a separate process, so the server sees a peer
/// process it can attribute to an application.
fn spawn_helper(
    script: &std::path::Path,
    wav: &std::path::Path,
    x11_port: u16,
    report: &std::path::Path,
) -> std::process::Child {
    // The helper's streams are INHERITED, not redirected: PipeWire's
    // client talks to the sound server over its standard streams when
    // they are pipes, and redirecting them makes the player fail to
    // register a node at all.
    //
    // The working directory is the system temporary directory: PipeWire's
    // client writes a state file into the working directory of whatever
    // starts it, and the helper starts the player.
    let mut command = std::process::Command::new("bash");
    command
        .arg(script)
        .arg(wav)
        .arg(x11_port.to_string())
        .arg(report)
        .current_dir(std::env::temp_dir());
    command.spawn().expect("the helper process starts")
}

/// Stop the helper.
///
/// The signal is SIGTERM, not SIGKILL: the helper's EXIT trap then kills
/// its player. A player left behind would keep its PipeWire node alive,
/// and the next run's capture would link to THAT stale player instead of
/// the live one — the capture would then deliver the wrong sound.
fn stop_helper(helper: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = helper.id() as i32;
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg("--")
            .arg(pid.to_string())
            .status();
        // Give the trap a moment to run, then make sure the shell is
        // gone.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = helper.kill();
    let _ = helper.wait();
}

/// Read frames from the relay until the predicate is satisfied or the
/// deadline passes.
async fn collect_until<F>(
    transport: &mut Rx11Transport<TcpStream>,
    deadline: Instant,
    mut accept: F,
) -> Vec<Frame>
where
    F: FnMut(&Frame) -> bool,
{
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, transport.recv_frame()).await {
            Ok(Ok(frame)) => {
                let done = accept(&frame);
                collected.push(frame);
                if done {
                    return collected;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    collected
}

/// Kill the players an earlier run of this test left behind.
///
/// A leftover player keeps its PipeWire node alive. The capture then links
/// to that stale node instead of the live one, and the stale node carries
/// no sound — the symptom is "the pairing is right, the link is in place,
/// and not a single packet arrives".
///
/// The players are found in the PipeWire graph by their MEDIA: it names
/// the WAV file, and that file's path contains this test's working
/// directory. The node NAME cannot be used — every `paplay` instance calls
/// itself "paplay".
fn kill_players_of_earlier_runs() {
    let Ok(topology) = rx11_core::audio::PipeWireTopology::read() else {
        return;
    };
    let marker = format!("{}/rx11-audio-per-app-e2e-", std::env::temp_dir().display());
    for node in topology.nodes {
        if !node.media_name.contains(&marker) {
            continue;
        }
        let Some(pid) = node.process_id else {
            continue;
        };
        eprintln!("audio per-app e2e: killing the player left by an earlier run (pid {pid})");
        let _ = std::process::Command::new("kill")
            .arg("--")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    // Give the nodes a moment to leave the graph.
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// The per-application capture tier, end to end, on the real PipeWire
/// route.
///
/// The test's helper process plays a 440 Hz tone AND holds the X11
/// connection, so the server sees one application that owns both. The
/// client (this test) reads the frames directly: it asserts that an
/// audio channel opens with the X11 connection's tag, and that the audio
/// on that channel carries the tone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "audio per-app e2e: needs isolated HOME for xauth and a PipeWire host"]
async fn per_application_capture_ships_that_applications_sound() {
    if !rx11_core::audio::per_app_capture_available() {
        eprintln!("skipping: this host has no pw-cat / pw-link / pw-dump");
        return;
    }

    // A player left behind by an earlier run would keep its PipeWire node
    // alive, and the capture would link to THAT stale player instead of
    // the live one — the capture would then carry no sound at all
    // (measured: the stale player's node is in the graph, the live
    // player's is not, and the capture's quant stays at zero).
    //
    // The leftovers are found by their MEDIA, which is this test's own
    // working directory — the node name is useless here, because every
    // `paplay` instance calls itself "paplay".
    kill_players_of_earlier_runs();

    let dir = workdir();
    // The directory goes when the test ends, however it ends: a failed
    // assertion would otherwise leave the tone WAV (34 MB) behind for
    // ever.
    let _scratch = ScratchDir(dir.clone());
    let wav = dir.join("tone.wav");
    // A LONG tone: the player must still be sounding when the server
    // links the capture, and it must outlast the test's wait window.
    // When the player's PipeWire node disappears, the capture stream
    // ends at once (measured: `pw-cat` exits with code 1 and no output),
    // so a tone shorter than the wait window means the capture never
    // delivers anything.
    write_tone_wav(&wav, 180);
    let script = dir.join("helper.sh");
    std::fs::write(&script, HELPER_REPORT).unwrap();
    let report = dir.join("helper.report");

    let relay_port = free_port().await;
    let x11_port = free_port().await;

    // The server: per-application audio ON, no recorder override — the
    // whole-desktop mix must NOT be used here.
    let runtime = RuntimeConfig {
        audio_forward: true,
        audio_per_app: true,
        audio_record_command: None,
        ..RuntimeConfig::default()
    };
    let server = tokio::spawn({
        let addr = format!("127.0.0.1:{relay_port}");
        let runtime = runtime.clone();
        async move {
            let server = RelayServer::with_config(addr, TOKEN.to_string(), x11_port, runtime);
            let _ = server.run().await;
        }
    });

    // Wait for the relay to accept connections.
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", relay_port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The client: this test, speaking the protocol directly.
    let mut transport = connect_relay(relay_port, &runtime).await;

    // Create the session: the server binds the X11 port for it.
    transport
        .send_frame(&Frame::SessionCreate(
            rx11_core::protocol::SessionCreateMessage {
                display: rx11_core::types::DisplayNumber::new(0).unwrap(),
                auth_name: "MIT-MAGIC-COOKIE-1".into(),
                auth_data: vec![1, 2, 3, 4],
            },
        ))
        .await
        .expect("the session request goes out");
    let ack = collect_until(
        &mut transport,
        Instant::now() + Duration::from_secs(10),
        |frame| matches!(frame, Frame::SessionAck(_)),
    )
    .await;
    let Some(Frame::SessionAck(ack)) = ack.iter().find(|f| matches!(f, Frame::SessionAck(_)))
    else {
        panic!("the server never acknowledged the session");
    };
    assert!(ack.success, "the session was created: {ack:?}");

    // The helper: plays the tone and holds the X11 connection.
    let mut helper = spawn_helper(&script, &wav, x11_port, &report);

    // Read until an audio channel opens with the X11 connection's tag,
    // then keep the audio that follows.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut opened_channel: Option<rx11_core::types::ConnectionId> = None;
    let mut audio_app_tag: Option<rx11_core::types::ConnectionId> = None;
    let mut audio_packets: Vec<bytes::Bytes> = Vec::new();
    let mut x11_connection: Option<rx11_core::types::ConnectionId> = None;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(Ok(frame)) = tokio::time::timeout(remaining, transport.recv_frame()).await else {
            break;
        };
        match frame {
            Frame::ChannelOpen(open) if open.kind.is_audio() => {
                assert!(
                    open.app.is_some(),
                    "a per-application audio channel carries the X11 connection it speaks for"
                );
                opened_channel = Some(open.connection_id);
                audio_app_tag = open.app;
            }
            Frame::ChannelOpen(open) if open.kind.is_x11() => {
                x11_connection = Some(open.connection_id);
            }
            Frame::StreamData(data) if Some(data.connection_id) == opened_channel => {
                // The frames carry OPUS packets, not PCM: the count of
                // packets is what measures the sound, not their byte
                // length (a 20 ms Opus frame is a couple of hundred bytes
                // against 3840 bytes of PCM).
                audio_packets.push(data.data.clone());
                if audio_packets.len() >= 20 {
                    break;
                }
            }
            _ => {}
        }
    }

    stop_helper(&mut helper);

    let channel = opened_channel.expect("an audio channel opened");
    let x11 = x11_connection.expect("the X11 channel opened");
    assert_ne!(
        channel, x11,
        "the audio channel has its own id, distinct from the X11 connection"
    );
    // The application label is the X11 CONNECTION whose application made
    // the sound — not the audio channel's own id. The client matches the
    // foreground application by this label, so a wrong one would play the
    // wrong application's sound.
    assert_eq!(
        audio_app_tag,
        Some(x11),
        "the audio channel's application label is the X11 connection it speaks for"
    );
    assert!(
        audio_packets.len() >= 20,
        "the audio channel carried sound: {} packets",
        audio_packets.len()
    );

    // The sound must be the TONE that was played: decode the packets and
    // look at the PCM they carry — the energy at that frequency, and a
    // healthy overall level. Silence or the wrong application cannot pass
    // this.
    let mut decoder = rx11_core::audio::Depacketizer::new().expect("the decoder starts");
    let mut pcm = Vec::new();
    for packet in &audio_packets {
        if let Some(bytes) = decoder.push(packet) {
            pcm.extend_from_slice(&bytes);
        }
    }
    assert!(
        pcm.len() >= 20 * PCM_FRAME_BYTES,
        "the packets decode to PCM: {} bytes",
        pcm.len()
    );

    let level = rms(&pcm);
    assert!(
        level > 2_000.0,
        "the captured sound carries the tone: rms {level:.0}"
    );
    let at_tone = tone_energy(&pcm, TONE_HZ);
    let off_tone = tone_energy(&pcm, TONE_HZ * 1.5);
    assert!(
        at_tone > off_tone * 4.0,
        "the captured sound is the played tone: {at_tone:.1} at {TONE_HZ} Hz vs {off_tone:.1} elsewhere"
    );

    server.abort();
}

/// Remove a directory when the test ends, however it ends.
///
/// The test's working directory holds the tone WAV (34 MB at 180 seconds),
/// so a run that fails on an assertion would otherwise leave it behind
/// for ever: the assertions are below the end of the test, and a panic
/// skips the cleanup that follows them.
struct ScratchDir(std::path::PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn free_port() -> u16 {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Connect to the relay and handshake, as a client that wants
/// per-application audio.
async fn connect_relay(port: u16, runtime: &RuntimeConfig) -> Rx11Transport<TcpStream> {
    use rx11_core::protocol::client_handshake;
    use rx11_core::types::Token;

    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut transport = Rx11Transport::new(stream);
    let token = Token::new(TOKEN.to_string()).expect("the token is well formed");
    let result = client_handshake(
        &mut transport,
        &token,
        None,
        Duration::from_secs(10),
        runtime,
    )
    .await
    .expect("the handshake succeeds");
    assert!(
        result.features.supports(Capability::AudioPerApp),
        "the server negotiated per-application audio: {:?}",
        result.features
    );
    transport
}
