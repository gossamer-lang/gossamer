//! Compile-time evaluation ("comptime") region discovery.
//!
//! A `comptime { ... }` block and every call to a `comptime fn` are
//! evaluated on the bytecode VM during compilation and replaced in the
//! source with their result literal, so the bytecode VM, the Cranelift
//! JIT, and the LLVM AOT backend all see a constant rather than the
//! original computation. This module finds the outermost such regions;
//! the VM evaluates them in [`crate::vm`], and [`fold_into_source`]
//! splices the results back into the source for every driver (the CLI
//! commands and the wasm playground alike).

use std::collections::{HashMap, HashSet};

use gossamer_hir::{
    HirArrayExpr, HirExpr, HirExprKind, HirItemKind, HirProgram, HirSelectOp, HirStmt, HirStmtKind,
};
use gossamer_lex::Span;

use crate::value::Value;

/// Evaluates every comptime region of `program` on a throwaway VM and
/// returns `augmented` with each region's source span replaced by its
/// result literal. `program` must be the front-end lowering of
/// `augmented` itself; `file_label` names the source in error messages.
/// Returns `Err` when a region fails to evaluate or its result is not
/// spliceable (a raw `codegen!` result that is not a `String`, or a
/// non-scalar, non-string value).
pub fn fold_into_source(
    program: &HirProgram,
    tcx: gossamer_types::TyCtxt,
    augmented: &str,
    file_label: &str,
) -> Result<String, String> {
    fold_into_source_anchored(program, tcx, augmented, file_label, file_label, Vec::new())
}

/// Same as [`fold_into_source`], with the directory that relative
/// paths resolve against - and that a `confined` read may not escape -
/// taken from `anchor_source` rather than from `file_label`.
///
/// A front end whose label is not a filesystem path, such as the
/// editor's, names the real document here so an embedded asset resolves
/// the same file the command line reads.
///
/// `origins` names the file each `(start, end)` byte range of an assembled
/// unit was written in, outermost first; a region inside one reads relative
/// to that file instead of `anchor_source`.
pub fn fold_into_source_anchored(
    program: &HirProgram,
    tcx: gossamer_types::TyCtxt,
    augmented: &str,
    file_label: &str,
    anchor_source: &str,
    origins: Vec<(u32, u32, String)>,
) -> Result<String, String> {
    // An embedded asset belongs to the source that embeds it, so a
    // relative path inside a `comptime` region reads the same file
    // whatever directory the build was started from.
    let _anchor = gossamer_runtime::comptime_paths::Anchored::at_source(anchor_source);
    // A compile-time region runs with the privileges of whoever
    // started the compile, so the capability policy is in force for
    // the length of the fold and the source's own directory is the
    // root a confined read may not escape.
    let _confinement = gossamer_runtime::comptime_policy::Confined::at_source(anchor_source);
    // Whatever this fold reads becomes an input of the build that
    // folded it, so the set is collected from here and read back with
    // `comptime_inputs::take`.
    gossamer_runtime::comptime_inputs::begin();
    let mut vm = crate::vm::Vm::new();
    vm.set_collect_comptime(true);
    vm.set_comptime_origins(origins);
    vm.set_comptime_gate(true);
    vm.load(program, tcx, false)
        .map_err(|err| format!("comptime evaluation failed: {err}"))?;
    let folds = vm.take_comptime_folds();

    // Apply replacements right-to-left so earlier byte offsets stay
    // valid as later regions are spliced. Outermost regions never
    // overlap, so a stable descending sort by start is sufficient.
    let mut repls: Vec<(usize, usize, String)> = Vec::with_capacity(folds.len());
    for (span, raw, outcome) in folds {
        let start = span.start as usize;
        let end = span.end as usize;
        let literal = match outcome {
            // A raw (`codegen!`) region splices its `String` result
            // verbatim as source, so reflection-driven `comptime fn`s
            // emit ordinary code compiled natively on every tier.
            Ok(Value::String(s)) if raw => s.as_str().to_string(),
            Ok(_) if raw => {
                return Err(format!(
                    "{}: codegen! result must be a string of source",
                    locate(augmented, file_label, start)
                ));
            }
            Ok(value) => render_literal(&value).ok_or_else(|| {
                format!(
                    "{}: comptime result must be a scalar or string",
                    locate(augmented, file_label, start)
                )
            })?,
            Err(message) => {
                return Err(format!(
                    "{}: {message}",
                    locate(augmented, file_label, start)
                ));
            }
        };
        repls.push((start, end, literal));
    }
    repls.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));

    let mut folded = augmented.to_string();
    for (start, end, literal) in repls {
        folded.replace_range(start..end, &literal);
    }
    Ok(folded)
}

