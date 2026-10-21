//! rx11-client — local side of the relay:
//!
//! - [`connector`] — connect, handshake, session create/resume, auto-reconnect
//! - [`serve`] — relay receive loop and frame dispatch
//! - [`proxy`] — per-application X11 socket pumps
//! - `audioout` — audio channel decoder/player (private)
//! - [`ssh`] — SSH tunnel creation, health monitoring, and supervised
//!   auto-rebuild across link failures

pub mod connector;
pub mod ssh;

mod audioout;
mod proxy;
mod serve;
