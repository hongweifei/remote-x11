//! Payload structs for every protocol frame.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::compress::CompressionAlgo;
use crate::error::Result;
use crate::types::{validate_auth_fields, ConnectionId, DisplayNumber, SessionId, Token};

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

/// Sent by the receiver when an incremental diff could not be applied
/// against its cache, asking the sender to fall back to full frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalRequestFullMessage {
    pub connection_id: ConnectionId,
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

/// Common accessors for frames carrying a compressed payload, so that
/// decompression helpers do not need per-message variants.
pub trait CompressedPayload {
    fn compressed_data(&self) -> &Bytes;
    fn original_len(&self) -> usize;
}

impl CompressedPayload for CompressedX11DataMessage {
    fn compressed_data(&self) -> &Bytes {
        &self.data
    }

    fn original_len(&self) -> usize {
        self.original_len
    }
}

impl CompressedPayload for CompressedIncrementalX11DataMessage {
    fn compressed_data(&self) -> &Bytes {
        &self.data
    }

    fn original_len(&self) -> usize {
        self.original_len
    }
}

pub(crate) fn validate_session_create(msg: &SessionCreateMessage) -> Result<()> {
    validate_auth_fields(&msg.auth_name, &msg.auth_data)
}

pub(crate) fn validate_session_auto_create(msg: &SessionAutoCreateMessage) -> Result<()> {
    validate_auth_fields(&msg.auth_name, &msg.auth_data)
}
