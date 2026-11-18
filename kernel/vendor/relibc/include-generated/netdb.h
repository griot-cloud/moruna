#ifndef _RELIBC_NETDB_H
#define _RELIBC_NETDB_H

#include <netinet/in.h>
#include <features.h>

/**
 * Socket address is inteded for `bind()`.
 */
#define AI_PASSIVE 1

/**
 * Request for cannonical name.
 */
#define AI_CANONNAME 2

/**
 * Return numeric host address as name.
 */
#define AI_NUMERICHOST 4

/**
 * If no IPv6 addresses are found, query for IPv4 addresses and return them to
 * the caller as IPv4-mapped IPv6 addresses.
 */
#define AI_V4MAPPED 8

/**
 * Query for both IPv4 and IPv6 addresses.
 */
#define AI_ALL 16

/**
 * Query for IPv4 addresses only when an IPv4 address is configured; query for
 * IPv6 addresses only when an IPv6 address is configured.
 */
#define AI_ADDRCONFIG 32

/**
 * Inhibit service name resolution.
 */
#define AI_NUMERICSERV 1024

/**
 * The flags had an invalid value.
 */
#define EAI_BADFLAGS -1

/**
 * The name does not resolve for the supplied parameters.
 * `NI_NAMEREQD` is set and the host's name cannot be located, or both
 * `nodename` and `servname` were null.
 */
#define EAI_NONAME -2

/**
 * The name could not be resolved at this time. Future attempts may succeed.
 */
#define EAI_AGAIN -3

/**
 * A non-recoverable error occurred.
 */
#define EAI_FAIL -4

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getaddrinfo.3.html>.
 * The specified network host exists but does not have any network addresses
 * defined.
 */
#define EAI_NODATA -5

/**
 * The address family was not recognized or the address length was invalid for
 * the specified family.
 */
#define EAI_FAMILY -6

/**
 * The intended socket type was not recognized.
 */
#define EAI_SOCKTYPE -7

/**
 * The service passed was not recognized for the specified socket type.
 */
#define EAI_SERVICE -8

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getaddrinfo.3.html>.
 * The specified network host does not have any network adresses in the
 * requested address family.
 */
#define EAI_ADDRFAMILY -9

/**
 * There was a memory allocation failure.
 */
#define EAI_MEMORY -10

/**
 * A system error occurred. The error code can be found in errno.
 */
#define EAI_SYSTEM -11

/**
 * An argument buffer overflowed.
 */
#define EAI_OVERFLOW -12

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getnameinfo.3.html>.
 */
#define NI_MAXHOST 1025

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getnameinfo.3.html>.
 */
#define NI_MAXSERV 32

/**
 * The numeric form of the node's address is returned instead of its name.
 */
#define NI_NUMERICHOST 1

/**
 * The numeric form of the service address is returned instead of its name.
 */
#define NI_NUMERICSERV 2

/**
 * Only the nodename portion of the FQDN is returned for local hosts.
 */
#define NI_NOFQDN 4

/**
 * Return an error if the node's name cannot be located in the database.
 */
#define NI_NAMEREQD 8

/**
 * Indicates that the service is a datagram service (`SOCK_DGRAM`).
 */
#define NI_DGRAM 16

/**
 * Removed in POSIX issue 7.
 */
#define HOST_NOT_FOUND 1

/**
 * Removed in POSIX issue 7.
 */
#define NO_DATA 2

/**
 * Removed in POSIX issue 7.
 */
#define NO_RECOVERY 3

/**
 * Removed in POSIX issue 7.
 */
#define TRY_AGAIN 4

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netdb.h.html>.
 */
