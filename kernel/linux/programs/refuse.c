/* LX-T14: a call outside the table (socket) is refused with ENOSYS; the process is not
   killed, and goes on. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>

int main(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (s != -1 || errno != ENOSYS) {
        printf("refuse: FAIL: socket gave %d, errno %d\n", s, errno);
        return 1;
    }
    printf("refuse: socket() was refused with ENOSYS (%s) and the process went on\n", strerror(errno));
    return 0;
}
