//! Adapts one interface-bound Linux UDP socket to the temporary iroh endpoint.
//!
//! Iroh authenticates the endpoint keys and protects QUIC streams. The socket
//! is bound to the selected interface before iroh starts. A separate route
//! check verifies that the observed peer is reached directly through that link.

use std::{
    fs,
    io::{self, IoSliceMut},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    task::{Context, Poll, ready},
    time::Duration,
};

use iroh::endpoint::transports::{
    CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit,
};
use iroh_base::CustomAddr;
use n0_watcher::{Direct, Watchable};
use noq_udp::RecvMeta;
use serde_json::Value;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    io::{AsyncReadExt, ReadBuf},
    net::UdpSocket,
    process::Command,
    time::timeout,
};

const LAN_ADDRESS_TAG: u64 = 0x4a53_5041_4952;
const MAX_IP_OUTPUT: u64 = 32 * 1024;

/// The selected IPv4 address and prefix on a Linux Ethernet or wireless link.
#[derive(Clone, Debug)]
pub(super) struct LanLink {
    interface: String,
    ifindex: String,
    local: SocketAddrV4,
    prefix: u8,
}

impl LanLink {
    /// Build an isolated Linux loopback fixture for the real QUIC exchange.
    #[cfg(test)]
    pub(super) fn loopback_fixture() -> Self {
        let temporary = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(local) = temporary.local_addr().unwrap() else {
            unreachable!("fixture socket uses IPv4");
        };
        Self {
            interface: "lo".into(),
            ifindex: "test-loopback".into(),
            local,
            prefix: 8,
        }
    }

    /// Verify an explicit physical link and its requested IPv4 address.
    pub(super) async fn inspect(interface: String, local: SocketAddrV4) -> io::Result<Self> {
        if interface.is_empty()
            || interface.len() > 15
            || !interface
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
            || interface == "."
            || interface == ".."
            || local.port() == 0
            || !usable(*local.ip())
        {
            return Err(io::Error::other(
                "select a physical interface and a usable IPv4 address and port",
            ));
        }
        let path = Path::new("/sys/class/net").join(&interface);
        let canonical = fs::canonicalize(&path)?;
        if canonical.starts_with("/sys/devices/virtual")
            || !path.join("device").is_dir()
            || fs::read_to_string(path.join("type"))?.trim() != "1"
        {
            return Err(io::Error::other(
                "pairing requires a physical Ethernet or wireless interface",
            ));
        }
        let ifindex = fs::read_to_string(path.join("ifindex"))?;
        let addresses = ip_json(&["-4", "addr", "show", "dev", &interface]).await?;
        let ip = local.ip().to_string();
        let prefix = addresses
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["addr_info"].as_array().into_iter().flatten())
            .find_map(|entry| {
                if entry["local"].as_str() != Some(ip.as_str()) {
                    return None;
                }
                u8::try_from(entry["prefixlen"].as_u64()?).ok()
            })
            .filter(|prefix| (1..=31).contains(prefix))
            .ok_or_else(|| {
                io::Error::other("the selected address is not assigned to the physical interface")
            })?;
        Ok(Self {
            interface,
            ifindex,
            local,
            prefix,
        })
    }

    /// Return the address advertised in the operator's invitation.
    pub(super) fn address(&self) -> SocketAddrV4 {
        self.local
    }

    /// Verify that the still-selected interface routes directly to this peer.
    pub(super) async fn peer_is_on_link(&self, peer: SocketAddrV4) -> bool {
        #[cfg(test)]
        if self.ifindex == "test-loopback" {
            return self.in_prefix(peer);
        }
        if !self.in_prefix(peer)
            || !fs::read_to_string(
                Path::new("/sys/class/net")
                    .join(&self.interface)
                    .join("ifindex"),
            )
            .is_ok_and(|index| index == self.ifindex)
        {
            return false;
        }
        let remote = peer.ip().to_string();
        let local = self.local.ip().to_string();
        let Ok(routes) = ip_json(&["-4", "route", "get", &remote, "from", &local]).await else {
            return false;
        };
        let Some(route) = routes
            .as_array()
            .filter(|routes| routes.len() == 1)
            .and_then(|routes| routes.first())
        else {
            return false;
        };
        route["dev"].as_str() == Some(self.interface.as_str())
            && route.get("gateway").is_none()
            && route.get("via").is_none()
            && matches!(
                route.get("type").and_then(Value::as_str),
                None | Some("unicast")
            )
    }

    /// Apply the bound link's subnet rule before passing datagrams to QUIC.
    fn in_prefix(&self, peer: SocketAddrV4) -> bool {
        #[cfg(test)]
        if self.ifindex == "test-loopback" {
            return peer.ip().is_loopback() && peer.ip() != self.local.ip();
        }
        let shift = u32::from(32 - self.prefix);
        let network = u32::from(*self.local.ip()) >> shift;
        let candidate = u32::from(*peer.ip());
        let host = candidate & ((1 << shift) - 1);
        usable(*peer.ip())
            && peer.ip() != self.local.ip()
            && candidate >> shift == network
            && (self.prefix == 31 || (host != 0 && host != (1 << shift) - 1))
    }
}

