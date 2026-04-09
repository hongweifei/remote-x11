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
    IncrementalDataX11 = 0x24,
    CompressedIncrementalDataX11 = 0x25,
    X11Connect = 0x22,
    X11Disconnect = 0x23,
    Heartbeat = 0x30,
    HeartbeatAck = 0x31,
    FlowControl = 0x40,
    Error = 0xFF,
}

impl TryFrom<u8> for MessageType {
    type Error = Rx11Error;

    fn try_from(value: u8) -> std::result::Result<Self, Rx11Error> {
        match value {
            0x01 => Ok(MessageType::Hello),
            0x02 => Ok(MessageType::HelloAck),
            0x03 => Ok(MessageType::AuthRequest),
            0x04 => Ok(MessageType::AuthResponse),
            0x10 => Ok(MessageType::SessionCreate),
            0x11 => Ok(MessageType::SessionAck),
            0x12 => Ok(MessageType::SessionDestroy),
            0x13 => Ok(MessageType::SessionResume),
            0x14 => Ok(MessageType::SessionAutoCreate),
            0x20 => Ok(MessageType::DataX11),
            0x21 => Ok(MessageType::CompressedDataX11),
            0x24 => Ok(MessageType::IncrementalDataX11),
            0x25 => Ok(MessageType::CompressedIncrementalDataX11),
            0x22 => Ok(MessageType::X11Connect),
            0x23 => Ok(MessageType::X11Disconnect),
            0x30 => Ok(MessageType::Heartbeat),
            0x31 => Ok(MessageType::HeartbeatAck),
            0x40 => Ok(MessageType::FlowControl),
            0xFF => Ok(MessageType::Error),
            _ => Err(Rx11Error::Protocol(format!(
                "Unknown message type: 0x{:02x}",
                value
            ))),
        }
    }
}

