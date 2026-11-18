#ifndef _RELIBC_SYS_UN_H
#define _RELIBC_SYS_UN_H

#include <bits/safamily-t.h> // for sa_family_t from sys/socket.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/sys_un.h.html>.
 *
 * # Implementation
 * Size of `sun_path` is `108` bytes.
 */
struct sockaddr_un {
  /**
   * Address family.
   */
  sa_family_t sun_family;
  /**
   * Socket pathname storage.
   */
  char sun_path[108];
};

#endif  /* _RELIBC_SYS_UN_H */
