# Aggligator 传输重构目标与模式设计

本文替换旧的 transport/KCP/UDP 重构假设。

核心结论先写清楚：**聚合不等于把同一份数据同时发到所有宽带上**。默认模式应该是把不同数据包按链路能力分发到不同链路上，从而获得总带宽。复制同一份数据只应该是一个可选策略，例如镜像模式，或在丢包时用少量额外带宽换低延迟的修复模式。

我们的真实目标不是“必须用 KCP”或“必须用 UDP”，而是：

1. 多条宽带真的能叠加吞吐；
2. 丢包、抖动、弱网下端到端延迟低，尤其是 P95/P99；
3. 额外带宽开销可配置、可观测、可限制；
4. TCP、UDP、KCP、WebSocket、fake-TCP 等都只是底层路径，核心策略应该在聚合层统一表达。

---

## 0. 当前代码状态核对

这份文档描述的是目标架构和阶段计划。按当前代码核对，已有能力和未完成能力需要分清：

| 项目 | 当前状态 | 说明 |
|---|---|---|
| UDP transport | 已有基础实现 | `aggligator-transport-udp` 通过 `DatagramBox` 暴露 UDP datagram，并支持多本地接口监听/连接。 |
| 自包含 datagram frame | 已有基础实现 | `StreamBox::UnreliableDatagram` 会把 `Data { seq }` 和 payload 封进同一个 `AGD1` data datagram，控制消息走 control datagram。 |
| UDP MTU 保护 | 已有 CLI 保护 | `agg-tunnel --udp-payload-size` 默认 1180，并在启用 UDP 时限制 `io_write_size`，避免默认写入包超过保守 MTU。 |
| KCP transport | 当前未接入 workspace | 旧 `aggligator-transport-kcp` 已不在当前 workspace 成员里；如重新引入，应保持为可选 transport，而不是低延迟主线。 |
| 显式 `AggMode` | 已有基础实现 | `Cfg::agg_mode` 支持 `Bandwidth`、`BandwidthRedundant`、`LowLatency`，`agg-tunnel --agg-mode` 可选择 `bandwidth`、`bandwidth-redundant`、`low-latency`。`Mirror` 模式由于实际价值低于 `LowLatency` 且带宽开销高，已从实现中移除。 |
| 一个 seq 多个 in-flight copy | 已有基础实现 | `SentReliableStatus::InFlight` 可记录 primary、hedge、resend 多个 copy；任意 copy 的 ACK 都会完成该 seq 并释放其它 copy 的未确认记账。 |
| Hedge / overhead budget | 已有基础实现 | `LowLatency` 和 `BandwidthRedundant` 会在 `hedge_delay` 后对未 ACK 的 data seq 发一份跨链路 hedge，受 `max_redundancy_overhead_percent` 限制。 |
| FEC | 未实现 | 还没有 aggregate seq 层 FEC repair packet。 |
| 低延迟观测指标 | 部分实现 | 现有统计增加了 primary bytes、redundant bytes、overhead percent、hedge sent、hedge won、duplicate data dropped；还缺 FEC repaired/failed 等细分指标。 |
| 低延迟验证 | 已有机制级测试 | `datagram_low_latency_hedges_loss` 用双 UDP datagram link 模拟一条响应链路 data 全丢，验证 `LowLatency` 会跨链路 hedge，且 hedge copy 能先于原 copy 完成；真实端到端耗时、吞吐、P95/P99 还需要专门 benchmark。 |

因此，当前代码已经具备“UDP datagram 能跑进 aggligator core”的基础，也已经有了第一版 aggregate-layer hedge/mirror。它不再只是 `Bandwidth` 模式加 ACK-timeout 跨链路重发；现在已有确定性的 datagram 丢包集成测试证明 hedge 可以用额外带宽跨链路修复丢失 data。但要证明“真实弱网 P95/P99 媲美单链路 KCP”，还需要更完整的丢包/抖动/限速 benchmark。

---

## 1. 什么叫聚合

默认聚合应该是 **striping / weighted scheduling**：

```text
业务数据流
   │
   ├─ seq 1 ──► link A
   ├─ seq 2 ──► link B
   ├─ seq 3 ──► link A
   ├─ seq 4 ──► link C
   └─ seq 5 ──► link B
```

如果有三条链路：

