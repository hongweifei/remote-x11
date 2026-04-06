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
    #[allow(dead_code)]
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

fn load_config() -> Config {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let config_path = Some(std::path::PathBuf::from(home).join(".config").join("rx11").join("config.toml"));

    if let Some(path) = config_path {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<Config>(&content) {
                    Ok(config) => {
                        info!("Loaded config from {}", path.display());
                        return config;
                    }
                    Err(e) => {
                        warn!("Failed to parse config file {}: {}", path.display(), e);
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

fn config_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    Some(std::path::PathBuf::from(home).join(".config").join("rx11").join("config.toml"))
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
    about = "rx11 — 远程 X11 转发工具",
    long_about = None,
    after_help = "快速开始:
  1. 远程服务器:  rx11 server -t <TOKEN>
  2. 本地电脑:    rx11 client -r <HOST>:7000 -t <TOKEN>
     或:          rx11 ssh -H <HOST> -u <USER> -t <TOKEN>
  3. 远程服务器:  DISPLAY=:0 <gui-program>  (Display 编号见客户端输出)

配置优先级: 命令行参数 > 环境变量 > 配置文件 > 默认值",
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(
        about = "启动中继服务器",
        long_about = "在远程服务器上启动中继服务，监听客户端连接并代理 X11 端口。

不指定 -t 时自动生成并打印 Token，需保存供客户端使用。

示例:
  rx11 server -t my-secret-token
  rx11 server -l 0.0.0.0:8000 -x 6100 -t my-secret-token",
        after_help = "默认值:
  --listen    0.0.0.0:7000
  --x11-port  6000

也可通过 RX11_TOKEN 环境变量提供 Token",
        next_line_help = true,
        display_order = 0,
    )]
    Server {
        #[arg(
            short, long,
            help = "监听地址",
            long_help = "中继服务器监听地址，格式为 IP:PORT"
        )]
        listen: Option<String>,

        #[arg(
            short = 'x', long = "x11-port",
            help = "X11 代理起始端口",
            long_help = "X11 代理起始端口号，Display :N 对应端口 6000+N"
        )]
        x11_port: Option<u16>,

        #[arg(
            short = 't', long, env = "RX11_TOKEN",
            help = "认证 Token (不指定则自动生成)",
            long_help = "客户端/服务端之间的认证 Token，不指定时自动生成并打印到终端。也可通过 RX11_TOKEN 环境变量提供"
        )]
        token: Option<String>,
    },

    #[command(
        about = "启动本地客户端",
        long_about = "连接到远程中继服务器，将 X11 数据转发到本地 X Server。

连接前会自动检测本地 X Server 是否可用。
默认自动分配 Display 编号，也可手动指定。

示例:
  rx11 client -r 192.168.1.100:7000 -t my-secret-token
  rx11 client -r server:7000 -t $TOKEN -d 1",
        after_help = "默认值:
  --relay    remote-server:7000
  --x11      127.0.0.1:6000
  --auto     true (自动分配 Display 编号)

Token 也可通过 RX11_TOKEN 环境变量提供",
        next_line_help = true,
        display_order = 1,
    )]
    Client {
        #[arg(
            short, long,
            help = "中继服务器地址",
            long_help = "远程中继服务器地址，格式为 IP:PORT"
        )]
        relay: Option<String>,

        #[arg(
            short = 't', long, env = "RX11_TOKEN",
            help = "认证 Token",
            long_help = "与远程服务器一致的认证 Token。也可通过 RX11_TOKEN 环境变量提供"
        )]
        token: Option<String>,

        #[arg(
            short, long,
            help = "本地 X Server 地址",
            long_help = "本地 X Server 监听地址，默认 127.0.0.1:6000 (即 Display :0)"
        )]
        x11: Option<String>,

        #[arg(
            short = 'd', long,
            help = "指定 Display 编号 (禁用自动分配)",
            long_help = "手动指定转发的 X11 Display 编号，对应远程端口 6000+N。指定后自动禁用 --auto"
        )]
        display: Option<u16>,

        #[arg(
            long,
            hide = true,
            help = "自动分配 Display 编号 (默认启用)",
        )]
        auto: Option<bool>,
    },

    #[command(
        about = "通过 SSH 隧道连接",
        long_about = "自动建立 SSH 端口转发连接远程中继服务器，数据全程加密。
无需在远程服务器开放额外端口，推荐用于公网环境。

远程服务器仍需先运行 rx11 server。
默认自动分配 Display 编号，也可手动指定。

示例:
  rx11 ssh -H 192.168.1.100 -u root -t my-secret-token
  rx11 ssh -H server -u user -i ~/.ssh/id_rsa -t $TOKEN -d 1",
        after_help = "默认值:
  --port        22
  --relay-port  7000
  --x11         127.0.0.1:6000
  --auto        true (自动分配 Display 编号)

