/* LX-T11: 8 threads (pthread_create: clone with CLONE_THREAD, futex) each sum a slice
   of 1..N, add it to a total under a mutex, and are joined; the total must be exact. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

#define THREADS 8
#define N 4000000ULL

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t done_cond = PTHREAD_COND_INITIALIZER;
static unsigned long long total;
static int finished;

static void *work(void *arg) {
    unsigned long long i = (unsigned long long)(size_t)arg, sum = 0;
    unsigned long long lo = i * (N / THREADS) + 1, hi = (i + 1) * (N / THREADS);
    for (unsigned long long k = lo; k <= hi; k++) sum += k;
    pthread_mutex_lock(&lock);
    total += sum;
    finished++;
    pthread_cond_broadcast(&done_cond);
    pthread_mutex_unlock(&lock);
    return (void *)(size_t)(i + 1);
}

int main(void) {
    pthread_t t[THREADS];
    for (size_t i = 0; i < THREADS; i++)
        if (pthread_create(&t[i], 0, work, (void *)i) != 0) { puts("threads: FAIL: pthread_create"); return 1; }
    pthread_mutex_lock(&lock);
    while (finished < THREADS) pthread_cond_wait(&done_cond, &lock);
    pthread_mutex_unlock(&lock);
    for (size_t i = 0; i < THREADS; i++) {
        void *r;
        if (pthread_join(t[i], &r) != 0 || (size_t)r != i + 1) { puts("threads: FAIL: pthread_join"); return 1; }
    }
    unsigned long long want = N * (N + 1) / 2;
    printf("threads: %d threads summed 1..%llu = %llu (want %llu)\n", THREADS, N, total, want);
    return total == want ? 0 : 1;
}
