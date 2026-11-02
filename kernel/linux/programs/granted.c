/* KD2-T4 (SDD 30 k): what moruna.linux 2.0 gives a program run with run_granted, as an
   unmodified static musl program sees it:
   - its start page, through the auxiliary vector, names its probe and the read-only filesystem capability,
     and the native handle_info call on that handle answers, so a Linux program holds a
     handle and reaches the kernel's own calls;
   - sysinfo, /proc/meminfo and /sys/devices/system/cpu/online say the budget it was
     granted and the CPUs: the total its account's limit, what is free what is left of it
     (moruna.linux 2.0), and the native account_info call on its own account (ABI 2.3)
     names the account its starter carved, with that limit and something charged to it;
   - eventfd and epoll as tokio's mio uses them: an edge-triggered eventfd, a wait that
     blocks until another thread writes, a wait that times out, a blocking read woken by
     a write, a nonblocking read refused with EAGAIN;
   - rename in its scratch directory, replacing a file, and RENAME_NOREPLACE refused.
   Prints one line beginning "granted: ok:" and exits 0, or a "granted: FAIL:" line and
   exits 1. Built with musl, -static. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <time.h>
#include <unistd.h>

#define AT_MORUNA_START 0x4d4f5255UL
#define START_MAGIC 0x31534b4dU
#define NATIVE_HANDLE_INFO 0x10003UL
#define NATIVE_ACCOUNT_INFO 0x10033UL
#define WANT_MEMORY (256UL << 20)

static int fail(const char *what) {
    printf("granted: FAIL: %s (errno %d)\n", what, errno);
    return 1;
}

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}

static long native1(unsigned long nr, unsigned long a) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a) : "rcx", "r11", "rdx", "memory");
    return r;
}

static long native2(unsigned long nr, unsigned long a, unsigned long b) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b) : "rcx", "r11", "rdx", "memory");
    return r;
}

static int efd_wait, efd_block;

static void *writer(void *arg) {
    (void)arg;
    struct timespec t = {0, 50 * 1000 * 1000};
    nanosleep(&t, 0);
    uint64_t one = 1;
    if (write(efd_wait, &one, 8) != 8) return (void *)1;
    nanosleep(&t, 0);
    uint64_t three = 3;
    if (write(efd_block, &three, 8) != 8) return (void *)2;
    return 0;
}

static int read_file(const char *path, char *buf, size_t size) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, size - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return (int)n;
}

int main(void) {
    /* The start page. */
    const unsigned char *page = (const unsigned char *)getauxval(AT_MORUNA_START);
    if (!page) return fail("no AT_MORUNA_START in the auxiliary vector");
    uint32_t magic, count, handle = 0, filesystem = 0;
    unsigned probe_count = 0, filesystem_count = 0;
    memcpy(&magic, page, 4);
    memcpy(&count, page + 8, 4);
    if (magic != START_MAGIC || count != 2)
        return fail("the start page does not name probe and filesystem");
    for (uint32_t i = 0; i < count; i++) {
        const unsigned char *entry = page + 48 + i * 40;
        uint32_t raw;
        memcpy(&raw, entry, 4);
        if (strncmp((const char *)entry + 8, "probe", 32) == 0) {
            handle = raw;
            probe_count++;
        } else if (strncmp((const char *)entry + 8, "filesystem", 32) == 0) {
            filesystem = raw;
            filesystem_count++;
        } else return fail("unexpected start-page grant");
    }
    if (probe_count != 1 || filesystem_count != 1 || handle == filesystem)
        return fail("duplicate or missing start-page grant");
    long info = native1(NATIVE_HANDLE_INFO, handle);
    if (info < 0 || native1(NATIVE_HANDLE_INFO, filesystem) < 0)
        return fail("handle_info on an actual granted handle");
    /* A value absent from both named grants, independent of slot adjacency. */
    uint32_t absent = 0x7fffffffU;
    while (absent == handle || absent == filesystem) absent--;
    if (native1(NATIVE_HANDLE_INFO, absent) >= 0)
        return fail("handle_info answered a value nothing was granted under");

    /* The machine. */
    struct sysinfo si;
    if (sysinfo(&si) != 0) return fail("sysinfo");
    if ((unsigned long)si.totalram * si.mem_unit != WANT_MEMORY)
        return fail("sysinfo's totalram is not the budget");
    char text[512];
    if (read_file("/proc/meminfo", text, sizeof text) < 0) return fail("/proc/meminfo");
    unsigned long kib = 0;
    if (sscanf(text, "MemTotal: %lu kB", &kib) != 1 || kib * 1024 != WANT_MEMORY)
        return fail("/proc/meminfo's MemTotal is not the budget");
    unsigned long free_bytes = (unsigned long)si.freeram * si.mem_unit;
    if (free_bytes == 0 || free_bytes >= WANT_MEMORY)
        return fail("sysinfo's freeram is not what is left of its account");
    /* Its own account, read with the kernel's call (zero names the caller's own): limit,
       charged, peak, held for children, then its name. */
    uint64_t account[8];
    if (native2(NATIVE_ACCOUNT_INFO, 0, (unsigned long)account) != 0)
        return fail("account_info on its own account");
    if (account[0] != WANT_MEMORY || account[1] == 0 || account[2] < account[1] ||
        strncmp((const char *)&account[4], "linux-test.granted", 32) != 0)
        return fail("its account is not the one its starter carved");
    if (read_file("/sys/devices/system/cpu/online", text, sizeof text) < 0 || text[0] != '0')
        return fail("/sys/devices/system/cpu/online");
    long cpus = sysconf(_SC_NPROCESSORS_ONLN);

    /* eventfd and epoll. */
    efd_wait = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    efd_block = eventfd(0, EFD_CLOEXEC);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (efd_wait < 0 || efd_block < 0 || ep < 0) return fail("eventfd or epoll_create1");
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.u64 = 0x5eed};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, efd_wait, &ev) != 0) return fail("epoll_ctl add");
    if (epoll_ctl(ep, EPOLL_CTL_ADD, efd_wait, &ev) == 0 || errno != EEXIST)
        return fail("a second add was not EEXIST");
    struct epoll_event got[4];
    uint64_t t0 = now_ns();
    if (epoll_wait(ep, got, 4, 100) != 0) return fail("an idle wait did not time out with 0");
    uint64_t idle_ms = (now_ns() - t0) / 1000000;
    if (idle_ms < 100) return fail("an idle wait returned before its timeout");
    uint64_t value;
    if (read(efd_wait, &value, 8) != -1 || errno != EAGAIN) return fail("an empty nonblocking read");
    pthread_t t;
    if (pthread_create(&t, 0, writer, 0) != 0) return fail("pthread_create");
    t0 = now_ns();
    int n = epoll_wait(ep, got, 4, 10000);
    uint64_t woke_ms = (now_ns() - t0) / 1000000;
    if (n != 1 || got[0].data.u64 != 0x5eed || !(got[0].events & EPOLLIN))
        return fail("the wait was not woken by the other thread's write");
    if (epoll_wait(ep, got, 4, 0) != 0) return fail("an edge reported twice");
    if (read(efd_block, &value, 8) != 8 || value != 3) return fail("a blocking read woken by a write");
    void *r;
    if (pthread_join(t, &r) != 0 || r != 0) return fail("the writer");
    if (read(efd_wait, &value, 8) != 8 || value != 1) return fail("the counter read");

    /* rename in the scratch directory. */
    int fd = open("/tmp/part.tmp", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "rows", 4) != 4) return fail("writing /tmp/part.tmp");
    close(fd);
    fd = open("/tmp/part", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) return fail("writing /tmp/part");
    close(fd);
    if (rename("/tmp/part.tmp", "/tmp/part") != 0) return fail("rename over a file");
    if (read_file("/tmp/part", text, sizeof text) != 4 || strcmp(text, "rows") != 0)
        return fail("the renamed file's bytes");
    if (access("/tmp/part.tmp", F_OK) == 0) return fail("the old name is still there");
    fd = open("/tmp/other", O_CREAT | O_WRONLY, 0644);
    close(fd);
    if (syscall(SYS_renameat2, AT_FDCWD, "/tmp/other", AT_FDCWD, "/tmp/part", 1) != -1 || errno != EEXIST)
        return fail("RENAME_NOREPLACE over a file");

    printf("granted: ok: start page names probe (handle %u, handle_info answered), "
           "sysinfo and /proc/meminfo say %lu MiB, %lu KiB of it free, %ld CPUs; its account "
           "linux-test.granted has %llu KiB charged; epoll: idle wait %llu ms, "
           "woken by a write in %llu ms, an edge once, a blocking read woken; rename replaced "
           "a file and RENAME_NOREPLACE was EEXIST\n",
           handle, WANT_MEMORY >> 20, free_bytes >> 10, cpus,
           (unsigned long long)(account[1] >> 10), (unsigned long long)idle_ms,
           (unsigned long long)woke_ms);
    return 0;
}
