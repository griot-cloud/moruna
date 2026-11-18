#ifndef _RELIBC_BITS_TIMESPEC_H
#define _RELIBC_BITS_TIMESPEC_H

#include <bits/time-t.h>


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/time.h.html>.
 */
struct timespec {
  /**
   * Whole seconds.
   */
  time_t tv_sec;
  /**
   * Nanoseconds.
   */
  long tv_nsec;
};

#endif  /* _RELIBC_BITS_TIMESPEC_H */
