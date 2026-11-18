#ifndef _RELIBC_PTHREAD_H
#define _RELIBC_PTHREAD_H

#include <sched.h>
#include <time.h>
#include <features.h>
#include <bits/pthread.h>   // for pthread types from sys/types.h
#include <bits/pthreadoi.h> // for PTHREAD_ONCE_INIT


// A value that shall not compare equal to any exiting thread.
#define PTHREAD_NULL ((pthread_t)NULL)

#define PTHREAD_COND_INITIALIZER {0}
#define PTHREAD_MUTEX_INITIALIZER {0}
#define PTHREAD_RWLOCK_INITIALIZER {0}

#define pthread_cleanup_push(ROUTINE, ARG) do { \
  struct { \
    void (*routine)(void *); \
    void *arg; \
    void *prev; \
  } __relibc_internal_pthread_ll_entry = { \
    .routine = (void (*)(void *))(ROUTINE), \
    .arg = (void *)(ARG), \
  }; \
  __relibc_internal_pthread_cleanup_push(&__relibc_internal_pthread_ll_entry);

#define pthread_cleanup_pop(EXECUTE) \
  __relibc_internal_pthread_cleanup_pop((EXECUTE)); \
} while(0)



#define _POSIX_THREADS 1

#define PTHREAD_BARRIER_SERIAL_THREAD -1

#define PTHREAD_CANCEL_ASYNCHRONOUS 0

#define PTHREAD_CANCEL_ENABLE 1

#define PTHREAD_CANCEL_DEFERRED 2

#define PTHREAD_CANCEL_DISABLE 3

#define PTHREAD_CANCELED (void*)~0

#define PTHREAD_CREATE_DETACHED 0

#define PTHREAD_CREATE_JOINABLE 1

/**
 * Specifies that the thread scheduling attributes shall be set to the
 * corresponding values from this attributes object.
 */
#define PTHREAD_EXPLICIT_SCHED 0

/**
 * Specifies that the thread scheduling attributes shall be inherited from the
 * creating thread, and the scheduling attributes in the `attr` argument shall
 * be ignored.
 */
#define PTHREAD_INHERIT_SCHED 1

#define PTHREAD_MUTEX_DEFAULT 0

#define PTHREAD_MUTEX_ERRORCHECK 1

#define PTHREAD_MUTEX_NORMAL 2

#define PTHREAD_MUTEX_RECURSIVE 3

#define PTHREAD_MUTEX_ROBUST 0

#define PTHREAD_MUTEX_STALLED 1

#define PTHREAD_PRIO_INHERIT 0

#define PTHREAD_PRIO_NONE 0

#define PTHREAD_PRIO_PROTECT 0

/**
 * Permits a condition variable to be operated upon by any thread that has
 * access to the memory where the condition variable is allocated, even if the
 * condition variable is allocated in memory that is shared by multiple
 * processes.
 */
#define PTHREAD_PROCESS_SHARED 0

#define PTHREAD_PROCESS_PRIVATE 1

/**
 * Signifies process scheduling contention scope.
 */
#define PTHREAD_SCOPE_PROCESS 0

/**
 * Signifies system scheduling contention scope.
 */
#define PTHREAD_SCOPE_SYSTEM 1

typedef void (*Dtor)(void *value);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cancel.html>.
 */
int pthread_cancel(pthread_t thread);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_create.html>.
 */
int pthread_create(pthread_t *pthread,
                   const pthread_attr_t *attr,
                   void *(*start_routine)(void *arg),
                   void *arg);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_detach.html>.
 */
int pthread_detach(pthread_t pthread);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_equal.html>.
 */
int pthread_equal(pthread_t pthread1, pthread_t pthread2);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_atfork.html>.
 */
int pthread_atfork(void (*prepare)(void), void (*parent)(void), void (*child)(void));

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_exit.html>.
 */
void pthread_exit(void *retval) __noreturn;

/**
 * See <https://pubs.opengroup.org/onlinepubs/009696799/functions/pthread_getconcurrency.html>.
 */
