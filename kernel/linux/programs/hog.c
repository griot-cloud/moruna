/* KB5-T1 (SDD 20 f.18, SDD 28 k): a program that takes memory past its account, as an
   unmodified static musl program does it. Run with run_granted and an account of
   HOG_LIMIT bytes, it reads its machine's memory (the account's limit), maps twice that
   (which allocates nothing until it is touched), and writes it a page at a time,
   printing each MiB, and never stops on its own: the kernel ends it at the first page
   its account has no room for, naming the account and its limit on the console, while
   the personality serves every other program on. Exits 1 with a "hog: FAIL:" line if
   anything else happens first. Built with musl, -static. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysinfo.h>

#define HOG_LIMIT (16UL << 20)
#define BLOCK (1UL << 20)

int main(void) {
    struct sysinfo si;
    if (sysinfo(&si) != 0) {
        printf("hog: FAIL: sysinfo (errno %d)\n", errno);
        return 1;
    }
    unsigned long total = (unsigned long)si.totalram * si.mem_unit;
    if (total != HOG_LIMIT) {
        printf("hog: FAIL: its machine has %lu bytes, not its account's %lu\n", total, HOG_LIMIT);
        return 1;
    }
    printf("hog: its machine has %lu MiB; taking it a MiB at a time\n", total >> 20);
    fflush(stdout);
    /* Twice what it has, mapped once so only the pages it touches are charged: the
       kernel ends it long before the loop does. */
    unsigned long span = 2 * HOG_LIMIT;
    volatile uint8_t *area = mmap(0, span, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (area == MAP_FAILED) {
        printf("hog: FAIL: mmap of %lu MiB (errno %d)\n", span >> 20, errno);
        return 1;
    }
    for (unsigned long mib = 0; mib < span / BLOCK; mib++) {
        for (unsigned long at = 0; at < BLOCK; at += 4096) area[mib * BLOCK + at] = (uint8_t)mib;
        printf("hog: wrote MiB %lu\n", mib + 1);
        fflush(stdout);
    }
    printf("hog: FAIL: wrote twice its account and was not stopped\n");
    return 1;
}
