//! Compiling `match` and `select` expressions and pattern tests.

use super::*;

impl<'tcx> FnBuilder<'tcx> {
    /// Native `match` compilation. Emits the scrutinee once, then a
    /// test-and-branch chain per arm: each arm's pattern lowers to a
    /// sequence of shape tests (`VariantIs` / `StructIs` / literal
    /// `Eq` / range compares) that branch to the next arm on failure
    /// and extract sub-values into freshly-bound registers on
    /// success. Every pattern shape - including or-patterns that bind -
    /// lowers natively via [`Self::emit_pattern_test`].
    pub(crate) fn compile_match(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[gossamer_hir::HirMatchArm],
        _whole: &HirExpr,
    ) -> RuntimeResult<Reg> {
        let scrut = self.compile_expr(scrutinee)?;
        let result = self.alloc_reg();
        let mut end_jumps: Vec<InstrIdx> = Vec::new();
        let scrut_ty = scrutinee.ty;
        // Move-on-last-use: a guard-free `match` may drain the matched
        // payload out of a uniquely-owned scrutinee instead of cloning
        // it. Eligible when the scrutinee is a consumable local or a
        // fresh temporary (e.g. the `?` desugar's `match parse(x) { ...
        // }`) - both leave a register nothing reads after the match.
        // Guards are excluded because a failed guard would fall through
        // and re-extract the drained scrutinee.
        let consume_eligible =
            self.value_consumable_here(scrutinee) && arms.iter().all(|arm| arm.guard.is_none());
        for arm in arms {
            self.push_scope();
            let mut fails: Vec<InstrIdx> = Vec::new();
            // Draining the scrutinee is only safe when this arm cannot fail
            // a refutable sub-test *after* extracting (and emptying) a field
            // and then fall through to a later arm that re-reads it. See
            // `pattern_consume_safe`.
            let arm_consume =
                consume_eligible && crate::compile::consume::pattern_consume_safe(&arm.pattern);
            self.emit_pattern_test_ex(scrut, &arm.pattern, &mut fails, arm_consume)?;
            // Tag collection-typed pattern bindings (`Some(arr)`, …) so a
            // `for x in <binding>` in the arm body iterates by index. The
            // binding's own `Path` carries an unresolved var when inferred,
            // so the type comes from the resolved scrutinee type instead.
            let mut coll_names: Vec<String> = Vec::new();
            self.collect_collection_binding_names(&arm.pattern, Some(scrut_ty), &mut coll_names);
            for name in coll_names {
                if let Some(tr) = self.lookup_local(&name) {
                    self.collection_locals.insert(tr.reg);
                }
            }
            if let Some(guard) = &arm.guard {
                let g = self.compile_expr(guard)?;
                fails.push(self.emit(Op::BranchIfNot { cond: g, target: 0 }));
            }
            // Hand the arm's result to the match register instead of
            // cloning when its last use is here (the `?` desugar's
            // `Ok(__try_value) => __try_value` keeps the unwrapped value
            // uniquely owned this way).
            let consume_body = self.value_consumable_here(&arm.body);
            let body_reg = self.compile_expr(&arm.body)?;
            self.writeback_variant_payload(scrutinee, scrut, arm)?;
            if consume_body {
                self.emit(Op::MoveConsume {
                    dst: result,
                    src: body_reg,
                });
            } else {
                self.emit(Op::Move {
                    dst: result,
                    src: body_reg,
                });
            }
            end_jumps.push(self.emit(Op::Jump { target: 0 }));
            self.pop_scope();
            let next = self.cur_idx();
            for f in fails {
                self.patch_jump(f, next);
            }
        }
        // No arm matched. Exhaustiveness covers well-typed programs, but a
        // checker blind spot (e.g. an unenumerable integer payload like
        // `Some(2)` against `Some(0) | Some(1) | None`) can reach here.
        // Panic cleanly with the same message the compiled tiers emit
        // rather than falling through with a zero/default value.
        let msg = self.const_idx(
            ConstKey::String(NON_EXHAUSTIVE_MATCH_MESSAGE.to_string()),
            Value::String(SmolStr::from(NON_EXHAUSTIVE_MATCH_MESSAGE)),
        );
        self.emit(Op::Panic { msg });
        let end = self.cur_idx();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(result)
    }

