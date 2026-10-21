//! Wire protocol definitions: frame layout, message types, codecs and handshake.
//!
//! Submodules:
//! - [`messages`] — payload structs for every frame kind
//! - [`message_type`] — the on-wire [`MessageType`] tag enum
//! - [`binary`] — [`BinaryMessageCodec`] impls for binary frames
//! - [`frame`] — the [`Frame`] enum, its derive macro and frame (de)serialization
//! - [`capability`] — feature flags negotiated in the handshake ([`capability::Features`])
//! - [`handshake`] — Hello/Auth/capability negotiation shared by server and client
//!
//! Evolution rules: the frame header self-describes the protocol version, so a
//! mismatched peer fails fast on the first byte pair rather than mis-parsing a
//! frame. New *messages* ride on capability negotiation (unknown capability
//! names are ignored on the wire); bumping `PROTOCOL_VERSION` is reserved for
//! breaking layout changes only.

mod binary;
mod capability;
mod frame;
mod handshake;
mod message_type;
mod messages;

pub use binary::BinaryMessageCodec;
pub use capability::{Capability, Features};
pub use frame::{decode_frame, encode_frame, frame_header_size, Frame};
pub use handshake::{client_handshake, server_handshake, HandshakeResult};
pub use message_type::MessageType;
pub use messages::*;

pub const PROTOCOL_VERSION: u32 = 1;
/// Version octet embedded in every frame header (`PROTOCOL_VERSION` fits u8
/// by construction of the current major; a >255 future version would split
/// sub-frames by stream sessions, not by this byte).
pub const WIRE_VERSION: u8 = PROTOCOL_VERSION as u8;
pub const MAGIC_BYTES: [u8; 4] = *b"RX11";
pub const DEFAULT_RELAY_PORT: u16 = 7000;
pub const DEFAULT_X11_PORT: u16 = 6000;
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// `MAGIC(4) | version(1) | type(2 BE) | payload_len(4 BE)`
///
/// The 16-bit type field keeps the frame table unbounded for practical
/// purposes (65536 slots; today's allocations all live in the low byte and
/// the high byte is reserved for future major partitions, e.g. per-channel
/// subtype ranges).
pub const FRAME_HEADER_SIZE: usize = 11;
