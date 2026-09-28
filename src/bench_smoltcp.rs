//! The benchmarks, on smoltcp.
//!
//! Structurally identical to `bench_xarxa.rs` — same loop shape, same buffer sizes, same
//! chunk sizes — so that what differs between the two runs is the stack.
//!
//! Built without smoltcp's `alloc` feature, like the xarxa side: the socket set and the
//! socket buffers are statics, lent to smoltcp by `&'static mut`.

use defmt::info;
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv6Address};

use super::*;
use crate::eth::Ethernet;
use crate::{GATEWAY, IP_ADDR, IP_PREFIX_LEN, IPV6_ADDR, IPV6_PREFIX_LEN, MAC_ADDR, now_micros};

fn now() -> Instant {
    Instant::from_micros(now_micros())
}

/// The perf-server's address, in the family the `bench-ipv4`/`bench-ipv6` feature selected.
#[cfg(feature = "bench-ipv4")]
const SERVER_ADDR: IpAddress = IpAddress::v4(SERVER_V4[0], SERVER_V4[1], SERVER_V4[2], SERVER_V4[3]);
#[cfg(feature = "bench-ipv6")]
const SERVER_ADDR: IpAddress = IpAddress::v6(
    SERVER_V6[0],
    SERVER_V6[1],
    SERVER_V6[2],
    SERVER_V6[3],
    SERVER_V6[4],
    SERVER_V6[5],
    SERVER_V6[6],
    SERVER_V6[7],
);

/// The stack, kept together so the benchmarks can pass one thing around. smoltcp splits
/// what xarxa keeps in one `Stack` into three values that all have to be handed to
/// `poll` together.
struct Net {
    iface: Interface,
    device: Ethernet<ETH_TX, ETH_RX>,
    sockets: SocketSet<'static>,
}

impl Net {
    fn poll(&mut self) {
        self.iface.poll(now(), &mut self.device, &mut self.sockets);
    }
}

/// Bring up the stack on the device, then hand over to the selected benchmark.
pub fn run(mut device: Ethernet<ETH_TX, ETH_RX>) -> ! {
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(MAC_ADDR)));
    // xarxa seeds its PRNG with a fixed value too, so neither side gets to be luckier
    // about initial sequence numbers or ephemeral ports.
    config.random_seed = 0x1234_5678_dead_beef;

    let mut iface = Interface::new(config, &mut device, now());
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(IP_ADDR)), IP_PREFIX_LEN))
            .unwrap();
        addrs
            .push(IpCidr::new(
                IpAddress::Ipv6(Ipv6Address::new(
                    IPV6_ADDR[0],
                    IPV6_ADDR[1],
                    IPV6_ADDR[2],
                    IPV6_ADDR[3],
                    IPV6_ADDR[4],
                    IPV6_ADDR[5],
                    IPV6_ADDR[6],
                    IPV6_ADDR[7],
                )),
                IPV6_PREFIX_LEN,
            ))
            .unwrap();
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::from(GATEWAY))
        .unwrap();

    // One socket, whichever benchmark is built.
    static mut SOCKETS: [SocketStorage<'static>; 1] = [SocketStorage::EMPTY; 1];
    // SAFETY: this runs once, and nothing else names the static.
    let storage: &'static mut [SocketStorage<'static>] = unsafe { &mut *(&raw mut SOCKETS) };
    let sockets = SocketSet::new(storage);

    let net = Net { iface, device, sockets };
    info!("{} {}: stack up, talking to {}", STACK, IPV, SERVER_ADDR);

    bench(net)
}

// ---------------------------------------------------------------------------
// TCP
// ---------------------------------------------------------------------------

/// Add the benchmark's one TCP socket, lending it a pair of static ring buffers.
///
/// The capacities are [`TCP_RX_CAP`] and [`TCP_TX_CAP`], the same two the xarxa side
/// uses.
#[cfg(any(feature = "bench-tcp-tx", feature = "bench-tcp-rx"))]
fn add_tcp_socket(net: &mut Net) -> smoltcp::iface::SocketHandle {
    use smoltcp::socket::tcp;

    static mut RX_BUF: [u8; TCP_RX_CAP] = [0; TCP_RX_CAP];
    static mut TX_BUF: [u8; TCP_TX_CAP] = [0; TCP_TX_CAP];

    // SAFETY: called once, and nothing else names either static.
    let rx: &'static mut [u8] = unsafe { &mut *(&raw mut RX_BUF) };
    let tx: &'static mut [u8] = unsafe { &mut *(&raw mut TX_BUF) };
    let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx));
    // Reno on all three stacks: it is what lwIP's TCP implements and cannot be
    // turned off, and xarxa and smoltcp both default to no congestion control at
    // all, so both are told to use it.
    socket.set_congestion_control(tcp::CongestionControl::Reno);
    net.sockets.add(socket)
}

