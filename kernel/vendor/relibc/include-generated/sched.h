#ifndef _RELIBC_SCHED_H
#define _RELIBC_SCHED_H

#include <bits/pid-t.h>    // for pid_t from sys/types
#include <bits/timespec.h> // for timespec from time.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sched.h.html>.
 *
 * First in first out (FIFO) scheduling policy.
 */
#define SCHED_FIFO 0

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sched.h.html>.
 *
 * Round robin scheduling policy.
 */
#define SCHED_RR 1

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sched.h.html>.
 *
 * Another scheduling policy.
 */
#define SCHED_OTHER 2

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sched.h.html>.
 *
 * Scheduling parameters required for each supported scheduling policy.
 */
struct sched_param {
  /**
   * Process or thread execution scheduling priority.
   */
  int sched_priority;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sched_yield.html>.
 *
 * Force the running thread to relinquish the processor until it again
 * becomes the head of its thread list.
 */
int sched_yield(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SCHED_H */
