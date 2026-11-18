#ifndef _RELIBC_SYS_SOCKET_H
#define _RELIBC_SYS_SOCKET_H

#include <bits/iovec.h>      // for iovec from sys/uio.h
#include <bits/safamily-t.h> // for sa_family_t
#include <bits/socklen-t.h>  // for socklen_t
#include <bits/ssize-t.h>    // for ssize_t from sys/types.h

#if defined(_GNU_SOURCE)
#include <bits/ucred.h>      // for Non-POSIX ucred struct
#endif


/**
 * Byte-stream socket.
 */
#define SOCK_STREAM 1

/**
 * Datagram socket.
 */
#define SOCK_DGRAM 2

/**
 * Raw Protocol Interface.
 */
#define SOCK_RAW 3

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/socket.2.html>.
 *
 * Provides a reliable datagram layer that does not guarantee ordering.
 */
#define SOCK_RDM 4

/**
 * Sequenced-packet socket.
 */
#define SOCK_SEQPACKET 5

/**
 * Create a socket file descriptor with the `O_NONBLOCK` flag atomically set
 * on the new open file description.
 */
#define SOCK_NONBLOCK 2048

/**
 * Create a socket file descriptor wit the `FD_CLOEXEC` flag atomically set
 * on that file descriptor.
 */
#define SOCK_CLOEXEC 524288

/**
 * Options to be accessed at socket level, not protocol level.
 */
#define SOL_SOCKET 1

/**
 * Debugging information is recorded.
 */
#define SO_DEBUG 1

/**
 * Reuse of local addresses is supported.
 */
#define SO_REUSEADDR 2

/**
 * Socket type.
 */
#define SO_TYPE 3

/**
 * Socket error status.
 */
#define SO_ERROR 4

/**
 * Bypass normal routing.
 */
#define SO_DONTROUTE 5

/**
 * Transmission of broadcast messages is supported.
 */
#define SO_BROADCAST 6

/**
 * Send buffer size.
 */
#define SO_SNDBUF 7

/**
 * Receive buffer size.
 */
#define SO_RCVBUF 8

/**
 * Connections are kept alive with periodic messages.
 */
#define SO_KEEPALIVE 9

/**
 * Out-of-band data is transmitted inline.
 */
#define SO_OOBINLINE 10

/**
 * Non-POSIX, found in LwIP.
 *
 * Don't create UDP checksum.
 */
#define SO_NO_CHECK 11

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * Set the protocol-defined priority for all packets to be sent on this
 * socket.
 */
#define SO_PRIORITY 12

/**
 * Socket lingers on close.
 */
#define SO_LINGER 13

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * Enable BSD bug-to-bug compatibility.
 */
#define SO_BSDCOMPAT 14

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * Permits multiple `AF_INET` or `AF_INET6` sockets to be bound to an
 * identical socket address.
 */
#define SO_REUSEPORT 15

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * Enable or disable the receiving of the `SCM_CREDENTIALS` control message.
 */
#define SO_PASSCRED 16

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * Return the credentials of the peer process connected to this socket.
 */
#define SO_PEERCRED 17

/**
 * Receive "low water mark".
 */
#define SO_RCVLOWAT 18

/**
 * Send "low water mark".
 */
#define SO_SNDLOWAT 19

/**
 * Receive timeout.
 */
#define SO_RCVTIMEO 20

/**
 * Send timeout.
 */
#define SO_SNDTIMEO 21

/**
 * Socket is accepting connections.
 */
#define SO_ACCEPTCONN 30

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/SO_PEERSEC.2const.html>.
 *
 * Get the security context of a peer socket.
 */
#define SO_PEERSEC 31

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * A privileged (`CAP_NET_ADMIN`) process can perform the same task as
 * `SO_SNDBUF`, but the `wmem_max` limit can be overridden.
 */
#define SO_SNDBUFFORCE 32

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/socket.7.html>.
 *
 * A privileged (`CAP_NET_ADMIN`) process can perform the same task as
 * `SO_RCVBUF`, but the `rmem_max` limit can be overridden.
 */
#define SO_RCVBUFFORCE 33

/**
 * Socket protocol.
 */
#define SO_PROTOCOL 38

/**
 * Socket domain.
 */
#define SO_DOMAIN 39

/**
 * The maximum backlog queue length.
 */
#define SOMAXCONN 128

/**
 * Control data truncated.
 */
#define MSG_CTRUNC 8

