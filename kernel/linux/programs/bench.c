/* LX-T16: the forward's round trip and the fast calls' latency. getppid is answered
   by the personality server (a forwarded call); clock_gettime, gettid and a futex wake
   with no waiter are answered by the kernel (fast calls). The boot runs every test at
   once, so each figure is given as the least of its samples, which is the mechanism's
   own cost, and the median, which is what the shared CPUs gave. */
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* FUTEX_WAKE | FUTEX_PRIVATE_FLAG. */
#define FUTEX_WAKE_PRIVATE 129
#define FORWARDED 2000
#define BATCHES 40
#define BATCH 500

static int word;

static unsigned long long now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ULL + t.tv_nsec;
}

static int ascending(const void *a, const void *b) {
    unsigned long long x = *(const unsigned long long *)a, y = *(const unsigned long long *)b;
    return x < y ? -1 : x > y;
}

static unsigned long long samples[FORWARDED];

/* The least and the median of `n` samples. */
static void summary(unsigned long long *s, int n, unsigned long long *least, unsigned long long *median) {
    qsort(s, n, sizeof s[0], ascending);
    *least = s[0];
    *median = s[n / 2];
}

/* Batches of `BATCH` calls of `which`: each sample is a batch's mean. */
static void fast(int which, unsigned long long *least, unsigned long long *median) {
    static unsigned long long batches[BATCHES];
    struct timespec t;
    for (int b = 0; b < BATCHES; b++) {
        unsigned long long t0 = now();
        for (int i = 0; i < BATCH; i++) {
            if (which == 0) clock_gettime(CLOCK_MONOTONIC, &t);
            else if (which == 1) syscall(SYS_gettid);
            else syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0);
        }
        batches[b] = (now() - t0) / BATCH;
    }
    summary(batches, BATCHES, least, median);
}

int main(void) {
    for (int i = 0; i < FORWARDED; i++) {
        unsigned long long t0 = now();
        syscall(SYS_getppid);
        samples[i] = now() - t0;
    }
    unsigned long long fl, fm, cl, cm, tl, tm, xl, xm;
    summary(samples, FORWARDED, &fl, &fm);
    fast(0, &cl, &cm);
    fast(1, &tl, &tm);
    fast(2, &xl, &xm);
    printf("bench: forwarded call (getppid) %llu ns a round trip at least, %llu ns the median; "
           "fast calls at least (median): clock_gettime %llu ns (%llu), gettid %llu ns (%llu), "
           "futex wake %llu ns (%llu)\n", fl, fm, cl, cm, tl, tm, xl, xm);
    return 0;
}
