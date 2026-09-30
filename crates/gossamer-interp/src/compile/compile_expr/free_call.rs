//! Compiling calls to functions, constructors, and described builtins, with `&mut` argument writeback.

use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn compile_call_ex(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        result_ty: Ty,
    ) -> RuntimeResult<Reg> {
        if let Some(reg) = self.try_compile_described_call(callee, args)? {
            return Ok(reg);
        }
        // Qualified Vec mutators use the same in-place contract as method
        // calls. Compile the referenced place directly so the legacy
        // Result-returning builtins cannot leak through this Rust-style API.
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() >= 2
            && let Some(method) = segments.last()
            && let Some(owner) = segments.get(segments.len() - 2)
            && owner.name == "Vec"
            && matches!(
                (method.name.as_str(), args.len()),
                ("insert", 3) | ("remove", 2)
            )
        {
            let place = peel_ref_wrappers_expr(&args[0]);
            let receiver = self.compile_expr(place)?;
            let index = self.compile_expr(&args[1])?;
            if method.name == "insert" {
                let value = self.compile_expr(&args[2])?;
                let dst = self.alloc_reg();
                self.emit(Op::VecInsert {
                    dst,
                    receiver,
                    index,
                    value,
                });
                self.compile_place_store(place, receiver)?;
                return Ok(dst);
            }
            let dst = self.alloc_reg();
            self.emit(Op::VecRemoveAt {
                dst,
                receiver,
                index,
            });
            self.compile_place_store(place, receiver)?;
            return Ok(dst);
        }
        // Rust UFCS syntax is semantically the same call as method syntax.
        // Route supported built-in mutators through the method compiler so
        // its receiver writeback and public return contract are identical.
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() >= 2
            && let Some(method) = segments.last()
            && let Some(owner) = segments.get(segments.len() - 2)
            && matches!(owner.name.as_str(), "String" | "Vec")
            && matches!(
                method.name.as_str(),
                "push"
                    | "push_str"
                    | "push_char"
                    | "push_byte"
                    | "clear"
                    | "truncate"
                    | "sort"
                    | "sort_by"
                    | "sort_by_key"
                    | "reverse"
                    | "swap"
                    | "fill"
                    | "insert"
                    | "remove"
            )
            && let Some((receiver, method_args)) = args.split_first()
        {
            return self.compile_method_call(
                peel_ref_wrappers_expr(receiver),
                method,
                method_args,
                None,
            );
        }
        // Qualified `Map` / `Set` mutators carry the same implicit
        // mutable-receiver contract as their method-call form (enforced by
        // the type checker's `check_mutating_qualified_call`), so route
        // them through the method compiler too. Left to the generic
        // by-value argument path below, the receiver would go through
        // `Op::CloneMapLike` - the independent-copy semantics an ordinary
        // function parameter needs - and every mutation would land on a
        // throwaway clone instead of the caller's binding.
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() >= 2
            && let Some(method) = segments.last()
            && let Some(owner) = segments.get(segments.len() - 2)
            && matches!(owner.name.as_str(), "Map" | "Set" | "BTreeSet")
            && matches!(
                method.name.as_str(),
                "insert"
                    | "remove"
                    | "clear"
                    | "inc"
                    | "inc_at"
                    | "inc_batch"
                    | "or_insert"
                    | "pop"
            )
            && let Some((receiver, method_args)) = args.split_first()
        {
            return self.compile_method_call(
                peel_ref_wrappers_expr(receiver),
                method,
                method_args,
                None,
            );
        }
        // A payload-less enum constructor is already represented by its
        // immutable global sentinel. Its HIR callee has the enum value type,
        // unlike a zero-argument function returning that enum, whose callee
        // has a function type. Return the loaded sentinel directly and avoid
        // generic call dispatch on every leaf construction.
        if args.is_empty()
            && let Some(TyKind::Adt { def, .. }) = self.tcx.kind(callee.ty)
            && let HirExprKind::Path { segments, .. } = &callee.kind
            && let Some(variant_name) = segments.last()
            && self
                .tcx
                .enum_variant_names(*def)
                .is_some_and(|variants| variants.iter().any(|name| name == &variant_name.name))
        {
            return self.compile_expr(callee);
        }
        if let Some(reg) = self.try_compile_variant_constructor(callee, args)? {
            return Ok(reg);
        }
        if let Some(reg) = self.try_compile_struct2_i64(callee, args, result_ty)? {
            return Ok(reg);
        }
        // Typed-IntMap construction fast path: when the callee is
        // `HashMap::new` and the result type is `HashMap<i64, i64>`,
        // emit a dedicated `Op::BuildIntMap` so the receiver lands
        // as `Value::IntMap` and downstream typed ops fire.
        if args.len() <= 1 {
            if let HirExprKind::Path { segments, .. } = &callee.kind {
                let segs: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
                // `BTreeMap` resolves to `TyKind::HashMap` and shares the map
                // runtime, so an i64-keyed `BTreeMap::new` lands as a typed
                // `IntMap` / `StrIntMap` exactly like `HashMap::new`. IntMap
                // iteration is key-sorted, matching BTreeMap's ordering.
                let is_map_new = args.is_empty()
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "new"] | ["collections", "Map" | "BTreeMap", "new"]
                    );
                let is_empty_map_from = args.len() == 1
                    && matches!(self.tcx.kind(args[0].ty), Some(TyKind::Unit))
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "from"] | ["collections", "Map" | "BTreeMap", "from"]
                    );
                let is_map_from = args.len() == 1
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "from"] | ["collections", "Map" | "BTreeMap", "from"]
                    );
                if (is_map_new || is_map_from)
                    && let Some(unsigned) = self.btree_map_unsigned(result_ty)
                {
                    let flag = self.load_int_value(i64::from(unsigned));
                    let mut operands = vec![flag];
                    if is_map_from && !is_empty_map_from {
                        operands.push(self.compile_expr(&args[0])?);
                    }
                    return self.emit_global_call("__btree_map_new", &operands);
                }
                if (is_map_new || is_empty_map_from) && self.is_int_map_ty(result_ty) {
                    let dst = self.alloc_reg();
                    self.emit(Op::BuildIntMap { dst_v: dst });
                    return Ok(dst);
                }
                if (is_map_new || is_empty_map_from) && self.is_str_int_map_ty(result_ty) {
                    let dst = self.alloc_reg();
                    self.emit(Op::BuildStrIntMap { dst_v: dst });
                    return Ok(dst);
                }
                let is_map_with_capacity = args.len() == 1
                    && matches!(
                        segs.as_slice(),
                        ["Map", "with_capacity"] | ["collections", "Map", "with_capacity"]
                    );
                if is_map_with_capacity
                    && (self.is_int_map_ty(result_ty) || self.is_str_int_map_ty(result_ty))
                {
                    let capacity = self.compile_expr_ex(&args[0])?;
                    let capacity_i = self.as_i64(capacity);
                    let dst = self.alloc_reg();
                    if self.is_int_map_ty(result_ty) {
                        self.emit(Op::BuildIntMapWithCapacity {
                            dst_v: dst,
                            capacity_i,
                        });
                    } else {
                        self.emit(Op::BuildStrIntMapWithCapacity {
                            dst_v: dst,
                            capacity_i,
                        });
                    }
                    return Ok(dst);
                }
                // `{1: 0, 2: 5}` (an int-keyed, int-valued map literal)
                // desugars to `Map::from([(1, 0), (2, 5)])`, an
                // explicit-entry-list array argument rather than the
                // empty/unit form above. Without this arm the call fell
                // through to the generic `Map::from` builtin, which builds
                // a boxed `Value::Map` - but `is_int_map_ty(result_ty)`
                // still reports this binding as the typed shape, so a later
                // `.insert()` / `.len()` / `.contains_key()` / `.get_or()`
                // emits the dedicated `IntMap*` op, which requires
                // `Value::IntMap` and errors "receiver lost typed
                // invariant" against the mismatched boxed map. Build the
                // typed map directly and unroll the (compile-time-known)
                // entry count into individual typed inserts instead, the
                // same representation `Map::new()` + `.insert()` produces.
                if args.len() == 1
                    && matches!(
                        segs.as_slice(),
                        ["Map" | "BTreeMap", "from"] | ["collections", "Map" | "BTreeMap", "from"]
                    )
                    && let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(entries)) =
                        &args[0].kind
                    && entries.iter().all(
                        |entry| matches!(&entry.kind, HirExprKind::Tuple(pair) if pair.len() == 2),
                    )
                {
                    if self.is_int_map_ty(result_ty) {
                        let dst = self.alloc_reg();
                        self.emit(Op::BuildIntMap { dst_v: dst });
                        for entry in entries {
                            let HirExprKind::Tuple(pair) = &entry.kind else {
                                unreachable!("filtered to 2-element tuples above")
                            };
                            let key_tr = self.compile_expr_ex(&pair[0])?;
                            let key_i = self.as_i64(key_tr);
                            let val_tr = self.compile_expr_ex(&pair[1])?;
                            let value_i = self.as_i64(val_tr);
                            let insert_dst = self.alloc_reg();
                            self.emit(Op::IntMapInsert {
                                dst_v: insert_dst,
                                map_reg: dst,
                                key_i,
                                value_i,
                            });
                        }
                        return Ok(dst);
                    }
                }
            }
        }
        if Self::callee_is_concat(callee)
            && args.len() == 2
            && matches!(self.tcx.kind(args[0].ty), Some(TyKind::String))
            && let HirExprKind::Call {
                callee: pad_callee,
                args: pad_args,
            } = &args[1].kind
            && let HirExprKind::Path {
                segments: pad_segments,
                ..
            } = &pad_callee.kind
            && pad_segments
                .last()
                .is_some_and(|segment| segment.name == "__fmt_pad")
            && pad_args.len() == 4
            && let HirExprKind::Call {
                callee: rendered_callee,
                args: rendered_args,
            } = &pad_args[0].kind
            && Self::callee_is_concat(rendered_callee)
            && rendered_args.len() == 1
            && matches!(self.tcx.kind(rendered_args[0].ty), Some(TyKind::Int(_)))
            && !self.expr_has_uint_display_provenance(&rendered_args[0])
        {
            let prefix = self.compile_expr(&args[0])?;
            let value = self.compile_expr(&rendered_args[0])?;
            let width = self.compile_expr(&pad_args[1])?;
            let fill = self.compile_expr(&pad_args[2])?;
            let align = self.compile_expr(&pad_args[3])?;
            let dst = self.alloc_reg();
            let idx = u16::try_from(self.wide_ops.len()).map_err(|_| {
                RuntimeError::Panic("too many wide bytecode operations".to_string())
            })?;
            self.wide_ops
                .push(crate::bytecode::WideOp::StrConcatPadI64 {
                    dst,
                    prefix,
                    value,
                    width,
                    fill,
                    align,
                });
            self.emit(Op::Wide { idx });
            return Ok(dst);
        }
        if Self::callee_is_concat(callee)
            && args.len() == 2
            && matches!(self.tcx.kind(args[0].ty), Some(TyKind::String))
            && matches!(self.tcx.kind(args[1].ty), Some(TyKind::Int(_)))
            && !self.expr_has_uint_display_provenance(&args[1])
        {
            let prefix = self.compile_expr(&args[0])?;
            let value = self.compile_expr_ex(&args[1])?;
            let value_i = self.as_i64(value);
            let dst = self.alloc_reg();
            self.emit(Op::StrConcatI64 {
                dst,
                prefix,
                value_i,
            });
            return Ok(dst);
        }
        let direct_global_idx = if let HirExprKind::Path { segments, def } = &callee.kind {
            let local = segments.len() == 1 && self.lookup_local(&segments[0].name).is_some();
            let module_const = def.is_some_and(|def| self.module_consts.contains_key(def));
            if local || module_const {
                None
            } else {
                let stripped = strip_module_relative(segments);
                let mut name = stripped
                    .iter()
                    .map(|segment| segment.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                // A wait given as a `time::Duration` counts nanoseconds.
                let duration_arg = |i: usize| {
                    args.get(i).is_some_and(|a| {
                        matches!(self.tcx.kind(self.static_ty(a)), Some(TyKind::Duration))
                    })
                };
                if def.is_none() && name == "time::sleep" && duration_arg(0) {
                    name = "time::__sleep_ns".to_string();
                } else if def.is_none() && name == "time::sleep_ctx" && duration_arg(1) {
                    name = "time::__sleep_ns_ctx".to_string();
                }
                Some(self.global_idx(&name))
            }
        } else {
            None
        };
        let callee_reg = if direct_global_idx.is_none() {
            Some(self.compile_expr(callee)?)
        } else {
            None
        };
        let argc = u16::try_from(args.len()).map_err(|_| RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: args.len(),
        })?;
        // Reserve `argc` contiguous Value-register slots for the call's
        // argument vector before compiling any arg expression. Without
        // this, an arg whose `compile_expr` allocates a fresh register
        // (e.g. a literal or call result) lands inside the not-yet-
        // populated args region, and the subsequent `Move dst=slot
        // src=arg_reg` clobbers earlier args before they reach the
        // callee.
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(argc)
            .expect("register overflow reserving call args");
        // `&mut Vec<T>` / `&mut [T]` / `&mut <scalar>` arguments ride the
        // write-back cell protocol: wrap the current value in a cell, pass
        // the cell (the callee unwraps it via `mut_ref_params`), and read
        // the callee's final value back after the call. A `&mut <local
        // Vec>` writes straight back into the local's register
        // (`cell_takes`); a non-local place (`&mut arr[i]`, `&mut
        // obj.field`, `&mut <scalar local>`) takes the cell's inner into a
        // temp and re-stores it through the place (`place_takes`).
        let mut cell_takes: Vec<(Reg, Reg)> = Vec::new();
        let mut place_takes: Vec<(&HirExpr, Reg)> = Vec::new();
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len());
        // Renderer calls need unsigned-64 arguments boxed as `Value::Uint` so
        // values above `i64::MAX` render as large positive decimals. This
        // mirrors the compiled tiers' printer choice from declared type and
        // MIR cast provenance.
        let render_call = Self::callee_renders_args(callee);
        let encodes_json = Self::callee_encodes_json(callee);
        let unsigned_leaves_call = render_call || encodes_json;
        // `__debug` is the `{:?}` channel and answers through `impl Debug`;
        // every other rendering callee is `{}` and answers through
        // `impl Display`.
        let render_method = if Self::callee_is_debug(callee) {
            "fmt"
        } else {
            "to_string"
        };
        let callee_param_tys = self.callee_param_tys(callee);
        for (i, arg) in args.iter().enumerate() {
            let expected_ty = callee_param_tys
                .as_ref()
                .and_then(|params| params.get(i))
                .copied();
            if let Some(home) = self.mut_ref_arg_home(arg, expected_ty) {
                // `&mut <local Vec>`: move the local into the cell when no
                // sibling argument reads it, giving the callee unique
                // ownership so its first mutation grows in place instead of
                // copy-on-writing the whole buffer. A read elsewhere keeps
                // the clone, matching the compiled tiers' by-pointer
                // snapshot semantics (see the `&mut self` precedent).
                let cell = self.alloc_reg();
                if Self::mut_ref_place_name(arg)
                    .is_some_and(|name| Self::mut_arg_move_safe(args, i, name))
                {
                    self.emit(Op::CellNewMove {
                        dst: cell,
                        src: home,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: cell,
                        src: home,
                    });
                }
                cell_takes.push((home, cell));
                arg_regs.push(cell);
            } else if let Some(place) = Self::mut_ref_writeback_place(self.tcx, arg, expected_ty) {
                let place_reg = self.compile_expr(place)?;
                let cell = self.alloc_reg();
                // A bare-local place (`&mut s` for a `String` / scalar /
                // struct local) is the local's own home register: it can be
                // moved into the cell (no-sibling-reads) and written back
                // with a direct `CellTake` into that register. A field /
                // index place keeps the clone (its value was copied out of
                // an aggregate that still holds a share) and re-stores
                // through the place expression.
                let local_home = Self::path_single_seg_name(place).and_then(|name| {
                    self.lookup_local(name)
                        .filter(|tr| tr.kind == RegKind::Value)
                        .map(|_| name)
                });
                if local_home.is_some_and(|name| Self::mut_arg_move_safe(args, i, name)) {
                    self.emit(Op::CellNewMove {
                        dst: cell,
                        src: place_reg,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: cell,
                        src: place_reg,
                    });
                }
                if local_home.is_some() {
                    // `place_reg` is the local's home register; publish the
                    // post-call value straight back into it. The temp +
                    // place-store form would leave a lingering clone in the
                    // temp register, forcing copy-on-write on every later
                    // mutation through a repeatedly-called `&mut <local>`.
                    cell_takes.push((place_reg, cell));
                } else {
                    place_takes.push((place, cell));
                }
                arg_regs.push(cell);
            } else if render_call
                && let Some(reg) = self.compile_user_rendering(arg, render_method)?
            {
                arg_regs.push(reg);
            } else if unsigned_leaves_call && self.expr_has_uint_display_provenance(arg) {
                let tr = self.compile_expr_ex(arg)?;
                let src_i = self.as_i64(tr);
                let dst_v = self.alloc_reg();
                self.emit(Op::I64ToUint { dst_v, src_i });
                arg_regs.push(dst_v);
            } else if unsigned_leaves_call
                && let Some(desc) = if encodes_json {
                    crate::value::json_descriptor(self.tcx, self.static_ty(arg))
                } else {
                    self.uint_leaves_desc(self.static_ty(arg))
                }
            {
                // An integer the type declared `u64` / `usize` reads as
                // unsigned wherever it sits, exactly as the compiled tiers'
                // element, payload, and slot tags render it.
                let src = self.compile_expr(arg)?;
                let dst = self.alloc_reg();
                let desc_idx = self.const_idx(
                    ConstKey::String(desc.clone()),
                    Value::String(desc.as_str().into()),
                );
                self.emit(Op::UintLeaves { dst, src, desc_idx });
                arg_regs.push(dst);
            } else {
                arg_regs.push(self.compile_expr(arg)?);
            }
        }
        for (i, arg_reg) in arg_regs.iter().enumerate() {
            let slot = args_start
                .checked_add(u16::try_from(i).unwrap())
                .expect("register overflow");
            // Move-on-last-use: when the argument is a consumable local
            // whose home register we are reading directly, hand the value
            // over instead of cloning so the callee receives unique
            // ownership and the caller's input frees as it is consumed.
            let consume = self
                .consumable_path(&args[i])
                .and_then(|name| self.lookup_local(name))
                .is_some_and(|tr| tr.kind == RegKind::Value && tr.reg == *arg_reg);
            if consume {
                self.emit(Op::MoveConsume {
                    dst: slot,
                    src: *arg_reg,
                });
            } else if is_path_expr(&args[i])
                && (self.expr_is_map(&args[i])
                    || self.expr_is_hashset(&args[i])
                    || self.expr_is_slot_container(&args[i])
                    // A struct or tuple carrying a container field shares
                    // that field's storage through a plain register copy, so
                    // the callee's value takes a clone the way a bare
                    // container argument does - which is what the compiled
                    // tiers give an aggregate argument through their
                    // struct-copy retain.
                    || self.expr_is_aggregate_with_container(&args[i]))
                // A callee that only reads the container, and lets nothing
                // derived from it outlive the call, sees the caller's storage:
                // copying it would be unobservable, and its cost is the
                // container's size on every call.
                && !self.callee_only_reads_param(callee, i)
                // A reference is an alias by construction: forwarding an
                // existing `&mut Map` / `&mut Set` parameter must reach the
                // callee as the same container, or the callee's `insert` /
                // `pop` lands on a copy and the caller sees nothing.
                && !matches!(self.tcx.kind(args[i].ty), Some(TyKind::Ref { .. }))
            {
                // A `Map` / `Set` local passed by value must reach the callee
                // as an independent value, not an `Arc<Mutex<_>>` alias the
                // callee could mutate out from under the caller - see
                // `Op::CloneMapLike`.
                self.emit(Op::CloneMapLike {
                    dst: slot,
                    src: *arg_reg,
                });
            } else {
                self.emit(Op::Move {
                    dst: slot,
                    src: *arg_reg,
                });
            }
        }
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        // A `flag::Cell` handle is a `Value::Struct("__Cell")`; a
        // primitive-scalar argument can never be one, so a call whose
        // every argument is scalar-typed needs no per-argument
        // auto-deref check.
        let may_have_cells = !args.iter().all(|a| {
            matches!(
                self.tcx.kind(a.ty),
                Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
            )
        });
        if let Some(global_idx) = direct_global_idx {
            self.emit(Op::CallGlobal {
                dst,
                global_idx,
                args: args_start,
                argc,
                cache_idx,
                may_have_cells,
            });
        } else {
            self.emit(Op::Call {
                dst,
                callee: callee_reg.expect("dynamic call has a callee register"),
                args: args_start,
                argc,
                cache_idx,
                may_have_cells,
            });
        }
        for (home, cell) in cell_takes {
            self.emit(Op::CellTake { dst: home, cell });
        }
        for (place, cell) in place_takes {
            let tmp = self.alloc_reg();
            self.emit(Op::CellTake { dst: tmp, cell });
            self.compile_place_store(place, tmp)?;
        }
        Ok(dst)
    }

    /// Lowers common payload enum constructors directly. The generic call
    /// path has to load a constructor sentinel, materialize an argument span,
    /// and rediscover that sentinel at runtime. The variant identity is
    /// already known from HIR, so one typed construction opcode is enough.
    fn try_compile_variant_constructor(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        if !matches!(args.len(), 1 | 2) {
            return Ok(None);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(callee.ty) else {
            return Ok(None);
        };
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        let Some(variant) = segments.last() else {
            return Ok(None);
        };
        if !self
            .tcx
            .enum_variant_names(*def)
            .is_some_and(|names| names.iter().any(|name| name == &variant.name))
        {
            return Ok(None);
        }

        let name_idx = self.const_idx(
            ConstKey::Variant(variant.name.clone()),
            Value::variant(variant.name.as_str(), Vec::new()),
        );
        let first = self.compile_expr(&args[0])?;
        let take_first = matches!(args[0].kind, HirExprKind::Call { .. })
            || self.consumable_path(&args[0]).is_some();
        let dst = self.alloc_reg();
        if args.len() == 1 {
            self.emit(Op::BuildVariant1 {
                dst,
                name_idx,
                field: first,
                take_field: take_first,
            });
        } else {
            let second = self.compile_expr(&args[1])?;
            let take_second = matches!(args[1].kind, HirExprKind::Call { .. })
                || self.consumable_path(&args[1]).is_some();
            self.emit(Op::BuildVariant2 {
                dst,
                name_idx,
                first,
                second,
                take_first,
                take_second,
            });
        }
        Ok(Some(dst))
    }

    /// `true` when `callee` spells the struct's own positional constructor
    /// (`Pair(a, b)`), rather than an associated function reached through
    /// the type (`Pair::new(a, b)`). Both carry the struct's `DefId`, and
    /// only the constructor spelling may be turned into a direct field
    /// packing - consuming the other shape would drop the callee's body.
    fn path_spells_struct_ctor(&self, callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, def, .. } = &callee.kind else {
            return false;
        };
        let (Some(def), Some(last)) = (def.as_ref(), segments.last()) else {
            return false;
        };
        self.tcx
            .def_name(*def)
            .is_some_and(|name| name.rsplit("::").next() == Some(last.name.as_str()))
    }

    /// Recognises the HIR lowering of `Pair(a, b)` / a two-field positional
    /// struct constructor and keeps both scalar operands in the integer
    /// register file. The generic `__struct` builtin remains the fallback for
    /// named fields, non-integer payloads, and every other arity.
    fn try_compile_struct2_i64(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        result_ty: Ty,
    ) -> RuntimeResult<Option<Reg>> {
        if let HirExprKind::Path { def: Some(def), .. } = &callee.kind
            && self.path_spells_struct_ctor(callee)
            && args.len() == 2
            && args
                .iter()
                .all(|arg| matches!(self.tcx.kind(arg.ty), Some(TyKind::Int(_))))
            && self.tcx.struct_field_tys(*def).is_some_and(|tys| {
                tys.len() == 2
                    && tys
                        .iter()
                        .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Int(_))))
            })
            && let Some(type_name) = self.tcx.def_name(*def)
            && matches!(self.tcx.kind(result_ty), Some(TyKind::Adt { def: result_def, .. }) if *result_def == *def)
        {
            let first = self.compile_expr_ex(&args[0])?;
            let first_i = self.as_i64(first);
            let second = self.compile_expr_ex(&args[1])?;
            let second_i = self.as_i64(second);
            let dst = self.alloc_reg();
            let type_name = self.shape_name_idx(type_name);
            let field0 = self.shape_name_idx("0");
            let field1 = self.shape_name_idx("1");
            self.emit(Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            });
            return Ok(Some(dst));
        }
        if let HirExprKind::Path { segments, .. } = &callee.kind
            && let [segment] = segments.as_slice()
            && args.len() == 2
            && args
                .iter()
                .all(|arg| matches!(self.tcx.kind(arg.ty), Some(TyKind::Int(_))))
            && let Some((def, field_names)) = self.layouts.iter().find(|(def, names)| {
                names.len() == 2 && self.tcx.def_name(**def) == Some(segment.name.as_str())
            })
            && self.tcx.struct_field_tys(*def).is_some_and(|tys| {
                tys.len() == 2
                    && tys
                        .iter()
                        .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Int(_))))
            })
            && matches!(self.tcx.kind(result_ty), Some(TyKind::Adt { def: result_def, .. }) if *result_def == *def)
        {
            let type_name = segment.name.clone();
            let field0 = field_names[0].clone();
            let field1 = field_names[1].clone();
            let first = self.compile_expr_ex(&args[0])?;
            let first_i = self.as_i64(first);
            let second = self.compile_expr_ex(&args[1])?;
            let second_i = self.as_i64(second);
            let dst = self.alloc_reg();
            let type_name = self.shape_name_idx(&type_name);
            let field0 = self.shape_name_idx(&field0);
            let field1 = self.shape_name_idx(&field1);
            self.emit(Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            });
            return Ok(Some(dst));
        }
        if let HirExprKind::Path { def: Some(def), .. } = &callee.kind
            && self.path_spells_struct_ctor(callee)
            && let Some(field_tys) = self.tcx.struct_field_tys(*def)
            && field_tys.len() == 2
            && field_tys
                .iter()
                .all(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Int(_))))
            && args.len() == 2
            && args
                .iter()
                .all(|arg| matches!(self.tcx.kind(arg.ty), Some(TyKind::Int(_))))
            && let Some(field_names) = self.layouts.get(def).filter(|names| names.len() == 2)
            && let Some(type_name) = self.tcx.def_name(*def)
            && matches!(self.tcx.kind(result_ty), Some(TyKind::Adt { def: result_def, .. }) if result_def == def)
        {
            let field0 = field_names[0].clone();
            let field1 = field_names[1].clone();
            let first = self.compile_expr_ex(&args[0])?;
            let first_i = self.as_i64(first);
            let second = self.compile_expr_ex(&args[1])?;
            let second_i = self.as_i64(second);
            let dst = self.alloc_reg();
            let type_name = self.shape_name_idx(type_name);
            let field0 = self.shape_name_idx(&field0);
            let field1 = self.shape_name_idx(&field1);
            self.emit(Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            });
            return Ok(Some(dst));
        }
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return Ok(None);
        };
        if !matches!(segments.as_slice(), [segment] if segment.name == "__struct")
            || args.len() != 5
        {
            return Ok(None);
        }
        let (
            HirExprKind::Literal(HirLiteral::String(type_name)),
            HirExprKind::Literal(HirLiteral::String(field0)),
            HirExprKind::Literal(HirLiteral::String(field1)),
        ) = (&args[0].kind, &args[1].kind, &args[3].kind)
        else {
            return Ok(None);
        };
        if !matches!(self.tcx.kind(args[2].ty), Some(TyKind::Int(_)))
            || !matches!(self.tcx.kind(args[4].ty), Some(TyKind::Int(_)))
        {
            return Ok(None);
        }
        let first = self.compile_expr_ex(&args[2])?;
        let first_i = self.as_i64(first);
        let second = self.compile_expr_ex(&args[4])?;
        let second_i = self.as_i64(second);
        let dst = self.alloc_reg();
        let type_name = self.shape_name_idx(type_name);
        let field0 = self.shape_name_idx(field0);
        let field1 = self.shape_name_idx(field1);
        self.emit(Op::Struct2I64 {
            dst,
            type_name,
            field0,
            field1,
            first_i,
            second_i,
        });
        Ok(Some(dst))
    }

    /// True when a call renders its arguments through `Display`.
    fn callee_renders_args(callee: &HirExpr) -> bool {
        // A path the resolver bound to a program item is that item, never
        // the builtin its name spells.
        let HirExprKind::Path {
            segments,
            def: None,
        } = &callee.kind
        else {
            return false;
        };
        segments.last().is_some_and(|s| {
            matches!(
                s.name.as_str(),
                "__concat" | "__debug" | "println" | "print" | "eprintln" | "format"
            )
        })
    }

    /// Whether `callee` encodes its argument as JSON text, which reads an
    /// integer declared `u64` / `usize` as unsigned the way rendering does.
    fn callee_encodes_json(callee: &HirExpr) -> bool {
        let HirExprKind::Path {
            segments,
            def: None,
        } = &callee.kind
        else {
            return false;
        };
        match segments.as_slice() {
            [.., module, name] => {
                module.name == "json"
                    && matches!(name.name.as_str(), "encode" | "render" | "encode_pretty")
            }
            _ => false,
        }
    }

    /// Whether `callee` is the `{:?}` rendering channel.
    fn callee_is_debug(callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        segments.last().is_some_and(|s| s.name == "__debug")
    }

    fn callee_is_concat(callee: &HirExpr) -> bool {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        segments.last().is_some_and(|s| s.name == "__concat")
    }

    /// The [`Op::UintLeaves`] descriptor a rendering method's receiver
    /// needs, or `None` when the method renders nothing the value alone
    /// cannot say. `join` renders the elements without the brackets, so
    /// its receiver takes the element-only descriptor.
    /// The ordering descriptor for the elements of a sequence or iterator of
    /// type `ty`, where those elements declare a `u64` / `usize`.
    fn ordering_elem_desc(&self, ty: Ty) -> Option<String> {
        match self.tcx.kind(self.unwrap_ref(ty)) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. }
                | TyKind::Iterator(elem),
            ) => crate::value::ordering_descriptor(self.tcx, *elem),
            _ => None,
        }
    }

    /// The ordering descriptor for a scalar operand list, where one of the
    /// operands is a `u64` / `usize`.
    fn ordering_scalar_desc(&self, operands: &[&HirExpr]) -> Option<String> {
        operands
            .iter()
            .find(|operand| self.is_unsigned64_ty(operand.ty))
            .and_then(|operand| crate::value::ordering_descriptor(self.tcx, operand.ty))
    }

    /// Loads `text` into a fresh register.
    fn load_string_value(&mut self, text: &str) -> Reg {
        let idx = self.const_idx(
            ConstKey::String(text.to_string()),
            Value::String(text.into()),
        );
        let dst = self.alloc_reg();
        self.emit(Op::LoadConst { dst, idx });
        dst
    }

    /// Calls the global builtin `name` with the already-compiled `operands`.
    fn emit_global_call(&mut self, name: &str, operands: &[Reg]) -> RuntimeResult<Reg> {
        let overflow = || RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: operands.len(),
        };
        let argc = u16::try_from(operands.len()).map_err(|_| overflow())?;
        // The operands are compiled already, so the argument window reserved
        // above them cannot overlap a register they occupy.
        let args_start = self.next_reg;
        self.next_reg = args_start.checked_add(argc).ok_or_else(overflow)?;
        for (slot, operand) in (args_start..self.next_reg).zip(operands) {
            self.ensure_reg_slot(slot);
            self.emit(Op::Move {
                dst: slot,
                src: *operand,
            });
        }
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        let global_idx = self.global_idx(name);
        self.emit(Op::CallGlobal {
            dst,
            global_idx,
            args: args_start,
            argc,
            cache_idx,
            may_have_cells: false,
        });
        Ok(dst)
    }

    /// Compiles the operands, appends the descriptor, and calls `builtin`.
    fn emit_described_call(
        &mut self,
        builtin: &str,
        operands: &[&HirExpr],
        desc: &str,
    ) -> RuntimeResult<Reg> {
        let mut regs = Vec::with_capacity(operands.len() + 1);
        for operand in operands {
            regs.push(self.compile_expr(operand)?);
        }
        regs.push(self.load_string_value(desc));
        self.emit_global_call(builtin, &regs)
    }

    /// An ordering method over a receiver whose type declares a `u64` /
    /// `usize`: those words order unsigned, which the value alone cannot
    /// say, so the call carries the type's ordering descriptor.
    pub(super) fn try_compile_described_method(
        &mut self,
        receiver: &HirExpr,
        name: &str,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        let scalar_receiver = self.is_unsigned64_ty(receiver.ty);
        let (builtin, desc) = match (name, args.len()) {
            ("sort", 0) => ("__ord_sort", self.ordering_elem_desc(receiver.ty)),
            ("binary_search", 1) => ("__ord_binary_search", self.ordering_elem_desc(receiver.ty)),
            ("min", 0) => ("__ord_min", self.ordering_elem_desc(receiver.ty)),
            ("max", 0) => ("__ord_max", self.ordering_elem_desc(receiver.ty)),
            ("min", 1) if scalar_receiver => ("__ord_min2", self.ordering_scalar_desc(&[receiver])),
            ("max", 1) if scalar_receiver => ("__ord_max2", self.ordering_scalar_desc(&[receiver])),
            ("clamp", 2) if scalar_receiver => {
                ("__ord_clamp", self.ordering_scalar_desc(&[receiver]))
            }
            _ => return Ok(None),
        };
        let Some(desc) = desc else {
            return Ok(None);
        };
        let mut operands = vec![receiver];
        operands.extend(args);
        let dst = self.emit_described_call(builtin, &operands, &desc)?;
        if name != "sort" {
            return Ok(Some(dst));
        }
        // The sorted sequence replaces the receiver's value in its place,
        // exactly as the in-place `sort` it stands for leaves it.
        if self.place_root_is_mut_static(receiver) || self.place_root_is_local(receiver) {
            self.compile_place_store(receiver, dst)?;
        }
        Ok(Some(self.load_unit()))
    }

    /// A free ordering call - `min`, `max`, `clamp`, the `iter::` reductions,
    /// the `sort::` searches - over operands whose type declares a `u64` /
    /// `usize`. See [`Self::try_compile_described_method`].
    fn try_compile_described_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        let HirExprKind::Path {
            segments,
            def: None,
        } = &callee.kind
        else {
            return Ok(None);
        };
        let path = segments
            .iter()
            .map(|segment| segment.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let path = path.strip_prefix("std::").unwrap_or(&path);
        let operands: Vec<&HirExpr> = args.iter().collect();
        let (builtin, desc) = match (path, args.len()) {
            ("min" | "iter::min", 1) => ("__ord_min", self.ordering_elem_desc(args[0].ty)),
            ("max" | "iter::max", 1) => ("__ord_max", self.ordering_elem_desc(args[0].ty)),
            ("min", 2) => ("__ord_min2", self.ordering_scalar_desc(&operands)),
            ("max", 2) => ("__ord_max2", self.ordering_scalar_desc(&operands)),
            ("clamp", 3) => ("__ord_clamp", self.ordering_scalar_desc(&operands)),
            ("sort::sort_stable", 1) => ("__ord_sort_stable", self.ordering_elem_desc(args[0].ty)),
            ("sort::binary_search", 2) => ("__ord_search", self.ordering_elem_desc(args[0].ty)),
            ("sort::partition_point", 2) => {
                ("__ord_partition_point", self.ordering_elem_desc(args[0].ty))
            }
            _ => return Ok(None),
        };
        let Some(desc) = desc else {
            return Ok(None);
        };
        self.emit_described_call(builtin, &operands, &desc)
            .map(Some)
    }

    /// `<` / `<=` / `>` / `>=` between two sequences, tuples, or carriers
    /// whose type declares a `u64` / `usize`. A scalar `u64` keeps the typed
    /// unsigned compare; a struct keeps the comparator its type derives.
    pub(super) fn try_compile_described_comparison(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> RuntimeResult<Option<Reg>> {
        let code = match op {
            HirBinaryOp::Lt => 0,
            HirBinaryOp::Le => 1,
            HirBinaryOp::Gt => 2,
            HirBinaryOp::Ge => 3,
            _ => return Ok(None),
        };
        let structural = match self.tcx.kind(self.unwrap_ref(lhs.ty)) {
            Some(TyKind::Tuple(_) | TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }) => {
                true
            }
            Some(TyKind::Adt { def, .. }) => def.local == u32::MAX || def.local == u32::MAX - 1,
            _ => false,
        };
        if !structural {
            return Ok(None);
        }
        let Some(desc) = crate::value::ordering_descriptor(self.tcx, lhs.ty) else {
            return Ok(None);
        };
        let lhs_reg = self.compile_expr(lhs)?;
        let rhs_reg = self.compile_expr(rhs)?;
        let desc_reg = self.load_string_value(&desc);
        let code_reg = self.load_int_value(code);
        self.emit_global_call("__ord_compare", &[lhs_reg, rhs_reg, desc_reg, code_reg])
            .map(Some)
    }

    pub(super) fn render_receiver_desc(&self, ty: Ty, method: &str, argc: usize) -> Option<String> {
        match (method, argc) {
            ("to_string" | "fmt", 0) => crate::value::render_descriptor(self.tcx, ty),
            ("join", 1) => crate::value::element_render_descriptor(self.tcx, ty),
            // A walk over a map or a set reads its keys in the order their
            // type gives them; one declaring a `u64` / `usize` key orders
            // unsigned, which only the described copy can say.
            ("keys" | "values" | "iter" | "to_vec", 0) if self.is_keyed_container(ty) => {
                crate::value::ordering_descriptor(self.tcx, ty)
            }
            _ => None,
        }
    }

    /// Whether `ty` is a map or a set, whose walk order follows its keys.
    fn is_keyed_container(&self, ty: Ty) -> bool {
        match self.tcx.kind(self.unwrap_ref(ty)) {
            Some(TyKind::HashMap { .. }) => true,
            Some(TyKind::Adt { def, .. }) => {
                matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
            }
            _ => false,
        }
    }

    /// The [`Op::UintLeaves`] descriptor for a rendered argument of type `ty`:
    /// where the type declared its integers `u64` / `usize`. `None` when it
    /// declared none, which is every value that renders as it always has.
    ///
    /// The shape mirrors what the compiled tiers' element, payload, and slot
    /// tags render unsigned, so all three tiers read one value the same way.
    pub(crate) fn uint_leaves_desc(&self, ty: Ty) -> Option<String> {
        crate::value::render_descriptor(self.tcx, ty)
    }

    /// The type `expr` has in this chunk: in a chunk compiled per
    /// instantiation, the instantiated type where the checked one names a
    /// type parameter, and the checked type everywhere else.
    pub(crate) fn static_ty(&self, expr: &HirExpr) -> Ty {
        self.dispatch
            .and_then(|table| table.instance_ty(self.dispatch_key, expr.id))
            .unwrap_or(expr.ty)
    }

    pub(crate) fn expr_has_uint_display_provenance(&self, expr: &HirExpr) -> bool {
        if self.is_unsigned64_ty(self.static_ty(expr)) {
            return true;
        }
        match &expr.kind {
            HirExprKind::Cast { ty, .. } => self.is_unsigned64_ty(*ty),
            HirExprKind::Path { segments, .. } if segments.len() == 1 => self
                .lookup_local(&segments[0].name)
                .is_some_and(|tr| self.uint_display_locals.contains(&tr.reg)),
            _ => false,
        }
    }

    /// Returns the home register of a `&mut Vec<T>` / `&mut [T]`
    /// call argument when the argument is a plain local place -
    /// either `&mut x` over a local or a bare path forwarding a
    /// `&mut` parameter. Non-local places (fields, indexes) and
    /// non-`&mut`-vec types return `None` and take the ordinary
    /// pass-by-value path.
    /// The single-segment local name of a bare-local place expression
    /// (`s`), or `None` for any other shape.
    pub(super) fn path_single_seg_name(place: &HirExpr) -> Option<&str> {
        if let HirExprKind::Path { segments, .. } = &place.kind {
            if let [seg] = segments.as_slice() {
                return Some(seg.name.as_str());
            }
        }
        None
    }

    /// The single-segment local name a `&mut <local>` argument refers
    /// to - the `RefMut` operand (`&mut s`) or a bare path forwarding a
    /// `&mut` parameter (`s`).
    pub(super) fn mut_ref_place_name(arg: &HirExpr) -> Option<&str> {
        let place = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => operand.as_ref(),
            _ => arg,
        };
        Self::path_single_seg_name(place)
    }

    /// `true` when moving (rather than cloning) the `&mut <local>`
    /// argument at `self_idx` into its write-back cell is safe: no other
    /// argument in the call reads the same local. A sibling read forces
    /// the clone so it observes the local's pre-call value, matching the
    /// compiled tiers, which pass `&mut` by pointer and evaluate the
    /// reading argument against the live binding.
    pub(super) fn mut_arg_move_safe(args: &[HirExpr], self_idx: usize, name: &str) -> bool {
        let bound: std::collections::HashSet<String> = std::collections::HashSet::new();
        let shadowed: std::collections::HashSet<String> =
            gossamer_hir::shadowed_global_names(|candidate| candidate == name);
        args.iter().enumerate().all(|(j, other)| {
            j == self_idx
                || !gossamer_hir::collect_free_vars(other, &bound, &shadowed)
                    .iter()
                    .any(|v| v == name)
        })
    }

    pub(super) fn mut_ref_arg_home(&self, arg: &HirExpr, expected_ty: Option<Ty>) -> Option<Reg> {
        // The callee's declared parameter decides, exactly as it does in
        // `mut_ref_writeback_place`: a `&Vec<T>` parameter reads the vector
        // and unwraps no cell, so a `&mut Vec<T>` argument reborrows as the
        // bare value.
        let expects_mut_vec = match expected_ty {
            Some(expected) => crate::compile::is_mut_ref_vec(self.tcx, expected),
            None => crate::compile::is_mut_ref_vec(self.tcx, arg.ty),
        };
        if !expects_mut_vec {
            return None;
        }
        let place = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => operand,
            HirExprKind::Path { .. } => arg,
            _ => return None,
        };
        let HirExprKind::Path { segments, .. } = &place.kind else {
            return None;
        };
        let [seg] = segments.as_slice() else {
            return None;
        };
        let tr = self.lookup_local(seg.name.as_str())?;
        (tr.kind == RegKind::Value).then_some(tr.reg)
    }

    /// Returns the lvalue place of a `&mut Vec<T>` / `&mut [T]` /
    /// `&mut <scalar>` call argument that is *not* a plain local Vec
    /// (`&mut s.field`, `&mut grid[i]`, `&mut <scalar local>`, or a bare
    /// path forwarding a `&mut` parameter). The caller wraps the place in
    /// a write-back cell and re-stores the callee's final value through it
    /// after the call. The plain-local-Vec case is handled separately by
    /// [`Self::mut_ref_arg_home`]; everything that isn't a write-through
    /// place (a temporary, a deref of a call result) returns `None`.
    pub(super) fn mut_ref_writeback_place<'a>(
        tcx: &TyCtxt,
        arg: &'a HirExpr,
        expected_ty: Option<Ty>,
    ) -> Option<&'a HirExpr> {
        let typed_as_mut_ref = crate::compile::is_mut_ref_writeback(tcx, arg.ty);
        let (place, explicit_mut_place) = match &arg.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => (operand.as_ref(), true),
            _ => (arg, false),
        };
        // The callee unwraps an incoming cell for exactly the parameters
        // its own declared type marks as write-back, so the declared type
        // decides here too; the argument's own shape answers only when the
        // callee is unknown at this call site.
        let participates = match expected_ty {
            Some(expected) => crate::compile::is_mut_ref_writeback(tcx, expected),
            None => {
                typed_as_mut_ref
                    || (explicit_mut_place && crate::compile::is_writeback_pointee(tcx, place.ty))
            }
        };
        if !participates {
            return None;
        }
        matches!(
            place.kind,
            HirExprKind::Path { .. }
                | HirExprKind::Field { .. }
                | HirExprKind::TupleIndex { .. }
                | HirExprKind::Index { .. }
        )
        .then_some(place)
    }
}