/// Renders a comptime result value as a Gossamer source literal, or
/// `None` when the value is not a scalar or string (the P0 boundary).
fn render_literal(value: &Value) -> Option<String> {
    Some(match value {
        Value::Unit | Value::Void => "()".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Uint(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => {
            // `{:?}` renders f64 with a decimal point (`64.0`, not
            // `64`) so the spliced literal re-parses as a float and
            // round-trips to the same value.
            format!("{f:?}")
        }
        Value::Char(c) => format!("'{}'", escape_char(*c)),
        Value::String(s) => format!("\"{}\"", escape_string(s.as_str())),
        _ => return None,
    })
}

fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_char(c: char) -> String {
    match c {
        '\\' => "\\\\".to_string(),
        '\'' => "\\'".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        _ => c.to_string(),
    }
}

/// Renders `file:line:col` for the byte offset `pos` in `source`.
fn locate(source: &str, file_label: &str, pos: usize) -> String {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i >= pos {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    format!("{file_label}:{line}:{col}")
}

/// One comptime region: the expression to evaluate and the source span
/// it occupies (so the result literal can be spliced over it).
pub(crate) struct Region<'a> {
    /// Source range covering the whole region (`comptime { ... }`, a
    /// `comptime fn` call, or a `comptime` parameter's argument).
    pub span: Span,
    /// Expression handed to the VM evaluator.
    pub expr: &'a HirExpr,
    /// When set, the region's `String` result is spliced as raw source
    /// (code emission via `codegen!`) rather than rendered as a quoted
    /// literal. A non-`String` result for a raw region is an error.
    pub raw: bool,
}

/// Comptime metadata gathered from the program: which free functions
/// are `comptime fn` (whole-call folding) and which functions have
/// `comptime` parameters (per-argument folding by position).
pub(crate) struct ComptimeInfo {
    fns: HashSet<String>,
    params: HashMap<String, Vec<usize>>,
}

impl ComptimeInfo {
    /// Collects comptime metadata from every function in the program.
    pub(crate) fn collect(program: &HirProgram) -> Self {
        let mut fns = HashSet::new();
        let mut params = HashMap::new();
        for item in &program.items {
            if let HirItemKind::Fn(f) = &item.kind {
                if f.is_comptime {
                    fns.insert(f.name.name.clone());
                }
                let indices: Vec<usize> = f
                    .params
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.is_comptime)
                    .map(|(i, _)| i)
                    .collect();
                if !indices.is_empty() {
                    params.insert(f.name.name.clone(), indices);
                }
            }
        }
        Self { fns, params }
    }

    /// `true` when a call to `name` folds in its entirety (a `comptime fn`).
    fn is_comptime_fn(&self, name: &str) -> bool {
        self.fns.contains(name)
    }
}

/// Collects the outermost comptime regions across every non-comptime
/// function body, impl method, and const/static initializer. Regions
/// nested inside another comptime region are not collected separately:
/// the enclosing region's evaluation already covers them.
pub(crate) fn collect_regions<'a>(program: &'a HirProgram, info: &ComptimeInfo) -> Vec<Region<'a>> {
    let mut out = Vec::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(f) => {
                // A comptime fn's own body is folded at its call sites,
                // not in isolation (its body may reference parameters
                // that are only comptime-known per call).
                if !f.is_comptime
                    && let Some(body) = &f.body
                {
                    walk_block_stmts(&body.block.stmts, info, &mut out);
                    if let Some(tail) = &body.block.tail {
                        walk(tail, info, &mut out);
                    }
                }
            }
            HirItemKind::Impl(imp) => {
                for m in &imp.methods {
                    if !m.is_comptime
                        && let Some(body) = &m.body
                    {
                        walk_block_stmts(&body.block.stmts, info, &mut out);
                        if let Some(tail) = &body.block.tail {
                            walk(tail, info, &mut out);
                        }
                    }
                }
            }
            HirItemKind::Const(c) => walk(&c.value, info, &mut out),
            HirItemKind::Static(s) => walk(&s.value, info, &mut out),
            _ => {}
        }
    }
    out
}

/// Returns the callee's leaf path name when `expr` is a direct call.
fn call_callee_name(expr: &HirExpr) -> Option<&str> {
    let HirExprKind::Call { callee, .. } = &expr.kind else {
        return None;
    };
    let HirExprKind::Path { segments, .. } = &callee.kind else {
        return None;
    };
    segments.last().map(|s| s.name.as_str())
}

