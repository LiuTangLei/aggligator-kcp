#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: UDP datagrams.
//!
//! This transport intentionally does not provide per-link reliability or
//! ordering. It exposes UDP packets through aggligator's unreliable datagram
//! link mode so the aggregate connection layer can perform sequencing,
//! acknowledgements, reordering, and retransmission across all links.

use aggligator::io::{DatagramBox, StreamBox};
use aggligator::{
    control::Direction,
    transport::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox},
    Link,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{stream::FuturesUnordered, Sink, SinkExt, Stream, StreamExt};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    any::Any,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
use std::num::NonZeroU32;
use tokio::{
    net::{lookup_host, UdpSocket},
    sync::{mpsc, watch},
    time::{sleep, timeout},
};
use tokio_util::sync::PollSender;

static NAME: &str = "udp";
const UDP_RECV_BUFFER_SIZE: usize = 65_535;
const UDP_SOCKET_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const UDP_SEND_QUEUE_SIZE: usize = 512;
const UDP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const UDP_CLOSED_SESSION_TTL: Duration = Duration::from_secs(60);
const UDP_PROBE_DATAGRAM: &[u8] = b"AGP1";
const UDP_FRAME_MAGIC: &[u8] = b"AGU1";
const UDP_SESSION_HEADER_LEN: usize = 20;
const UDP_LEGACY_DATAGRAM_MAGIC: &[u8] = b"AGD1";

type UdpSessionId = u128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UdpConnectionKey {
    remote: SocketAddr,
    session_id: Option<UdpSessionId>,
}

struct DecodedUdpDatagram {
    session_id: Option<UdpSessionId>,
    payload: Option<Bytes>,
}

fn is_transient_udp_send_error(err: &Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock) || matches!(err.raw_os_error(), Some(55 | 105 | 10055))
}

fn encode_udp_probe(session_id: Option<UdpSessionId>) -> Bytes {
    let Some(session_id) = session_id else {
        return Bytes::from_static(UDP_PROBE_DATAGRAM);
    };

    let mut datagram = Vec::with_capacity(UDP_SESSION_HEADER_LEN);
    datagram.extend_from_slice(UDP_PROBE_DATAGRAM);
    datagram.extend_from_slice(&session_id.to_be_bytes());
    datagram.into()
}

fn encode_udp_frame(session_id: Option<UdpSessionId>, payload: Bytes) -> Bytes {
    let Some(session_id) = session_id else {
        return payload;
    };

    let mut datagram = Vec::with_capacity(UDP_SESSION_HEADER_LEN + payload.len());
    datagram.extend_from_slice(UDP_FRAME_MAGIC);
    datagram.extend_from_slice(&session_id.to_be_bytes());
    datagram.extend_from_slice(&payload);
    datagram.into()
}

fn decode_udp_datagram(datagram: &[u8]) -> Option<DecodedUdpDatagram> {
    if datagram == UDP_PROBE_DATAGRAM {
        return Some(DecodedUdpDatagram { session_id: None, payload: None });
    }

    if datagram.len() == UDP_SESSION_HEADER_LEN && &datagram[..UDP_PROBE_DATAGRAM.len()] == UDP_PROBE_DATAGRAM {
        let session_id = UdpSessionId::from_be_bytes(datagram[UDP_PROBE_DATAGRAM.len()..].try_into().unwrap());
        return Some(DecodedUdpDatagram { session_id: Some(session_id), payload: None });
    }

    if datagram.len() >= UDP_SESSION_HEADER_LEN && &datagram[..UDP_FRAME_MAGIC.len()] == UDP_FRAME_MAGIC {
        let session_id = UdpSessionId::from_be_bytes(
            datagram[UDP_FRAME_MAGIC.len()..UDP_SESSION_HEADER_LEN].try_into().unwrap(),
        );
        return Some(DecodedUdpDatagram {
            session_id: Some(session_id),
            payload: Some(Bytes::copy_from_slice(&datagram[UDP_SESSION_HEADER_LEN..])),
        });
    }

    datagram
        .starts_with(UDP_LEGACY_DATAGRAM_MAGIC)
        .then(|| DecodedUdpDatagram { session_id: None, payload: Some(Bytes::copy_from_slice(datagram)) })
}

