//! End-to-end protocol tests over an in-memory duplex transport: real
//! framing, handshake, capability negotiation and the data-plane chain
//! including the incremental self-heal round trip.
//!
//! These exercise the public core surface exactly the way rx11-server and
//! rx11-client do, without sockets, xauth or processes.

use std::time::Duration;

use bytes::Bytes;
use tokio::io::{duplex, DuplexStream};

use rx11_core::auth::generate_token;
use rx11_core::compress::CompressionAlgo;
use rx11_core::config::RuntimeConfig;
use rx11_core::dataplane::{DataPlane, DataPlaneSide, InboundOutcome};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{
    client_handshake, decode_frame, encode_frame, server_handshake, Capability, ConnectionMode,
    Features, Frame, HelloMessage, StreamResyncRequestMessage,
};
use rx11_core::stats::ConnectionStats;
use rx11_core::transport::{Rx11Transport, Rx11TransportReadHalf, Rx11TransportWriteHalf};
use rx11_core::types::{ConnectionId, SessionId, Token};

type ReadHalf = Rx11TransportReadHalf<DuplexStream>;
type WriteHalf = Rx11TransportWriteHalf<DuplexStream>;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

fn all_features() -> Features {
    Features::from_list(&Capability::advertised())
}

fn test_dataplane(side: DataPlaneSide) -> DataPlane {
    DataPlane::new(side, all_features(), None, &RuntimeConfig::default())
}

/// Same, with the algorithm the default handshake negotiates — for tests
/// where compression stacks on top of the diff frames.
fn test_dataplane_zstd(side: DataPlaneSide) -> DataPlane {
    DataPlane::new(
        side,
        all_features(),
        Some(CompressionAlgo::ZstdStream),
        &RuntimeConfig::default(),
    )
}

struct Session {
    relay_tx: WriteHalf,
    relay_rx: ReadHalf,
    client_tx: WriteHalf,
    client_rx: ReadHalf,
}

/// Complete a real handshake over a duplex pair, then hand back the two
/// endpoints as framed read/write halves: `relay_*` play the server side of
/// the data plane, `client_rx` reads whatever the relay emits.
async fn negotiated_session() -> (Session, Features, Option<CompressionAlgo>) {
    let (server_side, client_side) = duplex(8192);
    let rt = RuntimeConfig::default();
    let mut server_transport = Rx11Transport::new(server_side);
    let mut client_transport = Rx11Transport::new(client_side);

    let token = Token::new(generate_token()).unwrap();
    let (server_hs, client_hs) = tokio::join!(
        server_handshake(
            &mut server_transport,
            token.as_str(),
            HANDSHAKE_TIMEOUT,
            &rt,
        ),
        client_handshake(&mut client_transport, &token, None, HANDSHAKE_TIMEOUT, &rt,)
    );

    let server_hs = server_hs.expect("server handshake");
    let client_hs = client_hs.expect("client handshake");
    assert_eq!(server_hs.session_id, client_hs.session_id);
    assert_eq!(server_hs.features, client_hs.features);
    assert_eq!(server_hs.compression, client_hs.compression);

    let (relay_rx, relay_tx) = server_transport.split();
    let (client_rx, client_tx) = client_transport.split();
    (
        Session {
            relay_tx,
            relay_rx,
            client_tx,
            client_rx,
        },
        client_hs.features,
        client_hs.compression,
    )
}

#[tokio::test]
async fn handshake_negotiates_all_capabilities_and_compression() {
    let (_session, features, compression) = negotiated_session().await;
    assert!(features.supports(Capability::Incremental));
    assert!(features.supports(Capability::RequestResync));
    assert!(features.supports(Capability::X11));
    // Defaults are on: recode (lossless PNG tier) and audio negotiate with
    // the stock RuntimeConfig — the kill switches are `= false`, not opt-in.
    assert!(features.supports(Capability::X11Recode));
    assert!(features.supports(Capability::AudioForward));
    // Flow control went from reserved to real: default-on as well.
    assert!(features.supports(Capability::FlowControl));
    // The best mutual algorithm wins — cross-frame streaming zstd since the
    // WAN round (it negotiates ahead of one-shot `zstd`).
    assert_eq!(compression, Some(CompressionAlgo::ZstdStream));
}

