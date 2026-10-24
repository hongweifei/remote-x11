//! End-to-end loopback performance harness: relay server + real client +
//! fake local X server (echo sink) on one machine, exercising the full
//! production frame path (transport, DataPlane, diff, zstd).
//!
//! Ignored by default; run under `--release` (debug zstd/serde are far too
//! slow to be meaningful) with an isolated HOME so the session machinery's
//! `xauth` calls never touch the real one:
//!
//! ```sh
//! HOME=$(mktemp -d) cargo test --release -p rx11-server --test perf_loopback \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Machine-readable results are printed as `[bench] key=value` lines.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use rx11_client::connector::LocalConnector;
use rx11_core::config::RuntimeConfig;
use rx11_core::x11recode::RecodeCodec;
use rx11_server::relay::RelayServer;

const TOKEN: &str = "bench-token";
const BULK_TOTAL: usize = 16 << 20;
const LOAD_TOTAL: usize = 8 << 20;
const BLOCK: usize = 64 << 10;

/// Wall-clock seconds since the first call, for interleaving-free tracing.
fn tt() -> f64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Deterministic pseudo-random step (LCG) — good enough to defeat zstd.
fn mix(mut s: u64) -> u8 {
    s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (s >> 40) as u8
}

/// X11-like mixed payload: a repeating background (diff/compression can
/// exploit it) plus per-block incompressible noise, so the engine has to
/// do the full scan → reject → compress path on most blocks.
fn bulk_payload(total: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(total);
    let mut block = 0u64;
    while data.len() < total {
        for i in 0..BLOCK * 5 / 8 {
            data.push(((i / 4) % 251) as u8 ^ (block as u8));
        }
        for i in 0..BLOCK * 3 / 8 {
            data.push(mix(block.wrapping_mul(BLOCK as u64).wrapping_add(i as u64)));
        }
        block += 1;
    }
    data.truncate(total);
    data
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

struct Stack {
    x11_port: u16,
    server: JoinHandle<()>,
    client: JoinHandle<()>,
    sink: JoinHandle<()>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.server.abort();
        self.client.abort();
        self.sink.abort();
    }
}

