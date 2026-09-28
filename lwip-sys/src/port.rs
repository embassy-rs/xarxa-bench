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

/// `atoi`, referenced by `netif_find`. There is no libc in this firmware; this is the
/// minimal C-compatible implementation (leading digits, optional sign, no whitespace).
#[no_mangle]
extern "C" fn atoi(mut s: *const c_char) -> c_int {
    // SAFETY: the caller passes a NUL-terminated string, per the C contract.
    unsafe {
        let mut sign = 1;
        if *s == b'-' as c_char {
            sign = -1;
            s = s.add(1);
        } else if *s == b'+' as c_char {
            s = s.add(1);
        }
        let mut n: c_int = 0;
        while (*s as u8).is_ascii_digit() {
            n = n.wrapping_mul(10).wrapping_add((*s as u8 - b'0') as c_int);
            s = s.add(1);
        }
        n.wrapping_mul(sign)
    }
}

/// `strstr`, referenced by the DNS client's `.local` check
/// (`LWIP_DNS_SUPPORT_MDNS_QUERIES`). Naive but tiny.
#[no_mangle]
extern "C" fn strstr(haystack: *const c_char, needle: *const c_char) -> *const c_char {
    // SAFETY: the caller passes NUL-terminated strings, per the C contract.
    unsafe {
        if *needle == 0 {
            return haystack;
        }
        let mut h = haystack;
        while *h != 0 {
            let (mut a, mut b) = (h, needle);
            while *b != 0 && *a == *b {
                a = a.add(1);
                b = b.add(1);
            }
            if *b == 0 {
                return h;
            }
            h = h.add(1);
        }
        core::ptr::null()
    }
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
