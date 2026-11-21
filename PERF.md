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
| v5.8 (re-timed) | 848af15 | 0.525 s | 1.793 s | 1.37 s | same binary, bundle window 23:35 |
| v5.9 | HEAD | 0.495 s | 1.700 s | - | + date memo (c45), PEXT pack (agent B), two-stage scanner scan2 (agent A); 60M = golden |

Rejected experiments (patches in `experiments/`): 16-byte dedup records, parallel binary-search link build,
parallel small-table load with small link chunks.
| v5.9 2nd draw | 4685e4a | - | - | 1.30 s | same commit as the 1.58 s draw: board noise ~20% |
| A | 8fd1206 | 0.495 s | 1.544 s | 1.29 s | + link bit filter, exit inside run(), clicks parsed early |
| v5.10 | HEAD | 0.519 s (cpu 3.63) | 1.751 s (cpu 12.25) vs 1.951 (13.96) same window | - | + early unmap (auditor), scan3 mask quote validation + prefetch (agent A); 60M = golden |

Headline arm from 2:00 AM on: 8 threads on 4 physical cores (cores 0-7), warm, plus total CPU-seconds.
Windows differ in load, so compare only rows measured in the same window.

