// Regressioni del kernel emulato trovate in revisione (M2/M3): ogni caso
// stampa il proprio esito; Vetro e QEMU devono stampare lo stesso
// (tests/linux/tests/regress.rs).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif

static void res(const char *what, long r) {
    if (r < 0)
        printf("%s: -1 %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

// /proc/self/fd/N si segue solo dove Linux segue l'ultimo componente.
static void proc_fd_links(void) {
    int fd = open("f", O_CREAT | O_RDWR, 0644);
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    struct stat st;
    res("lstat link", lstat(p, &st) == 0 ? (long)S_ISLNK(st.st_mode) : -1);
    res("stat link", stat(p, &st) == 0 ? (long)S_ISREG(st.st_mode) : -1);
    res("open O_NOFOLLOW", open(p, O_RDONLY | O_NOFOLLOW));
    res("unlink link", unlink(p));
    res("file ancora presente", access("f", F_OK));
    res("rename link", rename(p, "g"));
    res("file non spostato", access("f", F_OK));
    int fd2 = open(p, O_RDONLY);
    res("open segue il link", fd2 >= 0 ? 0 : -1);
    close(fd2);
    close(fd);
    unlink("f");
}

// Tempi enormi (tv_sec vicino a INT64_MAX) non devono traboccare.
static void huge_timeouts(void) {
    struct timespec huge = {INT64_MAX, 0};
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGALRM);
    sigprocmask(SIG_BLOCK, &set, NULL);
    alarm(1);
    res("sigtimedwait con timeout enorme", syscall(SYS_rt_sigtimedwait, &set, NULL, &huge, 8));
    struct itimerval it = {{0, 0}, {INT64_MAX / 2, 0}};
    res("setitimer enorme", setitimer(ITIMER_REAL, &it, NULL));
    struct itimerval zero = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &zero, NULL);
    struct timespec rq = {0, 1000}, rem;
    res("nanosleep breve", nanosleep(&rq, &rem));
}

// mremap: MREMAP_FIXED fuori dallo spazio d'indirizzamento, DONTUNMAP.
static void mremap_cases(void) {
    long pg = 4096;
    char *p = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 7;
    // Solo se fallisce: l'errno dipende dalla versione di QEMU (8.x ENOMEM,
    // 10 EINVAL come Linux); il difetto era che riusciva, su un indirizzo
    // avvolto.
    void *r = mremap(p, pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_FIXED, (void *)0xfffffffffffff000UL);
    printf("mremap FIXED oltre la fine fallisce: %d\n", r == MAP_FAILED);
    r = mremap(p, pg, 2 * pg, MREMAP_DONTUNMAP);
    res("mremap DONTUNMAP senza MAYMOVE", r == MAP_FAILED ? -1 : 0);
    r = mremap(p, pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    res("mremap DONTUNMAP con dimensioni diverse", r == MAP_FAILED ? -1 : 0);
    char *q = mremap(p, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    if (q == MAP_FAILED) {
        res("mremap DONTUNMAP", -1);
    } else {
        printf("mremap DONTUNMAP: spostata %d, nuovo %d, vecchio %d\n", q != p, q[0], p[0]);
    }
}

// Lock su una pipe: si rilasciano alla chiusura come sui file.
static void pipe_locks(void) {
    int fds[2];
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    for (int i = 0; i < 20; i++) {
        pipe(fds);
        fcntl(fds[1], F_SETLK, &fl);
        close(fds[0]);
        close(fds[1]);
    }
    pipe(fds);
    pid_t c = fork();
    if (c == 0) {
        _exit(fcntl(fds[1], F_SETLK, &fl) == 0 ? 0 : 1);
    }
    int st;
    waitpid(c, &st, 0);
    res("lock su una pipe nuova nel figlio", WEXITSTATUS(st));
    close(fds[0]);
    close(fds[1]);
}

static void on_alarm(int s) { (void)s; }

// F_SETLKW interrotta (EINTR) non deve lasciare un'attesa fantasma che fa
// dare EDEADLK a un altro processo.
static void setlkw_eintr(void) {
    int fd = open("lk", O_CREAT | O_RDWR, 0644);
    ftruncate(fd, 16);
    int to_child[2], to_parent[2];
    pipe(to_child);
    pipe(to_parent);
    struct flock a = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
    struct flock b = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 1, .l_len = 1};
    fcntl(fd, F_SETLK, &a);
    pid_t c = fork();
    char x;
    if (c == 0) {
        fcntl(fd, F_SETLK, &b);
        write(to_parent[1], "r", 1);
        read(to_child[0], &x, 1);
        // Il genitore non aspetta più: F_SETLKW deve bloccarsi, non EDEADLK.
        int r = fcntl(fd, F_SETLKW, &a);
        _exit(r == 0 ? 0 : errno);
    }
    read(to_parent[0], &x, 1);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm; // senza SA_RESTART
    sigaction(SIGALRM, &sa, NULL);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGALRM);
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    alarm(1);
    res("F_SETLKW interrotta", fcntl(fd, F_SETLKW, &b));
    write(to_child[1], "g", 1);
    struct timespec d = {0, 200000000};
    nanosleep(&d, NULL);
    struct flock u = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    fcntl(fd, F_SETLK, &u);
    int st;
    waitpid(c, &st, 0);
    res("F_SETLKW del figlio (0 = ottenuto)", WEXITSTATUS(st));
    close(fd);
    unlink("lk");
}

