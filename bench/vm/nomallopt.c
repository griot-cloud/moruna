/* Diagnosis only: ignore the runtime's mallopt settings when NOMALLOPT says so. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <malloc.h>
#include <stdlib.h>
#include <string.h>
int mallopt(int param, int value) {
    static int (*real)(int, int) = 0;
    if (!real) real = (int (*)(int, int))dlsym(RTLD_NEXT, "mallopt");
    const char *skip = getenv("NOMALLOPT");
    if (skip && param == M_ARENA_MAX) return 1;
    if (skip && strcmp(skip, "all") == 0 && param == M_MMAP_THRESHOLD) return 1;
    return real(param, value);
}
