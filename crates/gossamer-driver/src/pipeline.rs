//! End-to-end pipeline: source → AST → resolution → types → HIR →
//! MIR → Cranelift text → linked artifact.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use anyhow::anyhow;
use gossamer_ast::SourceFile;
use gossamer_codegen_cranelift::{NativeObject, compile_to_object, emit_module};
use gossamer_hir::{lift_closures, lower_source_file};
use gossamer_lex::{FileId, SourceMap};
use gossamer_mir::{
    Body, check_generic_layouts, inline_general, inline_small_callees, inline_trivial_wrappers,
    lower_program, optimise, optimise_debug,
};
use gossamer_resolve::Resolutions;
use gossamer_types::{TyCtxt, TypeTable};

use crate::link::{Artifact, LinkerOptions, TranslationUnit, link};

/// Compiles a single source buffer into a linked [`Artifact`].
#[must_use]
pub fn compile_source(source: &str, unit_name: &str, options: &LinkerOptions) -> Artifact {
    let bodies = lower_to_mir(source, unit_name);
    let module = emit_module(&bodies);
    let unit = TranslationUnit {
        name: unit_name.to_string(),
        module,
    };
    link(&[unit], options)
}

/// Pre-parsed, resolved, and typechecked program bundled for reuse.
/// Created once by `gos build`'s validation pass and consumed by
/// the codegen path, avoiding a redundant frontend round-trip.
pub struct CheckedFrontend {
    /// Parsed AST.
    pub sf: SourceFile,
    /// Name-resolution map.
    pub resolutions: Resolutions,
    /// Type-inference result table.
    pub table: TypeTable,
    /// Accumulated type context (mutated during lowering).
    pub tcx: TyCtxt,
}

/// Compiles `source` into a native object file suitable for linking
/// with `cc`. Returns `Err` only on lower-level failures (generic-ABI
/// enforcement, cranelift module emission); the MIR lowerer itself
/// covers every HIR shape, so user-visible compiler gaps no longer
/// short-circuit through this path.
pub fn compile_source_native(source: &str, unit_name: &str) -> anyhow::Result<NativeObject> {
    let (bodies, tcx) = lower_to_mir_with_tcx(source, unit_name, MirProfile::Debug);
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    compile_to_object(&bodies, &tcx)
}

/// Like [`compile_source_native`] but uses already-computed frontend
/// artifacts, skipping a second parse/resolve/typecheck round-trip.
pub fn compile_source_native_from_frontend(
    checked: CheckedFrontend,
) -> anyhow::Result<NativeObject> {
    let (bodies, tcx) = lower_to_mir_from_frontend(checked, MirProfile::Debug);
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    compile_to_object(&bodies, &tcx)
}

/// Path-oriented Cranelift debug build. Same as
/// [`compile_source_native_from_frontend`] but writes the produced
/// object directly to `obj_out` and returns only the triple, so
/// the caller never holds the full object bytes in memory.
pub fn compile_source_native_from_frontend_at_path(
    checked: CheckedFrontend,
    obj_out: &std::path::Path,
) -> anyhow::Result<String> {
    let (bodies, tcx) = lower_to_mir_from_frontend(checked, MirProfile::Debug);
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    gossamer_codegen_cranelift::compile_to_object_at_path(&bodies, &tcx, obj_out)
}

/// `--release` build path: lower through the LLVM backend
/// (text IR + `llc -O3`) for release-quality optimisation.
/// Backend invariant failures are reported as compiler bugs before
/// LLVM tool invocation; `gos build` no longer falls back to another
/// native backend for accepted MIR.
pub fn compile_source_native_release(
    source: &str,
    unit_name: &str,
) -> anyhow::Result<NativeObject> {
    let (bodies, tcx) = lower_to_mir_with_tcx(source, unit_name, MirProfile::Release);
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    let llvm_obj = gossamer_codegen_llvm::compile_to_object(&bodies, &tcx)?;
    Ok(NativeObject {
        triple: llvm_obj.triple,
        bytes: llvm_obj.bytes,
    })
}

/// Result of a native LLVM release build.
#[derive(Debug, Clone)]
pub struct ReleaseBuild {
    /// Object emitted by the LLVM backend.
    pub llvm: NativeObject,
    /// Always `None`; retained for API compatibility with older callers.
    pub cranelift: Option<NativeObject>,
    /// Always empty; retained for API compatibility with older callers.
    pub fallback_bodies: Vec<String>,
}

