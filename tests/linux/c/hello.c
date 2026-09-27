// The smallest program that uses the libc: printf, musl startup, exit.
#include <stdio.h>

int main(int argc, char **argv) {
    printf("hello %d %s\n", argc, argv[0][0] ? "argv0" : "vuoto");
    return 3;
}