int pthread_getconcurrency(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_getcpuclockid.html>.
 */
int pthread_getcpuclockid(pthread_t thread, clockid_t *clock_out);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_getschedparam.html>.
 */
int pthread_getschedparam(pthread_t thread, int *policy_out, struct sched_param *param_out);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_join.html>.
 */
int pthread_join(pthread_t thread, void **retval);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_self.html>.
 */
pthread_t pthread_self(void);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_setcancelstate.html>.
 */
int pthread_setcancelstate(int state, int *oldstate);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_setcancelstate.html>.
 */
int pthread_setcanceltype(int ty, int *oldty);

/**
 * See <https://pubs.opengroup.org/onlinepubs/000095399/functions/pthread_setconcurrency.html>.
 */
int pthread_setconcurrency(int concurrency);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_setschedparam.html>.
 */
int pthread_setschedparam(pthread_t thread, int policy, const struct sched_param *param);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_setschedprio.html>.
 */
int pthread_setschedprio(pthread_t thread, int prio);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_setcancelstate.html>.
 */
void pthread_testcancel(void);

void __relibc_internal_pthread_cleanup_push(void *new_entry);

void __relibc_internal_pthread_cleanup_pop(int execute);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_destroy.html>.
 *
 * Destroys a thread attributes object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `attr` is not already initialized when calling this function
 * - `attr` is used after calling this function without reinitializing
 */
int pthread_attr_destroy(pthread_attr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getdetachstate.html>.
 *
 * Gets the destachstate attribute in the `attr` object.
 *
 * Upon success, returns `0` and stores the detachstate attribute in
 * `detachstate`. Upon failure, returns an error number to indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getdetachstate(const pthread_attr_t *attr,
                                int *detachstate);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getguardsize.html>.
 *
 * Gets the guardsize attribute in the `attr` object.
 *
 * Upon success, returns `0` and stores the guardsize attribute in the
 * `guardsize` parameter. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getguardsize(const pthread_attr_t *attr, size_t *guardsize);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getinheritsched.html>.
 *
 * Gets the inheritsched attribute in the `attr` object.
 *
 * Upon success, returns `0` and stores the inheritsched attribute in the
 * `inheritsched` parameter. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getinheritsched(const pthread_attr_t *attr,
                                 int *inheritsched);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getschedparam.html>.
 *
 * Gets the scheduling parameter attributes in the `attr` object.
 *
 * Upon success, returns `0` and stores the scheduling parameter attributes
 * in the `param` parameter. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getschedparam(const pthread_attr_t *attr, struct sched_param *param);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getschedpolicy.html>.
 *
 * Gets the schedpolicy attribute in the `attr` object.
 *
 * Upon success, returns `0` and stores the schedpolicy attributes in the
 * `policy` parameter. Upon failure, an error number is returned to indicate
 * the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getschedpolicy(const pthread_attr_t *attr,
                                int *policy);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getscope.html>.
 *
 * Gets the contentionscope attribute in the `attr` object.
 *
 * Upon success, returns `0` and stores the contentionscope attribute in the
 * `contentionscope` parameter. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getscope(const pthread_attr_t *attr, int *contentionscope);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getstack.html>.
 *
 * Gets the thread creation stack attributes stackaddr and stacksize in the
 * `attr` object.
 *
 * Upon success, returns `0` and stores the stackaddr attribute in the
 * `stackaddr` parameter and the stacksize attribute in the `stacksize`
 * parameter. Upon failure, an error number is returned to indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getstack(const pthread_attr_t *attr, void **stackaddr, size_t *stacksize);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_getstacksize.html>.
 *
 * Gets the thread creation stacksize attribute in the `attr` object.
 *
 * Upon success, returns `0` and stores the stacksize attribute in the
 * `stacksize` parameter. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_getstacksize(const pthread_attr_t *attr, size_t *stacksize);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_init.html>.
 *
 * Initializes a thread attributes object `attr` with the default value for
 * all of the individual attributes used by a given implementation.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 */
int pthread_attr_init(pthread_attr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setdetachstate.html>.
 *
 * Sets the detachstate attribute in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setdetachstate(pthread_attr_t *attr,
                                int detachstate);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setguardsize.html>.
 *
 * Sets the guardsize attribute in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setguardsize(pthread_attr_t *attr, int guardsize);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setinheritsched.html>.
 *
 * Sets the inheritsched attribute in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setinheritsched(pthread_attr_t *attr,
                                 int inheritsched);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setschedparam.html>.
 *
 * Sets the scheduling parameter attributes in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setschedparam(pthread_attr_t *attr, const struct sched_param *param);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setschedpolicy.html>.
 *
 * Sets the schedpolicy attribute in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setschedpolicy(pthread_attr_t *attr,
                                int policy);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setscope.html>.
 *
 * Sets the contentionscope attribute in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setscope(pthread_attr_t *attr, int contentionscope);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setstack.html>.
 *
 * Sets the thread creation stack attributes stackaddr and stacksize in the
 * `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setstack(pthread_attr_t *attr, void *stackaddr, size_t stacksize);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_attr_setstacksize.html>.
 *
 * Sets the thread creation stacksize attribute in the `attr` object.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is not initialized.
 */
int pthread_attr_setstacksize(pthread_attr_t *attr, size_t stacksize);

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/pthread_getattr_np.3.html>.
 *
 * Initializes the thread attributes object referenced to by `attr` so that it
 * contains actual attribute values describing the running thread `thread`.
 *
 * Upon success, returns `0`. Upon failure, returns an error number to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds, so will never return an error number.
 */
int pthread_getattr_np(pthread_t thread, pthread_attr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrier_destroy.html>.
 *
 * Destroys the barrier referenced by `barrier` and releases any resources
 * used by the barrier.
 *
 * Upon success, returns `0`. Upon error, returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * The following will result in undefined behaviour:
 * - Use of `barrier` after this function unless reinitialized by
 *   `pthread_barrier_init()`.
 * - This function is called with an uninitialized `barrier`.
 * - This function is called when any thread is blocked on `barrier`.
 */
int pthread_barrier_destroy(pthread_barrier_t *barrier);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrier_init.html>.
 *
 * Allocates any resources required to use the barrier referenced by `barrier`
 * and shall initialize the barrier with attributes referenced by `attr`.
 *
 * Upon success, returns `0`. Upon error, returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * The following will result in undefined behaviour:
 * - Use of `barrier` before this function is called.
 * - This function is called with an already initialized `barrier`.
 * - This function is called when any thread is blocked on `barrier`.
 */
int pthread_barrier_init(pthread_barrier_t *barrier,
                         const pthread_barrierattr_t *attr,
                         unsigned int count);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrier_wait.html>.
 *
 * Synchronizes participating threads at the barrier referenced by `barrier`.
 *
 * Upon success, returns `PTHREAD_BARRIER_SERIAL_THREAD` for a single
 * (arbitrary) thread synchronized at the barrier and `0` for each of the
 * other threads. Upon failure, returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * Undefined behaviour will occur if `barrier` is uninitialized.
 */
int pthread_barrier_wait(pthread_barrier_t *barrier);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrierattr_init.html>.
 *
 * Initializes a barrier attributes object `attr` with the default value of
 * all of the attributes defined by the implementation.
 *
 * Upon success, returns `0`. Upon error, returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * Undefined behaviour will occur if `attr` is already initialized.
 */
int pthread_barrierattr_init(pthread_barrierattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrierattr_setpshared.html>.
 *
 * Sets the process-shared attribute in an initialized attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon error, returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * Undefined behaviour will occur if `attr` is uninitialized.
 */
int pthread_barrierattr_setpshared(pthread_barrierattr_t *attr,
                                   int pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrierattr_getpshared.html>.
 *
 * Obtains the value of the process-shared attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the process-shared
 * attribute of `attr` into the object referenced by `pshared`. Upon error,
 * returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * Undefined behaviour will occur if `attr` is uninitialized.
 */
int pthread_barrierattr_getpshared(const pthread_barrierattr_t *attr,
                                   int *pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_barrierattr_destroy.html>.
 *
 * Destroys a barrier attributes object.
 *
 * Upon success, returns `0`. Upon error, returns an error number.
 *
 * # Implementation
 * Not async-signal-safe.
 *
 * # Safety
 * Undefined behaviour will occur if `attr` is uninitialized or if `attr` is
 * used after calling this function.
 */
int pthread_barrierattr_destroy(pthread_barrierattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_broadcast.html>.
 *
 * As a single atomic operation, determines which threads, if any, are blocked
 * on the specified condition variable `cond` and unblocks all of these
 * threads.
 *
 * Has no effect if no threads are blocked on `cond`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `cond` is not initialized.
 */
int pthread_cond_broadcast(pthread_cond_t *cond);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_destroy.html>.
 *
 * Destroys the given condition variable specified by `cond`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `cond` is not initialized when calling this function.
 * - Use of `cond` after calling this function without reinitializing it.
 * - Attempting to destroy a condition variable upon which other threads are
 *   currently blocked.
 */
int pthread_cond_destroy(pthread_cond_t *cond);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_init.html>.
 *
 * Initializes the condition variable referenced by `cond` with attributes
 * referenced by `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `attr` is not initialized when calling this function.
 * - `cond` has already been initialized.
 */
int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_signal.html>.
 *
 * As a single atomic operation, determines which threads, if any, are blocked
 * on the specified condition variable `cond` and unblocks at least one of
 * these threads.
 *
 * Has no effect if no threads are blocked on `cond`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 * Identical behaviour to `pthread_cond_broadcast()`.
 *
 * # Safety
 * It is undefined behaviour if `cond` is not initialized.
 */
int pthread_cond_signal(pthread_cond_t *cond);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_timedwait.html>.
 *
 * Blocks on a condition variable.
 *
 * Clock used for timeout depends on the clock attribute in `cond`. Defaults
 * to `CLOCK_REALTIME` if not set.
 *
 * Upon success, returns `0` and the `mutex` shall have been locked and shall
 * be owned by the calling thread. Upon failure, an error number is returned
 * to indicate the error.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `cond` is uninitialized.
 * - `mutex` is uninitialized.
 * - `mutex` has not been locked by the calling thread before calling this
 *   function.
 */
int pthread_cond_timedwait(pthread_cond_t *cond,
                           pthread_mutex_t *mutex,
                           const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_clockwait.html>.
 *
 * Blocks on a condition variable.
 *
 * `clock_id` determines the clock to use for the timeout specified by
 * `abstime`.
 *
 * Upon success, returns `0` and the `mutex` shall have been locked and shall
 * be owned by the calling thread. Upon failure, an error number is returned
 * to indicate the error.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `cond` is uninitialized.
 * - `mutex` is uninitialized.
 * - `mutex` has not been locked by the calling thread before calling this
 *   function.
 */
int pthread_cond_clockwait(pthread_cond_t *cond,
                           pthread_mutex_t *mutex,
                           clockid_t clock_id,
                           const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_wait.html>.
 *
 * Blocks on a condition variable.
 *
 * Upon success, returns `0` and the `mutex` shall have been locked and shall
 * be owned by the calling thread. Upon failure, an error number is returned
 * to indicate the error.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `cond` is uninitialized.
 * - `mutex` is uninitialized.
 * - `mutex` has not been locked by the calling thread before calling this
 *   function.
 */
int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_condattr_destroy.html>.
 *
 * Destroys a condition variable attributes object.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicated the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - `attr` is uninitialized.
 * - `attr` is used after calling this function without reinitializing it.
 */
int pthread_condattr_destroy(pthread_condattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_condattr_getclock.html>.
 *
 * Obtains the value of the clock attribute from the attributes object
 * referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the clock attribute of
 * `attr` into the object refereced by the `clock_id` argument. Upon failure,
 * an error number is returned to indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is uninitialized.
 */
int pthread_condattr_getclock(const pthread_condattr_t *attr, clockid_t *clock_id);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_condattr_getpshared.html>.
 *
 * Obtains the value of the process-shared attribute from the attributes
 * object referenced by `attr`.
 *
 * Upon success, returns `0` and stores the value of the process-shared
 * attribute of `attr` into the object referenced by the `pshared` parameter.
 * Upon failure, an error number is returned to indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is uninitialized.
 */
int pthread_condattr_getpshared(const pthread_condattr_t *attr,
                                int *pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_condattr_init.html>.
 *
 * Initializes a condition variable attribute object `attr` with the default
 * value for all of the attributes defined by the implementation.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is already initialized.
 */
int pthread_condattr_init(pthread_condattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_condattr_setclock.html>.
 *
 * Sets the clock attribute in the attributes object referenced by `attr`.
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicated the error.
 *
 * # Implementation
 * Always successful, so will never return an error number.
 *
 * # Safety
 * It is undefined behaviour if `attr` is uninitialized.
 */
int pthread_condattr_setclock(pthread_condattr_t *attr, clockid_t clock_id);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_condattr_setpshared.html>.
 *
 * Sets the process-shared attribute in the attributes object referenced by
 * `attr`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if `attr` is uninitialized.
 */
int pthread_condattr_setpshared(pthread_condattr_t *attr,
                                int pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_getspecific.html>.
 */
void *pthread_getspecific(pthread_key_t key);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_setspecific.html>.
 */
int pthread_setspecific(pthread_key_t key, const void *value);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_key_create.html>.
 */
int pthread_key_create(pthread_key_t *key_ptr, Dtor destructor);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_key_delete.html>.
 */
int pthread_key_delete(pthread_key_t key);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_consistent.html>.
 */
int pthread_mutex_consistent(pthread_mutex_t *mutex);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_destroy.html>.
 */
int pthread_mutex_destroy(pthread_mutex_t *mutex);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_getprioceiling.html>.
 */
int pthread_mutex_getprioceiling(const pthread_mutex_t *mutex,
                                 int *prioceiling);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_init.html>.
 */
int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_lock.html>.
 */
int pthread_mutex_lock(pthread_mutex_t *mutex);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_setprioceiling.html>.
 */
int pthread_mutex_setprioceiling(pthread_mutex_t *mutex,
                                 int prioceiling,
                                 int *old_prioceiling);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_timedlock.html>.
 */
int pthread_mutex_timedlock(pthread_mutex_t *mutex, const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_trylock.html>.
 */
int pthread_mutex_trylock(pthread_mutex_t *mutex);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_unlock.html>.
 */
int pthread_mutex_unlock(pthread_mutex_t *mutex);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_destroy.html>.
 */
int pthread_mutexattr_destroy(pthread_mutexattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_getprioceiling.html>.
 */
int pthread_mutexattr_getprioceiling(const pthread_mutexattr_t *attr,
                                     int *prioceiling);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_getprotocol.html>.
 */
int pthread_mutexattr_getprotocol(const pthread_mutexattr_t *attr,
                                  int *protocol);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_getpshared.html>.
 */
int pthread_mutexattr_getpshared(const pthread_mutexattr_t *attr,
                                 int *pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_getrobust.html>.
 */
int pthread_mutexattr_getrobust(const pthread_mutexattr_t *attr,
                                int *robust);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_gettype.html>.
 */
int pthread_mutexattr_gettype(const pthread_mutexattr_t *attr, int *ty);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_init.html>.
 */
int pthread_mutexattr_init(pthread_mutexattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_setprioceiling.html>.
 */
int pthread_mutexattr_setprioceiling(pthread_mutexattr_t *attr,
                                     int prioceiling);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_setprotocol.html>.
 */
int pthread_mutexattr_setprotocol(pthread_mutexattr_t *attr,
                                  int protocol);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_setpshared.html>.
 */
int pthread_mutexattr_setpshared(pthread_mutexattr_t *attr,
                                 int pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_setrobust.html>.
 */
int pthread_mutexattr_setrobust(pthread_mutexattr_t *attr,
                                int robust);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_settype.html>.
 */
int pthread_mutexattr_settype(pthread_mutexattr_t *attr, int ty);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_once.html>.
 *
 * The first call to this function by any thread in a process, with a given
 * `once_control`, shall call the `init_routine` with no arguments. Subsequent
 * calls of this function with the same `once_control` shall not call the
 * `init_routine()`. The `once_control` parameter shall determine whether the
 * associated initialization routine has been called.
 *
 * This function is not a cancellation point. However, if `init_routine()` is
 * a cancellation point and is canceled, the effect on `once_control` shall be
 * as if this function was never called.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned to
 * indicate the error.
 *
 * # Implementation
 * Always succeeds so never returns an error number.
 *
 * # Safety
 * It is undefined behaviour for the following:
 * - The call to `init_routine()` is terminated by a call to `longjmp()` or
 *   `siglongjmp()`.
 * - If `once_control` has automatic storage duration or is not initialized by
 *   `PTHREAD_ONCE_INIT`.
 */
int pthread_once(pthread_once_t *once_control, void (*init_routine)(void));

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_init.html>.
 */
int pthread_rwlock_init(pthread_rwlock_t *rwlock, const pthread_rwlockattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_rdlock.html>.
 */
int pthread_rwlock_rdlock(pthread_rwlock_t *rwlock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_clockrdlock.html>.
 */
int pthread_rwlock_clockrdlock(pthread_rwlock_t *rwlock,
                               clockid_t clock_id,
                               const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_timedrdlock.html>.
 */
int pthread_rwlock_timedrdlock(pthread_rwlock_t *rwlock, const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_clockwrlock.html>.
 */
int pthread_rwlock_clockwrlock(pthread_rwlock_t *rwlock,
                               clockid_t clock_id,
                               const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_timedwrlock.html>.
 */
int pthread_rwlock_timedwrlock(pthread_rwlock_t *rwlock, const struct timespec *abstime);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_tryrdlock.html>.
 */
int pthread_rwlock_tryrdlock(pthread_rwlock_t *rwlock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_trywrlock.html>.
 */
int pthread_rwlock_trywrlock(pthread_rwlock_t *rwlock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_unlock.html>.
 */
int pthread_rwlock_unlock(pthread_rwlock_t *rwlock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_wrlock.html>.
 */
int pthread_rwlock_wrlock(pthread_rwlock_t *rwlock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlockattr_init.html>.
 */
int pthread_rwlockattr_init(pthread_rwlockattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlockattr_getpshared.html>.
 */
int pthread_rwlockattr_getpshared(const pthread_rwlockattr_t *attr,
                                  int *pshared_out);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlockattr_setpshared.html>.
 */
int pthread_rwlockattr_setpshared(pthread_rwlockattr_t *attr,
                                  int pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlockattr_destroy.html>.
 */
int pthread_rwlockattr_destroy(pthread_rwlockattr_t *attr);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_rwlock_destroy.html>.
 */
int pthread_rwlock_destroy(pthread_rwlock_t *rwlock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_spin_destroy.html>.
 *
 * Destroys the spin lock referenced by `lock` and releases any resources used
 * by the lock.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Implementation
 * Cannot fail on the Rust side so no error number is ever returned.
 *
 * # Safety
 * It is undefined behaviour for any of the following:
 * - Subsequent use of `lock` after calling this function unless it is
 *   reinitialized with `pthread_spin_init()`.
 * - This function is called when a thread holds the lock.
 * - `lock` is uninitialized when this function is called.
 */
int pthread_spin_destroy(pthread_spinlock_t *lock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_spin_init.html>.
 *
 * Allocates any resources required to use the spin lock referenced to by
 * `lock` and initializes the lock to an unlocked state.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Implementation
 * Cannot fail on the Rust side so no error number is ever returned.
 *
 * # Safety
 * It is undefined behaviour for any of the following:
 * - `lock` has already been initialized.
 * - `lock` has been used before being initialized by this function.
 */
int pthread_spin_init(pthread_spinlock_t *lock, int _pshared);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_spin_lock.html>.
 *
 * Locks the spin lock referenced by `lock`.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Safety
 * It is undefined behaviour for any of the following:
 * - `lock` is uninitialized.
 * - The calling thread holds `lock` at the time the call is made.
 */
int pthread_spin_lock(pthread_spinlock_t *lock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_spin_trylock.html>.
 *
 * Locks the spin lock referenced by `lock` if it is not held by any thread.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Safety
 * It is undefined behaviour if `lock` is uninitialized.
 */
int pthread_spin_trylock(pthread_spinlock_t *lock);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_spin_unlock.html>.
 *
 * Releases the spin lock referenced by `lock` which was locked via the
 * `pthread_spin_lock()` or `pthread_spin_trylock()` functions.
 *
 * Upon success, returns `0`. Upon failure, an error number is returned.
 *
 * # Implementation
 * Cannot fail on the Rust side so no error number is ever returned.
 *
 * # Safety
 * It is undefined behaviour for any of the following:
 * - `lock` is uninitialized.
 * - `lock` is not held by the calling thread.
 */
int pthread_spin_unlock(pthread_spinlock_t *lock);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_PTHREAD_H */