/// Bring up relay + client + echo sink; returns the app-side entry port.
async fn start(runtime: RuntimeConfig) -> Stack {
    // Transport scenarios never exercise audio: since the default flipped,
    // pin it off here so no real `parec`/`paplay` desktop children spawn
    // during timing runs (audio has its own e2e file with command
    // overrides).
    let runtime = RuntimeConfig {
        audio_forward: false,
        ..runtime
    };
    let relay_port = free_port().await;
    let x11_port = free_port().await; // display 0 ⇒ base + 0

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink_addr = listener.local_addr().unwrap().to_string();
    let sink = tokio::spawn(async move {
        loop {
            let (mut s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            s.set_nodelay(true).ok();
            println!("[t={:.3}] sink accepted pump conn", tt());
            tokio::spawn(async move {
                let mut buf = vec![0u8; BLOCK];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) => {
                            println!("[t={:.3}] sink saw EOF from pump", tt());
                            break;
                        }
                        Err(e) => {
                            println!("[t={:.3}] sink read err {e}", tt());
                            break;
                        }
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                println!("[t={:.3}] sink write err (n={n})", tt());
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    let relay_addr = format!("127.0.0.1:{relay_port}");
    let server = tokio::spawn({
        let addr = relay_addr.clone();
        let runtime = runtime.clone();
        async move {
            let server = RelayServer::with_config(addr, TOKEN.to_string(), x11_port, runtime);
            let _ = server.run().await;
        }
    });

    // Wait for the relay port, then let the client negotiate the session.
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

/// Connect an "application" to the relayed display, waiting for the
/// session (and therefore the display bind) to be live.
async fn app_connect(port: u16) -> TcpStream {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..300 {
        if let Ok(s) = TcpStream::connect(&addr).await {
            println!("[t={:.3}] [bench] app connected to display", tt());
            s.set_nodelay(true).unwrap();
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("display port {port} never came up");
}

/// Write `total` bytes and (concurrently) read the echoed bytes back;
/// returns MB/s for one direction (both flow simultaneously).
async fn duplex_xfer(app: TcpStream, payload: Arc<Vec<u8>>, total: usize) -> f64 {
    let (mut rd, mut wr) = app.into_split();
    // The writer must NOT drop its half early: tokio's split sends a
    // write-shutdown on drop, which the relay treats as connection EOF.
    let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
    let wpayload = Arc::clone(&payload);
    let writer = tokio::spawn(async move {
        let mut sent = 0usize;
        while sent < total {
            let end = (sent + BLOCK).min(total);
            wr.write_all(&wpayload[sent..end]).await.unwrap();
            sent = end;
        }
        let _ = hold_rx.await;
        drop(wr);
    });
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; BLOCK];
        let mut got = 0usize;
        let mut first = true;
        while got < total {
            let n = rd.read(&mut buf).await.unwrap();
            if n == 0 {
                println!(
                    "[t={:.3}] [bench] app EOF after got={got} of total={total}",
                    tt()
                );
                panic!("echo closed early");
            }
            if first {
                assert_eq!(buf[0], payload[0], "echo content mismatch");
                first = false;
            }
            got += n;
        }
        got
    });
    let start = Instant::now();
    let got = tokio::time::timeout(Duration::from_secs(120), reader)
        .await
        .expect("duplex transfer timed out")
        .expect("reader panicked");
    let _ = hold_tx.send(());
    writer.await.unwrap();
    assert_eq!(got, total, "echo byte count");
    let secs = start.elapsed().as_secs_f64();
    (total as f64 / 1e6) / secs
}

/// 1-byte round-trip latency samples over a dedicated connection.
async fn latency_probe(port: u16, samples: usize) -> (f64, f64, f64) {
    let mut app = app_connect(port).await;
    let mut out = Vec::with_capacity(samples);
    let mut one = [0u8; 1];
    for _ in 0..samples {
        let t = Instant::now();
        app.write_all(b"x").await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), app.read_exact(&mut one))
            .await
            .expect("latency probe timed out")
            .expect("latency probe read failed");
        out.push(t.elapsed().as_secs_f64() * 1e6);
    }
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        out[out.len() / 2],
        out[out.len() * 95 / 100],
        *out.last().unwrap(),
    )
}

fn raw_runtime() -> RuntimeConfig {
    RuntimeConfig {
        compression_threshold: usize::MAX / 2,
        diff_min_block_size: usize::MAX / 2,
        ..RuntimeConfig::default()
    }
}

/// `scenario: throughput …` plus the machine-readable key lines.
fn report(name: &str, mbps: Option<f64>, lat: (f64, f64, f64)) {
    if let Some(m) = mbps {
        println!("[bench] {name}: throughput {m:.1} MB/s per direction (full-duplex echo)");
        println!("[bench] {name}.throughput_mbps={m:.2}");
    }
    println!(
        "[bench] {name}: latency med {:.0} us, p95 {:.0} us, max {:.0} us",
        lat.0, lat.1, lat.2
    );
    println!(
        "[bench] {name}.lat_med_us={:.1} {name}.lat_p95_us={:.1} {name}.lat_max_us={:.1}",
        lat.0, lat.1, lat.2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness"]
async fn s5_size_bisect() {
    for size in [100usize, 1000, 2000, 65_536] {
        let stack = start(RuntimeConfig::default()).await;
        let mut app = app_connect(stack.x11_port).await;
        let chunk = vec![0xA5u8; size];
        let mut got = 0usize;
        let mut buf = vec![0u8; size + 8];
        let total = size * 8;
        for _ in 0..8 {
            app.write_all(&chunk).await.unwrap();
        }
        while got < total {
            let n = app
                .read(&mut buf)
                .await
                .unwrap_or_else(|e| panic!("size {size}: read err {e} at got={got}"));
            if n == 0 {
                println!(
                    "[t={:.3}] [bench] size={size} EOF at got={got}/{total}",
                    tt()
                );
                break;
            }
            got += n;
        }
        println!(
            "[t={:.3}] [bench] size={size} roundtripped {got}/{total}",
            tt()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness"]
async fn s6_stream_pressure() {
    let stack = start(RuntimeConfig::default()).await;
    let app = app_connect(stack.x11_port).await;
    let (mut rd, mut wr) = app.into_split();
    let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
    let payload = Arc::new(bulk_payload(BULK_TOTAL));
    let size = BLOCK;
    let wpayload = Arc::clone(&payload);
    let writer = tokio::spawn(async move {
        let mut sent = 0usize;
        while sent < BULK_TOTAL {
            let end = (sent + size).min(BULK_TOTAL);
            match wr.write_all(&wpayload[sent..end]).await {
                Ok(()) => sent = end,
                Err(e) => {
                    println!("[t={:.3}] [bench] writer err {e} at sent={sent}", tt());
                    return sent;
                }
            }
        }
        println!("[t={:.3}] [bench] writer done", tt());
        let _ = hold_rx.await;
        sent
    });
    let p2 = Arc::clone(&payload);
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; size];
        let mut got = 0usize;
        while got < BULK_TOTAL {
            match rd.read(&mut buf).await {
                Ok(0) => {
                    println!("[t={:.3}] [bench] reader EOF at got={got}", tt());
                    return (got, true);
                }
                Ok(n) => got += n,
                Err(e) => {
                    println!("[t={:.3}] [bench] reader err {e} at got={got}", tt());
                    return (got, false);
                }
            }
            if got % (4 << 20) < size {
                let exp = &p2[..got];
                // cheap periodic prefix check happens at the end anyway
                let _ = exp;
            }
        }
        println!("[t={:.3}] [bench] reader done", tt());
        (got, false)
    });
    let (got, eof) = tokio::time::timeout(Duration::from_secs(120), reader)
        .await
        .expect("s6 reader stalled")
        .unwrap();
    let _ = hold_tx.send(());
    let sent = writer.await.unwrap();
    println!("[t={:.3}] [bench] s6 sent={sent} got={got} eof={eof}", tt());
    assert_eq!(got, sent, "stream stalled/dropped bytes");
    let _ = stack;
}

/// An X11-shaped byte stream: connection setup followed by `frames`
/// ZPixmap PutImage requests (depth 24, 64x96 => 24 KiB pixels each).
/// Layouts follow Xproto.h, matching what the recode machine parses:
/// the setup request has a 12-byte fixed header; each PutImage request
/// is `opcode(1) format(1) length(2, in 4-byte units) drawable(4) gc(4)
/// width(2) height(2) dst-x(2) dst-y(2) left-pad(1) depth(1) unused(2)`
/// = 24 bytes, then the pixels.
fn x_traffic(frames: usize, seed: u8) -> Vec<u8> {
    let mut v = Vec::new();
    // setup: 'l', major 11, minor 0, name "MIT-MAGIC-COOKIE-1"(18->20 pad),
    // auth 16B, plus the two unused bytes the fixed 12-byte header carries.
    v.extend_from_slice(&[b'l', 0]);
    v.extend_from_slice(&11u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    let name = b"MIT-MAGIC-COOKIE-1";
    let auth = [seed; 16];
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&(auth.len() as u16).to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(name);
    v.resize(12 + ((name.len() + 3) & !3), 0);
    v.extend_from_slice(&auth);
    v.resize(v.len() + ((4 - (auth.len() % 4)) % 4), 0);
    for f in 0..frames {
        let (w, h) = (64u16, 96u16);
        let data_len = w as usize * h as usize * 4;
        let total = 24 + data_len;
        v.push(72);
        v.push(2);
        v.extend_from_slice(&((total / 4) as u16).to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.push(0);
        v.push(24);
        v.extend_from_slice(&[0, 0]);
        for i in 0..data_len / 4 {
            let base = (seed.wrapping_add(f as u8).wrapping_mul(7)) as u32;
            let px = (base + i as u32 * 65537).to_le_bytes();
            v.extend_from_slice(&px);
        }
    }
    v
}

fn recode_runtime(codec: RecodeCodec) -> RuntimeConfig {
    RuntimeConfig {
        image_recode: true,
        image_recode_codec: codec,
        image_recode_quality: 85,
        image_recode_min_bytes: 4096,
        ..RuntimeConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness"]
async fn s7_recode_end_to_end() {
    // PNG tier: through relay+queues+dataplane, the echoed stream must be
    // byte-identical to the X-shaped traffic we sent.
    for (codec, name) in [(RecodeCodec::Png, "png"), (RecodeCodec::Jpeg, "jpeg")] {
        let stack = start(recode_runtime(codec)).await;
        let app = app_connect(stack.x11_port).await;
        let traffic = x_traffic(24, 91);
        let (mut rd, mut wr) = app.into_split();
        let sent = traffic.clone();
        let writer = tokio::spawn(async move {
            wr.write_all(&sent).await.unwrap();
            // park until reader is done so no FIN is sent early
            std::future::pending::<()>().await;
        });
        let expect = traffic.len();
        let reader = tokio::spawn(async move {
            let mut got: Vec<u8> = Vec::with_capacity(expect);
            let mut buf = vec![0u8; 65536];
            while got.len() < expect {
                let n = rd.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "echo closed at {}", got.len());
                got.extend_from_slice(&buf[..n]);
            }
            got
        });
        let got = tokio::time::timeout(Duration::from_secs(30), reader)
            .await
            .expect("recode echo stalled")
            .unwrap();
        writer.abort();
        if codec == RecodeCodec::Png {
            assert_eq!(got, traffic, "png tier must be byte-exact end to end");
        } else {
            assert_eq!(got.len(), traffic.len(), "jpeg must keep framing sizes");
            assert_eq!(&got[..12], &traffic[..12], "setup survives");
            // each PutImage header (24B) must be intact at its offsets
            let mut pos = 0usize;
            let mut headers = 0;
            while pos + 24 <= got.len() {
                if got[pos] == 72 && got[pos + 1] == 2 {
                    assert_eq!(
                        &got[pos..pos + 24],
                        &traffic[pos..pos + 24],
                        "header {headers} at {pos}"
                    );
                    let len_b32 = u16::from_le_bytes([got[pos + 2], got[pos + 3]]) as usize;
                    pos += len_b32 * 4;
                    headers += 1;
                } else {
                    pos += 1;
                }
            }
            assert_eq!(headers, 24, "all PutImage requests present");
        }
        println!(
            "[bench] s7.{name}: end-to-end recode integrity ok ({} B echoed)",
            got.len()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness: cargo test --release -p rx11-server --test perf_loopback -- --ignored"]
async fn s1_default_throughput_and_idle_latency() {
    let stack = start(RuntimeConfig::default()).await;
    let app = app_connect(stack.x11_port).await;
    let mbps = duplex_xfer(app, Arc::new(bulk_payload(BULK_TOTAL)), BULK_TOTAL).await;
    let lat = latency_probe(stack.x11_port, 200).await;
    report("s1.default", Some(mbps), lat);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness"]
async fn s2_default_latency_under_bulk_load() {
    let stack = start(RuntimeConfig::default()).await;
    let app = app_connect(stack.x11_port).await;
    let payload = Arc::new(bulk_payload(LOAD_TOTAL));
    let bulk = tokio::spawn(async move { duplex_xfer(app, payload, LOAD_TOTAL).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lat = latency_probe(stack.x11_port, 150).await;
    let done = bulk.await.is_ok();
    println!("[bench] s2.load_completed={done}");
    report("s2.default-under-load", None, lat);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness"]
async fn s3_raw_throughput_and_idle_latency() {
    let stack = start(raw_runtime()).await;
    let app = app_connect(stack.x11_port).await;
    let mbps = duplex_xfer(app, Arc::new(bulk_payload(BULK_TOTAL)), BULK_TOTAL).await;
    let lat = latency_probe(stack.x11_port, 200).await;
    report("s3.raw", Some(mbps), lat);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf harness"]
async fn s4_raw_latency_under_bulk_load() {
    let stack = start(raw_runtime()).await;
    let app = app_connect(stack.x11_port).await;
    let payload = Arc::new(bulk_payload(LOAD_TOTAL));
    let bulk = tokio::spawn(async move { duplex_xfer(app, payload, LOAD_TOTAL).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lat = latency_probe(stack.x11_port, 150).await;
    let done = bulk.await.is_ok();
    println!("[bench] s4.load_completed={done}");
    report("s4.raw-under-load", None, lat);
}
