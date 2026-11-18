#ifndef _RELIBC_GLOB_H
#define _RELIBC_GLOB_H

#include <bits/size-t.h> // for size_t from sys/types.h


#define GLOB_ERR 1

#define GLOB_MARK 2

#define GLOB_NOSORT 4

#define GLOB_DOOFFS 8

#define GLOB_NOCHECK 16

#define GLOB_APPEND 32

#define GLOB_NOESCAPE 64

#define GLOB_PERIOD 128

#define GLOB_NOSPACE 1

#define GLOB_ABORTED 2

#define GLOB_NOMATCH 3

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/glob.h.html>.
 */
typedef struct {
  size_t gl_pathc;
  size_t gl_offs;
  char **gl_pathv;
  void *__opaque;
} glob_t;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/glob.html>.
 */
int glob(const char *pattern,
         int flags,
         int (*errfunc)(const char *epath, int eerrno),
         glob_t *pglob);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/glob.html>.
 */
void globfree(glob_t *pglob);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_GLOB_H */
