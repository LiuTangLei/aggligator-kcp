//! Utilities for network interface handling in KCP transport.

use network_interface::NetworkInterfaceConfig;
use std::{
    collections::HashSet,
    io::{Error, Result},
    net::{IpAddr, SocketAddr},
};

pub use network_interface::{Addr, NetworkInterface};

/// Gets the list of local network interfaces from the operating system.
///
/// Filters out interfaces that are most likely useless.
pub fn local_interfaces() -> Result<Vec<NetworkInterface>> {
    Ok(NetworkInterface::show()
        .map_err(|err| Error::other(err.to_string()))?
        .into_iter()
        .filter(|iface| !iface.name.starts_with("ifb"))
        .collect())
}

/// Returns the interface names usable for connecting to target.
///
/// Filters interfaces that either have no IP address or only support
/// an IP protocol version that does not match the target address.
pub fn interface_names_for_target(interfaces: &[NetworkInterface], target: SocketAddr) -> HashSet<Vec<u8>> {
    interfaces
        .iter()
        .filter_map(|iface| {
            iface
                .addr
                .iter()
                .any(|addr| {
                    !addr.ip().is_unspecified()
                        && addr.ip().is_loopback() == target.ip().is_loopback()
                        && addr.ip().is_ipv4() == target.is_ipv4()
                        && addr.ip().is_ipv6() == target.is_ipv6()
                })
                .then(|| iface.name.as_bytes().to_vec())
        })
        .collect()
}

/// Finds a local IP address on the given interface suitable for reaching the target.
///
/// Returns an IP address on `interface` that matches the address family of `target_ip`.
pub fn interface_local_addr(interface: &[u8], target_ip: IpAddr) -> Option<IpAddr> {
    let interfaces = local_interfaces().ok()?;
    interfaces.into_iter().find_map(|iface| {
        if iface.name.as_bytes() != interface {
            return None;
        }
        iface.addr.iter().find_map(|addr| {
            let ip = addr.ip();
            if ip.is_unspecified() {
                return None;
            }
            if ip.is_ipv4() == target_ip.is_ipv4() {
                Some(ip)
            } else {
                None
            }
        })
    })
}
