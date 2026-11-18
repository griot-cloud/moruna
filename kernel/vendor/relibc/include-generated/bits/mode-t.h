#ifndef _RELIBC_BITS_MODE_T_H
#define _RELIBC_BITS_MODE_T_H

#if (defined(__linux__) || defined(__moruna__))
/**
 * Used for some file attributes.
 */
typedef unsigned int mode_t;
#endif

#if !(defined(__linux__) || defined(__moruna__))
/**
 * Used for some file attributes.
 */
typedef int mode_t;
#endif

#endif  /* _RELIBC_BITS_MODE_T_H */
