#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: KCP
//!
//! KCP (Fast and Reliable ARQ Protocol) transport over UDP.
//! Provides better performance than TCP in high packet loss and high latency
//! network environments.
//!
//! ## Optional FEC
//!
//! Enable the `fec` feature for Reed-Solomon forward error correction at the
//! UDP packet level. This reduces retransmission latency on lossy networks.

#[cfg(feature = "fec")]
#[cfg_attr(docsrs, doc(cfg(feature = "fec")))]
pub mod fec;

#[cfg(feature = "phantun")]
#[cfg_attr(docsrs, doc(cfg(feature = "phantun")))]
pub mod phantun;

pub mod util;

use aggligator::io::{IoBox, StreamBox};
use async_trait::async_trait;
pub use kcp_tokio::KcpConfig;
use kcp_tokio::{KcpListener, KcpStream, UdpTransport};
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{split, AsyncRead, AsyncWrite, ReadBuf},
    net::lookup_host,
    sync::{mpsc, watch},
    time::sleep,
};

use aggligator::{
    control::Direction,
    transport::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox},
    Link,
};

static NAME: &str = "kcp";

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

/// KCP link filter method.
///
/// Controls which links are established between the local and remote endpoint.
#[derive(Default, Debug, Clone, Copy)]
pub enum KcpLinkFilter {
    /// No link filtering.
    None,
    /// Filter based on local interface and remote interface.
    ///
    /// One link for each pair of local interface and remote interface is established.
    #[default]
    InterfaceInterface,
    /// Filter based on local interface and remote IP address.
    ///
    /// One link for each pair of local interface and remote IP address is established.
    InterfaceIp,
}

/// Wrapper that provides a `Sync` impl for `Send`-only I/O halves.
///
/// `KcpStream` is `Send` but not `Sync` (it contains `Pin<Box<dyn Future + Send>>`).
/// `tokio::io::split` produces `ReadHalf`/`WriteHalf` that inherit this limitation.
/// `IoBox::new` requires `Send + Sync`, so this wrapper bridges the gap.
///
/// # Safety
///
/// The `Sync` impl is sound because `SyncIo` wraps a split half that is only
/// ever accessed through `&mut self` (via `poll_read`/`poll_write`).
struct SyncIo<T> {
    inner: T,
}

impl<T> SyncIo<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

unsafe impl<T: Send> Send for SyncIo<T> {}
unsafe impl<T: Send> Sync for SyncIo<T> {}

impl<T> AsyncRead for SyncIo<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for SyncIo<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Link tag for KCP link.
///
/// Identifies a KCP link by its local interface, remote address and direction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KcpLinkTag {
    /// Local interface name.
    pub interface: Option<Vec<u8>>,
    /// Remote address (UDP endpoint).
    pub remote: SocketAddr,
    /// Link direction.
    pub direction: Direction,
}

impl fmt::Display for KcpLinkTag {
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

impl LinkTag for KcpLinkTag {
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

/// KCP transport for outgoing connections.
///
/// This transport is IO-stream based, using KCP over UDP.
#[derive(Clone)]
pub struct KcpConnector {
    hosts: Vec<String>,
    kcp_config: KcpConfig,
    ip_version: IpVersion,
    resolve_interval: Duration,
    link_filter: KcpLinkFilter,
    multi_interface: bool,
    interface_filter: Arc<dyn Fn(&util::NetworkInterface) -> bool + Send + Sync>,
    #[cfg(feature = "fec")]
    fec_config: Option<fec::FecConfig>,
    #[cfg(feature = "phantun")]
    phantun_multi_stack: Option<Arc<phantun::PhantunMultiStack>>,
}

impl fmt::Debug for KcpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("KcpConnector")
            .field("hosts", &self.hosts)
            .field("ip_version", &self.ip_version)
            .field("resolve_interval", &self.resolve_interval)
            .field("link_filter", &self.link_filter)
            .field("multi_interface", &self.multi_interface)
            .finish()
    }
}

impl fmt::Display for KcpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.hosts.len() > 1 {
            write!(f, "[{}]", self.hosts.join(", "))
        } else {
            write!(f, "{}", &self.hosts[0])
        }
    }
}

