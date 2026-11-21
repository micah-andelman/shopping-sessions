#!/bin/sh
# Run the release binary on every fixture inside one Linux container, then diff each on the host.
# Usage (from repo root, Git Bash): sh tools/fixtures.sh
set -eu
rm -rf out-fx && mkdir -p out-fx
MSYS_NO_PATHCONV=1 docker run --rm ${SS_CPUSET:+--cpuset-cpus=$SS_CPUSET} -v "$(cygpath -w "$PWD"):/w" -w /w rust:1.85 sh -c '
  for d in fixtures/*/; do c=$(basename $d); [ -d $d/input ] || continue
    target/release/sessions $d/input out-fx/$c 2>/dev/null || echo "CRASH $c"; done'
pass=0; fail=0
for d in fixtures/*/; do c=$(basename $d); [ -d $d/expected ] || continue
  if ./.venv/Scripts/python scripts/compare.py out-fx/$c $d/expected >/tmp/cmp.txt 2>&1; then pass=$((pass+1)); else fail=$((fail+1)); echo "FAIL $c"; tail -8 /tmp/cmp.txt; fi
done
echo "$pass passed, $fail failed"
