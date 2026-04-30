# Command-Line Help for `agg-tunnel`

This document contains the help content for the `agg-tunnel` command-line program.

**Command Overview:**

* [`agg-tunnel`↴](#agg-tunnel)
* [`agg-tunnel client`↴](#agg-tunnel-client)
* [`agg-tunnel server`↴](#agg-tunnel-server)
* [`agg-tunnel udp-relay`↴](#agg-tunnel-udp-relay)
* [`agg-tunnel show-cfg`↴](#agg-tunnel-show-cfg)

## `agg-tunnel`

Forward TCP ports through a connection of aggregated links.

This uses Aggligator to combine multiple TCP links into one connection, providing the combined speed and resilience to individual link faults.

**Usage:** `agg-tunnel [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `client` — Tunnel client
* `server` — Tunnel server
* `udp-relay` — Relay UDP datagrams to another UDP address
* `show-cfg` — Shows the default configuration

###### **Options:**

* `--cfg <CFG>` — Configuration file
* `-d`, `--dump <DUMP>` — Dump analysis data to file

## `agg-tunnel client`

Tunnel client

**Usage:** `agg-tunnel client [OPTIONS] --port <PORT>`

###### **Options:**

* `-4`, `--ipv4` — Use IPv4
* `-6`, `--ipv6` — Use IPv6
* `-n`, `--no-monitor` — Do not display the link monitor
* `-a`, `--all-links` — Display all possible (including disconnected) links in the link monitor
* `-p`, `--port <PORT>` — Ports to forward from server to client.

   Takes the form `server_port:client_port` and can be specified multiple times.

   The port must have been enabled on the server.
* `-g`, `--global` — Forward ports on all local interfaces.

   If unspecified only loopback connections are accepted.
* `--once` — Exit after handling one connection
* `--tcp <TCP>` — TCP server name or IP addresses and port number
* `--udp <UDP>` — UDP server address (host:port or IP:port)
* `--udp-payload-size <UDP_PAYLOAD_SIZE>` — Maximum aggligator data payload size when UDP links are enabled.

   The default is conservative for IPv6 minimum MTU and avoids IP fragmentation.

  Default value: `1180`
* `--agg-mode <AGG_MODE>` — Aggregate mode: bandwidth, bandwidth-redundant, or low-latency.

   Default value: `bandwidth`
* `--tcp-link-filter <TCP_LINK_FILTER>` — TCP link filter.

   none: no link filtering.

   interface-interface: one link for each pair of local and remote interface.

   interface-ip: one link for each pair of local interface and remote IP address.

  Default value: `interface-interface`
* `--udp-link-filter <UDP_LINK_FILTER>` — UDP link filter.

   none: no link filtering.

   interface-interface: one link for each pair of local and remote interface.

   interface-ip: one link for each pair of local interface and remote IP address.

  Default value: `interface-interface`
* `--udp-single-interface` — Use the system route for UDP instead of creating one UDP link per local interface



## `agg-tunnel server`

Tunnel server

**Usage:** `agg-tunnel server [OPTIONS] --port <PORT>`

###### **Options:**

* `-n`, `--no-monitor` — Do not display the link monitor
* `-p`, `--port <PORT>` — Ports to forward to clients.

   Takes the form `port` or `target:port` and can be specified multiple times.

   Target can be a host name or IP address. If unspecified localhost is used as target.
* `--tcp <TCP>` — TCP port to listen on
* `--udp <UDP>` — UDP address(es) to listen on. Can be specified multiple times.

   Takes the form `port` or `address:port`. If only a port is specified, listens on all local interface addresses.
* `--udp-payload-size <UDP_PAYLOAD_SIZE>` — Maximum aggligator data payload size when UDP links are enabled.

   The default is conservative for IPv6 minimum MTU and avoids IP fragmentation.

  Default value: `1180`
* `--agg-mode <AGG_MODE>` — Aggregate mode: bandwidth, bandwidth-redundant, or low-latency.

   Default value: `bandwidth`



## `agg-tunnel udp-relay`

Relay UDP datagrams to another UDP address

**Usage:** `agg-tunnel udp-relay [OPTIONS] --listen <LISTEN> --target <TARGET>`

###### **Options:**

* `--listen <LISTEN>` — UDP address to listen on
* `--target <TARGET>` — UDP address to forward datagrams to
* `--client-timeout-secs <CLIENT_TIMEOUT_SECS>` — Close idle client mappings after this many seconds

   Default value: `120`



## `agg-tunnel show-cfg`

Shows the default configuration

**Usage:** `agg-tunnel show-cfg`



<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>