    /// Compiles a `match` whose value is discarded (statement position).
    /// Each arm body is compiled in statement context via
    /// `compile_expr_discarded`, so a tail-position in-place mutation
    /// (`v.push(x)`) lowers to its dedicated op rather than the
    /// value-returning builtin path that deep-copies the whole
    /// collection per call. Mirrors `compile_match` minus the result
    /// register and per-arm `Move`.
    pub(crate) fn compile_match_discarded(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[gossamer_hir::HirMatchArm],
        _whole: &HirExpr,
    ) -> RuntimeResult<()> {
        let scrut = self.compile_expr(scrutinee)?;
        let mut end_jumps: Vec<InstrIdx> = Vec::new();
        let scrut_ty = scrutinee.ty;
        // Drains the payload out of a scrutinee nothing reads afterwards, on
        // the same terms as `compile_match`.
        let consume_eligible =
            self.value_consumable_here(scrutinee) && arms.iter().all(|arm| arm.guard.is_none());
        for arm in arms {
            self.push_scope();
            let mut fails: Vec<InstrIdx> = Vec::new();
            let arm_consume =
                consume_eligible && crate::compile::consume::pattern_consume_safe(&arm.pattern);
            self.emit_pattern_test_ex(scrut, &arm.pattern, &mut fails, arm_consume)?;
            let mut coll_names: Vec<String> = Vec::new();
            self.collect_collection_binding_names(&arm.pattern, Some(scrut_ty), &mut coll_names);
            for name in coll_names {
                if let Some(tr) = self.lookup_local(&name) {
                    self.collection_locals.insert(tr.reg);
                }
            }
            if let Some(guard) = &arm.guard {
                let g = self.compile_expr(guard)?;
                fails.push(self.emit(Op::BranchIfNot { cond: g, target: 0 }));
            }
            self.compile_expr_discarded(&arm.body)?;
            self.writeback_variant_payload(scrutinee, scrut, arm)?;
            end_jumps.push(self.emit(Op::Jump { target: 0 }));
            self.pop_scope();
            let next = self.cur_idx();
            for f in fails {
                self.patch_jump(f, next);
            }
        }
        let msg = self.const_idx(
            ConstKey::String(NON_EXHAUSTIVE_MATCH_MESSAGE.to_string()),
            Value::String(SmolStr::from(NON_EXHAUSTIVE_MATCH_MESSAGE)),
        );
        self.emit(Op::Panic { msg });
        let end = self.cur_idx();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(())
    }

