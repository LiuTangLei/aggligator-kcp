# Aggligator transport: KCP

[Aggligator](https://crates.io/crates/aggligator) transport using KCP (Fast and Reliable ARQ Protocol) over UDP.

KCP provides better performance than TCP in high packet loss and high latency network environments.

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
aggligator-transport-kcp = "0.1.0"
```

### Client (Connector)

```rust
use aggligator_transport_kcp::KcpConnector;
use kcp_tokio::KcpConfig;

let kcp_config = KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None);
let connector = KcpConnector::new(vec!["server:5801".into()], 5801, kcp_config).await?;
```

### Server (Acceptor)

```rust
use aggligator_transport_kcp::KcpAcceptor;
use kcp_tokio::KcpConfig;
use std::net::SocketAddr;

let kcp_config = KcpConfig::new().fast_mode().stream_mode(true).keep_alive(None);
let acceptor = KcpAcceptor::new("[::]:5801".parse().unwrap(), kcp_config);
```

## License

Licensed under the Apache License, Version 2.0.
