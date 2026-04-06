# rx11 — Remote X11 Forwarding Tool

[中文文档](README.md)

Relay X11 connections via a custom protocol, letting you view GUI programs running on a remote server (public network) from your local machine (behind NAT).

Supports **TCP direct** and **SSH tunnel** modes.

## How It Works

```
 Remote Server (public)                         Local Machine (NAT)
 ┌────────────────────────┐                  ┌────────────────────────┐
 │  GUI App (X11 Client)  │                  │  X Server              │
 │         ↓              │                  │  (VcXsrv / Xorg)       │
 │  rx11 server           │  ◄── rx11 proto ─►│  rx11 client           │
 │  ├─ Relay  :7000       │     (TCP / SSH)  │  └─ connects to :6000  │
 │  └─ X11 Proxy :6000+N  │                  │                        │
 └────────────────────────┘                  └────────────────────────┘
```

**Core flow:**

1. Start `rx11 server` on the remote host — it listens on the relay port (7000) and proxies X11 ports (6000+N)
2. Start `rx11 client` on your local machine — it connects to the remote relay via TCP or SSH tunnel
3. Set `DISPLAY=:0` on the remote host and run a GUI program
4. X11 drawing commands are relayed to the local X Server, and the GUI appears on your screen

## Features

- **Multi-connection multiplexing** — Multiple X11 application connections over a single relay session, distinguished by `connection_id`
- **Auto-reconnect + session resume** — Exponential backoff reconnection (up to 10 retries) on network interruption; server preserves sessions for 60s grace period, existing X11 connections survive reconnection
- **Bidirectional heartbeat** — Client and server exchange heartbeats; auto-disconnect after 90s of no response to avoid half-open connections
- **X Server auto-detection** — Probes local X Server availability before connecting, with platform-specific hints
- **xauth integration** — Server automatically manages `xauth` entries (MIT-MAGIC-COOKIE-1) for enhanced security
- **Graceful shutdown** — Sends `SessionDestroy` frame on Ctrl+C to clean up remote sessions; `rx11 run` forwards signals to child processes
- **Connection statistics** — Reports bytes sent/received, frame counts, active connections every 30 seconds
- **Config file** — TOML config support. Priority: CLI args > env vars > config file > defaults
- **Multiple displays** — Run multiple independent GUI sessions simultaneously with the `-d` flag
- **Auto display assignment** — Server automatically assigns available display numbers by default; manual override available
- **Data compression** — zstd/lz4/zlib algorithms with auto-negotiation; data over 64 bytes is auto-compressed with fallback if compression increases size
- **Security hardening** — Session/connection ownership validation, auth data length limits, decompression size verification, frame sync recovery, read_buf limits for DoS prevention
- **SSH port conflict detection** — Checks for local port conflicts before establishing SSH tunnels

## Quick Start

### Build from Source

