//! Shared building blocks for the rx11 remote X11 forwarding protocol:
//!
//! - [`protocol`] — frame layout, message types, codecs and handshake
//! - [`transport`] — TCP framing over tokio with resynchronization
//! - [`compress`] — zstd/lz4/zlib negotiation and frame helpers
//! - [`incremental`] — chunked diff encoding for repeated payloads
//! - [`auth`] — token and X cookie generation, constant-time verification
//! - [`types`] — validated newtypes (`SessionId`, `Token`, `DisplayNumber`, …)
//! - [`stats`] — lock-free connection statistics
//! - [`config`] — centralized default constants
//! - [`error`] — [`error::Rx11Error`] with retry classification

pub mod auth;
pub mod compress;
pub mod config;
pub mod error;
pub mod incremental;
pub mod protocol;
pub mod stats;
pub mod transport;
pub mod types;
