//! The zero-chunk reference frame across the wire, and the byte
//! integrity of the expansion.
//!
//! A reference frame says "the content did not change". The receiver
//! expands it from its base. The expansion must be BYTE-IDENTICAL to the
//! base — the shortcut that hands out the base itself must not change a
//! single byte on the wire.
//!
//! This drives the real data plane over a duplex stream: send a payload,
//! send it again (the second one becomes a reference frame), and check
//! that what comes out the other end equals what went in.

use rx11_core::compress::CompressionAlgo;
use rx11_core::dataplane::{DataPlane, DataPlaneSide};
use rx11_core::incremental::ConnectionDataCache;
use rx11_core::protocol::{
    Capability, Features, Frame, IncrementalStreamDataMessage, StreamResyncRequestMessage,
};
use rx11_core::stats::ConnectionStats;
use rx11_core::types::ConnectionId;

/// A payload big enough to clear the diff floor and big enough that the
/// shortcut is worth taking.
const PAYLOAD_LEN: usize = 64 * 1024;

fn features() -> Features {
    Features::from_list(&[
        Capability::RequestResync.name().to_string(),
        Capability::Incremental.name().to_string(),
    ])
}

#[tokio::test]
async fn an_unchanged_payload_crosses_the_wire_as_a_reference_frame_and_expands_intact() {
    let conn = ConnectionId::new(42);
    let stats = ConnectionStats::new();

    let sender = DataPlane::new(
        DataPlaneSide::Relay,
        features(),
        Some(CompressionAlgo::Zstd),
        &Default::default(),
    );
    let receiver = DataPlane::new(
        DataPlaneSide::Client,
        features(),
        Some(CompressionAlgo::Zstd),
        &Default::default(),
    );

    let mut send_cache = ConnectionDataCache::from_runtime(&Default::default());
    let mut recv_cache = ConnectionDataCache::from_runtime(&Default::default());

    // A payload with real structure (not all zeros): a copy that lost
    // something would be visible.
    let payload: bytes::Bytes = (0..PAYLOAD_LEN)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<u8>>()
        .into();

    // First frame: the whole payload.
    let frame = sender.build_frame(
        conn,
        1,
        payload.clone(),
        Some(&mut send_cache),
        Some(&stats),
    );
    let frame = match frame {
        Frame::StreamData(_) | Frame::CompressedStreamData(_) => frame,
        other => panic!("the first frame must carry the payload: {other:?}"),
    };

    // The receiver takes it in and keeps it as its base.
    let outcome = receiver.apply_inbound(frame, &mut recv_cache, &stats);
    let first = match outcome {
        rx11_core::dataplane::InboundOutcome::Payload { data, .. } => data,
        other => panic!("the first frame must arrive as a payload: {other:?}"),
    };
    assert_eq!(first, payload, "the first frame must arrive intact");

    // Second frame: the same payload again. The sender's cache now holds
    // it, so this becomes a reference frame — zero content on the wire.
    let frame = sender.build_frame(
        conn,
        2,
        payload.clone(),
        Some(&mut send_cache),
        Some(&stats),
    );
    let is_reference = matches!(frame, Frame::IncrementalStreamData(ref m) if m.chunks.is_empty());
    assert!(
        is_reference,
        "an unchanged payload must become a reference frame, got {:?}",
        frame.msg_type()
    );

    // The reference frame's own size on the wire: the header only.
    let wire_bytes = match &frame {
        Frame::IncrementalStreamData(m) => {
            4 + 4 + 4 + 4 + 2 + m.chunks.iter().map(|c| c.data.len()).sum::<usize>()
        }
        _ => unreachable!(),
    };
    assert!(
        wire_bytes < 64,
        "a reference frame must carry no content, got {wire_bytes} bytes"
    );

    // The receiver expands it — and the expansion must equal the payload.
    let outcome = receiver.apply_inbound(frame, &mut recv_cache, &stats);
    let second = match outcome {
        rx11_core::dataplane::InboundOutcome::Payload { data, .. } => data,
        other => panic!("the reference frame must arrive as a payload: {other:?}"),
    };
    assert_eq!(
        second, payload,
        "the expansion of a reference frame must be byte-identical to the base"
    );
}

