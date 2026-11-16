/* SPDX-License-Identifier: MPL-2.0 */
/* B2-fs's checks through a task start (kernel/linuxlayer, stream B2): the `linux` server
   runs this static musl program on a Linux personality of its own, with an account and a
   scratch account (linuxlayer-check). It checks:
   - the image's root: a file of the image reads;
   - /proc and /sys: the paths programs read open and hold text, /proc/self/exe names the
     program, /proc/self/fd lists 0 to 2, and /proc/meminfo's total is the account's limit;
   - /tmp: writes stop with ENOSPC within the scratch account's limit, and room comes back
     when the file goes;
   - epoll: a POSIX thread adds an eventfd to the epoll the main thread is waiting on;
   - descriptor 2: a line written there reaches the server.
   Usage: taskcheck LIMIT_KIB SCRATCH_BYTES. Prints "taskcheck: NAME: ok: ..." for each,
   or "taskcheck: FAIL: ...", and exits 0 only when every check passed. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

static char text[65536];
static char block[65536];

static int failed(const char *what) {
    printf("taskcheck: FAIL: %s (errno %d)\n", what, errno);
    return 1;
}

/* Reads at most sizeof text - 1 bytes of `path` into `text`; the count, or -1. */
static long slurp(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    size_t got = 0;
    while (got < sizeof text - 1) {
        ssize_t n = read(fd, text + got, sizeof text - 1 - got);
        if (n < 0) { close(fd); return -1; }
        if (n == 0) break;
        got += (size_t)n;
    }
    close(fd);
    text[got] = 0;
    return (long)got;
}

static int image(void) {
    if (slurp("/kc1/greeting.txt") <= 0 || !strstr(text, "Moruna KC1 greets you"))
        return failed("the image's /kc1/greeting.txt");
    printf("taskcheck: image: ok: /kc1/greeting.txt read from the image's root\n");
    return 0;
}

static int proc(unsigned long limit_kib) {
    static const char *paths[] = {
        "/proc/self/status", "/proc/self/maps", "/proc/self/mounts", "/proc/self/mountinfo",
        "/proc/meminfo", "/proc/cpuinfo", "/proc/stat", "/proc/loadavg", "/proc/mounts",
        "/proc/sys/vm/overcommit_memory", "/proc/sys/kernel/yama/ptrace_scope",
        "/sys/devices/system/cpu/online", "/sys/devices/system/cpu/possible",
        "/sys/devices/system/cpu/present", "/sys/kernel/mm/transparent_hugepage/enabled",
    };
    unsigned n = sizeof paths / sizeof paths[0];
    for (unsigned i = 0; i < n; i++) {
        if (slurp(paths[i]) <= 0) {
            printf("taskcheck: FAIL: %s (errno %d)\n", paths[i], errno);
            return 1;
        }
    }
    char exe[256];
    ssize_t len = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (len < 0) return failed("readlink /proc/self/exe");
    exe[len] = 0;
    if (strcmp(exe, "/kc1/bin/taskcheck") != 0) {
        printf("taskcheck: FAIL: /proc/self/exe is %s\n", exe);
        return 1;
    }
    DIR *fds = opendir("/proc/self/fd");
    if (!fds) return failed("opendir /proc/self/fd");
    int seen = 0;
    struct dirent *e;
    while ((e = readdir(fds)) != NULL) {
        if (!strcmp(e->d_name, "0")) seen |= 1;
        if (!strcmp(e->d_name, "1")) seen |= 2;
        if (!strcmp(e->d_name, "2")) seen |= 4;
    }
    closedir(fds);
    if (seen != 7) {
        printf("taskcheck: FAIL: /proc/self/fd lists descriptors %d of 0 to 2 (bits)\n", seen);
        return 1;
    }
    unsigned long total = 0;
    char *line = slurp("/proc/meminfo") > 0 ? strstr(text, "MemTotal:") : NULL;
    if (!line || sscanf(line, "MemTotal: %lu kB", &total) != 1 || total != limit_kib) {
        printf("taskcheck: FAIL: /proc/meminfo's MemTotal is %lu kB, not the account's %lu kB\n",
               total, limit_kib);
        return 1;
    }
    printf("taskcheck: proc: ok: %u /proc and /sys paths read, /proc/self/exe is %s, "
           "/proc/self/fd lists 0 to 2, MemTotal is the account's %lu kB\n", n, exe, total);
    return 0;
}

static int tmp(unsigned long scratch) {
    memset(block, 'x', sizeof block);
    int fd = open("/tmp/fill", O_CREAT | O_WRONLY | O_TRUNC, 0600);
    if (fd < 0) return failed("open /tmp/fill");
    unsigned long total = 0;
    ssize_t n;
    while ((n = write(fd, block, sizeof block)) > 0) {
        total += (unsigned long)n;
        if (total > 2 * scratch) break;
    }
    int why = errno;
    close(fd);
    if (n >= 0 || why != ENOSPC || total > scratch || total < scratch / 2) {
        printf("taskcheck: FAIL: /tmp took %lu bytes of a %lu-byte quota, then errno %d\n",
               total, scratch, n < 0 ? why : 0);
        return 1;
    }
    if (unlink("/tmp/fill") != 0) return failed("unlink /tmp/fill");
    fd = open("/tmp/again", O_CREAT | O_WRONLY | O_TRUNC, 0600);
    if (fd < 0 || write(fd, block, sizeof block) != (ssize_t)sizeof block)
        return failed("writing /tmp/again after the file went");
    close(fd);
    unlink("/tmp/again");
    printf("taskcheck: tmp: ok: /tmp took %lu bytes of its %lu-byte quota, then ENOSPC; "
           "room came back when the file went\n", total, scratch);
    return 0;
}

static int epfd, evfd;

static void *adder(void *unused) {
    (void)unused;
    usleep(50000);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 7};
    if (epoll_ctl(epfd, EPOLL_CTL_ADD, evfd, &ev) != 0) return (void *)1;
    unsigned long long one = 1;
    if (write(evfd, &one, sizeof one) != sizeof one) return (void *)2;
    return NULL;
}

static int epoll(void) {
    epfd = epoll_create1(0);
    evfd = eventfd(0, EFD_NONBLOCK);
    if (epfd < 0 || evfd < 0) return failed("epoll_create1 or eventfd");
    pthread_t thread;
    if (pthread_create(&thread, NULL, adder, NULL) != 0) return failed("pthread_create");
    struct epoll_event ev;
    int n = epoll_wait(epfd, &ev, 1, 5000);
    void *added;
    pthread_join(thread, &added);
    if (added != NULL || n != 1 || ev.data.u64 != 7 || !(ev.events & EPOLLIN)) {
        printf("taskcheck: FAIL: epoll_wait answered %d (thread %ld)\n", n, (long)added);
        return 1;
    }
    printf("taskcheck: epoll: ok: a thread's epoll_ctl woke the main thread's epoll_wait\n");
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        printf("taskcheck: FAIL: usage: taskcheck LIMIT_KIB SCRATCH_BYTES\n");
        return 2;
    }
    unsigned long limit_kib = strtoul(argv[1], NULL, 10);
    unsigned long scratch = strtoul(argv[2], NULL, 10);
    int bad = image();
    bad |= proc(limit_kib);
    bad |= tmp(scratch);
    bad |= epoll();
    fprintf(stderr, "taskcheck: descriptor 2 reached the server\n");
    return bad;
}
