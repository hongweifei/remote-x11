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
- **Heartbeat keepalive** — Server sends heartbeats every 30s and the client replies immediately; the server drops a silent client after 90s, the client times out and reconnects after 100s without data — no half-open connections
- **X Server auto-detection** — Probes local X Server availability before connecting, with platform-specific hints
- **xauth integration** — Server automatically manages `xauth` entries (MIT-MAGIC-COOKIE-1) for enhanced security
- **Graceful shutdown** — Sends `SessionDestroy` frame on Ctrl+C to clean up remote sessions; `rx11 run` forwards signals to child processes
- **Data compression** — `zstd_stream` by default: one continuous compression stream flushed per frame, so later frames reference the history of everything already sent (redraw-heavy traffic collapses on slow links); negotiates down to one-shot zstd/lz4/zlib. Payloads over 256 bytes are compressed
- **WAN queue discipline** — the outbound queue towards the relay is capped by **in-flight bytes** (default 1 MB), not frame count, so a 3 Mbit/s link never accumulates minutes of bufferbloat; an optional `SO_SNDBUF` cap tames the kernel buffer too
- **Image re-encoding (on by default)** — the `x11_recode` capability recognizes large core `PutImage` requests (ZPixmap, depth 24/32) and ships them as JPEG (lossy, 10–50× smaller) or lossless PNG on the wire; the client re-expands them into the original request bytes before the local X server sees anything. Non-image traffic passes byte-for-byte. On by default with the **lossless** PNG tier; either end setting `image_recode = false` forces byte-for-byte passthrough, and the lossy JPEG tier only exists as an explicit `image_recode_codec = "jpeg"` choice. Honest edge: GL-rendered video and Xv extension paths are out of recognition scope — that's framebuffer territory (Xpra/x2go), not a transparent forwarder's
- **Desktop audio forwarding (on by default)** — the `audio` channel: the server captures its default output sink's monitor, encodes with Opus (**pure-Rust** port, 48 kHz stereo, no system library build requirements) and streams it on the **real-time priority lane**; the client decodes and feeds `paplay`. The capture is **shared process-wide** — one recorder and one encoder fanned out to every client: N clients never spawn N `parec`s, late joiners pick up at the next packet, and the recorder stops itself when the last listener leaves. 64 kbit/s ≈ 2% of a 3 Mbit/s link; the lane bypasses the outbound byte budget, so audio latency under an image flood is bounded by one write chunk. On by default; `audio_forward = false` on either end disables it, and endpoints without a capture/playback stack (parec/paplay) degrade to silence without touching the session. Honest edge: playback only — no microphone back-channel, no lip sync
- **Per-channel flow control (on by default)** — the `flow_control` capability: when a receiver's accepted-but-unconsumed backlog crosses the watermark (default 2 MB), it advertises `Pause` for that channel (`connection_id` granularity, each leg gauged separately); the sender's bulk pump stops reading its source socket, so the stall retreats into kernel buffers and the producing application's own `write()` — lossless by construction, not one byte discarded. Draining below the low watermark (default 256 KB) advertises `Resume`. Flow-control frames ride the **real-time priority lane** and are re-advertised periodically: the signal never queues behind a redraw burst, and a lane drop self-heals by the next re-advertisement; control frames and the audio lane are never gated. A channel congested in both directions at once gates only the uplink — the downlink advertisement yields, because gating both legs freezes an X11 conversation while deferring fresh requests never does. Either end setting `flow_control = false` returns the chain to plain transport-level backpressure (the byte-budgeted queue)
- **Incremental transfer** — Repetitive X11 frames ship only changed chunks (optionally re-compressed), in BOTH directions: the picture downstream and the request stream upstream (the client→relay leg is gated separately by `incremental_uplink`); **a payload that did not change at all becomes a zero-chunk reference frame — 29 bytes on the wire (11-byte frame header + 18-byte diff header), none of the content travels twice** (the frame itself still goes out: the X11 byte stream is positional, so the sequence number must advance); a receiver auto-requests full frames when its diff cache goes stale
- **Capability negotiation** — incremental/uplink-incremental/resync/flow-control/recode/audio features are intersected at handshake and never used unless agreed; new capabilities ship by name, not by version bump
- **Channel-kind extensibility** — the data-plane frame family is decoupled from payload type: `ChannelOpen.kind` declares the channel type (today `x11` and `audio`), kinds *are* capability names, and unknown kinds pass through without parse failures
- **Two-sided statistics** — Client and server each log bytes, compression/incremental savings and active connections every 30 seconds
- **Config file** — TOML config support. Priority: CLI args > env vars > config file > defaults
- **Multiple displays** — Run multiple independent GUI sessions simultaneously with the `-d` flag
- **Auto display assignment** — Server automatically assigns available display numbers by default; manual override available
- **Security hardening** — Session/connection ownership validation, auth data length limits, decompression size verification, frame sync recovery, read_buf limits for DoS prevention
- **SSH port conflict detection** — Checks for local port conflicts before establishing SSH tunnels
- **SSH tunnel auto-rebuild** — if the tunnel process dies or the health check gives up, a fresh tunnel is respawned on the same local port with exponential backoff (2s→30s, up to 8 attempts per drop) while the connector keeps retrying, so the session resumes transparently inside the server's grace window (`ssh.reconnect = false` restores fail-fast)
- **Native control console (`rx11-gui`, FLTK)** — one desktop window for starting and stopping all three roles (relay / client / SSH tunnel), watching a live link scope and traffic counters, editing every `[client]` / `[server]` / `[ssh]` / `[runtime]` knob, and reading the log. It shares the CLI's `config.toml` and preserves comments and unknown keys on write; the file is only touched when Save is pressed

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

