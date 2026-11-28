//! The checks every front end runs once a unit is parsed and resolved.
//!
//! The command line, the language server, and the playground each assemble
//! their own source and report in their own way, but what makes a program
//! acceptable is one policy, and it lives here: a front end that ran less
//! would call a file clean that `gos check` rejects.

#![forbid(unsafe_code)]

use gossamer_ast::{ItemKind, SourceFile};
use gossamer_diagnostics::Diagnostic;
use gossamer_resolve::{Resolutions, ResolveDiagnostic};

use crate::TypeDiagnostic;
use crate::context::TyCtxt;
use crate::exhaustiveness::ExhaustivenessDiagnostic;
use crate::table::TypeTable;

/// What [`check_resolved_unit`] found.
#[derive(Debug)]
pub struct UnitChecks {
    /// The unit's types, built even when a check failed so an editor can
    /// still navigate it.
    pub table: TypeTable,
    /// Findings that reject the program.
    pub fatal: Vec<Diagnostic>,
    /// Findings reported at warning severity, which never reject it.
    pub advisory: Vec<Diagnostic>,
}

/// A phase [`check_resolved_unit`] has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckPhase {
    /// Type checking.
    Typecheck,
    /// Match exhaustiveness.
    Exhaustiveness,
    /// Arena escape, spawned captures, and parallel purity.
    Analysis,
}

/// Told as each phase of [`check_resolved_unit`] ends, so a caller with a
/// clock can time them.
pub trait PhaseObserver {
    /// `phase` just finished.
    fn phase_done(&mut self, phase: CheckPhase);
}

impl PhaseObserver for () {
    fn phase_done(&mut self, _phase: CheckPhase) {}
}

/// Rewrites the unit's caller-side spellings, type-checks it, and runs every
/// analysis the command-line gate applies, sorting the findings into fatal
/// and advisory.
///
/// `parse_failed` withholds every finding: a program that does not parse is
/// not the one these passes see, and the parse diagnostics are the
/// actionable report. `earlier_fatal` says the caller already rejected the
/// program for another reason, which withholds the purity analysis that
/// needs a well-typed program to say anything.
pub fn check_resolved_unit(
    sf: &mut SourceFile,
    resolutions: &mut Resolutions,
    resolve_diags: &[ResolveDiagnostic],
    parse_failed: bool,
    earlier_fatal: bool,
    tcx: &mut TyCtxt,
    observer: &mut impl PhaseObserver,
) -> UnitChecks {
    let named_arg_diags = crate::normalize_caller_side_spellings(sf, resolutions);
    let in_scope = top_level_names(sf);
    let mut fatal: Vec<Diagnostic> = Vec::new();
    let mut advisory: Vec<Diagnostic> = Vec::new();
    if !parse_failed {
        fatal.extend(named_arg_diags.iter().map(|d| d.to_diagnostic(&in_scope)));
        fatal.extend(resolve_diags.iter().map(|d| d.to_diagnostic(&in_scope)));
    }

    // The checker reads only the items this build compiles. Setting the
    // rest aside lets it read the tree in place rather than a filtered copy.
    let inactive = gossamer_ast::cfg::SetAside::take(&mut sf.items, &|item| {
        gossamer_ast::cfg::item_is_active(&item.attrs)
    });
    let (table, type_diags) = crate::typecheck_source_file(sf, resolutions, tcx);
    inactive.restore(&mut sf.items);
    observer.phase_done(CheckPhase::Typecheck);
    if !parse_failed {
        fatal.extend(
            type_diags
                .iter()
                .filter(|diag| !diag.is_advisory())
                .map(TypeDiagnostic::to_diagnostic),
        );
    }

    let exhaustive = crate::check_exhaustiveness(sf, resolutions, &table, tcx);
    observer.phase_done(CheckPhase::Exhaustiveness);
    if !parse_failed {
        fatal.extend(
            exhaustive
                .iter()
                .map(ExhaustivenessDiagnostic::to_diagnostic),
        );
    }

    if !parse_failed {
        // A value allocated in an `arena { }` block that outlives it is a
        // use-after-free, so an escape rejects the program on every tier.
        for diag in crate::check_arena_escapes(sf, resolutions, &table, tcx) {
            fatal.push(diag.to_diagnostic());
        }
        // A write to a spawned closure's snapshot never reaches the binding,
        // and a callable that carries its captures into a goroutine races
        // with the code that spawned it.
        for diag in crate::check_capture_writes(sf, resolutions, &table, tcx) {
            fatal.push(diag.to_diagnostic());
        }
        // A binding declared without a value has none to read until a path
        // assigns it, so a read some path reaches first is refused.
        for diag in crate::check_definite_assignment(sf, resolutions, &table, tcx) {
            fatal.push(diag.to_diagnostic());
        }
        // A `mut` binding nothing assigns, hidden by an inner `let` of its
        // name, is a write to the outer binding that declared a new one.
        for diag in crate::check_shadowed_mut(sf, resolutions) {
            fatal.push(diag.to_diagnostic());
        }
        // A computed float compared with a literal it cannot hold exactly is
        // false for the inputs that reach that decimal.
        for diag in crate::check_float_equalities(sf, resolutions, &table, tcx) {
            fatal.push(diag.to_diagnostic());
        }
        // A mutex guard is released by the block that binds it, so it is
        // used for nothing but its own `unlock()`.
        for diag in crate::check_guard_uses(sf, resolutions, &table, tcx) {
            fatal.push(diag.to_diagnostic());
        }
        // A drop type is a struct shared by handle, and is never cloned.
        for diag in crate::check_drop_uses(sf, &table, tcx) {
            fatal.push(diag.to_diagnostic());
        }
        // A parallel adapter's callback runs on many workers at once, so one
        // whose purity cannot be shown is refused on every tier.
        if !earlier_fatal && fatal.is_empty() {
            for diag in crate::check_parallel_adapters(sf, resolutions, &table, tcx) {
                fatal.push(diag.to_diagnostic());
            }
        }
    }
    if !parse_failed {
        // A typed lint at `deny` refuses the program like any other error.
        let (denied, warned): (Vec<_>, Vec<_>) =
            typed_lints(sf, resolutions, &table, tcx, &type_diags)
                .into_iter()
                .partition(|diag| diag.severity == gossamer_diagnostics::Severity::Error);
        fatal.extend(denied);
        advisory.extend(warned);
    }
    observer.phase_done(CheckPhase::Analysis);

    UnitChecks {
        table,
        fatal,
        advisory,
    }
}

