#ifndef _RELIBC_PWD_H
#define _RELIBC_PWD_H

#include <bits/gid-t.h>  // for gid_t from sys/types.h
#include <bits/size-t.h> // for size_t from sys/types.h
#include <bits/uid-t.h>  // for uid_t from sys/types.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/pwd.h.html>
 * for POSIX minimum requirements, and
 * <https://www.man7.org/linux/man-pages/man3/getpwnam.3.html> for further
 * details.
 */
struct passwd {
  char *pw_name;
  char *pw_passwd;
  uid_t pw_uid;
  gid_t pw_gid;
  char *pw_gecos;
  char *pw_dir;
  char *pw_shell;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endpwent.html>.
 */
void endpwent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endpwent.html>.
 */
struct passwd *getpwent(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpwnam.html>.
 */
struct passwd *getpwnam(const char *name);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpwnam.html>.
 */
int getpwnam_r(const char *name,
               struct passwd *out,
               char *buf,
               size_t size,
               struct passwd **result);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpwuid.html>.
 */
struct passwd *getpwuid(uid_t uid);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpwuid.html>.
 */
int getpwuid_r(uid_t uid, struct passwd *out, char *buf, size_t size, struct passwd **result);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endpwent.html>.
 */
void setpwent(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_PWD_H */
