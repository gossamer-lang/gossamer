#![allow(clippy::wildcard_imports)]
use super::*;

/// Tail-node ceiling for an inlinable function: a callee whose tail
/// expression weighs more than this stays a real `Op::Call`. Bounds the
/// per-call-site code growth. One unit per expression node.
const INLINE_TAIL_COST_LIMIT: usize = 24;

/// Total inlined-node budget one caller may accrue before further
/// inlines into it fall back to real calls. Caps code blow-up when a hot
/// function inlines many small helpers (transitively).
const INLINE_CALLER_BUDGET: usize = 96;

/// Returns `false` when `GOSSAMER_INLINE=0` (or `false`) is set, so the
/// differential harness can compare a program's output with the
/// bytecode inliner on and off. Read live (not memoised) so a test can
/// flip it between runs. Mirrors the MIR tier's `inlining_enabled`.
fn inlining_enabled() -> bool {
    !matches!(
        std::env::var("GOSSAMER_INLINE").ok().as_deref(),
        Some("0" | "false")
    )
}

/// True when `callee` names an explicit diagnostic builtin (`panic` /
/// `assert` / `assert_eq`). A function that calls one is a traceback
/// boundary: inlining it would drop its frame from the panic call-stack
/// snapshot, so such a function stays a real `Op::Call`. Hot-path
/// numeric helpers never call these, so the inline win is unaffected.
fn callee_is_diagnostic(callee: &HirExpr) -> bool {
    let HirExprKind::Path { segments, .. } = &callee.kind else {
        return false;
    };
    matches!(
        segments.as_slice(),
        [seg] if matches!(seg.name.as_str(), "panic" | "assert" | "assert_eq")
    )
}

/// Weighted node count of `expr`, or `None` when `expr` contains a
/// construct that is unsafe to re-compile at a call site. The whitelist
/// admits only side-effect-transparent, control-flow-free shapes:
/// literals, name reads, arithmetic, casts, indexing, field / tuple
/// access, and nested calls (which may themselves inline). Everything
/// else - control flow (`if` / `match` / loops / `return` / `break` /
/// `continue`), closures, `go` / `select`, assignment, and method calls
/// - rejects the function, keeping the MVP correctness-first.
fn tail_inline_cost(expr: &HirExpr) -> Option<usize> {
    use HirExprKind as K;
    let children = match &expr.kind {
        K::Literal(_) | K::Path { .. } => 0,
        K::Binary { lhs, rhs, .. } => tail_inline_cost(lhs)? + tail_inline_cost(rhs)?,
        K::Unary { operand, .. } => tail_inline_cost(operand)?,
        K::Cast { value, .. } => tail_inline_cost(value)?,
        K::Index { base, index } => tail_inline_cost(base)? + tail_inline_cost(index)?,
        K::Field { receiver, .. } | K::TupleIndex { receiver, .. } => tail_inline_cost(receiver)?,
        K::Call { callee, args } => {
            if callee_is_diagnostic(callee) {
                return None;
            }
            let mut sum = tail_inline_cost(callee)?;
            for arg in args {
                sum += tail_inline_cost(arg)?;
            }
            sum
        }
        _ => return None,
    };
    Some(1 + children)
}

