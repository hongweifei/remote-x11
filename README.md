# rx11 — 远程 X11 转发工具

通过自研协议中继 X11 连接，让你在本地电脑（内网）查看远程服务器（公网）上运行的 GUI 程序画面。

支持 **TCP 直连** 和 **SSH 隧道** 两种模式。

## 工作原理

```
 远程服务器 (公网)                              本地电脑 (内网)
 ┌────────────────────────┐                  ┌────────────────────────┐
 │  GUI 程序 (X11 Client) │                  │  X Server              │
 │         ↓               │                  │  (VcXsrv / Xorg)       │
 │  rx11 server            │  ◄── rx11 协议 ──►│  rx11 client           │
 │  ├─ Relay  :7000        │     (TCP / SSH)  │  └─ 连接本地 :6000     │
 │  └─ X11 Proxy :6000+N  │                  │                        │
 └────────────────────────┘                  └────────────────────────┘
```

**核心流程：**

1. 远程服务器启动 `rx11 server`，监听中继端口 (7000) 并代理 X11 端口 (6000+N)
2. 本地电脑启动 `rx11 client`，通过 TCP 或 SSH 隧道连接到远程中继
3. 远程服务器设置 `DISPLAY=:0`，运行 GUI 程序
4. GUI 程序的 X11 绘图指令经中继转发到本地 X Server，画面呈现在本地屏幕上

## 特性

- **多连接多路复用** — 单个中继会话上承载多个 X11 应用连接，通过 `connection_id` 区分
- **自动重连 + 会话恢复** — 网络中断后客户端自动指数退避重连（最多 10 次），服务端保留会话 60 秒宽限期，重连后已建立的 X11 应用连接不受影响
- **双向心跳检测** — 客户端和服务端互相发送心跳，90 秒无响应自动断开，避免半开连接
- **X Server 自动检测** — 连接前自动探测本地 X Server 是否可用，并给出平台特定提示
- **xauth 集成** — 服务端自动管理 `xauth` 条目（MIT-MAGIC-COOKIE-1），增强安全性
- **优雅关闭** — Ctrl+C 时发送 `SessionDestroy` 帧清理远程会话；`rx11 run` 自动将信号转发给子进程
- **连接统计** — 每 30 秒输出收发字节数、帧数、活跃连接数等统计信息
- **配置文件** — 支持 TOML 配置文件，CLI 参数 > 环境变量 > 配置文件 > 默认值
- **多 Display** — 通过 `-d` 参数同时运行多个独立的 GUI 会话
- **Display 自动分配** — 默认由服务端自动分配可用的 Display 编号，需要时可手动指定
- **数据压缩** — 支持 zstd/lz4/zlib 三种算法，自动协商，超过 64 字节的数据自动压缩，压缩后更大则回退
- **安全加固** — 会话/连接归属权限校验、auth 数据长度限制、解压大小验证、帧同步恢复、read_buf 上限防 DoS
- **SSH 端口冲突检测** — 启动 SSH 隧道前自动检测本地端口是否被占用

## 快速开始

### 编译安装

