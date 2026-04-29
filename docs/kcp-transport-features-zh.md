# Aggligator KCP 传输层功能文档

## 概述

`aggligator-transport-kcp` 是为 Aggligator 多链路聚合框架开发的 KCP 传输层模块。KCP（Fast and Reliable ARQ Protocol）是一种基于 UDP 的可靠传输协议，相比 TCP，在高丢包率和高延迟的网络环境下拥有更优的性能表现。

本模块以独立 crate 的形式提供，包含三大核心功能：

1. **KCP 基础传输**（默认功能）
2. **FEC 前向纠错**（可选，`fec` feature）
3. **Phantun 伪 TCP 伪装**（可选，`phantun` feature）

---

## 1. KCP 基础传输

### 功能说明

基于 `kcp-tokio 0.4.0` 库，实现了 Aggligator 的 `ConnectingTransport`（客户端）和 `AcceptingTransport`（服务端）两个核心 trait，提供 KCP over UDP 的可靠传输能力。

### 核心组件

| 组件 | 说明 |
|------|------|
| `KcpConnector` | 客户端连接器，实现 `ConnectingTransport` trait，负责发起 KCP 连接 |
| `KcpAcceptor` | 服务端接受器，实现 `AcceptingTransport` trait，负责监听和接受 KCP 连接 |
| `KcpLinkTag` | 链路标识，通过远程地址和连接方向唯一标识一条 KCP 链路 |
| `KcpConfig` | KCP 协议配置，支持 `fast_mode`、`turbo_mode`、`normal_mode` 等预设模式 |

### 技术要点

- **Sync 兼容性处理**：`KcpStream` 仅实现 `Send` 而不实现 `Sync`，而 Aggligator 的 `IoBox` 要求 `Send + Sync`。通过 `tokio::io::split()` 将 `KcpStream` 分割为 `ReadHalf` 和 `WriteHalf`（内部使用 `Arc<Mutex<T>>`），自动满足 `Send + Sync` 约束。额外引入了 `SyncIo` 包装器以确保类型安全。
- **DNS 动态解析**：`KcpConnector` 支持主机名作为目标地址，定期重新解析 DNS（默认 10 秒间隔），无需重建传输实例即可跟踪 DNS 更新。
- **重复链路过滤**：`link_filter` 方法确保同一远程地址不会建立重复链路。
- **双栈绑定**：服务端同时绑定 IPv6 和 IPv4 地址，兼容 macOS（IPv6 socket 不接受 IPv4 连接）和 Linux（双栈模式下 IPv4 绑定可能因 IPv6 已覆盖而失败）。

### 使用方式

```rust
// 客户端
use aggligator_transport_kcp::{KcpConnector, KcpConfig};

let kcp_config = KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None);
let connector = KcpConnector::new(
    vec!["server:5801".into()],
    5801,
    kcp_config,
).await?;

// 服务端
use aggligator_transport_kcp::{KcpAcceptor, KcpConfig};

let kcp_config = KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None);
let acceptor = KcpAcceptor::new("[::]:5801".parse().unwrap(), kcp_config);
```

---

## 2. FEC 前向纠错

通过 `fec` feature 启用，基于 Reed-Solomon 纠删码在 UDP 数据包层面提供前向纠错能力。在有丢包的网络中，无需等待 KCP 重传即可恢复丢失的数据包，显著降低延迟。

### 工作原理

#### 编码端（发送方）

1. 将待发送的 UDP 数据报按 `data_shards` 个分组为一个 FEC 块
2. 对每个块计算 `parity_shards` 个冗余校验分片
3. 所有分片（数据 + 校验）加上 FEC 头部后独立发送

#### 解码端（接收方）

1. 按 `block_id` 收集属于同一块的分片
2. 只要收到至少 `data_shards` 个分片（数据或校验均可），即可重建完整数据
3. 恢复后按序交付原始数据报

### FEC 数据包格式

```
[block_id: 4字节][shard_idx: 1字节][shard_count: 1字节][parity_count: 1字节][payload_len: 2字节][payload...]
```

FEC 头部共 9 字节。每个分片的有效载荷最大 1500 字节。

### 配置参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `data_shards` | 3 | 每个 FEC 块的数据分片数量。越大则吞吐量越高，但延迟也会增加 |
| `parity_shards` | 1 | 每个 FEC 块的校验分片数量。越大则容错越强，但带宽开销也越大 |

**带宽开销**：`parity_shards / data_shards`。默认配置（3:1）的开销约为 33%，可容忍每块 1 个丢包。