impl KcpConnector {
    /// Create a new KCP transport for outgoing connections.
    ///
    /// `hosts` can contain IP addresses and hostnames, including port numbers.
    /// If an entry does not specify a port number, the `default_port` is used.
    ///
    /// It is checked at creation that `hosts` resolves to at least one IP address.
    ///
    /// Host name resolution is retried periodically, thus DNS updates will be taken
    /// into account without the need to recreate this transport.
    pub async fn new(
        hosts: impl IntoIterator<Item = String>, default_port: u16, kcp_config: KcpConfig,
    ) -> Result<Self> {
        let mut hosts: Vec<_> = hosts.into_iter().collect();

        if hosts.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one host is required"));
        }

        for host in &mut hosts {
            if !host.contains(':') {
                host.push_str(&format!(":{default_port}"));
            }
        }

        let this = Self {
            hosts,
            kcp_config,
            ip_version: IpVersion::Both,
            resolve_interval: Duration::from_secs(10),
            link_filter: KcpLinkFilter::default(),
            multi_interface: !cfg!(target_os = "android"),
            interface_filter: Arc::new(|_| true),
            #[cfg(feature = "fec")]
            fec_config: None,
            #[cfg(feature = "phantun")]
            phantun_multi_stack: None,
        };

        let addrs = this.resolve().await;
        if addrs.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, "cannot resolve IP address of host"));
        }
        tracing::info!(%this, ?addrs, "KCP hosts initially resolved");

        Ok(this)
    }

    /// Sets the KCP configuration.
    pub fn set_kcp_config(&mut self, kcp_config: KcpConfig) {
        self.kcp_config = kcp_config;
    }

    /// Sets the IP version used for connecting.
    pub fn set_ip_version(&mut self, ip_version: IpVersion) {
        self.ip_version = ip_version;
    }

    /// Sets the interval for re-resolving the hostname and checking for changed network interfaces.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Sets the link filter method.
    pub fn set_link_filter(&mut self, link_filter: KcpLinkFilter) {
        self.link_filter = link_filter;
    }

    /// Sets whether all available local interfaces should be used for connecting.
    ///
    /// If this is true (default for non-Android platforms), a separate link is
    /// established for each pair of server IP and local interface. Each outgoing
    /// UDP socket is explicitly bound to a local interface.
    ///
    /// If this is false (default for Android platform), one link is established for
    /// each server IP.  The operating system automatically assigns a local interface.
    pub fn set_multi_interface(&mut self, multi_interface: bool) {
        self.multi_interface = multi_interface;
    }

    /// Sets the local interface filter.
    ///
    /// It is only used when multi interface is enabled.
    ///
    /// The provided function is called for each discovered local interface and should
    /// return whether the interface should be used for establishing links.
    ///
    /// By default all local interfaces are used.
    pub fn set_interface_filter(
        &mut self, interface_filter: impl Fn(&util::NetworkInterface) -> bool + Send + Sync + 'static,
    ) {
        self.interface_filter = Arc::new(interface_filter);
    }

    /// Enables FEC (Forward Error Correction) with the given configuration.
    ///
    /// When enabled, Reed-Solomon erasure coding is applied at the UDP packet
    /// level, allowing lost packets to be recovered without KCP retransmission.
    #[cfg(feature = "fec")]
    #[cfg_attr(docsrs, doc(cfg(feature = "fec")))]
    pub fn set_fec(&mut self, fec_config: fec::FecConfig) {
        self.fec_config = Some(fec_config);
    }

    /// Enables Phantun fake-TCP transport.
    ///
    /// When enabled, KCP traffic is disguised as TCP using TUN-based fake
    /// TCP stacks, allowing it to traverse firewalls that block UDP.
    ///
    /// When `multi_interface` is enabled (default), a separate TUN device is
    /// created for each network interface, allowing fake-TCP traffic to be
    /// routed through different physical interfaces for bandwidth aggregation.
    /// Each TUN gets a unique IP pair (offset by 4, i.e. `/30` subnets).
    /// See [`phantun::PhantunMultiStack`] for the required routing setup.
    ///
    /// TUN devices are created lazily when each interface first connects.
    /// Requires root/admin privileges to create TUN interfaces.
    #[cfg(feature = "phantun")]
    #[cfg_attr(docsrs, doc(cfg(feature = "phantun")))]
    pub fn set_phantun(&mut self, phantun_config: phantun::PhantunConfig) -> Result<()> {
        self.phantun_multi_stack = Some(Arc::new(phantun::PhantunMultiStack::new(phantun_config)));
        Ok(())
    }

    /// Resolve target to socket addresses.
    async fn resolve(&self) -> Vec<SocketAddr> {
        resolve_hosts(&self.hosts, self.ip_version).await
    }
}

