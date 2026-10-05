//! The external LLVM toolchain: locating `opt`, `llc`, and `clang`, checking their versions, choosing target triples and CPUs, and running the pipeline.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{
    OptProfile, PgoMode, TARGET_TRIPLE_OVERRIDE, audit_llvm_ir_symbols,
    disable_loop_idiom_for_target, file_identity, fnv1a_64, opt_profile, pgo_mode,
    scopeguard_tool_time, toolchain_cache_dir, want_dwarf, want_reproducible,
};

/// Returns the temp directory the LLVM pipeline emits its
/// intermediate IR / opt-bitcode artifacts into.
///
/// the reproducible-mode name was a fixed
/// `gos-llvm-reproducible`, so parallel reproducible builds (two
/// `gos build --reproducible` of distinct projects on the same
/// host) raced on `unit.ll` / `unit.opt.bc` / `unit.o`. We now
/// keep the deterministic-prefix invariant the reproducible mode
/// needs (same input → same path → same artifact bytes) by
/// hashing the entry source path into the directory name. Two
/// builds of the same source still land in the same dir; two
/// concurrent builds of different sources get distinct dirs.
pub(super) fn pipeline_tmp_dir() -> Result<PathBuf> {
    use std::hash::Hasher as _;
    let tmp_dir = if want_reproducible() {
        // Hash the CWD + program path so parallel reproducible
        // builds of different inputs don't collide on a fixed
        // directory name. Same input → same hash → same dir →
        // bit-identical artifacts across two builds.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Ok(cwd) = std::env::current_dir() {
            hasher.write(cwd.as_os_str().as_encoded_bytes());
        }
        if let Some(arg0) = std::env::args_os().next() {
            hasher.write(arg0.as_encoded_bytes());
        }
        let h = hasher.finish();
        std::env::temp_dir().join(format!("gos-llvm-repro-{h:016x}"))
    } else {
        // Per-pid + per-call counter so concurrent
        // `render_ir_to_string` / `compile_to_object` calls inside
        // the same process don't trample each other's `unit.ll` /
        // `unit.o`. Two parallel tests in the same `cargo test`
        // process used to share a single tmp dir and produce
        // mutually-corrupted IR.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        std::env::temp_dir().join(format!("gos-llvm-{}-{seq}", std::process::id()))
    };
    std::fs::create_dir_all(&tmp_dir).with_context(|| format!("creating {}", tmp_dir.display()))?;
    Ok(tmp_dir)
}

