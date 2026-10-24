//! The [`Frame`] enum, its derive macro and frame-level (de)serialization.
//!
//! Frames are `MAGIC(4) || version(1) || type(2, BE) || payload_len(4, BE) ||
//! payload`. The
//! `define_frame_types!` macro is the single source of truth mapping each
//! `MessageType` tag to its payload kind (JSON, binary or empty).

use bytes::{Bytes, BytesMut};

use super::*;
use crate::error::{Result, Rx11Error};
use crate::protocol::messages::{validate_session_auto_create, validate_session_create};
use crate::types::ConnectionId;

macro_rules! define_frame_types {
    (
        json {
            $( $JVar:ident($JPayload:ty) $(validate($jvalidate:path))? ),* $(,)?
        }
        binary {
            $( $BVar:ident($BPayload:ty) ),* $(,)?
        }
        empty {
            $( $EVar:ident ),* $(,)?
        }
    ) => {
        #[derive(Debug, Clone)]
        #[non_exhaustive]
        pub enum Frame {
            $( $JVar($JPayload), )*
            $( $BVar($BPayload), )*
            $( $EVar, )*
        }

        impl Frame {
            pub fn msg_type(&self) -> MessageType {
                match self {
                    $( Frame::$JVar(_) => MessageType::$JVar, )*
                    $( Frame::$BVar(_) => MessageType::$BVar, )*
                    $( Frame::$EVar => MessageType::$EVar, )*
                }
            }

            /// The multiplexed channel a frame belongs to, when it names one
            /// (data-plane shapes, channel open/close, resync request, flow
            /// control scope). Receivers check ownership against this BEFORE
            /// letting the frame touch any per-connection state — an
            /// unauthenticated peer must not be able to poison another
            /// client's diff cache or compression stream.
            pub fn connection_id(&self) -> Option<ConnectionId> {
                match self {
                    Frame::StreamData(m) => Some(m.connection_id),
                    Frame::CompressedStreamData(m) => Some(m.connection_id),
                    Frame::IncrementalStreamData(m) => Some(m.connection_id),
                    Frame::CompressedIncrementalStreamData(m) => Some(m.connection_id),
                    Frame::ChannelOpen(m) => Some(m.connection_id),
                    Frame::ChannelClose(m) => Some(m.connection_id),
                    Frame::StreamResyncRequest(m) => Some(m.connection_id),
                    Frame::FlowControl(m) => m.connection_id,
                    _ => None,
                }
            }

            /// Append `MAGIC|version|type|len|payload` for this frame to
            /// `out` in a single buffer. Binary payloads are materialized
            /// with exactly one memcpy (the copy into `out`); JSON frames
            /// additionally borrow through `serde_json`'s Vec.
            pub fn encode_into(&self, out: &mut BytesMut) -> Result<()> {
                match self {
                    $(
                        Frame::$JVar(msg) => {
                            let json = serde_json::to_vec(msg)
                                .map_err(|e| Rx11Error::Protocol(e.to_string()))?;
                            if json.len() > MAX_FRAME_SIZE {
                                return Err(Rx11Error::Protocol(format!(
                                    "Frame payload too large: {} bytes (max {})",
                                    json.len(), MAX_FRAME_SIZE
                                )));
                            }
                            let at = begin_header(out, MessageType::$JVar as u16);
                            out.reserve(json.len());
                            out.extend_from_slice(&json);
                            finish_header(out, at);
                        }
                    )*
                    $(
                        Frame::$BVar(msg) => {
                            let at = begin_header(out, MessageType::$BVar as u16);
                            if let Err(e) = < $BPayload as BinaryMessageCodec >::encode_payload_into(msg, out) {
                                out.truncate(at);
                                return Err(e);
                            }
                            if let Err(e) = finish_header_checked(out, at, stringify!($BVar)) {
                                out.truncate(at);
                                return Err(e);
                            }
                        }
                    )*
                    $(
                        Frame::$EVar => {
                            let at = begin_header(out, MessageType::$EVar as u16);
                            finish_header(out, at);
                        }
                    )*
                }
                Ok(())
            }

            pub fn encode(&self) -> Result<Bytes> {
                let mut buf = BytesMut::with_capacity(FRAME_HEADER_SIZE + self.payload_len_hint());
                self.encode_into(&mut buf)?;
                Ok(buf.freeze())
            }

            /// Exact or optimistic payload size, used to seed encode buffers
            /// at their final size (one allocation, zero growth copies).
            fn payload_len_hint(&self) -> usize {
                match self {
                    $( Frame::$JVar(_) => 128, )*
                    $( Frame::$BVar(msg) => < $BPayload as BinaryMessageCodec >::payload_len(msg), )*
                    $( Frame::$EVar => 0, )*
                }
            }

            /// Frame size on the wire for queue accounting: deterministic
            /// per frame (exact for binary, optimistic for JSON), so the
            /// value charged on send equals the one released on receive.
            pub fn encoded_size(&self) -> usize {
                FRAME_HEADER_SIZE + self.payload_len_hint()
            }
        }

        $(
            impl From<$JPayload> for Frame {
                fn from(msg: $JPayload) -> Frame {
                    Frame::$JVar(msg)
                }
            }
        )*
        $(
            impl From<$BPayload> for Frame {
                fn from(msg: $BPayload) -> Frame {
                    Frame::$BVar(msg)
                }
            }
        )*

        fn decode_payload(msg_type: MessageType, payload: &[u8]) -> Result<Frame> {
            match msg_type {
                $(
                    MessageType::$JVar => {
                        let msg: $JPayload = serde_json::from_slice(payload)
                            .map_err(|e| Rx11Error::Protocol(e.to_string()))?;
                        $( $jvalidate(&msg)?; )?
                        Ok(Frame::$JVar(msg))
                    }
                )*
                $(
                    MessageType::$BVar => {
                        < $BPayload as BinaryMessageCodec >::decode_payload(payload)
                            .map(Frame::$BVar)
                    }
                )*
                $(
                    MessageType::$EVar => {
                        if !payload.is_empty() {
                            return Err(Rx11Error::Protocol(
                                concat!(stringify!($EVar), " frame must have empty payload").into(),
                            ));
                        }
                        Ok(Frame::$EVar)
                    }
                )*
            }
        }

        /// Owned-buffer twin of [`decode_payload`]: binary payloads alias the
        /// caller's allocation (refcounted slices, no memcpy).
        fn decode_payload_owned(msg_type: MessageType, payload: Bytes) -> Result<Frame> {
            match msg_type {
                $(
                    MessageType::$JVar => decode_payload(msg_type, &payload),
                )*
                $(
                    MessageType::$BVar => {
                        < $BPayload as BinaryMessageCodec >::decode_payload_owned(payload)
                            .map(Frame::$BVar)
                    }
                )*
                $(
                    MessageType::$EVar => decode_payload(msg_type, &payload),
                )*
            }
        }
    };
}

