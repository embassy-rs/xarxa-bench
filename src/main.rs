//! Network stack throughput benchmarks on a Nucleo-F429ZI.
//!
//! `main` brings up the clocks and the ethernet driver, and then hands the device to
//! [`bench`], which builds the stack the cargo features asked for and runs the benchmark
//! the cargo features asked for. Two axes, one feature each, no defaults:
//!
//! ```sh
//! cargo run --release --features stack-xarxa,bench-tcp-rx
//! cargo run --release --features stack-smoltcp,bench-udp-tx
//! ```
//!
//! The other end is embassy's `perf-server` (`embassy/tests/perf-server`, deployed with
//! its `deploy.sh`) on the machine at the other end of the cable.
//!
//! There is no async and no executor — this is a plain `cortex-m-rt` entry point that
//! spins on the stack's poll function, for either stack. There is no allocator either:
//! see [`MEMORY`] below.

#![no_std]
#![no_main]

mod bench;
mod eth;

use defmt::info;
use defmt_rtt as _;
use embassy_stm32::time::Hertz;
use embassy_time::Duration;
use panic_probe as _;

use crate::bench::{ETH_RX, ETH_TX};

// The board is flashed and run through teleprobe (`bench.py`), which reads both of these
// out of the ELF: which board to run on, and how long to let it run before giving up.
// The budget is the benchmark's own warmup + measured window (20 s, `bench.rs`), plus
// room for the link to come up and the connection to be established.
teleprobe_meta::target!(b"nucleo-stm32f429zi");
teleprobe_meta::timeout!(60);

/// The F429ZI has 192 KB of DMA-reachable SRAM. None of the three builds has an
/// allocator: every buffer is a static, and each stack divides the SRAM its own way.
///
///  * **xarxa** owns its buffers, and they live in its static packet pool (64 x ~1.5 KB
///    = 97 KB of `.bss`, `packet-buf-count-64` in Cargo.toml): the RX ring's buffers,
///    every frame in flight on TX, and the queued datagrams. A whole window in each
///    direction is 2 x 24 = 48 of them, so 64 leaves room to spare. On top of that, a
///    TCP benchmark lends the stack its two ring buffers (36 KB, `bench_xarxa.rs`).
///  * **smoltcp** borrows, so its frame buffers are static (`RINGS`, ~73 KB of `.bss`)
///    and its socket buffers are statics of their own — 36 KB for TCP, ~59 KB for the
///    UDP queues (`bench_smoltcp.rs`).
///  * **lwIP** has its own heap (`MEM_SIZE`, 64 KB of `.bss`) which every outgoing
///    packet comes out of, and receives into the ring's own static buffers (~36 KB).
///
/// Either way the total memory devoted to networking is about the same; what differs is
/// which side of the socket API it sits on.
///
/// (This item exists to hang the documentation on; nothing reads it.)
pub const MEMORY: () = ();

/// Locally administered MAC address (the `0x02` sets the "locally administered" bit).
pub const MAC_ADDR: [u8; 6] = [0x02, 0x00, 0x00, 0xde, 0xad, 0x01];

/// Our address on the link. Deliberately high in the subnet, to stay clear of whatever
/// else lives on it.
pub const IP_ADDR: [u8; 4] = [192, 168, 2, 250];
pub const IP_PREFIX_LEN: u8 = 24;
/// The machine on the other end of the switch.
pub const GATEWAY: [u8; 4] = [192, 168, 2, 2];

/// Our IPv6 address on the same link, out of the ULA prefix `fd00::/64` configured on
/// the server's interface. Both families are always configured, whichever one the
/// `ipv4`/`ipv6` feature points the benchmark's traffic at.
///
/// The server (`fd00::2`) is inside this prefix, so it is on-link and no IPv6 route is
/// needed — the mirror of the IPv4 side, where the default route to the gateway exists
/// but is never used because that address is on-link too.
pub const IPV6_ADDR: [u16; 8] = [0xfd00, 0, 0, 0, 0, 0, 0, 0x250];
pub const IPV6_PREFIX_LEN: u8 = 64;

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_stm32::init(rcc_config());
    info!("xarxa/smoltcp benchmarks on nucleo-stm32f429zi");

    // Descriptors — and, on the smoltcp build, the frame buffers — live in `.bss`, the
    // way embassy-stm32's own driver and any normal smoltcp application do it.
    static mut RINGS: eth::Rings<ETH_TX, ETH_RX> = eth::Rings::new();
    // SAFETY: taken once, here, and handed straight to the driver.
    let rings = unsafe { &mut *(&raw mut RINGS) };

    let mut device = eth::Ethernet::<ETH_TX, ETH_RX>::new(
        rings, p.ETH, p.PA1, p.PA7, p.PC4, p.PC5, p.PG13, p.PB13, p.PG11, p.PA2, p.PC1, MAC_ADDR,
    );

    // The benchmarks connect straight away, so wait for the link first — otherwise the
    // first SYN goes out into a cable that is not up yet.
    device.wait_link_up(Duration::from_secs(10));

    bench::run(device)
}

/// Microseconds since boot, the one clock both stacks are driven from.
pub fn now_micros() -> i64 {
    embassy_time::Instant::now().as_micros() as i64
}

/// The fastest the F429 goes: 180 MHz off the 8 MHz HSE bypass (the ST-LINK's MCO).
///
/// Copied from `embassy/tests/stm32/src/common.rs`.
fn rcc_config() -> embassy_stm32::Config {
    use embassy_stm32::rcc::*;

    let mut config = embassy_stm32::Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz(8_000_000),
        mode: HseMode::Bypass,
    });
    config.rcc.pll_src = PllSource::Hse;
    config.rcc.pll = Some(Pll {
        prediv: PllPreDiv::Div4,
        mul: PllMul::Mul180,
        divp: Some(PllPDiv::Div2), // 8mhz / 4 * 180 / 2 = 180Mhz.
        divq: Some(PllQDiv::Div2),
        divr: None,
    });
    config.rcc.ahb_pre = AHBPrescaler::Div1;
    config.rcc.apb1_pre = APBPrescaler::Div4;
    config.rcc.apb2_pre = APBPrescaler::Div2;
    config.rcc.sys = Sysclk::Pll1P;
    config
}
