#ifndef _RELIBC_SEMAPHORE_H
#define _RELIBC_SEMAPHORE_H

#include <fcntl.h>
#include <bits/clockid-t.h> // for clockid_t from sys/types.h
#include <bits/timespec.h>  // for timespec from time.h


/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/semaphore.h.html>.
 */
typedef union {
  char size[4];
  long align;
} sem_t;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_destroy.html>.
 */
int sem_destroy(sem_t *sem);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_getvalue.html>.
 */
int sem_getvalue(sem_t *sem, int *sval);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_init.html>.
 */
int sem_init(sem_t *sem, int _pshared, unsigned int value);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_post.html>.
 */
int sem_post(sem_t *sem);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_trywait.html>.
 */
int sem_trywait(sem_t *sem);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_trywait.html>.
 */
int sem_wait(sem_t *sem);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_clockwait.html>.
 */
int sem_clockwait(sem_t *sem, clockid_t clock_id, const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/sem_timedwait.html>.
 */
int sem_timedwait(sem_t *sem, const struct timespec *abstime);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SEMAPHORE_H */
