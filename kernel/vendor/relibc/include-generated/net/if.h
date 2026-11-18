#ifndef _RELIBC_NET_IF_H
#define _RELIBC_NET_IF_H

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/net_if.h.html>.
 *
 * Interface name length.
 */
#define IF_NAMESIZE 16

/**
 * Interface is running.
 */
#define IFF_UP 1

/**
 * Valid broadcast address set.
 */
#define IFF_BROADCAST 2

/**
 * Internal debugging flag.
 */
#define IFF_DEBUG 4

/**
 * Interface is a loopback interface.
 */
#define IFF_LOOPBACK 8

/**
 * Interface is a point-to-point link.
 */
#define IFF_POINTOPOINT 16

/**
 * Avoid use of trailers.
 */
#define IFF_NOTRAILERS 32

/**
 * Resources allocated.
 */
#define IFF_RUNNING 64

/**
 * No arp protocol, L2 destination address not set.
 */
#define IFF_NOARP 128

/**
 * Interface is in promiscuous mode.
 */
#define IFF_PROMISC 256

/**
 * Receive all multicast packets.
 */
#define IFF_ALLMULTI 512

/**
 * Master of a load balancing bundle.
 */
#define IFF_MASTER 1024

/**
 * Slave of a load balancing bundle.
 */
#define IFF_SLAVE 2048

/**
 * Supports multicast.
 */
#define IFF_MULTICAST 4096

/**
 * Is able to select media type via ifmap.
 */
#define IFF_PORTSEL 8192

/**
 * Auto media selection active.
 */
#define IFF_AUTOMEDIA 16384

/**
 * The addresses are lost when the interface goes down.
 */
#define IFF_DYNAMIC 32768

/**
 * Driver signals L1 up.
 */
#define IFF_LOWER_UP 65536

/**
 * Driver signals dormant.
 */
#define IFF_DORMANT 131072

/**
 * Echo sent packets.
 */
#define IFF_ECHO 262144

#define IFF_VOLATILE ((((((((IFF_LOOPBACK | IFF_POINTOPOINT) | IFF_BROADCAST) | IFF_ECHO) | IFF_MASTER) | IFF_SLAVE) | IFF_RUNNING) | IFF_LOWER_UP) | IFF_DORMANT)

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/net_if.h.html>.
 */
struct if_nameindex {
  /**
   * Numeric index of the interface.
   */
  unsigned int if_index;
  /**
   * Null-terminated name of the interface.
   */
  const char *if_name;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/if_freenameindex.html>.
 *
 * # Safety
 * this is a no-op: the list returned by if_nameindex() is a ref to a constant
 */
void if_freenameindex(struct if_nameindex *_s);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/if_indextoname.html>.
 *
 * # Safety
 * Returns only static lifetime references to const names, does not reuse the buf pointer.
 * Returns NULL in case of not found + ERRNO being set to ENXIO.
 * Currently only checks against inteface index 1.
 */
const char *if_indextoname(unsigned int idx, char *_buf);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/if_nameindex.html>.
 *
 * # Safety
 * Returns a constant pointer to a pre defined const stub list
 * The end of the list is determined by an if_nameindex struct having if_index 0 and if_name NULL
 */
const struct if_nameindex *if_nameindex(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/if_nametoindex.html>.
 *
 * # Safety
 * Compares the name to a constant string and only returns an int as a result.
 * An invalid name string will return an index of 0
 */
unsigned int if_nametoindex(const char *name);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_NET_IF_H */