Requires the Rust toolchain ([Install Rust](https://rustup.rs)).

```bash
git clone <repo-url> remote-x11 && cd remote-x11
cargo build --release
# Binary is at target/release/rx11
```

### Prerequisites

You need an X Server running on your local machine:

| Platform | Recommended |
|---|---|
| Linux | System Xorg / Wayland + XWayland |
| macOS | [XQuartz](https://www.xquartz.org/) |
| Windows | [VcXsrv](https://sourceforge.net/projects/vcxsrv/) or [Xming](https://sourceforge.net/projects/xming/) |

After starting your X Server, make sure it listens on TCP port 6000 (default behavior).

---

### Mode 1: TCP Direct

Use when you can directly access the remote server's port, or when you have your own tunnel.

**Step 1: Generate a token (on any machine)**

```bash
rx11 gen-token
# Example output: a3f8b2c1d4e5...
```

**Step 2: Remote server — start the service**

```bash
rx11 server -t <TOKEN>
```

Defaults to listening on `0.0.0.0:7000` (relay) and `6000` (X11 proxy). Use `--help` for all options.

**Step 3: Local machine — start the client**

```bash
rx11 client -r <REMOTE_IP>:7000 -t <TOKEN>
```

**Step 4: Remote server — run a GUI program**

Check the client output for the display number, e.g. `Session created for display :0`.

```bash
rx11 run xclock
rx11 run -d 0 firefox
# Or set DISPLAY manually
DISPLAY=:0 xclock
```

---

### Mode 2: SSH Tunnel (Recommended)

Automatically creates an SSH tunnel. No extra ports need to be opened, all data is encrypted.

```bash
# On your local machine, one command
rx11 ssh -H <REMOTE_IP> -u <USERNAME> -t <TOKEN>
```

The remote server still needs `rx11 server` running first.

**Manually specify display:**

```bash
rx11 ssh -H <REMOTE_IP> -u <USERNAME> -t <TOKEN> -d 1
```

---

## Command Reference

### `rx11 server` — Start remote service

```
Usage: rx11 server [OPTIONS]

Options:
  -l, --listen <LISTEN>        Listen address [default: 0.0.0.0:7000]
  -x, --x11-port <X11_PORT>    X11 proxy start port [default: 6000]
  -t, --token <TOKEN>          Auth token (or use RX11_TOKEN env var)
```

If `-t` is omitted, a token is auto-generated and printed.

### `rx11 client` — Start local client

```
Usage: rx11 client [OPTIONS]

Options:
   -r, --relay <RELAY>          Relay server address (required)
   -t, --token <TOKEN>          Auth token (or use RX11_TOKEN env var)
   -x, --x11 <X11>              Local X Server address [default: 127.0.0.1:6000]
   -d, --display <DISPLAY>      Manual display number (default: auto-assign)
```

Display number is auto-assigned by default. Specifying `-d` switches to manual mode.

### `rx11 ssh` — Connect via SSH tunnel

```
Usage: rx11 ssh --host <HOST> [OPTIONS]

Options:
   -H, --host <HOST>                  Remote server address (required)
   -P, --port <PORT>                  SSH port [default: 22]
   -u, --user <USER>                  SSH username
   -i, --identity <IDENTITY>          SSH private key path
   -t, --token <TOKEN>                Auth token (or use RX11_TOKEN env var)
   -r, --relay-port <RELAY_PORT>      Remote relay port [default: 7000]
   -x, --x11 <X11>                    Local X Server address [default: 127.0.0.1:6000]
   -d, --display <DISPLAY>            Manual display number (default: auto-assign)
```

Display number is auto-assigned by default, and a local ephemeral port is chosen automatically. With `-d`, manual mode is used and the local port is `17000 + display`.

### `rx11 run` — Run a GUI program

Automatically sets the `DISPLAY` environment variable and runs the given command, forwarding SIGINT/SIGTERM to the child process:

```bash
rx11 run xclock
rx11 run -d 1 firefox
rx11 run -- gedit /etc/hosts
```

```
Options:
  -d, --display <DISPLAY>      X11 display number [default: 0]
  <command>...                  Command and arguments to run
```

### `rx11 gen-token` — Generate auth token

```bash
rx11 gen-token
```

Outputs a random SHA-256 token for client-server authentication.

### `rx11 config` — Configuration management

```bash
rx11 config init   # Generate default config file ~/.config/rx11/config.toml
rx11 config path   # Show config file path
```

---

## Configuration File

Supports TOML config files (default path `~/.config/rx11/config.toml`) to avoid repeating CLI arguments.

Priority: **CLI args > env vars > config file > defaults**

```toml
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
# token, relay_port, x11, display are also supported
```

Use `rx11 config init` to generate a commented template file.

---

## Multiple Displays

Use the `-d` flag to manually specify display numbers and run multiple independent GUI sessions simultaneously:

```bash
# Terminal 1: Display :0
rx11 client -r server:7000 -t <TOKEN> -d 0

# Terminal 2: Display :1
rx11 client -r server:7000 -t <TOKEN> -d 1
```

On the remote server:

```bash
DISPLAY=:0 xclock       # Appears in Terminal 1
DISPLAY=:1 xeyes        # Appears in Terminal 2
# Or use rx11 run
rx11 run -d 0 xclock
rx11 run -d 1 xeyes
```

Display number `N` maps to remote X11 port `6000 + N`.

---

## Auto Display Assignment

`client` and `ssh` commands auto-assign display numbers by default — no manual management needed. The server returns the assigned display number in the SessionAck response (check client logs). Just use the corresponding `DISPLAY` value on the remote server to run GUI programs.

To switch to manual mode, use the `-d` flag:

```bash
# TCP mode
rx11 client -r server:7000 -t <TOKEN> -d 1

# SSH mode
rx11 ssh -H server -u user -t <TOKEN> -d 1
```

---

## Session Resume

After a client disconnects, the server preserves the session (including established X11 connections) for a **60-second grace period**. If the client reconnects within this window, the session is automatically resumed and already-running GUI programs continue to work.

```bash
# Client auto-reconnects (no extra action needed)
rx11 client -r server:7000 -t <TOKEN>
# After network interruption, reconnects automatically, open GUI programs keep working
```

Reconnection flow:

1. Client sends Hello with the previous `session_id`
2. After authentication, sends a `SessionResume` request
3. Server verifies the session exists and is within the grace period, then resumes it
4. Existing X11 application connections continue relaying data

After the 60-second grace period expires, the server automatically destroys the stale session and releases resources.

---

## Environment Variables

| Variable | Description |
|---|---|
| `RX11_TOKEN` | Auth token, equivalent to the `-t` flag |
| `RUST_LOG` | Log level, e.g. `rx11=debug` for debug logging |

---

## Protocol Format

rx11 uses a custom binary frame protocol. Each frame:

```
┌──────────┬──────────┬────────────┬─────────────┐
│ Magic    │ Type     │ Length     │ Payload     │
│ 4 bytes  │ 1 byte   │ 4 bytes    │ N bytes     │
│ RX11     │          │ (BE u32)   │             │
└──────────┴──────────┴────────────┴─────────────┘
```

Frame types:

| Type | Value | Purpose |
|---|---|---|
| Hello | 0x01 | Client/server handshake (may carry `resume_session_id` for session resume) |
| HelloAck | 0x02 | Handshake response |
| AuthRequest | 0x03 | Authentication request |
| AuthResponse | 0x04 | Authentication result |
| SessionCreate | 0x10 | Create X11 forwarding session (with specified display) |
| SessionAck | 0x11 | Session create/resume result (includes `session_id`) |
| SessionDestroy | 0x12 | Destroy session |
| SessionResume | 0x13 | Resume an existing session |
| SessionAutoCreate | 0x14 | Auto-assign display and create session |
| DataX11 | 0x20 | X11 data frame (binary, not JSON) |
| CompressedDataX11 | 0x21 | Compressed X11 data frame |
| X11Connect | 0x22 | X11 application connection notification |
| X11Disconnect | 0x23 | X11 application disconnection notification |
| Heartbeat | 0x30 | Heartbeat (bidirectional) |
| HeartbeatAck | 0x31 | Heartbeat response |
| Error | 0xFF | Error |

Control frame payloads use JSON encoding. X11 data frames use binary encoding:

```
┌───────────────┬─────────────────┐
│ Connection ID │ X11 Data        │
│ 4 bytes       │ remaining bytes │
│ (BE u32)      │                 │
└───────────────┴─────────────────┘
```

CompressedDataX11 frame format:

```
┌───────────────┬──────────────┬─────────────────┐
│ Connection ID │ Original Len │ Compressed Data │
│ 4 bytes       │ 4 bytes      │ remaining bytes │
│ (BE u32)      │ (BE u32)     │                 │
└───────────────┴──────────────┴─────────────────┘
```

`X11Connect` / `X11Disconnect` frames carry `{display, connection_id}` to notify the peer of X11 application connect/disconnect events. Multiple connections are multiplexed via `connection_id`.

Connection establishment flow:

```
Client                          Server
  │─── Hello ──────────────────►│  (may carry resume_session_id)
  │◄── HelloAck ────────────────│
  │─── AuthRequest ────────────►│
  │◄── AuthResponse ────────────│
  │─── SessionCreate ──────────►│  (or SessionResume / SessionAutoCreate)
  │◄── SessionAck ──────────────│  (includes session_id)
  │◄──► X11Connect/Disconnect ──│  (X11 app connect/disconnect notifications)
  │◄──► DataX11 (bidirectional)►│  (X11 data relay, includes connection_id)
  │◄──► Heartbeat / HeartbeatAck│  (bidirectional keepalive)
  │─── SessionDestroy ──────────►│  (optional, explicit destroy)
```

---

## Project Structure

```
remote-x11/
├── Cargo.toml                  # Workspace root config
└── crates/
    ├── rx11-core/              # Core lib: protocol, transport, auth, stats
    │   └── src/
     │       ├── protocol.rs     # Frame encode/decode, message type definitions
     │       ├── transport.rs    # Async transport (frame sync recovery, buffer limits)
     │       ├── compress.rs     # Data compression (zstd/lz4/zlib negotiation)
     │       ├── auth.rs         # Token generation & verification (constant-time compare)
    │       ├── stats.rs        # Connection stats (bytes, frames, active connections)
    │       └── error.rs        # Error types (with retryable error support)
    ├── rx11-server/            # Remote side: relay server + X11 listener
    │   └── src/
    │       ├── relay.rs        # Relay service (handshake, auth, session mgmt, heartbeat)
    │       ├── session.rs      # Session manager (persistence, grace period, auto display)
    │       └── x11_listener.rs # X11 port listener & proxy (multi-connection multiplexing)
    ├── rx11-client/            # Local side: relay connection + local X Server proxy
    │   └── src/
    │       ├── connector.rs    # Client connection, auto-reconnect, session resume, X11 relay
    │       └── ssh.rs          # SSH tunnel client
    └── rx11-cli/               # Unified CLI entry point
        └── src/
            └── main.rs         # CLI arg parsing, config loading, subcommand dispatch
```

---

## FAQ

**Q: No display after connecting and running a GUI program?**

Make sure your local X Server is running and listening on TCP port 6000. The client auto-checks X Server availability before connecting:
- Windows: Check "Disable access control" when launching VcXsrv
- macOS: Install and launch XQuartz
- Linux: Verify Xorg / XWayland is running

**Q: Getting `Cannot open display` error?**

You need to set the `DISPLAY` environment variable on the remote server:

```bash
export DISPLAY=:0
# Or directly
DISPLAY=:0 your-gui-program
# Or use rx11 run to set it automatically
rx11 run your-gui-program
```

**Q: How to enable debug logging?**

```bash
RUST_LOG=rx11=debug rx11 client -r server:7000 -t <TOKEN>
```

**Q: Do I need to manually reconnect after a network interruption?**

No. The client has a built-in auto-reconnect mechanism using exponential backoff (starting at 1s, max 30s, up to 10 retries). Only recoverable errors (network interruption, timeout, etc.) trigger retries; authentication failures do not. On reconnect, it automatically attempts to resume the previous session. Already-running GUI programs continue to work within the 60-second grace period.

**Q: Port conflict when starting SSH tunnel?**

`rx11 ssh` auto-assigns a local ephemeral port by default, so conflicts are rare. If you use `-d` to manually specify a display, the local port is `17000 + display`. A specific error message is shown on conflict.

**Q: Is it secure?**

- Token authentication prevents unauthorized connections
- Server automatically manages `xauth` entries (MIT-MAGIC-COOKIE-1) to restrict X11 access
- All data is encrypted via SSH in SSH mode
- In TCP direct mode, use a firewall to restrict access to port 7000
- Ctrl+C automatically sends `SessionDestroy` to clean up remote sessions
- Token verification uses constant-time comparison to prevent timing attacks
- Session and connection ownership validation prevents cross-session data injection
- Auth data length limits (auth_name 256B / auth_data 4KB)
- Decompression output size verification prevents zip bomb attacks
- Frame parsing supports sync recovery to prevent malformed data from breaking connections

---

## License

MIT
