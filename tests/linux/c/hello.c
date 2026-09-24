// Il programma più piccolo che usa la libc: printf, avvio di musl, exit.
#include <stdio.h>

int main(int argc, char **argv) {
    printf("hello %d %s\n", argc, argv[0][0] ? "argv0" : "vuoto");
    return 3;
}
