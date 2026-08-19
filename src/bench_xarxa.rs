//! The benchmarks, on xarxa.

use alloc::boxed::Box;

use defmt::info;
use xarxa::stack::Stack;
use xarxa::time::Instant;
use xarxa::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

use super::*;
use crate::eth::Ethernet;
use crate::{GATEWAY, IP_ADDR, IP_PREFIX_LEN, IPV6_ADDR, IPV6_PREFIX_LEN, MAC_ADDR, now_micros};

fn now() -> Instant {
    Instant::from_micros(now_micros())
}

/// Bring up the stack on the device, then hand over to the selected benchmark.
pub fn run(device: Ethernet<ETH_TX, ETH_RX>) -> ! {
    // Benchmarks don't care about ISN/port unpredictability; a constant seed
    // keeps runs reproducible. Real firmware should seed from the RNG peripheral.
    let mut stack = Stack::new(0x1234_5678_dead_beef);
    let iface = stack.add_iface(Box::new(device), HardwareAddress::Ethernet(EthernetAddress(MAC_ADDR)));
    stack.iface(iface).set_ip_addrs([
        IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(IP_ADDR)), IP_PREFIX_LEN),
        IpCidr::new(
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
        ),
    ]);
    stack
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::from(GATEWAY), iface);
    info!("{} {}: stack up, talking to {}", STACK, IPV, SERVER_ADDR);

    bench(&mut stack)
}

/// The perf-server's address, in the family the `ipv4`/`ipv6` feature selected.
#[cfg(feature = "ipv4")]
const SERVER_ADDR: IpAddress = IpAddress::v4(SERVER_V4[0], SERVER_V4[1], SERVER_V4[2], SERVER_V4[3]);
#[cfg(feature = "ipv6")]
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

// ---------------------------------------------------------------------------
// TCP
// ---------------------------------------------------------------------------

/// Open the connection, retrying on the next local port if the server does not answer.
///
/// The retry is not paranoia: a run ends by halting the core, never by closing its
/// socket, and the board is then reflashed — so a re-run of the same benchmark
/// would reconnect with an identical 4-tuple while the server still has the old
/// connection open — and Linux answers that with a challenge ACK, not a SYN|ACK. Walking
/// the local port forward sidesteps the stale tuple.
#[cfg(any(feature = "bench-tcp-tx", feature = "bench-tcp-rx"))]
fn connect(stack: &mut Stack, port: u16, rx_cap: usize, tx_cap: usize) -> xarxa::TcpHandle {
    use embassy_time::{Duration, Instant as HwInstant};
    use xarxa::tcp::State;

    for local_port in LOCAL_PORT.. {
        let handle = stack.add_tcp_socket(rx_cap, tx_cap);
        info!(
            "{} {}: connecting to {}:{} from :{}...",
            STACK, DIR, SERVER_ADDR, port, local_port
        );
        stack
            .tcp_socket(handle)
            .connect(IpEndpoint::from((SERVER_ADDR, port)), local_port)
            .unwrap();

        let deadline = HwInstant::now() + Duration::from_secs(2);
        loop {
            stack.poll(now());
            match stack.tcp_socket(handle).state() {
                State::Established => {
                    info!("{} {}: connected", STACK, DIR);
                    return handle;
                }
                State::Closed => break,
                _ if HwInstant::now() > deadline => break,
                _ => {}
            }
        }

        stack.tcp_socket(handle).abort();
        stack.poll(now());
        stack.remove_tcp_socket(handle);
    }
    unreachable!()
}

/// Board -> server: push into the send ring as fast as it drains.
#[cfg(feature = "bench-tcp-tx")]
fn bench(stack: &mut Stack) -> ! {
    let handle = connect(stack, TCP_UPLOAD_PORT, TCP_BUF_IDLE, TCP_BUF);
    let buf = [0x5au8; IO_CHUNK];
    let mut meter = Meter::new();

    loop {
        stack.poll(now());

        let mut socket = stack.tcp_socket(handle);
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
        stack.poll(now());
        meter.tick();
    }
}

/// Server -> board: drain the receive ring as fast as it fills.
#[cfg(feature = "bench-tcp-rx")]
fn bench(stack: &mut Stack) -> ! {
    let handle = connect(stack, TCP_DOWNLOAD_PORT, TCP_BUF, TCP_BUF_IDLE);
    let mut buf = [0u8; IO_CHUNK];
    let mut meter = Meter::new();

    loop {
        stack.poll(now());

        let mut socket = stack.tcp_socket(handle);
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
        stack.poll(now());
        meter.tick();
    }
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

/// Board -> server: fire full-MTU datagrams at the discard port.
///
/// `send_slice` never blocks — egress is synchronous, and a frame the device has no
/// room for is dropped — so the metered rate here is what the *stack* produced. The
/// `tx-full` count on each line is how much of that the device refused; what actually
/// reached the wire is the difference, and the perf-server prints what it received.
#[cfg(feature = "bench-udp-tx")]
fn bench(stack: &mut Stack) -> ! {
    let handle = stack.add_udp_socket();
    stack
        .udp_socket(handle)
        .bind(LOCAL_PORT, IpEndpoint::from((SERVER_ADDR, UDP_UPLOAD_PORT)))
        .unwrap();
    info!("{} {}: sending to {}:{}", STACK, DIR, SERVER_ADDR, UDP_UPLOAD_PORT);

    let buf = [0x5au8; UDP_PAYLOAD];
    let mut meter = Meter::new();

    loop {
        stack.poll(now());

        let mut socket = stack.udp_socket(handle);
        // A batch per poll, so ingress (ARP, ICMP) still gets serviced promptly.
        for _ in 0..8 {
            match socket.send_slice(&buf, IpEndpoint::UNSPECIFIED) {
                Ok(()) => meter.add(buf.len()),
                Err(e) => defmt::panic!("{} {}: send error: {}", STACK, DIR, e),
            }
        }

        meter.tick();
    }
}

/// Server -> board: subscribe to the flood, then count what arrives.
#[cfg(feature = "bench-udp-rx")]
fn bench(stack: &mut Stack) -> ! {
    use embassy_time::{Duration, Instant as HwInstant};

    let handle = stack.add_udp_socket();
    stack
        .udp_socket(handle)
        .bind(LOCAL_PORT, IpEndpoint::from((SERVER_ADDR, UDP_DOWNLOAD_PORT)))
        .unwrap();
    info!(
        "{} {}: subscribing to {}:{}",
        STACK, DIR, SERVER_ADDR, UDP_DOWNLOAD_PORT
    );

    // The subscription datagram's size is the size the server floods back with.
    let sub = [0u8; UDP_PAYLOAD];
    let mut meter = Meter::new();
    let mut resubscribe_at = HwInstant::from_ticks(0);

    loop {
        stack.poll(now());

        let mut socket = stack.udp_socket(handle);

        // The subscription lapses after 2s on the server; renew well inside that.
        if HwInstant::now() >= resubscribe_at {
            resubscribe_at = HwInstant::now() + Duration::from_millis(500);
            socket.send_slice(&sub, IpEndpoint::UNSPECIFIED).unwrap();
        }

        while let Ok(packet) = socket.recv() {
            meter.add(packet.payload().len());
        }

        meter.tick();
    }
}
