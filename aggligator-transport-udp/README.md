# Aggligator UDP 通道使用说明

`aggligator-transport-udp` 提供基于 UDP datagram 的 Aggligator 传输层。它不会在单条 UDP 链路内部做可靠、有序传输，而是把每个 UDP 包交给 Aggligator 聚合层，由聚合层统一完成排序、确认、重传、跨链路 hedge 和去重。

这使 UDP 通道适合低延迟、多链路和链路质量波动明显的场景，尤其推荐配合 `AggMode::LowLatency` 或 `AggMode::BandwidthRedundant` 使用。

## 适用场景

- 多网络路径聚合：例如 Wi-Fi + 蜂窝、多个网卡、多个公网入口。
- 低延迟交互：远程 shell、游戏控制、实时控制、小包 RPC。
- 有轻中度丢包或抖动的网络：UDP 不受 TCP 单链路队头阻塞影响，LowLatency 会按需发 hedge 副本降低尾延迟。

不建议把 UDP 通道当作单链路可靠传输直接使用。可靠性应该由 Aggligator 聚合连接承担。

## 推荐聚合模式

### `AggMode::Bandwidth`

吞吐优先，只在确认超时后走常规重传。带宽开销最低，但丢包时尾延迟会受 `link_ack_timeout_*` 影响。

### `AggMode::BandwidthRedundant`

带宽优先的 KCP 风格策略。主数据包仍优先铺满可用链路；当没有可立即发送的主数据、发送窗口受阻，或链路刚好有空闲发送机会时，会在冗余预算内对延迟未确认的数据包发送 hedge 副本。

适合“愿意多花一点带宽换低尾延迟，但仍希望多链路吞吐尽量打满”的场景。冗余开销同样受 `max_redundancy_overhead_percent` 控制，hedge 时机由 `hedge_delay_min` / `hedge_delay_max` 控制。

### `AggMode::LowLatency`

推荐的 UDP 默认策略。每个数据包先走一条主链路，如果在 hedge delay 后仍未确认，并且冗余预算允许，就在另一条可用链路上补发 hedge 副本。

特点：

- p50 通常接近最快可用链路。
- p95/p99 对丢包和抖动更稳。
- 冗余开销受 `max_redundancy_overhead_percent` 控制。
- 当前实现会优先把 hedge 发到近期 ack 表现好、RTT 更低的链路。

## Rust API 用法

服务端监听一个 UDP 地址：

```rust
use aggligator::cfg::{AggMode, Cfg};
use aggligator::transport::AcceptorBuilder;
use aggligator::connect::Server;
use aggligator_transport_udp::UdpAcceptor;
use std::net::SocketAddr;

let mut cfg = Cfg {
    agg_mode: AggMode::LowLatency,
    max_redundancy_overhead_percent: 20,
    ..Default::default()
};

// 建议 UDP payload 不超过路径 MTU。agg-tunnel 默认使用 1180 字节。
cfg.io_write_size = cfg.io_write_size.min(std::num::NonZeroUsize::new(1180).unwrap());

let server = Server::new(cfg);
let mut acceptor = AcceptorBuilder::new(server);
let bind: SocketAddr = "0.0.0.0:5802".parse()?;
acceptor.add(UdpAcceptor::new(bind));
```

客户端连接一个或多个 UDP 目标：

```rust
use aggligator::cfg::{AggMode, Cfg};
use aggligator::transport::ConnectorBuilder;
use aggligator::connect::connect;
use aggligator_transport_udp::{UdpConnector, UdpLinkFilter};
use std::num::NonZeroUsize;

let mut cfg = Cfg {
    agg_mode: AggMode::LowLatency,
    max_redundancy_overhead_percent: 20,
    hedge_delay_min: std::time::Duration::from_millis(1),
    hedge_delay_max: std::time::Duration::from_millis(30),
    ..Default::default()
};
cfg.io_write_size = cfg.io_write_size.min(NonZeroUsize::new(1180).unwrap());

let (task, outgoing, control) = connect(cfg);
let mut connector = ConnectorBuilder::new(control);

let mut udp = UdpConnector::new(vec!["server.example.com:5802".to_string()], 5802).await?;
udp.set_link_filter(UdpLinkFilter::InterfaceInterface);
connector.add(udp);
```

如果需要在每个本地接口上创建 UDP 链路，可以保持 `UdpConnector` 默认的 multi-interface 行为；如果只想使用一个系统路由出口，调用：

```rust
udp.set_multi_interface(false);
```

## `agg-tunnel` 命令行用法

服务端监听 UDP：

```bash
agg-tunnel server \
  --udp 0.0.0.0:5802 \
  --agg-mode low-latency \
  --udp-payload-size 1180 \
  --port 2222:22
```

客户端连接 UDP 服务端并转发端口：

```bash
agg-tunnel client \
  --udp server.example.com:5802 \
  --agg-mode low-latency \
  --udp-payload-size 1180 \
  --port 2222:2222
```

如果同时启用 TCP 和 UDP，Aggligator 会把它们都当成可用链路。低延迟场景下可以优先开启 UDP，并保留 TCP 作为额外路径。

多服务器中转时，可以在中转机上运行 UDP relay，把中转机公网 UDP 入口转发到真正的 Aggligator 服务端：

