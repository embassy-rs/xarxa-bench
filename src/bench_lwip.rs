//! The benchmarks, on lwIP.
//!
//! Structurally the same as `bench_xarxa.rs` and `bench_smoltcp.rs` — same loop shape,
//! same sizes, same chunk sizes — but written against lwIP's raw (callback) API, which
//! is the only one a `NO_SYS=1` build has: there is no socket to poll, so receiving is a
//! callback that counts what arrived and frees the pbuf, and the loop reads that counter
//! where the other two call `recv`.
//!
//! Three things follow from lwIP's design rather than from a choice made here, and are
//! worth knowing when reading the numbers:
//!
//!  * **There is no receive queue.** A UDP datagram is handed to the callback as it
//!    arrives, not stored; the counterpart of the other stacks' queue depth is the
//!    driver's receive ring, which is the same 24 frames deep for all three.
//!  * **Received data is never copied into a socket buffer.** TCP delivers the pbuf the
//!    frame arrived in, so this benchmark's one copy is the same `recv_slice`-shaped
//!    copy into the application's buffer that the other two make on top of theirs.
//!  * **Congestion control cannot be switched off.** lwIP always runs its slow start
//!    and congestion avoidance, and what it runs is RFC 5681 — Reno. That is why the
//!    other two are set to `CongestionControl::Reno` and built with only Reno
//!    compiled in: the alternative, turning congestion control off on the two stacks
//!    that can, would have compared them against an lwIP that cannot.

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use defmt::info;
use lwip_sys::*;

use super::*;
use crate::eth::Ethernet;
use crate::{GATEWAY, IP_ADDR, IP_PREFIX_LEN, IPV6_ADDR, IPV6_PREFIX_LEN, MAC_ADDR, now_micros};

/// The clock lwIP's timers run on. `lwip-sys` leaves it to the application, because the
/// application owns the timer driver.
#[unsafe(no_mangle)]
extern "C" fn sys_now() -> u32 {
    (now_micros() / 1000) as u32
}

// The sizes have to be the same on all three stacks, and lwIP's are compile-time
// constants in `lwip-sys/port/lwipopts.h` rather than arguments. Check them here, where
// both numbers are visible, rather than trusting two files to be edited together.
const _: () = {
    assert!(TCP_WND as usize == TCP_BUF);
    assert!(TCP_SND_BUF as usize == TCP_BUF);
    assert!(TCP_MSS as usize == 1500 - 20 - 20);
    // lwIP has no IPv6 prefix length: on-link is decided by comparing the first 64 bits.
    assert!(IPV6_PREFIX_LEN == 64);
};

/// The board's own IPv4 netmask, which lwIP takes instead of a prefix length.
const NETMASK: [u8; 4] = {
    assert!(IP_PREFIX_LEN > 0 && IP_PREFIX_LEN < 32);
    (u32::MAX << (32 - IP_PREFIX_LEN)).to_be_bytes()
};

