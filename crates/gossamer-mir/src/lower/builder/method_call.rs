#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

/// How the lazy iterator runtime carries one element in its 8-byte slot.
///
/// The slot is untyped, so the family decides three things at once: which
/// register file the element travels in when a helper calls a Gossamer
/// closure, how the arithmetic terminals read its bits, and what type the
/// closure's parameter must be for method dispatch inside the body to resolve
/// against the real receiver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LazyElemFamily {
    /// `i64`, carried by value in an integer register.
    Word,
    /// `f64`, carried as its 64-bit pattern and read back in an SSE register.
    Float,
    /// A managed pointer word (`String`).
    Ptr,
    /// Two `i64` fields on the dedicated pair state `zip` / `enumerate` build.
    PairWord,
    /// The address of an element whose storage is wider than one slot. The
    /// element stays in the source buffer the state keeps alive, so a callback
    /// reads it through the address - the same shape the eager multi-slot
    /// surface passes.
    Aggr,
}

impl LazyElemFamily {
    /// Suffix of the runtime symbol family that reads this element class.
    /// `Ptr` shares the word suffix: a managed pointer and an `i64` occupy the
    /// same register class, so only the closure's parameter type differs.
    pub(crate) fn word_or_float_suffix(self) -> &'static str {
        match self {
            Self::Float => "f64",
            Self::Word | Self::Ptr | Self::PairWord | Self::Aggr => "i64",
        }
    }
}

/// Register class a combinator's callback sees for one element or scalar.
///
/// The same class the ABI registry declares on a shim's row, so the class
/// computed from a sequence's type and the class its shim was written against
/// are one type and can be compared directly.
pub(crate) use gossamer_abi::ElemClass as ElemAbi;

/// Outcome of an early method-dispatch guard in `Builder::lower_method_call`.
enum MethodLowering {
    /// A guard claimed the call; carries the value to return.
    Handled(Option<Local>),
    /// No guard matched; fall through to the next dispatch stage.
    Pass,
}

