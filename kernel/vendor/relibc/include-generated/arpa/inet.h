#ifndef _RELIBC_ARPA_INET_H
#define _RELIBC_ARPA_INET_H

#include <netinet/in.h>
#include <features.h>
#include <bits/arpainet.h>  // for htonl, htons, ntohl, ntohs
#include <bits/socklen-t.h> // for socklen_t from sys/socket.h


#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/inet_addr.html>.
 *
 * Converts the string pointed to by `cp`, in the standard IPv4 dotted
 * decimal notation, to an integer value suitable for use as an Internet
 * address.
 *
 * # Deprecated
 * The `inet_addr()` function was marked obsolescent in the Open Group Base
 * Specifications Issue 8.
 *
 * Applications should prefer `inet_pton()` over `inet_addr()` for the
 * following reasons:
 * - The return value from `inet_addr()` when converting 255.255.255.255 is
 *   indistinguishable from an error.
 * - The `inet_pton()` function supports multiple address families.
 * - The alternative textual representations supported by `inet_addr()` (but
 *   not `inet_pton()`) are often used maliciously to confuse or mislead
 *   users (e.g, for phishing).
 */
__deprecated in_addr_t inet_addr(const char *cp);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/inet_aton.3.html>.
 *
 * Converts the Internet host address `cp` from the IPv4 numbers-and-dots
 * notation into binary form (in network byte order) and stores it in the
 * structure that `inp` points to.
 */
int inet_aton(const char *cp, struct in_addr *inp);

/**
 * See <https://pubs.opengroup.org/onlinepubs/7908799/xns/inet_lnaof.html>.
 *
 * Takes an Internet host address specified by `in` and extracts the local
 * network address part, in host byte order.
 *
 * # Deprecation
 * The `inet_lnaof()` function was specified in Networking Services Issue 5,
 * but not in the Open Group Base Specifications Issue 6 and later.
 */
__deprecated in_addr_t inet_lnaof(struct in_addr in);

/**
 * See <https://pubs.opengroup.org/onlinepubs/7908799/xns/inet_makeaddr.html>.
 *
 * Takes the Internet network number specified by `net` and the local network
 * address specified by `lna`, both in host byte order, and constructs an
 * Internet address from them.
 *
 * # Deprecation
 * The `inet_makeaddr()` function was specified in Networking Services Issue
 * 5, but not in the Open Group Base Specifications Issue 6 and later.
 */
__deprecated struct in_addr inet_makeaddr(in_addr_t net, in_addr_t lna);

/**
 * See <https://pubs.opengroup.org/onlinepubs/7908799/xns/inet_netof.html>.
 *
 * Takes an Internet host address specified by `in` and extracts the network
 * number part, in host byte order.
 *
 * # Deprecation
 * The `inet_netof()` function was specified in Networking Services Issue 5,
 * but not in the Open Group Base Specifications Issue 6 and later.
 */
__deprecated in_addr_t inet_netof(struct in_addr in);

/**
 * See <https://pubs.opengroup.org/onlinepubs/7908799/xns/inet_network.html>.
 *
 * Converts the string pointed to by `cp`, in the Internet standard dot
 * notation, to an integer value suitable for use as an Internet network
 * number.
 *
 * # Deprecation
 * The `inet_network()` function was specified in Networking Services Issue 5,
 * but not in the Open Group Base Specifications Issue 6 and later.
 */
__deprecated in_addr_t inet_network(const char *cp);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/inet_addr.html>.
 *
 * Converts the Internet host address specified by `in` to a string in the
 * Internet standard dot notation.
 *
 * # Deprecation
 * The `inet_ntoa()` function was marked obsolescent in the Open Group Base
 * Specifications Issue 8.
 *
 * Applications should prefer `inet_ntop()` over `inet_ntoa()` as it supports
 * multiple address families and is thread-safe.
 */
__deprecated char *inet_ntoa(struct in_addr in);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/inet_ntop.html>.
 *
 * Converts a numeric address into a text string suitable for presentation.
 */
const char *inet_ntop(int af, const void *src, char *dst, socklen_t size);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/inet_ntop.html>.
 *
 * Converts an address in its standard text presentation form into its
 * numeric binary form.
 */
int inet_pton(int af, const char *src, void *dst);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_ARPA_INET_H */