/// Like [`compile_source_native_release_with_fallback`] but uses
/// already-computed frontend artifacts.
pub fn compile_source_native_release_with_fallback_from_frontend(
    checked: CheckedFrontend,
) -> anyhow::Result<ReleaseBuild> {
    let (bodies, tcx) = lower_to_mir_from_frontend(checked, MirProfile::Release);
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    let outcome = gossamer_codegen_llvm::compile_with_fallback(&bodies, &tcx)?;
    debug_assert!(outcome.fallback_bodies.is_empty());
    Ok(ReleaseBuild {
        llvm: NativeObject {
            triple: outcome.object.triple,
            bytes: outcome.object.bytes,
        },
        cranelift: None,
        fallback_bodies: Vec::new(),
    })
}

/// Result of a path-oriented native LLVM release build.
#[derive(Debug, Clone)]
pub struct ReleaseBuildPaths {
    /// Triple the LLVM backend reported for the emitted objects.
    pub triple: String,
    /// Always empty; retained for API compatibility with older callers.
    pub fallback_bodies: Vec<String>,
    /// Always false; retained for API compatibility with older callers.
    pub has_cranelift_companion: bool,
    /// Number of MIR bodies presented to native code generation.
    pub body_count: usize,
    /// Number of MIR bodies dropped as unreachable from the entry.
    pub pruned_count: usize,
    /// Number of LLVM object files emitted or restored for this build.
    pub llvm_object_count: usize,
    /// Paths to the LLVM-emitted per-body object files.
    pub llvm_objects: Vec<std::path::PathBuf>,
    /// Where the build's time between the checked front end and LLVM went.
    pub mir_phases: MirPhaseTimes,
}

/// Wall time of each step from the checked front end to the MIR handed to
/// LLVM, so a slow build names the step that holds it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirPhaseTimes {
    /// AST to HIR, closure lifting included.
    pub hir: Duration,
    /// HIR to MIR bodies.
    pub mir_lower: Duration,
    /// Reachability pruning and generic specialisation.
    pub specialise: Duration,
    /// The profile's MIR optimisation passes.
    pub passes: Duration,
    /// The backend invariant and generic-layout checks.
    pub verify: Duration,
}

/// Path-oriented variant of
/// [`compile_source_native_release_with_fallback_from_frontend`].
pub fn compile_release_at_paths_from_frontend(
    checked: CheckedFrontend,
    llvm_obj_dir: &std::path::Path,
    cl_obj_out: &std::path::Path,
) -> anyhow::Result<ReleaseBuildPaths> {
    compile_at_paths_from_frontend(checked, llvm_obj_dir, cl_obj_out, true)
}

/// Profile-aware path-oriented native build used by the CLI. Release builds
/// run the full MIR optimisation pipeline; debug builds retain only the
/// canonicalisation passes required by native code generation.
pub fn compile_at_paths_from_frontend(
    checked: CheckedFrontend,
    llvm_obj_dir: &std::path::Path,
    _cl_obj_out: &std::path::Path,
    release: bool,
) -> anyhow::Result<ReleaseBuildPaths> {
    let profile = if release {
        MirProfile::Release
    } else {
        MirProfile::Debug
    };
    let (bodies, tcx, prune, mut mir_phases) = lower_to_mir_reporting_prune(checked, profile);
    let body_count = bodies.len();
    let started = Instant::now();
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    mir_phases.verify = started.elapsed();
    let (llvm_objects, triple, fallback_bodies) =
        gossamer_codegen_llvm::compile_with_fallback_at_path(&bodies, &tcx, llvm_obj_dir)?;
    debug_assert!(fallback_bodies.is_empty());
    Ok(ReleaseBuildPaths {
        triple,
        fallback_bodies: Vec::new(),
        has_cranelift_companion: false,
        body_count,
        pruned_count: prune.pruned,
        llvm_object_count: llvm_objects.len(),
        llvm_objects,
        mir_phases,
    })
}

/// Native LLVM release build. The function name is retained for API
/// compatibility; LLVM lowering bugs are hard errors.
pub fn compile_source_native_release_with_fallback(
    source: &str,
    unit_name: &str,
) -> anyhow::Result<ReleaseBuild> {
    let (bodies, tcx) = lower_to_mir_with_tcx(source, unit_name, MirProfile::Release);
    enforce_mir_backend_invariants(&bodies, &tcx)?;
    enforce_generic_abi(&bodies, &tcx)?;
    let outcome = gossamer_codegen_llvm::compile_with_fallback(&bodies, &tcx)?;
    debug_assert!(outcome.fallback_bodies.is_empty());
    Ok(ReleaseBuild {
        llvm: NativeObject {
            triple: outcome.object.triple,
            bytes: outcome.object.bytes,
        },
        cranelift: None,
        fallback_bodies: Vec::new(),
    })
}