/// Path-only variant of the historical `invoke_llc(ir_str, triple)
/// -> Vec<u8>`. Reads the IR from `ll_path` (already on disk) and
/// writes the resulting object directly to `obj_out`. The previous
/// API forced callers to round-trip the IR + the object through
/// memory; this one keeps both on disk and returns nothing.
///
/// Pipeline order: explicit IR verification via `opt -passes=verify`
/// → mid-end optimisation (`opt -O1`/`-O3`) → backend (`llc`). The
/// verify pass runs first so shape regressions surface with source-
/// level context (the verifier's stderr is forwarded verbatim)
/// before any optimisation rewrites obscure the offending value.
/// When `announce` is true and `GOS_LLVM_DUMP` is set, emits
/// `llvm backend: IR at <ll_path>` so callers / test harnesses can
/// locate the IR file. Pass `false` in the parallel per-body path
/// where the caller announces the concatenated dump instead.
pub(super) fn invoke_llc_pipeline(
    ll_path: &std::path::Path,
    obj_out: &std::path::Path,
    triple: &str,
    announce: bool,
) -> Result<()> {
    let started = std::time::Instant::now();
    let keep_artifacts = std::env::var("GOS_LLVM_DUMP").is_ok();
    if keep_artifacts && announce {
        eprintln!("llvm backend: IR at {}", ll_path.display());
    }
    audit_llvm_ir_symbols(ll_path)?;
    let _guard = scopeguard_tool_time(started);
    let profile = opt_profile();
    let mcpu = mcpu_target(triple);
    if let Some(clang) = integrated_clang_path(triple) {
        if std::env::var_os("GOS_PIPELINE_TRACE").is_some() {
            eprintln!("llvm pipeline: clang at {}", clang.display());
        }
        return invoke_clang_pipeline(&clang, ll_path, obj_out, triple, profile, &mcpu);
    }
    if std::env::var_os("GOS_PIPELINE_TRACE").is_some() {
        eprintln!("llvm pipeline: opt + llc");
    }

    let opt_path = ll_path.with_extension("opt.bc");
    // Both profiles run `opt` because the lowerer emits some
    // non-canonical shapes (e.g. integer-typed constants in
    // floating-point store positions) that `opt`'s
    // instcombine + verifier passes fix up. Skipping `opt`
    // entirely sends those shapes straight to `llc`, which
    // rejects them.
    //
    // Debug runs the smallest mid-end that makes the emitter's output
    // usable: `sroa` promotes the alloca per local that every lowered body
    // starts with, and `early-cse` / `instcombine` / `simplifycfg` clean up
    // after it. A full `default<O1>` or `default<O2>` mid-end costs more and
    // measures the same, because after `sroa` the remaining distance to a
    // release binary is not in the IR. Instcombine is asked not to verify its
    // fixpoint because a pipeline of this length gives it one iteration,
    // where the check expects the repeats a longer pipeline would run.
    //
    // The back end runs at `O1`, where the register allocator is the greedy
    // one and the machine passes run. `O0` selects the fast allocator, which
    // keeps every value in memory: it costs the benchmark suite 1.1x to 7x and
    // saves two thirds of the back end's time, which the fan-out across cores
    // (see [`JOB_CEILING`]) overlaps. `O2` costs more than `O1` and
    // measures the same, and there is no setting between the two - `-O0
    // -regalloc=greedy` is refused.
    //
    // Release profile uses `default<O3>` for full optimisation.
    //
    // The IR verifier runs first in both pipelines (as the
    // initial `verify` entry) so malformed IR surfaces with
    // source-level context before any optimisation rewrites
    // obscure the offending value.
    let (opt_passes, llc_level) = match profile {
        OptProfile::Debug => (
            "verify,sroa,early-cse,instcombine<no-verify-fixpoint>,simplifycfg",
            "-O1",
        ),
        OptProfile::Release => ("verify,default<O3>", "-O3"),
    };
    let opt_tool = find_opt()?;
    let mut opt_cmd = std::process::Command::new(&opt_tool);
    opt_cmd
        .arg(format!("-passes={opt_passes}"))
        .arg(format!("-mtriple={triple}"))
        // Match `rustc -C target-cpu=native`: tell the
        // mid-level optimiser the target's feature set so the
        // loop / SLP vectorisers can emit AVX2 / FMA when the
        // host supports them. Without this, `opt` only knows
        // the baseline triple's features.
        //
        // `GOS_LLVM_MCPU` overrides - `x86-64-v3` is the
        // documented escape hatch when the host's AVX-512
        // entry/exit transition penalty hurts short-running
        // benchmarks (the §5 release-perf investigation
        // found this on fannkuch).
        .arg(format!("-mcpu={mcpu}"))
        // `+prefer-256-bit` is an x86 AVX-512 feature flag, so only pass it
        // for x86_64 targets. Keeping the width capped avoids ZMM transition
        // costs around runtime calls.
        .args(if target_arch_from_triple(triple) == "x86_64" {
            &["-mattr=+prefer-256-bit"][..]
        } else {
            &[][..]
        })
        // Block `LoopIdiomRecognize` from rewriting trivial
        // copy / shift loops into `llvm.memcpy` / `llvm.memmove`
        // calls. Only relevant on release (debug uses a minimal
        // pass set that doesn't run this recogniser). On release,
        // the PLT call overhead around musl's `memcpy` dwarfs the
        // copy work on small n. Leaving idiom-recognise off keeps
        // the inline-loop shape that beats Cranelift on short-trip
        // benchmarks. The narrower `disable-memcpy-idiom` flag
        // no longer takes effect under LLVM 18's new pass manager.
        ;
    opt_cmd.args(llvm_pass_options(triple, profile));
    // PGO instrumentation builds an instrumented binary that emits raw
    // profile data when the program exits. Link with
    // `libclang_rt.profile-x86_64.a` (handled in
    // `gossamer-cli/src/cmd/build.rs`), then merge the resulting `.profraw`
    // with `llvm-profdata merge -output=...`. The environment variables are
    // retained only for embedding callers that predate the CLI options.
    let selected_pgo = pgo_mode();
    let pgo_collect = match selected_pgo.as_ref() {
        Some(PgoMode::Collect(path)) => Some(path.display().to_string()),
        Some(PgoMode::Profile(_)) => None,
        None => std::env::var("GOS_PGO_COLLECT").ok(),
    };
    if let Some(profraw) = pgo_collect {
        opt_cmd
            .arg("--pgo-kind=pgo-instr-gen-pipeline")
            .arg(format!("--pgo-test-profile-file={profraw}"));
    }
    // PGO optimisation feeds a previously collected and merged profile into
    // the `opt` mid-end so branch weights, inlining thresholds, and the loop
    // / SLP vectorisers are guided by real execution frequencies. A selected
    // CLI mode wins over the legacy environment to keep the two modes
    // mutually exclusive.
    let pgo_profile = match selected_pgo.as_ref() {
        Some(PgoMode::Collect(_)) => None,
        Some(PgoMode::Profile(path)) => Some(path.display().to_string()),
        None => std::env::var("GOS_PGO_PROFILE").ok(),
    };
    if let Some(profdata) = pgo_profile {
        opt_cmd
            .arg("--pgo-kind=pgo-instr-use-pipeline")
            .arg(format!("--profile-file={profdata}"));
    }
    opt_cmd.arg(ll_path).arg("-o").arg(&opt_path);
    let opt_output = run_with_timeout(opt_cmd, opt_timeout(), "opt")
        .with_context(|| format!("spawn {}", opt_tool.display()))?;
    if !opt_output.status.success() {
        if keep_artifacts {
            eprintln!("llvm backend: failing IR kept at {}", ll_path.display());
        }
        return Err(anyhow!(
            "opt failed ({status}): {stderr}\n\
             hint: if the error begins with 'Broken module' it is an IR \
             shape regression in the lowerer (dump with GOS_LLVM_DUMP=1); \
             otherwise it is an opt mid-end blowup - largest IR usually \
             drives those, inspect the function names in the IR.",
            status = opt_output.status,
            stderr = String::from_utf8_lossy(&opt_output.stderr)
        ));
    }
    // Backend: `llc -O3` → object file with PIC relocations
    // (matches the rest of the build pipeline; the linker
    // refuses non-PIC objects for default PIE binaries).
    // `-mcpu=native` lets LLVM target the host's full
    // instruction set (AVX2 / FMA / etc. on modern Ryzen) -
    // matches what `rustc -C target-cpu=native` does for the
    // bench-game references.
    let llc = find_llc()?;
    let mut llc_cmd = std::process::Command::new(&llc);
    llc_cmd
        .arg(llc_level)
        .arg("-filetype=obj")
        .arg(format!("-mtriple={triple}"))
        // COFF (Windows) has no GOT: `-relocation-model=pic` makes llc
        // emit GOT-relative relocations for external data symbols that
        // rust-lld's link flavour cannot resolve, so every `gos build`
        // fails at link time. ELF (PIE default) and Mach-O (PIC-only)
        // both require pic. Mirrors the Cranelift `is_pic` guard in
        // native/compile.rs.
        .args(if triple.contains("windows") {
            &[][..]
        } else {
            &["-relocation-model=pic"][..]
        })
        .arg(format!("-mcpu={mcpu}"))
        // Match the mid-end vector-width policy during late code generation.
        .args(if target_arch_from_triple(triple) == "x86_64" {
            &["-mattr=+prefer-256-bit"][..]
        } else {
            &[][..]
        })
        .arg(&opt_path)
        .arg("-o")
        .arg(obj_out);
    // Pin DWARF version to match what the module metadata declared
    // (`!{i32 7, "Dwarf Version", i32 4}`). `llc` may otherwise pick
    // a newer default if the host LLVM is bumped, producing object
    // files that older debuggers can't read.
    if want_dwarf() {
        llc_cmd.arg("-dwarf-version=4");
    }
    let output = run_with_timeout(llc_cmd, opt_timeout(), "llc")
        .with_context(|| format!("spawn {}", llc.display()))?;
    if !output.status.success() {
        if keep_artifacts {
            eprintln!("llvm backend: failing IR kept at {}", ll_path.display());
        }
        return Err(anyhow!(
            "llc failed ({status}): {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr)
        ));
    }
    let _ = std::fs::remove_file(&opt_path);
    Ok(())
}

