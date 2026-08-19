# lwip-sys

lwIP 2.2.1, vendored and built for `thumbv7em-none-eabihf`, for the benchmark in the
parent directory. It is not a general-purpose crate — there is no `no_std` abstraction
layer over lwIP here, just the C library, its port, and bindgen's output.

## What is vendored

`lwip/` is the official 2.2.1 release, trimmed:

| | |
|---|---|
| source | <https://download.savannah.nongnu.org/releases/lwip/lwip-2.2.1.zip> |
| sha256 | `7b622662dba2383d71f874f2e494b54ae948531559c17acbe75797966d646878` |
| kept | `src/core/`, `src/include/`, `src/netif/ethernet.c`, `COPYING`, `CHANGELOG`, `README` |
| dropped | `src/api/` and `src/apps/` (netconn/socket API and the bundled applications, none of which a `NO_SYS=1` build can use), the rest of `src/netif/` (PPP, SLIP, 6LoWPAN, bridge), `doc/`, `test/`, `contrib/` |

`src/include/` is kept whole, PPP headers included, because `lwip/opt.h` includes
`netif/ppp/ppp_opts.h` unconditionally.

Everything under `lwip/` is upstream, unmodified. lwIP is BSD-3-Clause; see
`lwip/COPYING`.

## The port

Everything this port has to supply is in `port/`, and it is small because `NO_SYS=1`
removes the whole `sys_arch` half of a normal lwIP port:

| | |
|---|---|
| `port/arch/cc.h` | Byte order, the two diagnostic hooks, and `LWIP_RAND`. |
| `port/lwipopts.h` | The configuration: which protocols exist and how big the buffers are. Read this one — it is where the benchmark's "same feature set, same sizes" claim lives. |
| `port/lwip_port.[ch]` | lwIP API that is only reachable as C macros (`tcp_sndbuf`, `IP_ADDR4`, `IP_ANY_TYPE`, …), wrapped as functions so bindgen can see them. |
| `port/wrapper.h` | What bindgen reads. |
| `src/port.rs` | The Rust side of the two hooks: assertions panic through defmt, `LWIP_RAND` is a xorshift seeded with the constant the other two stacks are seeded with. |

The application must provide the clock:

```rust
#[unsafe(no_mangle)]
extern "C" fn sys_now() -> u32 { /* milliseconds since boot */ }
```

## Building

`build.rs` compiles the C with [`cc`] and generates the bindings with [`bindgen`], both
pointed at the same headers and the same `lwipopts.h`, so what is compiled and what is
declared cannot drift apart.

- The C compiler is whatever `cc` picks for the target — `arm-none-eabi-gcc` here — and
  inherits cargo's optimisation level, so the benchmark's `opt-level = "s"` profile
  compiles lwIP with `-Os`, matching the Rust side.
- bindgen needs the same libc headers the C compiler uses; `build.rs` asks the compiler
  for its sysroot (`-print-sysroot`) rather than hardcoding a toolchain path, so clang
  and gcc read the same `string.h`.
- lwIP's `LWIP_ASSERT` checks are compiled out by default, matching the Rust stacks'
  release profile. The `assert` feature puts them back; they panic through defmt.

[`cc`]: https://docs.rs/cc
[`bindgen`]: https://docs.rs/bindgen
