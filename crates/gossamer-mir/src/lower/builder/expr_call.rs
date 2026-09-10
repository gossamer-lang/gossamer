#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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

impl Builder<'_> {
    /// A struct or enum rendered through the method its channel names, so a
    /// value the concat planner cannot route reaches it as the String it
    /// renders as. Any other local passes through unchanged.
    ///
    /// `method` is `to_string` for `Display` (`{}`) and `fmt` for `Debug`
    /// (`{:?}`); a struct with no `impl Display` still carries the
    /// synthesized structural `fmt`, so the two channels stay distinct.
    pub(crate) fn adt_fmt_rendered(&mut self, local: Local, method: &str, span: Span) -> Local {
        let arg_ty = self.locals[local.0 as usize].ty;
        let Some(sname) = self
            .adt_dispatch_name(arg_ty)
            .or_else(|| self.local_struct.get(&local).cloned())
            .or_else(|| self.runtime_kind_dispatch_name(local))
        else {
            return local;
        };
        if !self
            .impl_methods
            .contains_key(&format!("{sname}::{method}"))
        {
            return local;
        }
        let str_ty = self.tcx.string_ty();
        let dest = self.fresh(str_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(format!("{sname}::{method}"))),
            args: vec![Operand::Copy(Place::local(local))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }
}

