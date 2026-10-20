//! Wire protocol definitions: frame layout, message types, codecs and handshake.
//!
//! Submodules:
//! - [`messages`] — payload structs for every frame kind
//! - [`message_type`] — the on-wire [`MessageType`] tag enum
//! - [`binary`] — [`BinaryMessageCodec`] impls for binary frames
//! - [`frame`] — the [`Frame`] enum, its derive macro and frame (de)serialization
//! - [`handshake`] — Hello/Auth negotiation shared by server and client

mod binary;
mod frame;
mod handshake;
mod message_type;
mod messages;

pub use binary::BinaryMessageCodec;
pub use frame::{decode_frame, encode_frame, frame_header_size, Frame};
pub use handshake::{client_handshake, server_handshake, HandshakeResult};
pub use message_type::MessageType;
pub use messages::*;

pub const PROTOCOL_VERSION: u32 = 3;
pub const MAGIC_BYTES: [u8; 4] = *b"RX11";
pub const DEFAULT_RELAY_PORT: u16 = 7000;
pub const DEFAULT_X11_PORT: u16 = 6000;
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub const FRAME_HEADER_SIZE: usize = 9;