/// Open the connection, retrying on the next local port if the server does not answer.
///
/// See the same function in `bench_xarxa.rs` for why the port has to move, and why every
/// attempt reuses the one socket.
#[cfg(any(feature = "bench-tcp-tx", feature = "bench-tcp-rx"))]
fn connect(net: &mut Net, handle: smoltcp::iface::SocketHandle, port: u16) {
    use embassy_time::{Duration, Instant as HwInstant};
    use smoltcp::socket::tcp;

    for local_port in LOCAL_PORT.. {
        info!(
            "{} {}: connecting to {}:{} from :{}...",
            STACK, DIR, SERVER_ADDR, port, local_port
        );
        let cx = net.iface.context();
        net.sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(cx, IpEndpoint::from((SERVER_ADDR, port)), local_port)
            .unwrap();

        let deadline = HwInstant::now() + Duration::from_secs(2);
        loop {
            net.poll();
            match net.sockets.get::<tcp::Socket>(handle).state() {
                tcp::State::Established => {
                    info!("{} {}: connected", STACK, DIR);
                    return;
                }
                tcp::State::Closed => break,
                _ if HwInstant::now() > deadline => break,
                _ => {}
            }
        }

        net.sockets.get_mut::<tcp::Socket>(handle).abort();
        net.poll();
    }
    unreachable!()
}

/// Board -> server: push into the send ring as fast as it drains.
#[cfg(feature = "bench-tcp-tx")]
fn bench(mut net: Net) -> ! {
    use smoltcp::socket::tcp;

    let handle = add_tcp_socket(&mut net);
    connect(&mut net, handle, TCP_UPLOAD_PORT);
    let buf = [0x5au8; IO_CHUNK];
    let mut meter = Meter::new();

    loop {
        net.poll();

        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.may_send() {
            defmt::panic!("{} {}: connection closed", STACK, DIR);
        }
        while socket.can_send() {
            match socket.send_slice(&buf) {
                Ok(0) => break,
                Ok(n) => meter.add(n),
                Err(e) => defmt::panic!("{} {}: send error: {}", STACK, DIR, e),
            }
        }

        // The data just enqueued goes out in this poll.
        net.poll();
        meter.tick();
    }
}

/// Server -> board: drain the receive ring as fast as it fills.
#[cfg(feature = "bench-tcp-rx")]
fn bench(mut net: Net) -> ! {
    use smoltcp::socket::tcp;

    let handle = add_tcp_socket(&mut net);
    connect(&mut net, handle, TCP_DOWNLOAD_PORT);
    let mut buf = [0u8; IO_CHUNK];
    let mut meter = Meter::new();

    loop {
        net.poll();

        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.may_recv() {
            defmt::panic!("{} {}: connection closed", STACK, DIR);
        }
        while socket.can_recv() {
            match socket.recv_slice(&mut buf) {
                Ok(0) => break,
                Ok(n) => meter.add(n),
                Err(e) => defmt::panic!("{} {}: recv error: {}", STACK, DIR, e),
            }
        }

        // Reading opens the window; this poll sends the window update.
        net.poll();
        meter.tick();
    }
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

/// A UDP socket with a [`UDP_RX_PACKETS`]-deep queue in each direction.
///
/// smoltcp needs a transmit queue too (egress happens in `poll`, not in `send`), which
/// xarxa has no equivalent of — its `send_slice` builds the frame and hands it to the
/// device then and there.
#[cfg(any(feature = "bench-udp-tx", feature = "bench-udp-rx"))]
fn udp_socket() -> smoltcp::socket::udp::Socket<'static> {
    use smoltcp::socket::udp;
    use smoltcp::storage::PacketMetadata;

    static mut RX_META: [PacketMetadata<udp::UdpMetadata>; UDP_RX_PACKETS] = [PacketMetadata::EMPTY; UDP_RX_PACKETS];
    static mut RX_BUF: [u8; UDP_RX_PACKETS * UDP_PAYLOAD] = [0; UDP_RX_PACKETS * UDP_PAYLOAD];
    static mut TX_META: [PacketMetadata<udp::UdpMetadata>; UDP_TX_PACKETS] = [PacketMetadata::EMPTY; UDP_TX_PACKETS];
    static mut TX_BUF: [u8; UDP_TX_PACKETS * UDP_PAYLOAD] = [0; UDP_TX_PACKETS * UDP_PAYLOAD];

    // SAFETY: called once, and nothing else names any of the four statics.
    let rx_meta: &'static mut [PacketMetadata<udp::UdpMetadata>] = unsafe { &mut *(&raw mut RX_META) };
    let rx_buf: &'static mut [u8] = unsafe { &mut *(&raw mut RX_BUF) };
    let tx_meta: &'static mut [PacketMetadata<udp::UdpMetadata>] = unsafe { &mut *(&raw mut TX_META) };
    let tx_buf: &'static mut [u8] = unsafe { &mut *(&raw mut TX_BUF) };
    let rx = udp::PacketBuffer::new(rx_meta, rx_buf);
    let tx = udp::PacketBuffer::new(tx_meta, tx_buf);
    udp::Socket::new(rx, tx)
}

