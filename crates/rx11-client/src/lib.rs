//! rx11-client — local side of the relay:
//!
//! - [`connector`] — connect, handshake, session create/resume, auto-reconnect
//! - [`serve`] — relay receive loop and frame dispatch
//! - [`proxy`] — per-application X11 socket pumps
//! - [`ssh`] — SSH tunnel creation with health monitoring

pub mod connector;
pub mod ssh;

mod proxy;
mod serve;
