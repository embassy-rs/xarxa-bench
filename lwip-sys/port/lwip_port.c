#include "lwip_port.h"

const ip_addr_t *lwipx_ip_any_type(void)
{
  return IP_ANY_TYPE;
}

#if LWIP_IPV4
void lwipx_ip4(ip_addr_t *out, const u8_t bytes[4])
{
  IP_ADDR4(out, bytes[0], bytes[1], bytes[2], bytes[3]);
}
#endif

#if LWIP_IPV6
void lwipx_ip6(ip_addr_t *out, const u8_t bytes[16])
{
  IP_SET_TYPE(out, IPADDR_TYPE_V6);
  ip_2_ip6(out)->zone = IP6_NO_ZONE;
  MEMCPY(ip_2_ip6(out)->addr, bytes, 16);
}
#endif

#if LWIP_IPV4
void lwipx_netif_set_ip4(struct netif *netif, const u8_t addr[4], const u8_t netmask[4],
                         const u8_t gw[4])
{
  ip4_addr_t a, m, g;
  IP4_ADDR(&a, addr[0], addr[1], addr[2], addr[3]);
  IP4_ADDR(&m, netmask[0], netmask[1], netmask[2], netmask[3]);
  IP4_ADDR(&g, gw[0], gw[1], gw[2], gw[3]);
  netif_set_addr(netif, &a, &m, &g);
}
#endif

#if LWIP_IPV6
err_t lwipx_netif_add_ip6(struct netif *netif, const u8_t bytes[16])
{
  ip6_addr_t a;
  s8_t idx = -1;
  err_t err;

  MEMCPY(a.addr, bytes, 16);
  ip6_addr_clear_zone(&a);

  err = netif_add_ip6_address(netif, &a, &idx);
  if (err != ERR_OK) {
    return err;
  }
  /* Straight to valid: the other two stacks do no duplicate address detection either. */
  netif_ip6_addr_set_state(netif, idx, IP6_ADDR_PREFERRED);
  return ERR_OK;
}
#endif

#if LWIP_TCP
u16_t lwipx_tcp_sndbuf(const struct tcp_pcb *pcb)
{
  return tcp_sndbuf(pcb);
}

u16_t lwipx_tcp_mss(const struct tcp_pcb *pcb)
{
  return tcp_mss(pcb);
}

void lwipx_tcp_nagle_disable(struct tcp_pcb *pcb)
{
  tcp_nagle_disable(pcb);
}
#endif
