# Aggligator KCP Transport 开发文档

## 1. 项目概述

为 Aggligator 多链路聚合框架添加 KCP (Fast and Reliable ARQ Protocol) 传输层支持。KCP 是基于 UDP 的可靠传输协议，相比 TCP 在高丢包、高延迟网络环境下有更好的性能表现。

本开发将创建一个新的 crate `aggligator-transport-kcp`，并在 `aggligator-util` 的 `agg-tunnel` 工具中集成 KCP 传输选项。

### 1.1 使用的库

- **kcp-tokio 0.4.0**: 基于 tokio 的异步 KCP 实现
  - 文档: https://docs.rs/kcp-tokio/0.4.0/kcp_tokio/
  - 提供 `KcpStream` (实现 `AsyncRead + AsyncWrite`)
  - 提供 `KcpListener` (接受连接)
  - 提供 `KcpConfig` (配置管理)

---

## 2. 架构分析

### 2.1 Aggligator Transport 抽象层

Aggligator 通过两个核心 trait 抽象传输层:

#### ConnectingTransport (连接端/客户端)
```rust
#[async_trait]
pub trait ConnectingTransport: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()>;
    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox>;
    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool;
    async fn connected_links(&self, links: &[Link<LinkTagBox>]) {}
}
```

#### AcceptingTransport (监听端/服务端)
```rust
#[async_trait]
pub trait AcceptingTransport: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()>;
    async fn link_filter(&self, new: &BoxLink, existing: &[BoxLink]) -> bool;
}
```

#### LinkTag (链路标识)
```rust
pub trait LinkTag: Debug + Display + Send + Sync + 'static {
    fn transport_name(&self) -> &str;
    fn direction(&self) -> Direction;
    fn user_data(&self) -> Vec<u8>;
    fn as_any(&self) -> &dyn Any;
    fn box_clone(&self) -> LinkTagBox;
    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering;
    fn dyn_hash(&self, state: &mut dyn Hasher);
}
```

#### StreamBox (流类型)

传输层返回 `StreamBox`，有两种变体:
- `StreamBox::Io(IoBox)` — 基于 IO 流 (AsyncRead + AsyncWrite)，适用于 TCP、KCP
- `StreamBox::TxRx(TxRxBox)` — 基于包 (Sink + Stream)，适用于 WebSocket

```rust
pub struct IoBox {
    pub read: Pin<Box<dyn AsyncRead + Send + Sync + 'static>>,
    pub write: Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>,
}
```

### 2.2 现有 TCP Transport 实现模式 (参考模板)

文件: `aggligator-transport-tcp/src/lib.rs`

**关键组件**:
1. `TcpLinkTag` — 实现 `LinkTag`，包含 interface/remote/direction
2. `TcpConnector` — 实现 `ConnectingTransport`
   - `link_tags()`: 定期解析主机名 + 枚举网络接口，生成标签集合
   - `connect()`: 创建 TCP socket，连接，split 后包装为 `IoBox`
3. `TcpAcceptor` — 实现 `AcceptingTransport`
   - `listen()`: 循环 accept 连接，split 后包装为 `IoBox`

**IO 流包装方式**:
```rust
let (rh, wh) = tcp_stream.into_split();
Ok(IoBox::new(rh, wh).into()) // StreamBox::Io
```

### 2.3 agg-tunnel 工具集成模式

文件: `aggligator-util/src/bin/agg-tunnel.rs`

**客户端 (ClientCli)**:
- CLI 参数: `--tcp`, `--rfcomm`, `--usb` 等声明传输目标
- 创建对应 Connector，添加到 `ConnectorBuilder`
- 使用 feature flag 控制可选传输: `#[cfg(feature = "bluer")]`

**服务端 (ServerCli)**:
- CLI 参数: `--tcp`, `--rfcomm`, `--usb` 等声明监听端口
- 创建对应 Acceptor，添加到 `AcceptorBuilder`
- 使用 feature flag 控制可选传输

---

## 3. 关键技术挑战与解决方案

### 3.1 KcpStream 不实现 Sync (核心问题)

**问题**: `KcpStream<UdpTransport>` 实现了 `Send` 但 **没有实现 `Sync`**。
而 `IoBox` 要求 read/write 半部分都满足 `Send + Sync + 'static`。

