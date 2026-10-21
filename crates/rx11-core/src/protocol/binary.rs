//! Binary wire codec for the X11 data frames.
//!
//! JSON frames go through serde; the data-plane frames use a compact manual
//! layout to avoid JSON overhead on hot paths:
//!
//! - `StreamData`:                       `[conn_id: u32][seq: u32][data..]`
//! - `CompressedStreamData` /
//!   `CompressedIncrementalStreamData`:  `[conn_id: u32][seq: u32][original_len: u32][data..]`
//! - `IncrementalStreamData`:            `[conn_id][seq][base_seq][total_len][chunk_count: u16]`
//!   followed by `[offset: u32][length: u32][chunk_data..]` per chunk.

use bytes::{Bytes, BytesMut};

use crate::error::{Result, Rx11Error};
use crate::protocol::{
    CompressedIncrementalStreamDataMessage, CompressedStreamDataMessage, IncrementalChunk,
    IncrementalStreamDataMessage, StreamDataMessage, MAX_FRAME_SIZE,
};
use crate::types::ConnectionId;

pub trait BinaryMessageCodec {
    fn encode_payload(&self) -> Result<Bytes>;
    fn decode_payload(data: &[u8]) -> Result<Self>
    where
        Self: Sized;
}

/// Shared helpers for the two connection-data layouts above; `label` is only
/// used to build error messages that name the offending frame type.
mod helpers {
    use super::*;

    pub(super) fn encode_conn_seq(conn: u32, seq: u32, data: &[u8], label: &str) -> Result<Bytes> {
        if data.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "{label} payload too large: {} bytes (max {})",
                data.len(),
                MAX_FRAME_SIZE
            )));
        }
        let mut buf = BytesMut::with_capacity(4 + 4 + data.len());
        buf.extend_from_slice(&conn.to_be_bytes());
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(data);
        Ok(buf.freeze())
    }

    pub(super) fn decode_conn_seq(data: &[u8], label: &str) -> Result<(u32, u32, Bytes)> {
        const MIN_LEN: usize = 8;
        if data.len() < MIN_LEN {
            return Err(Rx11Error::Protocol(format!(
                "{label} payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }
        let conn = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let seq = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        Ok((conn, seq, Bytes::copy_from_slice(&data[MIN_LEN..])))
    }

    pub(super) fn encode_conn_seq_len(
        conn: u32,
        seq: u32,
        original_len: usize,
        data: &[u8],
        label: &str,
    ) -> Result<Bytes> {
        let len_u32: u32 = original_len
            .try_into()
            .map_err(|_| Rx11Error::Protocol("original_len exceeds u32".into()))?;
        if data.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "{label} payload too large: {} bytes (max {})",
                data.len(),
                MAX_FRAME_SIZE
            )));
        }
        let mut buf = BytesMut::with_capacity(4 + 4 + 4 + data.len());
        buf.extend_from_slice(&conn.to_be_bytes());
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&len_u32.to_be_bytes());
        buf.extend_from_slice(data);
        Ok(buf.freeze())
    }

    pub(super) fn decode_conn_seq_len(
        data: &[u8],
        label: &str,
    ) -> Result<(u32, u32, usize, Bytes)> {
        const MIN_LEN: usize = 12;
        if data.len() < MIN_LEN {
            return Err(Rx11Error::Protocol(format!(
                "{label} payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }
        let conn = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let seq = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let original_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        Ok((
            conn,
            seq,
            original_len,
            Bytes::copy_from_slice(&data[MIN_LEN..]),
        ))
    }
}

use helpers::{decode_conn_seq, decode_conn_seq_len, encode_conn_seq, encode_conn_seq_len};

impl BinaryMessageCodec for StreamDataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        encode_conn_seq(
            self.connection_id.get(),
            self.sequence_id,
            &self.data,
            "StreamData",
        )
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        let (conn, sequence_id, payload) = decode_conn_seq(data, "StreamData")?;
        Ok(Self {
            connection_id: ConnectionId::new(conn),
            sequence_id,
            data: payload,
        })
    }
}

impl BinaryMessageCodec for CompressedStreamDataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        encode_conn_seq_len(
            self.connection_id.get(),
            self.sequence_id,
            self.original_len,
            &self.data,
            "CompressedStreamData",
        )
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        let (conn, sequence_id, original_len, payload) =
            decode_conn_seq_len(data, "CompressedStreamData")?;
        Ok(Self {
            connection_id: ConnectionId::new(conn),
            sequence_id,
            original_len,
            data: payload,
        })
    }
}

