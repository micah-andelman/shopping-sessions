# Shopping Sessions: team "5-hour limit reached"

A Rust pipeline for the Databricks Shopping Sessions track (`src/`). The Dockerfile builds one static binary into a `FROM scratch` image. The DuckDB starter notes below are kept for reference.

## How it works

- `src/fast/e2.rs` (engine 2) runs first. It is built for IDs written the way the published generator writes them: a prefix, then base 36 of a scrambled counter. It recovers the scramble keys from a sample at startup and decodes event IDs to dense indices (direct-indexed dedup) and user IDs to (person, alias), so identity is a per-person alias union with a general union-find only for links that leave a person. Correctness never depends on the keys, because decoding is a bijection; IDs outside the dense ranges take exact byte-keyed paths. When the data doesn't fit the layout, it writes nothing and the path below runs instead.
- `src/fast.rs` is the main path when engine 2 declines:
  - Every ID becomes an exact 64-bit key: up to 10 bytes of `[0-9A-Z_a-z]` packed 6 bits per byte, so integer order equals byte order. Other IDs are hashed, and a global check confirms no two different strings share a hash.
  - The CSVs are memory-mapped and read ahead by background threads in parse order.
  - Event copies are scattered by `event_id` into 256 buckets and deduped per bucket.
  - Survivors and clicks are scattered into 256 person partitions, each sorted in cache.
  - Each person's sessions, credit and assists come from one pass with a sliding 7-day click window.
- `src/scan.rs` is a one-pass AVX2 CSV scanner. It builds comma, newline and quote bitmasks per 64 bytes and checks the dialect in the same pass. It is fuzz-tested against the scalar splitter.
- `src/main.rs` keeps the earlier byte-exact pipeline (v4). It runs whenever the fast path's assumptions do not hold: window too long, too many categories, or a hash collision. `SS_LEGACY=1` forces it for differential tests.
- `ref/reference.py` is an independent, slow Python implementation of `rules.md`. `tools/verify.sh` checks the 36 fixtures, the dev answer, 20M events against v4, and 10 generator seeds against the reference.

## Credits

All code here was written by our team during the event. Ideas we learned from other teams' public write-ups and then implemented our own way:
- Order-preserving packed integer ID keys: Wolverines, axhiao, Th3r0n.
- Background read-ahead for cold-cache input: Eagle Eye.
- Decoding the generator's scrambled IDs to counters (key recovery from a sample, direct-indexed dedup, person arithmetic for identity): Team 67's public design, read for ideas only; the code in `src/fast/e2.rs` is ours.
- Bitmask-based SIMD CSV scanning, hash-partitioned dedup, and person partitioning instead of a global sort: several teams, including enkyuan/polaris and Wolverines.

Libraries: memmap2, memchr, rayon, hashbrown, foldhash, mimalloc, arrow-array, arrow-schema, parquet (all MIT or Apache-2.0).

## Original DuckDB starter notes

This repository is a working skeleton, not a complete solution. It builds a `linux/amd64` image with DuckDB, runs inside the scorer's container limits, reads the inputs, and writes both output tables with the right schema. Both tables are empty, so the checker marks this skeleton incorrect until you implement the rules in `rules.md`.

If this starter and `rules.md` ever disagree, `rules.md` wins. Ask the organizers if `rules.md` leaves you unsure.

## What to do first

- Read `rules.md` for the full rules.
- Write your pipeline in `pipeline/pipeline.py`. The TODO lists the steps in order.
- Build the image and test against all 36 edge-case fixtures and the 1M-event dev set before submitting.

## Run and test locally

You need Docker with buildx, and Python 3.10+ with `polars` for the test scripts. Install it with `pip install polars`.

Build the image and run all 36 fixtures:

```sh
make build        # docker buildx build --platform linux/amd64 --load -t shopping-sessions:latest .
make fixtures     # runs the image on all 36 fixtures with scorer flags, diffs each against expected/
```

