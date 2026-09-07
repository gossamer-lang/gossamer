//! `gos bench [PATH] [--parallel N]` - discovers every
//! `#[bench]`-annotated function under `PATH` and times each one,
//! reporting `ns/op` per benchmark.
//!
//! Mirrors the discovery flow used by `gos test` (see
//! [`crate::cmd::attr_walk`] and [`crate::cmd::test`]): `PATH` may
//! name a single `.gos` source file or a directory to walk. When
//! omitted, falls back to `<project-root>/src/`.
//!
//! Each bench fn runs through the bytecode-VM-backed interpreter.
//! The harness auto-tunes the per-bench iteration count by doubling
//! `N` until the cumulative wall-clock exceeds a small calibration
//! window, then reports the mean cost across the calibrated batch
//! ([`auto_tune_iterations`]).
//!
//! Output format:
//! ```text
//! benchmark::add_two_ints          ... 23 ns/op (tier-ups 1, compiles 1,
//!                                        compile 42 us, native-code 768 B,
//!                                        peak-rss 123456 B, vm-bypassed 2048,
//!                                        allocs 8, alloc-bytes 256 B,
//!                                        arc +5/-5, boundary-copies 2 (64 B))
//! benchmark::create_thousand_keys  ... 1240 ns/op (...)
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::cmd::attr_walk::{collect_selected_fn_names, item_has_attr};
use crate::loaders::load_and_check_with_sf;
use crate::paths::{collect_lint_targets, default_test_root, read_source};

/// Options threaded into [`run_with_opts`].
pub(crate) struct BenchOpts {
    /// Source file or directory to walk. `None` falls back to
    /// `<project-root>/src/`.
    pub path: Option<PathBuf>,
    /// Number of files to bench in parallel. The default (1) keeps
    /// per-bench timings deterministic since two CPU-bound benches
    /// on the same core perturb each other's measurements.
    pub parallel: usize,
}

/// Calibration window: keep doubling the iteration count until a
/// timing trial exceeds this many nanoseconds. 50ms is the same
/// window Rust's `#[bench]` harness historically used; large enough
/// to swamp short-lived per-call jitter, small enough to keep
/// total bench time bounded.
const CALIBRATION_NANOS: u128 = 50_000_000;

/// Hard ceiling on per-bench iteration count. Prevents a no-op
/// bench (`fn bench_empty(b: &mut Bencher) { }`) from looping
/// forever on a fast machine where every trial completes below
/// the calibration window.
const MAX_ITERATIONS: u64 = 1 << 20;

/// One discovered bench fn and its source file.
#[derive(Clone)]
struct BenchTarget {
    file: PathBuf,
    name: String,
}

/// One bench fn's measured result.
#[derive(Debug)]
struct BenchRecord {
    /// Display label (`<file-stem>::<fn-name>`).
    label: String,
    /// Mean nanoseconds per call.
    ns_per_op: u128,
    /// Total iterations the harness settled on after calibration.
    iterations: u64,
    /// Deferred-JIT activity observed while calibrating and timing this target.
    jit_metrics: gossamer_interp::JitMetrics,
    /// Runtime allocation, ARC, and VM/JIT boundary-copy work for this target.
    runtime_metrics: gossamer_runtime::c_abi::ledger::BenchmarkCounters,
}

