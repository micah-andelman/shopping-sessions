#!/bin/sh
# Final rehearsal: run an image exactly as the final does (60M events, scorer container limits,
# 3-minute cap), on fresh seeds, cold cache once and warm once, and check the output against the
# byte-exact legacy path (same image, SS_LEGACY=1) with tools/canon_hash.py.
# Usage (Git Bash, repo root): sh tools/rehearse.sh IMAGE "SEED SEED ..." [EVENTS]
# Data goes to the Docker volume `shopdata` (/d/r<seed>); outputs to D:\buildathon-data\rehearse.
set -u
IMG=$1; SEEDS=$2; EV=${3:-60000000}; CK=$((EV / 4))
CPUS=${SS_CPUSET:-0-7}
G="$(cygpath -w "$PWD/kit/generator/target/release")"
mkdir -p /d/buildathon-data/rehearse
O='D:\buildathon-data\rehearse'
run() { # $1 = label, $2 = seed, $3 = extra env
  s=$(date +%s%N)
  MSYS_NO_PATHCONV=1 timeout 200 docker run --rm --cpuset-cpus=$CPUS --network none --read-only \
    --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL --security-opt no-new-privileges \
    --memory 24g --memory-swap 24g --pids-limit 4096 --user 1000:1000 \
    -v shopdata:/d:ro -v "$O:/r" -e DATA_DIR=/d/r$2 -e OUTPUT_DIR=/r/$1-$2 -e SCRATCH_DIR=/tmp \
    -e HOME=/tmp -e NCPU=8 -e MEM_GB=24 $3 "$IMG"
  rc=$?; e=$(date +%s%N); ms=$(( (e - s) / 1000000 ))
  echo "  $1 seed $2: exit $rc, ${ms} ms$( [ $ms -gt 180000 ] && echo '  OVER 3-MIN CAP')"
}
for sd in $SEEDS; do
  echo "seed $sd"
  MSYS_NO_PATHCONV=1 docker run --rm -v "$G:/g:ro" -v shopdata:/d debian:bookworm-slim \
    sh -c "[ -f /d/r$sd/manifest.json ] || /g/generate --seed $sd --events $EV --clicks $CK --output /d/r$sd >/dev/null; cat /d/r$sd/manifest.json | tr -d '\n '"; echo
  MSYS_NO_PATHCONV=1 docker run --rm --privileged alpine sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'
  run cold $sd ""
  run warm $sd ""
  run legacy $sd "-e SS_LEGACY=1"
  for k in cold warm legacy; do printf "  %-6s " $k; ./.venv/Scripts/python tools/canon_hash.py /d/buildathon-data/rehearse/$k-$sd | tr '\n' ' '; echo; done
  # Team rule: release the VM's file cache after every 60M run (keeps Windows from running out of RAM).
  MSYS_NO_PATHCONV=1 docker run --rm --privileged alpine sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches; echo 1 > /proc/sys/vm/compact_memory'
done
