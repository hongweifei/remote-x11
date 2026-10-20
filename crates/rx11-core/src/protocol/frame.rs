//! The [`Frame`] enum, its derive macro and frame-level (de)serialization.
//!
//! Frames are `MAGIC(4) || type(1) || payload_len(4, BE) || payload`. The
//! `define_frame_types!` macro is the single source of truth mapping each
//! `MessageType` tag to its payload kind (JSON, binary or empty).

use bytes::{Bytes, BytesMut};

use super::*;
use crate::error::{Result, Rx11Error};
use crate::protocol::messages::{validate_session_auto_create, validate_session_create};

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

            pub fn encode(&self) -> Result<Bytes> {
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
                            encode_raw(MessageType::$JVar as u8, &json)
                        }
                    )*
                    $(
                        Frame::$BVar(msg) => {
                            let payload = < $BPayload as BinaryMessageCodec >::encode_payload(msg)?;
                            encode_raw(MessageType::$BVar as u8, &payload)
                        }
                    )*
                    $(
                        Frame::$EVar => encode_raw(MessageType::$EVar as u8, &[]),
                    )*
                }
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
        X11Connect(X11ConnectMessage),
        X11Disconnect(X11DisconnectMessage),
        FlowControl(FlowControlMessage),
        Error(ErrorMessage),
        IncrementalRequestFull(IncrementalRequestFullMessage),
    }
    binary {
        DataX11(X11DataMessage),
        CompressedDataX11(CompressedX11DataMessage),
        IncrementalDataX11(IncrementalX11DataMessage),
        CompressedIncrementalDataX11(CompressedIncrementalX11DataMessage),
    }
    empty {
        Heartbeat,
        HeartbeatAck,
    }
}

fn encode_raw(msg_type: u8, payload: &[u8]) -> Result<Bytes> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| Rx11Error::Protocol("payload exceeds u32 max".into()))?;
    let mut buf = BytesMut::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&MAGIC_BYTES);
    buf.extend_from_slice(&[msg_type]);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    Ok(buf.freeze())
}

pub fn encode_frame(frame: &Frame) -> Result<Bytes> {
    frame.encode()
}

pub fn decode_frame(data: &[u8]) -> Result<Option<(Frame, usize)>> {
    if data.len() < FRAME_HEADER_SIZE {
        return Ok(None);
    }
    if data[0..4] != MAGIC_BYTES {
        return Err(Rx11Error::Protocol("Invalid magic bytes".into()));
    }
    let msg_type_byte = data[4];
    let payload_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
    if payload_len > MAX_FRAME_SIZE {
        return Err(Rx11Error::Protocol(format!(
            "Frame payload too large: {} bytes (max {})",
            payload_len, MAX_FRAME_SIZE
        )));
    }
    let total = FRAME_HEADER_SIZE + payload_len;
    if data.len() < total {
        return Ok(None);
    }
    let payload = &data[FRAME_HEADER_SIZE..total];
    let msg_type = MessageType::try_from(msg_type_byte)?;
    let frame = decode_payload(msg_type, payload)?;
    Ok(Some((frame, total)))
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
        let frame = Frame::DataX11(X11DataMessage {
            connection_id: ConnectionId::new(42),
            sequence_id: 7,
            data: Bytes::from_static(&[0xAA, 0xBB, 0xCC]),
        });
        match roundtrip(&frame) {
            Frame::DataX11(m) => {
                assert_eq!(m.connection_id.get(), 42);
                assert_eq!(m.sequence_id, 7);
                assert_eq!(&m.data[..], &[0xAA, 0xBB, 0xCC]);
            }
            other => panic!("Expected DataX11 frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_compressed_data_x11() {
        let frame = Frame::CompressedDataX11(CompressedX11DataMessage {
            connection_id: ConnectionId::new(42),
            sequence_id: 5,
            original_len: 1000,
            data: Bytes::from_static(&[0x01, 0x02, 0x03]),
        });
        match roundtrip(&frame) {
            Frame::CompressedDataX11(m) => {
                assert_eq!(m.original_len, 1000);
                assert_eq!(&m.data[..], &[0x01, 0x02, 0x03]);
            }
            other => panic!("Expected CompressedDataX11 frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_x11_connect() {
        let frame = Frame::X11Connect(X11ConnectMessage {
            display: DisplayNumber::new(5).unwrap(),
            connection_id: ConnectionId::new(100),
        });
        match roundtrip(&frame) {
            Frame::X11Connect(m) => {
                assert_eq!(m.display.get(), 5);
                assert_eq!(m.connection_id.get(), 100);
            }
            other => panic!("Expected X11Connect frame, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_incremental_request_full() {
        let frame = Frame::IncrementalRequestFull(IncrementalRequestFullMessage {
            connection_id: ConnectionId::new(2001),
        });
        assert_eq!(frame.msg_type(), MessageType::IncrementalRequestFull);
        match roundtrip(&frame) {
            Frame::IncrementalRequestFull(m) => assert_eq!(m.connection_id.get(), 2001),
            other => panic!("Expected IncrementalRequestFull frame, got {other:?}"),
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
        let data = [b'R', b'X', b'1', b'1', 0x01];
        let result = decode_frame(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_invalid_magic() {
        let data = [b'X', b'X', b'X', b'X', 0x01, 0, 0, 0, 0];
        let result = decode_frame(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_unknown_type() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_BYTES);
        data.push(0xFE);
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
        data.push(0x20);
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&[0, 1, 2, 3, 4]);
        let result = decode_frame(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_frame_with_nonempty_payload_rejected() {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_BYTES);
        data.push(MessageType::Heartbeat as u8);
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
        let msg = X11DataMessage {
            connection_id: ConnectionId::new(1),
            sequence_id: 0,
            data: Bytes::new(),
        };
        let frame: Frame = msg.into();
        assert_eq!(frame.msg_type(), MessageType::DataX11);
    }
}
