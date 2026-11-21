# Performance log

Local numbers: Docker, `--cpuset-cpus=0-7` (4 P-cores with hyperthreading, the scorer's shape), `NCPU=8`,
warm page cache, output to an in-container tmpfs, wall time from the `written` lap (process work only;
container start/exit is about 0.47 s on top). Medians of interleaved rounds against the previous candidate.
Board scores vary about 20% across scoring machines, so one board number cannot confirm a 5-15% change.

| Version | Commit | 20M local | 60M local | Board | Notes |
|---|---|---|---|---|---|
| v4 | dcbd6cf | ~2.0 s | - | 3.20 s | first fast Rust version |
| v5.3 | 804ab57 | 0.89 s | - | 1.73 s | packed keys, bucketed dedup, AVX2 scan, person partitions |
| v5.4 | 1ff0c20 | 0.74 s | - | 1.79 s | adaptive input I/O (board noise) |
| v5.5 | 44f244e | 0.66 s | - | 1.62 s | prefetched link table, dedup prepass, purchases-only product lookup |
| v5.7 | 26b0e6b | 0.60 s | 1.63 s | 1.39 s | Parquet writer, SWAR timestamps, links-first scheduling |
| v5.8 | 848af15 | 0.52 s | - | 1.37 s | radix person stage, largest-first person scheduling |
| main | 3860b34 | - | - | - | + per-worker date memo (row body -5% in microbench) |

Rejected experiments (patches in `experiments/`): 16-byte dedup records, parallel binary-search link build,
parallel small-table load with small link chunks.
