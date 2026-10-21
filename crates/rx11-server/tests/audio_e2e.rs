//! End-to-end audio forwarding over the full production path:
//! relay server + real client + recorder/player child overrides, one
//! machine, real Opus encode/decode and the real-time priority lane.
//!
//! Ignored by default (it drives the session machinery, which shells out
//! to `xauth`); run with an isolated HOME so nothing touches the real one:
//!
//! ```sh
//! HB=$(mktemp -d); cargo test -p rx11-server --test audio_e2e -- --ignored \
//!     --test-threads=1; rm -rf "$HB"   # HOME=$HB in front if xauth complains
//! ```
//!
//! The recorder/player are deterministic stand-ins (`cat` of a prepared
//! PCM file; `sh -c "cat > out"` capture), so the assertions are byte
//! counts, not wall-clock audio.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use rx11_client::connector::LocalConnector;
use rx11_core::audio::{FRAME_SAMPLES, PCM_FRAME_BYTES, SAMPLE_RATE};
use rx11_core::config::RuntimeConfig;
use rx11_server::relay::RelayServer;

const TOKEN: &str = "audio-e2e-token";
const FRAMES: usize = 50; // 1 s of audio

fn workdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rx11-audio-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_micros()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 440 Hz sine at the wire format (s16le, stereo, 48 kHz).
fn pcm_sine(frames: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(frames * PCM_FRAME_BYTES);
    for i in 0..frames * FRAME_SAMPLES {
        let t = i as f64 / SAMPLE_RATE as f64;
        let s = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 12_000.0) as i16;
        for _ in 0..2 {
            v.extend_from_slice(&s.to_le_bytes());
        }
    }
    v
}

async fn free_port() -> u16 {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

use tokio::net::TcpListener;

struct Stack {
    x11_port: u16,
    server: JoinHandle<()>,
    client: JoinHandle<()>,
    sink: Option<JoinHandle<()>>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.server.abort();
        self.client.abort();
        if let Some(s) = &self.sink {
            s.abort();
        }
    }
}

/// Echo sink the client's local-X leg talks to.
async fn spawn_sink() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let sink = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    (addr, sink)
}