/// True when `expr` folds in its entirety: a `comptime { ... }` block or
/// a call to a `comptime fn`.
fn is_region_root(expr: &HirExpr, info: &ComptimeInfo) -> bool {
    match &expr.kind {
        HirExprKind::Block(block) => block.is_comptime,
        HirExprKind::Call { .. } => call_callee_name(expr).is_some_and(|n| info.is_comptime_fn(n)),
        _ => false,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive match over every HIR expression kind; splitting the traversal would scatter it"
)]
fn walk<'a>(expr: &'a HirExpr, info: &ComptimeInfo, out: &mut Vec<Region<'a>>) {
    if is_region_root(expr, info) {
        let raw = call_callee_name(expr) == Some("__gos_codegen");
        out.push(Region {
            span: expr.span,
            expr,
            raw,
        });
        return;
    }
    match &expr.kind {
        HirExprKind::Call { callee, args } => {
            walk(callee, info, out);
            // A `comptime` parameter folds just its argument at the call
            // site: push the argument as a region, and recurse into the
            // remaining (runtime) arguments normally.
            let comptime_indices = call_callee_name(expr).and_then(|n| info.params.get(n));
            for (i, arg) in args.iter().enumerate() {
                if comptime_indices.is_some_and(|idx| idx.contains(&i)) {
                    out.push(Region {
                        span: arg.span,
                        expr: arg,
                        raw: false,
                    });
                } else {
                    walk(arg, info, out);
                }
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            walk(receiver, info, out);
            for arg in args {
                walk(arg, info, out);
            }
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            walk(receiver, info, out);
        }
        HirExprKind::Index { base, index } => {
            walk(base, info, out);
            walk(index, info, out);
        }
        HirExprKind::Unary { operand, .. } => walk(operand, info, out),
        HirExprKind::Binary { lhs, rhs, .. } => {
            walk(lhs, info, out);
            walk(rhs, info, out);
        }
        HirExprKind::Assign { place, value } => {
            walk(place, info, out);
            walk(value, info, out);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk(condition, info, out);
            walk(then_branch, info, out);
            if let Some(e) = else_branch {
                walk(e, info, out);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            walk(scrutinee, info, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    walk(guard, info, out);
                }
                walk(&arm.body, info, out);
            }
        }
        HirExprKind::Loop { body, .. } | HirExprKind::While { body, .. } => {
            walk(body, info, out);
        }
        HirExprKind::Block(block) => {
            walk_block_stmts(&block.stmts, info, out);
            if let Some(tail) = &block.tail {
                walk(tail, info, out);
            }
        }
        HirExprKind::Return(Some(inner))
        | HirExprKind::Break {
            value: Some(inner), ..
        }
        | HirExprKind::Cast { value: inner, .. } => walk(inner, info, out),
        HirExprKind::Tuple(elems) | HirExprKind::Array(HirArrayExpr::List(elems)) => {
            for e in elems {
                walk(e, info, out);
            }
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            walk(value, info, out);
            walk(count, info, out);
        }
        HirExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk(s, info, out);
            }
            if let Some(e) = end {
                walk(e, info, out);
            }
        }
        HirExprKind::Closure { body, .. } => walk(body, info, out),
        HirExprKind::LiftedClosure { captures, .. } => {
            for c in captures {
                walk(c, info, out);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &arm.op {
                    HirSelectOp::Recv { channel, .. } => walk(channel, info, out),
                    HirSelectOp::Send { channel, value } => {
                        walk(channel, info, out);
                        walk(value, info, out);
                    }
                    HirSelectOp::Default => {}
                }
                walk(&arm.body, info, out);
            }
        }
        HirExprKind::Literal(_)
        | HirExprKind::Path { .. }
        | HirExprKind::Continue { .. }
        | HirExprKind::Placeholder
        | HirExprKind::Return(None)
        | HirExprKind::Break { value: None, .. } => {}
    }
}

fn walk_block_stmts<'a>(stmts: &'a [HirStmt], info: &ComptimeInfo, out: &mut Vec<Region<'a>>) {
    for stmt in stmts {
        match &stmt.kind {
            HirStmtKind::Let {
                init: Some(expr), ..
            } => walk(expr, info, out),
            HirStmtKind::Expr { expr, .. } => walk(expr, info, out),
            HirStmtKind::Defer(inner) => walk(inner, info, out),
            HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
        }
    }
}