/// IP protocol version.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IpVersion {
    /// IP version 4.
    IPv4,
    /// IP version 6.
    IPv6,
    /// Both IP versions.
    #[default]
    Both,
}

impl IpVersion {
    /// Create from "only" arguments.
    pub fn from_only(only_ipv4: bool, only_ipv6: bool) -> Result<Self> {
        match (only_ipv4, only_ipv6) {
            (false, false) => Ok(Self::Both),
            (true, false) => Ok(Self::IPv4),
            (false, true) => Ok(Self::IPv6),
            (true, true) => {
                Err(Error::new(ErrorKind::InvalidInput, "IPv4 and IPv6 options are mutually exclusive"))
            }
        }
    }

    /// Whether only IPv4 should be supported.
    pub fn is_only_ipv4(&self) -> bool {
        matches!(self, Self::IPv4)
    }

    /// Whether only IPv6 should be supported.
    pub fn is_only_ipv6(&self) -> bool {
        matches!(self, Self::IPv6)
    }
}

/// UDP link filter method.
#[derive(Default, Debug, Clone, Copy)]
pub enum UdpLinkFilter {
    /// No link filtering.
    None,
    /// Filter based on local interface and remote interface.
    #[default]
    InterfaceInterface,
    /// Filter based on local interface and remote IP address.
    InterfaceIp,
}

/// Link tag for UDP links.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UdpLinkTag {
    /// Local interface name.
    pub interface: Option<Vec<u8>>,
    /// Remote address.
    pub remote: SocketAddr,
    /// Link direction.
    pub direction: Direction,
}

impl UdpLinkTag {
    /// Creates a new UDP link tag.
    pub fn new(interface: Option<&[u8]>, remote: SocketAddr, direction: Direction) -> Self {
        Self { interface: interface.map(|iface| iface.to_vec()), remote, direction }
    }
}

impl fmt::Display for UdpLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dir = match self.direction {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        };
        write!(
            f,
            "{:16} {dir} {}",
            String::from_utf8_lossy(self.interface.as_deref().unwrap_or_default()),
            self.remote
        )
    }
}

impl LinkTag for UdpLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        self.direction
    }

    fn user_data(&self) -> Vec<u8> {
        self.interface.clone().unwrap_or_default()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }

    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// UDP transport for outgoing connections.
#[derive(Clone)]
pub struct UdpConnector {
    hosts: Vec<String>,
    ip_version: IpVersion,
    resolve_interval: Duration,
    link_filter: UdpLinkFilter,
    multi_interface: bool,
    interface_filter: Arc<dyn Fn(&NetworkInterface) -> bool + Send + Sync>,
}

impl fmt::Debug for UdpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UdpConnector")
            .field("hosts", &self.hosts)
            .field("ip_version", &self.ip_version)
            .field("resolve_interval", &self.resolve_interval)
            .field("link_filter", &self.link_filter)
            .field("multi_interface", &self.multi_interface)
            .finish()
    }
}

impl fmt::Display for UdpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.hosts.len() > 1 {
            write!(f, "[{}]", self.hosts.join(", "))
        } else {
            write!(f, "{}", &self.hosts[0])
        }
    }
}

