//! Data-plane policy chain: how a raw X11 payload becomes a frame, and how
//! an inbound data frame becomes payload again.
//!
//! Both ends build a [`DataPlane`] from the negotiated [`Features`] +
//! [`RuntimeConfig`], tagged with the [`DataPlaneSide`] it sits on, so:
//! - diff/compression decisions are **capability-gated**, not guessed from
//!   whatever frame arrives;
//! - the diff policy is **per direction**: `incremental` covers the picture
//!   (relay → client, the plane's send side when `side == Relay`), while
//!   `incremental_uplink` opens the application request stream
//!   (client → relay). A receiver refuses a diff shape its side never
//!   negotiated instead of letting it touch the cache;
//! - savings accounting happens at one place (frame construction), keeping
//!   server and client stats semantics symmetric;
//! - adding a new data-plane stage (e.g. a future retransmit window or
//!   flow-control hook) means extending this module, not the relay loops.

use std::sync::Mutex;

use bytes::Bytes;
use zstd::stream::raw::{Decoder, Encoder, InBuffer, Operation, OutBuffer};

use crate::compress::CompressionAlgo;
use crate::config::RuntimeConfig;
use crate::incremental::ConnectionDataCache;
use crate::protocol::{
    BinaryMessageCodec, Capability, CompressedIncrementalStreamDataMessage,
    CompressedStreamDataMessage, Features, Frame, IncrementalStreamDataMessage, StreamDataMessage,
};
use crate::stats::ConnectionStats;
use crate::types::ConnectionId;

/// Result of feeding one inbound data frame through the chain.
#[derive(Debug, PartialEq, Eq)]
pub enum InboundOutcome {
    /// Plain application payload; the diff cache has been kept in sync.
    Payload {
        connection_id: ConnectionId,
        data: Bytes,
    },
    /// A diff could not be applied against our cache: the peer should stop
    /// diffing this connection and resend full frames.
    ResyncRequired { connection_id: ConnectionId },
    /// Frame unusable (e.g. compressed without a negotiated algorithm);
    /// already logged, nothing to deliver.
    Dropped,
    /// A negotiated compressed frame failed to decompress: those application
    /// bytes are gone, and X11 is a positional byte stream — skipping them
    /// would silently corrupt the session forever (in cross-frame stream
    /// mode the decoder context is desynchronized too). The caller MUST
    /// fail the connection; reconnect-with-resume is the recovery path.
    StreamBroken,
}

/// Reusable zstd contexts. `zstd::encode_all`/`decode_all` allocate and
/// initialize a fresh (de)compression context per call — at ~tens of
/// microseconds each, that dominated the per-frame CPU at our frame sizes.
/// A `DataPlane` is per-connection-task, so this state rides along with the
/// negotiated policy it belongs to; the mutex only guards interior mutability
/// (never held across an await).
/// Opaque pooled codec state; the zstd contexts don't implement Debug.
#[derive(Default)]
struct ZstdContexts {
    compressor: Mutex<Option<zstd::bulk::Compressor<'static>>>,
    decompressor: Mutex<Option<zstd::bulk::Decompressor<'static>>>,
    /// Cross-frame streaming contexts (`zstd_stream` mode): one continuous
    /// compression stream per direction, flushed after every frame so the
    /// peer can decode incrementally while history survives across frames.
    stream_encoder: Mutex<Option<Encoder<'static>>>,
    stream_decoder: Mutex<Option<Decoder<'static>>>,
}

impl std::fmt::Debug for ZstdContexts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZstdContexts")
            .field("pooled", &true)
            .finish()
    }
}

/// Which end of the relay link a [`DataPlane`] sits on. One instance serves
/// exactly one send direction and one receive direction (the relay plane
/// sends the picture and receives requests; the client plane is the mirror),
/// so the diff policy per direction derives from this instead of from a
/// guess about which frames happen to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPlaneSide {
    /// The server end: diffs the downlink picture on `incremental`, and
    /// accepts uplink diffs only when the peer also negotiated
    /// `incremental_uplink`.
    Relay,
    /// The client end: the mirror image of [`Self::Relay`].
    Client,
}