**解决方案**: 使用 `tokio::io::split()` 分割 `KcpStream`。

`tokio::io::split()` 返回的 `ReadHalf<T>` 和 `WriteHalf<T>` 内部使用 `Arc<Mutex<T>>`，只要 `T: Send` 即可同时满足 `Send + Sync`。

```rust
use tokio::io::split;

let kcp_stream = KcpStream::connect(addr, config).await?;
let (read_half, write_half) = split(kcp_stream);
Ok(IoBox::new(read_half, write_half).into())
```

> **验证**: `tokio::io::ReadHalf<T>` 和 `WriteHalf<T>` where `T: Send` 均自动实现 `Send + Sync`，因为内部通过 `Arc<Mutex<...>>` 共享所有权。`KcpStream` 满足 `Send`，所以可行。

### 3.2 KCP 基于 UDP, 无独立网络接口绑定

**问题**: 
- TCP 可以通过 `bind_device` 绑定到不同网络接口，从而利用多接口优势
- KCP 底层使用 UDP socket，`kcp-tokio` 的标准 API (`KcpStream::connect`/`KcpListener::bind`) 不暴露底层 UDP socket 配置

**解决方案**:
- 初期版本(**v1**)不做多接口绑定，类似 TCP 的 `multi_interface = false` 模式
- KCP 的每个目标地址生成一个 link tag（不区分本地接口）
- 后续版本可考虑通过 `connect_with_transport` 自定义 `UdpTransport` 来绑定到特定接口

### 3.3 KcpListener 生命周期管理

**问题**: `KcpListener` 需要持续运行并可接受多个连接，但它不是 `Clone`。在 `AcceptingTransport::listen()` 中需要持续调用 `accept()`。

**解决方案**: `KcpListener` 由 `KcpAcceptor` 持有（在 `listen()` 方法内创建并使用），生命周期与 `listen()` 函数一致。`listen()` 是一个长期运行的 async 函数，循环调用 `listener.accept()`。

### 3.4 KcpConfig 传递

**问题**: `KcpConfig` 需要在 Connector 和 Acceptor 中共享使用。

**解决方案**: `KcpConfig` 实现了 `Clone`，直接在 `KcpConnector` 和 `KcpAcceptor` 中持有一份 clone。用户可选择 `fast_mode()`, `turbo_mode()`, `normal_mode()` 等预设，也可自定义。

---

## 4. 详细设计

### 4.1 crate 结构

```
aggligator-transport-kcp/
├── Cargo.toml
├── LICENSE          (从其他 transport crate 复制)
├── NOTICE           (从其他 transport crate 复制)
├── README.md
└── src/
    └── lib.rs
```

### 4.2 Cargo.toml

```toml
[package]
name = "aggligator-transport-kcp"
version = "0.1.0"
description = "Aggligator transport: KCP"
categories = ["asynchronous", "network-programming"]
keywords = ["aggligator", "aggligator-transport", "kcp"]
readme = "README.md"
edition.workspace = true
rust-version.workspace = true
authors.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
aggligator = { version = "0.9.7", path = "../aggligator" }

async-trait = { workspace = true }
futures = { workspace = true }
tracing = { workspace = true }
tokio = { workspace = true, features = ["net", "io-util"] }

kcp-tokio = "0.4.0"

[package.metadata.docs.rs]
all-features = true
rustdoc-args = ["--cfg", "docsrs"]
```

### 4.3 KcpLinkTag

```rust
/// 链路标识，用于识别 KCP 链路
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KcpLinkTag {
    /// 远端地址 (UDP 端口)
    pub remote: SocketAddr,
    /// 链路方向
    pub direction: Direction,
}
```

**实现 LinkTag trait**:
- `transport_name()` → `"kcp"`
- `direction()` → 返回 `self.direction`
- `user_data()` → 空 `Vec<u8>` (v1 无多接口区分)
- 其余按标准模式实现

### 4.4 KcpConnector

```rust
/// KCP 传输层连接器 (客户端)
#[derive(Clone, Debug)]
pub struct KcpConnector {
    /// 目标主机列表 (含端口)
    hosts: Vec<String>,
    /// KCP 配置
    kcp_config: KcpConfig,
    /// 主机名解析间隔
    resolve_interval: Duration,
}
```