impl UdpConnector {
    /// Create a new UDP transport for outgoing connections.
    pub async fn new(hosts: impl IntoIterator<Item = String>, default_port: u16) -> Result<Self> {
        let this = Self::unresolved(hosts, default_port).await?;

        let addrs = this.resolve().await;
        if addrs.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, "cannot resolve IP address of host"));
        }
        tracing::info!(%this, ?addrs, "UDP hosts initially resolved");

        Ok(this)
    }

    /// Create a new UDP transport without checking initial name resolution.
    pub async fn unresolved(hosts: impl IntoIterator<Item = String>, default_port: u16) -> Result<Self> {
        let mut hosts: Vec<_> = hosts.into_iter().collect();

        if hosts.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one host is required"));
        }

        for host in &mut hosts {
            if !host.contains(':') {
                host.push_str(&format!(":{default_port}"));
            }
        }

        Ok(Self {
            hosts,
            ip_version: IpVersion::Both,
            resolve_interval: Duration::from_secs(10),
            link_filter: UdpLinkFilter::default(),
            multi_interface: !cfg!(target_os = "android"),
            interface_filter: Arc::new(|_| true),
        })
    }

    /// Sets the IP version used for connecting.
    pub fn set_ip_version(&mut self, ip_version: IpVersion) {
        self.ip_version = ip_version;
    }

    /// Sets the interval for re-resolving the hostname and checking network interfaces.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Sets the link filter method.
    pub fn set_link_filter(&mut self, link_filter: UdpLinkFilter) {
        self.link_filter = link_filter;
    }

    /// Sets whether all available local interfaces should be used for connecting.
    pub fn set_multi_interface(&mut self, multi_interface: bool) {
        self.multi_interface = multi_interface;
    }

    /// Sets the local interface filter.
    pub fn set_interface_filter(
        &mut self, interface_filter: impl Fn(&NetworkInterface) -> bool + Send + Sync + 'static,
    ) {
        self.interface_filter = Arc::new(interface_filter);
    }

    async fn resolve(&self) -> Vec<SocketAddr> {
        resolve_hosts(&self.hosts, self.ip_version).await
    }
}

#[async_trait]
impl ConnectingTransport for UdpConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let interfaces: Option<Vec<NetworkInterface>> = match self.multi_interface {
                true => {
                    Some(local_interfaces()?.into_iter().filter(|iface| (self.interface_filter)(iface)).collect())
                }
                false => None,
            };

            let mut tags: HashSet<LinkTagBox> = HashSet::new();
            for addr in self.resolve().await {
                match &interfaces {
                    Some(interfaces) => {
                        for iface in interface_names_for_target(interfaces, addr) {
                            tags.insert(Box::new(UdpLinkTag::new(Some(&iface), addr, Direction::Outgoing)));
                        }
                    }
                    None => {
                        tags.insert(Box::new(UdpLinkTag::new(None, addr, Direction::Outgoing)));
                    }
                }
            }

            tx.send_if_modified(|v| {
                if *v != tags {
                    *v = tags;
                    true
                } else {
                    false
                }
            });

            sleep(self.resolve_interval).await;
        }
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &UdpLinkTag = tag.as_any().downcast_ref().unwrap();
        let bind_addr = bind_addr_for(tag.interface.as_deref(), tag.remote)?;
        let socket = create_udp_socket(bind_addr, tag.interface.as_deref())?;
        socket.connect(tag.remote).await?;
        Ok(datagram_box_for_connected_socket(Arc::new(socket)).into())
    }

    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool {
        let Some(new_tag) = new.tag().as_any().downcast_ref::<UdpLinkTag>() else { return true };

        match existing.iter().find(|link| {
            let Some(tag) = link.tag().as_any().downcast_ref::<UdpLinkTag>() else { return false };
            match self.link_filter {
                UdpLinkFilter::None => false,
                UdpLinkFilter::InterfaceInterface => {
                    tag.interface == new_tag.interface && link.remote_user_data() == new.remote_user_data()
                }
                UdpLinkFilter::InterfaceIp => {
                    tag.interface == new_tag.interface && tag.remote.ip() == new_tag.remote.ip()
                }
            }
        }) {
            Some(other) => {
                let other_tag = other.tag().as_any().downcast_ref::<UdpLinkTag>().unwrap();
                tracing::debug!(new = %new_tag.remote, existing = %other_tag.remote, "UDP link is redundant, rejecting");
                false
            }
            None => true,
        }
    }
}

