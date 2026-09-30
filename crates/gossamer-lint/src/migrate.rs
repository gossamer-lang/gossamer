//! Source rewriters `gos fix` applies to bring a project forward.
//!
//! A migration is not a lint. A lint says something about the code the
//! author wrote and is reported whether or not they act on it; a
//! migration is a mechanical upgrade the toolchain owns, and running it
//! is the whole interaction. They share [`crate::Fix`] as the edit
//! representation and nothing else.
//!
//! Every rewriter here must be **deterministic** - the same input yields
//! the same edits - and **idempotent** - applying it to its own output
//! produces no further edits. `gos fix` re-runs the front end afterwards
//! and keeps the result only when the file still checks, so a rewriter
//! that breaks a program cannot land, but a rewriter that is not
//! idempotent would still churn a repository on every run.

use gossamer_ast::SourceFile;

use crate::Fix;

/// One named migration.
pub struct Rewriter {
    /// Stable identifier, used to select and to report.
    pub id: &'static str,
    /// One line describing what the rewrite does.
    pub summary: &'static str,
    /// Toolchain versions this rewriter prepares a project for. Empty
    /// means it applies to every version.
    pub versions: &'static [&'static str],
    /// Collects the edits this rewriter would make.
    pub collect: fn(&SourceFile, &str, &mut Vec<Fix>),
    /// When set, each collected edit is a candidate kept only where applying
    /// it removes a front-end diagnostic: the rewrite repairs code a meaning
    /// change broke and leaves code the change did not touch as written.
    pub repair_only: bool,
}

/// Every registered migration, in application order.
pub const REWRITERS: &[Rewriter] = &[
    Rewriter {
        id: "integer-pow-to-float",
        summary: "`n.pow(e)` on an integer answers an integer since 0.65.0; keeps the float power where the program relied on one",
        versions: &["v0.65.0"],
        collect: collect_integer_pow_to_float,
        repair_only: true,
    },
    Rewriter {
        id: "regex-compile-answers-pattern",
        summary: "`regex::compile(\"literal\")` answers the `Pattern` since 0.65.0; drops the unwrap, a pattern built at run time goes to `regex::new`, and `Pattern::compile` is `Pattern::new`",
        versions: &["v0.65.0"],
        collect: collect_regex_compile,
        repair_only: true,
    },
    Rewriter {
        id: "hash-inputs-are-bytes",
        summary: "hash `hex` functions, `hmac::sha256_hex`, and `fs::write_mode` take `Vec<u8>` since 0.65.0; passes a `String` argument as `.as_bytes()`",
        versions: &["v0.65.0"],
        collect: collect_bytes_arguments,
        repair_only: true,
    },
    Rewriter {
        id: "checksums-are-u32",
        summary: "CRC-32, CRC-32C, Adler-32, and `fnv::hash32` answer `u32` since 0.65.0; casts where the program used an `i64`",
        versions: &["v0.65.0"],
        collect: collect_checksum_width,
        repair_only: true,
    },
];

/// The segment names of a call's callee path, when the callee is a path.
fn callee_path(callee: &gossamer_ast::Expr) -> Option<Vec<&str>> {
    match &callee.kind {
        gossamer_ast::ExprKind::Path(path) => {
            Some(path.segments.iter().map(|s| s.name.name.as_str()).collect())
        }
        _ => None,
    }
}

/// Whether `segments` ends with `tail`.
fn ends_with(segments: &[&str], tail: &[&str]) -> bool {
    segments.len() >= tail.len() && segments[segments.len() - tail.len()..] == *tail
}

/// The source text of `span`.
fn text_of(source: &str, span: gossamer_lex::Span) -> Option<&str> {
    source.get(span.start as usize..span.end as usize)
}

/// Whether `expr` is a validated `regex::compile("literal")` call.
fn literal_regex_compile(expr: &gossamer_ast::Expr) -> bool {
    use gossamer_ast::{ExprKind, Literal};
    matches!(&expr.kind, ExprKind::Call { callee, args }
        if callee_path(callee).is_some_and(|p| ends_with(&p, &["regex", "compile"]))
            && matches!(args.first().map(|a| &a.kind), Some(ExprKind::Literal(Literal::String(_)))))
}

