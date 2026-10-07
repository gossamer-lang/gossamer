//! Patterns, iteration, and parameter shapes.

use super::{
    ArrayExpr, AstType, BINARY_HEAP_DEF_LOCAL, BTREE_SET_DEF_LOCAL, Block, ClosureParam, DefId,
    Expectation, Expr, ExprKind, FLOAT_SUFFIXES, FloatTy, FloatWidth, FnSig, HASH_SET_DEF_LOCAL,
    HashSet, INT_SUFFIXES, IntTy, Literal, LiteralConsts, MIN_HEAP_DEF_LOCAL, MatchArm, Mutbl,
    NodeId, OPTION_DEF_LOCAL, Pattern, PatternKind, PlainLetPatternProblem, PrimitiveTy,
    RESULT_DEF_LOCAL, REVERSE_DEF_LOCAL, Resolution, Span, Stmt, StmtKind, Ty, TyKind, TypeChecker,
    TypeError, UnaryOp, VEC_DEQUE_DEF_LOCAL, VEC_QUEUE_DEF_LOCAL, VEC_STACK_DEF_LOCAL,
    closure_bound_names, default_value_spelling, expr_display, expr_mentions_any_name,
    goroutine_bodies, int_assoc_const, int_literal_fits, int_ty_from_width, is_opaque_handle_def,
    is_stable_borrow_place, json_value_variant_of, parse_int_magnitude, pattern_binding_names,
    stmt_diverges,
};

