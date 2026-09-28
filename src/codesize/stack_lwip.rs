//! Dumb use of every enabled lwIP feature, mirroring `stack_xarxa.rs` job for job
//! through lwIP's raw (callback) API: the same echo ports, the same outgoing
//! connection, the same periodic DNS query, the same address configuration.

// Which of the items below exist depends on the feature combination; keeping every
// cfg-permutation warning-clean is not worth it in a probe.
#![allow(unused)]

use core::ffi::{c_char, c_void};
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use defmt::info;
use embassy_time::{Duration, Instant as HwInstant};
use lwip_sys::*;

use super::*;
use crate::eth::Ethernet;

/// The clock lwIP's timers run on.
#[unsafe(no_mangle)]
extern "C" fn sys_now() -> u32 {
    (now_micros() / 1000) as u32
}

/// `ERR_OK` at the width lwIP's functions return it.
const fn ok() -> err_t {
    err_enum_t_ERR_OK as err_t
}

/// The board's own IPv4 netmask, which lwIP takes instead of a prefix length.
#[cfg(feature = "ipv4")]
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

/// An IPv6 address as `ip6_addr_t.addr` wants it: four `u32`s holding the bytes in
/// network order.
#[cfg(feature = "ipv6")]
const fn v6_words(addr: [u16; 8]) -> [u32; 4] {
    let b = v6_bytes(addr);
    let mut out = [0u32; 4];
    let mut i = 0;
    while i < 4 {
        out[i] = u32::from_ne_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        i += 1;
    }
    out
}

