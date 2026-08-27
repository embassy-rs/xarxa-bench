//! Throughput benchmarks against embassy's `perf-server`, on either stack.
//!
//! Three axes, all cargo features, exactly one of each:
//!
//! ```sh
//! cargo run --release --features stack-xarxa,bench-tcp-rx,ipv4
//! cargo run --release --features stack-smoltcp,bench-tcp-rx,ipv6
//! ```
//!
//! `tx`/`rx` are from the board's point of view, which is the opposite of the
//! perf-server's port names (it calls board-rx "download" and board-tx "upload").
//!
//! The `ipv4`/`ipv6` axis picks which of the server's two addresses the traffic goes to,
//! and with it the header sizes the MTU-derived constants below are computed from. It
//! does *not* change what the stacks are built with: both are compiled with IPv4 and
//! IPv6 support in either case, and the board configures an address of each family in
//! either case, so all that differs between an `ipv4` and an `ipv6` run is the traffic.
//!
//! Each benchmark prints one line per second — the rate over that second, the running
//! average, and the frames the device refused. After [`WARMUP_SECS`] the averages reset
//! and the measured window starts; [`MEASURE_SECS`] later the run prints its `result`
//! line and halts the core (which is how `bench.py` collects it through teleprobe).
//!
//! The UDP ports need a word of protocol. For `udp-rx` the server has no way to know
//! where to send, so the board *subscribes* by sending a datagram to port 4324 twice a
//! second; the server floods datagrams of that same size back, paced to 100 Mbit/s on
//! the wire, so the measurement is of the stack rather than of the switch's drop policy.
//! For `udp-tx` the board just fires datagrams at port 4325, and the server discards
//! them and reports what arrived — compare that against the board's own numbers to see
//! the loss.
//!
//! # Keeping the comparison honest
//!
//! Everything below the socket API is shared: one ethernet driver ([`crate::eth`]), the
//! same ring depths, the same MAC configuration, software checksums on both sides, and
//! the same spin-on-poll loop shape. None of the three has an allocator: every buffer any
//! of them uses is a static, sized by the constants below. What differs is what each
//! stack's own design makes differ — see the module docs of [`crate::eth`] for the buffer ownership split, and
//! the feature list in `Cargo.toml` for how smoltcp is cut down to the protocols xarxa
//! actually implements.
//!
//! Sizes are matched where both stacks have the same knob:
//!
//!  * TCP: [`TCP_BUF`] in the measured direction, [`TCP_BUF_IDLE`] in the other;
//!  * UDP: a [`UDP_RX_PACKETS`]-deep receive queue on xarxa and smoltcp (lwIP has no
//!    receive queue at all — see `bench_lwip.rs`);
//!  * [`IO_CHUNK`] bytes per socket call, [`UDP_PAYLOAD`]-byte datagrams;
//!  * Reno congestion control, which is the one lwIP has; smoltcp defaults to none and
//!    is set to Reno at runtime, xarxa selects it at compile time with its
//!    `tcp-reno` feature.

// This module is the shared config table plus the meter: with one benchmark built at a
// time, roughly half of the table is unused in any given build, by construction.
#![allow(dead_code)]

use core::sync::atomic::Ordering;

use defmt::info;
use embassy_time::Instant as HwInstant;

use crate::eth::stats;

#[cfg(not(any(feature = "stack-xarxa", feature = "stack-smoltcp", feature = "stack-lwip")))]
compile_error!("enable exactly one of the `stack-xarxa` / `stack-smoltcp` / `stack-lwip` cargo features");
#[cfg(any(
    all(feature = "stack-xarxa", feature = "stack-smoltcp"),
    all(feature = "stack-xarxa", feature = "stack-lwip"),
    all(feature = "stack-smoltcp", feature = "stack-lwip"),
))]
compile_error!("only one of `stack-xarxa` / `stack-smoltcp` / `stack-lwip` can be enabled at a time");

#[cfg(not(any(
    feature = "bench-tcp-tx",
    feature = "bench-tcp-rx",
    feature = "bench-udp-tx",
    feature = "bench-udp-rx"
)))]
compile_error!("enable exactly one of the `bench-{tcp,udp}-{tx,rx}` cargo features");

#[cfg(any(
    all(feature = "bench-tcp-tx", feature = "bench-tcp-rx"),
    all(feature = "bench-tcp-tx", feature = "bench-udp-tx"),
    all(feature = "bench-tcp-tx", feature = "bench-udp-rx"),
    all(feature = "bench-tcp-rx", feature = "bench-udp-tx"),
    all(feature = "bench-tcp-rx", feature = "bench-udp-rx"),
    all(feature = "bench-udp-tx", feature = "bench-udp-rx"),
))]
compile_error!("only one `bench-*` cargo feature can be enabled at a time");

