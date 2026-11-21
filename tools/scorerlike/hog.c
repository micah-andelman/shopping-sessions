// hog THREADS MB : memory-bandwidth hog. Each thread copies between two MB-sized arrays forever
// (streaming reads and writes, far larger than any cache). Pin it with taskset. Kill to stop.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static size_t bytes;
static void *work(void *x) {
    (void)x;
    char *a = malloc(bytes), *b = malloc(bytes);
    memset(a, 1, bytes);
    memset(b, 2, bytes);
    for (;;) {
        memcpy(a, b, bytes);
        memcpy(b, a, bytes);
    }
    return 0;
}
int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 4;
    bytes = (size_t)(argc > 2 ? atoi(argv[2]) : 256) << 20;
    pthread_t t;
    for (int i = 0; i < n; i++) pthread_create(&t, 0, work, 0);
    pause();
    return 0;
}
