/* probe-hello.c -- step 1: does Wine run a trivial mingw-built exe at all? */
#include <stdio.h>

int main(void)
{
    printf("hello from mingw exe\n");
    fflush(stdout);
    fprintf(stderr, "hello on stderr\n");
    fflush(stderr);
    return 0;
}
