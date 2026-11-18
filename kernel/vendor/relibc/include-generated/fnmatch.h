#ifndef _RELIBC_FNMATCH_H
#define _RELIBC_FNMATCH_H

/**
 * The input string does not match the specified pattern.
 */
#define FNM_NOMATCH 1

/**
 * Disable backslash escaping.
 */
#define FNM_NOESCAPE 1

/**
 * Backslash in the input string only matches backlash in pattern.
 */
#define FNM_PATHNAME 2

/**
 * Leading period in input string only matches period in pattern.
 */
#define FNM_PERIOD 4

/**
 * Compare input string and pattern in a case-insensitive manner.
 */
#define FNM_CASEFOLD 8

/**
 * Equivalent to `FNM_CASEFOLD`.
 */
#define FNM_IGNORECASE FNM_CASEFOLD

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/fnmatch.html>.
 *
 * Checks the `input` string to see if it matches `pattern`.
 *
 * # Safety
 *
 * The `input` and `pattern` parameters must be non-null and properly aligned.
 */
int fnmatch(const char *pattern, const char *input, int flags);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_FNMATCH_H */