impl std::fmt::Display for MessageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageType::Hello => f.write_str("Hello"),
            MessageType::HelloAck => f.write_str("HelloAck"),
            MessageType::AuthRequest => f.write_str("AuthRequest"),
            MessageType::AuthResponse => f.write_str("AuthResponse"),
            MessageType::SessionCreate => f.write_str("SessionCreate"),
            MessageType::SessionAck => f.write_str("SessionAck"),
            MessageType::SessionDestroy => f.write_str("SessionDestroy"),
            MessageType::SessionResume => f.write_str("SessionResume"),
            MessageType::SessionAutoCreate => f.write_str("SessionAutoCreate"),
            MessageType::DataX11 => f.write_str("DataX11"),
            MessageType::CompressedDataX11 => f.write_str("CompressedDataX11"),
            MessageType::IncrementalDataX11 => f.write_str("IncrementalDataX11"),
            MessageType::CompressedIncrementalDataX11 => f.write_str("CompressedIncrementalDataX11"),
            MessageType::X11Connect => f.write_str("X11Connect"),
            MessageType::X11Disconnect => f.write_str("X11Disconnect"),
            MessageType::Heartbeat => f.write_str("Heartbeat"),
            MessageType::HeartbeatAck => f.write_str("HeartbeatAck"),
            MessageType::FlowControl => f.write_str("FlowControl"),
            MessageType::Error => f.write_str("Error"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ConnectionMode {
    Server,
    Client,
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
pub struct IncrementalX11DataMessage {
    pub connection_id: ConnectionId,
    pub sequence_id: u32,
    pub base_sequence_id: u32,
    pub total_len: usize,
    pub chunks: Vec<IncrementalChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalChunk {
    pub offset: usize,
    pub length: usize,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedIncrementalX11DataMessage {
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

pub trait BinaryMessageCodec {
    fn encode_payload(&self) -> Result<Bytes>;
    fn decode_payload(data: &[u8]) -> Result<Self>
    where
        Self: Sized;
}

impl BinaryMessageCodec for X11DataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        if self.data.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "DataX11 payload too large: {} bytes (max {})",
                self.data.len(),
                MAX_FRAME_SIZE
            )));
        }
        let mut buf = BytesMut::with_capacity(4 + 4 + self.data.len());
        buf.extend_from_slice(&self.connection_id.get().to_be_bytes());
        buf.extend_from_slice(&self.sequence_id.to_be_bytes());
        buf.extend_from_slice(&self.data);
        Ok(buf.freeze())
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        const MIN_LEN: usize = 8;
        if data.len() < MIN_LEN {
            return Err(Rx11Error::Protocol(format!(
                "DataX11 payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }
        let connection_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let sequence_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        Ok(Self {
            connection_id: ConnectionId::new(connection_id),
            sequence_id,
            data: Bytes::copy_from_slice(&data[MIN_LEN..]),
        })
    }
}

impl BinaryMessageCodec for CompressedX11DataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        let len_u32: u32 = (self.original_len)
            .try_into()
            .map_err(|_| Rx11Error::Protocol("original_len exceeds u32".into()))?;
        if self.data.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "CompressedDataX11 payload too large: {} bytes (max {})",
                self.data.len(),
                MAX_FRAME_SIZE
            )));
        }
        let mut buf = BytesMut::with_capacity(4 + 4 + 4 + self.data.len());
        buf.extend_from_slice(&self.connection_id.get().to_be_bytes());
        buf.extend_from_slice(&self.sequence_id.to_be_bytes());
        buf.extend_from_slice(&len_u32.to_be_bytes());
        buf.extend_from_slice(&self.data);
        Ok(buf.freeze())
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        const MIN_LEN: usize = 12;
        if data.len() < MIN_LEN {
            return Err(Rx11Error::Protocol(format!(
                "CompressedDataX11 payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }
        let connection_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let sequence_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let original_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        Ok(Self {
            connection_id: ConnectionId::new(connection_id),
            sequence_id,
            original_len,
            data: Bytes::copy_from_slice(&data[MIN_LEN..]),
        })
    }
}

impl BinaryMessageCodec for IncrementalX11DataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();
        
        buf.extend_from_slice(&self.connection_id.get().to_be_bytes());
        buf.extend_from_slice(&self.sequence_id.to_be_bytes());
        buf.extend_from_slice(&self.base_sequence_id.to_be_bytes());
        
        let total_len_u32: u32 = self.total_len
            .try_into()
            .map_err(|_| Rx11Error::Protocol("total_len exceeds u32".into()))?;
        buf.extend_from_slice(&total_len_u32.to_be_bytes());
        
        let chunk_count_u16: u16 = self.chunks.len()
            .try_into()
            .map_err(|_| Rx11Error::Protocol("too many chunks".into()))?;
        buf.extend_from_slice(&chunk_count_u16.to_be_bytes());
        
        for chunk in &self.chunks {
            let offset_u32: u32 = chunk.offset
                .try_into()
                .map_err(|_| Rx11Error::Protocol("chunk offset exceeds u32".into()))?;
            let length_u32: u32 = chunk.length
                .try_into()
                .map_err(|_| Rx11Error::Protocol("chunk length exceeds u32".into()))?;
            
            buf.extend_from_slice(&offset_u32.to_be_bytes());
            buf.extend_from_slice(&length_u32.to_be_bytes());
            buf.extend_from_slice(&chunk.data);
        }
        
        if buf.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "IncrementalDataX11 payload too large: {} bytes (max {})",
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
                "IncrementalDataX11 payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }
        
        let mut offset = 0;
        
        let connection_id = u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        
        let sequence_id = u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        
        let base_sequence_id = u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        
        let total_len = u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
        offset += 4;
        
        let chunk_count = u16::from_be_bytes([data[offset], data[offset+1]]) as usize;
        offset += 2;
        
        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            if offset + 8 > data.len() {
                return Err(Rx11Error::Protocol("Invalid IncrementalDataX11 chunk header".into()));
            }
            
            let chunk_offset = u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
            offset += 4;
            
            let chunk_length = u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
            offset += 4;
            
            if offset + chunk_length > data.len() {
                return Err(Rx11Error::Protocol("Invalid IncrementalDataX11 chunk data".into()));
            }
            
            let chunk_data = Bytes::copy_from_slice(&data[offset..offset+chunk_length]);
            offset += chunk_length;
            
            chunks.push(IncrementalChunk {
                offset: chunk_offset,
                length: chunk_length,
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

impl BinaryMessageCodec for CompressedIncrementalX11DataMessage {
    fn encode_payload(&self) -> Result<Bytes> {
        let len_u32: u32 = (self.original_len)
            .try_into()
            .map_err(|_| Rx11Error::Protocol("original_len exceeds u32".into()))?;
        if self.data.len() > MAX_FRAME_SIZE {
            return Err(Rx11Error::Protocol(format!(
                "CompressedIncrementalDataX11 payload too large: {} bytes (max {})",
                self.data.len(),
                MAX_FRAME_SIZE
            )));
        }
        let mut buf = BytesMut::with_capacity(4 + 4 + 4 + self.data.len());
        buf.extend_from_slice(&self.connection_id.get().to_be_bytes());
        buf.extend_from_slice(&self.sequence_id.to_be_bytes());
        buf.extend_from_slice(&len_u32.to_be_bytes());
        buf.extend_from_slice(&self.data);
        Ok(buf.freeze())
    }

    fn decode_payload(data: &[u8]) -> Result<Self> {
        const MIN_LEN: usize = 12;
        if data.len() < MIN_LEN {
            return Err(Rx11Error::Protocol(format!(
                "CompressedIncrementalDataX11 payload too short: {} bytes (min {})",
                data.len(),
                MIN_LEN
            )));
        }
        let connection_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let sequence_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let original_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        Ok(Self {
            connection_id: ConnectionId::new(connection_id),
            sequence_id,
            original_len,
            data: Bytes::copy_from_slice(&data[MIN_LEN..]),
        })
    }
}

fn validate_session_create(msg: &SessionCreateMessage) -> Result<()> {
    validate_auth_fields(&msg.auth_name, &msg.auth_data)
}

fn validate_session_auto_create(msg: &SessionAutoCreateMessage) -> Result<()> {
    validate_auth_fields(&msg.auth_name, &msg.auth_data)
}

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

pub struct HandshakeResult {
    pub session_id: SessionId,
    pub compression: Option<CompressionAlgo>,
    pub resume_session_id: Option<SessionId>,
}

pub async fn server_handshake(
    transport: &mut crate::transport::Rx11Transport,
    auth_token: &str,
    timeout: std::time::Duration,
) -> Result<HandshakeResult> {
    use crate::auth;

    let hello_frame = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for Hello".into()))?;

    let hello = match hello_frame {
        Frame::Hello(h) => h,
        _ => return Err(Rx11Error::Protocol("Expected Hello frame".into())),
    };

    if hello.version != PROTOCOL_VERSION {
        transport
            .send_frame(&Frame::HelloAck(HelloAckMessage {
                version: PROTOCOL_VERSION,
                session_id: SessionId::new(String::new())?,
                success: false,
                error_msg: Some(format!(
                    "Version mismatch: got {} expected {}",
                    hello.version, PROTOCOL_VERSION
                )),
                compression: None,
            }))
            .await?;
        return Err(Rx11Error::Protocol(format!(
            "Version mismatch: got {} expected {}",
            hello.version, PROTOCOL_VERSION
        )));
    }

    if !matches!(hello.mode, ConnectionMode::Client) {
        transport
            .send_frame(&Frame::HelloAck(HelloAckMessage {
                version: PROTOCOL_VERSION,
                session_id: SessionId::new(String::new())?,
                success: false,
                error_msg: Some("Expected Client mode".into()),
                compression: None,
            }))
            .await?;
        return Err(Rx11Error::Protocol("Expected Client mode".into()));
    }

    let compression = CompressionAlgo::negotiate(&hello.compression_algos, &CompressionAlgo::ALL);
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string())?;

    if let Some(ref sid) = hello.resume_session_id {
        tracing::info!("Client requests session resume: {}", sid);
    }

    transport
        .send_frame(&Frame::HelloAck(HelloAckMessage {
            version: PROTOCOL_VERSION,
            session_id: session_id.clone(),
            success: true,
            error_msg: None,
            compression,
        }))
        .await?;

    let auth_frame = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for AuthRequest".into()))?;

    match auth_frame {
        Frame::AuthRequest(auth_req) => {
            if let Err(e) = Token::new(auth_req.token.0.clone()) {
                transport
                    .send_frame(&Frame::AuthResponse(AuthResponseMessage {
                        success: false,
                        error_msg: Some(format!("Invalid token: {}", e)),
                    }))
                    .await?;
                return Err(Rx11Error::Auth("Invalid token format".into()));
            }
            if !auth::verify_token(auth_req.token.as_str(), auth_token) {
                transport
                    .send_frame(&Frame::AuthResponse(AuthResponseMessage {
                        success: false,
                        error_msg: Some("Invalid token".into()),
                    }))
                    .await?;
                return Err(Rx11Error::Auth("Token mismatch".into()));
            }
            transport
                .send_frame(&Frame::AuthResponse(AuthResponseMessage {
                    success: true,
                    error_msg: None,
                }))
                .await?;
        }
        _ => return Err(Rx11Error::Protocol("Expected AuthRequest frame".into())),
    }

    Ok(HandshakeResult {
        session_id,
        compression,
        resume_session_id: hello.resume_session_id,
    })
}

pub async fn client_handshake(
    transport: &mut crate::transport::Rx11Transport,
    auth_token: &Token,
    resume_session_id: Option<&SessionId>,
    timeout: std::time::Duration,
) -> Result<HandshakeResult> {
    transport
        .send_frame(&Frame::Hello(HelloMessage {
            version: PROTOCOL_VERSION,
            mode: ConnectionMode::Client,
            resume_session_id: resume_session_id.cloned(),
            compression_algos: CompressionAlgo::ALL.to_vec(),
        }))
        .await?;

    let ack = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for HelloAck".into()))?;

    let hello_ack = match ack {
        Frame::HelloAck(h) => h,
        _ => return Err(Rx11Error::Protocol("Expected HelloAck frame".into())),
    };

    if !hello_ack.success {
        return Err(Rx11Error::Protocol(format!(
            "Handshake failed: {}",
            hello_ack.error_msg.as_deref().unwrap_or("unknown error")
        )));
    }

    if hello_ack.version != PROTOCOL_VERSION {
        return Err(Rx11Error::Protocol(format!(
            "Protocol version mismatch: server {} client {}",
            hello_ack.version, PROTOCOL_VERSION
        )));
    }

    transport
        .send_frame(&Frame::AuthRequest(AuthRequestMessage {
            token: auth_token.clone(),
        }))
        .await?;

    let auth_resp = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for AuthResponse".into()))?;

    match auth_resp {
        Frame::AuthResponse(resp) => {
            if !resp.success {
                return Err(Rx11Error::Auth(format!(
                    "Auth failed: {}",
                    resp.error_msg.as_deref().unwrap_or("unknown error")
                )));
            }
        }
        _ => return Err(Rx11Error::Protocol("Expected AuthResponse frame".into())),
    }

    Ok(HandshakeResult {
        session_id: hello_ack.session_id,
        compression: hello_ack.compression,
        resume_session_id: resume_session_id.cloned(),
    })
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

    #[test]
    fn test_message_type_try_from_valid() {
        assert_eq!(MessageType::try_from(0x01).unwrap(), MessageType::Hello);
        assert_eq!(MessageType::try_from(0x02).unwrap(), MessageType::HelloAck);
        assert_eq!(
            MessageType::try_from(0x10).unwrap(),
            MessageType::SessionCreate
        );
        assert_eq!(MessageType::try_from(0x20).unwrap(), MessageType::DataX11);
        assert_eq!(MessageType::try_from(0xFF).unwrap(), MessageType::Error);
    }

    #[test]
    fn test_message_type_try_from_unknown() {
        assert!(MessageType::try_from(0x00).is_err());
        assert!(MessageType::try_from(0xFE).is_err());
        assert!(MessageType::try_from(0x50).is_err());
    }

    #[test]
    fn test_message_type_display() {
        assert_eq!(format!("{}", MessageType::Hello), "Hello");
        assert_eq!(format!("{}", MessageType::DataX11), "DataX11");
        assert_eq!(format!("{}", MessageType::Error), "Error");
    }

    #[test]
    fn test_message_type_roundtrip_u8() {
        for byte in [
            0x01u8, 0x02, 0x03, 0x04, 0x10, 0x11, 0x12, 0x13, 0x14, 0x20, 0x21, 0x22, 0x23, 0x30,
            0x31, 0x40, 0xFF,
        ] {
            let mt = MessageType::try_from(byte).unwrap();
            assert_eq!(mt as u8, byte, "roundtrip failed for 0x{:02x}", byte);
        }
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