/// The peer for outgoing traffic (TCP connects, DNS queries), in the first enabled
/// family.
fn server_addr() -> ip_addr_t {
    let mut addr = MaybeUninit::<ip_addr_t>::uninit();
    // SAFETY: both shims fully initialise what they are given.
    unsafe {
        #[cfg(feature = "ipv4")]
        lwipx_ip4(addr.as_mut_ptr(), GATEWAY.as_ptr());
        #[cfg(all(feature = "ipv6", not(feature = "ipv4")))]
        lwipx_ip6(addr.as_mut_ptr(), v6_bytes(IPV6_GATEWAY).as_ptr());
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
    fn poll(self) {
        // SAFETY: both pointers are to statics that live for the whole run.
        unsafe {
            Ethernet::lwip_input(self.dev, self.netif);
            sys_check_timeouts();
        }
    }
}

pub fn run(device: Ethernet<ETH_TX, ETH_RX>) -> ! {
    static mut DEVICE: MaybeUninit<Ethernet<ETH_TX, ETH_RX>> = MaybeUninit::uninit();
    static mut NETIF: MaybeUninit<netif> = MaybeUninit::uninit();

    // SAFETY: this runs once, and nothing else names either static; every lwIP call
    // below is single-threaded setup.
    let net = unsafe {
        let dev: *mut Ethernet<ETH_TX, ETH_RX> = (*(&raw mut DEVICE)).write(device);
        let netif: *mut netif = (&raw mut NETIF).cast();
        ptr::write_bytes(netif, 0, 1);

        lwip_init();

        // `netif_add`'s signature carries the IPv4 address parameters only when IPv4
        // is compiled in.
        #[cfg(feature = "ipv4")]
        let added = netif_add(
            netif,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            dev.cast::<c_void>(),
            Some(Ethernet::<ETH_TX, ETH_RX>::netif_init),
            Some(ethernet_input),
        );
        #[cfg(not(feature = "ipv4"))]
        let added = netif_add(
            netif,
            dev.cast::<c_void>(),
            Some(Ethernet::<ETH_TX, ETH_RX>::netif_init),
            Some(ethernet_input),
        );
        defmt::assert!(!added.is_null(), "lwip: netif_add failed");

        #[cfg(feature = "ipv4")]
        lwipx_netif_set_ip4(netif, IP_ADDR.as_ptr(), NETMASK.as_ptr(), GATEWAY.as_ptr());
        #[cfg(feature = "ipv6")]
        defmt::assert_eq!(
            lwipx_netif_add_ip6(netif, v6_bytes(IPV6_ADDR).as_ptr()),
            ok(),
            "lwip: could not add the IPv6 address"
        );

        #[cfg(feature = "hostname")]
        {
            (*netif).hostname = c"codesize".as_ptr();
        }
        #[cfg(feature = "slaac")]
        {
            netif_create_ip6_linklocal_address(netif, 1);
            (*netif).ip6_autoconfig_enabled = 1;
        }

        netif_set_default(netif);
        netif_set_up(netif);
        netif_set_link_up(netif);

        #[cfg(feature = "dhcpv4")]
        defmt::assert_eq!(dhcp_start(netif), ok(), "lwip: dhcp_start failed");

        #[cfg(feature = "multicast")]
        {
            #[cfg(feature = "ipv4")]
            {
                igmp_start(netif);
                let group = ip4_addr_t {
                    addr: u32::from_ne_bytes([224, 0, 0, 251]),
                };
                let _ = igmp_joingroup_netif(netif, &group);
            }
            #[cfg(feature = "ipv6")]
            {
                let mut group: ip6_addr_t = core::mem::zeroed();
                group.addr = v6_words([0xff02, 0, 0, 0, 0, 0, 0, 0xfb]);
                let _ = mld6_joingroup_netif(netif, &group);
            }
        }

        #[cfg(feature = "dns")]
        {
            let server = server_addr();
            dns_setserver(0, &server);
        }

        // UDP echo server.
        #[cfg(feature = "udp")]
        {
            let pcb = udp_new_ip_type(lwip_ip_addr_type_IPADDR_TYPE_ANY as u8);
            defmt::assert!(!pcb.is_null(), "lwip: out of udp pcbs");
            defmt::assert_eq!(udp_bind(pcb, lwipx_ip_any_type(), ECHO_PORT), ok());
            udp_recv(pcb, Some(udp_echo), ptr::null_mut());
        }

        // Raw socket on ICMP, echoing what it receives.
        #[cfg(feature = "raw-ip")]
        {
            let proto: u8 = if cfg!(feature = "ipv4") { 1 } else { 58 };
            let pcb = raw_new_ip_type(lwip_ip_addr_type_IPADDR_TYPE_ANY as u8, proto);
            defmt::assert!(!pcb.is_null(), "lwip: out of raw pcbs");
            raw_recv(pcb, Some(raw_echo), ptr::null_mut());
        }

        // TCP echo server.
        #[cfg(feature = "tcp-listener")]
        {
            let pcb = tcp_new_ip_type(lwip_ip_addr_type_IPADDR_TYPE_ANY as u8);
            defmt::assert!(!pcb.is_null(), "lwip: out of tcp pcbs");
            defmt::assert_eq!(tcp_bind(pcb, lwipx_ip_any_type(), ECHO_PORT), ok());
            let lpcb = tcp_listen_with_backlog(pcb, 4);
            defmt::assert!(!lpcb.is_null(), "lwip: tcp_listen failed");
            tcp_accept(lpcb, Some(tcp_accepted));
        }

        Net { dev, netif }
    };

    info!("codesize lwip: stack up");

    let mut next_tick = HwInstant::from_ticks(0);
    loop {
        net.poll();

        // A slow tick for the periodic actions (reconnects, DNS queries).
        if HwInstant::now() >= next_tick {
            next_tick = HwInstant::now() + Duration::from_secs(5);

            #[cfg(feature = "tcp")]
            if CLIENT_DEAD.load(Ordering::Relaxed) {
                // SAFETY: single-threaded, between polls.
                unsafe { client_connect() };
            }

            #[cfg(feature = "dns")]
            {
                #[cfg(feature = "mdns")]
                const NAME: &core::ffi::CStr = c"codesize.local";
                #[cfg(not(feature = "mdns"))]
                const NAME: &core::ffi::CStr = c"example.org";
                let mut resolved = MaybeUninit::<ip_addr_t>::uninit();
                // SAFETY: single-threaded, between polls; the callback ignores its
                // arguments, so nothing has to outlive the call.
                let _ = unsafe {
                    dns_gethostbyname(NAME.as_ptr(), resolved.as_mut_ptr(), Some(dns_found), ptr::null_mut())
                };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

/// Echo a datagram straight back to its sender.
#[cfg(feature = "udp")]
unsafe extern "C" fn udp_echo(_arg: *mut c_void, pcb: *mut udp_pcb, p: *mut pbuf, addr: *const ip_addr_t, port: u16) {
    // SAFETY: the datagram is ours to send on and free.
    unsafe {
        let _ = udp_sendto(pcb, p, addr, port);
        pbuf_free(p);
    }
}

/// Echo a raw packet straight back to its sender.
#[cfg(feature = "raw-ip")]
unsafe extern "C" fn raw_echo(_arg: *mut c_void, pcb: *mut raw_pcb, p: *mut pbuf, addr: *const ip_addr_t) -> u8 {
    // SAFETY: returning 1 makes the packet ours to send on and free.
    unsafe {
        let _ = raw_sendto(pcb, p, addr);
        pbuf_free(p);
    }
    1
}

/// Whether the outgoing TCP connection needs (re)creating.
#[cfg(feature = "tcp")]
static CLIENT_DEAD: AtomicBool = AtomicBool::new(true);

/// Move every received byte back into the send buffer. Shared by the client and the
/// accepted server connections; a null pbuf is the remote's FIN.
#[cfg(feature = "tcp")]
unsafe extern "C" fn tcp_echo_recv(arg: *mut c_void, pcb: *mut tcp_pcb, p: *mut pbuf, _err: err_t) -> err_t {
    // SAFETY: `p` is the chain lwIP delivered, ours until it is freed below.
    unsafe {
        if p.is_null() {
            if !arg.is_null() {
                // The client marks itself dead so the tick reconnects.
                CLIENT_DEAD.store(true, Ordering::Relaxed);
            }
            let _ = tcp_close(pcb);
            return ok();
        }
        let mut q = p;
        while !q.is_null() {
            let _ = tcp_write(pcb, (*q).payload, (*q).len, TCP_WRITE_FLAG_COPY as u8);
            q = (*q).next;
        }
        tcp_recved(pcb, (*p).tot_len);
        pbuf_free(p);
        let _ = tcp_output(pcb);
    }
    ok()
}

/// The handshake completed: send a greeting.
#[cfg(feature = "tcp")]
unsafe extern "C" fn tcp_connected(_arg: *mut c_void, pcb: *mut tcp_pcb, _err: err_t) -> err_t {
    // SAFETY: the pcb is live inside this callback.
    unsafe {
        let _ = tcp_write(pcb, c"hello".as_ptr().cast(), 5, TCP_WRITE_FLAG_COPY as u8);
        let _ = tcp_output(pcb);
    }
    ok()
}

/// The connection died; lwIP has already freed the pcb.
#[cfg(feature = "tcp")]
unsafe extern "C" fn tcp_client_err(_arg: *mut c_void, _err: err_t) {
    CLIENT_DEAD.store(true, Ordering::Relaxed);
}

/// Open the outgoing connection.
///
/// # Safety
/// Single-threaded, between polls.
#[cfg(feature = "tcp")]
unsafe fn client_connect() {
    unsafe {
        let pcb = tcp_new_ip_type(lwip_ip_addr_type_IPADDR_TYPE_ANY as u8);
        if pcb.is_null() {
            return;
        }
        // A non-null arg is how `tcp_echo_recv` tells the client's pcb apart.
        tcp_arg(pcb, pcb.cast());
        tcp_err(pcb, Some(tcp_client_err));
        tcp_recv(pcb, Some(tcp_echo_recv));
        let server = server_addr();
        if tcp_connect(pcb, &server, ECHO_PORT, Some(tcp_connected)) != ok() {
            tcp_abort(pcb);
            return;
        }
        CLIENT_DEAD.store(false, Ordering::Relaxed);
    }
}

/// An incoming connection was accepted: echo everything it sends.
#[cfg(feature = "tcp-listener")]
unsafe extern "C" fn tcp_accepted(_arg: *mut c_void, newpcb: *mut tcp_pcb, _err: err_t) -> err_t {
    // SAFETY: the new pcb is live inside this callback. A null arg marks it as a
    // server-side connection for `tcp_echo_recv`.
    unsafe {
        tcp_arg(newpcb, ptr::null_mut());
        tcp_recv(newpcb, Some(tcp_echo_recv));
    }
    ok()
}

/// A DNS query finished. The probe only cares that the resolver ran.
#[cfg(feature = "dns")]
unsafe extern "C" fn dns_found(_name: *const c_char, _addr: *const ip_addr_t, _arg: *mut c_void) {}