impl TypeChecker<'_> {
    /// Rejects `json::Value::Object(..)` / `::Int(..)` / `::Null` (etc.)
    /// constructor patterns in `match` / `if let` / `while let` arms. The
    /// `json::Value` handle carries no matchable discriminant across the
    /// tiers, so such a pattern silently falls through on the VM and faults
    /// on the compiled tiers; the dynamic accessor API (`json::as_*` /
    /// `json::get` / `json::keys`) is the supported way to read a document.
    /// The pattern is judged structurally (not on the scrutinee type) so a
    /// scrutinee left unresolved by inference is still caught - a
    /// `json::Value::X` path is never a valid matchable pattern.
    pub(super) fn reject_json_value_variant_patterns(&mut self, arms: &[MatchArm]) {
        for arm in arms {
            self.reject_json_value_variant_pattern(&arm.pattern);
        }
    }

    /// Emits GT0027 for a single `json::Value::X` constructor pattern,
    /// recursing through or-patterns so every alternative is flagged.
    pub(super) fn reject_json_value_variant_pattern(&mut self, pattern: &Pattern) {
        let path = match &pattern.kind {
            PatternKind::TupleStruct { path, .. }
            | PatternKind::Path(path)
            | PatternKind::Struct { path, .. } => Some(path),
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.reject_json_value_variant_pattern(alt);
                }
                None
            }
            _ => None,
        };
        if let Some(path) = path {
            if let Some(variant) = json_value_variant_of(path) {
                self.emit(
                    TypeError::JsonValuePatternUnsupported {
                        variant: variant.to_string(),
                    },
                    pattern.span,
                );
            }
        }
    }

    /// Type-checks a `select { … }` expression. Each arm is checked in its own
    /// scope: a recv arm binds its pattern to the channel's element type, a
    /// send arm unifies the sent value against the channel's element type, and
    /// every arm body unifies into the shared result type.
    pub(super) fn check_select(&mut self, arms: &[gossamer_ast::SelectArm]) -> Ty {
        use gossamer_ast::SelectOp;
        let result_ty = self.fresh();
        for arm in arms {
            self.push_scope();
            match &arm.op {
                SelectOp::Recv { pattern, channel } => {
                    let chan_ty = self.check_expr(channel);
                    let resolved = self.infer.resolve(self.tcx, chan_ty);
                    let elem = match self.tcx.kind_of(resolved).clone() {
                        TyKind::Receiver(inner) | TyKind::Sender(inner) => inner,
                        _ => self.fresh(),
                    };
                    let pat_ty = self.type_of_pattern(pattern);
                    self.unify(pat_ty, elem, pattern.span);
                    self.bind_pattern(pattern, elem);
                }
                SelectOp::Send { channel, value } => {
                    let chan_ty = self.check_expr(channel);
                    let resolved = self.infer.resolve(self.tcx, chan_ty);
                    let elem = match self.tcx.kind_of(resolved).clone() {
                        TyKind::Sender(inner) | TyKind::Receiver(inner) => inner,
                        _ => self.fresh(),
                    };
                    let val_ty = self.check_expr(value);
                    self.unify(elem, val_ty, value.span);
                }
                SelectOp::Default => {}
            }
            let body_ty = self.check_expr(&arm.body);
            self.unify(result_ty, body_ty, arm.body.span);
            self.pop_scope();
        }
        result_ty
    }

    pub(super) fn check_for(&mut self, pattern: &Pattern, iter: &Expr, body: &Expr) -> Ty {
        let iter_ty = self.check_expr(iter);
        self.reject_unbounded_generic_iteration(iter_ty, iter.span);
        self.reject_wrapper_iteration(iter_ty, iter.span);
        self.mark_consumed_iterator_expr("into_iter", iter, iter_ty);
        self.push_scope();
        // Derive the pattern's type from the iterator: arrays/slices
        // yield their element type, ranges over integers yield the
        // integer type. When the iterator is itself a method call
        // (`xs.iter()`, `xs.into_iter()`) whose return type is an
        // unresolved inference variable, fall back to looking at
        // the method's receiver - `.iter()` and friends always
        // produce the receiver's element type, regardless of which
        // wrapper they technically return.
        let derived = {
            let starting = self.infer.resolve(self.tcx, iter_ty);
            let is_var = matches!(self.tcx.kind(starting), Some(TyKind::Var(_)));
            let starting = if is_var {
                // Only `.iter()` / `.into_iter()` produce the receiver's
                // element type. Other methods (`m.get_or(k, d)`,
                // `m.values()`) return a different shape, so falling back to
                // the receiver there would derive the wrong element type.
                if let ExprKind::MethodCall { receiver, name, .. } = &iter.kind
                    && matches!(name.name.as_str(), "iter" | "into_iter")
                {
                    let recv_ty = self.check_expr(receiver);
                    self.infer.resolve(self.tcx, recv_ty)
                } else {
                    starting
                }
            } else {
                starting
            };
            let mut cur = starting;
            loop {
                match self.tcx.kind_of(cur).clone() {
                    TyKind::Ref { inner, mutability } => {
                        // Resolve the referent first: a `&mut` to a container
                        // whose element type is still being inferred would
                        // otherwise read as an opaque variable and leave the
                        // loop binding untyped.
                        let inner = self.infer.resolve(self.tcx, inner);
                        match self.tcx.kind_of(inner).clone() {
                            TyKind::Array { elem, .. }
                            | TyKind::Slice(elem)
                            | TyKind::Vec(elem) => {
                                // A shared borrow yields the same element binding
                                // the owned sequence does; only `&mut` keeps the
                                // reference, which is what carries a write through
                                // to the source.
                                if mutability == crate::Mutbl::Not {
                                    break Some(elem);
                                }
                                break Some(self.tcx.intern(TyKind::Ref {
                                    mutability,
                                    inner: elem,
                                }));
                            }
                            TyKind::Tuple(elems) => {
                                let Some(elem) = elems.first().copied() else {
                                    break None;
                                };
                                break Some(self.tcx.intern(TyKind::Ref {
                                    mutability,
                                    inner: elem,
                                }));
                            }
                            _ => cur = inner,
                        }
                    }
                    TyKind::Array { elem, .. }
                    | TyKind::Slice(elem)
                    | TyKind::Vec(elem)
                    | TyKind::Iterator(elem)
                    | TyKind::Range(elem) => {
                        break Some(elem);
                    }
                    TyKind::String => break Some(self.tcx.char_ty()),
                    // A `Set` / `BTreeSet` walked directly yields its elements,
                    // the same values its `iter()` cursor hands over.
                    TyKind::Adt { def, substs }
                        if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
                    {
                        let Some(elem) = substs.types().first().copied() else {
                            break None;
                        };
                        break Some(self.infer.resolve(self.tcx, elem));
                    }
                    // A map walked directly yields the same `(key, value)`
                    // pair its `iter()` cursor does, so a bare `for (k, v) in
                    // m` binds the map's own key and value types.
                    TyKind::HashMap { key, value, .. } => {
                        let key = self.infer.resolve(self.tcx, key);
                        let value = self.infer.resolve(self.tcx, value);
                        break Some(self.tcx.intern(TyKind::Tuple(vec![key, value])));
                    }
                    TyKind::Tuple(elems) => {
                        let Some(elem) = elems.first().copied() else {
                            break None;
                        };
                        break Some(elem);
                    }
                    _ => break None,
                }
            }
        };
        let pat_ty = match derived {
            Some(t) => {
                let p = self.type_of_pattern(pattern);
                let pattern_target = match (&pattern.kind, self.tcx.kind_of(t).clone()) {
                    (PatternKind::Tuple(_), TyKind::Ref { inner, .. }) => {
                        self.infer.resolve(self.tcx, inner)
                    }
                    _ => t,
                };
                self.unify(p, pattern_target, pattern.span);
                t
            }
            None => self.type_of_pattern(pattern),
        };
        self.reject_refutable_for_pattern(pattern, pat_ty);
        self.bind_pattern(pattern, pat_ty);
        self.check_discarded_expr(body);
        self.report_discarded_result(body, None);
        self.pop_scope();
        self.tcx.unit()
    }

    /// Reports GT0064 when `expr` discards a value of a `#[must_use]` type
    /// or the result of a call to a `#[must_use]` function. Returns whether
    /// a report was made.
    pub(super) fn report_discarded_must_use(&mut self, expr: &Expr, ty: Option<Ty>) -> bool {
        if let Some(ty) = ty {
            let resolved = self.infer.resolve(self.tcx, ty);
            if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved)
                && let Some(name) = self.must_use_types.get(def).cloned()
            {
                self.emit(
                    TypeError::DiscardedMustUse {
                        what: "value",
                        name,
                    },
                    expr.span,
                );
                return true;
            }
        }
        let ExprKind::Call { callee, .. } = &expr.kind else {
            return false;
        };
        let Some(Resolution::Def { def, .. }) = self.resolutions.get(callee.id) else {
            return false;
        };
        let Some(name) = self.must_use_fns.get(&def).cloned() else {
            return false;
        };
        self.emit(
            TypeError::DiscardedMustUse {
                what: "return value",
                name,
            },
            expr.span,
        );
        true
    }

    /// Name of a user generic type with no `fmt`, when `ty` is one.
    ///
    /// A generic declaration only gets a synthesized `fmt` from an explicit
    /// `#[derive(Debug)]`, because whether its fields render depends on the
    /// arguments each instantiation supplies. Formatting one without that
    /// would run on the interpreter and fail the native build, so it is
    /// rejected here, where both tiers see it.
    pub(super) fn generic_without_fmt(&mut self, ty: Ty) -> Option<String> {
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if substs.is_empty() {
            return None;
        }
        let name = self.tcx.def_name(*def)?.to_string();
        // Built-in generic types render through the runtime, not a `fmt`.
        if !self.user_type_defs.values().any(|d| d == def) {
            return None;
        }
        let has_fmt = self
            .method_homes
            .contains_key(&(name.clone(), "fmt".to_string()));
        (!has_fmt).then_some(name)
    }

    /// True when `ty` resolves to a `Result<T, E>`.
    /// Reports a `for` whose subject is a `Result` or an `Option`.
    ///
    /// Neither is a sequence. Iterating one binds nothing and runs the body
    /// zero times, and because the binding is then unconstrained, whatever
    /// the body reads off it type-checks - so the loop compiles, runs, and
    /// silently does nothing. The value inside has to be taken first.
    pub(super) fn reject_wrapper_iteration(&mut self, ty: Ty, span: Span) {
        let resolved = self.infer.resolve(self.tcx, ty);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def) else {
            return;
        };
        let taken = match name {
            "Result" => "`?`, a `match`, or `unwrap_or(..)`",
            "Option" => "`if let Some(v) = ..`, `?`, or `unwrap_or(..)`",
            _ => return,
        };
        let name = name.to_string();
        self.emit(TypeError::IterableWrapper { name, taken }, span);
    }

    pub(super) fn is_result_ty(&mut self, ty: Ty) -> bool {
        let resolved = self.infer.resolve(self.tcx, ty);
        matches!(self.tcx.kind(resolved), Some(TyKind::Adt { def, .. })
            if self.tcx.def_name(*def) == Some("Result"))
    }

    /// Reports GT0007 for `expr` sitting in a position whose value is
    /// discarded.
    ///
    /// A construct that passes its operand's value through - a block, an
    /// `if`, a `match` - discards that operand exactly when its own value is
    /// discarded, so the report lands on the expression that produced the
    /// `Result` rather than on the construct wrapping it. An else-less `if`
    /// is typed `()` while its `then` branch keeps the branch's own type, so
    /// recursion is what reaches it at all.
    ///
    /// `ty` is the type already computed for `expr` by the caller; the
    /// recursive steps read the side table instead.
    pub(super) fn report_discarded_result(&mut self, expr: &Expr, ty: Option<Ty>) {
        if self.unused_result_allowed {
            return;
        }
        let ty = ty.or_else(|| self.table.get(expr.id));
        if let Some(ty) = ty
            && self.is_result_ty(ty)
        {
            self.emit(TypeError::DiscardedResult, expr.span);
            return;
        }
        if self.report_discarded_must_use(expr, ty) {
            return;
        }
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Unsafe(block) => {
                if let Some(tail) = &block.tail {
                    self.report_discarded_result(tail, None);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.report_discarded_result(then_branch, None);
                if let Some(else_branch) = else_branch {
                    self.report_discarded_result(else_branch, None);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.report_discarded_result(&arm.body, None);
                }
            }
            _ => {}
        }
    }

    pub(super) fn check_block(&mut self, block: &Block, expected: Expectation) -> Ty {
        self.push_scope();
        let mut diverged = false;
        for stmt in &block.stmts {
            self.check_stmt(stmt);
            if !diverged && stmt_diverges(stmt) {
                diverged = true;
            }
        }
        let ty = if let Some(tail) = &block.tail {
            self.check_expr_expecting(tail, expected)
        } else if diverged {
            // A block whose statements unconditionally diverge
            // (`return`, `break`, `continue`, `panic!`) and whose
            // tail is missing has type `!`. Without this, a match
            // arm body like `{ eprintln!(...); return Err(msg) }`
            // would be typed as `unit` and force the match's
            // result type away from the other arms' real type.
            self.tcx.never()
        } else {
            self.tcx.unit()
        };
        self.pop_scope();
        ty
    }

    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, ty, init } => {
                self.check_let_stmt(pattern, ty.as_ref(), init.as_deref());
            }
            StmtKind::Expr { expr, .. } => {
                let expr_ty = self.check_discarded_expr(expr);
                // SPEC §9: a `Result<T, E>` value used as a statement (value
                // discarded) is a compile error. The explicit discard form
                // `let _ = expr` goes through `StmtKind::Let` and is not
                // subject to this check.
                self.report_discarded_result(expr, Some(expr_ty));
            }
            StmtKind::Item(item) => {
                // Block-local items are not part of the source file's
                // top-level signature prepass. Register their definitions
                // before checking the body so nested structs expose fields
                // and nested functions/types use the same DefId-keyed
                // metadata as module-level items.
                self.assoc.extend(std::slice::from_ref(item));
                self.collect_signatures(std::slice::from_ref(item));
                self.check_item(item);
            }
            StmtKind::Defer(inner) => {
                self.check_expr(inner);
            }
        }
    }

    pub(super) fn check_let_stmt(
        &mut self,
        pattern: &Pattern,
        ty: Option<&AstType>,
        init: Option<&Expr>,
    ) {
        let forced = self.write_arg_bindings.get(&pattern.id).copied();
        let binding_ty = if let Some(authored) = ty {
            self.type_from_ast(authored)
        } else {
            forced.unwrap_or_else(|| self.fresh())
        };
        if let Some(init) = init {
            let expected = if ty.is_some() || forced.is_some() {
                Expectation::HasType(binding_ty)
            } else {
                Expectation::None
            };
            let init_ty = self.check_expr_expecting(init, expected);
            if let Some(error) = self.option_value_mismatch(pattern, binding_ty, init, init_ty) {
                self.emit(error, init.span);
            } else {
                // A mutable binding may later hold any callable of the same
                // signature, so a function item stored in one is a callable
                // value rather than the item's own type.
                if ty.is_none()
                    && forced.is_none()
                    && matches!(
                        &pattern.kind,
                        PatternKind::Ident {
                            mutability: gossamer_ast::Mutability::Mutable,
                            ..
                        }
                    )
                    && let Some(callable) = self.fn_item_value_ty(init_ty)
                {
                    self.unify(binding_ty, callable, init.span);
                }
                self.record_fn_item_coercion(init, binding_ty);
                self.unify(binding_ty, init_ty, init.span);
            }
            // A target list says how many elements it expects, so its arity is
            // checked against a tuple value: `let a, b, c = pair` would
            // otherwise bind past the end of the pair. A sequence value keeps
            // its own element-wise binding, which names no arity to check.
            if matches!(pattern.kind, PatternKind::Tuple(_)) {
                let resolved = self.infer.resolve(self.tcx, binding_ty);
                if matches!(self.tcx.kind(resolved), Some(TyKind::Tuple(_))) {
                    let pattern_ty = self.type_of_pattern(pattern);
                    self.unify(pattern_ty, resolved, pattern.span);
                }
            }
            self.check_local_reference_storage(pattern, binding_ty, init);
            self.check_reference_pattern(pattern, init_ty);
        }
        if let Some(problem) = self.let_pattern_problem(pattern, Some(binding_ty)) {
            let error = match problem {
                PlainLetPatternProblem::Literal => TypeError::CannotAssignToLiteral,
                PlainLetPatternProblem::MayNotMatch => TypeError::LetPatternMayNotMatch,
            };
            self.emit(error, pattern.span);
        }
        if ty.is_none() && forced.is_none() {
            self.infer.default_numeric_vars_in_ty(self.tcx, binding_ty);
        }
        self.bind_pattern(pattern, binding_ty);
        if let Some(init) = init {
            self.register_named_mutable_borrow(pattern, init);
            self.record_closure_captures(pattern, init);
        }
    }

    /// Reports a `for` pattern that some element of type `elem` fails to
    /// match: the loop has nowhere to send that element.
    pub(super) fn reject_refutable_for_pattern(&mut self, pattern: &Pattern, elem: Ty) {
        if let Some(problem) = self.let_pattern_problem(pattern, Some(elem)) {
            let error = match problem {
                PlainLetPatternProblem::Literal => TypeError::CannotAssignToLiteral,
                PlainLetPatternProblem::MayNotMatch => TypeError::ForPatternMayNotMatch,
            };
            self.emit(error, pattern.span);
        }
    }

    /// Why a plain `let` cannot take `pattern` for a value of type `ty`, when
    /// it cannot: a literal in binding position, or a pattern some value of
    /// the type fails to match. A slice pattern over a fixed array whose
    /// length it spells out matches every value, as does one whose `..`
    /// absorbs the rest.
    pub(super) fn let_pattern_problem(
        &mut self,
        pattern: &Pattern,
        ty: Option<Ty>,
    ) -> Option<PlainLetPatternProblem> {
        let resolved = ty.and_then(|ty| {
            let resolved = self.infer.resolve(self.tcx, ty);
            self.tcx.kind(resolved).cloned()
        });
        match &pattern.kind {
            PatternKind::Literal(_) => Some(PlainLetPatternProblem::Literal),
            PatternKind::Range { .. } => Some(PlainLetPatternProblem::MayNotMatch),
            PatternKind::Ident { subpattern, .. } => subpattern
                .as_deref()
                .and_then(|sub| self.let_pattern_problem(sub, ty)),
            PatternKind::Tuple(parts) => {
                let elems = match resolved {
                    Some(TyKind::Tuple(elems)) if elems.len() == parts.len() => elems,
                    _ => Vec::new(),
                };
                parts
                    .iter()
                    .enumerate()
                    .find_map(|(i, part)| self.let_pattern_problem(part, elems.get(i).copied()))
            }
            PatternKind::Or(parts) => {
                let problems: Vec<_> = parts
                    .iter()
                    .map(|part| self.let_pattern_problem(part, ty))
                    .collect();
                if problems.iter().all(Option::is_some) {
                    problems
                        .into_iter()
                        .flatten()
                        .find(|problem| matches!(problem, PlainLetPatternProblem::Literal))
                        .or(Some(PlainLetPatternProblem::MayNotMatch))
                } else {
                    None
                }
            }
            PatternKind::Struct { fields, .. } => fields
                .iter()
                .filter_map(|field| field.pattern.as_ref())
                .find_map(|part| self.let_pattern_problem(part, None)),
            PatternKind::TupleStruct { elems, .. } => elems
                .iter()
                .find_map(|part| self.let_pattern_problem(part, None)),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let (elem, len) = match resolved {
                    Some(TyKind::Array { elem, len }) => (
                        Some(elem),
                        match len {
                            crate::ArrayLen::Concrete(n) => Some(n),
                            crate::ArrayLen::Param(_) => None,
                        },
                    ),
                    _ => (None, None),
                };
                if let Some(problem) = prefix
                    .iter()
                    .chain(suffix)
                    .find_map(|part| self.let_pattern_problem(part, elem))
                {
                    return Some(problem);
                }
                let written = prefix.len() + suffix.len();
                let always_matches = match len {
                    Some(n) if rest.is_some() => written <= n,
                    Some(n) => written == n,
                    None => written == 0 && rest.is_some(),
                };
                (!always_matches).then_some(PlainLetPatternProblem::MayNotMatch)
            }
            PatternKind::Ref { inner, .. } => self.let_pattern_problem(inner, None),
            PatternKind::Wildcard
            | PatternKind::Path(_)
            | PatternKind::Rest
            | PatternKind::Error => None,
        }
    }

    /// The GT0001 diagnostic for binding an `Option<T>` where the
    /// annotation asks for `T`, spelling both fixes with the initializer's
    /// own text. `None` when the shapes do not match that case or the
    /// initializer has no short spelling, leaving ordinary unification to
    /// report the mismatch.
    pub(super) fn option_value_mismatch(
        &mut self,
        pattern: &Pattern,
        binding_ty: Ty,
        init: &Expr,
        init_ty: Ty,
    ) -> Option<TypeError> {
        let want = self.infer.resolve(self.tcx, binding_ty);
        let found = self.infer.resolve(self.tcx, init_ty);
        if matches!(
            self.tcx.kind(want),
            Some(TyKind::Var(_) | TyKind::Error) | None
        ) {
            return None;
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(found) else {
            return None;
        };
        if def.local != OPTION_DEF_LOCAL {
            return None;
        }
        let payload = substs.types().first().copied()?;
        let payload = self.infer.resolve(self.tcx, payload);
        let expected = self.render_public_ty(want);
        if expected != self.render_public_ty(payload) {
            return None;
        }
        let actual = expr_display(init)?;
        let binding = match &pattern.kind {
            PatternKind::Ident { name, .. } => name.name.clone(),
            _ => "value".to_string(),
        };
        let default = default_value_spelling(self.tcx.kind(payload));
        Some(TypeError::OptionValueMismatch {
            expected,
            found: self.render_public_ty(found),
            actual,
            binding,
            default,
        })
    }

    pub(super) fn check_local_reference_storage(
        &mut self,
        pattern: &Pattern,
        binding_ty: Ty,
        init: &Expr,
    ) {
        if matches!(pattern.kind, PatternKind::Ref { .. }) {
            return;
        }
        let resolved = self.infer.resolve(self.tcx, binding_ty);
        if matches!(self.tcx.kind(resolved), Some(TyKind::Ref { .. })) {
            let stable = matches!(
                &init.kind,
                ExprKind::Unary {
                    op: UnaryOp::RefShared | UnaryOp::RefMut,
                    operand,
                } if is_stable_borrow_place(operand)
            ) || self.is_stable_shared_reference_alias(init);
            if !stable {
                self.emit(
                    TypeError::ReferenceEscapeUnsupported {
                        context: "be copied into a local or borrow a temporary".to_string(),
                    },
                    init.span,
                );
            }
        } else {
            // A function value whose signature accepts a reference does not
            // store a reference. Its reference exists only for the duration
            // of a future call. Captured references are rejected separately
            // by `check_closure`.
            if !matches!(
                self.tcx.kind(resolved),
                Some(TyKind::FnPtr(_) | TyKind::FnTrait(_))
            ) {
                self.deferred_reference_storage.push((
                    binding_ty,
                    pattern.span,
                    "be nested inside an owned local value",
                ));
            }
        }
    }

    pub(super) fn check_reference_pattern(&mut self, pattern: &Pattern, init_ty: Ty) {
        let PatternKind::Ref { mutability, .. } = &pattern.kind else {
            return;
        };
        self.infer.default_numeric_vars_in_ty(self.tcx, init_ty);
        let resolved = self.infer.resolve(self.tcx, init_ty);
        let expected = if mutability.is_mutable() {
            Mutbl::Mut
        } else {
            Mutbl::Not
        };
        let valid = match self.tcx.kind(resolved) {
            Some(TyKind::Ref {
                mutability: actual, ..
            }) => *actual == expected,
            Some(TyKind::Var(_) | TyKind::Error) => true,
            _ => false,
        };
        if !valid {
            self.emit(
                TypeError::ReferencePatternRequiresReference {
                    pattern: if mutability.is_mutable() { "&mut" } else { "&" },
                },
                pattern.span,
            );
            return;
        }
        let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let inner = *inner;
        self.check_reference_pattern_referent(pattern, inner);
    }

    /// A reference pattern copies its referent out of the reference, which
    /// only a scalar representation supports.
    pub(super) fn check_reference_pattern_referent(&mut self, pattern: &Pattern, referent: Ty) {
        let referent = self.infer.resolve(self.tcx, referent);
        if !matches!(
            self.tcx.kind_of(referent),
            TyKind::Bool
                | TyKind::Char
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Unit
                | TyKind::Never
                | TyKind::Var(_)
                | TyKind::Error
        ) {
            let ty = self.render_public_ty(referent);
            self.emit(
                TypeError::ReferencePatternAggregateUnsupported { ty },
                pattern.span,
            );
        }
    }

    /// A parameter's reference is declared in its type. A `&` pattern over a
    /// declared type that is not a matching reference has no referent to
    /// bind, so name the `name: &Ty` spelling the parameter meant.
    pub(super) fn check_param_reference_pattern(&mut self, pattern: &Pattern, param_ty: Ty) {
        let PatternKind::Ref { mutability, inner } = &pattern.kind else {
            return;
        };
        let expected = if mutability.is_mutable() {
            Mutbl::Mut
        } else {
            Mutbl::Not
        };
        let resolved = self.infer.resolve(self.tcx, param_ty);
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Ref {
                mutability: actual,
                inner: referent,
            }) if actual == expected => {
                self.check_reference_pattern_referent(pattern, referent);
            }
            Some(TyKind::Var(_) | TyKind::Error) => {}
            _ => {
                let ty = self.render_public_ty(resolved);
                let (spelling, reference_ty) = if mutability.is_mutable() {
                    ("&mut", format!("&mut {ty}"))
                } else {
                    ("&", format!("&{ty}"))
                };
                let mut names = Vec::new();
                pattern_binding_names(inner, &mut names);
                let binding = names
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "value".to_string());
                self.emit(
                    TypeError::ReferenceParameterPatternPosition {
                        pattern: spelling,
                        binding,
                        reference_ty,
                        ty,
                    },
                    pattern.span,
                );
            }
        }
    }

    /// Rejects a goroutine body that reads an outer binding whose type has
    /// no representation crossing the boundary.
    ///
    /// The spawning goroutine keeps its own handle on the value, so both
    /// sides would reach one piece of nested growable storage with nothing
    /// serialising them - the shape whose compiled ABI has no ownership
    /// descriptor, and which faults rather than racing. Checked here so the
    /// answer is the same on every tier, rather than at run time on one.
    /// Reports a `sync::Shared` payload the guarded slot cannot carry.
    ///
    /// Run after inference so a numeric literal has landed on its type:
    /// an integer is read back identically by every tier, and nothing else
    /// is, so nothing else may be guarded.
    pub(super) fn check_deferred_shared_payloads(&mut self) {
        let pending = std::mem::take(&mut self.deferred_shared_payloads);
        for (ty, span) in pending {
            let elem = self.infer.resolve(self.tcx, ty);
            if matches!(self.tcx.kind_of(elem), TyKind::Int(_) | TyKind::Error) {
                continue;
            }
            let rendered = self.render_public_ty(elem);
            self.emit(TypeError::SharedPayloadUnsupported { ty: rendered }, span);
        }
    }

    pub(super) fn reject_unshareable_goroutine_captures(&mut self, expr: &Expr) {
        let unshareable: Vec<(String, Ty)> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.iter())
            .filter_map(|(name, ty)| {
                let resolved = self.infer.resolve(self.tcx, *ty);
                self.ty_is_unshareable_across_goroutines(resolved)
                    .then(|| (name.to_string(), resolved))
            })
            .collect();
        if unshareable.is_empty() {
            return;
        }
        for body in goroutine_bodies(expr) {
            let bound = closure_bound_names(body.params);
            for (name, ty) in &unshareable {
                if bound.contains(name) {
                    continue;
                }
                let mut one = HashSet::new();
                one.insert(name.clone());
                if expr_mentions_any_name(body.body, &one) {
                    let rendered = self.render_public_ty(*ty);
                    self.emit(
                        TypeError::ConcurrentCaptureUnsupported {
                            name: name.clone(),
                            ty: rendered,
                        },
                        body.body.span,
                    );
                }
            }
        }
    }

    /// Whether a value of `ty` reaches a goroutine only as shared nested
    /// storage. A bare sequence or map is published as one owned container,
    /// and a scalar, a `String`, or a runtime handle carries its own
    /// representation; an aggregate *holding* growable storage does not.
    pub(super) fn ty_is_unshareable_across_goroutines(&self, ty: Ty) -> bool {
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        if matches!(
            self.tcx.kind_of(peeled),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. }
        ) {
            return false;
        }
        if self.ty_is_shareable_handle(peeled) {
            return false;
        }
        self.ty_contains_nested_vec(peeled)
    }

    /// A stdlib handle built to be reached from several goroutines: the
    /// synchronisation types, the channel ends, and `sync::Shared`.
    pub(super) fn ty_is_shareable_handle(&self, ty: Ty) -> bool {
        if matches!(
            self.tcx.kind_of(ty),
            TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_)
        ) {
            return true;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind_of(ty) else {
            return false;
        };
        self.tcx.def_name(*def).is_some_and(|name| {
            let bare = name.rsplit("::").next().unwrap_or(name);
            matches!(
                bare,
                "Shared"
                    | "Mutex"
                    | "RwLock"
                    | "Once"
                    | "WaitGroup"
                    | "Barrier"
                    | "AtomicI64"
                    | "AtomicI32"
                    | "AtomicU64"
                    | "AtomicBool"
            )
        })
    }

    /// Rejects an aggregate written inline at a spawned call.
    ///
    /// The argument is a closure, so the call it wraps is one level in: a
    /// `spawn(|| f(Pair { .. }))` puts the aggregate where the boundary is.
    pub(super) fn reject_spawn_inline_aggregate_args(&mut self, arg: &Expr) {
        let ExprKind::Closure { body, .. } = &arg.kind else {
            return self.reject_go_inline_aggregate_args(arg);
        };
        self.reject_go_inline_aggregate_args(body);
    }

    pub(super) fn reject_go_inline_aggregate_args(&mut self, expr: &Expr) {
        let ExprKind::Call { args, .. } = &expr.kind else {
            return;
        };
        for arg in args {
            let Some(ty) = self.table.get(arg.id) else {
                continue;
            };
            let resolved = self.infer.resolve(self.tcx, ty);
            let inline = match self.tcx.kind_of(resolved) {
                TyKind::Tuple(_) | TyKind::Array { .. } => true,
                TyKind::Adt { def, .. } => {
                    def.local < u32::MAX - 32 && self.tcx.struct_field_tys(*def).is_some()
                }
                _ => false,
            };
            if inline {
                let ty = self.render_public_ty(resolved);
                self.emit(
                    TypeError::ConcurrentAggregateUnsupported {
                        ty,
                        boundary: "cross a goroutine boundary",
                    },
                    arg.span,
                );
            }
        }
    }

    pub(super) fn check_closure(
        &mut self,
        params: &[ClosureParam],
        ret: Option<&AstType>,
        body: &Expr,
        expected: Expectation,
    ) -> Ty {
        let mut outer_references: HashSet<String> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.iter())
            .filter_map(|(name, ty)| {
                let ty = self.infer.resolve(self.tcx, *ty);
                matches!(self.tcx.kind_of(ty), TyKind::Ref { .. }).then(|| name.to_string())
            })
            .collect();
        for param in params {
            let mut names = Vec::new();
            pattern_binding_names(&param.pattern, &mut names);
            for name in names {
                outer_references.remove(&name);
            }
        }
        self.push_scope();
        // When the call site expects a function of a known shape (e.g. a
        // `Vec<T>` comparator pins `Fn(T, T) -> _`), unify each unannotated
        // parameter with the expected input type before the body is checked.
        // Without this a field access inside the body (`a.size`) sees the
        // parameter as an unresolved inference var and falls back to the
        // dynamic JSON-field path rather than the struct projection.
        let expected_target = self
            .expectation_target(expected)
            .map(|t| self.infer.resolve(self.tcx, t));
        // A closure standing where an `http::Handler` is taken is the handler
        // itself, so it takes the handler's request. It answers either a
        // `Response` or a `Result` of one, so its answer is left to its body.
        let handler_input = if expected_target.is_some()
            && expected_target == Some(self.http_handler_ty())
            && params.len() == 1
        {
            Some(vec![self.http_request_ty()])
        } else {
            None
        };
        let expected_inputs: Option<Vec<Ty>> = match handler_input {
            Some(inputs) => Some(inputs),
            None => expected_target.and_then(|t| match self.tcx.kind(t) {
                Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig))
                    if sig.inputs.len() == params.len() =>
                {
                    Some(sig.inputs.clone())
                }
                _ => None,
            }),
        };
        let inputs: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(i, param)| {
                let ty = if let Some(ty) = param.ty.as_ref() {
                    self.type_from_ast(ty)
                } else {
                    let ty = self.fresh();
                    self.unannotated_closure_params.push(ty);
                    ty
                };
                if let Some(want) = expected_inputs.as_ref().map(|inputs| inputs[i]) {
                    self.unify(ty, want, body.span);
                }
                self.check_param_reference_pattern(&param.pattern, ty);
                self.bind_pattern(&param.pattern, ty);
                self.register_reference_parameter_origins(&param.pattern);
                ty
            })
            .collect();
        let output = match ret {
            Some(ty) => self.type_from_ast(ty),
            None => self.fresh(),
        };
        let body_expected = if ret.is_some() {
            Expectation::HasType(output)
        } else {
            Expectation::None
        };
        // `return` inside the body leaves the CLOSURE, not the function
        // the closure was written in, so the body is checked against the
        // closure's own output type.
        let prev_ret = self.current_fn_ret.replace(output);
        let body_ty = self.check_expr_expecting(body, body_expected);
        self.current_fn_ret = prev_ret;
        self.unify(output, body_ty, body.span);
        if expr_mentions_any_name(body, &outer_references) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: "be captured by a closure".to_string(),
                },
                body.span,
            );
        }
        let resolved_output = self.infer.resolve(self.tcx, output);
        if self.ty_contains_reference(resolved_output) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: "escape through a closure return".to_string(),
                },
                body.span,
            );
        }
        self.pop_scope();
        self.tcx.intern(TyKind::FnPtr(FnSig { inputs, output }))
    }

    /// True when a value of `ty` can be hashed and compared by value, the
    /// requirement every `Map` / `Set` key must satisfy. Mirrors the
    /// hashable-key list in the language reference: scalars, `String`,
    /// tuples, fixed arrays, structs, and enums are hashable when every
    /// piece they carry is; `Vec`, `Map`, `Set`, closures, references, and
    /// runtime handles are not - the language has no `Hash` impl for a
    /// heap container or a value with no stable identity to hash.
    /// Unresolved (`Var`/`Error`/alias) types are treated as hashable so
    /// this never rejects a type the checker hasn't pinned down yet - except
    /// an unsuffixed float literal (`{1.5: 2}`), which is still an
    /// unresolved `Var` at this point (numeric-literal defaulting runs
    /// later) but can only ever default to `f64`.
    pub(super) fn is_hashable_ty(&mut self, ty: Ty) -> bool {
        if self.infer.is_float_literal_var(self.tcx, ty) {
            return false;
        }
        let resolved = self.infer.resolve(self.tcx, ty);
        let mut seen = std::collections::HashSet::new();
        self.is_hashable_ty_rec(resolved, &mut seen)
    }

    pub(super) fn is_hashable_ty_rec(
        &self,
        ty: Ty,
        seen: &mut std::collections::HashSet<gossamer_resolve::DefId>,
    ) -> bool {
        match self.tcx.kind(ty) {
            Some(
                TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Int(_)
                | TyKind::Unit
                | TyKind::Never,
            ) => true,
            // A dynamic value's shape is not known until it exists, so there
            // is no key layout to fold it into.
            Some(TyKind::DynValue) => false,
            Some(TyKind::Tuple(elems)) => elems
                .iter()
                .all(|elem| self.is_hashable_ty_rec(*elem, seen)),
            Some(TyKind::Array { elem, .. } | TyKind::Simd { elem, .. }) => {
                self.is_hashable_ty_rec(*elem, seen)
            }
            // A nominal alias hashes and compares exactly as the value it
            // erases to, so it is a key wherever its representation is.
            Some(TyKind::Nominal { repr, .. }) => self.is_hashable_ty_rec(*repr, seen),
            Some(TyKind::Adt { def, substs }) => {
                match def.local {
                    RESULT_DEF_LOCAL | OPTION_DEF_LOCAL => {
                        return substs
                            .types()
                            .iter()
                            .all(|t| self.is_hashable_ty_rec(*t, seen));
                    }
                    HASH_SET_DEF_LOCAL
                    | BTREE_SET_DEF_LOCAL
                    | VEC_DEQUE_DEF_LOCAL
                    | BINARY_HEAP_DEF_LOCAL
                    | REVERSE_DEF_LOCAL
                    | MIN_HEAP_DEF_LOCAL
                    | VEC_QUEUE_DEF_LOCAL
                    | VEC_STACK_DEF_LOCAL => return false,
                    _ => {}
                }
                if is_opaque_handle_def(def.local) {
                    return false;
                }
                // A recursive struct/enum (`List { next: Box<List> }`) would
                // otherwise recurse forever; a repeat visit can't be the
                // reason a type ISN'T hashable, since every concrete value
                // is still finite, so let it pass and let the other fields
                // decide.
                if !seen.insert(*def) {
                    return true;
                }
                if let Some(fields) = self.tcx.adt_field_tys(*def, substs) {
                    return fields
                        .iter()
                        .all(|field| self.is_hashable_ty_rec(*field, seen));
                }
                if let Some(variants) = self.tcx.enum_variant_tys(*def) {
                    return variants.iter().all(|fields| {
                        fields
                            .iter()
                            .all(|field| self.is_hashable_ty_rec(*field, seen))
                    });
                }
                // Unregistered def (a generic template body, or a shape the
                // checker doesn't track field-wise): permissive, matching
                // the Var/Error fallback below.
                true
            }
            Some(
                TyKind::Float(_)
                | TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Iterator(_)
                | TyKind::Range(_)
                | TyKind::HashMap { .. }
                | TyKind::Sender(_)
                | TyKind::Receiver(_)
                | TyKind::JoinHandle(_)
                | TyKind::JsonValue
                | TyKind::DynError
                | TyKind::Ref { .. }
                | TyKind::FnDef { .. }
                | TyKind::FnPtr(_)
                | TyKind::FnTrait(_)
                | TyKind::Closure { .. }
                | TyKind::Dyn(_),
            ) => false,
            // `Duration` / `Instant` hash as the `i64` they are at run time.
            Some(TyKind::Duration | TyKind::Instant) => true,
            // Unresolved / erased: don't reject what the checker can't see.
            Some(TyKind::Alias { .. } | TyKind::Var(_) | TyKind::Param { .. } | TyKind::Error)
            | None => true,
        }
    }

    pub(super) fn check_map_literal(&mut self, entries: &[Expr], expected: Expectation) -> Ty {
        // A brace literal builds whichever map the call site expects, the way
        // `#{..}` builds a `Set` or the `BTreeSet` an expectation names.
        let expected_map =
            self.expectation_target(expected)
                .and_then(|target| match self.tcx.kind(target) {
                    Some(TyKind::HashMap {
                        key,
                        value,
                        ordered,
                    }) => Some((*key, *value, *ordered)),
                    _ => None,
                });
        let ordered = expected_map.is_some_and(|(_, _, ordered)| ordered);
        let (mut key_ty, mut value_ty) =
            expected_map.map_or_else(|| (self.fresh(), self.fresh()), |(k, v, _)| (k, v));

        for entry in entries {
            let ExprKind::Tuple(parts) = &entry.kind else {
                let entry_ty = self.check_expr(entry);
                let found = self.render_public_ty(entry_ty);
                self.emit(
                    TypeError::TypeMismatch {
                        expected: "(K, V)".to_string(),
                        found,
                    },
                    entry.span,
                );
                continue;
            };
            let [key, value] = parts.as_slice() else {
                let entry_ty = self.check_expr(entry);
                let found = self.render_public_ty(entry_ty);
                self.emit(
                    TypeError::TypeMismatch {
                        expected: "(K, V)".to_string(),
                        found,
                    },
                    entry.span,
                );
                continue;
            };
            let got_key = self.check_expr_expecting(key, expected.rewrap(key_ty));
            let got_value = self.check_expr_expecting(value, expected.rewrap(value_ty));
            if expected_map.is_some() && expected.unifies() {
                self.unify(key_ty, got_key, key.span);
                self.unify(value_ty, got_value, value.span);
            } else {
                key_ty = self.join_branch_tys(key_ty, got_key, key.span);
                value_ty = self.join_branch_tys(value_ty, got_value, value.span);
            }
            let pair = self.tcx.intern(TyKind::Tuple(vec![key_ty, value_ty]));
            self.record(entry.id, pair);
        }

        if !self.is_hashable_ty(key_ty)
            && let Some(ExprKind::Tuple(parts)) = entries.first().map(|e| &e.kind)
            && let Some(key) = parts.first()
        {
            let ty = self.render_public_ty(key_ty);
            self.emit(
                TypeError::TraitBoundNotSatisfied {
                    ty,
                    bound: "Hash".to_string(),
                },
                key.span,
            );
        }
        self.tcx.intern(TyKind::HashMap {
            key: key_ty,
            value: value_ty,
            ordered,
        })
    }

    pub(super) fn check_set_literal(&mut self, entries: &[Expr], expected: Expectation) -> Ty {
        let expected_elem =
            self.expectation_target(expected)
                .and_then(|target| match self.tcx.kind(target) {
                    Some(TyKind::Adt { def, substs })
                        if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
                    {
                        substs
                            .types()
                            .first()
                            .copied()
                            .map(|elem| (def.local, elem))
                    }
                    _ => None,
                });

        if let Some((want_owner, want_elem)) = expected_elem {
            for entry in entries {
                let got = self.check_expr_expecting(entry, expected.rewrap(want_elem));
                if expected.unifies() {
                    self.unify(want_elem, got, entry.span);
                }
            }
            self.check_hashable_elem_ty(want_elem, entries);
            return if want_owner == BTREE_SET_DEF_LOCAL {
                self.btreeset_ty(want_elem)
            } else {
                self.hashset_ty(want_elem)
            };
        }

        let mut elem_ty = if let Some(first) = entries.first() {
            self.check_expr(first)
        } else {
            self.fresh()
        };
        for entry in entries.iter().skip(1) {
            let ty = self.check_expr(entry);
            elem_ty = self.join_branch_tys(elem_ty, ty, entry.span);
        }
        self.check_hashable_elem_ty(elem_ty, entries);
        self.hashset_ty(elem_ty)
    }

    /// Emits `GT0017` at the first set element's span when `elem_ty` fails
    /// [`Self::is_hashable_ty`]. Shared by both `check_set_literal` return
    /// paths so an explicitly-annotated `Set<Vec<i64>>` and an
    /// inference-only `#{v1, v2}` are rejected the same way.
    pub(super) fn check_hashable_elem_ty(&mut self, elem_ty: Ty, entries: &[Expr]) {
        if !self.is_hashable_ty(elem_ty)
            && let Some(entry) = entries.first()
        {
            let ty = self.render_public_ty(elem_ty);
            self.emit(
                TypeError::TraitBoundNotSatisfied {
                    ty,
                    bound: "Hash".to_string(),
                },
                entry.span,
            );
        }
    }

    pub(super) fn check_vec_literal(&mut self, arr: &ArrayExpr, expected: Expectation) -> Ty {
        let expected_elem =
            self.expectation_target(expected)
                .and_then(|target| match self.tcx.kind(target) {
                    Some(TyKind::Vec(elem) | TyKind::Slice(elem)) => Some(*elem),
                    _ => None,
                });
        match arr {
            ArrayExpr::List(elems) => {
                if let Some(want_elem) = expected_elem {
                    for elem in elems {
                        let got = self.check_expr_expecting(elem, expected.rewrap(want_elem));
                        if expected.unifies() {
                            self.unify(want_elem, got, elem.span);
                        }
                    }
                    return self.tcx.intern(TyKind::Vec(want_elem));
                }
                let elem_ty = self.join_element_tys(elems);
                self.tcx.intern(TyKind::Vec(elem_ty))
            }
            ArrayExpr::Repeat { value, count } => {
                let elem_ty = match expected_elem {
                    Some(want_elem) => {
                        let got = self.check_expr_expecting(value, expected.rewrap(want_elem));
                        if expected.unifies() {
                            self.unify(want_elem, got, value.span);
                        }
                        want_elem
                    }
                    None => self.check_expr(value),
                };
                self.check_expr(count);
                self.tcx.intern(TyKind::Vec(elem_ty))
            }
        }
    }

    pub(super) fn check_array(&mut self, arr: &ArrayExpr, expected: Expectation) -> Ty {
        // This handles explicit fixed-array literals (`#[...]`) and
        // expectation-shaped `[T; N]` arrays. Plain `[...]` literals are checked
        // through `check_vec` unless an array expectation selected this path.
        match arr {
            ArrayExpr::List(elems) => {
                let expected_elem = self
                    .expectation_target(expected)
                    .and_then(|target| match self.tcx.kind(target) {
                        Some(TyKind::Array { elem, len })
                            if *len == crate::ArrayLen::Concrete(elems.len()) =>
                        {
                            Some(*elem)
                        }
                        _ => None,
                    });
                if let Some(want_elem) = expected_elem {
                    for elem in elems {
                        let got = self.check_expr_expecting(elem, expected.rewrap(want_elem));
                        if expected.unifies() {
                            self.unify(want_elem, got, elem.span);
                        }
                    }
                    return self.tcx.intern(TyKind::Array {
                        elem: want_elem,
                        len: crate::ArrayLen::Concrete(elems.len()),
                    });
                }
                let elem_ty = self.join_element_tys(elems);
                self.tcx.intern(TyKind::Array {
                    elem: elem_ty,
                    len: crate::ArrayLen::Concrete(elems.len()),
                })
            }
            ArrayExpr::Repeat { value, count } => {
                let elem_ty = self.check_expr(value);
                self.check_expr(count);
                if let Some(len) = self.evaluate_array_len(count) {
                    let elem_ty = match self
                        .expectation_target(expected)
                        .and_then(|t| self.tcx.kind(t))
                    {
                        Some(TyKind::Array { elem, .. }) => self.infer.resolve(self.tcx, *elem),
                        _ => self.infer.resolve(self.tcx, elem_ty),
                    };
                    self.tcx.intern(TyKind::Array {
                        elem: elem_ty,
                        len: crate::ArrayLen::Concrete(len),
                    })
                } else if let Some((idx, _)) = self.const_generic_param(count) {
                    let elem_ty = self.infer.resolve(self.tcx, elem_ty);
                    self.tcx.intern(TyKind::Array {
                        elem: elem_ty,
                        len: crate::ArrayLen::Param(idx),
                    })
                } else {
                    self.emit(TypeError::ArrayLengthNotConstant, count.span);
                    self.tcx.intern(TyKind::Array {
                        elem: elem_ty,
                        len: crate::ArrayLen::Concrete(0),
                    })
                }
            }
        }
    }

    /// The type of an enum path in value position: `Enum::Variant` with no
    /// payload to infer its parameters from.
    pub(super) fn enum_path_value_ty(
        &mut self,
        node: NodeId,
        def: DefId,
        path: &gossamer_ast::PathExpr,
        span: Span,
        expected: Expectation,
    ) -> Ty {
        // A generic enum named without a payload to infer from -
        // `L::Nil` - still has to carry parameters, or it will
        // not unify with the `L<i64>` it is being bound to. A
        // const parameter has no payload to take its value from
        // either, so it takes the value the context expects.
        let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
        let consts = LiteralConsts::new(self.fn_generic_const_mask_of(def));
        let placeholder = self.tcx.error_ty();
        let substs: Vec<Ty> = (0..arity)
            .map(|i| {
                if consts.mask.get(i).copied().unwrap_or(false) {
                    placeholder
                } else {
                    self.fresh()
                }
            })
            .collect();
        let ty = self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::from_types(substs),
        });
        let enum_name = self
            .tcx
            .def_name(def)
            .and_then(|name| name.rsplit("::").next())
            .map(str::to_string);
        let variant_name = path
            .segments
            .last()
            .map(|segment| segment.name.name.clone());
        let carries_payload = match (enum_name, variant_name) {
            (Some(enum_name), Some(variant_name)) => self
                .enum_variant_payloads
                .get(&(enum_name, variant_name))
                .is_some_and(|payloads| !payloads.is_empty()),
            _ => true,
        };
        if !consts.has_const_positions()
            || carries_payload
            || self.callee_path_nodes.contains(&node)
        {
            return ty;
        }
        self.finish_struct_literal_consts(ty, &consts, expected, path, span)
    }

    pub(super) fn check_path_expr(
        &mut self,
        node: NodeId,
        path: &gossamer_ast::PathExpr,
        span: Span,
        expected: Expectation,
    ) -> Ty {
        self.check_path_read_conflict(path, span);
        // `Enum::Variant` naming a variant the enum does not declare: the
        // resolver resolves the path to the enum head and leaves the bad
        // tail to fault at runtime (GX0002 `Shape::Triangle`). Reject it
        // where the enum is known and the tail is neither a declared
        // variant nor an associated function on the enum.
        if self.reject_unknown_variant_path(path, span) {
            return self.tcx.error_ty();
        }
        if let Some(ty) = self.check_assoc_const_path(path, span) {
            return self.record(node, ty);
        }
        let Some(resolution) = self.resolutions.get(node) else {
            return self.check_std_path_value(node, path, span);
        };
        if let Resolution::Def { def, .. } = resolution
            && !self.callee_path_nodes.contains(&node)
            && let Some(name) = self.foreign_fns.get(&def).cloned()
        {
            self.emit(
                TypeError::Foreign(crate::ForeignError::AsValue { name }),
                span,
            );
        }
        match resolution {
            Resolution::Local(binding_id) => {
                if !self.suppressed.consumed_iterator_read
                    && path.segments.len() == 1
                    && let Some(name) = path.segments.first().map(|seg| seg.name.name.as_str())
                    && let Some(operation) = self
                        .consumed_iterators
                        .iter()
                        .rev()
                        .find_map(|scope| scope.get(&binding_id).cloned())
                {
                    self.emit(
                        TypeError::IteratorStateConsumed {
                            name: name.to_string(),
                            operation,
                        },
                        span,
                    );
                }
                if let Some(ty) = self.binding_types.get(&binding_id).copied() {
                    return ty;
                }
                if let Some(first) = path.segments.first() {
                    if let Some(ty) = self.lookup_local(&first.name.name) {
                        return ty;
                    }
                }
                self.fresh()
            }
            Resolution::Primitive(prim) => self.type_from_primitive(prim),
            Resolution::Def { def, kind } => match kind {
                gossamer_resolve::DefKind::Struct => {
                    if self.struct_fields.get(&def).is_some_and(|fields| {
                        !fields.is_empty() || self.tcx.is_tuple_struct(def.local)
                    }) && !self.callee_path_nodes.contains(&node)
                    {
                        let name = self
                            .tcx
                            .def_name(def)
                            .map_or_else(|| "<struct>".to_string(), ToString::to_string);
                        let error = if self.tcx.is_tuple_struct(def.local) {
                            TypeError::TupleStructConstructorParenthesesRequired { name }
                        } else {
                            TypeError::StructConstructorBracesRequired { name }
                        };
                        self.emit(error, span);
                    }
                    self.tcx.intern(TyKind::Adt {
                        def,
                        substs: crate::Substs::new(),
                    })
                }
                gossamer_resolve::DefKind::Enum => {
                    self.enum_path_value_ty(node, def, path, span, expected)
                }
                gossamer_resolve::DefKind::Fn => {
                    // Pull turbofish args (`ident::<i64, bool>`) off
                    // the last path segment, resolve each to a
                    // concrete [`Ty`], and stamp the callee's type as
                    // `TyKind::FnDef { def, substs }` so that the MIR
                    // lowerer reads the real substitution instead of
                    // deriving one heuristically from argument types.
                    let substs = self.fn_item_path_substs(node, def, path);
                    self.tcx.intern(TyKind::FnDef { def, substs })
                }
                gossamer_resolve::DefKind::Const | gossamer_resolve::DefKind::Static => {
                    self.const_path_ty(def, path)
                }
                gossamer_resolve::DefKind::TypeParam => {
                    match self.check_param_assoc_fn_path(def, path) {
                        Some(ty) => ty,
                        None => self.fresh(),
                    }
                }
                _ => self.fresh(),
            },
            Resolution::Import { .. } | Resolution::Err => {
                // A `use` of this unit's own item keeps its opaque
                // `Import` resolution so lowering still qualifies the
                // name, but the item's type is known here: type the
                // reference by its definition rather than by a fresh
                // variable, which would leave every use of the value
                // unchecked.
                if let Some(def) = self.resolutions.import_def(node)
                    && let Some(ty) = self.ty_of_imported_def(def, path)
                {
                    return self.record(node, ty);
                }
                self.check_std_path_value(node, path, span)
            }
        }
    }

    /// The substitution a path naming the function item `def` carries: its
    /// turbofish arguments when written. A generic function named as a value,
    /// not called, is instantiated by the callable type it coerces to, so each
    /// type position starts as a fresh variable that the coercion binds and the
    /// value names one instantiation rather than the template. A call records
    /// its own instantiation in `check_call`.
    pub(super) fn fn_item_path_substs(
        &mut self,
        node: NodeId,
        def: gossamer_resolve::DefId,
        path: &gossamer_ast::PathExpr,
    ) -> crate::Substs {
        let substs = self.substs_from_path(path);
        let arity = self.fn_generic_arity.get(&def).copied().unwrap_or(0);
        if !substs.is_empty()
            || arity == 0
            || self.callee_path_nodes.contains(&node)
            || self.fn_generic_const_mask_of(def).iter().any(|c| *c)
        {
            return substs;
        }
        let vars: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
        crate::Substs::from_types(vars)
    }

    /// Types `T::name`, where `T` is a type parameter in scope and `name` is
    /// a function one of `T`'s bounds declares, as that function with the
    /// trait's `Self` read as `T`. Records which parameter the path's head
    /// resolved to, so lowering can dispatch the call per instantiation.
    pub(super) fn check_param_assoc_fn_path(
        &mut self,
        def: gossamer_resolve::DefId,
        path: &gossamer_ast::PathExpr,
    ) -> Option<Ty> {
        let [head, function] = path.segments.as_slice() else {
            return None;
        };
        let head_name = head.name.name.clone();
        let (idx, name) = self.current_generic_scope.get(&head_name).cloned()?;
        let param_ty = self.tcx.intern(TyKind::Param { idx, name });
        let bounds = self.with_supertraits(self.bounds_of_param(&head_name));
        let found = bounds.iter().find_map(|bound| {
            self.trait_fn_self_sigs
                .get(&(bound.clone(), function.name.name.clone()))
                .cloned()
        })?;
        let substs = [param_ty];
        let mut inputs: Vec<Ty> = found
            .sig
            .inputs
            .iter()
            .map(|input| self.subst_params_in_ty(*input, &substs))
            .collect();
        if let Some(receiver) = found.receiver {
            let receiver_ty = match receiver {
                gossamer_ast::Receiver::Owned => param_ty,
                gossamer_ast::Receiver::RefShared => self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Not,
                    inner: param_ty,
                }),
                gossamer_ast::Receiver::RefMut => self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: param_ty,
                }),
            };
            inputs.insert(0, receiver_ty);
        }
        let output = self.subst_params_in_ty(found.sig.output, &substs);
        self.tcx.register_type_param_def(def, param_ty);
        Some(self.tcx.intern(TyKind::FnPtr(FnSig { inputs, output })))
    }

    pub(super) fn check_path_read_conflict(&mut self, path: &gossamer_ast::PathExpr, span: Span) {
        if self.suppressed.borrow_read_conflict {
            return;
        }
        let [segment] = path.segments.as_slice() else {
            return;
        };
        let Some(borrower) = self
            .active_mutable_borrower(&segment.name.name)
            .map(str::to_string)
        else {
            return;
        };
        self.emit(
            TypeError::BorrowedPlaceConflict {
                root: segment.name.name.clone(),
                borrower,
                action: "read",
            },
            span,
        );
    }

    pub(super) fn reject_unknown_variant_path(
        &mut self,
        path: &gossamer_ast::PathExpr,
        span: Span,
    ) -> bool {
        let n = path.segments.len();
        if n < 2 {
            return false;
        }
        let enum_name = path.segments[n - 2].name.name.as_str();
        let variant = path.segments[n - 1].name.name.as_str();
        let unknown = self.enum_variants.get(enum_name).is_some_and(|variants| {
            !variants.contains(variant)
                && !self
                    .user_method_owners
                    .get(variant)
                    .is_some_and(|owners| owners.contains(enum_name))
        });
        if unknown {
            let mut declared: Vec<String> = self
                .enum_variants
                .get(enum_name)
                .map(|variants| variants.iter().cloned().collect())
                .unwrap_or_default();
            declared.sort();
            self.emit(
                TypeError::UnknownVariant {
                    enum_name: enum_name.to_string(),
                    variant: variant.to_string(),
                    declared,
                },
                span,
            );
        }
        unknown
    }

    /// Types an unresolved path expression, handling std free
    /// functions used as first-class values. Tabled names type as a
    /// concrete `FnPtr` so combinator rows can pin against the
    /// signature; untabled std-fn-shaped paths in a value position
    /// are rejected uniformly (GT0015) because the compiled tiers
    /// have no symbol to take the address of. Everything else keeps
    /// the historical fresh-var fallback.
    /// Type of a reference to `def`, reached through a `use` of this
    /// unit's own item. Mirrors the `Resolution::Def` arm of
    /// Type of a path naming a `const` or `static` item, or a const generic
    /// parameter, which is a value of its declared type inside the body the
    /// call supplies it to.
    pub(super) fn const_path_ty(&mut self, def: DefId, path: &gossamer_ast::PathExpr) -> Ty {
        if let Some(ty) = self.const_tys.get(&def).copied() {
            return ty;
        }
        if let [seg] = path.segments.as_slice()
            && let Some((_, ty)) = self.current_const_generic_scope.get(&seg.name.name)
        {
            return *ty;
        }
        self.fresh()
    }

    /// [`Self::check_path_expr`] for the kinds a value path can name.
    pub(super) fn ty_of_imported_def(
        &mut self,
        def: DefId,
        path: &gossamer_ast::PathExpr,
    ) -> Option<Ty> {
        match self.resolutions.kind_of(def)? {
            gossamer_resolve::DefKind::Fn => {
                let substs = self.substs_from_path(path);
                Some(self.tcx.intern(TyKind::FnDef { def, substs }))
            }
            gossamer_resolve::DefKind::Struct | gossamer_resolve::DefKind::Enum => {
                Some(self.tcx.intern(TyKind::Adt {
                    def,
                    substs: crate::Substs::new(),
                }))
            }
            gossamer_resolve::DefKind::Const | gossamer_resolve::DefKind::Static => {
                self.const_tys.get(&def).copied()
            }
            _ => None,
        }
    }

    pub(super) fn check_std_path_value(
        &mut self,
        node: NodeId,
        path: &gossamer_ast::PathExpr,
        span: Span,
    ) -> Ty {
        let resolved_segments = self.resolved_value_path_names(node, path);
        let segments: Vec<&str> = resolved_segments.iter().map(String::as_str).collect();
        let joined = segments.join("::");
        if let Some((int_ty, _value)) = int_assoc_const(&segments) {
            return self.tcx.int_ty(int_ty);
        }
        // The empty arm is an `Option` whatever its payload turns out to be,
        // which is what lets a method on it type against that payload.
        if matches!(segments.as_slice(), ["None"] | ["Option", "None"]) {
            let payload = self.fresh();
            return self.option_adt_ty(payload);
        }
        // `fs::SEEK_SET` / `SEEK_CUR` / `SEEK_END` name the `whence`
        // selector `File::seek` takes.
        if matches!(
            segments.as_slice(),
            ["fs", "SEEK_SET" | "SEEK_CUR" | "SEEK_END"]
        ) {
            return self.tcx.int_ty(IntTy::I64);
        }
        if let Some(entry) = crate::std_fn_values::std_fn_value(
            joined.strip_prefix("std::").unwrap_or(joined.as_str()),
        ) {
            let inputs: Vec<Ty> = entry.params.iter().map(|p| self.std_val_ty(*p)).collect();
            let output = self.std_val_ty(entry.ret);
            return self.tcx.intern(TyKind::FnPtr(FnSig { inputs, output }));
        }
        if !self.callee_path_nodes.contains(&node)
            && crate::std_fn_values::is_std_free_fn_path(&segments)
        {
            // A macro path is not a function at all; the resolver already
            // named it and said how to write it, so adding a second report
            // about parameter lists describes the wrong thing.
            let relative = joined.strip_prefix("std::").unwrap_or(joined.as_str());
            if gossamer_resolve::stdlib_macro_named(relative).is_none() {
                self.emit(TypeError::StdFnValueUnsupported { path: joined }, span);
            }
            return self.tcx.error_ty();
        }
        self.fresh()
    }

    /// Concrete [`Ty`] for one [`crate::std_fn_values::StdValTy`] slot.
    pub(super) fn std_val_ty(&mut self, shape: crate::std_fn_values::StdValTy) -> Ty {
        use crate::std_fn_values::StdValTy;
        match shape {
            StdValTy::Str => self.tcx.string_ty(),
            StdValTy::I64 => self.tcx.int_ty(IntTy::I64),
            StdValTy::Error => self.tcx.dyn_error_ty(),
            StdValTy::ResultI64 => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let err = self.tcx.dyn_error_ty();
                self.result_adt_ty(i64_ty, err)
            }
        }
    }

    pub(super) fn substs_from_path(&mut self, path: &gossamer_ast::PathExpr) -> crate::Substs {
        let generics = match path.segments.last() {
            Some(seg) => &seg.generics,
            None => return crate::Substs::new(),
        };
        let args: Vec<crate::GenericArg> = generics
            .iter()
            .map(|arg| match arg {
                gossamer_ast::GenericArg::Type(t) => crate::GenericArg::Type(self.type_from_ast(t)),
                gossamer_ast::GenericArg::Const(expr) => {
                    crate::GenericArg::Const(self.evaluate_generic_const_arg(expr))
                }
            })
            .collect();
        crate::Substs::from_args(args)
    }

    pub(super) fn type_of_literal(&mut self, lit: &Literal, span: Span) -> Ty {
        match lit {
            Literal::Int(text) => self.type_of_int_literal(text, span),
            Literal::Float(text) => self.type_of_float_literal(text),
            Literal::String(_) | Literal::RawString { .. } => self.tcx.string_ty(),
            Literal::Char(_) => self.tcx.char_ty(),
            Literal::Byte(_) => self.tcx.int_ty(IntTy::U8),
            Literal::ByteString(_) | Literal::RawByteString { .. } => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Slice(u8_ty))
            }
            Literal::Bool(_) => self.tcx.bool_ty(),
            Literal::Unit => self.tcx.unit(),
        }
    }

    pub(super) fn type_of_int_literal(&mut self, text: &str, span: Span) -> Ty {
        for (suffix, int_ty) in INT_SUFFIXES {
            if text.ends_with(suffix) {
                if matches!(int_ty, IntTy::I128 | IntTy::U128) {
                    self.emit(
                        TypeError::Int128Unsupported {
                            ty: (*suffix).to_string(),
                        },
                        span,
                    );
                    return self.tcx.error_ty();
                }
                if !int_literal_fits(text, *int_ty) {
                    self.emit(
                        TypeError::IntLiteralOverflow {
                            literal: text.to_string(),
                            ty: (*suffix).to_string(),
                        },
                        span,
                    );
                    return self.tcx.error_ty();
                }
                return self.tcx.int_ty(*int_ty);
            }
        }
        for (suffix, float_ty) in FLOAT_SUFFIXES {
            if text.ends_with(suffix) {
                return self.tcx.float_ty(*float_ty);
            }
        }
        // Unsuffixed integer literal - Go-style untyped constant.
        // The fresh var is integer-constrained so it can only
        // unify with concrete integer types; if no use-site
        // constraints arise it defaults to `i64` at the end of
        // typechecking. Validate magnitude against the widest
        // integer bucket the language exposes (`u128`/`i128`),
        // not against `i64` alone - `let x: u64 = u64::MAX` is
        // a legitimate program; the use-site unification will
        // either succeed (assign to u64) or fail with a normal
        // type-mismatch diagnostic. Only literals whose
        // magnitude is genuinely impossible to represent in any
        // Gossamer integer type get the GT0009 here.
        let literal_too_wide =
            parse_int_magnitude(text).is_none_or(|magnitude| magnitude > u128::from(u64::MAX));
        if literal_too_wide {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: text.to_string(),
                    ty: "any integer type".to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        let ty = self.infer.fresh_int_var(self.tcx);
        self.deferred_literal_ranges
            .push((ty, text.to_string(), span));
        ty
    }

    pub(super) fn type_of_float_literal(&mut self, text: &str) -> Ty {
        for (suffix, float_ty) in FLOAT_SUFFIXES {
            if text.ends_with(suffix) {
                return self.tcx.float_ty(*float_ty);
            }
        }
        // Unsuffixed float literal: a float-defaulting inference var.
        // Takes its use-site float width when constrained, falls back
        // to `f64` otherwise (see `default_unresolved_float_vars`).
        self.infer.fresh_float_var(self.tcx)
    }

    pub(super) fn type_from_primitive(&mut self, prim: PrimitiveTy) -> Ty {
        match prim {
            PrimitiveTy::Bool => self.tcx.bool_ty(),
            PrimitiveTy::Char => self.tcx.char_ty(),
            PrimitiveTy::String => self.tcx.string_ty(),
            PrimitiveTy::Int(width) => self.tcx.int_ty(int_ty_from_width(width, true)),
            PrimitiveTy::UInt(width) => self.tcx.int_ty(int_ty_from_width(width, false)),
            PrimitiveTy::Float(FloatWidth::W32) => self.tcx.float_ty(FloatTy::F32),
            PrimitiveTy::Float(FloatWidth::W64) => self.tcx.float_ty(FloatTy::F64),
            PrimitiveTy::Never => self.tcx.never(),
            PrimitiveTy::Unit => self.tcx.unit(),
        }
    }
}
