#ifndef _RELIBC_BITS_LIMITS_PTDI_H
#define _RELIBC_BITS_LIMITS_PTDI_H

/**
 * Minimum required value for `PTHREAD_DESTRUCTOR_ITERATIONS`.
 */
#define _POSIX_THREAD_DESTRUCTOR_ITERATIONS 4

/**
 * Maximum number of attempts made to destroy a thread's thread-specific data
 * values on thread exit.
 */
#define PTHREAD_DESTRUCTOR_ITERATIONS _POSIX_THREAD_DESTRUCTOR_ITERATIONS

#endif  /* _RELIBC_BITS_LIMITS_PTDI_H */
