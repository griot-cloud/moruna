#ifndef _RELIBC_SYS_IPC_H
#define _RELIBC_SYS_IPC_H

#include <bits/gid-t.h>  // for gid_t from sys/types.h
#include <bits/uid-t.h>  // for uid_t from sys/types.h
#include <bits/key-t.h>  // for key_t from sys/types.h
#include <bits/mode-t.h> // for mode_t from sys/types.h


#define IPC_R 256

#define IPC_W 128

#define IPC_M 4096

/**
 * Remove identifier.
 */
#define IPC_RMID 0

/**
 * Set options.
 */
#define IPC_SET 1

/**
 * Get options.
 */
#define IPC_STAT 2

/**
 * Create entry if key does not exist.
 */
#define IPC_CREAT 512

/**
 * Fail if key exists.
 */
#define IPC_EXCL 1024

/**
 * Error if request would need to wait.
 */
#define IPC_NOWAIT 2048

struct ipc_perm {
  key_t __key;
  /**
   * Owner's user ID.
   */
  uid_t uid;
  /**
   * Owner's group ID.
   */
  gid_t gid;
  /**
   * Creator's user ID.
   */
  uid_t cuid;
  /**
   * Creator's group ID.
   */
  gid_t cgid;
  /**
   * Read/write permission.
   */
  mode_t mode;
  unsigned short __seq;
};

/**
 * Private key.
 */
#define IPC_PRIVATE 0

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/ftok.html>.
 *
 * Returns a key based on `path` and `id` that is usable in subsequent calls
 * to `msgget()`, `semget()`, and `shmget()`.
 *
 * Upon success, returns a key. Upon failure, returns `(key_t)-1` and sets
 * errno to indicate the error.
 */
key_t ftok(const char *path, int id);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_IPC_H */