/// Entry point for `gos bench`. Walks `opts.path`, runs every
/// discovered bench fn, and prints one summary line per fn.
pub(crate) fn run_with_opts(opts: BenchOpts) -> Result<()> {
    let resolved = match opts.path.as_ref() {
        Some(p) => p.clone(),
        None => default_test_root()?,
    };
    let files = collect_lint_targets(&resolved)?;
    if files.is_empty() {
        return Err(anyhow!(
            "no `.gos` sources found under {}",
            resolved.display()
        ));
    }
    let mut discovered: Vec<BenchTarget> = Vec::new();
    for file in &files {
        let names = collect_bench_names(file)?;
        for name in names {
            discovered.push(BenchTarget {
                file: file.clone(),
                name,
            });
        }
    }
    if discovered.is_empty() {
        println!(
            "bench: no #[bench] functions found under {}",
            resolved.display()
        );
        return Ok(());
    }

    let parallel = opts.parallel.max(1);
    let records = if parallel > 1 && discovered.len() > 1 {
        run_parallel(discovered, parallel)
    } else {
        discovered
            .into_iter()
            .map(|t| run_one(&t))
            .collect::<Result<Vec<_>>>()?
    };

    let label_width = records.iter().map(|r| r.label.len()).max().unwrap_or(0);
    let ns_width = records
        .iter()
        .map(|r| digits(r.ns_per_op))
        .max()
        .unwrap_or(1);
    for record in &records {
        println!(
            "{label:<lw$} ... {ns:>nw$} ns/op (tier-ups {tier_ups}, compiles {compiles}, \
             compile {compile_us} us, native-code {native_code} B, peak-rss {peak_rss} B, \
             vm-bypassed {vm_bypassed}, allocs {allocations}, alloc-bytes {allocation_bytes} B, \
             arc +{arc_retains}/-{arc_releases}, boundary-copies {boundary_copies} \
             ({boundary_copy_bytes} B))",
            label = record.label,
            lw = label_width,
            ns = record.ns_per_op,
            nw = ns_width,
            tier_ups = record.jit_metrics.tier_up_requests,
            compiles = record.jit_metrics.compile_attempts,
            compile_us = record.jit_metrics.total_compile_time_us,
            native_code = record.jit_metrics.emitted_code_bytes,
            peak_rss = record.jit_metrics.peak_observed_rss_bytes,
            vm_bypassed = record.jit_metrics.saved_vm_instructions,
            allocations = record.runtime_metrics.allocations,
            allocation_bytes = record.runtime_metrics.allocation_bytes,
            arc_retains = record.runtime_metrics.arc_retains,
            arc_releases = record.runtime_metrics.arc_releases,
            boundary_copies = record.runtime_metrics.boundary_copies,
            boundary_copy_bytes = record.runtime_metrics.boundary_copy_bytes,
        );
    }
    eprintln!(
        "bench: {} benchmark(s) across {} file(s)",
        records.len(),
        files.len(),
    );
    let _ = records.iter().map(|r| r.iterations).sum::<u64>();
    Ok(())
}

