/*
 * GalOSv2 (Starry OS) Performance Benchmark Suite
 *
 * Comprehensive kernel benchmark: syscall latency, memory ops,
 * process creation, file I/O, IPC, scheduling.
 *
 * Build: <arch>-linux-musl-gcc -static -O2 -o bench bench.c
 * Run:   ./bench [test_name ...]   (no args = run all)
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* ── Timing helpers ─────────────────────────────────────────────── */

static inline uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}

typedef struct {
    uint64_t min_ns;
    uint64_t max_ns;
    uint64_t total_ns;
    int iters;
} bench_result_t;

static void result_init(bench_result_t *r)
{
    r->min_ns = UINT64_MAX;
    r->max_ns = 0;
    r->total_ns = 0;
    r->iters = 0;
}

static void result_record(bench_result_t *r, uint64_t elapsed_ns)
{
    if (elapsed_ns < r->min_ns) r->min_ns = elapsed_ns;
    if (elapsed_ns > r->max_ns) r->max_ns = elapsed_ns;
    r->total_ns += elapsed_ns;
    r->iters++;
}

static void print_latency(const char *category, const char *name,
                           bench_result_t *r)
{
    if (r->iters == 0) {
        printf("[%-7s] %-22s: SKIPPED\n", category, name);
        return;
    }
    uint64_t avg = r->total_ns / r->iters;
    const char *unit;
    double scale;
    if (avg >= 1000000) { unit = "ms"; scale = 1e6; }
    else if (avg >= 1000) { unit = "us"; scale = 1e3; }
    else { unit = "ns"; scale = 1.0; }

    printf("[%-7s] %-22s: %5d iters, min=%8.1f%s, avg=%8.1f%s, max=%8.1f%s\n",
           category, name, r->iters,
           r->min_ns / scale, unit, avg / scale, unit, r->max_ns / scale, unit);
}

static void print_throughput(const char *category, const char *name,
                             uint64_t bytes, uint64_t elapsed_ns)
{
    double mb = (double)bytes / (1024.0 * 1024.0);
    double sec = (double)elapsed_ns / 1e9;
    printf("[%-7s] %-22s: %.1f MB in %.3fs = %.1f MB/s\n",
           category, name, mb, sec, sec > 0 ? mb / sec : 0);
}

static void warmup(void)
{
    for (int i = 0; i < 100; i++) { now_ns(); getpid(); }
}

/* ═══ SYSCALL LATENCY ═══════════════════════════════════════════ */

#define SYSCALL_ITERS 500