**方法**:
- `new(hosts, default_port, kcp_config) -> Result<Self>`: 创建并验证主机名可解析
- `set_kcp_config(&mut self, config: KcpConfig)`: 更新 KCP 配置
- `set_resolve_interval(&mut self, interval: Duration)`: 设置解析间隔

**ConnectingTransport 实现**:

| 方法 | 行为 |
|------|------|
| `name()` | 返回 `"kcp"` |
| `link_tags()` | 定期解析主机名 → `SocketAddr` 集合 → 为每个地址创建 `KcpLinkTag` |
| `connect()` | `KcpStream::connect(addr, config)` → `tokio::io::split()` → `IoBox` → `StreamBox` |
| `link_filter()` | 检查是否已有相同远端地址的链路 (避免重复) |

**link_tags() 详细流程**:
```
loop {
    1. 解析所有 hosts → Vec<SocketAddr>
    2. 为每个 SocketAddr 创建 KcpLinkTag { remote, direction: Outgoing }
    3. tx.send_if_modified(更新标签集合)
    4. sleep(resolve_interval)
}
```

**connect() 详细流程**:
```
1. 从 tag 中 downcast 获取 KcpLinkTag
2. let stream = KcpStream::connect(tag.remote, self.kcp_config.clone())
       .await
       .map_err(|e| std::io::Error::other(e))?;
   → 注意: KcpError 没有实现 Into<io::Error>，必须使用 map_err 显式转换
3. let (rh, wh) = tokio::io::split(stream)
4. Ok(IoBox::new(rh, wh).into())
```

### 4.5 KcpAcceptor

```rust
/// KCP 传输层接受器 (服务端)
#[derive(Debug)]
pub struct KcpAcceptor {
    /// 监听地址
    bind_addr: SocketAddr,
    /// KCP 配置
    kcp_config: KcpConfig,
}
```

**方法**:
- `new(addr: SocketAddr, kcp_config: KcpConfig) -> Self`: 创建 (不立即绑定)

**AcceptingTransport 实现**:

| 方法 | 行为 |
|------|------|
| `name()` | 返回 `"kcp"` |
| `listen()` | 绑定 KcpListener → 循环 accept → split → 发送 AcceptedStreamBox |
| `link_filter()` | 默认 true (不过滤) |

**listen() 详细流程**:
```
1. let mut listener = KcpListener::bind(self.bind_addr, self.kcp_config.clone())
       .await
       .map_err(|e| std::io::Error::other(e))?;
   → 注意: KcpError → io::Error 必须 map_err，不能直接用 ?
2. loop {
       let (stream, remote_addr) = listener.accept().await?
       let tag = KcpLinkTag { remote: remote_addr, direction: Incoming }
       let (rh, wh) = tokio::io::split(stream)
       tx.send(AcceptedStreamBox::new(IoBox::new(rh, wh).into(), tag)).await
   }
```

### 4.6 错误转换

`kcp-tokio` 使用自己的 `KcpError` 类型，需要转换为 `std::io::Error`:

```rust
fn kcp_err_to_io(err: kcp_tokio::KcpError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
}
```

或者如果 `KcpError` 实现了 `Into<std::io::Error>` 或 `std::error::Error`，可以使用 `Error::other(err)`。

---

## 5. agg-tunnel 集成

### 5.1 workspace Cargo.toml 修改

在 `[workspace] members` 中添加:
```toml
members = [
    ...
    "aggligator-transport-kcp",
    ...
]
```

### 5.2 aggligator-util/Cargo.toml 修改

添加 KCP 依赖 (默认启用):
```toml
[dependencies]
aggligator-transport-kcp = { version = "0.1.0", path = "../aggligator-transport-kcp" }
```

### 5.3 agg-tunnel CLI 参数

#### 客户端 (ClientCli)

新增参数:
```rust
/// KCP server address (host:port or IP:port).
#[arg(long)]
kcp: Vec<String>,
```

#### 服务端 (ServerCli)

新增参数:
```rust
/// KCP port to listen on.
#[arg(long)]
kcp: Option<u16>,
```

### 5.4 客户端集成代码 (ClientCli::run)

参照 TCP 的集成模式:

```rust
// 在 ClientCli::run() 中
let kcp_connector = if !self.kcp.is_empty() {
    let kcp_config = KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None);
    match KcpConnector::new(self.kcp.clone(), KCP_DEFAULT_PORT, kcp_config).await {
        Ok(kcp) => {
            targets.push(format!("KCP {kcp}"));
            watch_conn.push(Box::new(kcp.clone()));
            Some(kcp)
        }
        Err(err) => {
            eprintln!("cannot use KCP target: {err}");
            None
        }
    }
} else {
    None
};

// 在创建 connector 后:
if let Some(c) = kcp_connector.clone() {
    connector.add(c);
}
```

### 5.5 服务端集成代码 (ServerCli::run)

```rust
// 在 ServerCli::run() 中
if let Some(port) = self.kcp {
    let bind_addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);
    // 预检查: 验证 UDP 端口是否可用
    match tokio::net::UdpSocket::bind(bind_addr).await {
        Ok(_) => {
            let kcp_config = KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None);
            let kcp_acceptor = KcpAcceptor::new(bind_addr, kcp_config);
            server_ports.push(format!("KCP :{port}"));
            acceptor.add(kcp_acceptor);
        }
        Err(err) => eprintln!("Cannot listen on KCP port {port}: {err}"),
    }
}
```

### 5.6 常量定义

```rust
const KCP_DEFAULT_PORT: u16 = 5801; // 与 TCP (5800) 区分，避免用户混淆
```

---

## 6. 处理要点与潜在问题

### 6.1 ConnectingTransport 要求 Send + Sync

`KcpConnector` 需要满足 `Send + Sync + 'static`。
- `KcpConfig` 是 `Send + Sync + Clone` ✓
- `Vec<String>` 是 `Send + Sync` ✓
- `Duration` 是 `Send + Sync` ✓

结论: `KcpConnector` 自动满足 `Send + Sync` ✓

### 6.2 KcpStream split 的 Sync 要求

- `tokio::io::split(kcp_stream)` 返回 `ReadHalf<KcpStream>` 和 `WriteHalf<KcpStream>`
- 内部使用 `Arc<Mutex<KcpStream>>`，因此只要 `KcpStream: Send` 即可满足 `Sync`
- `KcpStream` 实现了 `Send` ✓
- 故 `ReadHalf` / `WriteHalf` 满足 `AsyncRead/AsyncWrite + Send + Sync + 'static` ✓

### 6.3 KcpListener 的 accept() 需要 &mut self

`KcpListener::accept(&mut self)` 需要可变引用。在 `listen()` 方法中，`KcpListener` 是局部变量，以可变方式持有，不存在问题。

### 6.4 KCP 配置模式选择

`kcp-tokio` 提供多种预设模式:
- `KcpConfig::new()` — 默认配置
- `.fast_mode()` — 快速模式 (推荐用于 tunnel)
- `.turbo_mode()` — 极速模式
- `.normal_mode()` — 标准模式
- `KcpConfig::gaming()` — 游戏优化
- `KcpConfig::file_transfer()` — 文件传输优化
- `KcpConfig::realtime()` — 实时通信优化

**v1 方案**: 默认使用 `fast_mode()`，暂不暴露配置选项到 CLI。后续可添加 `--kcp-mode` 参数。

### 6.5 DNS 解析复用

TCP transport 使用 `aggligator-transport-tcp::util::resolve_hosts` 进行 DNS 解析。KCP transport 可以:
- **方案 A**: 依赖 `aggligator-transport-tcp` 复用其 util 模块 (类似 WebSocket transport 的做法)
- **方案 B**: 直接使用 `tokio::net::lookup_host` 自行实现

**推荐方案 B**: KCP transport 不需要 TCP 的接口绑定逻辑，直接使用 tokio 标准 API 更简洁，减少不必要的依赖。

```rust
use tokio::net::lookup_host;

async fn resolve_hosts(hosts: &[String]) -> Vec<SocketAddr> {
    let mut addrs = HashSet::new();
    for host in hosts {
        if let Ok(resolved) = lookup_host(host).await {
            addrs.extend(resolved);
        }
    }
    let mut addrs: Vec<_> = addrs.into_iter().collect();
    addrs.sort();
    addrs
}
```

### 6.6 Display 实现