/// The findings of the lints that read the checked program's types, at the
/// levels the default registry and the unit's `#[lint(..)]` attributes give
/// them, with the advisory findings among the checker's own `type_diags`.
/// A finding at `deny` is an error that refuses the program; the rest are
/// warnings, which `gos lint --deny-warnings` makes errors.
#[must_use]
pub fn typed_lints(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
    type_diags: &[TypeDiagnostic],
) -> Vec<Diagnostic> {
    let mut registry = gossamer_lint::Registry::with_defaults();
    gossamer_lint::apply_attributes(&sf.attrs, &mut registry);
    let mut out = checker_lints(sf, &registry, type_diags);
    let casts = crate::find_lossy_casts(sf, table, tcx)
        .into_iter()
        .map(|cast| (cast.span, cast.title, Some(cast.help)))
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::LOSSY_CAST_LINT,
        sf,
        &registry,
        casts,
    ));
    let cycles = crate::find_closure_self_cycles(sf, resolutions, table)
        .into_iter()
        .map(|cycle| {
            let name = cycle.name;
            (
                cycle.span,
                format!(
                    "this closure is stored in `{name}`, which it captures: the two hold each other, and the cycle is never freed"
                ),
                Some(format!(
                    "capture a weak reference: `[weak {name}] || ..`, where `{name}` in the body is an `Option` that is `None` once the value is gone"
                )),
            )
        })
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::CLOSURE_SELF_CYCLE_LINT,
        sf,
        &registry,
        cycles,
    ));
    out.extend(signed_remainders(sf, resolutions, table, tcx, &registry));
    out.extend(float_and_task_lints(sf, resolutions, table, tcx, &registry));
    let split_updates = crate::find_split_shared_updates(sf, resolutions, table, tcx)
        .into_iter()
        .map(|found| {
            let (handle, read) = (found.handle, found.read);
            (
                found.span,
                format!(
                    "`{handle}.set(..)` stores a value `{handle}.{read}(..)` read under a separate lock: a write another goroutine makes between the two is lost"
                ),
                Some(format!(
                    "read and write under one lock: `{handle}.update(|value| ..)` stores what the callback answers"
                )),
            )
        })
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::SPLIT_SHARED_UPDATE_LINT,
        sf,
        &registry,
        split_updates,
    ));
    let day_durations = crate::find_day_durations(sf, table, tcx)
        .into_iter()
        .map(|found| {
            let days = found.days.unsigned_abs();
            let unit = if days == 1 { "day" } else { "days" };
            let hours = days * 24;
            (
                found.span,
                format!(
                    "a `Duration` of {days} {unit} is {hours} hours of elapsed time, which lands an hour off the time of day across a daylight-saving change"
                ),
                Some(format!(
                    "to move the calendar date and keep the time of day, write `.add_date(0, 0, {})?` on the `Time`",
                    found.days
                )),
            )
        })
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::DAY_DURATION_LINT,
        sf,
        &registry,
        day_durations,
    ));
    // Code the toolchain appended, and a dependency's own code, are not the
    // program's to change.
    let dependencies = dependency_spans(&sf.items);
    out.retain(|diag| {
        diag.labels.first().is_none_or(|label| {
            let span = label.location.span;
            sf.is_program_text(span)
                && !dependencies.iter().any(|dep| {
                    dep.file == span.file && dep.start <= span.start && span.end <= dep.end
                })
        })
    });
    out
}

