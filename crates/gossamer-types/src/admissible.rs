//! Which callbacks a parallel adapter may run on many workers at once.
//!
//! `par_map`, `par_filter`, and `par_reduce` run their callback on several
//! workers at the same time, so the callback must be one whose purity the
//! compiler can decide: a closure literal, whose body is visible, or a named
//! function. Either must be pure (see [`crate::purity`]). A callable reached
//! through a binding is not decidable at the call site and is refused with the
//! spelling that works. `par_min` and `par_max` run the element type's own
//! `cmp` on every worker, so a user ordering obeys the same rule.

#![forbid(unsafe_code)]

use std::fmt;

use gossamer_ast::{Expr, ExprKind, SourceFile};
use gossamer_lex::Span;
use gossamer_resolve::{DefKind, Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::purity::{Impurity, PurityFacts, analyze, closure_impurity, path_impurity};
use crate::table::TypeTable;
use crate::ty::TyKind;

/// Why an adapter's callback is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inadmissible {
    /// The callback is not pure, for the reason given.
    Impure(Impurity),
    /// The callback is reached through a binding or computed, so its body
    /// is not visible at the call site.
    NotDecidable {
        /// The callback as written.
        written: String,
    },
    /// The element type's ordering is not pure.
    ImpureOrdering {
        /// The element type's name.
        ty: String,
        /// Why its `cmp` is not pure.
        reason: Impurity,
    },
}

/// An adapter call whose callback may not run on many workers at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissibilityDiagnostic {
    /// The adapter method, `par_map` and so on.
    pub method: String,
    /// Why the callback is refused.
    pub reason: Inadmissible,
    /// The callback's span, or the call's for an ordering.
    pub span: Span,
}

impl AdmissibilityDiagnostic {
    /// The diagnostic code every refusal carries.
    pub const CODE: &'static str = "GT0090";

    /// Renders this refusal as a structured diagnostic.
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let location = Location::new(self.span.file, self.span);
        let method = &self.method;
        let effect_help = "a parallel adapter runs its callback on many workers at once; \
                           use `cohort { }` with `spawn` for work that must perform effects";
        match &self.reason {
            Inadmissible::Impure(reason) => {
                let diagnostic = Diagnostic::error(
                    Code(Self::CODE),
                    format!("the callback passed to `{method}` is not pure"),
                )
                .with_primary(location, format!("this callback {}", reason.describe()));
                let diagnostic = if reason.path.is_empty()
                    && reason.effect.kind == crate::purity::EffectKind::WritesCallerState
                {
                    diagnostic.with_note(
                        "a closure captures a `Vec`, `Map`, or `Set` by managed reference, so a write to one it captured would reach the same container from every worker",
                    )
                } else {
                    diagnostic
                };
                diagnostic.with_help(effect_help)
            }
            Inadmissible::NotDecidable { written } => Diagnostic::error(
                Code(Self::CODE),
                format!(
                    "the callback passed to `{method}` must be a closure literal or a named function"
                ),
            )
            .with_primary(
                location,
                format!("`{written}` is reached through a binding, so its body is not visible here"),
            )
            .with_help(
                "write the closure literal at the call, or name a function, so the compiler can decide it is pure",
            ),
            Inadmissible::ImpureOrdering { ty, reason } => Diagnostic::error(
                Code(Self::CODE),
                format!("`{method}` orders `{ty}` by a `cmp` that is not pure"),
            )
            .with_primary(location, format!("`{ty}::cmp` {}", reason.describe()))
            .with_help(effect_help),
        }
    }
}

impl fmt::Display for AdmissibilityDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.to_diagnostic().title)
    }
}