`KcpConnector` 和 `KcpAcceptor` 需要实现 `Display` 以便于日志和 UI 显示:

```rust
impl fmt::Display for KcpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.hosts.len() > 1 {
            write!(f, "[{}]", self.hosts.join(", "))
        } else {
            write!(f, "{}", &self.hosts[0])
        }
    }
}

impl fmt::Display for KcpAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.bind_addr)
    }
}
```

### 6.7 IPv6 支持

KCP 底层使用 UDP，`KcpStream::connect` 和 `KcpListener::bind` 接受 `SocketAddr`，天然支持 IPv4 和 IPv6。无需额外处理。

服务端绑定 `[::]:port` 即可同时接受 IPv4 和 IPv6 连接。

---

## 7. 开发步骤 (实施顺序)

### 第一步: 创建 crate 骨架
1. 创建 `aggligator-transport-kcp/` 目录结构
2. 编写 `Cargo.toml`
3. 复制 `LICENSE` 和 `NOTICE` 文件
4. 将 crate 添加到 workspace `Cargo.toml` 的 members

### 第二步: 实现核心类型
1. 实现 `KcpLinkTag` 结构体及其 `LinkTag` trait
2. 实现错误转换辅助函数

### 第三步: 实现 KcpConnector
1. 结构体定义与构造函数
2. DNS 解析辅助函数
3. 实现 `ConnectingTransport` trait
4. 实现 `Display`, `Debug`

### 第四步: 实现 KcpAcceptor
1. 结构体定义与构造函数
2. 实现 `AcceptingTransport` trait
3. 实现 `Display`, `Debug`

### 第五步: 集成到 agg-tunnel
1. 修改 `aggligator-util/Cargo.toml` 添加依赖
2. 修改 `agg-tunnel.rs` 添加 CLI 参数
3. 在 `ClientCli::run()` 中集成 KcpConnector
4. 在 `ServerCli::run()` 中集成 KcpAcceptor

### 第六步: 编译与测试
1. `cargo build` 确认编译通过
2. 手动测试: 启动 server `--kcp <port> -p <port>`, 客户端 `--kcp <addr:port> -p <port>:<port>`
3. 验证数据转发正常工作

---

## 8. 完整 lib.rs 伪代码概览

```rust
// aggligator-transport-kcp/src/lib.rs

use aggligator::{
    control::Direction,
    io::{IoBox, StreamBox},
    transport::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox},
    Link,
};
use async_trait::async_trait;
use kcp_tokio::{KcpConfig, KcpListener, KcpStream};
use std::{
    any::Any, cmp::Ordering, collections::HashSet, fmt, hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result}, net::SocketAddr, time::Duration,
};
use tokio::{io::split, net::lookup_host, sync::{mpsc, watch}};

static NAME: &str = "kcp";

// ===== KcpLinkTag =====
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KcpLinkTag { pub remote: SocketAddr, pub direction: Direction }
// impl Display for KcpLinkTag { ... }
// impl LinkTag for KcpLinkTag { ... }

// ===== KcpConnector =====
#[derive(Clone, Debug)]
pub struct KcpConnector { hosts: Vec<String>, kcp_config: KcpConfig, resolve_interval: Duration }
// impl KcpConnector { pub async fn new(...) -> Result<Self> { ... } }
// impl Display for KcpConnector { ... }
// impl ConnectingTransport for KcpConnector { ... }

// ===== KcpAcceptor =====
#[derive(Debug)]
pub struct KcpAcceptor { bind_addr: SocketAddr, kcp_config: KcpConfig }
// impl KcpAcceptor { pub fn new(...) -> Self { ... } }
// impl Display for KcpAcceptor { ... }
// impl AcceptingTransport for KcpAcceptor { ... }
```

---

## 9. 测试策略

### 9.1 手动测试

```bash
# 终端 1: 启动 KCP tunnel 服务端，转发端口 22
cargo run --bin agg-tunnel -- server --kcp 5800 -p 22

# 终端 2: 启动 KCP tunnel 客户端，连接并映射到本地 2222 端口
cargo run --bin agg-tunnel -- client --kcp 127.0.0.1:5800 -p 22:2222

# 终端 3: 通过 tunnel 连接
ssh -p 2222 localhost
```

### 9.2 与 TCP 混合测试