/// UDP transport for incoming connections.
#[derive(Debug, Clone)]
pub struct UdpAcceptor {
    binds: Vec<UdpBind>,
}

#[derive(Debug, Clone)]
struct UdpBind {
    addr: SocketAddr,
    interface: Option<Vec<u8>>,
}

impl fmt::Display for UdpAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let addrs: Vec<_> = self.binds.iter().map(|bind| bind.addr.to_string()).collect();
        if addrs.len() > 1 {
            write!(f, "[{}]", addrs.join(", "))
        } else {
            write!(f, "{}", addrs[0])
        }
    }
}

impl UdpAcceptor {
    /// Create a new UDP transport listening on one address.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self { binds: vec![UdpBind { addr: bind_addr, interface: None }] }
    }

    /// Create a UDP transport listening individually on all local interfaces.
    pub fn all_interfaces(port: u16) -> Result<Self> {
        let mut binds = Vec::new();
        for iface in local_interfaces()? {
            for addr in iface.addr {
                let ip = addr.ip();
                if ip.is_unspecified() {
                    continue;
                }
                binds.push(UdpBind {
                    addr: SocketAddr::new(ip, port),
                    interface: Some(iface.name.as_bytes().to_vec()),
                });
            }
        }

        if binds.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, "no local interface address found"));
        }

        Ok(Self { binds })
    }
}

#[async_trait]
impl AcceptingTransport for UdpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        let mut tasks = FuturesUnordered::new();
        for bind in self.binds.clone() {
            tasks.push(listen_one(bind, tx.clone()));
        }
        drop(tx);

        while let Some(res) = tasks.next().await {
            res?;
        }
        Ok(())
    }
}

async fn listen_one(bind: UdpBind, accepted_tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
    let socket = Arc::new(create_udp_socket(bind.addr, bind.interface.as_deref())?);
    tracing::info!(addr = %bind.addr, "UDP listener started");

    let mut connections: HashMap<UdpConnectionKey, (u64, mpsc::UnboundedSender<Result<Bytes>>)> = HashMap::new();
    let mut closed_sessions: HashMap<UdpConnectionKey, Instant> = HashMap::new();
    let mut next_connection_generation = 0u64;
    let (cleanup_tx, mut cleanup_rx) = mpsc::unbounded_channel();
    let mut buf = vec![0u8; UDP_RECV_BUFFER_SIZE];

    loop {
        tokio::select! {
            recv = socket.recv_from(&mut buf) => {
                let (len, remote) = recv?;
                let Some(decoded) = decode_udp_datagram(&buf[..len]) else { continue };
                let key = UdpConnectionKey { remote, session_id: decoded.session_id };
                let now = Instant::now();
                closed_sessions.retain(|_, closed_at| now.duration_since(*closed_at) < UDP_CLOSED_SESSION_TTL);
                if closed_sessions.contains_key(&key) {
                    continue;
                }

                let incoming_tx = if let Some((_, incoming_tx)) = connections.get(&key) {
                    incoming_tx.clone()
                } else {
                    let generation = next_connection_generation;
                    next_connection_generation = next_connection_generation.wrapping_add(1);

                    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
                    let (outgoing_tx, outgoing_rx) = mpsc::channel(UDP_SEND_QUEUE_SIZE);
                    spawn_unconnected_writer(socket.clone(), key, outgoing_rx, incoming_tx.clone());

                    let cleanup_tx = cleanup_tx.clone();
                    let incoming_closed_tx = incoming_tx.clone();
                    tokio::spawn(async move {
                        incoming_closed_tx.closed().await;
                        let _ = cleanup_tx.send((key, generation));
                    });

                    let tag = UdpLinkTag::new(bind.interface.as_deref(), remote, Direction::Incoming);
                    let stream = DatagramBox::new(DatagramSink::new(outgoing_tx), DatagramReceiver::new(incoming_rx)).into();
                    accepted_tx
                        .send(AcceptedStreamBox::new(stream, tag))
                        .await
                        .map_err(|_| Error::new(ErrorKind::BrokenPipe, "acceptor was closed"))?;

                    connections.insert(key, (generation, incoming_tx.clone()));
                    incoming_tx
                };

                if let Some(datagram) = decoded.payload {
                    if incoming_tx.send(Ok(datagram)).is_err() {
                        connections.remove(&key);
                        closed_sessions.insert(key, Instant::now());
                    }
                }
            }
            Some((key, generation)) = cleanup_rx.recv() => {
                if matches!(connections.get(&key), Some((current, _)) if *current == generation) {
                    connections.remove(&key);
                    closed_sessions.insert(key, Instant::now());
                }
            }
        }
    }
}

