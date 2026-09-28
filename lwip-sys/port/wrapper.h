/* What bindgen reads: the lwIP raw API the benchmark uses, plus this port's shims.
 * Every lwIP header below guards its own contents on the LWIP_* options, so including
 * one whose protocol is off contributes nothing. */
#include "lwip/init.h"
#include "lwip/netif.h"
#include "lwip/pbuf.h"
#include "lwip/tcp.h"
#include "lwip/timeouts.h"
#include "lwip/udp.h"
#include "lwip/raw.h"
#include "lwip/dhcp.h"
#include "lwip/dns.h"
#include "lwip/igmp.h"
#include "lwip/mld6.h"
#include "lwip/etharp.h"
#include "lwip/ethip6.h"
#include "netif/ethernet.h"

#include "lwip_port.h"
