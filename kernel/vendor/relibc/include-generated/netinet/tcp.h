#ifndef _RELIBC_NETINET_TCP_H
#define _RELIBC_NETINET_TCP_H

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/netinet_tcp.h.html>.
 *
 * Avoid coalescing of small segments.
 */
#define TCP_NODELAY 1

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/tcp.7.html>.
 *
 * The maximum segment size for outgoing TCP packets.
 */
#define TCP_MAXSEG 2

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/tcp.7.html>.
 *
 * The time (in seconds) the connection needs to remain idle before TCP starts
 * sending keepalive probes, if the socket option `SO_KEEPALIVE` has been set
 * on this socket.
 */
#define TCP_KEEPIDLE 4

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/tcp.7.html>.
 *
 * The time (in seconds) between individual keepalive probes.
 */
#define TCP_KEEPINTVL 5

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/tcp.7.html>.
 *
 * The maximum number of keepalive probes TCP should send before dropping the
 * connection.
 */
#define TCP_KEEPCNT 6

#endif  /* _RELIBC_NETINET_TCP_H */