define_frame_types! {
    json {
        Hello(HelloMessage),
        HelloAck(HelloAckMessage),
        AuthRequest(AuthRequestMessage),
        AuthResponse(AuthResponseMessage),
        SessionCreate(SessionCreateMessage) validate(validate_session_create),
        SessionAck(SessionAckMessage),
        SessionDestroy(SessionDestroyMessage),
        SessionResume(SessionResumeMessage),
        SessionAutoCreate(SessionAutoCreateMessage) validate(validate_session_auto_create),
        ChannelOpen(ChannelOpenMessage),
        ChannelClose(ChannelCloseMessage),
        FlowControl(FlowControlMessage),
        Error(ErrorMessage),
        StreamResyncRequest(StreamResyncRequestMessage),
    }
    binary {
        StreamData(StreamDataMessage),
        CompressedStreamData(CompressedStreamDataMessage),
        IncrementalStreamData(IncrementalStreamDataMessage),
        CompressedIncrementalStreamData(CompressedIncrementalStreamDataMessage),
    }
    empty {
        Heartbeat,
        HeartbeatAck,
    }
}

/// Write `MAGIC|version|type|len(placeholder)` at the current tail; returns
/// the start offset so `finish_header` can back-patch the payload length
/// once the payload has been appended.
fn begin_header(out: &mut BytesMut, msg_type: u16) -> usize {
    let at = out.len();
    out.reserve(FRAME_HEADER_SIZE);
    out.extend_from_slice(&MAGIC_BYTES);
    out.extend_from_slice(&[WIRE_VERSION]);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    at
}

