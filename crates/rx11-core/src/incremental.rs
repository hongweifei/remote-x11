use crate::config::{IncrementalDefaults, RuntimeConfig};
use crate::protocol::{IncrementalChunk, IncrementalX11DataMessage};
use crate::types::ConnectionId;
use bytes::Bytes;
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
    data: Vec<u8>,
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
        self.last_data.insert(
            connection_id,
            CachedData {
                sequence_id,
                data: data.to_vec(),
            },
        );
    }

    pub fn get_cached(&self, connection_id: ConnectionId) -> Option<(u32, &[u8])> {
        self.last_data
            .get(&connection_id)
            .map(|cached| (cached.sequence_id, cached.data.as_slice()))
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
    ) -> Option<IncrementalX11DataMessage> {
        let (base_sequence_id, old_data) = self.get_cached(connection_id)?;

        if new_data.len() < self.params.min_block_size {
            return None;
        }

        let chunks = self.compute_diff_chunks(old_data, new_data);

        // Identical payload: an empty diff relies on the receiver's cache
        // staying in sync; fall back to a full (compressible) frame instead.
        if chunks.is_empty() {
            return None;
        }

        let saved_bytes = old_data
            .len()
            .saturating_sub(chunks.iter().map(|c| c.data.len()).sum::<usize>());

        // Only send a diff when it saves a meaningful fraction of the base
        // frame; the floor matches the previous `len() / 10` integer rule.
        let min_saved = (old_data.len() as f64 * self.params.min_savings_ratio) as usize;
        if saved_bytes < min_saved {
            return None;
        }

        Some(IncrementalX11DataMessage {
            connection_id,
            sequence_id: new_sequence_id,
            base_sequence_id,
            total_len: new_data.len(),
            chunks,
        })
    }

    fn compute_diff_chunks(&self, old_data: &[u8], new_data: &[u8]) -> Vec<IncrementalChunk> {
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
                    data: Bytes::copy_from_slice(new_chunk),
                });
            }

            offset = chunk_end;
        }

        chunks
    }

    pub fn apply_incremental(&mut self, msg: &IncrementalX11DataMessage) -> Option<Vec<u8>> {
        let (cached_seq, cached_data) = self.get_cached(msg.connection_id)?;

        if cached_seq != msg.base_sequence_id {
            return None;
        }

        let mut result = vec![0u8; msg.total_len];

        if cached_data.len() <= msg.total_len {
            result[..cached_data.len()].copy_from_slice(cached_data);
        } else {
            result.copy_from_slice(&cached_data[..msg.total_len]);
        }

        for chunk in &msg.chunks {
            let end = chunk.offset + chunk.length;
            if end <= msg.total_len {
                result[chunk.offset..end].copy_from_slice(&chunk.data);
            }
        }

        self.update_cache(msg.connection_id, msg.sequence_id, &result);

        Some(result)
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
    fn test_compute_incremental_identical() {
        let mut cache = ConnectionDataCache::new();
        let conn_id = ConnectionId::new(1);
        let data = vec![0u8; 2048];

        cache.update_cache(conn_id, 1, &data);
        let result = cache.compute_incremental(conn_id, 2, &data);

        assert!(result.is_none());
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
