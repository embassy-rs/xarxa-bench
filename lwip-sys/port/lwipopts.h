/* lwIP configuration for the xarxa / smoltcp / lwIP benchmark on a Nucleo-F429ZI.
 *
 * Two rules decide everything in this file:
 *
 *  1. **Same feature set.** lwIP is cut down to the protocols xarxa implements, exactly
 *     as smoltcp is in `../../Cargo.toml`: ethernet + ARP + ND, IPv4, IPv6, ICMP, raw,
 *     UDP and TCP. Out go DHCP, DNS, autoip, IGMP/MLD, SLAAC, fragmentation and
 *     reassembly, PPP, 6LoWPAN and the netconn/socket API (which needs an OS anyway).
 *
 *  2. **Same sizes.** The knobs the three stacks share are set to the same numbers,
 *     which live in `../../src/bench.rs` and are checked against these at compile time
 *     (see `bench_lwip.rs`): a 32 KiB TCP window, a 1460-byte MSS over IPv4, and the
 *     same 1500-byte IP MTU.
 *
 * Everything else is left at lwIP's default.
 */
#ifndef LWIP_LWIPOPTS_H
#define LWIP_LWIPOPTS_H

/* ------------------------------------------------------------------ *
 * System                                                              *
 * ------------------------------------------------------------------ */

/* No OS: the benchmark spins on a poll loop in `main`, exactly as it does for the other
 * two stacks. This also picks the raw (callback) API — netconn and sockets need
 * threads. */
#define NO_SYS 1
#define SYS_LIGHTWEIGHT_PROT 0
#define LWIP_NETCONN 0
#define LWIP_SOCKET 0
#define LWIP_NETIF_API 0

/* ------------------------------------------------------------------ *
 * Memory                                                              *
 * ------------------------------------------------------------------ */

#define MEM_LIBC_MALLOC 0
#define MEMP_MEM_MALLOC 0
#define MEM_ALIGNMENT 4

/* lwIP's heap, out of which every outgoing packet (PBUF_RAM) is allocated. It has to
 * hold a full 32 KiB of unacknowledged TCP data — ~23 full-size segments, each in its
 * own pbuf — plus whatever the driver still holds in the transmit ring. Received
 * frames do *not* come from here: the driver hands lwIP custom pbufs that point into
 * the DMA ring (see `../../src/eth.rs`). */
#define MEM_SIZE (64 * 1024)

/* The pbuf pool is what a copying driver would receive into. This one does not copy,
 * so nothing in this build ever allocates a PBUF_POOL pbuf. */
#define PBUF_POOL_SIZE 0

#define MEMP_NUM_UDP_PCB 4
#define MEMP_NUM_TCP_PCB 4
#define MEMP_NUM_TCP_PCB_LISTEN 2
#define MEMP_NUM_RAW_PCB 2

/* ------------------------------------------------------------------ *
 * Protocols                                                           *
 * ------------------------------------------------------------------ */

/* Which protocols exist is driven by this crate's cargo features: build.rs defines
 * every feature-driven LWIP_* option as 0 or 1 on the compiler command line (see
 * `options()` there), so none of them may be defined here. What stays in this file is
 * what is *never* enabled, plus everything that is not a protocol at all. */

#define LWIP_ETHERNET 1

#define LWIP_AUTOIP 0
#define LWIP_ALTCP 0
#define LWIP_NETIF_LOOPBACK 0
#define LWIP_STATS 0

/* IPv6 fragmentation and reassembly are off — xarxa has neither (DESIGN.md §10). The
 * IPv4 pair is feature-driven (build.rs). */
#define LWIP_IPV6_REASS 0
#define LWIP_IPV6_FRAG 0

/* No duplicate address detection: static addresses go straight to preferred, matching
 * the other two stacks. This also applies to SLAAC addresses when the `slaac` feature
 * turns autoconfiguration on. */
#define LWIP_IPV6_DUP_DETECT_ATTEMPTS 0

/* ------------------------------------------------------------------ *
 * TCP                                                                 *
 * ------------------------------------------------------------------ */

/* The MSS over IPv4: 1500 - 20 (IP) - 20 (TCP). Over IPv6 lwIP derives 1440 from the
 * netif's MTU on its own, which is what the other two stacks do as well. */
#define TCP_MSS 1460

/* The window in each direction, matching `bench::TCP_BUF`. Neither of these preallocates
 * anything: unlike smoltcp and xarxa, lwIP has no per-socket ring buffers — received
 * data stays in the pbufs it arrived in, and sent data lives in pbufs on the heap above
 * until it is acknowledged. */
#define TCP_WND (32 * 1024)
#define TCP_SND_BUF (32 * 1024)

/* lwIP's own default formula, spelled out because `MEMP_NUM_TCP_SEG` has to be at least
 * this and is resolved before opt.h computes it. */
#define TCP_SND_QUEUELEN ((4 * TCP_SND_BUF) / TCP_MSS)
#define MEMP_NUM_TCP_SEG TCP_SND_QUEUELEN

/* No window scaling: a 32 KiB window needs no scale. (SACK is feature-driven, see
 * build.rs.) */
#define LWIP_WND_SCALE 0

/* ------------------------------------------------------------------ *
 * Driver interface                                                    *
 * ------------------------------------------------------------------ */

/* Received frames are handed up as custom pbufs pointing straight into the DMA ring, so
 * that ingress is zero-copy the way it is for xarxa (and the way the borrowed device
 * buffer is for smoltcp). See `RxPbuf` in `../../src/eth.rs`. */
#define LWIP_SUPPORT_CUSTOM_PBUF 1

/* The link header always fits in the first pbuf of an outgoing chain, so the driver
 * never has to reach across a chain boundary to find the ethernet header. Chained
 * payloads are still handled — one descriptor per pbuf. */
#define LWIP_NETIF_TX_SINGLE_PBUF_LINK 1

/* Checksums in software, both directions, all protocols — the MAC's offload engine is
 * switched off for all three stacks. These are lwIP's defaults; they are spelled out
 * because this is the one thing the benchmark most needs to be identical. */
#define CHECKSUM_GEN_IP 1
#define CHECKSUM_GEN_UDP 1
#define CHECKSUM_GEN_TCP 1
#define CHECKSUM_GEN_ICMP 1
#define CHECKSUM_GEN_ICMP6 1
#define CHECKSUM_CHECK_IP 1
#define CHECKSUM_CHECK_UDP 1
#define CHECKSUM_CHECK_TCP 1
#define CHECKSUM_CHECK_ICMP 1
#define CHECKSUM_CHECK_ICMP6 1
#define LWIP_CHECKSUM_ON_COPY 0

#endif /* LWIP_LWIPOPTS_H */