fn datagram_box_for_connected_socket(socket: Arc<UdpSocket>) -> DatagramBox {
    let session_id = rand::random();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let (outgoing_tx, outgoing_rx) = mpsc::channel(UDP_SEND_QUEUE_SIZE);

    spawn_connected_reader(socket.clone(), session_id, incoming_tx.clone());
    spawn_connected_writer(socket, session_id, outgoing_rx, incoming_tx);

    DatagramBox::new(DatagramSink::new(outgoing_tx), DatagramReceiver::new(incoming_rx))
}

fn spawn_connected_reader(
    socket: Arc<UdpSocket>, session_id: UdpSessionId, incoming_tx: mpsc::UnboundedSender<Result<Bytes>>,
) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; UDP_RECV_BUFFER_SIZE];
        loop {
            match socket.recv(&mut buf).await {
                Ok(len) => {
                    let Some(decoded) = decode_udp_datagram(&buf[..len]) else { continue };
                    if decoded.session_id != Some(session_id) {
                        continue;
                    }
                    let Some(payload) = decoded.payload else { continue };
                    if incoming_tx.send(Ok(payload)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = incoming_tx.send(Err(err));
                    break;
                }
            }
        }
    });
}

fn spawn_connected_writer(
    socket: Arc<UdpSocket>, session_id: UdpSessionId, mut outgoing_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::UnboundedSender<Result<Bytes>>,
) {
    tokio::spawn(async move {
        let probe = encode_udp_probe(Some(session_id));
        if let Err(err) = socket.send(&probe).await {
            if is_transient_udp_send_error(&err) {
                tracing::debug!(%err, "dropping UDP initial probe after transient send error");
            } else {
                tracing::warn!(%err, "UDP initial probe send failed");
                let _ = incoming_tx.send(Err(err));
                return;
            }
        }

        loop {
            match timeout(UDP_KEEPALIVE_INTERVAL, outgoing_rx.recv()).await {
                Ok(Some(datagram)) => {
                    let datagram = encode_udp_frame(Some(session_id), datagram);
                    if let Err(err) = socket.send(&datagram).await {
                        if is_transient_udp_send_error(&err) {
                            tracing::debug!(%err, "dropping UDP datagram after transient send error");
                            continue;
                        }
                        tracing::warn!(%err, "UDP datagram send failed");
                        let _ = incoming_tx.send(Err(err));
                        break;
                    }
                }
                Ok(None) => {
                    tracing::debug!("UDP writer channel closed");
                    break;
                }
                Err(_) => {
                    if let Err(err) = socket.send(&probe).await {
                        if is_transient_udp_send_error(&err) {
                            tracing::debug!(%err, "dropping UDP keepalive probe after transient send error");
                            continue;
                        }
                        tracing::warn!(%err, "UDP keepalive probe send failed");
                        let _ = incoming_tx.send(Err(err));
                        break;
                    }
                }
            }
        }
    });
}

