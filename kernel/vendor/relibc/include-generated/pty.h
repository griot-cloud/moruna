#ifndef _RELIBC_PTY_H
#define _RELIBC_PTY_H

#include <sys/ioctl.h>
#include <termios.h>

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://www.man7.org/linux/man-pages/man3/openpty.3.html>.
 */
int openpty(int *amaster,
            int *aslave,
            char *namep,
            const struct termios *termp,
            const struct winsize *winp);

/**
 * See <https://www.man7.org/linux/man-pages/man3/openpty.3.html>.
 */
int forkpty(int *pm, char *name, const struct termios *tio, const struct winsize *ws);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_PTY_H */
