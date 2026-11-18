#ifndef _RELIBC_POLL_H
#define _RELIBC_POLL_H

#include <bits/sigset-t.h> // for sigset_t from signal.h
#include <bits/timespec.h> // for timespec from time.h


/**
 * Data other than high-priority data may be read without blocking.
 */
#define POLLIN 1

/**
 * High-priority data may be read without blocking.
 */
#define POLLPRI 2

/**
 * Normal data may be written without blocking.
 */
#define POLLOUT 4

/**
 * An error has occurred (revents only).
 */
#define POLLERR 8

/**
 * Device has been disconnected (revents only).
 */
#define POLLHUP 16

/**
 * Invalid fd member (revents only).
 */
#define POLLNVAL 32

/**
 * Normal data may be read without blocking.
 */
#define POLLRDNORM 64

/**
 * Priority data may be read without blocking.
 */
#define POLLRDBAND 128

/**
 * Equivalent to POLLOUT.
 */
#define POLLWRNORM 256

/**
 * Priority data may be written.
 */
#define POLLWRBAND 512

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/poll.h.html>.
 *
 * A structure storing a file descriptor along with input and output flags for poll operations.
 */
struct pollfd {
  /**
   * The following descriptor being polled.
   */
  int fd;
  /**
   * The input event flags.
   */
  short events;
  /**
   * The output event flags.
   */
  short revents;
};

/**
 * An unsigned integer type used for the number of file descriptors.
 */
typedef unsigned long nfds_t;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/poll.html>.
 *
 * Provides applications with a mechanism for multiplexing input/output
 * over a set of file descriptors.
 *
 * - The `timeout` parameter represents milliseconds.
 * - A `timeout` of `-1` is equivalent to passing a null pointer for `tmo_p` to `ppoll`.
 * - `poll` should behave equivalent to `ppoll` with a null pointer for `sigmask`.
 *
 * Note: Uses epoll internally.
 */
int poll(struct pollfd *fds, nfds_t nfds, int timeout);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/ppoll.html>.
 *
 * Provides applications with a mechanism for multiplexing input/output
 * over a set of file descriptors.
 *
 * - The `tmo_p` parameter is the timeout as represented by a `timespec` struct.
 * - Passing a null pointer for timeout is equivalent to `-1` for `timeout` to `poll`.
 *
 * Note: Uses epoll internally.
 */
int ppoll(struct pollfd *fds, nfds_t nfds, const struct timespec *tmo_p, const sigset_t *sigmask);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_POLL_H */
