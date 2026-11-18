#ifndef _RELIBC_FMTMSG_H
#define _RELIBC_FMTMSG_H


#define MM_NULLLBL ((char*)0)
#define MM_NULLTXT ((char*)0)
#define MM_NULLACT ((char*)0)
#define MM_NULLTAG ((char*)0)


/**
 * Source of the condition is hardware.
 */
#define MM_HARD 1

/**
 * Source of the condition is software.
 */
#define MM_SOFT 2

/**
 * Source of the condition is firmware.
 */
#define MM_FIRM 4

/**
 * Condition detected by application.
 */
#define MM_APPL 8

/**
 * Condition detected by utility.
 */
#define MM_UTIL 16

/**
 * Condition detected by operating system.
 */
#define MM_OPSYS 32

/**
 * Recoverable error.
 */
#define MM_RECOVER 64

/**
 * Non-recoverable error.
 */
#define MM_NRECOV 128

/**
 * Display message on standard error.
 */
#define MM_PRINT 256

/**
 * Display message on system console.
 */
#define MM_CONSOLE 512

/**
 * Class argument.
 */
#define MM_NULLMC 0

/**
 * Error causing application to halt.
 */
#define MM_HALT 1

/**
 * Application has encountered a non-fatal fault.
 */
#define MM_ERROR 2

/**
 * Application has detected unusual non-error condition.
 */
#define MM_WARNING 3

/**
 * Informative message.
 */
#define MM_INFO 4

/**
 * No severity level provided for the message.
 */
#define MM_NOSEV 0

/**
 * The function succeeded.
 */
#define MM_OK 0

/**
 * The function failed completely.
 */
#define MM_NOTOK -1

/**
 * The function was unable to generate a message on standard error, but otherwise succeeded.
 */
#define MM_NOMSG 1

/**
 * The function was unable to generate a console message, but otherwise succeeded.
 */
#define MM_NOCON 4

/**
 * Severity argument.
 */
#define MM_NULLSEV 0

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/fmtmsg.h.html>
 */
int fmtmsg(long classification,
           const char *label,
           int severity,
           const char *text,
           const char *action,
           const char *tag);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_FMTMSG_H */
