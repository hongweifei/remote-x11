//! Flow-control end-to-end loopback: relay server + real client + a
//! controllable fake local X server, asserting the pause/resume semantics
//! over the production frame path:
//!
//! - a stalled consumer actually freezes the producing pump (observed via
//!   `ConnectionStats` flow counters and a stuck writer), instead of the
//!   bytes silently parking wherever the transport likes;
//! - pausing loses nothing: after resume every byte has arrived, in order;
//! - the session survives the freeze (heartbeats keep flowing).
//!
//! Ignored by default (like the perf suite) because the session machinery
//! shells out to `xauth`; run with an isolated HOME:
//!
//! ```sh
//! HOME=$(mktemp -d) cargo test --release -p rx11-server --test flow_e2e \
//!     -- --ignored --test-threads=1
//! ```

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use rx11_client::connector::LocalConnector;
use rx11_core::config::RuntimeConfig;
use rx11_core::stats::ConnectionStats;
use rx11_server::relay::RelayServer;

const TOKEN: &str = "flow-token";
const TOTAL: usize = 24 << 20; // 24 MiB: beyond any honest buffer chain
const BLOCK: usize = 64 << 10;

fn pat(i: usize) -> u8 {
    let mut s = (i as u64).wrapping_mul(6364136223846793005).wrapping_add(7);
    s = s.wrapping_mul(0x9E3779B97F4A7C15);
    (s >> 47) as u8
}

fn pattern(total: usize) -> Vec<u8> {
    (0..total).map(pat).collect()
}

/// Flow-control-tuned runtime: raw frames (no compression/diff noise), no
/// recode, no audio, and watermarks small enough that a 24 MiB flood trips
/// the gauge within the first handful of chunks.
fn flow_runtime() -> RuntimeConfig {
    RuntimeConfig {
        compression_threshold: usize::MAX / 2,
        diff_min_block_size: usize::MAX / 2,
        image_recode: false,
        audio_forward: false,
        flow_control: true,
        flow_pause_bytes: 256 * 1024,
        flow_resume_bytes: 64 * 1024,
        ..RuntimeConfig::default()
    }
}

async fn free_port() -> u16 {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

/// Shared control surface of the fake local X server.
#[derive(Default)]
struct SinkCtl {
    /// When set, the sink stops reading its socket (a busy X server that
    /// cannot drain requests).
    stall_reads: AtomicBool,
    received: Mutex<Vec<u8>>,
    received_len: AtomicUsize,
    /// Bytes the sink has written into its socket (uplink production).
    wrote_len: AtomicUsize,
}

struct Stack {
    x11_port: u16,
    stats: Arc<ConnectionStats>,
    sink: Arc<SinkCtl>,
    server: JoinHandle<()>,
    client: JoinHandle<()>,
    sink_task: JoinHandle<()>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.server.abort();
        self.client.abort();
        self.sink_task.abort();
    }
}

/// Bring up relay + client + controllable sink with a chosen runtime; the
/// sink writes `sink_writes` bytes of pattern toward the application on
/// connect.
async fn start_with(sink_writes: usize, runtime: RuntimeConfig) -> Stack {
    let relay_port = free_port().await;
    let x11_port = free_port().await; // display 0 ⇒ base + 0

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink_addr = listener.local_addr().unwrap().to_string();
    let sink = Arc::new(SinkCtl::default());

    let sink_task = {
        let sink = Arc::clone(&sink);
        tokio::spawn(async move {
            loop {
                let (s, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                s.set_nodelay(true).ok();
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let (mut rd, mut wr) = s.into_split();
                    let sink_reads = Arc::clone(&sink);
                    let reader = tokio::spawn(async move {
                        let mut buf = vec![0u8; BLOCK];
                        loop {
                            // The stall knob: a busy X server stops
                            // draining its client socket entirely.
                            while sink_reads.stall_reads.load(Ordering::Acquire) {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                            match rd.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    let mut got = sink_reads.received.lock().unwrap();
                                    got.extend_from_slice(&buf[..n]);
                                    sink_reads.received_len.store(got.len(), Ordering::Release);
                                }
                                Err(_) => break,
                            }
                        }
                    });
                    let mut sent = 0usize;
                    let mut buf = vec![0u8; BLOCK];
                    while sent < sink_writes {
                        let end = (sent + BLOCK).min(sink_writes);
                        for (i, b) in buf[..end - sent].iter_mut().enumerate() {
                            *b = pat(sent + i);
                        }
                        // write_all blocks naturally when the peer chain
                        // (client local_read) is gated — that stall IS the
                        // observable uplink backpressure.
                        if wr.write_all(&buf[..end - sent]).await.is_err() {
                            break;
                        }
                        sent = end;
                        sink.wrote_len.store(sent, Ordering::Release);
                    }
                    // The reader is NOT aborted on a normal (or zero-byte)
                    // write completion: a sink that writes nothing must
                    // still drain the downlink until the app closes, and a
                    // sink that wrote everything must hold the socket open
                    // — dropping it now would half-close and RST away
                    // bytes the chain has accepted but not yet delivered.
                    // Only a failed write (peer gone) retires it early.
                    if sent < sink_writes {
                        reader.abort();
                    } else {
                        let _ = reader.await;
                    }
                });
            }
        })
    };

    let relay_addr = format!("127.0.0.1:{relay_port}");
    let server_obj = RelayServer::with_config(
        relay_addr.clone(),
        TOKEN.to_string(),
        x11_port,
        runtime.clone(),
    );
    let stats = server_obj.stats();
    let server = tokio::spawn(async move {
        let _ = server_obj.run().await;
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
        stats,
        sink,
        server,
        client,
        sink_task,
    }
}

