//! clap command-line interface definitions for the `rx11` binary.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rx11",
    version,
    about = "rx11 — Remote X11 forwarding tool",
    long_about = None,
    after_help = "Quick start:
  1. Remote server:  rx11 server -t <TOKEN>
  2. Local machine:  rx11 client -r <HOST>:7000 -t <TOKEN>
     or:             rx11 ssh -H <HOST> -u <USER> -t <TOKEN>
  3. Remote server:  DISPLAY=:0 <gui-program>  (see client output for display number)

Priority: command-line args > environment variables > config file > defaults",
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(
        about = "Start relay server",
        long_about = "Start a relay server on the remote host that listens for client connections and proxies X11 ports.

If -t is not specified, a token is auto-generated and printed to the terminal. Save it for client use.

Examples:
  rx11 server -t my-secret-token
  rx11 server -l 0.0.0.0:8000 -x 6100 -t my-secret-token",
        after_help = "Defaults:
  --listen    0.0.0.0:7000
  --x11-port  6000

Token can also be provided via RX11_TOKEN environment variable",
        next_line_help = true,
        display_order = 0
    )]
    Server {
        #[arg(
            short,
            long,
            help = "Listen address",
            long_help = "Relay server listen address, format: IP:PORT"
        )]
        listen: Option<String>,

        #[arg(
            short = 'x',
            long = "x11-port",
            help = "X11 proxy start port",
            long_help = "X11 proxy start port number. Display :N maps to port 6000+N"
        )]
        x11_port: Option<u16>,

        #[arg(
            short = 't',
            long,
            env = "RX11_TOKEN",
            help = "Auth token (auto-generated if omitted)",
            long_help = "Authentication token between client and server. Auto-generated and printed if omitted. Can also be provided via RX11_TOKEN environment variable"
        )]
        token: Option<String>,
    },

    #[command(
        about = "Start local client",
        long_about = "Connect to a remote relay server and forward X11 data to the local X Server.

Automatically detects whether the local X Server is available before connecting.
Display number is auto-assigned by default, or can be specified manually.

Examples:
  rx11 client -r 192.168.1.100:7000 -t my-secret-token
  rx11 client -r server:7000 -t $TOKEN -d 1",
        after_help = "Defaults:
  --relay    remote-server:7000
  --x11      127.0.0.1:6000
  --auto     true (auto-assign display number)

Token can also be provided via RX11_TOKEN environment variable",
        next_line_help = true,
        display_order = 1
    )]
    Client {
        #[arg(
            short,
            long,
            help = "Relay server address",
            long_help = "Remote relay server address, format: IP:PORT"
        )]
        relay: Option<String>,

        #[arg(
            short = 't',
            long,
            env = "RX11_TOKEN",
            help = "Auth token",
            long_help = "Authentication token matching the remote server. Can also be provided via RX11_TOKEN environment variable"
        )]
        token: Option<String>,

        #[arg(
            short,
            long,
            help = "Local X Server address",
            long_help = "Local X Server listen address, default 127.0.0.1:6000 (i.e. Display :0)"
        )]
        x11: Option<String>,

        #[arg(
            short = 'd',
            long,
            help = "Specify display number (disables auto-assign)",
            long_help = "Manually specify the X11 display number to forward, maps to remote port 6000+N. Disables --auto"
        )]
        display: Option<u16>,

        #[arg(
            long,
            hide = true,
            help = "Auto-assign display number (enabled by default)"
        )]
        auto: Option<bool>,
    },

    #[command(
        about = "Connect via SSH tunnel",
        long_about = "Automatically create an SSH tunnel to connect to the remote relay server. All data is encrypted end-to-end.
No extra ports need to be opened on the remote server. Recommended for public networks.

The remote server still needs to run rx11 server first.
Display number is auto-assigned by default, or can be specified manually.

