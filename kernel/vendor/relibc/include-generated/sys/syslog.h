#ifndef _RELIBC_SYS_SYSLOG_H
#define _RELIBC_SYS_SYSLOG_H

/**
 * Record the caller's PID in log messages.
 */
#define LOG_PID 1

/**
 * Write to /dev/console if [`syslog`] fails.
 */
#define LOG_CONS 2

/**
 * Open the log on the first call to [`syslog`] rather than opening it early.
 * This is the default behavior and setting or unsetting this option does nothing.
 */
#define LOG_ODELAY 4

/**
 * Open the log file immediately.
 */
#define LOG_NDELAY 8

#define LOG_NOWAIT 16

/**
 * Print log message to stderr as well as the log.
 */
#define LOG_PERROR 32

#define LOG_KERN (0 << 3)

#define LOG_USER (1 << 3)

#define LOG_MAIL (2 << 3)

#define LOG_DAEMON (3 << 3)

#define LOG_AUTH (4 << 3)

#define LOG_SYSLOG (5 << 3)

#define LOG_LPR (6 << 3)

#define LOG_NEWS (7 << 3)

#define LOG_UUCP (8 << 3)

#define LOG_CRON (9 << 3)

#define LOG_AUTHPRIV (10 << 3)

#define LOG_FTP (11 << 3)

#define LOG_LOCAL0 (16 << 3)

#define LOG_LOCAL1 (17 << 3)

#define LOG_LOCAL2 (18 << 3)

#define LOG_LOCAL3 (19 << 3)

#define LOG_LOCAL4 (20 << 3)

#define LOG_LOCAL5 (21 << 3)

#define LOG_LOCAL6 (22 << 3)

#define LOG_LOCAL7 (23 << 3)

#define LOG_NFACILITIES 24

#define LOG_EMERG 0

#define LOG_ALERT 1

#define LOG_CRIT 2

#define LOG_ERR 3

#define LOG_WARNING 4

#define LOG_NOTICE 5

#define LOG_INFO 6

#define LOG_DEBUG 7

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Create a mask that includes all levels up to a certain priority.
 */
int LOG_UPTO(int p);

/**
 * Create a mask that enables a single priority.
 */
int LOG_MASK(int p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/closelog.html>.
 */
int setlogmask(int mask);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/closelog.html>.
 */
void openlog(const char *ident, int opt, int facility);

/**
 * See <https://www.man7.org/linux/man-pages/man3/vsyslog.3.html>.
 *
 * Non-POSIX, 4.3BSD-Reno.
 */
void vsyslog(int priority, const char *message, ...);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/closelog.html>.
 */
void syslog(int priority, const char *message, ...);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/closelog.html>.
 */
void closelog(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_SYSLOG_H */
