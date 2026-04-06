use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 3;
pub const MAGIC_BYTES: [u8; 4] = [b'R', b'X', b'1', b'1'];
pub const DEFAULT_RELAY_PORT: u16 = 7000;
pub const DEFAULT_X11_PORT: u16 = 6000;
pub const MAX_DISPLAY_NUMBER: u16 = 255;
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub const FRAME_HEADER_SIZE: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[non_exhaustive]
pub enum MessageType {
    Hello = 0x01,
    HelloAck = 0x02,
    AuthRequest = 0x03,
    AuthResponse = 0x04,
    SessionCreate = 0x10,
    SessionAck = 0x11,
    SessionDestroy = 0x12,
    SessionResume = 0x13,
    SessionAutoCreate = 0x14,
    DataX11 = 0x20,
    CompressedDataX11 = 0x21,
    X11Connect = 0x22,
    X11Disconnect = 0x23,
    Heartbeat = 0x30,
    HeartbeatAck = 0x31,
    FlowControl = 0x40,
    Error = 0xFF,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloMessage {
    pub version: u32,
    pub mode: ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session_id: Option<String>,
    #[serde(default = "default_compression_algos")]
    pub compression_algos: Vec<crate::compress::CompressionAlgo>,
}

fn default_compression_algos() -> Vec<crate::compress::CompressionAlgo> {
    crate::compress::CompressionAlgo::ALL.to_vec()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAckMessage {
    pub version: u32,
    pub session_id: String,
    pub success: bool,
    pub error_msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<crate::compress::CompressionAlgo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ConnectionMode {
    Server,
    Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequestMessage {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponseMessage {
    pub success: bool,
    pub error_msg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateMessage {
    pub display: u16,
    pub auth_name: String,
    pub auth_data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAckMessage {
    pub display: u16,
    pub success: bool,
    pub error_msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDestroyMessage {
    pub display: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeMessage {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAutoCreateMessage {
    pub auth_name: String,
    pub auth_data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorMessage {
    pub code: u32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X11DataMessage {
    pub connection_id: u32,
    pub sequence_id: u32,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X11ConnectMessage {
    pub display: u16,
    pub connection_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X11DisconnectMessage {
    pub display: u16,
    pub connection_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FlowControlAction {
    Pause = 0,
    Resume = 1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowControlMessage {
    pub action: FlowControlAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<u32>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Frame {
    Hello(HelloMessage),
    HelloAck(HelloAckMessage),
    AuthRequest(AuthRequestMessage),
    AuthResponse(AuthResponseMessage),
    SessionCreate(SessionCreateMessage),
    SessionAck(SessionAckMessage),
    SessionDestroy(SessionDestroyMessage),
    SessionResume(SessionResumeMessage),
    SessionAutoCreate(SessionAutoCreateMessage),
    DataX11(X11DataMessage),
    CompressedDataX11 {
        connection_id: u32,
        sequence_id: u32,
        original_len: usize,
        data: Bytes,
    },
    X11Connect(X11ConnectMessage),
    X11Disconnect(X11DisconnectMessage),
    Heartbeat,
    HeartbeatAck,
    FlowControl(FlowControlMessage),
    Error(ErrorMessage),
}

impl Frame {
    pub fn msg_type(&self) -> MessageType {
        match self {
            Frame::Hello(_) => MessageType::Hello,
            Frame::HelloAck(_) => MessageType::HelloAck,
            Frame::AuthRequest(_) => MessageType::AuthRequest,
            Frame::AuthResponse(_) => MessageType::AuthResponse,
            Frame::SessionCreate(_) => MessageType::SessionCreate,
            Frame::SessionAck(_) => MessageType::SessionAck,
            Frame::SessionDestroy(_) => MessageType::SessionDestroy,
            Frame::SessionResume(_) => MessageType::SessionResume,
            Frame::SessionAutoCreate(_) => MessageType::SessionAutoCreate,
            Frame::DataX11(_) => MessageType::DataX11,
            Frame::CompressedDataX11 { .. } => MessageType::CompressedDataX11,
            Frame::X11Connect(_) => MessageType::X11Connect,
            Frame::X11Disconnect(_) => MessageType::X11Disconnect,
            Frame::Heartbeat => MessageType::Heartbeat,
            Frame::HeartbeatAck => MessageType::HeartbeatAck,
            Frame::FlowControl(_) => MessageType::FlowControl,
            Frame::Error(_) => MessageType::Error,
        }
    }
}

pub fn encode_frame(frame: &Frame) -> crate::error::Result<Bytes> {
    let msg_type = frame.msg_type() as u8;
    let payload_bytes = match frame {
        Frame::DataX11(m) => {
            if m.data.len() > MAX_FRAME_SIZE {
                return Err(crate::error::Rx11Error::Protocol(format!(
                    "DataX11 payload too large: {} bytes (max {})",
                    m.data.len(),
                    MAX_FRAME_SIZE
                )));
            }
            let mut buf = BytesMut::with_capacity(4 + 4 + m.data.len());
            buf.extend_from_slice(&m.connection_id.to_be_bytes());
            buf.extend_from_slice(&m.sequence_id.to_be_bytes());
            buf.extend_from_slice(&m.data);
            return Ok(encode_raw(msg_type, &buf.freeze()));
        }
        Frame::CompressedDataX11 {
            connection_id,
            sequence_id,
            original_len,
            data,
        } => {
            let len_u32: u32 = (*original_len).try_into().map_err(|_| {
                crate::error::Rx11Error::Protocol("original_len exceeds u32".into())
            })?;
            if data.len() > MAX_FRAME_SIZE {
                return Err(crate::error::Rx11Error::Protocol(format!(
                    "CompressedDataX11 payload too large: {} bytes (max {})",
                    data.len(),
                    MAX_FRAME_SIZE
                )));
            }
            let mut buf = BytesMut::with_capacity(4 + 4 + 4 + data.len());
            buf.extend_from_slice(&connection_id.to_be_bytes());
            buf.extend_from_slice(&sequence_id.to_be_bytes());
            buf.extend_from_slice(&len_u32.to_be_bytes());
            buf.extend_from_slice(data);
            return Ok(encode_raw(msg_type, &buf.freeze()));
        }
        Frame::Heartbeat | Frame::HeartbeatAck => Bytes::new(),
        _ => {
            let json_bytes = encode_control_payload(frame)?;
            if json_bytes.len() > MAX_FRAME_SIZE {
                return Err(crate::error::Rx11Error::Protocol(format!(
                    "Frame payload too large: {} bytes (max {})",
                    json_bytes.len(),
                    MAX_FRAME_SIZE
                )));
            }
            json_bytes
        }
    };
    Ok(encode_raw(msg_type, &payload_bytes))
}

fn encode_control_payload(frame: &Frame) -> crate::error::Result<Bytes> {
    let json = match frame {
        Frame::Hello(m) => serde_json::to_vec(m),
        Frame::HelloAck(m) => serde_json::to_vec(m),
        Frame::AuthRequest(m) => serde_json::to_vec(m),
        Frame::AuthResponse(m) => serde_json::to_vec(m),
        Frame::SessionCreate(m) => serde_json::to_vec(m),
        Frame::SessionAck(m) => serde_json::to_vec(m),
        Frame::SessionDestroy(m) => serde_json::to_vec(m),
        Frame::SessionResume(m) => serde_json::to_vec(m),
        Frame::SessionAutoCreate(m) => serde_json::to_vec(m),
        Frame::X11Connect(m) => serde_json::to_vec(m),
        Frame::X11Disconnect(m) => serde_json::to_vec(m),
        Frame::FlowControl(m) => serde_json::to_vec(m),
        Frame::Error(m) => serde_json::to_vec(m),
        Frame::DataX11(_) | Frame::CompressedDataX11 { .. } => unreachable!(),
        Frame::Heartbeat | Frame::HeartbeatAck => unreachable!(),
    }
    .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?;
    Ok(Bytes::from(json))
}

fn encode_raw(msg_type: u8, payload: &[u8]) -> Bytes {
    let len: u32 = payload
        .len()
        .try_into()
        .expect("payload already validated against MAX_FRAME_SIZE");
    let mut buf = BytesMut::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&MAGIC_BYTES);
    buf.extend_from_slice(&[msg_type]);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    buf.freeze()
}

pub fn decode_frame(data: &[u8]) -> crate::error::Result<Option<(Frame, usize)>> {
    if data.len() < FRAME_HEADER_SIZE {
        return Ok(None);
    }
    if data[0..4] != MAGIC_BYTES {
        return Err(crate::error::Rx11Error::Protocol(
            "Invalid magic bytes".into(),
        ));
    }
    let msg_type = data[4];
    let payload_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
    if payload_len > MAX_FRAME_SIZE {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "Frame payload too large: {} bytes (max {})",
            payload_len, MAX_FRAME_SIZE
        )));
    }
    let total = FRAME_HEADER_SIZE + payload_len;
    if data.len() < total {
        return Ok(None);
    }
    let payload = &data[FRAME_HEADER_SIZE..total];
    let frame = match msg_type {
        0x01 => Frame::Hello(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x02 => Frame::HelloAck(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x03 => Frame::AuthRequest(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x04 => Frame::AuthResponse(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x10 => {
            let msg: SessionCreateMessage = serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?;
            validate_auth_fields(&msg.auth_name, &msg.auth_data)?;
            Frame::SessionCreate(msg)
        }
        0x11 => Frame::SessionAck(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x12 => Frame::SessionDestroy(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x13 => Frame::SessionResume(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x14 => {
            let msg: SessionAutoCreateMessage = serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?;
            validate_auth_fields(&msg.auth_name, &msg.auth_data)?;
            Frame::SessionAutoCreate(msg)
        }
        0x20 => {
            if payload.len() < 8 {
                return Err(crate::error::Rx11Error::Protocol(
                    "DataX11 payload too short".into(),
                ));
            }
            let connection_id =
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let sequence_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            Frame::DataX11(X11DataMessage {
                connection_id,
                sequence_id,
                data: Bytes::copy_from_slice(&payload[8..]),
            })
        }
        0x21 => {
            if payload.len() < 12 {
                return Err(crate::error::Rx11Error::Protocol(
                    "CompressedDataX11 payload too short".into(),
                ));
            }
            let connection_id =
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let sequence_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            let original_len =
                u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]) as usize;
            Frame::CompressedDataX11 {
                connection_id,
                sequence_id,
                original_len,
                data: Bytes::copy_from_slice(&payload[12..]),
            }
        }
        0x22 => Frame::X11Connect(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x23 => Frame::X11Disconnect(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0x30 => {
            if !payload.is_empty() {
                return Err(crate::error::Rx11Error::Protocol(
                    "Heartbeat frame must have empty payload".into(),
                ));
            }
            Frame::Heartbeat
        }
        0x31 => {
            if !payload.is_empty() {
                return Err(crate::error::Rx11Error::Protocol(
                    "HeartbeatAck frame must have empty payload".into(),
                ));
            }
            Frame::HeartbeatAck
        }
        0x40 => Frame::FlowControl(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        0xFF => Frame::Error(
            serde_json::from_slice(payload)
                .map_err(|e| crate::error::Rx11Error::Protocol(e.to_string()))?,
        ),
        _ => {
            return Err(crate::error::Rx11Error::Protocol(format!(
                "Unknown frame type: 0x{:02x}",
                msg_type
            )))
        }
    };
    Ok(Some((frame, total)))
}

pub const fn frame_header_size() -> usize {
    FRAME_HEADER_SIZE
}

const MAX_TOKEN_LEN: usize = 256;
const MAX_SESSION_ID_LEN: usize = 256;
const MAX_AUTH_NAME_LEN: usize = 256;
const MAX_AUTH_DATA_LEN: usize = 4096;

pub fn validate_display(display: u16) -> crate::error::Result<()> {
    if display > MAX_DISPLAY_NUMBER {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "Display number must be 0-{}, got {}",
            MAX_DISPLAY_NUMBER, display
        )));
    }
    Ok(())
}

pub fn validate_token_len(token: &str) -> crate::error::Result<()> {
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "Token length must be 1-{} bytes, got {}",
            MAX_TOKEN_LEN,
            token.len()
        )));
    }
    Ok(())
}

pub fn validate_session_id(session_id: &str) -> crate::error::Result<()> {
    if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_LEN {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "Session ID length must be 1-{} bytes, got {}",
            MAX_SESSION_ID_LEN,
            session_id.len()
        )));
    }
    Ok(())
}

fn validate_auth_fields(auth_name: &str, auth_data: &[u8]) -> crate::error::Result<()> {
    if auth_name.len() > MAX_AUTH_NAME_LEN {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "auth_name too long: {} bytes (max {})",
            auth_name.len(),
            MAX_AUTH_NAME_LEN
        )));
    }
    if auth_data.len() > MAX_AUTH_DATA_LEN {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "auth_data too long: {} bytes (max {})",
            auth_data.len(),
            MAX_AUTH_DATA_LEN
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_hello() {
        let frame = Frame::Hello(HelloMessage {
            version: 1,
            mode: ConnectionMode::Client,
            resume_session_id: None,
            compression_algos: vec![],
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::Hello(m) => {
                assert_eq!(m.version, 1);
                assert!(matches!(m.mode, ConnectionMode::Client));
                assert!(m.resume_session_id.is_none());
            }
            _ => panic!("Expected Hello frame"),
        }
    }

    #[test]
    fn test_encode_decode_hello_ack() {
        let frame = Frame::HelloAck(HelloAckMessage {
            version: 1,
            session_id: "test-session".into(),
            success: true,
            error_msg: None,
            compression: None,
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::HelloAck(m) => {
                assert_eq!(m.session_id, "test-session");
                assert!(m.success);
            }
            _ => panic!("Expected HelloAck frame"),
        }
    }

    #[test]
    fn test_encode_decode_auth_request() {
        let frame = Frame::AuthRequest(AuthRequestMessage {
            token: "my-token".into(),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::AuthRequest(m) => assert_eq!(m.token, "my-token"),
            _ => panic!("Expected AuthRequest frame"),
        }
    }

    #[test]
    fn test_encode_decode_auth_response() {
        let frame = Frame::AuthResponse(AuthResponseMessage {
            success: false,
            error_msg: Some("bad token".into()),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::AuthResponse(m) => {
                assert!(!m.success);
                assert_eq!(m.error_msg.unwrap(), "bad token");
            }
            _ => panic!("Expected AuthResponse frame"),
        }
    }

    #[test]
    fn test_encode_decode_session_create() {
        let frame = Frame::SessionCreate(SessionCreateMessage {
            display: 0,
            auth_name: "MIT-MAGIC-COOKIE-1".into(),
            auth_data: vec![1, 2, 3, 4],
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::SessionCreate(m) => {
                assert_eq!(m.display, 0);
                assert_eq!(m.auth_data, vec![1, 2, 3, 4]);
            }
            _ => panic!("Expected SessionCreate frame"),
        }
    }

    #[test]
    fn test_encode_decode_data_x11() {
        let frame = Frame::DataX11(X11DataMessage {
            connection_id: 42,
            sequence_id: 7,
            data: Bytes::from_static(&[0xAA, 0xBB, 0xCC]),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::DataX11(m) => {
                assert_eq!(m.connection_id, 42);
                assert_eq!(m.sequence_id, 7);
                assert_eq!(&m.data[..], &[0xAA, 0xBB, 0xCC]);
            }
            _ => panic!("Expected DataX11 frame"),
        }
    }

    #[test]
    fn test_encode_decode_x11_connect() {
        let frame = Frame::X11Connect(X11ConnectMessage {
            display: 5,
            connection_id: 100,
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::X11Connect(m) => {
                assert_eq!(m.display, 5);
                assert_eq!(m.connection_id, 100);
            }
            _ => panic!("Expected X11Connect frame"),
        }
    }

    #[test]
    fn test_encode_decode_heartbeat() {
        let frame = Frame::Heartbeat;
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        assert!(matches!(decoded, Frame::Heartbeat));
    }

    #[test]
    fn test_encode_decode_error() {
        let frame = Frame::Error(ErrorMessage {
            code: 1,
            message: "test error".into(),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::Error(m) => {
                assert_eq!(m.code, 1);
                assert_eq!(m.message, "test error");
            }
            _ => panic!("Expected Error frame"),
        }
    }

    #[test]
    fn test_encode_decode_flow_control() {
        let frame = Frame::FlowControl(FlowControlMessage {
            action: FlowControlAction::Pause,
            connection_id: Some(42),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::FlowControl(m) => {
                assert_eq!(m.action, FlowControlAction::Pause);
                assert_eq!(m.connection_id, Some(42));
            }
            _ => panic!("Expected FlowControl frame"),
        }
    }

    #[test]
    fn test_encode_decode_compressed_data_x11() {
        let frame = Frame::CompressedDataX11 {
            connection_id: 42,
            sequence_id: 5,
            original_len: 1000,
            data: Bytes::from_static(&[0x01, 0x02, 0x03]),
        };
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::CompressedDataX11 {
                connection_id,
                sequence_id,
                original_len,
                data,
            } => {
                assert_eq!(connection_id, 42);
                assert_eq!(sequence_id, 5);
                assert_eq!(original_len, 1000);
                assert_eq!(&data[..], &[0x01, 0x02, 0x03]);
            }
            _ => panic!("Expected CompressedDataX11 frame"),
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
    fn test_msg_type_roundtrip() {
        let frame = Frame::SessionDestroy(SessionDestroyMessage { display: 3 });
        assert_eq!(frame.msg_type(), MessageType::SessionDestroy);

        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::SessionDestroy(m) => assert_eq!(m.display, 3),
            _ => panic!("Expected SessionDestroy frame"),
        }
    }
}
