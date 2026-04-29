//! Phantun-compatible fake-TCP transport for KCP (cross-platform).
//!
//! Disguises KCP UDP traffic as TCP traffic using a userspace fake-TCP stack
//! on top of a TUN interface, allowing packets to traverse firewalls and NAT
//! devices that block UDP. The underlying datagram semantics are fully
//! preserved — no TCP retransmission or flow control is introduced.
//!
//! Uses [`tun2`] for cross-platform TUN support (Linux, macOS, Windows,
//! FreeBSD, Android, iOS) and a built-in minimal fake-TCP protocol
//! implementation compatible with the [Phantun](https://github.com/dndx/phantun)
//! wire format.
//!
//! # Architecture
//!
//! Two transport types are provided:
//!
//! * [`PhantunClientTransport`] — wraps a single outgoing fake-TCP connection
//!   to the server. Used by `KcpStream::connect_with_transport`.
//! * [`PhantunServerTransport`] — accepts multiple incoming fake-TCP
//!   connections and multiplexes them into the connectionless
//!   [`Transport`](kcp_tokio::Transport) interface. Used by
//!   `KcpListener::with_transport`.
//!
//! # Requirements
//!
//! * **Root / admin privileges** required to create TUN interfaces.
//!   - Linux: root or `CAP_NET_ADMIN`.
//!   - macOS: root.
//!   - Windows: administrator + [Wintun](https://wintun.net/) driver DLL.
//! * **Routing / NAT rules** to direct traffic through the TUN interface.
//!   - Linux: `iptables -t nat` or `nftables`.
//!   - macOS: `pfctl`.
//!   - Windows: `netsh` or route commands.
//! * **IP forwarding** must be enabled on the host.

use bytes::{Bytes, BytesMut};
use flume;
use internet_checksum::Checksum;
use kcp_tokio::Transport;
use pnet::packet::{ip, ipv4, ipv6, tcp, Packet};
use rand::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU8, AtomicU32, Ordering},
        Arc, RwLock as StdRwLock,
    },
};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio::time;

// =========================================================================
// Constants
// =========================================================================

const TIMEOUT: time::Duration = time::Duration::from_secs(1);
const RETRIES: usize = 6;
const MPMC_BUFFER_LEN: usize = 512;
const MPSC_BUFFER_LEN: usize = 128;
const MAX_UNACKED_LEN: u32 = 128 * 1024 * 1024;

const IPV4_HEADER_LEN: usize = 20;
const IPV6_HEADER_LEN: usize = 40;
const TCP_HEADER_LEN: usize = 20;
const MAX_PACKET_LEN: usize = 1500;

// =========================================================================
// Configuration
// =========================================================================

/// Configuration for the Phantun TUN interface.
///
/// Both client and server require a point-to-point TUN device.  The default
/// addresses follow the Phantun convention:
///
/// | Role   | `tun_local` (OS side) | `tun_peer` (Phantun side) |
/// |--------|-----------------------|---------------------------|
/// | Client | 192.168.200.1         | 192.168.200.2             |
/// | Server | 192.168.201.1         | 192.168.201.2             |
#[derive(Debug, Clone)]
pub struct PhantunConfig {
    /// TUN interface name.  Empty string = auto-assigned by kernel.
    pub tun_name: String,
    /// IPv4 address assigned to the TUN interface (OS side).
    pub tun_local: Ipv4Addr,
    /// IPv4 point-to-point destination address (Phantun side).
    pub tun_peer: Ipv4Addr,
    /// Optional IPv6 local address for the TUN interface.
    pub tun_local6: Option<Ipv6Addr>,
    /// Optional IPv6 peer address for the TUN interface.
    pub tun_peer6: Option<Ipv6Addr>,
}

impl PhantunConfig {
    /// Configuration with the standard Phantun **client** addresses.
    pub fn client_default() -> Self {
        Self {
            tun_name: String::new(),
            tun_local: Ipv4Addr::new(192, 168, 200, 1),
            tun_peer: Ipv4Addr::new(192, 168, 200, 2),
            tun_local6: None,
            tun_peer6: None,
        }
    }

    /// Configuration with the standard Phantun **server** addresses.
    pub fn server_default() -> Self {
        Self {
            tun_name: String::new(),
            tun_local: Ipv4Addr::new(192, 168, 201, 1),
            tun_peer: Ipv4Addr::new(192, 168, 201, 2),
            tun_local6: None,
            tun_peer6: None,
        }
    }
}

// =========================================================================
// Packet construction / parsing  (Phantun wire-compatible)
// =========================================================================