Build, run, and check the 1M-event dev set. `make dev` needs `generate` on your PATH: build it from the track page's generator.zip (Rust 1.85 or newer; its README has the two commands).

```sh
make dev                                # generate --seed 1 --events 1000000 --clicks 250000 --output dev
make run-dev                            # runs your image on dev/ with scorer flags and times it
make check-dev                          # exact diff against dev-answer/, the track page's dev-answer.zip unzipped here
```

If your laptop cannot spare 8 cores and 24 GB, run with lower limits using `make run-dev CPUS=4 MEM=8g`. Your local times will not match the scorer anyway; only the board's times count.

- **x86 Linux and Windows machines.** Build and run the amd64 image natively.
- **Apple-silicon Macs.** Emulating amd64 is slow. For fast local loops, build a native image with `make build run-dev PLATFORM=linux/arm64`. The Dockerfile pins no platform, so it builds either way. Check the amd64 image before you submit.

To iterate without Docker:

```sh
pip install duckdb==1.5.6
python3 scripts/run_fixtures.py fixtures -- python3 pipeline/pipeline.py
```

## Submitting and scoring

Submit your public GitHub repository on the Buildathon track page. Enter a commit SHA to score a specific revision, or leave it empty to score the newest commit on the default branch.

The image's `CMD` runs with no arguments. Exit code 0 means "done, check my output". Build time is never timed; only the run is timed. The track page lists the current practice limits and how the final is scored.

## Container limits

Scorers are Google Cloud n2 Intel Xeon machines, Cascade Lake or newer, with AVX-512 available. Your container gets:

| Resource | Setting |
| --- | --- |
| `/data` | The dataset, read-only |
| `/output` | Empty, writable. Write `/output/user_daily/` and `/output/campaign_daily/` as Parquet |
| `/scratch` | Empty, writable, fast local disk for spills and temp files |
| CPU / RAM | 8 vCPUs, hyperthreads of an Intel Xeon, 24 GB, no swap |
| Network | None |
| Env vars | `DATA_DIR=/data`, `OUTPUT_DIR=/output`, `SCRATCH_DIR=/scratch`, `HOME=/scratch`, `NCPU=8`, `MEM_GB=24` |
| Filesystem | Root is read-only apart from `/tmp`, 64 MB in memory. The process runs as uid 1000, not root |

## Common pitfalls

- **DuckDB's CSV reader differs from `rules.md`.** It reads a quoted empty field `""` as an empty string, not null. Its malformed-line handling, including `ignore_errors`, `strict_mode`, and `null_padding`, does not match `rules.md`. Test against the fixtures before relying on it.
- **Memory.** `pipeline.py` sets `memory_limit` to 80% of `MEM_GB` and `temp_directory` under `/scratch`, so big joins and sorts spill to disk instead of getting the container killed.
- **No extension downloads.** The container has no network at run time. Parquet and CSV are built into the wheel; anything else must be installed at build time.
- **`\d` is not `[0-9]`.** In several regex engines, including Rust's used by Polars, `\d` matches any Unicode digit. The rules mean ASCII digits only.
- **Extra files in output folders.** `_SUCCESS` and other non-Parquet files in the output folders are ignored.

## Self-checks

- Summed over all rows, `sessions` and `revenue_cents` are equal in both tables.
- `day` must be a Parquet `DATE`, not a string or timestamp.
- Row order and the number of Parquet files do not matter. Exact column names do, and there must be no extra columns.

## Files in this starter

- `pipeline/pipeline.py` holds your pipeline code.
- `Dockerfile` builds a `linux/amd64` image for the Intel Xeon scorers.
- `fixtures/` holds the 36 edge cases, each an `input/` folder and the `expected/` tables. In this monorepo they live in `../../fixtures`; copy them in when publishing the starter as its own repo.
- `scripts/` runs your image the way the scorer does and diffs the output.