#[tokio::test]
async fn the_expansion_of_a_reference_frame_survives_a_compressed_round_trip() {
    // The reference frame itself crosses the wire compressed; the
    // expansion must still be byte-identical.
    let conn = ConnectionId::new(7);
    let stats = ConnectionStats::new();

    let sender = DataPlane::new(
        DataPlaneSide::Relay,
        features(),
        Some(CompressionAlgo::Zstd),
        &Default::default(),
    );
    let receiver = DataPlane::new(
        DataPlaneSide::Client,
        features(),
        Some(CompressionAlgo::Zstd),
        &Default::default(),
    );

    let mut send_cache = ConnectionDataCache::from_runtime(&Default::default());
    let mut recv_cache = ConnectionDataCache::from_runtime(&Default::default());

    let payload: bytes::Bytes = (0..PAYLOAD_LEN)
        .map(|i| ((i * 7) % 253) as u8)
        .collect::<Vec<u8>>()
        .into();

    let frame = sender.build_frame(
        conn,
        1,
        payload.clone(),
        Some(&mut send_cache),
        Some(&stats),
    );
    let outcome = receiver.apply_inbound(frame, &mut recv_cache, &stats);
    assert!(matches!(
        outcome,
        rx11_core::dataplane::InboundOutcome::Payload { .. }
    ));

    let frame = sender.build_frame(
        conn,
        2,
        payload.clone(),
        Some(&mut send_cache),
        Some(&stats),
    );
    let outcome = receiver.apply_inbound(frame, &mut recv_cache, &stats);
    let expanded = match outcome {
        rx11_core::dataplane::InboundOutcome::Payload { data, .. } => data,
        other => panic!("the reference frame must arrive as a payload: {other:?}"),
    };
    assert_eq!(expanded, payload);
}

#[tokio::test]
async fn a_reference_frame_with_a_wrong_length_is_refused() {
    // A reference frame that declares a length the base does not have is
    // malformed: the receiver must refuse it whole (the standard resync
    // path heals the stream) rather than hand out zero-padded bytes.
    let conn = ConnectionId::new(9);
    let stats = ConnectionStats::new();
    let receiver = DataPlane::new(
        DataPlaneSide::Client,
        features(),
        Some(CompressionAlgo::Zstd),
        &Default::default(),
    );
    let mut recv_cache = ConnectionDataCache::from_runtime(&Default::default());
    recv_cache.update_cache(conn, 1, &vec![7u8; 4096]);

    let malformed = IncrementalStreamDataMessage {
        connection_id: conn,
        sequence_id: 2,
        base_sequence_id: 1,
        total_len: 2048,
        chunks: Vec::new(),
    };
    let outcome = receiver.apply_inbound(
        Frame::IncrementalStreamData(malformed),
        &mut recv_cache,
        &stats,
    );
    assert!(
        matches!(
            outcome,
            rx11_core::dataplane::InboundOutcome::Dropped
                | rx11_core::dataplane::InboundOutcome::ResyncRequired { .. }
        ),
        "a malformed reference frame must not be expanded, got {outcome:?}"
    );
    // The base survives: the next whole frame re-establishes it.
    assert_eq!(recv_cache.get_cached(conn).map(|(seq, _)| seq), Some(1));
}

/// The resync frame is what a receiver sends when it cannot expand a
/// diff. This test pins that the mechanism is still reachable after the
/// expansion shortcut: a stale base must still trigger it.
#[tokio::test]
async fn a_stale_base_still_triggers_the_resync_path() {
    let conn = ConnectionId::new(11);
    let stats = ConnectionStats::new();
    let receiver = DataPlane::new(
        DataPlaneSide::Client,
        features(),
        Some(CompressionAlgo::Zstd),
        &Default::default(),
    );
    let mut recv_cache = ConnectionDataCache::from_runtime(&Default::default());
    recv_cache.update_cache(conn, 5, &vec![3u8; 4096]);

    let stale = IncrementalStreamDataMessage {
        connection_id: conn,
        sequence_id: 6,
        base_sequence_id: 4, // the receiver holds 5
        total_len: 4096,
        chunks: Vec::new(),
    };
    let outcome =
        receiver.apply_inbound(Frame::IncrementalStreamData(stale), &mut recv_cache, &stats);
    assert!(
        matches!(
            outcome,
            rx11_core::dataplane::InboundOutcome::ResyncRequired { .. }
        ),
        "a stale base must trigger the resync path, got {outcome:?}"
    );
    // The frame the receiver would send back is the resync request.
    let _ = StreamResyncRequestMessage {
        connection_id: conn,
    };
}
