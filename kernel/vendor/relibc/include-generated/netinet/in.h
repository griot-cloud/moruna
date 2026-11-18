#ifndef _RELIBC_NETINET_IN_H
#define _RELIBC_NETINET_IN_H

#include <sys/socket.h>
#include <bits/arpainet.h> // for htonl, htons, ntohl, ntohs


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define INET_ADDRSTRLEN 16

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define INET6_ADDRSTRLEN 46

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_TOS 1

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_RECVTOS 13

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPPROTO_IP 0

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPPROTO_ICMP 1

/**
 * Non-POSIX.
 */
#define IPPROTO_IGMP 2

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPPROTO_TCP 6

/**
 * Non-POSIX.
 */
#define IPPROTO_PUP 12

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPPROTO_UDP 17

/**
 * Non-POSIX.
 */
#define IPPROTO_IDP 22

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPPROTO_IPV6 41

/**
 * Non-POSIX.
 */
#define IPPROTO_ICMPV6 58

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPPROTO_RAW 255

/**
 * Non-POSIX.
 */
#define IPPROTO_MAX 255

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_TTL 2

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPV6_UNICAST_HOPS 16

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPV6_MULTICAST_IF 17

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPV6_MULTICAST_HOPS 18

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPV6_MULTICAST_LOOP 19

/**
 * Non-POSIX.
 */
#define IPV6_ADD_MEMBERSHIP 20

#define IPV6_JOIN_GROUP 20

/**
 * Non-POSIX.
 */
#define IPV6_DROP_MEMBERSHIP 21

#define IPV6_LEAVE_GROUP 21

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define IPV6_V6ONLY 26

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_MULTICAST_IF 32

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_MULTICAST_TTL 33

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_MULTICAST_LOOP 34

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_ADD_MEMBERSHIP 35

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define IP_DROP_MEMBERSHIP 36

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define INADDR_ANY 0

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
#define INADDR_BROADCAST 4294967295

/**
 * Non-POSIX.
 */
#define INADDR_NONE 4294967295

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
#define INADDR_LOOPBACK 2130706433

/**
 * Non-POSIX.
 */
#define INADDR_UNSPEC_GROUP 3758096384

/**
 * Non-POSIX.
 */
#define INADDR_ALLHOSTS_GROUP 3758096385

/**
 * Non-POSIX.
 */
#define INADDR_ALLRTRS_GROUP 3758096386

/**
 * Non-POSIX.
 */