/// Negotiated data-plane configuration for one relay session.
#[derive(Debug, Clone)]
pub struct DataPlane {
    pub features: Features,
    pub compression: Option<CompressionAlgo>,
    pub compression_threshold: usize,
    side: DataPlaneSide,
    zstd: std::sync::Arc<ZstdContexts>,
}

impl DataPlane {
    pub fn new(
        side: DataPlaneSide,
        features: Features,
        compression: Option<CompressionAlgo>,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self {
            features,
            compression,
            compression_threshold: runtime.compression_threshold,
            side,
            zstd: std::sync::Arc::new(ZstdContexts::default()),
        }
    }

    /// May this plane emit `IncrementalStreamData` / compressed diffs?
    /// The frame family rides `incremental`; `incremental_uplink` is what
    /// opens the request stream (the client's send direction) on top of it.
    /// Public because the uplink pump decides with it whether to keep a
    /// diff base at all — the receiver still re-checks on arrival.
    pub fn may_diff_send(&self) -> bool {
        self.features.supports(Capability::Incremental)
            && (self.side == DataPlaneSide::Relay
                || self.features.supports(Capability::IncrementalUplink))
    }

    /// May this plane *apply* an inbound diff frame? Mirror of
    /// [`Self::may_diff_send`]: a receiver that never negotiated the shape
    /// refuses it outright rather than letting it touch the cache.
    pub fn may_diff_receive(&self) -> bool {
        self.features.supports(Capability::Incremental)
            && (self.side == DataPlaneSide::Client
                || self.features.supports(Capability::IncrementalUplink))
    }

    fn compress(&self, data: &[u8]) -> Option<Bytes> {
        let algo = self.compression?;
        if data.len() < self.compression_threshold {
            // Encoder untouched; the plain frame crosses the wire identically
            // on both ends — stream stays in lockstep.
            return None;
        }
        match algo {
            // Stream mode NEVER rejects: the encoder has already consumed
            // this payload into the shared history, so the fragment must go
            // on the wire even when it is slightly larger than the input —
            // the peer must feed its decoder the same bytes or the contexts
            // desynchronize and every later frame corrupts.
            CompressionAlgo::ZstdStream => self.zstd_stream_compress(data),
            CompressionAlgo::Zstd => {
                let c = self.zstd_compress(data)?;
                if c.len() >= data.len() {
                    None
                } else {
                    Some(c)
                }
            }
            other => other.compress_with(data, 0),
        }
    }

    fn zstd_compress(&self, data: &[u8]) -> Option<Bytes> {
        let mut guard = self.zstd.compressor.lock().ok()?;
        if guard.is_none() {
            *guard =
                Some(zstd::bulk::Compressor::new(crate::compress::ZSTD_COMPRESSION_LEVEL).ok()?);
        }
        // Reused context, one-shot compress; the Vec moves into `Bytes`
        // without a copy.
        let out = guard.as_mut()?.compress(data).ok()?;
        Some(Bytes::from(out))
    }

    /// Decompress one compressed data-plane payload with the negotiated
    /// algorithm, reusing the pooled zstd context. Sole decompression
    /// authority for both ends' inbound paths.
    pub fn decompress_payload(&self, compressed: &[u8], original_len: usize) -> Option<Vec<u8>> {
        let algo = self.compression?;
        match algo {
            CompressionAlgo::Zstd => self.zstd_decompress(compressed, original_len),
            CompressionAlgo::ZstdStream => self.zstd_stream_decompress(compressed, original_len),
            other => other.decompress(compressed, original_len),
        }
    }

    fn zstd_decompress(&self, compressed: &[u8], original_len: usize) -> Option<Vec<u8>> {
        let mut guard = self.zstd.decompressor.lock().ok()?;
        if guard.is_none() {
            *guard = Some(zstd::bulk::Decompressor::new().ok()?);
        }
        // Capped at the advertised size: an oversized/undersized frame
        // errors out here, which is exactly the integrity check the
        // stateless path performed by comparing result lengths.
        let out = guard.as_mut()?.decompress(compressed, original_len).ok()?;
        if out.len() != original_len {
            return None;
        }
        Some(out)
    }

