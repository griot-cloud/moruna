#ifndef _RELIBC_SYS_AUXV_H
#define _RELIBC_SYS_AUXV_H

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://www.man7.org/linux/man-pages/man3/getauxval.3.html>.
 */
unsigned long getauxval(unsigned long _t);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_AUXV_H */
