use std::io::{Read, Write};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const COMPRESSION_THRESHOLD: usize = 256;

const ZSTD_COMPRESSION_LEVEL: i32 = 3;
const ZLIB_COMPRESSION_LEVEL: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[non_exhaustive]
pub enum CompressionAlgo {
    Zstd = 0,
    Lz4 = 1,
    Zlib = 2,
}

impl CompressionAlgo {
    pub const ALL: [CompressionAlgo; 3] = [
        CompressionAlgo::Zstd,
        CompressionAlgo::Lz4,
        CompressionAlgo::Zlib,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            CompressionAlgo::Zstd => "zstd",
            CompressionAlgo::Lz4 => "lz4",
            CompressionAlgo::Zlib => "zlib",
        }
    }

    pub fn compress(&self, data: &[u8]) -> Option<Vec<u8>> {
        self.compress_to_bytes(data).map(|b| b.to_vec())
    }

    pub fn compress_to_bytes(&self, data: &[u8]) -> Option<Bytes> {
        if data.len() < COMPRESSION_THRESHOLD {
            return None;
        }
        let compressed = match self {
            CompressionAlgo::Zstd => compress_zstd(data)?,
            CompressionAlgo::Lz4 => compress_lz4(data)?,
            CompressionAlgo::Zlib => compress_zlib(data)?,
        };
        if compressed.len() >= data.len() {
            return None;
        }
        Some(compressed)
    }

    pub fn decompress(&self, compressed: &[u8], original_len: usize) -> Option<Vec<u8>> {
        let result = match self {
            CompressionAlgo::Zstd => decompress_zstd(compressed)?,
            CompressionAlgo::Lz4 => decompress_lz4(compressed)?,
            CompressionAlgo::Zlib => decompress_zlib(compressed, original_len)?,
        };
        if result.len() != original_len {
            return None;
        }
        Some(result)
    }

    pub fn negotiate(
        client_algos: &[CompressionAlgo],
        server_algos: &[CompressionAlgo],
    ) -> Option<CompressionAlgo> {
        for algo in &CompressionAlgo::ALL {
            if client_algos.contains(algo) && server_algos.contains(algo) {
                return Some(*algo);
            }
        }
        None
    }
}

fn compress_zstd(data: &[u8]) -> Option<Bytes> {
    zstd::encode_all(data, ZSTD_COMPRESSION_LEVEL)
        .ok()
        .map(Bytes::from)
}

fn decompress_zstd(compressed: &[u8]) -> Option<Vec<u8>> {
    zstd::decode_all(compressed).ok()
}

pub fn maybe_compress_frame(
    connection_id: crate::types::ConnectionId,
    sequence_id: u32,
    data: Bytes,
    compression_algo: Option<CompressionAlgo>,
) -> crate::protocol::Frame {
    use crate::protocol::{CompressedX11DataMessage, Frame, X11DataMessage};

    if let Some(algo) = compression_algo {
        if data.len() >= COMPRESSION_THRESHOLD {
            if let Some(compressed) = algo.compress_to_bytes(&data) {
                return Frame::CompressedDataX11(CompressedX11DataMessage {
                    connection_id,
                    sequence_id,
                    original_len: data.len(),
                    data: compressed,
                });
            }
        }
    }
    Frame::DataX11(X11DataMessage {
        connection_id,
        sequence_id,
        data,
    })
}

pub fn decompress_frame_data(
    msg: &crate::protocol::CompressedX11DataMessage,
    algo: CompressionAlgo,
) -> Option<Vec<u8>> {
    algo.decompress(&msg.data, msg.original_len)
}

pub fn maybe_incremental_or_compress_frame(
    connection_id: crate::types::ConnectionId,
    sequence_id: u32,
    data: Bytes,
    compression_algo: Option<CompressionAlgo>,
    incremental_cache: Option<&mut crate::incremental::ConnectionDataCache>,
) -> crate::protocol::Frame {
    use crate::protocol::{CompressedX11DataMessage, Frame, X11DataMessage};

    if let Some(cache) = incremental_cache {
        if let Some(incremental_msg) = cache.compute_incremental(connection_id, sequence_id, &data)
        {
            cache.update_cache(connection_id, sequence_id, &data);
            return Frame::IncrementalDataX11(incremental_msg);
        }
        cache.update_cache(connection_id, sequence_id, &data);
    }

    if let Some(algo) = compression_algo {
        if data.len() >= COMPRESSION_THRESHOLD {
            if let Some(compressed) = algo.compress_to_bytes(&data) {
                return Frame::CompressedDataX11(CompressedX11DataMessage {
                    connection_id,
                    sequence_id,
                    original_len: data.len(),
                    data: compressed,
                });
            }
        }
    }
    Frame::DataX11(X11DataMessage {
        connection_id,
        sequence_id,
        data,
    })
}