```bash
agg-tunnel udp-relay \
  --listen 0.0.0.0:5802 \
  --target server.example.com:5802
```

客户端同时连接服务端直连地址和中转机地址即可形成两条 UDP 链路：

```bash
agg-tunnel client \
  --udp server.example.com:5802 \
  --udp relay.example.com:5802 \
  --udp-single-interface \
  --udp-link-filter interface-ip \
  --agg-mode low-latency \
  --udp-payload-size 1180 \
  --port 2222:2222
```

这种方式不修改 Aggligator 数据包，只按客户端地址维护 UDP 映射并双向转发 datagram。中转机和服务端之间延迟低、丢包少时，它可以作为一条独立公网入口路径参与聚合。

如果目标是大流量转发，同时希望丢包或抖动时别退回秒级确认超时，可以把 `--agg-mode low-latency` 换成：

```bash
--agg-mode bandwidth-redundant
```

## 参数建议

### 通用低延迟配置

```rust
Cfg {
    agg_mode: AggMode::LowLatency,
    max_redundancy_overhead_percent: 20,
    hedge_delay_min: Duration::from_millis(1),
    hedge_delay_max: Duration::from_millis(30),
    link_flush_delay: Duration::from_millis(1),
    ..Default::default()
}
```

### 丢包较高或 RTT 不对称

- 提高 `max_redundancy_overhead_percent` 到 `20` 或 `30`。
- 保持 `hedge_delay_max` 在 30-80ms 范围，避免尾延迟退回到秒级确认超时。
- 使用 `UdpLinkFilter::InterfaceInterface`，避免同一接口和同一远端接口重复建太多等价链路。

### 高吞吐传输

- 如果更关心吞吐而不是尾延迟，使用 `AggMode::Bandwidth`。
- 如果仍想限制尾延迟，同时尽量让主数据优先占满链路，使用 `AggMode::BandwidthRedundant`，并把 `max_redundancy_overhead_percent` 控制在 `5` 到 `20`。
- 如果是交互小包或尾延迟优先，保留 `LowLatency`。
- 确保 `udp_payload_size` 不超过路径 MTU，避免 IP 分片。

## MTU 与 payload 大小

UDP 包过大时会触发 IP 分片，分片中任意一片丢失都会导致整个 datagram 丢失。建议：

- IPv6/跨公网保守值：`1180` 字节。
- 局域网或明确 jumbo frame：可以按实际 MTU 提高，但需要测试丢包率。
- 使用 `agg-tunnel` 时通过 `--udp-payload-size` 设置。
- 使用库 API 时限制 `cfg.io_write_size`。

## 链路过滤

`UdpLinkFilter` 控制是否接受看起来重复的 UDP 链路：

- `InterfaceInterface`：默认值。按本地接口 + 远端接口过滤，适合多网卡场景。
- `InterfaceIp`：按本地接口 + 远端 IP 过滤，适合远端端口可能变化但 IP 相同的场景。
- `None`：不过滤，可能创建较多重复链路，适合诊断或非常特殊的多路径环境。

## 排错

### 有连接但延迟偶尔到 1 秒以上

通常是丢包后走到了确认超时重传路径。优先尝试：

- 使用 `--agg-mode low-latency`。
- 降低 `hedge_delay_min` / `hedge_delay_max`。
- 提高 `max_redundancy_overhead_percent`。
- 检查 UDP 是否被 NAT、防火墙或运营商限速。

### 吞吐不如 TCP

UDP 低延迟模式会主动发送少量冗余副本，吞吐优先场景可以使用 `AggMode::Bandwidth`；如果还想保留低尾延迟，可以使用 `AggMode::BandwidthRedundant` 并降低冗余预算。

### 多接口没有产生多条链路

检查：

- 本机是否真的有多个可用网络接口。
- `UdpConnector::set_multi_interface(false)` 是否被设置。
- `UdpLinkFilter` 是否过滤掉了重复链路。
- 服务端是否用 `UdpAcceptor::all_interfaces(port)` 或多个 `UdpAcceptor::new(addr)` 监听对应接口。

### 多服务器中转没有形成第二条链路

检查：

- 服务端和中转机防火墙是否放行对应 UDP 端口。
- 中转机 `agg-tunnel udp-relay --listen ... --target ...` 是否正在运行。
- 客户端是否同时传入服务端地址和中转机地址。
- 使用 `--udp-single-interface` 时，建议配合 `--udp-link-filter interface-ip`，避免同一本地出口下的多目标路径被按接口误判为重复链路。
- 如果中转机到服务端走内网或专线地址，`--target` 应填写该低延迟地址，而不是必须填写服务端公网地址。

## 当前测试结论

本仓库的本地仿真测试显示：

- `Datagram LowLatency` 在丢包场景下能显著降低 p95/p99，且冗余开销远低于全量镜像策略。
- `Datagram BandwidthRedundant` 采用主数据优先的 hedge，适合大流量下保留低尾延迟，同时减少对吞吐调度的干扰。
- 推荐默认策略是 UDP + `AggMode::LowLatency`；高吞吐隧道可以优先试 UDP + `AggMode::BandwidthRedundant`，再按业务延迟和带宽预算调整 hedge 参数。

## License

Aggligator is licensed under the Apache 2.0 license.
