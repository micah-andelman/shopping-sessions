#!/bin/sh
# Overnight soak: random generator seeds (plus every known leap-day seed first), fast path vs the
# legacy path always, and vs the Python reference when the set is small enough.
# Usage (Git Bash, repo root): sh tools/soak.sh [HOURS]   (log: D:\buildathon-data\soak\soak.log)
# Runs a frozen copy of target/release/sessions, so rebuilding during the soak does not change it.
set -u
HOURS=${1:-8}; END=$(( $(date +%s) + HOURS * 3600 ))
CPUS=${SS_CPUSET:-0-7}
S=/d/buildathon-data/soak; W='D:\buildathon-data\soak'; mkdir -p $S/bin
cp target/release/sessions $S/bin/sessions
G="$(cygpath -w "$PWD/kit/generator/target/release")"
LOG=$S/soak.log
echo "soak start $(date) binary $(git rev-parse --short HEAD) $(sha256sum $S/bin/sessions | cut -c1-12)" >> $LOG
LEAP="15 62 249 254 272 355 537 633 800 857 1160 1222 1277 1330 1372 1481"
i=0
while [ $(date +%s) -lt $END ]; do
  set -- $LEAP
  if [ $i -lt $# ]; then shift $i; sd=$1; else sd=$(( (RANDOM << 15 | RANDOM) % 1000000 + 2000 )); fi
  case $(( RANDOM % 4 )) in 0) ev=200000;; 1) ev=1000000;; 2) ev=2000000;; 3) ev=5000000;; esac
  i=$((i + 1))
  MSYS_NO_PATHCONV=1 docker run --rm --cpuset-cpus=$CPUS -v "$G:/g:ro" -v "$W:/w" -v "$W\\bin:/b:ro" debian:bookworm-slim sh -c "
    rm -rf /w/in /w/fast /w/leg /w/ref
    /g/generate --seed $sd --events $ev --clicks $((ev / 4)) --output /w/in >/dev/null &&
    NCPU=8 /b/sessions /w/in /w/fast && NCPU=8 SS_LEGACY=1 /b/sessions /w/in /w/leg" 2>>$LOG
  a=$(./.venv/Scripts/python scripts/compare.py $S/fast $S/leg 2>&1 | tail -1)
  b="(ref skipped)"
  if [ $ev -le 2000000 ]; then
    ./.venv/Scripts/python ref/reference.py $S/in $S/ref >/dev/null 2>&1
    b=$(./.venv/Scripts/python scripts/compare.py $S/fast $S/ref 2>&1 | tail -1)
  fi
  st=OK; [ "$a" = MATCH ] || st=MISMATCH; [ "$b" = MATCH ] || [ "$b" = "(ref skipped)" ] || st=MISMATCH
  echo "$(date +%H:%M:%S) seed $sd events $ev: legacy=$a ref=$b $st" >> $LOG
  if [ $st = MISMATCH ]; then cp -r $S/in $S/fail-$sd; fi
done
echo "soak end $(date)" >> $LOG
