//! Compiling method calls.

use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn compile_method_call(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
        owner: Option<&Ident>,
    ) -> RuntimeResult<Reg> {
        if name.name == "downgrade" && args.is_empty() {
            return self.compile_downgrade(receiver);
        }
        // Keep explicit wrapping arithmetic on the unboxed integer register
        // path. Routing these methods through generic dynamic dispatch makes
        // an intentional opt-out from debug overflow checks substantially
        // slower than the original arithmetic operation.
        if let Some(result) = self.try_compile_i64_wrapping_method(receiver, name, args)? {
            return Ok(self.as_value(result));
        }
        // A `&mut self` user method on a writeback place rides the cell
        // protocol so its mutation of `self` reaches the caller's
        // binding - the mechanism `for x in <custom iterator>` and every
        // stateful `obj.advance()` depend on. Tried first so a user
        // struct whose `&mut self` method shadows a builtin name (`pop`,
        // `swap`, `insert`) routes to the user method.
        if let Some(reg) = self.try_compile_mut_self_method(receiver, name, args)? {
            return Ok(reg);
        }
        if let Some(reg) = self.try_compile_window_method(receiver, name, args, owner)? {
            return Ok(reg);
        }
        // `xs.join(sep)` renders each element the way `{}` does, so an
        // element type that supplies its own rendering answers through that
        // method rather than the synthesized shape.
        if name.name == "join"
            && args.len() == 1
            && let Some(reg) = self.try_compile_rendered_join(receiver, &args[0])?
        {
            return Ok(reg);
        }
        // Vec::insert is fallible and returns a Result independently from the
        // updated receiver. Keep those two values separate so an `Ok` or
        // `Err` can never replace the Vec binding in expression position.
        if name.name == "insert" && args.len() == 2 {
            let mut kind = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = kind {
                kind = self.tcx.kind(inner).cloned();
            }
            if matches!(kind, Some(TyKind::Vec(_))) {
                let receiver_reg = self.compile_expr(receiver)?;
                let index = self.compile_expr(&args[0])?;
                let value = self.compile_expr(&args[1])?;
                let dst = self.alloc_reg();
                self.emit(Op::VecInsert {
                    dst,
                    receiver: receiver_reg,
                    index,
                    value,
                });
                self.compile_place_store(receiver, receiver_reg)?;
                return Ok(dst);
            }
        }
        // Vec::remove is fallible and returns the removed value independently
        // from the updated receiver.
        if name.name == "remove" && args.len() == 1 {
            let mut kind = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = kind {
                kind = self.tcx.kind(inner).cloned();
            }
            if matches!(kind, Some(TyKind::Vec(_))) {
                let receiver_reg = self.compile_expr(receiver)?;
                let index = self.compile_expr(&args[0])?;
                let dst = self.alloc_reg();
                self.emit(Op::VecRemoveAt {
                    dst,
                    receiver: receiver_reg,
                    index,
                });
                self.compile_place_store(receiver, receiver_reg)?;
                return Ok(dst);
            }
        }
        // `s.byte_at(i)` on a statically-`String` receiver: emit the
        // dedicated `Op::StrByteAt` rather than routing through the
        // generic `MethodCall` machinery. The static-type guard means a
        // non-string receiver (e.g. a user type with its own `byte_at`)
        // never reaches this op and keeps the name-global dispatch.
        if name.name.as_str() == "byte_at" && args.len() == 1 {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::String)) {
                let recv_reg = self.compile_expr(receiver)?;
                let idx_reg = self.compile_expr(&args[0])?;
                let dst = self.alloc_reg();
                self.emit(Op::StrByteAt {
                    dst,
                    recv: recv_reg,
                    idx: idx_reg,
                });
                return Ok(dst);
            }
        }
        // Character pushes on a local String mutate its SmolStr directly.
        // SmolStr uses copy-on-write for shared heap strings, preserving value
        // semantics while reusing unique storage whenever possible.
        if matches!(name.name.as_str(), "push" | "push_char" | "push_byte") && args.len() == 1 {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::String))
                && let HirExprKind::Path { segments, .. } = &receiver.kind
                && let [seg] = segments.as_slice()
                && let Some(target) = self.lookup_local(&seg.name)
                && target.kind == RegKind::Value
            {
                let value = self.compile_expr(&args[0])?;
                self.emit(Op::StrPush {
                    receiver: target.reg,
                    value,
                    byte: name.name.as_str() == "push_byte",
                });
                return Ok(self.load_unit());
            }
        }
        // `s.push_str(x)` on a local String can use the same in-place append
        // op as `s += x`. The generic mutating-method route clones the
        // receiver into the builtin argument list and then writes the returned
        // String back, which preserves semantics but turns builder-style loops
        // into repeated whole-string copies.
        if name.name.as_str() == "push_str" && args.len() == 1 {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            if matches!(k, Some(TyKind::String)) {
                if let HirExprKind::Path { segments, .. } = &receiver.kind {
                    if let [seg] = segments.as_slice() {
                        if let Some(target) = self.lookup_local(&seg.name) {
                            if target.kind == RegKind::Value {
                                let suffix = self.compile_expr(&args[0])?;
                                self.emit(Op::StrAppend {
                                    receiver: target.reg,
                                    value: suffix,
                                });
                                return Ok(self.load_unit());
                            }
                        }
                    }
                }
            }
        }
        // `d.as_millis()`, `inst.elapsed()`, `later.duration_since(earlier)`:
        // a `time::Duration` / `time::Instant` is a bare `Value::Int` at run
        // time with no qualified-key receiver, so the method resolves
        // statically from the receiver's type to the `time::Duration::<m>` /
        // `time::Instant::<m>` global, called with the receiver first.
        {
            let mut k = self.tcx.kind(receiver.ty).cloned();
            while let Some(TyKind::Ref { inner, .. }) = k {
                k = self.tcx.kind(inner).cloned();
            }
            // A `flag::Set` duration cell (`fs.duration(...)`) carries no
            // Duration tag on its HIR type (an unresolved inference var), so
            // it dispatches on the compile-time `duration_cell_locals` tag.
            // The cell auto-derefs at the call boundary to its Duration.
            let is_duration_cell = self.receiver_is_duration_cell(receiver);
            let owner = match (&k, name.name.as_str(), args.len()) {
                (_, "as_nanos" | "as_micros" | "as_millis" | "as_secs" | "as_secs_f64", 0)
                    if is_duration_cell || matches!(k, Some(TyKind::Duration)) =>
                {
                    Some("time::Duration")
                }
                (Some(TyKind::Instant), "elapsed_ms" | "elapsed", 0)
                | (Some(TyKind::Instant), "duration_since", 1) => Some("time::Instant"),
                _ => None,
            };
            if let Some(owner) = owner {
                let idx = self.global_idx(&format!("{owner}::{}", name.name));
                let callee_reg = self.alloc_reg();
                self.emit(Op::LoadGlobal {
                    dst: callee_reg,
                    idx,
                });
                let argc = u16::try_from(args.len() + 1).expect("time method arity");
                let args_start = self.next_reg;
                self.next_reg = self
                    .next_reg
                    .checked_add(argc)
                    .expect("register overflow reserving time method args");
                for (slot, expr) in std::iter::once(receiver).chain(args.iter()).enumerate() {
                    let reg = self.compile_expr(expr)?;
                    self.emit(Op::Move {
                        dst: args_start + u16::try_from(slot).expect("time method arity"),
                        src: reg,
                    });
                }
                let dst = self.alloc_reg();
                let cache_idx = self.alloc_cache_idx();
                self.emit(Op::Call {
                    dst,
                    callee: callee_reg,
                    args: args_start,
                    argc,
                    cache_idx,
                    may_have_cells: is_duration_cell,
                });
                return Ok(dst);
            }
        }
        // Super-instruction fast path for the canonical
        // `m.insert(k, m.get_or(k, 0) + by)` counter-bump.
        // Detected here (before compiling args) so the inner
        // `get_or` call is never lowered.
        if name.name == "insert" && args.len() == 2 {
            if let Some((key_expr, by_expr)) = match_map_inc_pattern(receiver, &args[0], &args[1]) {
                // `StrIntMap` has no typed counter-bump op; let it fall
                // through to the generic `get_or` + `insert` builtins,
                // which dispatch on its storage. The `Op::MapInc` /
                // `Op::IntMapInc` super-instructions only cover the boxed
                // `Map` and the `IntMap`.
                if matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. }))
                    && !self.is_str_int_map_ty(receiver.ty)
                {
                    // Typed `HashMap<i64, i64>` route: use
                    // `Op::IntMapInc` so the key + delta stay in
                    // the i64 register file the whole time.
                    if self.is_int_map_ty(receiver.ty) {
                        let map_reg = self.compile_expr(receiver)?;
                        let key_tr = self.compile_expr_ex(key_expr)?;
                        let key_i = self.as_i64(key_tr);
                        let by_tr = self.compile_expr_ex(by_expr)?;
                        let by_i = self.as_i64(by_tr);
                        let dst_i = self.alloc_int();
                        self.emit(Op::IntMapInc {
                            dst_i,
                            map_reg,
                            key_i,
                            by_i,
                        });
                        // Caller wants a `Value` register; box the
                        // post-increment value back so the existing
                        // statement-context code keeps working.
                        let dst = self.alloc_reg();
                        self.emit(Op::BoxI64 {
                            dst_v: dst,
                            src_i: dst_i,
                        });
                        return Ok(dst);
                    }
                    let map_reg = self.compile_expr(receiver)?;
                    let key_reg = self.compile_expr(key_expr)?;
                    let by_reg = self.compile_expr(by_expr)?;
                    let dst = self.alloc_reg();
                    self.emit(Op::MapInc {
                        dst,
                        map_reg,
                        key_reg,
                        by_reg,
                    });
                    return Ok(dst);
                }
            }
        }
        // `m.inc_at(seq, start, len, by)` super-instruction for a
        // string-keyed integer-valued `HashMap`. Inlines the
        // slice-hash + entry-increment so a sliding-window
        // counter update doesn't pay the generic builtin-call
        // dispatch on each iteration.
        if name.name == "inc_at"
            && args.len() == 4
            && matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. }))
            && !self.is_str_int_map_ty(receiver.ty)
        {
            let map_reg = self.compile_expr(receiver)?;
            let seq_reg = self.compile_expr(&args[0])?;
            let start_reg = self.compile_expr(&args[1])?;
            let len_reg = self.compile_expr(&args[2])?;
            let by_reg = self.compile_expr(&args[3])?;
            let dst = self.alloc_reg();
            let wide_idx = u16::try_from(self.wide_ops.len()).expect("wide_ops index overflow");
            self.wide_ops.push(crate::bytecode::WideOp::MapIncAt {
                dst,
                map_reg,
                seq_reg,
                start_reg,
                len_reg,
                by_reg,
            });
            self.emit(Op::Wide { idx: wide_idx });
            return Ok(dst);
        }
        if name.name == "swap" && args.len() == 2 {
            let receiver_reg = self.compile_expr(receiver)?;
            let a = self.compile_expr(&args[0])?;
            let b = self.compile_expr(&args[1])?;
            let dst = self.alloc_reg();
            self.emit(Op::VecSwap {
                dst,
                receiver: receiver_reg,
                a,
                b,
            });
            self.compile_place_store(receiver, receiver_reg)?;
            return Ok(dst);
        }
        // Typed-IntMap method dispatch fast paths. Skip the
        // generic builtin-IC route for the handful of HashMap
        // methods that hot counter loops drive.
        if self.is_int_map_ty(receiver.ty) {
            if let Some(reg) = self.try_compile_int_map_method(receiver, &name.name, args)? {
                return Ok(reg);
            }
        }
        if let Some(reg) = self.try_compile_described_method(receiver, &name.name, args)? {
            return Ok(reg);
        }
        let receiver_reg = self.compile_expr(receiver)?;
        // A rendering method answers the text `{}` answers, and a `Vec`
        // and a fixed array share one runtime representation; the
        // descriptor built from the static type is what tells them
        // apart, so it travels with the renderer's copy here as it does
        // with a format argument.
        // A user `impl` of the channel answers with the receiver itself, so
        // the descriptor - which the built-in renderer reads and a written
        // body cannot - is not put in its way.
        let user_answers_channel = self.has_user_rendering(self.static_ty(receiver), &name.name);
        let receiver_desc = if user_answers_channel {
            None
        } else {
            self.render_receiver_desc(self.static_ty(receiver), &name.name, args.len())
        };
        let receiver_reg = match receiver_desc {
            Some(desc) => {
                let dst = self.alloc_reg();
                let desc_idx = self.const_idx(
                    ConstKey::String(desc.clone()),
                    Value::String(desc.as_str().into()),
                );
                self.emit(Op::UintLeaves {
                    dst,
                    src: receiver_reg,
                    desc_idx,
                });
                dst
            }
            None => receiver_reg,
        };
        // `xs.pop()` evaluates to `Option<last>` while shortening the
        // receiver. `Op::VecPop` does both in one in-place step: it
        // returns `Some(last)` / `None` and shrinks the receiver
        // register's backing storage without copying it. A bare-local
        // receiver's register is the local's own slot, so the mutation
        // persists; a temporary receiver's mutation is discarded, which
        // matches the compiled tiers (pop on a temporary is a no-op).
        // Unresolved receiver types (`Var` / missing) still take this
        // path: the dominant producer is a stdlib call like
        // `os::read_file` whose Vec result type the checker leaves open;
        // user receivers with their own `pop` are `Adt`-typed and excluded.
        if name.name == "pop"
            && args.is_empty()
            && matches!(
                self.tcx.kind(receiver.ty),
                None | Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Var(_))
            )
        {
            let opt_dst = self.alloc_reg();
            self.emit(Op::VecPop {
                dst: opt_dst,
                receiver: receiver_reg,
            });
            // A field or element receiver (`self.idle.pop()`,
            // `groups[i].pop()`) has its own storage, so the shortened
            // vector is spliced back through the place-store protocol -
            // the same contract `remove` / `insert` / `swap` follow, and
            // what the compiled tiers do by mutating in place.
            if !matches!(receiver.kind, HirExprKind::Path { .. }) {
                self.compile_place_store(receiver, receiver_reg)?;
            }
            return Ok(opt_dst);
        }
        // Super-instruction fast path for `<stream>.write_byte(<b>)`.
        // The runtime handler in `vm.rs::Op::StreamWriteByte`
        // verifies the receiver is a Stream and the byte is an
        // integer; if not, it falls through to a normal MethodCall
        // dispatch. Skipping the args-buf + IC + builtin-extract
        // chain saves the dominant per-character overhead in
        // fasta's hot output loop. Mirrors CPython 3.11's
        // `CALL_NO_KW_BUILTIN_O` specialisation.
        if name.name == "write_byte" && args.len() == 1 {
            // Use the typed compile path so a typed-i64 result (from
            // e.g. `Op::IntArrayGetI64`) can flow through an
            // explicit `BoxI64` rather than being re-fetched as a
            // boxed `Value::Int`. The handler still expects a
            // `Value` register, but `BoxI64` is a single op.
            let byte_tr = self.compile_expr_ex(&args[0])?;
            let byte_reg = self.as_value(byte_tr);
            let dst = self.alloc_reg();
            self.emit(Op::StreamWriteByte {
                dst,
                stream_reg: receiver_reg,
                byte_reg,
            });
            return Ok(dst);
        }
        // Mirror super-instruction for `<u8vec>.set_byte(<idx>, <byte>)`.
        // fasta's per-byte buffer fill drives this op millions of
        // times per phase; the inline handler skips the
        // MethodCall + IC + `&[Value]` round-trip.
        if name.name == "set_byte" && args.len() == 2 {
            let idx_tr = self.compile_expr_ex(&args[0])?;
            let idx_reg = self.as_value(idx_tr);
            let byte_tr = self.compile_expr_ex(&args[1])?;
            let byte_reg = self.as_value(byte_tr);
            let dst = self.alloc_reg();
            self.emit(Op::U8VecSetByte {
                dst,
                u8vec_reg: receiver_reg,
                idx_reg,
                byte_reg,
            });
            return Ok(dst);
        }
        // Mirror super-instruction for `<u8vec>.get_byte(<idx>) -> i64`.
        // The handler writes into a typed `i64` register, so a
        // downstream `Op::Add` etc. picks the result up without an
        // intermediate `Value::Int` round-trip. Caller still
        // expects a `Value` register, so we box back through
        // `Op::BoxI64` - the register allocator and downstream
        // typed-arith specialisation usually elide that pair.
        if name.name == "get_byte" && args.len() == 1 {
            let idx_tr = self.compile_expr_ex(&args[0])?;
            let idx_reg = self.as_value(idx_tr);
            let dst_i = self.alloc_int();
            self.emit(Op::U8VecGetByte {
                dst_i,
                u8vec_reg: receiver_reg,
                idx_reg,
            });
            let dst = self.alloc_reg();
            self.emit(Op::BoxI64 {
                dst_v: dst,
                src_i: dst_i,
            });
            return Ok(dst);
        }
        // Mirror super-instruction for `<str>.substring(<start>, <end>)`.
        // The sliding-window k-mer counter calls this once per position;
        // the inline handler skips the MethodCall + IC + receiver clone +
        // `&[Value]` round-trip. Non-string receivers fall back at runtime.
        if name.name == "substring" && args.len() == 2 {
            let start_reg = self.compile_expr(&args[0])?;
            let end_reg = self.compile_expr(&args[1])?;
            let dst = self.alloc_reg();
            self.emit(Op::StrSubstring {
                dst,
                recv_reg: receiver_reg,
                start_reg,
                end_reg,
            });
            return Ok(dst);
        }
        // Fused `m.inc(key[, by])` counter increment for a HashMap
        // receiver. The sliding-window counter calls this once per
        // k-mer; the inline handler acquires the map lock once and
        // skips the MethodCall + IC + map-handle clone round-trip.
        if name.name == "inc"
            && (args.len() == 1 || args.len() == 2)
            && matches!(self.tcx.kind(receiver.ty), Some(TyKind::HashMap { .. }))
        {
            let key_reg = self.compile_expr(&args[0])?;
            let by_reg = if args.len() == 2 {
                self.compile_expr(&args[1])?
            } else {
                self.load_int_value(1)
            };
            let dst = self.alloc_reg();
            self.emit(Op::MapIncMethod {
                dst,
                map_reg: receiver_reg,
                key_reg,
                by_reg,
            });
            return Ok(dst);
        }
        // `s.push_utf8(buf, start, end)` mutates the receiver AND answers a
        // bool, so the receiver crosses as a write-back cell: the replacement
        // protocol below has only the return value to thread back, and here
        // that value is the flag rather than the new string.
        if matches!(name.name.as_str(), "push_utf8" | "push_json_quoted")
            && args.len() == 3
            && {
                let mut peeled = receiver.ty;
                while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                    peeled = *inner;
                }
                matches!(self.tcx.kind(peeled), Some(TyKind::String))
            }
            && self.place_root_is_local(receiver)
        {
            let cell = self.alloc_reg();
            self.emit(Op::CellNew {
                dst: cell,
                src: receiver_reg,
            });
            let cell_args_start = self.next_reg;
            self.next_reg = self
                .next_reg
                .checked_add(3)
                .expect("register overflow reserving push_utf8 args");
            for (i, arg) in args.iter().enumerate() {
                let a = self.compile_expr(arg)?;
                let slot = cell_args_start
                    .checked_add(u16::try_from(i).expect("argc overflow"))
                    .expect("reg overflow");
                self.ensure_reg_slot(slot);
                self.emit(Op::Move { dst: slot, src: a });
            }
            let name_idx = self.global_idx(if name.name.as_str() == "push_utf8" {
                "String::push_utf8"
            } else {
                "String::push_json_quoted"
            });
            let dst = self.alloc_reg();
            let cache_idx = self.alloc_cache_idx();
            self.emit(Op::MethodCall {
                dst,
                receiver: cell,
                name_idx,
                args: cell_args_start,
                argc: 3,
                cache_idx,
            });
            let updated = self.alloc_reg();
            self.emit(Op::CellTake { dst: updated, cell });
            self.compile_place_store(receiver, updated)?;
            return Ok(dst);
        }
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(u16::try_from(args.len()).map_err(|_| RuntimeError::Arity {
                expected: u16::MAX as usize,
                found: args.len(),
            })?)
            .expect("register overflow reserving method args");
        let mut cell_takes: Vec<(Reg, Reg)> = Vec::new();
        let mut place_takes: Vec<(&HirExpr, Reg)> = Vec::new();
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if let Some(home) = self.mut_ref_arg_home(arg, None) {
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
            } else if let Some(place) = Self::mut_ref_writeback_place(self.tcx, arg, None) {
                let place_reg = self.compile_expr(place)?;
                let cell = self.alloc_reg();
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
                    cell_takes.push((place_reg, cell));
                } else {
                    place_takes.push((place, cell));
                }
                arg_regs.push(cell);
            } else {
                let reg = self.compile_expr(arg)?;
                // The value a storing method keeps is a value of its own.
                let stores = matches!(
                    name.name.as_str(),
                    "push" | "push_back" | "push_front" | "insert" | "or_insert"
                ) && i + 1 == args.len();
                let reg = if stores {
                    self.stored_value_reg(arg, reg)
                } else {
                    reg
                };
                arg_regs.push(reg);
            }
        }
        for (i, r) in arg_regs.iter().enumerate() {
            let slot = args_start
                .checked_add(u16::try_from(i).expect("argc overflow"))
                .expect("reg overflow");
            self.ensure_reg_slot(slot);
            self.emit(Op::Move { dst: slot, src: *r });
        }
        let argc = u16::try_from(args.len()).map_err(|_| RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: args.len(),
        })?;
        // `m.pop(k)` on a HashMap mutates the map in place (it is an
        // `Arc<Mutex<..>>`) and returns `Option<V>`. The name-global `pop`
        // resolves to the Vec pop builtin, and the mutating-writeback below
        // would then overwrite the map binding with that result; route to
        // the qualified map builtin and suppress the writeback instead.
        let mut resolved_receiver_ty = receiver.ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved_receiver_ty) {
            resolved_receiver_ty = *inner;
        }
        let is_map_pop = name.name == "pop"
            && args.len() == 1
            && matches!(
                self.tcx.kind(resolved_receiver_ty),
                Some(TyKind::HashMap { .. })
            );
        // A traversal on a map or set answers eagerly from that container's own
        // surface; the bare name would reach the variant or Vec builtin.
        let traversal_owner = match self.tcx.kind(resolved_receiver_ty) {
            _ if !gossamer_types::is_collection_traversal_method(name.name.as_str()) => None,
            Some(TyKind::Adt { def, .. }) if def.local == HASH_SET_DEF_LOCAL => Some("Set"),
            Some(TyKind::Adt { def, .. }) if def.local == BTREE_SET_DEF_LOCAL => Some("BTreeSet"),
            _ => None,
        };
        let qualified_collection_method = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
                    && matches!(
                        name.name.as_str(),
                        "insert"
                            | "remove"
                            | "contains"
                            | "len"
                            | "is_empty"
                            | "clear"
                            | "to_vec"
                            | "iter"
                            | "union"
                            | "intersection"
                            | "difference"
                            | "symmetric_difference"
                            | "is_subset"
                            | "is_superset"
                            | "is_disjoint"
                    ) =>
            {
                let owner = if def.local == BTREE_SET_DEF_LOCAL {
                    "BTreeSet"
                } else {
                    "Set"
                };
                Some(format!("{owner}::{}", name.name))
            }
            Some(TyKind::Adt { def, .. })
                if def.local == VEC_DEQUE_DEF_LOCAL
                    && matches!(
                        name.name.as_str(),
                        "push_back"
                            | "push_front"
                            | "pop_back"
                            | "pop_front"
                            | "peek_back"
                            | "peek_front"
                            | "len"
                            | "is_empty"
                            | "clear"
                    ) =>
            {
                Some(format!("Deque::{}", name.name))
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL)
                    && matches!(
                        name.name.as_str(),
                        "push" | "pop" | "peek" | "len" | "is_empty" | "clear"
                    ) =>
            {
                let owner = if def.local == VEC_STACK_DEF_LOCAL {
                    "Stack"
                } else {
                    "Queue"
                };
                Some(format!("{owner}::{}", name.name))
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL)
                    && matches!(
                        name.name.as_str(),
                        "push" | "pop" | "peek" | "len" | "is_empty" | "clear"
                    ) =>
            {
                let owner = if def.local == MIN_HEAP_DEF_LOCAL {
                    "MinHeap"
                } else {
                    "MaxHeap"
                };
                Some(format!("{owner}::{}", name.name))
            }
            _ => None,
        };
        // An `impl` method wins over a builtin of the same name. An enum
        // value carries only its variant name at run time, so the receiver's
        // own type cannot be recovered there; naming the method by its
        // declaring type here is what reaches the user's body.
        let recorded_owner = owner
            .map(|owner| format!("{}::{}", owner.name, name.name))
            .filter(|qualified| self.fn_param_tys.contains_key(qualified));
        let user_impl_method = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::Adt { def, .. }) => self
                .tcx
                .def_name(*def)
                .map(|type_name| format!("{type_name}::{}", name.name))
                .filter(|qualified| self.fn_param_tys.contains_key(qualified)),
            // A non-`Adt` receiver whose type is known reaches its `impl`
            // block through the owner the checker recorded, and a name the
            // type already carries is answered by the type. Guessing an owner
            // from the name here would have an `impl Trait for String`
            // declaring `len` take every `len` call on a string.
            _ if !self.ty_is_unresolved(resolved_receiver_ty) => None,
            // An open receiver - a type parameter, an inference variable -
            // has no recorded owner, because one body serves every
            // instantiation. The name is all there is to bind to.
            _ => self
                .impl_target_names(resolved_receiver_ty)
                .into_iter()
                .map(|type_name| format!("{type_name}::{}", name.name))
                .find(|qualified| self.fn_param_tys.contains_key(qualified))
                // A payload binding extracted from a generic enum keeps an
                // unresolved type, which is exactly the receiver of a
                // recursive method's inner call. One `impl` declaring the
                // name settles it without a type: there is nothing else the
                // call could reach. A receiver whose type *is* known owns its
                // method surface, so the guess is confined to receivers whose
                // type is genuinely open.
                .or_else(|| {
                    self.ty_is_unresolved(resolved_receiver_ty)
                        .then(|| self.sole_impl_method(&name.name))
                        .flatten()
                }),
        };
        let dispatch_name = if is_map_pop {
            "Map::pop"
        } else if matches!(
            name.name.as_str(),
            "__gos_wrapping_add" | "__gos_wrapping_sub" | "__gos_wrapping_mul"
        ) {
            match self.tcx.kind(resolved_receiver_ty) {
                Some(TyKind::Int(int_ty)) => {
                    wrapping_dispatch_name(*int_ty, &name.name).unwrap_or(&name.name)
                }
                _ => &name.name,
            }
        } else {
            match traversal_owner {
                Some(owner) => format!("{owner}::{}", name.name).leak(),
                // The `impl` block the checker resolved this call to answers
                // first: the receiver's type decided it there, while here a
                // container and a structural type are one runtime shape.
                None => recorded_owner
                    .as_deref()
                    .or(qualified_collection_method.as_deref())
                    .or(user_impl_method.as_deref())
                    .unwrap_or(&name.name),
            }
        };
        let name_idx = self.global_idx(dispatch_name);
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::MethodCall {
            dst,
            receiver: receiver_reg,
            name_idx,
            args: args_start,
            argc,
            cache_idx,
        });
        for (home, cell) in cell_takes {
            self.emit(Op::CellTake { dst: home, cell });
        }
        for (place, cell) in place_takes {
            let tmp = self.alloc_reg();
            self.emit(Op::CellTake { dst: tmp, cell });
            self.compile_place_store(place, tmp)?;
        }
        // Mutating-method writeback. The builtins for `push` /
        // `insert` / etc. return the *new* aggregate rather than
        // mutating in place, so the VM has to thread the result back
        // into the receiver's storage. A bare local receiver is the
        // common case (one `Op::Move`); an index / field place rooted
        // at a local (`groups[i].push(x)`, `bag.items.push(x)`) splices
        // the result back through the place-store protocol so the
        // mutation persists - matching the compiled tiers, which mutate
        // the backing storage in place.
        let replacement_writeback = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::String | TyKind::Vec(_) | TyKind::Slice(_)) => true,
            Some(TyKind::Array { .. }) => matches!(
                name.name.as_str(),
                "sort" | "sort_by" | "sort_by_key" | "reverse" | "swap" | "fill"
            ),
            _ => false,
        };
        if !is_map_pop && replacement_writeback && Self::is_mutating_method_name(name.name.as_str())
        {
            match &receiver.kind {
                // A `static mut` receiver's storage is the shared cell, which
                // the place store writes through. A read loads the cell into a
                // register, so without this the mutation reached only that
                // register.
                _ if self.place_root_is_mut_static(receiver) => {
                    self.compile_place_store(receiver, dst)?;
                }
                HirExprKind::Path { segments, .. } if segments.len() == 1 => {
                    if let Some(target) = self.lookup_local(&segments[0].name) {
                        if target.kind == RegKind::Value && target.reg == receiver_reg {
                            self.emit(Op::Move {
                                dst: target.reg,
                                src: dst,
                            });
                        }
                    }
                }
                HirExprKind::Index { .. }
                | HirExprKind::Field { .. }
                | HirExprKind::TupleIndex { .. }
                    if self.place_root_is_local(receiver) =>
                {
                    self.compile_place_store(receiver, dst)?;
                }
                // `m.or_insert(k, d).push(v)`: the entry the receiver came
                // from is where the mutation belongs, so the updated
                // aggregate goes back under the same key. The compiled tiers
                // hand back the stored value itself and mutate it in place.
                HirExprKind::MethodCall {
                    receiver: map_expr,
                    name: entry_name,
                    args: entry_args,
                    owner: None,
                } if entry_name.name.as_str() == "or_insert" && entry_args.len() == 2 => {
                    let map_reg = self.compile_expr(map_expr)?;
                    let key_reg = self.compile_expr(&entry_args[0])?;
                    let scratch = self.alloc_reg();
                    self.emit(Op::MapInsert {
                        dst: scratch,
                        map_reg,
                        key_reg,
                        value_reg: dst,
                    });
                    self.compile_place_store(map_expr, map_reg)?;
                }
                _ => {}
            }
        }
        self.release_chain_temp(receiver, receiver_reg);
        let returns_unit = match self.tcx.kind(resolved_receiver_ty) {
            Some(TyKind::String) => matches!(
                name.name.as_str(),
                "push" | "push_str" | "push_char" | "push_byte" | "clear" | "truncate"
            ),
            Some(TyKind::Vec(_) | TyKind::Slice(_)) => matches!(
                name.name.as_str(),
                "push"
                    | "insert"
                    | "clear"
                    | "extend"
                    | "extend_from_slice"
                    | "truncate"
                    | "sort"
                    | "sort_by"
                    | "sort_by_key"
                    | "reverse"
                    | "retain"
                    | "drain"
                    | "swap"
                    | "fill"
                    | "resize"
                    | "copy_within"
                    | "copy_from_slice"
            ),
            Some(TyKind::Array { .. }) => matches!(
                name.name.as_str(),
                "sort" | "sort_by" | "sort_by_key" | "reverse" | "swap" | "fill"
            ),
            Some(TyKind::HashMap { .. }) => name.name == "clear",
            Some(TyKind::Adt { def, .. }) if def.local == VEC_DEQUE_DEF_LOCAL => {
                matches!(name.name.as_str(), "push_back" | "push_front")
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL) =>
            {
                matches!(name.name.as_str(), "push" | "clear")
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                matches!(name.name.as_str(), "push" | "clear")
            }
            _ => false,
        };
        if returns_unit {
            Ok(self.load_unit())
        } else {
            Ok(dst)
        }
    }

    /// Lowers a `&mut self` user-method call (`obj.bump()`,
    /// `(&mut __for_iter).next()`) through the write-back cell protocol
    /// so the method's mutation of `self` persists in the caller's
    /// binding. Returns `Some(result_reg)` when the receiver resolves to
    /// a local-rooted place whose `Type::method` is a known `&mut self`
    /// method; otherwise `None`, leaving the generic dispatch to handle
    /// it (a temporary receiver's mutation is discarded, matching the
    /// compiled tiers). The receiver crosses as a `MutCell`; the callee
    /// unwraps it (its `self` register is a `mut_ref_param`) and
    /// publishes the post-call `self` on return, which `Op::CellTake` +
    /// `compile_place_store` write back into the receiver place.
    fn try_compile_mut_self_method(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
    ) -> RuntimeResult<Option<Reg>> {
        let place = peel_ref_wrappers_expr(receiver);
        // Direct locals, fields, and indexed elements rooted at locals are
        // writable places. Temporaries remain values and receive no writeback.
        if !self.place_root_is_local(place) {
            return Ok(None);
        }
        let qual = self
            .impl_target_names(place.ty)
            .into_iter()
            .map(|type_name| format!("{type_name}::{}", name.name))
            .find(|qual| self.method_muts.contains(qual))
            .or_else(|| {
                // Name-only fallback, for a receiver whose type is not
                // resolved here at all. A receiver whose type IS known owns
                // its method surface: a `&mut self` method of that name on
                // some other type is not a candidate for it, and binding to
                // one would call a body the receiver's type never declared.
                if !self.ty_is_unresolved(place.ty) {
                    return None;
                }
                let suffix = format!("::{}", name.name);
                let mut matches = self
                    .method_muts
                    .iter()
                    .filter(|qual| qual.ends_with(&suffix));
                let qual = matches.next()?.clone();
                matches.next().is_none().then_some(qual)
            });
        // A generic receiver names its type only at run time, so a method
        // several implementors declare is dispatched on the value. The cell
        // still carries the receiver, which is what publishes the mutation
        // back; only the callee is chosen dynamically.
        let dynamic = qual.is_none()
            && matches!(
                self.tcx.kind(self.unwrap_ref(place.ty)),
                Some(TyKind::Param { .. })
            )
            && self
                .method_muts
                .iter()
                .any(|entry| entry.ends_with(&format!("::{}", name.name)));
        if qual.is_none() && !dynamic {
            return Ok(None);
        }
        let total = args.len() + 1;
        let argc = u16::try_from(total).map_err(|_| RuntimeError::Arity {
            expected: u16::MAX as usize,
            found: total,
        })?;
        // `Type::method` is registered for every user `impl` method, so
        // the global resolves; loading it yields the callee identity the
        // `Op::Call` inline cache keys on.
        let callee_reg = qual.map(|qual| {
            let global_idx = self.global_idx(&qual);
            let callee_reg = self.alloc_reg();
            self.emit(Op::LoadGlobal {
                dst: callee_reg,
                idx: global_idx,
            });
            callee_reg
        });
        // Reserve the contiguous arg block (receiver cell + declared
        // args) before compiling any operand, so an operand whose
        // compile allocates fresh registers can't clobber the span.
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(argc)
            .expect("register overflow reserving mut-self method args");
        let place_reg = self.compile_expr(place)?;
        // Evaluate every argument BEFORE capturing the receiver, so an argument
        // that reads the receiver (`c.bump(c.value)`) still sees its live value
        // - `CellNewMove` empties the receiver's register immediately after.
        //
        // A `&mut` argument rides the same write-back cell the generic call
        // path gives it: a `&mut self` method may take an out-parameter
        // (`conn.next_row(&mut stream)`), and passing that by value would
        // leave every mutation on a copy.
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len());
        let mut arg_cell_takes: Vec<(Reg, Reg)> = Vec::new();
        let mut arg_place_takes: Vec<(&HirExpr, Reg)> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if let Some(home) = self.mut_ref_arg_home(arg, None) {
                let arg_cell = self.alloc_reg();
                if Self::mut_ref_place_name(arg)
                    .is_some_and(|name| Self::mut_arg_move_safe(args, i, name))
                {
                    self.emit(Op::CellNewMove {
                        dst: arg_cell,
                        src: home,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: arg_cell,
                        src: home,
                    });
                }
                arg_cell_takes.push((home, arg_cell));
                arg_regs.push(arg_cell);
            } else if let Some(place) = Self::mut_ref_writeback_place(self.tcx, arg, None) {
                let place_reg = self.compile_expr(place)?;
                let arg_cell = self.alloc_reg();
                let local_home = Self::path_single_seg_name(place).and_then(|name| {
                    self.lookup_local(name)
                        .filter(|tr| tr.kind == RegKind::Value)
                        .map(|_| name)
                });
                if local_home.is_some_and(|name| Self::mut_arg_move_safe(args, i, name)) {
                    self.emit(Op::CellNewMove {
                        dst: arg_cell,
                        src: place_reg,
                    });
                } else {
                    self.emit(Op::CellNew {
                        dst: arg_cell,
                        src: place_reg,
                    });
                }
                if local_home.is_some() {
                    arg_cell_takes.push((place_reg, arg_cell));
                } else {
                    arg_place_takes.push((place, arg_cell));
                }
                arg_regs.push(arg_cell);
            } else {
                arg_regs.push(self.compile_expr(arg)?);
            }
        }
        let cell = self.alloc_reg();
        // Move (not clone) the receiver into the cell. `CellTake` below
        // republishes the post-call `self` into the same place, and moving
        // keeps the receiver's refcount at one so the callee's first field
        // write mutates in place instead of forcing a copy-on-write clone.
        self.emit(Op::CellNewMove {
            dst: cell,
            src: place_reg,
        });
        self.emit(Op::Move {
            dst: args_start,
            src: cell,
        });
        for (i, r) in arg_regs.iter().enumerate() {
            let slot = args_start
                .checked_add(u16::try_from(i + 1).expect("argc overflow"))
                .expect("register overflow");
            self.emit(Op::Move { dst: slot, src: *r });
        }
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        // The receiver `MutCell` passes through `auto_deref_cell`
        // untouched (it only resolves `flag::Cell` handles); a declared
        // arg that is a `flag::Cell` still needs the auto-deref, so gate
        // it on a non-scalar arg being present.
        let may_have_cells = !args.iter().all(|a| {
            matches!(
                self.tcx.kind(a.ty),
                Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
            )
        });
        match callee_reg {
            Some(callee) => self.emit(Op::Call {
                dst,
                callee,
                args: args_start,
                argc,
                cache_idx,
                may_have_cells,
            }),
            None => {
                let name_idx = self.global_idx(name.name.as_str());
                let arg_block = args_start.checked_add(1).expect("register overflow");
                self.emit(Op::MethodCall {
                    dst,
                    receiver: args_start,
                    name_idx,
                    args: arg_block,
                    argc: argc.saturating_sub(1),
                    cache_idx,
                })
            }
        };
        let tmp = self.alloc_reg();
        self.emit(Op::CellTake { dst: tmp, cell });
        self.compile_place_store(place, tmp)?;
        for (home, arg_cell) in arg_cell_takes {
            self.emit(Op::CellTake {
                dst: home,
                cell: arg_cell,
            });
        }
        for (arg_place, arg_cell) in arg_place_takes {
            let taken = self.alloc_reg();
            self.emit(Op::CellTake {
                dst: taken,
                cell: arg_cell,
            });
            self.compile_place_store(arg_place, taken)?;
        }
        Ok(Some(dst))
    }
}

