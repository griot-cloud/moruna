#ifndef _RELIBC_BITS_SOCKLEN_T_H
#define _RELIBC_BITS_SOCKLEN_T_H

#include <bits/uint32-t.h>

/**
 * An integer type of at least 32bits width.
 *
 * Used in various length representations relating to sockets.
 */
typedef uint32_t socklen_t;

#endif  /* _RELIBC_BITS_SOCKLEN_T_H */