fn build_tcp_packet(
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: Option<&[u8]>,
) -> Bytes {
    let ip_header_len = match local_addr {
        SocketAddr::V4(_) => IPV4_HEADER_LEN,
        SocketAddr::V6(_) => IPV6_HEADER_LEN,
    };
    let wscale = (flags & tcp::TcpFlags::SYN) != 0;
    let tcp_header_len = TCP_HEADER_LEN + if wscale { 4 } else { 0 };
    let tcp_total_len = tcp_header_len + payload.map_or(0, |p| p.len());
    let total_len = ip_header_len + tcp_total_len;
    let mut buf = BytesMut::zeroed(total_len);

    let mut ip_buf = buf.split_to(ip_header_len);
    let mut tcp_buf = buf.split_to(tcp_total_len);

    match (local_addr, remote_addr) {
        (SocketAddr::V4(local), SocketAddr::V4(remote)) => {
            let mut v4 = ipv4::MutableIpv4Packet::new(&mut ip_buf).unwrap();
            v4.set_version(4);
            v4.set_header_length(IPV4_HEADER_LEN as u8 / 4);
            v4.set_next_level_protocol(ip::IpNextHeaderProtocols::Tcp);
            v4.set_ttl(64);
            v4.set_source(*local.ip());
            v4.set_destination(*remote.ip());
            v4.set_total_length(total_len as u16);
            v4.set_flags(ipv4::Ipv4Flags::DontFragment);
            let mut cksm = Checksum::new();
            cksm.add_bytes(v4.packet());
            v4.set_checksum(u16::from_be_bytes(cksm.checksum()));
        }
        (SocketAddr::V6(local), SocketAddr::V6(remote)) => {
            let mut v6 = ipv6::MutableIpv6Packet::new(&mut ip_buf).unwrap();
            v6.set_version(6);
            v6.set_payload_length(tcp_total_len as u16);
            v6.set_next_header(ip::IpNextHeaderProtocols::Tcp);
            v6.set_hop_limit(64);
            v6.set_source(*local.ip());
            v6.set_destination(*remote.ip());
        }
        _ => unreachable!(),
    };

    let mut t = tcp::MutableTcpPacket::new(&mut tcp_buf).unwrap();
    t.set_window(0xffff);
    t.set_source(local_addr.port());
    t.set_destination(remote_addr.port());
    t.set_sequence(seq);
    t.set_acknowledgement(ack);
    t.set_flags(flags);
    t.set_data_offset(TCP_HEADER_LEN as u8 / 4 + if wscale { 1 } else { 0 });
    if wscale {
        let ws = tcp::TcpOption::wscale(14);
        t.set_options(&[tcp::TcpOption::nop(), ws]);
    }
    if let Some(payload) = payload {
        t.set_payload(payload);
    }

    // TCP checksum with pseudo-header.
    let mut cksm = Checksum::new();
    let ip::IpNextHeaderProtocol(tcp_proto) = ip::IpNextHeaderProtocols::Tcp;
    match (local_addr, remote_addr) {
        (SocketAddr::V4(local), SocketAddr::V4(remote)) => {
            cksm.add_bytes(&local.ip().octets());
            cksm.add_bytes(&remote.ip().octets());
            let mut pseudo = [0u8, tcp_proto, 0, 0];
            pseudo[2..].copy_from_slice(&(tcp_total_len as u16).to_be_bytes());
            cksm.add_bytes(&pseudo);
        }
        (SocketAddr::V6(local), SocketAddr::V6(remote)) => {
            cksm.add_bytes(&local.ip().octets());
            cksm.add_bytes(&remote.ip().octets());
            let mut pseudo = [0u8, 0, 0, 0, 0, 0, 0, tcp_proto];
            pseudo[0..4].copy_from_slice(&(tcp_total_len as u32).to_be_bytes());
            cksm.add_bytes(&pseudo);
        }
        _ => unreachable!(),
    };
    cksm.add_bytes(t.packet());
    t.set_checksum(u16::from_be_bytes(cksm.checksum()));

    ip_buf.unsplit(tcp_buf);
    ip_buf.freeze()
}

enum IpPkt<'p> {
    V4(ipv4::Ipv4Packet<'p>),
    V6(ipv6::Ipv6Packet<'p>),
}

impl IpPkt<'_> {
    fn src(&self) -> IpAddr {
        match self {
            IpPkt::V4(p) => IpAddr::V4(p.get_source()),
            IpPkt::V6(p) => IpAddr::V6(p.get_source()),
        }
    }
    fn dst(&self) -> IpAddr {
        match self {
            IpPkt::V4(p) => IpAddr::V4(p.get_destination()),
            IpPkt::V6(p) => IpAddr::V6(p.get_destination()),
        }
    }
}

