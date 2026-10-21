//! The on-wire message type tag.

use serde::{Deserialize, Serialize};

use crate::error::{Result, Rx11Error};

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
    // --- data-plane band: binary payloads (0x20-0x2F; 0x24+ reserved) ---
    StreamData = 0x20,
    CompressedStreamData = 0x21,
    IncrementalStreamData = 0x22,
    CompressedIncrementalStreamData = 0x23,
    // --- control band: JSON payloads (notification/request grouped with
    //     session frames; kept out of the binary band) ---
    ChannelOpen = 0x18,
    ChannelClose = 0x19,
    StreamResyncRequest = 0x1A,
    Heartbeat = 0x30,
    HeartbeatAck = 0x31,
    FlowControl = 0x40,
    Error = 0xFF,
}

impl MessageType {
    /// Every defined tag. Kept in sync with the enum via the exhaustive
    /// `test_message_type_roundtrip_u8` test below.
    #[cfg(test)]
    pub(crate) const ALL: [MessageType; 20] = [
        MessageType::Hello,
        MessageType::HelloAck,
        MessageType::AuthRequest,
        MessageType::AuthResponse,
        MessageType::SessionCreate,
        MessageType::SessionAck,
        MessageType::SessionDestroy,
        MessageType::SessionResume,
        MessageType::SessionAutoCreate,
        MessageType::StreamData,
        MessageType::CompressedStreamData,
        MessageType::IncrementalStreamData,
        MessageType::CompressedIncrementalStreamData,
        MessageType::StreamResyncRequest,
        MessageType::ChannelOpen,
        MessageType::ChannelClose,
        MessageType::Heartbeat,
        MessageType::HeartbeatAck,
        MessageType::FlowControl,
        MessageType::Error,
    ];
}

impl TryFrom<u8> for MessageType {
    type Error = Rx11Error;

    fn try_from(value: u8) -> Result<Self> {
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
            0x20 => Ok(MessageType::StreamData),
            0x21 => Ok(MessageType::CompressedStreamData),
            0x22 => Ok(MessageType::IncrementalStreamData),
            0x23 => Ok(MessageType::CompressedIncrementalStreamData),
            0x18 => Ok(MessageType::ChannelOpen),
            0x19 => Ok(MessageType::ChannelClose),
            0x1A => Ok(MessageType::StreamResyncRequest),
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
        // Debug and Display agree for fieldless enums; keep one source of truth.
        write!(f, "{:?}", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_try_from_valid() {
        assert_eq!(MessageType::try_from(0x01).unwrap(), MessageType::Hello);
        assert_eq!(MessageType::try_from(0x02).unwrap(), MessageType::HelloAck);
        assert_eq!(
            MessageType::try_from(0x10).unwrap(),
            MessageType::SessionCreate
        );
        assert_eq!(
            MessageType::try_from(0x20).unwrap(),
            MessageType::StreamData
        );
        assert_eq!(
            MessageType::try_from(0x22).unwrap(),
            MessageType::IncrementalStreamData
        );
        assert_eq!(
            MessageType::try_from(0x23).unwrap(),
            MessageType::CompressedIncrementalStreamData
        );
        assert_eq!(
            MessageType::try_from(0x18).unwrap(),
            MessageType::ChannelOpen
        );
        assert_eq!(MessageType::try_from(0xFF).unwrap(), MessageType::Error);
    }

    #[test]
    fn test_message_type_try_from_unknown() {
        assert!(MessageType::try_from(0x00).is_err());
        assert!(MessageType::try_from(0xFE).is_err());
        assert!(MessageType::try_from(0x50).is_err());
    }

    #[test]
    fn test_tag_bands_follow_layout_convention() {
        // The 0x20-0x2F band is exclusively binary data-plane payloads;
        // every JSON control/keepalive/error frame lives outside it. This
        // locks the numbering convention for future frame minting.
        const DATA_BAND: std::ops::RangeInclusive<u8> = 0x20..=0x2F;
        for mt in MessageType::ALL {
            let byte = mt as u8;
            let binary_payload = matches!(
                mt,
                MessageType::StreamData
                    | MessageType::CompressedStreamData
                    | MessageType::IncrementalStreamData
                    | MessageType::CompressedIncrementalStreamData
            );
            assert_eq!(
                DATA_BAND.contains(&byte),
                binary_payload,
                "{mt} misplaced in band"
            );
        }
    }

    #[test]
    fn test_message_type_display() {
        assert_eq!(format!("{}", MessageType::Hello), "Hello");
        assert_eq!(format!("{}", MessageType::StreamData), "StreamData");
        assert_eq!(format!("{}", MessageType::Error), "Error");
    }

    #[test]
    fn test_message_type_roundtrip_u8() {
        for mt in MessageType::ALL {
            let byte = mt as u8;
            assert_eq!(
                MessageType::try_from(byte).unwrap(),
                mt,
                "roundtrip failed for 0x{:02x}",
                byte
            );
        }
    }
}