    /// `zstd_stream` encode: feed the payload into the continuous stream,
    /// then flush so the peer can decode *this* frame immediately. The
    /// internal window/history survives across frames — that is the whole
    /// point: repeated X11 content compresses against everything sent so
    /// far, not just against itself.
    fn zstd_stream_compress(&self, data: &[u8]) -> Option<Bytes> {
        let mut guard = self.zstd.stream_encoder.lock().ok()?;
        if guard.is_none() {
            *guard = Some(Encoder::new(crate::compress::ZSTD_COMPRESSION_LEVEL).ok()?);
        }
        let enc = guard.as_mut()?;

        let mut out: Vec<u8> =
            Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()) + 128);
        let mut consumed = 0usize;
        let mut cursor = 0usize;
        while consumed < data.len() {
            let mut ib = InBuffer::around(&data[consumed..]);
            let mut ob = OutBuffer::around_pos(&mut out, cursor);
            enc.run(&mut ib, &mut ob).ok()?;
            let read = ib.pos();
            cursor = ob.pos();
            if read == 0 {
                // No input consumed: either the output filled up (grow and
                // retry) or we are genuinely stuck (context malfunction).
                if cursor >= out.capacity() {
                    out.reserve(out.capacity().max(4096));
                    continue;
                }
                return None;
            }
            consumed += read;
        }
        // Drain the internal buffer; flush keeps history, finish would not.
        loop {
            let mut ob = OutBuffer::around_pos(&mut out, cursor);
            let remaining = enc.flush(&mut ob).ok()?;
            cursor = ob.pos();
            if remaining == 0 {
                break;
            }
            if cursor >= out.capacity() {
                out.reserve(out.capacity().max(4096));
            }
        }
        Some(Bytes::from(out))
    }

    /// `zstd_stream` decode: feed the fragment until exactly `original_len`
    /// bytes have been produced and the fragment is fully consumed. Any
    /// stall or leftover is a hard stream violation (`None` → the caller
    /// fails the connection — a desynced context cannot recover per frame).
    fn zstd_stream_decompress(&self, compressed: &[u8], original_len: usize) -> Option<Vec<u8>> {
        if original_len == 0 {
            return Some(Vec::new());
        }
        let mut guard = self.zstd.stream_decoder.lock().ok()?;
        if guard.is_none() {
            *guard = Some(Decoder::new().ok()?);
        }
        let dec = guard.as_mut()?;

        let mut out = vec![0u8; original_len];
        let mut consumed = 0usize;
        let mut produced = 0usize;
        while produced < original_len {
            let mut ib = InBuffer::around(&compressed[consumed..]);
            let mut ob = OutBuffer::around(&mut out[produced..]);
            dec.run(&mut ib, &mut ob).ok()?;
            consumed += ib.pos();
            produced += ob.pos();
            if ib.pos() == 0 && ob.pos() == 0 {
                return None; // needs more input than the frame carries
            }
        }
        if consumed != compressed.len() {
            return None; // trailing bytes belong to no payload
        }
        Some(out)
    }

    fn plain_or_compressed(
        &self,
        connection_id: ConnectionId,
        sequence_id: u32,
        data: Bytes,
    ) -> Frame {
        let original_len = data.len();
        if let Some(compressed) = self.compress(&data) {
            return Frame::CompressedStreamData(CompressedStreamDataMessage {
                connection_id,
                sequence_id,
                original_len,
                data: compressed,
            });
        }
        Frame::StreamData(StreamDataMessage {
            connection_id,
            sequence_id,
            data,
        })
    }

    /// Wrap a computed diff, optionally compressing the encoded diff.
    fn wrap_diff(&self, diff: IncrementalStreamDataMessage, conn: ConnectionId, seq: u32) -> Frame {
        if self.compression.is_some() {
            if let Ok(encoded) = diff.encode_payload() {
                let original_len = encoded.len();
                if let Some(compressed) = self.compress(&encoded) {
                    return Frame::CompressedIncrementalStreamData(
                        CompressedIncrementalStreamDataMessage {
                            connection_id: conn,
                            sequence_id: seq,
                            original_len,
                            data: compressed,
                        },
                    );
                }
            }
        }
        Frame::IncrementalStreamData(diff)
    }

    /// Build the outbound frame for raw X11 payload. When `cache` is present
    /// *and* the Incremental capability is negotiated, repetitive payloads
    /// ship as diffs. `stats` (either side's ConnectionStats) receives wire
    /// bytes and savings accounting for the frame actually emitted.
    pub fn build_frame(
        &self,
        connection_id: ConnectionId,
        sequence_id: u32,
        data: Bytes,
        cache: Option<&mut ConnectionDataCache>,
        stats: Option<&ConnectionStats>,
    ) -> Frame {
        let original_len = data.len();
        let frame = match (self.may_diff_send(), cache) {
            (true, Some(cache)) => {
                let diff = cache.compute_incremental_owned(connection_id, sequence_id, &data);
                cache.update_cache_owned(connection_id, sequence_id, data.clone());
                match diff {
                    Some(diff) => self.wrap_diff(diff, connection_id, sequence_id),
                    None => self.plain_or_compressed(connection_id, sequence_id, data),
                }
            }
            _ => self.plain_or_compressed(connection_id, sequence_id, data),
        };
        if let Some(stats) = stats {
            record_outbound(stats, &frame, original_len);
        }
        frame
    }

    /// Decode one inbound data frame, maintaining `cache` and inbound stats.
    /// The receive base is only maintained while THIS direction can see diff
    /// frames ([`Self::may_diff_receive`]): a plain frame is the base a later
    /// diff applies against, and caching payloads that can never be a base
    /// would be pure memory cost.
    pub fn apply_inbound(
        &self,
        frame: Frame,
        cache: &mut ConnectionDataCache,
        stats: &ConnectionStats,
    ) -> InboundOutcome {
        let keep_base = self.may_diff_receive();
        match frame {
            Frame::StreamData(msg) => {
                stats.add_bytes_received(msg.data.len() as u64);
                if keep_base {
                    cache.update_cache_owned(msg.connection_id, msg.sequence_id, msg.data.clone());
                }
                InboundOutcome::Payload {
                    connection_id: msg.connection_id,
                    data: msg.data,
                }
            }
            Frame::CompressedStreamData(msg) => {
                if self.compression.is_none() {
                    tracing::warn!("CompressedStreamData but no compression negotiated, dropping");
                    return InboundOutcome::Dropped;
                }
                let original_len = msg.original_len;
                let compressed_len = msg.data.len();
                let conn = msg.connection_id;
                let seq = msg.sequence_id;
                let Some(decompressed) = self.decompress_payload(&msg.data, original_len) else {
                    tracing::warn!(
                        "Decompression failed for {conn} — data bytes lost, failing the \
                         connection (strict byte stream; resume will rebuild)"
                    );
                    return InboundOutcome::StreamBroken;
                };
                stats.add_compression_saved(original_len.saturating_sub(compressed_len) as u64);
                let data = Bytes::from(decompressed);
                stats.add_bytes_received(data.len() as u64);
                if keep_base {
                    cache.update_cache_owned(conn, seq, data.clone());
                }
                InboundOutcome::Payload {
                    connection_id: conn,
                    data,
                }
            }
            Frame::IncrementalStreamData(msg) => {
                if !self.may_diff_receive() {
                    tracing::warn!(
                        "IncrementalStreamData for {} but this direction never negotiated diffs, \
                         dropping",
                        msg.connection_id
                    );
                    return InboundOutcome::Dropped;
                }
                self.apply_diff(msg, cache, stats)
            }
            Frame::CompressedIncrementalStreamData(msg) => {
                if !self.may_diff_receive() {
                    tracing::warn!(
                        "CompressedIncrementalStreamData for {} but this direction never \
                         negotiated diffs, dropping",
                        msg.connection_id
                    );
                    return InboundOutcome::Dropped;
                }
                if self.compression.is_none() {
                    tracing::warn!(
                        "CompressedIncrementalStreamData but no compression negotiated, dropping"
                    );
                    return InboundOutcome::Dropped;
                }
                let conn = msg.connection_id;
                let original_len = msg.original_len;
                let compressed_len = msg.data.len();
                let Some(diff_bytes) = self.decompress_payload(&msg.data, original_len) else {
                    tracing::warn!(
                        "Incremental decompression failed for {} — compression stream \
                         desynchronized, failing the connection",
                        conn
                    );
                    return InboundOutcome::StreamBroken;
                };
                stats.add_incremental_compression_saved(
                    original_len.saturating_sub(compressed_len) as u64,
                );
                match IncrementalStreamDataMessage::decode_payload(&diff_bytes) {
                    Ok(msg) => self.apply_diff(msg, cache, stats),
                    Err(e) => {
                        tracing::warn!("Incremental decode failed for {}: {}", conn, e);
                        InboundOutcome::ResyncRequired {
                            connection_id: conn,
                        }
                    }
                }
            }
            other => {
                tracing::warn!("Unexpected data-frame slot: {:?}", other.msg_type());
                InboundOutcome::Dropped
            }
        }
    }

    fn apply_diff(
        &self,
        msg: IncrementalStreamDataMessage,
        cache: &mut ConnectionDataCache,
        stats: &ConnectionStats,
    ) -> InboundOutcome {
        let conn = msg.connection_id;
        let total_len = msg.total_len;
        let Some(data) = cache.apply_incremental(&msg) else {
            tracing::warn!(
                "Incremental data apply failed for {}, requesting full data",
                conn
            );
            return InboundOutcome::ResyncRequired {
                connection_id: conn,
            };
        };
        let saved =
            total_len.saturating_sub(msg.chunks.iter().map(|c| c.data.len()).sum::<usize>());
        stats.add_incremental_saved(saved as u64);
        stats.add_bytes_received(data.len() as u64);
        // apply_incremental already refreshed the cache.
        InboundOutcome::Payload {
            connection_id: conn,
            data,
        }
    }
}

