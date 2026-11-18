#ifndef _RELIBC_GETOPT_H
#define _RELIBC_GETOPT_H

#include <unistd.h>

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getopt.3.html>.
 */
#define no_argument 0

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getopt.3.html>.
 */
#define required_argument 1

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getopt.3.html>.
 */
#define optional_argument 2

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getopt.3.html>.
 */
struct option {
  const char *name;
  int has_arg;
  int *flag;
  int val;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/getopt.3.html>.
 *
 * Functions the same as `getopt` but also accepts long options.
 */
int getopt_long(int argc,
                char *const *argv,
                const char *optstring,
                const struct option *longopts,
                int *longindex);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_GETOPT_H */
