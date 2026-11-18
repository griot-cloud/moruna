#ifndef _RELIBC_SYS_RESOURCE_H
#define _RELIBC_SYS_RESOURCE_H

#include <bits/id-t.h>    // for id_t from sys/types.h
#include <bits/timeval.h> // for timeval from sys/select.h (previously in sys/time.h)


/**
 * Returns information about the current process.
 */
#define RUSAGE_SELF 0

/**
 * Returns information about children of the current process.
 */
#define RUSAGE_CHILDREN -1

/**
 * Non-POSIX.
 *
 * Return resource consumption statistics for both the current process and
 * all of its terminated child processes that have been waited for.
 */
#define RUSAGE_BOTH -2

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
 *
 * Return resource usage statistics for the calling thread.
 */
#define RUSAGE_THREAD 1

/**
 * A value of `rlim_t` indicating no limit.
 */
#define RLIM_INFINITY 18446744073709551615ull

/**
 * A value of type `rlim_t` indicating an unrepresentable saved soft limit.
 */
#define RLIM_SAVED_CUR RLIM_INFINITY

/**
 * A value of type `rlim_t` indicating an unrepresentable saved hard limit.
 */
#define RLIM_SAVED_MAX RLIM_INFINITY

/**
 * Limit on CPU time per process.
 */
#define RLIMIT_CPU 0

/**
 * Limit on file size.
 */
#define RLIMIT_FSIZE 1

/**
 * Limit on data segment size.
 */
#define RLIMIT_DATA 2

/**
 * Limit on stack size.
 */
#define RLIMIT_STACK 3

/**
 * Limit on size of core image.
 */
#define RLIMIT_CORE 4

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is a limit (in bytes) on the process's resident set (the number of
 * virtual pages resident in RAM).
 * Only affects Linux 2.4.0 to 2.4.29.
 */
#define RLIMIT_RSS 5

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is a limit on the number of extant processes (or, more preciselu on
 * Linux, threads) for the real user ID of the calling process.
 */
#define RLIMIT_NPROC 6

/**
 * Limit on number of open files.
 */
#define RLIMIT_NOFILE 7

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is the maximum number of bytes of memory that may be locked into RAM.
 */
#define RLIMIT_MEMLOCK 8

/**
 * Limit on address space size.
 */
#define RLIMIT_AS 9

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is a limit on the combined number of `flock(2)` locks and `fcntl(2)`
 * leases that this process may establish.
 * Only affects Linux 2.4.0 to 2.4.24.
 */
#define RLIMIT_LOCKS 10

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is a limit on the number of signals that may be queued for the real
 * user ID of the calling process.
 */
#define RLIMIT_SIGPENDING 11

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is a limit on the number of bytes that can be allocated for POSIX
 * message queues for the real user ID of the calling process.
 */
#define RLIMIT_MSGQUEUE 12

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This specifies a ceiling to which the process's nice value can be raised
 * using `setpriority(2)` or `nice(2)`.
 */
#define RLIMIT_NICE 13

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This specifies a ceiling on the real-time priority that may be set for
 * this process using `sched_setscheduler(2)` and `sched_setparam(2)`.
 */
#define RLIMIT_RTPRIO 14

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrlimit.2.html>.
 *
 * This is a limit (in microseconds) on the amount of CPU time
 * that a process scheduled under a real-time scheduling
 * policy may consume without making a blocking system call.
 * For the purpose of this limit, each time a process makes a
 * blocking system call, the count of its consumed CPU time is
 * reset to zero.  The CPU time count is not reset if the
 * process continues trying to use the CPU but is preempted,
 * its time slice expires, or it calls sched_yield(2).
 */
#define RLIMIT_RTTIME 15

/**
 * Non-POSIX, found in glibc.
 *
 * Number of limit flavors.
 */
#define RLIMIT_NLIMITS 16

/**
 * Identifies the `who` argument as a process ID.
 */
#define PRIO_PROCESS 0

/**
 * Identifies the `who` argument as a process group ID.
 */
#define PRIO_PGRP 1

/**
 * Identifies the `who` argument as a user ID.
 */
#define PRIO_USER 2

/**
 * Unsigned integer type used for limit values.
 */
typedef unsigned long long rlim_t;

struct rlimit {
  /**
   * The current (soft) limit.
   */
  rlim_t rlim_cur;
  /**
   * The hard limit.
   */
  rlim_t rlim_max;
};

struct rusage {
  /**
   * User time used.
   */
  struct timeval ru_utime;
  /**
   * System time used.
   */
  struct timeval ru_stime;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Maximum resident set size.
   */
  long ru_maxrss;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Integral shared memory size.
   */
  long ru_ixrss;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Integral unshared data size.
   */
  long ru_idrss;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Integral unshared stack size.
   */
  long ru_isrss;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Page reclaims (soft page faults).
   */
  long ru_minflt;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Page faults (hard page faults).
   */
  long ru_majflt;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Swaps.
   */
  long ru_nswap;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Block input operations.
   */
  long ru_inblock;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Block output operations.
   */
  long ru_oublock;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * IPC messages sent.
   */
  long ru_msgsnd;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * IPC messages received.
   */
  long ru_msgrcv;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Signals received.
   */
  long ru_nsignals;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Voluntary context switches.
   */
  long ru_nvcsw;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/getrusage.2.html>.
   *
   * Involuntary context switches.
   */
  long ru_nivcsw;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpriority.html>.
 *
 * Obtains the nice value of a process, process group or user.
 *
 * A value of `0` for `who` indicates the current process, process group or
 * user.
 *
 * A return value of `-1` can either be a valid nice value or indicate that
 * an error occurred. To distinguish between them, first set `errno` to `0`
 * before calling `getpriority()`, then check the value of `errno` after the
 * call to `getpriority()`.
 */
int getpriority(int which, id_t who);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/setpriority.html>.
 *
 * Sets the nice value of a process, process group or user.
 *
 * A value of `0` for `who` indicates the current process, process group or
 * user.
 *
 * A return value of `0` indicates success, `-1` indicates error.
 */
int setpriority(int which, id_t who, int nice);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getrlimit.html>.
 *
 * Stores the value of the limit indicated by `resource` into the `rlimit`
 * struct pointed to by `rlp`.
 *
 * A return value of `0` indicates success, `-1` indicates error.
 */
int getrlimit(int resource, struct rlimit *rlp);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/setrlimit.html>.
 *
 * Sets the value of the limit indicated by `resource` to the value specified
 * in the `rlimit` struct pointed to by `rlp`.
 *
 * A return value of `0` indicates success, `-1` indicates error.
 */
int setrlimit(int resource, const struct rlimit *rlp);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getrusage.html>.
 *
 * Stores the measures of the resources used by the current process or its
 * terminated and waited-for child processes in the `rusage` struct pointed
 * to by `r_usage`.
 *
 * A return value of `0` indicates success, `-1` indicates error.
 */
int getrusage(int who, struct rusage *r_usage);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_RESOURCE_H */
