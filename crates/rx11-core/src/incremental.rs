use crate::protocol::{IncrementalChunk, IncrementalX11DataMessage};
use crate::types::ConnectionId;
use bytes::Bytes;
use std::collections::HashMap;

pub const DEFAULT_CHUNK_SIZE: usize = 4096;
pub const MIN_BLOCK_SIZE_FOR_DIFF: usize = 1024;

pub struct ConnectionDataCache {
    last_data: HashMap<ConnectionId, CachedData>,
    chunk_size: usize,
}

struct CachedData {
    sequence_id: u32,
    data: Vec<u8>,
}

impl ConnectionDataCache {
    pub fn new() -> Self {
        Self::with_chunk_size(DEFAULT_CHUNK_SIZE)
    }

    pub fn with_chunk_size(chunk_size: usize) -> Self {
        Self {
            last_data: HashMap::new(),
            chunk_size,
        }
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

    pub fn compute_incremental(
        &self,
        connection_id: ConnectionId,
        new_sequence_id: u32,
        new_data: &[u8],
    ) -> Option<IncrementalX11DataMessage> {
        let (base_sequence_id, old_data) = self.get_cached(connection_id)?;

        if new_data.len() < MIN_BLOCK_SIZE_FOR_DIFF {
            return None;
        }

        let chunks = self.compute_diff_chunks(old_data, new_data);

        let saved_bytes = old_data
            .len()
            .saturating_sub(chunks.iter().map(|c| c.data.len()).sum::<usize>());

        if saved_bytes < old_data.len() / 10 {
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
            let chunk_end = (offset + self.chunk_size).min(new_data.len());
            let new_chunk = &new_data[offset..chunk_end];

            let old_chunk = if offset < old_data.len() {
                let old_end = (offset + self.chunk_size).min(old_data.len());
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
