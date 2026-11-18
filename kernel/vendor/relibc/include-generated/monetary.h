#ifndef _RELIBC_MONETARY_H
#define _RELIBC_MONETARY_H

#include <bits/locale-t.h> // for locale_t from locale.h
#include <bits/size-t.h>   // for size_t from stddef.h
#include <bits/ssize-t.h>  // for ssize_t from sys/types.h


#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/strfmon.html>.
 *
 * The `strfmon()` function formats a monetary value according to the format string `format`
 * and writes the result to the character array `s` of size `maxsize`.
 * The format string can contain plain characters and format specifiers.
 *
 * Returns:
 * - The number of characters written (excluding the null terminator), or -1 if
 *   an error occurs (e.g., invalid input, buffer overflow)
 */
ssize_t strfmon(char *s, size_t maxsize, const char *format, ...);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_MONETARY_H */