fn lower_to_mir(source: &str, unit_name: &str) -> Vec<Body> {
    lower_to_mir_with_tcx(source, unit_name, MirProfile::Debug).0
}

#[derive(Clone, Copy)]
enum MirProfile {
    Debug,
    Release,
}

/// The body every native build starts from. `gos build` produces an
/// executable whatever the project's shape, so nothing else is a root:
/// a library's exported surface reaches this program only through the
/// entry that calls it.
const ENTRY_BODY: &str = "main";

/// HIR + MIR lowering from pre-computed frontend artifacts.
fn lower_to_mir_from_frontend(
    checked: CheckedFrontend,
    profile: MirProfile,
) -> (Vec<Body>, TyCtxt) {
    let (bodies, tcx, _, _) = lower_to_mir_reporting_prune(checked, profile);
    (bodies, tcx)
}

/// [`lower_to_mir_from_frontend`] plus how much the reachability pass
/// dropped, which the CLI reports in its timings line.
fn lower_to_mir_reporting_prune(
    checked: CheckedFrontend,
    profile: MirProfile,
) -> (Vec<Body>, TyCtxt, gossamer_mir::PruneReport, MirPhaseTimes) {
    let CheckedFrontend {
        sf,
        resolutions,
        table,
        mut tcx,
    } = checked;
    let mut phases = MirPhaseTimes::default();
    let started = Instant::now();
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let hir = lift_closures(hir, &mut tcx);
    phases.hir = started.elapsed();
    let started = Instant::now();
    let mut bodies = lower_program(&hir, &mut tcx);
    phases.mir_lower = started.elapsed();
    let started = Instant::now();
    // Twice, for two different reasons. The first pass keeps every
    // method whatever the graph says, because specialisation is where a
    // trait call through a type parameter becomes a call to a concrete
    // impl method and that edge does not exist yet - but it does drop
    // the free functions nothing reaches, so specialisation and every
    // pass after it walk a program the size of what it actually
    // compiles. The second pass runs on the finished graph, where every
    // name is final. A native build produces an executable, so its one
    // root is the entry.
    let roots = [ENTRY_BODY.to_string()];
    let early = gossamer_mir::prune_scoped(
        &mut bodies,
        &roots,
        gossamer_mir::Scope::BeforeSpecialisation,
    );
    gossamer_mir::monomorphise(&hir, &mut bodies, &mut tcx);
    let late = gossamer_mir::prune_unreachable(&mut bodies, &roots);
    let prune = gossamer_mir::PruneReport {
        kept: late.kept,
        pruned: early.pruned + late.pruned,
    };
    phases.specialise = started.elapsed();
    let started = Instant::now();
    match profile {
        // Debug deliberately keeps calls intact and runs only the inexpensive
        // canonicalisation needed by native codegen. This is both faster to
        // compile and makes `gos build` a real contrast to `--release`.
        MirProfile::Debug => {
            for body in &mut bodies {
                optimise_debug(body, &tcx);
            }
        }
        // Whole-program inlining is a release-only transformation. Keeping
        // it here, rather than relying solely on LLVM, lets release simplify
        // language-level ownership and bounds-check shapes before IR emission.
        MirProfile::Release => {
            inline_trivial_wrappers(&mut bodies);
            inline_small_callees(&mut bodies);
            inline_general(&mut bodies);
            for body in &mut bodies {
                optimise(body, &tcx);
            }
        }
    }
    phases.passes = started.elapsed();
    (bodies, tcx, prune, phases)
}

/// Surfaces the Tier B6.3 generic-ABI check as an `anyhow::Error`
/// so the CLI's existing `Err`-render path prints a clean
/// diagnostic. Compiled paths (`compile_source_native`,
/// `compile_source_native_release`,
/// `compile_source_native_release_with_fallback`) all gate on
/// this before handing bodies to a backend.
fn enforce_generic_abi(bodies: &[Body], tcx: &TyCtxt) -> anyhow::Result<()> {
    let errors = check_generic_layouts(bodies, tcx);
    if errors.is_empty() {
        return Ok(());
    }
    Err(anyhow!(errors.join("\n")))
}