fn spawn_unconnected_writer(
    socket: Arc<UdpSocket>, key: UdpConnectionKey, mut outgoing_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::UnboundedSender<Result<Bytes>>,
) {
    tokio::spawn(async move {
        let remote = key.remote;
        let probe = encode_udp_probe(key.session_id);
        loop {
            match timeout(UDP_KEEPALIVE_INTERVAL, outgoing_rx.recv()).await {
                Ok(Some(datagram)) => {
                    let datagram = encode_udp_frame(key.session_id, datagram);
                    if let Err(err) = socket.send_to(&datagram, remote).await {
                        if is_transient_udp_send_error(&err) {
                            tracing::debug!(%err, %remote, "dropping UDP datagram after transient send_to error");
                            continue;
                        }
                        tracing::warn!(%err, %remote, "UDP datagram send_to failed");
                        let _ = incoming_tx.send(Err(err));
                        break;
                    }
                }
                Ok(None) => {
                    tracing::debug!(%remote, "UDP writer channel closed");
                    break;
                }
                Err(_) => {
                    if let Err(err) = socket.send_to(&probe, remote).await {
                        if is_transient_udp_send_error(&err) {
                            tracing::debug!(%err, %remote, "dropping UDP keepalive probe after transient send_to error");
                            continue;
                        }
                        tracing::warn!(%err, %remote, "UDP keepalive probe send_to failed");
                        let _ = incoming_tx.send(Err(err));
                        break;
                    }
                }
            }
        }
    });
}

struct DatagramSink {
    tx: PollSender<Bytes>,
}

impl DatagramSink {
    fn new(tx: mpsc::Sender<Bytes>) -> Self {
        Self { tx: PollSender::new(tx) }
    }
}

impl Sink<Bytes> for DatagramSink {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.tx.poll_ready_unpin(cx).map_err(|_| Error::new(ErrorKind::BrokenPipe, "UDP writer task ended"))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<()> {
        self.tx.start_send_unpin(item).map_err(|_| Error::new(ErrorKind::BrokenPipe, "UDP writer task ended"))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.tx.poll_flush_unpin(cx).map_err(|_| Error::new(ErrorKind::BrokenPipe, "UDP writer task ended"))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.tx.poll_close_unpin(cx).map_err(|_| Error::new(ErrorKind::BrokenPipe, "UDP writer task ended"))
    }
}

struct DatagramReceiver {
    rx: mpsc::UnboundedReceiver<Result<Bytes>>,
}

impl DatagramReceiver {
    fn new(rx: mpsc::UnboundedReceiver<Result<Bytes>>) -> Self {
        Self { rx }
    }
}

unsafe impl Send for DatagramReceiver {}
unsafe impl Sync for DatagramReceiver {}

impl Stream for DatagramReceiver {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn create_udp_socket(bind_addr: SocketAddr, interface: Option<&[u8]>) -> Result<UdpSocket> {
    let domain = if bind_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    socket.set_recv_buffer_size(UDP_SOCKET_BUFFER_SIZE)?;
    socket.set_send_buffer_size(UDP_SOCKET_BUFFER_SIZE)?;

    if let Some(interface) = interface {
        bind_socket_to_interface(&socket, interface, bind_addr.ip())?;
    }

    socket.bind(&SockAddr::from(bind_addr))?;
    let socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(socket)
}

fn bind_socket_to_interface(socket: &Socket, interface: &[u8], local_ip: IpAddr) -> Result<()> {
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    {
        let _ = local_ip;
        socket.bind_device(Some(interface))?;
    }

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    {
        let index = interface_index(interface)?;
        if local_ip.is_ipv4() {
            socket.bind_device_by_index_v4(Some(index))?;
        } else {
            socket.bind_device_by_index_v6(Some(index))?;
        }
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    )))]
    {
        let _ = socket;
        let _ = interface;
        let _ = local_ip;
    }

    Ok(())
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn interface_index(interface: &[u8]) -> Result<NonZeroU32> {
    local_interfaces()?
        .into_iter()
        .find(|iface| iface.name.as_bytes() == interface)
        .and_then(|iface| NonZeroU32::new(iface.index))
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "network interface index not found"))
}