impl FnBuilder<'_> {
    /// A mutating method on a window receiver `(&mut base[lo..hi]).m(..)`.
    /// The window's elements are bound to a hidden local, the method runs
    /// against that local through the ordinary local write-back, and the
    /// result is spliced into the range the base and bounds named when the
    /// call began.
    fn try_compile_window_method(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        args: &[HirExpr],
        owner: Option<&Ident>,
    ) -> RuntimeResult<Option<Reg>> {
        let HirExprKind::Unary {
            op: HirUnaryOp::RefMut,
            operand,
        } = &receiver.kind
        else {
            return Ok(None);
        };
        if !Self::is_mutating_method_name(name.name.as_str()) {
            return Ok(None);
        }
        let window_ty = match self.tcx.kind(receiver.ty) {
            Some(TyKind::Ref { inner, .. }) => *inner,
            _ => return Ok(None),
        };
        let Some((base, range, window)) = self.compile_window_parts(operand)? else {
            return Ok(None);
        };
        let local_name = format!("__window_{}", receiver.id.0);
        self.push_scope();
        self.bind_local(
            &local_name,
            TypedReg {
                reg: window,
                kind: RegKind::Value,
            },
        );
        let local = HirExpr {
            id: receiver.id,
            span: receiver.span,
            ty: window_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(local_name)],
                def: None,
            },
        };
        let result = self.compile_method_call(&local, name, args, owner);
        self.pop_scope();
        let result = result?;
        self.splice_window(&base, &range, window)?;
        Ok(Some(result))
    }
}
