//! The Rust half of the lwIP port: the two platform hooks `port/arch/cc.h` declares.
//!
//! The other half of the port — the clock (`sys_now`) — is the application's, because
//! it is the application that owns the timer driver.

use core::ffi::{c_char, c_int, CStr};

/// `LWIP_PLATFORM_ASSERT`. Only reachable in a build with the `assert` feature, which
/// is what keeps lwIP's `LWIP_ASSERT` checks compiled in.
#[no_mangle]
extern "C" fn lwipx_assert_failed(msg: *const c_char, file: *const c_char, line: c_int) -> ! {
    // SAFETY: lwIP passes string literals from its own source.
    let (msg, file) = unsafe { (CStr::from_ptr(msg), CStr::from_ptr(file)) };
    defmt::panic!(
        "lwip: assertion \"{=str}\" failed at {=str}:{=i32}",
        msg.to_str().unwrap_or("?"),
        file.to_str().unwrap_or("?"),
        line,
    )
}

/// `LWIP_RAND`, used for TCP initial sequence numbers, ephemeral ports and IPv6 fragment
/// identifiers.
///
/// A plain xorshift seeded with the constant the other two stacks are seeded with, so
/// that no stack in the benchmark is luckier than another. Real firmware would seed this
/// from the RNG peripheral.
#[no_mangle]
extern "C" fn lwipx_rand() -> u32 {
    static mut STATE: u64 = 0x1234_5678_dead_beef;

    // SAFETY: lwIP is single-threaded here (NO_SYS=1, no interrupt calls into the
    // stack), so this is only ever reached from the poll loop.
    unsafe {
        let mut x = STATE;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        STATE = x;
        (x >> 32) as u32
    }
}
