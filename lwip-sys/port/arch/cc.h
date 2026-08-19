/* lwIP compiler/architecture port for the xarxa benchmark board (Cortex-M4, GCC).
 *
 * This is the whole "port" half of an lwIP port: NO_SYS=1 means there is no
 * `sys_arch.h` to write, so all that is left is the handful of things lwIP asks the
 * compiler and the platform for.
 */
#ifndef LWIP_ARCH_CC_H
#define LWIP_ARCH_CC_H

#include <stdint.h>
#include <stddef.h>

/* lwIP reads BYTE_ORDER and compares it against BIG_ENDIAN. Neither name exists in a
 * freestanding build, and leaving them undefined makes `#if BYTE_ORDER == BIG_ENDIAN`
 * compare 0 to 0 — i.e. silently build a big-endian stack. Define all three. */
#define LITTLE_ENDIAN 1234
#define BIG_ENDIAN 4321
#define BYTE_ORDER LITTLE_ENDIAN

/* There is no libc here beyond the freestanding headers, and nothing that provides
 * these two. lwIP only uses them for hostname handling (LWIP_DNS/LWIP_NETIF_HOSTNAME,
 * both off). */
#define LWIP_NO_UNISTD_H 1
#define LWIP_NO_CTYPE_H 1

/* Diagnostics and assertions come out through defmt, on the Rust side of the port
 * (`src/port.rs`) — lwIP's defaults pull in printf/abort, which do not exist here.
 *
 * LWIP_PLATFORM_DIAG takes a parenthesised printf argument list, which defmt cannot
 * consume, so it is dropped: nothing in this build enables LWIP_DEBUG, and the few
 * unconditional diagnostics (lwip_sanity_check) are warnings we do not act on. */
#define LWIP_PLATFORM_DIAG(x)  \
  do {                         \
  } while (0)

void lwipx_assert_failed(const char *msg, const char *file, int line);
#define LWIP_PLATFORM_ASSERT(x) lwipx_assert_failed((x), __FILE__, __LINE__)

/* TCP initial sequence numbers, ephemeral ports and IPv6 fragment ids. Seeded on the
 * Rust side with the same constant the other two stacks are seeded with. */
uint32_t lwipx_rand(void);
#define LWIP_RAND() (lwipx_rand())

#endif /* LWIP_ARCH_CC_H */
