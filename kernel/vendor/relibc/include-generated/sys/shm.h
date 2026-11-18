#ifndef _RELIBC_SYS_SHM_H
#define _RELIBC_SYS_SHM_H

#include <sys/ipc.h>
#include <bits/intptr-t.h> // for intptr_t from stdint.h
#include <bits/pid-t.h>    // for pid_t from sys/types.h
#include <bits/size-t.h>   // for size_t from sys/types.h
#include <bits/time-t.h>   // for time_t from sys/types.h


/**
 * Attach read-only (else read-write).
 */
#define SHM_RDONLY 4096

/**
 * Round attach address to SHMLBA.
 */
#define SHM_RND 8192

/**
 * Segment low boundary address multiple.
 */
#define SHMLBA 4096

/**
 * Return value of `shmat()` indicating shared memory has not been attached.
 */
#define SHM_FAILED (void*)-1

typedef unsigned short shmatt_t;

struct shmid_ds {
  struct ipc_perm shm_perm;
  size_t shm_segsz;
  pid_t shm_lpid;
  pid_t shm_cpid;
  shmatt_t shm_nattch;
  time_t shm_atime;
  time_t shm_dtime;
  time_t shm_ctime;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/shmget.html>.
 */
int shmget(key_t key, size_t size, int shmflg);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/shmat.html>.
 */
void *shmat(int shmid, const void *_shmaddr, int shmflg);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/shmdt.html>.
 */
int shmdt(const void *shmaddr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/shmctl.html>.
 */
int shmctl(int shmid, int cmd, struct shmid_ds *buf);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_SHM_H */