impl<'a> Builder<'a> {
    /// `true` if `ty` mentions a generic `Param` anywhere in its
    /// structure. A generic function's declared return type carries rigid
    /// `Param` slots; grounding a call's result from such a return would
    /// leak the un-instantiated parameter into codegen instead of the
    /// call's already-instantiated concrete type.
    pub(crate) fn ty_mentions_param(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Param { .. } => true,
            TyKind::Ref { inner, .. }
            | TyKind::Slice(inner)
            | TyKind::Vec(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner) => self.ty_mentions_param(*inner),
            TyKind::Array { elem, .. } => self.ty_mentions_param(*elem),
            TyKind::Tuple(elems) => elems.iter().any(|e| self.ty_mentions_param(*e)),
            TyKind::HashMap { key, value, .. } => {
                self.ty_mentions_param(*key) || self.ty_mentions_param(*value)
            }
            TyKind::Adt { substs, .. } | TyKind::Alias { substs, .. } => {
                substs.types().iter().any(|t| self.ty_mentions_param(*t))
            }
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                let sig = sig.clone();
                sig.inputs.iter().any(|t| self.ty_mentions_param(*t))
                    || self.ty_mentions_param(sig.output)
            }
            _ => false,
        }
    }

    /// Substitutes `subst[i]` for each `Param(i)` in `ty`, recursing through
    /// composite types and interning new ones. Used to instantiate a generic
    /// method's `Param` return / field type from the receiver's concrete
    /// generic arguments, so a method call's destination carries the real
    /// type (`Wrapper<i64>::get -> i64`) instead of an opaque `Param` slot.
    pub(crate) fn subst_params_with(&mut self, ty: Ty, subst: &[Ty]) -> Ty {
        use gossamer_types::{GenericArg, Substs, TyKind};
        match self.tcx.kind_of(ty).clone() {
            TyKind::Param { idx, .. } => subst.get(idx.0 as usize).copied().unwrap_or(ty),
            TyKind::Ref { inner, mutability } => {
                let inner = self.subst_params_with(inner, subst);
                self.tcx.intern(TyKind::Ref { inner, mutability })
            }
            TyKind::Vec(elem) => {
                let elem = self.subst_params_with(elem, subst);
                self.tcx.intern(TyKind::Vec(elem))
            }
            TyKind::Slice(elem) => {
                let elem = self.subst_params_with(elem, subst);
                self.tcx.intern(TyKind::Slice(elem))
            }
            TyKind::Array { elem, len } => {
                let elem = self.subst_params_with(elem, subst);
                self.tcx.intern(TyKind::Array { elem, len })
            }
            TyKind::Tuple(elems) => {
                let elems = elems
                    .iter()
                    .map(|&e| self.subst_params_with(e, subst))
                    .collect();
                self.tcx.intern(TyKind::Tuple(elems))
            }
            TyKind::HashMap {
                key,
                value,
                ordered,
            } => {
                let key = self.subst_params_with(key, subst);
                let value = self.subst_params_with(value, subst);
                self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered,
                })
            }
            TyKind::Adt { def, substs } => {
                let new_args = substs
                    .as_slice()
                    .iter()
                    .map(|a| match a {
                        GenericArg::Type(t) => GenericArg::Type(self.subst_params_with(*t, subst)),
                        GenericArg::Const(c) => GenericArg::Const(*c),
                    })
                    .collect();
                self.tcx.intern(TyKind::Adt {
                    def,
                    substs: Substs::from_args(new_args),
                })
            }
            _ => ty,
        }
    }

    /// The concrete type arguments of an ADT receiver (`Wrapper<i64>` ->
    /// `[i64]`), seeing through a single `&`/`&mut`. Empty for a non-ADT or
    /// non-generic receiver.
    pub(crate) fn adt_substs_vec(&self, mut ty: Ty) -> Vec<Ty> {
        use gossamer_types::TyKind;
        if let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Adt { substs, .. } => substs.types(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn lower_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        // `http::serve(addr, handler)` shortcut: pass the handler's
        // serve method address as a third argument so the runtime
        // can dispatch back into Gossamer code per request.
        if let HirExprKind::Path { segments, .. } = &callee.kind {
            let joined: String = segments
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("::");
            if joined == "http::serve" && args.len() == 2 {
                if let Some(local) = self.lower_http_serve(&args[0], &args[1], ty, span) {
                    return Some(local);
                }
            }
            // `httptest::record(handler, method, path, body)` - the handler
            // is called with a request built in memory, so it needs the
            // same dispatch address `http::serve` resolves.
            if joined == "httptest::record" && args.len() == 4 {
                if let Some(local) =
                    self.lower_httptest_record(&args[0], &args[1], &args[2], &args[3], span)
                {
                    return Some(local);
                }
            }
            // `http::serve_tls(addr, cert_pem, key_pem, handler)` - the
            // TLS-terminating variant. Same handler-fn-ptr dispatch as
            // `http::serve`, with the cert + key PEM threaded ahead of
            // the handler.
            if joined == "http::serve_tls" && args.len() == 4 {
                if let Some(local) =
                    self.lower_http_serve_tls(&args[0], &args[1], &args[2], &args[3], ty, span)
                {
                    return Some(local);
                }
            }
            // `websocket::serve(addr, handler)` - same handler-fn-ptr
            // dispatch as `http::serve`, but resolves the handler's
            // `handle(&self, ws: i64)` method and emits `gos_rt_ws_serve`.
            if matches!(
                joined.as_str(),
                "websocket::serve" | "http::websocket::serve"
            ) && args.len() == 2
            {
                if let Some(local) = self.lower_websocket_serve(&args[0], &args[1], ty, span) {
                    return Some(local);
                }
            }
            // `http_h3::serve(addr, cert_path, key_path, handler)` -
            // same handler-fn-ptr dispatch as `http::serve` with two
            // extra leading string args (the TLS keypair file paths).
            if joined == "http_h3::serve" && args.len() == 4 {
                if let Some(local) =
                    self.lower_http3_serve(&args[0], &args[1], &args[2], &args[3], ty, span)
                {
                    return Some(local);
                }
            }
            // `sql::register_native(name, driver)` (autoderive-mangled
            // to `__gos_sql_register_native`): capture the driver's env
            // + `gos_fn_addr("<Type>::dispatch")` so the runtime can
            // dispatch back into the `.gos` driver per op. Same Rust ->
            // Gossamer bridge as `http::serve`.
            if joined == "__gos_sql_register_native" && args.len() == 2 {
                if let Some(local) = self.lower_sql_register_native(&args[0], &args[1], ty, span) {
                    return Some(local);
                }
            }
            // `http::serve_h2c(addr, handler, config)` - ignore the
            // config argument in compiled mode and use the runtime
            // default; reuses the same handler-fn-ptr dispatch as
            // http::serve. Renamed from the original `http2::*`
            // spelling when HTTP/2 was folded into std::http per
            // the Go model (0.4.0).
            if joined == "http::serve_h2c" && args.len() >= 2 {
                if let Some(local) = self.lower_http2_bind_and_run_h2c(&args[0], &args[1], ty, span)
                {
                    return Some(local);
                }
            }
            // `flag::define(name, [flag::int(...), flag::string(...),
            // flag::bool(...)])` - declarative one-shot construction.
            // Expand to the imperative `flag::Set` builder pattern at
            // MIR level so the compiled tier reuses the now-working
            // cell-load helpers.
            if joined == "flag::define" && args.len() == 2 {
                if let Some(local) = self.lower_flag_define(&args[0], &args[1], ty, span) {
                    return Some(local);
                }
            }
            // `Box::new(x)` / `Arc::new(x)` / `Rc::new(x)` are identity
            // wrappers in a managed language - every value already lives on
            // the managed heap - so the argument itself is the answer, and a
            // generic dispatch on the name would hand back a typed zero in
            // its place. A program that declares one of these names owns its
            // associated functions, so `impl Box { fn new(p: i64) -> Box }`
            // is what `Box::new(5)` reaches; the passthrough answers only for
            // the wrapper nothing in the program declares.
            if matches!(joined.as_str(), "Box::new" | "Arc::new" | "Rc::new")
                && args.len() == 1
                && !self.impl_methods.contains_key(joined.as_str())
            {
                return self.lower_expr(&args[0]);
            }
            // `String::from(s)` is identity for a string argument - gos
            // `String` is already the owned C-string representation and a
            // string literal is already a `String`, mirroring Rust's
            // `String::from(&str)` without a separate conversion.
            if joined == "String::from"
                && args.len() == 1
                && !self.impl_methods.contains_key(joined.as_str())
            {
                return self.lower_expr(&args[0]);
            }
            // Fuse the common `format!("prefix{:08}", integer)` shape all
            // the way through concatenation. `__fmt_pad` already avoids an
            // intermediate decimal string; recognizing it as a concat piece
            // also avoids allocating the padded fragment only to copy it
            // immediately into the final string.
            if joined == "__concat"
                && args.len() == 2
                && let HirExprKind::Call {
                    callee: pad_callee,
                    args: pad_args,
                } = &args[1].kind
                && let HirExprKind::Path {
                    segments: pad_segments,
                    ..
                } = &pad_callee.kind
                && pad_segments.len() == 1
                && pad_segments[0].name.as_str() == "__fmt_pad"
                && pad_args.len() == 4
                && let HirExprKind::Call {
                    callee: rendered_callee,
                    args: rendered_args,
                } = &pad_args[0].kind
                && let HirExprKind::Path {
                    segments: rendered_segments,
                    ..
                } = &rendered_callee.kind
                && rendered_segments.len() == 1
                && rendered_segments[0].name.as_str() == "__concat"
                && rendered_args.len() == 1
                && matches!(
                    self.tcx.kind_of(rendered_args[0].ty),
                    gossamer_types::TyKind::Int(_)
                )
            {
                let mut locals = Vec::with_capacity(5);
                locals.push(self.lower_expr(&args[0])?);
                locals.push(self.lower_expr(&rendered_args[0])?);
                for arg in &pad_args[1..] {
                    locals.push(self.lower_expr(arg)?);
                }
                // The helper answers the joined `String`, so the destination
                // carries that type: a backend that reads the MIR type to pick
                // the value's machine shape needs it to say `String`, not the
                // word a string handle happens to fit in.
                let string_ty = self.tcx.string_ty();
                let dest = self.fresh(string_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_concat_pad_i64".to_string())),
                    args: locals
                        .into_iter()
                        .map(|local| Operand::Copy(Place::local(local)))
                        .collect(),
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
            if matches!(
                joined.as_str(),
                "Vec::from" | "collections::Vec::from" | "std::collections::Vec::from"
            ) && args.len() == 1
            {
                if let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) = &args[0].kind {
                    let dest = self.fresh(ty);
                    if self.lower_let_array_as_vec(dest, elems, span) {
                        return Some(dest);
                    }
                }
                if let HirExprKind::Array(gossamer_hir::HirArrayExpr::Repeat { value, count }) =
                    &args[0].kind
                {
                    let dest = self.fresh(ty);
                    return self.lower_array_repeat_into(value, count, ty, span, Some(dest));
                }
                let source = self.lower_expr(&args[0])?;
                if let TyKind::Array { elem, len } =
                    self.tcx.kind_of(self.locals[source.0 as usize].ty).clone()
                {
                    return Some(self.fixed_array_to_vec(source, elem, len, span));
                }
                return Some(source);
            }
            if args.is_empty()
                && let Some(local) = self.try_lower_slot_container_new(joined.as_str(), ty, span)
            {
                return Some(local);
            }
            if matches!(
                joined.as_str(),
                "Deque::from"
                    | "VecDeque::from"
                    | "collections::Deque::from"
                    | "collections::VecDeque::from"
                    | "std::collections::Deque::from"
                    | "std::collections::VecDeque::from"
                    | "Queue::from"
                    | "VecQueue::from"
                    | "collections::Queue::from"
                    | "collections::VecQueue::from"
                    | "std::collections::Queue::from"
                    | "std::collections::VecQueue::from"
                    | "Stack::from"
                    | "VecStack::from"
                    | "collections::Stack::from"
                    | "collections::VecStack::from"
                    | "std::collections::Stack::from"
                    | "std::collections::VecStack::from"
            ) && args.len() == 1
            {
                let (callee, runtime_kind) = match joined.as_str() {
                    "Queue::from"
                    | "VecQueue::from"
                    | "collections::Queue::from"
                    | "collections::VecQueue::from"
                    | "std::collections::Queue::from"
                    | "std::collections::VecQueue::from" => {
                        ("gos_rt_queue_from_vec_i64", "collections::VecQueue")
                    }
                    "Stack::from"
                    | "VecStack::from"
                    | "collections::Stack::from"
                    | "collections::VecStack::from"
                    | "std::collections::Stack::from"
                    | "std::collections::VecStack::from" => {
                        ("gos_rt_stack_from_vec_i64", "collections::VecStack")
                    }
                    _ => ("gos_rt_deque_from_vec_i64", "collections::VecDeque"),
                };
                let mut source = self.lower_expr(&args[0])?;
                if let TyKind::Array { elem, len } =
                    self.tcx.kind_of(self.locals[source.0 as usize].ty).clone()
                {
                    source = self.coerce_array_to_vec(source, elem, len, span);
                }
                let handle_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let dest = self.fresh(handle_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee.to_string())),
                    args: vec![Operand::Copy(Place::local(source))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                self.local_runtime_kind.insert(dest, runtime_kind);
                return Some(dest);
            }
            if matches!(
                joined.as_str(),
                "BinaryHeap::from"
                    | "collections::BinaryHeap::from"
                    | "std::collections::BinaryHeap::from"
                    | "MaxBinaryHeap::from"
                    | "collections::MaxBinaryHeap::from"
                    | "std::collections::MaxBinaryHeap::from"
                    | "MaxHeap::from"
                    | "collections::MaxHeap::from"
                    | "std::collections::MaxHeap::from"
                    | "MinBinaryHeap::from"
                    | "collections::MinBinaryHeap::from"
                    | "std::collections::MinBinaryHeap::from"
                    | "MinHeap::from"
                    | "collections::MinHeap::from"
                    | "std::collections::MinHeap::from"
            ) && args.len() == 1
            {
                let mut source = self.lower_expr(&args[0])?;
                if let TyKind::Array { elem, len } =
                    self.tcx.kind_of(self.locals[source.0 as usize].ty).clone()
                {
                    source = self.coerce_array_to_vec(source, elem, len, span);
                }
                let is_min = matches!(
                    joined.as_str(),
                    "MinBinaryHeap::from"
                        | "collections::MinBinaryHeap::from"
                        | "std::collections::MinBinaryHeap::from"
                        | "MinHeap::from"
                        | "collections::MinHeap::from"
                        | "std::collections::MinHeap::from"
                ) || self.binary_heap_ty_is_min(ty)
                    || self.binary_heap_elem_is_reverse_i64(ty);
                let float_elem = self.heap_elem_is_float(ty);
                // An element the one-word entry points cannot order carries
                // its ordering descriptor into the heapify.
                let desc = self
                    .first_generic_of(ty)
                    .filter(|elem| !float_elem && self.heap_elem_needs_desc(*elem))
                    .and_then(|elem| self.ordering_stream(elem));
                let symbol = match (is_min, float_elem, desc.is_some()) {
                    (true, _, true) => "gos_rt_bheap_min_from_vec_desc",
                    (false, _, true) => "gos_rt_bheap_max_from_vec_desc",
                    (true, true, _) => "gos_rt_bheap_min_from_vec_f64",
                    (true, false, _) => "gos_rt_bheap_min_from_vec_i64",
                    (false, true, _) => "gos_rt_bheap_max_from_vec_f64",
                    (false, false, _) => "gos_rt_bheap_max_from_vec_i64",
                };
                let mut call_args = vec![Operand::Copy(Place::local(source))];
                if let Some(stream) = desc {
                    let text: String = stream.iter().map(|&b| b as char).collect();
                    call_args.push(Operand::Const(ConstValue::Str(text)));
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
                self.local_runtime_kind.insert(
                    dest,
                    if is_min {
                        "collections::MinHeap"
                    } else {
                        "collections::MaxHeap"
                    },
                );
                if is_min && self.binary_heap_elem_is_reverse_i64(ty) {
                    self.local_binary_heap_min_i64.insert(dest);
                }
                return Some(dest);
            }
            if joined == "String::from_utf8" && args.len() == 1 {
                let mut bytes_local = self.lower_expr(&args[0])?;
                if let TyKind::Array { elem, len } = self
                    .tcx
                    .kind_of(self.locals[bytes_local.0 as usize].ty)
                    .clone()
                {
                    bytes_local = self.coerce_array_to_vec(bytes_local, elem, len, span);
                }
                let result_ty = self.result_string_error_adt_ty();
                let dest = self.fresh(result_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_string_from_utf8".to_string())),
                    args: vec![Operand::Copy(Place::local(bytes_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
            // `String::new()` / `String::with_capacity(_)` materialise
            // an empty owned String. Gos `String` is the runtime's
            // C-string representation, and the empty literal is the
            // canonical zero value. Without this passthrough the
            // call ends up as a typed-zero stub at codegen and any
            // downstream `push_str` (rewritten to `__concat`) sees
            // a null/garbage receiver instead of a real empty bytes
            // pointer.
            if joined == "String::with_capacity" && args.len() == 1 {
                let cap = self.lower_expr(&args[0])?;
                let str_ty = self.tcx.string_ty();
                let dest = self.fresh(str_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_with_capacity".to_string())),
                    args: vec![Operand::Copy(Place::local(cap))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
            if joined == "String::new" && args.is_empty() {
                let str_ty = self.tcx.string_ty();
                let dest = self.fresh(str_ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                    span,
                );
                return Some(dest);
            }
            // `Vec::with_capacity(n)` / `Vec::<T>::with_capacity(n)` reserves
            // an n-element buffer up front. The runtime helper takes
            // `(elem_bytes, cap)`, but the surface form passes only the
            // count, so derive the element width from the destination vec
            // type and emit the canonical two-argument call - the same shape
            // the `[v; n]` repeat lowering uses. Keyed under
            // `gos_rt_vec_with_capacity`, so the drop pass frees the result
            // like any other constructor-owned vec.
            if matches!(
                joined.as_str(),
                "Vec::with_capacity" | "collections::Vec::with_capacity"
            ) && args.len() == 1
            {
                use gossamer_types::{IntTy, TyKind};
                if let TyKind::Vec(e) | TyKind::Slice(e) = self.tcx.kind_of(ty) {
                    let elem_ty = *e;
                    let cap_local = self.lower_expr(&args[0])?;
                    let i64_ty = self.tcx.int_ty(IntTy::I64);
                    let elem_bytes_val = i128::from(self.elem_bytes_of(elem_ty).max(1));
                    let elem_bytes_local = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(elem_bytes_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(elem_bytes_val))),
                        span,
                    );
                    let dest = self.fresh(ty);
                    let after = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_vec_with_capacity".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(elem_bytes_local)),
                            Operand::Copy(Place::local(cap_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(after),
                    });
                    self.set_current(after);
                    return Some(dest);
                }
            }
        }
        // Variant constructor shortcut for `Result<T, E>` and
        // `Option<T>`: `Ok(v)` / `Err(v)` / `Some(v)` lower to
        // a `gos_rt_result_new(disc, payload)` call so the
        // resulting handle carries a real discriminant. Match
        // dispatch and `?`-propagation rely on the disc bit being
        // present at runtime.
        if let HirExprKind::Path { segments, .. } = &callee.kind {
            let last = segments.last().map(|s| s.name.as_str());
            let disc = match last {
                Some("Ok" | "Some") => Some(0),
                Some("Err") => Some(1),
                _ => None,
            };
            if let Some(disc) = disc {
                if args.len() == 1 {
                    return self.lower_result_ctor(disc, &args[0], ty, span);
                }
            }
            // User-defined enum variants with payloads: `List::Cons
            // (v, rest)`. Allocate `[disc, p0, p1, ...]` on the heap
            // and return the pointer. Match dispatch reads disc from
            // offset 0 via `gos_rt_enum_disc`.
            if !args.is_empty()
                && let Some((enum_name, idx)) = self.enums.lookup(segments)
            {
                let result = self.lower_user_enum_ctor(
                    &enum_name,
                    u32::try_from(idx).unwrap_or(0),
                    args,
                    ty,
                    span,
                );
                // Tag the result local with its enum name. The checker leaves a
                // variant constructor's type a `Var`, so `==` / `.method()` /
                // `{:?}` on the value (or a `let`-bound copy of it, propagated
                // by the Let lowering) recover the enum from `local_struct`.
                if let Some(local) = result {
                    self.local_struct.insert(local, enum_name);
                }
                return result;
            }
            // F#-style iter / option / result combinator surface
            // (SPEC §10.4 / §10.4a / §10.4b). Data-last; closures
            // pass through `coerce_to_fn_trait_if_needed` so the
            // unified callable infra builds a real env pointer.
            let joined: String = segments
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("::");
            if let Some(local) = self.try_lower_iter_call(&joined, args, ty, span) {
                return Some(local);
            }
            if let Some(local) = self.try_lower_option_call(&joined, args, ty, span) {
                return Some(local);
            }
            if let Some(local) = self.try_lower_combinator_call(&joined, args, ty, span) {
                return Some(local);
            }
        }
        // When the callee's `DefId` is known and its declared
        // return type is on record, prefer the callee's return
        // type over the call-expression's HIR type - the latter
        // may still be an inference variable.
        let ty = if let HirExprKind::Path { def: Some(def), .. } = &callee.kind {
            // Ground an unresolved call-expression type from the callee's
            // declared return type; the checker often leaves the former an
            // inference variable for cross-function calls. A concrete call
            // type is already the instantiated result - a generic call's
            // `id(s): String` is authoritative - so it is kept. A generic
            // declared return carries rigid `Param` slots that never ground
            // anything, so such a return is skipped in favor of the call's
            // own type.
            use gossamer_types::TyKind;
            if matches!(self.tcx.kind_of(ty), TyKind::Error | TyKind::Var(_)) {
                if let Some(registered) = self.fn_returns.get(def).copied() {
                    if matches!(self.tcx.kind_of(registered), TyKind::Error)
                        || self.ty_mentions_param(registered)
                    {
                        ty
                    } else {
                        registered
                    }
                } else {
                    ty
                }
            } else {
                ty
            }
        } else {
            ty
        };
        // Pin the call's dest type for known stdlib path callees
        // whose return kind is fixed. The typechecker leaves most
        // stdlib call-expression types as `Var` because no impl
        // index tracks them; the codegen then defaults to pointer-
        // or int-typed registers. Fix the printable kind here.
        let ty = {
            use gossamer_types::TyKind;
            if let HirExprKind::Path {
                segments,
                def: None,
                ..
            } = &callee.kind
            {
                let joined = segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                if matches!(self.tcx.kind_of(ty), TyKind::Error | TyKind::Var(_)) {
                    match joined.as_str() {
                        "math::sqrt" | "math::sin" | "math::cos" | "math::ln" | "math::log"
                        | "math::exp" | "math::abs" | "math::floor" | "math::ceil"
                        | "math::pow" | "time::now" => {
                            self.tcx.float_ty(gossamer_types::FloatTy::F64)
                        }
                        "time::now_ns" | "time::now_ms" | "strconv::parse_i64"
                        | "gos_rt_math_sqrt" => self.tcx.int_ty(gossamer_types::IntTy::I64),
                        // String-returning stdlib helpers. The
                        // runtime returns a `*mut c_char` which the
                        // codegen needs to know is a String so
                        // `.len()` / `.trim()` / `.as_bytes()` etc.
                        // dispatch to the `gos_rt_str_*` family
                        // instead of the generic `gos_rt_len`. The
                        // typechecker leaves these as `Var` because
                        // it doesn't index stdlib free functions;
                        // pin here as the last grounding step.
                        "path::join" | "std::path::join" | "io::read_line"
                        | "std::io::read_line" | "format" => self.tcx.string_ty(),
                        _ => ty,
                    }
                } else {
                    ty
                }
            } else {
                ty
            }
        };
        // When the callee is a single-segment Path bound to a
        // local whose static type is a callable (`FnPtr` /
        // `FnTrait`), and the call expression's HIR type is
        // unresolved, extract the return type from the callee
        // signature directly. Without this, `add5(3)` (for
        // `add5: fn(i64) -> i64`) leaves the result as an
        // inference variable, which the print path then treats
        // as String - producing a `strlen` segfault on the i64
        // bit pattern returned from the closure body.
        let ty = {
            use gossamer_types::TyKind;
            if matches!(self.tcx.kind_of(ty), TyKind::Error | TyKind::Var(_)) {
                if let HirExprKind::Path {
                    segments,
                    def: None,
                    ..
                } = &callee.kind
                {
                    if segments.len() == 1 {
                        if let Some(local) = self.lookup_local(&segments[0].name) {
                            let local_ty = self.locals[local.0 as usize].ty;
                            match self.tcx.kind_of(local_ty) {
                                TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => sig.output,
                                TyKind::FnDef { def, substs } => {
                                    let _ = substs;
                                    self.fn_returns.get(def).copied().unwrap_or(ty)
                                }
                                _ => ty,
                            }
                        } else {
                            ty
                        }
                    } else {
                        ty
                    }
                } else {
                    ty
                }
            } else {
                ty
            }
        };
        if let Some(local) = self.lower_struct_call(callee, args, ty, span) {
            return Some(local);
        }
        // Free-function `json::*` calls that route to runtime
        // helpers. Detect by joined path so the same lowering fires
        // whether the user wrote `use std::encoding::json` and
        // `json::parse(...)` or the fully-qualified
        // `std::encoding::json::parse(...)` form.
        if let Some(local) = self.lower_json_free_call(callee, args, span) {
            return Some(local);
        }
        if let Some(local) = self.lower_dyn_value_ctor(callee, args, span) {
            return Some(local);
        }
        // External Rust binding (`tuigoose::layout::rect`, etc.).
        // Resolves through `gossamer_resolve::external` populated
        // either by the runner's `install_all` or the build-time
        // `ensure_signatures` pass.
        if let Some(local) = self.lower_external_binding_call(callee, args, span) {
            return Some(local);
        }
        // Same for the rest of the stdlib that maps cleanly to
        // a single runtime helper (errors, regex, fs, path,
        // bufio, http, gzip, slog, testing, …).
        if let Some(local) = self.lower_stdlib_free_call(callee, args, span) {
            return Some(local);
        }
        // If the callee is a bare path that resolves to a local
        // previously registered as a lifted closure, dispatch
        // statically to that closure's top-level function and pass
        // the env pointer as the implicit first argument.
        if let HirExprKind::Path {
            segments,
            def: None,
            ..
        } = &callee.kind
        {
            if segments.len() == 1 {
                if let Some(local) = self.lookup_local(&segments[0].name) {
                    if let Some(fn_name) = self.local_closure.get(&local).cloned() {
                        let mut arg_operands = Vec::with_capacity(args.len() + 1);
                        arg_operands.push(Operand::Copy(Place::local(local)));
                        for arg in args {
                            let a = self.lower_expr(arg)?;
                            let a = self.auto_deref_cell(a, span);
                            arg_operands.push(Operand::Copy(Place::local(a)));
                        }
                        let dest = self.fresh(ty);
                        let next = self.new_block(span);
                        self.terminate(Terminator::Call {
                            callee: Operand::Const(ConstValue::Str(fn_name)),
                            args: arg_operands,
                            destination: Place::local(dest),
                            target: Some(next),
                        });
                        self.set_current(next);
                        return Some(dest);
                    }
                }
            }
        }
        // Pre-compute the joined path name for impl-method detection
        // and destination-type pinning (used twice below).
        let joined_path = match &callee.kind {
            HirExprKind::Path { segments, .. } => Some(
                segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::"),
            ),
            _ => None,
        };
        // If the callee is an impl-method path, pin `ty` to the
        // method's declared return type (the resolver doesn't track
        // impl methods, so the call expression's HIR type is often
        // an unresolved variable for this case).
        let ty = if let Some(name) = joined_path.as_ref() {
            if let Some(Some(ret)) = self.impl_methods.get(name).copied() {
                use gossamer_types::TyKind;
                if matches!(self.tcx.kind_of(ty), TyKind::Error | TyKind::Var(_)) {
                    ret
                } else {
                    ty
                }
            } else {
                ty
            }
        } else {
            ty
        };
        let callee_operand = match &callee.kind {
            HirExprKind::Path { def: Some(def), .. }
                if joined_path
                    .as_ref()
                    .is_some_and(|n| self.impl_methods.contains_key(n)) =>
            {
                let _ = def;
                let name = joined_path
                    .clone()
                    .expect("joined_path guarded by `is_some_and` above");
                Operand::Const(ConstValue::Str(name))
            }
            HirExprKind::Path { def: Some(def), .. } => Operand::FnRef {
                def: *def,
                substs: self.substs_of(callee.ty),
            },
            HirExprKind::Path {
                segments,
                def: None,
                ..
            } => {
                // Only treat a bare local as an indirect closure
                // callee when it came from a function parameter.
                // Other locals (e.g. bound to `Const(Str(name))`
                // by a `let f = bare_name`) still flow through the
                // by-name callee lookup so the direct dispatch path
                // resolves them to the named function body.
                if segments.len() == 1 {
                    if let Some(local) = self.lookup_local(&segments[0].name) {
                        use gossamer_types::TyKind;
                        // Prefer the recorded function-name binding
                        // when the local holds a `Const(Str(name))`
                        // (e.g. `let plus = __closure_0; plus(...)`).
                        // Falling back to the segment name alone
                        // loses the pointer to the synthesised body.
                        if let Some(name) = self.local_fn_name.get(&local).cloned() {
                            Operand::Const(ConstValue::Str(name))
                        } else if self.param_locals.contains(&local) {
                            Operand::Copy(Place::local(local))
                        } else if matches!(
                            self.tcx.kind_of(self.locals[local.0 as usize].ty),
                            TyKind::FnPtr(_)
                                | TyKind::FnDef { .. }
                                | TyKind::Closure { .. }
                                | TyKind::FnTrait(_)
                        ) {
                            // Local bound to a function-typed value
                            // (e.g. returned from `make_counter()`).
                            // Call it indirectly through the local.
                            Operand::Copy(Place::local(local))
                        } else {
                            Operand::Const(ConstValue::Str(segments[0].name.clone()))
                        }
                    } else {
                        Operand::Const(ConstValue::Str(segments[0].name.clone()))
                    }
                } else {
                    Operand::Const(ConstValue::Str(
                        segments
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join("::"),
                    ))
                }
            }
            _ => {
                let local = self.lower_expr(callee)?;
                Operand::Copy(Place::local(local))
            }
        };
        // Look up the callee's parameter types so we can apply
        // Fn-trait coercions per arg position. The call site of
        // `apply(f: Fn(i64) -> i64, ...)` with `f = bare_fn` needs
        // to wrap `bare_fn`'s code address into the env+code
        // shape; the call site of `apply(f, ...)` with `f` already
        // a closure (env-shaped) is a no-op.
        // Which parameters the callee only reads, and lets nothing outlive
        // the call through. A callee that is not a resolved name states
        // nothing, so none of its parameters is assumed read-only.
        let callee_param_shareable: Option<Vec<bool>> = match &callee.kind {
            HirExprKind::Path { def: Some(def), .. } => self.fn_param_shareable.get(def).cloned(),
            _ => None,
        };
        let callee_param_tys: Option<Vec<Ty>> = match &callee.kind {
            HirExprKind::Path { def: Some(def), .. } => self.fn_inputs.get(def).cloned(),
            _ => None,
        }
        // A callee that is not a resolved name - a closure, an `Fn`
        // parameter, a local holding a function value - states its
        // parameters in its own type. Without them every per-argument
        // coercion below is skipped, and a `&[T; N]` argument reaches a
        // `&[T]` parameter as a flat buffer the callee then reads as a
        // `GosVec` header.
        .or_else(|| match self.tcx.kind_of(callee.ty) {
            gossamer_types::TyKind::FnPtr(sig) | gossamer_types::TyKind::FnTrait(sig) => {
                Some(sig.inputs.clone())
            }
            gossamer_types::TyKind::Ref { inner, .. } => match self.tcx.kind_of(*inner) {
                gossamer_types::TyKind::FnPtr(sig) | gossamer_types::TyKind::FnTrait(sig) => {
                    Some(sig.inputs.clone())
                }
                _ => None,
            },
            _ => None,
        });
        // `__concat` / `__debug` carry every `println!` / `format!` argument,
        // and the bare print builtins take their values directly. A struct
        // argument with a derived `Type::fmt` is rendered to a String first so
        // the compiled tiers can format it (they cannot print an aggregate).
        // A program may declare its own `print` / `println`, which takes its
        // argument as written; only the prelude builtins (no user `def`
        // behind the name) render their arguments.
        // `__debug` is the `{:?}` channel and answers through `impl Debug`;
        // every other rendering callee is `{}` and answers through
        // `impl Display`.
        let renders_debug_channel_method = match &callee.kind {
            HirExprKind::Path { segments, .. }
                if segments.len() == 1 && segments[0].name.as_str() == "__debug" =>
            {
                "fmt"
            }
            _ => "to_string",
        };
        let callee_renders_aggregates = matches!(
            &callee.kind,
            HirExprKind::Path { segments, def, .. }
                if segments.len() == 1
                    && (matches!(segments[0].name.as_str(), "__concat" | "__debug")
                        || (def.is_none()
                            && matches!(
                                segments[0].name.as_str(),
                                "println" | "print" | "eprintln" | "eprint"
                            )))
        );
        let mut arg_operands = Vec::with_capacity(args.len());
        // `&mut <bare-local>` arguments of a writeback type (scalar / String)
        // lower to a by-slot-address `Rvalue::Ref`. After the call the callee
        // may have stored a new value through that address; reload the local
        // from `*ref` so the caller's binding sees it. This is mandatory on the
        // Cranelift tier - its locals are SSA Variables with no machine
        // address, so the Ref materialises a throwaway stack slot the callee
        // writes into; without the reload the round-trip is lost. On the LLVM
        // tier the local is alloca-backed and the reload re-reads the same
        // alloca (a harmless no-op). Recorded as (place_local, ref_local).
        let mut mut_ref_reloads: Vec<(Local, Local)> = Vec::new();
        for (idx, arg) in args.iter().enumerate() {
            // Detect an explicit `&mut <bare local>` of a writeback type;
            // the matching `Rvalue::Ref` emission lives in `lower_unary`.
            let reload_target = self.mut_ref_reload_target(arg);
            let local = self.lower_expr(arg)?;
            if let Some(place_local) = reload_target {
                mut_ref_reloads.push((place_local, local));
            }
            // 0.7.0 flag::Cell auto-deref at the user-fn call
            // boundary - matches the VM tier's behaviour so
            // `f(flags.output)` passes the unwrapped value.
            let local = self.auto_deref_cell(local, span);
            // Wrap when the source MIR local holds a raw code
            // address (named fn item, lifted closure name, or a
            // `let f = some_fn`). Capturing closures registered
            // in `local_closure` are env_ptr-shaped already and
            // skip this path.
            let in_closure_map = self.local_closure.contains_key(&local);
            let in_fn_name_map = self.local_fn_name.contains_key(&local);
            let local_ty = self.locals[local.0 as usize].ty;
            let local_kind_is_fn = matches!(
                self.tcx.kind_of(local_ty),
                gossamer_types::TyKind::FnDef { .. } | gossamer_types::TyKind::FnPtr(_)
            );
            let arg_is_fn_item = !in_closure_map
                && (in_fn_name_map
                    || local_kind_is_fn
                    || matches!(&arg.kind, HirExprKind::Path { def: Some(_), .. }));
            let local = if arg_is_fn_item {
                if let Some(params) = callee_param_tys.as_ref() {
                    if let Some(expected) = params.get(idx).copied() {
                        // Instantiated, because the thunk that adapts a bare
                        // fn item to the env-taking callable shape is named
                        // for its signature's register classes. A generic
                        // `Fn(T) -> U` parameter names the all-integer thunk,
                        // which hands an `f64` argument over in the integer
                        // file and reads the result back out of it; the call
                        // through the environment uses the instantiated
                        // signature, so the thunk has to as well.
                        let expected = self.instantiate_param_ty(callee.ty, expected);
                        self.coerce_to_fn_trait_if_needed(local, expected, span)
                    } else {
                        local
                    }
                } else {
                    local
                }
            } else {
                local
            };
            // Coerce flat Array { T, N } to a proper GosVec when the
            // callee's parameter expects Vec<T> / Slice<T>.  Without this
            // a literal like `[1,2,3]` bound as `Array{i64,3}` passes a
            // flat-buffer pointer to a function that calls gos_rt_vec_get
            // on it, reading element[0]=1 as the GosVec length and then
            // going out of bounds or segfaulting.
            let local = {
                use gossamer_types::TyKind;
                let local_ty = self.locals[local.0 as usize].ty;
                let expected_opt = callee_param_tys.as_ref().and_then(|p| p.get(idx).copied());
                // See through a `&` borrow on both sides. In the GC aliasing
                // model `&[T]` and `[T]` share a representation, so a `&[T]` /
                // `&Vec<T>` parameter is `Ref { Slice/Vec }`; and an inline
                // `[T; N]` array borrowed as `&[T]` (e.g. `f(&xs)` where
                // `let xs = [1,2,3]`) reaches here as `Ref { Array }`. Both
                // must still trigger the array→GosVec coercion, or the callee
                // reads the inline flat buffer as a GosVec header (garbage /
                // segfault). Without unwrapping the ref, the coercion fired
                // only for by-value `[T]`/`Vec` params and silently skipped
                // every `&[T]` parameter.
                let deref = |b: &Self, t: Ty| match b.tcx.kind_of(t) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => t,
                };
                let local_inner = deref(self, local_ty);
                if let TyKind::Array { elem, len } = self.tcx.kind_of(local_inner).clone() {
                    if let Some(expected) = expected_opt {
                        let expected_inner = deref(self, expected);
                        // A const generic array parameter (`[T; N]`) is carried
                        // by the callee as a runtime-length sequence, so a
                        // concrete-length array argument is coerced to a GosVec
                        // just like a `Vec<T>` / `[T]` parameter.
                        let expected_is_const_array = matches!(
                            self.tcx.kind_of(expected_inner),
                            TyKind::Array {
                                len: gossamer_types::ArrayLen::Param(_),
                                ..
                            }
                        );
                        if matches!(
                            self.tcx.kind_of(expected_inner),
                            TyKind::Vec(_) | TyKind::Slice(_)
                        ) || expected_is_const_array
                        {
                            // A `&[T]` parameter borrows: the caller's array
                            // outlives the call and reclaims its element
                            // children at its own drop. Build a non-owning
                            // view so the coerced slice never deep-frees
                            // those children. A by-value `Vec<T>` / `[T]`
                            // parameter takes ownership, so keep the owning
                            // copy.
                            let param_is_borrow =
                                matches!(self.tcx.kind_of(expected), TyKind::Ref { .. });
                            if param_is_borrow {
                                self.coerce_borrow_array_to_vec(local, elem, len, span)
                            } else {
                                self.coerce_array_to_vec(local, elem, len, span)
                            }
                        } else {
                            local
                        }
                    } else {
                        local
                    }
                } else {
                    local
                }
            };
            // Gossamer has value semantics for owned arguments, so the callee
            // gets independent growable storage - including Vec fields nested
            // in a by-value struct or tuple - whenever it could write the
            // parameter or let it outlive the call. A callee that provably
            // does neither reads the caller's storage instead, and the
            // argument crosses as the handle. A reference parameter keeps the
            // original storage either way and preserves write-through
            // behavior.
            let local = {
                use gossamer_types::TyKind;
                let source_ty = self.locals[local.0 as usize].ty;
                let expected_ty = callee_param_tys.as_ref().and_then(|p| p.get(idx).copied());
                let callee_shares_storage = callee_param_shareable
                    .as_ref()
                    .is_some_and(|flags| flags.get(idx).copied().unwrap_or(false));
                // A Gossamer callee's own frame swaps every value-container
                // field of a by-value aggregate parameter - a `Map`, `Set`,
                // `Deque`, or heap, none of which carries a reference count -
                // for a copy of its own on entry, which is the independent
                // storage a copy here would be for, so making one first copies
                // the same table twice per call. A `Vec` or slice field is the
                // exception the copy stays for: the callee's share of one is a
                // retain, so its `push` would reach the caller's vector.
                let callee_copies_its_aggregate = callee_param_shareable.is_some()
                    && expected_ty.is_some_and(|expected| {
                        let fields = crate::lower::aggregate_rc_field_paths(self.tcx, expected);
                        !fields.is_empty()
                            && fields
                                .iter()
                                .all(|(_, kind)| *kind != crate::lower::FieldRcKind::Vec)
                    });
                let owned_clone_parameter = !callee_shares_storage
                    && !callee_copies_its_aggregate
                    && expected_ty.is_some_and(|expected| {
                        matches!(
                            self.tcx.kind_of(expected),
                            TyKind::Vec(_)
                                | TyKind::Adt { .. }
                                | TyKind::Tuple(_)
                                | TyKind::Array { .. }
                                | TyKind::HashMap { .. }
                        )
                    });
                let caller_visible_place = matches!(
                    &arg.kind,
                    HirExprKind::Path { .. }
                        | HirExprKind::Field { .. }
                        | HirExprKind::TupleIndex { .. }
                        | HirExprKind::Index { .. }
                );
                if owned_clone_parameter
                    && caller_visible_place
                    && (matches!(
                        self.tcx.kind_of(source_ty),
                        TyKind::Vec(_)
                            | TyKind::Adt { .. }
                            | TyKind::Tuple(_)
                            | TyKind::Array { .. }
                            | TyKind::HashMap { .. }
                    ) || self.set_clone_symbol_for_local(local).is_some())
                {
                    let cloned = self.fresh(source_ty);
                    self.emit_owned_clone_binding(local, cloned, span);
                    cloned
                } else {
                    local
                }
            };
            // Render a struct / enum argument of a rendering callee through
            // the method its channel names (a String), so a `println!` of an
            // aggregate compiles on Cranelift / LLVM instead of bailing.
            let local = if callee_renders_aggregates {
                self.adt_fmt_rendered(local, renders_debug_channel_method, span)
            } else {
                local
            };
            let local = {
                let expected = callee_param_tys.as_ref().and_then(|p| p.get(idx).copied());
                self.match_reference_parameter(local, expected, span)
            };
            arg_operands.push(Operand::Copy(Place::local(local)));
        }
        // When an inline closure literal is the callee (e.g. `x |> |s| f(s)`),
        // `ty` may still be the FnPtr type of the closure rather than its
        // return type. Resolve the return type from the callee operand's sig.
        let ty = {
            use gossamer_types::TyKind;
            if matches!(
                self.tcx.kind_of(ty),
                TyKind::Var(_) | TyKind::Error | TyKind::FnPtr(_) | TyKind::FnTrait(_)
            ) {
                if let Operand::Copy(Place {
                    local: callee_local,
                    projection,
                }) = &callee_operand
                {
                    if projection.is_empty() {
                        let callee_local_ty = self.locals[callee_local.0 as usize].ty;
                        match self.tcx.kind_of(callee_local_ty) {
                            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                                let out = sig.output;
                                if matches!(self.tcx.kind_of(out), TyKind::Var(_) | TyKind::Error) {
                                    ty
                                } else {
                                    out
                                }
                            }
                            _ => ty,
                        }
                    } else {
                        ty
                    }
                } else {
                    ty
                }
            } else {
                ty
            }
        };
        let dest = self.fresh(ty);
        // Pre-register the destination's struct name so subsequent
        // `dest.field` projections resolve to a concrete struct
        // even when the type checker leaves the call's HIR type
        // partially elaborated.
        if let Some(sname) = self.struct_name_of(ty) {
            self.local_struct.insert(dest, sname);
        }
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: callee_operand,
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

    /// For a `&mut <bare local>` argument of a writeback type (scalar /
    /// `String`), returns the borrowed local - the destination of the
    /// post-call `place = *ref` reload. Mirrors the `Rvalue::Ref` emission
    /// gate in `lower_unary`: only `&mut` over a place-expr of a scalar or
    /// `String` operand takes a slot address. A bare path that *forwards* an
    /// existing `&mut` parameter is already a pointer and needs no reload, so
    /// only the explicit `&mut <local>` form qualifies.
    /// Hands an argument over in the shape the callee's declared parameter
    /// reads. A reference to a scalar is the address of a slot, so a `&T`
    /// parameter needs one even when the argument is a bare value; a shared
    /// `&String` is the string pointer itself, so a `&mut String` argument -
    /// which is the address of the caller's slot - is loaded first. Every
    /// other pairing already shares its representation and passes unchanged.
    fn match_reference_parameter(
        &mut self,
        local: Local,
        expected: Option<Ty>,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let Some(expected) = expected else {
            return local;
        };
        let TyKind::Ref {
            inner: expected_inner,
            mutability: expected_mut,
        } = self.tcx.kind_of(expected)
        else {
            return local;
        };
        let (expected_inner, expected_mut) = (*expected_inner, *expected_mut);
        let actual = self.locals[local.0 as usize].ty;
        let actual_ref = match self.tcx.kind_of(actual) {
            TyKind::Ref {
                inner, mutability, ..
            } => Some((*inner, *mutability)),
            _ => None,
        };
        if self.slot_addressed_pointee(expected_inner) {
            // Already a reference: forwarding one parameter into the next
            // hands over the same address.
            if actual_ref.is_some() {
                return local;
            }
            let dest = self.fresh(expected);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Ref {
                    mutable: expected_mut == gossamer_types::Mutbl::Mut,
                    place: Place::local(local),
                },
                span,
            );
            return dest;
        }
        let reborrowing_string = expected_mut == gossamer_types::Mutbl::Not
            && matches!(self.tcx.kind_of(expected_inner), TyKind::String)
            && matches!(actual_ref, Some((_, gossamer_types::Mutbl::Mut)));
        if reborrowing_string {
            return self.load_slot_value(local, expected_inner, span);
        }
        local
    }

    /// Lowers `Deque::new()` / `Queue::new()` / `Stack::new()` into a store
    /// typed for the element the call's own type names, plus the ownership
    /// tag that store needs. The element type is not recoverable from the
    /// handle the constructor answers, so both are settled here, where the
    /// call's type is still in hand.
    fn try_lower_slot_container_new(&mut self, joined: &str, ty: Ty, span: Span) -> Option<Local> {
        let (runtime_kind, ctor) = match joined {
            "Deque::new"
            | "VecDeque::new"
            | "collections::Deque::new"
            | "collections::VecDeque::new" => ("collections::VecDeque", "gos_rt_deque_new_typed"),
            "Queue::new"
            | "VecQueue::new"
            | "collections::Queue::new"
            | "collections::VecQueue::new" => ("collections::VecQueue", "gos_rt_deque_new_typed"),
            "Stack::new"
            | "VecStack::new"
            | "collections::Stack::new"
            | "collections::VecStack::new" => ("collections::VecStack", "gos_rt_deque_new_typed"),
            "MaxHeap::new"
            | "BinaryHeap::new"
            | "MaxBinaryHeap::new"
            | "collections::MaxHeap::new"
            | "collections::BinaryHeap::new"
            | "collections::MaxBinaryHeap::new" => {
                ("collections::MaxHeap", "gos_rt_bheap_new_typed")
            }
            "MinHeap::new"
            | "MinBinaryHeap::new"
            | "collections::MinHeap::new"
            | "collections::MinBinaryHeap::new" => {
                ("collections::MinHeap", "gos_rt_bheap_new_typed")
            }
            _ => return None,
        };
        // A heap of one-word scalars keeps the integer and float entry
        // points, whose store the untyped constructor already allocates.
        let heap = ctor == "gos_rt_bheap_new_typed";
        let elem = self.first_generic_of(ty)?;
        // A heap sifts through whole words - the scalar entry points read a
        // slot as an `i64` - so its store never takes a narrower stride.
        let slot_bytes = self.type_slot_bytes(elem).max(1);
        let elem_bytes = i128::from(if heap { slot_bytes.max(8) } else { slot_bytes });
        let kind = i128::from(self.vec_elem_kind_tag(elem));
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let const_arg = |builder: &mut Self, value: i128| {
            let local = builder.fresh(i64_ty);
            builder.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Int(value))),
                span,
            );
            Operand::Copy(Place::local(local))
        };
        let bytes_arg = const_arg(self, elem_bytes);
        let kind_arg = const_arg(self, kind);
        // The handle keeps the container's own type: it is one slot either
        // way, and the element type is what the format site reads to render
        // an element the integer shim cannot.
        let dest = self.fresh(ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(ctor.to_string())),
            args: vec![bytes_arg, kind_arg],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        self.local_runtime_kind.insert(dest, runtime_kind);
        if let Some(ownership) = crate::lower::helpers::elem_ownership(self.tcx, elem)
            && std::env::var_os("GOS_NO_ELEM_META").is_none()
        {
            // A heap's handle is its element store; a deque's addresses it.
            let store = if heap {
                dest
            } else {
                let store = self.fresh(i64_ty);
                let after_store = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_deque_vec".to_string())),
                    args: vec![Operand::Copy(Place::local(dest))],
                    destination: Place::local(store),
                    target: Some(after_store),
                });
                self.set_current(after_store);
                store
            };
            let mut args = vec![Operand::Copy(Place::local(store))];
            if let Some(meta) = ownership.meta() {
                args.push(Operand::Const(ConstValue::Str(meta.to_string())));
            }
            let unit_ty = self.tcx.unit();
            let sink = self.fresh(unit_ty);
            self.emit_assign(
                Place::local(sink),
                Rvalue::CallIntrinsic {
                    name: ownership.symbol(),
                    args,
                },
                span,
            );
        }
        Some(dest)
    }

    /// The `Vec` element-kind tag an element store of `elem` elements carries,
    /// naming what one slot owns: a counted string, container, or error
    /// handle, or nothing beyond the slot itself.
    pub(crate) fn vec_elem_kind_tag(&self, elem: Ty) -> u8 {
        match self.tcx.kind_of(elem) {
            TyKind::String => 1,
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Iterator(_) => 2,
            TyKind::HashMap { .. } => 3,
            TyKind::DynError => 4,
            // A `json::Value` element is a handle holding a share of the
            // document it views, so the vector's teardown gives each back.
            TyKind::JsonValue => 12,
            // A single-slot struct, tuple, or fixed array is held inline
            // and its slot address is the value; the store has to know,
            // since a one-word scalar element is the value itself.
            _ if self.tcx.is_flat_inline_aggregate(elem) => 10,
            _ => 0,
        }
    }

    pub(crate) fn mut_ref_reload_target(&self, arg: &HirExpr) -> Option<Local> {
        let HirExprKind::Unary {
            op: HirUnaryOp::RefMut,
            operand,
        } = &arg.kind
        else {
            return None;
        };
        let HirExprKind::Path { segments, .. } = &operand.kind else {
            return None;
        };
        let [seg] = segments.as_slice() else {
            return None;
        };
        let local = self.lookup_local(&seg.name)?;
        let writeback =
            self.mut_ref_takes_slot_address(operand.ty, self.locals[local.0 as usize].ty);
        writeback.then_some(local)
    }
}