fn finish_header(out: &mut BytesMut, at: usize) {
    let payload_len = (out.len() - at - FRAME_HEADER_SIZE) as u32;
    out[at + 7..at + FRAME_HEADER_SIZE].copy_from_slice(&payload_len.to_be_bytes());
}

/// `finish_header` plus the MAX_FRAME_SIZE check; on overflow the partially
/// written frame is truncated away so `out` keeps only prior frames.
fn finish_header_checked(out: &mut BytesMut, at: usize, label: &str) -> Result<()> {
    let payload_len = out.len() - at - FRAME_HEADER_SIZE;
    if payload_len > MAX_FRAME_SIZE {
        out.truncate(at);
        return Err(Rx11Error::Protocol(format!(
            "{label} payload too large: {payload_len} bytes (max {MAX_FRAME_SIZE})"
        )));
    }
    finish_header(out, at);
    Ok(())
}

/// Parse + validate the frame header WITHOUT consuming anything.
/// `Ok(None)` = not enough bytes for a header yet; `Ok(Some((mt, total)))` =
/// a well-formed header whose full frame is `total` bytes; `Err` = corrupt
/// header (magic, version, unknown type or oversized length).
pub(crate) fn peek_frame(data: &[u8]) -> Result<Option<(MessageType, usize)>> {
    if data.len() < FRAME_HEADER_SIZE {
        return Ok(None);
    }
    if data[0..4] != MAGIC_BYTES {
        return Err(Rx11Error::Protocol("Invalid magic bytes".into()));
    }
    let wire_version = data[4];
    if wire_version != WIRE_VERSION {
        return Err(Rx11Error::Protocol(format!(
            "Unsupported protocol version: {} (expected {})",
            wire_version, WIRE_VERSION
        )));
    }
    let msg_type = u16::from_be_bytes([data[5], data[6]]);
    let payload_len = u32::from_be_bytes([data[7], data[8], data[9], data[10]]) as usize;
    if payload_len > MAX_FRAME_SIZE {
        return Err(Rx11Error::Protocol(format!(
            "Frame payload too large: {} bytes (max {})",
            payload_len, MAX_FRAME_SIZE
        )));
    }
    let msg_type = MessageType::try_from(msg_type)?;
    Ok(Some((msg_type, FRAME_HEADER_SIZE + payload_len)))
}

pub fn encode_frame(frame: &Frame) -> Result<Bytes> {
    frame.encode()
}

/// Borrowed decode (payloads are copied out). Kept for tests and callers
/// that work on plain slices; the transport uses `decode_frame_bytes`.
pub fn decode_frame(data: &[u8]) -> Result<Option<(Frame, usize)>> {
    let (msg_type, total) = match peek_frame(data)? {
        Some(v) => v,
        None => return Ok(None),
    };
    if data.len() < total {
        return Ok(None);
    }
    let payload = &data[FRAME_HEADER_SIZE..total];
    let frame = decode_payload(msg_type, payload)?;
    Ok(Some((frame, total)))
}

/// Owned-buffer twin of [`decode_payload`]: binary payloads alias the
/// caller's allocation (refcounted slices, no memcpy).
pub(crate) fn decode_frame_owned(msg_type: MessageType, payload: Bytes) -> Result<Frame> {
    decode_payload_owned(msg_type, payload)
}

