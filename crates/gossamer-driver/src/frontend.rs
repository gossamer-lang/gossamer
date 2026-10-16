//! The single authoritative front-end gate.
//!
//! [`check_frontend`] runs parse + resolve + typecheck + exhaustiveness
//! under one fatal-error policy. `gos check`, `gos`, `gos build`,
//! `gos test`, and `gos bench` all call it and treat a non-empty
//! diagnostic list identically: render the diagnostics and refuse to
//! proceed. Centralising the policy here is what keeps the gates from
//! drifting (`check` rejecting a program that `build` then miscompiles).
//!
//! The contract: anything `gos` rejects dynamically or the LLVM
//! backend cannot lower must be rejected here, statically, on every
//! tier. `check` is the strongest gate, never a weaker one.
//!
//! An accepted pass publishes its complete result through
//! [`crate::frontend_cache`], and a later pass over unchanged inputs
//! restores it instead of re-running any stage.

#![forbid(unsafe_code)]

use gossamer_diagnostics::Diagnostic;
use gossamer_lex::FileId;
use gossamer_parse::autoderive::ParsedProgram;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{CheckPhase, PhaseObserver, TyCtxt, check_resolved_unit};
use std::time::{Duration, Instant};

use crate::frontend_cache::{
    CachedFrontend, cache_enabled, frontend_key, load_blob, store_frontend,
};
use crate::pipeline::CheckedFrontend;

/// Result of the shared front-end gate.
///
/// `diagnostics` carries every fatal diagnostic under the unified
/// policy, already lowered to the renderable [`Diagnostic`] form. An
/// empty list means the program is accepted; `checked` is always
/// populated so a caller that wants to keep lowering a partially-typed
/// program (the legacy infallible codegen entry points) still can.
pub struct FrontendOutcome {
    /// Parsed, resolved, and typechecked artifacts.
    pub checked: CheckedFrontend,
    /// Fatal diagnostics under the unified policy; empty == accepted.
    pub diagnostics: Vec<Diagnostic>,
    /// Advisory findings the checker is the only pass with the types to
    /// make. Reported at warning severity; they never move the exit code.
    pub warnings: Vec<Diagnostic>,
    /// Wall-clock timings for the frontend stages that produced this outcome.
    pub timings: FrontendTimings,
}

/// Timings from one authoritative frontend pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrontendTimings {
    /// Parse and parse-cache lookup time.
    pub parse: Duration,
    /// Name-resolution time.
    pub resolve: Duration,
    /// Typechecking time.
    pub typecheck: Duration,
    /// Match-exhaustiveness analysis time.
    pub exhaustiveness: Duration,
    /// Arena-escape analysis time.
    pub arena_escape: Duration,
    /// Whether the whole front-end result was restored from the cache.
    pub parse_cache_hit: bool,
}

