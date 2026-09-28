//! Dumb use of every enabled smoltcp feature, mirroring `stack_xarxa.rs` job for job:
//! the same echo ports, the same outgoing connection, the same periodic DNS query, the
//! same address configuration — through smoltcp's API.

// Which of the items below exist depends on the feature combination; keeping every
// cfg-permutation warning-clean is not worth it in a probe.
#![allow(unused)]

use defmt::info;
use embassy_time::{Duration, Instant as HwInstant};
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};

use super::*;
use crate::eth::Ethernet;

fn now() -> Instant {
    Instant::from_micros(now_micros())
}

/// The peer for outgoing traffic (TCP connects, DNS queries), in the first enabled
/// family.
#[cfg(feature = "ipv4")]
const SERVER_ADDR: IpAddress = IpAddress::v4(GATEWAY[0], GATEWAY[1], GATEWAY[2], GATEWAY[3]);
#[cfg(all(feature = "ipv6", not(feature = "ipv4")))]
const SERVER_ADDR: IpAddress = IpAddress::v6(
    IPV6_GATEWAY[0],
    IPV6_GATEWAY[1],
    IPV6_GATEWAY[2],
    IPV6_GATEWAY[3],
    IPV6_GATEWAY[4],
    IPV6_GATEWAY[5],
    IPV6_GATEWAY[6],
    IPV6_GATEWAY[7],
);

/// Packet-socket buffer sizing: a handful of small packets per direction.
const PKT_COUNT: usize = 4;
const PKT_SIZE: usize = 600;