#define INADDR_MAX_LOCAL_GROUP 3758096639

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
struct in6_addr {
  uint8_t s6_addr[16];
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
typedef uint16_t in_port_t;

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
struct sockaddr_in6 {
  sa_family_t sin6_family;
  in_port_t sin6_port;
  uint32_t sin6_flowinfo;
  struct in6_addr sin6_addr;
  uint32_t sin6_scope_id;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
typedef uint32_t in_addr_t;

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
struct in_addr {
  in_addr_t s_addr;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
struct sockaddr_in {
  sa_family_t sin_family;
  in_port_t sin_port;
  struct in_addr sin_addr;
  char sin_zero[8];
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
struct ipv6_mreq {
  struct in6_addr ipv6mr_multiaddr;
  uint32_t ipv6mr_interface;
};

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
struct ip_mreq {
  struct in_addr imr_multiaddr;
  struct in_addr imr_interface;
};

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/ip.7.html>.
 */
struct ip_mreq_source {
  struct in_addr imr_multiaddr;
  struct in_addr imr_interface;
  struct in_addr imr_sourceaddr;
};

/**
 * Non-POSIX.
 */
struct group_req {
  uint32_t gr_interface;
  struct sockaddr_storage gr_group;
};

/**
 * Non-POSIX.
 */
struct group_source_req {
  uint32_t gsr_interface;
  struct sockaddr_storage gsr_group;
  struct sockaddr_storage gsr_source;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
extern const struct in6_addr in6addr_any;

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_in.h.html>.
 */
extern const struct in6_addr in6addr_loopback;

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_NETINET_IN_H */

#ifndef _RELIBC_BITS_NETINET_IN_H
#define _RELIBC_BITS_NETINET_IN_H

extern const struct in6_addr in6addr_any;
extern const struct in6_addr in6addr_loopback;
#define IN6ADDR_ANY_INIT { { { 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0 } } }
#define IN6ADDR_LOOPBACK_INIT { { { 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1 } } }

// from musl {
#define IN6_IS_ADDR_UNSPECIFIED(a) (((uint32_t *) (a))[0] == 0 && ((uint32_t *) (a))[1] == 0 && ((uint32_t *) (a))[2] == 0 && ((uint32_t *) (a))[3] == 0)

#define IN6_IS_ADDR_LOOPBACK(a) (((uint32_t *) (a))[0] == 0 && ((uint32_t *) (a))[1] == 0 && ((uint32_t *) (a))[2] == 0 && ((uint8_t *) (a))[12] == 0 && ((uint8_t *) (a))[13] == 0 && ((uint8_t *) (a))[14] == 0 && ((uint8_t *) (a))[15] == 1 )

#define IN6_IS_ADDR_MULTICAST(a) (((uint8_t *) (a))[0] == 0xff)

#define IN6_IS_ADDR_LINKLOCAL(a) ((((uint8_t *) (a))[0]) == 0xfe && (((uint8_t *) (a))[1] & 0xc0) == 0x80)

#define IN6_IS_ADDR_SITELOCAL(a) ((((uint8_t *) (a))[0]) == 0xfe && (((uint8_t *) (a))[1] & 0xc0) == 0xc0)

#define IN6_IS_ADDR_V4MAPPED(a) (((uint32_t *) (a))[0] == 0 && ((uint32_t *) (a))[1] == 0 && ((uint8_t *) (a))[8] == 0 && ((uint8_t *) (a))[9] == 0 && ((uint8_t *) (a))[10] == 0xff && ((uint8_t *) (a))[11] == 0xff)

#define IN6_IS_ADDR_V4COMPAT(a) (((uint32_t *) (a))[0] == 0 && ((uint32_t *) (a))[1] == 0 && ((uint32_t *) (a))[2] == 0 && ((uint8_t *) (a))[15] > 1)

#define IN6_IS_ADDR_MC_NODELOCAL(a) (IN6_IS_ADDR_MULTICAST(a) && ((((uint8_t *) (a))[1] & 0xf) == 0x1))

#define IN6_IS_ADDR_MC_LINKLOCAL(a) (IN6_IS_ADDR_MULTICAST(a) && ((((uint8_t *) (a))[1] & 0xf) == 0x2))

#define IN6_IS_ADDR_MC_SITELOCAL(a) (IN6_IS_ADDR_MULTICAST(a) && ((((uint8_t *) (a))[1] & 0xf) == 0x5))

#define IN6_IS_ADDR_MC_ORGLOCAL(a) (IN6_IS_ADDR_MULTICAST(a) && ((((uint8_t *) (a))[1] & 0xf) == 0x8))

#define IN6_IS_ADDR_MC_GLOBAL(a) (IN6_IS_ADDR_MULTICAST(a) && ((((uint8_t *) (a))[1] & 0xf) == 0xe))

#define __ARE_4_EQUAL(a,b) (!( (0[a]-0[b]) | (1[a]-1[b]) | (2[a]-2[b]) | (3[a]-3[b]) ))
#define IN6_ARE_ADDR_EQUAL(a,b) __ARE_4_EQUAL((const uint32_t *)(a), (const uint32_t *)(b))

#define IN_CLASSA(a)        ((((in_addr_t)(a)) & 0x80000000) == 0)
#define IN_CLASSA_NET       0xff000000
#define IN_CLASSA_NSHIFT    24
#define IN_CLASSA_HOST      (0xffffffff & ~IN_CLASSA_NET)
#define IN_CLASSA_MAX       128
#define IN_CLASSB(a)        ((((in_addr_t)(a)) & 0xc0000000) == 0x80000000)
#define IN_CLASSB_NET       0xffff0000
#define IN_CLASSB_NSHIFT    16
#define IN_CLASSB_HOST      (0xffffffff & ~IN_CLASSB_NET)
#define IN_CLASSB_MAX       65536
#define IN_CLASSC(a)        ((((in_addr_t)(a)) & 0xe0000000) == 0xc0000000)
#define IN_CLASSC_NET       0xffffff00
#define IN_CLASSC_NSHIFT    8
#define IN_CLASSC_HOST      (0xffffffff & ~IN_CLASSC_NET)
#define IN_CLASSD(a)        ((((in_addr_t)(a)) & 0xf0000000) == 0xe0000000)
#define IN_MULTICAST(a)     IN_CLASSD(a)
#define IN_EXPERIMENTAL(a)  ((((in_addr_t)(a)) & 0xe0000000) == 0xe0000000)
#define IN_BADCLASS(a) ((((in_addr_t)(a)) & 0xf0000000) == 0xf0000000)
// } from musl

#endif // _RELIBC_BITS_NETINET_IN_H