/**
 * Send without using routing tables.
 */
#define MSG_DONTROUTE 4

/**
 * Terminates a record (if supported by the protocol).
 */
#define MSG_EOR 128

/**
 * Out-of-band data.
 */
#define MSG_OOB 1

/**
 * Leave received data in queue.
 */
#define MSG_PEEK 2

/**
 * Normal data truncated.
 */
#define MSG_TRUNC 32

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/send.2.html>.
 *
 * Enables nonblocking operation.
 */
#define MSG_DONTWAIT 64

/**
 * Attempt to fill the read buffer.
 */
#define MSG_WAITALL 256

/**
 * No SIGPIPE generated when an attempt to send is made on a stream-oriented
 * socket that is no longer connected.
 */
#define MSG_NOSIGNAL 16384

/**
 * Atomically set the `FD_CLOEXEC` flag on any file descriptors created via
 * `SCM_RIGHTS` during `recvmsg()`.
 */
#define MSG_CMSG_CLOEXEC 1073741824

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/IP_ADD_SOURCE_MEMBERSHIP.2const.html>.
 *
 * Join a multicast group and allow receiving data only from a specified
 * source.
 */
#define IP_ADD_SOURCE_MEMBERSHIP 70

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/IP_DROP_SOURCE_MEMBERSHIP.2const.html>.
 *
 * Leave a source-specific multicast group.
 */
#define IP_DROP_SOURCE_MEMBERSHIP 71

/**
 * Non-POSIX, see <https://datatracker.ietf.org/doc/html/rfc3678#section-5.1.2>.
 *
 * Join a source-specific group.
 */
#define MCAST_JOIN_SOURCE_GROUP 46

/**
 * Non-POSIX, see <https://datatracker.ietf.org/doc/html/rfc3678#section-5.1.2>.
 *
 * Leave a source-specific group.
 */
#define MCAST_LEAVE_SOURCE_GROUP 47

/**
 * Internet domain sockets for use with IPv4 sockets.
 */
#define AF_INET 2

/**
 * Internet domain sockets for use with IPv6 sockets.
 */
#define AF_INET6 10

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/unix.7.html>.
 *
 * Alias for `AF_UNIX`.
 */
#define AF_LOCAL AF_UNIX

/**
 * UNIX domain sockets.
 */
#define AF_UNIX 1

/**
 * Unspecified.
 */
#define AF_UNSPEC 0

/**
 * See `AF_INET`.
 */
#define PF_INET 2

/**
 * See `AF_INET6`.
 */
#define PF_INET6 10

/**
 * See `AF_LOCAL`.
 */
#define PF_LOCAL PF_UNIX

/**
 * See `AF_UNIX`.
 */
#define PF_UNIX 1

/**
 * See `AF_UNSPEC`.
 */
#define PF_UNSPEC 0

/**
 * Disables further receive operations.
 */
#define SHUT_RD 0

/**
 * Disables further send and receive operations.
 */
#define SHUT_RDWR 2

/**
 * Disables further send operations.
 */
#define SHUT_WR 1

/**
 * Indicates that the data array contains the access rights to be sent or
 * received.
 */
#define SCM_RIGHTS 1

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man7/unix.7.html>.
 *
 * Send or receive UNIX credentials.
 */