/// Outcome of the name-keyed runtime-symbol lookup.
enum SymbolLookup {
    /// The lowering must stop and return `None` (a `return None` table arm).
    Bail,
    /// A runtime symbol was resolved (`Some("")` = identity, `None` = no symbol).
    Found(Option<&'static str>),
}

type KindDispatchArgs = (Vec<Operand>, &'static str, Vec<(Local, Local)>);

const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
const VALIDATE_ERRORS_DEF_LOCAL: u32 = u32::MAX - 9;
const VALIDATE_FIELD_ERROR_DEF_LOCAL: u32 = u32::MAX - 10;
const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
const REVERSE_DEF_LOCAL: u32 = u32::MAX - 29;
const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;

/// Whether argument `index` of `rt` is a one-word map / set slot, which a
/// float reaches as its bit pattern rather than as a converted integer.
pub(crate) fn float_word_arg(rt: &str, index: usize) -> bool {
    if index == 0 {
        return false;
    }
    let key_or_value = rt.starts_with("gos_rt_map_insert")
        || rt.starts_with("gos_rt_map_or_insert")
        || rt.starts_with("gos_rt_map_get")
        || rt.starts_with("gos_rt_map_pop")
        || rt.starts_with("gos_rt_map_remove")
        || rt.starts_with("gos_rt_map_contains")
        || rt.starts_with("gos_rt_map_inc");
    let set_element = matches!(
        rt,
        "gos_rt_set_insert_i64" | "gos_rt_set_contains_i64" | "gos_rt_set_remove_i64"
    );
    key_or_value || set_element
}

fn tuple_get_const_index(expr: &HirExpr) -> Option<usize> {
    match &expr.kind {
        HirExprKind::Literal(HirLiteral::Int(raw)) => raw.parse::<usize>().ok(),
        _ => None,
    }
}

impl<'a> Builder<'a> {
    pub(crate) fn lower_method_call(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
        owner: Option<&Ident>,
    ) -> Option<Local> {
        // A `&mut <scalar / String>` reference is the address of the
        // caller's slot, so a method on it dispatches on the value the slot
        // holds, exactly as `(*x).m()` does. The rebinding string methods
        // keep the reference: they publish the runtime's replacement pointer
        // back through the same slot.
        if !Self::rebinds_receiver_place(method)
            && let Some(pointee) = self.mut_slot_receiver_pointee(receiver)
        {
            let deref = HirExpr {
                id: receiver.id,
                span: receiver.span,
                ty: pointee,
                kind: HirExprKind::Unary {
                    op: HirUnaryOp::Deref,
                    operand: Box::new(receiver.clone()),
                },
            };
            return self.lower_method_call(&deref, method, args, ty, span, owner);
        }
        // Some handle methods are the method spelling of a free stdlib
        // call, and only the free lowering knows how to build what they
        // need: an env thunk for a closure argument, or the carrier a
        // `Result` / `Option` return crosses the C-ABI in. `o.call(f)` and
        // `sync::Once::call(o, f)` name the same call, so the method
        // spelling routes there with the receiver in the first slot.
        if let Some(joined) = self.free_form_method_path(receiver, method, args) {
            let mut ordered = vec![receiver.clone()];
            ordered.extend(args.iter().cloned());
            if let Some(local) = self.try_lower_combinator_call(joined, &ordered, ty, span) {
                return Some(local);
            }
            if let Some((rt_name, ret_ty)) = self.lower_errors_regex_free(joined, &ordered) {
                return self.emit_stdlib_free_call(rt_name, ret_ty, &ordered, span);
            }
        }
        // `server.serve(handler)` needs the handler's dispatch address
        // alongside its environment, the same three-argument shape
        // `http::serve(addr, handler)` lowers to. The generic method
        // dispatch below passes arguments through unchanged, which would
        // hand the runtime an environment with no code pointer.
        if method.name == "serve"
            && args.len() == 1
            && self.runtime_kind_from_ty(receiver.ty) == Some("http::Server")
        {
            let receiver_local = self.lower_expr(receiver)?;
            return self.lower_http_server_serve(receiver_local, &args[0], span);
        }
        // Fuse signed-integer `n.to_string().chars()` into one runtime call.
        // The unfused form allocated a C string, scanned it into a second
        // allocation, then released the temporary string. Numeric text is
        // ASCII, so the runtime can format directly into the `Vec<char>`.
        let numeric_chars_receiver = if method.name == "chars" && args.is_empty() {
            match &receiver.kind {
                HirExprKind::MethodCall {
                    receiver: numeric,
                    name: stringify,
                    args: stringify_args,
                    owner: None,
                } if stringify.name == "to_string" && stringify_args.is_empty() => {
                    match self.tcx.kind_of(numeric.ty) {
                        TyKind::Int(int_ty) if int_ty.is_signed() => Some(numeric.as_ref()),
                        // Unconstrained integer expressions default to signed
                        // i64, which is also how the ordinary `to_string`
                        // dispatch resolves this HIR shape.
                        TyKind::Var(_) => Some(numeric.as_ref()),
                        _ => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(numeric) = numeric_chars_receiver {
            let numeric = self.lower_expr(numeric)?;
            let char_ty = self.tcx.char_ty();
            let vec_ty = self.tcx.intern(TyKind::Vec(char_ty));
            let scalars = self.fresh(vec_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_i64_chars".to_string())),
                args: vec![Operand::Copy(Place::local(numeric))],
                destination: Place::local(scalars),
                target: Some(next),
            });
            self.set_current(next);
            // `chars()` answers a cursor, so the formatted scalars are handed
            // over as a borrowed lazy iterator over them - the same shape a
            // `String` receiver's `chars()` produces.
            return Some(self.emit_combinator_call(
                "gos_rt_lazy_iter_from_vec_i64",
                vec![Operand::Copy(Place::local(scalars))],
                ty,
                span,
            ));
        }

        // A pair element rides the dedicated two-word state that `zip` and
        // `enumerate` build, which has no borrowed-Vec source, so a pair
        // sequence keeps the eager surface.
        if method.name == "iter"
            && args.is_empty()
            && let Some(family) = self.lazy_iter_ty_family(ty)
            && family != LazyElemFamily::PairWord
        {
            let mut receiver_ty = receiver.ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(receiver_ty) {
                receiver_ty = *inner;
            }
            if matches!(
                self.tcx.kind_of(receiver_ty),
                TyKind::Vec(_) | TyKind::Slice(_)
            ) {
                let helper = format!(
                    "gos_rt_lazy_iter_from_vec_{}",
                    family.word_or_float_suffix()
                );
                let source = self.lower_expr(receiver)?;
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper)),
                    args: vec![Operand::Copy(Place::local(source))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
        }

        if matches!(method.name.as_str(), "wrapping_add" | "wrapping_mul") && args.len() == 1 {
            let mut receiver_ty = receiver.ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(receiver_ty) {
                receiver_ty = *inner;
            }
            if matches!(
                self.tcx.kind_of(receiver_ty),
                TyKind::Int(_) | TyKind::Var(_)
            ) {
                let lhs = self.lower_expr(receiver)?;
                let rhs = self.lower_expr(&args[0])?;
                let dest = self.fresh(ty);
                let op = if method.name == "wrapping_add" {
                    BinOp::WrappingAdd
                } else {
                    BinOp::WrappingMul
                };
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::BinaryOp {
                        op,
                        lhs: Operand::Copy(Place::local(lhs)),
                        rhs: Operand::Copy(Place::local(rhs)),
                    },
                    span,
                );
                return Some(dest);
            }
        }

        // `HashSet::intersection` is eager in Gossamer so its ordinary
        // `.iter()` path used to allocate a whole temporary set and clone
        // every matching aggregate. Recognise the immediate snapshot and
        // emit one runtime call that writes the sorted Vec directly.
        if method.name == "iter"
            && args.is_empty()
            && let HirExprKind::MethodCall {
                receiver: left,
                name: intersection,
                args: intersection_args,
                owner: None,
            } = &receiver.kind
            && intersection.name == "intersection"
            && intersection_args.len() == 1
            && matches!(
                self.runtime_kind_from_ty(left.ty),
                Some("collections::HashSet" | "collections::BTreeSet")
            )
        {
            let right = &intersection_args[0];
            let left_local = self.lower_expr(left)?;
            let right_local = self.lower_expr(right)?;
            let aggregate_desc = self
                .first_generic_of(left.ty)
                .filter(|elem| self.is_aggregate_key(*elem))
                .and_then(|elem| self.key_descriptor(elem));
            let symbol = if aggregate_desc.is_some() {
                "gos_rt_set_intersection_to_vec_skey"
            } else if matches!(self.set_elem_kind_of(left), MapKeyKind::I64) {
                "gos_rt_set_intersection_to_vec_i64"
            } else {
                "gos_rt_set_intersection_to_vec"
            };
            let mut call_args = vec![
                Operand::Copy(Place::local(left_local)),
                Operand::Copy(Place::local(right_local)),
            ];
            if let Some(desc) = aggregate_desc {
                call_args.push(Operand::Const(ConstValue::Str(desc)));
            }
            let dest = self.fresh(ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                args: call_args,
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }

        // `x.into()` converts to the inferred target type `B` via its `B::from`
        // impl; the call's result type is `B`, so route to `B::from(x)` (the
        // tiers resolve free functions by mangled name). `x.try_into()` is the
        // same but the result is `Result<B, E>`, so `B` is its first type
        // argument and the method is `B::try_from`.
        if method.name.as_str() == "into"
            && args.is_empty()
            && let gossamer_types::TyKind::Vec(target_elem) = self.tcx.kind_of(ty).clone()
        {
            let source = self.lower_expr(receiver)?;
            if let gossamer_types::TyKind::Array { elem, len } =
                self.tcx.kind_of(self.locals[source.0 as usize].ty).clone()
            {
                // Rust provides `From<[T; N]> for Vec<T>`. Keep that conversion
                // explicit at the source level while using the same lowering as
                // `Vec::from(array)` on every execution tier.
                debug_assert_eq!(elem, target_elem);
                return Some(self.fixed_array_to_vec(source, elem, len, span));
            }
        }
        let conversion = match method.name.as_str() {
            "into" => self.adt_dispatch_name(ty).map(|b| (b, "from")),
            "try_into" => self
                .result_ok_ty(ty)
                .and_then(|b_ty| self.adt_dispatch_name(b_ty))
                .map(|b| (b, "try_from")),
            _ => None,
        };
        if args.is_empty()
            && let Some((bname, from_method)) = conversion
        {
            let mangled = format!("{bname}::{from_method}");
            if self.impl_methods.contains_key(&mangled) {
                let recv_local = self.lower_expr(receiver)?;
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(mangled)),
                    args: vec![Operand::Copy(Place::local(recv_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
        }
        // Stage 1 - early method-name guards, grouped by receiver category.
        // Each returns `Handled(result)` to claim the call, `Pass` to fall through.
        if let MethodLowering::Handled(r) =
            self.lower_rc_weak_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_time_unit_method(receiver, method, args, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_join_handle_method(receiver, method, args, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_json_clone_method(receiver, method, args, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_result_map_eager_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_scalar_bound_method(receiver, method, args, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_seq_combinator_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_tuple_get_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_hashmap_iter_binding_method(receiver, method, args, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_array_to_vec_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_array_mutation_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_map_idiom_method(receiver, method, args, ty, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_string_push_method(receiver, method, args, span)
        {
            return r;
        }

        // Stage 2 - recover the receiver's dispatch kind, then the two
        // receiver-shape early returns (header fold, fixed-array len).
        let (receiver_ty, receiver_kind_flat) = self.receiver_dispatch_kinds(receiver);
        if let MethodLowering::Handled(r) =
            self.lower_headers_fold_method(receiver, method, args, span)
        {
            return r;
        }
        if let MethodLowering::Handled(r) =
            self.lower_fixed_array_len_method(method, args, &receiver_kind_flat, span)
        {
            return r;
        }
        // Method-form `v.set(key, value)` on a `json::Value` is the
        // object field-update helper (append-or-replace, returns the
        // updated value). Custom-lowered because the value argument
        // crosses the FFI as a `*GosJson` and may need scalar boxing.
        // `HashMap` receivers never reach here: the checker rejects
        // `set` on a map (GT0002, `insert` is the map write).
        if method.name.as_str() == "set"
            && args.len() == 2
            && matches!(receiver_kind_flat, TyKind::JsonValue)
        {
            return self.lower_json_set_call(receiver, &args[0], &args[1], span);
        }
        // Closure-taking chain combinators on a Result/Option receiver
        // (and_then / or_else / filter / ok_or_else). Lowered like
        // their data-last free forms: the closure crosses the C-ABI as
        // the env-blob `lower_iter_closure` builds (which also thunks
        // non-capturing closures), so the generic table route - which
        // would pass the raw closure local - cannot carry them.
        if matches!(
            method.name.as_str(),
            "and_then" | "or_else" | "filter" | "ok_or_else" | "unwrap_or_else"
        ) && args.len() == 1
            && matches!(receiver_kind_flat, TyKind::Adt { .. })
            && self.is_result_or_option_adt(receiver_ty)
        {
            if let Some(r) =
                self.lower_variant_chain_method(receiver, method, &args[0], receiver_ty, ty, span)
            {
                return Some(r);
            }
        }

        // `x.to_string()` and `xs.join(sep)` render their values the way `{}`
        // does. Both reach the same formatter for every element type; only
        // the scalar shapes have a dedicated shim worth keeping.
        if method.name.as_str() == "to_string"
            && args.is_empty()
            && (self.display_to_string_receiver(receiver_ty)
                || self.receiver_is_set_handle(receiver))
            && !self.has_user_rendering_method(receiver_ty, "to_string")
            && !self.renders_through_own_shim(receiver, &receiver_kind_flat, receiver_ty)
        {
            return self.lower_display_to_string(receiver, span);
        }
        if method.name.as_str() == "join"
            && args.len() == 1
            && self.vec_join_symbol(receiver_ty).is_none()
            && let Some(local) = self.lower_display_join(receiver, &args[0], span)
        {
            return Some(local);
        }

        // `a.zip(b)` on an `Option` receiver pairs the two payloads. The
        // generic table would pass the second option's raw local, so route it
        // to the same intrinsic the `option::zip(a, b)` free form takes, with
        // the receiver leading as the call writes it.
        if method.name.as_str() == "zip"
            && args.len() == 1
            && matches!(receiver_kind_flat, TyKind::Adt { .. })
            && self.is_option_adt(receiver_ty)
        {
            let ordered = vec![receiver.clone(), args[0].clone()];
            if let Some(local) = self.try_lower_combinator_call("option::zip", &ordered, ty, span) {
                return Some(local);
            }
        }

        // Stage 3 - name-keyed runtime-symbol table; a user impl of the same
        // name shadows a bare-name runtime builtin.
        let mut runtime_symbol = match self.runtime_symbol_by_name(
            receiver,
            method,
            args,
            &receiver_kind_flat,
            receiver_ty,
        ) {
            SymbolLookup::Bail => return None,
            SymbolLookup::Found(s) => s,
        };
        if runtime_symbol.is_some()
            && let Some(sname) = self
                .struct_name_of(receiver_ty)
                .or_else(|| self.struct_name_from_expr(receiver))
            && self
                .impl_methods
                .contains_key(&format!("{sname}::{}", method.name.as_str()))
        {
            runtime_symbol = None;
        }

        // Stage 4 - receiver-runtime-kind dispatch (before lowering).
        let receiver_runtime_kind = self
            .receiver_local_from_path(receiver)
            .and_then(|l| self.local_runtime_kind.get(&l).copied())
            .or_else(|| self.expr_runtime_kind(receiver))
            .or_else(|| Self::stdlib_runtime_kind_from_kind(&receiver_kind_flat))
            .or_else(|| self.runtime_kind_from_ty(receiver_ty))
            .or_else(|| self.runtime_kind_from_ty(receiver.ty));
        let receiver_heap_reverse_i64 = self
            .receiver_local_from_path(receiver)
            .is_some_and(|local| self.local_binary_heap_min_i64.contains(&local))
            || self.binary_heap_elem_is_reverse_i64(receiver_ty)
            || self.binary_heap_elem_is_reverse_i64(receiver.ty);
        // The element decides how a heap orders, and the receiver's own type
        // may have been pinned to the handle word by now; the pushed value and
        // the `Option<T>` a pop answers name it either way.
        let receiver_heap_float_elem = self.heap_elem_is_float(receiver_ty)
            || self.heap_elem_is_float(receiver.ty)
            || args
                .first()
                .is_some_and(|arg| matches!(self.tcx.kind_of(arg.ty), TyKind::Float(_)))
            || matches!(self.tcx.kind_of(ty), TyKind::Adt { .. })
                && self
                    .first_generic_of(ty)
                    .is_some_and(|payload| matches!(self.tcx.kind_of(payload), TyKind::Float(_)));
        if let Some(rt) = self.kind_dispatch_symbol(
            receiver_runtime_kind,
            method,
            args,
            receiver_ty,
            receiver_heap_reverse_i64,
            receiver_heap_float_elem,
        ) {
            return self.lower_kind_dispatch_call(rt, receiver, args, ty, span);
        }

        // Stage 5 - Option / Result predicates (is_some / is_ok / ...).
        if let MethodLowering::Handled(r) = self.lower_option_result_predicate(
            receiver,
            method,
            &receiver_kind_flat,
            receiver_ty,
            span,
        ) {
            return r;
        }

        // Stage 6 - dispatch on the lowered receiver's runtime kind. User
        // methods declared with `&self` or `&mut self` must receive the
        // address of the actual place. Lowering `items[index]` as an ordinary
        // expression creates a value copy, so mutations would disappear and
        // native calls could use the wrong ABI.
        let user_receiver_ref_ty = owner
            .map(|owner| owner.name.clone())
            .or_else(|| self.struct_name_of(receiver_ty))
            .or_else(|| self.struct_name_from_expr(receiver))
            .or_else(|| self.primitive_impl_name(receiver_ty))
            .and_then(|name| {
                self.impl_method_receivers
                    .get(&format!("{name}::{}", method.name))
                    .copied()
            })
            .filter(|declared| matches!(self.tcx.kind_of(*declared), TyKind::Ref { .. }))
            // A shared receiver is borrowed only where the value and the
            // address differ, which is a scalar. Everything else - an enum, a
            // container, a string, an aggregate - is already the one word its
            // methods decode, so borrowing the place holding it would have the
            // callee decode a stack address instead. The decision reads the
            // CALLEE's declared receiver: the call site's own type is erased
            // to a handle for every container, and a handle is typed the way
            // an integer is. A `&mut self` receiver still borrows the place,
            // which is what carries a write back.
            .filter(|declared| {
                let TyKind::Ref {
                    mutability: gossamer_types::Mutbl::Not,
                    inner,
                } = self.tcx.kind_of(*declared)
                else {
                    return true;
                };
                matches!(
                    self.tcx.kind_of(*inner),
                    TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                )
            });
        let receiver_local = if let Some(declared_ref_ty) = user_receiver_ref_ty {
            // A chained by-value method result is not a source-level place,
            // but it is materialised in a MIR local and can be borrowed for
            // the next `&self` / `&mut self` call. Requiring
            // `lower_place_expr` to succeed silently discarded fluent chains
            // such as `Select::new(...).columns(...).order_by(...)`, leaving
            // the destination aggregate zero-initialised on compiled tiers.
            // A shared borrow of a primitive needs somewhere to point, not a
            // particular somewhere: the value is `Copy` and the callee cannot
            // write through the reference, so a temporary holding the value
            // observes exactly what the source place would. Reading it into
            // one keeps the address a plain stack slot whatever the receiver
            // was written as - a local, an element, a field. A `&mut self`
            // receiver still borrows the real place, which is what carries
            // the write back.
            let declared_is_mut = matches!(
                self.tcx.kind_of(declared_ref_ty),
                TyKind::Ref {
                    mutability: gossamer_types::Mutbl::Mut,
                    ..
                }
            );
            let receiver_place =
                if !declared_is_mut && self.primitive_impl_name(receiver_ty).is_some() {
                    Place::local(self.lower_expr(receiver)?)
                } else if let Some(place) = self.lower_place_expr(receiver) {
                    place
                } else {
                    Place::local(self.lower_expr(receiver)?)
                };
            if receiver_place.projection.is_empty()
                && matches!(
                    self.tcx
                        .kind_of(self.locals[receiver_place.local.0 as usize].ty),
                    TyKind::Ref { .. }
                )
            {
                receiver_place.local
            } else {
                let mutable = matches!(
                    self.tcx.kind_of(declared_ref_ty),
                    TyKind::Ref {
                        mutability: gossamer_types::Mutbl::Mut,
                        ..
                    }
                );
                // The impl declaration contains its template receiver
                // (`&Wrapper<T>`). Borrow the call site's concrete receiver
                // instead. Reusing the declared type collapsed every generic
                // method call onto one arbitrary instantiation, so
                // `Wrapper<Point>::get` called the scalar `Wrapper<i64>` ABI
                // and dereferenced `Point.x` as a pointer in native builds.
                let mut receiver_inner = receiver_ty;
                while let TyKind::Ref { inner, .. } = self.tcx.kind_of(receiver_inner) {
                    receiver_inner = *inner;
                }
                let receiver_ref_ty = self.tcx.intern(TyKind::Ref {
                    mutability: if mutable {
                        gossamer_types::Mutbl::Mut
                    } else {
                        gossamer_types::Mutbl::Not
                    },
                    inner: receiver_inner,
                });
                let receiver_ref = self.fresh(receiver_ref_ty);
                // A mutable borrow of a scalar place points at a slot the
                // backend materialises for it, so the place has to be reloaded
                // from that slot once the callee has written through it.
                if mutable
                    && receiver_place.projection.is_empty()
                    && matches!(
                        self.tcx.kind_of(receiver_inner),
                        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                    )
                {
                    self.mut_receiver_reloads
                        .insert(receiver_ref, receiver_place.local);
                }
                self.emit_assign(
                    Place::local(receiver_ref),
                    Rvalue::Ref {
                        place: receiver_place,
                        mutable,
                    },
                    span,
                );
                receiver_ref
            }
        } else {
            self.lower_expr(receiver)?
        };
        // `contains` / `index_of` / `count_of` over an element type the
        // flat-slot search shims cannot compare: scan with the language's own
        // structural `==` instead, so an f64 needle keeps its value and an
        // aggregate compares by value rather than by address.
        if matches!(method.name.as_str(), "contains" | "index_of" | "count_of")
            && args.len() == 1
            && let Some(elem_ty) = self
                .seq_elem_ty(receiver_ty)
                .or_else(|| self.seq_elem_ty(self.locals[receiver_local.0 as usize].ty))
            && !self.seq_search_shim_fits(elem_ty)
        {
            return self.lower_seq_eq_scan(
                receiver_local,
                &args[0],
                method.name.as_str(),
                elem_ty,
                ty,
                span,
            );
        }
        // `shared.with(f)` / `shared.update(f)` run a closure under the
        // lock, so they lower through the combinator-call convention
        // rather than the plain receiver-kind symbol table: the callback
        // has to reach the native side as an env blob, which a bare
        // symbol row cannot carry.
        if matches!(method.name.as_str(), "with" | "update")
            && args.len() == 1
            && (self.local_runtime_kind.get(&receiver_local).copied() == Some("sync::Shared")
                || self.ty_is_shared_handle(receiver_ty)
                || self.ty_is_shared_handle(self.locals[receiver_local.0 as usize].ty))
        {
            let symbol = if method.name.as_str() == "with" {
                "gos_rt_shared_with"
            } else {
                "gos_rt_shared_update"
            };
            // The callback sees the guarded value, so its parameter carries
            // that type: a hardcoded `i64` would leave a method on a
            // `String` or `Vec` payload with nothing to lower against.
            let elem_ty = self
                .shared_elem_ty(receiver_ty)
                .or_else(|| self.shared_elem_ty(self.locals[receiver_local.0 as usize].ty))
                .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
            // `update` stores what the callback answers, so it answers the
            // guarded type; `with` answers whatever the call site expects.
            let out_ty = if method.name.as_str() == "update" {
                elem_ty
            } else {
                ty
            };
            let closure = self.lower_iter_closure(&args[0], &[elem_ty], out_ty, span)?;
            return Some(self.emit_combinator_call(
                symbol,
                vec![
                    Operand::Copy(Place::local(receiver_local)),
                    Operand::Copy(Place::local(closure)),
                ],
                ty,
                span,
            ));
        }
        let lowered_runtime_kind = self.local_runtime_kind.get(&receiver_local).copied();
        let lowered_heap_reverse_i64 = self.local_binary_heap_min_i64.contains(&receiver_local)
            || self.binary_heap_elem_is_reverse_i64(receiver_ty)
            || self.binary_heap_elem_is_reverse_i64(receiver.ty);
        let lowered_heap_float_elem = self.heap_elem_is_float(receiver_ty)
            || self.heap_elem_is_float(receiver.ty)
            || args
                .first()
                .is_some_and(|arg| matches!(self.tcx.kind_of(arg.ty), TyKind::Float(_)))
            || matches!(self.tcx.kind_of(ty), TyKind::Adt { .. })
                && self
                    .first_generic_of(ty)
                    .is_some_and(|payload| matches!(self.tcx.kind_of(payload), TyKind::Float(_)));
        if let Some(rt) = self.lowered_kind_dispatch_symbol(
            lowered_runtime_kind,
            method,
            args,
            receiver_ty,
            lowered_heap_reverse_i64,
            lowered_heap_float_elem,
        ) {
            return self.lower_lowered_kind_dispatch_call(
                rt,
                receiver,
                receiver_local,
                args,
                ty,
                span,
            );
        }

        // Stage 7 - runtime-symbol fallback, user-impl, generic call.
        self.lower_method_call_fallback(
            receiver,
            receiver_local,
            method,
            args,
            ty,
            span,
            runtime_symbol,
            receiver_ty,
            owner,
        )
    }

    /// `x.downgrade()` / `w.upgrade()` - RC strong<->weak conversions.
    /// The lazy iterator family that carries `elem` in its 8-byte slot, or
    /// `None` for an element the state cannot address.
    ///
    /// This is the single place the lazy surface classifies an element type.
    /// Every producer, adapter, and terminal picks its runtime symbol and its
    /// closure parameter types from the family this returns, so a handle can
    /// never be built by one family and read by another: that reinterprets the
    /// slot, and the runtime's own class tag asserts the same agreement from
    /// the other side.
    pub(crate) fn lazy_iter_elem_family(&self, elem: Ty) -> Option<LazyElemFamily> {
        match self.tcx.kind_of(elem) {
            TyKind::Int(gossamer_types::IntTy::I64) => Some(LazyElemFamily::Word),
            // A `char` occupies a word slot like an `i64`, so the same
            // element family reads it. A narrower integer is packed at its
            // own width and is not one.
            TyKind::Char => Some(LazyElemFamily::Word),
            TyKind::String => Some(LazyElemFamily::Ptr),
            TyKind::Float(gossamer_types::FloatTy::F64) => Some(LazyElemFamily::Float),
            TyKind::Tuple(fields)
                if fields.len() == 2
                    && fields.iter().all(|field| {
                        matches!(
                            self.tcx.kind_of(*field),
                            TyKind::Int(gossamer_types::IntTy::I64)
                        )
                    }) =>
            {
                Some(LazyElemFamily::PairWord)
            }
            _ => None,
        }
    }

    /// Whether the lazy iterator runtime can carry `elem` in its 8-byte slot.
    pub(crate) fn lazy_iter_carries_elem(&self, elem: Ty) -> bool {
        self.lazy_iter_elem_family(elem).is_some()
    }

    /// Whether a sequence of `elem` can be borrowed as lazy state.
    ///
    /// The pair state is built only by `zip` and `enumerate`; it has no
    /// borrowed-sequence source, and a pair sequence stores two words per
    /// element, which the single-slot borrow cannot address. Such a sequence
    /// stays on the eager surface, where the element is read at its real width.
    pub(crate) fn lazy_iter_borrowable_elem(&self, elem: Ty) -> bool {
        // A borrowed source reads each element from a word-wide slot, so only
        // elements a sequence stores that way qualify. A narrower integer or a
        // `char` is packed at its own width in the buffer, and reading it as a
        // word would take the neighbouring bytes with it.
        matches!(
            self.tcx.kind_of(elem),
            TyKind::Int(gossamer_types::IntTy::I64)
                | TyKind::String
                | TyKind::Float(gossamer_types::FloatTy::F64)
        )
    }

    /// Family of an `Iterator<T>`-typed value, or `None` when `ty` is not
    /// iterator state the lazy runtime carries.
    pub(crate) fn lazy_iter_ty_family(&self, ty: Ty) -> Option<LazyElemFamily> {
        match self.tcx.kind_of(ty) {
            TyKind::Iterator(elem) => self.lazy_iter_elem_family(*elem),
            _ => None,
        }
    }

    fn lower_rc_weak_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `x.downgrade()` - create a `Weak<T>` from a strong RC value.
        // `gos_rt_rc_downgrade` bumps the weak count and returns the same
        // payload pointer, now typed `Weak<T>` so the drop pass releases
        // it through `gos_rt_rc_weak_release`. The referent it observes is
        // first pinned in a frame-owned local, so liveness follows the
        // downgrading scope rather than the source binding's last use -
        // the schedule the interpreter's downgrade pin keeps.
        if method.name.as_str() == "downgrade" && args.is_empty() {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let weak_ty = self.weak_adt_ty(recv_ty);
            let Some(pin_local) = self.pin_weak_referent(recv_local, recv_ty, span) else {
                // No RC allocation exists to observe (an opaque runtime
                // handle), so the weak is born dead: a null referent, which
                // every weak helper reads as `None`.
                let dest = self.fresh(weak_ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                return MethodLowering::Handled(Some(dest));
            };
            let dest = self.fresh(weak_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_rc_downgrade".to_string())),
                args: vec![Operand::Copy(Place::local(pin_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return MethodLowering::Handled(Some(dest));
        }
        // `w.upgrade()` - turn a `Weak<T>` back into `Option<T>`.
        // `gos_rt_rc_weak_upgrade_opt` packs `Some(payload)` when the
        // referent is still alive (`strong > 0`) and `None` otherwise,
        // as the `{disc, payload}` pair the standard match / if-let
        // discriminant read works on, on every tier. The Some payload
        // carries a fresh strong reference taken atomically inside the
        // shim (a CAS from a non-zero count for shared referents), so an
        // upgrade racing another goroutine's final release can never hand
        // out a dead pointer. That reference is pinned in a frame-owned
        // shadow local (`gos_rt_weak_opt_payload` extracts the payload
        // word, null for `None`), which the drop pass releases at scope
        // exit / reassignment - mirroring the interpreter, whose
        // `Some(value)` holds an `Arc` clone until its binding dies.
        if method.name.as_str() == "upgrade" && args.is_empty() {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let payload_ty = self.weak_payload_ty(recv_ty).unwrap_or(ty);
            let opt_ty = self.option_payload_adt_ty(payload_ty);
            let dest = self.fresh(opt_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_rc_weak_upgrade_opt".to_string())),
                args: vec![Operand::Copy(Place::local(recv_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            // The pin's type decides the release helper the drop pass emits
            // for it. A by-value aggregate payload lives in an RC cell, so
            // the pin holds the cell pointer, not aggregate slot data.
            let shadow_ty = if self.weak_referent_needs_cell(payload_ty) {
                self.weak_cell_adt_ty(payload_ty)
            } else {
                payload_ty
            };
            let shadow = self.fresh(shadow_ty);
            self.emit_assign(
                Place::local(shadow),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_weak_opt_payload",
                    args: vec![Operand::Copy(Place::local(dest))],
                },
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// Materialises the referent a `Weak` observes and leaves it owned by the
    /// enclosing frame. An RC-managed receiver is pinned by an ordinary owning
    /// copy; a by-value aggregate has no RC header, so its slot bytes are
    /// copied into an RC cell that becomes the referent. Returns `None` for a
    /// receiver with no RC allocation to observe at all.
    fn pin_weak_referent(&mut self, recv_local: Local, recv_ty: Ty, span: Span) -> Option<Local> {
        if self.tcx.is_rc_managed(recv_ty) && !self.tcx.is_weak_ty(recv_ty) {
            let pin = self.fresh(recv_ty);
            self.emit_assign(
                Place::local(pin),
                Rvalue::Use(Operand::Copy(Place::local(recv_local))),
                span,
            );
            return Some(pin);
        }
        if !self.weak_referent_needs_cell(recv_ty) {
            return None;
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let size_bytes = i128::from(self.type_slot_bytes(recv_ty));
        let meta_sym = self.ensure_aggr_struct_meta(recv_ty).unwrap_or_default();
        let size_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(size_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(size_bytes))),
            span,
        );
        let cell_ty = self.weak_cell_adt_ty(recv_ty);
        let cell = self.fresh(cell_ty);
        self.emit_assign(
            Place::local(cell),
            Rvalue::CallIntrinsic {
                name: "gos_rt_rc_weak_cell",
                args: vec![
                    Operand::Copy(Place::local(size_local)),
                    Operand::Const(ConstValue::Str(meta_sym)),
                    Operand::Copy(Place::local(recv_local)),
                ],
            },
            span,
        );
        Some(cell)
    }

    /// `d.as_millis()` / `inst.elapsed_ms()` - transparent time accessors.
    fn lower_time_unit_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `d.as_millis()` / `d.as_secs()` / `d.as_micros()` - method
        // form of the `time::Duration` accessors. The receiver's static
        // type carries the transparent Duration tag (its runtime value is
        // a bare `i64`), so route to the same `gos_rt_duration_*` helper
        // the qualified `time::Duration::as_millis(d)` free call uses.
        if matches!(method.name.as_str(), "as_millis" | "as_secs" | "as_micros") && args.is_empty()
        {
            let mut recv_kind = self.tcx.kind_of(receiver.ty).clone();
            while let TyKind::Ref { inner, .. } = recv_kind {
                recv_kind = self.tcx.kind_of(inner).clone();
            }
            // A `flag::Set` duration cell carries no Duration tag on its
            // HIR type (the typechecker leaves it an inference var); its
            // MIR binding is tagged `flag::Cell::Duration`. Auto-deref it
            // to the transparent i64-of-ms Duration local so the accessor
            // routes exactly like a plain `time::Duration` receiver.
            let is_duration_cell = self
                .receiver_local_from_path(receiver)
                .and_then(|l| self.local_runtime_kind.get(&l).copied())
                == Some("flag::Cell::Duration");
            if matches!(recv_kind, TyKind::Duration) || is_duration_cell {
                let sym = match method.name.as_str() {
                    "as_secs" => "gos_rt_duration_as_secs",
                    "as_micros" => "gos_rt_duration_as_micros",
                    _ => "gos_rt_duration_as_millis",
                };
                let Some(recv_local) = self.lower_expr(receiver) else {
                    return MethodLowering::Handled(None);
                };
                let recv_local = self.auto_deref_cell(recv_local, span);
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let dest = self.fresh(i64_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(sym.to_string())),
                    args: vec![Operand::Copy(Place::local(recv_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return MethodLowering::Handled(Some(dest));
            }
        }
        // `inst.elapsed_ms()` - method form of the `time::Instant`
        // accessor. The receiver's static type carries the transparent
        // Instant tag (its runtime value is a bare `i64` of monotonic ms),
        // so route to `gos_rt_time_since_ms`, the same helper the
        // qualified `time::Instant::elapsed_ms(inst)` free call uses.
        if method.name.as_str() == "elapsed_ms" && args.is_empty() {
            let mut recv_kind = self.tcx.kind_of(receiver.ty).clone();
            while let TyKind::Ref { inner, .. } = recv_kind {
                recv_kind = self.tcx.kind_of(inner).clone();
            }
            if matches!(recv_kind, TyKind::Instant) {
                let Some(recv_local) = self.lower_expr(receiver) else {
                    return MethodLowering::Handled(None);
                };
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let dest = self.fresh(i64_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_time_since_ms".to_string())),
                    args: vec![Operand::Copy(Place::local(recv_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return MethodLowering::Handled(Some(dest));
            }
        }
        MethodLowering::Pass
    }

    /// `h.join()` - block on a spawned goroutine's outcome.
    fn lower_join_handle_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `h.join()` - block on a spawned goroutine's outcome.
        // `gos_rt_join` recvs the SpawnOutcome over the handle's
        // one-shot channel and packs it into `Result<T, String>` (Ok
        // value, or Err panic message). Gated on a `JoinHandle`
        // receiver so a same-named user method or the string / Vec
        // `.join(sep)` (which takes a separator argument) is never
        // shadowed. Peek the receiver type first so the receiver is
        // lowered only when this arm actually consumes it.
        if method.name.as_str() == "join"
            && args.is_empty()
            && self
                .peek_struct_type(receiver)
                .is_some_and(|t| matches!(self.tcx.kind_of(t), TyKind::JoinHandle(_)))
        {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let elem = match self.tcx.kind_of(recv_ty).clone() {
                TyKind::JoinHandle(e) => e,
                _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
            };
            let result_ty = self.result_payload_string_error_ty(elem);
            let dest = self.fresh(result_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_join".to_string())),
                args: vec![Operand::Copy(Place::local(recv_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// `.clone()` on a `json::Value` receiver - identity copy keeping the tag.
    fn lower_json_clone_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `.clone()` on a `json::Value` receiver. The generic
        // identity-copy arm walks `match self.tcx.kind_of(ty)` and
        // falls through to `_ =>` for `JsonValue`, then the MIR
        // receiver-kind probe defaulted to `receiver_ty` (a `Var`
        // for chained accesses like `tcs[k].clone()`). The cloned
        // local then lost its `JsonValue` tag and downstream
        // `json::get(&clone_local, ...)` missed the json runtime
        // helper, returning the empty string. Short-circuit clone
        // on a JsonValue receiver to a direct copy with the
        // receiver's MIR type preserved.
        // `.clone()` on a `json::Value` receiver short-circuits to a
        // direct copy with the receiver's MIR type preserved (the
        // generic identity-copy arm later falls through to a Var dest
        // for `tcs[k].clone()` shapes and downstream json helpers stop
        // dispatching). Only lower the receiver when we know we'll
        // consume it here - falling through after the lower would
        // leave behind the receiver's lowered Call as dead but live
        // MIR, and any heap-container result (e.g. `gos_rt_vec_get_i64`
        // producing a `Vec<T>`-typed dest) would be marked twice for
        // `gos_rt_vec_free`, producing a double free at scope end.
        // A query on a parsed document is the same operation whichever way
        // it is spelled, so `doc.keys()` lowers exactly as
        // `json::keys(doc)` does - one helper, one return type, both tiers.
        if self.is_json_value_ty(receiver.ty)
            && matches!(
                method.name.as_str(),
                "get"
                    | "at"
                    | "keys"
                    | "len"
                    | "is_null"
                    | "as_str"
                    | "as_i64"
                    | "as_f64"
                    | "as_bool"
                    | "as_array"
                    | "render"
                    | "encode"
                    | "encode_pretty"
            )
        {
            let mut query_args = Vec::with_capacity(args.len() + 1);
            query_args.push(receiver.clone());
            query_args.extend(args.iter().cloned());
            if let Some(local) = self.lower_json_query(method.name.as_str(), &query_args, span) {
                return MethodLowering::Handled(Some(local));
            }
        }
        if method.name.as_str() == "clone" && args.is_empty() && self.is_json_value_ty(receiver.ty)
        {
            let Some(recv_local) = self.lower_expr(receiver) else {
                return MethodLowering::Handled(None);
            };
            let recv_mir_ty = self.locals[recv_local.0 as usize].ty;
            let dest = self.fresh(recv_mir_ty);
            if let Some(rk) = self.local_runtime_kind.get(&recv_local).copied() {
                self.local_runtime_kind.insert(dest, rk);
            }
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(recv_local))),
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// `result.map(_)` / `map_err(_)` when the lowered receiver is a Result Adt.
    fn lower_result_map_eager_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `result.map_err(closure)` / `result.map(closure)` when the
        // HIR receiver type is unresolved but its lowered MIR type
        // turns out to be a Result Adt. Without this short-circuit
        // the generic dispatch sees the unresolved kind, falls
        // through to the identity-copy arm, and silently drops the
        // mapping (errors.gos `text.parse().map_err(|_| …)?`
        // reproducer).
        if matches!(method.name.as_str(), "map_err" | "map") && args.len() == 1 {
            let receiver_ty_for_kind = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            if matches!(self.tcx.kind_of(receiver_ty_for_kind), TyKind::Var(_)) {
                if let Some(local) =
                    self.try_lower_result_map_with_eager_recv(receiver, method, &args[0], ty, span)
                {
                    return MethodLowering::Handled(Some(local));
                }
            }
        }
        MethodLowering::Pass
    }

    /// `let entries = m.iter()` on a map - materialise `Vec<(K, V)>`.
    fn lower_hashmap_iter_binding_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `let entries = m.iter()` on a HashMap or BTreeMap - materialise a real
        // `Vec<(K, V)>` of entries. The `for (k, v) in m.iter()` form
        // is lowered earlier in `try_lower_for_hashmap_iter`; this
        // direct-binding form would otherwise fall through to the
        // generic `gos_rt_arr_iter` dispatch, reinterpret the
        // `*mut GosMap` as a `*mut GosVec`, and segfault on the
        // compiled tiers. Materialising here makes both forms behave
        // identically across the VM, Cranelift, and LLVM tiers.
        if method.name.as_str() == "iter" && args.is_empty() {
            let mut recv_ty_for_kind = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            // Peel `&` / `&mut` so `m.iter()` on a `&HashMap` parameter is
            // recognised as a map receiver and materialised; otherwise it falls
            // through to the generic `gos_rt_arr_iter` path, which reads the map
            // handle as a `*mut GosVec`. The handle the runtime helpers receive
            // is the same value `m.len()` / `m.get_or()` already pass through a
            // borrow, so only the receiver-type check needs the peel.
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty_for_kind) {
                recv_ty_for_kind = *inner;
            }
            if matches!(self.tcx.kind_of(recv_ty_for_kind), TyKind::HashMap { .. }) {
                let entries = self.materialize_hashmap_entries(receiver, recv_ty_for_kind, span);
                // The pairs are read out under the map's lock, and the cursor
                // over them is what `iter()` answers: the value carries its own
                // position, so a downstream adapter runs per element pulled
                // rather than over the whole snapshot.
                let cursor = entries.and_then(|entries| self.entries_cursor(entries, span));
                return MethodLowering::Handled(cursor.or(entries));
            }
        }
        MethodLowering::Pass
    }

    /// `[].to_vec()` / `[a, b].to_vec()` on array-literal receivers.
    fn lower_array_to_vec_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `[].to_vec()` - the empty-array literal carries no
        // element type, so the generic `gos_rt_vec_clone` arm
        // produces a `GosVec { elem_bytes: 0, … }`. Subsequent
        // `.push(t)` allocates `0 * cap` bytes for the data
        // buffer; `xs[0]` then reads through a bogus offset
        // and segfaults. Detect the empty-array shape and pin
        // the dest's `elem_bytes` from the call's HIR return
        // type (`Vec<T>`) by emitting a direct
        // `gos_rt_vec_new(elem_bytes_for_T)` instead.
        if method.name.as_str() == "to_vec"
            && args.is_empty()
            && let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) = &receiver.kind
            && elems.is_empty()
        {
            let mut peeled = ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            let elem_ty_opt = match self.tcx.kind_of(peeled) {
                TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                TyKind::Array { elem, .. } => Some(*elem),
                _ => None,
            };
            if let Some(elem_ty) = elem_ty_opt {
                let elem_bytes = self.elem_bytes_of(elem_ty);
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let elem_bytes_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(elem_bytes_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(elem_bytes)))),
                    span,
                );
                let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
                let dest = self.fresh(vec_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_new".to_string())),
                    args: vec![Operand::Copy(Place::local(elem_bytes_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return MethodLowering::Handled(Some(dest));
            }
        }
        // `[a, b, c].to_vec()` on a non-empty literal-array
        // receiver. The default `to_vec` arm lowers to
        // `gos_rt_vec_clone(receiver)`, but `gos_rt_vec_clone`
        // expects a real `*const GosVec` header (len/cap/
        // elem_bytes/ptr). The lowered receiver is a stack
        // `[T; N]` aggregate whose first 24 bytes are the raw
        // payload - `gos_rt_vec_clone` then reads `elems[0]` as
        // `len`, `elems[1]` as `cap`, etc. and either segfaults
        // or panics with a bogus `memory allocation of <huge>
        // bytes failed` when the runtime tries to copy that
        // many bytes. Detect the literal-array shape, lower the
        // elements normally, and route through the existing
        // `gos_rt_vec_from_arr(elem_bytes, &arr, len)` shim that
        // builds a real `GosVec` header around the stack
        // payload. Mirrors `coerce_arg_for_binding`'s `[T; N] →
        // Vec<T>` fix for binding calls.
        if method.name.as_str() == "to_vec"
            && args.is_empty()
            && let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) = &receiver.kind
            && !elems.is_empty()
        {
            let dest = self.fresh(ty);
            return if self.lower_let_array_as_vec(dest, elems, span) {
                MethodLowering::Handled(Some(dest))
            } else {
                MethodLowering::Handled(None)
            };
        }
        MethodLowering::Pass
    }

    /// `arr.swap(i, j)` / `xs.sort_by(closure)` in-place array operations.
    fn lower_array_mutation_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // `arr.swap(i, j)` super-instruction. The generic Call
        // fallback at the end of this function would lower this as
        // `Call(Const(Str("swap")), …)` which the cranelift backend
        // can't resolve - JIT- and AOT-compiled bodies silently
        // produced a typed-zero stub, leaving the receiver
        // unmutated. Inlining as four index ops (read i, read j,
        // write j-into-i, write i-into-j) keeps the semantics
        // intact across every backend.
        if method.name.as_str() == "swap" && args.len() == 2 {
            if let Some(swap_local) =
                self.try_lower_array_swap(receiver, &args[0], &args[1], ty, span)
            {
                return MethodLowering::Handled(Some(swap_local));
            }
        }
        if method.name.as_str() == "sort"
            && args.is_empty()
            && let Some(local) = self.try_lower_tuple_sort(receiver, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if matches!(method.name.as_str(), "sort" | "reverse")
            && args.is_empty()
            && let Some(local) =
                self.try_lower_fixed_array_ordering(receiver, method.name.as_str(), span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if method.name.as_str() == "fill"
            && let [value] = args
            && let Some(local) = self.try_lower_sequence_fill(receiver, value, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if method.name.as_str() == "resize"
            && let [new_len, value] = args
            && let Some(local) = self.try_lower_vec_resize(receiver, new_len, value, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        if matches!(
            method.name.as_str(),
            "copy_within" | "copy_from_slice" | "binary_search"
        ) && let Some(local) = self.try_lower_vec_bulk_method(receiver, method, args, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        // `xs.sort_by(closure)` for `[i64; N]` / `[i64]` / `Vec<i64>`.
        // Routes through one of two runtime helpers depending on
        // the receiver shape: fixed buffers go through
        // `gos_rt_arr_sort_by_i64(ptr, len, env)`; Vec receivers
        // through `gos_rt_vec_sort_by_i64(vec, env)`. Both load the
        // closure body address from `env[0]` and forward the
        // `(env, *const T, *const T) -> i64` callback.
        if method.name.as_str() == "sort_by" && args.len() == 1 {
            if let Some(local) = self.try_lower_sort_by(receiver, &args[0], ty, span) {
                return MethodLowering::Handled(Some(local));
            }
        }
        MethodLowering::Pass
    }

    /// Map counter idioms: fused `insert(k, get_or+by)`, `inc`, struct-keyed ops.
    fn lower_map_idiom_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        // Fused-increment peephole: `m.insert(k, m.get_or(k, 0)
        // + by)` (or `… + 1`) on an i64-keyed map collapses into
        // a single `gos_rt_map_inc_i64(m, k, by)` call. Halves
        // the lock + hash work on every counter-style loop.
        if method.name.as_str() == "insert" && args.len() == 2 {
            if let Some(local) = self.try_lower_map_inc(receiver, &args[0], &args[1], ty, span) {
                return MethodLowering::Handled(Some(local));
            }
        }
        // `m.inc(key)` / `m.inc(key, by)` for `HashMap<String, i64>`.
        // The interpreter ships a dedicated counter idiom; the
        // compiled tier needs a matching dispatch or values stay
        // at zero. Default `by` to 1 when only the key is given.
        if method.name.as_str() == "inc" && (args.len() == 1 || args.len() == 2) {
            let recv_ty_local = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            let val_kind = self.hash_map_value_kind(recv_ty_local);
            let key_kind = self.hash_map_key_kind(recv_ty_local);
            if std::env::var("GOS_DEBUG_FALLBACK").is_ok() {
                let (key_dbg, val_dbg) = match self.tcx.kind_of(recv_ty_local) {
                    gossamer_types::TyKind::HashMap { key, value, .. } => (
                        format!("{:?}", self.tcx.kind_of(*key)),
                        format!("{:?}", self.tcx.kind_of(*value)),
                    ),
                    other => (format!("{other:?}"), String::new()),
                };
                eprintln!(
                    "inc gate: key={key_dbg} value={val_dbg} val_is_i64={} key_str={} key_i64={}",
                    matches!(val_kind, Some(MapValueKind::I64)),
                    matches!(key_kind, Some(MapKeyKind::String)),
                    matches!(key_kind, Some(MapKeyKind::I64)),
                );
            }
            if matches!(val_kind, Some(MapValueKind::I64)) {
                let (fn_name, key_kind_ok) = match key_kind {
                    Some(MapKeyKind::String) => ("gos_rt_map_inc_typed_str_i64", true),
                    Some(MapKeyKind::I64) => ("gos_rt_map_inc_i64", true),
                    _ => ("", false),
                };
                if key_kind_ok {
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    let Some(recv_local) = self.lower_expr(receiver) else {
                        return MethodLowering::Handled(None);
                    };
                    let Some(key_local) = self.lower_expr(&args[0]) else {
                        return MethodLowering::Handled(None);
                    };
                    let by_local = if args.len() == 2 {
                        match self.lower_expr(&args[1]) {
                            Some(v) => v,
                            None => return MethodLowering::Handled(None),
                        }
                    } else {
                        let l = self.fresh(i64_ty);
                        self.emit_assign(
                            Place::local(l),
                            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                            span,
                        );
                        l
                    };
                    let dest = self.fresh(i64_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(fn_name.to_string())),
                        args: vec![
                            Operand::Copy(Place::local(recv_local)),
                            Operand::Copy(Place::local(key_local)),
                            Operand::Copy(Place::local(by_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return MethodLowering::Handled(Some(dest));
                }
            }
        }
        // Map operations on a HashMap keyed by a flat aggregate: hash the
        // key's content bytes (the VM value-keys; the compiled tier would
        // otherwise use the key's pointer and miss on a distinct allocation
        // of an equal value).
        if matches!(
            method.name.as_str(),
            "insert"
                | "get"
                | "contains_key"
                | "contains"
                | "pop"
                | "remove"
                | "get_or"
                | "or_insert"
                | "inc"
        ) && let Some(local) =
            self.try_lower_struct_key_map_op(receiver, method.name.as_str(), args, span)
        {
            return MethodLowering::Handled(Some(local));
        }
        MethodLowering::Pass
    }

    /// `true` for the methods that rebind a `String` receiver in place.
    /// They own their receiver's write-back, so the receiver reaches them
    /// as the reference it was written as.
    fn rebinds_receiver_place(method: &Ident) -> bool {
        matches!(
            method.name.as_str(),
            "push_str" | "push" | "push_char" | "push_byte" | "push_utf8" | "clear" | "truncate"
        )
    }

    /// The pointee type when `receiver` denotes a `&mut <scalar / String>`
    /// place - the shapes a reference addresses by slot rather than by
    /// value. `None` for every other receiver, including a shared `&T`,
    /// which already holds the value's own pointer.
    fn mut_slot_receiver_pointee(&self, receiver: &HirExpr) -> Option<Ty> {
        let ref_ty = if matches!(self.tcx.kind_of(receiver.ty), TyKind::Ref { .. }) {
            receiver.ty
        } else {
            let local = self.receiver_local_from_path(receiver)?;
            self.locals[local.0 as usize].ty
        };
        self.mut_slot_pointee(ref_ty)
    }

    /// The place a receiver's value lives in: the slot a
    /// `&mut <scalar / String>` addresses, and the local itself for every
    /// other receiver. Reading and writing the same place keeps the rebind
    /// in the shape the drop schedule already reasons about - the value a
    /// consuming helper answers with moves into the place it came from.
    fn receiver_slot_place(&self, local: Local) -> Place {
        if self.mut_slot_pointee_of_local(local).is_some() {
            Place {
                local,
                projection: vec![crate::ir::Projection::Deref],
            }
        } else {
            Place::local(local)
        }
    }

    /// Releases the String a receiver's slot currently holds. The push family
    /// hands its receiver to a helper that consumes it, so the replacement
    /// carries that share on; `clear` and `truncate` only read the receiver,
    /// which leaves the displaced value for the writeback to account for.
    fn release_receiver_slot(&mut self, recv_local: Local, span: Span) {
        if self.mut_slot_pointee_of_local(recv_local).is_none() {
            return;
        }
        let place = self.receiver_slot_place(recv_local);
        let unit_ty = self.tcx.unit();
        let rel = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(rel),
            Rvalue::CallIntrinsic {
                name: "gos_rt_rc_release",
                args: vec![Operand::Copy(place)],
            },
            span,
        );
    }

    /// Publishes a rebound receiver value into the place the receiver was
    /// read from.
    fn store_receiver_slot(&mut self, recv_local: Local, value: Local, span: Span) {
        let place = self.receiver_slot_place(recv_local);
        self.emit_assign(place, Rvalue::Use(Operand::Copy(Place::local(value))), span);
    }

    /// Pointee of a local typed `&mut <scalar / String>`.
    fn mut_slot_pointee_of_local(&self, local: Local) -> Option<Ty> {
        self.mut_slot_pointee(self.locals[local.0 as usize].ty)
    }

    /// Pointee of `&mut T` when `T` is a shape a reference addresses by
    /// slot: a scalar, a `String`, or one of the transparent `i64` time
    /// newtypes. `None` for a shared reference or any other referent.
    fn mut_slot_pointee(&self, ty: Ty) -> Option<Ty> {
        let TyKind::Ref {
            inner,
            mutability: gossamer_types::Mutbl::Mut,
        } = self.tcx.kind_of(ty)
        else {
            return None;
        };
        let inner = *inner;
        matches!(
            self.tcx.kind_of(inner),
            TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Duration
                | TyKind::Instant
        )
        .then_some(inner)
    }

    /// `s.push_str/push/push_char/push_byte(_)` on an owned `String` receiver.
    fn lower_string_push_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        // `b.push_str(s)` on an owned `String` receiver. The runtime takes
        // ownership of the accumulator and may grow its unique buffer in
        // place, returning the replacement pointer for receiver writeback.
        // This is materially different from lowering to `__concat`: callers
        // using `String::with_capacity` (streaming JSON, encoders, log
        // builders) must not copy the whole prefix for every append.
        if method.name.as_str() == "push_str"
            && args.len() == 1
            && let Some(recv_local) = self.receiver_local_from_path(receiver)
        {
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let mut peeled = recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if matches!(self.tcx.kind_of(peeled), TyKind::String) {
                let literal_len = match &args[0].kind {
                    HirExprKind::Literal(gossamer_hir::HirLiteral::String(text)) => {
                        Some(text.len() as i128)
                    }
                    _ => None,
                };
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let recv_place = self.receiver_slot_place(recv_local);
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                let (callee, call_args) = match literal_len {
                    Some(len) => (
                        "gos_rt_str_append_bytes",
                        vec![
                            Operand::Copy(recv_place.clone()),
                            Operand::Copy(Place::local(arg_local)),
                            Operand::Const(ConstValue::Int(len)),
                        ],
                    ),
                    None => (
                        "gos_rt_str_concat_drop_a",
                        vec![
                            Operand::Copy(recv_place.clone()),
                            Operand::Copy(Place::local(arg_local)),
                        ],
                    ),
                };
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee.to_string())),
                    args: call_args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.store_receiver_slot(recv_local, dest, span);
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.push_utf8(buf, start, end)` appends a validated UTF-8 window of a
        // byte buffer straight onto the receiver's storage. The shim answers a
        // carrier: the payload is the pointer the receiver takes, and the
        // discriminant is the `bool` the call evaluates to.
        if method.name.as_str() == "push_utf8"
            && args.len() == 3
            && let Some(recv_local) = self.receiver_local_from_path(receiver)
        {
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let mut peeled = recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if matches!(self.tcx.kind_of(peeled), TyKind::String) {
                let mut lowered = Vec::with_capacity(3);
                for arg in args {
                    let Some(a) = self.lower_expr(arg) else {
                        return MethodLowering::Handled(None);
                    };
                    lowered.push(a);
                }
                let recv_place = self.receiver_slot_place(recv_local);
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                // The shim answers the two-word carrier: the payload is the
                // pointer the receiver takes and the discriminant is the flag.
                let carrier_ty = self.result_of(peeled);
                let carrier = self.fresh(carrier_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_utf8".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(lowered[0])),
                        Operand::Copy(Place::local(lowered[1])),
                        Operand::Copy(Place::local(lowered[2])),
                    ],
                    destination: Place::local(carrier),
                    target: Some(next),
                });
                self.set_current(next);
                let updated = self.fresh(peeled);
                self.emit_assign(
                    Place::local(updated),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_payload",
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                self.store_receiver_slot(recv_local, updated, span);
                let disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(disc),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_disc",
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                let bool_ty = self.tcx.bool_ty();
                let ok = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(ok),
                    Rvalue::BinaryOp {
                        op: crate::ir::BinOp::Eq,
                        lhs: Operand::Copy(Place::local(disc)),
                        rhs: Operand::Const(ConstValue::Int(0)),
                    },
                    span,
                );
                return MethodLowering::Handled(Some(ok));
            }
        }
        // `s.push(ch)` on an owned `String` receiver uses the growable-string
        // mutation helper. It consumes the old receiver reference, mutates a
        // unique buffer when capacity permits, and returns the pointer to
        // write back. The unqualified `push` arm below routes Vec receivers
        // to `gos_rt_vec_push`; this block claims only String ones.
        if method.name.as_str() == "push"
            && args.len() == 1
            && let Some(recv_local) = self.receiver_local_from_path(receiver)
        {
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let mut peeled = recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if matches!(self.tcx.kind_of(peeled), TyKind::String) {
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let recv_place = self.receiver_slot_place(recv_local);
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_char".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(arg_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.store_receiver_slot(recv_local, dest, span);
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.push_char(c)` on a String receiver. Same receiver-rebind
        // contract as `push`; dispatches to `gos_rt_str_push_char` which
        // interprets the argument as a Unicode codepoint.
        if method.name.as_str() == "push_char"
            && args.len() == 1
            && let Some(recv_local) = self.receiver_local_from_path(receiver)
        {
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let mut peeled = recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if matches!(self.tcx.kind_of(peeled), TyKind::String) {
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let recv_place = self.receiver_slot_place(recv_local);
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_char".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(arg_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.store_receiver_slot(recv_local, dest, span);
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.push_byte(b)` on a String receiver. Same receiver-rebind
        // contract as `push`; dispatches to `gos_rt_str_push_byte` which
        // interprets the argument as a raw byte value.
        if method.name.as_str() == "push_byte"
            && args.len() == 1
            && let Some(recv_local) = self.receiver_local_from_path(receiver)
        {
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let mut peeled = recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if matches!(self.tcx.kind_of(peeled), TyKind::String) {
                let Some(arg_local) = self.lower_expr(&args[0]) else {
                    return MethodLowering::Handled(None);
                };
                let recv_place = self.receiver_slot_place(recv_local);
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_push_byte".to_string())),
                    args: vec![
                        Operand::Copy(recv_place.clone()),
                        Operand::Copy(Place::local(arg_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.store_receiver_slot(recv_local, dest, span);
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        // `s.clear()` / `s.truncate(n)` on a String receiver. Strings are
        // immutable runtime byte buffers, so mutation is modeled as a fresh
        // string plus receiver-local writeback, matching the push family.
        if matches!(method.name.as_str(), "clear" | "truncate")
            && (method.name.as_str() == "clear" && args.is_empty()
                || method.name.as_str() == "truncate" && args.len() == 1)
            && let Some(recv_local) = self.receiver_local_from_path(receiver)
        {
            let recv_ty = self.locals[recv_local.0 as usize].ty;
            let mut peeled = recv_ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if matches!(self.tcx.kind_of(peeled), TyKind::String) {
                let recv_place = self.receiver_slot_place(recv_local);
                let mut call_args = vec![Operand::Copy(recv_place.clone())];
                let rt = if method.name.as_str() == "clear" {
                    call_args.clear();
                    "gos_rt_str_clear"
                } else {
                    let Some(arg_local) = self.lower_expr(&args[0]) else {
                        return MethodLowering::Handled(None);
                    };
                    call_args.push(Operand::Copy(Place::local(arg_local)));
                    "gos_rt_str_truncate"
                };
                let dest = self.fresh(peeled);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(rt.to_string())),
                    args: call_args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.release_receiver_slot(recv_local, span);
                self.store_receiver_slot(recv_local, dest, span);
                return MethodLowering::Handled(Some(self.lower_unit(span)));
            }
        }
        MethodLowering::Pass
    }

    /// Ground-truth type of a `<parent>.<field>` receiver whose `parent` is a
    /// bound local of struct type: the field's declared type from the struct
    /// definition, with the instantiation's generic arguments applied. The HIR
    /// type of such a field access can be left degraded - a match-payload
    /// binding (`match r { Ok(m) => m.tags... }`) loses the field's generic
    /// substitution - which map key/value dispatch then reads as `i64`, sending
    /// `HashMap<String, _>` accessors to the integer-keyed helpers. The parent
    /// struct's declared field type is ground truth. `None` for non-field
    /// receivers, an unresolvable parent, or a non-concrete declared type (a
    /// generic template's rigid `Param`, where the HIR type is more specific).
    pub(crate) fn field_declared_ty(&self, receiver: &HirExpr) -> Option<Ty> {
        let HirExprKind::Field {
            receiver: parent,
            name: field,
        } = &receiver.kind
        else {
            return None;
        };
        let parent_local = self.receiver_local_from_path(parent)?;
        let mut pty = self.locals[parent_local.0 as usize].ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(pty) {
            pty = *inner;
        }
        let TyKind::Adt { def, substs } = self.tcx.kind_of(pty).clone() else {
            return None;
        };
        let sname = self.struct_defs.get(&def).cloned()?;
        let order = self.structs.get(&sname).cloned()?;
        let pos = order.iter().position(|f| f == &field.name)?;
        let field_ty = self
            .tcx
            .adt_field_tys(def, &substs)
            .and_then(|t| t.get(pos).copied())?;
        if matches!(
            self.tcx.kind_of(field_ty),
            TyKind::Var(_) | TyKind::Error | TyKind::Param { .. }
        ) {
            return None;
        }
        Some(field_ty)
    }

    /// Recover the receiver's flattened dispatch kind and its ground type.
    fn receiver_dispatch_kinds(&mut self, receiver: &HirExpr) -> (Ty, TyKind) {
        let mut receiver_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |local| self.locals[local.0 as usize].ty);
        let receiver_kind = self.tcx.kind_of(receiver_ty).clone();
        // Unwrap a leading `&T` so `s.len()` on a `&String`
        // parameter lowers the same as on an owned `String`.
        let mut receiver_kind_flat = match &receiver_kind {
            TyKind::Ref { inner, .. } => self.tcx.kind_of(*inner).clone(),
            other => other.clone(),
        };
        // `(*flags.<long>).method(...)` - the HIR receiver type is
        // an unresolved inference variable, but the underlying cell
        // kind is known statically from `local_define_layout`.
        // Promote the receiver kind so method dispatch (`to_string`,
        // `len`, …) picks the right runtime helper.
        if matches!(receiver_kind_flat, TyKind::Var(_)) {
            if let Some(kind) = self.peek_define_deref_kind(receiver) {
                receiver_kind_flat = kind;
            }
        }
        // `<chain>.method().to_string()` - when the chain ends in
        // a call whose return shape is pinned (`len`, `parse`,
        // `to_string`, integer-yielding helpers), surface that
        // shape so downstream `.to_string()` dispatches through
        // the i64/f64 runtime formatter instead of the
        // identity-copy arm. Expression-only walk; emits no MIR.
        if matches!(receiver_kind_flat, TyKind::Var(_)) {
            if let Some(kind) = self.peek_method_chain_kind(receiver) {
                receiver_kind_flat = kind;
            }
        }
        // `r.query.len()` / other methods on a field of an opaque
        // runtime-kind struct (`http::Request` / `http::Response`):
        // the field expression's HIR type is an inference Var (the
        // structs are checker-opaque), but the field-accessor table
        // knows the static type. Without this, `.len()` falls to the
        // len-prefixed `gos_rt_len` and dereferences a c-string -
        // a misaligned-pointer abort on the first proxied request.
        if matches!(receiver_kind_flat, TyKind::Var(_))
            && let HirExprKind::Field {
                receiver: obj,
                name: fname,
            } = &receiver.kind
            && let Some(rk) = self
                .receiver_local_from_path(obj)
                .and_then(|l| self.local_runtime_kind.get(&l).copied())
                .or_else(|| self.expr_runtime_kind(obj))
            && let Some(field_ty) = self.runtime_field_static_ty(rk, fname.name.as_str())
        {
            receiver_kind_flat = self.tcx.kind_of(field_ty).clone();
        }
        // `args[i].method()` - when typeck resolves the Index
        // expression to its base collection (Vec / Slice / Array)
        // instead of the element type (a multi-module typeck
        // regression: single-file builds correctly type
        // `args[i]` as `String`, but with `mod util;` the HIR
        // node retains the base `Vec<String>`), prefer the
        // element kind taken from the base local's MIR type.
        // Without this, `.len()` on `args[i]` lands on the
        // collection arm `gos_rt_arr_len`, which then crashes
        // inside `mov (%rdi),%rax` reading a Vec header out of a
        // `*const c_char` string pointer.
        let needs_index_fixup = matches!(
            receiver_kind_flat,
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } | TyKind::Var(_)
        );
        if needs_index_fixup {
            if let HirExprKind::Index { base, .. } = &receiver.kind {
                if let Some(base_ty) = self.peek_collection_type(base) {
                    let elem_ty = match self.tcx.kind_of(base_ty) {
                        TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                        TyKind::Array { elem, .. } => Some(*elem),
                        _ => None,
                    };
                    if let Some(elem_ty) = elem_ty {
                        let mut elem_kind = self.tcx.kind_of(elem_ty).clone();
                        while let TyKind::Ref { inner, .. } = elem_kind {
                            elem_kind = self.tcx.kind_of(inner).clone();
                        }
                        if !matches!(elem_kind, TyKind::Var(_)) {
                            receiver_kind_flat = elem_kind;
                        }
                    }
                }
            }
        }

        // `<recv>.<field>.method()` - the field-access HIR type can
        // be wrongly resolved to `String` (e.g. a `match Ok(q) =>
        // q.bytes.len()` binding where the field came back as
        // `String` instead of `[u8]`, sending `.len()` to strlen and
        // reading the i64-per-element Vec as a c-string), or left with
        // a degraded generic substitution (a `HashMap<String, _>`
        // field reached through a match binding, whose key/value substs
        // are lost). The parent struct's *declared* field type is
        // ground truth - recover the full type so both the receiver
        // kind AND `receiver_ty` (which map key/value dispatch reads the
        // substitution from) are correct. Ungated (the HIR type may be
        // concrete-but-wrong, not just `Var`).
        if let Some(field_ty) = self.field_declared_ty(receiver) {
            receiver_ty = field_ty;
            let mut k = self.tcx.kind_of(field_ty).clone();
            while let TyKind::Ref { inner, .. } = k {
                k = self.tcx.kind_of(inner).clone();
            }
            receiver_kind_flat = k;
        }
        (receiver_ty, receiver_kind_flat)
    }

    fn stdlib_runtime_kind_from_kind(kind: &TyKind) -> Option<&'static str> {
        match kind {
            TyKind::Adt { def, .. } if def.local == HASH_SET_DEF_LOCAL => {
                Some("collections::HashSet")
            }
            TyKind::Adt { def, .. } if def.local == BTREE_SET_DEF_LOCAL => {
                Some("collections::BTreeSet")
            }
            TyKind::Adt { def, .. } if def.local == BINARY_HEAP_DEF_LOCAL => {
                Some("collections::MaxHeap")
            }
            TyKind::Adt { def, .. } if def.local == MIN_HEAP_DEF_LOCAL => {
                Some("collections::MinHeap")
            }
            TyKind::Adt { def, .. } if def.local == VEC_QUEUE_DEF_LOCAL => {
                Some("collections::VecQueue")
            }
            TyKind::Adt { def, .. } if def.local == VEC_STACK_DEF_LOCAL => {
                Some("collections::VecStack")
            }
            TyKind::Adt { def, .. } if def.local == VALIDATE_ERRORS_DEF_LOCAL => {
                Some("validate::Errors")
            }
            TyKind::Adt { def, .. } if def.local == VALIDATE_FIELD_ERROR_DEF_LOCAL => {
                Some("validate::FieldError")
            }
            // A handle that arrives as a parameter carries no construction
            // site to recover the kind from, so the sentinel itself has to
            // answer it - otherwise `fn f(p: &regex::Pattern)` dispatches by
            // bare name, which the VM has and the compiled tiers do not.
            TyKind::Adt { def, .. } if def.local == u32::MAX - 26 => Some("regex::Pattern"),
            _ => None,
        }
    }

    /// Fold `recv.headers.insert/get(...)` into a single runtime header call.
    fn lower_headers_fold_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> MethodLowering {
        if let HirExprKind::Field {
            receiver: inner,
            name: field_name,
        } = &receiver.kind
        {
            if field_name.name.as_str() == "headers" {
                let inner_local_for_kind = self.receiver_local_from_path(inner);
                let inner_kind = inner_local_for_kind
                    .and_then(|l| self.local_runtime_kind.get(&l).copied())
                    .or_else(|| {
                        let inner_ty =
                            inner_local_for_kind.map_or(inner.ty, |l| self.locals[l.0 as usize].ty);
                        match self.tcx.kind_of(inner_ty) {
                            TyKind::Ref { inner: i, .. } => self.struct_name_of(*i),
                            _ => self.struct_name_of(inner_ty),
                        }
                        .and_then(|s| match s.as_str() {
                            "Response" => Some("http::Response"),
                            "Request" => Some("http::Request"),
                            _ => None,
                        })
                    });
                if matches!(inner_kind, Some("http::Response" | "http::Request")) {
                    let helper = match (inner_kind, method.name.as_str()) {
                        (Some("http::Response"), "insert") => {
                            Some(("gos_rt_http_response_set_header", self.tcx.unit(), 2usize))
                        }
                        (Some("http::Response"), "get") => Some((
                            "gos_rt_http_response_get_header",
                            self.tcx.string_ty(),
                            1usize,
                        )),
                        (Some("http::Request"), "insert") => {
                            Some(("gos_rt_http_request_set_header", self.tcx.unit(), 2usize))
                        }
                        (Some("http::Request"), "get") => Some((
                            "gos_rt_http_request_get_header",
                            self.tcx.string_ty(),
                            1usize,
                        )),
                        _ => None,
                    };
                    if let Some((rt, ret_ty, want_args)) = helper {
                        if args.len() == want_args {
                            let Some(inner_local) = self.lower_expr(inner) else {
                                return MethodLowering::Handled(None);
                            };
                            let mut ops = Vec::with_capacity(args.len() + 1);
                            ops.push(Operand::Copy(Place::local(inner_local)));
                            for a in args {
                                let Some(al) = self.lower_expr(a) else {
                                    return MethodLowering::Handled(None);
                                };
                                ops.push(Operand::Copy(Place::local(al)));
                            }
                            let dest = self.fresh(ret_ty);
                            let next = self.new_block(span);
                            self.terminate(Terminator::Call {
                                callee: Operand::Const(ConstValue::Str(rt.to_string())),
                                args: ops,
                                destination: Place::local(dest),
                                target: Some(next),
                            });
                            self.set_current(next);
                            return MethodLowering::Handled(Some(dest));
                        }
                    }
                }
            }
        }
        MethodLowering::Pass
    }

    /// `.len()` on a fixed-size `[T; N]` array - a compile-time constant.
    fn lower_fixed_array_len_method(
        &mut self,
        method: &Ident,
        args: &[HirExpr],
        receiver_kind_flat: &TyKind,
        span: Span,
    ) -> MethodLowering {
        let receiver_kind_flat = receiver_kind_flat.clone();
        // A tuple's element count is part of its type, exactly like a fixed
        // array's length, so both fold to a constant here. Without the
        // tuple arm the dispatch fell through to the generic vec-header
        // read, which reported 1.
        let constant_len = match &receiver_kind_flat {
            TyKind::Array { len, .. } => Some(len.to_usize()),
            TyKind::Tuple(elems) => Some(elems.len()),
            _ => None,
        };
        if method.name.as_str() == "len"
            && args.is_empty()
            && let Some(n) = constant_len
        {
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let dest = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Const(ConstValue::Int(n as i128))),
                span,
            );
            return MethodLowering::Handled(Some(dest));
        }
        MethodLowering::Pass
    }

    /// Name-keyed runtime-symbol dispatch table (`Bail` = stop the lowering).
    #[allow(
        clippy::too_many_lines,
        reason = "flat method-name dispatch table; arm order encodes guarded/unguarded same-name shadowing"
    )]
    /// True when the receiver's own type declares `method` in an `impl`.
    ///
    /// Keyed by the type's registered identity, so a method on a type
    /// declared inside a module is found under the same name its `impl`
    /// registered.
    fn user_impl_method_exists(&self, receiver_ty: Ty, receiver: &HirExpr, method: &str) -> bool {
        [
            self.adt_dispatch_name(receiver_ty),
            self.struct_name_from_expr(receiver),
            self.builtin_impl_owner_name(receiver_ty),
        ]
        .into_iter()
        .flatten()
        .any(|owner| {
            // A core type's own surface answers before an `impl` block that
            // spells one of its names, so such a block adds names to the type
            // rather than replacing any it already had. A declared type has no
            // surface of its own here, and a primitive's is not one an `impl`
            // can collide with, so both keep the block they name.
            !gossamer_types::core_type_declares_method(&owner, method)
                && self
                    .impl_methods
                    .contains_key(&format!("{owner}::{method}"))
        })
    }

    fn runtime_symbol_by_name(
        &self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        receiver_kind_flat: &TyKind,
        receiver_ty: Ty,
    ) -> SymbolLookup {
        let receiver_kind_flat = receiver_kind_flat.clone();
        // A user `impl` method wins over every builtin of the same name. The
        // table below keys on method name first and receiver kind second, and
        // several arms end in a catch-all that would take a user type with
        // it - `len` on an enum reaching `gos_rt_len`, which reads a Vec
        // header out of the enum's pointer.
        if self.user_impl_method_exists(receiver_ty, receiver, &method.name) {
            return SymbolLookup::Found(None);
        }
        // The open dynamic value answers its whole surface through the
        // `gos_rt_dyn_*` family, on every tier. Matched ahead of the shared
        // names below (`at`, `len`, `as_i64`, ...) so a `DynValue` receiver
        // never falls into another type's helper.
        if matches!(&receiver_kind_flat, TyKind::DynValue) {
            if let Some(symbol) = match method.name.as_str() {
                "kind" => Some("gos_rt_dyn_kind_name"),
                "name" => Some("gos_rt_dyn_name"),
                "len" => Some("gos_rt_dyn_len"),
                "at" => Some("gos_rt_dyn_at"),
                "key_at" => Some("gos_rt_dyn_key_at"),
                "as_i64" => Some("gos_rt_dyn_as_i64"),
                "as_f64" => Some("gos_rt_dyn_as_f64"),
                "as_bool" => Some("gos_rt_dyn_as_bool"),
                "as_char" => Some("gos_rt_dyn_as_char"),
                "as_str" => Some("gos_rt_dyn_as_str"),
                "as_bytes" => Some("gos_rt_dyn_as_bytes"),
                "clone" => Some("gos_rt_dyn_clone"),
                "to_string" => Some("gos_rt_dyn_format"),
                _ => None,
            } {
                return SymbolLookup::Found(Some(symbol));
            }
        }
        SymbolLookup::Found(match method.name.as_str() {
            // `.to_string()` routes to the runtime numeric
            // formatter for integer / float receivers. String
            // receivers fall through to the identity copy.
            // `to_string()` (no args) - scalar-to-string for
            // integer / float receivers; identity copy for the
            // others.
            //
            // `to_string(len)` (1 arg) - the canonical "freeze the
            // build buffer" step at the end of a `U8Vec`-backed
            // incremental construction loop. Mirrors F#'s
            // `StringBuilder.ToString()` and Rust's
            // `String::from_utf8(vec).unwrap()`. Routes to a
            // runtime helper that copies the first `len` bytes
            // into a fresh immutable `String`.
            "to_string" => {
                if args.len() == 1 {
                    Some("gos_rt_heap_u8_to_string")
                } else {
                    match &receiver_kind_flat {
                        TyKind::Int(_) => Some("gos_rt_i64_to_str"),
                        TyKind::Float(_) => Some("gos_rt_f64_to_str"),
                        // A `char` is a scalar Unicode value, so its String
                        // form is built rather than reinterpreted.
                        TyKind::Char => Some("gos_rt_char_to_str"),
                        TyKind::Bool => Some("gos_rt_bool_to_str"),
                        _ => Some(""),
                    }
                }
            }
            "clone" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Slice(_) => Some("gos_rt_vec_clone"),
                // A `String` clone takes a share of its own. Lowering it
                // to nothing left the clone and its source as one value
                // with one share, so a consuming callee given the clone
                // took the caller's.
                TyKind::String => Some("gos_rt_str_clone"),
                _ => Some(""),
            },
            "extend" | "extend_from_slice" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_extend"),
                _ => None,
            },
            "truncate" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_truncate"),
                _ => None,
            },
            "reserve" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_reserve_at_least"),
                _ => None,
            },
            "reserve_exact" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_reserve_exact"),
                _ => None,
            },
            "capacity" if args.is_empty() => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_capacity"),
                _ => None,
            },
            // Option / Result methods. Result/Option now live as
            // `*mut GosResult { disc, payload }` heap aggregates
            // (see `gos_rt_result_new`), so `.unwrap()` /
            // `.unwrap_or()` / `.ok()` / `.err()` route through
            // runtime helpers that read the disc and return the
            // payload (or default) as a raw 64-bit slot. The
            // older identity-copy path was a leftover from the
            // pre-discriminator layout and silently returned the
            // aggregate pointer for callers expecting an i64 -
            // see e.g. fasta's `args[0].to_i64().unwrap_or(1000)`,
            // which yielded an arena address instead of 10. Fall
            // back to identity for non-Result receivers (e.g.
            // stdlib helpers that still return raw inner values
            // tagged with a Result-shaped HIR type).
            "unwrap" | "expect" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    if self.is_option_adt(receiver_ty) {
                        Some("gos_rt_option_unwrap")
                    } else {
                        Some("gos_rt_result_unwrap")
                    }
                } else {
                    Some("")
                }
            }
            "unwrap_or" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    // A payload that is itself a carrier was boxed at
                    // construction, so it is loaded back rather than
                    // handed over as the address the word helper returns.
                    if self.carrier_payload_is_carrier(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_carrier")
                    } else if self.carrier_payload_is_sequence(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_vec")
                    } else if self.carrier_payload_is_string(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_str")
                    } else {
                        Some("gos_rt_result_unwrap_or")
                    }
                } else {
                    Some("")
                }
            }
            // `.ok()` / `.err()` answer an `Option`, so they take the carrier
            // helpers the `result::ok` / `result::err` free forms use. The
            // payload-reading helpers of the same name are the `unwrap` family
            // and hand back a bare word, which a `match` then reads as a
            // discriminant.
            "ok" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    Some("gos_rt_result_to_opt_ok")
                } else {
                    Some("")
                }
            }
            "err" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    Some("gos_rt_result_to_opt_err")
                } else {
                    Some("")
                }
            }
            "next" if args.is_empty() => match &receiver_kind_flat {
                TyKind::Iterator(elem) if self.lazy_iter_carries_elem(*elem) => {
                    Some("gos_rt_lazy_iter_next_i64")
                }
                _ => None,
            },
            // `option.ok_or(new_err)` converts None into Err and
            // passes Some through.
            "ok_or" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_option_adt(receiver_ty)
                {
                    Some("gos_rt_result_ok_or")
                } else {
                    Some("")
                }
            }
            "len" => match &receiver_kind_flat {
                TyKind::String => Some("gos_rt_str_len"),
                TyKind::HashMap { .. } => Some("gos_rt_map_len"),
                TyKind::JsonValue => Some("gos_rt_json_len"),
                TyKind::Vec(_) | TyKind::Array { .. } | TyKind::Slice(_) => Some("gos_rt_len"),
                // The MIR type didn't resolve. Inspect the HIR
                // expression's static type as a fallback - common
                // shape is `let s = fs::read_to_string(...)?; s.len()`
                // where the typechecker leaves `s` as `Var(...)` but
                // the HIR `Path(s)` node still carries `String`.
                // Without this fallback the dispatch lands on the
                // generic `gos_rt_len`, which reads a Vec header
                // out of a `*const c_char` and returns garbage.
                _ => {
                    let mut peeled = receiver.ty;
                    while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                        peeled = *inner;
                    }
                    match self.tcx.kind_of(peeled) {
                        TyKind::String => Some("gos_rt_str_len"),
                        TyKind::HashMap { .. } => Some("gos_rt_map_len"),
                        TyKind::JsonValue => Some("gos_rt_json_len"),
                        _ => Some("gos_rt_len"),
                    }
                }
            },
            "trim" => Some("gos_rt_str_trim"),
            "trim_start" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_trim_start")
            }
            "trim_end" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_trim_end")
            }
            "contains" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_contains")
            }
            "starts_with" => Some("gos_rt_str_starts_with"),
            "ends_with" => Some("gos_rt_str_ends_with"),
            "find" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_find_opt"),
            "rfind" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_rfind_opt")
            }
            "to_i64" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_i64_opt")
            }
            "to_f64" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_f64_opt")
            }
            "to_bool" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_bool_opt")
            }
            "replace" => Some("gos_rt_str_replace"),
            "split" => Some("gos_rt_str_split"),
            // 0.7.0 string surface - split_once / rsplit_once return
            // `Option<(String, String)>` packed as a `*mut GosResult`
            // pair payload (see `gos_rt_str_split_once`).
            "split_once" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_split_once")
            }
            "rsplit_once" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_rsplit_once")
            }
            "count" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_count"),
            "trim_start_matches" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_lstrip_chars")
            }
            "trim_end_matches" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_rstrip_chars")
            }
            "center" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_center"),
            "slice" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_slice"),
            "substring" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_substring")
            }
            // 0.14.0 - the remaining canonical String method surface.
            // Each already exists as a `strings::*` free fn (see
            // `stdlib_free.rs`); wiring the method form here lets
            // `s.method(...)` and the `_.method` pipe placeholder dispatch
            // on the compiled tiers the same way the VM does, instead of
            // emitting an undefined `@method` symbol.
            "split_whitespace" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_split_whitespace")
            }
            "splitn" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_splitn"),
            "to_title" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_title")
            }
            "trim_matches" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_trim_matches")
            }
            "replacen" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_replacen")
            }
            "pad_left" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_pad_left")
            }
            "pad_right" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_pad_right")
            }
            "contains_any" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_contains_any")
            }
            "equal_fold" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_equal_fold")
            }
            "find_any" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_index_any")
            }
            "rfind_any" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_last_index_any")
            }
            "strip_prefix" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_strip_prefix")
            }
            "strip_suffix" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_strip_suffix")
            }
            // 0.7.0 Vec method surface. `xs.slice(a, b)?` returns a
            // Result<Vec<T>, errors::Error>; `xs.first()` / `xs.last()`
            // return Option<T>; `xs.rev()` returns a fresh Vec;
            // `xs.contains` / `xs.index_of` / `xs.count_of` need
            // element-type dispatch (String vs i64).
            // `xs.slice(a, b)?` - receiver shape decides which
            // helper handles the buffer layout. Vec receivers
            // (`Vec<T>` and `&[T]` after the `to_vec` route) carry
            // a `GosVec` header; raw `[T; N]` array literals are
            // a len-prefixed flat `*const i64` buffer the
            // intarr / floatarr shims walk directly.
            "slice" if matches!(&receiver_kind_flat, TyKind::Vec(_) | TyKind::Slice(_)) => {
                Some("gos_rt_vec_slice_result")
            }
            "slice" if matches!(&receiver_kind_flat, TyKind::Array { .. }) => {
                let elem_kind = match &receiver_kind_flat {
                    TyKind::Array { elem, .. } => self.tcx.kind_of(*elem),
                    _ => unreachable!(),
                };
                if matches!(elem_kind, TyKind::Float(_)) {
                    Some("gos_rt_floatarr_slice_result")
                } else if matches!(elem_kind, TyKind::Int(gossamer_types::IntTy::U8)) {
                    // Byte-packed result (stride 1) - keeps a `[u8]` slice at
                    // one byte per element instead of 8x.
                    Some("gos_rt_bytearr_slice_result")
                } else {
                    Some("gos_rt_intarr_slice_result")
                }
            }
            "first"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                Some("gos_rt_vec_first")
            }
            "last"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                Some("gos_rt_vec_last")
            }
            "get"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                Some("gos_rt_vec_get_opt")
            }
            "rev"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                Some("gos_rt_vec_reversed")
            }
            "take"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                Some("gos_rt_vec_take")
            }
            "step_by"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                Some("gos_rt_vec_step_by")
            }
            // `xs.join(sep)` - the method form of `strings::join` for a
            // String element, and the Display-rendering join shims for
            // scalar elements. Keyed on the element TyKind so a numeric
            // vec never joins pointer words; the one-arg gate keeps
            // `JoinHandle::join` (zero args, handled above) unshadowed.
            "join"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                self.vec_join_symbol(receiver_ty)
            }
            "contains"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                let elem = vec_element_kind(self.tcx, receiver_ty);
                Some(if elem == VecElemKind::Str {
                    "gos_rt_vec_contains_str"
                } else {
                    "gos_rt_vec_contains_i64"
                })
            }
            "index_of"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                let elem = vec_element_kind(self.tcx, receiver_ty);
                Some(if elem == VecElemKind::Str {
                    "gos_rt_vec_index_of_str"
                } else {
                    "gos_rt_vec_index_of_i64"
                })
            }
            "count_of"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                let elem = vec_element_kind(self.tcx, receiver_ty);
                Some(if elem == VecElemKind::Str {
                    "gos_rt_vec_count_of_str"
                } else {
                    "gos_rt_vec_count_of_i64"
                })
            }
            // 0.7.0 HashMap method surface - keys / values yield
            // Vec<K> / Vec<V>; pop returns Option<V>.
            "keys" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                Some("gos_rt_map_keys_vec")
            }
            "values" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                Some("gos_rt_map_values_vec")
            }
            "pop" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                let key = hashmap_key_kind(self.tcx, receiver_ty);
                Some(if key == VecElemKind::Str {
                    "gos_rt_map_pop_typed_str"
                } else {
                    "gos_rt_map_pop_i64"
                })
            }
            "lines" => Some("gos_rt_str_lines"),
            "repeat" => Some("gos_rt_str_repeat"),
            "byte_len" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_byte_len")
            }
            "byte_at" => Some("gos_rt_str_byte_at"),
            // `s.chars()` - materialise the Unicode scalars as a
            // `Vec<char>` (one i64 codepoint per slot) so the
            // for-loop reads each via `gos_rt_vec_get_i64` and binds
            // a `char`. Gated on a String receiver: a user struct
            // with its own `chars` method falls through to user
            // dispatch.
            // A String's `chars()` answers a cursor over the text.
            "chars" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_lazy_iter_str_chars")
            }
            // `is_empty` collapses to `len(self) == 0`. Route to
            // a small helper that delegates to the right `len`
            // backend for the receiver kind.
            "is_empty" => match &receiver_kind_flat {
                TyKind::String => Some("gos_rt_str_is_empty"),
                _ => Some("gos_rt_len_is_zero"),
            },
            // errors::Error methods. `is` here routes
            // unconditionally to the runtime helper because no
            // other type in the stdlib defines a `.is(...)`
            // method today; if a user struct defines one, the
            // user-impl dispatch below wins (it runs after this
            // table).
            "message" => Some("gos_rt_error_message"),
            "cause" => Some("gos_rt_error_cause"),
            "is" => Some("gos_rt_error_is"),
            "chain" => Some("gos_rt_error_chain"),
            "with_field" => Some("gos_rt_error_with_field"),
            "field" => Some("gos_rt_error_field"),
            "fields" => Some("gos_rt_error_fields"),
            // bufio::Scanner methods.
            "scan" => Some("gos_rt_bufio_scanner_scan"),
            "text" => Some("gos_rt_bufio_scanner_text"),
            // `ResponseStream::next_line() -> Option<String>`. The
            // receiver is the 3-slot blob `[__handle, status,
            // content_type]` returned by `gos_rt_http_stream`; the
            // helper reads the leading i64 (the handle) and pops
            // one line from the registered Vec.
            "next_line" => Some("gos_rt_http_stream_next_line"),
            // `ResponseStream::next_chunk(max_bytes) ->
            // Option<[u8]>` - same blob receiver as `next_line`;
            // the Some payload is a packed `elem_bytes = 1` byte
            // vec (the `raw_bytes` representation contract).
            "next_chunk" => Some("gos_rt_http_stream_next_chunk"),
            // http::Response getters.
            "status" => Some("gos_rt_http_response_status"),
            "body" => Some("gos_rt_http_response_body"),
            // http builder. The kind-dispatch above already routes
            // tagged `http::Request` receivers for `.header(k, v)`
            // builder calls; this name-only arm catches untagged
            // ones - `.send` falls below to the channel default
            // because channel sends are far more common in user
            // code than untagged-http requests.
            "header" => Some("gos_rt_http_request_header"),
            // Chainable server-response builder: replace-then-push
            // a header and return the same response pointer.
            "with_header" => Some("gos_rt_http_response_with_header"),
            "send" => Some("gos_rt_chan_send"),
            // string parsing - `text.parse()` for an i64 binding
            // routes to gos_rt_parse_i64 with a discarded ok flag.
            // Pin return to i64 for the common case; users with
            // f64 / float must annotate explicitly today.
            "parse" => Some("gos_rt_parse_i64_result"),
            // Result/Option chained helpers map to identity on
            // the happy path. The user passes in a closure; we
            // discard it (the compiled tier doesn't run the
            // error-mapping closure today).
            // map_err / map only dispatch when the receiver's
            // MIR-pinned type is a real Result Adt. Stdlib helpers
            // like `fs::write` return a bool today; routing
            // `.map_err(...)` on a bool through the result-helper
            // would feed an i8 to a `*mut GosResult` parameter and
            // trip the cranelift verifier.
            "map_err" => {
                if (matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty))
                    || self.expr_is_send_result(receiver)
                {
                    Some("gos_rt_result_map_err")
                } else {
                    Some("")
                }
            }
            "map" => {
                if (matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty))
                    || self.expr_is_send_result(receiver)
                {
                    Some("gos_rt_result_map")
                } else {
                    Some("")
                }
            }
            "to_lowercase" => Some("gos_rt_str_to_lower"),
            "to_uppercase" => Some("gos_rt_str_to_upper"),
            "push" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Var(_) => Some("gos_rt_vec_push"),
                _ => None,
            },
            "pop" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Var(_) => Some("gos_rt_vec_pop_opt"),
                _ => None,
            },
            "sort" => Some(
                if vec_element_kind(self.tcx, receiver_ty) == VecElemKind::Str {
                    "gos_rt_vec_sort_str"
                } else {
                    "gos_rt_vec_sort_i64"
                },
            ),
            "reverse" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Var(_) => Some("gos_rt_vec_reverse"),
                _ => None,
            },
            "iter" => match &receiver_kind_flat {
                // HashMap `.iter()` is handled before the helper-name
                // dispatch: the `for (k, v) in m.iter()` shape by
                // `try_lower_for_hashmap_iter` and the direct-binding
                // `let xs = m.iter()` shape by
                // `materialize_hashmap_entries` (both produce a real
                // `Vec<(K, V)>`). Reaching this arm would mean a map
                // receiver slipped past both; fall back to a MIR error
                // rather than the `gos_rt_arr_iter` path, which would
                // reinterpret the `*mut GosMap` as a `*mut GosVec`.
                TyKind::HashMap { .. } => return SymbolLookup::Bail,
                // A sequence whose element the lazy state cannot carry keeps
                // its own handle, so the eager sequence helpers - which read
                // the element at its real width - consume it directly.
                TyKind::Vec(elem) | TyKind::Slice(elem)
                    if !self.lazy_iter_borrowable_elem(*elem) =>
                {
                    Some("")
                }
                // A fixed array lives inline in its slots rather than behind a
                // `*mut GosVec` header, so the handle-taking iterator helper
                // would read those slots as one. Take the eager sequence path,
                // which reads the elements at their real width.
                TyKind::Array { .. } => Some(""),
                _ => Some("gos_rt_arr_iter"),
            },
            "collect" | "to_vec" => match &receiver_kind_flat {
                // Vec/Slice/Array `.to_vec()` and `.collect()` must produce an
                // independent copy - bubble_sort's `out.swap(...)`
                // was mutating the caller's slice through the
                // aliased pointer. Other types fall through to
                // the identity copy.
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
                    Some("gos_rt_vec_clone")
                }
                _ => Some(""),
            },
            // `s.bytes()` is the UTF-8 byte view of a String receiver, the
            // same buffer `as_bytes` materialises. Gated on a String
            // receiver so a user struct with its own `bytes` method falls
            // through to user dispatch.
            "bytes" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_as_bytes"),
            "as_bytes" => match &receiver_kind_flat {
                // String-receiver `.as_bytes()` materialises a real
                // length-prefixed arena buffer. The previous identity
                // lowering returned the raw c_char ptr; passing that
                // to a callee with `&[u8]` parameter and calling
                // `.len()` on it inside the callee read the first
                // 8 string bytes as a length, crashing on dereference.
                TyKind::String => Some("gos_rt_str_as_bytes"),
                _ => Some(""),
            },
            "as_str" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_as_str_opt"),
                _ => Some(""),
            },
            // JSON value query/cast methods. The runtime helpers
            // accept a `*mut GosJson` (passed as a flat pointer)
            // and return either a fresh `*mut GosJson` (for
            // chained queries) or a primitive scalar.
            "as_i64" => Some("gos_rt_json_as_i64_opt"),
            "as_f64" => Some("gos_rt_json_as_f64_opt"),
            "as_bool" => Some("gos_rt_json_as_bool_opt"),
            "is_null" => Some("gos_rt_json_is_null"),
            "at" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_at"),
                _ => None,
            },
            "recv" => Some("gos_rt_chan_recv_option"),
            // `rx.recv_ctx(&ctx)` - same shape as `recv`, but
            // takes a Context handle as the second arg. The
            // runtime helper polls cancellation on both the
            // goroutine park path and the OS-thread condvar
            // path, returning None when the context fires.
            "recv_ctx" => Some("gos_rt_chan_recv_ctx_option"),
            "try_send" => Some("gos_rt_chan_try_send"),
            "try_recv" => Some("gos_rt_chan_try_recv_option"),
            // `close` is also a user-facing method on structs (the
            // injected sql `Rows` / `Conn` wrappers). Route to the
            // channel helper only when the receiver is not a struct
            // carrying its own `close` impl - the same receiver gate
            // as `insert` / `get` below. Without it, `rows.close()`
            // closed a bogus channel handle instead of dispatching
            // to `__gos_sql_Rows::close`.
            "close" => {
                let user_close = self
                    .struct_name_of(receiver_ty)
                    .or_else(|| self.struct_name_from_expr(receiver))
                    .is_some_and(|s| self.impl_methods.contains_key(&format!("{s}::close")));
                if user_close {
                    None
                } else {
                    Some("gos_rt_chan_close")
                }
            }
            // Stream methods (on `io::stdout()` / `io::stderr()`
            // / `io::stdin()` handles). Mirrors Rust's `Write` /
            // `BufRead` trait surface.
            "write_byte" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some("gos_rt_stream_write_byte")
            }
            "write_byte_array" | "write_bytes"
                if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") =>
            {
                Some("gos_rt_stream_write_byte_array")
            }
            "write" | "write_str"
                if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") =>
            {
                Some("gos_rt_stream_write_str")
            }
            "flush" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some("gos_rt_stream_flush")
            }
            "read_line" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some(if args.is_empty() {
                    "gos_rt_stream_next_line"
                } else {
                    "gos_rt_stream_read_line"
                })
            }
            "read_to_string" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some("gos_rt_stream_read_to_string")
            }
            // HashMap method dispatch - gated on the receiver
            // actually being a `HashMap`, not just on having a
            // matching method name. Without the gate, a user
            // struct with an `impl Foo { fn get(...) }` would
            // route through the map helper at codegen time and
            // either segfault on the wrong ABI or read garbage.
            // `get` extends the gate to `JsonValue` because the
            // json runtime also exposes a single-arg `get(key)`.
            "insert" => match &receiver_kind_flat {
                // An aggregate value (Vec / struct) is stored as an 8-byte
                // handle word, so the helper routes by KEY kind there - a
                // String key must still use the str path, not the i64/i64
                // path that reinterprets the key pointer.
                TyKind::HashMap { .. } => Some(self.map_insert_helper(receiver_ty)),
                // Vec insertion mutates in place and returns a bounds error.
                // A struct / tuple / array element reaches the runtime as the
                // address of its slot block, the way `gos_rt_vec_push` takes
                // one; every other element is the word itself. The two shapes
                // need different entry points because a one-slot struct is
                // indistinguishable from a scalar once it is in the vec.
                TyKind::Vec(elem) | TyKind::Slice(elem) => {
                    if self.is_inline_slot_block(*elem) {
                        Some("gos_rt_vec_insert_slots_safe")
                    } else {
                        Some("gos_rt_vec_insert_safe")
                    }
                }
                TyKind::Array { elem, .. } => {
                    if self.is_inline_slot_block(*elem) {
                        Some("gos_rt_vec_insert_slots_safe")
                    } else {
                        Some("gos_rt_vec_insert_safe")
                    }
                }
                _ => None,
            },
            "get" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_get"),
                // HashMap::get now uniformly returns Option<V> packed
                // in a *mut GosResult. The MIR pin restores V from the
                // call's Option<V> substs so `if let Some(p) = m.get(&k)`
                // binds `p` with the right element type - struct refs
                // included. Pre-0.8.0 the bare i64-returning helpers
                // collided None with stored-0 (HashMap<_, i64>) and
                // produced a silent miscompile on field access through
                // struct-valued maps. See feature-testing-examples/
                // hashmap_get_some_field.gos.
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_get_typed_str_opt"),
                    _ => Some("gos_rt_map_get_i64_opt"),
                },
                _ => None,
            },
            "get_or" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_value_kind(receiver_ty) {
                    Some(MapValueKind::String) => match self.hash_map_key_kind(receiver_ty) {
                        Some(MapKeyKind::String) => Some("gos_rt_map_get_or_str_str"),
                        _ => Some("gos_rt_map_get_or_i64_str"),
                    },
                    Some(MapValueKind::Bytes) => match self.hash_map_key_kind(receiver_ty) {
                        Some(MapKeyKind::String) => Some("gos_rt_map_get_or_typed_str_i64"),
                        _ => Some("gos_rt_map_get_or_i64"),
                    },
                    _ => match self.hash_map_key_kind(receiver_ty) {
                        Some(MapKeyKind::String) => Some("gos_rt_map_get_or_typed_str_i64"),
                        _ => Some("gos_rt_map_get_or_i64"),
                    },
                },
                _ => None,
            },
            // `or_insert` stores an 8-byte value word (i64 scalar or
            // an aggregate handle: Vec / struct), so route purely by
            // KEY kind - a String-keyed, Vec-valued map needs the str
            // path, not the absent value-kind branch that emitted an
            // undefined `@or_insert` call.
            "or_insert" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_or_insert_typed_str_i64"),
                    _ => Some("gos_rt_map_or_insert_i64_i64"),
                },
                _ => None,
            },
            "remove" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_pop_typed_str"),
                    _ => Some("gos_rt_map_pop_i64"),
                },
                // Vec removal mutates in place and returns a bounds error.
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
                    Some("gos_rt_vec_remove_safe")
                }
                _ => None,
            },
            "contains_key" | "contains"
                if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) =>
            {
                match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_contains_key_typed_str"),
                    _ => Some("gos_rt_map_contains_key_i64"),
                }
            }
            "clear" if args.is_empty() => match &receiver_kind_flat {
                TyKind::HashMap { .. } => Some("gos_rt_map_clear"),
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
                    Some("gos_rt_vec_clear")
                }
                _ => None,
            },
            // `m.inc_at(seq, start, len, by)` - zero-copy slice
            // hash for `HashMap<String, i64>`. Single hash lookup
            // per call, no per-iteration scratch allocation -
            // mirrors `*m.entry(&seq[i..i+k]).or_insert(0) += by`.
            "inc_at" => match self.hash_map_value_kind(receiver_ty) {
                Some(MapValueKind::I64) => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_inc_at_str_i64"),
                    _ => None,
                },
                _ => None,
            },
            // HashMap iteration. Each helper snapshots the
            // requested column into a fresh `GosVec` so the
            // for-loop lowerer can drive iteration with the
            // regular `gos_rt_vec_*` helpers. String-keyed /
            // string-valued shapes go through `*_str`; everything
            // else through `*_i64`.
            "keys" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_keys_str"),
                    _ => Some("gos_rt_map_keys_i64"),
                },
                _ => None,
            },
            "values" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_value_kind(receiver_ty) {
                    Some(MapValueKind::String) => Some("gos_rt_map_values_str"),
                    Some(MapValueKind::Bytes) => Some("gos_rt_map_values_vec"),
                    _ => Some("gos_rt_map_values_i64"),
                },
                _ => None,
            },
            // Mutex<T> / WaitGroup / Atomic / heap-Vec
            // primitives. Each method dispatches by name -
            // the runtime function takes the receiver
            // pointer as its first arg, matching the rest of
            // the table.
            "lock" => Some("gos_rt_mutex_lock"),
            "unlock" => Some("gos_rt_mutex_unlock"),
            "add" => Some("gos_rt_wg_add"),
            "done" => Some("gos_rt_wg_done"),
            "wait" => Some("gos_rt_wg_wait"),
            "wait_ctx" => Some("gos_rt_wg_wait_ctx"),
            "load" => Some("gos_rt_atomic_i64_load"),
            "store" => Some("gos_rt_atomic_i64_store"),
            "fetch_add" => Some("gos_rt_atomic_i64_fetch_add"),
            "fetch_sub" => Some("gos_rt_atomic_i64_fetch_sub"),
            // The AtomicBool receiver is matched by kind above; every other
            // atomic compares through the i64 storage they all share.
            "compare_exchange" => Some("gos_rt_atomic_i64_cas"),
            "set_at" => Some("gos_rt_heap_i64_set"),
            "get_at" => Some("gos_rt_heap_i64_get"),
            "vec_len" => Some("gos_rt_heap_i64_len"),
            "write_range_to_stdout" => Some("gos_rt_heap_i64_write_bytes_to_stdout"),
            "write_lines_to_stdout" => Some("gos_rt_heap_i64_write_lines_to_stdout"),
            // U8Vec methods. Distinct names from the I64Vec
            // family because MIR's method dispatch is by name
            // alone - sharing `set_at` between i64 and u8
            // receivers would silently write through the
            // i64-stride helper to a u8 buffer, corrupting
            // adjacent bytes.
            "set_byte" => Some("gos_rt_heap_u8_set"),
            "window_key" => Some("gos_rt_heap_u8_window_key"),
            "count_singles" => Some("gos_rt_heap_u8_count_singles"),
            "count_pairs" => Some("gos_rt_heap_u8_count_pairs"),
            "count_kmers" => Some("gos_rt_heap_u8_count_kmers"),
            "get_byte" => Some("gos_rt_heap_u8_get"),
            "byte_len" => Some("gos_rt_heap_u8_len"),
            "write_byte_range_to_stdout" => Some("gos_rt_heap_u8_write_bytes_to_stdout"),
            "write_byte_lines_to_stdout" => Some("gos_rt_heap_u8_write_lines_to_stdout"),
            _ => None,
        })
    }

    /// Receiver-runtime-kind dispatch table (flag/http/stdlib handles).
    fn kind_dispatch_symbol(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        heap_reverse_i64: bool,
        heap_float_elem: bool,
    ) -> Option<&'static str> {
        self.kind_dispatch_symbol_a(
            rk,
            method,
            args,
            receiver_ty,
            heap_reverse_i64,
            heap_float_elem,
        )
        .or_else(|| {
            self.kind_dispatch_symbol_b(
                rk,
                method,
                args,
                receiver_ty,
                heap_reverse_i64,
                heap_float_elem,
            )
        })
    }

    /// First half of the receiver-runtime-kind dispatch table.
    fn kind_dispatch_symbol_a(
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
            (Some("http::Server"), "read_header_timeout_ms") => {
                Some("gos_rt_http_server_read_header_timeout_ms")
            }
            (Some("http::Server"), "request_timeout_ms") => {
                Some("gos_rt_http_server_request_timeout_ms")
            }
            (Some("http::Server"), "read_body_timeout_ms") => {
                Some("gos_rt_http_server_read_body_timeout_ms")
            }
            (Some("http::Server"), "write_timeout_ms") => {
                Some("gos_rt_http_server_write_timeout_ms")
            }
            (Some("http::Server"), "idle_timeout_ms") => Some("gos_rt_http_server_idle_timeout_ms"),
            (Some("http::Server"), "max_header_bytes") => {
                Some("gos_rt_http_server_max_header_bytes")
            }
            (Some("http::Server"), "max_body_bytes") => Some("gos_rt_http_server_max_body_bytes"),
            (Some("http::Server"), "max_connections") => Some("gos_rt_http_server_max_connections"),
            (Some("http::Server"), "server_name") => Some("gos_rt_http_server_server_name"),
            (Some("http::ResponseStream"), "write") => Some("gos_rt_http_response_stream_write"),
            (Some("http::ResponseStream"), "write_bytes") => {
                Some("gos_rt_http_response_stream_write_bytes")
            }
            (Some("http::ResponseStream"), "close") => Some("gos_rt_http_response_stream_close"),
            (Some("http::ResponseStream"), "is_open") => {
                Some("gos_rt_http_response_stream_is_open")
            }
            (Some("http::Server"), "listen") => Some("gos_rt_http_server_listen"),
            (Some("http::Server"), "addr") => Some("gos_rt_http_server_addr"),
            (Some("http::Server"), "shutdown") => Some("gos_rt_http_server_shutdown"),
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
            (Some("collections::HashSet" | "collections::BTreeSet"), "to_vec" | "iter") => {
                Some("gos_rt_set_to_vec")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "clear") => {
                Some("gos_rt_set_clear")
            }
            // A `GosSet` table is reached through a handle carrying no count
            // of its holders, so the clone takes a table of its own.
            (Some("collections::HashSet" | "collections::BTreeSet"), "clone") => {
                Some("gos_rt_set_clone")
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

    fn binary_heap_runtime_symbol(
        &self,
        rk: Option<&'static str>,
        receiver_ty: Ty,
        method: &Ident,
        heap_reverse_i64: bool,
        float_elem: bool,
    ) -> Option<&'static str> {
        let min_heap = rk == Some("collections::MinHeap")
            || heap_reverse_i64
            || self.binary_heap_ty_is_min(receiver_ty)
            || self.binary_heap_elem_is_reverse_i64(receiver_ty);
        // A float element is stored as its bit pattern, whose integer order is
        // not the float order, so the sift compares the value the bits spell.
        // Peek reads the root without comparing, so it stays on the integer
        // entry point.
        let float_elem = float_elem || self.heap_elem_is_float(receiver_ty);
        // An element the one-word integer and float entry points cannot
        // order - a `String`, a tuple, a struct, a sequence, an `Option`, an
        // enum - is compared through its ordering descriptor instead.
        let desc_elem = !float_elem
            && self
                .first_generic_of(receiver_ty)
                .is_some_and(|elem| self.heap_elem_needs_desc(elem));
        if desc_elem {
            return match (method.name.as_str(), min_heap) {
                ("push", true) => Some("gos_rt_bheap_min_push_desc"),
                ("pop", true) => Some("gos_rt_bheap_min_pop_desc"),
                ("push", false) => Some("gos_rt_bheap_max_push_desc"),
                ("pop", false) => Some("gos_rt_bheap_max_pop_desc"),
                ("peek", _) => Some("gos_rt_bheap_peek_elem"),
                ("len", _) => Some("gos_rt_bheap_len"),
                ("is_empty", _) => Some("gos_rt_bheap_is_empty"),
                ("clear", _) => Some("gos_rt_bheap_clear"),
                _ => None,
            };
        }
        match (method.name.as_str(), min_heap) {
            ("push", true) if float_elem => Some("gos_rt_bheap_min_push_f64"),
            ("pop", true) if float_elem => Some("gos_rt_bheap_min_pop_f64"),
            ("push", false) if float_elem => Some("gos_rt_bheap_max_push_f64"),
            ("pop", false) if float_elem => Some("gos_rt_bheap_max_pop_f64"),
            ("push", true) => Some("gos_rt_bheap_min_push_i64"),
            ("pop", true) => Some("gos_rt_bheap_min_pop_i64"),
            ("peek", true) => Some("gos_rt_bheap_min_peek_i64"),
            ("push", false) => Some("gos_rt_bheap_max_push_i64"),
            ("pop", false) => Some("gos_rt_bheap_max_pop_i64"),
            ("peek", false) => Some("gos_rt_bheap_max_peek_i64"),
            ("len", _) => Some("gos_rt_bheap_len"),
            ("is_empty", _) => Some("gos_rt_bheap_is_empty"),
            ("clear", _) => Some("gos_rt_bheap_clear"),
            _ => None,
        }
    }

    /// Emits `gos_rt_f64_to_bits(value)` and answers the local holding the
    /// word, so a float reaches a one-word store as its own bits rather than
    /// as the integer the value converts to.
    pub(crate) fn emit_float_bits(&mut self, value: Local, span: Span) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bits = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_f64_to_bits".to_string())),
            args: vec![Operand::Copy(Place::local(value))],
            destination: Place::local(bits),
            target: Some(next),
        });
        self.set_current(next);
        bits
    }

    /// Whether a heap element orders through its descriptor rather than as
    /// one integer or float word: everything but the scalars a slot holds
    /// and compares directly.
    pub(crate) fn heap_elem_needs_desc(&self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = elem;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        !matches!(
            self.tcx.kind_of(cur),
            TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::Duration
                | TyKind::Instant
                | TyKind::Var(_)
        )
    }

    /// Whether a heap receiver's element is a 64-bit unsigned integer, whose
    /// slots span the whole unsigned range and so order as `u64` rather than
    /// as the signed value the same bits spell.
    pub(crate) fn heap_elem_is_unsigned(&self, ty: Ty) -> bool {
        use gossamer_types::{IntTy, TyKind};
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { substs, .. }) = self.tcx.kind(cur) else {
            return false;
        };
        substs.types().first().is_some_and(|elem| {
            matches!(
                self.tcx.kind_of(*elem),
                TyKind::Int(IntTy::U64 | IntTy::Usize)
            )
        })
    }

    /// Whether a heap receiver's element is a float, whose slots hold bit
    /// patterns the ordering entry points read as floats.
    pub(crate) fn heap_elem_is_float(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { substs, .. }) = self.tcx.kind(cur) else {
            return false;
        };
        substs
            .types()
            .first()
            .is_some_and(|elem| matches!(self.tcx.kind_of(*elem), TyKind::Float(_)))
    }

    pub(crate) fn binary_heap_elem_is_reverse_i64(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(cur) else {
            return false;
        };
        if def.local != BINARY_HEAP_DEF_LOCAL && self.tcx.def_name(*def) != Some("BinaryHeap") {
            return false;
        }
        let Some(elem) = substs.types().first().copied() else {
            return false;
        };
        let mut elem = elem;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(elem) {
            elem = *inner;
        }
        self.is_reverse_i64_ty(elem)
    }

    pub(crate) fn binary_heap_ty_is_min(&self, ty: Ty) -> bool {
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(self.tcx.kind(cur), Some(TyKind::Adt { def, .. }) if def.local == MIN_HEAP_DEF_LOCAL)
    }

    pub(crate) fn is_reverse_i64_ty(&self, ty: Ty) -> bool {
        use gossamer_types::{IntTy, TyKind};
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(cur) else {
            return false;
        };
        (def.local == REVERSE_DEF_LOCAL || self.tcx.def_name(*def) == Some("Reverse"))
            && substs.types().first().is_some_and(|payload| {
                matches!(self.tcx.kind_of(*payload), TyKind::Int(IntTy::I64))
            })
    }

    /// Second half of the receiver-runtime-kind dispatch table.
    fn kind_dispatch_symbol_b(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        _heap_reverse_i64: bool,
        _heap_float_elem: bool,
    ) -> Option<&'static str> {
        let _ = receiver_ty;
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
            // AtomicBool load/store route to the bool-typed shims so
            // the load result renders `true` / `false`; the name-only
            // table below keeps AtomicI64 on the i64 path.
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
            (Some("fs::File"), "write_at") => Some("gos_rt_fs_file_write_at"),
            (Some("fs::File"), "seek") => Some("gos_rt_fs_file_seek"),
            (Some("fs::File"), "set_len") => Some("gos_rt_fs_file_set_len"),
            (Some("fs::File"), "len") => Some("gos_rt_fs_file_len"),
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
            _ => None,
        }
    }

    /// Lower a runtime-kind dispatched call: lower receiver, build args, emit.
    fn lower_kind_dispatch_call(
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
    fn kind_dispatch_arg_operands(
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
        if rt == "gos_rt_set_to_vec" && matches!(self.set_elem_kind_of(receiver), MapKeyKind::I64) {
            rt = "gos_rt_set_to_vec_i64";
        }
        if enum_set_desc.is_some() {
            rt = match rt {
                "gos_rt_set_insert" | "gos_rt_set_insert_i64" => "gos_rt_set_insert_ekey",
                "gos_rt_set_contains" | "gos_rt_set_contains_i64" => "gos_rt_set_contains_ekey",
                "gos_rt_set_remove" | "gos_rt_set_remove_i64" => "gos_rt_set_remove_ekey",
                "gos_rt_set_to_vec" | "gos_rt_set_to_vec_i64" => "gos_rt_set_to_vec_ekey",
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
                // A value sent on a channel escapes to the receiving
                // goroutine: switch any RC-managed value to atomic
                // reference counting before it is enqueued.
                if rt == "gos_rt_chan_send" {
                    self.emit_mark_shared_if_rc(a, span);
                }
                arg_operands.push(Operand::Copy(Place::local(a)));
            }
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
    fn dispatch_pinned_ty(
        &mut self,
        rt: &'static str,
        receiver: &HirExpr,
        receiver_local: Local,
        ty: Ty,
    ) -> Ty {
        match rt {
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
            "gos_rt_set_to_vec_ekey" => {
                let elem = self
                    .first_generic_of(receiver.ty)
                    .or_else(|| self.first_generic_of(self.locals[receiver_local.0 as usize].ty))
                    .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                self.tcx.intern(gossamer_types::TyKind::Vec(elem))
            }
            "gos_rt_set_to_vec_i64" => {
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
            "gos_rt_signal_wait" => self.tcx.unit(),
            "gos_rt_signal_try_wait" => self.tcx.bool_ty(),
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
            "gos_rt_lazy_iter_next_i64" => {
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
            "gos_rt_sync_map_get" => self.option_string_adt_ty(),
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
            _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
        }
    }

    /// Tag a dispatched call's destination with its chained runtime kind.
    fn dispatch_dest_kind(&self, rt: &'static str) -> Option<&'static str> {
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
    fn lower_option_result_predicate(
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
    fn lowered_kind_dispatch_symbol(
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
    fn lowered_kind_dispatch_symbol_a(
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
    fn lowered_kind_dispatch_symbol_b(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        _heap_reverse_i64: bool,
        _heap_float_elem: bool,
    ) -> Option<&'static str> {
        let _ = receiver_ty;
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
            (Some("fs::File"), "write_at") => Some("gos_rt_fs_file_write_at"),
            (Some("fs::File"), "seek") => Some("gos_rt_fs_file_seek"),
            (Some("fs::File"), "set_len") => Some("gos_rt_fs_file_set_len"),
            (Some("fs::File"), "len") => Some("gos_rt_fs_file_len"),
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
            (Some("vec::Iter"), "next") => Some("gos_rt_arr_iter_next"),
            _ => None,
        }
    }

    /// Lower a lowered-receiver runtime-kind dispatched call.
    fn lower_lowered_kind_dispatch_call(
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
    fn lowered_kind_dispatch_arg_operands(
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
                // A value sent on a channel escapes to the receiving
                // goroutine: switch any RC-managed value to atomic
                // reference counting before it is enqueued.
                if rt == "gos_rt_chan_send" {
                    self.emit_mark_shared_if_rc(a, span);
                }
                arg_operands.push(Operand::Copy(Place::local(a)));
            }
        }
        Some((arg_operands, rt))
    }

    /// Runtime-symbol fallback: refine the symbol then dispatch / emit.
    fn lower_method_call_fallback(
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
        let method_inputs = self
            .struct_name_of(receiver_ty)
            .or_else(|| self.struct_name_from_expr(receiver))
            .and_then(|name| {
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
                TyKind::Int(_) => Some("gos_rt_i64_to_str"),
                TyKind::Float(_) => Some("gos_rt_f64_to_str"),
                _ => runtime_symbol,
            };
        }
        if lowered_is_result {
            match method.name.as_str() {
                "unwrap" | "expect" => {
                    runtime_symbol = Some(if self.is_option_adt(lowered_recv_ty) {
                        "gos_rt_option_unwrap"
                    } else {
                        "gos_rt_result_unwrap"
                    });
                }
                "unwrap_or" => {
                    // The lowered receiver's own type decides, since the
                    // HIR type is often a variable for a chained call. A
                    // payload that is itself a carrier was boxed, so it is
                    // loaded back rather than handed over as an address.
                    runtime_symbol = Some(if self.carrier_payload_is_carrier(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_carrier"
                    } else if self.carrier_payload_is_sequence(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_vec"
                    } else if self.carrier_payload_is_string(lowered_recv_ty) {
                        "gos_rt_result_unwrap_or_str"
                    } else {
                        "gos_rt_result_unwrap_or"
                    });
                }
                "ok" => runtime_symbol = Some("gos_rt_result_to_opt_ok"),
                "err" => runtime_symbol = Some("gos_rt_result_to_opt_err"),
                _ => {}
            }
        }
        if runtime_symbol.is_none()
            && method.name.as_str() == "next"
            && args.is_empty()
            && matches!(
                self.tcx.kind_of(lowered_recv_ty),
                TyKind::Iterator(elem) if self.lazy_iter_carries_elem(*elem)
            )
        {
            runtime_symbol = Some("gos_rt_lazy_iter_next_i64");
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
                if self.is_inline_aggregate_ty(value) {
                    let _ = self.ensure_aggr_copy_meta(value);
                }
            }
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
    fn load_slot_pointer(&mut self, slot: Local, slot_ty: Ty, span: Span) -> Local {
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

    fn build_fallback_arg_operands(
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
            let a = self.lower_expr(arg)?;
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
            arg_operands.push(Operand::Copy(Place::local(a)));
        }
        Some((receiver_local, arg_operands))
    }

    /// Rewrite a non-capturing `map`/`map_err` closure arg to the bare-fn ABI.
    fn rewrite_result_map_closure_arg(
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

    /// Emit the user-impl method call or the generic by-name fallback call.
    fn emit_fallback_call(
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
            let dest = self.fresh(dest_ty);
            if let Some(out_struct) = self.struct_name_of(dest_ty) {
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
            let dest = self.fresh(dest_ty);
            if let Some(out_struct) = self.struct_name_of(dest_ty) {
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

    /// Resolve the runtime symbol for the type-guarded Vec / String /
    /// HashMap method surface from a receiver type recovered after lowering.
    ///
    /// The top-of-method dispatch table keys on the HIR receiver type, which
    /// is an unresolved inference `Var` when a stdlib call is used directly
    /// as a receiver (`env::args().first()`, `s.split_whitespace()` consumed
    /// in place) - stdlib return types are not all pinned in the checker. The
    /// lowered MIR receiver type is ground truth, so re-keying the guarded
    /// dispatch off it lets a chained temporary resolve the same symbol as a
    /// `let`-bound receiver, identically across the VM, Cranelift, and LLVM
    /// tiers. Mirrors the guarded arms in [`Self::lower_method_call`]'s table.
    /// `xs.map(f)` / `xs.filter(f)` / `xs.sum()` / … - the method form
    /// of the `iter::` combinators on a sequence receiver. Routes
    /// through `try_lower_iter_call` with the receiver threading in as
    /// the data-last argument, so both surfaces share one lowering.
    /// Non-sequence receivers pass through: `Result::map`,
    /// `Option::map`, `HashMap` accessors, and the String surface keep
    /// their own dispatch.
    /// `n.max(m)` / `n.min(m)` / `n.clamp(lo, hi)` on a numeric receiver.
    /// These name the same operation as the prelude's free `max(n, m)`, so
    /// they lower through the same runtime helpers rather than reaching the
    /// by-name fallback, which had no symbol to call.
    fn lower_scalar_bound_method(
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
        if !arity_ok {
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

    fn lower_seq_combinator_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        let joined: Option<&str> = match (method.name.as_str(), args.len()) {
            ("chain", 1) => Some("iter::chain"),
            ("zip", 1) => Some("iter::zip"),
            ("map", 1) => Some("iter::map"),
            ("filter", 1) => Some("iter::filter"),
            ("take_while", 1) => Some("iter::take_while"),
            ("skip_while", 1) => Some("iter::skip_while"),
            ("take", 1) => Some("iter::take"),
            ("skip", 1) => Some("iter::skip"),
            ("step_by", 1) => Some("iter::step_by"),
            ("for_each", 1) => Some("iter::for_each"),
            ("any", 1) => Some("iter::any"),
            ("all", 1) => Some("iter::all"),
            ("find", 1) => Some("iter::find"),
            ("position", 1) => Some("iter::position"),
            ("max_by_key", 1) => Some("iter::max_by_key"),
            ("min_by_key", 1) => Some("iter::min_by_key"),
            ("fold", 2) => Some("iter::fold"),
            ("sum", 0) => Some("iter::sum"),
            ("product", 0) => Some("iter::product"),
            ("collect", 0) => Some("iter::collect"),
            ("min", 0) => Some("iter::min"),
            ("max", 0) => Some("iter::max"),
            ("count", 0) => Some("iter::count"),
            ("enumerate", 0) => Some("iter::enumerate"),
            ("rev", 0) => Some("iter::rev"),
            ("chunks", 1) => Some("iter::chunks"),
            ("windows", 1) => Some("iter::windows"),
            ("dedup", 0) => Some("iter::dedup"),
            ("flatten", 0) => Some("iter::flatten"),
            // Every adapter is a method. These had a data-last free call and
            // no receiver form, which made the rule "an adapter chains" hold
            // everywhere except here.
            ("filter_map", 1) => Some("iter::filter_map"),
            ("find_map", 1) => Some("iter::find_map"),
            ("flat_map", 1) => Some("iter::flat_map"),
            ("chunk_by", 1) => Some("iter::chunk_by"),
            ("count_by", 1) => Some("iter::count_by"),
            ("max_by", 1) => Some("iter::max_by"),
            ("min_by", 1) => Some("iter::min_by"),
            ("partition", 1) => Some("iter::partition"),
            ("product_by", 1) => Some("iter::product_by"),
            ("reduce", 1) => Some("iter::reduce"),
            ("sum_by", 1) => Some("iter::sum_by"),
            ("unzip", 0) => Some("iter::unzip"),
            ("scan", 2) => Some("iter::scan"),
            _ => None,
        };
        let is_pred_count = method.name.as_str() == "count" && args.len() == 1;
        if joined.is_none() && !is_pred_count {
            return MethodLowering::Pass;
        }
        // Key the sequence gate off the recovered receiver kind, not the
        // raw HIR type: a match-extracted payload binding (or a chained
        // stdlib temporary) carries an unresolved inference `Var` in HIR
        // while its lowered local's type is ground truth. Keying on the
        // raw type sent `payload.sum()` to the generic by-name fallback,
        // which only the VM's runtime dispatch could resolve.
        let (_, mut recv_kind) = self.receiver_dispatch_kinds(receiver);
        while let TyKind::Ref { inner, .. } = recv_kind {
            recv_kind = self.tcx.kind_of(inner).clone();
        }
        if !matches!(
            recv_kind,
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } | TyKind::Iterator(_)
        ) {
            return MethodLowering::Pass;
        }
        // A sequence receiver keeps the dedicated buffer symbols routed from
        // the guarded table above; only an iterator receiver needs the
        // `iter::` combinator lowering here.
        if matches!(
            method.name.as_str(),
            "take" | "skip" | "step_by" | "collect" | "product" | "rev"
        ) && !matches!(recv_kind, TyKind::Iterator(_))
        {
            return MethodLowering::Pass;
        }
        // The data-last free forms take the sequence in the last slot, which
        // is why the receiver goes there. `chain` reads its two sequences in
        // order, and the one written first in `xs.chain(ys)` is the receiver.
        let mut reordered: Vec<HirExpr> = if matches!(method.name.as_str(), "chain" | "zip") {
            // Both read their two sequences in order, and the one written
            // first in `xs.chain(ys)` / `xs.zip(ys)` is the receiver.
            let mut ordered = vec![receiver.clone()];
            ordered.extend(args.iter().cloned());
            let joined = if method.name.as_str() == "chain" {
                "iter::chain"
            } else {
                "iter::zip"
            };
            return match self
                .try_lower_iter_call(joined, &ordered, ty, span)
                .or_else(|| self.try_lower_combinator_call(joined, &ordered, ty, span))
            {
                Some(dest) => MethodLowering::Handled(Some(dest)),
                None => MethodLowering::Pass,
            };
        } else {
            args.to_vec()
        };
        reordered.push(receiver.clone());
        // `xs.count(f)` - the accepted-element count: `iter::filter`
        // then a length read of the filtered vec. The filter's
        // destination carries the receiver's sequence type (as a Vec),
        // not the count's i64.
        if is_pred_count {
            let elem = match recv_kind {
                TyKind::Vec(e) | TyKind::Slice(e) | TyKind::Iterator(e) => e,
                TyKind::Array { elem, .. } => elem,
                _ => return MethodLowering::Pass,
            };
            let filtered_ty = self.tcx.intern(TyKind::Vec(elem));
            let Some(filtered) =
                self.try_lower_iter_call("iter::filter", &reordered, filtered_ty, span)
            else {
                return MethodLowering::Pass;
            };
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let dest = self.fresh(i64_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                args: vec![Operand::Copy(Place::local(filtered))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return MethodLowering::Handled(Some(dest));
        }
        let Some(joined) = joined else {
            return MethodLowering::Pass;
        };
        if let Some(dest) = self.try_lower_iter_call(joined, &reordered, ty, span) {
            return MethodLowering::Handled(Some(dest));
        }
        // `max_by_key` / `min_by_key` / `position` and friends lower
        // through the combinator table rather than the iter table.
        match self.try_lower_combinator_call(joined, &reordered, ty, span) {
            Some(dest) => MethodLowering::Handled(Some(dest)),
            None => MethodLowering::Pass,
        }
    }

    /// Free-function path a handle method stands for, or `None` when the
    /// method is not one. Keyed on the receiver's runtime kind so a user
    /// type that happens to declare `call` or `captures` is untouched.
    fn free_form_method_path(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
    ) -> Option<&'static str> {
        if args.len() != 1 {
            return None;
        }
        // A handle whose constructor took no argument leaves the checker
        // nothing to pin its type to, so the construction tag recorded on
        // the receiver's local answers where the type cannot.
        let kind = self.runtime_kind_from_ty(receiver.ty).or_else(|| {
            self.receiver_local_from_path(receiver)
                .and_then(|local| self.local_runtime_kind.get(&local).copied())
        });
        match (kind, method.name.as_str()) {
            (Some("sync::Once"), "call") => Some("sync::Once::call"),
            (Some("sync::RwLock"), "with_read") => Some("sync::RwLock::with_read"),
            (Some("sync::RwLock"), "with_write") => Some("sync::RwLock::with_write"),
            (Some("sync::Shared"), "with") => Some("sync::Shared::with"),
            (Some("sync::Shared"), "update") => Some("sync::Shared::update"),
            (Some("regex::Pattern"), "captures") => Some("regex::captures"),
            (Some("regex::Pattern"), "captures_all") => Some("regex::captures_all"),
            _ => None,
        }
    }

    fn lower_tuple_get_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> MethodLowering {
        if method.name != "get" || args.len() != 1 {
            return MethodLowering::Pass;
        }
        let Some(index) = tuple_get_const_index(&args[0]) else {
            return MethodLowering::Pass;
        };
        let (_, mut receiver_kind) = self.receiver_dispatch_kinds(receiver);
        while let TyKind::Ref { inner, .. } = receiver_kind {
            receiver_kind = self.tcx.kind_of(inner).clone();
        }
        let typecheck_fields = match receiver_kind {
            TyKind::Tuple(fields) => Some(fields),
            _ => None,
        };
        if typecheck_fields.is_none() {
            return MethodLowering::Pass;
        }
        let receiver_local = match self.lower_expr(receiver) {
            Some(local) => local,
            None => return MethodLowering::Handled(None),
        };
        let receiver_ty = self.locals[receiver_local.0 as usize].ty;
        let resolved_ty = self.resolve_var_tuple_fields(receiver_ty);
        if resolved_ty != receiver_ty {
            self.locals[receiver_local.0 as usize].ty = resolved_ty;
        }
        let fields = match self.tcx.kind_of(resolved_ty).clone() {
            TyKind::Tuple(fields) => fields,
            _ => typecheck_fields.expect("tuple gate checked before lowering"),
        };
        let payload_ty = fields
            .get(index)
            .copied()
            .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
        let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Adt { .. }) {
            ty
        } else {
            self.option_payload_adt_ty(payload_ty)
        };
        let dest = self.fresh(dest_ty);
        let payload = if index < fields.len() {
            let idx = match u32::try_from(index) {
                Ok(idx) => idx,
                Err(_) => return MethodLowering::Pass,
            };
            let payload = self.fresh(payload_ty);
            self.emit_assign(
                Place::local(payload),
                Rvalue::Use(Operand::Copy(Place {
                    local: receiver_local,
                    projection: vec![crate::ir::Projection::Field(idx)],
                })),
                span,
            );
            Operand::Copy(Place::local(payload))
        } else {
            Operand::Const(ConstValue::Int(0))
        };
        let disc = i128::from(index >= fields.len());
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![Operand::Const(ConstValue::Int(disc)), payload],
            },
            span,
        );
        MethodLowering::Handled(Some(dest))
    }

    /// The join shim for a sequence receiver, keyed on the element
    /// TyKind: String elements reuse `gos_rt_strings_join`, scalar
    /// elements Display-render through the typed join shims; every other
    /// element type renders through the same formatter `{}` uses
    /// ([`Self::lower_display_join`]).
    fn vec_join_symbol(&self, receiver_ty: Ty) -> Option<&'static str> {
        let mut ty = receiver_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        let elem = match self.tcx.kind_of(ty) {
            TyKind::Vec(e) | TyKind::Slice(e) => *e,
            TyKind::Array { elem, .. } => *elem,
            _ => return None,
        };
        match self.tcx.kind_of(elem) {
            TyKind::String => Some("gos_rt_strings_join"),
            TyKind::Float(_) => Some("gos_rt_vec_join_f64"),
            TyKind::Bool => Some("gos_rt_vec_join_bool"),
            TyKind::Char => Some("gos_rt_vec_join_char"),
            TyKind::Int(_) => Some("gos_rt_vec_join_i64"),
            // An element the inference never grounded - the sequence a bare
            // `impl Display for Set` reaches through `self` - is not an
            // integer, it is unknown. Answering the integer join renders a
            // String element's pointer as a number, so an unknown element
            // takes the Display join, which reads whatever `{}` reads.
            _ => None,
        }
    }

    /// The rendering method of a struct-shaped element on `method`'s channel,
    /// which reads its fields through the element's address. An enum's value
    /// is one word its own formatter decodes, so it is not answered here.
    fn element_fmt_symbol(&mut self, elem_ty: Ty, method: &str) -> Option<String> {
        if !self.elem_is_slot_addressed(elem_ty)
            || !matches!(self.tcx.kind_of(elem_ty), TyKind::Adt { .. })
        {
            return None;
        }
        let name = self.adt_dispatch_name(elem_ty)?;
        let symbol = format!("{name}::{method}");
        self.impl_methods.contains_key(&symbol).then_some(symbol)
    }

    /// Whether the receiver's own type is a user enum, whose runtime value is
    /// one word rather than the address of a slot buffer.
    fn receiver_is_enum(&self, receiver_ty: Ty) -> bool {
        let mut ty = receiver_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        if self.tcx.is_inline_enum_ty(ty) {
            return true;
        }
        match self.tcx.kind_of(ty) {
            // A struct registers its field layout; an enum does not, and its
            // value is one word rather than a slot buffer.
            TyKind::Adt { def, .. } => {
                self.tcx.enum_variant_tys(*def).is_some()
                    || self.tcx.struct_field_tys(*def).is_none()
            }
            _ => false,
        }
    }

    /// Element `index` of `source` in the form its rendering reads it.
    ///
    /// Aggregate storage - a struct, a tuple, an array - is read through its
    /// slot address, which is what a derived `fmt` and the tuple tag stream
    /// take. Every other element's value is the slot itself: a float carries
    /// its bits, and a handle, an enum, and a String carry one word. A struct
    /// or enum then renders through its own `fmt`, since the concat planner
    /// takes a rendered String for either.
    fn read_display_element(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        method: &str,
        span: Span,
    ) -> Local {
        // A struct renders through its derived `fmt`, which reads its fields
        // from the element's own storage: borrowing `source[index]` is how the
        // backends already hand over an element's address.
        if let Some(symbol) = self.element_fmt_symbol(elem_ty, method) {
            let mut place = Place::local(source);
            place.projection.push(crate::ir::Projection::Index(index));
            let ref_ty = self.tcx.intern(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Not,
                inner: elem_ty,
            });
            let borrowed = self.fresh(ref_ty);
            self.emit_assign(
                Place::local(borrowed),
                Rvalue::Ref {
                    place,
                    mutable: false,
                },
                span,
            );
            let string_ty = self.tcx.string_ty();
            let dest = self.fresh(string_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(symbol)),
                args: vec![Operand::Copy(Place::local(borrowed))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return dest;
        }
        let element = if self.elem_is_slot_addressed(elem_ty)
            || self.elem_bytes_of(elem_ty) > 8
            || matches!(self.tcx.kind_of(elem_ty), TyKind::Float(_))
        {
            self.zip_read_element(source, index, elem_ty, span)
        } else {
            self.emit_combinator_call(
                "gos_rt_vec_get_i64",
                vec![
                    Operand::Copy(Place::local(source)),
                    Operand::Copy(Place::local(index)),
                ],
                elem_ty,
                span,
            )
        };
        self.adt_fmt_rendered(element, "to_string", span)
    }

    /// Whether `to_string` on this receiver is the Display rendering rather
    /// than a dedicated conversion. A scalar has its own shim, and a String
    /// is already its own text.
    /// Whether `receiver_ty` names a type whose own `impl` supplies `method`,
    /// which then wins over the synthesized rendering.
    pub(crate) fn has_user_rendering_method(&mut self, receiver_ty: Ty, method: &str) -> bool {
        self.adt_dispatch_name(receiver_ty)
            .is_some_and(|name| self.impl_methods.contains_key(&format!("{name}::{method}")))
    }

    /// Whether the receiver renders through a runtime shim of its own
    /// rather than through the structural formatter.
    ///
    /// A runtime handle is one word, so the structural path would hand
    /// that word to the string formatter and render whatever bytes it
    /// points at. `errors::Error` is the case that matters: `{}` already
    /// routes to `gos_rt_error_display` for the colon-joined chain, and
    /// `to_string` has to answer the same text.
    fn renders_through_own_shim(
        &self,
        receiver: &HirExpr,
        receiver_kind_flat: &TyKind,
        receiver_ty: Ty,
    ) -> bool {
        let kind = self
            .receiver_local_from_path(receiver)
            .and_then(|l| self.local_runtime_kind.get(&l).copied())
            .or_else(|| self.expr_runtime_kind(receiver))
            .or_else(|| Self::stdlib_runtime_kind_from_kind(receiver_kind_flat))
            .or_else(|| self.runtime_kind_from_ty(receiver_ty))
            .or_else(|| self.runtime_kind_from_ty(receiver.ty));
        kind == Some("errors::Error")
    }

    /// Whether the receiver is a set, whose local carries the bare handle
    /// word rather than the set's own type. The width tells the renderer
    /// nothing, so the display path is what reaches the set formatter -
    /// the word's own `to_string` would answer its decimal.
    fn receiver_is_set_handle(&self, receiver: &HirExpr) -> bool {
        let mut ty = receiver.ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        matches!(
            self.tcx.kind_of(ty),
            TyKind::Adt { def, .. }
                if def.local == HASH_SET_DEF_LOCAL || def.local == BTREE_SET_DEF_LOCAL
        )
    }

    fn display_to_string_receiver(&mut self, receiver_ty: Ty) -> bool {
        let mut ty = receiver_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        matches!(
            self.tcx.kind_of(ty),
            TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Array { .. }
                | TyKind::HashMap { .. }
                | TyKind::Tuple(_)
                | TyKind::Adt { .. }
                | TyKind::Nominal { .. }
        )
    }

    /// `x.to_string()` as the one-piece concatenation `format!("{}", x)`
    /// lowers to, so every element type renders through one formatter.
    fn lower_display_to_string(&mut self, receiver: &HirExpr, span: Span) -> Option<Local> {
        let value = self.lower_expr(receiver)?;
        let value = self.adt_fmt_rendered(value, "to_string", span);
        let string_ty = self.tcx.string_ty();
        let dest = self.fresh(string_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("__concat".to_string())),
            args: vec![Operand::Copy(Place::local(value))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// `xs.join(sep)` for an element the typed shims do not carry: renders
    /// element `i` through the formatter `{}` uses and appends it after the
    /// separator every element but the first is preceded by.
    fn lower_display_join(
        &mut self,
        receiver: &HirExpr,
        separator: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let elem_ty = self
            .iter_element_kind(receiver.ty)
            .map(|k| self.tcx.intern(k))?;
        let source = self.lower_iter_vec_arg(receiver)?;
        let sep = self.lower_expr(separator)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let string_ty = self.tcx.string_ty();

        let acc = self.fresh(string_ty);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
            span,
        );
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(source))],
            i64_ty,
            span,
        );
        let index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        // A struct's derived `fmt` reads its fields out of the element's own
        // storage, so it takes the slot's address rather than a copy of it.
        let element = self.read_display_element(source, index, elem_ty, "to_string", span);
        let first = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(first),
            Rvalue::BinaryOp {
                op: BinOp::Eq,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Const(ConstValue::Int(0)),
            },
            span,
        );
        let lead = self.new_block(span);
        let rest = self.new_block(span);
        let joined = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(first)),
            arms: vec![(0, rest)],
            default: lead,
        });

        self.set_current(lead);
        let head = self.fresh(string_ty);
        let after_head = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("__concat".to_string())),
            args: vec![Operand::Copy(Place::local(element))],
            destination: Place::local(head),
            target: Some(after_head),
        });
        self.set_current(after_head);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Copy(Place::local(head))),
            span,
        );
        self.terminate(Terminator::Goto { target: joined });

        self.set_current(rest);
        let grown = self.fresh(string_ty);
        let after_grown = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("__concat".to_string())),
            args: vec![
                Operand::Copy(Place::local(acc)),
                Operand::Copy(Place::local(sep)),
                Operand::Copy(Place::local(element)),
            ],
            destination: Place::local(grown),
            target: Some(after_grown),
        });
        self.set_current(after_grown);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Copy(Place::local(grown))),
            span,
        );
        self.terminate(Terminator::Goto { target: joined });

        self.set_current(joined);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let next_index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(next_index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Copy(Place::local(next_index))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(acc)
    }

    /// True when the `gos_rt_vec_*_i64` / `_str` search shims can represent
    /// `elem` faithfully. Those shims compare one raw 8-byte slot against the
    /// needle word, which equals the element's value only for an i64-slot
    /// scalar or a String's c-string pointer. An `f64` needle would be
    /// converted to an integer at the `i64` parameter, and every aggregate
    /// (struct, tuple, enum, nested sequence) is either stored inline across
    /// several slots or reached through a pointer, so a slot compare answers
    /// pointer identity rather than value equality. Those element types take
    /// the structural scan instead.
    /// The type a `sync::Shared` guards, or `None` for another receiver.
    fn shared_elem_ty(&self, ty: Ty) -> Option<Ty> {
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let TyKind::Adt { def, substs } = self.tcx.kind_of(cur) else {
            return None;
        };
        if self.tcx.def_name(*def) != Some("sync::Shared") {
            return None;
        }
        substs.types().first().copied()
    }

    /// `true` when `ty` is a `sync::Shared` handle, however it was reached.
    ///
    /// A constructor result is tracked by its runtime kind, but a handle
    /// arriving as a parameter has only its type to go on, and both spellings
    /// must reach the same lowering.
    fn ty_is_shared_handle(&self, ty: Ty) -> bool {
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(
            self.tcx.kind_of(cur),
            TyKind::Adt { def, .. } if self.tcx.def_name(*def) == Some("sync::Shared")
        )
    }

    fn seq_search_shim_fits(&self, elem: Ty) -> bool {
        let mut t = elem;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        matches!(
            self.tcx.kind_of(t),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::String | TyKind::Var(_)
        )
    }

    /// Element type of a sequence receiver, peeling references.
    fn seq_elem_ty(&self, receiver_ty: Ty) -> Option<Ty> {
        let mut t = receiver_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        match self.tcx.kind_of(t) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
            TyKind::Array { elem, .. } => Some(*elem),
            _ => None,
        }
    }

    /// A Vec element of this type is reached by its slot address rather than
    /// a loaded word: a user struct is address-is-value at any width (a
    /// one-field struct still projects its field off the slot pointer), and a
    /// tuple / fixed array spans several inline slots.
    fn seq_elem_is_inline_aggregate(&self, elem_ty: Ty) -> bool {
        matches!(
            self.tcx.kind_of(elem_ty),
            TyKind::Tuple(_) | TyKind::Array { .. }
        ) || matches!(
            self.tcx.kind_of(elem_ty),
            TyKind::Adt { def, .. }
                if def.local < u32::MAX - 16 && self.tcx.struct_field_tys(*def).is_some()
        )
    }

    /// Lowers `xs.contains(&n)` / `xs.index_of(&n)` / `xs.count_of(&n)` as a
    /// scan that compares each element with `==`, the same structural
    /// equality the language applies to a bare `a == b`. Used for element
    /// types the flat-slot search shims cannot compare (see
    /// [`Self::seq_search_shim_fits`]).
    fn lower_seq_eq_scan(
        &mut self,
        receiver_local: Local,
        needle: &HirExpr,
        method: &str,
        elem_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let needle_local = self.lower_expr(needle)?;
        // The scan walks its receiver through the `GosVec` surface, so a
        // fixed array - a bare block of slots with no header - is given one
        // first.
        let receiver_local = match self
            .tcx
            .kind_of(self.locals[receiver_local.0 as usize].ty)
            .clone()
        {
            TyKind::Array { elem, len } => {
                self.coerce_array_to_vec(receiver_local, elem, len, span)
            }
            _ => receiver_local,
        };

        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(receiver_local))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        // `found` doubles as `contains`'s result; `idx` carries the first
        // matching position and `count` the number of matches.
        let found = self.push_local(bool_ty, None, true);
        let idx = self.push_local(i64_ty, None, true);
        let count = self.push_local(i64_ty, None, true);
        let counter = self.push_local(i64_ty, None, true);
        for (slot, init) in [(found, 0i128), (idx, 0), (count, 0), (counter, 0)] {
            self.emit_assign(
                Place::local(slot),
                Rvalue::Use(Operand::Const(ConstValue::Int(init))),
                span,
            );
        }

        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let hit_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let in_range = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(in_range),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(in_range)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        let ptr_local = self.fresh(i64_ty);
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
            args: vec![
                Operand::Copy(Place::local(receiver_local)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(ptr_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);
        let elem_local = self.fresh(elem_ty);
        if self.seq_elem_is_inline_aggregate(elem_ty) {
            self.emit_assign(
                Place::local(elem_local),
                Rvalue::Use(Operand::Copy(Place::local(ptr_local))),
                span,
            );
        } else {
            let zero_off = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(zero_off),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                span,
            );
            let after_load = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
                args: vec![
                    Operand::Copy(Place::local(ptr_local)),
                    Operand::Copy(Place::local(zero_off)),
                ],
                destination: Place::local(elem_local),
                target: Some(after_load),
            });
            self.set_current(after_load);
        }
        let is_eq = self.emit_structural_eq(elem_local, needle_local, elem_ty, span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(is_eq)),
            arms: vec![(0, step_block)],
            default: hit_block,
        });

        // `count_of` tallies every match, so it keeps scanning; the other two
        // only need the first one and leave the loop.
        self.set_current(hit_block);
        let counting = method == "count_of";
        if counting {
            self.emit_assign(
                Place::local(count),
                Rvalue::BinaryOp {
                    op: BinOp::Add,
                    lhs: Operand::Copy(Place::local(count)),
                    rhs: Operand::Const(ConstValue::Int(1)),
                },
                span,
            );
            self.terminate(Terminator::Goto { target: step_block });
        } else {
            self.emit_assign(
                Place::local(found),
                Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                span,
            );
            self.emit_assign(
                Place::local(idx),
                Rvalue::Use(Operand::Copy(Place::local(counter))),
                span,
            );
            self.terminate(Terminator::Goto { target: exit });
        }

        self.set_current(step_block);
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Const(ConstValue::Int(1)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        match method {
            "contains" => Some(found),
            "count_of" => Some(count),
            // `index_of` yields `Option<i64>`: discriminant 0 carries the
            // position, 1 is `None`.
            _ => {
                let disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(disc),
                    Rvalue::BinaryOp {
                        op: BinOp::Sub,
                        lhs: Operand::Const(ConstValue::Int(1)),
                        rhs: Operand::Copy(Place::local(found)),
                    },
                    span,
                );
                let rty = self.result_repr_ty(ty);
                let dest = self.fresh(rty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![
                            Operand::Copy(Place::local(disc)),
                            Operand::Copy(Place::local(idx)),
                        ],
                    },
                    span,
                );
                Some(dest)
            }
        }
    }

    fn seq_str_method_from_lowered(
        &self,
        name: &str,
        receiver_ty: Ty,
        args_len: usize,
    ) -> Option<&'static str> {
        let mut ty = receiver_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        let kind = self.tcx.kind_of(ty).clone();
        let is_seq = matches!(
            kind,
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
        );
        let elem_str = |this: &Self| vec_element_kind(this.tcx, ty) == VecElemKind::Str;
        match name {
            // String receiver surface.
            "contains" if matches!(kind, TyKind::String) => Some("gos_rt_str_contains"),
            "find" if matches!(kind, TyKind::String) => Some("gos_rt_str_find_opt"),
            "rfind" if matches!(kind, TyKind::String) => Some("gos_rt_str_rfind_opt"),
            "to_i64" if matches!(kind, TyKind::String) => Some("gos_rt_str_to_i64_opt"),
            "to_f64" if matches!(kind, TyKind::String) => Some("gos_rt_str_to_f64_opt"),
            "to_bool" if matches!(kind, TyKind::String) => Some("gos_rt_str_to_bool_opt"),
            "split_once" if matches!(kind, TyKind::String) => Some("gos_rt_str_split_once"),
            "rsplit_once" if matches!(kind, TyKind::String) => Some("gos_rt_str_rsplit_once"),
            "count" if matches!(kind, TyKind::String) => Some("gos_rt_str_count"),
            "trim_start_matches" if matches!(kind, TyKind::String) => {
                Some("gos_rt_str_lstrip_chars")
            }
            "trim_end_matches" if matches!(kind, TyKind::String) => Some("gos_rt_str_rstrip_chars"),
            "center" if matches!(kind, TyKind::String) => Some("gos_rt_str_center"),
            "slice" if matches!(kind, TyKind::String) => Some("gos_rt_str_slice"),
            "substring" if matches!(kind, TyKind::String) => Some("gos_rt_str_substring"),
            "split_whitespace" if matches!(kind, TyKind::String) => {
                Some("gos_rt_str_split_whitespace")
            }
            "splitn" if matches!(kind, TyKind::String) => Some("gos_rt_str_splitn"),
            "to_title" if matches!(kind, TyKind::String) => Some("gos_rt_str_to_title"),
            "trim_matches" if matches!(kind, TyKind::String) => Some("gos_rt_str_trim_matches"),
            "replacen" if matches!(kind, TyKind::String) => Some("gos_rt_str_replacen"),
            "pad_left" if matches!(kind, TyKind::String) => Some("gos_rt_str_pad_left"),
            "pad_right" if matches!(kind, TyKind::String) => Some("gos_rt_str_pad_right"),
            "contains_any" if matches!(kind, TyKind::String) => Some("gos_rt_str_contains_any"),
            "equal_fold" if matches!(kind, TyKind::String) => Some("gos_rt_str_equal_fold"),
            "find_any" if matches!(kind, TyKind::String) => Some("gos_rt_str_index_any"),
            "rfind_any" if matches!(kind, TyKind::String) => Some("gos_rt_str_last_index_any"),
            "strip_prefix" if matches!(kind, TyKind::String) => Some("gos_rt_str_strip_prefix"),
            "strip_suffix" if matches!(kind, TyKind::String) => Some("gos_rt_str_strip_suffix"),
            // Vec / Slice / Array receiver surface.
            "slice" if matches!(kind, TyKind::Vec(_) | TyKind::Slice(_)) => {
                Some("gos_rt_vec_slice_result")
            }
            "slice" if matches!(kind, TyKind::Array { .. }) => {
                let elem_kind = match &kind {
                    TyKind::Array { elem, .. } => self.tcx.kind_of(*elem),
                    _ => unreachable!(),
                };
                Some(if matches!(elem_kind, TyKind::Float(_)) {
                    "gos_rt_floatarr_slice_result"
                } else if matches!(elem_kind, TyKind::Int(gossamer_types::IntTy::U8)) {
                    "gos_rt_bytearr_slice_result"
                } else {
                    "gos_rt_intarr_slice_result"
                })
            }
            "first" if is_seq => Some("gos_rt_vec_first"),
            "last" if is_seq => Some("gos_rt_vec_last"),
            "get" if args_len == 1 && is_seq => Some("gos_rt_vec_get_opt"),
            "rev" if is_seq => Some("gos_rt_vec_reversed"),
            "take" if args_len == 1 && is_seq => Some("gos_rt_vec_take"),
            "skip" if args_len == 1 && is_seq => Some("gos_rt_vec_skip"),
            "step_by" if args_len == 1 && is_seq => Some("gos_rt_vec_step_by"),
            "join" if args_len == 1 && is_seq => self.vec_join_symbol(ty),
            "contains" if is_seq => Some(if elem_str(self) {
                "gos_rt_vec_contains_str"
            } else {
                "gos_rt_vec_contains_i64"
            }),
            "index_of" if is_seq => Some(if elem_str(self) {
                "gos_rt_vec_index_of_str"
            } else {
                "gos_rt_vec_index_of_i64"
            }),
            "count_of" if is_seq => Some(if elem_str(self) {
                "gos_rt_vec_count_of_str"
            } else {
                "gos_rt_vec_count_of_i64"
            }),
            // HashMap receiver surface.
            "keys" if matches!(kind, TyKind::HashMap { .. }) => Some("gos_rt_map_keys_vec"),
            "values" if matches!(kind, TyKind::HashMap { .. }) => Some("gos_rt_map_values_vec"),
            "pop" if matches!(kind, TyKind::HashMap { .. }) => {
                Some(if hashmap_key_kind(self.tcx, ty) == VecElemKind::Str {
                    "gos_rt_map_pop_typed_str"
                } else {
                    "gos_rt_map_pop_i64"
                })
            }
            _ => None,
        }
    }
}