/// Refuse non-unicast and link-local addresses for the initial IPv4 adapter.
fn usable(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !ip.is_link_local()
        && !ip.is_broadcast()
        && ip.octets()[0] != 0
}

/// Obtain small structured address/route evidence from the host iproute2 utility.
async fn ip_json(args: &[&str]) -> io::Result<Value> {
    let binary = ["/usr/sbin/ip", "/usr/bin/ip", "/sbin/ip", "/bin/ip"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| {
            fs::metadata(path)
                .is_ok_and(|entry| entry.is_file() && entry.uid() == 0 && entry.mode() & 0o022 == 0)
        })
        .ok_or_else(|| io::Error::other("iproute2 is unavailable"))?;
    timeout(Duration::from_secs(2), async {
        let mut child = Command::new(binary)
            .arg("-j")
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("iproute2 has no output pipe"))?
            .take(MAX_IP_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > MAX_IP_OUTPUT || !child.wait().await?.success() {
            return Err(io::Error::other(
                "iproute2 could not verify the selected link",
            ));
        }
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await
    .map_err(io::Error::other)?
}

/// Create the sole UDP transport from an already checked link.
#[derive(Debug)]
pub(super) struct LanTransport(pub(super) LanLink);

impl CustomTransport for LanTransport {
    /// Bind a socket to the actual device as well as its selected IP and port.
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.bind_device(Some(self.0.interface.as_bytes()))?;
        socket.set_nonblocking(true)?;
        socket.bind(&SocketAddr::V4(self.0.local).into())?;
        let local = encode(self.0.local);
        let endpoint = LanSocket {
            socket: Arc::new(UdpSocket::from_std(socket.into())?),
            link: self.0.clone(),
        };
        Ok(Box::new(LanEndpoint {
            endpoint,
            local,
            advertised: Watchable::new(vec![local]),
        }))
    }
}

/// One kernel UDP socket shared between iroh's receive and send interfaces.
#[derive(Clone, Debug)]
struct LanSocket {
    socket: Arc<UdpSocket>,
    link: LanLink,
}

/// Associates incoming packet metadata with the bound kernel socket.
#[derive(Debug)]
struct LanEndpoint {
    endpoint: LanSocket,
    local: CustomAddr,
    advertised: Watchable<Vec<CustomAddr>>,
}

impl CustomEndpoint for LanEndpoint {
    /// Expose just the explicitly configured address to iroh's routing layer.
    fn watch_local_addrs(&self) -> Direct<Vec<CustomAddr>> {
        self.advertised.watch()
    }

    /// Share the same kernel-bound socket for outbound datagrams.
    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(self.endpoint.clone())
    }

    /// Deliver one on-subnet datagram and its actual peer address to QUIC.
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        metas: &mut [RecvMeta],
        info: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() {
            return Poll::Ready(Ok(0));
        }
        for _ in 0..16 {
            let mut buf = ReadBuf::new(&mut bufs[0]);
            let remote = ready!(self.endpoint.socket.poll_recv_from(cx, &mut buf))?;
            if let SocketAddr::V4(remote) = remote
                && self.endpoint.link.in_prefix(remote)
            {
                let len = buf.filled().len();
                metas[0] = RecvMeta {
                    len,
                    stride: len,
                    ..RecvMeta::default()
                };
                info[0] = RecvInfo::new(encode(remote), Some(self.local));
                return Poll::Ready(Ok(1));
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl CustomSender for LanSocket {
    /// Route only this adapter's on-subnet addresses to the interface socket.
    fn is_valid_send_addr(&self, address: &CustomAddr) -> bool {
        decode(address).is_some_and(|address| self.link.in_prefix(address))
    }

    /// Send one QUIC datagram through the pinned interface.
    fn poll_send(
        &self,
        cx: &mut Context<'_>,
        dst: &CustomAddr,
        _src: Option<&CustomAddr>,
        packet: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(dst) = decode(dst).filter(|dst| self.link.in_prefix(*dst)) else {
            return Poll::Ready(Err(io::Error::other(
                "invalid pairing datagram destination",
            )));
        };
        if packet
            .segment_size
            .is_some_and(|size| size < packet.contents.len())
        {
            return Poll::Ready(Err(io::Error::other(
                "pairing UDP transport accepts one datagram",
            )));
        }
        ready!(self.socket.poll_send_to(cx, packet.contents, dst.into()))?;
        Poll::Ready(Ok(()))
    }
}

/// Encode the actual peer IP and port into iroh's custom address space.
fn encode(address: SocketAddrV4) -> CustomAddr {
    let [a, b, c, d] = address.ip().octets();
    let [e, f] = address.port().to_be_bytes();
    CustomAddr::from_parts(LAN_ADDRESS_TAG, &[a, b, c, d, e, f])
}

/// Decode only this adapter's six-byte addresses.
pub(super) fn decode(address: &CustomAddr) -> Option<SocketAddrV4> {
    let [a, b, c, d, e, f] = *address.data() else {
        return None;
    };
    (address.id() == LAN_ADDRESS_TAG)
        .then(|| SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), u16::from_be_bytes([e, f])))
}
