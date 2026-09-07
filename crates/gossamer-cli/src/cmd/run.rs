//! `gos run [PATH]` - execute a program through the bytecode VM.
//!
//! The register-based bytecode VM is the only execution engine for
//! `gos run`. The tree-walker interpreter was removed in 0.14.0 (the
//! `--tree-walker` flag in 0.5.0); no CLI / env-var path selects an
//! engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::loaders::{load_and_check, profile_rss_stage};
use crate::paths::resolve_entry_arg;

/// Runs a source file through the bytecode VM.
pub(crate) fn dispatch(path: Option<PathBuf>, main_thread: bool, args: &[String]) -> Result<()> {
    let resolved = resolve_entry_arg(path)?;
    run(&resolved, main_thread, args)
}

fn run(file: &Path, main_thread: bool, forwarded: &[String]) -> Result<()> {
    let file = file.to_path_buf();
    let forwarded = forwarded.to_vec();
    if main_thread {
        // Execute directly on the process main thread so native
        // libraries that require it (GLFW / OpenGL / Cocoa / Metal) work
        // from `[rust-bindings]`. Trades the large spawned-thread stack
        // for the OS-default main-thread stack (see `cmd::on_main_thread`).
        crate::cmd::on_main_thread(move || run_on_vm(&file, &forwarded))
    } else {
        // Execute on a thread with a large native stack so the host's
        // default main-thread stack size never bounds recursion depth or
        // the in-process JIT compile pass (see `cmd::with_vm_stack`).
        crate::cmd::with_vm_stack(move || run_on_vm(&file, &forwarded))
    }
}

fn run_on_vm(file: &Path, forwarded: &[String]) -> Result<()> {
    let file_label = file.to_string_lossy();
    let unit = crate::paths::read_entry_unit(file)?;
    run_source_on_vm(
        &unit.source,
        &file_label,
        forwarded,
        Some((unit.entry.as_path(), unit.origins.as_slice())),
    )
}

/// Executes inline source passed through `gos -e` or `gos --eval`.
pub(crate) fn command(source: String) -> Result<()> {
    crate::cmd::with_vm_stack(move || run_source_on_vm(&source, "<command>", &[], None))
}

fn run_source_on_vm(
    user_source: &str,
    file_label: &str,
    forwarded: &[String],
    origins: Option<(&Path, &[gossamer_pkg::bundle::BundledSpan])>,
) -> Result<()> {
    // Compile-time codegen pass: synthesize `from_json` / `to_json`
    // for every user struct so the resulting program has real
    // methods (no VM-only intercept).
    let source = gossamer_parse::autoderive::augment_source(user_source);
    // Comptime fold: evaluate `comptime { ... }` / `comptime fn` calls
    // now and splice their result literals in, so the VM compiles a
    // constant identical to what the compiled tiers see.
    let source = crate::comptime_fold::fold_comptime(source, file_label)?;
    profile_rss_stage("source_prepared");
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file_label, source);
    if let Some((entry, spans)) = origins {
        crate::paths::register_unit_origins(&mut map, file_id, entry, spans);
    }
    // Static checks always run first. A program with parse / resolve /
    // type errors has no business reaching the VM - execution would
    // either crash or produce unsound output.
    let (program, tcx) = load_and_check(map.source(file_id), file_id, &map)?;
    gossamer_interp::set_program_name(file_label);
    gossamer_interp::set_program_args(forwarded);
    gossamer_interp::reset_lazy_iterator_state();
    let mut vm = gossamer_interp::Vm::new();
    // The bytecode compiler resolves expression spans into compact chunk-local
    // locations during load. The full source map can then be released before
    // execution, preserving the old frontend/runtime peak-memory boundary.
    vm.set_source_map(Arc::new(map));
    // Running a program enters it at `main` and nowhere else, so the
    // promotion snapshot covers what `main` reaches and leaves the rest.
    vm.set_entry_points(&["main".to_string()]);
    profile_rss_stage("vm_created");
    // `load` consumes `tcx` (moves the interner into the JIT snapshot).
    vm.load(&program, tcx, true)
        .map_err(|err| anyhow!("vm load failed: {err}"))?;
    vm.clear_source_map();
    profile_rss_stage("frontend_released");
    profile_rss_stage("vm_loaded");
    drop(program);
    // `main` runs inside the root cohort, so every `spawn` has a cohort
    // to belong to: no goroutine outlives the program, and a child's
    // failure that nothing reads is reported instead of vanishing.
    gossamer_interp::open_root_cohort();
    // A fault raised inside a JIT-compiled body exits through the runtime,
    // which reports the host's frames through this window.
    let r = vm.with_active_trace(|| vm.call("main", Vec::new()));
    profile_rss_stage("execution_complete");
    vm.release_jit_prelude();
    gossamer_interp::close_root_cohort();
    gossamer_interp::join_outstanding_goroutines();
    gossamer_interp::flush_runtime_stdout();
    match r {
        Ok(val) => {
            // An entry point returning `Err(e)` - an explicit `fn main() ->
            // Result<..>`, or the implicit `?`-desugared top-level main -
            // reports the error's Display (the colon-joined cause chain) to
            // stderr and exits nonzero, matching the native tier's
            // `gos_rt_main_exit_code_err`. Previously the return value was
            // discarded, so the error was silent and the exit code was 0.
            if let gossamer_interp::Value::Variant(inner) = &val
                && inner.name == "Err"
            {
                if let Some(payload) = inner.fields.first() {
                    eprintln!("{payload}");
                }
                std::process::exit(1);
            }
            Ok(())
        }
        Err(err) => {
            let trace = crate::cmd::traceback::render_call_stack(&vm.call_stack_frames());
            // A stack overflow is a fault, not a failed command: the
            // compiled tiers raise it through the same path as a panic and
            // exit 101, so the VM does too rather than reporting it as a
            // non-zero command result.
            if gossamer_interp::is_panic_error(&err) || gossamer_interp::is_stack_overflow(&err) {
                // A user hook replaces the default report; either way a
                // main-goroutine panic exits with the pinned code 101
                // (Rust parity - scripts depend on it).
                let hooked = vm.invoke_panic_hook(&gossamer_interp::panic_message(&err));
                if !hooked {
                    // Everything the program printed belongs ahead of the
                    // report, and the report itself opens with the code, the
                    // way the compiled tiers write it.
                    gossamer_interp::flush_runtime_stdout();
                    eprintln!("{err}{trace}");
                }
                gossamer_interp::flush_runtime_stdout();
                std::process::exit(101);
            }
            Err(anyhow!("runtime error: {err}{trace}"))
        }
    }
}