struct hostent {
  char *h_name;
  char **h_aliases;
  int h_addrtype;
  int h_length;
  char **h_addr_list;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netdb.h.html>.
 */
struct netent {
  char *n_name;
  char **n_aliases;
  int n_addrtype;
  uint32_t n_net;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netdb.h.html>.
 */
struct protoent {
  char *p_name;
  char **p_aliases;
  int p_proto;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netdb.h.html>.
 */
struct servent {
  char *s_name;
  char **s_aliases;
  int s_port;
  char *s_proto;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netdb.h.html>.
 */
struct addrinfo {
  int ai_flags;
  int ai_family;
  int ai_socktype;
  int ai_protocol;
  socklen_t ai_addrlen;
  char *ai_canonname;
  struct sockaddr *ai_addr;
  struct addrinfo *ai_next;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endhostent.html>.
 */
void endhostent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endnetent.html>.
 */
void endnetent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endprotoent.html>.
 */
void endprotoent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endservent.html>.
 */
void endservent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/009696799/functions/gethostbyaddr.html>.
 * Resolve a host name from a given network address.
 *
 * # Arguments
 * * `v` - Address to resolve as a non-null [`in_addr`]
 * * `length` -
 * * `format` - AF_INET or AF_INET6
 *
 * # Safety
 * * `v` must be a valid pointer.
 * * `length` must correctly match the size of `v` as expected by `format` (usually 4 or 16).
 * * This function is not reentrant and may modify static data.
 *
 * # Panics
 * Panics if `v` is a null pointer.
 *
 * # Deprecation
 * Deprecated as of POSIX.1-2001 and removed in POSIX.1-2008.
 * New code should use [`getaddrinfo`] instead.
 */
__deprecated struct hostent *gethostbyaddr(const void *v, socklen_t length, int format);

/**
 * See <https://pubs.opengroup.org/onlinepubs/009696799/functions/gethostbyaddr.html>.
 * Resolve host information by name or IP address.
 *
 * # Arguments
 * * `name` - Host name or IP address.
 *
 * # Safety
 * `name` must be a valid string.
 * This function is not reentrant and may modify static data.
 *
 * # Panics
 * Panics if `name` is a null pointer.
 *
 * # Deprecation
 * Deprecated as of POSIX.1-2001 and removed in POSIX.1-2008.
 * New code should use [`getaddrinfo`] instead.
 */
__deprecated struct hostent *gethostbyname(const char *name);

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man3/gethostbyname.3.html>.
 * Resolve host information by name or IP address using AF_INET or AF_INET6.
 *
 * # Deprecation
 * New code should use [`getaddrinfo`] instead.
 */
__deprecated struct hostent *gethostbyname2(const char *name, int af);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endhostent.html>.
 */
struct hostent *gethostent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endnetent.html>.
 */
struct netent *getnetbyaddr(uint32_t net, int net_type);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endnetent.html>.
 */
struct netent *getnetbyname(const char *name);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endnetent.html>.
 */
struct netent *getnetent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endprotoent.html>.
 */
struct protoent *getprotobyname(const char *name);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endprotoent.html>.
 */
struct protoent *getprotobynumber(int number);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endprotoent.html>.
 */
struct protoent *getprotoent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endservent.html>.
 */
struct servent *getservbyname(const char *name, const char *proto);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endservent.html>.
 */
struct servent *getservbyport(int port, const char *proto);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endservent.html>.
 */
struct servent *getservent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endhostent.html>.
 */
void sethostent(int stayopen);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endnetent.html>.
 */
void setnetent(int stayopen);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endprotoent.html>.
 */
void setprotoent(int stayopen);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endservent.html>.
 */
void setservent(int stayopen);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/freeaddrinfo.html>.
 */
int getaddrinfo(const char *node,
                const char *service,
                const struct addrinfo *hints,
                struct addrinfo **res_out);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getnameinfo.html>.
 */
int getnameinfo(const struct sockaddr *addr,
                socklen_t addrlen,
                char *host,
                socklen_t hostlen,
                char *serv,
                socklen_t servlen,
                int flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/freeaddrinfo.html>.
 */
void freeaddrinfo(struct addrinfo *res);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/gai_strerror.html>.
 */
const char *gai_strerror(int errcode);

/**
 * Provide a pointer to relibc's internal [`H_ERRNO`].
 */
__deprecated int *__h_errno_location(void);

__deprecated const char *hstrerror(int errcode);

/**
 * Print error message associated with [`H_ERRNO`] to stderr.
 *
 * # Arguments
 * * `prefix` - An optional prefix to prepend to the error message. May be null or an empty
 *   (`""`) C string.
 *
 * # Safety
 * Like [`crate::header::stdio::perror`], `prefix` should be a valid, NUL terminated C string if
 * used. The caller may safely call this function with a null pointer.
 *
 * # Deprecation
 * [`H_ERRNO`], [`hstrerror`], [`herror`], and other functions are deprecated as of
 * POSIX.1-2001 and removed as of POSIX.1-2008. These functions are provided for backwards
 * compatibility but should not be used by new code.
 */
__deprecated void herror(const char *prefix);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_NETDB_H */

#ifndef _RELIBC_BITS_NETDB_H
#define _RELIBC_BITS_NETDB_H

#define h_errno (*__h_errno_location())
#define h_addr  h_addr_list[0] /* Address, for backward compatibility.*/

#endif // _RELIBC_BITS_NETDB_H