#[async_trait]
impl ConnectingTransport for KcpConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let interfaces: Option<Vec<util::NetworkInterface>> = match self.multi_interface {
                true => Some(
                    util::local_interfaces()?
                        .into_iter()
                        .filter(|iface| (self.interface_filter)(iface))
                        .collect(),
                ),
                false => None,
            };

            let mut tags: HashSet<LinkTagBox> = HashSet::new();
            for addr in self.resolve().await {
                match &interfaces {
                    Some(interfaces) => {
                        for iface in util::interface_names_for_target(interfaces, addr) {
                            let tag = KcpLinkTag {
                                interface: Some(iface),
                                remote: addr,
                                direction: Direction::Outgoing,
                            };
                            tags.insert(Box::new(tag));
                        }
                    }
                    None => {
                        let tag = KcpLinkTag {
                            interface: None,
                            remote: addr,
                            direction: Direction::Outgoing,
                        };
                        tags.insert(Box::new(tag));
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
        let tag: &KcpLinkTag = tag.as_any().downcast_ref().unwrap();

        // Determine local bind address based on interface.
        let bind_addr: SocketAddr = if let Some(interface) = &tag.interface {
            let local_ip = util::interface_local_addr(interface, tag.remote.ip()).ok_or_else(|| {
                Error::new(
                    ErrorKind::AddrNotAvailable,
                    format!(
                        "no suitable address on interface {}",
                        String::from_utf8_lossy(interface)
                    ),
                )
            })?;
            SocketAddr::new(local_ip, 0)
        } else {
            match tag.remote {
                SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
                SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
            }
        };

        // Phantun (fake TCP) transport — optionally with FEC on top.
        #[cfg(feature = "phantun")]
        if let Some(phantun_multi_stack) = &self.phantun_multi_stack {
            let phantun_stack = phantun_multi_stack
                .get_or_create(&tag.interface)
                .await
                .map_err(|e| Error::other(format!(
                    "failed to create Phantun TUN for interface {:?}: {e}",
                    tag.interface.as_deref().map(String::from_utf8_lossy)
                )))?;
            let phantun_transport =
                phantun::PhantunClientTransport::connect(&phantun_stack, tag.remote)
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;

            #[cfg(feature = "fec")]
            if let Some(fec_cfg) = &self.fec_config {
                let fec_transport = fec::FecTransport::new(phantun_transport, fec_cfg);
                let stream = KcpStream::connect_with_transport(
                    Arc::new(fec_transport),
                    tag.remote,
                    self.kcp_config.clone(),
                )
                .await
                .map_err(|e| Error::other(e.to_string()))?;
                let (rh, wh) = split(stream);
                return Ok(IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into());
            }

            let stream = KcpStream::connect_with_transport(
                Arc::new(phantun_transport),
                tag.remote,
                self.kcp_config.clone(),
            )
            .await
            .map_err(|e| Error::other(e.to_string()))?;
            let (rh, wh) = split(stream);
            return Ok(IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into());
        }

        // FEC over plain UDP transport.
        #[cfg(feature = "fec")]
        if let Some(fec_cfg) = &self.fec_config {
            let udp = UdpTransport::bind(bind_addr)
                .await
                .map_err(|e| Error::other(e.to_string()))?;
            let fec_transport = fec::FecTransport::new(udp, fec_cfg);
            let stream =
                KcpStream::connect_with_transport(Arc::new(fec_transport), tag.remote, self.kcp_config.clone())
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;
            let (rh, wh) = split(stream);
            return Ok(IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into());
        }

        // Plain KCP over UDP.
        let udp = UdpTransport::bind(bind_addr)
            .await
            .map_err(|e| Error::other(e.to_string()))?;
        let stream =
            KcpStream::connect_with_transport(Arc::new(udp), tag.remote, self.kcp_config.clone())
                .await
                .map_err(|e| Error::other(e.to_string()))?;

        let (rh, wh) = split(stream);
        Ok(IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into())
    }

    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool {
        let Some(new_tag) = new.tag().as_any().downcast_ref::<KcpLinkTag>() else { return true };

        let intro = format!(
            "Judging {} KCP link {} {} ({}) on {}",
            new.direction(),
            match new.direction() {
                Direction::Incoming => "from",
                Direction::Outgoing => "to",
            },
            new_tag.remote,
            String::from_utf8_lossy(new.remote_user_data()),
            String::from_utf8_lossy(new_tag.interface.as_deref().unwrap_or(b"any interface"))
        );

        match existing.iter().find(|link| {
            let Some(tag) = link.tag().as_any().downcast_ref::<KcpLinkTag>() else { return false };
            match self.link_filter {
                KcpLinkFilter::None => false,
                KcpLinkFilter::InterfaceInterface => {
                    tag.interface == new_tag.interface && link.remote_user_data() == new.remote_user_data()
                }
                KcpLinkFilter::InterfaceIp => {
                    tag.interface == new_tag.interface && tag.remote.ip() == new_tag.remote.ip()
                }
            }
        }) {
            Some(other) => {
                let other_tag = other.tag().as_any().downcast_ref::<KcpLinkTag>().unwrap();
                tracing::debug!("{intro} => link {} is redundant, rejecting.", other_tag.remote);
                false
            }
            None => {
                tracing::debug!("{intro} => accepted.");
                true
            }
        }
    }
}

/// KCP transport for incoming connections.
///
/// This transport is IO-stream based, using KCP over UDP.
#[derive(Debug)]
pub struct KcpAcceptor {
    bind_addr: SocketAddr,
    kcp_config: KcpConfig,
    #[cfg(feature = "fec")]
    fec_config: Option<fec::FecConfig>,
    #[cfg(feature = "phantun")]
    phantun_config: Option<phantun::PhantunConfig>,
}

impl fmt::Display for KcpAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.bind_addr)
    }
}