Examples:
  rx11 ssh -H 192.168.1.100 -u root -t my-secret-token
  rx11 ssh -H server -u user -i ~/.ssh/id_rsa -t $TOKEN -d 1",
        after_help = "Defaults:
  --port        22
  --relay-port  7000
  --x11         127.0.0.1:6000
  --auto        true (auto-assign display number)

Token can also be provided via RX11_TOKEN environment variable",
        next_line_help = true,
        display_order = 2
    )]
    Ssh {
        #[command(flatten)]
        args: SshArgs,
    },

    #[command(
        about = "Generate auth token",
        long_about = "Generate a random 256-bit token for authentication between rx11 server and rx11 client.

Examples:
  TOKEN=$(rx11 gen-token)
  rx11 server -t $TOKEN &  # remote
  rx11 client -r server:7000 -t $TOKEN  # local",
        display_order = 3
    )]
    GenToken,

    #[command(
        about = "Run a GUI program",
        long_about = "Automatically set the DISPLAY environment variable and execute the specified command, so you don't need to manually export DISPLAY each time.

Examples:
  rx11 run xclock
  rx11 run -d 1 firefox
  rx11 run -- gedit /etc/hosts",
        display_order = 4
    )]
    Run {
        #[arg(
            short,
            long,
            help = "X11 display number",
            long_help = "DISPLAY environment variable value to set, default 0 (i.e. DISPLAY=:0)"
        )]
        display: u16,

        #[arg(
            trailing_var_arg = true,
            required = true,
            help = "Command and arguments to run",
            long_help = "Command and arguments to run. Use -- to separate from rx11 args

Example: rx11 run -- gedit /etc/hosts"
        )]
        command: Vec<String>,
    },

    #[command(
        about = "Configuration management",
        long_about = "Manage rx11 configuration file.

Config file path: ~/.config/rx11/config.toml

Priority: command-line args > environment variables > config file > defaults

Examples:
  rx11 config init    # generate default config file
  rx11 config path    # show config file path",
        subcommand_required = true,
        display_order = 5
    )]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    #[command(about = "Generate default config file")]
    Init,
    #[command(about = "Show config file path")]
    Path,
}

/// Arguments of the `ssh` subcommand, grouped to keep the handler
/// signature small.
#[derive(clap::Args)]
pub struct SshArgs {
    #[arg(
        short = 'H',
        long,
        help = "Remote server address",
        long_help = "Remote server hostname or IP address (required)"
    )]
    pub host: Option<String>,

    #[arg(
        short = 'P',
        long,
        help = "SSH port",
        long_help = "Remote server SSH port"
    )]
    pub port: Option<u16>,

    #[arg(
        short = 'u',
        long,
        help = "SSH username",
        long_help = "SSH login username. Uses SSH default config if omitted"
    )]
    pub user: Option<String>,

    #[arg(
        short = 'i',
        long,
        help = "SSH private key path",
        long_help = "Private key file for SSH authentication, e.g. ~/.ssh/id_rsa"
    )]
    pub identity: Option<String>,

    #[arg(
        short = 't',
        long,
        env = "RX11_TOKEN",
        help = "Auth token",
        long_help = "Authentication token matching the remote server. Can also be provided via RX11_TOKEN environment variable"
    )]
    pub token: Option<String>,

    #[arg(
        short,
        long = "relay-port",
        help = "Remote relay port",
        long_help = "Relay listen port of rx11 server on the remote host"
    )]
    pub relay_port: Option<u16>,

    #[arg(
        short,
        long,
        help = "Local X Server address",
        long_help = "Local X Server listen address, default 127.0.0.1:6000 (i.e. Display :0)"
    )]
    pub x11: Option<String>,

    #[arg(
        short = 'd',
        long,
        help = "Specify display number (disables auto-assign)",
        long_help = "Manually specify the X11 display number to forward, maps to remote port 6000+N. Disables --auto"
    )]
    pub display: Option<u16>,

    #[arg(
        long,
        hide = true,
        help = "Auto-assign display number (enabled by default)"
    )]
    pub auto: Option<bool>,
}