/// Board -> server: fire full-MTU datagrams at the discard port.
///
/// Unlike xarxa's synchronous egress, `send_slice` here only enqueues; `poll` is what
/// puts frames on the wire. So the meter counts bytes accepted by the socket, and a
/// datagram that never makes it out shows up as a `SendError::BufferFull` on a later
/// call rather than as a silent drop — which is why this loop stops offering when the
/// socket says it is full, instead of a fixed batch.
#[cfg(feature = "bench-udp-tx")]
fn bench(mut net: Net) -> ! {
    use smoltcp::socket::udp;

    let handle = net.sockets.add(udp_socket());
    net.sockets.get_mut::<udp::Socket>(handle).bind(LOCAL_PORT).unwrap();
    info!("{} {}: sending to {}:{}", STACK, DIR, SERVER_ADDR, UDP_UPLOAD_PORT);

    let buf = [0x5au8; UDP_PAYLOAD];
    let dst = IpEndpoint::from((SERVER_ADDR, UDP_UPLOAD_PORT));
    let mut meter = Meter::new();

    loop {
        net.poll();

        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        // A batch per poll, matching the xarxa side; `can_send` only reports whether
        // *some* room is left, so a full-size datagram can still be refused.
        for _ in 0..8 {
            match socket.send_slice(&buf, dst) {
                Ok(()) => meter.add(buf.len()),
                Err(udp::SendError::BufferFull) => break,
                Err(e) => defmt::panic!("{} {}: send error: {}", STACK, DIR, e),
            }
        }

        net.poll();
        meter.tick();
    }
}

/// Server -> board: subscribe to the flood, then count what arrives.
#[cfg(feature = "bench-udp-rx")]
fn bench(mut net: Net) -> ! {
    use embassy_time::{Duration, Instant as HwInstant};
    use smoltcp::socket::udp;

    let handle = net.sockets.add(udp_socket());
    net.sockets.get_mut::<udp::Socket>(handle).bind(LOCAL_PORT).unwrap();
    info!(
        "{} {}: subscribing to {}:{}",
        STACK, DIR, SERVER_ADDR, UDP_DOWNLOAD_PORT
    );

    // The subscription datagram's size is the size the server floods back with.
    let sub = [0u8; UDP_PAYLOAD];
    let dst = IpEndpoint::from((SERVER_ADDR, UDP_DOWNLOAD_PORT));
    let mut meter = Meter::new();
    let mut resubscribe_at = HwInstant::from_ticks(0);

    loop {
        net.poll();

        let socket = net.sockets.get_mut::<udp::Socket>(handle);

        // The subscription lapses after 2s on the server; renew well inside that.
        if HwInstant::now() >= resubscribe_at {
            resubscribe_at = HwInstant::now() + Duration::from_millis(500);
            socket.send_slice(&sub, dst).unwrap();
        }

        while let Ok((payload, _meta)) = socket.recv() {
            meter.add(payload.len());
        }

        meter.tick();
    }
}
