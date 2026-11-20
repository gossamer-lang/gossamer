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

use crate::context::TyCtxt;
use crate::exhaustiveness::ExhaustivenessError;
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
        for diag in &type_diags {
            if diag.is_advisory() {
                advisory.push(diag.to_diagnostic());
            } else {
                fatal.push(diag.to_diagnostic());
            }
        }
    }

    let exhaustive = crate::check_exhaustiveness(sf, resolutions, &table, tcx);
    observer.phase_done(CheckPhase::Exhaustiveness);
    if !parse_failed {
        for diag in &exhaustive {
            match diag.error {
                ExhaustivenessError::NonExhaustive { .. } => fatal.push(diag.to_diagnostic()),
                ExhaustivenessError::UnreachableArm => advisory.push(diag.to_diagnostic()),
            }
        }
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
        // A parallel adapter's callback runs on many workers at once, so one
        // whose purity cannot be shown is refused on every tier.
        if !earlier_fatal && fatal.is_empty() {
            for diag in crate::check_parallel_adapters(sf, resolutions, &table, tcx) {
                fatal.push(diag.to_diagnostic());
            }
        }
    }
    if !parse_failed {
        advisory.extend(typed_lints(sf, resolutions, &table, tcx));
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
/// them. They are advisory wherever they are reported from a check:
/// `gos lint --deny-warnings` is the gate that makes them errors.
#[must_use]
pub fn typed_lints(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<Diagnostic> {
    let mut registry = gossamer_lint::Registry::with_defaults();
    gossamer_lint::apply_attributes(&sf.attrs, &mut registry);
    let casts = crate::find_lossy_casts(sf, table, tcx)
        .into_iter()
        .map(|cast| (cast.span, cast.title, Some(cast.help)))
        .collect();
    let mut out = gossamer_lint::leveled(crate::LOSSY_CAST_LINT, sf, &registry, casts);
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
    let remainders = crate::find_signed_remainders(sf, resolutions, table, tcx)
        .into_iter()
        .map(|found| (found.span, found.title, Some(found.help)))
        .collect();
    out.extend(gossamer_lint::leveled(
        crate::SIGNED_REMAINDER_LINT,
        sf,
        &registry,
        remainders,
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
    for diag in &mut out {
        diag.severity = gossamer_diagnostics::Severity::Warning;
    }
    out
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
