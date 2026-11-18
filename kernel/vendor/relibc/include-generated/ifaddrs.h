#ifndef _RELIBC_IFADDRS_H
#define _RELIBC_IFADDRS_H

#include <features.h>
#include <netinet/in.h>

/**
 * Either the broadcast address associated with `ifa_addr` (if applicable
 * for the address family) or the destination address of the
 * point-to-point interface.
 */
union ifaddrs_ifa_ifu {
  /**
   * Broadcast address of interface.
   */
  struct sockaddr *ifu_broadaddr;
  /**
   * Point-to-point destination address.
   */
  struct sockaddr *ifu_dstaddr;
};

/**
 * An entry in a linked list describing the network interfaces of the local
 * system.
 */
struct ifaddrs {
  /**
   * Next item in list.
   */
  struct ifaddrs *ifa_next;
  /**
   * Name of interface.
   */
  char *ifa_name;
  /**
   * Flags from `SIOCGIFFLAGS`.
   */
  unsigned int ifa_flags;
  /**
   * Address of interface.
   */
  struct sockaddr *ifa_addr;
  /**
   * Netmask of interface.
   */
  struct sockaddr *ifa_netmask;
  /**
   * Depends on the bit `IFF_BROADCAST` or `IFF_POINTOPOINT` being set in
   * `ifa_flags`. The bits are mutually exclusive.
   */
  union ifaddrs_ifa_ifu ifa_ifu;
  /**
   * Address-specific data.
   */
  void *ifa_data;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Frees the dynamically allocated memory used by `ifa`.
 */
void freeifaddrs(struct ifaddrs *ifa);

/**
 * Creates a linked list of structures describing the network interfaces of
 * the local system, and stores the address of the first item of the list
 * in `ifap`.
 *
 * The data returned by `getifaddrs()` is dynamically allocated and should
 * be freed using `freeifaddrs()` when no longer needed.
 */
int getifaddrs(struct ifaddrs **ifap);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_IFADDRS_H */

#define ifa_broadaddr ifa_ifu.ifu_broadaddr
#define ifa_dstaddr ifa_ifu.ifu_dstaddr
