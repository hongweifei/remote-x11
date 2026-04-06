use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::compress::CompressionAlgo;
use crate::error::{Result, Rx11Error};
use crate::types::{validate_auth_fields, ConnectionId, DisplayNumber, SessionId, Token};

pub const PROTOCOL_VERSION: u32 = 3;
pub const MAGIC_BYTES: [u8; 4] = [b'R', b'X', b'1', b'1'];
pub const DEFAULT_RELAY_PORT: u16 = 7000;
pub const DEFAULT_X11_PORT: u16 = 6000;
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
    pub resume_session_id: Option<SessionId>,
    #[serde(default = "default_compression_algos")]
    pub compression_algos: Vec<CompressionAlgo>,
}

fn default_compression_algos() -> Vec<CompressionAlgo> {
    CompressionAlgo::ALL.to_vec()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAckMessage {
    pub version: u32,
    pub session_id: SessionId,
    pub success: bool,
    pub error_msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionAlgo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ConnectionMode {
    Server,
    Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequestMessage {
    pub token: Token,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponseMessage {
    pub success: bool,
    pub error_msg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateMessage {
    pub display: DisplayNumber,
    pub auth_name: String,
    pub auth_data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAckMessage {
    pub display: DisplayNumber,
    pub success: bool,
    pub error_msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDestroyMessage {
    pub display: DisplayNumber,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeMessage {
    pub session_id: SessionId,
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
    pub connection_id: ConnectionId,
    pub sequence_id: u32,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedX11DataMessage {
    pub connection_id: ConnectionId,
    pub sequence_id: u32,
    pub original_len: usize,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X11ConnectMessage {
    pub display: DisplayNumber,
    pub connection_id: ConnectionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X11DisconnectMessage {
    pub display: DisplayNumber,
    pub connection_id: ConnectionId,
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
    pub connection_id: Option<ConnectionId>,
}

pub enum PayloadEncoding {
    Json,
    Binary,
    Empty,
}

pub trait FramePayload: serde::Serialize {
    const MSG_TYPE: MessageType;
    const ENCODING: PayloadEncoding;

    fn encode_payload(&self) -> Result<Bytes> {
        match Self::ENCODING {
            PayloadEncoding::Json => {
                let json =
                    serde_json::to_vec(self).map_err(|e| Rx11Error::Protocol(e.to_string()))?;
                if json.len() > MAX_FRAME_SIZE {
                    return Err(Rx11Error::Protocol(format!(
                        "Frame payload too large: {} bytes (max {})",
                        json.len(),
                        MAX_FRAME_SIZE
                    )));
                }
                Ok(Bytes::from(json))
            }
            PayloadEncoding::Empty => Ok(Bytes::new()),
            PayloadEncoding::Binary => Err(Rx11Error::Protocol(
                "Binary encoding requires custom encode_payload implementation".into(),
            )),
        }
    }

    fn decode_payload(data: &[u8]) -> Result<Self>
    where
        Self: serde::de::DeserializeOwned + Sized,
    {
        serde_json::from_slice(data).map_err(|e| Rx11Error::Protocol(e.to_string()))
    }
}

impl FramePayload for HelloMessage {
    const MSG_TYPE: MessageType = MessageType::Hello;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for HelloAckMessage {
    const MSG_TYPE: MessageType = MessageType::HelloAck;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for AuthRequestMessage {
    const MSG_TYPE: MessageType = MessageType::AuthRequest;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for AuthResponseMessage {
    const MSG_TYPE: MessageType = MessageType::AuthResponse;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for SessionCreateMessage {
    const MSG_TYPE: MessageType = MessageType::SessionCreate;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for SessionAckMessage {
    const MSG_TYPE: MessageType = MessageType::SessionAck;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for SessionDestroyMessage {
    const MSG_TYPE: MessageType = MessageType::SessionDestroy;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for SessionResumeMessage {
    const MSG_TYPE: MessageType = MessageType::SessionResume;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for SessionAutoCreateMessage {
    const MSG_TYPE: MessageType = MessageType::SessionAutoCreate;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for X11ConnectMessage {
    const MSG_TYPE: MessageType = MessageType::X11Connect;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for X11DisconnectMessage {
    const MSG_TYPE: MessageType = MessageType::X11Disconnect;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for FlowControlMessage {
    const MSG_TYPE: MessageType = MessageType::FlowControl;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
}

impl FramePayload for ErrorMessage {
    const MSG_TYPE: MessageType = MessageType::Error;
    const ENCODING: PayloadEncoding = PayloadEncoding::Json;
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
    CompressedDataX11(CompressedX11DataMessage),
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
            Frame::CompressedDataX11(_) => MessageType::CompressedDataX11,
            Frame::X11Connect(_) => MessageType::X11Connect,
            Frame::X11Disconnect(_) => MessageType::X11Disconnect,
            Frame::Heartbeat => MessageType::Heartbeat,
            Frame::HeartbeatAck => MessageType::HeartbeatAck,
            Frame::FlowControl(_) => MessageType::FlowControl,
            Frame::Error(_) => MessageType::Error,
        }
    }

    pub fn encode_json_payload<P: FramePayload>(msg: &P) -> Result<Bytes> {
        msg.encode_payload()
    }

    pub fn decode_json_payload<P: FramePayload + serde::de::DeserializeOwned>(
        data: &[u8],
    ) -> Result<P> {
        P::decode_payload(data)
    }
}

pub fn encode_frame(frame: &Frame) -> Result<Bytes> {
    let msg_type = frame.msg_type() as u8;
    let payload_bytes = match frame {
        Frame::DataX11(m) => {
            if m.data.len() > MAX_FRAME_SIZE {
                return Err(Rx11Error::Protocol(format!(
                    "DataX11 payload too large: {} bytes (max {})",
                    m.data.len(),
                    MAX_FRAME_SIZE
                )));
            }
            let mut buf = BytesMut::with_capacity(4 + 4 + m.data.len());
            buf.extend_from_slice(&m.connection_id.get().to_be_bytes());
            buf.extend_from_slice(&m.sequence_id.to_be_bytes());
            buf.extend_from_slice(&m.data);
            return Ok(encode_raw(msg_type, &buf.freeze()));
        }
        Frame::CompressedDataX11(m) => {
            let len_u32: u32 = (m.original_len)
                .try_into()
                .map_err(|_| Rx11Error::Protocol("original_len exceeds u32".into()))?;
            if m.data.len() > MAX_FRAME_SIZE {
                return Err(Rx11Error::Protocol(format!(
                    "CompressedDataX11 payload too large: {} bytes (max {})",
                    m.data.len(),
                    MAX_FRAME_SIZE
                )));
            }
            let mut buf = BytesMut::with_capacity(4 + 4 + 4 + m.data.len());
            buf.extend_from_slice(&m.connection_id.get().to_be_bytes());
            buf.extend_from_slice(&m.sequence_id.to_be_bytes());
            buf.extend_from_slice(&len_u32.to_be_bytes());
            buf.extend_from_slice(&m.data);
            return Ok(encode_raw(msg_type, &buf.freeze()));
        }
        Frame::Heartbeat | Frame::HeartbeatAck => Bytes::new(),
        _ => {
            let json_bytes = match frame {
                Frame::Hello(m) => Frame::encode_json_payload(m),
                Frame::HelloAck(m) => Frame::encode_json_payload(m),
                Frame::AuthRequest(m) => Frame::encode_json_payload(m),
                Frame::AuthResponse(m) => Frame::encode_json_payload(m),
                Frame::SessionCreate(m) => Frame::encode_json_payload(m),
                Frame::SessionAck(m) => Frame::encode_json_payload(m),
                Frame::SessionDestroy(m) => Frame::encode_json_payload(m),
                Frame::SessionResume(m) => Frame::encode_json_payload(m),
                Frame::SessionAutoCreate(m) => Frame::encode_json_payload(m),
                Frame::X11Connect(m) => Frame::encode_json_payload(m),
                Frame::X11Disconnect(m) => Frame::encode_json_payload(m),
                Frame::FlowControl(m) => Frame::encode_json_payload(m),
                Frame::Error(m) => Frame::encode_json_payload(m),
                Frame::DataX11(_) | Frame::CompressedDataX11 { .. } => unreachable!(),
                Frame::Heartbeat | Frame::HeartbeatAck => unreachable!(),
            }?;
            json_bytes
        }
    };
    Ok(encode_raw(msg_type, &payload_bytes))
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

pub fn decode_frame(data: &[u8]) -> Result<Option<(Frame, usize)>> {
    if data.len() < FRAME_HEADER_SIZE {
        return Ok(None);
    }
    if data[0..4] != MAGIC_BYTES {
        return Err(Rx11Error::Protocol("Invalid magic bytes".into()));
    }
    let msg_type = data[4];
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
    let frame = match msg_type {
        0x01 => Frame::Hello(Frame::decode_json_payload(payload)?),
        0x02 => Frame::HelloAck(Frame::decode_json_payload(payload)?),
        0x03 => Frame::AuthRequest(Frame::decode_json_payload(payload)?),
        0x04 => Frame::AuthResponse(Frame::decode_json_payload(payload)?),
        0x10 => {
            let msg: SessionCreateMessage = Frame::decode_json_payload(payload)?;
            validate_auth_fields(&msg.auth_name, &msg.auth_data)?;
            Frame::SessionCreate(msg)
        }
        0x11 => Frame::SessionAck(Frame::decode_json_payload(payload)?),
        0x12 => Frame::SessionDestroy(Frame::decode_json_payload(payload)?),
        0x13 => Frame::SessionResume(Frame::decode_json_payload(payload)?),
        0x14 => {
            let msg: SessionAutoCreateMessage = Frame::decode_json_payload(payload)?;
            validate_auth_fields(&msg.auth_name, &msg.auth_data)?;
            Frame::SessionAutoCreate(msg)
        }
        0x20 => {
            if payload.len() < 8 {
                return Err(Rx11Error::Protocol("DataX11 payload too short".into()));
            }
            let connection_id =
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let sequence_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            Frame::DataX11(X11DataMessage {
                connection_id: ConnectionId::new(connection_id),
                sequence_id,
                data: Bytes::copy_from_slice(&payload[8..]),
            })
        }
        0x21 => {
            if payload.len() < 12 {
                return Err(Rx11Error::Protocol(
                    "CompressedDataX11 payload too short".into(),
                ));
            }
            let connection_id =
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let sequence_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            let original_len =
                u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]) as usize;
            Frame::CompressedDataX11(CompressedX11DataMessage {
                connection_id: ConnectionId::new(connection_id),
                sequence_id,
                original_len,
                data: Bytes::copy_from_slice(&payload[12..]),
            })
        }
        0x22 => Frame::X11Connect(Frame::decode_json_payload(payload)?),
        0x23 => Frame::X11Disconnect(Frame::decode_json_payload(payload)?),
        0x30 => {
            if !payload.is_empty() {
                return Err(Rx11Error::Protocol(
                    "Heartbeat frame must have empty payload".into(),
                ));
            }
            Frame::Heartbeat
        }
        0x31 => {
            if !payload.is_empty() {
                return Err(Rx11Error::Protocol(
                    "HeartbeatAck frame must have empty payload".into(),
                ));
            }
            Frame::HeartbeatAck
        }
        0x40 => Frame::FlowControl(Frame::decode_json_payload(payload)?),
        0xFF => Frame::Error(Frame::decode_json_payload(payload)?),
        _ => {
            return Err(Rx11Error::Protocol(format!(
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
            session_id: SessionId::new("test-session".to_string()).unwrap(),
            success: true,
            error_msg: None,
            compression: None,
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::HelloAck(m) => {
                assert_eq!(m.session_id.as_str(), "test-session");
                assert!(m.success);
            }
            _ => panic!("Expected HelloAck frame"),
        }
    }

    #[test]
    fn test_encode_decode_auth_request() {
        let frame = Frame::AuthRequest(AuthRequestMessage {
            token: Token::new("my-token".to_string()).unwrap(),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::AuthRequest(m) => assert_eq!(m.token.as_str(), "my-token"),
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
            display: DisplayNumber::new(0).unwrap(),
            auth_name: "MIT-MAGIC-COOKIE-1".into(),
            auth_data: vec![1, 2, 3, 4],
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::SessionCreate(m) => {
                assert_eq!(m.display.get(), 0);
                assert_eq!(m.auth_data, vec![1, 2, 3, 4]);
            }
            _ => panic!("Expected SessionCreate frame"),
        }
    }

    #[test]
    fn test_encode_decode_data_x11() {
        let frame = Frame::DataX11(X11DataMessage {
            connection_id: ConnectionId::new(42),
            sequence_id: 7,
            data: Bytes::from_static(&[0xAA, 0xBB, 0xCC]),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::DataX11(m) => {
                assert_eq!(m.connection_id.get(), 42);
                assert_eq!(m.sequence_id, 7);
                assert_eq!(&m.data[..], &[0xAA, 0xBB, 0xCC]);
            }
            _ => panic!("Expected DataX11 frame"),
        }
    }

    #[test]
    fn test_encode_decode_x11_connect() {
        let frame = Frame::X11Connect(X11ConnectMessage {
            display: DisplayNumber::new(5).unwrap(),
            connection_id: ConnectionId::new(100),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::X11Connect(m) => {
                assert_eq!(m.display.get(), 5);
                assert_eq!(m.connection_id.get(), 100);
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
            connection_id: Some(ConnectionId::new(42)),
        });
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::FlowControl(m) => {
                assert_eq!(m.action, FlowControlAction::Pause);
                assert_eq!(m.connection_id.unwrap().get(), 42);
            }
            _ => panic!("Expected FlowControl frame"),
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
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::CompressedDataX11(m) => {
                assert_eq!(m.connection_id.get(), 42);
                assert_eq!(m.sequence_id, 5);
                assert_eq!(m.original_len, 1000);
                assert_eq!(&m.data[..], &[0x01, 0x02, 0x03]);
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
        let frame = Frame::SessionDestroy(SessionDestroyMessage {
            display: DisplayNumber::new(3).unwrap(),
        });
        assert_eq!(frame.msg_type(), MessageType::SessionDestroy);

        let encoded = encode_frame(&frame).unwrap();
        let (decoded, _) = decode_frame(&encoded).unwrap().unwrap();
        match decoded {
            Frame::SessionDestroy(m) => assert_eq!(m.display.get(), 3),
            _ => panic!("Expected SessionDestroy frame"),
        }
    }
}