/// Runs LLVM's mid-end and object backend through one Clang driver process.
/// Clang consumes LLVM IR directly, so this is equivalent to the normal
/// `opt` then `llc` sequence for non-PGO builds while avoiding a second child
/// launch and the intermediate bitcode file. The split-tool path remains the
/// compatibility route for PGO and installations without Clang.
/// Optimiser options both pipelines hand LLVM, `opt` directly and `clang`
/// through `-mllvm`.
pub(super) fn llvm_pass_options(triple: &str, profile: OptProfile) -> Vec<&'static str> {
    let mut options = Vec::new();
    if matches!(profile, OptProfile::Release) && disable_loop_idiom_for_target(triple) {
        options.push("-disable-loop-idiom-all");
    }
    options
}

fn invoke_clang_pipeline(
    clang: &std::path::Path,
    ll_path: &std::path::Path,
    obj_out: &std::path::Path,
    triple: &str,
    profile: OptProfile,
    mcpu: &str,
) -> Result<()> {
    let mut cmd = std::process::Command::new(clang);
    cmd.arg("-x")
        .arg("ir")
        .arg("-c")
        .arg(match profile {
            OptProfile::Debug => "-O0",
            OptProfile::Release => "-O3",
        })
        .arg(format!("--target={triple}"));
    if target_arch_from_triple(triple) == "x86_64" {
        cmd.arg(format!("-march={mcpu}"));
        cmd.arg("-mprefer-vector-width=256");
    } else {
        cmd.arg(format!("-mcpu={mcpu}"));
    }
    if !triple.contains("windows") {
        cmd.arg("-fPIC");
    }
    for option in llvm_pass_options(triple, profile) {
        cmd.arg("-mllvm").arg(option);
    }
    if want_dwarf() {
        cmd.arg("-gdwarf-4");
    }
    cmd.arg(ll_path).arg("-o").arg(obj_out);
    let output = run_with_timeout(cmd, opt_timeout(), "clang")
        .with_context(|| format!("spawn {}", clang.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "clang IR-to-object pipeline failed ({status}): {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

/// Returns the wall-clock cap for the `opt` and `llc` subprocesses.
/// `GOS_LLVM_OPT_TIMEOUT_SECS=N` overrides; defaults to 10 minutes,
/// generous enough for huge monomorph fan-outs but tight enough
/// that an unbounded `opt -O3` blowup turns into a build failure
/// instead of a process holding the runner forever.
/// Target CPU passed to `opt` and `llc`. Host release builds default to
/// `native` (matching `rustc -C target-cpu=native`); `GOS_LLVM_MCPU` lets
/// callers override. Reproducible and cross builds keep a portable target
/// baseline.
/// Default LLVM `-mcpu` target used when `GOS_LLVM_MCPU` is unset.
pub(super) fn mcpu_target(triple: &str) -> String {
    if let Ok(s) = std::env::var("GOS_LLVM_MCPU") {
        return s;
    }
    mcpu_for(
        triple,
        TARGET_TRIPLE_OVERRIDE.get().is_some(),
        want_reproducible(),
    )
}

/// The architecture component of an LLVM target triple
/// (`x86_64-unknown-linux-gnu` -> `"x86_64"`).
pub(super) fn target_arch_from_triple(triple: &str) -> &'static str {
    match triple.split('-').next().unwrap_or("") {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        "riscv64" | "riscv64gc" => "riscv64",
        _ => "unknown",
    }
}

pub(super) fn llvm_target_triple_for(triple: &str) -> String {
    llvm_target_triple_for_with_deployment(
        triple,
        std::env::var("MACOSX_DEPLOYMENT_TARGET").ok().as_deref(),
    )
}

fn llvm_target_triple_for_with_deployment(triple: &str, configured: Option<&str>) -> String {
    if !triple.ends_with("-apple-darwin") {
        return triple.to_string();
    }
    let arch = triple.split('-').next().unwrap_or("");
    if arch.is_empty() {
        return triple.to_string();
    }
    let deployment = normalized_macos_deployment_target(configured);
    format!("{arch}-apple-macosx{deployment}")
}

fn normalized_macos_deployment_target(configured: Option<&str>) -> String {
    const DEFAULT: &str = "15.0";

    let value = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT);
    let mut parts = value.split('.');
    let Some(major) = parts.next().filter(|part| is_decimal_component(part)) else {
        return value.to_string();
    };
    let minor = parts
        .next()
        .filter(|part| is_decimal_component(part))
        .unwrap_or("0");
    let patch = parts
        .next()
        .filter(|part| is_decimal_component(part))
        .unwrap_or("0");
    format!("{major}.{minor}.{patch}")
}

fn is_decimal_component(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

/// `-mcpu` for `triple`. `is_cross` is true when an explicit `--target`
/// override is active. A cross or reproducible build must never use `native`,
/// which names the host CPU.
fn mcpu_for(triple: &str, is_cross: bool, reproducible: bool) -> String {
    if !is_cross {
        return if reproducible {
            match target_arch_from_triple(triple) {
                "x86_64" => "x86-64-v3".to_string(),
                "aarch64" => "generic".to_string(),
                _ => "generic".to_string(),
            }
        } else {
            "native".to_string()
        };
    }
    match target_arch_from_triple(triple) {
        // Reproducible x86-64 baseline (AVX2 + BMI2 + FMA, ~2013+).
        "x86_64" => "x86-64-v3".to_string(),
        // Generic ARMv8-A: portable across every aarch64 device;
        // `GOS_LLVM_MCPU=cortex-a76` opts into Pi 5 tuning.
        "aarch64" => "generic".to_string(),
        _ => "generic".to_string(),
    }
}

fn opt_timeout() -> std::time::Duration {
    let secs = std::env::var("GOS_LLVM_OPT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(600);
    std::time::Duration::from_secs(secs)
}

/// Spawns `cmd`, waits up to `timeout`, and surfaces a clear error
/// when the subprocess exceeds the cap (kills the child first so
/// it doesn't outlive the build). Captures stdout / stderr through
/// a 64 KiB-per-stream cap so a runaway `opt`/`llc` diagnostic
/// stream cannot grow unbounded. The polling cadence (50 ms) keeps
/// the steady-state overhead negligible compared to `opt -O3`'s
/// usual runtime.
fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
    tool: &str,
) -> Result<std::process::Output> {
    use std::io::Read;
    use wait_timeout::ChildExt as _;

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {tool}"))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = stdout_pipe.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            cap_diagnostic_stream(buf)
        })
    });
    let stderr_thread = stderr_pipe.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            cap_diagnostic_stream(buf)
        })
    });
    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "{tool} exceeded {secs}s timeout (set GOS_LLVM_OPT_TIMEOUT_SECS to raise it)",
                secs = timeout.as_secs(),
            ));
        }
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("{tool} wait failed: {err}"));
        }
    };
    let stdout = stdout_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Caps a captured subprocess stream at the last 64 KiB. LLVM tools
