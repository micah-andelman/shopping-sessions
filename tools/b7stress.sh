#!/bin/sh
# b7: claude-28's stress variants of the current target/release build on dev and seeds 2/5/8/11 vs reference.
# Usage (Git Bash, repo root): sh tools/b7stress.sh
set -u
P="$(cygpath -w "$PWD")"; R="$P\target\release"
O=/d/buildathon-data/b7sweep/st; W='D:\buildathon-data\b7sweep\st'; mkdir -p $O
VARS="SS_E2BADKEY=1 NCPU=1 NCPU=3 NCPU=24,SS_CHUNK=4096 SS_REVFILES=1,SS_CHUNK=65536 SS_E2ACAP=40"
MSYS_NO_PATHCONV=1 docker run --rm --cpuset-cpus=16-23 -v "$P\\dev:/in/dev:ro" -v "D:\buildathon-data\diff:/df:ro" -v "$R:/b:ro" -v "$W:/o" debian:bookworm-slim sh -c "
  rm -rf /o/*
  for v in $VARS; do e=\$(echo \$v | tr , ' ')
    for d in dev 2 5 8 11; do src=/df/s\$d; [ \$d = dev ] && src=/in/dev
      env NCPU=8 \$e /b/sessions \$src /o/\$v-\$d 2>/o/\$v-\$d.err || echo \"CRASH \$v \$d\"
    done
  done"
for v in $VARS; do for d in dev 2 5 8 11; do
  ref=/d/buildathon-data/diff/ref$d; [ $d = dev ] && ref=dev-answer
  printf "%-28s %-4s " "$v" "$d"; ./.venv/Scripts/python scripts/compare.py $O/$v-$d $ref 2>&1 | tail -1
done; done
