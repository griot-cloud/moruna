#ifndef _RELIBC_SYS_EPOLL_H
#define _RELIBC_SYS_EPOLL_H

#include <bits/sigset-t.h> // for sigset_t from signal.h


#if (defined(__linux__) || defined(__moruna__))
/**
 * Set the close-on-exec (`FD_CLOEXEC`) flag on the new file descriptor.
 */
#define EPOLL_CLOEXEC 524288
#endif

#if defined(__redox__)
/**
 * Set the close-on-exec (`FD_CLOEXEC`) flag on the new file descriptor.
 */
#define EPOLL_CLOEXEC 16777216
#endif

/**
 * The associated file is available for read operations.
 */
#define EPOLLIN 1

/**
 * There is an exceptional condition on the file descriptor.
 */
#define EPOLLPRI 2

/**
 * The associated file is available for write operations.
 */
#define EPOLLOUT 4

/**
 * Error condition happened on the associated file descriptor.
 */
#define EPOLLERR 8

/**
 * Hang up happened onthe associated file descriptor.
 */
#define EPOLLHUP 16

#define EPOLLNVAL 32

#define EPOLLRDNORM 64

#define EPOLLRDBAND 128

#define EPOLLWRNORM 256

#define EPOLLWRBAND 512

#define EPOLLMSG 1024

/**
 * Stream socket peer closed connection, or shut down writing half of
 * connection.
 */
#define EPOLLRDHUP 8192

/**
 * Sets an exclusive wakeup mode for the epoll file descriptor that is being
 * attached to the target file descriptor, `fd`.
 */
#define EPOLLEXCLUSIVE (1 << 28)

/**
 * If `EPOLLONESHOT` and `EPOLLET` are clear and the process has the
 * `CAP_BLOCK_SUSPEND` capability, ensure that the system does not enter
 * "suspend" or "hibernate" while this event is pending or being processed.
 */
#define EPOLLWAKEUP (1 << 29)

/**
 * Requests one-shot notification for the associated file descriptor.
 */
#define EPOLLONESHOT (1 << 30)

/**
 * Requests edge-triggered notification for the associated file descriptor.
 */
#define EPOLLET (1 << 31)

/**
 * Add an entry to the interest list of the epoll file descriptor, `epfd`.
 */
#define EPOLL_CTL_ADD 1

/**
 * Remove (deregister) the target file descriptor `fd` from the interest list.
 */
#define EPOLL_CTL_DEL 2

/**
 * Change the settings associated with `fd` in the interest list to the new
 * settings specified in `event`.
 */
#define EPOLL_CTL_MOD 3

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man3/epoll_event.3type.html>.
 */
union epoll_data {
  void *ptr;
  int fd;
  unsigned int u32;
  unsigned long long u64;
};

#if (defined(__redox__) && defined(__LP64__))
/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man3/epoll_event.3type.html>.
 *
 * Specifies data that the kernel should save and return when the
 * corresponding file descriptor becomes ready.
 */
struct epoll_event {
  /**
   * Epoll events.
   */
  unsigned int events;
  /**
   * User data variable.
   */
  union epoll_data data;
  unsigned long long _pad;
};
#endif

#if !(defined(__redox__) && defined(__LP64__))
/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man3/epoll_event.3type.html>.
 *
 * Specifies data that the kernel should save and return when the
 * corresponding file descriptor becomes ready.
 */
struct epoll_event {
  /**
   * Epoll events.
   */
  unsigned int events;
  /**
   * User data variable.
   */
  union epoll_data data;
};
#endif

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man2/epoll_create.2.html>.
 *
 * Creates a file descriptor referring to the new epoll instance.
 *
 * Upon success, returns a file descriptor (a nonnegative integer). Upon
 * error, `-1` is returned and errno set to indicate the error.
 *
 * # Implementation
 * The `_size` parameter is deliberately unused as it is no longer required.
 * This function simply calls `epoll_create1`.
 */
int epoll_create(int _size);

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man2/epoll_create1.2.html>.
 *
 * Creates a file descriptor referring to the new epoll instance.
 *
 * Upon success, returns a file descriptor (a nonnegative integer). Upon
 * error, `-1` is returned and errno set to indicate the error.
 */
int epoll_create1(int flags);

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man2/epoll_ctl.2.html>.
 *
 * Add, modify, or remove entries in the interest list of the epoll instance
 * referred to by the file descriptor `epfd`.
 *
 * Upon success, returns `0`. Upon error, returns `-1` and sets errno to
 * indicate the error.
 */
int epoll_ctl(int epfd, int op, int fd, struct epoll_event *event);

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man2/epoll_wait.2.html>.
 *
 * Waits for events on the epoll instance referred to by the file descriptor
 * `epdf`.
 *
 * `timeout` represents the number of milliseconds this function call will
 * block.
 *
 * Will block until either:
 * - a file descriptor delivers an event
 * - the call is interrupted by a signal handler
 * - the `timeout` expires
 *
 * A `timeout` of `-1` will block indefinitely. A `timeout` of `0` will return
 * immediately, even if no events are available.
 *
 * Upon success, returns the number of file descriptors ready for the
 * requested I/O operation, or `0` if no file descriptor became ready during
 * the requested `timeout`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int epoll_wait(int epfd, struct epoll_event *events, int maxevents, int timeout);

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man2/epoll_wait.2.html>.
 *
 * Allows an application to safely wait until either a file descriptor becomes
 * ready or a signal is caught.
 *
 * If `sigmask` is NULL, calling this function is equivalent to
 * `epoll_wait()`.
 *
 * Upon success, returns the number of file descriptors ready for the
 * requested I/O operation, or `0` if no file descriptor became ready during
 * the requested `timeout`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int epoll_pwait(int epfd,
                struct epoll_event *events,
                int maxevents,
                int timeout,
                const sigset_t *sigmask);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_EPOLL_H */