/// occasionally emit hundreds of MB of repetitive diagnostics (e.g.
/// instcombine loops on pathological IR); without a cap, the parent
/// build process would mirror that growth in RSS while waiting for
/// the child to exit. The tail is kept rather than the head because
/// the actionable error (the failure point) is invariably last.
fn cap_diagnostic_stream(buf: Vec<u8>) -> Vec<u8> {
    const CAP: usize = 64 * 1024;
    if buf.len() <= CAP {
        return buf;
    }
    let trimmed_start = buf.len() - CAP;
    let mut out = Vec::with_capacity(CAP + 64);
    out.extend_from_slice(
        format!("[diagnostic stream truncated: dropped {trimmed_start} bytes]\n").as_bytes(),
    );
    out.extend_from_slice(&buf[trimmed_start..]);
    out
}

/// The LLVM major discovery prefers: the one `rustc` bundles.
///
/// What this buys is one optimiser between a developer's machine and
/// CI. The emitted IR is optimised by whichever `clang` / `opt` / `llc`
/// discovery finds, so an unpinned toolchain means two machines compile
/// the same program through two different optimisers - and in a project
/// whose quality argument is that any tier divergence is a bug, a
/// divergence that is really an LLVM-version artifact costs a real
/// investigation. Preferring the major `rustc` already bundles means
/// the whole toolchain has one version to name.
///
/// It is pinned to `rust-toolchain.toml`, not to LLVM's release
/// cadence: `preferred_llvm_major_matches_rustc` fails the build when
/// the two drift apart, so a toolchain bump is one deliberate change to
/// both.
pub const PREFERRED_LLVM_MAJOR: u32 = 22;

/// The oldest LLVM major that still compiles this emitter's IR.
///
/// It stays where the toolchain has always had it. Every fixture that
/// builds under 18 builds bit-identically under 20, and the two changes
/// this emitter would have had to care about - LLVM 21's `nocapture`
/// rename and the debug-record transition - reach IR it does not write:
/// it emits only `nounwind` and `noalias`, and no `llvm.dbg.*` at all.
/// An older major therefore produces the same program, and Ubuntu 24.04
/// still ships 18, so raising this floor would cost those users a manual
/// LLVM install and buy them nothing.
pub const MINIMUM_LLVM_MAJOR: u32 = 18;

/// A floor above the preferred major would leave no version satisfying
/// both, so the pair is checked where it is written rather than at run
/// time.
const _: () = assert!(MINIMUM_LLVM_MAJOR <= PREFERRED_LLVM_MAJOR);

/// The LLVM major `tool` reports, or `None` when it does not answer a
/// recognisable `--version`.
fn llvm_tool_major(tool: &Path) -> Option<u32> {
    if let Some(remembered) = remembered_llvm_major(tool) {
        return remembered.0;
    }
    let major = probe_llvm_major(tool);
    remember_llvm_major(tool, major);
    major
}

/// A version answer read back from the cache. The inner `Option` is the
/// answer itself, which is `None` for a tool whose banner said nothing.
struct RememberedMajor(Option<u32>);

/// Path of the note recording `tool`'s major version, or `None` when this
/// build keeps no cache.
///
/// Asking a tool its version runs it, and an LLVM tool's startup maps the
/// whole shared library, which on a small program costs more than every
/// other piece of cache bookkeeping together. The answer changes only when
/// the binary does, so the note is keyed by the binary's identity.
fn llvm_major_note_path(tool: &Path) -> Option<PathBuf> {
    let dir = toolchain_cache_dir()?;
    let key = fnv1a_64(file_identity(tool).as_bytes());
    Some(dir.join(format!("llvm-major-{key:016x}")))
}

fn remembered_llvm_major(tool: &Path) -> Option<RememberedMajor> {
    let text = std::fs::read_to_string(llvm_major_note_path(tool)?).ok()?;
    let text = text.trim();
    if text == "unknown" {
        return Some(RememberedMajor(None));
    }
    text.parse().ok().map(|major| RememberedMajor(Some(major)))
}

fn remember_llvm_major(tool: &Path, major: Option<u32>) {
    let Some(path) = llvm_major_note_path(tool) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        major.map_or_else(|| "unknown".to_string(), |m| m.to_string()),
    );
}

fn probe_llvm_major(tool: &Path) -> Option<u32> {
    let out = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_llvm_major(&text)
}

/// The major from an LLVM tool's `--version` banner, which spells the
/// version either as `LLVM version 22.1.2` or, for a vendor clang, as
/// `clang version 22.1.8`.
fn parse_llvm_major(text: &str) -> Option<u32> {
    for marker in ["LLVM version ", "clang version "] {
        if let Some(rest) = text.split(marker).nth(1) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(major) = digits.parse::<u32>() {
                return Some(major);
            }
        }
    }
    None
}

