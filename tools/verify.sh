#!/bin/sh
# Full correctness gate for the current target/release build (Git Bash, Docker).
# 36 fixtures, exact dev answer, 20M vs the v4 output in D:\buildathon-data\cmp\v4, seeds 2-11 vs reference.
set -u
P="$(cygpath -w "$PWD")"; R="$P\target\release"
sh tools/fixtures.sh | tail -1
MSYS_NO_PATHCONV=1 docker run --rm ${SS_CPUSET:+--cpuset-cpus=$SS_CPUSET} -e NCPU=8 -v "$P:/w" -v "D:\buildathon-data\cmp:/c" -v shopdata:/d debian:bookworm-slim \
  sh -c 'rm -rf /w/out-rs /c/v5; /w/target/release/sessions /w/dev /w/out-rs; /w/target/release/sessions /d/s20m /c/v5'
printf "dev: "; ./.venv/Scripts/python scripts/compare.py out-rs dev-answer | tail -1
printf "20M vs v4: "; ./.venv/Scripts/python scripts/compare.py /d/buildathon-data/cmp/v5 /d/buildathon-data/cmp/v4 | tail -1
for s in 2 3 4 5 6 7 8 9 10 11; do
  MSYS_NO_PATHCONV=1 docker run --rm ${SS_CPUSET:+--cpuset-cpus=$SS_CPUSET} -v "D:\buildathon-data\diff:/w" -v "$R:/b:ro" debian:bookworm-slim sh -c "rm -rf /w/rs$s; /b/sessions /w/s$s /w/rs$s"
  printf "seed $s: "; ./.venv/Scripts/python scripts/compare.py /d/buildathon-data/diff/rs$s /d/buildathon-data/diff/ref$s | tail -1
done
