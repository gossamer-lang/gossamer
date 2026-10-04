//! Kind-dispatched calls: destination kinds, argument operands, and the fallback call.

use super::*;

impl<'a> Builder<'a> {
    /// Lower a runtime-kind dispatched call: lower receiver, build args, emit.
    pub(super) fn lower_kind_dispatch_call(
        &mut self,
        rt: &'static str,
        receiver: &HirExpr,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let receiver_local = self.lower_expr(receiver)?;
        let (arg_operands, rt, mut_ref_reloads) =
            self.kind_dispatch_arg_operands(rt, receiver, receiver_local, args, span)?;
        let pinned = self.dispatch_pinned_ty(rt, receiver, receiver_local, ty);
        let dest = self.fresh(pinned);
        if let Some(k) = self.dispatch_dest_kind(rt) {
            let k = if k == "collections::HashSet"
                && self.runtime_kind_from_ty(receiver.ty) == Some("collections::BTreeSet")
            {
                "collections::BTreeSet"
            } else {
                k
            };
            self.local_runtime_kind.insert(dest, k);
        }
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(rt.to_string())),
            args: arg_operands,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        for (place_local, ref_local) in mut_ref_reloads {
            self.emit_assign(
                Place::local(place_local),
                Rvalue::Use(Operand::Copy(Place {
                    local: ref_local,
                    projection: vec![crate::ir::Projection::Deref],
                })),
                span,
            );
        }
        Some(dest)
    }

    /// Build the argument operand list for a pre-lowering kind-dispatched call.
    pub(super) fn kind_dispatch_arg_operands(
        &mut self,
        rt: &'static str,
        receiver: &HirExpr,
        receiver_local: Local,
        args: &[HirExpr],
        span: Span,
    ) -> Option<KindDispatchArgs> {
        let mut arg_operands: Vec<Operand> = Vec::with_capacity(args.len() + 1);
        let mut mut_ref_reloads = Vec::new();
        arg_operands.push(Operand::Copy(Place::local(receiver_local)));
        // `xs.slice(a, b)` on a `[T; N]` literal needs the
        // static length: the inline buffer carries no length
        // prefix, so the runtime helper takes
        // `(ptr, len, start, end)` instead of the
        // `(ptr, start, end)` shape used by Vec receivers.
        // Splice the constant N read from the receiver's MIR
        // type before the user-supplied start/end args.
        // Router HTTP-verb methods take (router, pattern,
        // env, fn_addr) - synthesize the handler's env+fn_addr
        // from the trailing user argument (must be a struct
        // whose impl Handler { fn serve(...) }).
        let router_handler_method = matches!(
            rt,
            "gos_rt_router_get"
                | "gos_rt_router_post"
                | "gos_rt_router_put"
                | "gos_rt_router_delete"
                | "gos_rt_router_patch"
                | "gos_rt_router_head"
                | "gos_rt_router_options"
                | "gos_rt_router_add"
        );
        let mut rt = rt;
        // A slot-backed container holds one word per element, so a float
        // element crosses as its bit pattern: the `_f64` entry point stores
        // the bits the `Option<f64>` read on the way out reinterprets.
        if matches!(rt, "gos_rt_deque_push_back" | "gos_rt_deque_push_front")
            && args
                .first()
                .is_some_and(|arg| matches!(self.tcx.kind_of(arg.ty), TyKind::Float(_)))
        {
            rt = match rt {
                "gos_rt_deque_push_back" => "gos_rt_deque_push_back_f64",
                _ => "gos_rt_deque_push_front_f64",
            };
        }
        // An aggregate element - a struct, tuple, array, inline enum, or
        // `Option` / `Result` carrier - is handed over by the address of
        // its slots, which is what the wide entry point takes. This holds
        // however narrow it is: a one-field struct is still an aggregate,
        // and the one-word entry point would store its address.
        if matches!(rt, "gos_rt_deque_push_back" | "gos_rt_deque_push_front")
            && args.first().is_some_and(|arg| {
                self.type_slot_bytes(arg.ty) > 8 || self.tcx.is_flat_inline_aggregate(arg.ty)
            })
        {
            rt = match rt {
                "gos_rt_deque_push_back" => "gos_rt_deque_push_back_wide",
                _ => "gos_rt_deque_push_front_wide",
            };
        }
        let aggregate_set_desc = self
            .first_generic_of(receiver.ty)
            .filter(|elem| self.is_aggregate_key(*elem))
            .and_then(|elem| self.key_descriptor(elem));
        // A user enum's value is a counted node, so a set of them keys by the
        // same discriminant-and-payload bytes an enum-keyed map uses.
        let enum_set_desc = self
            .first_generic_of(receiver.ty)
            .filter(|elem| aggregate_set_desc.is_none() && self.struct_name_of(*elem).is_none())
            .and_then(|elem| self.ensure_enum_eq_desc(elem));
        // An i64-element `HashSet` stores its keys as decimal strings;
        // passing the raw i64 to the String shims reinterprets it as a
        // key pointer and crashes. The element kind is erased from the
        // set's handle type, so read it from the queried element
        // argument.
        if matches!(
            rt,
            "gos_rt_set_insert" | "gos_rt_set_contains" | "gos_rt_set_remove"
        ) && matches!(
            args.first()
                .map(|a| map_key_kind_from(self.tcx, self.peel_ref_ty(a.ty))),
            Some(MapKeyKind::I64)
        ) {
            rt = match rt {
                "gos_rt_set_insert" => "gos_rt_set_insert_i64",
                "gos_rt_set_contains" => "gos_rt_set_contains_i64",
                "gos_rt_set_remove" => "gos_rt_set_remove_i64",
                _ => rt,
            };
        }
        // `to_vec` / `iter` carry no element argument, so recover the
        // set's element kind from the receiver's HIR type to read an
        // i64 set's keys back as integers (sorted numerically).
        if rt == "gos_rt_set_range_str"
            && matches!(self.set_elem_kind_of(receiver), MapKeyKind::I64)
        {
            rt = "gos_rt_set_range_i64";
        }
        if rt == "gos_rt_set_to_vec" && matches!(self.set_elem_kind_of(receiver), MapKeyKind::I64) {
            rt = if self.set_elems_unsigned(receiver.ty) {
                "gos_rt_set_to_vec_u64"
            } else {
                "gos_rt_set_to_vec_i64"
            };
        }
        if enum_set_desc.is_some() {
            rt = match rt {
                "gos_rt_set_insert" | "gos_rt_set_insert_i64" => "gos_rt_set_insert_ekey",
                "gos_rt_set_contains" | "gos_rt_set_contains_i64" => "gos_rt_set_contains_ekey",
                "gos_rt_set_remove" | "gos_rt_set_remove_i64" => "gos_rt_set_remove_ekey",
                "gos_rt_set_to_vec" | "gos_rt_set_to_vec_i64" => "gos_rt_set_to_vec_ekey",
                "gos_rt_set_range_str" | "gos_rt_set_range_i64" => "gos_rt_set_range_ekey",
                _ => rt,
            };
        }
        if aggregate_set_desc.is_some() {
            rt = match rt {
                "gos_rt_set_insert" => "gos_rt_set_insert_skey",
                "gos_rt_set_contains" => "gos_rt_set_contains_skey",
                "gos_rt_set_remove" => "gos_rt_set_remove_skey",
                "gos_rt_set_to_vec" => "gos_rt_set_to_vec_skey",
                "gos_rt_set_intersection" => "gos_rt_set_intersection_skey",
                "gos_rt_set_range_str" => "gos_rt_set_range_skey",
                _ => rt,
            };
        }
        if router_handler_method && !args.is_empty() {
            let handler_idx = args.len() - 1;
            for arg in &args[..handler_idx] {
                let reload_target = self.mut_ref_reload_target(arg);
                let a = self.lower_expr(arg)?;
                if let Some(place_local) = reload_target {
                    mut_ref_reloads.push((place_local, a));
                }
                let a = self.auto_deref_cell(a, span);
                arg_operands.push(Operand::Copy(Place::local(a)));
            }
            let reload_target = self.mut_ref_reload_target(&args[handler_idx]);
            let handler_local = self.lower_expr(&args[handler_idx])?;
            if let Some(place_local) = reload_target {
                mut_ref_reloads.push((place_local, handler_local));
            }
            match self.emit_router_handler_abi(handler_local, span) {
                RouterHandlerAbi::Bare(fn_addr) => {
                    arg_operands.push(fn_addr);
                    if let Some(bare_rt) = Self::router_bare_variant(rt) {
                        rt = bare_rt;
                    }
                }
                RouterHandlerAbi::WithEnv { env, fn_addr } => {
                    arg_operands.push(env);
                    arg_operands.push(fn_addr);
                }
            }
        } else {
            // `Client::request` / `request_bytes` take Vec-shaped
            // body/header args; coerce `[a, b]` array literals to
            // the heap GosVec shape the runtime ABI expects (same
            // treatment as the free `http::request` lowering).
            let coerce_vec_args = matches!(
                rt,
                "gos_rt_http_client_request"
                    | "gos_rt_http_client_request_bytes"
                    | "gos_rt_tcp_stream_write"
                    | "gos_rt_fs_file_write"
                    | "gos_rt_unix_stream_write"
                    | "gos_rt_udp_send_to"
                    | "gos_rt_flag_set_parse"
            );
            for arg in args {
                let reload_target = self.mut_ref_reload_target(arg);
                let a = self.lower_expr(arg)?;
                if let Some(place_local) = reload_target {
                    mut_ref_reloads.push((place_local, a));
                }
                let a = self.auto_deref_cell(a, span);
                let mut a = if coerce_vec_args {
                    let lt = self.locals[a.0 as usize].ty;
                    if let TyKind::Array { elem, len } = self.tcx.kind_of(lt).clone() {
                        self.coerce_array_to_vec(a, elem, len, span)
                    } else {
                        a
                    }
                } else {
                    a
                };
                if rt == "gos_rt_chan_send"
                    && matches!(
                        self.tcx.kind_of(self.locals[a.0 as usize].ty),
                        TyKind::Vec(_)
                            | TyKind::Adt { .. }
                            | TyKind::Tuple(_)
                            | TyKind::Array { .. }
                    )
                    && matches!(
                        &arg.kind,
                        HirExprKind::Path { .. }
                            | HirExprKind::Field { .. }
                            | HirExprKind::TupleIndex { .. }
                            | HirExprKind::Index { .. }
                    )
                {
                    let cloned = self.fresh(self.locals[a.0 as usize].ty);
                    self.emit_owned_clone_binding(a, cloned, span);
                    a = cloned;
                }
                arg_operands.push(Operand::Copy(Place::local(a)));
            }
        }
        // A range's descriptor sits between its two bounds: `(set, lo, desc,
        // hi, mode)`, the shape the other key-descriptor calls share.
        if let Some(desc) = match rt {
            "gos_rt_set_range_skey" => aggregate_set_desc.clone(),
            "gos_rt_set_range_ekey" => enum_set_desc.clone(),
            _ => None,
        } && arg_operands.len() >= 2
        {
            arg_operands.insert(2, Operand::Const(ConstValue::Str(desc)));
        }
        if matches!(
            rt,
            "gos_rt_set_insert_skey"
                | "gos_rt_set_contains_skey"
                | "gos_rt_set_remove_skey"
                | "gos_rt_set_to_vec_skey"
        ) && let Some(desc) = aggregate_set_desc
        {
            arg_operands.push(Operand::Const(ConstValue::Str(desc)));
        }
        if matches!(
            rt,
            "gos_rt_set_insert_ekey" | "gos_rt_set_contains_ekey" | "gos_rt_set_remove_ekey"
        ) && let Some(desc) = enum_set_desc
        {
            arg_operands.push(Operand::Const(ConstValue::Str(desc)));
        }
        // A map or set stores one word per key and per scalar value, and a
        // float's word is its bit pattern: the entry point's `i64` parameter
        // would otherwise convert the value, so `1.5` would be stored - and
        // read back - as the float those bits spell.
        for (index, operand) in arg_operands.iter_mut().enumerate() {
            if !float_word_arg(rt, index) {
                continue;
            }
            let Operand::Copy(place) = operand else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            if !matches!(
                self.tcx.kind_of(self.locals[place.local.0 as usize].ty),
                TyKind::Float(_)
            ) {
                continue;
            }
            let bits = self.emit_float_bits(place.local, span);
            *operand = Operand::Copy(Place::local(bits));
        }
        // An ordered container compares its elements through the ordering
        // descriptor of the element type, which travels with the call.
        if matches!(
            rt,
            "gos_rt_bheap_max_push_desc"
                | "gos_rt_bheap_min_push_desc"
                | "gos_rt_bheap_max_pop_desc"
                | "gos_rt_bheap_min_pop_desc"
        ) {
            let stream = self
                .first_generic_of(receiver.ty)
                .or_else(|| self.first_generic_of(self.locals[receiver_local.0 as usize].ty))
                .and_then(|elem| self.ordering_stream(elem))?;
            let text: String = stream.iter().map(|&b| b as char).collect();
            arg_operands.push(Operand::Const(ConstValue::Str(text)));
        }
        Some((arg_operands, rt, mut_ref_reloads))
    }

    /// Pin the MIR result type for a dispatched runtime symbol.
    #[allow(
        clippy::too_many_lines,
        reason = "flat runtime-symbol to MIR-result-type table; one arm per symbol"
    )]
    pub(super) fn dispatch_pinned_ty(
        &mut self,
        rt: &'static str,
        receiver: &HirExpr,
        receiver_local: Local,
        ty: Ty,
    ) -> Ty {
        match rt {
            // An atomic's word is the width its type names, which the checker
            // already gave the call: a `u64` read renders unsigned.
            "gos_rt_atomic_i64_load"
            | "gos_rt_atomic_i64_fetch_add"
            | "gos_rt_atomic_i64_fetch_sub"
            | "gos_rt_atomic_i32_fetch_add"
            | "gos_rt_atomic_i32_fetch_sub"
            | "gos_rt_math_rng_next_u64"
            | "gos_rt_math_rng_next_u32"
            | "gos_rt_math_rng_range_u64" => ty,
            "gos_rt_mutex_lock"
            | "gos_rt_mutex_unlock"
            | "gos_rt_wg_add"
            | "gos_rt_wg_done"
            | "gos_rt_wg_wait"
            | "gos_rt_barrier_wait"
            | "gos_rt_atomic_i64_store" => self.tcx.unit(),
            "gos_rt_error_display"
            | "gos_rt_error_message"
            | "gos_rt_bufio_scanner_text"
            | "gos_rt_http_response_body"
            | "gos_rt_http_request_path"
            | "gos_rt_http_request_path_value"
            | "gos_rt_http_request_value"
            | "gos_rt_http_request_form_value"
            | "gos_rt_http_request_method"
            | "gos_rt_regex_find"
            | "gos_rt_regex_replace"
            | "gos_rt_regex_replace_all"
            | "gos_rt_strings_join"
            | "gos_rt_flag_set_usage" => self.tcx.string_ty(),
            "gos_rt_http_server_addr" => self.tcx.string_ty(),
            "gos_rt_http_response_stream_is_open"
            | "gos_rt_http_server_shutdown"
            | "gos_rt_error_is"
            | "gos_rt_error_is_sentinel"
            | "gos_rt_regex_is_match"
            | "gos_rt_bufio_scanner_scan"
            | "gos_rt_set_insert"
            | "gos_rt_set_insert_i64"
            | "gos_rt_set_insert_skey"
            | "gos_rt_set_contains_skey"
            | "gos_rt_set_remove_skey"
            | "gos_rt_set_insert_ekey"
            | "gos_rt_set_contains_ekey"
            | "gos_rt_set_remove_ekey"
            | "gos_rt_set_contains"
            | "gos_rt_set_contains_i64"
            | "gos_rt_set_remove"
            | "gos_rt_set_remove_i64"
            | "gos_rt_set_is_subset"
            | "gos_rt_set_is_superset"
            | "gos_rt_set_is_disjoint" => self.tcx.bool_ty(),
            "gos_rt_set_to_vec" => {
                let s = self.tcx.string_ty();
                self.tcx.intern(gossamer_types::TyKind::Vec(s))
            }
            // A window or a range of a set is a set of the same type.
            "gos_rt_set_window"
            | "gos_rt_set_range_i64"
            | "gos_rt_set_range_str"
            | "gos_rt_set_range_skey"
            | "gos_rt_set_range_ekey" => receiver.ty,
            "gos_rt_set_to_vec_ekey" => {
                let elem = self
                    .first_generic_of(receiver.ty)
                    .or_else(|| self.first_generic_of(self.locals[receiver_local.0 as usize].ty))
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                self.tcx.intern(gossamer_types::TyKind::Vec(elem))
            }
            "gos_rt_set_to_vec_i64" | "gos_rt_set_to_vec_u64" => {
                // The snapshot's slots are the set's own words, so the vec
                // takes the element type they hold: a float set answers a
                // `Vec<f64>` whose slots are those same bits.
                let elem = self
                    .first_generic_of(receiver.ty)
                    .or_else(|| self.first_generic_of(self.locals[receiver_local.0 as usize].ty))
                    .filter(|elem| {
                        matches!(
                            self.tcx.kind_of(*elem),
                            TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Int(_)
                        )
                    })
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                self.tcx.intern(gossamer_types::TyKind::Vec(elem))
            }
            // The content-keyed snapshot is a materialised vec on every tier,
            // exactly as the scalar-element ones above are, so `iter()` names
            // the same sequence `to_vec()` does rather than a lazy cursor the
            // handle has no protocol for.
            "gos_rt_set_to_vec_skey" => match self.tcx.kind_of(ty) {
                gossamer_types::TyKind::Iterator(elem) | gossamer_types::TyKind::Vec(elem) => {
                    let elem = *elem;
                    self.tcx.intern(gossamer_types::TyKind::Vec(elem))
                }
                _ => ty,
            },
            "gos_rt_http_response_status"
            | "gos_rt_vec_capacity"
            | "gos_rt_set_len"
            | "gos_rt_set_clear" => self.tcx.int_ty(gossamer_types::IntTy::I64),
            "gos_rt_flag_set_short" => self.tcx.unit(),
            "gos_rt_deque_push_back"
            | "gos_rt_deque_push_back_f64"
            | "gos_rt_deque_push_back_wide"
            | "gos_rt_deque_push_front"
            | "gos_rt_deque_push_front_f64"
            | "gos_rt_deque_push_front_wide"
            | "gos_rt_deque_clear" => self.tcx.unit(),
            "gos_rt_bheap_max_push_i64"
            | "gos_rt_bheap_max_push_f64"
            | "gos_rt_bheap_min_push_i64"
            | "gos_rt_bheap_min_push_f64"
            | "gos_rt_bheap_clear" => self.tcx.unit(),
            "gos_rt_bheap_max_pop_i64"
            | "gos_rt_bheap_max_pop_f64"
            | "gos_rt_bheap_max_peek_i64"
            | "gos_rt_bheap_min_pop_i64"
            | "gos_rt_bheap_min_pop_f64"
            | "gos_rt_bheap_min_peek_i64" => ty,
            "gos_rt_bheap_is_empty" => self.tcx.bool_ty(),
            // `Child::read_line() -> Option<String>`; `wait` returns
            // `Result<i64, errors::Error>`. Pinned so the while-let /
            // match extraction reads the packed enum correctly.
            "gos_rt_child_read_line" | "gos_rt_stream_next_line" => self.option_string_adt_ty(),
            "gos_rt_stream_read_line" => self.result_i64_error_adt_ty(),
            "gos_rt_child_read_stdout" => self.tcx.string_ty(),
            "gos_rt_option_unwrap"
            | "gos_rt_result_unwrap"
            | "gos_rt_result_unwrap_or"
            | "gos_rt_result_unwrap_or_node"
            | "gos_rt_result_ok" => {
                let inner = self
                    .first_generic_of(receiver.ty)
                    .or_else(|| {
                        let recv_mir_ty = self.locals[receiver_local.0 as usize].ty;
                        self.first_generic_of(recv_mir_ty)
                    })
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                if self.is_reverse_i64_ty(inner) {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                } else {
                    inner
                }
            }
            // `.ok()` / `.err()` wrap the selected side in an `Option`, so the
            // destination is the carrier rather than the payload.
            "gos_rt_result_to_opt_ok" | "gos_rt_result_to_opt_err" => {
                let recv_mir_ty = self.locals[receiver_local.0 as usize].ty;
                let payload = if rt == "gos_rt_result_to_opt_ok" {
                    self.first_generic_of(receiver.ty)
                        .or_else(|| self.first_generic_of(recv_mir_ty))
                } else {
                    self.second_generic_of(receiver.ty)
                        .or_else(|| self.second_generic_of(recv_mir_ty))
                }
                .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                self.option_payload_adt_ty(payload)
            }
            "gos_rt_child_write_stdin" | "gos_rt_child_kill" => self.tcx.bool_ty(),
            "gos_rt_child_close_stdin" => self.tcx.unit(),
            "gos_rt_signal_wait" => self.tcx.bool_ty(),
            "gos_rt_signal_try_wait" => self.tcx.bool_ty(),
            "gos_rt_signal_stop" => self.tcx.unit(),
            "gos_rt_child_wait" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
                self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                })
            }
            // `VecDeque<T>::pop_front` / `pop_back` / `peek_front` /
            // `peek_back` return `Option<T>`. Recover the element from the
            // deque's sole generic so a `VecDeque<String>` binds its
            // Some-payload as a String rather than the pointer bits an i64
            // payload would render.
            "gos_rt_deque_pop_front"
            | "gos_rt_deque_pop_back"
            | "gos_rt_deque_peek_front"
            | "gos_rt_deque_peek_back"
            | "gos_rt_bheap_max_pop_desc"
            | "gos_rt_bheap_min_pop_desc"
            | "gos_rt_bheap_peek_elem" => {
                let recv_mir_ty = self.locals[receiver_local.0 as usize].ty;
                let elem = self
                    .first_generic_of(receiver.ty)
                    .or_else(|| self.first_generic_of(recv_mir_ty))
                    .or_else(|| self.first_generic_of(ty))
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                let substs = gossamer_types::Substs::from_types([elem]);
                self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                })
            }
            "gos_rt_deque_is_empty" | "gos_rt_set_is_empty" => self.tcx.bool_ty(),
            "gos_rt_router_add" | "gos_rt_router_add_fn" => self.tcx.unit(),
            "gos_rt_router_get"
            | "gos_rt_router_post"
            | "gos_rt_router_put"
            | "gos_rt_router_delete"
            | "gos_rt_router_patch"
            | "gos_rt_router_head"
            | "gos_rt_router_options"
            | "gos_rt_router_get_fn"
            | "gos_rt_router_post_fn"
            | "gos_rt_router_put_fn"
            | "gos_rt_router_delete_fn"
            | "gos_rt_router_patch_fn"
            | "gos_rt_router_head_fn"
            | "gos_rt_router_options_fn" => self.locals[receiver_local.0 as usize].ty,
            "gos_rt_regex_find_all" | "gos_rt_regex_split" => {
                let s = self.tcx.string_ty();
                self.tcx.intern(gossamer_types::TyKind::Vec(s))
            }
            "gos_rt_flag_set_parse" => self.result_vec_string_error_ty(),
            "gos_rt_error_cause" => self.option_adt_ty(),
            "gos_rt_error_chain" => {
                let e = self.tcx.dyn_error_ty();
                self.tcx.intern(gossamer_types::TyKind::Vec(e))
            }
            "gos_rt_error_with_field" => self.tcx.dyn_error_ty(),
            "gos_rt_error_field" => self.option_string_adt_ty(),
            "gos_rt_error_fields" => {
                let s = self.tcx.string_ty();
                let pair = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                self.tcx.intern(gossamer_types::TyKind::Vec(pair))
            }
            "gos_rt_arr_iter_next" => {
                // Recover element type from the iterator local's MIR
                // type (pinned to the original Vec<T> by `gos_rt_arr_iter`
                // dispatch) so `Some(s)` binds `s` with the right type.
                let mut iter_ty = self.locals[receiver_local.0 as usize].ty;
                while let TyKind::Ref { inner, .. } = self.tcx.kind_of(iter_ty) {
                    iter_ty = *inner;
                }
                let elem_opt = match self.tcx.kind_of(iter_ty) {
                    TyKind::Vec(e) | TyKind::Slice(e) => Some(*e),
                    TyKind::Array { elem, .. } => Some(*elem),
                    _ => None,
                };
                if let Some(elem) = elem_opt {
                    let substs = gossamer_types::Substs::from_types([elem]);
                    self.tcx.intern(gossamer_types::TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX - 1),
                        substs,
                    })
                } else if matches!(self.tcx.kind_of(ty), TyKind::Adt { .. }) {
                    ty
                } else {
                    self.option_adt_ty()
                }
            }
            "gos_rt_lazy_iter_next_i64" | "gos_rt_lazy_iter_next_pair_i64" => {
                // The slot the shim returns carries whatever the state yields,
                // so the payload takes the iterator's element type and `Some(s)`
                // binds `s` as that type rather than as the raw slot.
                let mut iter_ty = self.locals[receiver_local.0 as usize].ty;
                while let TyKind::Ref { inner, .. } = self.tcx.kind_of(iter_ty) {
                    iter_ty = *inner;
                }
                match self.tcx.kind_of(iter_ty) {
                    TyKind::Iterator(elem) | TyKind::Range(elem) => {
                        let substs = gossamer_types::Substs::from_types([*elem]);
                        self.tcx.intern(gossamer_types::TyKind::Adt {
                            def: gossamer_resolve::DefId::local(u32::MAX - 1),
                            substs,
                        })
                    }
                    _ => self.option_i64_adt_ty(),
                }
            }
            "gos_rt_sync_map_get" | "gos_rt_bufio_scanner_next" => self.option_string_adt_ty(),
            "gos_rt_sync_map_keys" => {
                let s = self.tcx.string_ty();
                self.tcx.intern(gossamer_types::TyKind::Vec(s))
            }
            "gos_rt_sync_map_len" => self.tcx.int_ty(gossamer_types::IntTy::I64),
            "gos_rt_sync_map_contains" => self.tcx.bool_ty(),
            "gos_rt_sync_map_set" | "gos_rt_sync_map_delete" => self.tcx.unit(),
            "gos_rt_http_request_send"
            | "gos_rt_http_client_request"
            | "gos_rt_http_client_request_bytes" => self.result_response_error_adt_ty(),
            "gos_rt_http_request_path_int" => self.option_i64_adt_ty(),
            "gos_rt_http_request_path_float" => self.option_f64_adt_ty(),
            "gos_rt_http_request_basic_auth" => self.option_pair_string_adt_ty(),
            "gos_rt_math_rng_next_f64" => self.tcx.float_ty(gossamer_types::FloatTy::F64),
            "gos_rt_field_error_path"
            | "gos_rt_field_error_message"
            | "gos_rt_field_error_code"
            | "gos_rt_validate_errors_get"
            | "gos_rt_validate_errors_collect"
            | "gos_rt_metrics_registry_render"
            | "gos_rt_trace_ended_to_otlp_json" => self.tcx.string_ty(),
            "gos_rt_validate_errors_is_empty"
            | "gos_rt_ctx_is_cancelled"
            | "gos_rt_atomic_bool_load"
            | "gos_rt_atomic_bool_cas"
            | "gos_rt_atomic_i64_cas"
            | "gos_rt_wg_wait_ctx"
            | "gos_rt_ctx_done" => self.tcx.bool_ty(),
            "gos_rt_ctx_cancelled" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                self.tcx.intern(gossamer_types::TyKind::Receiver(i))
            }
            "gos_rt_atomic_bool_store" => self.tcx.unit(),
            "gos_rt_validate_errors_len"
            | "gos_rt_validate_errors_count"
            | "gos_rt_rwlock_get"
            | "gos_rt_shared_get"
            | "gos_rt_metrics_counter_value"
            | "gos_rt_metrics_histogram_count" => self.tcx.int_ty(gossamer_types::IntTy::I64),
            "gos_rt_metrics_gauge_value" | "gos_rt_metrics_histogram_sum" => {
                self.tcx.float_ty(gossamer_types::FloatTy::F64)
            }
            "gos_rt_validate_errors_add"
            | "gos_rt_vec_reserve_at_least"
            | "gos_rt_vec_reserve_exact"
            | "gos_rt_rwlock_set"
            | "gos_rt_shared_set"
            | "gos_rt_ctx_cancel"
            | "gos_rt_metrics_counter_inc"
            | "gos_rt_metrics_gauge_set"
            | "gos_rt_metrics_gauge_inc"
            | "gos_rt_metrics_gauge_dec"
            | "gos_rt_metrics_histogram_observe"
            | "gos_rt_metrics_registry_register"
            | "gos_rt_trace_span_set_attribute"
            | "gos_rt_trace_span_set_status" => self.tcx.unit(),
            "gos_rt_bytes_builder_build"
            | "gos_rt_bytes_builder_as_str"
            | "gos_rt_bytes_buffer_to_string" => self.tcx.string_ty(),
            "gos_rt_bytes_builder_len" | "gos_rt_bytes_buffer_len" => {
                self.tcx.int_ty(gossamer_types::IntTy::I64)
            }
            "gos_rt_bytes_buffer_is_empty" => self.tcx.bool_ty(),
            "gos_rt_bytes_builder_write"
            | "gos_rt_bytes_builder_write_char"
            | "gos_rt_bytes_buffer_write_str"
            | "gos_rt_bytes_buffer_push"
            | "gos_rt_bytes_buffer_clear" => self.tcx.unit(),
            "gos_rt_tcp_stream_read_into" => {
                let count = self.tcx.int_ty(gossamer_types::IntTy::I64);
                self.result_of(count)
            }
            "gos_rt_tcp_listener_local_addr"
            | "gos_rt_tcp_stream_read_to_string"
            | "gos_rt_unix_stream_read_to_string"
            | "gos_rt_udp_local_addr" => self.result_string_error_adt_ty(),
            "gos_rt_tcp_stream_write"
            | "gos_rt_tcp_stream_set_read_timeout_ms"
            | "gos_rt_tcp_stream_set_write_timeout_ms"
            | "gos_rt_tcp_stream_set_nodelay"
            | "gos_rt_tcp_stream_clear_read_timeout"
            | "gos_rt_tcp_stream_clear_write_timeout"
            | "gos_rt_fs_file_create"
            | "gos_rt_fs_file_open"
            | "gos_rt_fs_open_options_open"
            | "gos_rt_fs_file_write"
            | "gos_rt_fs_file_write_bytes"
            | "gos_rt_fs_file_write_at"
            | "gos_rt_fs_file_seek"
            | "gos_rt_fs_file_len"
            | "gos_rt_fs_file_fd"
            | "gos_rt_fs_file_flush"
            | "gos_rt_unix_stream_write"
            | "gos_rt_udp_send_to"
            | "gos_rt_tcp_start_tls"
            | "gos_rt_tcp_start_tls_insecure"
            | "gos_rt_tcp_start_tls_ca" => self.result_i64_error_adt_ty(),
            "gos_rt_tcp_stream_read"
            | "gos_rt_unix_stream_read"
            | "gos_rt_fs_file_read"
            | "gos_rt_fs_file_read_at" => self.result_vec_u8_error_ty(),
            "gos_rt_fs_file_set_len"
            | "gos_rt_fs_file_sync_all"
            | "gos_rt_fs_file_sync_data"
            | "gos_rt_fs_file_unlock_range"
            | "gos_rt_fs_file_unlock"
            | "gos_rt_fs_sync_dir" => {
                let unit = self.tcx.unit();
                self.result_of(unit)
            }
            "gos_rt_fs_file_try_lock_range"
            | "gos_rt_fs_file_try_lock_shared"
            | "gos_rt_fs_file_try_lock_exclusive" => {
                let b = self.tcx.bool_ty();
                self.result_of(b)
            }
            "gos_rt_fs_file_read_to_string" => self.result_string_error_adt_ty(),
            "gos_rt_tcp_listener_accept" | "gos_rt_unix_listener_accept" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i, s]));
                self.result_of(tup)
            }
            "gos_rt_udp_recv_from" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let vec_u8 = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                let s = self.tcx.string_ty();
                let tup = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![vec_u8, s]));
                self.result_of(tup)
            }
            "gos_rt_tcp_listener_close"
            | "gos_rt_tcp_stream_close"
            | "gos_rt_fs_file_close"
            | "gos_rt_unix_listener_close"
            | "gos_rt_unix_stream_close"
            | "gos_rt_udp_close" => self.tcx.unit(),
            // A symbol the ABI declares as answering `I128` hands back the
            // two-word carrier, whose low word is the discriminant a `match`
            // over it reads. Typing the destination as the handle word every
            // other dispatch answers erases that discriminant, which leaves
            // `Ok` / `Some` unconditional and the other arm unreachable, so
            // the carrier the checker resolved stands wherever no row above
            // names a narrower shape.
            _ if self.is_result_or_option_adt(ty)
                && matches!(
                    gossamer_abi::registry::lookup(rt).map(|entry| entry.sig.ret),
                    Some(gossamer_abi::types::AbiType::I128)
                ) =>
            {
                ty
            }
            // A symbol the ABI declares as answering nothing gives the call
            // the unit value, whatever word the call site would otherwise hold.
            _ if matches!(
                gossamer_abi::registry::lookup(rt).map(|entry| entry.sig.ret),
                Some(gossamer_abi::types::AbiType::Void)
            ) =>
            {
                self.tcx.unit()
            }
            _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
        }
    }

    /// Tag a dispatched call's destination with its chained runtime kind.
    pub(super) fn dispatch_dest_kind(&self, rt: &'static str) -> Option<&'static str> {
        match rt {
            "gos_rt_http_client_get"
            | "gos_rt_http_client_post"
            | "gos_rt_http_client_put"
            | "gos_rt_http_client_options"
            | "gos_rt_http_client_delete"
            | "gos_rt_http_client_head" => Some("http::Request"),
            "gos_rt_http_request_header"
            | "gos_rt_http_request_body"
            | "gos_rt_http_request_set_value" => Some("http::Request"),
            "gos_rt_http_client_builder_max_redirects"
            | "gos_rt_http_client_builder_timeout_ms"
            | "gos_rt_http_client_builder_cookie_jar"
            | "gos_rt_http_client_builder_proxy" => Some("http::ClientBuilder"),
            "gos_rt_http_client_builder_build" => Some("http::Client"),
            // Every `Server` setter answers the server, so a `|>` chain of
            // them keeps dispatching on `http::Server`.
            "gos_rt_http_server_read_header_timeout_ms"
            | "gos_rt_http_server_read_body_timeout_ms"
            | "gos_rt_http_server_write_timeout_ms"
            | "gos_rt_http_server_idle_timeout_ms"
            | "gos_rt_http_server_max_header_bytes"
            | "gos_rt_http_server_max_body_bytes"
            | "gos_rt_http_server_max_connections"
            | "gos_rt_http_server_request_timeout_ms"
            | "gos_rt_http_server_server_name" => Some("http::Server"),
            "gos_rt_http_response_with_header" => Some("http::Response"),
            "gos_rt_flag_set_string" => Some("flag::Cell::String"),
            "gos_rt_flag_set_int" => Some("flag::Cell::Int"),
            "gos_rt_flag_set_uint" => Some("flag::Cell::Uint"),
            "gos_rt_flag_set_float" => Some("flag::Cell::Float"),
            "gos_rt_flag_set_bool" => Some("flag::Cell::Bool"),
            "gos_rt_flag_set_duration" => Some("flag::Cell::Duration"),
            "gos_rt_flag_set_string_list" => Some("flag::Cell::StringList"),
            "gos_rt_set_union"
            | "gos_rt_set_intersection"
            | "gos_rt_set_difference"
            | "gos_rt_set_symmetric_difference" => Some("collections::HashSet"),
            "gos_rt_tcp_listener_accept" => Some("net::accept_pair"),
            "gos_rt_unix_listener_accept" => Some("net::unix_accept_pair"),
            "gos_rt_fs_file_create" | "gos_rt_fs_file_open" | "gos_rt_fs_open_options_open" => {
                Some("fs::File")
            }
            "gos_rt_fs_open_options_new"
            | "gos_rt_fs_open_options_read"
            | "gos_rt_fs_open_options_write"
            | "gos_rt_fs_open_options_append"
            | "gos_rt_fs_open_options_truncate"
            | "gos_rt_fs_open_options_create"
            | "gos_rt_fs_open_options_create_new" => Some("fs::OpenOptions"),
            "gos_rt_tcp_start_tls"
            | "gos_rt_tcp_start_tls_insecure"
            | "gos_rt_tcp_start_tls_ca" => Some("net::TcpStream"),
            "gos_rt_trace_tracer_start_span" => Some("trace::Span"),
            "gos_rt_trace_span_end" => Some("trace::EndedSpan"),
            // Router verb methods return the router pointer so |> chaining works.
            "gos_rt_router_get"
            | "gos_rt_router_post"
            | "gos_rt_router_put"
            | "gos_rt_router_delete"
            | "gos_rt_router_patch"
            | "gos_rt_router_head"
            | "gos_rt_router_options"
            | "gos_rt_router_get_fn"
            | "gos_rt_router_post_fn"
            | "gos_rt_router_put_fn"
            | "gos_rt_router_delete_fn"
            | "gos_rt_router_patch_fn"
            | "gos_rt_router_head_fn"
            | "gos_rt_router_options_fn" => Some("http::Router"),
            _ => None,
        }
    }

    /// `is_some` / `is_ok` / `is_none` / `is_err` on a lowered receiver.
    pub(super) fn lower_option_result_predicate(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        receiver_kind_flat: &TyKind,
        receiver_ty: Ty,
        span: Span,
    ) -> MethodLowering {
        let receiver_kind_flat = receiver_kind_flat.clone();
        if let name @ ("is_some" | "is_ok" | "is_none" | "is_err") = method.name.as_str() {
            let Some(receiver_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let lowered_ty = self.locals[receiver_local.0 as usize].ty;
            let lowered_is_result = matches!(self.tcx.kind_of(lowered_ty), TyKind::Adt { .. })
                && self.is_result_or_option_adt(lowered_ty);
            let recv_is_result = matches!(&receiver_kind_flat, TyKind::Adt { .. })
                && self.is_result_or_option_adt(receiver_ty);
            let bool_ty = self.tcx.bool_ty();
            if lowered_is_result || recv_is_result {
                let helper = match name {
                    "is_some" | "is_ok" => "gos_rt_result_is_ok",
                    _ => "gos_rt_result_is_err",
                };
                let dest = self.fresh(bool_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(receiver_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return MethodLowering::Handled(Some(dest));
            }
            // Legacy: receiver is the inner value with a
            // null/zero sentinel for the missing case.
            let constant = matches!(name, "is_some" | "is_ok");
            let dest = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Const(ConstValue::Bool(constant))),
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// Lowered-receiver-runtime-kind dispatch table.
    pub(super) fn lowered_kind_dispatch_symbol(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        heap_reverse_i64: bool,
        heap_float_elem: bool,
    ) -> Option<&'static str> {
        self.lowered_kind_dispatch_symbol_a(
            rk,
            method,
            args,
            receiver_ty,
            heap_reverse_i64,
            heap_float_elem,
        )
        .or_else(|| {
            self.lowered_kind_dispatch_symbol_b(
                rk,
                method,
                args,
                receiver_ty,
                heap_reverse_i64,
                heap_float_elem,
            )
        })
    }

    /// First half of the lowered-receiver-runtime-kind dispatch table.
    pub(super) fn lowered_kind_dispatch_symbol_a(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        _args: &[HirExpr],
        receiver_ty: Ty,
        heap_reverse_i64: bool,
        heap_float_elem: bool,
    ) -> Option<&'static str> {
        if matches!(
            rk,
            Some("collections::BinaryHeap" | "collections::MaxHeap" | "collections::MinHeap")
        ) {
            return self.binary_heap_runtime_symbol(
                rk,
                receiver_ty,
                method,
                heap_reverse_i64,
                heap_float_elem,
            );
        }
        match (rk, method.name.as_str()) {
            (Some("flag::Set"), "string") => Some("gos_rt_flag_set_string"),
            (Some("flag::Set"), "int") => Some("gos_rt_flag_set_int"),
            (Some("flag::Set"), "uint") => Some("gos_rt_flag_set_uint"),
            (Some("flag::Set"), "float") => Some("gos_rt_flag_set_float"),
            (Some("flag::Set"), "bool") => Some("gos_rt_flag_set_bool"),
            (Some("flag::Set"), "duration") => Some("gos_rt_flag_set_duration"),
            (Some("flag::Set"), "string_list") => Some("gos_rt_flag_set_string_list"),
            (Some("flag::Set"), "short") => Some("gos_rt_flag_set_short"),
            (Some("flag::Set"), "usage") => Some("gos_rt_flag_set_usage"),
            (Some("flag::Set"), "parse") => Some("gos_rt_flag_set_parse"),
            // 0.4.0 stateful HTTP types - method-call dispatch.
            (Some("http::Router"), "add") => Some("gos_rt_router_add"),
            (Some("http::Router"), "get") => Some("gos_rt_router_get"),
            (Some("http::Router"), "post") => Some("gos_rt_router_post"),
            (Some("http::Router"), "put") => Some("gos_rt_router_put"),
            (Some("http::Router"), "delete") => Some("gos_rt_router_delete"),
            (Some("http::Router"), "patch") => Some("gos_rt_router_patch"),
            (Some("http::Router"), "head") => Some("gos_rt_router_head"),
            (Some("http::Router"), "options") => Some("gos_rt_router_options"),
            (Some("http::Router"), "serve") => Some("gos_rt_router_serve"),
            (Some("http::FileServer"), "serve") => Some("gos_rt_file_server_serve"),
            (Some("http::NativeClient"), "get") => Some("gos_rt_native_client_get"),
            (Some("http::Proxy"), "forward") => Some("gos_rt_proxy_forward"),
            (Some("http::Client"), "get") => Some("gos_rt_http_client_get"),
            (Some("http::Client"), "post") => Some("gos_rt_http_client_post"),
            (Some("http::Client"), "put") => Some("gos_rt_http_client_put"),
            (Some("http::Client"), "options") => Some("gos_rt_http_client_options"),
            (Some("http::Client"), "delete") => Some("gos_rt_http_client_delete"),
            (Some("http::Client"), "head") => Some("gos_rt_http_client_head"),
            (Some("http::Client"), "request") => Some("gos_rt_http_client_request"),
            (Some("http::Client"), "request_bytes") => Some("gos_rt_http_client_request_bytes"),
            (Some("http::ClientBuilder"), "max_redirects") => {
                Some("gos_rt_http_client_builder_max_redirects")
            }
            (Some("http::ClientBuilder"), "timeout_ms") => {
                Some("gos_rt_http_client_builder_timeout_ms")
            }
            (Some("http::ClientBuilder"), "cookie_jar") => {
                Some("gos_rt_http_client_builder_cookie_jar")
            }
            (Some("http::ClientBuilder"), "proxy") => Some("gos_rt_http_client_builder_proxy"),
            (Some("http::ClientBuilder"), "build") => Some("gos_rt_http_client_builder_build"),
            (Some("http::Request"), "header") => Some("gos_rt_http_request_header"),
            (Some("http::Request"), "body") => Some("gos_rt_http_request_body"),
            (Some("http::Request"), "send") => Some("gos_rt_http_request_send"),
            (Some("http::Request"), "path") => Some("gos_rt_http_request_path"),
            (Some("http::Request"), "path_value") => Some("gos_rt_http_request_path_value"),
            (Some("http::Request"), "path_int") => Some("gos_rt_http_request_path_int"),
            (Some("http::Request"), "path_float") => Some("gos_rt_http_request_path_float"),
            (Some("http::Request"), "method") => Some("gos_rt_http_request_method"),
            (Some("http::Request"), "value") => Some("gos_rt_http_request_value"),
            (Some("http::Request"), "set_value") => Some("gos_rt_http_request_set_value"),
            (Some("http::Request"), "form_value") => Some("gos_rt_http_request_form_value"),
            (Some("http::Request"), "basic_auth") => Some("gos_rt_http_request_basic_auth"),
            (Some("http::Response"), "with_header") => Some("gos_rt_http_response_with_header"),
            (Some("http::Response"), "status") => Some("gos_rt_http_response_status"),
            (Some("http::Response"), "body") => Some("gos_rt_http_response_body"),
            (Some("bufio::Scanner"), "scan") => Some("gos_rt_bufio_scanner_scan"),
            (Some("bufio::Scanner"), "text") => Some("gos_rt_bufio_scanner_text"),
            (Some("bufio::Scanner"), "next") => Some("gos_rt_bufio_scanner_next"),
            (Some("errors::Error"), "message") => Some("gos_rt_error_message"),
            // `{}` on an error renders the colon-joined chain, and
            // `to_string` is the same contract by another spelling.
            (Some("errors::Error"), "to_string") => Some("gos_rt_error_display"),
            (Some("errors::Error"), "cause") => Some("gos_rt_error_cause"),
            (Some("errors::Error"), "is") => Some("gos_rt_error_is"),
            (Some("errors::Error"), "chain") => Some("gos_rt_error_chain"),
            (Some("errors::Error"), "with_field") => Some("gos_rt_error_with_field"),
            (Some("errors::Error"), "field") => Some("gos_rt_error_field"),
            (Some("errors::Error"), "fields") => Some("gos_rt_error_fields"),
            (Some("regex::Pattern"), "is_match") => Some("gos_rt_regex_is_match"),
            (Some("regex::Pattern"), "count") => Some("gos_rt_regex_count"),
            (Some("regex::Pattern"), "find") => Some("gos_rt_regex_find"),
            (Some("regex::Pattern"), "find_all") => Some("gos_rt_regex_find_all"),
            (Some("regex::Pattern"), "replace") => Some("gos_rt_regex_replace"),
            (Some("regex::Pattern"), "replace_all") => Some("gos_rt_regex_replace_all"),
            (Some("regex::Pattern"), "split") => Some("gos_rt_regex_split"),
            (Some("collections::HashSet" | "collections::BTreeSet"), "insert") => {
                Some("gos_rt_set_insert")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "contains") => {
                Some("gos_rt_set_contains")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "remove") => {
                Some("gos_rt_set_remove")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "len") => {
                Some("gos_rt_set_len")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_empty") => {
                Some("gos_rt_set_is_empty")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "union") => {
                Some("gos_rt_set_union")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "intersection") => {
                Some("gos_rt_set_intersection")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "difference") => {
                Some("gos_rt_set_difference")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "symmetric_difference") => {
                Some("gos_rt_set_symmetric_difference")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_subset") => {
                Some("gos_rt_set_is_subset")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_superset") => {
                Some("gos_rt_set_is_superset")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_disjoint") => {
                Some("gos_rt_set_is_disjoint")
            }
            (Some("collections::VecDeque"), "push_back") => Some("gos_rt_deque_push_back"),
            (Some("collections::VecDeque"), "push_front") => Some("gos_rt_deque_push_front"),
            (Some("collections::VecDeque"), "pop_front") => Some("gos_rt_deque_pop_front"),
            (Some("collections::VecDeque"), "pop_back") => Some("gos_rt_deque_pop_back"),
            (Some("collections::VecDeque"), "peek_front") => Some("gos_rt_deque_peek_front"),
            (Some("collections::VecDeque"), "peek_back") => Some("gos_rt_deque_peek_back"),
            (Some("collections::VecDeque"), "len") => Some("gos_rt_deque_len"),
            (Some("collections::VecDeque"), "is_empty") => Some("gos_rt_deque_is_empty"),
            (Some("collections::VecDeque"), "clear") => Some("gos_rt_deque_clear"),
            (Some("collections::VecQueue"), "push") => Some("gos_rt_deque_push_back"),
            (Some("collections::VecQueue"), "pop") => Some("gos_rt_deque_pop_front"),
            (Some("collections::VecQueue"), "peek") => Some("gos_rt_deque_peek_front"),
            (Some("collections::VecQueue"), "len") => Some("gos_rt_deque_len"),
            (Some("collections::VecQueue"), "is_empty") => Some("gos_rt_deque_is_empty"),
            (Some("collections::VecQueue"), "clear") => Some("gos_rt_deque_clear"),
            (Some("collections::VecStack"), "push") => Some("gos_rt_deque_push_back"),
            (Some("collections::VecStack"), "pop") => Some("gos_rt_deque_pop_back"),
            (Some("collections::VecStack"), "peek") => Some("gos_rt_deque_peek_back"),
            (Some("collections::VecStack"), "len") => Some("gos_rt_deque_len"),
            (Some("collections::VecStack"), "is_empty") => Some("gos_rt_deque_is_empty"),
            (Some("collections::VecStack"), "clear") => Some("gos_rt_deque_clear"),
            _ => None,
        }
    }

    /// Second half of the lowered-receiver-runtime-kind dispatch table.
    pub(super) fn lowered_kind_dispatch_symbol_b(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        _heap_reverse_i64: bool,
        _heap_float_elem: bool,
    ) -> Option<&'static str> {
        let _ = receiver_ty;
        if let Some(symbol) =
            rk.and_then(|kind| super::types::sync_method_symbol(kind, method.name.as_str()))
        {
            return Some(symbol);
        }
        match (rk, method.name.as_str()) {
            (Some("sync::Map"), "insert") => Some("gos_rt_sync_map_set"),
            (Some("sync::Map"), "get") => Some("gos_rt_sync_map_get"),
            (Some("sync::Map"), "remove") => Some("gos_rt_sync_map_delete"),
            (Some("sync::Map"), "len") => Some("gos_rt_sync_map_len"),
            (Some("sync::Map"), "contains_key") => Some("gos_rt_sync_map_contains"),
            (Some("sync::Map"), "keys") => Some("gos_rt_sync_map_keys"),
            (Some("math::rand::Rng"), "next_u64") => Some("gos_rt_math_rng_next_u64"),
            (Some("math::rand::Rng"), "next_u32") => Some("gos_rt_math_rng_next_u32"),
            (Some("math::rand::Rng"), "range_u64") => Some("gos_rt_math_rng_range_u64"),
            (Some("math::rand::Rng"), "next_f64") => Some("gos_rt_math_rng_next_f64"),
            (Some("validate::FieldError"), "path") => Some("gos_rt_field_error_path"),
            (Some("validate::FieldError"), "message") => Some("gos_rt_field_error_message"),
            (Some("validate::FieldError"), "code") => Some("gos_rt_field_error_code"),
            (Some("validate::Errors"), "add") => Some("gos_rt_validate_errors_add"),
            (Some("validate::Errors"), "is_empty") => Some("gos_rt_validate_errors_is_empty"),
            (Some("validate::Errors"), "len") => Some("gos_rt_validate_errors_len"),
            (Some("validate::Errors"), "count") => Some("gos_rt_validate_errors_count"),
            (Some("validate::Errors"), "get") => Some("gos_rt_validate_errors_get"),
            (Some("validate::Errors"), "collect") => Some("gos_rt_validate_errors_collect"),
            (Some("sync::RwLock"), "read") => Some("gos_rt_rwlock_get"),
            (Some("sync::RwLock"), "write") => Some("gos_rt_rwlock_set"),
            (Some("sync::Shared"), "get") => Some("gos_rt_shared_get"),
            (Some("sync::Shared"), "set") => Some("gos_rt_shared_set"),
            (Some("sync::AtomicBool"), "load") => Some("gos_rt_atomic_bool_load"),
            (Some("sync::AtomicBool"), "store") => Some("gos_rt_atomic_bool_store"),
            (Some("sync::AtomicBool"), "compare_exchange") => Some("gos_rt_atomic_bool_cas"),
            (Some("context::Context"), "is_cancelled") => Some("gos_rt_ctx_is_cancelled"),
            (Some("context::Context"), "cancel") => Some("gos_rt_ctx_cancel"),
            (Some("context::Context"), "done") => Some("gos_rt_ctx_done"),
            (Some("context::Context"), "done_chan") => Some("gos_rt_ctx_cancelled"),
            (Some("metrics::Counter"), "inc") => Some("gos_rt_metrics_counter_inc"),
            (Some("metrics::Counter"), "value") => Some("gos_rt_metrics_counter_value"),
            (Some("metrics::Gauge"), "set") => Some("gos_rt_metrics_gauge_set"),
            (Some("metrics::Gauge"), "inc") => Some("gos_rt_metrics_gauge_inc"),
            (Some("metrics::Gauge"), "dec") => Some("gos_rt_metrics_gauge_dec"),
            (Some("metrics::Gauge"), "value") => Some("gos_rt_metrics_gauge_value"),
            (Some("metrics::Histogram"), "observe") => Some("gos_rt_metrics_histogram_observe"),
            (Some("metrics::Histogram"), "sum") => Some("gos_rt_metrics_histogram_sum"),
            (Some("metrics::Histogram"), "count") => Some("gos_rt_metrics_histogram_count"),
            (Some("metrics::Registry"), "register") => Some("gos_rt_metrics_registry_register"),
            (Some("metrics::Registry"), "render") => Some("gos_rt_metrics_registry_render"),
            (Some("trace::Tracer"), "start_span") => Some("gos_rt_trace_tracer_start_span"),
            (Some("trace::Span"), "set_attribute") => Some("gos_rt_trace_span_set_attribute"),
            (Some("trace::Span"), "set_status") => Some("gos_rt_trace_span_set_status"),
            (Some("trace::Span"), "end") => Some("gos_rt_trace_span_end"),
            (Some("trace::EndedSpan"), "to_otlp_json") => Some("gos_rt_trace_ended_to_otlp_json"),
            (Some("bytes::Builder"), "write") => Some("gos_rt_bytes_builder_write"),
            (Some("bytes::Builder"), "write_char") => Some("gos_rt_bytes_builder_write_char"),
            (Some("bytes::Builder"), "build") => Some("gos_rt_bytes_builder_build"),
            (Some("bytes::Builder"), "as_str") => Some("gos_rt_bytes_builder_as_str"),
            (Some("bytes::Builder"), "len") => Some("gos_rt_bytes_builder_len"),
            (Some("bytes::Buffer"), "write_str") => Some("gos_rt_bytes_buffer_write_str"),
            (Some("bytes::Buffer"), "push") => Some("gos_rt_bytes_buffer_push"),
            (Some("bytes::Buffer"), "len") => Some("gos_rt_bytes_buffer_len"),
            (Some("bytes::Buffer"), "is_empty") => Some("gos_rt_bytes_buffer_is_empty"),
            (Some("bytes::Buffer"), "clear") => Some("gos_rt_bytes_buffer_clear"),
            (Some("bytes::Buffer"), "to_string") => Some("gos_rt_bytes_buffer_to_string"),
            (Some("net::TcpListener"), "accept") => Some("gos_rt_tcp_listener_accept"),
            (Some("net::TcpListener"), "local_addr") => Some("gos_rt_tcp_listener_local_addr"),
            (Some("net::TcpListener"), "close") => Some("gos_rt_tcp_listener_close"),
            (Some("net::TcpStream"), "read") => Some("gos_rt_tcp_stream_read"),
            (Some("net::TcpStream"), "read_into") => Some("gos_rt_tcp_stream_read_into"),
            (Some("net::TcpStream"), "read_to_string") => Some("gos_rt_tcp_stream_read_to_string"),
            (Some("net::TcpStream"), "write" | "write_all") => Some("gos_rt_tcp_stream_write"),
            (Some("net::TcpStream"), "set_read_timeout_ms") => {
                Some("gos_rt_tcp_stream_set_read_timeout_ms")
            }
            (Some("net::TcpStream"), "set_write_timeout_ms") => {
                Some("gos_rt_tcp_stream_set_write_timeout_ms")
            }
            (Some("net::TcpStream"), "set_nodelay") => Some("gos_rt_tcp_stream_set_nodelay"),
            (Some("net::TcpStream"), "clear_read_timeout") => {
                Some("gos_rt_tcp_stream_clear_read_timeout")
            }
            (Some("net::TcpStream"), "clear_write_timeout") => {
                Some("gos_rt_tcp_stream_clear_write_timeout")
            }
            (Some("net::TcpStream"), "start_tls") => Some("gos_rt_tcp_start_tls"),
            (Some("net::TcpStream"), "start_tls_insecure") => Some("gos_rt_tcp_start_tls_insecure"),
            (Some("net::TcpStream"), "start_tls_ca") => Some("gos_rt_tcp_start_tls_ca"),
            (Some("net::TcpStream"), "peer_certificate") => Some("gos_rt_tcp_tls_peer_cert"),
            (Some("net::TcpStream"), "close") => Some("gos_rt_tcp_stream_close"),
            (Some("fs::File"), "read") => Some("gos_rt_fs_file_read"),
            (Some("fs::File"), "read_to_string") => Some("gos_rt_fs_file_read_to_string"),
            (Some("fs::File"), "write" | "write_all") => Some("gos_rt_fs_file_write"),
            (Some("fs::File"), "write_bytes") => Some("gos_rt_fs_file_write_bytes"),
            (Some("fs::File"), "read_at") => Some("gos_rt_fs_file_read_at"),
            (Some("fs::File"), "read_at_into") => Some("gos_rt_fs_file_read_at_into"),
            (Some("fs::File"), "write_at") => Some("gos_rt_fs_file_write_at"),
            (Some("fs::File"), "seek") => Some("gos_rt_fs_file_seek"),
            (Some("fs::File"), "set_len") => Some("gos_rt_fs_file_set_len"),
            (Some("fs::File"), "len") => Some("gos_rt_fs_file_len"),
            (Some("fs::File"), "fd") => Some("gos_rt_fs_file_fd"),
            (Some("fs::File"), "sync_all") => Some("gos_rt_fs_file_sync_all"),
            (Some("fs::File"), "sync_data") => Some("gos_rt_fs_file_sync_data"),
            (Some("fs::File"), "try_lock_range") => Some("gos_rt_fs_file_try_lock_range"),
            (Some("fs::File"), "unlock_range") => Some("gos_rt_fs_file_unlock_range"),
            (Some("fs::File"), "try_lock_shared") => Some("gos_rt_fs_file_try_lock_shared"),
            (Some("fs::File"), "try_lock_exclusive") => Some("gos_rt_fs_file_try_lock_exclusive"),
            (Some("fs::File"), "unlock") => Some("gos_rt_fs_file_unlock"),
            (Some("fs::File"), "flush") => Some("gos_rt_fs_file_flush"),
            (Some("fs::File"), "close") => Some("gos_rt_fs_file_close"),
            (Some("fs::OpenOptions"), "read") => Some("gos_rt_fs_open_options_read"),
            (Some("fs::OpenOptions"), "write") => Some("gos_rt_fs_open_options_write"),
            (Some("fs::OpenOptions"), "append") => Some("gos_rt_fs_open_options_append"),
            (Some("fs::OpenOptions"), "truncate") => Some("gos_rt_fs_open_options_truncate"),
            (Some("fs::OpenOptions"), "create") => Some("gos_rt_fs_open_options_create"),
            (Some("fs::OpenOptions"), "create_new") => Some("gos_rt_fs_open_options_create_new"),
            (Some("fs::OpenOptions"), "open") => Some("gos_rt_fs_open_options_open"),
            (Some("net::UnixListener"), "accept") => Some("gos_rt_unix_listener_accept"),
            (Some("net::UnixListener"), "close") => Some("gos_rt_unix_listener_close"),
            (Some("net::UnixStream"), "read") => Some("gos_rt_unix_stream_read"),
            (Some("net::UnixStream"), "read_to_string") => {
                Some("gos_rt_unix_stream_read_to_string")
            }
            (Some("net::UnixStream"), "write" | "write_all") => Some("gos_rt_unix_stream_write"),
            (Some("net::UnixStream"), "close") => Some("gos_rt_unix_stream_close"),
            (Some("net::UdpSocket"), "send_to") => Some("gos_rt_udp_send_to"),
            (Some("net::UdpSocket"), "recv_from") => Some("gos_rt_udp_recv_from"),
            (Some("net::UdpSocket"), "local_addr") => Some("gos_rt_udp_local_addr"),
            (Some("net::UdpSocket"), "close") => Some("gos_rt_udp_close"),
            (Some("process::Child"), "write_stdin") => Some("gos_rt_child_write_stdin"),
            (Some("process::Child"), "close_stdin") => Some("gos_rt_child_close_stdin"),
            (Some("process::Child"), "read_line") => Some("gos_rt_child_read_line"),
            (Some("process::Child"), "read_stdout") => Some("gos_rt_child_read_stdout"),
            (Some("process::Child"), "wait") => Some("gos_rt_child_wait"),
            (Some("process::Child"), "kill") => Some("gos_rt_child_kill"),
            (Some("io::Stream"), "write_byte") => Some("gos_rt_stream_write_byte"),
            (Some("io::Stream"), "write_byte_array" | "write_bytes") => {
                Some("gos_rt_stream_write_byte_array")
            }
            (Some("io::Stream"), "write" | "write_str") => Some("gos_rt_stream_write_str"),
            (Some("io::Stream"), "flush") => Some("gos_rt_stream_flush"),
            (Some("io::Stream"), "read_line") => Some(if args.is_empty() {
                "gos_rt_stream_next_line"
            } else {
                "gos_rt_stream_read_line"
            }),
            (Some("io::Stream"), "read_to_string") => Some("gos_rt_stream_read_to_string"),
            (Some("signal::Notifier"), "wait") => Some("gos_rt_signal_wait"),
            (Some("signal::Notifier"), "try_wait") => Some("gos_rt_signal_try_wait"),
            (Some("signal::Notifier"), "stop") => Some("gos_rt_signal_stop"),
            (Some("vec::Iter"), "next") => Some("gos_rt_arr_iter_next"),
            _ => None,
        }
    }

    /// Lower a lowered-receiver runtime-kind dispatched call.
    pub(super) fn lower_lowered_kind_dispatch_call(
        &mut self,
        rt: &'static str,
        receiver: &HirExpr,
        receiver_local: Local,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let (arg_operands, rt) =
            self.lowered_kind_dispatch_arg_operands(rt, receiver_local, args, span)?;
        let pinned = self.dispatch_pinned_ty(rt, receiver, receiver_local, ty);
        let dest = self.fresh(pinned);
        if let Some(k) = self.dispatch_dest_kind(rt) {
            let k = if k == "collections::HashSet"
                && self.runtime_kind_from_ty(receiver.ty) == Some("collections::BTreeSet")
            {
                "collections::BTreeSet"
            } else {
                k
            };
            self.local_runtime_kind.insert(dest, k);
        }
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(rt.to_string())),
            args: arg_operands,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// Build the argument operand list for a post-lowering kind-dispatched call.
    pub(super) fn lowered_kind_dispatch_arg_operands(
        &mut self,
        rt: &'static str,
        receiver_local: Local,
        args: &[HirExpr],
        span: Span,
    ) -> Option<(Vec<Operand>, &'static str)> {
        let mut arg_operands = Vec::with_capacity(args.len() + 1);
        arg_operands.push(Operand::Copy(Place::local(receiver_local)));
        // Router HTTP-verb methods take (router, pattern,
        // env, fn_addr) - synthesize the handler's env+fn_addr
        // from the last user argument (must be a struct whose
        // impl Handler { fn serve(...) }).
        let router_handler_method = matches!(
            rt,
            "gos_rt_router_get"
                | "gos_rt_router_post"
                | "gos_rt_router_put"
                | "gos_rt_router_delete"
                | "gos_rt_router_patch"
                | "gos_rt_router_head"
                | "gos_rt_router_options"
                | "gos_rt_router_add"
        );
        let mut rt = rt;
        if router_handler_method && !args.is_empty() {
            let handler_idx = args.len() - 1;
            // Lower non-handler args (method-name for add,
            // pattern for verb methods).
            for arg in &args[..handler_idx] {
                let a = self.lower_expr(arg)?;
                let a = self.auto_deref_cell(a, span);
                arg_operands.push(Operand::Copy(Place::local(a)));
            }
            let handler_local = self.lower_expr(&args[handler_idx])?;
            match self.emit_router_handler_abi(handler_local, span) {
                RouterHandlerAbi::Bare(fn_addr) => {
                    arg_operands.push(fn_addr);
                    if let Some(bare_rt) = Self::router_bare_variant(rt) {
                        rt = bare_rt;
                    }
                }
                RouterHandlerAbi::WithEnv { env, fn_addr } => {
                    arg_operands.push(env);
                    arg_operands.push(fn_addr);
                }
            }
        } else {
            // `Client::request` / `request_bytes` take Vec-shaped
            // body/header args; coerce `[a, b]` array literals to
            // the heap GosVec shape the runtime ABI expects (same
            // treatment as the free `http::request` lowering).
            let coerce_vec_args = matches!(
                rt,
                "gos_rt_http_client_request"
                    | "gos_rt_http_client_request_bytes"
                    | "gos_rt_tcp_stream_write"
                    | "gos_rt_unix_stream_write"
                    | "gos_rt_udp_send_to"
                    | "gos_rt_flag_set_parse"
            );
            for arg in args {
                let a = self.lower_expr(arg)?;
                let a = self.auto_deref_cell(a, span);
                let mut a = if coerce_vec_args {
                    let lt = self.locals[a.0 as usize].ty;
                    if let TyKind::Array { elem, len } = self.tcx.kind_of(lt).clone() {
                        self.coerce_array_to_vec(a, elem, len, span)
                    } else {
                        a
                    }
                } else {
                    a
                };
                if rt == "gos_rt_chan_send"
                    && matches!(
                        self.tcx.kind_of(self.locals[a.0 as usize].ty),
                        TyKind::Vec(_)
                            | TyKind::Adt { .. }
                            | TyKind::Tuple(_)
                            | TyKind::Array { .. }
                    )
                    && matches!(
                        &arg.kind,
                        HirExprKind::Path { .. }
                            | HirExprKind::Field { .. }
                            | HirExprKind::TupleIndex { .. }
                            | HirExprKind::Index { .. }
                    )
                {
                    let cloned = self.fresh(self.locals[a.0 as usize].ty);
                    self.emit_owned_clone_binding(a, cloned, span);
                    a = cloned;
                }
                arg_operands.push(Operand::Copy(Place::local(a)));
            }
        }
        Some((arg_operands, rt))
    }

    /// Runtime-symbol fallback: refine the symbol then dispatch / emit.
    pub(super) fn lower_method_call_fallback(
        &mut self,
        receiver: &HirExpr,
        receiver_local: Local,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
        runtime_symbol: Option<&'static str>,
        receiver_ty: Ty,
        owner: Option<&Ident>,
    ) -> Option<Local> {
        // An iterator whose lowering materialised its elements (a set's
        // `iter()`, a sorted walk) advances through a cursor over them, the
        // state `next` steps.
        let materialised_walk = method.name == "next"
            && args.is_empty()
            && matches!(self.tcx.kind_of(receiver.ty), TyKind::Iterator(_))
            && matches!(
                self.tcx.kind_of(self.locals[receiver_local.0 as usize].ty),
                TyKind::Vec(_) | TyKind::Slice(_)
            );
        let cursor = if materialised_walk {
            self.entries_cursor(receiver_local, span)
        } else {
            None
        };
        // An element no cursor carries is read in place: `next` consumes its
        // iterator (GT0042), so the one pull a walk takes is its first element.
        let runtime_symbol = if materialised_walk && cursor.is_none() {
            Some("gos_rt_vec_first")
        } else {
            runtime_symbol
        };
        let receiver_local = cursor.unwrap_or(receiver_local);
        // The `impl` the checker resolved names the parameters; a method a
        // program declares on a built-in type (`impl i64`, `impl<T> Vec<T>`)
        // has no struct name to find them by.
        let method_inputs = [
            owner.map(|owner| owner.name.clone()),
            self.struct_name_of(receiver_ty),
            self.struct_name_from_expr(receiver),
            self.builtin_impl_owner_name(receiver_ty),
        ]
        .into_iter()
        .flatten()
        .find_map(|name| {
            self.impl_method_inputs
                .get(&format!("{name}::{}", method.name))
        })
        .map(|inputs| inputs.get(1..).unwrap_or_default());
        let (receiver_local, mut arg_operands) = self.build_fallback_arg_operands(
            runtime_symbol,
            receiver_local,
            receiver,
            args,
            method_inputs,
            span,
        )?;
        let runtime_symbol =
            self.rewrite_result_map_closure_arg(runtime_symbol, &mut arg_operands, span);
        // Re-check the dispatch for Result/Option methods now that
        // the receiver has been lowered. The HIR-side `receiver_ty`
        // is often a `Var` for chained method calls (e.g.
        // `s.to_i64().unwrap_or(...)`), so the table at the top
        // selected `Some("")` (identity) without seeing that the
        // pinned local type is in fact a Result/Option Adt. Without
        // this fix-up `.unwrap_or(default)` returns the aggregate
        // pointer instead of the inner payload.
        let lowered_recv_ty = self.locals[receiver_local.0 as usize].ty;
        let lowered_is_result = matches!(self.tcx.kind_of(lowered_recv_ty), TyKind::Adt { .. })
            && self.is_result_or_option_adt(lowered_recv_ty);
        // Inverse of the lowered_is_result fix-up above: if the HIR
        // typechecker thought the receiver was a Result/Option Adt
        // (because the call site chained `.unwrap_or(...)` /
        // `.unwrap()` / `.ok()` / `.err()`) but the lowered MIR type
        // is a real scalar - `json::as_i64(v).unwrap_or(0)` is the
        // canonical case, where `gos_rt_json_as_i64` returns a raw
        // `i64` - fall back to identity. The runtime helpers picked
        // by the original dispatch (`gos_rt_result_unwrap_or` etc.)
        // would treat the i64 as a `*mut GosResult` pointer, read
        // garbage as the `disc`, and return the receiver itself
        // bit-cast as the inner value. The askq tool-call accumulator
        // hit exactly this: every `idx` it computed for a tool_call's
        // `index` field was a multi-trillion garbage number, and the
        // ensuing `while (tc_ids.len() as i64) <= idx` push loop
        // grew the vec to 100+ empty slots before the `[idx] = s`
        // write hit a stale pointer.
        let lowered_kind = self.tcx.kind_of(lowered_recv_ty);
        let lowered_is_scalar = matches!(
            lowered_kind,
            TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_) | TyKind::String
        );
        // Inverse fix-up: when the lowered receiver is a real scalar
        // (the typechecker thought `json::as_str(v)` returned
        // `Option<&str>` but the runtime helper hands back a raw
        // `*c_char`), force the dispatch to identity so the
        // `gos_rt_result_*` helpers don't dereference the scalar
        // value as a `*mut GosResult`. The askq tool-call name
        // corruption (`json_escape ← strlen_evex`) was the canonical
        // case - see
        // ~/dev/contexts/lang/fix_architecture_ownership.md.
        let mut runtime_symbol = if lowered_is_scalar
            && matches!(
                method.name.as_str(),
                "unwrap" | "unwrap_or" | "ok" | "err" | "expect" | "map" | "map_err"
            ) {
            Some("")
        } else {
            runtime_symbol
        };
        // The `to_string` empty-symbol promotion is only valid for
        // the `.to_string()` method. Without this gate, an inverse
        // fix-up that forces `unwrap_or` on a scalar back to
        // identity (`Some("")`) would accidentally promote to
        // `gos_rt_i64_to_str`, turning `as_i64(v).unwrap_or(0)`
        // into a string render of the i64.
        if matches!(runtime_symbol, Some("")) && method.name.as_str() == "to_string" {
            runtime_symbol = match self.tcx.kind_of(lowered_recv_ty) {
                TyKind::Int(int) => Some(super::int_to_str_symbol(*int)),
                TyKind::Float(gossamer_types::FloatTy::F32) => Some("gos_rt_f32_to_str"),
                TyKind::Float(_) => Some("gos_rt_f64_to_str"),
                _ => runtime_symbol,
            };
        }
        if lowered_is_result {
            match method.name.as_str() {
                "unwrap" | "expect" => {
                    let nested = self.carrier_payload_is_carrier(lowered_recv_ty);
                    runtime_symbol = Some(match (self.is_option_adt(lowered_recv_ty), nested) {
                        (true, true) => "gos_rt_option_unwrap_carrier",
                        (true, false) => "gos_rt_option_unwrap",
                        (false, true) => "gos_rt_result_unwrap_carrier",
                        (false, false) => "gos_rt_result_unwrap",
                    });
                }
                "unwrap_or" => {
                    // The lowered receiver's own type decides, since the
                    // HIR type is often a variable for a chained call. A
                    // payload that is itself a carrier was boxed, so it is
                    // loaded back rather than handed over as an address.
                    runtime_symbol = Some(if self.carrier_payload_is_carrier(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_carrier"
                    } else if self.carrier_payload_is_map(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_map"
                    } else if self.carrier_payload_is_sequence(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_vec"
                    } else if self.carrier_payload_is_string(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_str"
                    } else if self.carrier_payload_is_counted_node(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_node"
                    } else {
                        "gos_rt_result_unwrap_or"
                    });
                }
                "ok" => runtime_symbol = Some("gos_rt_result_to_opt_ok"),
                "err" => runtime_symbol = Some("gos_rt_result_to_opt_err"),
                _ => {}
            }
        }
        // The dispatch table above names an advance from the element type,
        // which cannot tell an address-carrying stream of pairs from the pair
        // state `zip` and `enumerate` build, so the lowered state decides.
        if (runtime_symbol.is_none() || self.local_aggr_iter.contains(&receiver_local))
            && method.name.as_str() == "next"
            && args.is_empty()
            && let TyKind::Iterator(elem) = self.tcx.kind_of(lowered_recv_ty).clone()
        {
            // An address-carrying stream advances through the word shim: its
            // slot is the element's address, which the `Some` payload is.
            runtime_symbol = if self.local_aggr_iter.contains(&receiver_local) {
                Some("gos_rt_lazy_iter_next_i64")
            } else {
                self.lazy_iter_next_symbol(elem)
            };
        }
        // `.clone()` / `.collect()` on a Vec/Slice receiver: dispatch to
        // `gos_rt_vec_clone` so the result is a fresh independent
        // `GosVec` allocation rather than a bitwise pointer alias.
        // Without this, `caps[0].clone()` (where `caps[0]` returns an
        // inner `*mut GosVec` pinned to a fresh local) leaves two
        // locals holding the same pointer; the auto-drop pass then
        // emits `gos_rt_vec_free` for each, producing a double free.
        // The top-of-method dispatch table (`runtime_symbol = match
        // method.name.as_str() { … }`) keys on the HIR receiver kind,
        // which is still a `Var` for chained `Index<i>.clone()` shapes
        // - `lowered_recv_ty` is the resolved MIR-side type.
        if matches!(method.name.as_str(), "clone" | "collect")
            && matches!(
                self.tcx.kind_of(lowered_recv_ty),
                TyKind::Vec(_) | TyKind::Slice(_)
            )
        {
            runtime_symbol = Some("gos_rt_vec_clone");
        }
        // `.len()` on an inline `gos_rt_*` runtime-call temporary: the
        // HIR receiver type is an unresolved `Var`, so the top-of-method
        // dispatch defaulted to `gos_rt_len` (a GosVec-header read) even
        // when the lowered value is a c-string / map / json handle. Bind
        // to a local first always worked because that pins a real type;
        // re-key off the resolved `lowered_recv_ty` so the inline
        // temporary path matches. `sha256::hex(x).len()` must reach
        // `gos_rt_str_len` (strlen), not `gos_rt_len`.
        if runtime_symbol == Some("gos_rt_len") {
            let mut k = self.tcx.kind_of(lowered_recv_ty);
            while let TyKind::Ref { inner, .. } = k {
                k = self.tcx.kind_of(*inner);
            }
            runtime_symbol = match k {
                TyKind::String => Some("gos_rt_str_len"),
                TyKind::HashMap { .. } => Some("gos_rt_map_len"),
                TyKind::JsonValue => Some("gos_rt_json_len"),
                _ => runtime_symbol,
            };
        }
        // `<stdlib-call>.method()` consumed in place: the HIR receiver type
        // is an unresolved `Var`, so the type-guarded Vec / String / HashMap
        // arms in the dispatch table above were skipped and the method is
        // about to fall through to an undefined bare `@method` symbol. The
        // lowered receiver carries the real MIR type - re-key the guarded
        // dispatch off it so `env::args().first()` resolves the same as
        // `let a = env::args(); a.first()` on every tier.
        if runtime_symbol.is_none() {
            runtime_symbol =
                self.seq_str_method_from_lowered(method.name.as_str(), lowered_recv_ty, args.len());
        }

        // LLVM copies a multi-slot map value out of the inserting frame. Give
        // that copy its structural child layout now, so the backend can retain
        // direct String / Vec children and the map's eventual drop can release
        // them. The ordinary guarded copy meta is intentionally insufficient:
        // it only describes conditional Option/Result copy-blob payloads.
        if matches!(
            runtime_symbol,
            Some(s) if s.starts_with("gos_rt_map_insert") || s.starts_with("gos_rt_map_or_insert")
        ) {
            // The receiver may be reached through `&mut`; the map type is
            // what carries the value type either way.
            let mut recv = lowered_recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv) {
                recv = *inner;
            }
            if let TyKind::HashMap { value, .. } = self.tcx.kind_of(recv) {
                // Any aggregate value takes the copy-blob path, a one-field
                // struct included: its blob owns the field the copy retains,
                // and the map's drop releases the blob.
                // `ensure_aggr_struct_meta` answers `None` on its own for a
                // value with no owning children.
                let value = *value;
                let _ = self.ensure_aggr_struct_meta(value);
                // An aggregate value with no owning children still needs a
                // copy meta: it is what tags the map as holding blob values,
                // so the entry takes the share the inserting frame gives back.
                // A two-word carrier is boxed the same way.
                if self.is_inline_aggregate_ty(value) || self.map_value_is_carrier(recv) {
                    let _ = self.ensure_aggr_copy_meta(value);
                }
            }
        }

        // An aggregate payload is copied out of the carrier in place, so the
        // answer takes its own shares of the value's children the way a
        // `match` binding does; the word helper would hand back the payload's
        // address with no share behind it. The lowered receiver decides, since
        // a chained call leaves the HIR receiver type a variable.
        if runtime_symbol == Some("gos_rt_result_unwrap_or")
            && let Some(payload_ty) = self.enum_payload_ty(lowered_recv_ty, 0)
            && self.tcx.elem_is_addressed_aggregate(payload_ty)
            && let [_, Operand::Copy(fallback)] = arg_operands.as_slice()
            && fallback.projection.is_empty()
        {
            let dest_ty = if matches!(
                self.tcx.kind_of(ty),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            ) {
                payload_ty
            } else {
                ty
            };
            let is_option = self.is_option_adt(lowered_recv_ty);
            return Some(self.lower_unwrap_or_inline(
                receiver_local,
                super::intrinsic::CarrierFallback::Value(fallback.local),
                lowered_recv_ty,
                is_option,
                dest_ty,
                span,
            ));
        }
        if let Some(sym) = runtime_symbol {
            return self.dispatch_via_runtime_symbol(
                sym,
                receiver,
                method,
                args,
                ty,
                span,
                receiver_local,
                arg_operands,
            );
        }
        self.emit_fallback_call(
            receiver,
            receiver_local,
            method,
            ty,
            span,
            arg_operands,
            receiver_ty,
            owner,
        )
    }

    /// Coerce the receiver and build the operand list for the fallback call.
    /// Loads the container pointer a vec slot holds. The loop binding for a
    /// `&mut` element is the slot's address, so a method call dereferences it
    /// once to reach the container itself.
    pub(super) fn load_slot_pointer(&mut self, slot: Local, slot_ty: Ty, span: Span) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let inner_ty = match self.tcx.kind_of(slot_ty) {
            TyKind::Ref { inner, .. } => *inner,
            _ => slot_ty,
        };
        let zero = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let loaded = self.fresh(inner_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
            args: vec![
                Operand::Copy(Place::local(slot)),
                Operand::Copy(Place::local(zero)),
            ],
            destination: Place::local(loaded),
            target: Some(next),
        });
        self.set_current(next);
        loaded
    }

    pub(super) fn build_fallback_arg_operands(
        &mut self,
        runtime_symbol: Option<&'static str>,
        receiver_local: Local,
        receiver: &HirExpr,
        args: &[HirExpr],
        expected_args: Option<&[Ty]>,
        span: Span,
    ) -> Option<(Local, Vec<Operand>)> {
        // A `&mut` for-loop binds a heap-container element to its slot
        // address; the container the method acts on is the pointer that slot
        // holds, so load it before dispatch.
        let receiver_local =
            if runtime_symbol.is_some() && self.slot_ref_locals.contains(&receiver_local) {
                self.load_slot_pointer(receiver_local, receiver.ty, span)
            } else {
                receiver_local
            };
        let receiver_local = match runtime_symbol {
            Some(sym) if sym.starts_with("gos_rt_vec_") || sym == "gos_rt_strings_join" => {
                match self
                    .tcx
                    .kind_of(self.locals[receiver_local.0 as usize].ty)
                    .clone()
                {
                    TyKind::Array { elem, len } => {
                        self.coerce_array_to_vec(receiver_local, elem, len, span)
                    }
                    _ => receiver_local,
                }
            }
            _ => receiver_local,
        };
        let mut arg_operands = Vec::with_capacity(args.len() + 1);
        arg_operands.push(Operand::Copy(Place::local(receiver_local)));
        // `xs.slice(a, b)` on a `[T; N]` literal receiver: splice
        // the static length read from `TyKind::Array { len }` between
        // the receiver pointer and the user-supplied `start` / `end`.
        // The runtime helper takes `(ptr, len, start, end)` because
        // inline `[T; N]` storage carries no length prefix.
        if matches!(
            runtime_symbol,
            Some(
                "gos_rt_intarr_slice_result"
                    | "gos_rt_floatarr_slice_result"
                    | "gos_rt_bytearr_slice_result"
            )
        ) {
            let recv_ty_kind = self.tcx.kind_of(receiver.ty);
            let recv_ty_kind = if let TyKind::Ref { inner, .. } = recv_ty_kind {
                self.tcx.kind_of(*inner)
            } else {
                recv_ty_kind
            };
            if let TyKind::Array { len: array_len, .. } = recv_ty_kind {
                let n = i128::try_from(array_len.to_usize()).unwrap_or(0);
                arg_operands.push(Operand::Const(ConstValue::Int(n)));
            }
        }
        // A `HashMap<_, Vec<_>>` insert / or_insert whose value is an
        // inline `[a, b, c]` array literal must marshal a real heap
        // `GosVec` (with the RC header the map's blob ownership and the
        // later `.len()` / index reads depend on), not the header-less
        // stack `[T; N]` buffer the literal lowers to. The key arg is a
        // String / i64 (never an Array) so it is left untouched.
        // Only when the map's declared value is a SEQUENCE. A map whose
        // value is the fixed array itself stores the array's own slots, the
        // way it stores a struct's or a tuple's; marshalling it into a
        // `GosVec` would have the read side take the vec's header words for
        // the array's first elements.
        let map_value_is_sequence = self
            .hash_map_value_ty(self.locals[receiver_local.0 as usize].ty)
            .or_else(|| self.hash_map_value_ty(receiver.ty))
            .is_some_and(|value| {
                matches!(self.tcx.kind_of(value), TyKind::Vec(_) | TyKind::Slice(_))
            });
        let coerce_map_value = map_value_is_sequence
            && matches!(
                runtime_symbol,
                Some(
                    "gos_rt_map_insert_i64_i64"
                        | "gos_rt_map_insert_str_i64"
                        | "gos_rt_map_insert_i64_i64_opt"
                        | "gos_rt_map_insert_str_i64_opt"
                        | "gos_rt_map_insert_typed_str_i64_opt"
                        | "gos_rt_map_or_insert_i64_i64"
                        | "gos_rt_map_or_insert_str_i64"
                        | "gos_rt_map_or_insert_typed_str_i64"
                )
            );
        let coerce_vec_extend_arg = matches!(runtime_symbol, Some("gos_rt_vec_extend"));
        // The value a push or insert stores fills one element slot, so it
        // takes the element's shape: a callable element holds the env-shaped
        // callable every callable slot holds.
        let stored_elem_slot = match runtime_symbol {
            Some("gos_rt_vec_push" | "gos_rt_vec_insert_safe" | "gos_rt_vec_insert_slots_safe") => {
                match self
                    .tcx
                    .kind_of(self.locals[receiver_local.0 as usize].ty)
                    .clone()
                {
                    TyKind::Vec(elem) | TyKind::Slice(elem) => Some(elem),
                    TyKind::Ref { inner, .. } => match self.tcx.kind_of(inner) {
                        TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                        _ => None,
                    },
                    _ => None,
                }
            }
            // An `unwrap_or` fallback is an answer in place of the payload, so
            // it takes the payload's shape.
            Some(sym) if sym.starts_with("gos_rt_result_unwrap_or") => self
                .first_generic_of(self.locals[receiver_local.0 as usize].ty)
                .or_else(|| self.first_generic_of(receiver.ty)),
            Some(sym)
                if sym.starts_with("gos_rt_map_insert")
                    || sym.starts_with("gos_rt_map_or_insert") =>
            {
                self.hash_map_kv_tys(self.locals[receiver_local.0 as usize].ty)
                    .or_else(|| self.hash_map_kv_tys(receiver.ty))
                    .map(|(_, value)| value)
            }
            _ => None,
        };
        // String methods whose needle / pattern argument is a `&str`
        // the runtime helper reads as a `*const c_char`. A `char`
        // literal (`s.contains('e')`, `s.replace('l', "L")`) lowers to
        // an i32 codepoint, so it must be converted to a one-char
        // String via `gos_rt_char_to_str` before the call - otherwise
        // the helper dereferences the codepoint as a pointer. Mirrors
        // the front-end coercion the free-function form already gets.
        let coerce_char_needle = matches!(
            runtime_symbol,
            Some(
                "gos_rt_str_contains"
                    | "gos_rt_str_find_opt"
                    | "gos_rt_str_rfind_opt"
                    | "gos_rt_str_replace"
                    | "gos_rt_str_replacen"
                    | "gos_rt_str_starts_with"
                    | "gos_rt_str_ends_with"
                    | "gos_rt_str_trim_matches"
                    | "gos_rt_str_lstrip_chars"
                    | "gos_rt_str_rstrip_chars"
                    | "gos_rt_str_split_once"
                    | "gos_rt_str_rsplit_once"
                    | "gos_rt_str_count"
                    | "gos_rt_str_strip_prefix"
                    | "gos_rt_str_strip_suffix"
                    | "gos_rt_str_split"
            )
        );
        for (index, arg) in args.iter().enumerate() {
            let a = match self.string_as_bytes_text(arg) {
                Some(text) if runtime_symbol == Some("gos_rt_vec_extend_str_bytes") => {
                    self.lower_expr(text)?
                }
                _ => self.lower_expr(arg)?,
            };
            // 0.7.0 flag::Cell auto-deref at the call boundary -
            // mirrors the bytecode VM's auto-unwrap shape so
            // `get_comic(flags.number)` works without `*`.
            let a = self.auto_deref_cell(a, span);
            let a = if coerce_map_value || coerce_vec_extend_arg {
                let lt = self.locals[a.0 as usize].ty;
                if let TyKind::Array { elem, len } = self.tcx.kind_of(lt).clone() {
                    self.coerce_array_to_vec(a, elem, len, span)
                } else {
                    a
                }
            } else {
                a
            };
            let a = if coerce_char_needle {
                self.coerce_char_arg_to_str(a, span)
            } else {
                a
            };
            let a = match stored_elem_slot {
                Some(slot_ty) if index + 1 == args.len() => {
                    self.coerce_to_fn_trait_if_needed(a, slot_ty, span)
                }
                _ => a,
            };
            // A callable parameter is called through an environment on every
            // tier, so a bare function item or lifted closure handed to one
            // is wrapped exactly as a free function's argument is. The thunk
            // is named for the instantiated signature: a method's own `Fn() ->
            // T` names the register classes of the `T` this call passes.
            let a = match expected_args.and_then(|tys| tys.get(index)).copied() {
                Some(expected)
                    if !self.local_closure.contains_key(&a)
                        && matches!(
                            self.tcx.kind_of(expected),
                            TyKind::FnPtr(_) | TyKind::FnTrait(_)
                        ) =>
                {
                    let source_ty = self.locals[a.0 as usize].ty;
                    let expected = self.instantiate_param_ty_from_arg(expected, source_ty);
                    self.coerce_to_fn_trait_if_needed(a, expected, span)
                }
                _ => a,
            };
            // User impl arguments obey the same array-to-slice coercions as
            // free-function calls. In particular, `method(&[a, b])` passes a
            // real GosVec-backed borrowed slice when the declared parameter is
            // `&[T]`; forwarding the inline `[T; N]` address makes the callee
            // interpret element zero as a Vec length/header and dereference a
            // wild data pointer.
            let a = if let Some(expected) = expected_args.and_then(|tys| tys.get(index)).copied() {
                let source_ty = self.locals[a.0 as usize].ty;
                let source_inner = match self.tcx.kind_of(source_ty) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => source_ty,
                };
                let expected_inner = match self.tcx.kind_of(expected) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => expected,
                };
                if let TyKind::Array { elem, len } = self.tcx.kind_of(source_inner).clone() {
                    if matches!(self.tcx.kind_of(expected_inner), TyKind::Slice(_)) {
                        if matches!(self.tcx.kind_of(expected), TyKind::Ref { .. }) {
                            self.coerce_borrow_array_to_vec(a, elem, len, span)
                        } else {
                            self.coerce_array_to_vec(a, elem, len, span)
                        }
                    } else {
                        a
                    }
                } else {
                    a
                }
            } else {
                a
            };
            // A map handle carries no count, so the channel always takes a
            // table of its own: a named map stays the sender's, and a
            // temporary may itself be a map borrowed out of another.
            let a = if runtime_symbol == Some("gos_rt_chan_send")
                && matches!(
                    self.tcx.kind_of(self.locals[a.0 as usize].ty),
                    TyKind::HashMap { .. }
                ) {
                let cloned = self.fresh(self.locals[a.0 as usize].ty);
                self.emit_owned_clone_binding(a, cloned, span);
                cloned
            } else {
                a
            };
            arg_operands.push(Operand::Copy(Place::local(a)));
        }
        Some((receiver_local, arg_operands))
    }

    /// Rewrite a non-capturing `map`/`map_err` closure arg to the bare-fn ABI.
    pub(super) fn rewrite_result_map_closure_arg(
        &mut self,
        runtime_symbol: Option<&'static str>,
        arg_operands: &mut Vec<Operand>,
        span: Span,
    ) -> Option<&'static str> {
        let mut runtime_symbol = runtime_symbol;
        // `gos_rt_result_map_err` / `gos_rt_result_map` expect a
        // closure handle whose first 8 bytes hold the lifted
        // function's address. The HIR lift pass turns
        // non-capturing closures into a bare-name path
        // (`__closure_N`) which lowers to a string-literal pointer
        // - passing that to the helper segfaults the moment it
        // transmutes the first 8 ASCII bytes into a function
        // pointer. Wrap the arg as a 16-byte heap blob
        // `[fn_addr, _]` so the helper's first-word load resolves
        // to the actual lifted function.
        // Dispatch closure args by capture shape. Two distinct
        // ABIs are in play and the runtime helpers separate them
        // explicitly:
        //
        //   - **Capturing closures** lift to `extern "C" fn(env,
        //     payload) -> ret`. The MIR-side LiftedClosure node
        //     produces a heap-allocated env blob whose first slot
        //     is the lifted function's address; the rest are the
        //     captured values. Dispatched through
        //     `gos_rt_result_map` / `_map_err` (env-first ABI).
        //
        //   - **Non-capturing closures** lift to `extern "C" fn
        //     (payload) -> ret` - no env. The HIR lift pass turns
        //     them into a bare `Path` that lowers to a fn-name
        //     constant; `local_fn_name` is then set on the local.
        //     Dispatched through `gos_rt_result_map_bare` /
        //     `_map_err_bare` (no-env ABI), passing the function
        //     address directly.
        //
        // Pre-fix the same `gos_rt_result_map` was used for both,
        // with the call site wrapping the bare fn-pointer in a
        // 16-byte `[fn_addr, _]` blob and praying the C ABI's
        // unused-arg semantics would let the closure's first
        // param shadow the env pointer. On x86_64 it didn't -
        // RDI/RSI assignment matched the helper's perspective
        // (env_ptr, payload), so the closure's `v` param shadowed
        // RDI = env_ptr while the actual payload sat unread in
        // RSI. The closure body then transformed env_ptr instead
        // of payload, which corrupted the resulting Result and
        // produced the askq round-2 strlen-on-bad-pointer crash.
        // See `~/dev/contexts/lang/fix_architecture_ownership.md`
        // for the closure-carrier root cause.
        if matches!(
            runtime_symbol,
            Some("gos_rt_result_map_err" | "gos_rt_result_map")
        ) && arg_operands.len() == 2
        {
            let closure_local = match &arg_operands[1] {
                Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
                _ => None,
            };
            if let Some(local) = closure_local
                && let Some(fn_name) = self.local_fn_name.get(&local).cloned()
            {
                // Non-capturing path: pass the lifted fn addr as a
                // raw i64 and dispatch through the `_bare` helper
                // that calls it as `f(payload)` - single arg, no
                // env. Switch the dispatched symbol to the bare
                // variant.
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let fn_addr_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(fn_addr_local),
                    Rvalue::CallIntrinsic {
                        name: "gos_fn_addr",
                        args: vec![Operand::Const(ConstValue::Str(fn_name))],
                    },
                    span,
                );
                arg_operands[1] = Operand::Copy(Place::local(fn_addr_local));
                runtime_symbol = match runtime_symbol {
                    Some("gos_rt_result_map") => Some("gos_rt_result_map_bare"),
                    Some("gos_rt_result_map_err") => Some("gos_rt_result_map_err_bare"),
                    other => other,
                };
            }
            // Capturing-closure path: the LiftedClosure lowering
            // already produced an `env_ptr` whose first 8 bytes
            // hold the lifted fn addr. The original
            // `gos_rt_result_map(_err)` env-first dispatch is
            // correct for this shape; nothing to rewrite.
        }
        runtime_symbol
    }

    /// The fixed `[elem; len]` a call typed `ty` answers when it reaches impl
    /// function `mangled`, whose declared const generic array result the body
    /// hands back as a runtime-length sequence.
    pub(crate) fn const_array_method_result(&self, mangled: &str, ty: Ty) -> Option<(Ty, usize)> {
        let mut ret = self.impl_methods.get(mangled).copied().flatten()?;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ret) {
            ret = *inner;
        }
        if !matches!(
            self.tcx.kind_of(ret),
            TyKind::Array {
                len: gossamer_types::ArrayLen::Param(_),
                ..
            }
        ) {
            return None;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Array {
                elem,
                len: gossamer_types::ArrayLen::Concrete(len),
            } => Some((*elem, *len)),
            _ => None,
        }
    }

    /// Emit the user-impl method call or the generic by-name fallback call.
    pub(super) fn emit_fallback_call(
        &mut self,
        receiver: &HirExpr,
        receiver_local: Local,
        method: &Ident,
        ty: Ty,
        span: Span,
        arg_operands: Vec<Operand>,
        receiver_ty: Ty,
        owner: Option<&Ident>,
    ) -> Option<Local> {
        // The `impl` block the checker resolved this call to, which is the
        // answer whenever it has one: the receiver's type decided it there,
        // and below this point a container and a structural type both reach a
        // method as an untyped handle that names no block.
        let recorded = owner
            .map(|owner| format!("{}::{}", owner.name, method.name))
            .filter(|mangled| self.impl_methods.contains_key(mangled));
        // User-defined `impl` method dispatch: when the receiver's
        // static type names a known struct, look up the mangled
        // method name (`Struct::method`) and emit a direct call
        // with the receiver as the first argument. Mirrors the
        // tree-walker's qualified-method lookup so user code can
        // build natively without rewriting every method as a free
        // function.
        let lowered_receiver_ty = self
            .locals
            .get(receiver_local.0 as usize)
            .map(|decl| decl.ty);
        let struct_name = self
            .struct_name_of(receiver_ty)
            .or_else(|| lowered_receiver_ty.and_then(|ty| self.struct_name_of(ty)))
            .or_else(|| {
                self.local_struct
                    .get(&receiver_local)
                    .cloned()
                    .or_else(|| self.struct_name_from_expr(receiver))
            })
            .or_else(|| {
                // Enum receivers aren't in `struct_defs`; dispatch `e.method()`
                // to `Enum::method` when that impl method actually exists (so a
                // derived `clone`/`eq`/`fmt` on an enum resolves instead of
                // emitting an undefined bare `@method`).
                self.adt_dispatch_name(receiver_ty)
                    .filter(|n| {
                        self.impl_methods
                            .contains_key(&format!("{n}::{}", method.name))
                    })
                    .or_else(|| {
                        lowered_receiver_ty.and_then(|ty| {
                            self.adt_dispatch_name(ty).filter(|n| {
                                self.impl_methods
                                    .contains_key(&format!("{n}::{}", method.name))
                            })
                        })
                    })
            });
        if let Some(sname) = struct_name {
            let mangled = format!("{}::{}", sname, method.name);
            // Pin a sensible destination type if HIR left it
            // unresolved. Trait-dispatched method calls
            // (`circle.name()` where `name` is declared on the
            // `Shape` trait) often arrive with the destination ty
            // still an inference variable; use the impl's known
            // return type when available so the codegen sees the
            // real `String` / `f64` / etc. instead of falling
            // back to `i64` and printing the pointer bits.
            let dest_ty = match self.tcx.kind_of(ty) {
                gossamer_types::TyKind::Error | gossamer_types::TyKind::Var(_) => self
                    .impl_methods
                    .get(&mangled)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64)),
                _ => ty,
            };
            // A generic method's return type is the impl's `Param`; the call
            // expression often carries it un-instantiated. Substitute the
            // receiver's concrete generic arguments (`Wrapper<i64>` -> `i64`)
            // so the destination is the real type, not an opaque `Param` slot
            // codegen would render as a pointer.
            let dest_ty = if self.ty_mentions_param(dest_ty) {
                let recv_substs = self.adt_substs_vec(receiver_ty);
                self.subst_params_with(dest_ty, &recv_substs)
            } else {
                dest_ty
            };
            let carrier = self.const_array_method_result(&mangled, dest_ty);
            let dest = match carrier {
                Some((elem, _)) => {
                    let sequence = self.tcx.intern(TyKind::Slice(elem));
                    self.fresh(sequence)
                }
                None => self.fresh(dest_ty),
            };
            if carrier.is_none()
                && let Some(out_struct) = self.struct_name_of(dest_ty)
            {
                self.local_struct.insert(dest, out_struct);
            }
            let next = self.new_block(span);
            let receiver_reload = arg_operands.first().and_then(|arg| match arg {
                Operand::Copy(place) if place.projection.is_empty() => self
                    .mut_receiver_reloads
                    .remove(&place.local)
                    .map(|target| (target, place.local)),
                _ => None,
            });
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(mangled)),
                args: arg_operands,
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            if let Some((target, ref_local)) = receiver_reload {
                self.emit_assign(
                    Place::local(target),
                    Rvalue::Use(Operand::Copy(Place {
                        local: ref_local,
                        projection: vec![crate::ir::Projection::Deref],
                    })),
                    span,
                );
            }
            if let Some((elem, len)) = carrier {
                return self.array_from_const_generic_carrier(dest, elem, len, dest_ty, receiver);
            }
            return Some(dest);
        }

        // Pick the impl whose receiver is the type in front of us. Choosing by
        // the method name being unique in the program instead answers nothing
        // once a second type implements the same trait, and the call is then
        // left naming a method rather than a body - which the compiled tiers
        // have no way to resolve and reject as an undefined symbol.
        let receiver_key = self.peel_ref_ty(receiver_ty);
        let mut named: Vec<&str> = Vec::new();
        let mut on_receiver: Vec<&str> = Vec::new();
        for name in self.impl_methods.keys() {
            if !name
                .rsplit_once("::")
                .is_some_and(|(_, tail)| tail == method.name.as_str())
            {
                continue;
            }
            named.push(name.as_str());
            if self
                .impl_method_receivers
                .get(name)
                .is_some_and(|declared| self.peel_ref_ty(*declared) == receiver_key)
            {
                on_receiver.push(name.as_str());
            }
        }
        // A container reaches a method as an untyped handle, which the flat
        // model types the way it types an `i64`, so a receiver type only tells
        // the impls apart when every candidate declares a scalar one. With a
        // container among them the types collide and the name is left for the
        // resolution below rather than answered wrongly.
        let all_scalar_receivers = named.iter().all(|name| {
            self.impl_method_receivers
                .get(*name)
                .is_some_and(|declared| {
                    matches!(
                        self.tcx.kind_of(self.peel_ref_ty(*declared)),
                        gossamer_types::TyKind::Int(_)
                            | gossamer_types::TyKind::Float(_)
                            | gossamer_types::TyKind::Bool
                            | gossamer_types::TyKind::Char
                    )
                })
        });
        if !all_scalar_receivers {
            on_receiver.clear();
        }
        // The impl whose receiver is the type in front of us. Two impls on one
        // type is a coherence error the checker reports, so more than one
        // match names no single body and falls through with the rest.
        let unique_impl = match recorded.as_deref() {
            Some(mangled) => Some(mangled),
            None => match on_receiver.as_slice() {
                [only] => Some(*only),
                _ => match named.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                },
            },
        };
        if let Some(mangled) = unique_impl {
            let dest_ty = match self.tcx.kind_of(ty) {
                gossamer_types::TyKind::Error | gossamer_types::TyKind::Var(_) => self
                    .impl_methods
                    .get(mangled)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64)),
                _ => ty,
            };
            let carrier = self.const_array_method_result(mangled, dest_ty);
            let dest = match carrier {
                Some((elem, _)) => {
                    let sequence = self.tcx.intern(TyKind::Slice(elem));
                    self.fresh(sequence)
                }
                None => self.fresh(dest_ty),
            };
            if carrier.is_none()
                && let Some(out_struct) = self.struct_name_of(dest_ty)
            {
                self.local_struct.insert(dest, out_struct);
            }
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(mangled.to_string())),
                args: arg_operands,
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            if let Some((elem, len)) = carrier {
                return self.array_from_const_generic_carrier(dest, elem, len, dest_ty, receiver);
            }
            return Some(dest);
        }

        if std::env::var("GOS_DEBUG_FALLBACK").is_ok() {
            eprintln!(
                "fallback method={} receiver_ty={:?} dest_ty={:?}",
                method.name,
                self.tcx.kind_of(receiver_ty),
                self.tcx.kind_of(ty)
            );
        }
        // No stdlib helper, no struct-impl match. Emit a generic
        // by-name Call: cranelift's `Const(Str(name))` callee path
        // resolves the symbol via `callees_by_name` (lifted
        // closures, free fns) or falls back to a typed-zero stub
        // for genuinely unknown names. Either branch produces a
        // well-formed CFG, so the build never refuses to lower a
        // method shape we haven't taught the dispatch table about.
        let dest_ty = match self.tcx.kind_of(ty) {
            TyKind::Error | TyKind::Var(_) => self.tcx.int_ty(gossamer_types::IntTy::I64),
            _ => ty,
        };
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(method.name.clone())),
            args: arg_operands,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// `f.round()`, `x.pow(2.0)`, `f.atan2(y)` - a `math::` function called
    /// on a number, which names the free call with the receiver as its first
    /// argument, and lowers as that call does. An integer receiver or
    /// argument reaches a float parameter converted to `f64`.
    pub(super) fn lower_numeric_math_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        let name = method.name.as_str();
        // A method the program declares on the primitive is that method,
        // as the checker typed it.
        if self.user_impl_method_exists(receiver.ty, receiver, name) {
            return MethodLowering::Pass;
        }
        let (_, recv_kind) = self.receiver_dispatch_kinds(receiver);
        // An integer's magnitude is computed as a word and narrowed back to
        // the receiver's width, which wraps `i8::MIN` to itself as unary `-`
        // does, so the value never leaves its type's range.
        if name == "abs"
            && args.is_empty()
            && let TyKind::Int(int) = recv_kind
        {
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let recv_ty = self.tcx.int_ty(int);
            let value = match self.lower_expr(receiver) {
                Some(value) => value,
                None => return MethodLowering::Handled(None),
            };
            let wide = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(wide),
                Rvalue::Cast {
                    operand: Operand::Copy(Place::local(value)),
                    target: i64_ty,
                },
                span,
            );
            let magnitude = self.emit_combinator_call(
                "gos_rt_math_abs_i64",
                vec![Operand::Copy(Place::local(wide))],
                i64_ty,
                span,
            );
            if recv_ty == i64_ty {
                return MethodLowering::Handled(Some(magnitude));
            }
            let narrowed = self.fresh(recv_ty);
            self.emit_assign(
                Place::local(narrowed),
                Rvalue::Cast {
                    operand: Operand::Copy(Place::local(magnitude)),
                    target: recv_ty,
                },
                span,
            );
            return MethodLowering::Handled(Some(narrowed));
        }
        // The bounds keep their own typed lowering.
        if matches!(name, "abs" | "min" | "max" | "clamp") {
            return MethodLowering::Pass;
        }
        let receiver_is_float = match recv_kind {
            TyKind::Float(_) => true,
            TyKind::Int(_) => false,
            _ => return MethodLowering::Pass,
        };
        let Some(shape) =
            gossamer_types::stdlib_signatures::function_shape_for_path(&["math"], name)
        else {
            return MethodLowering::Pass;
        };
        if shape.params.len() != args.len() + 1 {
            return MethodLowering::Pass;
        }
        let f64_ty = self.tcx.float_ty(gossamer_types::FloatTy::F64);
        let widen = |expr: &HirExpr| HirExpr {
            id: expr.id,
            span: expr.span,
            ty: f64_ty,
            kind: HirExprKind::Cast {
                value: Box::new(expr.clone()),
                ty: f64_ty,
            },
        };
        let first = if receiver_is_float {
            receiver.clone()
        } else {
            widen(receiver)
        };
        let mut free_args = Vec::with_capacity(args.len() + 1);
        free_args.push(first);
        for (arg, param) in args.iter().zip(&shape.params[1..]) {
            let arg_is_int = matches!(self.tcx.kind_of(arg.ty), TyKind::Int(_));
            if arg_is_int && param.ty == "f64" {
                free_args.push(widen(arg));
            } else {
                free_args.push(arg.clone());
            }
        }
        let call = HirExpr {
            id: receiver.id,
            span,
            ty,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: receiver.id,
                    span,
                    ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("math"), method.clone()],
                        def: None,
                    },
                }),
                args: free_args,
            },
        };
        MethodLowering::Handled(self.lower_expr(&call))
    }

    /// `n.max(m)` / `n.min(m)` / `n.clamp(lo, hi)` on a numeric receiver.
    /// These name the same operation as the prelude's free `max(n, m)`, so
    /// they lower through the same runtime helpers rather than reaching the
    /// by-name fallback, which had no symbol to call.
    pub(super) fn lower_scalar_bound_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        let arity_ok = match method.name.as_str() {
            "min" | "max" => args.len() == 1,
            "clamp" => args.len() == 2,
            _ => false,
        };
        if !arity_ok || self.user_impl_method_exists(receiver.ty, receiver, &method.name) {
            return MethodLowering::Pass;
        }
        let (_, recv_kind) = self.receiver_dispatch_kinds(receiver);
        if !matches!(recv_kind, TyKind::Int(_) | TyKind::Float(_) | TyKind::Char) {
            return MethodLowering::Pass;
        }
        let mut free_args = Vec::with_capacity(args.len() + 1);
        free_args.push(receiver.clone());
        free_args.extend(args.iter().cloned());
        match self.lower_stdlib_free_by_name(method.name.as_str(), &free_args, span) {
            Some(local) => MethodLowering::Handled(Some(local)),
            None => MethodLowering::Pass,
        }
    }
}
