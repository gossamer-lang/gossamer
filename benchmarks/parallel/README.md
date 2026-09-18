# Parallel adapter scaling

Two benchmarks, each beside a sequential twin that differs only in the
adapters it calls: `mandelbrot.gos` maps rows of escape counts with
`par_map`, and `stats.gos` reduces two million computed values with
`par_sum`, `par_min`, `par_max`, and a custom `par_reduce`.

`run.gos` builds all four with `gos build --release`, runs each parallel
benchmark with its pool pinned to 1, 2, 4, 8, and all cores through
`GOSSAMER_MAX_PROCS`, runs each sequential twin at one worker, and reports the
median of repeated runs. It exits non-zero when either claim fails on the
machine it runs on:

- at 8 workers a parallel benchmark is at least 6x faster than at 1 worker
  (checked only on a machine with at least 8 cores);
- at 1 worker the parallel spelling is within 5% of its sequential twin.

```bash
cargo build --release --bin gos
./target/release/gos run benchmarks/parallel/run.gos --runs 5
```

`--runs N` sets how many runs each median is taken over, and `--gos PATH`
names the toolchain to measure. Both benchmarks compute more per element than
they move, which is the shape the adapters speed up: a walk whose cost is
dominated by writing its results is bound by memory bandwidth, and more cores
do not add any.
