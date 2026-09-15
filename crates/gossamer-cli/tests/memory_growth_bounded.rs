//! Catches the `gos_rt_heap_*_free` regression (C2 in
//! `~/dev/contexts/lang/adversarial_analysis.md`).

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

#[test]
fn compiled_vec_alloc_and_drop_stays_under_rss_cap() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-mem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("mem.gos");
    std::fs::write(
        &source,
        "
fn pump() {
    let buf = U8Vec::new(8388608)
    let mut i = 0
    while i < 1024 {
        buf.set_byte(i, ((i * 7) % 256) as i64)
        i = i + 1
    }
}

fn main() {
    let mut k = 0
    while k < 32 {
        pump()
        k = k + 1
    }
}
",
    )
    .unwrap();

    for release in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }
        cmd.arg(&source);
        let build = cmd.output().expect("spawn gos build");
        assert!(
            build.status.success(),
            "build failed (release={release}): {}",
            String::from_utf8_lossy(&build.stderr)
        );

        let profile = if release { "release" } else { "debug" };
        let bin = dir
            .join("target")
            .join(profile)
            .join(format!("mem{}", std::env::consts::EXE_SUFFIX));
        assert!(bin.exists(), "missing {}", bin.display());

        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(&bin)
            .output()
            .expect("spawn /usr/bin/time");
        assert!(
            out.status.success(),
            "binary failed (release={release}): stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let kb = parse_max_rss_kb(&stderr)
            .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"));
        let cap_kb = 96 * 1024;
        assert!(
            kb < cap_kb,
            "RSS {kb} KiB exceeded {cap_kb} KiB cap (release={release}); heap_*_free regression"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Gates the compiled-tier reference-counting release of recursive enum
/// values. A loop builds and discards a depth-14 binary tree on every
/// iteration; each per-iteration temporary must be released. Before RC
/// landed these `gos_rc_alloc`'d (formerly `malloc`'d) nodes leaked
/// unboundedly - 200 iterations would accumulate well over 100 MiB and
/// keep growing with depth. With deterministic RC release the peak stays
/// near a single tree's footprint. Runs under the full `-O3` release
/// pipeline, where the old tracing GC was unsound.
#[test]
fn compiled_recursive_enum_loop_stays_under_rss_cap() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-rcmem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("rcmem.gos");
    std::fs::write(
        &source,
        "
enum Tree { Leaf, Node(i64, Tree, Tree) }

fn build(d: i64) -> Tree {
    if d == 0 {
        Tree::Leaf
    } else {
        Tree::Node(d, Box::new(build(d - 1)), Box::new(build(d - 1)))
    }
}

fn checksum(t: Tree) -> i64 {
    match t {
        Tree::Leaf => 1,
        Tree::Node(v, l, r) => *v + checksum(l) + checksum(r),
    }
}

fn main() {
    let mut total = 0
    let mut i = 0
    while i < 200 {
        total += checksum(build(14))
        i += 1
    }
    println(\"total = {}\", total)
}
",
    )
    .unwrap();

    let mut cmd = Command::new(gos_bin());
    cmd.arg("build").arg("--release").arg(&source);
    let build = cmd.output().expect("spawn gos build --release");
    assert!(
        build.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("rcmem{}", std::env::consts::EXE_SUFFIX));
    assert!(bin.exists(), "missing {}", bin.display());

    let out = Command::new("/usr/bin/time")
        .arg("-v")
        .arg(&bin)
        .output()
        .expect("spawn /usr/bin/time");
    assert!(
        out.status.success(),
        "binary failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("total = 9827200"),
        "unexpected output: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let kb = parse_max_rss_kb(&stderr)
        .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"));
    let cap_kb = 64 * 1024;
    assert!(
        kb < cap_kb,
        "RSS {kb} KiB exceeded {cap_kb} KiB cap; recursive-enum RC release regression \
         (per-iteration trees are leaking)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A recursive-enum tree bound to a *named local* and rebuilt each loop
/// iteration must release the previous iteration's value before
/// reassignment. Before the fix this leaked every iteration's tree
/// (the release fired only at function return), so 200 depth-14 trees stayed
/// resident (~hundreds of MB). The earlier test only exercised the
/// *temporary* shape (`checksum(&build(14))`), which the single-use path
/// already released - this is the gap it missed.
#[test]
fn compiled_named_binding_loop_stays_under_rss_cap() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-rcnamed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("rcnamed.gos");
    std::fs::write(
        &source,
        "
enum Tree { Leaf, Node(i64, Tree, Tree) }

fn build(d: i64) -> Tree {
    if d == 0 { Tree::Leaf } else { Tree::Node(d, Box::new(build(d - 1)), Box::new(build(d - 1))) }
}

fn checksum(t: Tree) -> i64 {
    match t { Tree::Leaf => 1, Tree::Node(v, l, r) => *v + checksum(l) + checksum(r) }
}

fn main() {
    let mut total = 0
    let mut i = 0
    while i < 200 {
        let t = build(14)
        total += checksum(t)
        i += 1
    }
    println(\"total = {}\", total)
}
",
    )
    .unwrap();

    let mut cmd = Command::new(gos_bin());
    cmd.arg("build").arg("--release").arg(&source);
    let build = cmd.output().expect("spawn gos build --release");
    assert!(
        build.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("rcnamed{}", std::env::consts::EXE_SUFFIX));
    assert!(bin.exists(), "missing {}", bin.display());

    let out = Command::new("/usr/bin/time")
        .arg("-v")
        .arg(&bin)
        .output()
        .expect("spawn /usr/bin/time");
    assert!(
        out.status.success(),
        "binary failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("total = 9827200"),
        "unexpected output: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let kb = parse_max_rss_kb(&stderr)
        .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"));
    let cap_kb = 64 * 1024;
    assert!(
        kb < cap_kb,
        "RSS {kb} KiB exceeded {cap_kb} KiB cap; named-binding loop is leaking \
         (release fires only at function return, not before reassignment)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Gates the three container / accumulator leak classes fixed in the
/// drop-pass move-transfer plus return-copy-move work. A1 is an owning
/// container binding reassigned each loop iteration (`v = make()`); A2 is a
/// dynamic repeat array `[x; n]` built in a function called in a loop; A3
/// is a String accumulator built and returned by a function called in a
/// loop. Each previously retained one buffer or reference per iteration,
/// hundreds of MB over the loop; with the fixes the peak stays near a
/// single buffer's footprint. Runs under the full `-O3` release pipeline.
#[test]
fn compiled_container_and_accumulator_loops_stay_under_rss_cap() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-leakclass-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("leakclass.gos");
    std::fs::write(
        &source,
        "
fn make_vec(n: i64) -> Vec<i64> {
    let mut v: Vec<i64> = Vec::from([])
    let mut i = 0
    while i < n {
        v.push(i * 2)
        i += 1
    }
    v
}

fn repeat_sum(n: i64) -> i64 {
    let d: Vec<i64> = Vec::from([7; 64])
    d[0] + d[n - 1]
}

fn make_str(n: i64) -> String {
    let mut s = \"\"
    let mut i = 0
    while i < n {
        s += \"ab\"
        i += 1
    }
    s
}

fn main() {
    let mut v: Vec<i64> = Vec::from([])
    let mut total = 0
    let mut r = 0
    while r < 200000 {
        v = make_vec(64)
        total += repeat_sum(64)
        let s = make_str(16)
        total += s.len()
        r += 1
    }
    total += v.len()
    println(\"total = {}\", total)
}
",
    )
    .unwrap();

    let mut cmd = Command::new(gos_bin());
    cmd.arg("build").arg("--release").arg(&source);
    let build = cmd.output().expect("spawn gos build --release");
    assert!(
        build.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("leakclass{}", std::env::consts::EXE_SUFFIX));
    assert!(bin.exists(), "missing {}", bin.display());

    let out = Command::new("/usr/bin/time")
        .arg("-v")
        .arg(&bin)
        .output()
        .expect("spawn /usr/bin/time");
    assert!(
        out.status.success(),
        "binary failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("total = 9200064"),
        "unexpected output: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let kb = parse_max_rss_kb(&stderr)
        .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"));
    let cap_kb = 32 * 1024;
    assert!(
        kb < cap_kb,
        "RSS {kb} KiB exceeded {cap_kb} KiB cap; container-reassign / repeat-array / \
         string-accumulator loop is leaking one buffer or reference per iteration"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Correctness and leak gate for a by-value struct with a `Vec`/`[T]` field that
/// is moved into the struct in a returning function and back out across the call
/// boundary (`struct { data: Vec<i64>, name: String }`). The struct and its indexed
/// field must read correctly on the bytecode VM, the Cranelift JIT, and both LLVM
/// AOT modes, and each iteration's field buffer must be released when the struct
/// drops, so peak RSS stays bounded across 300k iterations.
#[test]
fn compiled_struct_vec_field_loop_runs_correctly_on_all_tiers() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-structvec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("structvec.gos");
    std::fs::write(
        &source,
        "
struct Rec { data: Vec<i64>, name: String }

fn make(n: i64) -> Rec {
    let mut v: Vec<i64> = Vec::from([])
    let mut i = 0
    while i < n {
        v.push(i * 2)
        i += 1
    }
    Rec { data: v, name: \"row\" }
}

fn main() {
    let mut total = 0
    let mut r = 0
    while r < 300000 {
        let rec = make(16)
        total += rec.data.len() + rec.name.len()
        r += 1
    }
    println(\"total = {}\", total)
}
",
    )
    .unwrap();

    // gos (bytecode VM + Cranelift JIT): output only - RSS is dominated by
    // the interpreter baseline, but a per-iteration leak would still crash /
    // OOM, and this exercises the JIT single-slot / field-free paths.
    let run = Command::new(gos_bin())
        .arg("run")
        .arg(&source)
        .output()
        .expect("spawn gos");
    assert!(
        run.status.success() && String::from_utf8_lossy(&run.stdout).contains("total = 5700000"),
        "gos failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    for release in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }
        cmd.arg(&source);
        let build = cmd.output().expect("spawn gos build");
        assert!(
            build.status.success(),
            "build failed (release={release}): {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let profile = if release { "release" } else { "debug" };
        let bin = dir
            .join("target")
            .join(profile)
            .join(format!("structvec{}", std::env::consts::EXE_SUFFIX));
        assert!(bin.exists(), "missing {}", bin.display());

        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(&bin)
            .output()
            .expect("spawn /usr/bin/time");
        assert!(
            out.status.success(),
            "binary failed (release={release}): stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("total = 5700000"),
            "unexpected output (release={release}): {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let kb = parse_max_rss_kb(&stderr)
            .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"));
        let cap_kb = 32 * 1024;
        assert!(
            kb < cap_kb,
            "RSS {kb} KiB exceeded {cap_kb} KiB cap (release={release}); struct Vec-field \
             buffer is leaking one allocation per iteration"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every closure the VM invokes from a builtin combinator (`map`, `filter`,
/// `fold`, `for_each`, `sort_by`, and the lazy `iter()` adapters) marshals its
/// arguments through the frame pool's argument free list, and every `iter()`
/// takes a slot in the lazy-iterator registry. Both are caches whose depth must
/// stay bounded no matter how many callbacks a goroutine runs.
///
/// The gate compares peak RSS at N against 4N rather than against one absolute
/// cap. A fixed cap conflates the interpreter's own startup footprint - which
/// moves with the profile, the toolchain, and the host - with the workload's
/// growth, so it can only be set loose enough to admit a real per-iteration
/// leak. Growth is the property under test: bounded reclamation makes peak RSS
/// independent of the iteration count, so the two runs land within noise of
/// each other whatever the baseline happens to be. Runs on the bytecode VM,
/// where every such body is refused by JIT admission anyway.
#[test]
fn vm_builtin_callback_loop_rss_is_independent_of_iteration_count() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-cbpool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let run_at = |iterations: i64| -> (u64, String) {
        let source = dir.join(format!("cbpool{iterations}.gos"));
        std::fs::write(
            &source,
            format!(
                "
fn work(xs: Vec<i64>) -> i64 {{
    xs.iter().filter(|x| x > 5).count()
}}

fn main() {{
    let mut xs: Vec<i64> = #[]
    let mut i = 0
    while i < 1000 {{
        xs.push(i)
        i += 1
    }}
    let mut total = 0
    let mut k = 0
    while k < {iterations} {{
        total += work(xs)
        k += 1
    }}
    println(\"total = {{}}\", total)
}}
"
            ),
        )
        .unwrap();
        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(gos_bin())
            .arg("run")
            .arg(&source)
            .env("GOS_JIT", "0")
            .output()
            .expect("spawn /usr/bin/time");
        assert!(
            out.status.success(),
            "gos run failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let kb = parse_max_rss_kb(&stderr)
            .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"));
        (kb, String::from_utf8_lossy(&out.stdout).into_owned())
    };

    let (small_kb, small_out) = run_at(12_000);
    let (large_kb, large_out) = run_at(48_000);
    assert!(
        small_out.contains("total = 11928000"),
        "unexpected output at 12000: {small_out}"
    );
    assert!(
        large_out.contains("total = 47712000"),
        "unexpected output at 48000: {large_out}"
    );

    // Four times the callbacks may cost allocator noise and a larger high-water
    // mark in the pools themselves, never a share of the work. A per-iteration
    // leak at these counts adds tens of MiB.
    let growth_kb = large_kb.saturating_sub(small_kb);
    let allowance_kb = 8 * 1024;
    assert!(
        growth_kb < allowance_kb,
        "peak RSS grew {growth_kb} KiB going from 12000 to 48000 iterations \
         ({small_kb} KiB -> {large_kb} KiB), over the {allowance_kb} KiB allowance; \
         a per-callback allocation is being retained - the frame pool's argument \
         free list or the lazy-iterator registry is growing with the call count"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn parse_max_rss_kb(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Maximum resident set size (kbytes):") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// A container built into an aggregate the callee returns must not leave a
/// reference behind. The count the constructor made, the count the aggregate
/// construction mints, and the count the return copy mints are three; the
/// frame gives up one at the return-site field free and the caller owns one,
/// so the constructor's belongs to the frame. Without that release a builder
/// called in a loop retains one buffer per call, which the allocation ledger
/// reports as a live-Vec count that tracks the iteration count.
#[test]
fn returned_aggregate_container_leaves_no_live_vec_per_call() {
    let dir = env::temp_dir().join(format!("gos-agg-share-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("agg_share.gos");
    std::fs::write(
        &source,
        "
use std::env

fn pair(i: i64) -> (i64, Vec<i64>) { (i, #[i, i + 1]) }

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut k = 0
    let mut tags = #[]
    for i in 0..n { k, tags = pair(i) }
    println(\"{} {}\", k, tags.len())
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("agg_share");

    let live = |iterations: &str| -> usize {
        let out = Command::new(&binary)
            .arg(iterations)
            .env("GOS_LEAK_LEDGER", "1")
            .output()
            .expect("run with the allocation ledger");
        let stderr = String::from_utf8_lossy(&out.stderr);
        stderr
            .split("vec=")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("ledger must report a live Vec count: {stderr}"))
    };

    let small = live("64");
    let large = live("4096");
    assert_eq!(
        small, large,
        "live Vec count tracks the call count ({small} at 64 calls, {large} at \
         4096): the reference the constructor made is reaching nothing that \
         frees it"
    );
    assert!(
        large <= 4,
        "a builder loop should end holding one buffer, not {large}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A value handed to a counted box - a generic struct wrapped in `Some` or
/// stored as a `Map` value - and a `Vec` key rebuilt for a `keys()` snapshot
/// are each owned by something that frees them. A heap field the box's layout
/// does not name, or a key vec the snapshot does not record, survives its
/// owner, so the live-Vec count tracks the iteration count.
#[test]
fn boxed_generic_fields_and_vec_key_snapshots_leave_no_live_vec_per_call() {
    let dir = env::temp_dir().join(format!("gos-box-fields-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("box_fields.gos");
    std::fs::write(
        &source,
        "
use std::env

struct Wrap<T> { value: T, tag: i64 }

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut total = 0
    for i in 0..n {
        let boxed = Some(Wrap { value: #[i, i + 1], tag: i })
        if let Some(w) = boxed { total += w.value.len() + w.tag }
        let mut held = Map::new()
        held.insert(\"k\", Wrap { value: #[i], tag: 1 })
        total += held.len()
        let mut by_path = Map::new()
        by_path.insert(#[i, 2], 3)
        total += by_path.keys().len()
    }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    for release in [false, true] {
        let mut cmd = Command::new(gos_bin());
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }
        let build = cmd
            .arg("--out-dir")
            .arg(&dir)
            .arg(&source)
            .output()
            .expect("gos build");
        assert!(
            build.status.success(),
            "build failed (release={release}): {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let binary = dir.join(format!("box_fields{}", std::env::consts::EXE_SUFFIX));
        let live = |iterations: &str| -> usize {
            let out = Command::new(&binary)
                .arg(iterations)
                .env("GOS_LEAK_LEDGER", "1")
                .output()
                .expect("run with the allocation ledger");
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr
                .split("vec=")
                .nth(1)
                .and_then(|tail| tail.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or_else(|| panic!("ledger must report a live Vec count: {stderr}"))
        };
        let small = live("64");
        let large = live("4096");
        assert_eq!(
            small, large,
            "live Vec count tracks the iteration count (release={release}): {small} at 64, \
             {large} at 4096"
        );
        assert!(
            large <= 4,
            "the loop should end holding no per-iteration buffer, not {large} (release={release})"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Gates reclamation of a capturing closure's environment on the compiled
/// tiers.
///
/// Every iteration coerces a fresh closure to a callback type, which allocates
/// an environment holding the capture. That allocation carries a
/// reference-counting header, so the ordinary drop passes own it and peak
/// memory is a property of the program rather than of how long it runs.
///
/// The bound is scale-invariance: the same program is run at two iteration
/// counts sixteen times apart, and an environment leaked per iteration grows
/// peak memory with that count. Comparing a run against itself is what keeps
/// the gate honest - a non-capturing closure allocates an environment too, so
/// it would leak alongside the capturing one and never register as a control.
#[test]
fn compiled_capturing_closure_env_does_not_grow_with_iterations() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-closure-env-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let write = |stem: &str, iterations: u64| {
        let source = dir.join(format!("{stem}.gos"));
        std::fs::write(
            &source,
            format!(
                "
fn apply(x: i64, f: Fn(i64) -> i64) -> i64 {{ f(x) }}

fn main() {{
    let mut total = 0
    for i in 0..{iterations} {{
        let k = i
        total += apply(i, |v| v + k)
    }}
    println(\"{{}}\", total)
}}
"
            ),
        )
        .unwrap();
        source
    };
    let short = write("short", 250_000);
    let long = write("long", 4_000_000);

    for release in [false, true] {
        let rss = |source: &std::path::Path, stem: &str| -> u64 {
            let mut cmd = Command::new(gos_bin());
            cmd.arg("build");
            if release {
                cmd.arg("--release");
            }
            cmd.arg(source);
            let build = cmd.output().expect("spawn gos build");
            assert!(
                build.status.success(),
                "build failed (release={release}): {}",
                String::from_utf8_lossy(&build.stderr)
            );
            let profile = if release { "release" } else { "debug" };
            let bin = dir
                .join("target")
                .join(profile)
                .join(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
            let out = Command::new("/usr/bin/time")
                .arg("-v")
                .arg(&bin)
                .output()
                .expect("spawn /usr/bin/time");
            assert!(
                out.status.success(),
                "{stem} failed (release={release}): {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            parse_max_rss_kb(&stderr)
                .unwrap_or_else(|| panic!("could not parse Maximum resident set size:\n{stderr}"))
        };

        let short_kb = rss(&short, "short");
        let long_kb = rss(&long, "long");
        // Sixteen times the iterations. A leak tracks that factor; a reclaimed
        // environment leaves the two runs within allocator noise of each
        // other, so a generous doubling still separates the two outcomes.
        let cap_kb = short_kb.max(2048) * 2;
        assert!(
            long_kb < cap_kb,
            "peak RSS grew from {short_kb} KiB at 250k iterations to {long_kb} KiB \
             at 4M (release={release}): the environment each iteration \
             allocates is not being reclaimed"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether this host has GNU `/usr/bin/time -v`, which the RSS gates read the
/// peak resident set from.
fn gnu_time_available() -> bool {
    if !std::path::Path::new("/usr/bin/time").exists() {
        eprintln!("skipping: /usr/bin/time not available on this host");
        return false;
    }
    let probe = Command::new("/usr/bin/time").arg("-v").arg("true").output();
    let is_gnu = probe
        .as_ref()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stderr).contains("Maximum resident set size"));
    if !is_gnu {
        eprintln!("skipping: /usr/bin/time does not support GNU -v on this host");
    }
    is_gnu
}

/// Runs `program` as a native binary at two argument values and answers the
/// allocation ledger's live counts for each, as `(strings, vecs, aggregates)`.
///
/// The scale-invariance the callers assert is what makes these gates honest: a
/// value leaked once per turn of a loop grows the live count with the turn
/// count, while a program that reclaims what it builds ends both runs holding
/// the same handful of values.
fn live_counts_at(name: &str, program: &str, small: &str, large: &str) -> [(usize, usize); 2] {
    let dir = env::temp_dir().join(format!("gos-own-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join(format!("{name}.gos"));
    std::fs::write(&source, program).unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join(name);
    let read = |field: &str, stderr: &str| -> usize {
        stderr
            .split(field)
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("ledger must report {field}: {stderr}"))
    };
    let run = |iterations: &str| -> (usize, usize) {
        let out = Command::new(&binary)
            .arg(iterations)
            .env("GOS_LEAK_LEDGER", "1")
            .output()
            .expect("run with the allocation ledger");
        assert!(
            out.status.success(),
            "run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        (read("str=", &stderr), read("vec=", &stderr))
    };
    let counts = [run(small), run(large)];
    let _ = std::fs::remove_dir_all(&dir);
    counts
}

/// The allocation ledger's whole line for `program` at two argument values.
///
/// A share taken per turn of a loop and never given back moves one of the
/// counts with the argument; a program that reclaims what it names ends both
/// runs holding the same handful of values.
fn ledger_at(name: &str, program: &str, small: &str, large: &str) -> [String; 2] {
    let dir = env::temp_dir().join(format!("gos-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join(format!("{name}.gos"));
    std::fs::write(&source, program).unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join(name);
    let run = |iterations: &str| -> String {
        let out = Command::new(&binary)
            .arg(iterations)
            .env("GOS_LEAK_LEDGER", "1")
            .output()
            .expect("run with the allocation ledger");
        assert!(
            out.status.success(),
            "run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|line| line.contains("LEAK LEDGER"))
            .unwrap_or_else(|| panic!("no ledger line for {name}"))
            .to_string()
    };
    let lines = [run(small), run(large)];
    let _ = std::fs::remove_dir_all(&dir);
    lines
}

/// A typed decode gives back the document it read.
///
/// The decoder hands its parsed handle to the function that reads the fields;
/// a frame that disowns the handle at that call leaves the whole document
/// alive, so peak memory tracks the number of decodes rather than the size of
/// one.
#[test]
fn typed_json_decode_holds_one_document_at_a_time() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-decode-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("decodelive.gos");
    std::fs::write(
        &source,
        "
use std::encoding::json
use std::env

struct Point { x: f64, y: f64 }
struct Cloud { points: Vec<Point> }

fn main() {
    let mut text = \"{\\\"points\\\":[\"
    let mut i = 0
    while i < 4000 {
        if i > 0 { text += \",\" }
        text += format(\"{{\\\"x\\\":{}.5,\\\"y\\\":{}.25}}\", i, i)
        i += 1
    }
    text += \"]}\"
    let rounds = env::args().first().unwrap_or(\"4\").to_i64().unwrap_or(4)
    let mut total = 0
    let mut k = 0
    while k < rounds {
        total += json::decode::<Cloud>(text).unwrap().points.len()
        k += 1
    }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--release", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("decodelive");
    let rss = |rounds: &str| -> u64 {
        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(&binary)
            .arg(rounds)
            .output()
            .expect("run under /usr/bin/time");
        parse_max_rss_kb(&String::from_utf8_lossy(&out.stderr))
            .expect("GNU time reports a maximum resident set size")
    };
    let few = rss("4");
    let many = rss("64");
    let _ = std::fs::remove_dir_all(&dir);
    // Sixteen times the decodes may cost allocator noise, never a share of
    // the documents: one retained per call is tens of megabytes here.
    assert!(
        many < few + 16 * 1024,
        "peak RSS tracks the decode count ({few} KiB at 4, {many} KiB at 64); \
         each decode is holding on to the document it read"
    );
}

/// A walk over a tree of guarded aggregates reclaims every hop.
///
/// Each step names a node the root already owns, so the cursor's own share of
/// the node's children is redundant. Removing a share without its pair is what
/// a count that tracks the step count reports.
#[test]
fn guarded_tree_walk_holds_a_constant_number_of_values() {
    let lines = ledger_at(
        "guardedwalk",
        "
use std::env

struct Node { sym: i64, left: Option<Node>, right: Option<Node> }

fn build(depth: i64) -> Node {
    if depth == 0 {
        Node { sym: 1, left: None, right: None }
    } else {
        Node { sym: -1, left: Some(build(depth - 1)), right: Some(build(depth - 1)) }
    }
}

fn decode(root: Node, bits: Vec<i64>) -> i64 {
    let mut node = root
    let mut out = 0
    for b in bits {
        node = if b == 0 { node.left.unwrap() } else { node.right.unwrap() }
        if node.sym >= 0 {
            out += node.sym
            node = root
        }
    }
    out
}

fn main() {
    let n = env::args().first().unwrap_or(\"1024\").to_i64().unwrap_or(1024)
    let root = build(4)
    let mut bits = #[]
    for i in 0..n { bits.push((i * 7 + i / 3) % 2) }
    println(\"{}\", decode(root, bits))
}
",
        "1024",
        "65536",
    );
    assert_eq!(
        lines[0], lines[1],
        "the walk holds a number of values that tracks its step count"
    );
}

/// A `&self` call on an aggregate field reclaims each receiver copy.
///
/// The copy names the caller's own storage for the length of the call, so its
/// share of the field is redundant; a share left behind per call moves the
/// live count with the call count.
#[test]
fn reference_receiver_copies_hold_a_constant_number_of_values() {
    let lines = ledger_at(
        "refreceiver",
        "
use std::env

struct Tape { cells: Vec<u8>, pos: i64 }

impl Tape {
    fn get(&self) -> i64 { self.cells[self.pos] as i64 }
    fn add(&mut self, delta: i64) { self.cells[self.pos] = ((self.cells[self.pos] as i64 + delta) & 255) as u8 }
}

struct Machine { tape: Tape, result: i64 }

impl Machine {
    fn step(&mut self, n: i64) -> i64 {
        let mut acc = 0
        for _ in 0..n {
            self.tape.add(1)
            while self.tape.get() > 200 { self.tape.add(-100) }
            acc += self.tape.get()
        }
        acc
    }
}

fn main() {
    let n = env::args().first().unwrap_or(\"1000\").to_i64().unwrap_or(1000)
    let mut m = Machine { tape: Tape { cells: #[0; 4], pos: 1 }, result: 0 }
    println(\"{}\", m.step(n))
}
",
        "1000",
        "64000",
    );
    assert_eq!(
        lines[0], lines[1],
        "the receiver copies hold a number of values that tracks the call count"
    );
}

/// A carrier handed to a Gossamer function stays the caller's.
///
/// The caller gives its share of the payload back itself, so a callee that
/// only reads the carrier holds nothing afterwards, and one that hands the
/// payload on - returned, relayed, or unwrapped - takes a share for it. An
/// extraction out of a copied carrier reads the payload the carrier still
/// owns. A share held or dropped per call moves the live count with the call
/// count.
#[test]
fn lent_carrier_arguments_hold_a_constant_number_of_values() {
    let lines = ledger_at(
        "lentcarrier",
        "
use std::env

fn maybe(i: i64) -> Option<String> {
    if i % 3 == 0 { None } else { Some(\"m\" + i.to_string()) }
}

fn risky(i: i64) -> Result<i64, String> {
    if i % 4 == 0 { Err(\"e\" + i.to_string()) } else { Ok(i) }
}

fn width(r: Result<i64, String>) -> i64 {
    match r {
        Ok(n) => n,
        Err(e) => e.len()
    }
}

fn direct_opt(o: Option<String>) -> Option<String> {
    o
}

fn relay(o: Option<String>) -> Option<String> {
    direct_opt(o)
}

fn take(o: Option<String>) -> String {
    if o.is_some() { o.unwrap() } else { \"none\" }
}

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut total = 0
    let mut kept: Vec<String> = #[]
    for i in 0..n {
        total += width(risky(i))
        let r = risky(i + 1)
        match r {
            Ok(v) => total += v,
            Err(e) => total += e.len()
        }
        if let Some(s) = relay(maybe(i)) {
            total += s.len()
        }
        let a = maybe(i + 1)
        let b = a
        if let Some(s) = b {
            total += s.len()
        }
        let t = take(maybe(i + 2))
        if i < 8 {
            kept.push(t)
        }
    }
    println(\"{} {}\", total, kept.len())
}
",
        "64",
        "4096",
    );
    assert_eq!(
        lines[0], lines[1],
        "lent carriers hold a number of values that tracks the call count"
    );
}

/// A carrier nested in a carrier is reclaimed with the carrier that boxes it.
///
/// The box owns the inner payload, and each reader of the inner carrier takes
/// a share of its own, so building, reading, and dropping nested carriers per
/// turn of a loop ends every run holding the same values.
#[test]
fn nested_carrier_boxes_hold_a_constant_number_of_values() {
    let lines = ledger_at(
        "nestedcarrier",
        "
use std::env

fn inner(i: i64) -> Option<String> {
    if i % 3 == 0 { None } else { Some(\"in\" + i.to_string()) }
}

fn wrap(i: i64) -> Option<Option<String>> {
    if i % 2 == 0 { None } else { Some(inner(i)) }
}

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut total = 0
    let mut kept: Vec<Option<Option<String>>> = #[]
    for i in 0..n {
        let w = wrap(i)
        if let Some(Some(s)) = w {
            total += s.len()
        }
        let e = Some(inner(i + 1)).unwrap()
        if let Some(s) = e {
            total += s.len()
        }
        let f = wrap(i + 1).unwrap_or(Some(\"fb\"))
        if let Some(s) = f {
            total += s.len()
        }
        if i < 8 {
            kept.push(w)
        }
    }
    println(\"{} {}\", total, kept.len())
}
",
        "64",
        "4096",
    );
    assert_eq!(
        lines[0], lines[1],
        "nested carriers hold a number of values that tracks the loop count"
    );
}

/// A `Map` carries no reference count, so each struct field holding one owns
/// its table outright: a struct built from a map the function created and
/// returned, one built from a `Map` parameter, and one bound inside a loop
/// each free exactly the tables they hold. A lent carrier parameter pushed
/// into a `Vec` gives the vector a share of its own.
#[test]
fn map_fields_and_lent_carrier_pushes_hold_a_constant_number_of_values() {
    let lines = ledger_at(
        "mapfields",
        "
use std::env

struct Tags {
    name: String,
    tags: Map<String, i64>,
    ids: Vec<i64>
}

fn build(i: i64) -> Tags {
    let mut m: Map<String, i64> = Map::new()
    m.insert(\"k\" + i.to_string(), i)
    Tags { name: \"t\" + i.to_string(), tags: m, ids: #[i, i + 1] }
}

fn wrap(m: Map<String, i64>) -> Tags {
    Tags { name: \"w\", tags: m, ids: #[] }
}

fn keep(o: Option<Option<String>>) -> Vec<Option<Option<String>>> {
    let mut v: Vec<Option<Option<String>>> = #[]
    v.push(o)
    v
}

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut total = 0
    let mut lens: Vec<i64> = #[]
    for i in 0..n {
        let t = build(i)
        lens.push(t.tags.len())
        let mut m: Map<String, i64> = Map::new()
        m.insert(\"a\", i)
        let w = wrap(m)
        m.insert(\"b\", i)
        total += w.tags.len() + m.len()
        let kept = keep(Some(Some(\"in\" + i.to_string())))
        for k in kept {
            if let Some(Some(s)) = k {
                total += s.len()
            }
        }
    }
    println(\"{} {}\", total, lens.len())
}
",
        "64",
        "4096",
    );
    assert_eq!(
        lines[0], lines[1],
        "map fields and lent carrier pushes hold a number of values that tracks the loop count"
    );
}

/// A struct payload is owned by its carrier's box, so dropping a carrier,
/// extracting from one twice, storing one in a `Vec`, and relaying one through
/// `?` each end every turn of a loop holding the same values.
#[test]
fn struct_payloads_in_carriers_hold_a_constant_number_of_values() {
    let lines = ledger_at(
        "structpayload",
        "
use std::env

struct Tags {
    name: String,
    tags: Map<String, i64>,
    ids: Vec<i64>
}

fn build(i: i64) -> Result<Tags, i64> {
    if i % 3 == 0 {
        return Err(i)
    }
    let mut m: Map<String, i64> = Map::new()
    m.insert(\"k\" + i.to_string(), i)
    Ok(Tags { name: \"t\" + i.to_string(), tags: m, ids: #[i, i + 1] })
}

fn relay(r: Result<Tags, i64>) -> Result<i64, i64> {
    let t = r?
    Ok(t.name.len() + t.tags.len())
}

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut total = 0
    let mut names: Vec<String> = #[]
    for i in 0..n {
        let r = build(i)
        if let Ok(t) = r {
            names.push(t.name)
        }
        if let Ok(u) = r {
            total += u.ids.len() + u.tags.len()
        }
        let dropped = build(i + 1)
        let stored = #[build(i + 2)]
        for s in stored {
            if let Ok(t) = s {
                total += t.ids.len()
            }
        }
        total += relay(build(i)).unwrap_or(0)
    }
    println(\"{} {}\", total, names.len())
}
",
        "64",
        "4096",
    );
    assert_eq!(
        lines[0], lines[1],
        "struct payloads in carriers hold a number of values that tracks the loop count"
    );
}

/// An error is reclaimed from each place that holds one - a binding, a cause,
/// a `Result` arm handed through `map_err`, `map`, `and_then`, `or_else`, and
/// `?`, a struct field, a tuple position - so every turn of a loop ends holding
/// the same values.
#[test]
fn errors_in_every_holder_hold_a_constant_number_of_values() {
    let lines = ledger_at(
        "errorcells",
        "
use std::{env, errors, option, result}

struct Failure {
    code: i64,
    err: errors::Error
}

struct Point {
    x: i64,
    y: i64
}

fn located(i: i64) -> Result<Point, errors::Error> {
    if i % 2 == 0 {
        return Err(errors::new(\"unplaced\"))
    }
    Ok(Point { x: i, y: 1 })
}

fn check(i: i64) -> Result<i64, errors::Error> {
    if i % 2 == 0 {
        return Err(errors::new(\"even\"))
    }
    Ok(i)
}

fn layered(i: i64) -> Result<i64, errors::Error> {
    let v = check(i).map_err(|e| errors::wrap(e, \"layer\"))?
    Ok(v + 1)
}

fn pair(i: i64) -> (Result<String, errors::Error>, i64) {
    if i % 3 == 0 {
        (Err(errors::new(\"third\")), i)
    } else {
        (Ok(\"node-\" + i.to_string()), i)
    }
}

fn kept(i: i64) -> Result<String, errors::Error> {
    let r, _n = pair(i)
    r
}

fn measured(reply: Result<String, errors::Error>) -> i64 {
    reply.unwrap_or(\"none\").len()
}

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut total = 0
    for i in 0..n {
        match layered(i) {
            Ok(v) => total += v,
            Err(e) => total += e.to_string().len(),
        }
        let f = Failure { code: i, err: errors::new(\"f\") }
        total += f.code + f.err.message().len()
        match check(i).map_err(|e| e.to_string()) {
            Ok(v) => total += v,
            Err(s) => total += s.len(),
        }
        match kept(i) {
            Ok(s) => total += s.len(),
            Err(e) => total += e.message().len(),
        }
        let chained = errors::wrap(errors::wrap(errors::new(\"root\"), \"mid\"), \"top\")
        total += chained.chain().len()
        let reply = kept(i)
        if reply.is_ok() {
            total += 1
        }
        total += measured(reply) + reply.unwrap_or(\"fallback\").len()
        if check(i) |> result::err |> option::is_some {
            total += 1
        }
        let spot = located(i)
        match spot |> |v| result::map(v, |p: Point| p.x) {
            Ok(v) => total += v,
            Err(e) => total += e.message().len(),
        }
        match spot.and_then(|p| Ok(p.y)) {
            Ok(v) => total += v,
            Err(e) => total += e.to_string().len(),
        }
        match spot |> |v| result::or_else(v, |_e: errors::Error| Ok(Point { x: 2, y: 2 })) {
            Ok(p) => total += p.x,
            Err(_) => total += 9,
        }
    }
    println(\"{}\", total)
}
",
        "64",
        "4096",
    );
    assert_eq!(
        lines[0], lines[1],
        "errors in every holder hold a number of values that tracks the loop count"
    );
}

/// An indexed store gives back the element it replaces.
///
/// A `Vec<String>` owns each element - its teardown releases one share per
/// slot - so a write over a slot has to return the outgoing string here.
#[test]
fn indexed_string_store_releases_the_element_it_replaces() {
    let counts = live_counts_at(
        "idxstore",
        "
use std::env

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut keys: Vec<String> = #[]
    for i in 0..8 { keys.push(format(\"k{}\", i)) }
    let mut i = 0
    while i < n {
        keys[i % 8] = format(\"item_{}\", i)
        i += 1
    }
    println(\"{}\", keys.len())
}
",
        "64",
        "4096",
    );
    assert_eq!(
        counts[0].0, counts[1].0,
        "live String count tracks the store count ({} at 64, {} at 4096)",
        counts[0].0, counts[1].0
    );
}

/// A store into a struct field leaves the value named once.
///
/// The field takes a share of what it is handed, so the frame gives its own
/// back at the store rather than naming the value a second time.
#[test]
fn struct_field_store_leaves_one_share_per_value() {
    let counts = live_counts_at(
        "fieldstore",
        "
use std::env

struct Doc { rendered: String, rows: Vec<i64>, total: i64 }

fn build(i: i64) -> Vec<i64> { #[i, i + 1, i + 2] }

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut d = Doc { rendered: \"\", rows: #[], total: 0 }
    let mut i = 0
    while i < n {
        let mut r = String::with_capacity(16)
        r.push_str(\"row \")
        r.push_char('x')
        d.rendered = r
        d.rows = build(i)
        d.total += d.rendered.byte_len() + d.rows.len()
        i += 1
    }
    println(\"{}\", d.total)
}
",
        "64",
        "4096",
    );
    assert_eq!(
        counts[0], counts[1],
        "live counts track the iteration count ({:?} at 64, {:?} at 4096)",
        counts[0], counts[1]
    );
}

/// A map insert whose answer nothing reads gives back the value it replaced.
///
/// The answering form hands over the previous value, so a call site that binds
/// it to a name no one reads takes the form that answers nothing.
#[test]
fn discarded_map_insert_answer_frees_the_replaced_value() {
    let counts = live_counts_at(
        "mapinsert",
        "
use std::env

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut m: Map<String, String> = Map::new()
    let mut i = 0
    while i < n {
        let _ = m.insert(format(\"k{}\", i % 8), format(\"v{}\", i))
        i += 1
    }
    println(\"{}\", m.len())
}
",
        "64",
        "4096",
    );
    assert_eq!(
        counts[0].0, counts[1].0,
        "live String count tracks the insert count ({} at 64, {} at 4096)",
        counts[0].0, counts[1].0
    );
}

/// A buffer moved into an outer binding each turn reclaims the prior one.
///
/// The reads and writes through the buffer stand beside it rather than between
/// it and the binding it is handed to, so the move still transfers.
#[test]
fn buffer_moved_into_an_outer_binding_reclaims_each_prior_one() {
    let counts = live_counts_at(
        "rankmove",
        "
use std::env

fn work(n: i64, rounds: i64) -> i64 {
    let mut rank: Vec<i64> = #[0; n]
    let mut k = 0
    while k < rounds {
        let mut next: Vec<i64> = #[0; n]
        for i in 1..n { next[i] = rank[i - 1] + 1 }
        rank = next
        k += 1
    }
    rank[0]
}

fn main() {
    let rounds = env::args().first().unwrap_or(\"8\").to_i64().unwrap_or(8)
    println(\"{}\", work(64, rounds))
}
",
        "8",
        "512",
    );
    assert_eq!(
        counts[0].1, counts[1].1,
        "live Vec count tracks the round count ({} at 8, {} at 512)",
        counts[0].1, counts[1].1
    );
}

/// A tuple popped out of a container is reclaimed where it is read.
///
/// An element wider than a word comes back as the address of a copy the
/// container allocated, and the words are in the reader's own storage once the
/// read lands - so the copy dies there.
#[test]
fn popped_tuple_copies_do_not_accumulate() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-pop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("poptuple.gos");
    std::fs::write(
        &source,
        "
use std::env

fn main() {
    let n = env::args().first().unwrap_or(\"64\").to_i64().unwrap_or(64)
    let mut stack: Vec<(i64, i64)> = #[(0, 0)]
    let mut total = 0
    let mut i = 0
    while i < n {
        stack.push((i, i + 1))
        while let Some(e) = stack.pop() { let a, b = e; total += a + b }
        i += 1
    }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("poptuple");
    let rss = |iterations: &str| -> u64 {
        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(&binary)
            .arg(iterations)
            .output()
            .expect("run under /usr/bin/time");
        parse_max_rss_kb(&String::from_utf8_lossy(&out.stderr))
            .expect("GNU time reports a maximum resident set size")
    };
    let small = rss("1024");
    let large = rss("1048576");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        large < small + 4096,
        "peak RSS tracks the pop count ({small} KB at 1024 pops, {large} KB at \
         1048576): every popped element's copy is still held"
    );
}

/// Gates ownership of the strings a split hands back.
///
/// A shim that answers `[String]` puts the pieces in the vector, so the vector
/// has to own them: it is what frees them, and a vector built without the
/// string element kind reclaims only its own buffer. The bound is
/// scale-invariance - the same program is run at two iteration counts a
/// thousand apart, and a leaked piece per word grows the live string count
/// with that count.
#[test]
fn split_results_release_their_pieces() {
    let dir = env::temp_dir().join(format!("gos-split-own-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("split_own.gos");
    std::fs::write(
        &source,
        "
use std::{env, strings}

fn main() {
    let n = env::args().first().unwrap_or(\"1\").to_i64().unwrap_or(1)
    let text = \"alpha beta gamma delta epsilon zeta\".repeat(8)
    let mut total = 0
    for _ in 0..n {
        total += text.split_whitespace().len()
        total += strings::splitn(text, 4, \" \").len()
        total += strings::split(text, \" \").len()
    }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("split_own");

    let live = |iterations: &str| -> usize {
        let out = Command::new(&binary)
            .arg(iterations)
            .env("GOS_LEAK_LEDGER", "1")
            .output()
            .expect("run with the allocation ledger");
        assert!(
            out.status.success(),
            "binary failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        stderr
            .split("str=")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("ledger must report a live string count: {stderr}"))
    };

    let small = live("1");
    let large = live("1000");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        large <= small + 8,
        "live string count tracks the split count ({small} after 1 pass, \
         {large} after 1000): the pieces are reaching nothing that frees them"
    );
}

/// Gates ownership of the documents `yaml::parse_all` hands back.
///
/// Each element is a handle holding a share of its document's tree, so the
/// vector that holds them has to give those shares back when it dies. The
/// bound is scale-invariance: the same program is run at two iteration counts
/// a hundred apart, and a document kept alive per parse grows peak memory with
/// that count.
#[test]
fn yaml_parse_all_releases_its_documents() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-yaml-own-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("yaml_own.gos");
    std::fs::write(
        &source,
        "
use std::{env, encoding::yaml}

fn main() {
    let n = env::args().first().unwrap_or(\"1\").to_i64().unwrap_or(1)
    let text = \"a: 1\\nb: [1, 2, 3]\\n---\\nc: hello\\nd: [4, 5, 6]\\n\"
    let mut total = 0
    for _ in 0..n { total += yaml::parse_all(text).unwrap_or(#[]).len() }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("yaml_own");
    let rss = |iterations: &str| -> u64 {
        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(&binary)
            .arg(iterations)
            .output()
            .expect("run under /usr/bin/time");
        assert!(
            out.status.success(),
            "binary failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_max_rss_kb(&String::from_utf8_lossy(&out.stderr))
            .expect("GNU time reports a maximum resident set size")
    };
    let small = rss("100");
    let large = rss("100000");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        large < small + 4096,
        "peak RSS tracks the parse count ({small} KB at 100 parses, {large} KB \
         at 100000): every parsed document is still held"
    );
}

/// A `json::get` answers a handle onto the parsed document inside an
/// `Option`, and that arm is the only name the handle has. If the arm does
/// not give it back, every document the loop parses stays alive, so peak RSS
/// tracks the parse count.
#[test]
fn json_get_releases_the_document_it_views() {
    if !gnu_time_available() {
        return;
    }
    let dir = env::temp_dir().join(format!("gos-json-own-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("json_own.gos");
    std::fs::write(
        &source,
        "
use std::{env, encoding::json}

fn field(value: json::Value, name: String) -> i64 {
    json::get(value, name).and_then(json::as_i64).unwrap_or(0)
}

fn main() {
    let n = env::args().first().unwrap_or(\"1\").to_i64().unwrap_or(1)
    let text = \"{\\\"a\\\": 1, \\\"b\\\": [1, 2, 3], \\\"c\\\": {\\\"d\\\": 4}}\"
    let mut total = 0
    for _ in 0..n {
        let doc = json::parse(text).unwrap()
        total += field(doc, \"a\")
        if json::get(doc, \"c\").is_some() { total += 1 }
        total += json::get(doc, \"b\").and_then(json::as_array).unwrap_or(#[]).len()
    }
    println(\"{}\", total)
}
",
    )
    .unwrap();
    let build = Command::new(gos_bin())
        .args(["build", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("gos build");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("json_own");
    let rss = |iterations: &str| -> u64 {
        let out = Command::new("/usr/bin/time")
            .arg("-v")
            .arg(&binary)
            .arg(iterations)
            .output()
            .expect("run under /usr/bin/time");
        assert!(
            out.status.success(),
            "binary failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_max_rss_kb(&String::from_utf8_lossy(&out.stderr))
            .expect("GNU time reports a maximum resident set size")
    };
    let small = rss("100");
    let large = rss("100000");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        large < small + 4096,
        "peak RSS tracks the parse count ({small} KB at 100 parses, {large} KB \
         at 100000): every parsed document is still held"
    );
}