If the tunnel dies mid-session (link flaps, server restart), it is rebuilt automatically and the session resumes — no need to rerun the command. Rebuilds run unattended under BatchMode, so key auth (ssh-agent or a passphrase-free key) is required.

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

## Graphical Console (rx11-gui)

The repository also builds a native FLTK console that puts role control, the live link, configuration editing and the log in one window:

```bash
cargo run -p rx11-gui            # or target/release/rx11-gui
```

The window is four regions: a navigation rail (overview / server / client / SSH / runtime / logs), a top link band (`relay ⇄ client` with a pulse that moves with throughput), the working area, and a status bar showing the config path, an unsaved marker and the most recent notice.

- **Start and stop all three roles** — relay, direct client and SSH tunnel run in-process (the console does not fork the `rx11` binary), so the figures come from the same counters the link uses rather than from parsed logs
- **Link scope** — a 90-second envelope of both directions plus the compression savings ratio; hovering reads the exact rate at a point, and the axis snaps to 1/2/5 so three orders of magnitude stay legible
- **Configuration editing** — four pages cover every `config.toml` field with validation messages; saving rewrites the file with `toml_edit` in place, keeping comments, key order and unknown keys, and nothing is written until Save is pressed
- **Log panel** — level-coloured, searchable, exportable; events go to stderr and to a 4000-line ring in the window
- **Display scaling** — follows `GDK_SCALE`/`QT_SCALE_FACTOR` by default, or an explicit `RX11_GUI_SCALE=1.25`, so type is rendered at device resolution instead of being stretched by the compositor

Two switches exist for demos and for taking screenshots: `RX11_GUI_DEMO=1` opens the console on fixed sample data (no config reads or writes, no network), and `RX11_GUI_PAGE=logs` opens a specific page.

The code is organised as six layers with one-way dependencies: `settings`/`launcher` (domain) → `model` (the cross-layer contract) → `theme`/`state`/`ui` (presentation) → `backend`/`demo` (port implementations) → `app` (composition root), with a one-line `main.rs`. UI and data layer speak only flat `key → string` values, so a page never learns the config schema and a new runtime knob costs one field declaration. The direction of those dependencies is asserted by `crates/rx11-gui/tests/architecture.rs` rather than agreed verbally. Structure, design rationale and verification notes: [`crates/rx11-gui/DESIGN.md`](crates/rx11-gui/DESIGN.md).

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
rx11 config show   # Show config file path and current contents
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
# reconnect = true             # rebuild the tunnel when it dies (false = fail fast)
# token, relay_port, x11, display are also supported

