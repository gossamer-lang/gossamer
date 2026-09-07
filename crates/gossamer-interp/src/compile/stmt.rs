#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    /// Registers a coverage counter slot for `span`'s `(file, line)`
    /// and emits an [`Op::CovHit`] against it. No-op unless the VM
    /// loaded the program with coverage active (`self.cov` set). The
    /// slot is registered at compile time so uncovered lines still
    /// appear in the lcov report with a zero hit count.
    fn emit_coverage_hit(&mut self, span: gossamer_lex::Span) {
        let Some(map) = self.cov else {
            return;
        };
        let line = map.line_col(span.file, span.start).line;
        let file = map.file_name(span.file);
        let slot = gossamer_runtime::coverage::register(file, line, 0);
        let slot = u32::try_from(slot).unwrap_or(u32::MAX);
        self.emit(Op::CovHit { slot });
    }

    /// Tags a freshly-bound register when its initializer constructs a
    /// `flag::Set` or a duration-flag cell. The typechecker leaves both
    /// shapes as unresolved inference vars, so the method-form
    /// `time::Duration` accessors (`cell.as_millis()`) rely on the
    /// `duration_cell_locals` tag rather than the receiver's static type.
    fn record_flag_init(&mut self, init: &HirExpr, reg: Reg) {
        match &init.kind {
            HirExprKind::Call { callee, .. } => {
                if let HirExprKind::Path { segments, .. } = &callee.kind {
                    let n = segments.len();
                    if n >= 2
                        && segments[n - 2].name.as_str() == "Set"
                        && segments[n - 1].name.as_str() == "new"
                    {
                        self.flag_set_locals.insert(reg);
                    }
                }
            }
            HirExprKind::MethodCall { receiver, name, .. } if name.name.as_str() == "duration" => {
                if let HirExprKind::Path { segments, .. } = &receiver.kind
                    && let [seg] = segments.as_slice()
                    && let Some(tr) = self.lookup_local(&seg.name)
                    && self.flag_set_locals.contains(&tr.reg)
                {
                    self.duration_cell_locals.insert(reg);
                }
            }
            _ => {}
        }
    }

    /// Tags a freshly-bound register as holding a Vec. Prefer the static
    /// initializer type so explicit conversions such as `array.into()` and
    /// `Vec::from(array)` retain Vec mutability. Constructor recognition is a
    /// fallback for unresolved empty Vecs. Array literals are deliberately not
    /// treated as Vecs because `[T; N]` and `Vec<T>` have distinct identities.
    fn record_vec_init(&mut self, init: &HirExpr, reg: Reg) {
        let is_vec_ctor = matches!(self.tcx.kind(init.ty), Some(TyKind::Vec(_)))
            || match &init.kind {
                HirExprKind::Call { callee, .. } => match &callee.kind {
                    HirExprKind::Path { segments, .. } => {
                        let n = segments.len();
                        n >= 2
                            && segments[n - 2].name.as_str() == "Vec"
                            && matches!(segments[n - 1].name.as_str(), "new" | "with_capacity")
                    }
                    _ => false,
                },
                HirExprKind::MethodCall { name, args, .. } => {
                    args.is_empty() && matches!(name.name.as_str(), "collect" | "to_vec")
                }
                _ => false,
            };
        if is_vec_ctor || Self::init_is_sequence_snapshot(init) {
            self.collection_locals.insert(reg);
        } else {
            // A register a scope gave back is bound again by the next
            // local, whose value is its own; the tag says what THIS
            // binding holds, so a binding that is not a Vec clears it.
            self.collection_locals.remove(&reg);
        }
    }

    /// `true` when `init` is a temporary sequence literal. The value is an
    /// indexable snapshot, so the for-loop's `&mut` binding is driven by index
    /// rather than by the `next()` cursor protocol, which such a value does
    /// not carry. `iter()` and the adapters past it answer iterator state,
    /// which carries the cursor and is driven through it.
    fn init_is_sequence_snapshot(init: &HirExpr) -> bool {
        matches!(&init.kind, HirExprKind::Array(_))
    }

    fn record_uint_display_init(&mut self, init: &HirExpr, reg: Reg) {
        if self.expr_has_uint_display_provenance(init) {
            self.uint_display_locals.insert(reg);
        } else {
            self.uint_display_locals.remove(&reg);
        }
    }

    /// `true` when `receiver` is a path bound to a `flag::Set` duration
    /// cell, so a `time::Duration` accessor in method form dispatches on
    /// the cell's element type.
    pub(crate) fn receiver_is_duration_cell(&self, receiver: &HirExpr) -> bool {
        if let HirExprKind::Path { segments, .. } = &receiver.kind
            && let [seg] = segments.as_slice()
            && let Some(tr) = self.lookup_local(&seg.name)
        {
            return self.duration_cell_locals.contains(&tr.reg);
        }
        false
    }

    /// Returns `true` when the statement diverges (return/break/continue).
    pub(crate) fn compile_stmt(&mut self, stmt: &HirStmt) -> RuntimeResult<bool> {
        self.emit_coverage_hit(stmt.span);
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if matches!(pattern.kind, HirPatKind::Wildcard)
                    && let Some(init) = init
                {
                    self.compile_expr_discarded(init)?;
                    return Ok(false);
                }
                if let HirPatKind::Binding { name, .. } = &pattern.kind {
                    // A direct local reference shares the source
                    // register. Disable flat-array specialisation for that
                    // register: the generic projected-store path preserves
                    // the alias, whereas the flat path assumes one concrete
                    // non-reference binding owns its representation.
                    if let Some(HirExpr {
                        kind:
                            HirExprKind::Unary {
                                op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
                                operand,
                            },
                        ..
                    }) = init
                        && let HirExprKind::Path { segments, .. } = &operand.kind
                        && let [segment] = segments.as_slice()
                        && let Some(source) = self.lookup_local(&segment.name)
                    {
                        self.flat_int_locals.remove(&source.reg);
                        self.flat_float_locals.remove(&source.reg);
                        self.reference_alias_regs.insert(source.reg);
                        self.bind_reference_local(&name.name, source);
                        return Ok(false);
                    }
                    if let Some(init) = init {
                        // Compile the init in its natural kind.
                        // Most exprs produce a freshly-allocated
                        // reg we can bind directly; only a bare
                        // path lookup (`let y = x`) aliases an
                        // existing reg, so in that case copy
                        // into a fresh slot.
                        let tr = self.compile_expr_ex(init)?;
                        if let Some(home) = self.returned_mut_ref_home(init) {
                            self.flat_int_locals.remove(&home.reg);
                            self.flat_float_locals.remove(&home.reg);
                            self.reference_alias_regs.insert(home.reg);
                            self.bind_reference_local(&name.name, home);
                            return Ok(false);
                        }
                        // Move-on-last-use: a `let y = x` aliasing a
                        // consumable local hands the aggregate over
                        // instead of cloning it into the fresh slot.
                        let consume_init = tr.kind == RegKind::Value
                            && self
                                .consumable_path(init)
                                .and_then(|name| self.lookup_local(name))
                                .is_some_and(|home| home.reg == tr.reg);
                        // A bare path aliases its source register, and so does
                        // any initializer the compiler folded down to one - an
                        // inlined call that answers its own argument, say.
                        let aliases_live_binding = is_place_expr(init)
                            || (tr.kind == RegKind::Value && self.reg_is_bound_local(tr.reg));
                        let typed = if consume_init {
                            let dst = self.alloc_reg();
                            self.emit(Op::MoveConsume { dst, src: tr.reg });
                            TypedReg {
                                reg: dst,
                                kind: RegKind::Value,
                            }
                        } else if aliases_live_binding
                            && (self.expr_is_map(init)
                                || self.expr_is_hashset(init)
                                || self.expr_is_slot_container(init)
                                || self.expr_is_aggregate_with_container(init))
                        {
                            // `Map` / `Set` entries live behind `Arc<Mutex<_>>`,
                            // so the plain register copy `bind_to_fresh` uses for
                            // every other aliased binding would share the same
                            // backing table between `y` and `x` in `let y = x` -
                            // unlike `Vec`, whose `Arc<Vec<Value>>` has no
                            // interior mutability and gets independent storage
                            // for free from the next mutating op's `Arc::make_mut`.
                            let dst = self.alloc_reg();
                            self.emit(Op::CloneMapLike { dst, src: tr.reg });
                            TypedReg {
                                reg: dst,
                                kind: RegKind::Value,
                            }
                        } else if aliases_live_binding {
                            self.bind_to_fresh(tr)
                        } else {
                            tr
                        };
                        self.record_flag_init(init, typed.reg);
                        self.record_vec_init(init, typed.reg);
                        if matches!(
                            self.tcx.kind(self.unwrap_ref(pattern.ty)),
                            Some(TyKind::Iterator(_))
                        ) || matches!(
                            self.tcx.kind(self.unwrap_ref(init.ty)),
                            Some(TyKind::Iterator(_))
                        ) {
                            self.lazy_iterator_locals.insert(typed.reg);
                        }
                        self.record_uint_display_init(init, typed.reg);
                        self.bind_local(&name.name, typed);
                        self.install_capture_cell(&name.name, typed, init.ty);
                    } else {
                        // Declared-only - default to Value; an
                        // assignment before read will overwrite.
                        let reg = self.alloc_reg();
                        self.bind_local(
                            &name.name,
                            TypedReg {
                                reg,
                                kind: RegKind::Value,
                            },
                        );
                    }
                } else if let Some(init) = init {
                    // Destructuring - compile init to a Value reg,
                    // then bind sub-patterns via TupleIndex. Only
                    // tuple/wildcard/binding shapes are handled
                    // here; complex patterns error out instead of
                    // silently dropping the bindings.
                    let init_reg = self.compile_expr(init)?;
                    self.bind_pattern_locals(pattern, init_reg)?;
                }
                Ok(false)
            }
            HirStmtKind::Expr { expr, .. } => {
                self.compile_expr_discarded(expr)?;
                Ok(expr_diverges(expr))
            }
            HirStmtKind::Defer(expr) => {
                // Register for block-scoped execution: emitted LIFO when
                // control leaves the enclosing block by any path.
                // `compile_block` emits the frame on normal fall-through;
                // `return` / `break` / `continue` emit the pending frames
                // before their jump. A `defer` never diverges the enclosing
                // statement sequence.
                if let Some(frame) = self.defer_stack.last_mut() {
                    frame.push(expr.clone());
                }
                Ok(false)
            }
            HirStmtKind::Item(item) => match &item.kind {
                gossamer_hir::HirItemKind::Const(_)
                | gossamer_hir::HirItemKind::Fn(_)
                | gossamer_hir::HirItemKind::Adt(_) => Ok(false),
                _ => Err(RuntimeError::Unsupported("nested items")),
            },
        }
    }

    /// Compiles `expr` purely for its side effects, discarding its value.
    /// An assignment lowers straight to its store and an in-place Vec
    /// mutation to its dedicated op, so neither materialises the dead
    /// `LoadConst(Unit)` its expression form would otherwise yield - the
    /// one register nothing reads in a tight loop body. Every other shape
    /// compiles normally and leaves its result register unread.
    pub(crate) fn compile_expr_discarded(&mut self, expr: &HirExpr) -> RuntimeResult<()> {
        match &expr.kind {
            HirExprKind::Assign { place, value } => {
                self.compile_assign_store(place, value)?;
                return Ok(());
            }
            // Propagate the discarded context into the tails of
            // control-flow expressions so a tail-position in-place
            // mutation (`v.push(x)`) still lowers to its dedicated op.
            // Without this, `if c { v.push(x) }` compiles its then-tail
            // as a value through the builtin path, which deep-copies the
            // whole collection on every call - O(n) per push, O(n^2) over
            // a growth loop.
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.compile_if_discarded(condition, then_branch, else_branch.as_deref())?;
                return Ok(());
            }
            HirExprKind::Block(block) => {
                self.compile_block_inner(block, true)?;
                return Ok(());
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.compile_match_discarded(scrutinee, arms, expr)?;
                return Ok(());
            }
            // Handled by a dedicated in-place Vec op; result discarded. A
            // `false` from the guard leaves nothing emitted, so the call
            // falls through to the generic `compile_expr` below.
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner: None,
            } if self.try_compile_inplace_vec_stmt(receiver, name, args)? => {
                return Ok(());
            }
            _ => {}
        }
        let _ = self.compile_expr(expr)?;
        Ok(())
    }

    /// Compiles `place = value` in expression position, yielding the
    /// `()` result in a register so callers that consume it (a block
    /// tail, `let x = (a = b)`) see a real value.
    pub(crate) fn compile_assign(
        &mut self,
        place: &HirExpr,
        value: &HirExpr,
    ) -> RuntimeResult<Reg> {
        self.compile_assign_store(place, value)?;
        Ok(self.load_unit())
    }

    /// Emits the store(s) for `place = value` and stops there. The unit
    /// such an expression yields is dead in statement position, so the
    /// statement path calls this to avoid one `LoadConst(Unit)` per
    /// assignment. Sub-expressions still compile normally, so a nested
    /// `a = (b = c)` materialises its inner unit through the wrapper.
    pub(crate) fn compile_assign_store(
        &mut self,
        place: &HirExpr,
        value: &HirExpr,
    ) -> RuntimeResult<()> {
        // Rebinding a reference local changes its referent, not the old
        // referent value. Direct local references share a register with their
        // current source, so update that binding during lowering before any
        // ordinary assignment can write the old source.
        if let HirExprKind::Path { segments, .. } = &place.kind
            && let [name] = segments.as_slice()
            && self.is_reference_binding(&name.name)
            && let HirExprKind::Unary {
                op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
                operand,
            } = &value.kind
        {
            let source = self.compile_expr_ex(operand)?;
            let source_is_named_local =
                reference_operand_local_name(operand).and_then(|source_name| {
                    self.lookup_local(source_name)
                        .filter(|local| local.reg == source.reg && local.kind == source.kind)
                });
            let copied_temporary = source_is_named_local.is_none();
            let target = if copied_temporary {
                self.bind_to_fresh(source)
            } else {
                source
            };
            if self.rebind_reference_local(&name.name, target) {
                self.flat_int_locals.remove(&source.reg);
                self.flat_float_locals.remove(&source.reg);
                self.flat_int_locals.remove(&target.reg);
                self.flat_float_locals.remove(&target.reg);
                self.reference_alias_regs.insert(target.reg);
                if copied_temporary {
                    self.escaped_reference_reg_floor = self
                        .escaped_reference_reg_floor
                        .max(target.reg.saturating_add(1));
                }
                return Ok(());
            }
        }
        if self.try_compile_inplace_str_append(place, value)? {
            return Ok(());
        }
        if let HirExprKind::Path { segments, .. } = &place.kind {
            if let Some(first) = segments.first() {
                if let Some(target) = self.lookup_local(&first.name) {
                    // Typed-local reassignment: compile the
                    // RHS in the destination kind so no box /
                    // unbox round-trip happens in hot loops.
                    let rhs_start = self.cur_idx();
                    let src_tr = self.compile_expr_ex(value)?;
                    if self.expr_has_uint_display_provenance(value) {
                        self.uint_display_locals.insert(target.reg);
                    } else {
                        self.uint_display_locals.remove(&target.reg);
                    }
                    if target.kind == RegKind::I64
                        && src_tr.kind == RegKind::I64
                        && self.try_fold_i64_move(rhs_start, src_tr.reg, target.reg)
                    {
                        return Ok(());
                    }
                    if let Some(cell) = self.capture_cell_for_local(target) {
                        let src_v = self.as_value(src_tr);
                        self.rebind_capture_cell(target.reg, cell, src_v);
                        return Ok(());
                    }
                    self.emit_move_into(target, src_tr);
                    return Ok(());
                }
            }
        }
        // Native `local.field = value` and `local[i] = value`
        // writes: emit FieldSet / IndexSet directly so the VM's
        // hot loops (nbody's body.vx / body.x updates) stay on the
        // fast path.
        if let HirExprKind::Field { receiver, name } = &place.kind {
            if let HirExprKind::Path { segments, .. } = &receiver.kind {
                if let Some(first) = segments.first() {
                    if let Some(target) = self.lookup_local(&first.name) {
                        if target.kind == RegKind::Value {
                            let value_is_i64 =
                                matches!(self.tcx.kind(value.ty), Some(TyKind::Int(IntTy::I64)));
                            let receiver_ty = self.unwrap_ref(receiver.ty);
                            let offset =
                                self.resolve_struct_field_offset(receiver_ty, name.name.as_str());
                            if value_is_i64 && let Some(offset) = offset {
                                let value_tr = self.compile_expr_ex(value)?;
                                let value_i = self.as_i64(value_tr);
                                self.emit(Op::FieldSetI64ByOffset {
                                    receiver: target.reg,
                                    offset,
                                    value_i,
                                });
                                return Ok(());
                            }
                            let value_reg = self.compile_expr(value)?;
                            let name_idx = self.const_idx(
                                ConstKey::String(name.name.clone()),
                                Value::String(SmolStr::from(name.name.clone())),
                            );
                            self.emit(Op::FieldSet {
                                receiver: target.reg,
                                name_idx,
                                value: value_reg,
                            });
                            return Ok(());
                        }
                        // Typed local can't be a struct;
                        // fall through to the deferred path.
                    }
                }
            }
        }
        if let HirExprKind::Index { base, index } = &place.kind {
            if let HirExprKind::Path { segments, .. } = &base.kind {
                if let Some(first) = segments.first() {
                    if let Some(target) = self.lookup_local(&first.name) {
                        if target.kind == RegKind::Value {
                            // Typed flat-f64 store fast path: when
                            // the receiver is a `Value::FloatVec`
                            // and the RHS is f64-typed, write
                            // straight from the f64 register file.
                            // Mirrors the IntArray IndexSet bypass
                            // in `IntArray` users.
                            let value_is_f64 = matches!(
                                self.tcx.kind(value.ty),
                                Some(TyKind::Float(FloatTy::F64))
                            );
                            if value_is_f64 && self.flat_float_locals.contains(&target.reg) {
                                let idx_tr = self.compile_expr_ex(index)?;
                                let idx_i = self.as_i64(idx_tr);
                                let value_tr = self.compile_expr_ex(value)?;
                                let value_f = self.as_f64(value_tr);
                                self.emit(Op::FloatVecSetF64 {
                                    base: target.reg,
                                    index_i: idx_i,
                                    value_f,
                                });
                                return Ok(());
                            }
                            // Mirror typed-i64 store fast path for
                            // `Value::IntArray`-backed locals:
                            // `arr[i] = v` where `arr: [i64; N]`
                            // skips the box/unbox the generic
                            // `Op::IndexSet` would impose. fannkuch's
                            // `perm[j] = perm1[j]` and count
                            // manipulation rely on this path. The
                            // gate is purely the receiver tracking
                            // (any `flat_int_locals` member is
                            // guaranteed `Value::IntArray` storage),
                            // because the value's HIR type can be a
                            // still-bound `TyKind::Var` for arithmetic
                            // expressions that typeck couldn't
                            // substitute in-place.
                            if self.flat_int_locals.contains(&target.reg) {
                                let idx_tr = self.compile_expr_ex(index)?;
                                let idx_i = self.as_i64(idx_tr);
                                let value_tr = self.compile_expr_ex(value)?;
                                let value_i = self.as_i64(value_tr);
                                self.emit(Op::IntArraySetI64 {
                                    base: target.reg,
                                    index_i: idx_i,
                                    value_i,
                                });
                                return Ok(());
                            }
                            let idx_reg = self.compile_expr(index)?;
                            let value_reg = self.compile_expr(value)?;
                            self.emit(Op::IndexSet {
                                base: target.reg,
                                index: idx_reg,
                                value: value_reg,
                            });
                            return Ok(());
                        }
                    }
                }
            }
        }
        // `local[idx].field = value` - fused in-place write.
        // The `IndexedFieldSet` op mutates the array and the
        // body's field vec via `Arc::make_mut`, which is O(1)
        // here because `target` is the sole holder of the
        // array's Arc. This is the nbody inner-loop hot path
        // (`bodies[i].vx = ...`).
        if let HirExprKind::Field { receiver, name } = &place.kind {
            if let HirExprKind::Index { base, index } = &receiver.kind {
                if let HirExprKind::Path { segments, .. } = &base.kind {
                    if let Some(first) = segments.first() {
                        if let Some(target) = self.lookup_local(&first.name) {
                            if target.kind == RegKind::Value {
                                let name_idx = self.const_idx(
                                    ConstKey::String(name.name.clone()),
                                    Value::String(SmolStr::from(name.name.clone())),
                                );
                                // Phase-2 typed store: when the RHS is
                                // an f64 expression, write straight
                                // from the float register file into
                                // `base[i].field`, skipping the
                                // `BoxF64` that the generic path
                                // would emit.
                                let value_is_f64 = matches!(
                                    self.tcx.kind(value.ty),
                                    Some(TyKind::Float(FloatTy::F64))
                                );
                                if value_is_f64 {
                                    // Resolve the struct's field
                                    // offset for this write; emit the
                                    // offset-based op when possible.
                                    let elem_ty = self.array_elem_ty(base.ty);
                                    let offset = elem_ty.and_then(|t| {
                                        self.resolve_struct_field_offset(t, name.name.as_str())
                                    });
                                    // Compile the index in its native
                                    // register file: an int-bank index
                                    // (loop counter) feeds the flat write
                                    // directly, skipping the per-access
                                    // `BoxI64`.
                                    let idx_tr = self.compile_expr_ex(index)?;
                                    let value_tr = self.compile_expr_ex(value)?;
                                    let value_f = self.as_f64(value_tr);
                                    // Known-flat fast path.
                                    if let (Some(offset), Some(&stride)) =
                                        (offset, self.flat_locals.get(&target.reg))
                                    {
                                        if idx_tr.kind == RegKind::I64 {
                                            self.emit(Op::FlatSetF64I {
                                                base: target.reg,
                                                index_i: idx_tr.reg,
                                                stride,
                                                offset,
                                                value_f,
                                            });
                                        } else {
                                            let idx_reg = self.as_value(idx_tr);
                                            self.emit(Op::FlatSetF64 {
                                                base: target.reg,
                                                index: idx_reg,
                                                stride,
                                                offset,
                                                value_f,
                                            });
                                        }
                                        return Ok(());
                                    }
                                    let idx_reg = self.as_value(idx_tr);
                                    if let Some(offset) = offset {
                                        self.emit(Op::IndexedFieldSetF64ByOffset {
                                            base: target.reg,
                                            index: idx_reg,
                                            offset,
                                            value_f,
                                        });
                                    } else {
                                        self.emit(Op::IndexedFieldSetF64 {
                                            base: target.reg,
                                            index: idx_reg,
                                            name_idx,
                                            value_f,
                                        });
                                    }
                                    return Ok(());
                                }
                                let idx_reg = self.compile_expr(index)?;
                                let value_reg = self.compile_expr(value)?;
                                self.emit(Op::IndexedFieldSet {
                                    base: target.reg,
                                    index: idx_reg,
                                    name_idx,
                                    value: value_reg,
                                });
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        // Anything more complex (`a.b.c = x`, `grid[i][j] = x`, `*p = v`,
        // assignment through a temporary) lowers natively via the
        // recursive place store. A local-rooted place bottoms out in a
        // register move; a `static mut`-rooted place (`COUNTER = …`,
        // `STATIC.field = …`) bottoms out in an `Op::StoreStatic` against
        // the shared `Global::MutStatic` cell. Those are the only
        // assignable place roots - a `const` or immutable `static` is
        // rejected by the typechecker before it reaches here.
        if self.place_root_is_local(place) || self.place_root_is_mut_static(place) {
            let value_reg = self.compile_expr(value)?;
            return self.compile_place_store(place, value_reg);
        }
        Err(RuntimeError::Unsupported(
            "assignment to a place that is neither a local nor a mutable static",
        ))
    }

    fn returned_mut_ref_home(&self, init: &HirExpr) -> Option<TypedReg> {
        if !self.is_mut_ref_ty(init.ty) {
            return None;
        }
        let HirExprKind::Call { args, .. } = &init.kind else {
            return None;
        };
        let [arg] = args.as_slice() else {
            return None;
        };
        let name = Self::mut_ref_arg_local_name(arg)?;
        self.lookup_local(name)
    }

    fn mut_ref_arg_local_name(arg: &HirExpr) -> Option<&str> {
        let place = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => operand.as_ref(),
            _ => arg,
        };
        if let HirExprKind::Path { segments, .. } = &place.kind
            && let [segment] = segments.as_slice()
        {
            return Some(segment.name.as_str());
        }
        None
    }

    fn is_mut_ref_ty(&self, ty: gossamer_types::Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            Some(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Mut,
                ..
            })
        )
    }

    /// Lowers `place += rhs` for a `String` place to the in-place
    /// [`Op::StrAppend`] when `place` resolves to a local Value
    /// register (`s += x`, or `*out += x` for a `&mut String` local).
    /// Returns `true` when handled. A compound `+=` (and an explicit
    /// `s = s + rhs`) lowers to `place = place + rhs`; appending in
    /// place avoids the whole-string copy the concat-then-store path
    /// makes on every call, so a build loop grows in amortized O(1).
    fn try_compile_inplace_str_append(
        &mut self,
        place: &HirExpr,
        value: &HirExpr,
    ) -> RuntimeResult<bool> {
        if !matches!(self.tcx.kind(place.ty), Some(TyKind::String)) {
            return Ok(false);
        }
        let HirExprKind::Binary {
            op: HirBinaryOp::Add,
            lhs,
            rhs,
        } = &value.kind
        else {
            return Ok(false);
        };
        if !Self::str_place_eq(place, lhs) {
            return Ok(false);
        }
        let Some(reg) = self.str_place_local_reg(place) else {
            return Ok(false);
        };
        let rhs_reg = self.compile_expr(rhs)?;
        self.emit(Op::StrAppend {
            receiver: reg,
            value: rhs_reg,
        });
        Ok(true)
    }

    /// Structural equality for the place shapes the in-place String
    /// append supports - a bare local path or a deref chain bottoming
    /// out at one. Confirms the binary's LHS is the assignment's place
    /// (the lowered LHS of `+=` is a clone of that place).
    fn str_place_eq(a: &HirExpr, b: &HirExpr) -> bool {
        match (&a.kind, &b.kind) {
            (HirExprKind::Path { segments: sa, .. }, HirExprKind::Path { segments: sb, .. }) => {
                sa.len() == sb.len() && sa.iter().zip(sb).all(|(x, y)| x.name == y.name)
            }
            (
                HirExprKind::Unary {
                    op: HirUnaryOp::Deref,
                    operand: oa,
                },
                HirExprKind::Unary {
                    op: HirUnaryOp::Deref,
                    operand: ob,
                },
            ) => Self::str_place_eq(oa, ob),
            _ => false,
        }
    }

    /// Resolves a String place to the local Value register that owns
    /// it, peeling deref projections. `None` for any non-local or
    /// non-`Value`-kind root, which keeps the generic store path.
    fn str_place_local_reg(&self, place: &HirExpr) -> Option<Reg> {
        match &place.kind {
            HirExprKind::Path { segments, .. } => {
                let first = segments.first()?;
                let target = self.lookup_local(&first.name)?;
                (target.kind == RegKind::Value).then_some(target.reg)
            }
            HirExprKind::Unary {
                op: HirUnaryOp::Deref,
                operand,
            } => self.str_place_local_reg(operand),
            _ => None,
        }
    }

    /// `true` when the lvalue `place` is rooted at a bound local - a
    /// chain of field / index / deref projections bottoming out at a
    /// local binding. A static-rooted place returns `false`.
    pub(crate) fn place_root_is_local(&self, place: &HirExpr) -> bool {
        match &place.kind {
            HirExprKind::Path { segments, .. } => segments
                .first()
                .is_some_and(|s| self.lookup_local(&s.name).is_some()),
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.place_root_is_local(receiver)
            }
            HirExprKind::Index { base, .. } => self.place_root_is_local(base),
            HirExprKind::Unary {
                op: HirUnaryOp::Deref,
                operand,
            } => self.place_root_is_local(operand),
            _ => false,
        }
    }

    /// `true` when the lvalue `place` is rooted at a `static mut` - the
    /// static side of [`Self::place_root_is_local`]. A field / index /
    /// deref chain bottoming out at a known mutable static matches.
    fn place_root_is_mut_static(&self, place: &HirExpr) -> bool {
        match &place.kind {
            HirExprKind::Path { segments, .. } => self.mut_static_global_name(segments).is_some(),
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.place_root_is_mut_static(receiver)
            }
            HirExprKind::Index { base, .. } => self.place_root_is_mut_static(base),
            HirExprKind::Unary {
                op: HirUnaryOp::Deref,
                operand,
            } => self.place_root_is_mut_static(operand),
            _ => false,
        }
    }

    /// Resolves `segments` to the global name of a `static mut`, or
    /// `None` when the path isn't one. A single-segment same-module
    /// reference (`COUNTER`) and a qualified reference (`mod::COUNTER`)
    /// both match when the final segment names a known mutable static and
    /// the first segment isn't a local shadowing it. The returned name is
    /// the `::`-joined path, under which the cell is registered in the
    /// global table.
    fn mut_static_global_name(&self, segments: &[Ident]) -> Option<String> {
        // `super::COUNTER` / `crate::COUNTER` inside an inline module
        // addresses the flat cell registered under the unqualified or
        // module-joined name, so strip the relative prefix first.
        let segments = strip_module_relative(segments);
        let first = segments.first()?;
        if self.lookup_local(&first.name).is_some() {
            return None;
        }
        let last = segments.last()?;
        if !self.mut_statics.contains(last.name.as_str()) {
            return None;
        }
        Some(
            segments
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("::"),
        )
    }

    /// Stores `value_reg` into the lvalue `place`, recursing up a
    /// field / index / deref chain. Each step reads the receiver, mutates
    /// the leaf in place (`FieldSet` / `IndexSet`, which clone-on-write
    /// through `Arc::make_mut`), then writes the updated receiver back to
    /// its own place so value-aggregate aliasing semantics match the
    /// compiled tiers.
    pub(crate) fn compile_place_store(
        &mut self,
        place: &HirExpr,
        value_reg: Reg,
    ) -> RuntimeResult<()> {
        match &place.kind {
            HirExprKind::Path { segments, .. } => {
                let Some(first) = segments.first() else {
                    return Err(RuntimeError::Unsupported("assignment to empty path"));
                };
                if let Some(target) = self.lookup_local(&first.name) {
                    self.emit_move_into(
                        target,
                        TypedReg {
                            reg: value_reg,
                            kind: RegKind::Value,
                        },
                    );
                    return Ok(());
                }
                // `static mut` root: publish into the shared global cell.
                let Some(name) = self.mut_static_global_name(segments) else {
                    return Err(RuntimeError::UnresolvedName(first.name.clone()));
                };
                let idx = self.global_idx(&name);
                self.emit(Op::StoreStatic {
                    name_idx: idx,
                    src: value_reg,
                });
                Ok(())
            }
            HirExprKind::Field { receiver, name } => {
                let recv_reg = self.compile_expr(receiver)?;
                let name_idx = self.const_idx(
                    ConstKey::String(name.name.clone()),
                    Value::String(SmolStr::from(name.name.clone())),
                );
                self.emit(Op::FieldSet {
                    receiver: recv_reg,
                    name_idx,
                    value: value_reg,
                });
                self.compile_place_store(receiver, recv_reg)
            }
            HirExprKind::TupleIndex { receiver, index } => {
                let recv_reg = self.compile_expr(receiver)?;
                self.emit(Op::TupleSet {
                    receiver: recv_reg,
                    index: *index,
                    value: value_reg,
                });
                self.compile_place_store(receiver, recv_reg)
            }
            HirExprKind::Index { base, index } => {
                let base_reg = self.compile_expr(base)?;
                let idx_reg = self.compile_expr(index)?;
                self.emit(Op::IndexSet {
                    base: base_reg,
                    index: idx_reg,
                    value: value_reg,
                });
                self.compile_place_store(base, base_reg)
            }
            HirExprKind::Unary {
                op: HirUnaryOp::Deref,
                operand,
            } => self.compile_place_store(operand, value_reg),
            _ => Err(RuntimeError::Unsupported(
                "assignment to non-place expression",
            )),
        }
    }
}