fn parse_ip_packet(buf: &[u8]) -> Option<(IpPkt<'_>, tcp::TcpPacket<'_>)> {
    if buf.is_empty() {
        return None;
    }
    match buf[0] >> 4 {
        4 => {
            let v4 = ipv4::Ipv4Packet::new(buf)?;
            if v4.get_next_level_protocol() != ip::IpNextHeaderProtocols::Tcp {
                return None;
            }
            // Use actual IHL field instead of hardcoded 20 to handle
            // packets with IP options correctly.
            let ihl = (v4.get_header_length() as usize) * 4;
            if ihl < IPV4_HEADER_LEN || buf.len() < ihl {
                return None;
            }
            let tcp = tcp::TcpPacket::new(&buf[ihl..])?;
            Some((IpPkt::V4(v4), tcp))
        }
        6 => {
            let v6 = ipv6::Ipv6Packet::new(buf)?;
            if v6.get_next_header() != ip::IpNextHeaderProtocols::Tcp {
                return None;
            }
            let tcp = tcp::TcpPacket::new(&buf[IPV6_HEADER_LEN..])?;
            Some((IpPkt::V6(v6), tcp))
        }
        _ => None,
    }
}

// =========================================================================
// Fake-TCP Socket
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AddrTuple {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

enum FtcpState {
    Idle,
    SynSent,
    SynReceived,
    Established,
}

struct Shared {
    tuples: StdRwLock<HashMap<AddrTuple, flume::Sender<Bytes>>>,
    listening: StdRwLock<HashSet<u16>>,
    tun_writer: Mutex<tokio::io::WriteHalf<tun2::AsyncDevice>>,
    ready: mpsc::Sender<FtcpSocket>,
    tuples_purge: broadcast::Sender<AddrTuple>,
}

struct FtcpSocket {
    shared: Arc<Shared>,
    incoming: flume::Receiver<Bytes>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    seq: AtomicU32,
    ack: AtomicU32,
    last_ack: AtomicU32,
    state: FtcpState,
}

impl FtcpSocket {
    fn new(
        shared: Arc<Shared>,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ack: Option<u32>,
        state: FtcpState,
    ) -> (Self, flume::Sender<Bytes>) {
        let (tx, rx) = flume::bounded(MPMC_BUFFER_LEN);
        (
            Self {
                shared,
                incoming: rx,
                local_addr,
                remote_addr,
                seq: AtomicU32::new(0),
                ack: AtomicU32::new(ack.unwrap_or(0)),
                last_ack: AtomicU32::new(ack.unwrap_or(0)),
                state,
            },
            tx,
        )
    }

    fn build_pkt(&self, flags: u8, payload: Option<&[u8]>) -> Bytes {
        let ack = self.ack.load(Ordering::Relaxed);
        self.last_ack.store(ack, Ordering::Relaxed);
        build_tcp_packet(
            self.local_addr,
            self.remote_addr,
            self.seq.load(Ordering::Relaxed),
            ack,
            flags,
            payload,
        )
    }

    async fn tun_send_pkt(&self, data: &[u8]) -> Option<()> {
        use tokio::io::AsyncWriteExt;
        let mut writer = self.shared.tun_writer.lock().await;
        writer.write_all(data).await.ok().map(|_| ())
    }

    /// Send a datagram.
    pub async fn send(&self, payload: &[u8]) -> Option<()> {
        match self.state {
            FtcpState::Established => {
                // Atomically reserve seq space first, then build packet
                // with the reserved seq value to avoid races between
                // concurrent senders.
                let seq = self.seq.fetch_add(payload.len() as u32, Ordering::Relaxed);
                let ack = self.ack.load(Ordering::Relaxed);
                self.last_ack.store(ack, Ordering::Relaxed);
                let buf = build_tcp_packet(
                    self.local_addr,
                    self.remote_addr,
                    seq,
                    ack,
                    tcp::TcpFlags::ACK,
                    Some(payload),
                );
                self.tun_send_pkt(&buf).await
            }
            _ => None,
        }
    }

    /// Receive a datagram.
    ///
    /// Bare ACK packets (no payload) are consumed internally and not
    /// returned to the caller — only data-carrying packets yield a result.
    pub async fn recv(&self, buf: &mut [u8]) -> Option<usize> {
        match self.state {
            FtcpState::Established => loop {
                let raw_buf = self.incoming.recv_async().await.ok()?;
                let (_ip, tcp_pkt) = parse_ip_packet(&raw_buf)?;

                if (tcp_pkt.get_flags() & tcp::TcpFlags::RST) != 0 {
                    tracing::info!(
                        remote = %self.remote_addr,
                        "Fake TCP connection reset by peer"
                    );
                    return None;
                }

                let payload = tcp_pkt.payload();
                let new_ack = tcp_pkt
                    .get_sequence()
                    .wrapping_add(payload.len() as u32);
                let last = self.last_ack.load(Ordering::Relaxed);
                self.ack.store(new_ack, Ordering::Relaxed);

                if new_ack.overflowing_sub(last).0 > MAX_UNACKED_LEN {
                    let ack_pkt = self.build_pkt(tcp::TcpFlags::ACK, None);
                    let _ = self.tun_send_pkt(&ack_pkt).await;
                }

                // Skip bare ACKs (no payload) — only return data packets.
                if payload.is_empty() {
                    continue;
                }

                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                return Some(n);
            },
            _ => None,
        }
    }

    async fn do_accept(mut self) {
        for _ in 0..RETRIES {
            match self.state {
                FtcpState::Idle => {
                    let pkt =
                        self.build_pkt(tcp::TcpFlags::SYN | tcp::TcpFlags::ACK, None);
                    let _ = self.tun_send_pkt(&pkt).await;
                    self.state = FtcpState::SynReceived;
                    tracing::debug!("Sent SYN+ACK to client");
                }
                FtcpState::SynReceived => {
                    let res =
                        time::timeout(TIMEOUT, self.incoming.recv_async()).await;
                    if let Ok(Ok(raw)) = res {
                        if let Some((_ip, tcp_pkt)) = parse_ip_packet(&raw) {
                            if (tcp_pkt.get_flags() & tcp::TcpFlags::RST) != 0 {
                                return;
                            }
                            if tcp_pkt.get_flags() == tcp::TcpFlags::ACK
                                && tcp_pkt.get_acknowledgement()
                                    == self.seq.load(Ordering::Relaxed) + 1
                            {
                                self.seq.fetch_add(1, Ordering::Relaxed);
                                self.state = FtcpState::Established;
                                tracing::info!(
                                    remote = %self.remote_addr,
                                    "Fake TCP connection accepted"
                                );
                                let ready = self.shared.ready.clone();
                                let _ = ready.send(self).await;
                                return;
                            }
                        }
                    } else {
                        tracing::debug!("Waiting for client ACK timed out");
                        self.state = FtcpState::Idle;
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    async fn do_connect(&mut self) -> Option<()> {
        for _ in 0..RETRIES {
            match self.state {
                FtcpState::Idle => {
                    let pkt = self.build_pkt(tcp::TcpFlags::SYN, None);
                    let _ = self.tun_send_pkt(&pkt).await;
                    self.state = FtcpState::SynSent;
                    tracing::debug!("Sent SYN to server");
                }
                FtcpState::SynSent => {
                    match time::timeout(TIMEOUT, self.incoming.recv_async()).await {
                        Ok(Ok(raw)) => {
                            if let Some((_ip, tcp_pkt)) = parse_ip_packet(&raw) {
                                if (tcp_pkt.get_flags() & tcp::TcpFlags::RST) != 0 {
                                    return None;
                                }
                                if tcp_pkt.get_flags()
                                    == tcp::TcpFlags::SYN | tcp::TcpFlags::ACK
                                    && tcp_pkt.get_acknowledgement()
                                        == self.seq.load(Ordering::Relaxed) + 1
                                {
                                    self.seq.fetch_add(1, Ordering::Relaxed);
                                    self.ack.store(
                                        tcp_pkt.get_sequence() + 1,
                                        Ordering::Relaxed,
                                    );
                                    let ack_pkt =
                                        self.build_pkt(tcp::TcpFlags::ACK, None);
                                    let _ =
                                        self.tun_send_pkt(&ack_pkt).await;
                                    self.state = FtcpState::Established;
                                    tracing::info!(
                                        remote = %self.remote_addr,
                                        "Fake TCP connection established"
                                    );
                                    return Some(());
                                }
                            }
                        }
                        _ => {
                            tracing::debug!("Waiting for SYN+ACK timed out");
                            self.state = FtcpState::Idle;
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        None
    }
}

impl Drop for FtcpSocket {
    fn drop(&mut self) {
        let tuple = AddrTuple {
            local_addr: self.local_addr,
            remote_addr: self.remote_addr,
        };
        if let Ok(mut tuples) = self.shared.tuples.write() {
            tuples.remove(&tuple);
        }
        let _ = self.shared.tuples_purge.send(tuple);

        let rst = build_tcp_packet(
            self.local_addr,
            self.remote_addr,
            self.seq.load(Ordering::Relaxed),
            0,
            tcp::TcpFlags::RST,
            None,
        );
        let shared = self.shared.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut writer = shared.tun_writer.lock().await;
            let _ = writer.write_all(&rst).await;
        });
    }
}

// =========================================================================
// Fake-TCP Stack
// =========================================================================

struct FtcpStack {
    shared: Arc<Shared>,
    local_ip: Ipv4Addr,
    local_ip6: Option<Ipv6Addr>,
    ready: mpsc::Receiver<FtcpSocket>,
}

impl FtcpStack {
    fn new(
        tun: tun2::AsyncDevice,
        local_ip: Ipv4Addr,
        local_ip6: Option<Ipv6Addr>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel(MPSC_BUFFER_LEN);
        let (purge_tx, _purge_rx) = broadcast::channel(16);

        let (tun_reader, tun_writer) = tokio::io::split(tun);

        let shared = Arc::new(Shared {
            tuples: StdRwLock::new(HashMap::new()),
            listening: StdRwLock::new(HashSet::new()),
            tun_writer: Mutex::new(tun_writer),
            ready: ready_tx,
            tuples_purge: purge_tx.clone(),
        });

        // Spawn the reader task.
        tokio::spawn(Self::reader_task(
            tun_reader,
            shared.clone(),
            purge_tx.subscribe(),
        ));

        Self {
            shared,
            local_ip,
            local_ip6,
            ready: ready_rx,
        }
    }

    fn listen(&mut self, port: u16) {
        self.shared
            .listening
            .write()
            .unwrap()
            .insert(port);
    }

    async fn accept(&mut self) -> FtcpSocket {
        self.ready.recv().await.unwrap()
    }

    async fn connect(&mut self, addr: SocketAddr) -> Option<FtcpSocket> {
        let mut rng = SmallRng::try_from_rng(&mut rand::rngs::SysRng).expect("OS RNG failure");
        for local_port in rng.random_range(32768..=60999)..=60999 {
            let local_addr = SocketAddr::new(
                if addr.is_ipv4() {
                    IpAddr::V4(self.local_ip)
                } else {
                    IpAddr::V6(
                        self.local_ip6
                            .expect("IPv6 local address undefined"),
                    )
                },
                local_port,
            );
            let tuple = AddrTuple {
                local_addr,
                remote_addr: addr,
            };

            let mut sock;
            {
                let mut tuples = self.shared.tuples.write().unwrap();
                if tuples.contains_key(&tuple) {
                    continue;
                }
                let incoming;
                (sock, incoming) = FtcpSocket::new(
                    self.shared.clone(),
                    local_addr,
                    addr,
                    None,
                    FtcpState::Idle,
                );
                tuples.insert(tuple, incoming);
            }

            return sock.do_connect().await.map(|_| sock);
        }

        tracing::error!(
            remote = %addr,
            "Fake TCP connect failed: ephemeral ports exhausted"
        );
        None
    }

    async fn reader_task(
        mut tun_reader: tokio::io::ReadHalf<tun2::AsyncDevice>,
        shared: Arc<Shared>,
        mut tuples_purge: broadcast::Receiver<AddrTuple>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut local_cache: HashMap<AddrTuple, flume::Sender<Bytes>> =
            HashMap::new();

        loop {
            let mut buf = BytesMut::zeroed(MAX_PACKET_LEN);

            tokio::select! {
                result = tun_reader.read(&mut buf) => {
                    let size = match result {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!("TUN read error: {e}");
                            continue;
                        }
                    };
                    buf.truncate(size);
                    let buf = buf.freeze();

                    // Parse the packet to extract addresses and flags.
                    // We extract the needed info and drop borrows before
                    // moving `buf` into the channel.
                    let parsed = parse_ip_packet(&buf).map(|(ip_pkt, tcp_pkt)| {
                        let local_addr = SocketAddr::new(
                            ip_pkt.dst(),
                            tcp_pkt.get_destination(),
                        );
                        let remote_addr = SocketAddr::new(
                            ip_pkt.src(),
                            tcp_pkt.get_source(),
                        );
                        let flags = tcp_pkt.get_flags();
                        let seq = tcp_pkt.get_sequence();
                        let ack_num = tcp_pkt.get_acknowledgement();
                        let payload_len = tcp_pkt.payload().len();
                        (local_addr, remote_addr, flags, seq, ack_num, payload_len)
                    });

                    let Some((local_addr, remote_addr, flags, seq, ack_num, payload_len)) = parsed else {
                        continue;
                    };

                    let tuple = AddrTuple { local_addr, remote_addr };

                    // Fast path: check local cache.
                    if let Some(c) = local_cache.get(&tuple) {
                        if c.send_async(buf.clone()).await.is_ok() {
                            continue;
                        }
                        // Receiver closed, fall through.
                    }

                    // Slow path: check shared tuples.
                    let sender = {
                        let tuples = shared.tuples.read().unwrap();
                        tuples.get(&tuple).cloned()
                    };
                    if let Some(c) = sender {
                        local_cache.insert(tuple, c.clone());
                        let _ = c.send_async(buf).await;
                        continue;
                    }

                    // SYN on a listening port?
                    if flags == tcp::TcpFlags::SYN
                        && shared
                            .listening
                            .read()
                            .unwrap()
                            .contains(&local_addr.port())
                    {
                        if seq == 0 {
                            let (sock, incoming) = FtcpSocket::new(
                                shared.clone(),
                                local_addr,
                                remote_addr,
                                Some(seq + 1),
                                FtcpState::Idle,
                            );
                            shared
                                .tuples
                                .write()
                                .unwrap()
                                .insert(tuple, incoming);
                            tokio::spawn(sock.do_accept());
                        } else {
                            let rst = build_tcp_packet(
                                local_addr,
                                remote_addr,
                                0,
                                seq + payload_len as u32 + 1,
                                tcp::TcpFlags::RST | tcp::TcpFlags::ACK,
                                None,
                            );
                            let mut writer = shared.tun_writer.lock().await;
                            let _ = writer.write_all(&rst).await;
                        }
                    } else if (flags & tcp::TcpFlags::RST) == 0 {
                        let rst = build_tcp_packet(
                            local_addr,
                            remote_addr,
                            ack_num,
                            seq + payload_len as u32,
                            tcp::TcpFlags::RST | tcp::TcpFlags::ACK,
                            None,
                        );
                        let mut writer = shared.tun_writer.lock().await;
                        let _ = writer.write_all(&rst).await;
                    }
                }
                tuple = tuples_purge.recv() => {
                    if let Ok(tuple) = tuple {
                        local_cache.remove(&tuple);
                    }
                }
            }
        }
    }
}

// =========================================================================
// TUN device creation (cross-platform)
// =========================================================================

fn create_tun(config: &PhantunConfig) -> io::Result<tun2::AsyncDevice> {
    let mut tun_config = tun2::Configuration::default();
    tun_config
        .address(config.tun_local)
        .netmask((255, 255, 255, 252))
        .destination(config.tun_peer)
        .up();

    if !config.tun_name.is_empty() {
        tun_config.tun_name(&config.tun_name);
    }

    #[cfg(target_os = "linux")]
    tun_config.platform_config(|p| {
        p.ensure_root_privileges(true);
    });

    tun2::create_as_async(&tun_config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

// =========================================================================
// Client transport
// =========================================================================

/// Shared fake-TCP stack for the Phantun client.
///
/// Multiple [`PhantunClientTransport`] instances share a single TUN device
/// through this handle, avoiding TUN/routing conflicts when aggregating
/// multiple KCP links.
///
/// Create one via [`PhantunClientStack::new`] and pass it to
/// [`PhantunClientTransport::connect`].
pub struct PhantunClientStack {
    stack: Mutex<FtcpStack>,
    local_addr: SocketAddr,
}

impl fmt::Debug for PhantunClientStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhantunClientStack")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl PhantunClientStack {
    /// Create a shared Phantun client stack.
    ///
    /// This allocates one TUN device.  All subsequent
    /// [`PhantunClientTransport::connect`] calls using this stack reuse
    /// the same device, so only one set of routing / NAT rules is needed.
    pub fn new(config: &PhantunConfig) -> io::Result<Arc<Self>> {
        let tun = create_tun(config)?;
        let local_ip6 = config.tun_peer6;
        let stack = FtcpStack::new(tun, config.tun_peer, local_ip6);
        let local_addr = SocketAddr::new(config.tun_peer.into(), 0);
        Ok(Arc::new(Self {
            stack: Mutex::new(stack),
            local_addr,
        }))
    }
}

/// Manages multiple TUN devices for per-interface Phantun client connections.
///
/// When aggregating traffic across multiple network interfaces (e.g. 3 broadband
/// connections), each interface needs its own TUN device with a unique IP pair.
/// This enables policy-based routing to direct fake-TCP packets through different
/// physical interfaces.
///
/// TUN devices are created lazily on first use for each interface.
///
/// # Routing setup
///
/// After the TUN devices are created, configure policy routing so that
/// packets from each TUN source IP go through the correct physical interface.
/// The TUN peer IP for each interface is logged at `INFO` level.
///
/// Example (Linux):
///
/// ```bash
/// # Prevent kernel RST for all Phantun source IPs:
/// iptables -A OUTPUT -p tcp --tcp-flags RST RST -s 192.168.200.0/24 -j DROP
///
/// # TUN 0 (source 192.168.200.2) → eth0:
/// ip rule add from 192.168.200.2 table 100
/// ip route add default via <eth0_gateway> dev eth0 table 100
/// iptables -t nat -A POSTROUTING -o eth0 -s 192.168.200.0/30 -j MASQUERADE
///
/// # TUN 1 (source 192.168.200.6) → wlan0:
/// ip rule add from 192.168.200.6 table 101
/// ip route add default via <wlan0_gateway> dev wlan0 table 101
/// iptables -t nat -A POSTROUTING -o wlan0 -s 192.168.200.4/30 -j MASQUERADE
///
/// # TUN 2 (source 192.168.200.10) → 4G:
/// ip rule add from 192.168.200.10 table 102
/// ip route add default via <4g_gateway> dev 4g0 table 102
/// iptables -t nat -A POSTROUTING -o 4g0 -s 192.168.200.8/30 -j MASQUERADE
/// ```
pub struct PhantunMultiStack {
    base_config: PhantunConfig,
    stacks: Mutex<HashMap<Option<Vec<u8>>, Arc<PhantunClientStack>>>,
    next_index: AtomicU8,
}

impl PhantunMultiStack {
    /// Create a new multi-stack with the given base configuration.
    ///
    /// No TUN devices are created until [`get_or_create`](Self::get_or_create)
    /// is called.  The first interface uses the base configuration as-is;
    /// subsequent interfaces get auto-incremented TUN IP pairs (offset by 4,
    /// i.e. one `/30` subnet per TUN device).
    pub fn new(base_config: PhantunConfig) -> Self {
        Self {
            base_config,
            stacks: Mutex::new(HashMap::new()),
            next_index: AtomicU8::new(0),
        }
    }

    /// Get or create a [`PhantunClientStack`] for the given interface.
    ///
    /// If a stack already exists for this interface, it is returned.
    /// Otherwise a new TUN device is created with auto-assigned IP
    /// addresses and the mapping is cached.
    pub async fn get_or_create(
        &self,
        interface: &Option<Vec<u8>>,
    ) -> io::Result<Arc<PhantunClientStack>> {
        let mut stacks = self.stacks.lock().await;
        if let Some(stack) = stacks.get(interface) {
            return Ok(stack.clone());
        }

        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        let config = self.config_for_index(index);

        tracing::info!(
            interface = %String::from_utf8_lossy(interface.as_deref().unwrap_or(b"default")),
            tun_local = %config.tun_local,
            tun_peer = %config.tun_peer,
            index,
            "Phantun: created TUN device (route traffic from source {} through this interface)",
            config.tun_peer,
        );

        let stack = PhantunClientStack::new(&config)?;
        stacks.insert(interface.clone(), stack.clone());
        Ok(stack)
    }

    /// Generate a [`PhantunConfig`] for the given index.
    ///
    /// Index 0 reuses the base config.  Each subsequent index offsets the
    /// TUN local and peer IPv4 addresses by `index * 4` (one `/30` subnet
    /// per TUN device).
    fn config_for_index(&self, index: u8) -> PhantunConfig {
        if index == 0 {
            return self.base_config.clone();
        }

        let offset = index as u32 * 4;
        let local_u32 = u32::from_be_bytes(self.base_config.tun_local.octets())
            .wrapping_add(offset);
        let peer_u32 = u32::from_be_bytes(self.base_config.tun_peer.octets())
            .wrapping_add(offset);

        let tun_name = if self.base_config.tun_name.is_empty() {
            String::new()
        } else {
            format!("{}{}", self.base_config.tun_name, index)
        };

        PhantunConfig {
            tun_name,
            tun_local: Ipv4Addr::from(local_u32),
            tun_peer: Ipv4Addr::from(peer_u32),
            tun_local6: self.base_config.tun_local6.map(|ip6| {
                let mut octets = ip6.octets();
                let last = u16::from_be_bytes([octets[14], octets[15]]);
                let new_last = last.wrapping_add(offset as u16);
                let bytes = new_last.to_be_bytes();
                octets[14] = bytes[0];
                octets[15] = bytes[1];
                Ipv6Addr::from(octets)
            }),
            tun_peer6: self.base_config.tun_peer6.map(|ip6| {
                let mut octets = ip6.octets();
                let last = u16::from_be_bytes([octets[14], octets[15]]);
                let new_last = last.wrapping_add(offset as u16);
                let bytes = new_last.to_be_bytes();
                octets[14] = bytes[0];
                octets[15] = bytes[1];
                Ipv6Addr::from(octets)
            }),
        }
    }
}

/// Phantun client transport for KCP.
///
/// Wraps a single fake-TCP connection to the Phantun server.
/// Created via [`PhantunClientTransport::connect`].
pub struct PhantunClientTransport {
    socket: Arc<FtcpSocket>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl fmt::Debug for PhantunClientTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhantunClientTransport")
            .field("local_addr", &self.local_addr)
            .field("remote_addr", &self.remote_addr)
            .finish()
    }
}

impl PhantunClientTransport {
    /// Establish a fake-TCP connection to `remote_addr` using a shared stack.
    ///
    /// The [`PhantunClientStack`] manages the TUN device and fake-TCP
    /// protocol, allowing multiple connections to share a single TUN.
    pub async fn connect(
        client_stack: &PhantunClientStack,
        remote_addr: SocketAddr,
    ) -> io::Result<Self> {
        let socket = {
            let mut stack = client_stack.stack.lock().await;
            stack.connect(remote_addr).await.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("phantun fake-TCP connect to {remote_addr} failed"),
                )
            })?
        };

        tracing::info!(%remote_addr, "Phantun: fake-TCP connection established");

        Ok(Self {
            socket: Arc::new(socket),
            local_addr: client_stack.local_addr,
            remote_addr,
        })
    }
}

impl Transport for PhantunClientTransport {
    type Addr = SocketAddr;

    async fn send_to(&self, buf: &[u8], _addr: &SocketAddr) -> io::Result<usize> {
        self.socket
            .send(buf)
            .await
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "phantun send failed")
            })?;
        Ok(buf.len())
    }

    async fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        let size = self
            .socket
            .recv(buf)
            .await
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "phantun recv failed")
            })?;
        Ok((size, self.remote_addr))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

