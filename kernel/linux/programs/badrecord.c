/* KD2's refused record (SDD 30 k): a task of the data plane, unmodified static musl, that takes
   its output slot's window with the kernel's native batch_get and puts bytes that are not an
   Arrow record with batch_put. The engine's check refuses the record before anything else of the
   engine sees it (SDD 29 DP1), and the kernel ends this process with the check's sentence; it
   waits for the next window after its put, and is ended before that wait returns. Its input
   slot is 0 and its output slot 1, as
   moruna.linux's run_plane gives them. */
#include <stdio.h>
#include <string.h>

#define BATCH_GET 0x10100UL
#define BATCH_PUT 0x10101UL
#define OUTPUT 1

static long call1(unsigned long nr, unsigned long a, unsigned long *rdx_out) {
    long rax;
    unsigned long rdx;
    __asm__ volatile("syscall" : "=a"(rax), "=d"(rdx) : "a"(nr), "D"(a) : "rcx", "r11", "memory");
    *rdx_out = rdx;
    return rax;
}

static long call3(unsigned long nr, unsigned long a, unsigned long b, unsigned long c) {
    long rax;
    __asm__ volatile("syscall"
                     : "=a"(rax), "+d"(c)
                     : "a"(nr), "D"(a), "S"(b)
                     : "rcx", "r11", "memory");
    return rax;
}

int main(void) {
    unsigned long len;
    long window = call1(BATCH_GET, OUTPUT, &len);
    if (window <= 0 || len < 4096) {
        fprintf(stderr, "badrecord: batch_get of the output slot answered %ld, %lu bytes\n", window, len);
        return 4;
    }
    const char garbage[] = "this is not an Arrow IPC record";
    memset((void *)window, 0x5a, 4096);
    memcpy((void *)window, garbage, sizeof garbage);
    long r = call3(BATCH_PUT, OUTPUT, (unsigned long)window, 4096);
    if (r != 0) {
        fprintf(stderr, "badrecord: batch_put refused the window with %ld\n", r);
        return 5;
    }
    /* batch_put only queues the record (SDD 29 f.3). On another CPU the engine may
       not have checked it yet. Stay alive for its refusal instead of racing that
       check with a normal exit, which cannot prove the kernel ended this task. */
    r = call1(BATCH_GET, OUTPUT, &len);
    fprintf(stderr, "badrecord: batch_get after the bad record answered %ld; not ended\n", r);
    return 5;
}
