//! Compression algorithm negotiation and frame-level compress/decompress
//! helpers shared by server and client.

use std::io::{Read, Write};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::protocol::CompressedPayload;

/// Payloads smaller than this skip compression entirely.
pub const COMPRESSION_THRESHOLD: usize = 256;

/// zstd level used by both the stateless helper (`encode_all`) and the
/// pooled `DataPlane` contexts; a single source of truth for the wire's
/// compression profile.
pub(crate) const ZSTD_COMPRESSION_LEVEL: i32 = 3;
const ZLIB_COMPRESSION_LEVEL: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CompressionAlgo {
    Zstd = 0,
    Lz4 = 1,
    Zlib = 2,
}

impl CompressionAlgo {
    /// Preferred order used for negotiation and advertised by both peers.
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

    /// Compress when the payload is worth it (above the threshold and
    /// actually smaller afterwards); `None` means "send uncompressed".
    pub fn compress_to_bytes(&self, data: &[u8]) -> Option<Bytes> {
        self.compress_with(data, COMPRESSION_THRESHOLD)
    }

    /// [`Self::compress_to_bytes`] with an explicit minimum-size threshold,
    /// as configured by [`crate::config::RuntimeConfig`].
    pub fn compress_with(&self, data: &[u8], threshold: usize) -> Option<Bytes> {
        if data.len() < threshold {
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

    /// Pick the first algorithm from our preference list that the peer also
    /// supports.
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

/// Decompress a compressed-payload frame using the negotiated algorithm.
pub fn decompress_frame_data(
    msg: &impl CompressedPayload,
    algo: CompressionAlgo,
) -> Option<Vec<u8>> {
    algo.decompress(msg.compressed_data(), msg.original_len())
}

fn compress_zstd(data: &[u8]) -> Option<Bytes> {
    zstd::encode_all(data, ZSTD_COMPRESSION_LEVEL)
        .ok()
        .map(Bytes::from)
}

fn decompress_zstd(compressed: &[u8]) -> Option<Vec<u8>> {
    zstd::decode_all(compressed).ok()
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
    use crate::protocol::{CompressedIncrementalStreamDataMessage, CompressedStreamDataMessage};
    use crate::types::ConnectionId;

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
                .unwrap_or_else(|| panic!("compress failed: {:?}", algo));
            let decompressed = algo
                .decompress(&compressed, data.len())
                .unwrap_or_else(|| panic!("decompress failed: {:?}", algo));
            assert_eq!(data, decompressed, "roundtrip failed: {:?}", algo);
        }
    }

    #[test]
    fn test_roundtrip_mixed() {
        let data: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        for algo in &CompressionAlgo::ALL {
            let compressed = algo
                .compress(&data)
                .unwrap_or_else(|| panic!("compress failed: {:?}", algo));
            let decompressed = algo
                .decompress(&compressed, data.len())
                .unwrap_or_else(|| panic!("decompress failed: {:?}", algo));
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
    fn test_algo_wire_names_match_log_names() {
        // One canonical spelling: serde wire form must equal as_str(), and
        // must be lowercase to match capability-name style.
        for algo in CompressionAlgo::ALL {
            let wire = serde_json::to_string(&algo).unwrap();
            assert_eq!(wire, format!("\"{}\"", algo.as_str()), "wire/as_str drift");
            assert_eq!(algo.as_str().to_lowercase(), algo.as_str());
        }
        let back: CompressionAlgo = serde_json::from_str("\"zstd\"").unwrap();
        assert_eq!(back, CompressionAlgo::Zstd);
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
                .unwrap_or_else(|| panic!("compress failed: {:?}", algo));
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
                .unwrap_or_else(|| panic!("compress_to_bytes failed: {:?}", algo));
            let decompressed = algo
                .decompress(&compressed, data.len())
                .unwrap_or_else(|| panic!("decompress failed: {:?}", algo));
            assert_eq!(data, decompressed, "roundtrip failed: {:?}", algo);
        }
    }

    #[test]
    fn test_decompress_frame_data_generic_over_compressed_payload() {
        let data = vec![0xEF; 2048];
        let algo = CompressionAlgo::Zstd;
        let compressed = algo.compress_to_bytes(&data).expect("compressible");
        let msg = CompressedStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 0,
            original_len: data.len(),
            data: compressed,
        };
        assert_eq!(decompress_frame_data(&msg, algo).unwrap(), data);

        let inc_msg = CompressedIncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 0,
            original_len: data.len(),
            data: algo.compress_to_bytes(&data).unwrap(),
        };
        assert_eq!(decompress_frame_data(&inc_msg, algo).unwrap(), data);
    }
}