/// Reports the found tool's major once, when it is not the preferred
/// one. An older major still builds a correct program; what it cannot
/// do is take part in cross-language LTO, so the warning names that and
/// not something vaguer. Below the minimum is an error, because the IR
/// this emitter writes is not known to parse there at all.
fn check_llvm_major(tool_name: &str, path: &Path) -> std::result::Result<(), String> {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let Some(major) = llvm_tool_major(path) else {
        return Ok(());
    };
    if major == PREFERRED_LLVM_MAJOR {
        return Ok(());
    }
    if major < MINIMUM_LLVM_MAJOR {
        return Err(format!(
            "{tool_name} at {} is LLVM {major}, below the minimum this toolchain \
             emits IR for (LLVM {MINIMUM_LLVM_MAJOR}). Install LLVM \
             {PREFERRED_LLVM_MAJOR}, or set the tool override to a newer one.",
            path.display(),
        ));
    }
    WARNED.get_or_init(|| {
        eprintln!(
            "note: building with LLVM {major} ({}), not the LLVM \
             {PREFERRED_LLVM_MAJOR} this toolchain is built against. The program is \
             the same; a timing or a codegen difference measured against a \
             build made with LLVM {PREFERRED_LLVM_MAJOR} is not comparable.",
            path.display(),
        );
    });
    Ok(())
}

#[cfg(test)]
mod llvm_version_tests {
    use super::parse_llvm_major;

    /// The two banners the tools this backend shells out to actually
    /// print: `opt` and `llc` name LLVM, a vendor clang names itself.
    #[test]
    fn a_major_is_read_from_either_banner_spelling() {
        assert_eq!(
            parse_llvm_major("Ubuntu LLVM version 22.1.2\n  Optimized build.\n"),
            Some(22)
        );
        assert_eq!(
            parse_llvm_major("Ubuntu clang version 22.1.8 (++20260714)\nTarget: x86_64\n"),
            Some(22)
        );
    }

    #[test]
    fn a_banner_with_no_version_answers_nothing() {
        assert_eq!(parse_llvm_major("some other tool\n"), None);
        assert_eq!(parse_llvm_major(""), None);
    }

    /// A tool that answers nothing recognisable is left alone rather
    /// than refused: a wrapper script that prints its own banner is
    /// still the tool the caller chose.
    #[test]
    fn an_unparsed_banner_does_not_look_like_a_mismatch() {
        assert!(parse_llvm_major("clang wrapper\n").is_none());
    }
}

pub(super) fn find_opt() -> Result<PathBuf> {
    static OPT_PATH: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    OPT_PATH
        .get_or_init(|| find_llvm_tool("opt", "GOS_LLVM_OPT", OPT_CANDIDATES))
        .clone()
        .map_err(anyhow::Error::msg)
}

