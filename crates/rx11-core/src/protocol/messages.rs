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
    /// Feature names advertised by the sender; unknown names are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
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
    /// Negotiated (intersected) capability names the server will honor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
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
pub struct StreamDataMessage {
    pub connection_id: ConnectionId,
    pub sequence_id: u32,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedStreamDataMessage {
    pub connection_id: ConnectionId,
    pub sequence_id: u32,
    pub original_len: usize,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalStreamDataMessage {
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
pub struct CompressedIncrementalStreamDataMessage {
    pub connection_id: ConnectionId,
    pub sequence_id: u32,
    pub original_len: usize,
    pub data: Bytes,
}

/// Sent by the receiver when an incremental diff could not be applied
/// against its cache, asking the sender to fall back to full frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamResyncRequestMessage {
    pub connection_id: ConnectionId,
}

/// Identifies what flows over an opened channel. Wire form is the kind's
/// lowercase name; unknown kinds decode faithfully so a future clipboard or
/// file channel passes through without breaking existing peers (opening is
/// still gated by the matching capability).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelKind(String);

impl ChannelKind {
    pub const X11_NAME: &'static str = "x11";
    pub const AUDIO_NAME: &'static str = "audio";

    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn x11() -> Self {
        Self(Self::X11_NAME.to_string())
    }

    pub fn is_x11(&self) -> bool {
        self.0 == Self::X11_NAME
    }

    pub fn audio() -> Self {
        Self(Self::AUDIO_NAME.to_string())
    }

    pub fn is_audio(&self) -> bool {
        self.0 == Self::AUDIO_NAME
    }

    pub fn name(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opens a multiplexed channel. Kind-specific parameters (X11's `display`)
/// travel as optional fields; consumers must ignore fields their kind does
/// not define.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelOpenMessage {
    pub connection_id: ConnectionId,
    pub kind: ChannelKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayNumber>,
}

/// Closes a channel by id; the peer removes all per-channel state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCloseMessage {
    pub connection_id: ConnectionId,
}

/// Bulk flow-control edge. `Pause` means "stop delivering bulk X11 bytes to
/// me": the receiver has accepted them but its final consumer (local X
/// server, or the application socket on the other leg) is stalled. Control
/// frames and the audio real-time lane are exempt from any pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FlowControlAction {
    Pause = 0,
    Resume = 1,
}

/// One channel's flow state. `connection_id: None` is the bulk-scope form:
/// receivers apply it to every x11 channel they currently hold (a sender
/// never emits it — per-channel is the protocol).
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

impl CompressedPayload for CompressedStreamDataMessage {
    fn compressed_data(&self) -> &Bytes {
        &self.data
    }

    fn original_len(&self) -> usize {
        self.original_len
    }
}

impl CompressedPayload for CompressedIncrementalStreamDataMessage {
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
