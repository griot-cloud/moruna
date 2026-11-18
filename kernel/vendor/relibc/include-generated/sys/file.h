#ifndef _RELIBC_SYS_FILE_H
#define _RELIBC_SYS_FILE_H

/**
 * Place a shared lock.
 */
#define LOCK_SH 1

/**
 * Place an exclusive lock.
 */
#define LOCK_EX 2

/**
 * To make a nonblocking request, include `LOCK_NB` (by ORing) with any of
 * `LOCK_SH`, `LOCK_EX` and `LOCK_UN`.
 */
#define LOCK_NB 4

/**
 * Remove an existing lock held by this process.
 */
#define LOCK_UN 8

#define L_SET 0

#define L_INCR 1

#define L_XTND 2

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://man7.org/linux/man-pages/man2/flock.2.html>.
 *
 * Apply or remove an advisory lock on an open file.
 */
int flock(int fd, int operation);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_FILE_H */
