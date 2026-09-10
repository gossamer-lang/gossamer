//! `gos lint` - clippy-style lints over the Gossamer AST.
//! Each lint is a pure function `(&SourceFile) -> Vec<(Span, title,
//! help)>`. The runner filters by per-lint level and renders the
//! findings through `gossamer-diagnostics`.
//! The day-one lint set targets mistakes that actually bite in real
//! Gossamer code: unused bindings and imports, boolean literal
//! comparisons, single-arm matches, redundant clones, double
//! negation, `panic!` inside `main`, and `let _ = Ok/Err(...)` that
//! silently drops a `Result`.

#![forbid(unsafe_code)]
// Each lint is a flat dispatch over AST kinds; collapsing them into
// helpers obscures the per-lint shape that makes the file readable.
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;

use gossamer_ast::{Attrs, Item, ItemKind, SourceFile};
use gossamer_diagnostics::{Code, Diagnostic, Location, Severity};
use gossamer_lex::Span;

pub mod explain;
pub mod fix;
mod lints;

pub mod migrate;

pub use explain::lint_explanation;
pub use fix::{Fix, apply as apply_fixes, fixes};

/// Level attached to a lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// Disabled entirely.
    Allow,
    /// Reported as a warning.
    Warn,
    /// Reported as an error.
    Deny,
}

impl Level {
    /// Parses a textual level.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "allow" => Self::Allow,
            "warn" => Self::Warn,
            "deny" => Self::Deny,
            _ => return None,
        })
    }

    fn severity(self) -> Option<Severity> {
        match self {
            Self::Allow => None,
            Self::Warn => Some(Severity::Warning),
            Self::Deny => Some(Severity::Error),
        }
    }
}

/// Registry mapping lint IDs to levels. The default registry has
/// every day-one lint enabled at `warn`.
#[derive(Debug, Clone)]
pub struct Registry {
    levels: BTreeMap<&'static str, Level>,
}

impl Registry {
    /// Empty registry - every lookup returns [`Level::Allow`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            levels: BTreeMap::new(),
        }
    }

    /// Registry with the default day-one lint levels.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut levels = BTreeMap::new();
        for id in DAY_ONE_LINTS {
            levels.insert(*id, Level::Warn);
        }
        Self { levels }
    }

    /// Overrides the level for `id`.
    pub fn set(&mut self, id: &'static str, level: Level) {
        self.levels.insert(id, level);
    }

    /// Returns the level attached to `id`, defaulting to [`Level::Allow`].
    #[must_use]
    pub fn level(&self, id: &str) -> Level {
        self.levels.get(id).copied().unwrap_or(Level::Allow)
    }

    /// Sorted list of `(id, level)` tuples.
    #[must_use]
    pub fn entries(&self) -> Vec<(&'static str, Level)> {
        self.levels.iter().map(|(k, v)| (*k, *v)).collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Every registered lint identifier. Originally "day one" - kept
/// under that name so downstream attribute parsing and the
/// explain-subcommand lookup tables do not churn as lints are
/// added.
pub const DAY_ONE_LINTS: &[&str] = &[
    "unused_variable",
    "unused_import",
    "unused_mut_variable",
    "needless_return",
    "needless_bool",
    "comparison_to_bool_literal",
    "single_match",
    "shadowed_binding",
    "unchecked_result",
    "empty_block",
    "panic_in_main",
    "redundant_clone",
    "double_negation",
    "self_assignment",
    "todo_macro",
    // Style: one canonical spelling per construct.
    "i64_only_container_family",
    "no_effect_statement",
    // Batch 2 (breadth push).
    "bool_literal_in_condition",
    "let_and_return",
    "collapsible_if",
    "if_same_then_else",
    "needless_else_after_return",
    "self_compare",
    "identity_op",
    "unit_let",
    "float_eq_zero",
    "empty_else",
    "match_bool",
    "needless_parens",
    "manual_not_equal",
    "nested_ternary_if",
    // Batch 3.
    "absurd_range",
    "string_literal_concat",
    "chained_negation_literals",
    "if_not_else",
    "empty_string_concat",
    "println_newline_only",
    "match_same_arms",
    // Batch 4 (breadth push to ~50).
    "manual_swap",
    "consecutive_assignment",
    "large_unreadable_literal",
    "redundant_closure",
    "empty_if_body",
    "bool_to_int_match",
    "fn_returns_unit_explicit",
    "let_with_unit_type",
    "useless_default_only_match",
    "unnecessary_parens_in_condition",
    "pattern_matching_unit",
    "panic_without_message",
    "empty_loop",
    // Batch 5 (conciseness nudges).
    "fill_loop",
    "substring_byte_scan",
];

/// Runs every enabled lint over `source_file` (parsed from `src`) and
/// returns their diagnostics in deterministic order (lint code asc,
/// source offset asc).
#[must_use]
pub fn run(source_file: &SourceFile, src: &str, registry: &Registry) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let scopes = collect_lint_scopes(source_file);
    for (id, level) in registry.entries() {
        // A lint the file disables can still be denied on one item, so the
        // pass runs whenever any scope asks for it.
        if level.severity().is_none() && !scopes.iter().any(|s| s.levels.contains_key(id)) {
            continue;
        }
        let findings = lints::run_lint(id, source_file, src);
        for (span, title, help) in findings {
            let Some(severity) = scoped_level(&scopes, id, span).unwrap_or(level).severity() else {
                continue;
            };
            let location = Location::new(span.file, span);
            let code = lint_code(id);
            let mut diag = match severity {
                Severity::Error => Diagnostic::error(code, title.clone()),
                Severity::Warning => Diagnostic::warning(code, title.clone()),
                Severity::Note | Severity::Help => Diagnostic::note(code, title.clone()),
            };
            diag = diag.with_primary(location, title);
            if let Some(help) = help {
                diag = diag.with_help(help);
            }
            diag = diag.with_note(format!("lint: {id}"));
            out.push(diag);
        }
    }
    out.sort_by(|a, b| {
        let a_span = a.labels.first().map_or(0, |l| l.location.span.start);
        let b_span = b.labels.first().map_or(0, |l| l.location.span.start);
        a.code
            .as_str()
            .cmp(b.code.as_str())
            .then(a_span.cmp(&b_span))
    });
    out
}