验证 KCP 和 TCP 可以同时作为 transport 使用:

```bash
# 服务端同时监听 TCP 和 KCP
cargo run --bin agg-tunnel -- server --tcp 5800 --kcp 5801 -p 22

# 客户端同时使用 TCP 和 KCP 链路
cargo run --bin agg-tunnel -- client --tcp 127.0.0.1:5800 --kcp 127.0.0.1:5801 -p 22:2222
```

---

## 10. 文档审查: 已发现的问题与修正

> 以下是对本文档进行自审后发现的问题，按严重程度排序。
> 实施时务必按修正方案处理。

### 10.1 [严重] stream_mode 未在代码示例中启用

**问题**: 文档中所有代码示例使用 `KcpConfig::new().fast_mode()`，但查看 kcp-tokio 源码:
- `KcpConfig::default()` 中 `stream_mode: false`
- `fast_mode()` 仅修改 `nodelay` 参数，**不会启用 stream_mode**

`stream_mode = false` 时，KCP 按消息模式工作，每次 `send` 是独立消息包。而 Aggligator 的 `IntegrityCodec` 将数据作为连续字节流处理（通过 `AsyncRead`/`AsyncWrite`），如果 KCP 不启用 stream_mode，可能导致数据边界不对齐，影响 codec 解帧。

**修正**: 所有构建 `KcpConfig` 的地方必须显式启用 stream_mode:
```rust
let kcp_config = KcpConfig::new().fast_mode().stream_mode(true);
```

### 10.2 [严重] KcpError 到 std::io::Error 的转换方向

**问题**: 文档 4.6 节的错误转换描述不够精确。查看 kcp-tokio 源码:
- `KcpError` 实现了 `From<std::io::Error>` (io::Error → KcpError) ✓
- `KcpError` **没有**实现 `Into<std::io::Error>` (KcpError → io::Error) ✗
- `KcpError` 通过 `thiserror` 实现了 `std::error::Error + Display`

因此在 `connect()` 和 `listen()` 中不能直接使用 `?` 算子将 `kcp_tokio::Result` 转换为 `std::io::Result`。

**修正**: 必须使用 `map_err` 显式转换:
```rust
let stream = KcpStream::connect(addr, config)
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?;
```
`Error::other()` 接受 `impl Into<Box<dyn Error + Send + Sync>>`，`KcpError` 实现了 `Error`，所以也可以:
```rust
    .map_err(|e| std::io::Error::other(e))?;
```

### 10.3 [中等] KcpAcceptor 延迟绑定 — 与 TCP 行为不一致

**问题**: 文档设计 `KcpAcceptor::new()` 为同步、不可失败方法（仅保存地址），绑定推迟到 `listen()` 中。
但 `TcpAcceptor::new()` 是 async 且立即绑定端口，如果端口被占用会在启动时立即报错。

当前 KCP 方案下，端口被占用的错误只会在 `listen()` 被框架调用时出现（延迟报错），用户的服务端启动时看不到明确的绑定失败提示。

**根本原因**: `KcpListener::accept(&mut self)` 需要 `&mut self`，而 `AcceptingTransport::listen(&self, ...)` 接收 `&self`。如果预先绑定并存储 `KcpListener`，需要 `Mutex` 包装。

**修正方案 (推荐)**: 保持当前延迟绑定设计（避免 Mutex开销），但在 agg-tunnel 集成时做预检查:
```rust
if let Some(port) = self.kcp {
    let bind_addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);
    // 预检查: 尝试绑定 UDP 端口验证可用性
    match tokio::net::UdpSocket::bind(bind_addr).await {
        Ok(_) => {
            let kcp_config = KcpConfig::new().fast_mode().stream_mode(true);
            let kcp_acceptor = KcpAcceptor::new(bind_addr, kcp_config);
            server_ports.push(format!("KCP :{port}"));
            acceptor.add(kcp_acceptor);
        }
        Err(err) => eprintln!("Cannot listen on KCP port {port}: {err}"),
    }
}
```

### 10.4 [中等] tokio::io::split 的性能开销

**问题**: TCP transport 使用 `TcpStream::into_split()`，返回的 `OwnedReadHalf`/`OwnedWriteHalf` 无锁、零开销，因为读写由 OS 独立管理。