/// Checks every parallel adapter call in `sf`.
#[must_use]
pub fn check_parallel_adapters(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<AdmissibilityDiagnostic> {
    let mut finder = AdapterPresence { found: false };
    gossamer_ast::visitor::Visitor::visit_source_file(&mut finder, sf);
    if !finder.found {
        return Vec::new();
    }
    let facts = analyze(sf, resolutions, table, tcx);
    let mut checker = Checker {
        sf,
        resolutions,
        table,
        tcx,
        facts: &facts,
        out: Vec::new(),
    };
    gossamer_ast::visitor::Visitor::visit_source_file(&mut checker, sf);
    checker.out
}

/// Whether a program calls an adapter at all; most do not, and the purity
/// analysis is not needed for them.
struct AdapterPresence {
    found: bool,
}

impl gossamer_ast::visitor::Visitor for AdapterPresence {
    fn visit_expr(&mut self, expr: &Expr) {
        if is_adapter_call(expr) {
            self.found = true;
            return;
        }
        gossamer_ast::visitor::walk_expr(self, expr);
    }
}

fn is_adapter_call(expr: &Expr) -> bool {
    matches!(
        &expr.kind,
        ExprKind::MethodCall { name, .. }
            if matches!(
                name.name.as_str(),
                "par_map" | "par_filter" | "par_reduce" | "par_min" | "par_max"
            )
    )
}

struct Checker<'a> {
    sf: &'a SourceFile,
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    facts: &'a PurityFacts,
    out: Vec<AdmissibilityDiagnostic>,
}

impl gossamer_ast::visitor::Visitor for Checker<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if is_adapter_call(expr)
            && let Some(diagnostic) = self.check(expr)
        {
            self.out.push(diagnostic);
        }
        gossamer_ast::visitor::walk_expr(self, expr);
    }
}

impl Checker<'_> {
    fn check(&self, call: &Expr) -> Option<AdmissibilityDiagnostic> {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &call.kind
        else {
            return None;
        };
        let elem = self.sequence_elem(receiver)?;
        let method = name.name.clone();
        match (method.as_str(), args.as_slice()) {
            ("par_map" | "par_filter", [callback]) | ("par_reduce", [_, callback]) => self
                .callback(callback)
                .map(|reason| AdmissibilityDiagnostic {
                    method,
                    reason,
                    span: callback.span,
                }),
            ("par_min" | "par_max", []) => {
                let (TyKind::Adt { def, .. } | TyKind::Nominal { def, .. }) =
                    self.tcx.kind_of(elem)
                else {
                    return None;
                };
                let ty = self.tcx.def_name(*def)?.rsplit("::").next()?.to_string();
                let reason = self.facts.method_impurity(&ty, "cmp")?;
                Some(AdmissibilityDiagnostic {
                    method,
                    reason: Inadmissible::ImpureOrdering { ty, reason },
                    span: call.span,
                })
            }
            _ => None,
        }
    }

    /// The element type an adapter receiver walks, or `None` when the
    /// receiver is not one the adapters apply to.
    fn sequence_elem(&self, receiver: &Expr) -> Option<crate::ty::Ty> {
        let mut ty = self.table.get(receiver.id)?;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Vec(elem)
            | TyKind::Slice(elem)
            | TyKind::Array { elem, .. }
            | TyKind::Range(elem) => Some(*elem),
            _ => None,
        }
    }

    fn callback(&self, callback: &Expr) -> Option<Inadmissible> {
        match &callback.kind {
            ExprKind::Closure { .. } => closure_impurity(
                self.facts,
                callback,
                self.sf,
                self.resolutions,
                self.table,
                self.tcx,
            )
            .map(Inadmissible::Impure),
            ExprKind::Path(path) => {
                let segments: Vec<String> = path
                    .segments
                    .iter()
                    .map(|seg| seg.name.name.clone())
                    .collect();
                match self.resolutions.get(callback.id) {
                    Some(Resolution::Def {
                        def,
                        kind: DefKind::Fn,
                    }) => self.facts.impurity(def).map(Inadmissible::Impure),
                    Some(Resolution::Def {
                        kind: DefKind::Struct | DefKind::Variant,
                        ..
                    }) => None,
                    Some(Resolution::Local(_)) => Some(Inadmissible::NotDecidable {
                        written: segments.join("::"),
                    }),
                    _ => {
                        if let Some(reason) = self.facts.path_method_impurity(&segments) {
                            return Some(Inadmissible::Impure(reason));
                        }
                        path_impurity(
                            callback.id,
                            &segments,
                            self.sf,
                            self.resolutions,
                            callback.span,
                        )
                        .map(|effect| {
                            Inadmissible::Impure(Impurity {
                                path: Vec::new(),
                                effect,
                            })
                        })
                    }
                }
            }
            _ => Some(Inadmissible::NotDecidable {
                written: "this expression".to_string(),
            }),
        }
    }
}
