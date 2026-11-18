#ifndef _RELIBC_UTMP_H
#define _RELIBC_UTMP_H

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://www.man7.org/linux/man-pages/man3/openpty.3.html>.
 */
int login_tty(int fd);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_UTMP_H */