// RLIMIT_NOFILE a 0: nessun nuovo descrittore.
static void nofile_zero(void) {
    pid_t c = fork();
    if (c == 0) {
        struct rlimit r = {0, 0};
        setrlimit(RLIMIT_NOFILE, &r);
        int fd = open("/dev/null", O_RDONLY);
        _exit(fd < 0 && errno == EMFILE ? 0 : 1);
    }
    int st;
    waitpid(c, &st, 0);
    res("open con RLIMIT_NOFILE = 0 (0 = EMFILE)", WEXITSTATUS(st));
}

// /proc/self/pagemap: pagine toccate e non toccate, offset enorme.
static void pagemap(void) {
    int fd = open("/proc/self/pagemap", O_RDONLY);
    char *p = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 1;
    uint64_t e[2];
    pread(fd, e, sizeof e, (uint64_t)(uintptr_t)p / 4096 * 8);
    printf("pagemap: toccata %d, non toccata %d\n", (int)(e[0] >> 63), (int)(e[1] >> 63));
    uint64_t x;
    lseek(fd, (off_t)1 << 62, SEEK_SET);
    res("pagemap oltre la fine", read(fd, &x, 8));
    close(fd);
}

// pwritev su un file con una MAP_SHARED: la mappatura vede i dati;
// preadv su un descrittore O_PATH: EBADF.
static void vectored_io(void) {
    int fd = open("m", O_CREAT | O_RDWR | O_TRUNC, 0644);
    ftruncate(fd, 4096);
    char *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    char data[] = "vetro";
    struct iovec iov = {data, 5};
    res("pwritev", pwritev(fd, &iov, 1, 0));
    printf("la mappatura vede pwritev: %.5s\n", m);
    int pfd = open("m", O_PATH);
    char buf[8];
    struct iovec riov = {buf, 8};
    res("preadv su O_PATH", preadv(pfd, &riov, 1, 0));
    close(pfd);
    munmap(m, 4096);
    close(fd);
    unlink("m");
}

// fstat di un O_PATH|O_NOFOLLOW su un link simbolico: il link stesso.
static void opath_symlink(void) {
    symlink("bersaglio", "ln");
    int fd = open("ln", O_PATH | O_NOFOLLOW);
    struct stat st;
    res("fstat O_PATH|O_NOFOLLOW è un link", fstat(fd, &st) == 0 ? (long)S_ISLNK(st.st_mode) : -1);
    close(fd);
    unlink("ln");
}

// F_SETOWN: INT_MIN non è un gruppo valido.
static void setown(void) {
    int fds[2];
    pipe(fds);
    res("F_SETOWN INT_MIN", fcntl(fds[0], F_SETOWN, INT32_MIN));
    res("F_SETOWN getpid", fcntl(fds[0], F_SETOWN, getpid()));
    close(fds[0]);
    close(fds[1]);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    proc_fd_links();
    huge_timeouts();
    mremap_cases();
    pipe_locks();
    setlkw_eintr();
    nofile_zero();
    pagemap();
    vectored_io();
    opath_symlink();
    setown();
    return 0;
}
