//! One module per console page. Each page owns a background canvas plus the
//! interactive widgets that sit on it, exposes `set_data` for the poll timer
//! and `on_action` for user intent, and never touches the launcher directly —
//! the app routes actions so pages stay dumb and testable.
//!
//! `config` is the declarative side: four pages that differ only in their
//! [`config::FormSpec`] data, all rendered by the single engine in [`form`].

pub mod config;
pub mod console;
pub mod form;
pub mod logs;

pub use config::{keys_for, CLIENT, RUNTIME, SERVER, SSH};
pub use console::{ConsoleAction, ConsoleData, ConsolePage};
pub use form::{FormAction, FormPage, FormRole};
pub use logs::{LogAction, LogEntry, LogPage};
