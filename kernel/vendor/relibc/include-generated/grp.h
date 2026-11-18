#ifndef _RELIBC_GRP_H
#define _RELIBC_GRP_H

#include <bits/gid-t.h>  // for gid_t from sys/types.h
#include <bits/size-t.h> // for size_t from sys/types.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/grp.h.html>.
 */
struct group {
  /**
   * The name of the group.
   */
  char *gr_name;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getgrnam.3.html>.
   *
   * Group password.
   */
  char *gr_passwd;
  /**
   * Numerical group ID.
   */
  gid_t gr_gid;
  /**
   * Pointer to a null-terminated array of character pointers to member
   * names.
   */
  char **gr_mem;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * MT-Unsafe race:grgid locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getgrgid.html>.
 *
 * Searches the group database for an entry with a matching `gid`.
 */
struct group *getgrgid(gid_t gid);

/**
 * MT-Unsafe race:grnam locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getgrnam.html>.
 *
 * Searches the group database for an entry with a matching `name`.
 */
struct group *getgrnam(const char *name);

/**
 * MT-Safe locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getgrgid_r.html>.
 *
 * Updates the `group` structure pointed to by `result_buf` and store a
 * pointer to that structure at the location pointed to by `result`.
 */
int getgrgid_r(gid_t gid,
               struct group *result_buf,
               char *buffer,
               size_t buflen,
               struct group **result);

/**
 * MT-Safe locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/getgrnam_r.html>.
 *
 * Updates the `group` structure pointed to by `result_buf` and store a
 * pointer to that structure at the location pointed to by `result`.
 */
int getgrnam_r(const char *name,
               struct group *result_buf,
               char *buffer,
               size_t buflen,
               struct group **result);

/**
 * MT-Unsafe race:grent race:grentbuf locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endgrent.html>.
 *
 * Returns a pointer to a structure containing the broken-out fields of an
 * entry in the group database.
 */
struct group *getgrent(void);

/**
 * MT-Unsafe race:grent locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endgrent.html>.
 *
 * Closes the group database.
 */
void endgrent(void);

/**
 * MT-Unsafe race:grent locale
 *
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/endgrent.html>.
 *
 * Rewinds the group database so that the next `getgrent()` call returns the
 * first entry, allowing repeated searches.
 */
void setgrent(void);

/**
 * MT-Safe locale
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getgrouplist.3.html>.
 *
 * Get a list of groups to which a user belongs.
 */
int getgrouplist(const char *user, gid_t _group, gid_t *groups, int *ngroups);

/**
 * MT-Safe locale
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/initgroups.3.html>.
 *
 * Initialize the supplementary group access list.
 */
int initgroups(const char *user, gid_t gid);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/setgroups.2.html>.
 *
 * Set a list of supplementary group IDs.
 */
int setgroups(size_t size, const gid_t *list);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_GRP_H */
