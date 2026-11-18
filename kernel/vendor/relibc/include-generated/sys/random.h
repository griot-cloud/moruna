#ifndef _RELIBC_SYS_RANDOM_H
#define _RELIBC_SYS_RANDOM_H

#include <bits/size-t.h>  // for size_t from sys/types.h
#include <bits/ssize-t.h> // for ssize_t from sys/types.h


/**
 * See <https://www.man7.org/linux/man-pages/man2/getrandom.2.html>.
 *
 * Do not block when requesting random bytes.
 * Will set `errno` to `EAGAIN` if requested entropy is not available.
 */
#define GRND_NONBLOCK 1

/**
 * See <https://www.man7.org/linux/man-pages/man2/getrandom.2.html>.
 *
 * If this bit is set, then random bytes are drawn from the `random` source
 * instead of the `urandom` source.
 */
#define GRND_RANDOM 2

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://www.man7.org/linux/man-pages/man2/getrandom.2.html>.
 *
 * Fills the buffer pointed to by `buf` with up to `buflen` random bytes.
 */
ssize_t getrandom(void *buf, size_t buflen, unsigned int flags);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_RANDOM_H */
