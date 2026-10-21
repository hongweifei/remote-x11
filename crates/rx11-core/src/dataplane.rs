//! Data-plane policy chain: how a raw X11 payload becomes a frame, and how
//! an inbound data frame becomes payload again.
//!
//! Both ends build a [`DataPlane`] from the negotiated [`Features`] +
//! [`RuntimeConfig`], so:
//! - diff/compression decisions are **capability-gated**, not guessed from
//!   whatever frame arrives;
//! - savings accounting happens at one place (frame construction), keeping
//!   server and client stats semantics symmetric;
//! - adding a new data-plane stage (e.g. a future retransmit window or
//!   flow-control hook) means extending this module, not the relay loops.

use std::sync::Mutex;

use bytes::Bytes;

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
}

impl std::fmt::Debug for ZstdContexts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZstdContexts")
            .field("pooled", &true)
            .finish()
    }
}

/// Negotiated data-plane configuration for one relay session.
#[derive(Debug, Clone)]
pub struct DataPlane {
    pub features: Features,
    pub compression: Option<CompressionAlgo>,
    pub compression_threshold: usize,
    zstd: std::sync::Arc<ZstdContexts>,
}

impl DataPlane {
    pub fn new(
        features: Features,
        compression: Option<CompressionAlgo>,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self {
            features,
            compression,
            compression_threshold: runtime.compression_threshold,
            zstd: std::sync::Arc::new(ZstdContexts::default()),
        }
    }

    fn compress(&self, data: &[u8]) -> Option<Bytes> {
        let algo = self.compression?;
        if data.len() < self.compression_threshold {
            return None;
        }
        // zstd runs through the pooled context; other algorithms use the
        // stateless codec (their setup cost is negligible).
        let compressed = match algo {
            CompressionAlgo::Zstd => self.zstd_compress(data)?,
            other => other.compress_with(data, 0)?,
        };
        if compressed.len() >= data.len() {
            return None;
        }
        Some(compressed)
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
        let frame = match (self.features.supports(Capability::Incremental), cache) {
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
    pub fn apply_inbound(
        &self,
        frame: Frame,
        cache: &mut ConnectionDataCache,
        stats: &ConnectionStats,
    ) -> InboundOutcome {
        match frame {
            Frame::StreamData(msg) => {
                stats.add_bytes_received(msg.data.len() as u64);
                cache.update_cache_owned(msg.connection_id, msg.sequence_id, msg.data.clone());
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
                    tracing::warn!("Decompression failed for {}, dropping frame", conn);
                    return InboundOutcome::Dropped;
                };
                stats.add_compression_saved(original_len.saturating_sub(compressed_len) as u64);
                let data = Bytes::from(decompressed);
                stats.add_bytes_received(data.len() as u64);
                cache.update_cache_owned(conn, seq, data.clone());
                InboundOutcome::Payload {
                    connection_id: conn,
                    data,
                }
            }
            Frame::IncrementalStreamData(msg) => self.apply_diff(msg, cache, stats),
            Frame::CompressedIncrementalStreamData(msg) => {
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
                        "Incremental decompression failed for {}, dropping frame",
                        conn
                    );
                    return InboundOutcome::ResyncRequired {
                        connection_id: conn,
                    };
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
        let dp = DataPlane::new(Features::default(), None, &runtime());
        let frame = dp.build_frame(conn(), 1, Bytes::from_static(b"hello"), None, None);
        assert!(matches!(frame, Frame::StreamData(_)));
    }

    #[test]
    fn diff_disabled_by_features_even_with_cache() {
        let mut cache = ConnectionDataCache::new();
        let big = vec![0u8; 4096];
        cache.update_cache(conn(), 1, &big);
        let dp = DataPlane::new(Features::default(), None, &runtime());
        let frame = dp.build_frame(conn(), 2, Bytes::from(big.clone()), Some(&mut cache), None);
        // no Incremental capability ⇒ plain frame, not a diff
        assert!(matches!(frame, Frame::StreamData(_)), "{frame:?}");
    }

    #[test]
    fn diff_produced_when_capability_and_savings_align() {
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        let dp = DataPlane::new(all_features(), None, &runtime());
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
        let dp = DataPlane::new(all_features(), None, &runtime());
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
        let dp = DataPlane::new(all_features(), None, &runtime());
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
        let dp = DataPlane::new(Features::default(), None, &runtime());
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
}
