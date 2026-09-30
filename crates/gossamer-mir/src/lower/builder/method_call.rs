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

    /// Suffix of the producer symbol that hands out one value of this family:
    /// a `String` stream counts the share each pull carries, so it has
    /// producers of its own.
    pub(crate) fn value_suffix(self) -> &'static str {
        match self {
            Self::Ptr => "str",
            other => other.word_or_float_suffix(),
        }
    }

    /// Runtime symbol that borrows a sequence of this family as lazy state.
    pub(crate) fn vec_source_symbol(self) -> &'static str {
        match self {
            Self::Float => "gos_rt_lazy_iter_from_vec_f64",
            Self::Ptr => "gos_rt_lazy_iter_from_vec_str",
            Self::Aggr => "gos_rt_lazy_iter_from_vec_aggr",
            Self::Word | Self::PairWord => "gos_rt_lazy_iter_from_vec_i64",
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
                let helper = family.vec_source_symbol();
                let source = self.lower_expr(receiver)?;
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(source))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
        }
        // An iterator over elements wider than a slot, lowered as a value,
        // is the address-carrying state: it keeps its position however it
        // is held, bound, returned, or stored. A consumer written directly
        // on `xs.iter()` reads the sequence before this is reached.
        if method.name == "iter"
            && args.is_empty()
            && let TyKind::Iterator(elem) = self.tcx.kind_of(ty).clone()
            && self.lazy_elem_is_addressed(elem)
        {
            let mut receiver_ty = receiver.ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(receiver_ty) {
                receiver_ty = *inner;
            }
            if matches!(
                self.tcx.kind_of(receiver_ty),
                TyKind::Vec(_) | TyKind::Slice(_)
            ) {
                let source = self.lower_expr(receiver)?;
                return Some(self.canonical_wide_iterator(source, ty, span));
            }
        }

        if matches!(
            method.name.as_str(),
            "__gos_wrapping_add" | "__gos_wrapping_sub" | "__gos_wrapping_mul"
        ) && args.len() == 1
        {
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
                let op = match method.name.as_str() {
                    "__gos_wrapping_add" => BinOp::WrappingAdd,
                    "__gos_wrapping_sub" => BinOp::WrappingSub,
                    _ => BinOp::WrappingMul,
                };
                // The op runs at i64 width. A type narrower than that wraps
                // at its own width, so the wide result is narrowed back.
                let narrow = match self.tcx.kind_of(receiver_ty) {
                    TyKind::Int(int_ty)
                        if crate::lower::builder::expr::narrow_int_width(*int_ty).is_some() =>
                    {
                        Some(receiver_ty)
                    }
                    _ => None,
                };
                let wide_ty = if narrow.is_some() {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                } else {
                    ty
                };
                let dest = self.fresh(wide_ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::BinaryOp {
                        op,
                        lhs: Operand::Copy(Place::local(lhs)),
                        rhs: Operand::Copy(Place::local(rhs)),
                    },
                    span,
                );
                let Some(narrow_ty) = narrow else {
                    return Some(dest);
                };
                let narrowed = self.fresh(narrow_ty);
                self.emit_assign(
                    Place::local(narrowed),
                    Rvalue::Cast {
                        operand: Operand::Copy(Place::local(dest)),
                        target: narrow_ty,
                    },
                    span,
                );
                return Some(narrowed);
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
            self.lower_numeric_math_method(receiver, method, args, ty, span)
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

        // `carrier.map(f)` calls the closure in this frame, so its argument
        // and answer keep their own types - a float, an aggregate, another
        // carrier - and the answer is boxed the way a `Some` / `Ok` literal
        // boxes one, rather than crossing a callback as one integer word.
        if method.name.as_str() == "map"
            && args.len() == 1
            && matches!(receiver_kind_flat, TyKind::Adt { .. })
            && self.is_result_or_option_adt(receiver_ty)
            && let Some(mapped_ty) = self.enum_payload_ty(ty, 0)
            && !matches!(
                self.tcx.kind_of(mapped_ty),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            )
        {
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
            let recv = self.lower_expr(receiver)?;
            let closure = self.lower_iter_closure(&args[0], &[payload_ty], mapped_ty, span)?;
            return Some(self.lower_map_inline(recv, closure, receiver_ty, mapped_ty, ty, span));
        }

        // `a.or(b)` on an `Option` receiver answers `a` when it holds a value
        // and `b` otherwise. The generic table has no row for it, so route it
        // to the intrinsic the `option::or` free form takes, whose arguments
        // come data-last: the alternative, then the receiver.
        if method.name.as_str() == "or"
            && args.len() == 1
            && matches!(receiver_kind_flat, TyKind::Adt { .. })
            && self.is_option_adt(receiver_ty)
        {
            let ordered = vec![args[0].clone(), receiver.clone()];
            if let Some(local) = self.try_lower_combinator_call("option::or", &ordered, ty, span) {
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
            // An enum is not in the struct index, so its impl is reached
            // through the enum index. Without it a receiver whose only
            // evidence is its type - a parameter - misses the declared
            // receiver and is handed over by value.
            .or_else(|| self.enum_index_name_of(receiver_ty))
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
                // from that slot once the callee has written through it. A
                // payload enum is the same case: `*self = Variant(..)` names a
                // whole new node, so the reference has to name the receiver's
                // slot rather than a copy of the node pointer.
                if mutable
                    && receiver_place.projection.is_empty()
                    && (matches!(
                        self.tcx.kind_of(receiver_inner),
                        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                    ) || self.tcx.is_payload_enum(receiver_inner))
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
            TyKind::Tuple(_) if crate::lower::helpers::lazy_iter_is_pair_state(self.tcx, elem) => {
                Some(LazyElemFamily::PairWord)
            }
            _ => None,
        }
    }

    /// Whether a lazy stream carries `elem` as the address of its storage: a
    /// tuple, a struct, or an array.
    pub(crate) fn lazy_elem_is_addressed(&self, elem: Ty) -> bool {
        match self.tcx.kind_of(elem) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            // An `Option` / `Result` is a discriminant and a payload word.
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1 => true,
            TyKind::Adt { def, .. } => {
                self.tcx.enum_variant_tys(*def).is_none()
                    && self.tcx.struct_field_tys(*def).is_some()
            }
            _ => false,
        }
    }
}

mod kind_dispatch;
mod receivers;
mod sequences;
mod symbols;
