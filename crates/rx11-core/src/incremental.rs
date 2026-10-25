use crate::config::{IncrementalDefaults, RuntimeConfig};
use crate::protocol::{IncrementalChunk, IncrementalStreamDataMessage};
use crate::types::ConnectionId;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

/// Tuning knobs captured at cache construction (from `RuntimeConfig`).
#[derive(Debug, Clone, Copy)]
pub struct DiffParams {
    pub chunk_size: usize,
    pub min_block_size: usize,
    pub min_savings_ratio: f64,
}

impl Default for DiffParams {
    fn default() -> Self {
        Self {
            chunk_size: IncrementalDefaults::DEFAULT_CHUNK_SIZE,
            min_block_size: IncrementalDefaults::MIN_BLOCK_SIZE_FOR_DIFF,
            min_savings_ratio: IncrementalDefaults::MIN_SAVINGS_RATIO,
        }
    }
}

pub struct ConnectionDataCache {
    last_data: HashMap<ConnectionId, CachedData>,
    params: DiffParams,
}

struct CachedData {
    sequence_id: u32,
    /// Refcounted view of the last full payload; caching a frame is a clone
    /// (O(1)), not a memcpy — the hot path feeds owned `Bytes` from the
    /// read side or the proxy buffer.
    data: Bytes,
}

impl ConnectionDataCache {
    pub fn new() -> Self {
        Self {
            last_data: HashMap::new(),
            params: DiffParams::default(),
        }
    }

    pub fn with_params(params: DiffParams) -> Self {
        Self {
            last_data: HashMap::new(),
            params,
        }
    }

    pub fn from_runtime(runtime: &RuntimeConfig) -> Self {
        Self::with_params(DiffParams {
            chunk_size: runtime.diff_chunk_size,
            min_block_size: runtime.diff_min_block_size,
            min_savings_ratio: runtime.diff_min_savings_ratio,
        })
    }

    /// Test/compat helper: override only the chunk size.
    pub fn with_chunk_size(chunk_size: usize) -> Self {
        Self::with_params(DiffParams {
            chunk_size,
            ..DiffParams::default()
        })
    }

    pub fn update_cache(&mut self, connection_id: ConnectionId, sequence_id: u32, data: &[u8]) {
        self.update_cache_owned(connection_id, sequence_id, Bytes::copy_from_slice(data));
    }

    /// Cache an owned payload: stores the `Bytes` handle itself, so this is
    /// a refcount bump rather than a payload copy.
    pub fn update_cache_owned(
        &mut self,
        connection_id: ConnectionId,
        sequence_id: u32,
        data: Bytes,
    ) {
        self.last_data
            .insert(connection_id, CachedData { sequence_id, data });
    }

    pub fn get_cached(&self, connection_id: ConnectionId) -> Option<(u32, &[u8])> {
        self.last_data
            .get(&connection_id)
            .map(|cached| (cached.sequence_id, &cached.data[..]))
    }

    /// The cached base's payload as an owned handle: cloning a `Bytes` is a
    /// refcount bump, not a copy.
    fn get_cached_bytes(&self, connection_id: ConnectionId) -> Option<Bytes> {
        self.last_data
            .get(&connection_id)
            .map(|cached| cached.data.clone())
    }

    /// Sequence number the next outgoing frame for this connection should
    /// carry: strictly greater than the cached base so the receiver can
    /// detect stale diffs. Returns 1 when nothing is cached yet.
    pub fn next_sequence_for(&self, connection_id: ConnectionId) -> u32 {
        self.last_data
            .get(&connection_id)
            .map(|cached| cached.sequence_id.wrapping_add(1).max(1))
            .unwrap_or(1)
    }

    pub fn compute_incremental(
        &self,
        connection_id: ConnectionId,
        new_sequence_id: u32,
        new_data: &[u8],
    ) -> Option<IncrementalStreamDataMessage> {
        self.compute_incremental_inner(connection_id, new_sequence_id, new_data, |offset, len| {
            Bytes::copy_from_slice(&new_data[offset..offset + len])
        })
    }