#[cfg(not(any(feature = "ipv4", feature = "ipv6")))]
compile_error!("enable exactly one of the `ipv4` / `ipv6` cargo features");
#[cfg(all(feature = "ipv4", feature = "ipv6"))]
compile_error!("only one of `ipv4` / `ipv6` can be enabled at a time");

#[cfg(feature = "stack-xarxa")]
#[path = "bench_xarxa.rs"]
mod imp;
#[cfg(feature = "stack-smoltcp")]
#[path = "bench_smoltcp.rs"]
mod imp;
#[cfg(feature = "stack-lwip")]
#[path = "bench_lwip.rs"]
mod imp;

pub use imp::run;

/// The machine running `embassy/tests/perf-server`, on each family — the `ipv4`/`ipv6`
/// feature picks which one the benchmark talks to. Both are on-link for the board's own
/// addresses (see `main.rs`), so no gateway is involved either way.
pub const SERVER_V4: [u8; 4] = [192, 168, 2, 2];
pub const SERVER_V6: [u16; 8] = [0xfd00, 0, 0, 0, 0, 0, 0, 2];

/// perf-server ports, named as perf-server names them (from the *client's* point of
/// view, so "download" is the board receiving).
pub const TCP_DOWNLOAD_PORT: u16 = 4321;
pub const TCP_UPLOAD_PORT: u16 = 4322;
pub const UDP_DOWNLOAD_PORT: u16 = 4324;
pub const UDP_UPLOAD_PORT: u16 = 4325;

/// TCP window, in the direction being measured. At the ~1.7 ms round trip of this link
/// a 32 KiB window is good for ~150 Mbit/s, comfortably above the 100 Mbit/s line rate,
/// so the window is never what limits the result.
pub const TCP_BUF: usize = 32 * 1024;
/// The other direction carries nothing but ACKs.
pub const TCP_BUF_IDLE: usize = 4 * 1024;

/// The two capacities above, sorted into receive and transmit by the benchmark that is
/// built. Both stacks declare their ring buffers as statics of exactly these sizes.
pub const TCP_RX_CAP: usize = if cfg!(feature = "bench-tcp-rx") {
    TCP_BUF
} else {
    TCP_BUF_IDLE
};
pub const TCP_TX_CAP: usize = if cfg!(feature = "bench-tcp-tx") {
    TCP_BUF
} else {
    TCP_BUF_IDLE
};

/// The selected family's IP header size — 20 bytes for IPv4, 40 for IPv6. Everything
/// derived from the MTU below goes through this, so an `ipv6` run carries 20 bytes less
/// payload per frame than an `ipv4` one, exactly as it does on the wire.
pub const IP_HEADER: usize = if cfg!(feature = "ipv4") { 20 } else { 40 };

/// Largest TCP segment either stack will emit or receive: the MTU less the IP and TCP
/// headers.
const MSS: usize = 1500 - IP_HEADER - 20;

/// Descriptor ring depth, RX and TX alike.
///
/// Sized so that a whole [`TCP_BUF`] window of back-to-back full-size segments fits in
/// the ring: the peer may have that much in flight, and if it lands faster than the CPU
/// drains the ring, whatever does not fit is lost. That loss is what smoltcp's
/// `max_burst_size` window clamp exists to prevent, and sizing the ring properly removes
/// the need for it — at this depth the clamp no longer binds, so both stacks advertise
/// the window their buffer actually has, and neither can be limited by frames the driver
/// dropped rather than by the stack itself.
pub const ETH_RING: usize = TCP_BUF.div_ceil(MSS) + 1;
pub const ETH_TX: usize = ETH_RING;
pub const ETH_RX: usize = ETH_RING;

/// Payload handed to (or taken from) the socket in one call.
pub const IO_CHUNK: usize = 2048;
/// A full-MTU UDP payload: 1500 - the IP header - 8 (UDP), i.e. 1472 over IPv4 and 1452
/// over IPv6.
pub const UDP_PAYLOAD: usize = 1500 - IP_HEADER - 8;
/// Datagrams a UDP socket can hold before it starts dropping.
///
/// Deeper than the RX ring, so that a poll which picks up a full ring never overflows the
/// socket either, and a power of two because xarxa's depth is a cargo feature
/// (`udp-rx-queue-count-N`) that only takes those past 8 — this is the one number that
/// has to be spelled identically in `Cargo.toml` and here.
pub const UDP_RX_PACKETS: usize = 32;
const _: () = assert!(UDP_RX_PACKETS >= ETH_RX);

/// Datagrams smoltcp's UDP socket can hold on the way out. xarxa has no transmit queue
/// at all — its `send_slice` builds the frame and hands it to the device then and there —
/// so this only needs to be deep enough not to be the thing that throttles smoltcp.
pub const UDP_TX_PACKETS: usize = 8;

