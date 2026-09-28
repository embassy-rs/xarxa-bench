//! Dumb use of every enabled xarxa feature, so its code survives the linker.
//!
//! Built without `alloc`, like the throughput benchmark: the driver and the TCP ring
//! buffers are lent to the stack from statics (DESIGN.md §3 "Lent storage").

// Which of the items below exist depends on the feature combination; keeping every
// cfg-permutation warning-clean is not worth it in a probe.
#![allow(unused)]

use defmt::info;
use embassy_time::{Duration, Instant as HwInstant};
use xarxa::Stack;
use xarxa::time::Instant;
use xarxa::wire::{IpAddr, IpCidr, ListenSocketAddr, SocketAddr};

use super::*;
use crate::eth::Ethernet;

fn now() -> Instant {
    // The low 32 bits of the millisecond count: xarxa's instants wrap around.
    Instant::from_millis((now_micros() / 1000) as u32)
}

/// The peer for outgoing traffic (TCP connects, DNS queries), in the first enabled
/// family.
#[cfg(feature = "ipv4")]
const SERVER_ADDR: IpAddr = IpAddr::v4(GATEWAY[0], GATEWAY[1], GATEWAY[2], GATEWAY[3]);
#[cfg(all(feature = "ipv6", not(feature = "ipv4")))]
const SERVER_ADDR: IpAddr = IpAddr::v6(
    IPV6_GATEWAY[0],
    IPV6_GATEWAY[1],
    IPV6_GATEWAY[2],
    IPV6_GATEWAY[3],
    IPV6_GATEWAY[4],
    IPV6_GATEWAY[5],
    IPV6_GATEWAY[6],
    IPV6_GATEWAY[7],
);

