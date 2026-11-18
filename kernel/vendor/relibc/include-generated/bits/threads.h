#ifndef _RELIBC_BITS_THREADS_H
#define _RELIBC_BITS_THREADS_H

/**
 * Used for condition variables.
 */
typedef union pthread_cond_t {
  unsigned char __relibc_internal_size[8];
  int __relibc_internal_align;
} pthread_cond_t;

/**
 * Used for thread-specific data keys.
 */
typedef unsigned long pthread_key_t;

/**
 * Used for mutexes.
 */
typedef union pthread_mutex_t {
  unsigned char __relibc_internal_size[12];
  int __relibc_internal_align;
} pthread_mutex_t;

#endif  /* _RELIBC_BITS_THREADS_H */