#[tokio::test]
async fn handshake_fails_fast_on_wrong_token() {
    let (server_side, client_side) = duplex(4096);
    let rt = RuntimeConfig::default();
    let mut server_transport = Rx11Transport::new(server_side);
    let mut client_transport = Rx11Transport::new(client_side);

    let server_token = Token::new(generate_token()).unwrap();
    let attacker = Token::new("definitely-not-the-token".to_string()).unwrap();

    let (server_result, client_result) = tokio::join!(
        server_handshake(
            &mut server_transport,
            server_token.as_str(),
            HANDSHAKE_TIMEOUT,
            &rt,
        ),
        client_handshake(
            &mut client_transport,
            &attacker,
            None,
            HANDSHAKE_TIMEOUT,
            &rt,
        )
    );

    let server_err = match server_result {
        Ok(_) => panic!("server must reject a bad token"),
        Err(e) => e,
    };
    assert!(server_err.to_string().contains("Token"), "{server_err}");
    match client_result {
        Ok(_) => panic!("client must observe auth failure"),
        Err(rx11_core::error::Rx11Error::Auth(_)) => {}
        Err(other) => panic!("unexpected client error: {other}"),
    }
}

#[tokio::test]
async fn handshake_rejects_other_protocol_version() {
    let rt = RuntimeConfig::default();
    let (server_side, mut client_side) = duplex(4096);
    let mut server_transport = Rx11Transport::new(server_side);

    let server = tokio::spawn(async move {
        server_handshake(&mut server_transport, "t", HANDSHAKE_TIMEOUT, &rt).await
    });

    // Same wire version, foreign handshake version: rejected by the Hello
    // comparison, with a proper HelloAck error sent back first.
    let hello = Frame::Hello(HelloMessage {
        version: 99,
        mode: ConnectionMode::Client,
        resume_session_id: None,
        compression_algos: vec![],
        capabilities: vec![],
    });
    client_side
        .write_all(&encode_frame(&hello).unwrap())
        .await
        .unwrap();

    let err = server.await.unwrap().expect_err("must reject");
    assert!(err.to_string().contains("Version mismatch"), "{err}");

    // And the client side receives the failure HelloAck before giving up.
    let mut acc: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 256];
    let frame = loop {
        let n = client_side.read(&mut tmp).await.unwrap();
        acc.extend_from_slice(&tmp[..n]);
        if let Some((frame, _consumed)) = decode_frame(&acc).unwrap() {
            break frame;
        }
    };
    match frame {
        Frame::HelloAck(ack) => {
            assert!(!ack.success);
            assert!(ack.error_msg.unwrap().contains("Version mismatch"));
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

#[tokio::test]
async fn wire_version_octet_is_enforced_by_the_codec() {
    let mut bytes = encode_frame(&Frame::Heartbeat).unwrap().to_vec();
    bytes[4] = 9; // header version octet
    let err = decode_frame(&bytes).unwrap_err();
    assert!(
        err.to_string().contains("Unsupported protocol version"),
        "{err}"
    );
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn relay_send(s: &mut Session, frame: &Frame) {
    s.relay_tx.send_frame(frame).await.unwrap();
}

async fn client_recv(s: &mut Session) -> Frame {
    s.client_rx.recv_frame().await.unwrap()
}

async fn relay_recv(s: &mut Session) -> Frame {
    s.relay_rx.recv_frame().await.unwrap()
}

#[tokio::test]
async fn data_plane_diff_and_self_heal_over_the_wire() {
    let (mut session, features, _compression) = negotiated_session().await;
    assert!(features.supports(Capability::Incremental));
    assert!(features.supports(Capability::RequestResync));

    let server_dp = test_dataplane(DataPlaneSide::Relay);
    let client_dp = test_dataplane(DataPlaneSide::Client);
    // Chunk 1024 on a 2048-byte payload: one chunked region changes per step.
    let mut server_cache = ConnectionDataCache::with_chunk_size(1024);
    let mut client_cache = ConnectionDataCache::new();
    // Two stats instances mirror the real topology: the relay counts what it
    // sends, the client counts what it receives.
    let relay_stats = ConnectionStats::new();
    let client_stats = ConnectionStats::new();
    let conn = ConnectionId::new(7);

    // 1) First payload has no diff base → plain frame on the wire.
    let zeros = vec![0u8; 2048];
    let seq = server_cache.next_sequence_for(conn);
    let frame = server_dp.build_frame(
        conn,
        seq,
        Bytes::from(zeros.clone()),
        Some(&mut server_cache),
        Some(&relay_stats),
    );
    assert!(matches!(frame, Frame::StreamData(_)), "{frame:?}");
    relay_send(&mut session, &frame).await;
    let outcome = client_dp.apply_inbound(
        client_recv(&mut session).await,
        &mut client_cache,
        &client_stats,
    );
    assert!(matches!(outcome, InboundOutcome::Payload { .. }));

    // 2) Mostly-identical payload → diff frame, applied to identical bytes.
    let mut changed = zeros.clone();
    changed[1100..1200].fill(0x5A);
    let seq = server_cache.next_sequence_for(conn);
    let frame = server_dp.build_frame(
        conn,
        seq,
        Bytes::from(changed.clone()),
        Some(&mut server_cache),
        Some(&relay_stats),
    );
    assert!(
        matches!(frame, Frame::IncrementalStreamData(_)),
        "expected diff frame, got {frame:?}"
    );
    relay_send(&mut session, &frame).await;
    match client_dp.apply_inbound(
        client_recv(&mut session).await,
        &mut client_cache,
        &client_stats,
    ) {
        InboundOutcome::Payload { data, .. } => assert_eq!(data, Bytes::from(changed.clone())),
        other => panic!("expected payload, got {other:?}"),
    }

    // 3) Client loses its diff cache → next diff cannot apply → resync
    //    request travels back over the wire and the server drops its base.
    client_cache.clear_connection(conn);
    let mut changed_again = changed.clone();
    changed_again[1500..1600].fill(0x33);
    let seq = server_cache.next_sequence_for(conn);
    let frame = server_dp.build_frame(
        conn,
        seq,
        Bytes::from(changed_again.clone()),
        Some(&mut server_cache),
        Some(&relay_stats),
    );
    assert!(matches!(frame, Frame::IncrementalStreamData(_)));
    relay_send(&mut session, &frame).await;
    let arrived = client_recv(&mut session).await;
    match client_dp.apply_inbound(arrived, &mut client_cache, &client_stats) {
        InboundOutcome::ResyncRequired { connection_id } => {
            assert_eq!(connection_id, conn);
        }
        other => panic!("expected ResyncRequired, got {other:?}"),
    }

    // The client-side response path (mirror of serve::request_full_resend):
    // the request frame travels back over the real client→relay channel and
    // the relay end clears its diff base (relay::connection would do the same).
    session
        .client_tx
        .send_frame(&Frame::StreamResyncRequest(StreamResyncRequestMessage {
            connection_id: conn,
        }))
        .await
        .unwrap();
    match session.relay_rx.recv_frame().await.unwrap() {
        Frame::StreamResyncRequest(m) => {
            assert_eq!(m.connection_id, conn);
            server_cache.clear_connection(conn);
        }
        other => panic!("expected StreamResyncRequest at relay, got {other:?}"),
    }

    // 4) Next payload must be a full frame again (base was dropped).
    let seq = server_cache.next_sequence_for(conn);
    let frame = server_dp.build_frame(
        conn,
        seq,
        Bytes::from(changed_again.clone()),
        Some(&mut server_cache),
        Some(&relay_stats),
    );
    assert!(matches!(frame, Frame::StreamData(_)), "{frame:?}");
    relay_send(&mut session, &frame).await;
    match client_dp.apply_inbound(
        client_recv(&mut session).await,
        &mut client_cache,
        &client_stats,
    ) {
        InboundOutcome::Payload { data, .. } => {
            assert_eq!(data, Bytes::from(changed_again));
        }
        other => panic!("expected payload after resync, got {other:?}"),
    }

    // Relay emitted two diffs (steps 2 & 3); client applied exactly one.
    let relay_summary = relay_stats.summary();
    assert!(
        relay_summary.contains("Incremental: 2 frames"),
        "{relay_summary}"
    );
    let client_summary = client_stats.summary();
    assert!(
        client_summary.contains("Incremental: 1 frames"),
        "{client_summary}"
    );
}

/// The CLIENT's request stream (local X server → relay) goes through the
/// same chain as the picture, mirrored: diffs ride the wire once
/// `incremental_uplink` negotiated, compression stacks on top of them
/// (0x23), and a desync self-heals back the other way.
#[tokio::test]
async fn uplink_diff_compression_and_self_heal_over_the_wire() {
    let (mut session, features, compression) = negotiated_session().await;
    // Defaults negotiate the whole data-plane family in both directions,
    // and the best mutual algorithm — stream zstd, which stacks on diffs.
    assert!(features.supports(Capability::Incremental));
    assert!(features.supports(Capability::IncrementalUplink));
    assert_eq!(compression, Some(CompressionAlgo::ZstdStream));

    let client_dp = test_dataplane_zstd(DataPlaneSide::Client);
    let relay_dp = test_dataplane_zstd(DataPlaneSide::Relay);
    let mut uplink_cache = ConnectionDataCache::with_chunk_size(1024);
    let mut relay_receive_cache = ConnectionDataCache::new();
    let client_stats = ConnectionStats::new();
    let relay_stats = ConnectionStats::new();
    let conn = ConnectionId::new(9);

    // A high-entropy-ish base so the stream compressor has real content to
    // carry; mostly-unchanged afterwards so the DIFFER sees savings.
    fn payload(seed: u8) -> Vec<u8> {
        (0..2048u32)
            .map(|j| ((j * 31 + seed as u32 * 7) % 251) as u8 ^ seed)
            .collect()
    }

    // 1) First payload: no base yet → whole payload over the wire,
    //    compressed (the plain frame never becomes a diff).
    let first = payload(1);
    let seq = uplink_cache.next_sequence_for(conn);
    let frame = client_dp.build_frame(
        conn,
        seq,
        Bytes::from(first.clone()),
        Some(&mut uplink_cache),
        Some(&client_stats),
    );
    assert!(matches!(frame, Frame::CompressedStreamData(_)), "{frame:?}");
    session.client_tx.send_frame(&frame).await.unwrap();
    match relay_dp.apply_inbound(
        relay_recv(&mut session).await,
        &mut relay_receive_cache,
        &relay_stats,
    ) {
        InboundOutcome::Payload { data, .. } => assert_eq!(data, Bytes::from(first.clone())),
        other => panic!("expected payload, got {other:?}"),
    }

    // 2) Mostly-identical payload → diff frame, and stream compression
    //    stacks on it: shape 0x23 on the wire. The changed region is
    //    deliberately wider than the compression threshold so the encoded
    //    diff is big enough for the compressor to take it at all.
    let mut second = payload(1);
    second[1500..1900].fill(0xA5);
    let seq = uplink_cache.next_sequence_for(conn);
    let frame = client_dp.build_frame(
        conn,
        seq,
        Bytes::from(second.clone()),
        Some(&mut uplink_cache),
        Some(&client_stats),
    );
    assert!(
        matches!(frame, Frame::CompressedIncrementalStreamData(_)),
        "expected compressed diff, got {frame:?}"
    );
    session.client_tx.send_frame(&frame).await.unwrap();
    match relay_dp.apply_inbound(
        relay_recv(&mut session).await,
        &mut relay_receive_cache,
        &relay_stats,
    ) {
        InboundOutcome::Payload { data, .. } => assert_eq!(data, Bytes::from(second.clone())),
        other => panic!("expected payload, got {other:?}"),
    }

    // 3) The RELAY loses its receive base (channel reset, restart, any
    //    reason): the next diff cannot apply → resync request travels back
    //    over the wire relay→client, and the client pump drops its send base.
    relay_receive_cache.clear_connection(conn);
    let mut third = second.clone();
    third[100..500].fill(0x33);
    let seq = uplink_cache.next_sequence_for(conn);
    let frame = client_dp.build_frame(
        conn,
        seq,
        Bytes::from(third.clone()),
        Some(&mut uplink_cache),
        Some(&client_stats),
    );
    assert!(matches!(frame, Frame::CompressedIncrementalStreamData(_)));
    session.client_tx.send_frame(&frame).await.unwrap();
    match relay_dp.apply_inbound(
        relay_recv(&mut session).await,
        &mut relay_receive_cache,
        &relay_stats,
    ) {
        InboundOutcome::ResyncRequired { connection_id } => assert_eq!(connection_id, conn),
        other => panic!("expected ResyncRequired, got {other:?}"),
    }
    // Mirror of relay::connection's handler: stats fallback + the request
    // frame on its way back to the client.
    relay_stats.add_incremental_full_fallback();
    session
        .relay_tx
        .send_frame(&Frame::StreamResyncRequest(StreamResyncRequestMessage {
            connection_id: conn,
        }))
        .await
        .unwrap();
    match client_recv(&mut session).await {
        Frame::StreamResyncRequest(m) => {
            assert_eq!(m.connection_id, conn);
            // The pump's response (proxy::uplink): drop the base so the
            // next payload ships whole.
            uplink_cache.clear_connection(conn);
        }
        other => panic!("expected StreamResyncRequest at client, got {other:?}"),
    }

    // 4) Next payload is a compressed WHOLE frame again (no base to diff
    //    against), and it re-establishes the receive base for future diffs.
    let seq = uplink_cache.next_sequence_for(conn);
    let frame = client_dp.build_frame(
        conn,
        seq,
        Bytes::from(third.clone()),
        Some(&mut uplink_cache),
        Some(&client_stats),
    );
    assert!(matches!(frame, Frame::CompressedStreamData(_)), "{frame:?}");
    session.client_tx.send_frame(&frame).await.unwrap();
    match relay_dp.apply_inbound(
        relay_recv(&mut session).await,
        &mut relay_receive_cache,
        &relay_stats,
    ) {
        InboundOutcome::Payload { data, .. } => assert_eq!(data, Bytes::from(third.clone())),
        other => panic!("expected payload after resync, got {other:?}"),
    }

    // 5) And a diff right after the reset applies cleanly — the loop is
    //    self-healing in BOTH directions now. The change stays inside one
    //    1024-byte diff block so the diff clears the savings ratio.
    let mut fourth = third.clone();
    fourth[200..600].fill(0x77);
    let seq = uplink_cache.next_sequence_for(conn);
    let frame = client_dp.build_frame(
        conn,
        seq,
        Bytes::from(fourth.clone()),
        Some(&mut uplink_cache),
        Some(&client_stats),
    );
    assert!(matches!(frame, Frame::CompressedIncrementalStreamData(_)));
    session.client_tx.send_frame(&frame).await.unwrap();
    match relay_dp.apply_inbound(
        relay_recv(&mut session).await,
        &mut relay_receive_cache,
        &relay_stats,
    ) {
        InboundOutcome::Payload { data, .. } => assert_eq!(data, Bytes::from(fourth)),
        other => panic!("expected payload, got {other:?}"),
    }

    // The client sent three compressed diffs (steps 2, 3, 5); the relay
    // applied two payloads and fell back exactly once.
    let client_summary = client_stats.summary();
    assert!(
        client_summary.contains("Incremental: 3 frames"),
        "{client_summary}"
    );
    let relay_summary = relay_stats.summary();
    assert!(
        relay_summary.contains("Incremental: 2 frames") && relay_summary.contains("1 fallbacks"),
        "{relay_summary}"
    );
}

#[tokio::test]
async fn feature_gated_dataplane_never_emits_diffs() {
    // Same caches, but Incremental not negotiated: every frame is plain or
    // compressed, matching what an older/feature-restricted peer would get.
    // Everything except Incremental.
    let features = Features::from_list(&["request_resync".to_string(), "flow_control".to_string()]);
    let dp = DataPlane::new(
        DataPlaneSide::Relay,
        features,
        None,
        &RuntimeConfig::default(),
    );
    let mut cache = ConnectionDataCache::new();
    let conn = ConnectionId::new(3);
    let first = vec![0u8; 2048];
    dp.build_frame(conn, 1, Bytes::from(first.clone()), Some(&mut cache), None);
    let mut second = first.clone();
    second[500] = 1;
    let frame = dp.build_frame(conn, 2, Bytes::from(second), Some(&mut cache), None);
    assert!(matches!(frame, Frame::StreamData(_)), "{frame:?}");
}

#[tokio::test]
async fn resume_session_id_round_trips_through_negotiation() {
    let rt = RuntimeConfig::default();
    let (server_side, client_side) = duplex(4096);
    let mut server_transport = Rx11Transport::new(server_side);
    let mut client_transport = Rx11Transport::new(client_side);
    let token = Token::new(generate_token()).unwrap();
    let resume = SessionId::new("resume-me-42".to_string()).unwrap();

    let (server_hs, client_hs) = tokio::join!(
        server_handshake(
            &mut server_transport,
            token.as_str(),
            HANDSHAKE_TIMEOUT,
            &rt,
        ),
        client_handshake(
            &mut client_transport,
            &token,
            Some(&resume),
            HANDSHAKE_TIMEOUT,
            &rt,
        )
    );
    assert_eq!(server_hs.unwrap().resume_session_id, Some(resume.clone()));
    assert_eq!(client_hs.unwrap().resume_session_id, Some(resume));
}

/// A length-valid header carrying an undecodable payload must fail the
/// transport: X11 is a strict byte stream, so silently skipping the frame
/// would lose those bytes forever. Reconnect-with-resume is the recovery.
#[tokio::test]
async fn valid_header_with_corrupt_payload_fails_the_connection() {
    use tokio::io::AsyncWriteExt;
    let (server_side, mut client_side) = duplex(4096);
    let mut transport = Rx11Transport::new(server_side);

    let mut raw = Vec::new();
    raw.extend_from_slice(&rx11_core::protocol::MAGIC_BYTES);
    raw.push(rx11_core::protocol::WIRE_VERSION);
    // StreamData (0x20) claiming a 4-byte payload: below the 8-byte minimum.
    raw.extend_from_slice(&0x20u16.to_be_bytes());
    raw.extend_from_slice(&4u32.to_be_bytes());
    raw.extend_from_slice(&[0u8; 4]);
    client_side.write_all(&raw).await.unwrap();

    let err = transport.recv_frame().await.unwrap_err();
    assert!(
        matches!(err, rx11_core::error::Rx11Error::Protocol(_)),
        "expected protocol error, got {err:?}"
    );
}

/// The coalescing writer must preserve frame ORDER and byte-exact framing
/// when multiple frames arrive while it is busy (the whole point of the
/// batch optimization is that the wire stays identical).
#[tokio::test]
async fn drain_frames_batches_without_changing_the_wire() {
    let (a, b) = duplex(64 * 1024);
    let transport_a = Rx11Transport::new(a);
    let mut reader = Rx11Transport::new(b);
    let (tx, rx) = rx11_core::queue::outbound_channel(64, rx11_core::queue::UNLIMITED);
    let (_, write_half) = transport_a.split();

    let frames: Vec<Frame> = (1..=20u32)
        .map(|i| {
            Frame::StreamData(rx11_core::protocol::StreamDataMessage {
                connection_id: ConnectionId::new(7),
                sequence_id: i,
                data: Bytes::from(vec![i as u8; 4096]),
            })
        })
        .collect();
    let expected = frames.clone();
    let sender = tokio::spawn(async move {
        for f in &frames {
            tx.send(f.clone()).await.unwrap();
        }
        // Close the queue before draining: drain_frames runs over whatever
        // piled up (all 20 here, duplex capacity permitting) and ends.
        drop(tx);
        rx11_core::transport::drain_frames(rx, write_half).await;
    });
    for want in &expected {
        let got = reader.recv_frame().await.unwrap();
        match (want, &got) {
            (Frame::StreamData(w), Frame::StreamData(g)) => {
                assert_eq!(w.sequence_id, g.sequence_id);
                assert_eq!(&w.data[..], &g.data[..]);
            }
            _ => panic!("frame shape drift: expected {want:?}"),
        }
    }
    sender.await.unwrap();
}