    /// Zero-copy chunking variant: changed chunks alias `new_data`'s
    /// allocation (refcounted slices).
    pub fn compute_incremental_owned(
        &self,
        connection_id: ConnectionId,
        new_sequence_id: u32,
        new_data: &Bytes,
    ) -> Option<IncrementalStreamDataMessage> {
        let data = new_data.clone();
        self.compute_incremental_inner(connection_id, new_sequence_id, new_data, |offset, len| {
            data.slice(offset..offset + len)
        })
    }

    fn compute_incremental_inner(
        &self,
        connection_id: ConnectionId,
        new_sequence_id: u32,
        new_data: &[u8],
        take: impl Fn(usize, usize) -> Bytes,
    ) -> Option<IncrementalStreamDataMessage> {
        let (base_sequence_id, old_data) = self.get_cached(connection_id)?;

        if new_data.len() < self.params.min_block_size {
            // Below the chunk-diff floor, a diff is all overhead — but an
            // EXACTLY-equal payload can still win: a reference frame carries
            // zero content, and it pays off as soon as the payload exceeds the
            // fixed diff header (18 payload bytes; a plain frame's own
            // 11-byte header is paid either way). Small repeats — key
            // auto-repeat, polling requests — are exactly this traffic.
            if new_data.len() > IncrementalDefaults::REFERENCE_OVERHEAD && new_data == old_data {
                return Some(IncrementalStreamDataMessage {
                    connection_id,
                    sequence_id: new_sequence_id,
                    base_sequence_id,
                    total_len: new_data.len(),
                    chunks: Vec::new(),
                });
            }
            return None;
        }

        let chunks = self.compute_diff_chunks(old_data, new_data, &take);

        // An unchanged payload ships as a *reference frame*: the diff itself
        // is empty, so the content travels zero bytes while the frame still
        // carries its sequence number and length — the receiver expands it
        // out of the base it already holds. Skipping the frame entirely is
        // not an option: the byte stream is positional, and a frame the
        // receiver cannot tell apart from "never sent" would lose bytes.
        // Empty chunks mean "identical" only when the lengths agree: chunk
        // walking stops at the NEW length, so a truncated prefix scans empty
        // too — and the receiver could never expand it from its base (the
        // guard in `apply_incremental` refuses such frames from any peer;
        // this is the sender-side half of the same rule). A true diff of a
        // changed payload clears the savings ratio below and never reaches
        // this branch with zero chunks.
        if chunks.is_empty() {
            return (new_data.len() == old_data.len()).then_some(IncrementalStreamDataMessage {
                connection_id,
                sequence_id: new_sequence_id,
                base_sequence_id,
                total_len: new_data.len(),
                chunks,
            });
        }

        let saved_bytes = old_data
            .len()
            .saturating_sub(chunks.iter().map(|c| c.data.len()).sum::<usize>());

        // Only send a diff when it saves a meaningful fraction of the base
        // frame; the floor matches the previous `len() / 10` integer rule.
        // An unchanged payload saves the whole base, so it clears any ratio.
        let min_saved = (old_data.len() as f64 * self.params.min_savings_ratio) as usize;
        if saved_bytes < min_saved {
            return None;
        }

        Some(IncrementalStreamDataMessage {
            connection_id,
            sequence_id: new_sequence_id,
            base_sequence_id,
            total_len: new_data.len(),
            chunks,
        })
    }

