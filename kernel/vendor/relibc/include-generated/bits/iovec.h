#ifndef _RELIBC_BITS_IOVEC_H
#define _RELIBC_BITS_IOVEC_H

#include <bits/size-t.h> // for size_t from sys/types.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_uio.h.html>.
 */
struct iovec {
  /**
   * Base address of a memory region for input or output.
   */
  void *iov_base;
  /**
   * The size of the memory pointed to by `iov_base`.
   */
  size_t iov_len;
};

#endif  /* _RELIBC_BITS_IOVEC_H */