[runtime]
# heartbeat_interval_secs = 30      server heartbeat period
# heartbeat_timeout_secs = 90       server drops silent clients after this
# client_read_timeout_secs = 100    client treats relay silence as death
# compression_threshold = 256       payloads below this size skip compression
# diff_chunk_size = 4096            incremental diff granularity
# diff_min_block_size = 1024        frames shorter than this are never diffed
# diff_min_savings_ratio = 0.1      minimum saved fraction for sending a diff
# incremental_uplink = true         diff the client uplink too (false = picture-only diffs)
# outbound_queue_bytes = 1048576    in-flight byte budget towards the WAN (queueing-delay cap)
# tcp_send_buffer_bytes = 262144    cap the kernel send buffer too; omit to let the OS autotune
# image_recode = true               image re-encoding master switch (default on; false on either end = passthrough)
# image_recode_codec = "png"          png (default, lossless) | jpeg (lossy — explicit choice only)
# image_recode_quality = 80         JPEG quality
# image_recode_min_bytes = 16384    pixel blocks below this pass through untouched
# audio_forward = true              desktop audio forwarding switch (default on; false on either end disables)
# audio_bitrate_kbps = 64           Opus bitrate (32 = speech-grade, 64 ≈ transparent)
# audio_record_command = []         override recorder argv (default parec; ships with PulseAudio AND PipeWire)
# audio_play_command = []           override player argv (default paplay --raw)
# flow_control = true               per-channel flow control switch (default on; false = transport backpressure only)
# flow_pause_bytes = 2097152        high watermark: unconsumed backlog per channel that triggers Pause
# flow_resume_bytes = 262144        low watermark: drain level that triggers Resume
```

On slow links (a 3 Mbit/s VPS running Xfce, say) set the last two explicitly:
queueing delay ≈ (`outbound_queue_bytes` + `tcp_send_buffer_bytes`) / link rate.
Compression is already cross-frame streaming zstd by default — no
compatibility trade-off is needed to save bandwidth.

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
┌──────────┬─────────┬───────────┬────────────┬─────────────┐
│ Magic    │ Version │ Type      │ Length     │ Payload     │
│ 4 bytes  │ 1 byte  │ 2 bytes   │ 4 bytes    │ N bytes     │
│ RX11     │ (u8=1)  │ (BE u16)  │ (BE u32)   │             │
└──────────┴─────────┴───────────┴────────────┴─────────────┘
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
| ChannelOpen | 0x18 | Open a multiplexed channel (`kind` + optional `display`) |
| ChannelClose | 0x19 | Close a channel by id |
| StreamResyncRequest | 0x1A | Ask peer to stop diffing and resend full frames (self-heal) |
| StreamData | 0x20 | Channel data frame (binary payload, type-agnostic) |
| CompressedStreamData | 0x21 | Compressed channel data frame |
| IncrementalStreamData | 0x22 | Channel incremental diff frame (binary) |
| CompressedIncrementalStreamData | 0x23 | Compressed incremental diff frame |
| Heartbeat | 0x30 | Heartbeat (server-initiated) |
| HeartbeatAck | 0x31 | Heartbeat response |
| FlowControl | 0x40 | Per-channel bulk Pause/Resume (rides the real-time lane) |
| Error | 0xFF | Error |

The current protocol version is **1**, enforced twice: a frame-header version octet and `Hello.version`; any mismatch fails the handshake immediately.

**Tag band convention**: `0x01-0x0F` handshake, `0x10-0x1F` JSON control (session/channel/resync), `0x20-0x2F` binary data-plane (0x24+ reserved for diff variants), `0x30-0x3F` keepalive, `0x40-0x4F` flow control, `0xF0-0xFF` errors. The Type field is **u16** (65536 slots): every tag allocated today sits in the low byte (pinned by test); `0x0100+` is held in reserve for future major partitions.

Bands are not a gentlemen's agreement — a `TagBand` registry plus a per-band roster test enforce them: **minting a frame requires registering its band and updating that band's expected roster, or CI goes red**, and squatting a tag in no-man's-land (0x05, 0x2C, …) fails the same way.

**Capability negotiation**: `Hello.capabilities` / `HelloAck.capabilities` advertise
feature names (`incremental`, `incremental_uplink`, `request_resync`, `flow_control`, …) and the server
replies with the intersection. Features outside the intersection are never used by
either side (`incremental` gates the picture downlink, `incremental_uplink` opens
diffing of the client's request stream; a receiver refuses diff shapes its direction
never negotiated); unrecognized names are ignored — shipping a new frame type later only
requires minting a new capability name, not a protocol bump.

Control frame payloads use JSON encoding. Binary `StreamData` frames look like:

```
┌───────────────┬─────────────┬─────────────────┐
│ Connection ID │ Sequence ID │ X11 Data        │
│ 4 bytes       │ 4 bytes     │ remaining bytes │
│ (BE u32)      │ (BE u32)    │                 │
└───────────────┴─────────────┴─────────────────┘
```

`CompressedStreamData` frames add an original-length field used to validate decompression:

```
┌───────────────┬─────────────┬──────────────┬─────────────────┐
│ Connection ID │ Sequence ID │ Original Len │ Compressed Data │
│ 4 bytes       │ 4 bytes     │ 4 bytes      │ remaining bytes │
│ (BE u32)      │ (BE u32)    │ (BE u32)     │                 │
└───────────────┴─────────────┴──────────────┴─────────────────┘
```

`IncrementalStreamData` frames start with `[conn][seq][base_seq][total_len][chunk_count: u16]` followed by `[offset][length][chunk]` per chunk; the receiver matches `base_seq` against its cache and sends `StreamResyncRequest` to resynchronize when it falls out of step — in either direction. Stacked compression wraps the whole encoded diff in `CompressedIncrementalStreamData`, decompressed before parsing.

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
  │◄──► ChannelOpen/Close ────│  (channel open/close, kind=x11)
  │◄──► StreamData (bidirectional)►│  (X11 data relay, includes connection_id)
  │◄──► StreamResyncRequest ────►│  (self-heal: request full frames, either direction)
  │◄──► Heartbeat / HeartbeatAck│  (server-driven keepalive)
  │─── SessionDestroy ──────────►│  (optional, explicit destroy)
```

