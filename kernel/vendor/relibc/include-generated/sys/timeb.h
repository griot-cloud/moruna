#ifndef _RELIBC_SYS_TIMEB_H
#define _RELIBC_SYS_TIMEB_H

#include <features.h>
#include <bits/time-t.h> // for time_t from sys/types.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/009695399/basedefs/sys/timeb.h.html>.
 */
struct timeb {
  /**
   * The seconds portion of the current time.
   */
  time_t time;
  /**
   * The milliseconds portion of the current time.
   */
  unsigned short millitm;
  /**
   * The local timezone in minutes west of Greenwich.
   */
  short timezone;
  /**
   * TRUE if Daylight Savings Time is in effect.
   */
  short dstflag;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/009695399/functions/ftime.html>.
 *
 * Sets the `time` and `millitm` members of the `timeb` structure pointed to
 * by `tp` to contain the seconds and milliseconds portions, respectively,
 * of the current time in seconds since the Epoch.
 *
 * # Safety
 * The caller must ensure that `tp` is convertible to an [`Out<timeb>`].
 *
 * # Deprecation
 * The `ftime()` function was marked as legacy in the Open Group Base
 * Specifications Issue 6, and the entire `sys/timeb.h` header was removed in
 * Issue 7.
 */
__deprecated int ftime(struct timeb *tp);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_TIMEB_H */