/// The checker's own advisory findings, at the levels `registry` gives
/// their lints.
fn checker_lints(
    sf: &SourceFile,
    registry: &gossamer_lint::Registry,
    type_diags: &[TypeDiagnostic],
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for diag in type_diags {
        let Some(lint) = diag.advisory_lint() else {
            continue;
        };
        let rendered = diag.to_diagnostic();
        let help = rendered.helps.into_iter().next();
        out.extend(gossamer_lint::leveled(
            lint,
            sf,
            registry,
            vec![(diag.span, rendered.title, help)],
        ));
    }
    out
}

/// `GL0025` for an inexact float literal compared with a value not computed
/// in sight, and `GL0065` for a task handle `main` never joins, at the levels
/// `registry` gives them.
fn float_and_task_lints(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
    registry: &gossamer_lint::Registry,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let float_equalities = crate::find_inexact_float_equalities(sf, resolutions, table, tcx)
        .into_iter()
        .filter(|found| found.refusal.is_none())
        .map(|found| {
            (
                found.span,
                "equality against a float literal its type cannot hold exactly".to_string(),
                Some("compare `(x - y).abs() < eps` with an explicit tolerance".to_string()),
            )
        })
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::FLOAT_EQ_LINT,
        sf,
        registry,
        float_equalities,
    ));
    let unjoined = crate::find_unjoined_tasks(sf, resolutions)
        .into_iter()
        .map(|task| {
            let name = task.name;
            (
                task.span,
                format!(
                    "`{name}` is never joined: nothing waits for its task, so the program goes on, and can finish, before the task's work is done"
                ),
                Some(format!(
                    "call `{name}.join()` where the task's work must be done; a task meant to run on its own is spawned without a binding: `spawn(..)`"
                )),
            )
        })
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::UNJOINED_TASK_LINT,
        sf,
        registry,
        unjoined,
    ));
    out
}

/// `GL0064` at the level `registry` gives it, each finding carrying the
/// comparison respelled for every sign where both operands have a spelling.
fn signed_remainders(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
    registry: &gossamer_lint::Registry,
) -> Vec<Diagnostic> {
    let remainders = crate::find_signed_remainders(sf, resolutions, table, tcx);
    let rewrites: Vec<(gossamer_lex::Span, String)> = remainders
        .iter()
        .filter_map(|found| found.fix.clone().map(|fix| (found.span, fix)))
        .collect();
    let mut leveled = gossamer_lint::leveled(
        crate::SIGNED_REMAINDER_LINT,
        sf,
        registry,
        remainders
            .into_iter()
            .map(|found| (found.span, found.title, Some(found.help)))
            .collect(),
    );
    for diag in &mut leveled {
        let Some(label) = diag.labels.first() else {
            continue;
        };
        let location = label.location;
        if let Some((_, fix)) = rewrites.iter().find(|(span, _)| *span == location.span) {
            diag.suggestions
                .push(gossamer_diagnostics::Suggestion::replacement(
                    location,
                    "compare a remainder that holds for every sign",
                    fix.clone(),
                ));
        }
    }
    leveled
}

/// The spans of the modules the bundler wrapped a dependency's sources in
/// (`#[dependency("id")] mod name { .. }`).
fn dependency_spans(items: &[gossamer_ast::Item]) -> Vec<gossamer_lex::Span> {
    items
        .iter()
        .filter(|item| {
            matches!(item.kind, ItemKind::Mod(_))
                && item
                    .attrs
                    .outer
                    .iter()
                    .any(|attr| attr.is_named("dependency"))
        })
        .map(|item| item.span)
        .collect()
}

/// Every top-level item name `sf` declares: the candidates a resolve
/// diagnostic's `did you mean ...?` suggestion is drawn from.
#[must_use]
pub fn top_level_names(sf: &SourceFile) -> Vec<&str> {
    sf.items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Fn(decl) => Some(decl.name.name.as_str()),
            ItemKind::Struct(decl) => Some(decl.name.name.as_str()),
            ItemKind::Enum(decl) => Some(decl.name.name.as_str()),
            ItemKind::Trait(decl) => Some(decl.name.name.as_str()),
            ItemKind::TypeAlias(decl) => Some(decl.name.name.as_str()),
            ItemKind::Const(decl) => Some(decl.name.name.as_str()),
            ItemKind::Static(decl) => Some(decl.name.name.as_str()),
            ItemKind::Mod(decl) => Some(decl.name.name.as_str()),
            ItemKind::Impl(_) | ItemKind::AttrItem(_) => None,
        })
        .collect()
}