/// Proposes, for each `regex::compile` call: dropping a `?`, `.unwrap()`,
/// or `.expect(..)` applied to a literal one, which now answers the
/// `Pattern` itself; and `regex::new` in place of a call handed anything
/// but a literal.
fn collect_regex_compile(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    use gossamer_ast::ExprKind;
    use gossamer_ast::visitor::{Visitor, walk_expr};

    struct Scan<'a> {
        source: &'a str,
        out: &'a mut Vec<Fix>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &gossamer_ast::Expr) {
            let unwrapped = match &expr.kind {
                ExprKind::Try(inner) => Some(inner.as_ref()),
                ExprKind::MethodCall { receiver, name, .. }
                    if matches!(name.name.as_str(), "unwrap" | "expect") =>
                {
                    Some(receiver.as_ref())
                }
                _ => None,
            };
            // `match regex::compile("..") { Ok(p) => p, Err(..) => .. }` is the
            // pattern itself: the `Ok` arm is the only one that runs.
            let matched = match &expr.kind {
                ExprKind::Match { scrutinee, arms }
                    if literal_regex_compile(scrutinee)
                        && arms.iter().any(ok_arm_answers_binding) =>
                {
                    Some(scrutinee.as_ref())
                }
                _ => None,
            };
            if let Some(inner) = unwrapped.or(matched)
                && literal_regex_compile(inner)
                && let Some(text) = text_of(self.source, inner.span)
            {
                self.out.push(Fix {
                    span: expr.span,
                    replacement: text.to_string(),
                    lint_id: "regex-compile-answers-pattern",
                });
            }
            // `Pattern::compile(p)` was the type-qualified fallible spelling;
            // it is `Pattern::new(p)` now, answering the same `Result`.
            if let ExprKind::Call { callee, .. } = &expr.kind
                && callee_path(callee).is_some_and(|p| ends_with(&p, &["Pattern", "compile"]))
                && let Some(text) = text_of(self.source, callee.span)
                && let Some(stem) = text.strip_suffix("compile")
            {
                self.out.push(Fix {
                    span: callee.span,
                    replacement: format!("{stem}new"),
                    lint_id: "regex-compile-answers-pattern",
                });
            }
            if let ExprKind::Call { callee, .. } = &expr.kind
                && callee_path(callee).is_some_and(|p| ends_with(&p, &["regex", "compile"]))
                && !literal_regex_compile(expr)
                && let Some(text) = text_of(self.source, callee.span)
                && let Some(stem) = text.strip_suffix("compile")
            {
                self.out.push(Fix {
                    span: callee.span,
                    replacement: format!("{stem}new"),
                    lint_id: "regex-compile-answers-pattern",
                });
            }
            walk_expr(self, expr);
        }
    }
    Scan { source, out }.visit_source_file(sf);
}

/// Whether `arm` is `Ok(name) => name`.
fn ok_arm_answers_binding(arm: &gossamer_ast::MatchArm) -> bool {
    use gossamer_ast::{ExprKind, PatternKind};
    let PatternKind::TupleStruct { path, elems } = &arm.pattern.kind else {
        return false;
    };
    let [binding] = elems.as_slice() else {
        return false;
    };
    let PatternKind::Ident {
        name,
        subpattern: None,
        ..
    } = &binding.kind
    else {
        return false;
    };
    let ok = path.segments.last().is_some_and(|s| s.name.name == "Ok");
    let answers = matches!(&arm.body.kind, ExprKind::Path(p)
        if p.segments.len() == 1 && p.segments[0].name.name == name.name);
    ok && arm.guard.is_none() && answers
}

/// Proposes `arg.as_bytes()` for each argument of a call that takes bytes
/// since 0.65.0, where a `String` used to be accepted.
fn collect_bytes_arguments(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    use gossamer_ast::ExprKind;
    use gossamer_ast::visitor::{Visitor, walk_expr};

    // (path tail, which arguments take bytes)
    const CALLS: &[(&[&str], &[usize])] = &[
        (&["sha256", "hex"], &[0]),
        (&["sha512", "hex"], &[0]),
        (&["blake3", "hex"], &[0]),
        (&["insecure", "md5_hex"], &[0]),
        (&["insecure", "sha1_hex"], &[0]),
        (&["hmac", "sha256_hex"], &[0, 1]),
        (&["fs", "write_mode"], &[1]),
    ];
    struct Scan<'a> {
        source: &'a str,
        out: &'a mut Vec<Fix>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &gossamer_ast::Expr) {
            if let ExprKind::Call { callee, args } = &expr.kind
                && let Some(path) = callee_path(callee)
                && let Some((_, positions)) = CALLS.iter().find(|(tail, _)| ends_with(&path, tail))
            {
                for &i in *positions {
                    let Some(arg) = args.get(i) else { continue };
                    let Some(text) = text_of(self.source, arg.span) else {
                        continue;
                    };
                    let tight = matches!(
                        arg.kind,
                        ExprKind::Path(_)
                            | ExprKind::Literal(_)
                            | ExprKind::Call { .. }
                            | ExprKind::MethodCall { .. }
                            | ExprKind::FieldAccess { .. }
                            | ExprKind::Index { .. }
                            | ExprKind::Tuple(_)
                    );
                    let replacement = if tight {
                        format!("{text}.as_bytes()")
                    } else {
                        format!("({text}).as_bytes()")
                    };
                    self.out.push(Fix {
                        span: arg.span,
                        replacement,
                        lint_id: "hash-inputs-are-bytes",
                    });
                }
            }
            walk_expr(self, expr);
        }
    }
    Scan { source, out }.visit_source_file(sf);
}