| 链路 | 可用带宽 | 默认聚合行为 |
|---|---:|---|
| A | 100 Mbps | 发更多 primary packet |
| B | 50 Mbps | 发中等数量 primary packet |
| C | 20 Mbps | 发少量 primary packet |

默认情况下，每个 seq 只发一次。接收端按全局 seq 重排，应用层看到的仍是一条有序连接。

这才是“聚合宽带”。如果每个包都同时发到 A/B/C，那不是聚合吞吐，而是 mirror，高可靠但浪费带宽。

---

## 2. 模式分层

我们需要把“怎么选链路”和“要不要额外发副本”拆开。

### 2.1 Bandwidth 模式：纯聚合

目标：最大化总吞吐，额外带宽接近 0%。

行为：

- 每个 seq 选择一条 primary link；
- 链路选择基于权重、RTT、未确认字节、估计带宽、丢包率；
- ACK 超时后可以重发，但不主动复制；
- 适合下载、大文件、普通 TCP-like 流量。

优点：最省带宽，最符合“多宽带叠加”。

缺点：丢包时尾延迟可能变差，因为必须等超时或检测后再重发。

### 2.2 Hedge 模式：少量冗余换低延迟

目标：用可控额外带宽降低丢包和抖动造成的 P99 延迟。

行为：

1. seq 先发到 primary link；
2. 如果在很短时间内未 ACK，例如 `1.25 * RTT` 且至少 5ms，则把同一个 seq 复制到另一条 link；
3. 任意副本被 ACK，这个 seq 就算完成；
4. 额外副本计入全局 overhead budget，例如最多 10%。

```text
seq 42 ──► link A primary
          │
          └─ 5ms/1.25RTT 后仍未 ACK ──► link B hedge copy
```

这不是“所有包都复制”。只有疑似慢了、丢了、排队了的包才补一份，适合游戏、远程桌面、交互式 tunnel。

### 2.3 Budget Redundancy 模式：固定冗余预算

目标：类似“愿意浪费 10% 总带宽换更低延迟”的策略。

行为可以有两种实现：

- proactive copy：按概率或按风险复制少量包，例如每 10 个包中提前复制 1 个；
- small-window FEC：每 N 个 data packet 发送 M 个 repair packet，例如 10+1 约等于 10% 冗余。

这类模式必须有硬上限：

```text
max_overhead = 0.10
target = low_latency
repair_scope = aggregate_seq
```

FEC 不应该在 UDP transport 内部偷偷做，因为那会让聚合层看不到丢包。更合适的位置是聚合层，以全局 seq 为单位做 repair。

### 2.4 Mirror 模式：完整镜像

目标：最低恢复延迟，最高可用性，不追求省带宽。

行为：

- 每个 seq 同时发送到 N 条 link，甚至所有 link；
- 第一个到达的副本被接收，后续副本去重丢弃；
- 适合非常小但非常关键的数据，或极端弱网保命模式。

代价很明确：

| mirror 数 | 带宽开销 |
|---:|---:|
| 2 条 | 约 100% 额外开销 |
| 3 条 | 约 200% 额外开销 |

所以 mirror 只能是显式模式，不能是默认聚合行为。

### 2.5 KCP Link 模式：兼容或快速落地

KCP 可以作为一种底层 link：

```text
aggligator aggregate layer
        │
        ▼
KCP over UDP link
        │
        ▼
UDP socket / fake-TCP wrapper / network
```

它的价值：

- 对单条丢包链路有成熟的低延迟 ARQ；
- 可以满足当前 aggligator 对 link “可靠、有序”的要求；
- 对不方便改 core 的阶段，可能是最快能跑起来的方案。

它的问题：

- KCP 在单条 link 内部修复丢包，聚合层看不到真实丢包信号；
- 丢包包可能先在坏链路里重传，而不是立刻跨链路补发；
- 和 aggligator 自己的 ACK/reorder/resend 形成双层可靠性，容易增加 head-of-line blocking；
- 真正的多链路低延迟策略会被削弱。

结论：KCP 可以作为 transport mode 支持，但不应该是最终架构里唯一的低延迟机制。最终应该让聚合层直接支持不可靠 datagram 和跨链路修复。

---

## 3. 修改后架构需要支持什么

旧 aggligator link contract 默认 link 自己保证完整、可靠、有序。也就是说，TCP、WebSocket、TLS stream、KCP stream 都能天然匹配旧的 `TxRxBox`。

