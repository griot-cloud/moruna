#ifndef _RELIBC_SPAWN_H
#define _RELIBC_SPAWN_H

#include <sched.h>
#include <bits/sigset-t.h>
#include <bits/mode-t.h>
#include <bits/size-t.h>

#define POSIX_SPAWN_RESETIDS 1

#define POSIX_SPAWN_SETPGROUP 2

#define POSIX_SPAWN_SETSIGDEF 3

#define POSIX_SPAWN_SETSIGMASK 4

#define POSIX_SPAWN_SETSCHEDPARAM 5

#define POSIX_SPAWN_SETSCHEDULER 6

#define POSIX_SPAWN_SETSID 7

/**
 * A spawn file actions object.
 */
typedef struct {
  unsigned char __relibc_internal_size[24];
  size_t __relibc_internal_align;
} posix_spawn_file_actions_t;

/**
 * A spawn attributes object.
 */
typedef struct {
  struct sched_param param;
  short flags;
  int pgroup;
  int policy;
  sigset_t sigdefault;
  sigset_t sigmask;
} posix_spawnattr_t;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn.html>.
 *
 * Creates a new process (child process) from the specified process image. The
 * `path` argument is a pathname that identifies the new process image file to
 * execute.
 *
 * Returns the process ID of the child process to the parent process, in the
 * variable pointed to by a non-null `pid` argument, and shall return `0` as
 * the function return value. Upon error, an error number shall be returned
 * as the function return value.
 *
 * # Panics
 * `argv` must **not** be `NULL` and must contain atleast the program name.
 * `path` must also **not** be `NULL`. Failure to ensure any of this will
 * result in a panic.
 *
 * # Safety
 * `file_actions` and `attrp` must either be `NULL` or be pointers to properly
 * initialised objects. Doing otherwise is undefined behaviour.
 *
 * `path` and the elements in `argv` must be a pointers to valid
 * null-terminated character arrays. Failure to ensure any of this will result
 * in undefined behaviour.
 */
int posix_spawn(pid_t *pid,
                const char *path,
                const posix_spawn_file_actions_t *file_actions,
                const posix_spawnattr_t *attrp,
                char *const *argv,
                char *const *envp);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnp.html>.
 *
 * Creates a new process (child process) from the specified process image. The
 * `file` argument is used to construct a pathname that identifies the new
 * process image file.
 *
 * Returns the process ID of the child process to the parent process, in the
 * variable pointed to by a non-null `pid` argument, and shall return `0` as
 * the function return value. Upon error, an error number shall be returned
 * as the function return value.
 *
 * # Panics
 * `argv` must **not** be `NULL` and must contain atleast the program name.
 * `file` must also **not** be `NULL`. Failure to ensure any of this will
 * result in a panic.
 *
 * # Safety
 * `file_actions` and `attrp` must either be `NULL` or be pointers to properly
 * initialised objects. Doing otherwise is undefined behaviour.
 *
 * `file` and the elements in `argv` must be a pointers to valid
 * null-terminated character arrays. Failure to ensure any of this will result
 * in undefined behaviour.
 */
int posix_spawnp(pid_t *pid,
                 const char *file,
                 const posix_spawn_file_actions_t *file_actions,
                 const posix_spawnattr_t *attrp,
                 char *const *argv,
                 char *const *envp);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_init.html>.
 *
 * Initializes the object referenced by `file_actions` to contain no file
 * actions for `posix_spawn()` or `posix_spawnp()` to perform.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 */
int posix_spawn_file_actions_init(posix_spawn_file_actions_t *file_actions);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_destroy.html>.
 *
 * Destroys the object referenced by `file_actions`, effectively making it
 * uninitialized.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 *
 * # Safety
 * If `file_actions` is not `NULL`, then it must be a pointer to a
 * `file_actions` object that was initialised by calling
 * `posix_spawn_file_actions_init()`.
 */
int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *file_actions);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_addopen.html>.
 *
 * Adds an open action to a spawn file actions object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 *
 * # Safety
 * `path` must be a valid null-terminated C string.
 */
int posix_spawn_file_actions_addopen(posix_spawn_file_actions_t *file_actions,
                                     int fildes,
                                     const char *path,
                                     int oflag,
                                     mode_t mode);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_addclose.html>.
 *
 * Adds a close action to a spawn file actions object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 *
 * # Safety
 * `file_actions` must be initialised.
 */
int posix_spawn_file_actions_addclose(posix_spawn_file_actions_t *file_actions,
                                      int fildes);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_addchdir.html>.
 *
 * Adds a chdir action to the object referenced by `file_actions` that shall
 * cause the working directory to be set to `path` (as if `chdir(path)` had
 * been called) when a new process is spawned using this file actions object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 *
 * # Safety
 * `file_actions` must be initialised and `path` must be a valid
 * null-terminated C string.
 */