Token 也可通过 RX11_TOKEN 环境变量提供",
        next_line_help = true,
        display_order = 2,
    )]
    Ssh {
        #[arg(
            short = 'H', long,
            help = "远程服务器地址",
            long_help = "远程服务器主机名或 IP 地址 (必填)"
        )]
        host: Option<String>,

        #[arg(
            short = 'P', long,
            help = "SSH 端口",
            long_help = "远程服务器 SSH 端口"
        )]
        port: Option<u16>,

        #[arg(
            short = 'u', long,
            help = "SSH 用户名",
            long_help = "SSH 登录用户名，不指定则使用 SSH 默认配置"
        )]
        user: Option<String>,

        #[arg(
            short = 'i', long,
            help = "SSH 私钥文件路径",
            long_help = "SSH 认证使用的私钥文件，如 ~/.ssh/id_rsa"
        )]
        identity: Option<String>,

        #[arg(
            short = 't', long, env = "RX11_TOKEN",
            help = "认证 Token",
            long_help = "与远程服务器一致的认证 Token。也可通过 RX11_TOKEN 环境变量提供"
        )]
        token: Option<String>,

        #[arg(
            short, long = "relay-port",
            help = "远程中继端口",
            long_help = "远程服务器上 rx11 server 的中继监听端口"
        )]
        relay_port: Option<u16>,

        #[arg(
            short, long,
            help = "本地 X Server 地址",
            long_help = "本地 X Server 监听地址，默认 127.0.0.1:6000 (即 Display :0)"
        )]
        x11: Option<String>,

        #[arg(
            short = 'd', long,
            help = "指定 Display 编号 (禁用自动分配)",
            long_help = "手动指定转发的 X11 Display 编号，对应远程端口 6000+N。指定后自动禁用 --auto"
        )]
        display: Option<u16>,

        #[arg(
            long,
            hide = true,
            help = "自动分配 Display 编号 (默认启用)",
        )]
        auto: Option<bool>,
    },

    #[command(
        about = "生成认证 Token",
        long_about = "生成一个随机 256-bit Token，用于 rx11 server 和 rx11 client 之间的认证。

示例:
  TOKEN=$(rx11 gen-token)
  rx11 server -t $TOKEN &  # 远程
  rx11 client -r server:7000 -t $TOKEN  # 本地",
        display_order = 3,
    )]
    GenToken,

    #[command(
        about = "运行 GUI 程序",
        long_about = "自动设置 DISPLAY 环境变量并执行指定命令，省去每次手动 export DISPLAY。

示例:
  rx11 run xclock
  rx11 run -d 1 firefox
  rx11 run -- gedit /etc/hosts",
        display_order = 4,
    )]
    Run {
        #[arg(
            short, long,
            help = "X11 Display 编号",
            long_help = "设置的 DISPLAY 环境变量值，默认 0 (即 DISPLAY=:0)"
        )]
        display: u16,

        #[arg(
            trailing_var_arg = true,
            required = true,
            help = "要运行的命令及其参数",
            long_help = "要运行的命令及参数，使用 -- 与 rx11 参数分隔

示例: rx11 run -- gedit /etc/hosts"
        )]
        command: Vec<String>,
    },

    #[command(
        about = "配置管理",
        long_about = "管理 rx11 配置文件。

配置文件路径: ~/.config/rx11/config.toml

配置优先级: 命令行参数 > 环境变量 > 配置文件 > 默认值

示例:
  rx11 config init    # 生成默认配置文件
  rx11 config path    # 显示配置文件路径",
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
    #[command(about = "生成默认配置文件")]
    Init,
    #[command(about = "显示配置文件路径")]
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
                eprintln!("生成 Token: {}", t);
                eprintln!("请保存此 Token，客户端连接时需要使用");
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
                anyhow::anyhow!("Relay address is required. Use --relay or config file")
            })?;
            let x11 = x11.or(cc.x11).unwrap_or_else(|| "127.0.0.1:6000".to_string());

            let token = token.or(cc.token);
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("Token is required. Use --token, RX11_TOKEN env, or config file")
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
                anyhow::anyhow!("SSH host is required. Use --host or config file")
            })?;

            let port = port.or(sc.port).unwrap_or(22);
            let relay_port = relay_port.or(sc.relay_port).unwrap_or(rx11_core::protocol::DEFAULT_RELAY_PORT);
            let x11 = x11.or(sc.x11).unwrap_or_else(|| "127.0.0.1:6000".to_string());

            let auto_display = auto.unwrap_or(true) && display.is_none() && sc.display.is_none();

            let token = token.or(sc.token);
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("Token is required. Use --token, RX11_TOKEN env, or config file")
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

            let local_bind_addr = format!("127.0.0.1:{}", local_bind_port);

            if !auto_display && tokio::net::TcpStream::connect(&local_bind_addr).await.is_ok() {
                anyhow::bail!(
                    "Port {} is already in use. Another rx11 ssh instance may be running, or use a different display (-d) to pick another port",
                    local_bind_port
                );
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

            let relay_addr = format!("127.0.0.1:{}", local_bind_port);
            let connector =
                rx11_client::connector::LocalConnector::new(
                    relay_addr, token, x11, display, auto_display,
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
                    info!("Forwarding SIGINT to child process...");
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
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
                        child.kill().await?;
                        child.wait().await
                    }
                }
            }?;

            if !result.success() {
                std::process::exit(result.code().unwrap_or(1));
            }
        }
        Commands::Config { action } => match action {
            ConfigAction::Init => {
                let cfg_path = config_path().ok_or_else(|| anyhow::anyhow!("Cannot determine config directory"))?;

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
                let cfg_path = config_path().ok_or_else(|| anyhow::anyhow!("Cannot determine config directory"))?;
                println!("{}", cfg_path.display());
            }
        },
    }

    Ok(())
}
