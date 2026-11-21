#!/bin/sh
# Parallel-boundary stress: tiny chunks x 8 threads, and 1 thread, must equal the reference on small seeds.
# Usage (Git Bash, repo root): sh tools/stress.sh
set -u
R="$(cygpath -w "$PWD")\target\release"
for s in 2 3 4 5 6; do
  for cfg in "SS_CHUNK=4096 NCPU=8" "SS_CHUNK=777 NCPU=3" "NCPU=1"; do
    MSYS_NO_PATHCONV=1 docker run --rm ${SS_CPUSET:+--cpuset-cpus=$SS_CPUSET} -v "D:\buildathon-data\diff:/w" -v "$R:/b:ro" debian:bookworm-slim \
      sh -c "rm -rf /w/st$s; env $cfg /b/sessions /w/s$s /w/st$s"
    printf "seed $s [$cfg]: "; ./.venv/Scripts/python scripts/compare.py /d/buildathon-data/diff/st$s /d/buildathon-data/diff/ref$s | tail -1
  done
done
