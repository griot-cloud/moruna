#ifndef _RELIBC_DLFCN_H
#define _RELIBC_DLFCN_H

/**
 * Relocations are performed at an implementation-defined time.
 */
#define RTLD_LAZY (1 << 0)

/**
 * Relocations are performed when the object is loaded.
 */
#define RTLD_NOW (1 << 1)

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/dlopen.3.html>.
 * Don't load the shared object.
 */
#define RTLD_NOLOAD (1 << 2)

/**
 * All symbols are available for relocation processing of other modules.
 */
#define RTLD_GLOBAL (1 << 8)

/**
 * All symbold are not made available for relocation processing by other
 * modules.
 */
#define RTLD_LOCAL 0

/**
 * Special purpose value for `handle` for `dlsym()` reserved by POSIX for
 * future use.
 * The identifier lookup happens in the normal global scope; that is, a
 * search for an identifier using `handle` would find the same definition
 * as a direct use of this identifier in the program code.
 */
#define RTLD_DEFAULT (void*)0

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/dlfcn.h.html>.
 */
typedef struct {
  /**
   * Pathname of mapped object file.
   */
  const char *dli_fname;
  /**
   * Base of mapped address range.
   */
  void *dli_fbase;
  /**
   * Symbol name or null pointer.
   */
  const char *dli_sname;
  /**
   * Symbol address of null pointer.
   */
  void *dli_saddr;
} Dl_info_t;

/**
 * alias as per spec update: <https://www.austingroupbugs.net/view.php?id=1847>
 */
typedef Dl_info_t Dl_info;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/dladdr.html>.
 */
int dladdr(const void *addr, Dl_info_t *dlip);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/dlopen.html>.
 */
void *dlopen(const char *cfilename, int flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/dlsym.html>.
 */
void *dlsym(void *handle, const char *symbol);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/dlclose.html>.
 */
int dlclose(void *handle);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/dlerror.html>.
 */
char *dlerror(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_DLFCN_H */