    /// Native `select { … }` compilation. Evaluates each arm's
    /// channel (and value, for sends) into registers up front, emits a
    /// single [`Op::Select`] referencing a contiguous range of
    /// [`crate::bytecode::SelectArmMeta`] entries, then compiles every
    /// arm body as a basic block the op jumps to. Recv arms destructure
    /// the received value (written into the arm's `bind_reg` by the
    /// handler) at the top of their block. The handler's poll/park loop
    /// operates over `Value::Channel`.
    pub(crate) fn compile_select(
        &mut self,
        arms: &[gossamer_hir::HirSelectArm],
    ) -> RuntimeResult<Reg> {
        use crate::bytecode::{SelectArmKind, SelectArmMeta};
        use gossamer_hir::HirSelectOp;

        // An empty `select {}` blocks forever in Go; degrade it to Unit
        // rather than emitting an arm-less op that would spin in the
        // park loop.
        if arms.is_empty() {
            return Ok(self.load_unit());
        }

        let result = self.alloc_reg();
        let first = u32::try_from(self.select_arms.len())
            .map_err(|_| RuntimeError::Unsupported("too many select arms in one function"))?;

        // Pass 1: evaluate operand expressions (channels / send values)
        // and pre-allocate recv binding registers. The `body_block`
        // index is filled in pass 2 once each body is laid down.
        for arm in arms {
            let meta = match &arm.op {
                HirSelectOp::Recv { channel, .. } => {
                    let channel_reg = self.compile_expr(channel)?;
                    let bind_reg = self.alloc_reg();
                    SelectArmMeta {
                        kind: SelectArmKind::Recv,
                        channel_reg,
                        value_reg: 0,
                        bind_reg,
                        body_block: 0,
                    }
                }
                HirSelectOp::Send { channel, value } => {
                    let channel_reg = self.compile_expr(channel)?;
                    let value_reg = self.compile_expr(value)?;
                    SelectArmMeta {
                        kind: SelectArmKind::Send,
                        channel_reg,
                        value_reg,
                        bind_reg: 0,
                        body_block: 0,
                    }
                }
                HirSelectOp::Default => SelectArmMeta {
                    kind: SelectArmKind::Default,
                    channel_reg: 0,
                    value_reg: 0,
                    bind_reg: 0,
                    body_block: 0,
                },
            };
            self.select_arms.push(meta);
        }
        let count = u16::try_from(arms.len())
            .map_err(|_| RuntimeError::Unsupported("too many select arms in one select"))?;
        self.emit(Op::Select { first, count });

        // Pass 2: each arm body becomes a basic block. The handler
        // jumps to one of them; every block moves its result into the
        // shared `result` register and jumps to the continuation.
        let mut end_jumps: Vec<InstrIdx> = Vec::new();
        for (i, arm) in arms.iter().enumerate() {
            let meta_idx = first as usize + i;
            self.select_arms[meta_idx].body_block = self.cur_idx();
            self.push_scope();
            if let HirSelectOp::Recv { pattern, .. } = &arm.op {
                let bind_reg = self.select_arms[meta_idx].bind_reg;
                self.bind_pattern_locals(pattern, bind_reg)?;
            }
            let body_reg = self.compile_expr(&arm.body)?;
            self.emit(Op::Move {
                dst: result,
                src: body_reg,
            });
            end_jumps.push(self.emit(Op::Jump { target: 0 }));
            self.pop_scope();
        }
        let end = self.cur_idx();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(result)
    }

    /// Emits the shape-test + binding-extraction sequence for one
    /// pattern against the value in `scrut`. Pushes a branch index
    /// onto `fails` for every test that must jump to the next arm on
    /// mismatch; on the fall-through (match) path the pattern's
    /// bindings are live in the current scope.
    pub(crate) fn emit_pattern_test(
        &mut self,
        scrut: Reg,
        pat: &HirPat,
        fails: &mut Vec<InstrIdx>,
    ) -> RuntimeResult<()> {
        self.emit_pattern_test_ex(scrut, pat, fails, false)
    }