当前代码已经新增了 `StreamBox::UnreliableDatagram(DatagramBox)` 适配层，让 UDP 这类不可靠 datagram 也能进入现有消息主循环。它解决的是“裸 UDP datagram 不能承载两帧 `Data + payload`”的问题；但如果要让 UDP/fake-TCP/QUIC datagram 这类路径发挥最大价值，core 还需要把能力、调度和冗余策略显式化。

### 3.1 Transport 能力模型

不要把 transport 写死成 TCP 或 UDP，而是声明能力。当前代码用 `StreamBox` variant 表达这个差异；后续如果要做策略引擎，建议把能力提升成显式模型：

```rust
pub enum LinkCapability {
    ReliableOrderedStream,
    ReliableOrderedPacket,
    UnreliableDatagram,
}
```

对应关系：

| 底层 | 能力 | 适合模式 |
|---|---|---|
| TCP / TLS / WebSocket | ReliableOrderedStream | bandwidth、兼容模式 |
| KCP over UDP | ReliableOrderedPacket | bandwidth、兼容低延迟 |
| UDP datagram | UnreliableDatagram | hedge、budget redundancy、mirror、FEC |
| fake-TCP/Phantun 外层 | 通常承载 UDP datagram | 取决于内层协议 |

### 3.2 自包含 datagram frame

当前 `LinkMsg::Data { seq }` 后面跟一个单独 payload frame，这要求底层 link 有序可靠。datagram 模式不能沿用两帧格式，必须把元信息和 payload 放进同一个 datagram：

```text
ReliableData { seq, payload }
Ack { received }
Consumed { seq, consumed }
Ping { nonce }
Pong { nonce }
Goodbye
```

这样 UDP 乱序、重复、丢包时，接收端仍然能独立解析每个 datagram，不会把 payload 误当成协议头。

### 3.3 一个 seq 多个 in-flight copy

低延迟模式的关键不是 transport 名字，而是 aggregate layer 能不能记录“同一个 seq 在多条 link 上有多个副本”。

需要从：

```rust
Sent { link_id, msg }
```

扩展成：

```rust
Sent {
    msg,
    copies: Vec<InFlightCopy>,
}

struct InFlightCopy {
    link_id: LinkId,
    sent_at: Instant,
    kind: CopyKind,
}

enum CopyKind {
    Primary,
    Hedge,
    Mirror,
    FecRepair,
}
```

ACK 到达任意副本即完成该 seq。未确认字节、带宽统计、丢包统计、冗余开销都按 copy 回收和记账。

### 3.4 策略引擎

聚合层需要一个策略引擎决定：

- primary packet 发哪条 link；
- 什么时候 hedge；
- overhead budget 是否还有余额；
- mirror 选几条 link；
- FEC repair 包在哪条 link 上发；
- 哪些 link 因高丢包、高 RTT、排队严重而降权。

这部分应该是 aggligator core 的能力，而不是某个 UDP transport 的私有逻辑。

---

## 4. 推荐模式配置

建议把用户能理解的模式暴露出来，而不是暴露一堆底层协议细节。

```rust
pub enum AggMode {
    Bandwidth,
    LowLatency,
    Mirror { copies: usize },
    BudgetRedundancy { max_overhead: f32 },
    Manual(PolicyConfig),
}
```

建议默认值：

| 模式 | 默认冗余 | 目标 |
|---|---:|---|
| `Bandwidth` | 0% | 跑满多宽带 |
| `LowLatency` | 5% 到 15% | 降低弱网 P99 |
| `BudgetRedundancy { 0.10 }` | 10% | 固定浪费上限换延迟 |
| `Mirror { 2 }` | 100% | 极低延迟/高可用 |
| `Mirror { all }` | N-1 倍 | 极端保命 |

CLI 可以长这样：

```text
--agg-mode bandwidth
--agg-mode low-latency
--agg-mode budget:10%
--agg-mode mirror:2

--transport tcp://host:port
--transport udp://host:port
--transport kcp://host:port
--transport websocket://host:port
```

更细的专家参数：

```text
--link-weight wifi=100
--link-weight cellular=40
--max-redundancy-overhead 10%
--hedge-delay 1.25rtt,min=5ms,max=30ms
--fec-window 10+1
--mirror-copies 2
```

---

## 5. 传输选择建议

### 5.1 短期可跑：KCP transport