需要 Rust 工具链（[安装 Rust](https://rustup.rs)）。

```bash
git clone <repo-url> remote-x11 && cd remote-x11
cargo build --release
# 二进制文件位于 target/release/rx11
```

### 前置条件

本地电脑需要一个 X Server：

| 平台 | 推荐方案 |
|---|---|
| Linux | 系统自带 Xorg / Wayland + XWayland |
| macOS | [XQuartz](https://www.xquartz.org/) |
| Windows | [VcXsrv](https://sourceforge.net/projects/vcxsrv/) 或 [Xming](https://sourceforge.net/projects/xming/) |

启动 X Server 后，确认它监听在 TCP 端口 6000（默认行为）。

---

### 模式一：TCP 直连

适用于本地能直接访问远程服务器端口的情况，或已自行建立隧道的情况。

**第 1 步：生成 Token（任意一台机器）**

```bash
rx11 gen-token
# 输出示例：a3f8b2c1d4e5...
```

**第 2 步：远程服务器 — 启动服务**

```bash
rx11 server -t <TOKEN>
```

默认监听 `0.0.0.0:7000`（中继）和 `6000`（X11 代理）。用 `--help` 查看所有选项。

**第 3 步：本地电脑 — 启动客户端**

```bash
rx11 client -r <远程IP>:7000 -t <TOKEN>
```

**第 4 步：远程服务器 — 运行 GUI 程序**

Display 编号见客户端输出日志，例如 `Session created for display :0`。

```bash
rx11 run xclock
rx11 run -d 0 firefox
# 或手动设置 DISPLAY
DISPLAY=:0 xclock
```

---

### 模式二：SSH 隧道（推荐）

自动建立 SSH 端口转发，无需开放额外端口，数据全程加密。

```bash
# 本地电脑，一条命令完成
rx11 ssh -H <远程IP> -u <用户名> -t <TOKEN>
```

远程服务器同样需要先运行 `rx11 server`。

**手动指定 Display：**

```bash
rx11 ssh -H <远程IP> -u <用户名> -t <TOKEN> -d 1
```

---

## 命令参考

### `rx11 server` — 启动远程服务

```
Usage: rx11 server [OPTIONS]

Options:
  -l, --listen <LISTEN>        监听地址 [default: 0.0.0.0:7000]
  -x, --x11-port <X11_PORT>    X11 代理起始端口 [default: 6000]
  -t, --token <TOKEN>          认证 Token (也可用 RX11_TOKEN 环境变量)
```

不指定 `-t` 时会自动生成并打印 Token。

### `rx11 client` — 启动本地客户端

```
Usage: rx11 client [OPTIONS]

Options:
   -r, --relay <RELAY>          中继服务器地址 (必填)
   -t, --token <TOKEN>          认证 Token (也可用 RX11_TOKEN 环境变量)
   -x, --x11 <X11>              本地 X Server 地址 [default: 127.0.0.1:6000]
   -d, --display <DISPLAY>      手动指定 Display 编号 (默认自动分配)
```

默认自动分配 Display 编号。指定 `-d` 后切换为手动模式。

### `rx11 ssh` — 通过 SSH 隧道连接

```
Usage: rx11 ssh --host <HOST> [OPTIONS]

Options:
   -H, --host <HOST>                  远程服务器地址 (必填)
   -P, --port <PORT>                  SSH 端口 [default: 22]
   -u, --user <USER>                  SSH 用户名
   -i, --identity <IDENTITY>          SSH 私钥文件路径
   -t, --token <TOKEN>                认证 Token (也可用 RX11_TOKEN 环境变量)
   -r, --relay-port <RELAY_PORT>      远程中继端口 [default: 7000]
   -x, --x11 <X11>                    本地 X Server 地址 [default: 127.0.0.1:6000]
   -d, --display <DISPLAY>            手动指定 Display 编号 (默认自动分配)
```

默认自动分配 Display 编号，同时自动选择本地临时端口。指定 `-d` 后切换为手动模式，本地端口为 `17000 + display`。

### `rx11 run` — 运行 GUI 程序

自动设置 `DISPLAY` 环境变量并执行指定命令，同时将 SIGINT/SIGTERM 信号转发给子进程：

```bash
rx11 run xclock
rx11 run -d 1 firefox
rx11 run -- gedit /etc/hosts
```

```
Options:
  -d, --display <DISPLAY>      X11 Display 编号 [default: 0]
  <command>...                  要运行的命令及其参数
```

### `rx11 gen-token` — 生成认证 Token

```bash
rx11 gen-token
```

输出一个 SHA-256 随机 Token，用于客户端-服务端之间的认证。

### `rx11 config` — 配置管理

```bash
rx11 config init   # 生成默认配置文件 ~/.config/rx11/config.toml
rx11 config path   # 显示配置文件路径
```

---

## 配置文件

支持 TOML 格式的配置文件（默认路径 `~/.config/rx11/config.toml`），可省去每次输入重复参数。

优先级：**CLI 参数 > 环境变量 > 配置文件 > 默认值**

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
# token, relay_port, x11, display 同样支持
```

使用 `rx11 config init` 生成带注释的模板文件。

---

## 多 Display 支持

通过 `-d` 参数手动指定 Display 编号，可同时运行多个独立的 GUI 会话：

```bash
# 终端 1：Display :0
rx11 client -r server:7000 -t <TOKEN> -d 0

# 终端 2：Display :1
rx11 client -r server:7000 -t <TOKEN> -d 1
```

远程服务器上：

```bash
DISPLAY=:0 xclock       # 画面出现在终端 1
DISPLAY=:1 xeyes        # 画面出现在终端 2
# 或使用 rx11 run
rx11 run -d 0 xclock
rx11 run -d 1 xeyes
```

Display 编号 `N` 对应远程服务器的 X11 端口 `6000 + N`。

---

## Display 自动分配

`client` 和 `ssh` 命令默认自动分配 Display 编号，无需手动管理。服务端会在 SessionAck 响应中返回分配的 Display 编号（见客户端日志输出）。远程服务器上使用对应的 `DISPLAY` 值运行 GUI 程序即可。

需要手动指定时，使用 `-d` 参数即可切换为手动模式：

```bash
# TCP 模式
rx11 client -r server:7000 -t <TOKEN> -d 1

# SSH 模式
rx11 ssh -H server -u user -t <TOKEN> -d 1
```

---

## 会话恢复

客户端断开连接后，服务端会在 **60 秒宽限期**内保留会话（包括已建立的 X11 应用连接）。客户端在此期间重连时会自动恢复会话，已运行的 GUI 程序不受影响。

```bash
# 客户端自动重连（无需任何额外操作）
rx11 client -r server:7000 -t <TOKEN>
# 网络中断后自动重连，已打开的 GUI 程序继续工作
```

重连流程：

1. 客户端携带上次的 `session_id` 发起 Hello
2. 认证后发送 `SessionResume` 请求恢复
3. 服务端验证 session 存在且在宽限期内，恢复会话
4. 已有的 X11 应用连接继续通过中继转发数据

超过 60 秒宽限期后，服务端会自动销毁过期会话并释放资源。

---

## 环境变量

| 变量 | 说明 |
|---|---|
| `RX11_TOKEN` | 认证 Token，等同于 `-t` 参数 |
| `RUST_LOG` | 日志级别，如 `rx11=debug` 开启调试日志 |

---

## 协议格式

rx11 使用自定义二进制帧协议，每帧结构：

```
┌──────────┬──────────┬────────────┬─────────────┐
│ Magic    │ Type     │ Length     │ Payload     │
│ 4 bytes  │ 1 byte   │ 4 bytes    │ N bytes     │
│ RX11     │          │ (BE u32)   │             │
└──────────┴──────────┴────────────┴─────────────┘
```

帧类型：

| 类型 | 值 | 用途 |
|---|---|---|
| Hello | 0x01 | 客户端/服务端握手（可携带 `resume_session_id` 用于会话恢复） |
| HelloAck | 0x02 | 握手响应 |
| AuthRequest | 0x03 | 认证请求 |
| AuthResponse | 0x04 | 认证结果 |
| SessionCreate | 0x10 | 创建 X11 转发会话（指定 Display） |
| SessionAck | 0x11 | 会话创建/恢复结果（含 `session_id`） |
| SessionDestroy | 0x12 | 销毁会话 |
| SessionResume | 0x13 | 恢复已有的会话 |
| SessionAutoCreate | 0x14 | 自动分配 Display 并创建会话 |
| DataX11 | 0x20 | X11 数据帧（二进制，非 JSON） |
| CompressedDataX11 | 0x21 | 压缩的 X11 数据帧 |
| X11Connect | 0x22 | X11 应用连接通知 |
| X11Disconnect | 0x23 | X11 应用断开通知 |
| Heartbeat | 0x30 | 心跳（双向） |
| HeartbeatAck | 0x31 | 心跳响应 |
| Error | 0xFF | 错误 |

控制帧的 Payload 使用 JSON 编码。X11 数据帧使用二进制编码：

```
┌───────────────┬─────────────────┐
│ Connection ID │ X11 Data        │
│ 4 bytes       │ remaining bytes │
│ (BE u32)      │                 │
└───────────────┴─────────────────┘
```

CompressedDataX11 帧格式：

```
┌───────────────┬──────────────┬─────────────────┐
│ Connection ID │ Original Len │ Compressed Data │
│ 4 bytes       │ 4 bytes      │ remaining bytes │
│ (BE u32)      │ (BE u32)     │                 │
└───────────────┴──────────────┴─────────────────┘
```

`X11Connect` / `X11Disconnect` 帧携带 `{display, connection_id}`，用于通知对端 X11 应用的连接与断开。多连接通过 `connection_id` 实现多路复用。

连接建立流程：

```
Client                          Server
  │─── Hello ──────────────────►│  (可携带 resume_session_id)
  │◄── HelloAck ────────────────│
  │─── AuthRequest ────────────►│
  │◄── AuthResponse ────────────│
  │─── SessionCreate ──────────►│  (或 SessionResume / SessionAutoCreate)
  │◄── SessionAck ──────────────│  (含 session_id)
  │◄──► X11Connect/Disconnect ──│  (X11 应用连接/断开通知)
  │◄──► DataX11 (双向) ────────►│  (X11 数据转发，含 connection_id)
  │◄──► Heartbeat / HeartbeatAck│  (双向保活)
  │─── SessionDestroy ──────────►│  (可选，显式销毁)
```

---

## 项目结构

```
remote-x11/
├── Cargo.toml                  # Workspace 根配置
└── crates/
    ├── rx11-core/              # 核心库：协议定义、传输层、认证、统计
    │   └── src/
     │       ├── protocol.rs     # 帧编解码、消息类型定义
     │       ├── transport.rs    # 异步传输层（帧同步恢复、缓冲区限制）
     │       ├── compress.rs     # 数据压缩（zstd/lz4/zlib 协商与编解码）
     │       ├── auth.rs         # Token 生成与验证（常量时间比较、空 token 拒绝）
    │       ├── stats.rs        # 连接统计（字节数、帧数、活跃连接等）
    │       └── error.rs        # 错误类型（支持可重试判断）
    ├── rx11-server/            # 远程端：中继服务器 + X11 监听
    │   └── src/
    │       ├── relay.rs        # 中继服务主逻辑（握手、认证、会话管理、心跳检测）
    │       ├── session.rs      # 会话管理器（支持持久化、宽限期、自动分配 Display）
    │       └── x11_listener.rs # X11 端口监听与代理（多连接多路复用）
    ├── rx11-client/            # 本地端：连接中继 + 本地 X Server 代理
    │   └── src/
    │       ├── connector.rs    # 客户端连接、自动重连、会话恢复、X11 数据转发
    │       └── ssh.rs          # SSH 隧道客户端
    └── rx11-cli/               # 统一 CLI 入口
        └── src/
            └── main.rs         # 命令行参数解析、配置文件加载、子命令分发
```

---

## 常见问题

**Q: 连接后运行 GUI 程序没有画面？**

确认本地 X Server 已启动并监听 TCP 6000 端口。客户端会在连接前自动检测 X Server 可用性并给出提示：
- Windows：启动 VcXsrv 时需勾选 "Disable access control"
- macOS：安装并启动 XQuartz
- Linux：确认 Xorg / XWayland 正常运行

**Q: 报错 `Cannot open display`？**

远程服务器上需要设置 `DISPLAY` 环境变量：

```bash
export DISPLAY=:0
# 或者直接
DISPLAY=:0 your-gui-program
# 或者使用 rx11 run 自动设置
rx11 run your-gui-program
```

**Q: 如何查看调试日志？**

```bash
RUST_LOG=rx11=debug rx11 client -r server:7000 -t <TOKEN>
```

**Q: 网络断开后需要手动重连吗？**

不需要。客户端内置自动重连机制，使用指数退避策略（初始 1 秒，最长 30 秒，最多重试 10 次），仅对可恢复的错误（网络中断、超时等）进行重试，认证失败不会重试。重连时会自动尝试恢复之前的会话，已运行的 GUI 程序在 60 秒宽限期内可以继续工作。

**Q: SSH 隧道启动报端口被占用？**

`rx11 ssh` 默认自动分配本地临时端口，通常不会冲突。如果使用 `-d` 手动指定 Display，则本地端口为 `17000 + display`，冲突时会提示具体错误。

**Q: 是否安全？**

- Token 认证防止未授权连接
- 服务端自动管理 `xauth` 条目（MIT-MAGIC-COOKIE-1），限制 X11 访问
- SSH 模式下所有数据经 SSH 加密传输
- TCP 直连模式下建议配合防火墙限制 7000 端口的访问来源
- Ctrl+C 退出时自动发送 `SessionDestroy` 清理远程会话
- Token 验证使用常量时间比较，防止时序攻击
- 会话和连接归属权限校验，防止跨会话数据注入
- auth 数据长度限制（auth_name 256B / auth_data 4KB）
- 解压输出大小验证，防止 zip bomb 攻击
- 帧解析支持同步恢复，防止畸形数据导致连接不可用

---

## License

MIT
