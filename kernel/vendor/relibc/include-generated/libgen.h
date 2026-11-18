#ifndef _RELIBC_LIBGEN_H
#define _RELIBC_LIBGEN_H

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/basename.html>.
 *
 * Takes the pathname pointed to by `path` and returns a pointer to the final
 * component of the pathname, deleting any trailing `/` characters.
 *
 * Always successful. The return value never represents an error.
 */
char *basename(char *path);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/dirname.html>.
 *
 * Takes a pointer to a character string that contains a pathname, and return
 * a pointer to a string that is a pathname of the directory containing the
 * entry of the final pathname component.
 *
 * Always successful. The return value never represents an error.
 */
char *dirname(char *path);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_LIBGEN_H */
