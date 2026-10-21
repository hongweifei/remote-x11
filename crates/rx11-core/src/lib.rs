//! Shared building blocks for the rx11 remote X11 forwarding protocol:
//!
//! - [`protocol`] — frame layout, message types, codecs and handshake
//! - [`transport`] — TCP framing over tokio with resynchronization
//! - [`compress`] — zstd/lz4/zlib algorithm negotiation
//! - [`dataplane`] — capability-gated frame build/apply policy chain
//! - [`incremental`] — chunked diff encoding for repeated payloads
//! - [`auth`] — token and X cookie generation, constant-time verification
//! - [`types`] — validated newtypes (`SessionId`, `Token`, `DisplayNumber`, …)
//! - [`stats`] — lock-free connection statistics
//! - [`queue`] — byte-budgeted outbound frame queue (WAN latency bound)
//! - [`config`] — centralized default constants
//! - [`error`] — [`error::Rx11Error`] with retry classification

pub mod auth;
pub mod compress;
pub mod config;
pub mod dataplane;
pub mod error;
pub mod incremental;
pub mod protocol;
pub mod queue;
pub mod stats;
pub mod transport;
pub mod types;