pub fn run(mut device: Ethernet<ETH_TX, ETH_RX>) -> ! {
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(MAC_ADDR)));
    config.random_seed = SEED;
    #[cfg(feature = "slaac")]
    {
        config.slaac = true;
    }

    let mut iface = Interface::new(config, &mut device, now());
    iface.update_ip_addrs(|addrs| {
        #[cfg(feature = "ipv4")]
        addrs
            .push(IpCidr::new(
                IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from(IP_ADDR)),
                IP_PREFIX_LEN,
            ))
            .unwrap();
        #[cfg(feature = "ipv6")]
        addrs
            .push(IpCidr::new(
                IpAddress::v6(
                    IPV6_ADDR[0],
                    IPV6_ADDR[1],
                    IPV6_ADDR[2],
                    IPV6_ADDR[3],
                    IPV6_ADDR[4],
                    IPV6_ADDR[5],
                    IPV6_ADDR[6],
                    IPV6_ADDR[7],
                ),
                IPV6_PREFIX_LEN,
            ))
            .unwrap();
    });
    #[cfg(feature = "ipv4")]
    iface
        .routes_mut()
        .add_default_ipv4_route(smoltcp::wire::Ipv4Address::from(GATEWAY))
        .unwrap();
    #[cfg(feature = "ipv6")]
    iface
        .routes_mut()
        .add_default_ipv6_route(smoltcp::wire::Ipv6Address::new(
            IPV6_GATEWAY[0],
            IPV6_GATEWAY[1],
            IPV6_GATEWAY[2],
            IPV6_GATEWAY[3],
            IPV6_GATEWAY[4],
            IPV6_GATEWAY[5],
            IPV6_GATEWAY[6],
            IPV6_GATEWAY[7],
        ))
        .unwrap();

    #[cfg(feature = "multicast")]
    {
        #[cfg(feature = "ipv4")]
        iface
            .join_multicast_group(smoltcp::wire::Ipv4Address::new(224, 0, 0, 251))
            .unwrap();
        #[cfg(feature = "ipv6")]
        iface
            .join_multicast_group(smoltcp::wire::Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb))
            .unwrap();
    }

    static mut SOCKETS: [SocketStorage<'static>; 8] = [SocketStorage::EMPTY; 8];
    // SAFETY: this runs once, and nothing else names the static.
    let storage: &'static mut [SocketStorage<'static>] = unsafe { &mut *(&raw mut SOCKETS) };
    let mut sockets = SocketSet::new(storage);

    // UDP echo server.
    #[cfg(feature = "udp")]
    let udp = {
        use smoltcp::socket::udp;
        use smoltcp::storage::PacketMetadata;

        static mut RX_META: [PacketMetadata<udp::UdpMetadata>; PKT_COUNT] = [PacketMetadata::EMPTY; PKT_COUNT];
        static mut RX_BUF: [u8; PKT_COUNT * PKT_SIZE] = [0; PKT_COUNT * PKT_SIZE];
        static mut TX_META: [PacketMetadata<udp::UdpMetadata>; PKT_COUNT] = [PacketMetadata::EMPTY; PKT_COUNT];
        static mut TX_BUF: [u8; PKT_COUNT * PKT_SIZE] = [0; PKT_COUNT * PKT_SIZE];
        // SAFETY: this runs once, and nothing else names any of the four statics.
        let socket = unsafe {
            udp::Socket::new(
                udp::PacketBuffer::new(&mut (&mut *(&raw mut RX_META))[..], &mut (&mut *(&raw mut RX_BUF))[..]),
                udp::PacketBuffer::new(&mut (&mut *(&raw mut TX_META))[..], &mut (&mut *(&raw mut TX_BUF))[..]),
            )
        };
        let handle = sockets.add(socket);
        let socket = sockets.get_mut::<udp::Socket>(handle);
        socket.bind(ECHO_PORT).unwrap();
        #[cfg(feature = "async")]
        {
            socket.register_recv_waker(core::task::Waker::noop());
            socket.register_send_waker(core::task::Waker::noop());
        }
        handle
    };

    // Outgoing TCP connection.
    #[cfg(feature = "tcp")]
    let tcp_client = {
        use smoltcp::socket::tcp;

        static mut RX: [u8; 1024] = [0; 1024];
        static mut TX: [u8; 1024] = [0; 1024];
        // SAFETY: this runs once, and nothing else names either static.
        let socket = unsafe {
            tcp::Socket::new(
                tcp::SocketBuffer::new(&mut (&mut *(&raw mut RX))[..]),
                tcp::SocketBuffer::new(&mut (&mut *(&raw mut TX))[..]),
            )
        };
        let handle = sockets.add(socket);
        #[cfg(feature = "async")]
        {
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            socket.register_recv_waker(core::task::Waker::noop());
            socket.register_send_waker(core::task::Waker::noop());
        }
        handle
    };

    // TCP echo server. smoltcp has no listener type: one socket listens, serves the
    // connection, and goes back to listening when it closes.
    #[cfg(feature = "tcp-listener")]
    let tcp_server = {
        use smoltcp::socket::tcp;

        static mut RX: [u8; 1024] = [0; 1024];
        static mut TX: [u8; 1024] = [0; 1024];
        // SAFETY: this runs once, and nothing else names either static.
        let socket = unsafe {
            tcp::Socket::new(
                tcp::SocketBuffer::new(&mut (&mut *(&raw mut RX))[..]),
                tcp::SocketBuffer::new(&mut (&mut *(&raw mut TX))[..]),
            )
        };
        let handle = sockets.add(socket);
        sockets.get_mut::<tcp::Socket>(handle).listen(ECHO_PORT).unwrap();
        handle
    };

    // Raw socket, echoing received packets back out verbatim.
    #[cfg(feature = "raw-ip")]
    let raw = {
        use smoltcp::socket::raw;
        use smoltcp::storage::PacketMetadata;

        static mut RX_META: [PacketMetadata<()>; PKT_COUNT] = [PacketMetadata::EMPTY; PKT_COUNT];
        static mut RX_BUF: [u8; PKT_COUNT * PKT_SIZE] = [0; PKT_COUNT * PKT_SIZE];
        static mut TX_META: [PacketMetadata<()>; PKT_COUNT] = [PacketMetadata::EMPTY; PKT_COUNT];
        static mut TX_BUF: [u8; PKT_COUNT * PKT_SIZE] = [0; PKT_COUNT * PKT_SIZE];
        // SAFETY: this runs once, and nothing else names any of the four statics.
        let socket = unsafe {
            raw::Socket::new(
                None,
                None,
                raw::PacketBuffer::new(&mut (&mut *(&raw mut RX_META))[..], &mut (&mut *(&raw mut RX_BUF))[..]),
                raw::PacketBuffer::new(&mut (&mut *(&raw mut TX_META))[..], &mut (&mut *(&raw mut TX_BUF))[..]),
            )
        };
        let handle = sockets.add(socket);
        #[cfg(feature = "async")]
        {
            let socket = sockets.get_mut::<raw::Socket>(handle);
            socket.register_recv_waker(core::task::Waker::noop());
            socket.register_send_waker(core::task::Waker::noop());
        }
        handle
    };

    #[cfg(feature = "dhcpv4")]
    let dhcp = sockets.add(smoltcp::socket::dhcpv4::Socket::new());

    #[cfg(feature = "dns")]
    let dns = {
        use smoltcp::socket::dns;

        static mut QUERIES: [Option<dns::DnsQuery>; 1] = [None];
        // SAFETY: this runs once, and nothing else names the static.
        let queries: &'static mut [Option<dns::DnsQuery>] = unsafe { &mut *(&raw mut QUERIES) };
        sockets.add(dns::Socket::new(&[SERVER_ADDR], queries))
    };
    #[cfg(feature = "dns")]
    let mut dns_query: Option<smoltcp::socket::dns::QueryHandle> = None;

    info!("codesize smoltcp: stack up");

    let mut next_tick = HwInstant::from_ticks(0);
    let mut client_port: u16 = 49152;
    loop {
        iface.poll(now(), &mut device, &mut sockets);

        // A slow tick for the periodic actions (reconnects, DNS queries).
        let tick = HwInstant::now() >= next_tick;
        if tick {
            next_tick = HwInstant::now() + Duration::from_secs(5);
        }

        #[cfg(feature = "udp")]
        {
            use smoltcp::socket::udp;
            let socket = sockets.get_mut::<udp::Socket>(udp);
            let mut buf = [0u8; PKT_SIZE];
            while let Ok((n, mut meta)) = socket.recv_slice(&mut buf) {
                // Echo back to the sender, letting the stack pick the source. The
                // packet metadata (id) rides along unchanged.
                meta.local_address = None;
                let _ = socket.send_slice(&buf[..n], meta);
            }
        }

        #[cfg(feature = "tcp")]
        {
            use smoltcp::socket::tcp;
            let (socket, cx) = sockets_and_cx::<tcp::Socket>(&mut sockets, &mut iface, tcp_client);
            if tick {
                if socket.is_open() {
                    if socket.may_send() {
                        let _ = socket.send_slice(b"hello");
                    }
                } else {
                    // A fresh local port for every attempt, like an ephemeral port.
                    client_port = client_port.wrapping_add(1).max(49152);
                    let _ = socket.connect(cx, (SERVER_ADDR, ECHO_PORT), client_port);
                }
            }
            tcp_echo(socket);
        }

        #[cfg(feature = "tcp-listener")]
        {
            use smoltcp::socket::tcp;
            let socket = sockets.get_mut::<tcp::Socket>(tcp_server);
            tcp_echo(socket);
            // The peer closed and everything is echoed: close, then listen again.
            if !socket.may_recv() && socket.may_send() {
                socket.close();
            }
            if !socket.is_open() {
                let _ = socket.listen(ECHO_PORT);
            }
        }

        #[cfg(feature = "raw-ip")]
        {
            use smoltcp::socket::raw;
            let socket = sockets.get_mut::<raw::Socket>(raw);
            let mut buf = [0u8; PKT_SIZE];
            while let Ok(n) = socket.recv_slice(&mut buf) {
                let _ = socket.send_slice(&buf[..n]);
            }
        }

        #[cfg(feature = "dhcpv4")]
        {
            use smoltcp::socket::dhcpv4;
            match sockets.get_mut::<dhcpv4::Socket>(dhcp).poll() {
                Some(dhcpv4::Event::Configured(config)) => {
                    let address = config.address;
                    let router = config.router;
                    iface.update_ip_addrs(|addrs| {
                        let _ = addrs.push(IpCidr::Ipv4(address));
                    });
                    if let Some(router) = router {
                        let _ = iface.routes_mut().add_default_ipv4_route(router);
                    }
                }
                Some(dhcpv4::Event::Deconfigured) => {}
                None => {}
            }
        }

        #[cfg(feature = "dns")]
        {
            use smoltcp::socket::dns;
            let (socket, cx) = sockets_and_cx::<dns::Socket>(&mut sockets, &mut iface, dns);
            match dns_query {
                Some(query) => match socket.get_query_result(query) {
                    Err(dns::GetQueryResultError::Pending) => {}
                    result => {
                        info!("codesize smoltcp: dns query done, ok={}", result.is_ok());
                        dns_query = None;
                    }
                },
                None if tick => {
                    dns_query = socket.start_query(cx, DNS_NAME, smoltcp::wire::DnsQueryType::A).ok();
                }
                None => {}
            }
        }
    }
}

/// One socket plus the interface context, borrowed together for the calls that need
/// both (`connect`, `start_query`).
fn sockets_and_cx<'a, T: smoltcp::socket::AnySocket<'static>>(
    sockets: &'a mut SocketSet<'static>,
    iface: &'a mut Interface,
    handle: smoltcp::iface::SocketHandle,
) -> (&'a mut T, &'a mut smoltcp::iface::Context) {
    (sockets.get_mut::<T>(handle), iface.context())
}

/// Move every received byte back into the transmit ring.
#[cfg(feature = "tcp")]
fn tcp_echo(socket: &mut smoltcp::socket::tcp::Socket) {
    let mut buf = [0u8; 256];
    while socket.can_recv() && socket.can_send() {
        let n = socket.recv_slice(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        let _ = socket.send_slice(&buf[..n]);
    }
}
