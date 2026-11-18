#ifndef _RELIBC_THREADS_H
#define _RELIBC_THREADS_H

#include <time.h>
#include <bits/limits/ptdi.h> // for PTHREAD_DESTRUCTOR_ITERATIONS from limits.h
#include <bits/pthreadoi.h>   // for PTHREAD_ONCE_INIT from pthread.h
#include <bits/pthread-t.h>   // for pthread_t from sys/types.h
#include <bits/threads.h>     // for pthread_{key|mutex|cond}_t from sys/types.h

#ifndef __cplusplus
#define thread_local _Thread_local
#endif
#define ONCE_FLAG_INIT PTHREAD_ONCE_INIT // once_flag == pthread_once_t


#define thrd_success 0

#define thrd_busy 1

#define thrd_error 2

#define thrd_nomem 3

#define thrd_timedout 4

#define mtx_plain 0

#define mtx_recursive 1

#define mtx_timed 2

#define TSS_DTOR_ITERATIONS PTHREAD_DESTRUCTOR_ITERATIONS

typedef pthread_once_t once_flag;

typedef pthread_cond_t cnd_t;

typedef pthread_mutex_t mtx_t;

typedef pthread_t thrd_t;

typedef int (*thrd_start_t)(void*);

typedef pthread_key_t tss_t;

typedef void (*tss_dtor_t)(void*);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/call_once.html>.
 */
void call_once(once_flag *flag, void (*func)(void));

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cnd_broadcast.html>.
 */
int cnd_broadcast(cnd_t *cnd);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cnd_destroy.html>.
 */
void cnd_destroy(cnd_t *cnd);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cnd_init.html>.
 */
int cnd_init(cnd_t *cnd);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cnd_signal.html>.
 */
int cnd_signal(cnd_t *cnd);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cnd_timedwait.html>.
 */
int cnd_timedwait(cnd_t *cnd, mtx_t *mtx, const struct timespec *timeout);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cnd_wait.html>.
 */
int cnd_wait(cnd_t *cnd, mtx_t *mtx);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/mtx_destroy.html>.
 */
void mtx_destroy(mtx_t *mtx);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/mtx_init.html>.
 */
int mtx_init(mtx_t *mtx, int ty);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/mtx_lock.html>.
 */
int mtx_lock(mtx_t *mtx);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/mtx_timedlock.html>.
 */
int mtx_timedlock(mtx_t *mtx, const struct timespec *timeout);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/mtx_trylock.html>.
 */
int mtx_trylock(mtx_t *mtx);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/mtx_unlock.html>.
 */
int mtx_unlock(mtx_t *mtx);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_create.html>.
 */
int thrd_create(thrd_t *thrd, thrd_start_t start, void *arg);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_current.html>.
 */
thrd_t thrd_current(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_detach.html>.
 */
int thrd_detach(thrd_t thrd);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_equal.html>.
 */
int thrd_equal(thrd_t thrd1, thrd_t thrd2);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_exit.html>.
 */
void thrd_exit(int ret) __noreturn;

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_join.html>.
 */
int thrd_join(thrd_t thrd, int *retval);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_sleep.html>.
 */
int thrd_sleep(const struct timespec *time1, struct timespec *time2);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/thrd_yield.html>.
 */
void thrd_yield(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tss_create.html>.
 */
int tss_create(tss_t *tss, tss_dtor_t dtor);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tss_delete.html>.
 */
void tss_delete(tss_t tss);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tss_get.html>.
 */
void *tss_get(tss_t tss);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tss_set.html>.
 */
int tss_set(tss_t tss, void *value);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_THREADS_H */
