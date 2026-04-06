use clap::{Parser, Subcommand};
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Debug, Default, Deserialize)]
struct Config {
    client: Option<ClientConfig>,
    server: Option<ServerConfig>,
    ssh: Option<SshConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ClientConfig {
    relay: Option<String>,
    token: Option<String>,
    x11: Option<String>,
    display: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct ServerConfig {
    listen: Option<String>,
    x11_port: Option<u16>,
    token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SshConfig {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    identity: Option<String>,
    token: Option<String>,
    relay_port: Option<u16>,
    x11: Option<String>,
    display: Option<u16>,
}

fn config_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(std::path::PathBuf::from))
}

fn config_path() -> Option<std::path::PathBuf> {
    config_home().map(|h| h.join(".config").join("rx11").join("config.toml"))
}

fn load_config() -> Config {
    if let Some(path) = config_path() {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<Config>(&content) {
                    Ok(config) => {
                        info!("Loaded config from {}", path.display());
                        return config;
                    }
                    Err(e) => {
                        warn!("Failed to parse config file {}: {}", path.display(), e);
                        eprintln!("WARNING: Failed to parse config file {}: {}. Using default config.", path.display(), e);
                    }
                },
                Err(e) => {
                    warn!("Failed to read config file {}: {}", path.display(), e);
                }
            }
        }
    }
    Config::default()
}

async fn detect_x11_server(addr: &str) {
    use tokio::time::{timeout, Duration};
    match timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => {
            info!("Local X Server detected at {}", addr);
        }
        _ => {
            warn!("No X Server detected at {}", addr);
            eprintln!("WARNING: No X Server detected at {}. Start an X Server before connecting (VcXsrv/XQuartz/Xorg).", addr);
        }
    }
}

async fn wait_for_port(addr: &str, timeout_duration: std::time::Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(50);
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if start.elapsed() > timeout_duration {
                    return Err(anyhow::anyhow!("Timeout waiting for {}", addr));
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_millis(500));
            }
        }
    }
}

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
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
        display_order = 0,
    )]
    Server {
        #[arg(
            short, long,
            help = "Listen address",
            long_help = "Relay server listen address, format: IP:PORT"
        )]
        listen: Option<String>,

        #[arg(
            short = 'x', long = "x11-port",
            help = "X11 proxy start port",
            long_help = "X11 proxy start port number. Display :N maps to port 6000+N"
        )]
        x11_port: Option<u16>,

        #[arg(
            short = 't', long, env = "RX11_TOKEN",
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
        display_order = 1,
    )]
        Client {
        #[arg(
            short, long,
            help = "Relay server address",
            long_help = "Remote relay server address, format: IP:PORT"
        )]
        relay: Option<String>,

        #[arg(
            short = 't', long, env = "RX11_TOKEN",
            help = "Auth token",
            long_help = "Authentication token matching the remote server. Can also be provided via RX11_TOKEN environment variable"
        )]
        token: Option<String>,

        #[arg(
            short, long,
            help = "Local X Server address",
            long_help = "Local X Server listen address, default 127.0.0.1:6000 (i.e. Display :0)"
        )]
        x11: Option<String>,

        #[arg(
            short = 'd', long,
            help = "Specify display number (disables auto-assign)",
            long_help = "Manually specify the X11 display number to forward, maps to remote port 6000+N. Disables --auto"
        )]
        display: Option<u16>,

        #[arg(
            long,
            hide = true,
            help = "Auto-assign display number (enabled by default)",
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
        display_order = 2,
    )]
    Ssh {
        #[arg(
            short = 'H', long,
            help = "Remote server address",
            long_help = "Remote server hostname or IP address (required)"
        )]
        host: Option<String>,

        #[arg(
            short = 'P', long,
            help = "SSH port",
            long_help = "Remote server SSH port"
        )]
        port: Option<u16>,

        #[arg(
            short = 'u', long,
            help = "SSH username",
            long_help = "SSH login username. Uses SSH default config if omitted"
        )]
        user: Option<String>,

        #[arg(
            short = 'i', long,
            help = "SSH private key path",
            long_help = "Private key file for SSH authentication, e.g. ~/.ssh/id_rsa"
        )]
        identity: Option<String>,

        #[arg(
            short = 't', long, env = "RX11_TOKEN",
            help = "Auth token",
            long_help = "Authentication token matching the remote server. Can also be provided via RX11_TOKEN environment variable"
        )]
        token: Option<String>,

        #[arg(
            short, long = "relay-port",
            help = "Remote relay port",
            long_help = "Relay listen port of rx11 server on the remote host"
        )]
        relay_port: Option<u16>,

        #[arg(
            short, long,
            help = "Local X Server address",
            long_help = "Local X Server listen address, default 127.0.0.1:6000 (i.e. Display :0)"
        )]
        x11: Option<String>,

        #[arg(
            short = 'd', long,
            help = "Specify display number (disables auto-assign)",
            long_help = "Manually specify the X11 display number to forward, maps to remote port 6000+N. Disables --auto"
        )]
        display: Option<u16>,

        #[arg(
            long,
            hide = true,
            help = "Auto-assign display number (enabled by default)",
        )]
        auto: Option<bool>,
    },

    #[command(
        about = "Generate auth token",
        long_about = "Generate a random 256-bit token for authentication between rx11 server and rx11 client.