如果目标是先得到一个能在丢包网络上工作的版本，可以把 KCP 作为可靠有序 link 接入现有 aggligator。

适合：

- 快速验证 CLI、隧道和多链路管理；
- 单链路 UDP 丢包场景；
- 暂时不改 aggligator core。

限制：

- 聚合层无法直接做 per-packet hedge；
- 难以精确控制“总额外带宽 10%”；
- 多条 KCP link 各自重传，可能导致整体延迟不可控。

### 5.2 中期主线：UDP datagram + aggregate repair

这是最符合目标的路线。

适合：

- 多宽带真正叠加；
- 丢包时跨链路快速补包；
- 统一实现 hedge、mirror、FEC、overhead budget；
- 后续做更好的调度和可观测性。

需要改动：

- core 增加 `UnreliableDatagram` link capability；
- datagram frame 自包含；
- 发送状态支持多副本；
- 策略引擎支持冗余预算。

### 5.3 TCP/WebSocket transport：兼容路径

TCP/WebSocket 仍然有价值：

- 穿透性好；
- 与现有 aggligator contract 完全匹配；
- 适合作为 fallback link；
- 在不丢包或稳定网络上可以提供聚合吞吐。

但在丢包网络下，TCP 内部重传和队头阻塞会放大延迟。低延迟模式应优先使用 datagram-capable transport。

---

## 6. 调度原则

### 6.1 Primary 调度

primary packet 应该按“可用能力”分布，而不是轮询所有 link。

输入信号：

- 用户配置权重；
- 估计带宽；
- RTT / jitter；
- 未确认字节；
- 最近 loss / timeout；
- link 是否 expensive，例如蜂窝流量；
- MTU 和发送队列积压。

手动权重要优先于自动判断。自动探测容易被蜂窝限速、Wi-Fi 抖动、运营商 NAT、系统路由影响，用户明确指定的比例不能被悄悄覆盖。

### 6.2 冗余调度

冗余不是重新分配 primary 带宽，而是额外预算。

```text
total_sent_bytes = primary_bytes + redundant_bytes
overhead = redundant_bytes / primary_bytes
```

策略必须保证：

- `Bandwidth` 模式 overhead 接近 0；
- `LowLatency` 模式 overhead 在配置范围内；
- `Mirror` 模式清楚展示实际倍率；
- 链路越差，越不应该无限制地吃掉 hedge 预算；
- ACK 已完成的 seq 不再发送额外副本。

---

## 7. MTU 与包大小

datagram 模式必须尊重 MTU。

建议：

- v1 不做 UDP 层分片；
- datagram payload 默认保守值约 1180 字节，兼容 IPv6 1280 MTU；
- `io_write_size` 在 datagram mode 下自动降到 payload 上限；
- 大业务写入由 aggligator 切成多个 seq；
- FEC/repair 也以 MTU 内的 seq/datagram 为单位。

不要在 UDP transport 内部分片，因为任意 fragment 丢失都会导致整包不可用，反而放大丢包损失和重组延迟。

---

## 8. 可观测性

如果要调好这些模式，必须能看到每条 link 的状态。

至少记录：

| 指标 | 用途 |
|---|---|
| primary bytes | 真实聚合带宽分布 |
| redundant bytes | 额外带宽开销 |
| overhead percent | 是否超过预算 |
| RTT / jitter | hedge delay 和调度权重 |
| loss / timeout | 降权和 repair 触发 |
| hedge sent / hedge won | hedge 是否真的有用 |
| mirror duplicate dropped | mirror 成本 |
| FEC repaired / failed | FEC 是否值得保留 |
| reorder buffer depth | 是否产生队头阻塞 |

没有这些指标，就无法判断“10% 冗余”到底有没有换到延迟收益。

---

## 9. 阶段化实施计划

### Phase 1：明确模式与配置

1. 引入 `AggMode` / `RedundancyPolicy` 配置；
2. 文档和 CLI 明确默认是 bandwidth aggregation，不是 mirror；
3. 现有 TCP/WebSocket link 继续走 `Bandwidth`；
4. 加统计字段：primary bytes、redundant bytes、overhead。

验收：用户能明确选择 `bandwidth`、`low-latency`、`mirror`，且默认不会复制所有包。

### Phase 2：KCP transport 可选接入

1. 实现 KCP 作为可靠有序 packet link；
2. 保持它是 transport option，不把 core 绑死到 KCP；
3. 记录 KCP link 的 RTT、重传、丢包估计；
4. 用它验证丢包网络下的基础可用性。

