/* The parts of lwIP's API that are macros, wrapped as functions so bindgen can see them.
 *
 * Everything here is a one-liner over a public lwIP macro; there is no logic in this
 * file. Anything lwIP already exports as a function is called directly from Rust.
 */
#ifndef LWIPX_PORT_H
#define LWIPX_PORT_H

#include "lwip/ip_addr.h"
#include "lwip/netif.h"
#include "lwip/tcp.h"

#ifdef __cplusplus
extern "C" {
#endif

/** `IP_ANY_TYPE`: the address to bind to for "any address, either family". */
const ip_addr_t *lwipx_ip_any_type(void);

/** Build an `ip_addr_t` (a tagged union) from its bytes, in either family. */
void lwipx_ip4(ip_addr_t *out, const u8_t bytes[4]);
void lwipx_ip6(ip_addr_t *out, const u8_t bytes[16]);

/** `netif_set_addr` over plain bytes: address, netmask and gateway at once. */
void lwipx_netif_set_ip4(struct netif *netif, const u8_t addr[4], const u8_t netmask[4],
                         const u8_t gw[4]);

/** Add a static IPv6 address to a netif, already valid (no duplicate address detection,
 * matching the other two stacks). Returns lwIP's error code. */
err_t lwipx_netif_add_ip6(struct netif *netif, const u8_t bytes[16]);

/** `tcp_sndbuf`: how much more the send buffer accepts right now. */
u16_t lwipx_tcp_sndbuf(const struct tcp_pcb *pcb);

/** `tcp_mss`: the effective MSS of this connection. */
u16_t lwipx_tcp_mss(const struct tcp_pcb *pcb);

/** `tcp_nagle_disable`: set TF_NODELAY. */
void lwipx_tcp_nagle_disable(struct tcp_pcb *pcb);

#ifdef __cplusplus
}
#endif

#endif /* LWIPX_PORT_H */