而 KCP 必须使用 `tokio::io::split()` (因为 `KcpStream` 没有 `into_split`)，其内部使用 `Arc<Mutex<KcpStream>>`，读写操作会互相竞争锁。在高吞吐场景下可能成为瓶颈。

**影响评估**: Aggligator 的 IntegrityCodec 对每个链路的读写是交替进行的（非高并发），锁竞争通常不大。但在极端情况下可能有影响。

**修正**: 当前方案可接受，但应在文档中标注。未来可考虑:
- 向 kcp-tokio 提 PR 添加 `into_split()` 方法
- 自行封装无锁读写分离（如果 kcp-tokio 内部支持）

### 10.5 [中等] KCP 与 Aggligator 双重 keep-alive 冗余

**问题**: KCP 有自己的 keep-alive 机制 (`KcpConfig.keep_alive`，默认 30s)，Aggligator 也有连接级别的 keep-alive/心跳。两者同时运行会产生冗余的网络流量。

**修正**: 禁用 KCP 层的 keep-alive，让 Aggligator 管理连接活性:
```rust
let kcp_config = KcpConfig::new()
    .fast_mode()
    .stream_mode(true)
    .keep_alive(None);  // 禁用 KCP keep-alive
```

### 10.6 [轻微] 默认端口与 TCP 冲突

**问题**: `KCP_DEFAULT_PORT = 5800` 与 `TCP_PORT = 5800` 相同。虽然 UDP/TCP 的端口命名空间在 OS 层面独立（不会真正冲突），但用户在命令行使用时容易混淆（"我是连的 TCP 还是 KCP？"）。

**修正**: 使用不同的默认端口:
```rust
const KCP_DEFAULT_PORT: u16 = 5801;
```

### 10.7 [轻微] 主机名端口拼接逻辑未显式展示

**问题**: TCP connector 在 `new()` / `unresolved()` 中对不含端口号的主机名追加默认端口:
```rust
for host in &mut hosts {
    if !host.contains(':') {
        host.push_str(&format!(":{default_port}"));
    }
}
```
文档的 `KcpConnector::new()` 提到接受 `default_port` 参数，但没有显式展示这段逻辑。

**修正**: 实现时需包含相同的端口拼接逻辑。

### 10.8 [轻微] 缺少 KCP 连接重连行为说明

**问题**: 当一条 KCP 链路断开时, Aggligator 框架会调用 `connect()` 尝试重连。每次重连都会创建全新的 `KcpStream`（新的 UDP socket + 新的 KCP conversation ID）。这与 TCP 的重连行为一致——但 KCP 每次重连的 UDP 源端口可能不同，服务端的 `KcpListener` 需要能接受来自不同源端口的新连接。

**影响**: `KcpListener` 天然支持接受多个独立连接（不同 conv_id），所以这不是问题。但值得在设计中显式确认。

### 10.9 [轻微] IntegrityCodec 与 KCP 双重可靠性

**问题**: Aggligator 的 `IntegrityCodec` 在 IO stream 上添加帧头 + CRC32 校验。KCP 本身已经提供可靠传输和数据完整性保证。两层一起使用有冗余。

**影响**: 这与 TCP transport 的行为一致（TCP 也有可靠传输，仍然使用 IntegrityCodec）。IntegrityCodec 的主要作用是帧分界（length-prefix framing），不仅仅是校验。所以冗余是可接受的。

**修正**: 无需修改，与项目现有设计一致。

---

## 11. 注意事项

1. **kcp-tokio 版本锁定**: 使用 `0.4.0` 版本，API 可能在未来版本变化
2. **UDP 端口冲突**: KCP 和 TCP 使用不同的默认端口，建议 KCP 用 5801
3. **防火墙**: KCP 使用 UDP，需确保防火墙允许 UDP 流量
4. **NAT 穿透**: KCP/UDP 的 NAT 穿透行为与 TCP 不同，可能需要注意
5. **stream_mode**: 必须显式启用 `stream_mode(true)`
6. **tokio feature**: 需要 `tokio` 的 `io-util` feature (用于 `tokio::io::split`)
7. **keep_alive**: 应禁用 KCP 层 keep-alive `keep_alive(None)`，避免与 Aggligator 冗余
8. **KcpConfig 最终构建**: `KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None)`