### 关键实现细节

- **分片长度嵌入**：每个数据分片的有效载荷前添加 2 字节的原始长度前缀，该前缀参与 Reed-Solomon 编码，因此即使分片被重建也能恢复原始长度。
- **滑动窗口管理**：解码器维护最多 256 个进行中的块、512 个已完成块 ID，通过滑动窗口机制防止历史数据占用过多内存。
- **防重复交付**：已完成的块 ID 被记录在集合中，迟到的分片不会导致重复交付。
- **不完整块刷新**：提供 `flush_fec_encoder` 函数，可在连接空闲时刷新不完整的块，确保缓冲的数据分片被发送。

### 使用方式

```rust
use aggligator_transport_kcp::fec::FecConfig;

// 3 个数据分片，1 个校验分片
let fec_cfg = FecConfig::new(3, 1);

let mut connector = KcpConnector::new(hosts, port, kcp_config).await?;
connector.set_fec(fec_cfg);

// 服务端同理
let mut acceptor = KcpAcceptor::new(bind_addr, kcp_config);
acceptor.set_fec(fec_cfg);
```

### 组合支持

FEC 可与 Phantun 叠加使用。启用时，数据流经：

```
应用数据 → KCP → FEC 编码 → Phantun 伪 TCP / 或原始 UDP → 网络
```

---

## 3. Phantun 伪 TCP 伪装

通过 `phantun` feature 启用。将 KCP 的 UDP 流量伪装成 TCP 流量，使用用户态伪 TCP 协议栈通过 TUN 接口发送报文，从而穿越阻止 UDP 的防火墙和 NAT 设备。底层的数据报语义被完整保留，不会引入 TCP 的重传和流控机制。

### 工作原理

#### 架构

