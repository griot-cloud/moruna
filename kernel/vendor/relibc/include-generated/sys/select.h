#ifndef _RELIBC_SYS_SELECT_H
#define _RELIBC_SYS_SELECT_H

#include <bits/sigset-t.h> // for sigset_t from signal.h
#include <bits/timespec.h> // for timespec from time.h
#include <bits/timeval.h>  // for timeval

// from musl license MIT {
#define FD_SETSIZE 1024

typedef unsigned long fd_mask;

typedef struct {
    unsigned long fds_bits[FD_SETSIZE / 8 / sizeof(long)];
} fd_set;

// Initializes the descriptor set pointed to by `s` to the null set.
#define FD_ZERO(s) do { int __i; unsigned long *__b=(s)->fds_bits; for(__i=sizeof (fd_set)/sizeof (long); __i; __i--) *__b++=0; } while(0)
// Adds the file descriptor `d` to the set pointed to by `s`.
#define FD_SET(d, s)   ((s)->fds_bits[(d)/(8*sizeof(long))] |= (1UL<<((d)%(8*sizeof(long)))))
// Removes the file descriptor `d` from the set pointed to by `s`.
#define FD_CLR(d, s)   ((s)->fds_bits[(d)/(8*sizeof(long))] &= ~(1UL<<((d)%(8*sizeof(long)))))
// Evaluates to non-zero if the file descriptor `d` is a member of the set pointed to by `s`, otherwise evaluates to zero.
#define FD_ISSET(d, s) !!((s)->fds_bits[(d)/(8*sizeof(long))] & (1UL<<((d)%(8*sizeof(long)))))

#if defined(_GNU_SOURCE) || defined(_BSD_SOURCE)
#define NFDBITS (8*(int)sizeof(long))
#endif
// } from musl license MIT


#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/select.html>.
 *
 * Examines the file descriptor sets whose addresses are passed in the
 * `readfds`, `writefds` and `errorfds` parameters to see whether some of
 * their descriptors are ready for reading, ready for writing, or have an
 * exceptional condition pending, respectively.
 *
 * `timeout` is given in seconds and microseconds as represented by `timeval`.
 * Behaves as `pselect()` does when `sigmask` is a null pointer. When
 * successful, may modify the object pointed to by `timeout`.
 *
 * Upon success, returns the total number of bits set in the bit masks. Upon
 * failure, returns `-1` and sets errno to indicate the error.
 */
int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds, struct timeval *timeout);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pselect.html>.
 *
 * Examines the file descriptor sets whose addresses are passed in the
 * `readfds`, `writefds` and `errorfds` parameters to see whether some of
 * their descriptors are ready for reading, ready for writing, or have an
 * exceptional condition pending, respectively.
 *
 * `timeout` is given in seconds and nanoseconds as represented by `timespec`.
 *
 * Upon success, returns the total number of bits set in the bit masks. Upon
 * failure, returns `-1` and sets errno to indicate the error.
 */
int pselect(int nfds,
            fd_set *readfds,
            fd_set *writefds,
            fd_set *exceptfds,
            const struct timespec *timeout,
            const sigset_t *sigmask);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_SELECT_H */