impl FrontendOutcome {
    /// `true` when the program passed every front-end gate.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Writes the blob of an accepted pass to `path`, as
/// [`crate::encode_frontend`] answers it. A rejected pass has no blob, and
/// writes nothing.
///
/// # Errors
///
/// The blob cannot be encoded or the file cannot be written.
pub fn write_frontend_blob(
    outcome: &FrontendOutcome,
    path: &std::path::Path,
) -> std::io::Result<()> {
    if !outcome.is_ok() {
        return Ok(());
    }
    let checked = &outcome.checked;
    let bytes = crate::encode_frontend(
        &checked.sf,
        &checked.resolutions,
        &checked.table,
        &checked.tcx,
        &outcome.warnings,
    )
    .map_err(std::io::Error::other)?;
    std::fs::write(path, bytes)
}

/// Writes the blob `GOS_EMIT_FRONTEND_BLOB` asks for, so `gos run` and
/// `gos build` can hand their frontend's output to a comparison.
fn emit_requested_blob(outcome: &FrontendOutcome) {
    if let Some(path) = std::env::var_os("GOS_EMIT_FRONTEND_BLOB")
        && let Err(err) = write_frontend_blob(outcome, std::path::Path::new(&path))
    {
        eprintln!("warning: GOS_EMIT_FRONTEND_BLOB: {err}");
    }
}

/// Parses `source` with its derived items. Answers the tree, its parse
/// diagnostics plus any foreign declaration the project does not allow, and
/// whether the parse failed.
fn parse_phase(
    source: &str,
    file_id: FileId,
    program: Option<ParsedProgram>,
) -> (gossamer_ast::SourceFile, Vec<Diagnostic>, bool) {
    let (sf, parse_diags) =
        gossamer_parse::autoderive::parse_with_autoderive_from(source, file_id, program);
    let mut diagnostics: Vec<Diagnostic> = parse_diags
        .iter()
        .map(gossamer_parse::ParseDiagnostic::to_diagnostic)
        .collect();
    // A program that does not parse is not the program the later passes
    // see: `autoderive::augment_source` declines to synthesize from a
    // recovered tree, so the derived `fmt` / `to_string` / serde surface a
    // clean parse would carry is absent, and every pass would report its
    // absence somewhere the user did not write. The parse diagnostics are
    // the actionable report; the passes still run so the LSP keeps a type
    // table to answer from.
    let parse_failed = !parse_diags.is_empty();
    diagnostics.extend(
        crate::foreign_policy::foreign_policy()
            .diagnostics(&sf, gossamer_parse::autoderive::program_source_end(source)),
    );
    (sf, diagnostics, parse_failed)
}

/// Runs the full front-end on already-augmented `source` and applies the
/// single fatal-error policy:
///
/// - **Parse**: every parse diagnostic is fatal.
/// - **Resolve**: every emitted resolve diagnostic is fatal, so `check`
///   reports the same resolve set the LSP and the REPL do.
/// - **Type**: every type diagnostic is fatal.
/// - **Exhaustiveness**: a non-exhaustive `match` (GM0001) is fatal.
/// - **Arena escape**: a value allocated in an `arena { }` block that is
///   used after the block (GM0003) is fatal.
///
/// `source` must already carry the autoderive augmentation (the declared
/// `__gos_serde_*` stubs and the synthesized derives); the CLI applies
/// `autoderive::augment_source` before calling.
#[must_use]
pub fn check_frontend(source: &str, file_id: FileId) -> FrontendOutcome {
    check_frontend_with_program(source, file_id, None)
}

/// [`check_frontend`] over the text autoderive answered with `program`, the
/// tree of the program's own text it already parsed, so the parse reads
/// only what autoderive appended.
#[must_use]
pub fn check_frontend_with_program(
    source: &str,
    file_id: FileId,
    program: Option<ParsedProgram>,
) -> FrontendOutcome {
    let cache_key = cache_enabled().then(|| frontend_key(source, file_id));
    if let Some(key) = &cache_key
        && let Some(restored) = restore_frontend(key)
    {
        emit_requested_blob(&restored);
        return restored;
    }

    let phase_started = Instant::now();
    let (mut sf, mut diagnostics, parse_failed) = parse_phase(source, file_id, program);
    gossamer_ast::cfg::strip_inactive_members(&mut sf);
    let parse = phase_started.elapsed();

    let phase_started = Instant::now();
    let (mut resolutions, resolve_diags) = resolve_source_file(&sf);
    let resolve = phase_started.elapsed();

    let mut tcx = TyCtxt::new();
    let mut clock = PhaseClock::new();
    let earlier_fatal = !diagnostics.is_empty();
    let checks = check_resolved_unit(
        &mut sf,
        &mut resolutions,
        &resolve_diags,
        parse_failed,
        earlier_fatal,
        &mut tcx,
        &mut clock,
    );
    diagnostics.extend(checks.fatal);
    let warnings = checks.advisory;
    let table = checks.table;

    // The blob is the sole cache-validity marker, and publishing it is what
    // makes a later invocation's hit proof of acceptance. A rejected program
    // therefore never reaches the cache.
    let checked = CheckedFrontend {
        sf,
        resolutions,
        table,
        tcx,
    };
    if diagnostics.is_empty()
        && let Some(key) = &cache_key
    {
        store_frontend(
            key,
            &checked.sf,
            &checked.resolutions,
            &checked.table,
            &checked.tcx,
            &warnings,
        );
    }

    let outcome = FrontendOutcome {
        checked,
        diagnostics,
        warnings,
        timings: FrontendTimings {
            parse,
            resolve,
            typecheck: clock.typecheck,
            exhaustiveness: clock.exhaustiveness,
            arena_escape: clock.analysis,
            parse_cache_hit: false,
        },
    };
    emit_requested_blob(&outcome);
    outcome
}

/// Answers a cached frontend for `key`, when one was published.
///
/// Only a pass that produced zero diagnostics publishes a blob, so a hit is
/// proof the program was accepted under this exact key.
fn restore_frontend(key: &crate::FrontendCacheKey) -> Option<FrontendOutcome> {
    let restore_started = Instant::now();
    let cached = load_blob::<CachedFrontend>(key)?;
    crate::trace_cache_hit("frontend restored", key);
    Some(FrontendOutcome {
        checked: CheckedFrontend {
            sf: cached.sf,
            resolutions: cached.resolutions,
            table: cached.table,
            tcx: cached.tcx,
        },
        diagnostics: Vec::new(),
        warnings: cached.warnings,
        timings: FrontendTimings {
            parse: restore_started.elapsed(),
            parse_cache_hit: true,
            ..FrontendTimings::default()
        },
    })
}

/// Times each phase of [`check_resolved_unit`] from the end of the one
/// before it.
struct PhaseClock {
    last: Instant,
    typecheck: Duration,
    exhaustiveness: Duration,
    analysis: Duration,
}

impl PhaseClock {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            typecheck: Duration::ZERO,
            exhaustiveness: Duration::ZERO,
            analysis: Duration::ZERO,
        }
    }
}

impl PhaseObserver for PhaseClock {
    fn phase_done(&mut self, phase: CheckPhase) {
        let now = Instant::now();
        let spent = now - self.last;
        self.last = now;
        match phase {
            CheckPhase::Typecheck => self.typecheck = spent,
            CheckPhase::Exhaustiveness => self.exhaustiveness = spent,
            CheckPhase::Analysis => self.analysis = spent,
        }
    }
}
