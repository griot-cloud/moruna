/* LX-T10: a static musl program prints, reads a file of its image, writes a file in
   /tmp, reads it back, and exits 0. */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    char greeting[128] = {0};
    int fd = open("/kc1/greeting.txt", O_RDONLY);
    if (fd < 0) { perror("hello: FAIL: open /kc1/greeting.txt"); return 1; }
    ssize_t n = read(fd, greeting, sizeof greeting - 1);
    close(fd);
    if (n <= 0) { perror("hello: FAIL: read"); return 1; }
    if (greeting[n - 1] == '\n') greeting[n - 1] = 0;

    const char *text = "written to the scratch directory";
    fd = open("/tmp/out.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("hello: FAIL: open /tmp/out.txt"); return 1; }
    if (write(fd, text, strlen(text)) != (ssize_t)strlen(text)) { perror("hello: FAIL: write"); return 1; }
    close(fd);
    char back[128] = {0};
    fd = open("/tmp/out.txt", O_RDONLY);
    if (fd < 0 || read(fd, back, sizeof back - 1) != (ssize_t)strlen(text)) {
        perror("hello: FAIL: reading /tmp/out.txt back");
        return 1;
    }
    close(fd);
    printf("hello from musl (%s, %d args): the image says '%s'; /tmp gave back '%s'\n",
           argv[0], argc, greeting, back);
    return strcmp(back, text) == 0 ? 0 : 1;
}
