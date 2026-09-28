# `xarxa` benchmarks

This repo contains a benchmark comparing the [`xarxa`](https://github.com/embassy-rs/xarxa) network stack with smoltcp and lwIP. Benchmarks run on a Nucleo STM32F429ZI board connected via Ethernet to a computer running `perf-server`.

There are two binaries:

- the **throughput benchmark** (`src/main.rs`, run by `bench.py`), which always builds
  the full protocol set so every matrix cell runs the same firmware;
- the **code-size probe** (`src/codesize/main.rs`, run by `codesize.py`), which builds
  a ladder of representative feature sets — or any combination you name — and compares
  the three stacks' flash footprints.

Code size is `codesize.py`'s alone: `bench.py` no longer reports it, since the footprint
of the throughput firmware is a property of the one feature set that firmware is pinned
to, not of the benchmark it runs.

## Code size

`codesize.py` walks a ladder of feature sets, each a superset of the one before, from
the smallest thing all three stacks can build up to everything they share. It builds
every rung for every stack and draws `bench-codesize.svg`:

```sh
./codesize.py                        # the whole ladder, all three stacks
./codesize.py --only bare,full       # some rungs of it
./codesize.py --render-only          # redraw from codesize-results.json
```

| rung | what it adds |
|---|---|
| `bare` | IPv4 + UDP — the floor, since smoltcp cannot build without a socket type |
| `tcp` | + TCP, listener, Reno |
| `client` | + DHCP, DNS, ping replies, reassembly — the shape most products ship |
| `full` | + IPv6, SLAAC, mDNS, multicast, IPv4 fragmentation, raw IP — the full three-way intersection |

**Every rung is apples-to-apples.** A rung only holds features all three stacks have an
equivalent of, so a bar is never short merely because that stack lacks the capability.

Three features count as shared although they forward to nothing on some stacks, because
the capability is there regardless: listening is part of TCP itself on smoltcp and lwIP,
lwIP's TCP is always Reno, and lwIP answers pings whenever its ICMP is compiled in.
`medium-ip` is the opposite case and is deliberately *not* in `full` — lwIP's netif is
medium-agnostic, so it pays nothing for it while the other two pay real bytes.

Features not all three stacks have (`medium-ip`, `raw-ethernet`, `dhcpv4-options`,
`icmp-errors`, `iface-bind`, `packetmeta-*`, `async`, `hostname`, `tcp-sack`,
`tcp-timestamps`) are on no rung, because there is nothing to compare them against.
Measure those with an ad-hoc combination.

Results cache in `codesize-results.json` along with the feature list each was built
from, so editing a rung rebuilds that rung instead of silently reporting a stale number.

Any positional argument is an ad-hoc combination instead, answering "what does *this*
cost" without touching the ladder or its chart:

```sh
./codesize.py ipv4,udp                # one combination, all three stacks
./codesize.py ipv4,udp ipv4,udp,tcp   # several combinations, one table row each
```

The crate re-exports xarxa's feature list one for one (`ipv4`, `ipv6`, `udp`, `tcp`,
`tcp-listener`, `raw-ip`, `raw-ethernet`, `dns`, `mdns`, `dhcpv4`, `slaac`,
`multicast`, `icmp-ping-reply`, `icmp-errors`, `iface-bind`, `tcp-reno`/`tcp-cubic`,
`tcp-sack`, `tcp-timestamps`, `packetmeta-*`, `async`, fragmentation/reassembly, the
media, ...). Each one also enables the closest equivalent in smoltcp (its cargo
features) and lwIP (`LWIP_*` options, driven from cargo features by
`lwip-sys/build.rs`). Where a stack has no equivalent — smoltcp has no TCP timestamps,
lwIP has no packet metadata, only xarxa has `icmp-errors` — the feature enables nothing
there, and the size comparison shows what the capability costs on the stacks that have
it.

For every enabled feature the probe's main loop makes plain, dumb use of the feature's
API — UDP and TCP echo ports, an outgoing TCP connection, raw sockets echoing packets
back, a DNS query every few seconds, DHCP, SLAAC, a multicast join — so the linker
cannot discard the code under measurement, and each stack does the same jobs with its
own API. Caveats: `medium-ethernet` is always included (the board's interface is
Ethernet); smoltcp cannot build socket-less combinations at all (its column shows `-`);
lwIP answers pings whenever a family's ICMP is compiled in, with or without
`icmp-ping-reply`.

The probe is built with the `codesize` profile of `Cargo.toml`, which is the release
profile plus `panic = "immediate-abort"`, and with core rebuilt from source
(`-Zbuild-std=core`). A panic compiles to a plain abort, so no panic messages, source
locations or formatting code end up in the image. Without it, the two Rust stacks pay
for every panic site they have, which lwIP, being C, never does. This needs nightly and
the `rust-src` component.

## Throughput

Code is very rough, mostly AI generated. You've been warned.

I've ensured the benchmark is as apples-to-apples as possible:

- Same HAL (embassy-stm32), same(-ish) ETH driver, same RCC config.
- Same set of features enabled in all 3 stacks (IPv4, IPv6, TCP, UDP all at once)
- Same buffer sizes, large enough that there is no drops. Same UDP receive queue depth
  (32 datagrams) on xarxa and smoltcp; lwIP has no receive queue at all.
- No allocator in any of the three builds. Every buffer is a static: xarxa's packet pool,
  smoltcp's device rings and socket buffers, lwIP's `MEM_SIZE` heap and rings. xarxa is
  built without its `alloc` feature and smoltcp without its own.
- xarxa `tcp-timestamps` disabled, also disabled in the other stacks
- xarxa `icmp-errors` disabled, since smoltcp doesn't have it.
- `icmp-ping-reply` enabled

There is a few things that aren't apples-to-apples:

- lwIP TCP RX is zero-copy, which is why it's so much faster. The API is different, it hands the data to the user by *calling a callback synchronously from ingress code*. This is unfair against xarxa and smoltcp: they could also easily do zero-copy TCP RX if they were also allowed to have such a terrible API.

## Text alignment issues.

Sometimes a critical function gets some "unlucky" alignment (hot instruction split across cache lines?) which varies performance. This seems to change randomly, if you run into it build with a different number in the TEXT_SHIFT env var. Most vary slightly but smoltcp udp rx varies wildly, going from 19.3 mbps to 45.5 mbps. `bench.py` runs each bench 4 times with the 4 alignments and picks the best result.
