//! Implementations of the `rx11` subcommands. One file per command, each
//! resolving defaults in the order: CLI args > config file > built-ins.

mod client;
mod config_cmd;
mod run;
mod server;
mod ssh;

pub use client::client;
pub use config_cmd::{config_init, config_path_cmd};
pub use run::run;
pub use server::server;
pub use ssh::ssh;

use rx11_core::auth::generate_token;

/// `rx11 gen-token`
pub fn gen_token() {
    println!("{}", generate_token());
}