pub(super) fn find_llc() -> Result<PathBuf> {
    static LLC_PATH: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    LLC_PATH
        .get_or_init(|| find_llvm_tool("llc", "GOS_LLC", LLC_CANDIDATES))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn find_clang() -> Result<PathBuf> {
    static CLANG_PATH: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    CLANG_PATH
        .get_or_init(|| {
            if let Ok(path) = std::env::var("GOS_LLVM_CLANG") {
                let path = PathBuf::from(path);
                check_llvm_major("clang", &path)?;
                return Ok(path);
            }
            // Prefer the Clang beside the selected `opt`; this avoids mixing
            // LLVM major versions when several installations are present.
            if let Ok(opt) = find_opt()
                && let Some(dir) = opt.parent()
            {
                for name in if cfg!(windows) {
                    &["clang.exe"][..]
                } else {
                    &[
                        "clang", "clang-22", "clang-21", "clang-20", "clang-19", "clang-18",
                    ][..]
                } {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        check_llvm_major("clang", &candidate)?;
                        return Ok(candidate);
                    }
                }
            }
            find_llvm_tool("clang", "GOS_LLVM_CLANG", CLANG_CANDIDATES)
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Selects the single-process IR-to-object route. Apple targets deliberately
/// retain the explicit `opt -O3` then `llc -O3` pipeline: the system Apple
/// Clang driver is not a substitute for the selected LLVM toolchain and made
/// release performance indistinguishable from debug in issue #102. Debug keeps
/// the split path because its minimal `mem2reg` pass is essential for usable
/// loop code, while Clang `-O0` leaves the emitted alloca-heavy IR in memory.
/// PGO also keeps explicit `opt` because its pipeline flags and profile-file
/// semantics are not interchangeable with the Clang driver's source-oriented
/// PGO switches.
pub(super) fn integrated_clang_path(triple: &str) -> Option<PathBuf> {
    if matches!(opt_profile(), OptProfile::Debug)
        || triple.contains("apple")
        || std::env::var("GOS_LLVM_SPLIT_TOOLS").is_ok()
        || pgo_mode().is_some()
        || std::env::var("GOS_PGO_COLLECT").is_ok()
        || std::env::var("GOS_PGO_PROFILE").is_ok()
    {
        return None;
    }
    find_clang().ok()
}

fn find_llvm_tool(
    tool: &str,
    env_var: &str,
    candidates: &[&str],
) -> std::result::Result<PathBuf, String> {
    // The override says which tool to use, not that its version stops
    // mattering: a major below the floor cannot compile this emitter's
    // IR whoever chose it.
    let found = if let Ok(path) = std::env::var(env_var) {
        PathBuf::from(path)
    } else if let Some(bundled) = bundled_llvm_tool(tool) {
        bundled
    } else {
        candidates
            .iter()
            .find(|candidate| is_executable(candidate))
            .map(PathBuf::from)
            .ok_or_else(|| missing_llvm_tool_message(tool, env_var))?
    };
    check_llvm_major(tool, &found)?;
    Ok(found)
}

/// Cross-platform candidate list for the LLVM `opt` driver. Order
/// matters: PATH-resolvable bare names first (cheap), then well-known
/// system locations on Linux (apt), macOS (Homebrew, both Apple Silicon
/// and Intel prefixes), and Windows (MSYS2 mingw - which is the only
/// commonly-installed source that actually ships `opt.exe` / `llc.exe`
/// on Windows, since the upstream LLVM installer ships only the clang
/// front-end).
///
/// Version-suffixed entries lead with [`PREFERRED_LLVM_MAJOR`], the
/// major `rustc` bundles, so a host that has it uses it, and descend to
/// [`MINIMUM_LLVM_MAJOR`]. Which one gets picked used to depend on the
/// order a list happened to be written in - the previous ordering put
/// 18 ahead of 20, so a host with both silently built with the older -
/// and [`check_llvm_major`] now says which one answered. The bare name
/// comes after the suffixed ones because it is whichever major the
/// distro's `llvm` metapackage last pointed at.
const OPT_CANDIDATES: &[&str] = &[
    // PATH lookups
    "opt-22",
    "opt-21",
    "opt-20",
    "opt-19",
    "opt-18",
    "opt",
    // Linux (apt-installed)
    "/usr/lib/llvm-22/bin/opt",
    "/usr/lib/llvm-21/bin/opt",
    "/usr/lib/llvm-20/bin/opt",
    "/usr/lib/llvm-19/bin/opt",
    "/usr/lib/llvm-18/bin/opt",
    // macOS Homebrew (Apple Silicon)
    "/opt/homebrew/opt/llvm@22/bin/opt",
    "/opt/homebrew/opt/llvm@21/bin/opt",
    "/opt/homebrew/opt/llvm@20/bin/opt",
    "/opt/homebrew/opt/llvm@19/bin/opt",
    "/opt/homebrew/opt/llvm@18/bin/opt",
    "/opt/homebrew/opt/llvm/bin/opt",
    "/opt/homebrew/bin/opt",
    // macOS Homebrew (Intel)
    "/usr/local/opt/llvm@22/bin/opt",
    "/usr/local/opt/llvm@21/bin/opt",
    "/usr/local/opt/llvm@20/bin/opt",
    "/usr/local/opt/llvm@19/bin/opt",
    "/usr/local/opt/llvm@18/bin/opt",
    "/usr/local/opt/llvm/bin/opt",
    "/usr/local/bin/opt",
    // Windows (MSYS2 mingw - full LLVM via `pacman -S
    // mingw-w64-x86_64-llvm`; also `mingw-w64-clang-x86_64-llvm`
    // under `clang64/`).
    "C:\\msys64\\mingw64\\bin\\opt.exe",
    "C:\\msys64\\clang64\\bin\\opt.exe",
    "C:\\msys64\\ucrt64\\bin\\opt.exe",
    // Windows (LLVM upstream installer - usually clang-only,
    // but a custom-built distribution may include opt; kept as a
    // last-resort path).
    "C:\\Program Files\\LLVM\\bin\\opt.exe",
    "C:\\Program Files (x86)\\LLVM\\bin\\opt.exe",
];

/// Parallel candidate list for `llc`. See [`OPT_CANDIDATES`] for the
/// ordering rationale; the entries mirror it directly.
const LLC_CANDIDATES: &[&str] = &[
    "llc-22",
    "llc-21",
    "llc-20",
    "llc-19",
    "llc-18",
    "llc",
    "/usr/lib/llvm-22/bin/llc",
    "/usr/lib/llvm-21/bin/llc",
    "/usr/lib/llvm-20/bin/llc",
    "/usr/lib/llvm-19/bin/llc",
    "/usr/lib/llvm-18/bin/llc",
    "/opt/homebrew/opt/llvm@22/bin/llc",
    "/opt/homebrew/opt/llvm@21/bin/llc",
    "/opt/homebrew/opt/llvm@20/bin/llc",
    "/opt/homebrew/opt/llvm@19/bin/llc",
    "/opt/homebrew/opt/llvm@18/bin/llc",
    "/opt/homebrew/opt/llvm/bin/llc",
    "/opt/homebrew/bin/llc",
    "/usr/local/opt/llvm@22/bin/llc",
    "/usr/local/opt/llvm@21/bin/llc",
    "/usr/local/opt/llvm@20/bin/llc",
    "/usr/local/opt/llvm@19/bin/llc",
    "/usr/local/opt/llvm@18/bin/llc",
    "/usr/local/opt/llvm/bin/llc",
    "/usr/local/bin/llc",
    "C:\\msys64\\mingw64\\bin\\llc.exe",
    "C:\\msys64\\clang64\\bin\\llc.exe",
    "C:\\msys64\\ucrt64\\bin\\llc.exe",
    "C:\\Program Files\\LLVM\\bin\\llc.exe",
    "C:\\Program Files (x86)\\LLVM\\bin\\llc.exe",
];

/// Clang candidates used for the integrated LLVM IR-to-object pipeline.
/// Ordered like [`OPT_CANDIDATES`].
const CLANG_CANDIDATES: &[&str] = &[
    "clang-22",
    "clang-21",
    "clang-20",
    "clang-19",
    "clang-18",
    "clang",
    "/usr/lib/llvm-22/bin/clang",
    "/usr/lib/llvm-21/bin/clang",
    "/usr/lib/llvm-20/bin/clang",
    "/usr/lib/llvm-19/bin/clang",
    "/usr/lib/llvm-18/bin/clang",
    "/opt/homebrew/opt/llvm@22/bin/clang",
    "/opt/homebrew/opt/llvm@21/bin/clang",
    "/opt/homebrew/opt/llvm@20/bin/clang",
    "/opt/homebrew/opt/llvm@19/bin/clang",
    "/opt/homebrew/opt/llvm@18/bin/clang",
    "/opt/homebrew/opt/llvm/bin/clang",
    "/usr/local/opt/llvm@22/bin/clang",
    "/usr/local/opt/llvm@21/bin/clang",
    "/usr/local/opt/llvm@20/bin/clang",
    "/usr/local/opt/llvm@19/bin/clang",
    "/usr/local/opt/llvm@18/bin/clang",
    "/usr/local/opt/llvm/bin/clang",
    "C:\\msys64\\mingw64\\bin\\clang.exe",
    "C:\\msys64\\clang64\\bin\\clang.exe",
    "C:\\msys64\\ucrt64\\bin\\clang.exe",
    "C:\\Program Files\\LLVM\\bin\\clang.exe",
    "C:\\Program Files (x86)\\LLVM\\bin\\clang.exe",
];

/// Directories a release archive places its own LLVM tools in, relative to
/// the directory holding `gos`: beside it, or under an install prefix's
/// `lib/gossamer`. A bundled tool is the version the toolchain was built
/// against, so it is preferred over whatever the host has installed.
const BUNDLED_LLVM_DIRS: &[&str] = &["llvm/bin", "../lib/gossamer/llvm/bin"];

/// `tool` from an LLVM bundled with this `gos`, when one is.
fn bundled_llvm_tool(tool: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let file = if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    };
    BUNDLED_LLVM_DIRS
        .iter()
        .map(|dir| exe_dir.join(dir).join(&file))
        .find(|candidate| candidate.is_file())
}

/// One LLVM tool as a native build would resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlvmToolStatus {
    /// The tool's name: `opt`, `llc`, or `clang`.
    pub tool: &'static str,
    /// The resolved path and its reported LLVM major, or the reason the
    /// tool cannot be used.
    pub resolved: std::result::Result<(PathBuf, Option<u32>), String>,
}

/// How `gos build` would resolve each LLVM tool on this host, with the
/// version each reports. Nothing is compiled.
#[must_use]
pub fn llvm_toolchain_status() -> Vec<LlvmToolStatus> {
    let resolve = |found: Result<PathBuf>| {
        found
            .map(|path| {
                let major = llvm_tool_major(&path);
                (path, major)
            })
            .map_err(|e| e.to_string())
    };
    vec![
        LlvmToolStatus {
            tool: "opt",
            resolved: resolve(find_opt()),
        },
        LlvmToolStatus {
            tool: "llc",
            resolved: resolve(find_llc()),
        },
        LlvmToolStatus {
            tool: "clang",
            resolved: resolve(find_clang()),
        },
    ]
}

fn missing_llvm_tool_message(tool: &str, env_var: &str) -> String {
    format!(
        "{tool} (LLVM toolchain) not found. Install LLVM {MINIMUM_LLVM_MAJOR} or \
         newer and retry ({PREFERRED_LLVM_MAJOR} is what this toolchain is built \
         against):\n  \
         Linux:   apt install llvm-{MINIMUM_LLVM_MAJOR}-dev clang-{MINIMUM_LLVM_MAJOR}\n           \
                  (for LLVM {PREFERRED_LLVM_MAJOR}, which most distros do not\n           \
                  package yet, add the apt.llvm.org repository)\n  \
         macOS:   brew install llvm@{PREFERRED_LLVM_MAJOR}\n  \
         Windows: pacman -S mingw-w64-x86_64-llvm       (from MSYS2; the upstream LLVM\n           \
                                                         Windows installer ships clang\n           \
                                                         but not `opt`/`llc`)\n\
         Or set `{env_var}` to the absolute path of `{tool}`."
    )
}

fn is_executable(path: &str) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        return meta.is_file();
    }
    // Bare name (no path separator)? Walk `PATH` looking for it.
    // Use `std::env::split_paths` so the separator is correct on
    // every platform (`:` on Unix, `;` on Windows), and try the
    // `.exe` suffix on Windows when the caller passed a bare stem.
    let has_separator = path.contains('/') || path.contains('\\');
    if has_separator {
        return false;
    }
    let Ok(paths) = std::env::var("PATH") else {
        return false;
    };
    let suffixes: &[&str] = if cfg!(windows) && !path.to_ascii_lowercase().ends_with(".exe") {
        &["", ".exe"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&paths) {
        for suffix in suffixes {
            let candidate = dir.join(format!("{path}{suffix}"));
            if std::fs::metadata(&candidate).is_ok_and(|m| m.is_file()) {
                return true;
            }
        }
    }
    false
}