impl BinaryMessageCodec for CompressedIncrementalStreamDataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        encode_conn_seq_len(
            self.connection_id.get(),
            self.sequence_id,
            self.original_len,
            &self.data,
            "CompressedIncrementalStreamData",
        )
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        let (conn, sequence_id, original_len, payload) =
            decode_conn_seq_len(data, "CompressedIncrementalStreamData")?;
        Ok(Self {
            connection_id: ConnectionId::new(conn),
            sequence_id,
            original_len,
            data: payload,
        })
    }
}

/// Read a big-endian `u32` at `pos`; caller guarantees bounds.
fn be_u32_at(data: &[u8], pos: usize) -> u32 {
    u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
}

impl BinaryMessageCodec for IncrementalStreamDataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        let total_len_u32: u32 = self
            .total_len
            .try_into()
            .map_err(|_| Rx11Error::Protocol("total_len exceeds u32".into()))?;
        let chunk_count_u16: u16 = self
            .chunks
            .len()
            .try_into()
            .map_err(|_| Rx11Error::Protocol("too many chunks".into()))?;

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.connection_id.get().to_be_bytes());
        buf.extend_from_slice(&self.sequence_id.to_be_bytes());
        buf.extend_from_slice(&self.base_sequence_id.to_be_bytes());
        buf.extend_from_slice(&total_len_u32.to_be_bytes());
        buf.extend_from_slice(&chunk_count_u16.to_be_bytes());

        for chunk in &self.chunks {
            let offset_u32: u32 = chunk
                .offset
                .try_into()
                .map_err(|_| Rx11Error::Protocol("chunk offset exceeds u32".into()))?;
            let length_u32: u32 = chunk
                .length
                .try_into()
                .map_err(|_| Rx11Error::Protocol("chunk length exceeds u32".into()))?;
            buf.extend_from_slice(&offset_u32.to_be_bytes());
            buf.extend_from_slice(&length_u32.to_be_bytes());
            buf.extend_from_slice(&chunk.data);
        }

        if buf.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "IncrementalStreamData payload too large: {} bytes (max {})",
                buf.len(),
                MAX_FRAME_SIZE
            )));
        }

        Ok(buf.freeze())
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        const MIN_LEN: usize = 4 + 4 + 4 + 4 + 2;
        if data.len() < MIN_LEN {
            return Err(Rx11Error::Protocol(format!(
                "IncrementalStreamData payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }

        let mut pos = 0;
        let connection_id = be_u32_at(data, pos);
        pos += 4;
        let sequence_id = be_u32_at(data, pos);
        pos += 4;
        let base_sequence_id = be_u32_at(data, pos);
        pos += 4;
        let total_len = be_u32_at(data, pos) as usize;
        pos += 4;
        let chunk_count = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            if pos + 8 > data.len() {
                return Err(Rx11Error::Protocol(
                    "Invalid IncrementalStreamData chunk header".into(),
                ));
            }
            let offset = be_u32_at(data, pos) as usize;
            pos += 4;
            let length = be_u32_at(data, pos) as usize;
            pos += 4;
            if pos + length > data.len() {
                return Err(Rx11Error::Protocol(
                    "Invalid IncrementalStreamData chunk data".into(),
                ));
            }
            let chunk_data = Bytes::copy_from_slice(&data[pos..pos + length]);
            pos += length;
            chunks.push(IncrementalChunk {
                offset,
                length,
                data: chunk_data,
            });
        }

        Ok(Self {
            connection_id: ConnectionId::new(connection_id),
            sequence_id,
            base_sequence_id,
            total_len,
            chunks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_x11_data_codec_roundtrip() {
        let msg = StreamDataMessage {
            connection_id: ConnectionId::new(42),
            sequence_id: 7,
            data: Bytes::from_static(&[0xAA, 0xBB, 0xCC]),
        };
        let encoded = msg.encode_payload().unwrap();
        let decoded = StreamDataMessage::decode_payload(&encoded).unwrap();
        assert_eq!(decoded.connection_id.get(), 42);
        assert_eq!(decoded.sequence_id, 7);
        assert_eq!(&decoded.data[..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn test_x11_data_codec_too_short() {
        let err = StreamDataMessage::decode_payload(&[0u8; 7]).unwrap_err();
        assert!(err.to_string().contains("StreamData payload too short"));
    }

    #[test]
    fn test_compressed_codec_roundtrip() {
        let msg = CompressedStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 2,
            original_len: 1000,
            data: Bytes::from_static(&[1, 2, 3]),
        };
        let encoded = msg.encode_payload().unwrap();
        let decoded = CompressedStreamDataMessage::decode_payload(&encoded).unwrap();
        assert_eq!(decoded.original_len, 1000);
        assert_eq!(&decoded.data[..], &[1, 2, 3]);
    }

    #[test]
    fn test_compressed_incremental_codec_roundtrip() {
        let msg = CompressedIncrementalStreamDataMessage {
            connection_id: ConnectionId::new(9),
            sequence_id: 8,
            original_len: 512,
            data: Bytes::from_static(&[7, 7, 7]),
        };
        let encoded = msg.encode_payload().unwrap();
        let decoded = CompressedIncrementalStreamDataMessage::decode_payload(&encoded).unwrap();
        assert_eq!(decoded.connection_id.get(), 9);
        assert_eq!(decoded.sequence_id, 8);
        assert_eq!(decoded.original_len, 512);
        assert_eq!(&decoded.data[..], &[7, 7, 7]);
    }

    #[test]
    fn test_incremental_codec_roundtrip() {
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(3),
            sequence_id: 5,
            base_sequence_id: 4,
            total_len: 2048,
            chunks: vec![
                IncrementalChunk {
                    offset: 0,
                    length: 2,
                    data: Bytes::from_static(&[1, 2]),
                },
                IncrementalChunk {
                    offset: 1024,
                    length: 3,
                    data: Bytes::from_static(&[3, 4, 5]),
                },
            ],
        };
        let encoded = msg.encode_payload().unwrap();
        let decoded = IncrementalStreamDataMessage::decode_payload(&encoded).unwrap();
        assert_eq!(decoded.base_sequence_id, 4);
        assert_eq!(decoded.total_len, 2048);
        assert_eq!(decoded.chunks.len(), 2);
        assert_eq!(decoded.chunks[1].offset, 1024);
        assert_eq!(&decoded.chunks[1].data[..], &[3, 4, 5]);
    }

    #[test]
    fn test_incremental_codec_empty_chunks_roundtrip() {
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 1,
            base_sequence_id: 0,
            total_len: 16,
            chunks: vec![],
        };
        let encoded = msg.encode_payload().unwrap();
        let decoded = IncrementalStreamDataMessage::decode_payload(&encoded).unwrap();
        assert!(decoded.chunks.is_empty());
    }

    #[test]
    fn test_incremental_codec_truncated_chunk_header() {
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 1,
            base_sequence_id: 0,
            total_len: 64,
            chunks: vec![IncrementalChunk {
                offset: 0,
                length: 2,
                data: Bytes::from_static(&[1, 2]),
            }],
        };
        let mut encoded = msg.encode_payload().unwrap().to_vec();
        encoded.truncate(18 + 4); // header + partial chunk header
        let err = IncrementalStreamDataMessage::decode_payload(&encoded).unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid IncrementalStreamData chunk header"));
    }

    #[test]
    fn test_incremental_codec_truncated_chunk_data() {
        let msg = IncrementalStreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 1,
            base_sequence_id: 0,
            total_len: 64,
            chunks: vec![IncrementalChunk {
                offset: 0,
                length: 8,
                data: Bytes::from_static(&[1, 2]),
            }],
        };
        let mut encoded = msg.encode_payload().unwrap().to_vec();
        encoded.truncate(26 + 2); // header + chunk header + partial chunk data
        let err = IncrementalStreamDataMessage::decode_payload(&encoded).unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid IncrementalStreamData chunk data"));
    }

    #[test]
    fn test_incremental_codec_too_short() {
        let err = IncrementalStreamDataMessage::decode_payload(&[0u8; 17]).unwrap_err();
        assert!(err.to_string().contains("payload too short"));
    }
}
