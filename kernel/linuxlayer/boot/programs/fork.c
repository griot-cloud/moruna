// SPDX-License-Identifier: MPL-2.0
/* B2's booted fork test (static musl, run first): a child is a Moruna process of its own
   that shares the task's handles. The child gets its own copy of memory, writes to its
   parent through a pipe and exits with a code the parent's wait sees; the parent's
   memory is unchanged; a second generation forks from the child. */
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int counter = 1;

static int fail(const char *what) {
    printf("fork: FAIL: %s\n", what);
    fflush(stdout);
    return 1;
}

int main(void) {
    int fds[2];
    if (pipe(fds)) return fail("pipe");
    pid_t child = fork();
    if (child < 0) return fail("fork");
    if (child == 0) {
        counter = 42;
        close(fds[0]);
        const char *msg = "from the child";
        if (write(fds[1], msg, strlen(msg)) != (ssize_t)strlen(msg)) _exit(10);
        close(fds[1]);
        pid_t grandchild = fork();
        if (grandchild < 0) _exit(11);
        if (grandchild == 0) _exit(counter == 42 ? 5 : 12);
        int status;
        if (waitpid(grandchild, &status, 0) != grandchild) _exit(13);
        _exit(WIFEXITED(status) ? WEXITSTATUS(status) + 2 : 14);
    }
    close(fds[1]);
    char buf[64] = {0};
    ssize_t n = read(fds[0], buf, sizeof buf - 1);
    if (n <= 0 || strcmp(buf, "from the child")) return fail("reading the child's pipe");
    int status;
    if (waitpid(child, &status, 0) != child) return fail("waitpid");
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 7)
        return fail("the child's exit status");
    if (counter != 1) return fail("the parent's memory changed");
    printf("fork: ok: a child and a grandchild ran in their own processes, the pipe carried '%s', exit 7\n", buf);
    return 0;
}
