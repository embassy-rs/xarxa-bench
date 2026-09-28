//! Code-size probe: one firmware per (stack, feature set), compared by flash footprint.
//!
//! Unlike the throughput benchmarks, which always build the full protocol set, this
//! binary is built with *any* combination of the crate's protocol features (`ipv4`,
//! `udp`, `dns`, ... — see `Cargo.toml`, they mirror xarxa's own features and enable
//! each stack's closest equivalent). `codesize.py` drives it:
//!
//! ```sh
//! cargo build --release --features stack-xarxa,codesize,medium-ethernet,ipv4,udp
//! ../codesize.py medium-ethernet,ipv4,udp     # all three stacks, sizes compared
//! ```
//!
//! The program is not a benchmark and is never expected to do useful work; it exists so
//! the linker cannot discard the code under measurement. For every enabled feature it
//! makes plain, "dumb" use of the feature's API — a UDP echo port, a TCP echo port plus
//! an outgoing connection, raw sockets echoing packets back, a DNS query every few
//! seconds, DHCP/SLAAC address configuration, a multicast group join, and so on — so
//! that everything a real application would link in is linked in, and the stacks stay
//! comparable: each stack's module does the same jobs with its own API.

#![no_std]
#![no_main]

#[path = "../eth.rs"]
mod eth;

use defmt::info;
use defmt_rtt as _;
use embassy_stm32::time::Hertz;
use embassy_time::Duration;
use panic_probe as _;

teleprobe_meta::target!(b"nucleo-stm32f429zi");
teleprobe_meta::timeout!(60);

#[cfg(not(any(feature = "stack-xarxa", feature = "stack-smoltcp", feature = "stack-lwip")))]
compile_error!("enable exactly one of the `stack-xarxa` / `stack-smoltcp` / `stack-lwip` cargo features");
#[cfg(any(
    all(feature = "stack-xarxa", feature = "stack-smoltcp"),
    all(feature = "stack-xarxa", feature = "stack-lwip"),
    all(feature = "stack-smoltcp", feature = "stack-lwip"),
))]
compile_error!("only one of `stack-xarxa` / `stack-smoltcp` / `stack-lwip` can be enabled at a time");

// The board's one interface is Ethernet, so every measured configuration includes the
// Ethernet medium and at least one IP family — the smallest stack that can exist here.
#[cfg(not(feature = "medium-ethernet"))]
compile_error!("the code-size probe needs the `medium-ethernet` feature: the board's interface is Ethernet");
#[cfg(not(any(feature = "ipv4", feature = "ipv6")))]
compile_error!("enable at least one of the `ipv4` / `ipv6` cargo features");

#[cfg(feature = "stack-xarxa")]
#[path = "stack_xarxa.rs"]
mod imp;
#[cfg(feature = "stack-smoltcp")]
#[path = "stack_smoltcp.rs"]
mod imp;
#[cfg(feature = "stack-lwip")]
#[path = "stack_lwip.rs"]
mod imp;

/// Small rings: this binary measures flash, and the descriptor rings only cost RAM.
pub const ETH_TX: usize = 4;
pub const ETH_RX: usize = 4;

/// Locally administered MAC address, distinct from the throughput benchmark's.
pub const MAC_ADDR: [u8; 6] = [0x02, 0x00, 0x00, 0xde, 0xad, 0x02];

/// The board's addresses and the peer everything talks to, same network layout as the
/// throughput benchmarks (`src/main.rs`).
pub const IP_ADDR: [u8; 4] = [192, 168, 2, 251];
pub const IP_PREFIX_LEN: u8 = 24;
pub const GATEWAY: [u8; 4] = [192, 168, 2, 2];
pub const IPV6_ADDR: [u16; 8] = [0xfd00, 0, 0, 0, 0, 0, 0, 0x251];
pub const IPV6_GATEWAY: [u16; 8] = [0xfd00, 0, 0, 0, 0, 0, 0, 2];
pub const IPV6_PREFIX_LEN: u8 = 64;

/// The port both echo servers (UDP and TCP) listen on, and the port the outgoing TCP
/// connection targets on the peer.
pub const ECHO_PORT: u16 = 7;

/// The name the DNS client resolves, over and over. With `mdns` it is a `.local` name,
/// so the query takes the multicast path.
#[cfg(feature = "mdns")]
pub const DNS_NAME: &str = "codesize.local";
#[cfg(not(feature = "mdns"))]
pub const DNS_NAME: &str = "example.org";

/// PRNG seed. Constant, like the throughput benchmarks': this firmware's runs need no
/// unpredictability.
pub const SEED: u64 = 0x1234_5678_dead_beef;

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_stm32::init(rcc_config());
    info!("codesize probe on nucleo-stm32f429zi");

    static mut RINGS: eth::Rings<ETH_TX, ETH_RX> = eth::Rings::new();
    // SAFETY: taken once, here, and handed straight to the driver.
    let rings = unsafe { &mut *(&raw mut RINGS) };

    let mut device = eth::Ethernet::<ETH_TX, ETH_RX>::new(
        rings, p.ETH, p.PA1, p.PA7, p.PC4, p.PC5, p.PG13, p.PB13, p.PG11, p.PA2, p.PC1, MAC_ADDR,
    );
    device.wait_link_up(Duration::from_secs(10));

    imp::run(device)
}

/// Microseconds since boot, the one clock every stack is driven from.
pub fn now_micros() -> i64 {
    embassy_time::Instant::now().as_micros() as i64
}

/// Same clock tree as the throughput benchmarks: 180 MHz off the 8 MHz HSE bypass.
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
