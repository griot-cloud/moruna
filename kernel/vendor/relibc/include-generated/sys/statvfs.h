#ifndef _RELIBC_SYS_STATVFS_H
#define _RELIBC_SYS_STATVFS_H

#include <bits/sys/statvfs.h> // for fsblkcnt_t and fsfilcnt_t from sys/types.h


struct statvfs {
  unsigned long f_bsize;
  unsigned long f_frsize;
  fsblkcnt_t f_blocks;
  fsblkcnt_t f_bfree;
  fsblkcnt_t f_bavail;
  fsfilcnt_t f_files;
  fsfilcnt_t f_ffree;
  fsfilcnt_t f_favail;
  unsigned long f_fsid;
  unsigned long f_flag;
  unsigned long f_namemax;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/fstatvfs.html>.
 */
int fstatvfs(int fildes, struct statvfs *buf);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/fstatvfs.html>.
 */
int statvfs(const char *file, struct statvfs *buf);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_STATVFS_H */