impl KcpAcceptor {
    /// Create a new KCP transport for incoming connections.
    ///
    /// The actual binding is deferred until [`AcceptingTransport::listen`] is called.
    /// Use a UDP port pre-check in your application code if you need early failure
    /// detection.
    pub fn new(bind_addr: SocketAddr, kcp_config: KcpConfig) -> Self {
        Self {
            bind_addr,
            kcp_config,
            #[cfg(feature = "fec")]
            fec_config: None,
            #[cfg(feature = "phantun")]
            phantun_config: None,
        }
    }

    /// Enables FEC (Forward Error Correction) with the given configuration.
    ///
    /// Both client and server must use the same FEC parameters.
    #[cfg(feature = "fec")]
    #[cfg_attr(docsrs, doc(cfg(feature = "fec")))]
    pub fn set_fec(&mut self, fec_config: fec::FecConfig) {
        self.fec_config = Some(fec_config);
    }

    /// Enables Phantun fake-TCP transport for incoming connections.
    ///
    /// When enabled, the server accepts fake-TCP connections through a TUN
    /// interface instead of listening on a UDP socket.
    ///
    /// Requires root/admin privileges to create TUN interfaces and
    /// appropriate routing/NAT rules.
    ///
    /// Requires root/admin privileges to create TUN interfaces.
    #[cfg(feature = "phantun")]
    #[cfg_attr(docsrs, doc(cfg(feature = "phantun")))]
    pub fn set_phantun(&mut self, phantun_config: phantun::PhantunConfig) {
        self.phantun_config = Some(phantun_config);
    }
}