/// Hard gate before any native backend sees MIR. Debug-only verifier
/// assertions catch pass bugs during development; this production check
/// keeps malformed MIR from surfacing later as backend panics or LLVM tool
/// errors.
fn enforce_mir_backend_invariants(bodies: &[Body], tcx: &TyCtxt) -> anyhow::Result<()> {
    match gossamer_mir::verify::verify_program(bodies, tcx) {
        Ok(()) => Ok(()),
        Err(errors) => {
            if std::env::var("GOS_VERIFY_DUMP").is_ok() {
                for body in bodies {
                    if errors.iter().any(|e| format!("{e:?}").contains(&body.name)) {
                        eprintln!("body {}:", body.name);
                        eprintln!("  locals: {:?}", body.locals);
                        for (i, block) in body.blocks.iter().enumerate() {
                            eprintln!("  bb{i}:");
                            for stmt in &block.stmts {
                                eprintln!("    {:?}", stmt.kind);
                            }
                            eprintln!("    -> {:?}", block.terminator);
                        }
                    }
                }
            }
            Err(anyhow!(
                "MIR backend invariant violation:\n{}",
                errors
                    .iter()
                    .map(|err| format!("  {err:?}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        }
    }
}

/// Hands the native backend where each line of `file` begins and which file
/// each assembled region of it was read from, so a MIR span resolves to the
/// file and line a panic report names. The source map itself lives only for
/// the duration of the frontend, which is why the table is registered rather
/// than threaded through.
pub fn register_source_positions(unit_name: &str, map: &SourceMap, file: FileId) {
    let mut positions = gossamer_codegen_llvm::SourcePositions::new(unit_name, map.source(file));
    for region in map.origins(file) {
        // A region read from the unit's own entry file keeps the unit's
        // positions, as `SourceMap::origin_of` does.
        if region.origin == file {
            continue;
        }
        let path = map.file_name(region.origin);
        let name = std::path::Path::new(path).file_name().map_or_else(
            || path.to_string(),
            |base| base.to_string_lossy().into_owned(),
        );
        positions.add_region(
            region.start,
            region.end,
            region.origin_start,
            &name,
            map.source(region.origin),
        );
    }
    gossamer_codegen_llvm::set_source_positions(positions);
}

/// Same as `lower_to_mir`, but returns the [`TyCtxt`] alongside
/// the MIR bodies so downstream passes that need type information
/// (e.g. the native codegen's primitive-type classification) can
/// walk `body.local_ty(local)` back into the kind table.
///
/// Runs the shared front-end gate ([`crate::frontend::check_frontend`])
/// rather than an independent parse/resolve/typecheck of its own, so
/// the single fatal-error policy is the only frontend in the tree.
/// This is the source-taking codegen entry point used by the legacy
/// `compile_source` artifact path and the package builder, both of
/// which validate the program through the gate before reaching codegen;
/// any residual diagnostics here would have been surfaced there.
fn lower_to_mir_with_tcx(
    source: &str,
    unit_name: &str,
    profile: MirProfile,
) -> (Vec<Body>, TyCtxt) {
    let augmented = gossamer_parse::autoderive::augment_source(source);
    let mut map = SourceMap::new();
    let file = map.add_file(unit_name, augmented.clone());
    register_source_positions(unit_name, &map, file);
    let outcome = crate::frontend::check_frontend(&augmented, file);
    lower_to_mir_from_frontend(outcome.checked, profile)
}

#[cfg(test)]
mod tests {
    use super::{MirProfile, lower_to_mir_with_tcx};
    use gossamer_types::TyKind;

    #[test]
    fn release_mir_inlines_small_callees_while_debug_keeps_call_boundaries() {
        let source = r#"
            fn helper(x: i64) -> i64 { x + 1 }
            fn main() { println("{}", helper(41)) }
        "#;
        let (debug, _) = lower_to_mir_with_tcx(source, "profile.gos", MirProfile::Debug);
        let (release, _) = lower_to_mir_with_tcx(source, "profile.gos", MirProfile::Release);
        let debug_main = debug
            .iter()
            .find(|body| body.name == "main")
            .expect("debug main");
        let release_main = release
            .iter()
            .find(|body| body.name == "main")
            .expect("release main");
        assert_ne!(
            format!("{debug_main:?}"),
            format!("{release_main:?}"),
            "debug and release MIR must retain distinct optimization contracts"
        );
    }

    #[test]
    fn inferred_integer_wrapping_chain_has_concrete_mir_locals() {
        let source = r"
            fn step() -> i64 {
                let state = 17
                state.wrapping_mul(6364136223846793005).wrapping_add(1)
            }
        ";
        let (bodies, tcx) = lower_to_mir_with_tcx(source, "wrapping.gos", MirProfile::Debug);
        let step = bodies
            .iter()
            .find(|body| body.name == "step")
            .expect("step body");
        assert!(
            step.locals
                .iter()
                .all(|local| !matches!(tcx.kind_of(local.ty), TyKind::Var(_))),
            "wrapping arithmetic left an unresolved MIR local: {:?}",
            step.locals
                .iter()
                .map(|local| tcx.kind_of(local.ty))
                .collect::<Vec<_>>()
        );
    }
}
