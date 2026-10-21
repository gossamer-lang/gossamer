//! Comptime fold pass.
//!
//! `comptime { ... }` blocks and `comptime fn` calls are evaluated on
//! the bytecode VM during compilation and spliced back into the source
//! as literals. Running once here, ahead of the per-tier pipelines,
//! guarantees the bytecode VM, the Cranelift JIT, and the LLVM AOT
//! backend all compile the identical constant - comptime never reaches
//! a backend.
//!
//! The evaluation-and-splice core is `gossamer_interp::fold_into_source`
//! (shared with the wasm playground); this wrapper runs the front-end
//! gate over the already-augmented source and hands the lowered program
//! to the core. Programs with no `comptime` spelling skip the whole
//! pass.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use gossamer_runtime::comptime_policy::{self, ComptimeIo};

/// The level `--comptime-io` asked for, or `None` when the flag was
/// not given. Written once by the argument parser before any command
/// runs.
static COMMAND_LINE_LEVEL: OnceLock<Option<ComptimeIo>> = OnceLock::new();

/// Records what `--comptime-io` asked for.
pub(crate) fn set_command_line_level(level: Option<ComptimeIo>) {
    let _ = COMMAND_LINE_LEVEL.set(level);
}

/// Resolves the effective compile-time capability level for a fold of
/// `file_label` and installs it.
///
/// The manifest may tighten the command line's posture and may never
/// loosen it, because the manifest is written by the party the policy
/// defends against.
fn install_level(file_label: &str) -> ComptimeIo {
    let from_manifest = crate::paths::project_context_for_entry(Path::new(file_label))
        .manifest_result()
        .and_then(Result::ok)
        .and_then(|manifest| manifest.project.comptime_io.clone())
        .and_then(|text| ComptimeIo::parse(&text));
    let level =
        comptime_policy::resolve(COMMAND_LINE_LEVEL.get().copied().flatten(), from_manifest);
    comptime_policy::set_level(level);
    level
}

/// Evaluates every comptime region in `augmented` (autoderive-augmented
/// source) and returns the source with each region replaced by its
/// result literal. Returns `augmented` unchanged when it contains no
/// `comptime` spelling, or when the front-end gate rejects the program
/// (the caller's real pass re-runs the gate and reports those errors).
/// Returns `Err` when a comptime region is not compile-time-known or
/// does not evaluate to a scalar or string.
///
/// `origins` names the file each `(start, end)` range of an assembled unit
/// was written in, so a region in a bundled dependency reads relative to it.
pub(crate) fn fold_comptime(
    augmented: String,
    file_label: &str,
    origins: Vec<(u32, u32, String)>,
) -> Result<String> {
    if !gossamer_lex::mentions_comptime(&augmented) {
        return Ok(augmented);
    }
    install_level(file_label);

    // Comptime evaluation runs the bytecode VM during compilation, whose
    // native dispatch and in-process JIT grow the real machine stack just
    // as `gos` does. Run it on a VM-sized stack so the host
    // main-thread stack (roughly 1 MiB on Windows) never bounds comptime
    // recursion: the `build` and `check` paths reach here on the main
    // thread, unlike `run` / `test`, which already execute inside
    // `with_vm_stack`.
    let file_label = file_label.to_string();
    crate::cmd::with_vm_stack(move || fold_comptime_on_vm(augmented, file_label, origins))
}

/// Evaluates the comptime regions of `augmented` on the current thread,
/// returning the spliced source. Always invoked through
/// [`crate::cmd::with_vm_stack`] so the bytecode VM has a generous native
/// stack regardless of which command (`build` / `check` / `run` / `test`)
/// reached the fold.
fn fold_comptime_on_vm(
    augmented: String,
    file_label: String,
    origins: Vec<(u32, u32, String)>,
) -> Result<String> {
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file_label.clone(), augmented);
    let outcome = gossamer_driver::check_frontend(map.source(file_id), file_id);
    if !outcome.is_ok() {
        // Other front-end errors exist; let the caller's authoritative
        // gate render them rather than masking them behind a comptime
        // failure.
        return Ok(map.into_source(file_id));
    }

    let gossamer_driver::CheckedFrontend {
        sf,
        resolutions,
        table,
        mut tcx,
    } = outcome.checked;
    let program = gossamer_hir::lower_source_file(&sf, &resolutions, &table, &mut tcx);

    gossamer_interp::fold_into_source_anchored(
        &program,
        tcx,
        map.source(file_id),
        &file_label,
        &file_label,
        origins,
    )
    .map_err(|message| anyhow!(message))
}
