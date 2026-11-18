#ifndef _RELIBC_SYSEXITS_H
#define _RELIBC_SYSEXITS_H

/**
 * Successful termination.
 */
#define EX_OK 0

/**
 * Command line usage error.
 */
#define EX_USAGE 64

/**
 * Data format error.
 */
#define EX_DATAERR 65

/**
 * Cannot open input.
 */
#define EX_NOINPUT 66

/**
 * Addressee unknown.
 */
#define EX_NOUSER 67

/**
 * Host name unknown.
 */
#define EX_NOHOST 68

/**
 * Service unavailable.
 */
#define EX_UNAVAILABLE 69

/**
 * Internal software error.
 */
#define EX_SOFTWARE 70

/**
 * System error (e.g., can't fork).
 */
#define EX_OSERR 71

/**
 * Critical OS file missing.
 */
#define EX_OSFILE 72

/**
 * Can't create (user) output file.
 */
#define EX_CANTCREAT 73

/**
 * Input/Output error.
 */
#define EX_IOERR 74

/**
 * Temp failure; user is invited to retry.
 */
#define EX_TEMPFAIL 75

/**
 * Remote error in protocol.
 */
#define EX_PROTOCOL 76

/**
 * Permission denied.
 */
#define EX_NOPERM 77

/**
 * Configuration error.
 */
#define EX_CONFIG 78

#endif  /* _RELIBC_SYSEXITS_H */
