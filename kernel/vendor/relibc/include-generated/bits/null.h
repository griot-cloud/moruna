#ifndef _RELIBC_BITS_NULL_T_H
#define _RELIBC_BITS_NULL_T_H

// Null pointer constant.
// Any pointer object whose representation has all bits set to zero, perhaps by
// memset() to 0 or by calloc(), shall be treated as a null pointer.
#ifdef __cplusplus
#if __cplusplus >= 201103L
  #define NULL nullptr
#else
  #define NULL 0L
#endif
#else
  #define NULL ((void *)0)
#endif


#endif  /* _RELIBC_BITS_NULL_T_H */