// =========================================================================
// Server transport
// =========================================================================

/// Phantun server transport for KCP.
///
/// Listens for incoming fake-TCP connections, accepts them in a background
/// task, and multiplexes all per-connection sockets into the connectionless
/// [`Transport`] interface that KCP expects.
///
/// Created via [`PhantunServerTransport::listen`].
pub struct PhantunServerTransport {
    local_addr: SocketAddr,
    sockets: Arc<RwLock<HashMap<SocketAddr, Arc<FtcpSocket>>>>,
    rx: Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl fmt::Debug for PhantunServerTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhantunServerTransport")
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

impl PhantunServerTransport {
    /// Start listening for fake-TCP connections on `port`.
    ///
    /// A background task is spawned that accepts new connections and relays
    /// their datagrams through the transport.
    pub async fn listen(
        config: &PhantunConfig,
        port: u16,
    ) -> io::Result<Self> {
        let tun = create_tun(config)?;
        let local_ip6 = config.tun_local6;

        let mut stack = FtcpStack::new(tun, config.tun_local, local_ip6);
        stack.listen(port);

        tracing::info!(port, "Phantun: fake-TCP listener started");

        let local_addr = SocketAddr::new(config.tun_peer.into(), port);

        let sockets: Arc<RwLock<HashMap<SocketAddr, Arc<FtcpSocket>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let (tx, rx) = mpsc::channel(4096);

        let sockets_accept = sockets.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let socket = stack.accept().await;
                let remote_addr = socket.remote_addr;
                tracing::debug!(
                    %remote_addr,
                    "Phantun: accepted fake-TCP connection"
                );

                let socket = Arc::new(socket);
                sockets_accept
                    .write()
                    .await
                    .insert(remote_addr, socket.clone());

                let tx = tx.clone();
                let sockets_reader = sockets_accept.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match socket.recv(&mut buf).await {
                            Some(size) if size > 0 => {
                                if tx
                                    .send((buf[..size].to_vec(), remote_addr))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            _ => {
                                tracing::debug!(
                                    %remote_addr,
                                    "Phantun: connection closed"
                                );
                                sockets_reader
                                    .write()
                                    .await
                                    .remove(&remote_addr);
                                break;
                            }
                        }
                    }
                });
            }
        });

        Ok(Self {
            local_addr,
            sockets,
            rx: Mutex::new(rx),
            accept_task,
        })
    }
}

impl Drop for PhantunServerTransport {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl Transport for PhantunServerTransport {
    type Addr = SocketAddr;

    async fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        // Clone the Arc to release the read-lock before the async send,
        // so that connection accept/remove is not blocked during I/O.
        let socket = {
            let sockets = self.sockets.read().await;
            sockets.get(addr).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("no phantun connection to {addr}"),
                )
            })?
        };
        socket
            .send(buf)
            .await
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "phantun send failed")
            })?;
        Ok(buf.len())
    }

    async fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        let mut rx = self.rx.lock().await;
        let (data, addr) = rx.recv().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "phantun accept loop closed",
            )
        })?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok((n, addr))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