/// Proposes `(call as i64)` for each checksum call, and `(seed as u32)` for
/// the running value an `update` takes, where the program used `i64`.
fn collect_checksum_width(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    use gossamer_ast::ExprKind;
    use gossamer_ast::visitor::{Visitor, walk_expr};

    struct Scan<'a> {
        source: &'a str,
        out: &'a mut Vec<Fix>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &gossamer_ast::Expr) {
            if let ExprKind::Call { callee, args } = &expr.kind
                && let Some(path) = callee_path(callee)
                && path.len() >= 2
            {
                let module = path[path.len() - 2];
                let function = path[path.len() - 1];
                let checksum = matches!(module, "crc32" | "crc32c" | "adler32")
                    && matches!(
                        function,
                        "checksum" | "checksum_string" | "update" | "update_window"
                    )
                    || (module == "fnv" && function == "hash32");
                if checksum && let Some(text) = text_of(self.source, expr.span) {
                    self.out.push(Fix {
                        span: expr.span,
                        replacement: format!("({text} as i64)"),
                        lint_id: "checksums-are-u32",
                    });
                    if function.starts_with("update")
                        && let Some(seed) = args.first()
                        && let Some(seed_text) = text_of(self.source, seed.span)
                    {
                        self.out.push(Fix {
                            span: seed.span,
                            replacement: format!("({seed_text} as u32)"),
                            lint_id: "checksums-are-u32",
                        });
                    }
                }
            }
            walk_expr(self, expr);
        }
    }
    Scan { source, out }.visit_source_file(sf);
}

/// Proposes `(recv as f64).pow(e)` for every `recv.pow(e)` whose receiver is
/// not already a float spelling. Before 0.65.0 an integer receiver reached
/// the `math::pow` row and answered `f64`; a caller that used that `f64` (or
/// passed a float exponent) no longer checks, and the cast restores it.
fn collect_integer_pow_to_float(sf: &SourceFile, source: &str, out: &mut Vec<Fix>) {
    use gossamer_ast::visitor::{Visitor, walk_expr};
    use gossamer_ast::{Expr, ExprKind, Literal};

    struct Scan<'a> {
        source: &'a str,
        out: &'a mut Vec<Fix>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let ExprKind::MethodCall { receiver, name, .. } = &expr.kind
                && name.name == "pow"
            {
                let floaty = match &receiver.kind {
                    ExprKind::Literal(Literal::Float(_)) => true,
                    ExprKind::Cast { ty, .. } => {
                        matches!(
                            self.source
                                .get(ty.span.start as usize..ty.span.end as usize),
                            Some("f64" | "f32")
                        )
                    }
                    _ => false,
                };
                if !floaty
                    && let Some(text) = self
                        .source
                        .get(receiver.span.start as usize..receiver.span.end as usize)
                {
                    self.out.push(Fix {
                        span: receiver.span,
                        replacement: format!("({text} as f64)"),
                        lint_id: "integer-pow-to-float",
                    });
                }
            }
            walk_expr(self, expr);
        }
    }
    Scan { source, out }.visit_source_file(sf);
}

/// Looks up a rewriter by id.
#[must_use]
pub fn rewriter(id: &str) -> Option<&'static Rewriter> {
    REWRITERS.iter().find(|r| r.id == id)
}

/// Collects the edits every rewriter in `selected` would make.
#[must_use]
pub fn migrations(sf: &SourceFile, source: &str, selected: &[&Rewriter]) -> Vec<Fix> {
    let mut out = Vec::new();
    for rewriter in selected {
        (rewriter.collect)(sf, source, &mut out);
    }
    out
}
