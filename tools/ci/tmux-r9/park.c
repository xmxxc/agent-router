#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int rename(const char *from, const char *to) {
    int (*next)(const char *, const char *) = dlsym(RTLD_NEXT, "rename");
    int result = next(from, to);
    const char *claim = getenv("R9_CUT_CLAIM");
    if (result == 0 && claim && strcmp(to, claim) == 0) {
        unsetenv("R9_CUT_CLAIM");
        if (next(getenv("R9_CUT_SOCKET"), getenv("R9_CUT_PARKED")) != 0)
            abort();
    }
    return result;
}
