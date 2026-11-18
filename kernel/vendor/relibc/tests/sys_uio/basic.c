#include <assert.h>
#include <errno.h>
#include <limits.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

int main(void) {
    int pip[2];
    assert(pipe(pip) == 0);

    char *p1 = "Hello ";
    char *p2 = "World";
    char *p3 = "!";
    struct iovec wv[3] = {
        {p1, 6},
        {p2, 5},
        {p3, 1},
    };
    assert(writev(pip[1], wv, 3) == 12);

    char b1[6] = {0};
    char b2[5] = {0};
    char b3[1] = {0};
    struct iovec rv[3] = {
        {b1, 6},
        {b2, 5},
        {b3, 1},
    };
    assert(readv(pip[0], rv, 3) == 12);
    assert(memcmp(b1, "Hello ", 6) == 0);
    assert(memcmp(b2, "World", 5) == 0);
    assert(memcmp(b3, "!", 1) == 0);

    struct iovec one[1] = {{b1, 6}};
    errno = 0;
    assert(readv(pip[0], one, -1) == -1);
    assert(errno == EINVAL);
    errno = 0;
    assert(writev(pip[1], one, -1) == -1);
    assert(errno == EINVAL);
    errno = 0;

    assert(readv(pip[0], one, IOV_MAX + 1) == -1);
    assert(errno == EINVAL);
    errno = 0;
    assert(writev(pip[1], one, IOV_MAX + 1) == -1);
    assert(errno == EINVAL);

    assert(close(pip[0]) == 0);
    assert(close(pip[1]) == 0);

    return 0;
}
