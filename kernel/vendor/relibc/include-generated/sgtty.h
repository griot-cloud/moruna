#ifndef _RELIBC_SGTTY_H
#define _RELIBC_SGTTY_H

#include <sys/ioctl.h>

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

int gtty(int fd, struct sgttyb *out);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SGTTY_H */
