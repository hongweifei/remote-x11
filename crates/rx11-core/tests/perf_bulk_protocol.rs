//! Bulk self-heal probe at pure protocol level (ignored; --release).
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};

use rx11_core::config::RuntimeConfig;
use rx11_core::dataplane::{DataPlane, DataPlaneSide, InboundOutcome};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{Capability, Features, Frame};
use rx11_core::transport::Rx11Transport;
use rx11_core::types::ConnectionId;

fn mix(mut s: u64) -> u8 {
    s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (s >> 40) as u8
}

fn bulk_payload(total: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(total);
    let mut block = 0u64;
    const BLOCK: usize = 64 << 10;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf probe"]
async fn bulk_over_tcp_dataplane_only() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let rt = Arc::new(RuntimeConfig::default());
    let features = Features::from_list(&Capability::advertised());
    let total = 16usize << 20;
    let payload = Arc::new(bulk_payload(total));

    let sender = tokio::spawn({
        let rt = Arc::clone(&rt);
        let features = features.clone();
        let payload = Arc::clone(&payload);
        async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let mut tx = Rx11Transport::connect_tcp(stream, None).unwrap();
            let dp = DataPlane::new(
                DataPlaneSide::Relay,
                features,
                Some(rx11_core::compress::CompressionAlgo::ZstdStream),
                &rt,
            );
            let mut cache = ConnectionDataCache::from_runtime(&rt);
            let stats = Arc::new(rx11_core::stats::ConnectionStats::new());
            let mut seq = 1u32;
            let mut sent = 0usize;
            while sent < total {
                let end = (sent + (64 << 10)).min(total);
                let data = Bytes::copy_from_slice(&payload[sent..end]);
                let frame = dp.build_frame(
                    ConnectionId::new(1),
                    seq,
                    data,
                    Some(&mut cache),
                    Some(&stats),
                );
                tx.send_frame(&frame).await.unwrap();
                seq += 1;
                sent = end;
            }
        }
    });

    let (peer, _) = listener.accept().await.unwrap();
    let mut rx = Rx11Transport::connect_tcp(peer, None).unwrap();
    let rt = Arc::clone(&rt);
    let dp = DataPlane::new(
        DataPlaneSide::Client,
        features.clone(),
        Some(rx11_core::compress::CompressionAlgo::ZstdStream),
        &rt,
    );
    let mut cache = ConnectionDataCache::from_runtime(&rt);
    let stats = rx11_core::stats::ConnectionStats::new();
    let mut out: Vec<u8> = Vec::with_capacity(total);
    let mut requests = 0usize;

    let recv_task = async {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(30), rx.recv_frame())
                .await
                .expect("recv stalled")
                .expect("recv error");
            match frame {
                f @ (Frame::StreamData(_)
                | Frame::CompressedStreamData(_)
                | Frame::IncrementalStreamData(_)
                | Frame::CompressedIncrementalStreamData(_)) => {
                    match dp.apply_inbound(f, &mut cache, &stats) {
                        InboundOutcome::Payload { data, .. } => out.extend_from_slice(&data),
                        InboundOutcome::ResyncRequired { .. } => requests += 1,
                        InboundOutcome::Dropped => requests += 1000,
                        InboundOutcome::StreamBroken => panic!("compression stream broke"),
                    }
                }
                other => panic!("unexpected frame {:?}", other.msg_type()),
            }
            if out.len() >= total {
                break;
            }
        }
    };
    // No select needed: sender only blocks on the kernel buffer, which the
    // receiver drains — there is no cyclic wait.
    recv_task.await;
    sender.await.unwrap();
    assert_eq!(out.len(), total, "bytes delivered (resyncs={requests})");
    assert_eq!(out, payload[..], "content corrupted, resyncs={requests}");
    println!("[probe] ok resyncs={requests} savings: {}", stats.summary());
}