// `device` is lent to the stack, so it must outlive it: taking it `mut` in the
// signature rather than rebinding it in the body keeps it out of a local slot.
pub fn run(mut device: Ethernet<ETH_TX, ETH_RX>) -> ! {
    let mut stack = Stack::new(SEED);
    let iface = stack.add_iface_borrowed(&mut device).unwrap();

    // Static addresses and default routes, per enabled family.
    #[cfg(feature = "ipv4")]
    {
        use xarxa::wire::Ipv4Addr;
        stack
            .iface(iface)
            .add_ip_addr(IpCidr::new(IpAddr::V4(Ipv4Addr::from(IP_ADDR)), IP_PREFIX_LEN))
            .unwrap();
        stack
            .routes_mut()
            .add_default_ipv4_route(Ipv4Addr::from(GATEWAY), iface)
            .unwrap();
    }
    #[cfg(feature = "ipv6")]
    {
        use xarxa::wire::Ipv6Addr;
        stack
            .iface(iface)
            .add_ip_addr(IpCidr::new(
                IpAddr::v6(
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
        stack
            .routes_mut()
            .add_default_ipv6_route(
                Ipv6Addr::new(
                    IPV6_GATEWAY[0],
                    IPV6_GATEWAY[1],
                    IPV6_GATEWAY[2],
                    IPV6_GATEWAY[3],
                    IPV6_GATEWAY[4],
                    IPV6_GATEWAY[5],
                    IPV6_GATEWAY[6],
                    IPV6_GATEWAY[7],
                ),
                iface,
            )
            .unwrap();
    }

    #[cfg(feature = "hostname")]
    stack.set_hostname("codesize");

    #[cfg(feature = "dhcpv4")]
    {
        let mut config = xarxa::iface::dhcpv4::DhcpConfig::default();
        // Also ask for NTP servers (option 42), read raw from the lease below.
        #[cfg(feature = "dhcpv4-options")]
        {
            config.parameter_request_list = Some(&[1, 3, 6, 42]);
        }
        stack.iface(iface).set_dhcpv4(Some(config));
    }

    #[cfg(feature = "slaac")]
    stack
        .iface(iface)
        .set_slaac(Some(xarxa::iface::slaac::SlaacConfig::default()));

    #[cfg(feature = "multicast")]
    {
        #[cfg(feature = "ipv4")]
        stack
            .iface(iface)
            .join_multicast_group(xarxa::wire::Ipv4Addr::new(224, 0, 0, 251))
            .unwrap();
        #[cfg(feature = "ipv6")]
        stack
            .iface(iface)
            .join_multicast_group(xarxa::wire::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb))
            .unwrap();
    }

    // UDP echo server.
    #[cfg(feature = "udp")]
    let udp = {
        let handle = stack.add_udp_socket().unwrap();
        let mut socket = stack.udp_socket(handle);
        #[cfg(feature = "iface-bind")]
        socket.bind_to_iface(Some(iface)).unwrap();
        socket.bind(ECHO_PORT, ListenSocketAddr::UNSPECIFIED).unwrap();
        #[cfg(feature = "async")]
        {
            socket.register_recv_waker(core::task::Waker::noop());
            socket.register_send_waker(core::task::Waker::noop());
        }
        handle
    };

    // Outgoing TCP connection (an echo client of sorts: it sends a greeting and echoes
    // whatever comes back).
    #[cfg(feature = "tcp")]
    let tcp_client = {
        static mut RX: [u8; 1024] = [0; 1024];
        static mut TX: [u8; 1024] = [0; 1024];
        // SAFETY: this runs once, and nothing else names either static.
        let (rx, tx) = unsafe { (&mut *(&raw mut RX), &mut *(&raw mut TX)) };
        let handle = stack.add_tcp_socket_with_bufs(rx, tx).unwrap();
        let mut socket = stack.tcp_socket(handle);
        #[cfg(feature = "iface-bind")]
        socket.bind_to_iface(Some(iface)).unwrap();
        #[cfg(feature = "async")]
        {
            socket.register_recv_waker(core::task::Waker::noop());
            socket.register_send_waker(core::task::Waker::noop());
        }
        handle
    };

    // TCP echo server: a listener plus one connection socket its tokens are
    // accepted into, reused as connections come and go (the no-alloc form).
    #[cfg(feature = "tcp-listener")]
    let (listener, tcp_server) = {
        static mut RX: [u8; 1024] = [0; 1024];
        static mut TX: [u8; 1024] = [0; 1024];
        // SAFETY: this runs once, and nothing else names either static.
        let (rx, tx) = unsafe { (&mut *(&raw mut RX), &mut *(&raw mut TX)) };
        let conn = stack.add_tcp_socket_with_bufs(rx, tx).unwrap();

        let handle = stack.add_tcp_listener().unwrap();
        let mut listener = stack.tcp_listener(handle);
        #[cfg(feature = "iface-bind")]
        listener.bind_to_iface(Some(iface)).unwrap();
        listener.listen(ECHO_PORT).unwrap();
        #[cfg(feature = "async")]
        listener.register_accept_waker(core::task::Waker::noop());
        (handle, conn)
    };

    // Raw sockets: one per mode, echoing whatever they receive back out verbatim,
    // which drives both the receive filter and the verbatim egress paths.
    #[cfg(feature = "raw-ip")]
    let raw_ip = {
        let handle = stack.add_raw_socket().unwrap();
        let mut socket = stack.raw_socket(handle);
        #[cfg(feature = "iface-bind")]
        socket.bind_to_iface(Some(iface)).unwrap();
        socket
            .bind(xarxa::raw::RawMode::Ip {
                version: None,
                protocol: None,
            })
            .unwrap();
        #[cfg(feature = "async")]
        {
            socket.register_recv_waker(core::task::Waker::noop());
            socket.register_send_waker(core::task::Waker::noop());
        }
        handle
    };
    #[cfg(feature = "raw-ethernet")]
    let raw_eth = {
        let handle = stack.add_raw_socket().unwrap();
        stack
            .raw_socket(handle)
            .bind(xarxa::raw::RawMode::Ethernet { ethertype: None })
            .unwrap();
        handle
    };

    #[cfg(feature = "dns")]
    let mut dns = xarxa::dns::DnsClient::new(&mut stack, &[SERVER_ADDR]).unwrap();
    #[cfg(feature = "dns")]
    let mut dns_query: Option<xarxa::dns::DnsQueryHandle> = None;

    info!("codesize xarxa: stack up");

    let mut next_tick = HwInstant::from_ticks(0);
    loop {
        stack.poll(now());

        // A slow tick for the periodic actions (reconnects, DNS queries).
        let tick = HwInstant::now() >= next_tick;
        if tick {
            next_tick = HwInstant::now() + Duration::from_secs(5);
        }

        #[cfg(feature = "udp")]
        {
            let mut socket = stack.udp_socket(udp);
            #[cfg(feature = "icmp-errors")]
            {
                let _ = socket.take_icmp_error();
            }
            while let Ok(packet) = socket.recv() {
                let mut meta = packet.meta();
                // Echo back to the sender, letting the stack pick the source.
                meta.local_addr = None;
                #[cfg(feature = "packetmeta-id")]
                {
                    meta.meta.id = meta.meta.id.wrapping_add(1);
                }
                #[cfg(feature = "packetmeta-timestamp")]
                {
                    let _ = meta.meta.timestamp;
                    meta.meta.request_timestamp = true;
                }
                let _ = socket.send_slice(packet.payload(), meta);
            }
        }

        #[cfg(feature = "tcp")]
        {
            let mut socket = stack.tcp_socket(tcp_client);
            #[cfg(feature = "icmp-errors")]
            {
                let _ = socket.take_icmp_error();
            }
            if tick {
                if socket.is_open() {
                    if socket.may_send() {
                        let _ = socket.send_slice(b"hello");
                    }
                } else {
                    // Ephemeral local port (0): a new tuple for every attempt.
                    let _ = socket.connect(SocketAddr::from((SERVER_ADDR, ECHO_PORT)), 0);
                }
            }
            tcp_echo(&mut socket);
        }

        #[cfg(feature = "tcp-listener")]
        {
            if !stack.tcp_socket(tcp_server).is_open()
                && let Some(token) = stack.tcp_listener(listener).accept()
            {
                let _ = stack.tcp_socket(tcp_server).accept(token);
            }
            let mut socket = stack.tcp_socket(tcp_server);
            tcp_echo(&mut socket);
            // The peer closed and everything is echoed: close our half too.
            if !socket.may_recv() && socket.may_send() {
                socket.close();
            }
        }

        #[cfg(feature = "raw-ip")]
        {
            let mut socket = stack.raw_socket(raw_ip);
            while let Ok(packet) = socket.recv() {
                let _ = socket.send_slice(&packet);
            }
        }
        #[cfg(feature = "raw-ethernet")]
        {
            let mut socket = stack.raw_socket(raw_eth);
            while let Ok(packet) = socket.recv() {
                let _ = socket.send_slice(&packet);
            }
        }

        #[cfg(feature = "dns")]
        {
            let _ = dns.poll(&mut stack, now());
            match dns_query {
                Some(query) => match dns.get_query_result(query) {
                    Err(xarxa::dns::GetQueryResultError::Pending) => {}
                    result => {
                        info!("codesize xarxa: dns query done, ok={}", result.is_ok());
                        dns_query = None;
                    }
                },
                None if tick => {
                    dns_query = dns.start_query(&mut stack, DNS_NAME, xarxa::wire::DnsType::A).ok();
                }
                None => {}
            }
        }

        #[cfg(feature = "dhcpv4")]
        if tick {
            if let Some(lease) = stack.iface(iface).dhcpv4_lease() {
                let _ = lease.address;
                #[cfg(feature = "dhcpv4-options")]
                {
                    let _ = lease.options.get(42);
                }
            }
        }

        #[cfg(feature = "slaac")]
        if tick {
            let _ = stack.iface(iface).slaac();
        }

        #[cfg(feature = "packetmeta-timestamp")]
        {
            let _ = stack.poll_tx_timestamp();
        }
    }
}

/// Move every received byte back into the transmit ring.
#[cfg(feature = "tcp")]
fn tcp_echo(socket: &mut xarxa::tcp::TcpSocket<'_, '_>) {
    let mut buf = [0u8; 256];
    while socket.can_recv() && socket.can_send() {
        let n = socket.recv_slice(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        let _ = socket.send_slice(&buf[..n]);
    }
}