async fn start(sink_writes: usize) -> Stack {
    start_with(sink_writes, flow_runtime()).await
}

async fn app_connect(port: u16) -> TcpStream {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..300 {
        if let Ok(s) = TcpStream::connect(&addr).await {
            s.set_nodelay(true).unwrap();
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("display port {port} never came up");
}

async fn wait_until(what: &str, deadline: Duration, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < deadline,
            "condition never held within {deadline:?}: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn flow_totals(stats: &Arc<ConnectionStats>) -> (u64, u64) {
    stats.flow_totals()
}

/// Downlink: the fake X server stops draining; the client's gauge crosses
/// the pause watermark, the relay enforces the `Pause` (counter), the
/// application writer freezes mid-stream, and nothing is lost on resume.
#[tokio::test]
#[ignore]
async fn stalled_local_x_pauses_the_app_pump_and_resumes_losslessly() {
    let stack = start(0).await;
    let mut app = app_connect(stack.x11_port).await;

    // Stall the sink BEFORE any flood: the very first chunks will back the
    // chain up and trigger the advertisement.
    stack.sink.stall_reads.store(true, Ordering::Release);

    let payload = Arc::new(pattern(TOTAL));
    let written = Arc::new(AtomicUsize::new(0));
    let writer = {
        let payload = Arc::clone(&payload);
        let written = Arc::clone(&written);
        let sink = Arc::clone(&stack.sink);
        tokio::spawn(async move {
            let mut w = 0usize;
            while w < TOTAL {
                let end = (w + BLOCK).min(TOTAL);
                if app.write_all(&payload[w..end]).await.is_err() {
                    break;
                }
                w = end;
                written.store(w, Ordering::Release);
            }
            // Hold the socket until the sink has drained everything: an
            // early FIN would tear the channel down before the last
            // bytes land.
            while sink.received_len.load(Ordering::Acquire) < TOTAL {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(app);
        })
    };

    // 1. Flow control engaged for real: the relay advertised its gauge at
    //    least once… (server counts only pauses WE advertise; enforcement
    //    of the client's pause is the counter that matters here)
    wait_until(
        "downlink pause enforced at the relay",
        Duration::from_secs(15),
        || flow_totals(&stack.stats).1 >= 1,
    )
    .await;

    // 2. The application writer is genuinely stuck: progress is capped by
    //    the finite buffer chain, not by the sink draining, and it stays
    //    stuck while the pause holds.
    let stuck_at = written.load(Ordering::Acquire);
    assert!(
        stuck_at < TOTAL,
        "writer completed {stuck_at} bytes despite flow control"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        written.load(Ordering::Acquire),
        stuck_at,
        "writer made progress while the pump was gated"
    );
    let sink_held = stack.sink.received_len.load(Ordering::Acquire);

    // 3. Resume: every byte arrives, in order, and the writer finishes.
    stack.sink.stall_reads.store(false, Ordering::Release);
    wait_until(
        "sink received the full flood",
        Duration::from_secs(30),
        || stack.sink.received_len.load(Ordering::Acquire) >= TOTAL,
    )
    .await;
    writer.await.unwrap();
    let got = stack.sink.received.lock().unwrap().clone();
    assert!(got.len() >= sink_held, "received count is monotone");
    assert_eq!(got.len(), TOTAL, "no byte lost, none added");
    assert_eq!(
        got, *payload,
        "byte-exact order preserved across pause/resume"
    );
}

/// Uplink: the application stops reading replies; the relay's app-socket
/// gauge crosses, the `Pause` rides the priority lane, the client gates its
/// local read, and the sink's writes freeze — then everything drains.
#[tokio::test]
#[ignore]
async fn stalled_app_pauses_the_uplink_and_resumes_losslessly() {
    let stack = start(TOTAL).await;
    let app = app_connect(stack.x11_port).await;

    // The application connects but reads nothing: replies pile up, the
    // relay's per-channel uplink gauge crosses its watermark.
    let (mut rd, _wr) = app.into_split();
    let read_gate = Arc::new(AtomicBool::new(false));
    let reader = {
        let gate = Arc::clone(&read_gate);
        tokio::spawn(async move {
            while !gate.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut got = Vec::with_capacity(TOTAL);
            let mut buf = vec![0u8; BLOCK];
            while got.len() < TOTAL {
                let n = rd.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            got
        })
    };

    // 1. The relay advertised an uplink pause (counter, then priority lane).
    wait_until("uplink pause advertised", Duration::from_secs(15), || {
        flow_totals(&stack.stats).0 >= 1
    })
    .await;

    // 2. The sink's write progress froze below total: the client stopped
    //    reading its socket, so backpressure reached the producer.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stuck_at = stack.sink.wrote_len.load(Ordering::Acquire);
    assert!(
        stuck_at < TOTAL,
        "sink wrote all {TOTAL} bytes despite flow control"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        stack.sink.wrote_len.load(Ordering::Acquire),
        stuck_at,
        "sink kept writing while the client was gated"
    );

    // 3. Resume: the app reads everything, byte-exact.
    read_gate.store(true, Ordering::Release);
    let got = tokio::time::timeout(Duration::from_secs(30), reader)
        .await
        .expect("reader never finished after resume")
        .expect("reader panicked");
    assert_eq!(got.len(), TOTAL, "every uplink byte survived the freeze");
    assert_eq!(got, pattern(TOTAL), "byte-exact order preserved");
}

/// Kill switch: with `flow_control = false` on the server the capability
/// is never advertised, negotiation drops it, and a stalled consumer
/// produces zero flow frames — the session still survives on plain
/// transport backpressure, losslessly.
#[tokio::test]
#[ignore]
async fn kill_switch_falls_back_to_transport_backpressure() {
    let runtime = RuntimeConfig {
        flow_control: false,
        ..flow_runtime()
    };
    let stack = start_with(0, runtime).await;
    let mut app = app_connect(stack.x11_port).await;
    stack.sink.stall_reads.store(true, Ordering::Release);

    // Push enough to have tripped the gauges if they existed — from a
    // background writer, because with flow control OFF the stalled sink
    // legitimately freezes this writer on transport backpressure alone.
    const HALF: usize = 4 << 20;
    let payload = Arc::new(pattern(HALF));
    let sink = Arc::clone(&stack.sink);
    let writer = {
        let payload = Arc::clone(&payload);
        tokio::spawn(async move {
            let mut w = 0usize;
            while w < HALF {
                let end = (w + BLOCK).min(HALF);
                if app.write_all(&payload[w..end]).await.is_err() {
                    break;
                }
                w = end;
            }
            while sink.received_len.load(Ordering::Acquire) < HALF {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(app);
        })
    };

    tokio::time::sleep(Duration::from_millis(500)).await;
    let (adv, enf) = flow_totals(&stack.stats);
    assert_eq!((adv, enf), (0, 0), "ungated session must stay silent");

    // Unstall: transport backpressure alone kept the stream byte-exact.
    stack.sink.stall_reads.store(false, Ordering::Release);
    wait_until("sink drains after unstall", Duration::from_secs(15), || {
        stack.sink.received_len.load(Ordering::Acquire) >= HALF
    })
    .await;
    writer.await.unwrap();
    let got = stack.sink.received.lock().unwrap().clone();
    assert_eq!(got, *payload, "lossless without flow control, too");
    // And still nothing was ever advertised/enforced after the storm.
    assert_eq!(flow_totals(&stack.stats), (0, 0));
}
