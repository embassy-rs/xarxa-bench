# `xarxa` benchmarks

This repo contains a benchmark comparing the [`xarxa`](https://github.com/embassy-rs/xarxa) network stack with smoltcp and lwIP. Benchmarks run on a Nucleo STM32F429ZI board connected via Ethernet to a computer running `perf-server`.

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
