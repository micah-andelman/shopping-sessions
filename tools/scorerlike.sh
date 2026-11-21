#!/bin/sh
# "Scorer-like" benchmark arm: runs release binaries on the 20M set under conditions closer to
# the scoring VM than a plain local run, and times each run from OUTSIDE the process (fork to
# reaped exit, so exit teardown counts), with user/sys CPU, faults and peak RSS (tools/scorerlike/runstat.c).
#
# Usage (Git Bash, repo root):
#   sh tools/scorerlike.sh [options] BIN[:ENV=V[,ENV=V]] [BIN...]
# BIN is a path to a Linux release binary (e.g. target/release/sessions); several binaries are
# run interleaved for ROUNDS rounds and medians are printed per binary.
#
# The two standard arms (they bracket the board's in-process time for main 8fd1206, 0.99 s and 1.35 s
# against 0.61 s for a plain 8-core local run), compare builds in BOTH:
#   sh tools/scorerlike.sh -r 7 -c 16-19 -H        OLD NEW    # 8 threads on 4 cores, THP off
#   sh tools/scorerlike.sh -r 7 -c 16-19 -B 1      OLD NEW    # 8 threads on 4 cores, 1 hog thread on 20-23
#
# Options (all optional, defaults in brackets):
#   -r ROUNDS     interleaved rounds per binary [5]
#   -c CPUSET     docker --cpuset-cpus [16-23]. "16-19" with -t 8 puts 8 threads on 4 cores: a crude
#                 stand-in for the scorer's 4 physical cores x 2 hyperthreads.
#   -t THREADS    NCPU for the binary [8]
#   -m LIMIT      docker --memory (cgroup limit), e.g. 8g, 16g, 24g [24g]
#   -T            copy the inputs to a tmpfs inside the container first (the scorer's inputs are
#                 on tmpfs); skipped with a warning if the VM has < 8 GB available
#   -H            THP off for the measured process (prctl PR_SET_THP_DISABLE via runstat --nothp)
#   -B N          memory-bandwidth hog: N threads copying 256 MB arrays, pinned to HOGCPUS
#   -b HOGCPUS    cpus for the hog [20-23] (use with -c 16-19 so the hog is on neighbouring cores)
#   -d DATA       dataset path inside the shopdata volume [/d/s20m]
#   -n            also time a no-op run (runstat of /bin/true) to show the harness floor
#
# What this can and cannot reproduce on this laptop: cores 16-23 are E-cores WITHOUT
# hyperthreading, so real SMT sibling contention is not reproducible there; -c 16-19 -t 8 only
# time-slices two threads per core. The VM-wide THP mode (always/madvise/never) is not changed
# by this script; -H gives "never" for the measured process only. Memory cgroup charging is on
# for every docker container already; -m only sets the limit.
#
# Needs: Docker volume `shopdata` with the dataset, target/scorerlike/{runstat,hog} (built by
# this script on first use in the rust:1.85 image), and waits while C:\Users\micah\code\TIMING.lock exists.
set -u
ROUNDS=5; CPUSET=16-23; THREADS=8; MEM=24g; TMPFS=0; NOTHP=0; HOG=0; HOGCPUS=20-23; DATA=/d/s20m; NOOP=0
while getopts "r:c:t:m:THB:b:d:n" o; do case $o in
  r) ROUNDS=$OPTARG;; c) CPUSET=$OPTARG;; t) THREADS=$OPTARG;; m) MEM=$OPTARG;; T) TMPFS=1;; H) NOTHP=1;;
  B) HOG=$OPTARG;; b) HOGCPUS=$OPTARG;; d) DATA=$OPTARG;; n) NOOP=1;; *) exit 2;; esac; done