pub const fn frame_header_size() -> usize {
    FRAME_HEADER_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConnectionId, DisplayNumber, SessionId, Token};

    /// Encode then decode a frame, asserting it consumes the whole buffer.
    fn roundtrip(frame: &Frame) -> Frame {
        let encoded = encode_frame(frame).unwrap();
        let (decoded, consumed) = decode_frame(&encoded).unwrap().unwrap();
        assert_eq!(consumed, encoded.len());
        decoded
    }

    #[test]
    fn test_encode_decode_hello() {
        let frame = Frame::Hello(HelloMessage {
            version: 1,
            mode: ConnectionMode::Client,
            resume_session_id: None,
            compression_algos: vec![],
            capabilities: vec![],
        });
        match roundtrip(&frame) {
            Frame::Hello(m) => {
                assert_eq!(m.version, 1);
                assert!(matches!(m.mode, ConnectionMode::Client));
                assert!(m.resume_session_id.is_none());
            }
            other => panic!("Expected Hello frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_hello_ack() {
        let frame = Frame::HelloAck(HelloAckMessage {
            version: 1,
            session_id: SessionId::new("test-session".to_string()).unwrap(),
            success: true,
            error_msg: None,
            compression: None,
            capabilities: vec![],
        });
        match roundtrip(&frame) {
            Frame::HelloAck(m) => {
                assert_eq!(m.session_id.as_str(), "test-session");
                assert!(m.success);
            }
            other => panic!("Expected HelloAck frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_auth_request() {
        let frame = Frame::AuthRequest(AuthRequestMessage {
            token: Token::new("my-token".to_string()).unwrap(),
        });
        match roundtrip(&frame) {
            Frame::AuthRequest(m) => assert_eq!(m.token.as_str(), "my-token"),
            other => panic!("Expected AuthRequest frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_auth_response() {
        let frame = Frame::AuthResponse(AuthResponseMessage {
            success: false,
            error_msg: Some("bad token".into()),
        });
        match roundtrip(&frame) {
            Frame::AuthResponse(m) => {
                assert!(!m.success);
                assert_eq!(m.error_msg.unwrap(), "bad token");
            }
            other => panic!("Expected AuthResponse frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_session_create() {
        let frame = Frame::SessionCreate(SessionCreateMessage {
            display: DisplayNumber::new(0).unwrap(),
            auth_name: "MIT-MAGIC-COOKIE-1".into(),
            auth_data: vec![1, 2, 3, 4],
        });
        match roundtrip(&frame) {
            Frame::SessionCreate(m) => {
                assert_eq!(m.display.get(), 0);
                assert_eq!(m.auth_data, vec![1, 2, 3, 4]);
            }
            other => panic!("Expected SessionCreate frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_session_destroy() {
        let frame = Frame::SessionDestroy(SessionDestroyMessage {
            display: DisplayNumber::new(3).unwrap(),
        });
        assert_eq!(frame.msg_type(), MessageType::SessionDestroy);
        match roundtrip(&frame) {
            Frame::SessionDestroy(m) => assert_eq!(m.display.get(), 3),
            other => panic!("Expected SessionDestroy frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_data_x11() {
        let frame = Frame::StreamData(StreamDataMessage {
            connection_id: ConnectionId::new(42),
            sequence_id: 7,
            data: Bytes::from_static(&[0xAA, 0xBB, 0xCC]),
        });
        match roundtrip(&frame) {
            Frame::StreamData(m) => {
                assert_eq!(m.connection_id.get(), 42);
                assert_eq!(m.sequence_id, 7);
                assert_eq!(&m.data[..], &[0xAA, 0xBB, 0xCC]);
            }
            other => panic!("Expected StreamData frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_compressed_data_x11() {
        let frame = Frame::CompressedStreamData(CompressedStreamDataMessage {
            connection_id: ConnectionId::new(42),
            sequence_id: 5,
            original_len: 1000,
            data: Bytes::from_static(&[0x01, 0x02, 0x03]),
        });
        match roundtrip(&frame) {
            Frame::CompressedStreamData(m) => {
                assert_eq!(m.original_len, 1000);
                assert_eq!(&m.data[..], &[0x01, 0x02, 0x03]);
            }
            other => panic!("Expected CompressedStreamData frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_channel_open() {
        let frame = Frame::ChannelOpen(ChannelOpenMessage {
            connection_id: ConnectionId::new(100),
            kind: ChannelKind::x11(),
            display: Some(DisplayNumber::new(5).unwrap()),
        });
        match roundtrip(&frame) {
            Frame::ChannelOpen(m) => {
                assert!(m.kind.is_x11());
                assert_eq!(m.display.unwrap().get(), 5);
                assert_eq!(m.connection_id.get(), 100);
            }
            other => panic!("Expected ChannelOpen frame, got {other:?}"),
        }
    }

    #[test]
    fn test_channel_kind_unknown_names_survive_the_wire() {
        // A future peer opening a "clipboard" channel must decode cleanly on
        // a build that only knows "x11" — rejection happens at routing, not
        // at parsing.
        let frame = Frame::ChannelOpen(ChannelOpenMessage {
            connection_id: ConnectionId::new(1),
            kind: ChannelKind::new("clipboard"),
            display: None,
        });
        match roundtrip(&frame) {
            Frame::ChannelOpen(m) => {
                assert_eq!(m.kind.name(), "clipboard");
                assert!(!m.kind.is_x11());
                assert!(m.display.is_none());
            }
            other => panic!("expected ChannelOpen, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_incremental_request_full() {
        let frame = Frame::StreamResyncRequest(StreamResyncRequestMessage {
            connection_id: ConnectionId::new(2001),
        });
        assert_eq!(frame.msg_type(), MessageType::StreamResyncRequest);
        match roundtrip(&frame) {
            Frame::StreamResyncRequest(m) => assert_eq!(m.connection_id.get(), 2001),
            other => panic!("Expected StreamResyncRequest frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_heartbeat() {
        let frame = Frame::Heartbeat;
        assert!(matches!(roundtrip(&frame), Frame::Heartbeat));
    }

    #[test]
    fn test_encode_decode_error() {
        let frame = Frame::Error(ErrorMessage {
            code: 1,
            message: "test error".into(),
        });
        match roundtrip(&frame) {
            Frame::Error(m) => {
                assert_eq!(m.code, 1);
                assert_eq!(m.message, "test error");
            }
            other => panic!("Expected Error frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_flow_control() {
        let frame = Frame::FlowControl(FlowControlMessage {
            action: FlowControlAction::Pause,
            connection_id: Some(ConnectionId::new(42)),
        });
        match roundtrip(&frame) {
            Frame::FlowControl(m) => {
                assert_eq!(m.action, FlowControlAction::Pause);
                assert_eq!(m.connection_id.unwrap().get(), 42);
            }
            other => panic!("Expected FlowControl frame, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_incomplete_returns_none() {
        let data = [b'R', b'X', b'1', b'1', 1, 0x01];
        let result = decode_frame(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_invalid_magic() {
        let data = [b'X', b'X', b'X', b'X', 0x01, 0, 0, 0, 0, 0, 0];
        let result = decode_frame(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_unknown_type() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_BYTES);
        data.push(WIRE_VERSION);
        data.extend_from_slice(&0xFEu16.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        let result = decode_frame(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_header_size() {
        assert_eq!(frame_header_size(), FRAME_HEADER_SIZE);
    }

    #[test]
    fn test_data_x11_too_short_payload() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_BYTES);
        data.push(WIRE_VERSION);
        data.extend_from_slice(&0x20u16.to_be_bytes());
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&[0, 1, 2, 3, 4]);
        let result = decode_frame(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_frame_with_nonempty_payload_rejected() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_BYTES);
        data.push(WIRE_VERSION);
        data.extend_from_slice(&(MessageType::Heartbeat as u16).to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.push(0xFF);
        let result = decode_frame(&data);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Heartbeat frame must have empty payload"));
    }

    #[test]
    fn test_from_payload_to_frame() {
        let msg = ErrorMessage {
            code: 42,
            message: "test".into(),
        };
        let frame: Frame = msg.into();
        assert_eq!(frame.msg_type(), MessageType::Error);
    }

    #[test]
    fn test_from_binary_payload_to_frame() {
        let msg = StreamDataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 0,
            data: Bytes::new(),
        };
        let frame: Frame = msg.into();
        assert_eq!(frame.msg_type(), MessageType::StreamData);
    }

    #[test]
    fn test_header_layout_is_magic_version_type_len() {
        let frame = Frame::Heartbeat;
        let encoded = encode_frame(&frame).unwrap();
        assert_eq!(&encoded[0..4], &MAGIC_BYTES);
        assert_eq!(encoded[4], WIRE_VERSION);
        assert_eq!(
            &encoded[5..7],
            &(MessageType::Heartbeat as u16).to_be_bytes()
        );
        assert_eq!(&encoded[7..11], &0u32.to_be_bytes());
        assert_eq!(encoded.len(), frame_header_size());
    }

    #[test]
    fn test_decode_rejects_other_wire_version() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_BYTES);
        data.push(WIRE_VERSION + 9);
        data.extend_from_slice(&(MessageType::Heartbeat as u16).to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        let err = decode_frame(&data).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported protocol version"),
            "{err}"
        );
    }
}
