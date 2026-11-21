// runstat [--nothp] [--label NAME] -- cmd args...
// Runs cmd as a child and prints one line: wall seconds measured from outside the process
// (fork to reaped exit, so address-space teardown is included), user/sys CPU seconds,
// minor/major faults, peak RSS and the exit code. --nothp disables transparent huge pages
// for the child (prctl PR_SET_THP_DISABLE is inherited across exec).
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/wait.h>
int main(int argc, char **argv) {
    int i = 1, nothp = 0;
    const char *label = "";
    for (; i < argc; i++) {
        if (!strcmp(argv[i], "--nothp")) nothp = 1;
        else if (!strcmp(argv[i], "--label") && i + 1 < argc) label = argv[++i];
        else if (!strcmp(argv[i], "--")) { i++; break; }
        else break;
    }
    if (i >= argc) { fprintf(stderr, "usage: runstat [--nothp] [--label L] -- cmd...\n"); return 2; }
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    pid_t p = fork();
    if (p == 0) {
        if (nothp && prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) != 0) perror("prctl");
        execv(argv[i], argv + i);
        perror("execv");
        _exit(127);
    }
    int st = 0;
    struct rusage r;
    wait4(p, &st, 0, &r);
    clock_gettime(CLOCK_MONOTONIC, &b);
    double wall = (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) * 1e-9;
    printf("RUNSTAT %s wall %.4f user %.3f sys %.3f minflt %ld majflt %ld maxrss_mb %ld rc %d\n", label, wall,
           r.ru_utime.tv_sec + r.ru_utime.tv_usec * 1e-6, r.ru_stime.tv_sec + r.ru_stime.tv_usec * 1e-6,
           r.ru_minflt, r.ru_majflt, r.ru_maxrss / 1024, WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    return 0;
}