---

## Project Structure

```
remote-x11/
├── Cargo.toml                    # Workspace root config (deps, release profile)
├── LICENSE                       # MIT license
├── .github/workflows/ci.yml      # CI: fmt / clippy / test / release build
└── crates/
    ├── rx11-core/                # Core lib: protocol, transport, auth, stats
    │   └── src/
    │       ├── protocol/         # Protocol layer
    │       │   ├── mod.rs        #   Frame constants and module facade
    │       │   ├── messages.rs   #   Payload structs for every frame kind
    │       │   ├── message_type.rs #  On-wire message type tags
    │       │   ├── binary.rs     #   Binary codecs for X11 data frames
    │       │   ├── frame.rs      #   Frame enum, derive macro, frame (de)serialization
    │       │   └── handshake.rs  #   Hello/Auth negotiation (shared by both sides)
    │       ├── transport.rs      # Async transport (frame sync recovery, buffer limits)
    │       ├── compress.rs       # Data compression (zstd/lz4/zlib negotiation)
    │       ├── incremental.rs    # Incremental diff encoding
    │       ├── auth.rs           # Token generation & verification (constant-time compare)
    │       ├── types.rs          # Validated newtypes (SessionId/Token/DisplayNumber, …)
    │       ├── stats.rs          # Connection stats (bytes, frames, active connections)
    │       ├── config.rs         # Centralized default constants (timeouts/ports/buffers)
    │       └── error.rs          # Error types (with retryable error support)
    ├── rx11-server/              # Remote side: relay server + X11 listener
    │   └── src/
    │       ├── relay/
    │       │   ├── mod.rs        #   RelayServer: accept loop & connection cap
    │       │   └── connection.rs #   Per-client connection task (sessions, heartbeat, forwarding)
    │       ├── session/
    │       │   ├── mod.rs        #   Session manager (grace period, auto display)
    │       │   ├── events.rs     #   Channel messages between listener and relay
    │       │   └── xauth.rs      #   xauth entry management
    │       └── x11_listener.rs   # X11 port listener & proxy (multi-connection multiplexing)
    ├── rx11-client/              # Local side: relay connection + local X Server proxy
    │   └── src/
    │       ├── connector.rs      # Connection, session create/resume, reconnect backoff
    │       ├── serve.rs          # Relay receive loop and frame dispatch
    │       ├── proxy.rs          # Local X11 app connections and socket pumps
    │       └── ssh.rs            # SSH tunnel client (health check + auto-rebuild)
    └── rx11-cli/                 # Unified CLI entry point
        └── src/
            ├── main.rs           # Logging init + subcommand dispatch
            ├── cli.rs            # clap argument definitions
            ├── settings.rs       # TOML config loading and template
            ├── util.rs           # X Server probing, port waiting
            └── commands/         # server / client / ssh / run / gen-token / config
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