/// Attribute one outbound X11 data frame: wire bytes plus compression /
/// incremental savings relative to the original payload length.
pub fn record_outbound(stats: &ConnectionStats, frame: &Frame, original_len: usize) {
    match frame {
        Frame::StreamData(m) => stats.add_bytes_sent(m.data.len() as u64),
        Frame::CompressedStreamData(m) => {
            stats.add_bytes_sent(m.data.len() as u64);
            stats.add_compression_saved(m.original_len.saturating_sub(m.data.len()) as u64);
        }
        Frame::IncrementalStreamData(m) => {
            let chunks_len: usize = m.chunks.iter().map(|c| c.data.len()).sum();
            stats.add_bytes_sent(chunks_len as u64);
            stats.add_incremental_saved(original_len.saturating_sub(chunks_len) as u64);
        }
        Frame::CompressedIncrementalStreamData(m) => {
            stats.add_bytes_sent(m.data.len() as u64);
            // Two stacked layers: diff-vs-payload, then compression-vs-diff.
            stats.add_incremental_saved(original_len.saturating_sub(m.original_len) as u64);
            stats.add_incremental_compression_saved(
                m.original_len.saturating_sub(m.data.len()) as u64
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Capability, IncrementalChunk};
    use bytes::Bytes;

    fn runtime() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    fn all_features() -> Features {
        Features::from_list(&Capability::advertised())
    }

    fn conn() -> ConnectionId {
        ConnectionId::new(1)
    }

    #[test]
    fn plain_payload_without_compression_yields_data_frame() {
        let dp = DataPlane::new(DataPlaneSide::Relay, Features::default(), None, &runtime());
        let frame = dp.build_frame(conn(), 1, Bytes::from_static(b"hello"), None, None);
        assert!(matches!(frame, Frame::StreamData(_)));
    }

    #[test]
    fn diff_disabled_by_features_even_with_cache() {
        let mut cache = ConnectionDataCache::new();
        let big = vec![0u8; 4096];
        cache.update_cache(conn(), 1, &big);
        let dp = DataPlane::new(DataPlaneSide::Relay, Features::default(), None, &runtime());
        let frame = dp.build_frame(conn(), 2, Bytes::from(big.clone()), Some(&mut cache), None);
        // no Incremental capability ⇒ plain frame, not a diff
        assert!(matches!(frame, Frame::StreamData(_)), "{frame:?}");
    }

    #[test]
    fn diff_produced_when_capability_and_savings_align() {
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        let dp = DataPlane::new(DataPlaneSide::Relay, all_features(), None, &runtime());
        let old = vec![0u8; 2048];
        let mut new = vec![0u8; 2048];
        new[1500..1600].fill(1);
        cache.update_cache(conn(), 1, &old);
        let frame = dp.build_frame(conn(), 2, Bytes::from(new.clone()), Some(&mut cache), None);
        match frame {
            Frame::IncrementalStreamData(m) => {
                assert_eq!(m.base_sequence_id, 1);
                assert_eq!(m.sequence_id, 2);
                assert_eq!(m.chunks.len(), 1);
            }
            other => panic!("expected diff frame, got {other:?}"),
        }
    }

    #[test]
    fn record_outbound_counts_wire_bytes_and_savings() {
        let stats = ConnectionStats::new();
        let frame = Frame::CompressedStreamData(CompressedStreamDataMessage {
            connection_id: conn(),
            sequence_id: 2,
            original_len: 1000,
            data: Bytes::from(vec![0u8; 300]),
        });
        record_outbound(&stats, &frame, 1000);
        let summary = stats.summary();
        assert!(summary.contains("Sent: 300 B"), "{summary}");
        assert!(
            summary.contains("Compressed (full): 1 frames, saved 700 B"),
            "{summary}"
        );
    }

    #[test]
    fn record_outbound_compressed_diff_reports_both_layers() {
        let stats = ConnectionStats::new();
        let frame =
            Frame::CompressedIncrementalStreamData(CompressedIncrementalStreamDataMessage {
                connection_id: conn(),
                sequence_id: 4,
                original_len: 512, // encoded diff
                data: Bytes::from(vec![0u8; 120]),
            });
        record_outbound(&stats, &frame, 4096); // app payload len
        let summary = stats.summary();
        assert!(
            summary.contains("Incremental: 1 frames, saved 3.5 KB"),
            "{summary}"
        );
        assert!(
            summary.contains("Compressed (incremental): 1 frames, saved 392 B"),
            "{summary}"
        );
    }

    #[test]
    fn inbound_plain_updates_cache_and_stats() {
        let dp = DataPlane::new(DataPlaneSide::Relay, all_features(), None, &runtime());
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = dp.apply_inbound(
            Frame::StreamData(StreamDataMessage {
                connection_id: conn(),
                sequence_id: 7,
                data: Bytes::from_static(&[1, 2, 3]),
            }),
            &mut cache,
            &stats,
        );
        assert_eq!(
            outcome,
            InboundOutcome::Payload {
                connection_id: conn(),
                data: Bytes::from_static(&[1, 2, 3])
            }
        );
        assert_eq!(cache.get_cached(conn()).unwrap().0, 7);
        assert!(stats.summary().contains("Recv: 3 B"));
    }

    #[test]
    fn inbound_stale_diff_requests_resync() {
        let dp = DataPlane::new(DataPlaneSide::Relay, all_features(), None, &runtime());
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = dp.apply_inbound(
            Frame::IncrementalStreamData(IncrementalStreamDataMessage {
                connection_id: conn(),
                sequence_id: 2,
                base_sequence_id: 99,
                total_len: 2048,
                chunks: vec![IncrementalChunk {
                    offset: 0,
                    length: 4,
                    data: Bytes::from_static(&[1, 2, 3, 4]),
                }],
            }),
            &mut cache,
            &stats,
        );
        assert_eq!(
            outcome,
            InboundOutcome::ResyncRequired {
                connection_id: conn()
            }
        );
        stats.add_incremental_full_fallback();
        assert!(stats.summary().contains("1 fallbacks"));
    }

    #[test]
    fn inbound_compressed_without_negotiation_is_dropped() {
        let dp = DataPlane::new(DataPlaneSide::Relay, Features::default(), None, &runtime());
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = dp.apply_inbound(
            Frame::CompressedStreamData(CompressedStreamDataMessage {
                connection_id: conn(),
                sequence_id: 1,
                original_len: 8,
                data: Bytes::from_static(&[1, 2, 3, 4]),
            }),
            &mut cache,
            &stats,
        );
        assert_eq!(outcome, InboundOutcome::Dropped);
    }

    fn uplink_features() -> Features {
        // Downlink diffs + resync, but the request stream was never opened:
        // exactly what a relay negotiates with an older client.
        Features::from_list(&["incremental".to_string(), "request_resync".to_string()])
    }

    #[test]
    fn client_plane_without_uplink_cap_never_diffs_send() {
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        let dp = DataPlane::new(DataPlaneSide::Client, uplink_features(), None, &runtime());
        let old = vec![0u8; 2048];
        let mut new = vec![0u8; 2048];
        new[1500..1600].fill(1);
        cache.update_cache(conn(), 1, &old);
        assert!(!dp.may_diff_send());
        let frame = dp.build_frame(conn(), 2, Bytes::from(new), Some(&mut cache), None);
        assert!(matches!(frame, Frame::StreamData(_)), "{frame:?}");
    }

    #[test]
    fn relay_plane_accepts_uplink_diffs_only_when_negotiated() {
        let base = Bytes::from(vec![0u8; 2048]);
        let make_diff = || {
            Frame::IncrementalStreamData(IncrementalStreamDataMessage {
                connection_id: conn(),
                sequence_id: 2,
                base_sequence_id: 1,
                total_len: 2048,
                chunks: vec![crate::protocol::IncrementalChunk {
                    offset: 1500,
                    length: 100,
                    data: Bytes::from(vec![1u8; 100]),
                }],
            })
        };

        // Uplink never negotiated: the shape is refused before it can touch
        // the receive base — a peer must not dictate cache churn.
        let dp = DataPlane::new(DataPlaneSide::Relay, uplink_features(), None, &runtime());
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = dp.apply_inbound(make_diff(), &mut cache, &stats);
        assert_eq!(outcome, InboundOutcome::Dropped);
        // And plain frames are not cached either: no base can form.
        let outcome = dp.apply_inbound(
            Frame::StreamData(StreamDataMessage {
                connection_id: conn(),
                sequence_id: 3,
                data: base.clone(),
            }),
            &mut cache,
            &stats,
        );
        assert!(matches!(outcome, InboundOutcome::Payload { .. }));
        assert_eq!(cache.get_cached(conn()), None);

        // Negotiated: the same diff applies against the base maintained from
        // the plain frame before it.
        let dp = DataPlane::new(DataPlaneSide::Relay, all_features(), None, &runtime());
        let mut cache = ConnectionDataCache::new();
        dp.apply_inbound(
            Frame::StreamData(StreamDataMessage {
                connection_id: conn(),
                sequence_id: 1,
                data: base.clone(),
            }),
            &mut cache,
            &stats,
        );
        let outcome = dp.apply_inbound(make_diff(), &mut cache, &stats);
        match outcome {
            InboundOutcome::Payload { data, .. } => {
                assert_eq!(data.len(), 2048);
                assert_eq!(&data[1500..1600], &[1u8; 100][..]);
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn compressed_uplink_diff_without_negotiation_is_dropped() {
        // The compressed wrapper must meet the SAME direction gate as the
        // plain diff — checked before any decompression happens.
        let dp = DataPlane::new(
            DataPlaneSide::Relay,
            uplink_features(),
            Some(CompressionAlgo::ZstdStream),
            &runtime(),
        );
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = dp.apply_inbound(
            Frame::CompressedIncrementalStreamData(CompressedIncrementalStreamDataMessage {
                connection_id: conn(),
                sequence_id: 2,
                original_len: 64,
                data: Bytes::from_static(&[0x99; 32]),
            }),
            &mut cache,
            &stats,
        );
        assert_eq!(outcome, InboundOutcome::Dropped);
    }

    /// Streaming pair helper: sender and receiver DataPlanes that live for
    /// the whole "connection" — the same lifetime relationship the halves
    /// have in the real topology (one DataPlane per direction, per session).
    fn stream_pair() -> (DataPlane, DataPlane) {
        let rt = runtime();
        (
            DataPlane::new(
                DataPlaneSide::Relay,
                Features::default(),
                Some(CompressionAlgo::ZstdStream),
                &rt,
            ),
            DataPlane::new(
                DataPlaneSide::Client,
                all_features(),
                Some(CompressionAlgo::ZstdStream),
                &rt,
            ),
        )
    }

    fn push_one(sender: &DataPlane, receiver: &DataPlane, payload: &[u8]) {
        let frame = sender.build_frame(conn(), 1, Bytes::from(payload.to_vec()), None, None);
        let compressed = match frame {
            Frame::CompressedStreamData(m) => m,
            other => panic!("expected compressed frame, got {other:?}"),
        };
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        match receiver.apply_inbound(
            Frame::CompressedStreamData(compressed.clone()),
            &mut cache,
            &stats,
        ) {
            InboundOutcome::Payload { data, .. } => assert_eq!(&data[..], payload),
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    #[test]
    fn zstd_stream_round_trips_many_frames() {
        let (sender, receiver) = stream_pair();
        // Vary sizes and content; each frame must decode to itself through
        // the continuous stream (per-frame flush semantics).
        for i in 0..50u8 {
            let payload: Vec<u8> = (0..500 + i as usize * 137)
                .map(|j| (j % 251) as u8 ^ i)
                .collect();
            push_one(&sender, &receiver, &payload);
        }
        // A below-threshold payload rides plain, then bigger ones resume
        // streaming on the same contexts.
        let frame = sender.build_frame(conn(), 1, Bytes::from(vec![7u8; 8]), None, None);
        assert!(matches!(frame, Frame::StreamData(_)));
        push_one(&sender, &receiver, &[42u8; 4096]);
    }

    #[test]
    fn zstd_stream_beats_per_frame_compression_on_repeats() {
        // X11 redraw pattern: a mostly-unchanged high-entropy region
        // (photo/desktop pixels) with a small changed stripe per frame.
        // One-shot compression must re-encode the whole region every time;
        // the stream finds it in its window history and ships the delta.
        fn mix(mut s: u64) -> u8 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 40) as u8
        }
        let base: Vec<u8> = (0..8192u64).map(mix).collect();
        let frames: Vec<Vec<u8>> = (0..32)
            .map(|i| {
                let mut v = base.clone();
                let off = (i * 256) % (base.len() - 256);
                v[off..off + 256].fill(i as u8);
                v
            })
            .collect();
        // Direct wire-byte comparison: sum the payload each mode emits.
        let rt = runtime();
        let sum = |algo: CompressionAlgo| -> usize {
            let dp = DataPlane::new(DataPlaneSide::Relay, Features::default(), Some(algo), &rt);
            frames
                .iter()
                .map(
                    |f| match dp.build_frame(conn(), 1, Bytes::from(f.clone()), None, None) {
                        Frame::CompressedStreamData(m) => m.data.len(),
                        Frame::StreamData(m) => m.data.len(),
                        other => panic!("{other:?}"),
                    },
                )
                .sum()
        };
        let one_shot = sum(CompressionAlgo::Zstd);
        let stream = sum(CompressionAlgo::ZstdStream);
        assert!(
            stream < one_shot / 4,
            "stream {stream} should beat one-shot {one_shot} decisively on redraw-like repeats"
        );
    }

    #[test]
    fn zstd_stream_corruption_reports_stream_broken() {
        let (_sender, receiver) = stream_pair();
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = receiver.apply_inbound(
            Frame::CompressedStreamData(CompressedStreamDataMessage {
                connection_id: conn(),
                sequence_id: 1,
                original_len: 4096,
                // Not a zstd frame fragment at all.
                data: Bytes::from_static(b"garbage-bytes-that-are-not-zstd"),
            }),
            &mut cache,
            &stats,
        );
        assert_eq!(outcome, InboundOutcome::StreamBroken);
    }

    #[test]
    fn one_shot_decompression_failure_also_breaks_the_stream() {
        // Byte-stream truth does not care which mode: lost bytes are lost.
        let rt = runtime();
        let dp = DataPlane::new(
            DataPlaneSide::Client,
            all_features(),
            Some(CompressionAlgo::Zstd),
            &rt,
        );
        let mut cache = ConnectionDataCache::new();
        let stats = ConnectionStats::new();
        let outcome = dp.apply_inbound(
            Frame::CompressedStreamData(CompressedStreamDataMessage {
                connection_id: conn(),
                sequence_id: 1,
                original_len: 100,
                data: Bytes::from_static(&[0xFF; 32]),
            }),
            &mut cache,
            &stats,
        );
        assert_eq!(outcome, InboundOutcome::StreamBroken);
    }
}
