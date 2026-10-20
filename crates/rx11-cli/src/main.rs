//! rx11 — remote X11 forwarding tool. Entry point: parse CLI, initialize
//! logging and dispatch to the subcommand implementations.

mod cli;
mod commands;
mod settings;
mod util;

use clap::Parser;

use cli::{Cli, Commands, ConfigAction};

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rx11=info".parse().unwrap()),
        )
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Commands::Server {
            listen,
            x11_port,
            token,
        } => commands::server(listen, x11_port, token).await,
        Commands::Client {
            relay,
            token,
            x11,
            display,
            auto,
        } => commands::client(relay, token, x11, display, auto).await,
        Commands::Ssh { args } => commands::ssh(args).await,
        Commands::GenToken => {
            commands::gen_token();
            Ok(())
        }
        Commands::Run { display, command } => commands::run(display, command).await,
        Commands::Config { action } => match action {
            ConfigAction::Init => commands::config_init().await,
            ConfigAction::Path => commands::config_path_cmd(),
            ConfigAction::Show => commands::config_show(),
        },
    }
}
