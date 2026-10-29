/* LX-T12: a dynamically linked glibc program, started through its interpreter
   (ld-linux-x86-64.so.2) from the image's Debian layer. */
#include <gnu/libc-version.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char *copy = strdup("through ld-linux");
    if (!copy) return 1;
    printf("dynamic glibc %s: hello %s, pid %d\n", gnu_get_libc_version(), copy, (int)getpid());
    free(copy);
    return 0;
}
