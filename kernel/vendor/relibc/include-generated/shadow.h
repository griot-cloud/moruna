#ifndef _RELIBC_SHADOW_H
#define _RELIBC_SHADOW_H

#include <stdint.h>
#include <stdio.h>
#include <bits/size-t.h> // for size_t from sys/types.h


/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getspnam.3.html>.
 */
struct spwd {
  char *sp_namp;
  char *sp_pwdp;
  long sp_lstchg;
  long sp_min;
  long sp_max;
  long sp_warn;
  long sp_inact;
  long sp_expire;
  unsigned long sp_flag;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getspnam.3.html>.
 */
struct spwd *getspnam(const char *name);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getspnam.3.html>.
 */
int getspnam_r(const char *name,
               struct spwd *result_buf,
               char *buffer,
               size_t buflen,
               struct spwd **result);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getspnam.3.html>.
 */
void setspent(void);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getspnam.3.html>.
 */
void endspent(void);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getspnam.3.html>.
 */
struct spwd *getspent(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SHADOW_H */