/// Cost of a straight-line statement that can be safely replayed at an inline
/// call site.  A bare-path assignment covers both local scratch bindings and
/// `static mut` updates such as a tiny PRNG step; the isolated callee scope
/// preserves name hygiene and the normal assignment compiler still decides
/// whether the path is local or static.
fn stmt_inline_cost(stmt: &HirStmt) -> Option<usize> {
    match &stmt.kind {
        HirStmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } if matches!(pattern.kind, HirPatKind::Binding { .. }) => {
            Some(1 + tail_inline_cost(init)?)
        }
        HirStmtKind::Expr { expr, .. } => match &expr.kind {
            HirExprKind::Assign { place, value }
                if matches!(place.kind, HirExprKind::Path { .. }) =>
            {
                Some(1 + tail_inline_cost(value)?)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Recognises a user function the bytecode compiler may inline at its
/// call sites: a free function (no `self` receiver) whose body is a
/// single tail expression (no statements), every parameter a plain
/// by-value binding (no `&mut` write-back shape), and whose tail passes
/// [`tail_inline_cost`] under [`INLINE_TAIL_COST_LIMIT`]. Returns an
/// owned snapshot so the inliner never re-borrows the `HirProgram`.
pub(crate) fn detect_inlinable_fn(decl: &HirFn, tcx: &TyCtxt) -> Option<InlinableFn> {
    if decl.has_self {
        return None;
    }
    let body = decl.body.as_ref()?;
    let tail = body.block.tail.as_deref()?;
    for param in &decl.params {
        if !matches!(param.pattern.kind, HirPatKind::Binding { .. }) {
            return None;
        }
        // A `&mut Vec` / `&mut [T]` / `&mut <scalar>` parameter rides the
        // write-back cell protocol; inlining its body would drop the
        // caller-visible mutation, so leave it a real call.
        if is_mut_ref_writeback(tcx, param.ty) {
            return None;
        }
    }
    let stmt_cost = body
        .block
        .stmts
        .iter()
        .try_fold(0usize, |sum, stmt| Some(sum + stmt_inline_cost(stmt)?))?;
    let cost = stmt_cost + tail_inline_cost(tail)?;
    if cost > INLINE_TAIL_COST_LIMIT {
        return None;
    }
    Some(InlinableFn {
        params: decl.params.iter().map(|p| p.pattern.clone()).collect(),
        stmts: body.block.stmts.clone(),
        tail: tail.clone(),
        cost,
    })
}

impl<'tcx> FnBuilder<'tcx> {
    /// Inlines a call to a detected [`InlinableFn`] by re-compiling the
    /// callee's tail expression directly into the caller, with the
    /// callee's parameters bound to the already-compiled argument
    /// registers. Returns `Some(tail_reg)` - preserving the tail's
    /// `RegKind` so a numeric result stays unboxed - or `None` when the
    /// call is not inlinable (unknown callee, arity mismatch, recursion,
    /// or budget exhausted), in which case the caller emits a real
    /// `Op::Call`.
    ///
    /// The callee body compiles in an isolated scope stack containing
    /// only its parameters, so a body reference to a global never
    /// accidentally resolves to a caller local of the same name
    /// (inlining hygiene). Register allocation stays shared: the inlined
    /// body's temporaries live in the caller's register file.
    pub(crate) fn try_inline_user_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<TypedReg>> {
        if !inlining_enabled() {
            return Ok(None);
        }
        // Only a bare single-segment path naming a free function inlines.
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        let [seg] = segments.as_slice() else {
            return Ok(None);
        };
        // A local of the same name shadows the global function (a
        // fn-pointer binding): leave dispatch to the value in the local.
        if self.lookup_local(seg.name.as_str()).is_some() {
            return Ok(None);
        }
        // Copy the `'tcx` map reference out of `self` first so the looked
        // up `&InlinableFn` is independent of the `&mut self` borrows
        // below - no clone of the callee's HIR is needed.
        let fns: &'tcx InlinableFns = self.inline_fns;
        let Some(info) = fns.get(seg.name.as_str()) else {
            return Ok(None);
        };
        if info.params.len() != args.len() {
            return Ok(None);
        }
        // Recursion guard: never inline the function currently being
        // compiled into itself, nor any function already on the inline
        // stack (covers direct, mutual, and transitive recursion).
        let name = crate::value::intern_type_name(seg.name.as_str());
        if name == self.name || self.inlining.contains(&name) {
            return Ok(None);
        }
        // Per-caller budget: once exhausted, further calls stay real.
        if self.inlined_nodes + info.cost > INLINE_CALLER_BUDGET {
            return Ok(None);
        }
        // Evaluate every argument once, left-to-right, in the caller's
        // current scope - matching call-by-value evaluation order.
        let mut arg_regs: Vec<TypedReg> = Vec::with_capacity(args.len());
        for arg in args {
            arg_regs.push(self.compile_expr_ex(arg)?);
        }
        self.inlining.push(name);
        self.inlined_nodes += info.cost;
        // The callee's instructions carry the call they came from, so a
        // traceback through them names the callee's frame and the caller's
        // call site.
        let site = u32::try_from(self.inline_sites.len()).unwrap_or(u32::MAX);
        self.inline_sites.push(crate::bytecode::InlineSite {
            function: name,
            call: self.source_location(callee.span),
            parent: self.current_inline_site,
        });
        let caller_site = self.current_inline_site.replace(site);
        // Swap in a fresh scope stack holding only the parameters; the
        // caller's locals are invisible to the callee body.
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![Scope::default()]);
        // The consumability set was computed for the caller, not this
        // callee body, so suppress consuming ops while the callee
        // compiles - a callee local sharing a caller name must not be
        // moved on the caller's read-once proof.
        let saved_consumable = std::mem::take(&mut self.consumable);
        // The callee's own bindings never need a capture cell: an
        // inlinable body holds no closure. Its scope stack is swapped
        // out, so record the caller's active cells and restore them
        // rather than relying on `pop_scope`. The caller's cells stay
        // active meanwhile: an argument register may be a caller
        // binding's home.
        let saved_cell_names = std::mem::take(&mut self.capture_cell_names);
        let capture_cell_mark = self.capture_cells.len();
        for (idx, (pattern, arg)) in info.params.iter().zip(arg_regs.iter()).enumerate() {
            if let HirPatKind::Binding {
                name: param_name,
                mutable,
            } = &pattern.kind
            {
                // A `Map` / `Set` / slot-container argument, and a struct
                // carrying one, reaches an ordinary call as a value of its own
                // unless the callee only reads it. An inlined body owes the
                // caller the same: without the copy the callee's write, or a
                // field it hands back, lands on the caller's own table.
                let takes_own_value = args.get(idx).is_some_and(|a| {
                    is_path_expr(a)
                        && (self.expr_is_map(a)
                            || self.expr_is_hashset(a)
                            || self.expr_is_slot_container(a)
                            || self.expr_is_aggregate_with_container(a))
                        && !self.callee_only_reads_param(callee, idx)
                        && !matches!(self.tcx.kind(a.ty), Some(TyKind::Ref { .. }))
                });
                // A parameter the callee may write gets a register of
                // its own, the way a `let` binding does. Binding it to
                // the argument register is what makes the read-only
                // case free, and is exactly what a `mut` parameter must
                // not do: it takes the caller's value, not the caller's
                // variable.
                let bound = if takes_own_value {
                    let dst = self.alloc_reg();
                    self.emit(Op::CloneMapLike { dst, src: arg.reg });
                    TypedReg {
                        reg: dst,
                        kind: RegKind::Value,
                    }
                } else if *mutable {
                    self.bind_to_fresh(*arg)
                } else {
                    *arg
                };
                self.bind_local(&param_name.name, bound);
            }
        }
        let result = (|| {
            for stmt in &info.stmts {
                // `stmt_inline_cost` rejects every diverging statement shape.
                let diverges = self.compile_stmt(stmt)?;
                debug_assert!(!diverges);
            }
            self.compile_expr_ex(&info.tail)
        })();
        // Restore the caller's scopes and pop the recursion stack on every
        // exit path, including the error path, before surfacing the result.
        self.scopes = saved_scopes;
        self.consumable = saved_consumable;
        self.capture_cell_names = saved_cell_names;
        self.capture_cells.truncate(capture_cell_mark);
        self.current_inline_site = caller_site;
        self.inlining.pop();
        Ok(Some(result?))
    }
}

#[cfg(test)]
mod tests {
    use super::detect_inlinable_fn;
    use gossamer_hir::{HirItemKind, lower_source_file};
    use gossamer_lex::SourceMap;
    use gossamer_parse::parse_source_file;
    use gossamer_resolve::resolve_source_file;
    use gossamer_types::{TyCtxt, typecheck_source_file};

    #[test]
    fn straight_line_static_update_is_inlinable() {
        let source = r"
static mut STATE: i64 = 1
fn next() -> i64 {
    STATE = STATE * 3 + 1
    STATE >> 1
}
";
        let mut map = SourceMap::new();
        let file = map.add_file("inline_static.gos", source.to_string());
        let (mut sf, parse_diags) = parse_source_file(source, file);
        assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
        let (resolutions, resolve_diags) = resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
        assert!(resolve_diags.is_empty(), "resolve: {resolve_diags:?}");
        let mut tcx = TyCtxt::new();
        let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
        assert!(type_diags.is_empty(), "typecheck: {type_diags:?}");
        let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
        let next = program
            .items
            .iter()
            .find_map(|item| match &item.kind {
                HirItemKind::Fn(decl) if decl.name.name.as_str() == "next" => Some(decl),
                _ => None,
            })
            .expect("next function");
        let inline = detect_inlinable_fn(next, &tcx).expect("straight-line helper should inline");
        assert_eq!(inline.stmts.len(), 1, "the state update must be replayed");
    }
}