/// Local port both stacks bind to. Fixed rather than ephemeral because smoltcp has no
/// ephemeral port allocator (xarxa does, but using it would be one more difference).
pub const LOCAL_PORT: u16 = 49152;

/// Which stack is under test, for the log lines.
pub const STACK: &str = if cfg!(feature = "stack-xarxa") {
    "xarxa"
} else if cfg!(feature = "stack-smoltcp") {
    "smoltcp"
} else {
    "lwip"
};

/// Which IP version is under test, for the log lines.
pub const IPV: &str = if cfg!(feature = "ipv4") { "ipv4" } else { "ipv6" };

/// How long the benchmark ignores before it starts measuring: the connection is up by
/// then, the TCP window has opened, and the server's UDP flood has reached its pace.
pub const WARMUP_SECS: u64 = 5;
/// How long the measured window lasts. The single number the run reports is the average
/// over exactly this window; the per-second lines keep coming throughout both phases.
pub const MEASURE_SECS: u64 = 15;

/// Which direction is under test, for the log lines.
pub const DIR: &str = if cfg!(feature = "bench-tcp-tx") {
    "tcp tx"
} else if cfg!(feature = "bench-tcp-rx") {
    "tcp rx"
} else if cfg!(feature = "bench-udp-tx") {
    "udp tx"
} else {
    "udp rx"
};

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Accumulates bytes and prints a rate line once a second.
///
/// It also owns the run's lifetime: after [`WARMUP_SECS`] it resets its totals, and
/// [`MEASURE_SECS`] later it prints the `result` line the benchmark runner parses and
/// stops the board ([`stop`]). Every benchmark loop calls [`Meter::tick`] once per
/// iteration, so that is the one place either has to happen.
pub struct Meter {
    started: HwInstant,
    /// Start of the measured window: `None` while still warming up.
    measure_start: Option<HwInstant>,
    window_start: HwInstant,
    window_bytes: u64,
    total_bytes: u64,
    window_tx_full: u32,
}

impl Meter {
    pub fn new() -> Self {
        let now = HwInstant::now();
        Self {
            started: now,
            measure_start: None,
            window_start: now,
            window_bytes: 0,
            total_bytes: 0,
            window_tx_full: stats::TX_FULL.load(Ordering::Relaxed),
        }
    }

    pub fn add(&mut self, n: usize) {
        self.window_bytes += n as u64;
        self.total_bytes += n as u64;
    }

    /// Print a line if a second has gone by. Call this every loop iteration.
    pub fn tick(&mut self) {
        let now = HwInstant::now();
        let window = (now - self.window_start).as_micros();
        if window < 1_000_000 {
            return;
        }

        let tx_full = stats::TX_FULL.load(Ordering::Relaxed);
        let total_us = (now - self.measure_start.unwrap_or(self.started)).as_micros();

        info!(
            "{} {} {}: {} kbit/s (avg {} kbit/s, {} kB total, {} tx-full){}",
            STACK,
            IPV,
            DIR,
            kbits_per_sec(self.window_bytes, window),
            kbits_per_sec(self.total_bytes, total_us),
            self.total_bytes / 1024,
            tx_full.wrapping_sub(self.window_tx_full),
            if self.measured() { "" } else { " [warmup]" },
        );

        self.window_start = now;
        self.window_bytes = 0;
        self.window_tx_full = tx_full;

        if !self.measured() {
            // Still warming up: throw away everything counted so far and start the
            // measured window here.
            if (now - self.started).as_micros() >= WARMUP_SECS * 1_000_000 {
                self.measure_start = Some(now);
                self.total_bytes = 0;
            }
        } else if total_us >= MEASURE_SECS * 1_000_000 {
            // The one line the benchmark runner reads.
            info!(
                "result {} {} {}: {} kbit/s over {} s",
                STACK,
                IPV,
                DIR,
                kbits_per_sec(self.total_bytes, total_us),
                total_us / 1_000_000,
            );
            stop();
        }
    }

    /// Whether the measured window has started (i.e. the warmup is over).
    fn measured(&self) -> bool {
        self.measure_start.is_some()
    }
}

/// End the run: halt the core, which is how teleprobe sees a binary finish. The board
/// stays halted afterwards, so a transmitting benchmark stops loading the link instead
/// of running on until the next flash.
pub fn stop() -> ! {
    loop {
        cortex_m::asm::bkpt();
    }
}

fn kbits_per_sec(bytes: u64, micros: u64) -> u64 {
    if micros == 0 {
        return 0;
    }
    bytes * 8 * 1_000_000 / micros / 1000
}
