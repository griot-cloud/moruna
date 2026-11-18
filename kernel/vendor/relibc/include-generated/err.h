#ifndef _RELIBC_ERR_H
#define _RELIBC_ERR_H

#include <stdarg.h>
#include <stdio.h>
#include <features.h>

typedef void (*ExitCallback)(int);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Set global [`FILE`] sink to write errors and warnings.
 */
void err_set_file(FILE *fp);

/**
 * Set or remove a callback to invoke before exiting on error.
 */
void err_set_exit(ExitCallback ef);

/**
 * Print a user message then an error message for [`ERRNO`] followed by exiting with `eval`.
 *
 * The message format is `progname: fmt: strerror(ERRNO)`
 *
 * # Return
 * Does not return. Exits with `eval` as an error code.
 */
void err(int eval, const char *fmt, ...) __noreturn;

/**
 * Print a user message then an error message for `code` before exiting with `eval` as a return.
 *
 * The message format is `progname: fmt: strerror(code)`
 *
 * # Return
 * Exits with `eval` as an error code.
 */
void errc(int eval, int code, const char *fmt, ...) __noreturn;

/**
 * Print a user message then exits with `eval` as a return.
 *
 * The message format is `progname: fmt`
 *
 * # Return
 * Exits with `eval` as an error code.
 */
void errx(int eval, const char *fmt, ...) __noreturn;

/**
 * Print a user message and then an error message for [`ERRNO`].
 *
 * The message format is `progname: fmt: strerror(ERRNO)`
 */
void warn(const char *fmt, ...);

/**
 * Print a user message then an error message for `code`.
 *
 * The message format is `progname: fmt: strerror(code)`
 */
void warnc(int code, const char *fmt, ...);

/**
 * Print a user message as a warning.
 *
 * The message format is `progname: fmt`
 */
void warnx(const char *fmt, ...);

/**
 * See [`err`].
 */
void verr(int eval, const char *fmt, va_list args) __noreturn;

/**
 * See [`errc`].
 */
void verrc(int eval, int code, const char *fmt, va_list args) __noreturn;

/**
 * See [`errx`];
 */
void verrx(int eval, const char *fmt, va_list args) __noreturn;

/**
 * See [`warn`].
 */
void vwarn(const char *fmt, va_list args);

/**
 * See [`warnc`].
 */
void vwarnc(int code, const char *fmt, va_list args);

/**
 * See [`warnx`].
 */
void vwarnx(const char *fmt, va_list args);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_ERR_H */