pub(super) fn host_triple() -> String {
    // An explicit `--target` (via `set_target_triple`) wins over
    // everything so a cross `gos build` drives opt/llc, the i128 ABI
    // marshalling, and the object-cache key at the requested triple.
    if let Some(triple) = TARGET_TRIPLE_OVERRIDE.get() {
        return triple.clone();
    }
    detect_host_triple()
}

/// The host's own LLVM target triple, ignoring any cross-compile
/// override. `llc` uses this to pick the object-file format (ELF on
/// Linux, Mach-O on Darwin, COFF on Windows); getting the OS portion
/// wrong produces an object the host's `ld` rejects as "unknown file
/// type". `TARGET` (set by cargo build scripts) takes precedence so a
/// build-script-driven cross still works; otherwise arch + OS come
/// from `std::env::consts` (cross-platform, no subprocess).
fn detect_host_triple() -> String {
    if let Ok(triple) = std::env::var("TARGET") {
        return triple;
    }
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        "freebsd" => "unknown-freebsd",
        "ios" => "apple-ios",
        // Conservative default - Linux is the dev host. Any
        // unrecognised target will produce a clear `llc` error
        // rather than a silently mis-formatted object.
        _ => "unknown-linux-gnu",
    };
    format!("{arch}-{os}")
}

/// Whether the build target's LLVM backend implements `preserve_mostcc`, the
/// convention under which a call leaves the caller's general-purpose
/// registers intact.
pub(crate) fn target_has_preserve_most() -> bool {
    matches!(
        target_arch_from_triple(&llvm_target_triple_for(&host_triple())),
        "x86_64" | "aarch64"
    )
}

/// True when the build target is `x86_64-pc-windows-*`, driving the
/// Win64 i128 (Fat-aggregate) calling-convention adjustments at the
/// `gos_rt_*` C-ABI boundary. Derived from the resolved target triple
/// ([`host_triple`], which honours `TARGET`) rather than `cfg!(windows)`
/// so a Linux-hosted cross-build to a Windows triple emits the Win64
/// marshalling instead of the host's SysV shape - the two disagree on
/// how `extern "C"` returns/passes a 2-word `i128` (xmm `<16 x i8>` vs
/// a GP-register pair), and keying off the host silently miscompiled
/// every cross-target build.
pub(crate) fn target_is_windows() -> bool {
    host_triple().contains("windows")
}

/// The C calling convention by-value structs follow on the target being
/// built, or `None` for a target with no lowering for them.
pub(crate) fn target_c_abi() -> Option<gossamer_abi::c_aggregate::CAbi> {
    let triple = host_triple();
    let os = if triple.contains("windows") {
        "windows"
    } else {
        "unix"
    };
    gossamer_abi::c_aggregate::CAbi::for_target(target_arch_from_triple(&triple), os)
}