/// Parallel variant of the per-target run loop. Records are sorted
/// back into discovery order so the final report is deterministic.
fn run_parallel(targets: Vec<BenchTarget>, parallel: usize) -> Vec<BenchRecord> {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    let queue: Arc<StdMutex<Vec<(usize, BenchTarget)>>> = Arc::new(StdMutex::new(
        targets.into_iter().enumerate().rev().collect(),
    ));
    let results: Arc<StdMutex<Vec<(usize, BenchRecord)>>> = Arc::new(StdMutex::new(Vec::new()));
    let n_workers = parallel.min(queue.lock().expect("queue lock").len()).max(1);
    let mut handles = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let queue = Arc::clone(&queue);
        let results = Arc::clone(&results);
        handles.push(std::thread::spawn(move || {
            loop {
                let next = {
                    let mut q = queue.lock().expect("queue lock");
                    q.pop()
                };
                let Some((idx, target)) = next else {
                    return;
                };
                if let Ok(rec) = run_one(&target) {
                    results.lock().expect("results lock").push((idx, rec));
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut collected = Arc::try_unwrap(results)
        .expect("results arc unwrap")
        .into_inner()
        .expect("results lock");
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, rec)| rec).collect()
}

/// Calibrates and runs one bench fn end-to-end. The VM is
/// re-created per-target so cross-bench JIT state cannot perturb
/// the timing of a later bench.
fn run_one(target: &BenchTarget) -> Result<BenchRecord> {
    // Execute on a thread with a large native stack so a deeply
    // recursive `#[bench]` does not overflow the host's default
    // main-thread stack (see `cmd::with_vm_stack`).
    let target = target.clone();
    crate::cmd::with_vm_stack(move || run_one_inner(&target))
}

fn run_one_inner(target: &BenchTarget) -> Result<BenchRecord> {
    let source = read_source(&target.file)?;
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(target.file.to_string_lossy().into_owned(), source.clone());
    let (program, _sf, tcx) = load_and_check_with_sf(&source, file_id, &map)?;
    let mut vm = gossamer_interp::Vm::new();
    // A benchmark run enters the one function it times, so that is the body
    // the promotion snapshot is built around.
    vm.set_entry_points(std::slice::from_ref(&target.name));
    vm.load(&program, tcx, true)
        .map_err(|e| anyhow!("bench {} load failed: {e}", target.name))?;

    gossamer_runtime::c_abi::ledger::begin_benchmark_counters();
    let iterations = auto_tune_iterations(&vm, &target.name)?;
    let started = Instant::now();
    for _ in 0..iterations {
        vm.call(&target.name, Vec::new())
            .map_err(|e| anyhow!("bench {} failed: {e}", target.name))?;
    }
    let elapsed = started.elapsed();
    let runtime_metrics = gossamer_runtime::c_abi::ledger::finish_benchmark_counters();

    let total_nanos = elapsed.as_nanos();
    let ns_per_op = if iterations == 0 {
        0
    } else {
        total_nanos / u128::from(iterations)
    };
    let label = format!(
        "{}::{}",
        target
            .file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("benchmark"),
        target.name,
    );
    Ok(BenchRecord {
        label,
        ns_per_op,
        iterations,
        jit_metrics: vm.jit_metrics(),
        runtime_metrics,
    })
}

/// Doubles the iteration count starting at 1 until a single trial
/// exceeds [`CALIBRATION_NANOS`] or the count hits
/// [`MAX_ITERATIONS`]. Returns the count to use for the final
/// timed batch.
fn auto_tune_iterations(vm: &gossamer_interp::Vm, name: &str) -> Result<u64> {
    let mut n: u64 = 1;
    loop {
        let started = Instant::now();
        for _ in 0..n {
            vm.call(name, Vec::new())
                .map_err(|e| anyhow!("bench {name} failed during calibration: {e}"))?;
        }
        let elapsed = started.elapsed();
        if elapsed.as_nanos() >= CALIBRATION_NANOS {
            return Ok(n);
        }
        if n >= MAX_ITERATIONS {
            return Ok(MAX_ITERATIONS);
        }
        // The calibration window doubles N until the bench fn's
        // wall-clock per trial overtakes CALIBRATION_NANOS. A
        // no-op fn never crosses the threshold within
        // MAX_ITERATIONS - that path falls through to the cap.
        n = n.saturating_mul(2).min(MAX_ITERATIONS);
        if elapsed > Duration::from_secs(2) {
            // Safety hatch - one trial took longer than the
            // entire bench budget. Stop here rather than doubling
            // again.
            return Ok(n / 2);
        }
    }
}

/// Decimal width of `n`, minimum 1 (so `0` renders as 1 column).
fn digits(n: u128) -> usize {
    if n == 0 {
        return 1;
    }
    let mut d = 0;
    let mut v = n;
    while v > 0 {
        d += 1;
        v /= 10;
    }
    d
}

/// Loads `file`, runs frontend checks, returns every `#[bench]`-
/// annotated fn name in source order. Mirrors `gos test`'s
/// `collect_test_names`.
fn collect_bench_names(file: &Path) -> Result<Vec<String>> {
    let source = read_source(file)?;
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
    let (_program, sf, _tcx) = load_and_check_with_sf(&source, file_id, &map)?;
    let mut names = Vec::new();
    collect_selected_fn_names(&sf.items, &|item| item_has_attr(item, "bench"), &mut names);
    Ok(names)
}