/// One item's `#[lint(..)]` levels, keyed by the source range the item
/// covers. A finding inside that range takes the innermost level set for
/// its lint.
struct LintScope {
    span: Span,
    levels: std::collections::HashMap<&'static str, Level>,
}

/// The level the innermost enclosing item sets for `id`, when one does.
fn scoped_level(scopes: &[LintScope], id: &str, span: Span) -> Option<Level> {
    scopes
        .iter()
        .filter(|scope| {
            scope.span.file == span.file
                && scope.span.start <= span.start
                && span.end <= scope.span.end
        })
        .filter_map(|scope| scope.levels.get(id).map(|level| (scope.span.len(), *level)))
        .min_by_key(|(width, _)| *width)
        .map(|(_, level)| level)
}

/// Every `#[lint(..)]`-carrying item in the file, including methods inside
/// `impl` and `trait` bodies and items inside nested modules.
fn collect_lint_scopes(source_file: &SourceFile) -> Vec<LintScope> {
    let mut out = Vec::new();
    for item in &source_file.items {
        collect_item_lint_scopes(item, &mut out);
    }
    out
}

fn collect_item_lint_scopes(item: &Item, out: &mut Vec<LintScope>) {
    push_lint_scope(&item.attrs, item.span, out);
    match &item.kind {
        ItemKind::Impl(decl) => {
            for member in &decl.items {
                if let gossamer_ast::ImplItem::Fn(f) = member {
                    push_lint_scope(&f.attrs, f.span, out);
                }
            }
        }
        ItemKind::Trait(decl) => {
            for member in &decl.items {
                if let gossamer_ast::TraitItem::Fn(f) = member {
                    push_lint_scope(&f.attrs, f.span, out);
                }
            }
        }
        ItemKind::Mod(decl) => {
            if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                for nested in inner {
                    collect_item_lint_scopes(nested, out);
                }
            }
        }
        _ => {}
    }
}

fn push_lint_scope(attrs: &Attrs, span: Span, out: &mut Vec<LintScope>) {
    if attrs.is_empty() {
        return;
    }
    let mut registry = Registry::new();
    apply_attributes(attrs, &mut registry);
    let levels: std::collections::HashMap<&'static str, Level> =
        registry.entries().into_iter().collect();
    if !levels.is_empty() {
        out.push(LintScope { span, levels });
    }
}