#[cfg(test)]
mod host_triple_tests {
    use super::{
        detect_host_triple, integrated_clang_path, llvm_target_triple_for_with_deployment,
        mcpu_for, normalized_macos_deployment_target, target_arch_from_triple,
    };
    use crate::emit::module_datalayout;
    use crate::emit::settings::disable_loop_idiom_for_target_with_static_musl;

    /// `llc` selects the object-file format from the OS portion of
    /// the triple - ELF on a Linux triple, Mach-O on `apple-darwin`,
    /// COFF on `pc-windows-msvc`. If `host_triple` hardcoded
    /// `unknown-linux-gnu` on every host (as it used to), macOS /
    /// Windows builds linked with `ld: unknown file type` because
    /// the object format was wrong. This regression pins the OS
    /// portion of the triple to the running host so any future
    /// drift fails at unit-test time rather than at `gos build`
    /// time.
    ///
    /// Cargo sets `TARGET` for build scripts, not for normal test
    /// binaries - in `cargo test` runs the env var is unset and
    /// the function exercises its OS-detection branch, which is
    /// exactly what we want to cover here.
    #[test]
    fn host_triple_matches_running_os() {
        if std::env::var("TARGET").is_ok() {
            // Cross-compilation override is active - the function
            // is just echoing back `TARGET` and the host-detection
            // branch isn't covered. Skip rather than assert a
            // mismatch we can't control.
            return;
        }
        // Call the detection helper directly, not `host_triple`: the
        // latter consults the process-wide target override, which a
        // sibling test may have set, and would pollute this assertion.
        let triple = detect_host_triple();
        let expected_os_part = match std::env::consts::OS {
            "linux" => "unknown-linux-gnu",
            "macos" => "apple-darwin",
            "windows" => "pc-windows-msvc",
            "freebsd" => "unknown-freebsd",
            "ios" => "apple-ios",
            _ => "unknown-linux-gnu",
        };
        assert!(
            triple.ends_with(expected_os_part),
            "host_triple {triple:?} does not end with {expected_os_part:?} \
             for OS {os:?}; llc would emit the wrong object format and the \
             system linker would reject it",
            os = std::env::consts::OS,
        );
        assert!(
            triple.starts_with(std::env::consts::ARCH),
            "host_triple {triple:?} does not start with arch {arch:?}",
            arch = std::env::consts::ARCH,
        );
    }

    #[test]
    fn mcpu_cross_target_is_portable_not_host() {
        // A cross build to aarch64 must not inherit an x86 -mcpu, and
        // must never use `native` (the host CPU). Tested through the
        // pure helper so it needs no process-wide override.
        assert_eq!(
            mcpu_for("aarch64-unknown-linux-gnu", true, false),
            "generic"
        );
        assert_eq!(
            mcpu_for("aarch64-unknown-linux-musl", true, false),
            "generic"
        );
        assert_eq!(
            mcpu_for("x86_64-unknown-linux-gnu", true, false),
            "x86-64-v3"
        );
    }

    #[test]
    fn mcpu_native_host_uses_host_cpu_unless_reproducible() {
        assert_eq!(mcpu_for("x86_64-unknown-linux-gnu", false, false), "native");
        assert_eq!(
            mcpu_for("x86_64-unknown-linux-gnu", false, true),
            "x86-64-v3"
        );
    }

    #[test]
    fn prefer_256_bit_only_for_x86_target() {
        // The AVX-512 width cap is x86-only; an aarch64 target (cross
        // from any host) must not receive it.
        assert_eq!(
            target_arch_from_triple("aarch64-unknown-linux-musl"),
            "aarch64"
        );
        assert_eq!(
            target_arch_from_triple("x86_64-unknown-linux-gnu"),
            "x86_64"
        );
    }

    #[test]
    fn loop_idiom_workaround_is_scoped_to_static_musl_without_runtime_mem_routines() {
        assert!(disable_loop_idiom_for_target_with_static_musl(
            true,
            "aarch64-unknown-linux-gnu"
        ));
        assert!(disable_loop_idiom_for_target_with_static_musl(
            false,
            "aarch64-unknown-linux-musl"
        ));
        for triple in ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"] {
            assert!(
                !disable_loop_idiom_for_target_with_static_musl(true, triple),
                "{triple} links the runtime's memory routines and keeps loop idiom recognition"
            );
        }
        for triple in [
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-macosx15.0.0",
            "x86_64-pc-windows-msvc",
        ] {
            assert!(
                !disable_loop_idiom_for_target_with_static_musl(false, triple),
                "{triple} must keep LLVM loop idiom recognition"
            );
        }
    }

    #[test]
    fn apple_targets_keep_the_explicit_llvm_release_pipeline() {
        assert!(integrated_clang_path("aarch64-apple-macosx15.0.0").is_none());
    }

    #[test]
    fn darwin_llvm_triple_pins_deployment_target() {
        assert_eq!(
            llvm_target_triple_for_with_deployment("aarch64-apple-darwin", None),
            "aarch64-apple-macosx15.0.0"
        );
        assert_eq!(
            llvm_target_triple_for_with_deployment("x86_64-apple-darwin", Some("14.2")),
            "x86_64-apple-macosx14.2.0"
        );
        assert_eq!(
            llvm_target_triple_for_with_deployment("aarch64-unknown-linux-gnu", Some("14.2")),
            "aarch64-unknown-linux-gnu"
        );
    }

    #[test]
    fn macos_deployment_target_normalizes_for_llvm() {
        assert_eq!(normalized_macos_deployment_target(None), "15.0.0");
        assert_eq!(normalized_macos_deployment_target(Some("15")), "15.0.0");
        assert_eq!(normalized_macos_deployment_target(Some("15.1")), "15.1.0");
        assert_eq!(normalized_macos_deployment_target(Some("15.1.2")), "15.1.2");
    }

    #[test]
    fn datalayout_keeps_i128_at_flat_slot_alignment() {
        let x86 = module_datalayout("x86_64-unknown-linux-gnu").expect("x86_64 layout");
        assert!(x86.contains("i128:64"), "{x86}");
        let arm = module_datalayout("aarch64-apple-macosx15.0.0").expect("aarch64 layout");
        assert!(arm.contains("m:o"), "{arm}");
        assert!(arm.contains("i128:64"), "{arm}");
        assert!(arm.contains("n32:64"), "{arm}");
    }
}