验收：丢包网络上能跑通，但文档明确它不是最终的跨链路低延迟方案。

### Phase 3：UnreliableDatagram core

1. core 新增 datagram-capable link；
2. 增加自包含 `LinkDatagram`；
3. 支持乱序、重复、丢包进入现有全局 seq/ACK/reorder/resend；
4. mock datagram link 测试乱序、重复、丢包。

验收：不用真实 UDP，仅用测试 link 就能证明 core 不依赖单 link 有序可靠。

### Phase 4：Hedge / Mirror / Budget Redundancy

1. 发送状态支持一个 seq 多个 in-flight copy；
2. 实现 `LowLatency` 的 hedge；
3. 实现 `Mirror { copies }`；
4. 实现 `BudgetRedundancy { max_overhead }` 的硬预算；
5. 增加 `hedge won`、`overhead`、`duplicate dropped` 指标。

当前进展：1、2、3 已完成；`LowLatency` 的 overhead budget、`hedge sent/won`、`duplicate data dropped` 统计已完成；`BudgetRedundancy` 作为独立模式还未实现。

验收：已有 `datagram_low_latency_hedges_loss` 证明单条 datagram 路径 data 丢失时，`LowLatency` 会跨链路发送 hedge，且 hedge copy 可以赢过原 copy；后续还需要 3% 到 10% 丢包、抖动和限速组合下的 P95/P99 benchmark，并确认长期 overhead 不超过配置。

### Phase 5：UDP transport 与多接口绑定

1. 实现 UDP datagram transport；
2. 支持按接口或 ifindex 绑定发送；
3. 支持多接口 listener；
4. NAT keepalive；
5. 接入 fake-TCP/Phantun 类 wrapper 时不改变 core 策略。

验收：0% 丢包时双链路吞吐接近两条链路之和；丢包时 low-latency 模式降低尾延迟。

### Phase 6：可选 FEC

只有当 hedge/mirror 的数据证明不够时，再做 FEC。

FEC 要求：

- 工作在 aggregate seq 层；
- 小窗口，避免攒包增加延迟；
- 受 `max_overhead` 限制；
- 有明确指标证明比 hedge 更省或更快。

---

## 10. 验证矩阵

| 场景 | 期望 |
|---|---|
| 单 TCP link，0% loss | 兼容现有行为 |
| 双 TCP link，0% loss | 吞吐接近两条链路之和 |
| 单 KCP link，3% loss | 比 TCP 更低尾延迟 |
| 双 KCP link，3% loss | 可用，但可能不如 aggregate hedge |
| 双 UDP datagram，0% loss | 吞吐接近两条链路之和 |
| 双 UDP datagram，3% loss，Bandwidth | 吞吐聚合，尾延迟可能上升 |
| 双 UDP datagram，3% loss，LowLatency | P99 明显低于 Bandwidth，overhead 在预算内 |
| 双 UDP datagram，10%/0% loss 混合 | primary 调度偏向好链路，坏链路不无限消耗 hedge |
| Mirror:2 | 延迟最低之一，带宽约 2 倍 |
| Budget:10% | overhead 不超过 10%，延迟收益可观测 |
| UDP 乱序/重复 | 不协议崩溃，重复 seq 被去重 |
| NAT idle 30s | keepalive 后 link 不失效 |

---

## 11. 最终决策

1. 默认聚合是按链路能力分发不同数据包，不是所有链路同时发同一份数据。
2. mirror 是显式模式，用带宽换最低延迟和最高可用性。
3. 低延迟弱网的主线方案是 aggregate-layer hedge / budget redundancy，用 5% 到 15% 这类可控额外带宽降低 P99。
4. FEC 可以作为后续优化，但必须在 aggregate seq 层做，并受 overhead budget 限制。
5. KCP 可以作为一种 transport mode 支持，尤其适合短期落地和单链路弱网，但它不应该替代聚合层的跨链路修复策略。
6. 修改后的架构如果支持 `UnreliableDatagram`、自包含 datagram frame、多副本 in-flight 状态和策略引擎，就能同时支持 bandwidth、low-latency、budget redundancy、mirror，以及 KCP/TCP/UDP 等不同底层传输。

一句话：**底层传输可以很多种，核心能力必须是聚合层可控的带宽分配与冗余策略。**