shift $((OPTIND - 1))
[ $# -ge 1 ] || { sed -n 2,30p "$0"; exit 2; }
LOCK=/c/Users/micah/code/TIMING.lock
waitlock() { while [ -e "$LOCK" ] && [ "${SS_LOCK_MINE:-0}" != 1 ]; do sleep 15; done; }
P="$(cygpath -w "$PWD")"
ALLCPUS=$CPUSET; [ "$HOG" != 0 ] && ALLCPUS="$CPUSET,$HOGCPUS"
if [ ! -x target/scorerlike/runstat ] || [ ! -x target/scorerlike/hog ]; then
  waitlock
  MSYS_NO_PATHCONV=1 docker run --rm --cpuset-cpus=$CPUSET -v "$P:/w" -w /w rust:1.85 sh -c 'mkdir -p target/scorerlike && gcc -O2 -static -o target/scorerlike/runstat tools/scorerlike/runstat.c && gcc -O2 -static -pthread -o target/scorerlike/hog tools/scorerlike/hog.c' || exit 1
fi
# Stage the binaries (a Windows bind mount is slow to exec from; they are copied to /tmp inside).
ST=target/scorerlike/stage; rm -rf $ST; mkdir -p $ST
i=0; LABELS=""
for spec in "$@"; do i=$((i + 1)); bin=${spec%%:*}; cp "$bin" $ST/bin$i || exit 1; LABELS="$LABELS $spec"; done
cp target/scorerlike/runstat target/scorerlike/hog $ST/
# One container for the whole measurement: staging, optional tmpfs copy and hog, then the rounds.
INNER='
set -u
mkdir -p /tmp/s && cp /st/* /tmp/s/ && chmod +x /tmp/s/*
IN='"$DATA"'
if [ '"$TMPFS"' = 1 ]; then
  avail=$(awk "/MemAvailable/{print int(\$2/1048576)}" /proc/meminfo)
  if [ "$avail" -ge 8 ]; then mkdir -p /in/data && cp -r '"$DATA"'/. /in/data/ && IN=/in/data && echo "INFO inputs on tmpfs ($avail GB was available)"
  else echo "INFO tmpfs skipped: only $avail GB available"; fi
fi
cat $(find $IN -type f) > /dev/null   # warm the page cache
if [ '"$HOG"' != 0 ]; then taskset -c '"$HOGCPUS"' /tmp/s/hog '"$HOG"' 256 & HP=$!; sleep 1; echo "INFO hog: '"$HOG"' threads on cpus '"$HOGCPUS"'"; fi
NT=""; [ '"$NOTHP"' = 1 ] && NT=--nothp
echo "INFO thp=$(cat /sys/kernel/mm/transparent_hugepage/enabled) defrag=$(cat /sys/kernel/mm/transparent_hugepage/defrag) nothp_for_process='"$NOTHP"' cpus='"$CPUSET"' threads='"$THREADS"' mem='"$MEM"'"
if [ '"$NOOP"' = 1 ]; then for k in 1 2 3; do taskset -c '"$CPUSET"' /tmp/s/runstat --label noop -- /bin/true; done; fi
for r in $(seq '"$ROUNDS"'); do
  i=0
  for spec in '"$LABELS"'; do i=$((i + 1))
    envs=""; case $spec in *:*) envs=$(echo "${spec#*:}" | tr "," " ");; esac
    rm -rf /out/o; mkdir -p /out
    env NCPU='"$THREADS"' $envs taskset -c '"$CPUSET"' /tmp/s/runstat $NT --label $i -- /tmp/s/bin$i $IN /out/o 2>/dev/null
  done
done
[ '"$HOG"' != 0 ] && kill $HP
true'
waitlock
TM=""; [ "$TMPFS" = 1 ] && TM="--tmpfs /in:size=2400m"
OUT=$(MSYS_NO_PATHCONV=1 docker run --rm --cpuset-cpus=$ALLCPUS --memory=$MEM $TM --tmpfs /out:size=1g -v "$P/$ST:/st:ro" -v shopdata:/d:ro debian:bookworm-slim sh -c "$INNER" 2>&1)
echo "$OUT" | grep -E "^INFO|noop|rror|annot"
i=0
for spec in "$@"; do i=$((i + 1))
  echo "$OUT" | awk -v i=$i -v c="$spec" '$1=="RUNSTAT" && $2==i {n++; w[n]=$4; u[n]=$6; s[n]=$8; f[n]=$10; m[n]=$14; rc+=$16}
    END{ if(n==0){print c": no runs"; exit}
      # insertion sort (portable awk)
      for(a=2;a<=n;a++){for(b=a;b>1&&w[b-1]>w[b];b--){t=w[b];w[b]=w[b-1];w[b-1]=t}}
      for(a=2;a<=n;a++){for(b=a;b>1&&u[b-1]>u[b];b--){t=u[b];u[b]=u[b-1];u[b-1]=t}}
      for(a=2;a<=n;a++){for(b=a;b>1&&s[b-1]>s[b];b--){t=s[b];s[b]=s[b-1];s[b-1]=t}}
      for(a=2;a<=n;a++){for(b=a;b>1&&f[b-1]>f[b];b--){t=f[b];f[b]=f[b-1];f[b-1]=t}}
      for(a=2;a<=n;a++){for(b=a;b>1&&m[b-1]>m[b];b--){t=m[b];m[b]=m[b-1];m[b-1]=t}}
      k=int((n+1)/2)
      printf "%-40s wall med %.3f min %.3f | user %.2f sys %.2f | minflt %d | peak RSS %d MB | rc %d (n=%d)\n", c, w[k], w[1], u[k], s[k], f[k], m[k], rc, n }'
done
