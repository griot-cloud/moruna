#ifndef _RELIBC_SYS_PTRACE_H
#define _RELIBC_SYS_PTRACE_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Indicate that this process is to be traced by its parent.
 * Only used by the tracee.
 */
#define PTRACE_TRACEME 0

/**
 * Equivalent to `PTRACE_PEEKDATA`.
 */
#define PTRACE_PEEKTEXT 1

/**
 * Read a word at the address `addr` in the tracee's memory, returning the
 * word as a result of the `ptrace()` call.
 */
#define PTRACE_PEEKDATA 2

/**
 * Equivalent to `PTRACE_POKEDATA`.
 */
#define PTRACE_POKETEXT 4

/**
 * Copy the word `data` to the address `addr` in the tracee's memory.
 */
#define PTRACE_POKEDATA 5

/**
 * Restart the stopped tracee process.
 */
#define PTRACE_CONT 7

/**
 * Send the tracee a `SIGKILL` to terminate it.
 * Considered deprecated. Should just send `SIGKILL` directly instead.
 */
#define PTRACE_KILL 8

/**
 * Restart the stopped tracee as for `PTRACE_CONT`, but arrange for the
 * tracee to be stopped after execution of a single instruction.
 */
#define PTRACE_SINGLESTEP 9

/**
 * Copy the tracee's general-purpose registers to the address `data` in
 * the tracer.
 */
#define PTRACE_GETREGS 12

/**
 * Modify the tracee's general-purpose registers from the address `data`
 * in the tracer.
 */
#define PTRACE_SETREGS 13

/**
 * Copy the tracee's floating-point registers to the address `data` in
 * the tracer.
 */
#define PTRACE_GETFPREGS 14

/**
 * Modify the tracee's floating-point registers from the address `data`
 * in the tracer.
 */
#define PTRACE_SETFPREGS 15

/**
 * Attach to the process specified in `pid`, making it a tracee of the
 * calling process.
 */
#define PTRACE_ATTACH 16

/**
 * Restart the stopped tracee as for `PTRACE_CONT`, but first detach from it.
 */
#define PTRACE_DETACH 17

/**
 * Restart the stopped tracee as for `PTRACE_CONT`, but arrange for the
 * tracee to be stopped at the next entry to or exit from a system call.
 */
#define PTRACE_SYSCALL 24

/**
 * Continue and stop on entry to the next system call, which will not be
 * executed.
 */
#define PTRACE_SYSEMU 31

/**
 * Same as `PTRACE_SYSEMU` but also singlestep if not a system call.
 */
#define PTRACE_SYSEMU_SINGLESTEP 32

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/ptrace.2.html>.
 *
 * Provides a means by which one process (the "tracer") may observe and
 * control the execution of another process (the "tracee"), and examine
 * and change the tracee's memory and registers.
 *
 * The `__valist` argument represents 3 arguments in this order:
 * - pid
 * - *addr
 * - *data
 *
 * Currently only implemented on x86_64.
 */
int ptrace(int request, ...);

#endif  /* _RELIBC_SYS_PTRACE_H */