#[async_trait]
impl AcceptingTransport for KcpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        // Phantun (fake TCP) listener — optionally with FEC on top.
        #[cfg(feature = "phantun")]
        if let Some(phantun_cfg) = &self.phantun_config {
            let port = self.bind_addr.port();
            let phantun_transport =
                phantun::PhantunServerTransport::listen(phantun_cfg, port)
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;

            #[cfg(feature = "fec")]
            if let Some(fec_cfg) = &self.fec_config {
                let fec_transport = fec::FecTransport::new(phantun_transport, fec_cfg);
                let mut listener =
                    KcpListener::with_transport(Arc::new(fec_transport), self.kcp_config.clone())
                        .await
                        .map_err(|e| Error::other(e.to_string()))?;

                tracing::info!(port, "KCP+Phantun+FEC listener started");

                loop {
                    let (stream, remote_addr) = listener
                        .accept()
                        .await
                        .map_err(|e| Error::other(e.to_string()))?;

                    tracing::debug!(%remote_addr, "Accepted KCP+Phantun+FEC connection");
                    let tag = KcpLinkTag { interface: None, remote: remote_addr, direction: Direction::Incoming };
                    let (rh, wh) = split(stream);
                    let _ = tx
                        .send(AcceptedStreamBox::new(
                            IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into(),
                            tag,
                        ))
                        .await;
                }
            }

            let mut listener =
                KcpListener::with_transport(Arc::new(phantun_transport), self.kcp_config.clone())
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;

            tracing::info!(port, "KCP+Phantun listener started");

            loop {
                let (stream, remote_addr) = listener
                    .accept()
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;

                tracing::debug!(%remote_addr, "Accepted KCP+Phantun connection");
                let tag = KcpLinkTag { interface: None, remote: remote_addr, direction: Direction::Incoming };
                let (rh, wh) = split(stream);
                let _ = tx
                    .send(AcceptedStreamBox::new(
                        IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into(),
                        tag,
                    ))
                    .await;
            }
        }

        // FEC over plain UDP transport.
        #[cfg(feature = "fec")]
        if let Some(fec_cfg) = &self.fec_config {
            let udp = UdpTransport::bind(self.bind_addr)
                .await
                .map_err(|e| Error::other(e.to_string()))?;
            let fec_transport = fec::FecTransport::new(udp, fec_cfg);
            let mut listener =
                KcpListener::with_transport(Arc::new(fec_transport), self.kcp_config.clone())
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;

            tracing::info!(addr = %self.bind_addr, "KCP+FEC listener started");

            loop {
                let (stream, remote_addr) = listener
                    .accept()
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;

                tracing::debug!(%remote_addr, "Accepted KCP+FEC connection");
                let tag = KcpLinkTag { interface: None, remote: remote_addr, direction: Direction::Incoming };

                let (rh, wh) = split(stream);
                let _ = tx
                    .send(AcceptedStreamBox::new(
                        IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into(),
                        tag,
                    ))
                    .await;
            }
        }

        // Plain KCP over UDP.
        let mut listener = KcpListener::bind(self.bind_addr, self.kcp_config.clone())
            .await
            .map_err(|e| Error::other(e.to_string()))?;

        tracing::info!(addr = %self.bind_addr, "KCP listener started");

        loop {
            let (stream, remote_addr) = listener
                .accept()
                .await
                .map_err(|e| Error::other(e.to_string()))?;

            tracing::debug!(%remote_addr, "Accepted KCP connection");
            let tag = KcpLinkTag { interface: None, remote: remote_addr, direction: Direction::Incoming };

            let (rh, wh) = split(stream);
            let _ = tx.send(AcceptedStreamBox::new(IoBox::new(SyncIo::new(rh), SyncIo::new(wh)).into(), tag)).await;
        }
    }
}

/// Resolve hostnames to socket addresses, filtered by IP version.
async fn resolve_hosts(hosts: &[String], ip_version: IpVersion) -> Vec<SocketAddr> {
    let mut addrs = HashSet::new();
    for host in hosts {
        match lookup_host(host).await {
            Ok(resolved) => {
                addrs.extend(resolved.filter(|addr| {
                    !((addr.is_ipv4() && ip_version.is_only_ipv6())
                        || (addr.is_ipv6() && ip_version.is_only_ipv4()))
                }));
            }
            Err(err) => {
                tracing::warn!(host = %host, %err, "KCP host resolution failed");
            }
        }
    }
    let mut addrs: Vec<_> = addrs.into_iter().collect();
    addrs.sort();
    addrs
}