    /// Like [`Self::emit_pattern_test`] but with a `consume` flag: when
    /// set, the scrutinee is a uniquely-owned value the arm may drain
    /// (guard-free `match` on a consumable local), so variant-field and
    /// binding extraction move instead of clone. The runtime
    /// `Arc::get_mut` guard on `VariantFieldConsume` degrades a
    /// still-shared scrutinee to a safe clone.
    pub(crate) fn emit_pattern_test_ex(
        &mut self,
        scrut: Reg,
        pat: &HirPat,
        fails: &mut Vec<InstrIdx>,
        consume: bool,
    ) -> RuntimeResult<()> {
        match &pat.kind {
            HirPatKind::Wildcard | HirPatKind::Rest => {}
            HirPatKind::Binding { name, mutable } => {
                // Copy into a fresh reg so a `let mut`-style rebind in
                // the arm body can't clobber the scrutinee register.
                // Under `consume` the scrutinee is a read-once uniquely
                // owned value, so hand it over instead of cloning. A `mut`
                // binding is a value of its own: a table it holds sits
                // behind a shared handle, so it takes a copy of that too.
                let r = self.alloc_reg();
                if consume {
                    self.emit(Op::MoveConsume { dst: r, src: scrut });
                } else if *mutable {
                    self.emit(Op::CloneMapLike { dst: r, src: scrut });
                } else {
                    self.emit(Op::Move { dst: r, src: scrut });
                }
                self.bind_local(
                    &name.name,
                    TypedReg {
                        reg: r,
                        kind: RegKind::Value,
                    },
                );
            }
            HirPatKind::Literal(lit) => {
                let lit_reg = self.compile_literal(lit)?;
                let eq = self.alloc_reg();
                self.emit(Op::Eq {
                    dst: eq,
                    lhs: scrut,
                    rhs: lit_reg,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: eq,
                    target: 0,
                }));
            }
            HirPatKind::Variant { name, fields } => {
                let name_idx = self.shape_name_idx(name.name.as_str());
                let arity = u16::try_from(fields.len())
                    .map_err(|_| RuntimeError::Unsupported("variant arity exceeds 65535"))?;
                let test = self.alloc_reg();
                self.emit(Op::VariantIs {
                    dst: test,
                    src: scrut,
                    name_idx,
                    arity,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: test,
                    target: 0,
                }));
                for (i, fp) in fields.iter().enumerate() {
                    let fr = self.alloc_reg();
                    let idx = u16::try_from(i).expect("field index overflow");
                    if consume {
                        self.emit(Op::VariantFieldConsume {
                            dst: fr,
                            src: scrut,
                            idx,
                        });
                    } else {
                        self.emit(Op::VariantField {
                            dst: fr,
                            src: scrut,
                            idx,
                        });
                    }
                    // A drained field is uniquely owned, so propagate
                    // `consume` into its sub-pattern.
                    self.emit_pattern_test_ex(fr, fp, fails, consume)?;
                }
            }
            HirPatKind::Struct { name, fields, .. } => {
                let name_idx = self.shape_name_idx(name.name.as_str());
                let test = self.alloc_reg();
                self.emit(Op::StructIs {
                    dst: test,
                    src: scrut,
                    name_idx,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: test,
                    target: 0,
                }));
                for fp in fields {
                    let fname_idx = self.const_idx(
                        ConstKey::String(fp.name.name.clone()),
                        Value::String(SmolStr::from(fp.name.name.as_str())),
                    );
                    let fr = self.alloc_reg();
                    let cache_idx = self.alloc_field_cache_idx();
                    self.emit(Op::FieldGet {
                        dst: fr,
                        receiver: scrut,
                        name_idx: fname_idx,
                        cache_idx,
                    });
                    if let Some(sub) = &fp.pattern {
                        self.emit_pattern_test(fr, sub, fails)?;
                    } else {
                        // `Struct { field }` shorthand binds `field`.
                        self.bind_local(
                            &fp.name.name,
                            TypedReg {
                                reg: fr,
                                kind: RegKind::Value,
                            },
                        );
                    }
                }
            }
            HirPatKind::Ref { inner, .. } => {
                self.emit_pattern_test_ex(scrut, inner, fails, consume)?;
            }
            HirPatKind::At { name, sub, mutable } => {
                self.emit_pattern_test(scrut, sub, fails)?;
                let r = self.alloc_reg();
                if *mutable {
                    self.emit(Op::CloneMapLike { dst: r, src: scrut });
                } else {
                    self.emit(Op::Move { dst: r, src: scrut });
                }
                self.bind_local(
                    &name.name,
                    TypedReg {
                        reg: r,
                        kind: RegKind::Value,
                    },
                );
            }
            HirPatKind::Range { lo, hi, inclusive } => {
                let lo_reg = self.compile_literal(lo)?;
                let ge = self.alloc_reg();
                self.emit(Op::Ge {
                    dst: ge,
                    lhs: scrut,
                    rhs: lo_reg,
                });
                fails.push(self.emit(Op::BranchIfNot {
                    cond: ge,
                    target: 0,
                }));
                let hi_reg = self.compile_literal(hi)?;
                let cmp = self.alloc_reg();
                if *inclusive {
                    self.emit(Op::Le {
                        dst: cmp,
                        lhs: scrut,
                        rhs: hi_reg,
                    });
                } else {
                    self.emit(Op::Lt {
                        dst: cmp,
                        lhs: scrut,
                        rhs: hi_reg,
                    });
                }
                fails.push(self.emit(Op::BranchIfNot {
                    cond: cmp,
                    target: 0,
                }));
            }
            HirPatKind::Tuple(parts) => {
                let rest_pos = parts
                    .iter()
                    .position(|p| matches!(p.kind, HirPatKind::Rest));
                for (i, part) in parts.iter().enumerate() {
                    if matches!(part.kind, HirPatKind::Rest) {
                        continue;
                    }
                    let elem = self.alloc_reg();
                    match rest_pos {
                        Some(rp) if i > rp => {
                            // Element after `..` - index from the end.
                            let from_end = parts.len() - 1 - i;
                            self.emit(Op::TupleTailIndex {
                                dst: elem,
                                receiver: scrut,
                                offset_from_end: u32::try_from(from_end)
                                    .expect("tuple tail index overflow"),
                            });
                        }
                        _ => {
                            self.emit(Op::TupleIndex {
                                dst: elem,
                                receiver: scrut,
                                index: u32::try_from(i).expect("tuple index overflow"),
                            });
                        }
                    }
                    self.emit_pattern_test(elem, part, fails)?;
                }
            }
            HirPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                // `len = scrut.len()` as a boxed `Value::Int`.
                let len_reg = self.alloc_reg();
                let len_name = self.global_idx("len");
                let cache_idx = self.alloc_cache_idx();
                self.emit(Op::MethodCall {
                    dst: len_reg,
                    receiver: scrut,
                    name_idx: len_name,
                    args: 0,
                    argc: 0,
                    cache_idx,
                });
                let n_prefix = i64::try_from(prefix.len())
                    .map_err(|_| RuntimeError::Unsupported("slice prefix too long"))?;
                let n_suffix = i64::try_from(suffix.len())
                    .map_err(|_| RuntimeError::Unsupported("slice suffix too long"))?;
                // Length guard: `len >= n_prefix + n_suffix` with a `..`,
                // `len == n_prefix` for a fixed-length slice pattern.
                let bound = if rest.is_some() {
                    n_prefix + n_suffix
                } else {
                    n_prefix
                };
                let bound_reg = self.load_int_value(bound);
                let test = self.alloc_reg();
                if rest.is_some() {
                    self.emit(Op::Ge {
                        dst: test,
                        lhs: len_reg,
                        rhs: bound_reg,
                    });
                } else {
                    self.emit(Op::Eq {
                        dst: test,
                        lhs: len_reg,
                        rhs: bound_reg,
                    });
                }
                fails.push(self.emit(Op::BranchIfNot {
                    cond: test,
                    target: 0,
                }));
                // Prefix elements: `scrut[i]`.
                for (i, sub) in prefix.iter().enumerate() {
                    let idx_reg = self.load_int_value(i as i64);
                    let elem = self.alloc_reg();
                    self.emit(Op::IndexGet {
                        dst: elem,
                        base: scrut,
                        index: idx_reg,
                    });
                    self.emit_pattern_test(elem, sub, fails)?;
                }
                // Suffix elements: `scrut[len - n_suffix + j]`.
                for (j, sub) in suffix.iter().enumerate() {
                    let off_reg = self.load_int_value(j as i64 - n_suffix);
                    let idx_reg = self.alloc_reg();
                    let add_cache = self.next_arith_cache();
                    self.emit(Op::AddInt {
                        dst: idx_reg,
                        lhs: len_reg,
                        rhs: off_reg,
                        cache_idx: add_cache,
                    });
                    let elem = self.alloc_reg();
                    self.emit(Op::IndexGet {
                        dst: elem,
                        base: scrut,
                        index: idx_reg,
                    });
                    self.emit_pattern_test(elem, sub, fails)?;
                }
                // `..rest` binding: `scrut.slice(n_prefix, len - n_suffix)`
                // yields `Ok(sub)`; extract the payload and bind it.
                if let Some(rest) = rest {
                    if let HirPatKind::Binding { name, .. } = &rest.kind {
                        let lo_reg = self.load_int_value(n_prefix);
                        let neg_suffix = self.load_int_value(-n_suffix);
                        let hi_reg = self.alloc_reg();
                        let hi_cache = self.next_arith_cache();
                        self.emit(Op::AddInt {
                            dst: hi_reg,
                            lhs: len_reg,
                            rhs: neg_suffix,
                            cache_idx: hi_cache,
                        });
                        let args_start = self.next_reg;
                        self.next_reg = self
                            .next_reg
                            .checked_add(2)
                            .expect("register overflow reserving slice args");
                        self.emit(Op::Move {
                            dst: args_start,
                            src: lo_reg,
                        });
                        self.emit(Op::Move {
                            dst: args_start + 1,
                            src: hi_reg,
                        });
                        let slice_res = self.alloc_reg();
                        let slice_name = self.global_idx("slice");
                        let slice_cache = self.alloc_cache_idx();
                        self.emit(Op::MethodCall {
                            dst: slice_res,
                            receiver: scrut,
                            name_idx: slice_name,
                            args: args_start,
                            argc: 2,
                            cache_idx: slice_cache,
                        });
                        let sub = self.alloc_reg();
                        self.emit(Op::VariantField {
                            dst: sub,
                            src: slice_res,
                            idx: 0,
                        });
                        self.bind_local(
                            &name.name,
                            TypedReg {
                                reg: sub,
                                kind: RegKind::Value,
                            },
                        );
                    }
                }
            }
            HirPatKind::Or(alts) if !pattern_has_binding(pat) => {
                // No alternative binds, so each alt is a pure test.
                // Emit them as a short-circuit OR: the first alt that
                // matches jumps past the rest to the shared
                // continuation; if every alt fails, fall through to
                // the arm-fail branch.
                let mut matched: Vec<InstrIdx> = Vec::new();
                for alt in alts {
                    let mut alt_fails: Vec<InstrIdx> = Vec::new();
                    self.emit_pattern_test(scrut, alt, &mut alt_fails)?;
                    // This alt matched - jump to the continuation.
                    matched.push(self.emit(Op::Jump { target: 0 }));
                    // This alt failed - next alt starts here.
                    let next_alt = self.cur_idx();
                    for f in alt_fails {
                        self.patch_jump(f, next_alt);
                    }
                }
                // All alternatives failed: jump to the arm-fail target.
                fails.push(self.emit(Op::Jump { target: 0 }));
                // Matched continuation.
                let cont = self.cur_idx();
                for m in matched {
                    self.patch_jump(m, cont);
                }
            }
            HirPatKind::Or(alts) => {
                // Binding or-pattern: every alternative binds the same
                // set of names (a typecheck invariant). One shared
                // register per name is the single home the arm body
                // reads, regardless of which alternative won - so each
                // alternative copies its freshly-extracted bindings
                // into those shared registers on its match path before
                // jumping to the continuation. Mirrors the MIR lowering
                // (`gossamer-mir/.../ctrl.rs`), which writes every
                // alternative's bindings into common slots.
                let mut names: Vec<String> = Vec::new();
                collect_pattern_binding_names(pat, &mut names);
                let shared: Vec<(String, Reg)> = names
                    .into_iter()
                    .map(|name| (name, self.alloc_reg()))
                    .collect();

                let mut matched: Vec<InstrIdx> = Vec::new();
                for alt in alts {
                    debug_assert!(
                        {
                            let mut alt_names = Vec::new();
                            collect_pattern_binding_names(alt, &mut alt_names);
                            alt_names.len() == shared.len()
                                && alt_names.iter().all(|n| shared.iter().any(|(s, _)| s == n))
                        },
                        "or-pattern alternatives must bind the same set of names"
                    );
                    let mut alt_fails: Vec<InstrIdx> = Vec::new();
                    // Compile the alternative in an inner scope so its
                    // leaf bindings land in fresh registers that this
                    // scope owns; relocate each into its shared home,
                    // then discard the scope.
                    self.push_scope();
                    self.emit_pattern_test(scrut, alt, &mut alt_fails)?;
                    for (name, dst) in &shared {
                        if let Some(src) = self.lookup_local(name) {
                            let src_v = self.as_value(src);
                            self.emit(Op::Move {
                                dst: *dst,
                                src: src_v,
                            });
                        }
                    }
                    self.pop_scope();
                    // This alt matched - jump to the continuation.
                    matched.push(self.emit(Op::Jump { target: 0 }));
                    // This alt failed - next alt starts here.
                    let next_alt = self.cur_idx();
                    for f in alt_fails {
                        self.patch_jump(f, next_alt);
                    }
                }
                // All alternatives failed: jump to the arm-fail target.
                fails.push(self.emit(Op::Jump { target: 0 }));
                // Matched continuation: expose each shared register to
                // the arm body (and any guard) under its bound name.
                let cont = self.cur_idx();
                for m in matched {
                    self.patch_jump(m, cont);
                }
                for (name, reg) in &shared {
                    self.bind_local(
                        name,
                        TypedReg {
                            reg: *reg,
                            kind: RegKind::Value,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}