#define SCM_CREDENTIALS 2

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
struct cmsghdr {
  /**
   * Data byte count, including the `cmsghdr`.
   */
  size_t cmsg_len;
  /**
   * Originating protocol.
   */
  int cmsg_level;
  /**
   * Protocol-specific type.
   */
  int cmsg_type;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
struct msghdr {
  /**
   * Optional address.
   */
  void *msg_name;
  /**
   * Size of address.
   */
  socklen_t msg_namelen;
  /**
   * Scatter/gather array.
   */
  struct iovec *msg_iov;
  /**
   * Members in `msg_iov`.
   */
  size_t msg_iovlen;
  /**
   * Ancilliary data.
   */
  void *msg_control;
  /**
   * Ancilliary data buffer length.
   */
  size_t msg_controllen;
  /**
   * Flags on received message.
   */
  int msg_flags;
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
struct sockaddr {
  /**
   * Address family.
   */
  sa_family_t sa_family;
  /**
   * Socket address.
   */
  char sa_data[14];
};

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
struct linger {
  /**
   * Indicates whether linger option is enabled.
   */
  int l_onoff;
  /**
   * Linger time, in seconds.
   */
  int l_linger;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
unsigned char *CMSG_DATA(const struct cmsghdr *cmsg);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
struct cmsghdr *CMSG_NXTHDR(const struct msghdr *mhdr, const struct cmsghdr *cmsg);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
struct cmsghdr *CMSG_FIRSTHDR(const struct msghdr *mhdr);

size_t CMSG_ALIGN(size_t len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
unsigned int CMSG_SPACE(unsigned int len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_socket.h.html>.
 */
unsigned int CMSG_LEN(unsigned int length);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/accept.html>.
 */
int accept(int socket, struct sockaddr *address, socklen_t *address_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/bind.html>.
 */
int bind(int socket, const struct sockaddr *address, socklen_t address_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/connect.html>.
 */
int connect(int socket, const struct sockaddr *address, socklen_t address_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpeername.html>.
 */
int getpeername(int socket, struct sockaddr *address, socklen_t *address_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getsockname.html>.
 */
int getsockname(int socket, struct sockaddr *address, socklen_t *address_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getsockopt.html>.
 */
int getsockopt(int socket, int level, int option_name, void *option_value, socklen_t *option_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/listen.html>.
 */
int listen(int socket, int backlog);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/recv.html>.
 */
ssize_t recv(int socket, void *buffer, size_t length, int flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/recvfrom.html>.
 */
ssize_t recvfrom(int socket,
                 void *buffer,
                 size_t length,
                 int flags,
                 struct sockaddr *address,
                 socklen_t *address_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/recvmsg.html>.
 */
ssize_t recvmsg(int socket, struct msghdr *msg, int flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/send.html>.
 */
ssize_t send(int socket, const void *message, size_t length, int flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sendmsg.html>.
 */
ssize_t sendmsg(int socket, const struct msghdr *msg, int flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sendto.html>.
 */
ssize_t sendto(int socket,
               const void *message,
               size_t length,
               int flags,
               const struct sockaddr *dest_addr,
               socklen_t dest_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/setsockopt.html>.
 */
int setsockopt(int socket,
               int level,
               int option_name,
               const void *option_value,
               socklen_t option_len);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/shutdown.html>.
 */
int shutdown(int socket, int how);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/socket.html>.
 */
int socket(int domain, int kind, int protocol);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/socketpair.html>.
 */
int socketpair(int domain, int kind, int protocol, int *sv);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_SOCKET_H */

#ifndef _RELIBC_BITS_SYS_SOCKET_H
#define _RELIBC_BITS_SYS_SOCKET_H

struct sockaddr_storage {
    sa_family_t ss_family;
    char __ss_padding[128-sizeof(long)-sizeof(sa_family_t)];
    unsigned long __ss_align;
};

// These definitions were taken from musl, license MIT {
#define __CMSG_LEN(cmsg) (((cmsg)->cmsg_len + sizeof(long) - 1) & ~(long)(sizeof(long) - 1))
#define __CMSG_NEXT(cmsg) ((unsigned char *)(cmsg) + __CMSG_LEN(cmsg))
#define __MHDR_END(mhdr) ((unsigned char *)(mhdr)->msg_control + (mhdr)->msg_controllen)

#define CMSG_DATA(cmsg) ((unsigned char *) (((struct cmsghdr *)(cmsg)) + 1))
#define CMSG_NXTHDR(mhdr, cmsg) ((cmsg)->cmsg_len < sizeof (struct cmsghdr) || __CMSG_LEN(cmsg) + sizeof(struct cmsghdr) >= __MHDR_END(mhdr) - (unsigned char *)(cmsg) ? 0 : (struct cmsghdr *)__CMSG_NEXT(cmsg))
#define CMSG_FIRSTHDR(mhdr) ((size_t) (mhdr)->msg_controllen >= sizeof (struct cmsghdr) ? (struct cmsghdr *) (mhdr)->msg_control : (struct cmsghdr *) 0)

#define CMSG_ALIGN(len) (((len) + sizeof (size_t) - 1) & (size_t) ~(sizeof (size_t) - 1))
#define CMSG_SPACE(len) (CMSG_ALIGN (len) + CMSG_ALIGN (sizeof (struct cmsghdr)))
#define CMSG_LEN(len)   (CMSG_ALIGN (sizeof (struct cmsghdr)) + (len))
// } from musl, license MIT

#endif // _RELIBC_BITS_SYS_SOCKET_H