/// An IPv6 address as lwIP wants it: 16 bytes, network order.
const fn v6_bytes(addr: [u16; 8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 8 {
        out[2 * i] = (addr[i] >> 8) as u8;
        out[2 * i + 1] = addr[i] as u8;
        i += 1;
    }
    out
}

/// The perf-server's address, in the family the `bench-ipv4`/`bench-ipv6` feature selected.
///
/// Built at runtime rather than declared as a constant because `ip_addr_t` is a tagged
/// union that only C can initialise readably (see `lwip-sys/port/lwip_port.c`).
fn server_addr() -> ip_addr_t {
    let mut addr = MaybeUninit::<ip_addr_t>::uninit();
    // SAFETY: both shims fully initialise what they are given.
    unsafe {
        #[cfg(feature = "bench-ipv4")]
        lwipx_ip4(addr.as_mut_ptr(), SERVER_V4.as_ptr());
        #[cfg(feature = "bench-ipv6")]
        lwipx_ip6(addr.as_mut_ptr(), v6_bytes(SERVER_V6).as_ptr());
        addr.assume_init()
    }
}

/// The stack: the driver and its netif, both pinned in statics (lwIP holds pointers to
/// them) and reached only through raw pointers, because lwIP calls back into the driver
/// from inside its own calls.
#[derive(Clone, Copy)]
struct Net {
    dev: *mut Ethernet<ETH_TX, ETH_RX>,
    netif: *mut netif,
}

impl Net {
    /// One turn of the crank: hand lwIP every frame that arrived, then run its timers.
    /// This is the equivalent of the other two stacks' `poll`.
    fn poll(self) {
        // SAFETY: both pointers are to statics that live for the whole run.
        unsafe {
            Ethernet::lwip_input(self.dev, self.netif);
            sys_check_timeouts();
        }
    }
}

/// Bring up the stack on the device, then hand over to the selected benchmark.
pub fn run(device: Ethernet<ETH_TX, ETH_RX>) -> ! {
    static mut DEVICE: MaybeUninit<Ethernet<ETH_TX, ETH_RX>> = MaybeUninit::uninit();
    static mut NETIF: MaybeUninit<netif> = MaybeUninit::uninit();

    // SAFETY: this runs once, and nothing else names either static.
    let net = unsafe {
        let dev: *mut Ethernet<ETH_TX, ETH_RX> = (*(&raw mut DEVICE)).write(device);
        let netif: *mut netif = (&raw mut NETIF).cast();
        ptr::write_bytes(netif, 0, 1);

        lwip_init();

        // The addresses are set below rather than passed here, so that the IPv4 and IPv6
        // sides are configured the same way.
        let added = netif_add(
            netif,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            dev.cast::<c_void>(),
            Some(Ethernet::<ETH_TX, ETH_RX>::netif_init),
            Some(ethernet_input),
        );
        defmt::assert!(!added.is_null(), "lwip: netif_add failed");
        lwipx_netif_set_ip4(netif, IP_ADDR.as_ptr(), NETMASK.as_ptr(), GATEWAY.as_ptr());
        let err = lwipx_netif_add_ip6(netif, v6_bytes(IPV6_ADDR).as_ptr());
        defmt::assert_eq!(err, ok(), "lwip: could not add the IPv6 address");

        netif_set_default(netif);
        netif_set_up(netif);
        netif_set_link_up(netif);

        Net { dev, netif }
    };

    info!(
        "{} {}: stack up on {=[u8]:02x}, talking to the server",
        STACK, IPV, MAC_ADDR
    );

    bench(net)
}

/// `ERR_OK` at the width lwIP's functions return it.
const fn ok() -> err_t {
    err_enum_t_ERR_OK as err_t
}

/// Bytes the receive callbacks have counted since the loop last looked. Receiving is a
/// callback on this API, so this is where the meter gets its numbers from.
static RX_BYTES: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// TCP
// ---------------------------------------------------------------------------

#[cfg(any(feature = "bench-tcp-tx", feature = "bench-tcp-rx"))]
mod tcp {
    use core::sync::atomic::AtomicBool;

    use super::*;

    /// Handshake and teardown, reported by callbacks into the loop that waits on them.
    pub static CONNECTED: AtomicBool = AtomicBool::new(false);
    pub static CLOSED: AtomicBool = AtomicBool::new(false);

    pub unsafe extern "C" fn connected(_arg: *mut c_void, _pcb: *mut tcp_pcb, _err: err_t) -> err_t {
        CONNECTED.store(true, Ordering::Relaxed);
        ok()
    }

    /// lwIP has already freed the pcb by the time this runs — there is nothing to abort.
    pub unsafe extern "C" fn error(_arg: *mut c_void, err: err_t) {
        defmt::warn!("lwip: tcp error {}", err);
        CLOSED.store(true, Ordering::Relaxed);
    }

    /// Received data, counted and copied out exactly as `recv_slice` does on the other
    /// two stacks — into one [`IO_CHUNK`]-sized application buffer, which is then thrown
    /// away.
    pub unsafe extern "C" fn recv(_arg: *mut c_void, pcb: *mut tcp_pcb, p: *mut pbuf, _err: err_t) -> err_t {
        static mut SCRATCH: [u8; IO_CHUNK] = [0; IO_CHUNK];

        // A null pbuf is the remote's FIN.
        if p.is_null() {
            CLOSED.store(true, Ordering::Relaxed);
            return ok();
        }

        // SAFETY: `p` is the chain lwIP delivered, ours until it is freed below.
        unsafe {
            let total = (*p).tot_len;
            let mut q = p;
            while !q.is_null() {
                let len = ((*q).len as usize).min(IO_CHUNK);
                ptr::copy_nonoverlapping((*q).payload.cast::<u8>(), (&raw mut SCRATCH).cast(), len);
                q = (*q).next;
            }
            // Read, so the window reopens; then let go of the frame, which is what hands
            // its buffer back to the receive ring.
            tcp_recved(pcb, total);
            pbuf_free(p);
            RX_BYTES.fetch_add(total as u32, Ordering::Relaxed);
        }
        ok()
    }

    /// Open the connection, retrying on the next local port if the server does not
    /// answer.
    ///
    /// See the same function in `bench_xarxa.rs` for why the port has to move.
    pub fn connect(net: Net, port: u16) -> *mut tcp_pcb {
        use embassy_time::{Duration, Instant as HwInstant};

        let server = server_addr();
        for local_port in LOCAL_PORT.. {
            CONNECTED.store(false, Ordering::Relaxed);
            CLOSED.store(false, Ordering::Relaxed);

            // SAFETY: single-threaded, and every pointer here is live for the call.
            unsafe {
                let pcb = tcp_new_ip_type(lwip_ip_addr_type_IPADDR_TYPE_ANY as u8);
                defmt::assert!(!pcb.is_null(), "lwip: out of tcp pcbs");
                tcp_err(pcb, Some(error));
                tcp_recv(pcb, Some(recv));
                defmt::assert_eq!(tcp_bind(pcb, lwipx_ip_any_type(), local_port), ok());

                info!("{} {}: connecting to :{} from :{}...", STACK, DIR, port, local_port);
                defmt::assert_eq!(tcp_connect(pcb, &server, port, Some(connected)), ok());

                let deadline = HwInstant::now() + Duration::from_secs(2);
                loop {
                    net.poll();
                    if CONNECTED.load(Ordering::Relaxed) {
                        info!("{} {}: connected", STACK, DIR);
                        return pcb;
                    }
                    // The error callback fires on a refused connection, and lwIP has
                    // freed the pcb by then.
                    if CLOSED.load(Ordering::Relaxed) {
                        break;
                    }
                    if HwInstant::now() > deadline {
                        tcp_abort(pcb);
                        net.poll();
                        break;
                    }
                }
            }
        }
        unreachable!()
    }
}

/// Board -> server: push into the send buffer as fast as it drains.
#[cfg(feature = "bench-tcp-tx")]
fn bench(net: Net) -> ! {
    let pcb = tcp::connect(net, TCP_UPLOAD_PORT);
    let buf = [0x5au8; IO_CHUNK];
    let mut meter = Meter::new();

    loop {
        net.poll();

        if tcp::CLOSED.load(Ordering::Relaxed) {
            defmt::panic!("{} {}: connection closed", STACK, DIR);
        }
        // SAFETY: the connection is up, so the pcb is live.
        unsafe {
            // `tcp_write` is all-or-nothing, so offer exactly what fits — the same bytes
            // `send_slice` would have accepted on the other two stacks.
            loop {
                let n = (lwipx_tcp_sndbuf(pcb) as usize).min(IO_CHUNK);
                if n == 0 {
                    break;
                }
                match tcp_write(pcb, buf.as_ptr().cast(), n as u16, TCP_WRITE_FLAG_COPY as u8) {
                    e if e == ok() => meter.add(n),
                    // Out of segments or out of heap: it will fit again once some of what
                    // is in flight is acknowledged.
                    e if e == err_enum_t_ERR_MEM as err_t => break,
                    e => defmt::panic!("{} {}: send error: {}", STACK, DIR, e),
                }
            }
            // The data just enqueued goes out here.
            tcp_output(pcb);
        }

        net.poll();
        meter.tick();
    }
}

/// Server -> board: count what the receive callback delivers.
#[cfg(feature = "bench-tcp-rx")]
fn bench(net: Net) -> ! {
    let _pcb = tcp::connect(net, TCP_DOWNLOAD_PORT);
    let mut meter = Meter::new();

    loop {
        net.poll();

        if tcp::CLOSED.load(Ordering::Relaxed) {
            defmt::panic!("{} {}: connection closed", STACK, DIR);
        }
        meter.add(RX_BYTES.swap(0, Ordering::Relaxed) as usize);
        meter.tick();
    }
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

/// Datagrams arrive here rather than in a queue: lwIP's raw API has no receive buffer,
/// so this counts the payload and hands the frame straight back to the receive ring.
#[cfg(any(feature = "bench-udp-tx", feature = "bench-udp-rx"))]
unsafe extern "C" fn udp_recv_cb(
    _arg: *mut c_void,
    _pcb: *mut udp_pcb,
    p: *mut pbuf,
    _addr: *const ip_addr_t,
    _port: u16,
) {
    // SAFETY: the datagram is ours to free.
    unsafe {
        RX_BYTES.fetch_add((*p).tot_len as u32, Ordering::Relaxed);
        pbuf_free(p);
    }
}

/// A UDP pcb bound to the benchmark's local port.
#[cfg(any(feature = "bench-udp-tx", feature = "bench-udp-rx"))]
fn udp_socket() -> *mut udp_pcb {
    // SAFETY: single-threaded setup, before any traffic.
    unsafe {
        let pcb = udp_new_ip_type(lwip_ip_addr_type_IPADDR_TYPE_ANY as u8);
        defmt::assert!(!pcb.is_null(), "lwip: out of udp pcbs");
        defmt::assert_eq!(udp_bind(pcb, lwipx_ip_any_type(), LOCAL_PORT), ok());
        udp_recv(pcb, Some(udp_recv_cb), ptr::null_mut());
        pcb
    }
}

/// Board -> server: fire full-MTU datagrams at the discard port.
///
/// Like xarxa's, this is synchronous egress: `udp_sendto` builds the datagram and hands
/// it to the driver then and there. A datagram the device has no room for is reported
/// back as an error (and counted in `tx-full`), so the metered rate is what reached the
/// hardware.
#[cfg(feature = "bench-udp-tx")]
fn bench(net: Net) -> ! {
    let pcb = udp_socket();
    let server = server_addr();
    info!("{} {}: sending to :{}", STACK, DIR, UDP_UPLOAD_PORT);

    let buf = [0x5au8; UDP_PAYLOAD];
    let mut meter = Meter::new();

    loop {
        net.poll();

        // A batch per poll, matching the other two stacks.
        for _ in 0..8 {
            // SAFETY: the pcb is live, and the pbuf is freed on every path below.
            unsafe {
                let p = pbuf_alloc(pbuf_layer_PBUF_TRANSPORT, UDP_PAYLOAD as u16, pbuf_type_PBUF_RAM);
                if p.is_null() {
                    // lwIP's heap is momentarily full of unsent datagrams.
                    break;
                }
                ptr::copy_nonoverlapping(buf.as_ptr(), (*p).payload.cast::<u8>(), UDP_PAYLOAD);
                let err = udp_sendto(pcb, p, &server, UDP_UPLOAD_PORT);
                pbuf_free(p);
                if err == ok() {
                    meter.add(UDP_PAYLOAD);
                }
            }
        }

        meter.tick();
    }
}

/// Server -> board: subscribe to the flood, then count what arrives.
#[cfg(feature = "bench-udp-rx")]
fn bench(net: Net) -> ! {
    use embassy_time::{Duration, Instant as HwInstant};

    let pcb = udp_socket();
    let server = server_addr();
    info!("{} {}: subscribing to :{}", STACK, DIR, UDP_DOWNLOAD_PORT);

    // The subscription datagram's size is the size the server floods back with.
    let sub = [0u8; UDP_PAYLOAD];
    let mut meter = Meter::new();
    let mut resubscribe_at = HwInstant::from_ticks(0);

    loop {
        net.poll();

        // The subscription lapses after 2s on the server; renew well inside that.
        if HwInstant::now() >= resubscribe_at {
            resubscribe_at = HwInstant::now() + Duration::from_millis(500);
            // SAFETY: the pcb is live, and the pbuf is freed on every path below.
            unsafe {
                let p = pbuf_alloc(pbuf_layer_PBUF_TRANSPORT, UDP_PAYLOAD as u16, pbuf_type_PBUF_RAM);
                defmt::assert!(!p.is_null(), "lwip: no room for the subscription datagram");
                ptr::copy_nonoverlapping(sub.as_ptr(), (*p).payload.cast::<u8>(), UDP_PAYLOAD);
                defmt::assert_eq!(udp_sendto(pcb, p, &server, UDP_DOWNLOAD_PORT), ok());
                pbuf_free(p);
            }
        }

        meter.add(RX_BYTES.swap(0, Ordering::Relaxed) as usize);
        meter.tick();
    }
}
