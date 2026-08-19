//! Raw bindings to lwIP 2.2.1, built for `thumbv7em-none-eabihf`.
//!
//! The vendored C sources are in `lwip/`, this port's configuration in `port/` — see
//! `README.md` for what is vendored and how it is configured. The bindings are
//! generated at build time from `port/wrapper.h`, so what this crate exports is exactly
//! lwIP's raw (callback) API for the protocols `port/lwipopts.h` turns on, plus the
//! handful of `lwipx_*` shims that wrap lwIP macros as functions.
//!
//! Everything here is `unsafe` C API. The one thing the *user* of this crate must
//! provide is the clock:
//!
//! ```ignore
//! #[unsafe(no_mangle)]
//! extern "C" fn sys_now() -> u32 { /* milliseconds since boot */ }
//! ```

#![no_std]
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

mod port;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
