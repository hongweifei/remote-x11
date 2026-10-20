//! rx11-server — remote side of the relay:
//!
//! - [`relay`] — TCP relay server and per-client connection task
//! - [`session`] — session/display registry with grace-period recycling
//! - [`x11_listener`] — X11 port proxying into the relay channels

pub mod relay;
pub mod session;
pub mod x11_listener;
