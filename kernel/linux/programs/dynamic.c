/* LX-T12: a dynamically linked glibc program, started through its interpreter
   (ld-linux-x86-64.so.2) from the image's Debian layer. */
#include <gnu/libc-version.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int readonly_mapping_regression(void) {
    const size_t page = 4096;
    unsigned char expected[4096];
    int fd = open("/kc1/bin/dynamic", O_RDONLY);
    if (fd < 0 || pread(fd, expected, page, page * 2) != (ssize_t)page) return 1;
    unsigned char *whole = mmap(NULL, page * 2, PROT_READ, MAP_PRIVATE, fd, page);
    if (whole == MAP_FAILED) return 2;
    unsigned char *part = mmap(whole + page, page, PROT_READ,
                               MAP_PRIVATE | MAP_FIXED, fd, page * 2);
    if (part != whole + page || memcmp(part, expected, page)) return 3;
    unsigned char *private = mmap(NULL, page, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE, fd, page * 2);
    if (private == MAP_FAILED) return 4;
    private[0] ^= 0xff;
    if (memcmp(part, expected, page)) return 5;
    unsigned char byte;
    if (pread(fd, &byte, 1, page * 2) != 1 || byte != expected[0]) return 6;
    if (close(fd)) return 7;
    // Closing the file retires its cache handle, not the active VM mappings.
    if (memcmp(part, expected, page)) return 8;
    int other = open("/kc1/greeting.txt", O_RDONLY);
    unsigned char greeting[128];
    ssize_t n = other < 0 ? -1 : pread(other, greeting, sizeof greeting, 0);
    if (n <= 0) return 9;
    unsigned char *fresh = mmap(NULL, page, PROT_READ, MAP_PRIVATE, other, 0);
    if (fresh == MAP_FAILED || memcmp(fresh, greeting, (size_t)n)) return 10;
    if (close(other) || munmap(fresh, page) || munmap(private, page)
        || munmap(whole, page * 2)) return 11;
    puts("dynamic: readonly subrange alias, private copy and descriptor reuse ok");
    return 0;
}

static int readlink_regression(void) {
    char target[4096];
    const char *paths[] = {"/kc1/bin/dynamic", "/kc1/bin", "/kc1/no-such-readlink-fixture"};
    const int errors[] = {EINVAL, EINVAL, ENOENT};
    for (size_t i = 0; i < sizeof paths / sizeof paths[0]; ++i) {
        errno = 0;
        if (readlink(paths[i], target, sizeof target) != -1 || errno != errors[i]) return 1;
        errno = 0;
        if (readlinkat(AT_FDCWD, paths[i], target, sizeof target) != -1 || errno != errors[i]) return 2;
    }
    int dir = open("/kc1/bin", O_RDONLY | O_DIRECTORY);
    if (dir < 0) return 3;
    errno = 0;
    if (readlinkat(dir, "dynamic", target, sizeof target) != -1 || errno != EINVAL) return 4;
    errno = 0;
    if (readlinkat(dir, "no-such-readlink-fixture", target, sizeof target) != -1 || errno != ENOENT) return 5;
    if (close(dir)) return 6;
    const char *exe = "/kc1/bin/dynamic";
    ssize_t n = readlink("/proc/self/exe", target, sizeof target);
    if (n != (ssize_t)strlen(exe) || memcmp(target, exe, (size_t)n)) return 7;
    n = readlinkat(AT_FDCWD, "/proc/self/exe", target, sizeof target);
    if (n != (ssize_t)strlen(exe) || memcmp(target, exe, (size_t)n)) return 8;
    char *canonical = realpath("/kc1/bin/dynamic", NULL);
    if (!canonical || strcmp(canonical, exe)) return 9;
    free(canonical);
    puts("dynamic: readlink non-link, missing, real target and canonical path ok");
    return 0;
}

int main(void) {
    int link = readlink_regression();
    if (link) { fprintf(stderr, "dynamic: readlink regression failed %d\n", link); return 1; }
    int mapping = readonly_mapping_regression();
    if (mapping) { fprintf(stderr, "dynamic: mapping regression failed %d\n", mapping); return 1; }
    char *copy = strdup("through ld-linux");
    if (!copy) return 1;
    printf("dynamic glibc %s: hello %s, pid %d\n", gnu_get_libc_version(), copy, (int)getpid());
    free(copy);
    return 0;
}