fn bind_addr_for(interface: Option<&[u8]>, remote: SocketAddr) -> Result<SocketAddr> {
    if let Some(interface) = interface {
        let local_ip = interface_local_addr(interface, remote.ip()).ok_or_else(|| {
            Error::new(
                ErrorKind::AddrNotAvailable,
                format!("no suitable address on interface {}", String::from_utf8_lossy(interface)),
            )
        })?;
        Ok(SocketAddr::new(local_ip, 0))
    } else if remote.is_ipv4() {
        Ok("0.0.0.0:0".parse().unwrap())
    } else {
        Ok("[::]:0".parse().unwrap())
    }
}

fn interface_local_addr(interface: &[u8], target_ip: IpAddr) -> Option<IpAddr> {
    local_interfaces().ok()?.into_iter().find_map(|iface| {
        if iface.name.as_bytes() != interface {
            return None;
        }
        iface.addr.iter().find_map(|addr| {
            let ip = addr.ip();
            (!ip.is_unspecified() && ip.is_ipv4() == target_ip.is_ipv4()).then_some(ip)
        })
    })
}

fn local_interfaces() -> Result<Vec<NetworkInterface>> {
    Ok(NetworkInterface::show()
        .map_err(|err| Error::other(err.to_string()))?
        .into_iter()
        .filter(|iface| !iface.name.starts_with("ifb"))
        .collect())
}

fn interface_names_for_target(interfaces: &[NetworkInterface], target: SocketAddr) -> HashSet<Vec<u8>> {
    interfaces
        .iter()
        .filter(|iface| {
            iface.addr.iter().any(|addr| {
                !addr.ip().is_unspecified()
                    && addr.ip().is_loopback() == target.ip().is_loopback()
                    && addr.ip().is_ipv4() == target.is_ipv4()
                    && addr.ip().is_ipv6() == target.is_ipv6()
            })
        })
        .map(|iface| iface.name.as_bytes().to_vec())
        .collect()
}

async fn resolve_hosts(hosts: &[String], ip_version: IpVersion) -> Vec<SocketAddr> {
    let mut all_addrs = HashSet::new();

    for host in hosts {
        match lookup_host(host).await {
            Ok(addrs) => {
                all_addrs.extend(addrs.filter(|addr| {
                    !((addr.is_ipv4() && ip_version.is_only_ipv6())
                        || (addr.is_ipv6() && ip_version.is_only_ipv4()))
                }));
            }
            Err(err) => {
                tracing::warn!(host = %host, %err, "UDP host resolution failed");
            }
        }
    }

    let mut all_addrs: Vec<_> = all_addrs.into_iter().collect();
    all_addrs.sort();
    all_addrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_session_probe_roundtrips() {
        let session_id = 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00;
        let datagram = encode_udp_probe(Some(session_id));
        let decoded = decode_udp_datagram(&datagram).unwrap();

        assert_eq!(decoded.session_id, Some(session_id));
        assert!(decoded.payload.is_none());
    }

    #[test]
    fn udp_session_frame_roundtrips() {
        let session_id = 0xfedc_ba98_7654_3210_0123_4567_89ab_cdef;
        let payload = Bytes::from_static(b"AGD1 payload");
        let datagram = encode_udp_frame(Some(session_id), payload.clone());
        let decoded = decode_udp_datagram(&datagram).unwrap();

        assert_eq!(decoded.session_id, Some(session_id));
        assert_eq!(decoded.payload, Some(payload));
    }

    #[test]
    fn udp_legacy_probe_and_frame_decode() {
        let decoded_probe = decode_udp_datagram(UDP_PROBE_DATAGRAM).unwrap();
        assert_eq!(decoded_probe.session_id, None);
        assert!(decoded_probe.payload.is_none());

        let legacy_payload = Bytes::from_static(b"AGD1 legacy");
        let decoded_frame = decode_udp_datagram(&legacy_payload).unwrap();
        assert_eq!(decoded_frame.session_id, None);
        assert_eq!(decoded_frame.payload, Some(legacy_payload));
    }

    #[test]
    fn udp_decoder_ignores_unknown_datagrams() {
        assert!(decode_udp_datagram(b"not aggligator").is_none());
    }
}
