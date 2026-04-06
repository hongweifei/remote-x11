use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

pub const COMPRESSION_THRESHOLD: usize = 64;

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
        match self {
            CompressionAlgo::Zstd => decompress_zstd(compressed, original_len),
            CompressionAlgo::Lz4 => decompress_lz4(compressed, original_len),
            CompressionAlgo::Zlib => decompress_zlib(compressed),
        }
    }

    pub fn negotiate(
        client_algos: &[CompressionAlgo],
        server_algos: &[CompressionAlgo],
    ) -> Option<CompressionAlgo> {
        for algo in server_algos {
            if client_algos.contains(algo) {
                return Some(*algo);
            }
        }
        None
    }
}

fn compress_zstd(data: &[u8]) -> Option<Vec<u8>> {
    zstd::encode_all(data, 3).ok()
}

fn decompress_zstd(compressed: &[u8], _original_len: usize) -> Option<Vec<u8>> {
    zstd::decode_all(compressed).ok()
}

fn compress_lz4(data: &[u8]) -> Option<Vec<u8>> {
    Some(lz4_flex::compress_prepend_size(data))
}

fn decompress_lz4(compressed: &[u8], _original_len: usize) -> Option<Vec<u8>> {
    lz4_flex::decompress_size_prepended(compressed).ok()
}

fn compress_zlib(data: &[u8]) -> Option<Vec<u8>> {
    use flate2::{write::ZlibEncoder, Compression};
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(3));
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

fn decompress_zlib(compressed: &[u8]) -> Option<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    let mut decoder = ZlibDecoder::new(compressed);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output).ok()?;
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
            Some(CompressionAlgo::Lz4)
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
}