static void bench_getpid(void)
{
    bench_result_t r; result_init(&r);
    for (int i = 0; i < SYSCALL_ITERS; i++) {
        uint64_t t0 = now_ns(); getpid(); uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    print_latency("syscall", "getpid", &r);
}

static void bench_clock_gettime_lat(void)
{
    bench_result_t r; result_init(&r);
    struct timespec ts;
    for (int i = 0; i < SYSCALL_ITERS; i++) {
        uint64_t t0 = now_ns();
        clock_gettime(CLOCK_MONOTONIC, &ts);
        uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    print_latency("syscall", "clock_gettime", &r);
}

static void bench_write_devnull(void)
{
    bench_result_t r; result_init(&r);
    int fd = open("/dev/null", O_WRONLY);
    if (fd < 0) { printf("[syscall] write_devnull          : SKIPPED\n"); return; }
    char buf[1] = {'x'};
    for (int i = 0; i < SYSCALL_ITERS; i++) {
        uint64_t t0 = now_ns(); write(fd, buf, 1); uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    close(fd);
    print_latency("syscall", "write_devnull", &r);
}

/* ═══ MEMORY ════════════════════════════════════════════════════ */

#define MEM_ITERS 200
#define PAGE_SIZE 4096

static void bench_mmap_munmap(void)
{
    bench_result_t r; result_init(&r);
    for (int i = 0; i < MEM_ITERS; i++) {
        uint64_t t0 = now_ns();
        void *p = mmap(NULL, PAGE_SIZE, PROT_READ|PROT_WRITE,
                       MAP_ANONYMOUS|MAP_PRIVATE, -1, 0);
        uint64_t t1 = now_ns();
        if (p != MAP_FAILED) munmap(p, PAGE_SIZE);
        result_record(&r, t1 - t0);
    }
    print_latency("memory", "mmap+munmap", &r);
}

static void bench_page_fault(void)
{
    bench_result_t r; result_init(&r);
    for (int i = 0; i < MEM_ITERS; i++) {
        void *p = mmap(NULL, PAGE_SIZE, PROT_READ|PROT_WRITE,
                       MAP_ANONYMOUS|MAP_PRIVATE, -1, 0);
        if (p == MAP_FAILED) continue;
        uint64_t t0 = now_ns();
        *(volatile char *)p = 42;
        uint64_t t1 = now_ns();
        munmap(p, PAGE_SIZE);
        result_record(&r, t1 - t0);
    }
    print_latency("memory", "page_fault", &r);
}

static void bench_mprotect(void)
{
    bench_result_t r; result_init(&r);
    void *p = mmap(NULL, PAGE_SIZE, PROT_READ|PROT_WRITE,
                   MAP_ANONYMOUS|MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED) { printf("[memory ] mprotect               : SKIPPED\n"); return; }
    for (int i = 0; i < MEM_ITERS; i++) {
        uint64_t t0 = now_ns();
        mprotect(p, PAGE_SIZE, (i & 1) ? PROT_READ : (PROT_READ|PROT_WRITE));
        uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    munmap(p, PAGE_SIZE);
    print_latency("memory", "mprotect", &r);
}

/* ═══ PROCESS ═══════════════════════════════════════════════════ */

#define PROC_ITERS 20

static void bench_fork_exit_wait(void)
{
    bench_result_t r; result_init(&r);
    for (int i = 0; i < PROC_ITERS; i++) {
        uint64_t t0 = now_ns();
        pid_t pid = fork();
        if (pid == 0) _exit(0);
        else if (pid > 0) {
            waitpid(pid, NULL, 0);
            uint64_t t1 = now_ns();
            result_record(&r, t1 - t0);
        }
    }
    print_latency("process", "fork+exit+wait", &r);
}

static void bench_exec(void)
{
    bench_result_t r; result_init(&r);
    const char *prog = "/bin/true";
    if (access(prog, X_OK) != 0) {
        printf("[process] execve                 : SKIPPED (no /bin/true)\n");
        return;
    }
    for (int i = 0; i < PROC_ITERS; i++) {
        uint64_t t0 = now_ns();
        pid_t pid = fork();
        if (pid == 0) {
            char *argv[] = {(char *)prog, NULL};
            char *envp[] = {NULL};
            execve(prog, argv, envp);
            _exit(127);
        } else if (pid > 0) {
            waitpid(pid, NULL, 0);
            uint64_t t1 = now_ns();
            result_record(&r, t1 - t0);
        }
    }
    print_latency("process", "execve", &r);
}

/* ═══ FILE I/O ══════════════════════════════════════════════════ */

#define FIO_ITERS    200
#define FIO_BUF_SIZE 4096
#define FIO_TOTAL    (2 * 1024 * 1024)

static void bench_open_close(void)
{
    bench_result_t r; result_init(&r);
    for (int i = 0; i < FIO_ITERS; i++) {
        uint64_t t0 = now_ns();
        int fd = open("/dev/null", O_RDONLY);
        uint64_t t1 = now_ns();
        if (fd >= 0) close(fd);
        result_record(&r, t1 - t0);
    }
    print_latency("file", "open+close", &r);
}

static void bench_stat_lat(void)
{
    bench_result_t r; result_init(&r);
    struct stat st;
    for (int i = 0; i < FIO_ITERS; i++) {
        uint64_t t0 = now_ns(); stat("/dev/null", &st); uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    print_latency("file", "stat", &r);
}

static void bench_read_throughput(void)
{
    const char *path = "/tmp/bench_read_tmp";
    char buf[FIO_BUF_SIZE];
    memset(buf, 'A', sizeof(buf));
    int fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, 0644);
    if (fd < 0) { printf("[file   ] read_4k                : SKIPPED\n"); return; }
    for (size_t w = 0; w < FIO_TOTAL; ) {
        ssize_t n = write(fd, buf, sizeof(buf));
        if (n <= 0) break;
            w += n;
    }
    close(fd);

    fd = open(path, O_RDONLY);
    if (fd < 0) { unlink(path); return; }
    uint64_t t0 = now_ns();
    size_t total = 0;
    while (1) { ssize_t n = read(fd, buf, sizeof(buf)); if (n <= 0) break;
            total += n; }
    uint64_t t1 = now_ns();
    close(fd); unlink(path);
    print_throughput("file", "read_4k", total, t1 - t0);
}

static void bench_write_throughput(void)
{
    const char *path = "/tmp/bench_write_tmp";
    char buf[FIO_BUF_SIZE];
    memset(buf, 'B', sizeof(buf));
    int fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, 0644);
    if (fd < 0) { printf("[file   ] write_4k               : SKIPPED\n"); return; }
    uint64_t t0 = now_ns();
    size_t total = 0;
    while (total < FIO_TOTAL) {
        ssize_t n = write(fd, buf, sizeof(buf));
        if (n <= 0) break;
            total += n;
    }
    fsync(fd);
    uint64_t t1 = now_ns();
    close(fd); unlink(path);
    print_throughput("file", "write_4k", total, t1 - t0);
}

/* ═══ IPC ═══════════════════════════════════════════════════════ */

#define PIPE_PING_ITERS 200
#define PIPE_BUF_SIZE   (64 * 1024)
#define PIPE_TOTAL      (2 * 1024 * 1024)

static void bench_pipe_pingpong(void)
{
    bench_result_t r; result_init(&r);
    int p2c[2], c2p[2];
    if (pipe(p2c) < 0 || pipe(c2p) < 0) {
        printf("[ipc    ] pipe_pingpong          : SKIPPED\n"); return;
    }
    pid_t pid = fork();
    if (pid == 0) {
        close(p2c[1]); close(c2p[0]);
        char buf[1];
        while (read(p2c[0], buf, 1) == 1) write(c2p[1], buf, 1);
        close(p2c[0]); close(c2p[1]); _exit(0);
    }
    close(p2c[0]); close(c2p[1]);
    char buf[1] = {'p'};
    for (int i = 0; i < PIPE_PING_ITERS; i++) {
        uint64_t t0 = now_ns();
        write(p2c[1], buf, 1); read(c2p[0], buf, 1);
        uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    close(p2c[1]); close(c2p[0]);
    waitpid(pid, NULL, 0);
    print_latency("ipc", "pipe_pingpong", &r);
}

static void bench_pipe_throughput(void)
{
    int fds[2];
    if (pipe(fds) < 0) { printf("[ipc    ] pipe_throughput         : SKIPPED\n"); return; }
    pid_t pid = fork();
    if (pid == 0) {
        close(fds[1]);
        char buf[PIPE_BUF_SIZE];
        while (read(fds[0], buf, sizeof(buf)) > 0);
        close(fds[0]); _exit(0);
    }
    close(fds[0]);
    char buf[PIPE_BUF_SIZE]; memset(buf, 'X', sizeof(buf));
    uint64_t t0 = now_ns();
    size_t total = 0;
    while (total < PIPE_TOTAL) {
        ssize_t n = write(fds[1], buf, sizeof(buf));
        if (n <= 0) break;
            total += n;
    }
    close(fds[1]);
    uint64_t t1 = now_ns();
    waitpid(pid, NULL, 0);
    print_throughput("ipc", "pipe_throughput", total, t1 - t0);
}

static volatile sig_atomic_t sig_received;
static void sig_handler(int sig) { (void)sig; sig_received = 1; }

#define SIG_ITERS 200

static void bench_signal_delivery(void)
{
    bench_result_t r; result_init(&r);
    struct sigaction sa;
    sa.sa_handler = sig_handler;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;
    sigaction(SIGUSR1, &sa, NULL);
    pid_t self = getpid();
    for (int i = 0; i < SIG_ITERS; i++) {
        sig_received = 0;
        uint64_t t0 = now_ns(); kill(self, SIGUSR1); uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    print_latency("ipc", "signal_delivery", &r);
}

/* ═══ SCHEDULING ════════════════════════════════════════════════ */

#define SCHED_ITERS 300

static void bench_sched_yield(void)
{
    bench_result_t r; result_init(&r);
    for (int i = 0; i < SCHED_ITERS; i++) {
        uint64_t t0 = now_ns(); sched_yield(); uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    print_latency("sched", "sched_yield", &r);
}

static void bench_nanosleep_zero(void)
{
    bench_result_t r; result_init(&r);
    struct timespec ts = {0, 0};
    for (int i = 0; i < SCHED_ITERS; i++) {
        uint64_t t0 = now_ns(); nanosleep(&ts, NULL); uint64_t t1 = now_ns();
        result_record(&r, t1 - t0);
    }
    print_latency("sched", "nanosleep(0)", &r);
}

#define CTX_ITERS 200

static void bench_context_switch(void)
{
    bench_result_t r; result_init(&r);
    int p1[2], p2[2];
    if (pipe(p1) < 0 || pipe(p2) < 0) {
        printf("[sched  ] context_switch          : SKIPPED\n"); return;
    }
    pid_t pid = fork();
    if (pid == 0) {
        close(p1[1]); close(p2[0]);
        char buf[1];
        for (int i = 0; i < CTX_ITERS; i++) { read(p1[0], buf, 1); write(p2[1], buf, 1); }
        close(p1[0]); close(p2[1]); _exit(0);
    }
    close(p1[0]); close(p2[1]);
    char buf[1] = {'c'};
    for (int i = 0; i < CTX_ITERS; i++) {
        uint64_t t0 = now_ns();
        write(p1[1], buf, 1); read(p2[0], buf, 1);
        uint64_t t1 = now_ns();
        result_record(&r, (t1 - t0) / 2);
    }
    close(p1[1]); close(p2[0]);
    waitpid(pid, NULL, 0);
    print_latency("sched", "context_switch", &r);
}

/* ═══ REGISTRY ══════════════════════════════════════════════════ */

typedef struct { const char *name; void (*func)(void); } bench_entry_t;

static const bench_entry_t benchmarks[] = {
    {"getpid",           bench_getpid},
    {"clock_gettime",    bench_clock_gettime_lat},
    {"write_devnull",    bench_write_devnull},
    {"mmap_munmap",      bench_mmap_munmap},
    {"page_fault",       bench_page_fault},
    {"mprotect",         bench_mprotect},
    {"fork_exit_wait",   bench_fork_exit_wait},
    {"execve",           bench_exec},
    {"open_close",       bench_open_close},
    {"stat",             bench_stat_lat},
    {"read_throughput",  bench_read_throughput},
    {"write_throughput", bench_write_throughput},
    {"pipe_pingpong",    bench_pipe_pingpong},
    {"pipe_throughput",  bench_pipe_throughput},
    {"signal_delivery",  bench_signal_delivery},
    {"sched_yield",      bench_sched_yield},
    {"nanosleep_zero",   bench_nanosleep_zero},
    {"context_switch",   bench_context_switch},
    {NULL, NULL},
};

int main(int argc, char *argv[])
{
    printf("\n=======================================================\n");
    printf("  GalOS Benchmark Suite\n");
    printf("=======================================================\n\n");
    warmup();

    if (argc > 1) {
        for (int a = 1; a < argc; a++) {
            int found = 0;
            for (const bench_entry_t *b = benchmarks; b->name; b++) {
                if (strcmp(argv[a], b->name) == 0) { b->func(); found = 1; break; }
            }
            if (!found) {
                printf("Unknown benchmark: %s\nAvailable:", argv[a]);
                for (const bench_entry_t *b = benchmarks; b->name; b++)
                    printf(" %s", b->name);
                printf("\n");
                return 1;
            }
        }
    } else {
        for (const bench_entry_t *b = benchmarks; b->name; b++)
            b->func();
    }

    printf("\n=======================================================\n");
    printf("  Done\n");
    printf("=======================================================\n");
    return 0;
}