async fn start(runtime: RuntimeConfig, with_sink: bool) -> Stack {
    let relay_port = free_port().await;
    let x11_port = free_port().await;
    let (sink_addr, sink) = if with_sink {
        let (a, h) = spawn_sink().await;
        (a, Some(h))
    } else {
        ("unused".to_string(), None)
    };

    let relay_addr = format!("127.0.0.1:{relay_port}");
    let server = tokio::spawn({
        let addr = relay_addr.clone();
        let runtime = runtime.clone();
        async move {
            let server = RelayServer::with_config(addr, TOKEN.to_string(), x11_port, runtime);
            let _ = server.run().await;
        }
    });

    for _ in 0..200 {
        if TcpStream::connect(&relay_addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let client = tokio::spawn(async move {
        let client = LocalConnector::new(relay_addr, TOKEN.to_string(), sink_addr, Some(0), false)
            .with_runtime(runtime);
        let _ = client.connect_and_serve().await;
    });

    Stack {
        x11_port,
        server,
        client,
        sink,
    }
}

fn audio_runtime(dir: &std::path::Path, flood: bool) -> RuntimeConfig {
    let pcm_in = dir.join("in.pcm");
    std::fs::write(&pcm_in, pcm_sine(FRAMES)).unwrap();
    RuntimeConfig {
        audio_forward: true,
        audio_record_command: Some(vec!["cat".into(), pcm_in.to_str().unwrap().into()]),
        audio_play_command: Some(vec![
            "sh".into(),
            "-c".into(),
            format!(
                "cat > {}",
                dir.join(if flood { "out-flood.pcm" } else { "out.pcm" })
                    .display()
            ),
        ]),
        image_recode: flood,
        ..RuntimeConfig::default()
    }
}

async fn await_file(path: &std::path::Path, expect_len: usize) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() as usize == expect_len {
                // Small settle so the final write is fully flushed.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let bytes = std::fs::read(path).unwrap();
                if bytes.len() == expect_len {
                    return bytes;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "player capture never reached {expect_len} bytes at {:?}",
            path
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn rms(v: &[u8]) -> f64 {
    let mut acc = 0f64;
    for pair in v.as_chunks::<2>().0 {
        let s = i16::from_le_bytes(*pair) as f64;
        acc += s * s;
    }
    (acc / (v.len() as f64 / 2.0)).sqrt()
}

/// A "PutImage-sized" burst: high-entropy blocks above the recode/compress
/// thresholds, streaming through the bulk lane.
fn flood_traffic(frames: usize, seed: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(frames * 64 * 1024);
    for f in 0..frames {
        for i in 0usize..64 * 1024 {
            v.push((i.wrapping_mul(31) ^ (f * 257) ^ seed as usize) as u8);
        }
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "audio e2e: needs isolated HOME for xauth"]
async fn audio_flows_end_to_end_over_the_priority_lane() {
    let dir = workdir();
    let stack = start(audio_runtime(&dir, false), false).await;

    // Recorder `cat` hits EOF fast: the pump encodes all 50 frames, sends
    // them plus the ChannelClose, and the client player drains them.
    let out = await_file(&dir.join("out.pcm"), FRAMES * PCM_FRAME_BYTES).await;
    assert_eq!(out.len(), FRAMES * PCM_FRAME_BYTES);
    let energy = rms(&out);
    assert!(
        energy > 2_000.0,
        "decoded sine carries energy: rms {energy:.0}"
    );

    drop(stack);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "audio e2e: needs isolated HOME for xauth"]
async fn audio_survives_a_bulk_flood_on_the_same_link() {
    // The whole point of the priority lane: even while a 4 MiB incompressible
    // burst occupies the writer, every audio frame must arrive — no shed,
    // no reorder, no X11-side corruption.
    let dir = workdir();
    let stack = start(audio_runtime(&dir, true), true).await;

    let app = {
        let addr = format!("127.0.0.1:{}", stack.x11_port);
        let mut attempt = None;
        for _ in 0..300 {
            match TcpStream::connect(&addr).await {
                Ok(s) => {
                    attempt = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        attempt.expect("display port")
    };
    let traffic = Arc::new(flood_traffic(64, 7));
    let flood = tokio::spawn(async move {
        let (mut rd, mut wr) = app.into_split();
        let t = Arc::clone(&traffic);
        let writer = tokio::spawn(async move {
            let mut sent = 0usize;
            while sent < 4 << 20 {
                let chunk = &t[..(sent * 7 % t.len() + 1).min(t.len())];
                wr.write_all(chunk).await.unwrap();
                sent += chunk.len();
            }
            std::future::pending::<()>().await; // park writer: no early FIN
        });
        let mut got = vec![0u8; 64 * 1024];
        let mut acc = 0usize;
        while acc < 2 << 20 {
            let n = rd.read(&mut got).await.unwrap();
            assert!(n > 0, "echo stream ended early");
            acc += n;
        }
        writer.abort();
        acc
    });

    let out = await_file(&dir.join("out-flood.pcm"), FRAMES * PCM_FRAME_BYTES).await;
    assert_eq!(
        out.len(),
        FRAMES * PCM_FRAME_BYTES,
        "priority lane shed nothing under flood"
    );
    assert!(rms(&out) > 2_000.0);

    let echoed = flood.await.unwrap();
    assert!(
        echoed >= 2 << 20,
        "bulk stream kept flowing: {echoed} B echoed"
    );

    drop(stack);
    std::fs::remove_dir_all(&dir).ok();
}

/// Two clients, ONE recorder: the shared hub must spawn the capture child
/// exactly once (pid marker line count) and every late joiner's stream must
/// be an exact SUFFIX of the early joiner's — fan-out delivers the same
/// packet sequence to all lanes, and Opus packets decode statelessly in
/// `Application::Audio` (CELT) mode, so identical packets → identical PCM.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "audio e2e: needs isolated HOME for xauth"]
async fn audio_shared_capture_fans_out_to_two_clients() {
    let dir = workdir();
    let pcm_in = dir.join("in.pcm");
    std::fs::write(&pcm_in, pcm_sine(FRAMES)).unwrap();
    let pacer = dir.join("pacer.py");
    std::fs::write(
        &pacer,
        "import sys,time\n\
         d=open(sys.argv[1],'rb').read()\n\
         o=sys.stdout.buffer\n\
         for i in range(0,len(d),3840):\n\
         \x20   o.write(d[i:i+3840]); o.flush(); time.sleep(0.02)\n",
    )
    .unwrap();
    let marker = dir.join("recorder.pids");
    let relay_port = free_port().await;
    let x11_port = free_port().await;
    let relay_addr = format!("127.0.0.1:{relay_port}");

    let server_runtime = RuntimeConfig {
        audio_record_command: Some(vec![
            "sh".into(),
            "-c".into(),
            format!(
                "echo $$ >> {}; python3 {} {}",
                marker.display(),
                pacer.display(),
                pcm_in.display()
            ),
        ]),
        ..RuntimeConfig::default()
    };
    let server = tokio::spawn({
        let addr = relay_addr.clone();
        let rt = server_runtime.clone();
        async move {
            let s = RelayServer::with_config(addr, TOKEN.to_string(), x11_port, rt);
            let _ = s.run().await;
        }
    });
    for _ in 0..200 {
        if TcpStream::connect(&relay_addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let spawn_client = |out_name: &'static str| {
        let addr = relay_addr.clone();
        let rt = RuntimeConfig {
            audio_play_command: Some(vec![
                "sh".into(),
                "-c".into(),
                format!("cat > {}", dir.join(out_name).display()),
            ]),
            ..RuntimeConfig::default()
        };
        tokio::spawn(async move {
            let c = LocalConnector::new(addr, TOKEN.to_string(), "unused".into(), None, true)
                .with_runtime(rt);
            let _ = c.connect_and_serve().await;
        })
    };

    let client_a = spawn_client("out-a.pcm");
    // Wait until A's stream is demonstrably in flight, THEN attach B.
    let a_path = dir.join("out-a.pcm");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if std::fs::metadata(&a_path)
            .map(|m| m.len() >= 10 * PCM_FRAME_BYTES as u64)
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "client A never received shared audio"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let client_b = spawn_client("out-b.pcm");

    // Recorder EOF closes both outlets; A completes the full stream.
    let a_full = await_file(&a_path, FRAMES * PCM_FRAME_BYTES).await;
    let b_bytes = await_settled(&dir.join("out-b.pcm")).await;
    assert!(
        b_bytes.len() >= 10 * PCM_FRAME_BYTES && b_bytes.len() < a_full.len(),
        "B joined mid-stream: {} of {} frames",
        b_bytes.len() / PCM_FRAME_BYTES,
        FRAMES
    );
    // A late joiner's first frame may decode slightly differently (its
    // Opus decoder starts with no previous-frame energy/pitch history —
    // postfilter state), converging from the next frame on. Everything
    // after that one frame must be byte-identical to A's tail.
    // Opus decoding is a stateful stream transform: the joiner's fresh
    // decoder gives valid — but byte-different — PCM, and its very first
    // frame decodes low-energy before converging. Assert per-frame signal
    // energy from frame 2 on (B decoded real audio, streaming), not
    // cross-client byte equality.
    {
        let mut checked = 0usize;
        for (i, frame) in b_bytes
            .as_chunks::<PCM_FRAME_BYTES>()
            .0
            .iter()
            .enumerate()
            .skip(1)
        {
            let e = rms(frame);
            assert!(e > 2_000.0, "B frame {i} carries no signal (rms {e:.0})");
            checked += 1;
        }
        assert!(
            checked >= 20,
            "B must have streamed most of the tail: {checked}"
        );
    }
    let pids = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(
        pids.lines().count(),
        1,
        "exactly one recorder child for two clients: {pids}"
    );

    client_a.abort();
    client_b.abort();
    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}

/// Wait for a capture file to exist and stop growing (upper bound unknown,
/// unlike [`await_file`]).
async fn await_settled(path: &std::path::Path) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = usize::MAX;
    loop {
        let len = std::fs::metadata(path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if len > 0 && len == last {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let again = std::fs::metadata(path)
                .map(|m| m.len() as usize)
                .unwrap_or(0);
            if again == len {
                return std::fs::read(path).unwrap();
            }
            last = again;
        } else {
            last = len;
        }
        assert!(
            Instant::now() < deadline,
            "capture at {path:?} never settled (last {last} B)"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[allow(dead_code)]
fn unused_bytes_hint(_: Bytes) {}