系统通过创建 TUN 虚拟网络接口，在用户态实现了一个最小化的伪 TCP 协议栈（与 [Phantun](https://github.com/dndx/phantun) 项目的线格式兼容）。

提供两种传输类型：

| 类型 | 说明 |
|------|------|
| `PhantunClientTransport` | 客户端传输，建立到服务器的一条伪 TCP 连接 |
| `PhantunServerTransport` | 服务端传输，接收多条伪 TCP 连接，将其多路复用到 KCP 所需的无连接 `Transport` 接口 |

#### 伪 TCP 握手流程

1. 客户端发送 SYN
2. 服务端回复 SYN+ACK
3. 客户端发送 ACK 完成握手
4. 连接建立后，数据通过伪 TCP（带 ACK 标志的数据包）传输
5. 连接关闭时发送 RST

#### TUN 接口默认地址

| 角色 | `tun_local`（操作系统侧） | `tun_peer`（Phantun 侧） |
|------|---------------------------|--------------------------|
| 客户端 | 192.168.200.1 | 192.168.200.2 |
| 服务端 | 192.168.201.1 | 192.168.201.2 |

### 跨平台支持

基于 `tun2` 库提供跨平台 TUN 支持：

| 平台 | 权限要求 | 路由/NAT 工具 |
|------|----------|---------------|
| Linux | root 或 `CAP_NET_ADMIN` | `iptables -t nat` 或 `nftables` |
| macOS | root | `pfctl` |
| Windows | 管理员 + Wintun 驱动 | `netsh` 或 route |
| FreeBSD, Android, iOS | 平台相关权限 | 平台相关工具 |

此外需要在主机上启用 IP 转发。

### 关键实现细节

- **IPv4/IPv6 双栈**：支持 IPv4 和 IPv6 数据包的构建与解析，运行时根据目标地址自动选择。
- **TCP 校验和计算**：使用 `internet-checksum` 库计算标准 TCP 校验和（含伪首部），确保报文能被中间网络设备正确转发。
- **并发安全的 send**：通过 `fetch_add` 原子地预留序列号空间，避免并发发送者之间的竞态条件。
- **ACK 节流**：仅当未确认的数据量超过 128 MiB 时才发送独立 ACK，减少不必要的控制开销。
- **连接超时与重试**：握手阶段超时时间为 1 秒，最多重试 6 次。
- **连接清理**：`FtcpSocket` 在 `Drop` 时自动发送 RST 并清理连接映射表。
- **本地缓存加速**：TUN 读取器维护一个本地连接映射缓存，避免每个数据包都竞争共享锁。

### 配置

```rust
use aggligator_transport_kcp::phantun::PhantunConfig;

// 使用默认客户端地址
let client_cfg = PhantunConfig::client_default();

// 使用默认服务端地址
let server_cfg = PhantunConfig::server_default();

// 自定义配置
let custom_cfg = PhantunConfig {
    tun_name: "mytun".to_string(),      // 空字符串 = 由内核自动分配
    tun_local: "10.0.0.1".parse().unwrap(),
    tun_peer: "10.0.0.2".parse().unwrap(),
    tun_local6: None,                    // 可选 IPv6 地址
    tun_peer6: None,
};
```

### 使用方式

```rust
// 客户端
let mut connector = KcpConnector::new(hosts, port, kcp_config).await?;
connector.set_phantun(PhantunConfig::client_default());

// 服务端
let mut acceptor = KcpAcceptor::new(bind_addr, kcp_config);
acceptor.set_phantun(PhantunConfig::server_default());
```

---

## 4. agg-tunnel 命令行集成

KCP 传输已集成到 `agg-tunnel` 命令行工具中，支持通过命令行参数启用所有功能。

### 客户端参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `--kcp <地址>` | 指定 KCP 服务器地址（支持主机名），默认端口 5801 | `--kcp server:5801` |
| `--kcp-fec <配置>` | 启用 FEC，格式为 `数据分片:校验分片` | `--kcp-fec 3:1` |
| `--kcp-phantun` | 启用 Phantun 伪 TCP 伪装（使用默认客户端地址） | `--kcp-phantun` |

### 服务端参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `--kcp <端口>` | 指定 KCP 监听端口 | `--kcp 5801` |
| `--kcp-fec <配置>` | 启用 FEC，必须与客户端一致 | `--kcp-fec 3:1` |
| `--kcp-phantun` | 启用 Phantun 伪 TCP 伪装（使用默认服务端地址） | `--kcp-phantun` |

### 使用示例

```bash
# 基础 KCP 传输
agg-tunnel server --kcp 5801 -p 8080
agg-tunnel client --kcp myserver:5801 -p 8080:8080

# KCP + FEC（3 数据分片 + 1 校验分片）
agg-tunnel server --kcp 5801 --kcp-fec 3:1 -p 8080
agg-tunnel client --kcp myserver:5801 --kcp-fec 3:1 -p 8080:8080

# KCP + Phantun 伪 TCP
agg-tunnel server --kcp 5801 --kcp-phantun -p 8080
agg-tunnel client --kcp myserver:5801 --kcp-phantun -p 8080:8080

# KCP + FEC + Phantun 全功能
agg-tunnel server --kcp 5801 --kcp-fec 3:1 --kcp-phantun -p 8080
agg-tunnel client --kcp myserver:5801 --kcp-fec 3:1 --kcp-phantun -p 8080:8080

# KCP 与 TCP 混合使用（多链路聚合）
agg-tunnel server --tcp 5800 --kcp 5801 -p 8080
agg-tunnel client --tcp myserver:5800 --kcp myserver:5801 -p 8080:8080
```

---

## 5. Feature 与依赖关系

### Cargo Feature

| Feature | 说明 | 引入的依赖 |
|---------|------|------------|
| （默认） | 基础 KCP over UDP 传输 | `kcp-tokio` |
| `fec` | Reed-Solomon 前向纠错 | `reed-solomon-erasure` |
| `phantun` | 伪 TCP 伪装 | `tun2`、`pnet`、`internet-checksum`、`bytes`、`rand`、`flume` |

### 启用方式

```toml
[dependencies]
# 仅基础 KCP
aggligator-transport-kcp = "0.1.0"

# KCP + FEC
aggligator-transport-kcp = { version = "0.1.0", features = ["fec"] }

# KCP + Phantun
aggligator-transport-kcp = { version = "0.1.0", features = ["phantun"] }

# 全部功能
aggligator-transport-kcp = { version = "0.1.0", features = ["fec", "phantun"] }
```

---

## 6. 传输模式组合矩阵

| 模式 | 数据流路径 | 适用场景 |
|------|-----------|----------|
| KCP | 应用 → KCP → UDP → 网络 | 通用高丢包/高延迟环境 |
| KCP + FEC | 应用 → KCP → FEC → UDP → 网络 | 丢包率较高，需要降低重传延迟 |
| KCP + Phantun | 应用 → KCP → Phantun → TUN → 网络 | UDP 被防火墙阻止 |
| KCP + FEC + Phantun | 应用 → KCP → FEC → Phantun → TUN → 网络 | UDP 被阻止且丢包率较高 |