int posix_spawn_file_actions_addchdir(posix_spawn_file_actions_t *file_actions,
                                      const char *path);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_addfchdir.html>.
 *
 * Adds a fchdir action to the object referenced by `file_actions` that shall
 * cause the working directory to be set to `fildes` (as if `fchdir(fildes)`
 * had been called) when a new process is spawned using this file actions
 * object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 *
 * # Safety
 * `file_actions` must be initialised.
 */
int posix_spawn_file_actions_addfchdir(posix_spawn_file_actions_t *file_actions,
                                       int fildes);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawn_file_actions_adddup2.html>.
 *
 * Adds a dup2 action to the object referenced by `file_actions` that shall
 * cause the file descriptor `fildes` to be duplicated as `newfildes` (as if
 * `dup2(fildes, newfildes)` had been called) when a new process is spawned
 * using this file actions object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `file_actions` is `NULL`.
 *
 * # Safety
 * `file_actions` must be initialised.
 */
int posix_spawn_file_actions_adddup2(posix_spawn_file_actions_t *file_actions,
                                     int fildes,
                                     int newfildes);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_init.html>.
 *
 * Initializes a spawn attributes object `attr` with a default value for all
 * the individual attributes used by the implementation.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` is `NULL`.
 */
int posix_spawnattr_init(posix_spawnattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_destroy.html>.
 *
 * Destroys a spawn attributes object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` is `NULL`.
 *
 * # Safety
 * `attr` must be a pointer to `posix_spawnattr_t` and must at least be
 * initialised.
 */
int posix_spawnattr_destroy(posix_spawnattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_setschedparam.html>.
 *
 * Sets the spawn-schedparam attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `schedparam` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_setschedparam(posix_spawnattr_t *attr,
                                  const struct sched_param *schedparam);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_getschedparam.html>.
 *
 * Obtains the value of the spawn-schedparam attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the spawn-schedparam
 * attribute of `attr` into the object referenced by `schedparam`. Upon
 * failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `schedparam` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_getschedparam(const posix_spawnattr_t *attr,
                                  struct sched_param *schedparam);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_setschedpolicy.html>.
 *
 * Sets the spawn-schedpolicy attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_setschedpolicy(posix_spawnattr_t *attr,
                                   int schedpolicy);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_getschedpolicy.html>.
 *
 * Obtains the value of the spawn-schedpolicy attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the spawn-schedpolicy
 * attribute of `attr` into the object referenced by `schedpolicy`. Upon
 * failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `schedpolicy` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_getschedpolicy(const posix_spawnattr_t *attr,
                                   int *schedpolicy);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_setsigdefault.html>.
 *
 * Sets the spawn-sigdefault attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `sigdefault` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_setsigdefault(posix_spawnattr_t *attr,
                                  const sigset_t *sigdefault);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_getsigdefault.html>.
 *
 * Obtains the value of the spawn-sigdefault attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the spawn-sigdefault
 * attribute of `attr` into the object referenced by `sigdefault`. Upon
 * failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `sigdefault` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_getsigdefault(const posix_spawnattr_t *attr,
                                  sigset_t *sigdefault);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_setsigmask.html>.
 *
 * Sets the spawn-sigmask attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `sigmask` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_setsigmask(posix_spawnattr_t *attr, const sigset_t *sigmask);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_getsigmask.html>.
 *
 * Obtains the value of the spawn-sigmask attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the spawn-sigmask
 * attribute of `attr` into the object referenced by `sigmask`. Upon
 * failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `sigmask` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_getsigmask(const posix_spawnattr_t *attr, sigset_t *sigmask);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_setflags.html>.
 *
 * Sets the spawn-flags attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_setflags(posix_spawnattr_t *attr, short flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_getflags.html>.
 *
 * Obtains the value of the spawn-flags attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the spawn-flags
 * attribute of `attr` into the object referenced by `flags`. Upon
 * failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `flags` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_getflags(const posix_spawnattr_t *attr, short *flags);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_setpgroup.html>.
 *
 * Sets the spawn-pgroup attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_setpgroup(posix_spawnattr_t *attr, pid_t pgroup);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/posix_spawnattr_getpgroup.html>.
 *
 * Obtains the value of the spawn-pgroup attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the spawn-pgroup
 * attribute of `attr` into the object referenced by `pgroup`. Upon
 * failure, an error number is returned.
 *
 * # Panics
 * Panics if `attr` or `pgroup` is `NULL`.
 *
 * # Safety
 * `attr` must be initialised.
 */
int posix_spawnattr_getpgroup(const posix_spawnattr_t *attr, pid_t *pgroup);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SPAWN_H */