/// Stable diagnostic code for a lint identifier.
///
/// Codes are assigned per identifier rather than by position, so retiring a
/// lint leaves every other code where it was. Anything that presents codes,
/// including the generated documentation, reads them from here.
#[must_use]
pub fn lint_code(id: &str) -> Code {
    match id {
        "unused_variable" => Code("GL0001"),
        "unused_import" => Code("GL0002"),
        "unused_mut_variable" => Code("GL0003"),
        "needless_return" => Code("GL0004"),
        "needless_bool" => Code("GL0005"),
        "comparison_to_bool_literal" => Code("GL0006"),
        "single_match" => Code("GL0007"),
        "shadowed_binding" => Code("GL0008"),
        "unchecked_result" => Code("GL0009"),
        "empty_block" => Code("GL0010"),
        "panic_in_main" => Code("GL0011"),
        "redundant_clone" => Code("GL0012"),
        "double_negation" => Code("GL0013"),
        "self_assignment" => Code("GL0014"),
        "todo_macro" => Code("GL0015"),
        "bool_literal_in_condition" => Code("GL0016"),
        "let_and_return" => Code("GL0017"),
        "collapsible_if" => Code("GL0018"),
        "if_same_then_else" => Code("GL0019"),
        "needless_else_after_return" => Code("GL0021"),
        "self_compare" => Code("GL0022"),
        "identity_op" => Code("GL0023"),
        "unit_let" => Code("GL0024"),
        "float_eq_zero" => Code("GL0025"),
        "empty_else" => Code("GL0026"),
        "match_bool" => Code("GL0027"),
        "needless_parens" => Code("GL0028"),
        "manual_not_equal" => Code("GL0029"),
        "nested_ternary_if" => Code("GL0030"),
        "absurd_range" => Code("GL0031"),
        "string_literal_concat" => Code("GL0032"),
        "chained_negation_literals" => Code("GL0033"),
        "if_not_else" => Code("GL0034"),
        "empty_string_concat" => Code("GL0035"),
        "println_newline_only" => Code("GL0036"),
        "match_same_arms" => Code("GL0037"),
        "manual_swap" => Code("GL0038"),
        "consecutive_assignment" => Code("GL0039"),
        "large_unreadable_literal" => Code("GL0040"),
        "redundant_closure" => Code("GL0041"),
        "empty_if_body" => Code("GL0042"),
        "bool_to_int_match" => Code("GL0043"),
        "fn_returns_unit_explicit" => Code("GL0044"),
        "let_with_unit_type" => Code("GL0045"),
        "useless_default_only_match" => Code("GL0046"),
        "unnecessary_parens_in_condition" => Code("GL0047"),
        "pattern_matching_unit" => Code("GL0048"),
        "panic_without_message" => Code("GL0049"),
        "empty_loop" => Code("GL0050"),
        "fill_loop" => Code("GL0051"),
        "substring_byte_scan" => Code("GL0052"),
        "i64_only_container_family" => Code("GL0054"),
        "no_effect_statement" => Code("GL0056"),
        _ => Code("GL9999"),
    }
}

pub(crate) type Finding = (Span, String, Option<String>);

/// Applies `#[lint(...)]` attributes to modify `registry`.
///
/// Recognised forms: `#[lint(allow(<id>))]`, `#[lint(warn(<id>))]`,
/// `#[lint(deny(<id>))]`.
pub fn apply_attributes(attrs: &Attrs, registry: &mut Registry) {
    for attr in attrs.outer.iter().chain(attrs.inner.iter()) {
        let Some(last) = attr.path.segments.last() else {
            continue;
        };
        if last.name.name != "lint" {
            continue;
        }
        let Some(text) = attr.tokens.as_deref() else {
            continue;
        };
        let normalised: String = text.split_whitespace().collect::<Vec<_>>().join("");
        for part in normalised.split(',') {
            let part = part.trim();
            let (level, body) = if let Some(rest) = part
                .strip_prefix("allow(")
                .and_then(|r| r.strip_suffix(')'))
            {
                (Level::Allow, rest.trim())
            } else if let Some(rest) = part.strip_prefix("warn(").and_then(|r| r.strip_suffix(')'))
            {
                (Level::Warn, rest.trim())
            } else if let Some(rest) = part.strip_prefix("deny(").and_then(|r| r.strip_suffix(')'))
            {
                (Level::Deny, rest.trim())
            } else {
                continue;
            };
            if let Some(id) = DAY_ONE_LINTS.iter().copied().find(|i| *i == body) {
                registry.set(id, level);
            }
        }
    }
}

// ---------------------------------------------------------------------
// Traversal helpers
// ---------------------------------------------------------------------