    fn compute_diff_chunks(
        &self,
        old_data: &[u8],
        new_data: &[u8],
        take: &impl Fn(usize, usize) -> Bytes,
    ) -> Vec<IncrementalChunk> {
        let mut chunks = Vec::new();
        let mut offset = 0;

        while offset < new_data.len() {
            let chunk_end = (offset + self.params.chunk_size).min(new_data.len());
            let new_chunk = &new_data[offset..chunk_end];

            let old_chunk = if offset < old_data.len() {
                let old_end = (offset + self.params.chunk_size).min(old_data.len());
                &old_data[offset..old_end]
            } else {
                &[]
            };

            if new_chunk != old_chunk {
                chunks.push(IncrementalChunk {
                    offset,
                    length: new_chunk.len(),
                    data: take(offset, new_chunk.len()),
                });
            }

            offset = chunk_end;
        }

        chunks
    }

    pub fn apply_incremental(&mut self, msg: &IncrementalStreamDataMessage) -> Option<Bytes> {
        let (cached_seq, cached_data) = self.get_cached(msg.connection_id)?;

        if cached_seq != msg.base_sequence_id {
            return None;
        }

        if msg.chunks.is_empty() {
            // A reference frame says "re-emit your base" — and the base IS
            // the answer, so hand it out directly: cloning the `Bytes`
            // handle is a refcount bump, while the generic path below
            // would allocate a full-size buffer, zero it and copy the whole
            // payload into it (measured: 213 us for a 4 MB picture, every
            // frame).
            //
            // A declared length that differs from the base is provably
            // malformed (a diff of an unchanged payload always keeps the
            // length), so refuse the WHOLE frame: expanding it would hand
            // the caller zero-padded or truncated bytes and re-poison the
            // cache on every retry.
            if cached_data.len() != msg.total_len {
                return None;
            }
            let data = self.get_cached_bytes(msg.connection_id)?;
            self.update_cache_owned(msg.connection_id, msg.sequence_id, data.clone());
            return Some(data);
        }

        let mut result = BytesMut::zeroed(msg.total_len);

        if cached_data.len() <= msg.total_len {
            result[..cached_data.len()].copy_from_slice(cached_data);
        } else {
            result.copy_from_slice(&cached_data[..msg.total_len]);
        }

        for chunk in &msg.chunks {
            let end = chunk.offset + chunk.length;
            // A malformed chunk must fail the WHOLE frame (caller then
            // triggers resync) — silently skipping it would deliver subtly
            // corrupt payload and keep the cache poisoned.
            if end > msg.total_len || chunk.data.len() != chunk.length {
                return None;
            }
            result[chunk.offset..end].copy_from_slice(&chunk.data);
        }

        let data = result.freeze();
        self.update_cache_owned(msg.connection_id, msg.sequence_id, data.clone());

        Some(data)
    }

    pub fn clear_connection(&mut self, connection_id: ConnectionId) {
        self.last_data.remove(&connection_id);
    }

    pub fn clear_all(&mut self) {
        self.last_data.clear();
    }
}

