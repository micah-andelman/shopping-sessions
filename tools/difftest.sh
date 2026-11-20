#!/bin/sh
# Differential test: generate datasets with new seeds, run reference and Rust, compare.
# Usage: sh tools/difftest.sh FIRST_SEED LAST_SEED EVENTS
set -eu
D=/d/buildathon-data/diff; mkdir -p $D
G="$(cygpath -w "$PWD/kit/generator/target/release")"
for s in $(seq $1 $2); do
  n=$3; c=$((n/4))
  rm -rf $D/s$s $D/ref$s $D/rs$s
  MSYS_NO_PATHCONV=1 docker run --rm -v "D:\buildathon-data\diff:/w" -v "$G:/g" -w /w rust:1.85 /g/generate --seed $s --events $n --clicks $c --output s$s >/dev/null
  ./.venv/Scripts/python ref/reference.py $D/s$s $D/ref$s
  MSYS_NO_PATHCONV=1 docker run --rm -v "D:\buildathon-data\diff:/w" -v "$(cygpath -w "$PWD"):/p" -w /w rust:1.85 /p/target/release/sessions s$s rs$s 2>/dev/null
  printf "seed %s: " $s; ./.venv/Scripts/python scripts/compare.py $D/rs$s $D/ref$s | tail -1
done
