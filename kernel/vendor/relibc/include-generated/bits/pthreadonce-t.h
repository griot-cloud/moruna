#ifndef _RELIBC_BITS_PTHREAD_ONCE_T_H
#define _RELIBC_BITS_PTHREAD_ONCE_T_H

/**
 * Used for dynamic package initialization.
 */
typedef union pthread_once_t {
  unsigned char __relibc_internal_size[4];
  int __relibc_internal_align;
} pthread_once_t;

#endif  /* _RELIBC_BITS_PTHREAD_ONCE_T_H */