Examples:
  TOKEN=$(rx11 gen-token)
  rx11 server -t $TOKEN &  # remote
  rx11 client -r server:7000 -t $TOKEN  # local",
        display_order = 3,
    )]
    GenToken,

    #[command(
        about = "Run a GUI program",
        long_about = "Automatically set the DISPLAY environment variable and execute the specified command, so you don't need to manually export DISPLAY each time.

Examples:
  rx11 run xclock
  rx11 run -d 1 firefox
  rx11 run -- gedit /etc/hosts",
        display_order = 4,
    )]
    Run {
        #[arg(
            short, long,
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
        display_order = 5,
    )]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    #[command(about = "Generate default config file")]
    Init,
    #[command(about = "Show config file path")]
    Path,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rx11=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server {
            listen,
            x11_port,
            token,
        } => {
            let config = load_config();
            let sc = config.server.unwrap_or_default();

            let listen = listen.or(sc.listen).unwrap_or_else(|| "0.0.0.0:7000".to_string());
            let x11_port = x11_port.or(sc.x11_port).unwrap_or(rx11_core::protocol::DEFAULT_X11_PORT);
            let token = token.or(sc.token).unwrap_or_else(|| {
                let t = rx11_core::auth::generate_token();
                eprintln!("Generated token: {}", t);
                eprintln!("Save this token, it is required for client connections");
                t
            });

            let server = rx11_server::relay::RelayServer::new(listen, token, x11_port);
            server.run().await?;
        }
        Commands::Client {
            relay,
            token,
            x11,
            display,
            auto,
        } => {
            let config = load_config();
            let cc = config.client.unwrap_or_default();

            let relay = relay.or(cc.relay).ok_or_else(|| {
                anyhow::anyhow!("Missing relay server address. Use --relay or set it in config file")
            })?;
            let x11 = x11.or(cc.x11).unwrap_or_else(|| "127.0.0.1:6000".to_string());

            let token = token.or(cc.token);
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("Missing auth token. Use --token, RX11_TOKEN env var, or set it in config file")
            })?;

            let auto_display = auto.unwrap_or(true) && display.is_none() && cc.display.is_none();

            detect_x11_server(&x11).await;
            let connector = rx11_client::connector::LocalConnector::new(
                relay, token, x11, display, auto_display,
            );
            connector.connect_and_serve().await?;
        }
        Commands::Ssh {
            host,
            port,
            user,
            identity,
            token,
            relay_port,
            x11,
            display,
            auto,
        } => {
            let config = load_config();
            let sc = config.ssh.unwrap_or_default();

            let host = host.or(sc.host);
            let host = host.ok_or_else(|| {
                anyhow::anyhow!("Missing remote server address. Use --host or set it in config file")
            })?;

            let port = port.or(sc.port).unwrap_or(22);
            let relay_port = relay_port.or(sc.relay_port).unwrap_or(rx11_core::protocol::DEFAULT_RELAY_PORT);
            let x11 = x11.or(sc.x11).unwrap_or_else(|| "127.0.0.1:6000".to_string());

            let auto_display = auto.unwrap_or(true) && display.is_none() && sc.display.is_none();

            let token = token.or(sc.token);
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("Missing auth token. Use --token, RX11_TOKEN env var, or set it in config file")
            })?;

            let user = user.or(sc.user);
            let identity = identity.or(sc.identity);

            let local_bind_port: u16 = if auto_display {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let port = listener.local_addr()?.port();
                drop(listener);
                port
            } else {
                let display_for_port = display.or(sc.display).unwrap_or(0);
                17000u16.saturating_add(display_for_port)
            };

            if !auto_display {
                let check_addr = format!("127.0.0.1:{}", local_bind_port);
                if tokio::net::TcpStream::connect(&check_addr).await.is_ok() {
                    anyhow::bail!(
                        "Port {} is already in use. Another rx11 ssh instance may be running, or use a different display (-d) to pick another port",
                        local_bind_port
                    );
                }
            }

            let mut ssh_child = rx11_client::ssh::SshClient::create_forward_tunnel(
                &host,
                port,
                user.as_deref(),
                relay_port,
                local_bind_port,
                identity.as_deref(),
            )
            .await?;

            let local_addr = format!("127.0.0.1:{}", local_bind_port);
            if let Err(e) = wait_for_port(&local_addr, std::time::Duration::from_secs(10)).await {
                let _ = ssh_child.kill().await;
                return Err(e.context("SSH tunnel failed to become ready"));
            }

            detect_x11_server(&x11).await;

            let connector =
                rx11_client::connector::LocalConnector::new(
                    local_addr, token, x11, display, auto_display,
                );

            tokio::select! {
                r = connector.connect_and_serve() => r?,
                status = ssh_child.wait() => {
                    anyhow::bail!("SSH tunnel exited with status: {:?}", status?);
                }
            }
        }
        Commands::GenToken => {
            let token = rx11_core::auth::generate_token();
            println!("{}", token);
        }
        Commands::Run { display, command } => {
            let display_str = format!(":{}", display);
            tracing::info!(
                "Setting DISPLAY={} and running: {}",
                display_str,
                command.join(" ")
            );

            let mut child = tokio::process::Command::new(&command[0])
                .args(&command[1..])
                .env("DISPLAY", &display_str)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()?;

            let result = tokio::select! {
                status = child.wait() => status,
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, waiting for child process to exit...");
                    #[cfg(unix)]
                    {
                        if let Some(id) = child.id() {
                            let _ = unsafe { libc::kill(id as libc::pid_t, libc::SIGTERM) };
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        match child.try_wait()? {
                            Some(status) => Ok(status),
                            None => {
                                info!("Child did not exit, sending SIGKILL...");
                                if let Some(id) = child.id() {
                                    let _ = unsafe { libc::kill(id as libc::pid_t, libc::SIGKILL) };
                                }
                                child.wait().await
                            }
                        }
                    }
                    #[cfg(windows)]
                    {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        match child.try_wait()? {
                            Some(status) => Ok(status),
                            None => {
                                child.kill().await?;
                                child.wait().await
                            }
                        }
                    }
                }
            }?;

            if !result.success() {
                std::process::exit(result.code().unwrap_or(1));
            }
        }
        Commands::Config { action } => match action {
            ConfigAction::Init => {
                let cfg_path = config_path().ok_or_else(|| anyhow::anyhow!("Cannot determine config directory (HOME/USERPROFILE not set)"))?;

                if cfg_path.exists() {
                    anyhow::bail!("Config file already exists at {}", cfg_path.display());
                }

                if let Some(parent) = cfg_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                let template = r#"# rx11 configuration file
# Priority: command-line args > environment variables > this config > defaults

[client]
# relay = "my-server:7000"
# token = "your-token-here"
# x11 = "127.0.0.1:6000"
# display = 0

[server]
# listen = "0.0.0.0:7000"
# x11_port = 6000
# token = "your-token-here"

[ssh]
# host = "my-server"
# port = 22
# user = "myuser"
# identity = "~/.ssh/id_rsa"
"#;
                tokio::fs::write(&cfg_path, template).await?;
                info!("Config file created at {}", cfg_path.display());
                println!("Config file created at {}", cfg_path.display());
            }
            ConfigAction::Path => {
                let cfg_path = config_path().ok_or_else(|| anyhow::anyhow!("Cannot determine config directory (HOME/USERPROFILE not set)"))?;
                println!("{}", cfg_path.display());
            }
        },
    }

    Ok(())
}
