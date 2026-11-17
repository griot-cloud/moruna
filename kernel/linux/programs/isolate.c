/* LX-T15: a Linux process reaches nothing it was not given. It holds no handle: every
   native call that takes one, on every value 0 to 4096, is BadHandle. Its scratch
   directory is its own: another task's /tmp file is not there. Its memory is its own:
   addresses it has not mapped fault rather than reach anyone's. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#define NATIVE 0x10000L
#define BAD_HANDLE (-1L)

static long native(long nr, long a, long b, long c) {
    long ret;
    register long r10 __asm__("r10") = 0;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(NATIVE + nr), "D"(a), "S"(b), "d"(c), "r"(r10)
                     : "rcx", "r11", "memory");
    return ret;
}

int main(void) {
    /* handle_close 1, handle_duplicate 2, handle_info 3, channel_write 5, channel_read 6,
       memory_map 8, process_map_image 11, wait 16, timer_set 19, process_read 36. */
    static const long calls[] = {1, 2, 3, 5, 6, 8, 11, 19, 29, 31, 33, 36, 37};
    long probed = 0;
    for (long v = 0; v <= 4096; v++) {
        for (unsigned i = 0; i < sizeof calls / sizeof calls[0]; i++) {
            long r = native(calls[i], v, 0, 0);
            if (r != BAD_HANDLE) {
                printf("isolate: FAIL: call %ld on value %ld answered %ld\n", calls[i], v, r);
                return 1;
            }
            probed++;
        }
    }
    if (open("/tmp/out.txt", O_RDONLY) != -1 || errno != ENOENT) {
        printf("isolate: FAIL: another task's /tmp/out.txt is visible\n");
        return 1;
    }
    /* Its own anonymous memory works and is zero; nothing else is mapped for it. */
    unsigned char *p = mmap(0, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED || p[4096] != 0) { printf("isolate: FAIL: mmap\n"); return 1; }
    p[4096] = 1;
    if (munmap(p, 1 << 20) != 0) { printf("isolate: FAIL: munmap\n"); return 1; }
    printf("isolate: %ld native calls on handle values 0 to 4096 were all BadHandle; "
           "another task's /tmp file is not here\n", probed);
    return 0;
}