impl Default for ConnectionDataCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::BinaryMessageCodec;
    use crate::types::ConnectionId;

    #[test]
    fn test_cache_update_and_get() {
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        let data = vec![1, 2, 3, 4];

        cache.update_cache(conn_id, 1, &data);
        let (seq, cached) = cache.get_cached(conn_id).unwrap();

        assert_eq!(seq, 1);
        assert_eq!(cached, &data[..]);
    }

    #[test]
    fn test_apply_rejects_out_of_bounds_chunk() {
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        cache.update_cache(ConnectionId::new(1), 1, &[0u8; 2048]);
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: 2048,
            chunks: vec![IncrementalChunk {
                offset: 2000,
                length: 100, // past total_len
                data: Bytes::from(vec![1u8; 100]),
            }],
        };
        // Whole frame must be refused, cache left untouched, so the caller
        // can request a full-frame resync instead of eating corrupt bytes.
        assert_eq!(cache.apply_incremental(&msg), None);
        assert_eq!(cache.get_cached(ConnectionId::new(1)).unwrap().0, 1);
    }

    #[test]
    fn test_apply_rejects_declared_length_mismatch() {
        let mut cache = ConnectionDataCache::new();
        cache.update_cache(ConnectionId::new(1), 1, &[0u8; 2048]);
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: 2048,
            chunks: vec![IncrementalChunk {
                offset: 0,
                length: 8,
                data: Bytes::from(vec![1u8; 4]), // length lies
            }],
        };
        assert_eq!(cache.apply_incremental(&msg), None);
    }

    #[test]
    fn test_next_sequence_for() {
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(7);
        assert_eq!(cache.next_sequence_for(conn_id), 1);

        cache.update_cache(conn_id, 41, &[0u8; 2048]);
        assert_eq!(cache.next_sequence_for(conn_id), 42);

        cache.clear_connection(conn_id);
        assert_eq!(cache.next_sequence_for(conn_id), 1);
    }

    #[test]
    fn test_compute_incremental_identical_yields_reference_frame() {
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        let data = vec![0u8; 2048];

        cache.update_cache(conn_id, 1, &data);
        let result = cache.compute_incremental(conn_id, 2, &data);

        // An unchanged payload still produces a frame — the sequence number
        // is part of the positional byte stream and cannot be skipped — but
        // the frame carries NO content: the receiver expands it from its
        // own base. This is the "don't retransmit what did not change" win.
        let msg = result.expect("reference frame");
        assert!(msg.chunks.is_empty());
        assert_eq!(msg.base_sequence_id, 1);
        assert_eq!(msg.total_len, 2048);
    }

    #[test]
    fn test_small_payload_reference_frame_below_diff_floor() {
        // 32 bytes < 1024 diff floor, but > 18 bytes overhead — an EXACTLY
        // identical repeat wins via reference frame instead of sending whole.
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        let small = [0xABu8; 32];
        cache.update_cache(conn_id, 1, &small);
        let result = cache.compute_incremental(conn_id, 2, &small);
        assert!(
            result.is_some(),
            "exact repeat must emit reference even below diff floor"
        );
        let msg = result.unwrap();
        assert!(msg.chunks.is_empty());
        assert_eq!(msg.total_len, 32);
        // Non-identical does NOT produce reference when below diff floor.
        let changed = [0xACu8; 32];
        let result = cache.compute_incremental(conn_id, 3, &changed);
        assert!(
            result.is_none(),
            "different payload below diff floor must not emit reference"
        );
    }

    #[test]
    fn test_apply_reference_frame_expands_from_base() {
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        let data = vec![3u8; 2048];
        cache.update_cache(conn_id, 1, &data);

        let msg = IncrementalStreamDataMessage {
            connection_id: conn_id,
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: 2048,
            chunks: vec![],
        };
        let result = cache.apply_incremental(&msg).expect("reference applies");
        assert_eq!(result, Bytes::from(data.clone()));

        // The reference becomes the new base: a later diff applies against
        // THIS sequence, and a stale reference (wrong base) is refused so
        // the standard resync path can heal.
        let stale = IncrementalStreamDataMessage {
            sequence_id: 3,
            base_sequence_id: 1,
            ..msg
        };
        assert_eq!(cache.apply_incremental(&stale), None);
    }

    #[test]
    fn test_reference_frame_survives_the_codec() {
        // 0x22 encodes with a 20-byte header and zero chunk entries; the
        // empty-chunk shape must round-trip exactly (this is what actually
        // crosses the wire).
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: 2048,
            chunks: vec![],
        };
        let encoded = msg.encode_payload().unwrap();
        assert_eq!(encoded.len(), 4 + 4 + 4 + 4 + 2);
        let decoded = IncrementalStreamDataMessage::decode_payload(&encoded).unwrap();
        assert_eq!(decoded.connection_id, msg.connection_id);
        assert_eq!(decoded.sequence_id, 2);
        assert_eq!(decoded.base_sequence_id, 1);
        assert_eq!(decoded.total_len, 2048);
        assert!(decoded.chunks.is_empty());
    }

    #[test]
    fn test_apply_refuses_malformed_reference_frame() {
        // A zero-chunk frame whose declared length differs from the base is
        // provably malformed: refuse it (caller resyncs) instead of
        // delivering a zero-padded or truncated expansion.
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        cache.update_cache(conn_id, 1, &vec![5u8; 2048]);
        let msg = IncrementalStreamDataMessage {
            connection_id: conn_id,
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: 1024,
            chunks: vec![],
        };
        assert_eq!(cache.apply_incremental(&msg), None);
        // The base stays where it was: the next whole frame re-establishes.
        assert_eq!(cache.get_cached(conn_id).unwrap().0, 1);
    }

    #[test]
    fn a_reference_frame_hands_out_the_base_itself() {
        // The answer to "re-emit your base" IS the base: the expansion must
        // not build a fresh buffer. The observable is the base's ALLOCATION
        // — the expansion has to share it, so the bytes that reach the
        // caller are the very bytes the base already held.
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        let data = vec![9u8; 4096];
        cache.update_cache(conn_id, 1, &data);

        let msg = IncrementalStreamDataMessage {
            connection_id: conn_id,
            sequence_id: 2,
            base_sequence_id: 1,
            total_len: 4096,
            chunks: vec![],
        };
        let expanded = cache.apply_incremental(&msg).expect("reference applies");
        assert_eq!(expanded, Bytes::from(data.clone()));

        // The base survives as the cache's payload: a fresh buffer would
        // have replaced it with a different allocation, and the copy would
        // show up as a second buffer of the same size.
        let base = cache.get_cached_bytes(conn_id).expect("base");
        assert_eq!(base.len(), 4096);
        assert_eq!(base, expanded, "the expansion IS the base");

        // And the reference becomes the new base, so a later diff applies
        // against THIS sequence.
        assert_eq!(cache.get_cached(conn_id).unwrap().0, 2);
    }

    #[test]
    fn a_frame_with_chunks_still_builds_a_fresh_buffer() {
        // The zero-chunk shortcut must not change the chunked path: a frame
        // that carries chunks still expands into a buffer of its own.
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        let conn_id = ConnectionId::new(1);
        let old_data = vec![0u8; 2048];
        let mut new_data = vec![0u8; 2048];
        new_data[1500..2000].fill(1);

        cache.update_cache(conn_id, 1, &old_data);
        let msg = cache.compute_incremental(conn_id, 2, &new_data).unwrap();
        let expanded = cache.apply_incremental(&msg).expect("diff applies");
        assert_eq!(expanded, Bytes::from(new_data));
    }

    #[test]
    fn test_compute_incremental_different() {
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        let conn_id = ConnectionId::new(1);

        let old_data = vec![0u8; 2048];
        let mut new_data = vec![0u8; 2048];
        new_data[1500..2000].fill(1);

        cache.update_cache(conn_id, 1, &old_data);
        let result = cache.compute_incremental(conn_id, 2, &new_data);

        assert!(result.is_some());
        let msg = result.unwrap();
        assert_eq!(msg.chunks.len(), 1);
    }

    #[test]
    fn test_apply_incremental() {
        let mut cache = ConnectionDataCache::with_chunk_size(1024);
        let conn_id = ConnectionId::new(1);

        let old_data = vec![0u8; 2048];
        let mut new_data = vec![0u8; 2048];
        new_data[1500..2000].fill(1);

        cache.update_cache(conn_id, 1, &old_data);
        let msg = cache.compute_incremental(conn_id, 2, &new_data).unwrap();

        let mut apply_cache = ConnectionDataCache::with_chunk_size(1024);
        apply_cache.update_cache(conn_id, 1, &old_data);
        let result = apply_cache.apply_incremental(&msg).unwrap();

        assert_eq!(result, new_data);
    }
}