fn compress_lz4(data: &[u8]) -> Option<Bytes> {
    Some(Bytes::from(lz4_flex::compress_prepend_size(data)))
}

fn decompress_lz4(compressed: &[u8]) -> Option<Vec<u8>> {
    lz4_flex::decompress_size_prepended(compressed).ok()
}

fn compress_zlib(data: &[u8]) -> Option<Bytes> {
    use flate2::{write::ZlibEncoder, Compression};
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(ZLIB_COMPRESSION_LEVEL));
    encoder.write_all(data).ok()?;
    encoder.finish().ok().map(Bytes::from)
}

fn decompress_zlib(compressed: &[u8], original_len: usize) -> Option<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    let mut decoder = ZlibDecoder::new(compressed);
    let mut output = Vec::with_capacity(original_len);
    decoder.read_to_end(&mut output).ok()?;
    if output.len() != original_len {
        return None;
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_data_skipped() {
        for algo in &CompressionAlgo::ALL {
            assert!(
                algo.compress(b"hello").is_none(),
                "small data should skip: {:?}",
                algo
            );
        }
    }

    #[test]
    fn test_empty_data_skipped() {
        for algo in &CompressionAlgo::ALL {
            assert!(
                algo.compress(b"").is_none(),
                "empty data should skip: {:?}",
                algo
            );
        }
    }

    #[test]
    fn test_roundtrip_repetitive() {
        let data = vec![0xAB; 4096];
        for algo in &CompressionAlgo::ALL {
            let compressed = algo
                .compress(&data)
                .expect(&format!("compress failed: {:?}", algo));
            let decompressed = algo
                .decompress(&compressed, data.len())
                .expect(&format!("decompress failed: {:?}", algo));
            assert_eq!(data, decompressed, "roundtrip failed: {:?}", algo);
        }
    }

    #[test]
    fn test_roundtrip_mixed() {
        let data: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        for algo in &CompressionAlgo::ALL {
            let compressed = algo
                .compress(&data)
                .expect(&format!("compress failed: {:?}", algo));
            let decompressed = algo
                .decompress(&compressed, data.len())
                .expect(&format!("decompress failed: {:?}", algo));
            assert_eq!(data, decompressed, "roundtrip failed: {:?}", algo);
        }
    }

    #[test]
    fn test_incompressible_data_skipped() {
        use rand::RngExt;
        let mut rng = rand::rng();
        let data: Vec<u8> = (0..512).map(|_| rng.random()).collect();
        for algo in &CompressionAlgo::ALL {
            let result = algo.compress(&data);
            if let Some(ref c) = result {
                assert!(
                    c.len() < data.len(),
                    "compressed should be smaller: {:?}",
                    algo
                );
            }
        }
    }

    #[test]
    fn test_negotiate() {
        let client = vec![CompressionAlgo::Zstd, CompressionAlgo::Lz4];
        let server = vec![
            CompressionAlgo::Lz4,
            CompressionAlgo::Zstd,
            CompressionAlgo::Zlib,
        ];
        assert_eq!(
            CompressionAlgo::negotiate(&client, &server),
            Some(CompressionAlgo::Zstd)
        );
    }

    #[test]
    fn test_negotiate_no_common() {
        let client = vec![CompressionAlgo::Zstd];
        let server = vec![CompressionAlgo::Lz4, CompressionAlgo::Zlib];
        assert_eq!(CompressionAlgo::negotiate(&client, &server), None);
    }

    #[test]
    fn test_negotiate_empty_client() {
        let client: Vec<CompressionAlgo> = vec![];
        let server = vec![CompressionAlgo::Zstd];
        assert_eq!(CompressionAlgo::negotiate(&client, &server), None);
    }

    #[test]
    fn test_as_str() {
        assert_eq!(CompressionAlgo::Zstd.as_str(), "zstd");
        assert_eq!(CompressionAlgo::Lz4.as_str(), "lz4");
        assert_eq!(CompressionAlgo::Zlib.as_str(), "zlib");
    }

    #[test]
    fn test_decompress_wrong_original_len() {
        let data = vec![0xAB; 4096];
        for algo in &CompressionAlgo::ALL {
            let compressed = algo
                .compress(&data)
                .expect(&format!("compress failed: {:?}", algo));
            assert!(
                algo.decompress(&compressed, data.len() + 1).is_none(),
                "should reject wrong original_len: {:?}",
                algo
            );
        }
    }

    #[test]
    fn test_compress_to_bytes_roundtrip() {
        let data = vec![0xCD; 4096];
        for algo in &CompressionAlgo::ALL {
            let compressed = algo
                .compress_to_bytes(&data)
                .expect(&format!("compress_to_bytes failed: {:?}", algo));
            let decompressed = algo
                .decompress(&compressed, data.len())
                .expect(&format!("decompress failed: {:?}", algo));
            assert_eq!(data, decompressed, "roundtrip failed: {:?}", algo);
        }
    }
}
