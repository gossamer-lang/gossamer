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
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

impl<'a> Builder<'a> {
    pub(crate) fn lower_struct_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        let last = segments.last()?;
        if last.name != "__struct" {
            return None;
        }
        let (name_expr, pairs) = args.split_first()?;
        let HirExprKind::Literal(HirLiteral::String(struct_name)) = &name_expr.kind else {
            return None;
        };
        if pairs.len() % 2 != 0 {
            return None;
        }
        // Try the free-struct table first. Then check whether the
        // name is a struct-payload enum variant (`Shape::Rect { w,
        // h }`); if so, route through `lower_user_enum_ctor` so
        // the runtime layout matches what the variant-pattern
        // match expects (`[p0, p1, …]` heap aggregate; the discriminant
        // lives in the RC header byte).
        // Mixing the flat-tuple lowering with the disc-prefixed
        // match was the root cause of the
        // control_flow / data_structures crash - the match read
        // disc from offset 0 of a value that didn't have one.
        let is_free_struct = self.structs.contains_key(struct_name);
        let mut variant_idx: Option<usize> = None;
        let mut variant_enum_name: Option<String> = None;
        if !is_free_struct {
            // The variant-fields map is keyed by the bare variant
            // name; look up its declaration index in the parent
            // enum so the constructor writes the right disc.
            let variant_ident = Ident::new(struct_name.clone());
            if let Some((enum_name, idx)) = self.enums.lookup(std::slice::from_ref(&variant_ident))
            {
                variant_idx = Some(idx);
                variant_enum_name = Some(enum_name);
            }
        }
        // `http::Response { … }` - the type is an opaque runtime handle
        // on the compiled tiers (a `GosHttpResponse` ptr), so the
        // generic aggregate lowering can't apply (it would emit an
        // undefined `__struct` call). Lower to the runtime constructor
        // + setter chain instead. "Response" is always present in
        // `self.structs` via `stdlib_struct_shapes`, so the stdlib
        // route applies unless the USER declared a `Response` struct
        // (visible as a non-sentinel entry in `struct_defs`).
        let user_defined_response = struct_name == "Response"
            && self
                .struct_defs
                .iter()
                .any(|(def, name)| name == "Response" && def.local < u32::MAX - 16);
        if variant_idx.is_none() && struct_name == "Response" && !user_defined_response {
            return self.lower_http_response_literal(pairs, span);
        }
        if variant_idx.is_none()
            && struct_name == "Reverse"
            && self.is_reverse_i64_ty(ty)
            && pairs.len() == 2
            && matches!(
                pairs[0].kind,
                HirExprKind::Literal(HirLiteral::String(ref field)) if field == "0"
            )
        {
            return self.lower_expr(&pairs[1]);
        }
        let order = self
            .structs
            .get(struct_name)
            .cloned()
            .or_else(|| self.enums.variant_fields.get(struct_name).cloned())?;
        let mut provided: HashMap<String, &HirExpr> = HashMap::new();
        let mut base_expr: Option<&HirExpr> = None;
        let mut chunks = pairs.chunks_exact(2);
        for chunk in chunks.by_ref() {
            let HirExprKind::Literal(HirLiteral::String(field_name)) = &chunk[0].kind else {
                return None;
            };
            // Functional-update sentinel: `..base` is encoded by HIR
            // as a `"__base"` key whose value is the base expression.
            // Stash it and fill in any unprovided fields by projecting
            // `base.field` below.
            if field_name == "__base" {
                base_expr = Some(&chunk[1]);
                continue;
            }
            provided.insert(field_name.clone(), &chunk[1]);
        }
        if let Some(idx) = variant_idx {
            // Enum struct-variant: build a `[w, h, …]` heap
            // aggregate. Args are HirExpr in field-declaration
            // order so the load offsets in
            // `lower_pattern_predicate`'s Struct arm line up.
            let arg_exprs: Vec<HirExpr> = order
                .iter()
                .filter_map(|f| provided.get(f.as_str()).map(|e| (*e).clone()))
                .collect();
            let enum_name = variant_enum_name.as_deref().unwrap_or("");
            let result = self.lower_user_enum_ctor(
                enum_name,
                u32::try_from(idx).unwrap_or(0),
                &arg_exprs,
                ty,
                span,
            );
            // Tag the struct-variant literal with its ENUM name (not the bare
            // variant) so `==` / `{:?}` route to `Enum::eq` / `Enum::fmt`.
            if let Some(local) = result {
                self.local_struct.insert(local, enum_name.to_string());
            }
            return result;
        }
        // Resolve the base expression once (when present) so missing
        // fields can be filled by projecting `base.field`. Lowering
        // the base into a fresh local also lets us register its
        // struct identity for nested-field projection lookups.
        let base_local: Option<Local> = if let Some(b) = base_expr {
            let local = self.lower_expr(b)?;
            self.local_struct
                .entry(local)
                .or_insert_with(|| struct_name.clone());
            Some(local)
        } else {
            None
        };
        // Declared field types (when the struct's `ty` carries a real
        // DefId) let us coerce a flat `[T; N]` array-literal value into
        // a heap `GosVec` when the field is declared `[T]` (Vec) /
        // `[T]`-slice. Without this, `Q { bytes: [1, 2, 3] }` stores the
        // 3-slot inline array straight into the 1-slot Vec field - the
        // struct alloca overflows and a later `q.bytes.len()` reads
        // element[0] as the Vec pointer (misaligned-deref crash).
        let field_tys: Option<Vec<Ty>> = match self.tcx.kind_of(ty) {
            gossamer_types::TyKind::Adt { def, substs } => {
                self.tcx.adt_field_tys(*def, substs).map(<[Ty]>::to_vec)
            }
            _ => None,
        };
        let mut operands = Vec::with_capacity(order.len());
        for (idx, field) in order.iter().enumerate() {
            if let Some(value_expr) = provided.get(field.as_str()) {
                let mut value_local = self.lower_expr(value_expr)?;
                // Coerce an array-literal value to a Vec when the field
                // is declared as a growable `[T]` / slice.
                if let Some(field_ty) = field_tys.as_ref().and_then(|t| t.get(idx)).copied() {
                    use gossamer_types::TyKind;
                    value_local = self.coerce_to_fn_trait_if_needed(value_local, field_ty, span);
                    let val_ty = self.locals[value_local.0 as usize].ty;
                    if let TyKind::Array { elem, len } = self.tcx.kind_of(val_ty).clone()
                        && matches!(
                            self.tcx.kind_of(field_ty),
                            TyKind::Vec(_) | TyKind::Slice(_)
                        )
                    {
                        value_local = self.coerce_array_to_vec(value_local, elem, len, span);
                    }
                }
                operands.push(Operand::Copy(Place::local(value_local)));
            } else if let Some(base) = base_local {
                let projection_idx = u32::try_from(idx).ok()?;
                let mut place = Place::local(base);
                place
                    .projection
                    .push(crate::ir::Projection::Field(projection_idx));
                // A field filled from `..base` shares the base's heap child;
                // the base still owns its copy and releases it at its own
                // drop, so the new struct takes its own share. A direct RC
                // field (String / Vec / weak / boxed enum) retains its one
                // pointer; a by-value sub-struct / tuple field shares each of
                // its NESTED RC pointers, so retain those in lockstep with the
                // recursive release in the drop pass - otherwise a nested
                // `String` is freed twice.
                if let Some(field_ty) = field_tys.as_ref().and_then(|t| t.get(idx)).copied() {
                    use gossamer_types::TyKind;
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    let is_vec_field = matches!(
                        self.tcx.kind_of(field_ty),
                        TyKind::Vec(_) | TyKind::Slice(_)
                    );
                    // A `Vec` / `[T]` field carries no RC header (it is a stack
                    // word pointing at a GosVec); a `..base` copy shares it, so
                    // retain through the vec count so the base's and the new
                    // struct's `gos_rt_vec_free` at their deaths each balance.
                    if is_vec_field || self.tcx.is_rc_managed(field_ty) {
                        let retain = if is_vec_field {
                            "gos_rt_vec_retain"
                        } else if self.tcx.is_weak_ty(field_ty) {
                            "gos_rt_rc_weak_retain"
                        } else {
                            "gos_rt_rc_retain"
                        };
                        let retain_dest = self.fresh(i64_ty);
                        self.emit_assign(
                            Place::local(retain_dest),
                            Rvalue::CallIntrinsic {
                                name: retain,
                                args: vec![Operand::Copy(place.clone())],
                            },
                            span,
                        );
                    } else {
                        for (sub, kind) in
                            crate::lower::aggregate_rc_field_paths(self.tcx, field_ty)
                        {
                            let mut nested = place.clone();
                            for s in &sub {
                                nested.projection.push(crate::ir::Projection::Field(*s));
                            }
                            let retain = match kind {
                                crate::lower::FieldRcKind::Vec => "gos_rt_vec_retain",
                                crate::lower::FieldRcKind::Weak => "gos_rt_rc_weak_retain",
                                crate::lower::FieldRcKind::Rc => "gos_rt_rc_retain",
                                crate::lower::FieldRcKind::Carrier { .. } => {
                                    "gos_rt_result_payload_retain"
                                }
                                // Takes the field's address and swaps in a
                                // clone: a container with no reference count
                                // cannot be co-owned.
                                other => match other.value_container_helpers() {
                                    Some((clone, _)) => clone,
                                    None => continue,
                                },
                            };
                            let mut args = vec![Operand::Copy(nested)];
                            if let crate::lower::FieldRcKind::Carrier { ok, err } = kind {
                                args.push(Operand::Const(ConstValue::Int(i128::from(ok))));
                                args.push(Operand::Const(ConstValue::Int(i128::from(err))));
                            }
                            let retain_dest = self.fresh(i64_ty);
                            self.emit_assign(
                                Place::local(retain_dest),
                                Rvalue::CallIntrinsic { name: retain, args },
                                span,
                            );
                        }
                    }
                }
                operands.push(Operand::Copy(place));
            } else {
                return None;
            }
        }
        let dest = self.fresh(ty);
        self.local_struct.insert(dest, struct_name.clone());
        // Make the guarded copy-blob meta for this struct type available
        // to the drop pass and the backend heap-copy site (idempotent;
        // no-op for types without guarded child slots).
        let _ = self.ensure_aggr_copy_meta(ty);
        // Adt requires a DefId we don't have handy at this layer.
        // The native codegen treats every aggregate as a flat i64-per
        // slot stack slot regardless of kind, so `Tuple` is a safe
        // structural stand-in until monomorphisation wires real DefIds
        // through.
        self.emit_assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands,
            },
            span,
        );
        Some(dest)
    }

    /// The type of field `index` of the struct `def` instantiated with
    /// `substs`: the declared type with the instantiation's type arguments
    /// substituted wherever it names them.
    pub(crate) fn instantiated_field_ty(
        &mut self,
        def: gossamer_resolve::DefId,
        substs: &gossamer_types::Substs,
        index: usize,
    ) -> Option<Ty> {
        let declared = self.tcx.struct_field_tys(def)?.get(index).copied()?;
        let subst_tys = crate::monomorph::subst_type_arguments(substs);
        Some(crate::monomorph::subst_param_ty(
            self.tcx, declared, &subst_tys,
        ))
    }

    pub(crate) fn lower_field_access(
        &mut self,
        receiver: &HirExpr,
        name: &Ident,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        // Try the place-expression path first: for `a.x`, `a[i].x`,
        // and other lvalue-shaped receivers this builds a direct
        // projected place read without materialising the intermediate
        // struct copy. That lets `a[i].x` lower to `copy a[i].x`
        // instead of `tmp = a[i]; tmp.x` (and the latter's
        // lost-struct-name fallback to the unsupported placeholder).
        if let Some(mut place) = self.lower_place_expr(receiver) {
            // Recover the runtime-handle kind from the local's *type* when
            // its construction-site tag was lost - e.g. an `http::Response`
            // returned from a user function (`let r = attach(r)`) or bound
            // through a `let`. Without this the `.headers` / `.body` field
            // projection falls through to a positional struct read against a
            // handle the codegen treats as an opaque pointer.
            let rk = self
                .local_runtime_kind
                .get(&place.local)
                .copied()
                .or_else(|| {
                    let lty = self.locals[place.local.0 as usize].ty;
                    let inner = match self.tcx.kind_of(lty) {
                        gossamer_types::TyKind::Ref { inner, .. } => *inner,
                        _ => lty,
                    };
                    self.struct_name_of(inner).and_then(|s| match s.as_str() {
                        "Response" => Some("http::Response"),
                        "Request" => Some("http::Request"),
                        _ => None,
                    })
                });
            if let Some(rk) = rk {
                let helper: Option<(&'static str, Ty)> = match (rk, name.name.as_str()) {
                    ("http::Response", "status") => Some((
                        "gos_rt_http_response_status",
                        self.tcx.int_ty(gossamer_types::IntTy::I64),
                    )),
                    ("http::Response", "body") => {
                        Some(("gos_rt_http_response_body", self.tcx.string_ty()))
                    }
                    ("http::Response", "raw_bytes") => {
                        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                        Some((
                            "gos_rt_http_response_raw_bytes",
                            self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                        ))
                    }
                    ("http::Response", "headers") => {
                        let s = self.tcx.string_ty();
                        let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                        Some((
                            "gos_rt_http_response_headers",
                            self.tcx.intern(gossamer_types::TyKind::Vec(tup)),
                        ))
                    }
                    ("http::Response", "content_type") => {
                        Some(("gos_rt_http_response_content_type", self.tcx.string_ty()))
                    }
                    ("http::Response", "location") => {
                        Some(("gos_rt_http_response_location", self.tcx.string_ty()))
                    }
                    ("http::Request", "method") => {
                        Some(("gos_rt_http_request_method", self.tcx.string_ty()))
                    }
                    ("http::Request", "path") => {
                        Some(("gos_rt_http_request_path", self.tcx.string_ty()))
                    }
                    ("http::Request", "query") => {
                        Some(("gos_rt_http_request_query", self.tcx.string_ty()))
                    }
                    ("http::Request", "peer_addr") => {
                        Some(("gos_rt_http_request_peer_addr", self.tcx.string_ty()))
                    }
                    ("http::Request", "context") => Some((
                        "gos_rt_http_request_context",
                        self.tcx.int_ty(gossamer_types::IntTy::I64),
                    )),
                    ("http::Request", "body") => {
                        Some(("gos_rt_http_request_body_str", self.tcx.string_ty()))
                    }
                    ("http::Request", "raw_body") => {
                        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                        Some((
                            "gos_rt_http_request_raw_body",
                            self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                        ))
                    }
                    ("http::Request", "headers") => {
                        let s = self.tcx.string_ty();
                        let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                        Some((
                            "gos_rt_http_request_headers",
                            self.tcx.intern(gossamer_types::TyKind::Vec(tup)),
                        ))
                    }
                    ("http::Request", "query_pairs") => {
                        let s = self.tcx.string_ty();
                        let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                        Some((
                            "gos_rt_http_request_query_pairs",
                            self.tcx.intern(gossamer_types::TyKind::Vec(tup)),
                        ))
                    }
                    ("errors::Error", "message") => {
                        Some(("gos_rt_error_message", self.tcx.string_ty()))
                    }
                    ("errors::Error", "cause") => Some((
                        "gos_rt_error_cause",
                        self.tcx.int_ty(gossamer_types::IntTy::I64),
                    )),
                    _ => None,
                };
                if let Some((rt_name, ret_ty)) = helper {
                    let dest = self.fresh(ret_ty);
                    // See the sibling table: the request's context is an
                    // opaque handle in an i64 slot, tagged rather than
                    // typed so the RC passes leave it alone.
                    if rt_name == "gos_rt_http_request_context" {
                        self.local_runtime_kind.insert(dest, "context::Context");
                    }
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(rt_name.to_string())),
                        args: vec![Operand::Copy(Place::local(place.local))],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
            }
            // The field belongs to the receiver's own type, which for a
            // projected receiver (`outer.inner.field`) is the leaf struct
            // reached by walking `place`'s projection from the root local's
            // type (`Inner`), not the root local's own struct (`Outer`).
            // Resolve it from the pinned MIR types so a stale HIR receiver
            // type - an inference variable, or the JsonValue default the
            // checker assigns an opaque nested field - cannot misroute the
            // read through `gos_rt_json_get`. The receiver-expression and
            // root-local tags remain as fallbacks for partial type info.
            let projected_leaf_ty = {
                let mut leaf = self.locals[place.local.0 as usize].ty;
                for proj in &place.projection {
                    let crate::ir::Projection::Field(fidx) = proj else {
                        continue;
                    };
                    let mut walk = leaf;
                    while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(walk) {
                        walk = *inner;
                    }
                    leaf = match self.tcx.kind_of(walk).clone() {
                        gossamer_types::TyKind::Adt { def, .. } => self
                            .tcx
                            .struct_field_tys(def)
                            .and_then(|tys| tys.get(*fidx as usize).copied())
                            .unwrap_or(leaf),
                        gossamer_types::TyKind::Tuple(elems) => {
                            elems.get(*fidx as usize).copied().unwrap_or(leaf)
                        }
                        _ => leaf,
                    };
                }
                leaf
            };
            let struct_name = self
                .struct_name_of(projected_leaf_ty)
                .or_else(|| self.struct_name_from_expr(receiver))
                .or_else(|| self.local_struct.get(&place.local).cloned());
            if let Some(sname) = struct_name {
                if let Some(order) = self.structs.get(&sname).cloned() {
                    if let Some(pos) = order.iter().position(|f| f == &name.name) {
                        let idx = u32::try_from(pos).ok()?;
                        // The HIR-recorded `ty` for a field projection
                        // can be an unresolved inference variable when
                        // the receiver's type only crystallised at MIR
                        // pinning time (e.g. `body_f.value` after
                        // `let body_f = body.to_fahrenheit()`). Fall
                        // through to the struct's declared field type
                        // - looked up via the receiver local's MIR
                        // `Adt` def - so downstream printing and
                        // temp-local typing see the real `f64` /
                        // `String` / etc. Without this, the lower
                        // tier alloca's the temp as `ptr` and stores
                        // an `f64` through it, producing invalid IR.
                        // Resolve the field's concrete type. The HIR
                        // `ty` is unreliable for generic-struct fields:
                        // it can be an unresolved inference variable, or
                        // the field's bound `Param(n)` (`third: C` on
                        // `Triple<A, B, C>`). In both cases fall through
                        // to the struct's declared field type looked up
                        // via the receiver's MIR `Adt` def, then
                        // substitute any `Param` against the receiver's
                        // concrete type arguments. Without this the field
                        // local defaults to i64/ptr and the `println!`
                        // arg dispatcher mis-stringifies an `f64` (prints
                        // its bit pattern) or strlen's a non-pointer.
                        // The struct's *declared* field type (looked up
                        // via the receiver local's MIR `Adt` def) is
                        // authoritative - the HIR-recorded `ty` is
                        // unreliable here: it can be an unresolved
                        // inference var, a bound `Param(n)`, OR a
                        // wrongly-resolved concrete type (e.g. a
                        // `match Ok(q) => q.bytes` binding where `q`'s
                        // struct type didn't fully propagate and the
                        // field came back as `String` instead of
                        // `[u8]`, sending `.len()` to strlen). Prefer
                        // the declared type whenever the receiver is an
                        // Adt / Tuple with a known field at `pos`,
                        // substituting any generic `Param` against the
                        // receiver's concrete type arguments.
                        // The receiver type is the root local's type with
                        // its projection applied - not the bare local type.
                        // For a nested receiver like `t.0` (the first element
                        // of a tuple), the field offset must be taken on the
                        // projected `P`, or `t.0.x` reads element-0's type
                        // (the whole `P`) instead of `x`'s `i64`.
                        let mut recv_local_ty = self.locals[place.local.0 as usize].ty;
                        for proj in &place.projection {
                            if let crate::ir::Projection::Field(fidx) = proj {
                                let mut w = recv_local_ty;
                                while let gossamer_types::TyKind::Ref { inner, .. } =
                                    self.tcx.kind_of(w)
                                {
                                    w = *inner;
                                }
                                recv_local_ty = match self.tcx.kind_of(w).clone() {
                                    gossamer_types::TyKind::Adt { def, substs } => self
                                        .instantiated_field_ty(def, &substs, *fidx as usize)
                                        .unwrap_or(recv_local_ty),
                                    gossamer_types::TyKind::Tuple(elems) => {
                                        elems.get(*fidx as usize).copied().unwrap_or(recv_local_ty)
                                    }
                                    _ => recv_local_ty,
                                };
                            }
                        }
                        let mut walk = recv_local_ty;
                        while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(walk)
                        {
                            walk = *inner;
                        }
                        let pinned_ty = match self.tcx.kind_of(walk).clone() {
                            // The declared field type with the receiver's type
                            // arguments substituted wherever it names them, at
                            // any depth (`items: Vec<T>` on `Wrap<i64>`).
                            gossamer_types::TyKind::Adt { def, substs } => {
                                self.instantiated_field_ty(def, &substs, pos).unwrap_or(ty)
                            }
                            gossamer_types::TyKind::Tuple(elems) => {
                                elems.get(pos).copied().unwrap_or(ty)
                            }
                            _ => ty,
                        };
                        // A struct is address-is-value: its slot holds the field
                        // words inline, so a `Field(idx)` projection walks them,
                        // even when the receiver type is an unresolved inference
                        // var - the positional path's whole reason to exist.
                        let dest = self.fresh(pinned_ty);
                        place.projection.push(crate::ir::Projection::Field(idx));
                        self.emit_assign(
                            Place::local(dest),
                            Rvalue::Use(Operand::Copy(place)),
                            span,
                        );
                        return Some(dest);
                    }
                }
            }
        }

        // Fallback: recurse into the receiver and use its local's
        // recorded struct name (the original path, kept for cases
        // where the receiver is an expression rather than a place
        // - e.g. a call that returns a struct).
        let receiver_local = self.lower_expr(receiver)?;

        // `flags.<long>` for the synthesised `flag::define(...)`
        // aggregate. The per-local layout table records each spec's
        // field index + cell kind, so dispatch lowers to a plain
        // `Field(idx)` projection plus a `flag::Cell::*` runtime tag
        // that the deref `*` later picks up.
        if let Some(field_local) = self.lookup_define_field(receiver_local, &name.name, span) {
            return Some(field_local);
        }

        // A struct recorded through a `local_struct` tag: resolve the field
        // name to a positional `Field(idx)` against the registered shape BEFORE
        // the JsonValue fallback fires - otherwise a typechecker-opaque ADT
        // routes through `gos_rt_json_get`.
        if let Some(struct_name) = self.local_struct.get(&receiver_local).cloned() {
            if let Some(order) = self.structs.get(&struct_name).cloned() {
                if let Some(idx) = order.iter().position(|f| f == name.name.as_str()) {
                    let dest = self.fresh(ty);
                    self.emit_assign(
                        Place::local(dest),
                        Rvalue::Use(Operand::Copy(Place {
                            local: receiver_local,
                            projection: vec![crate::ir::Projection::Field(
                                u32::try_from(idx).unwrap_or(0),
                            )],
                        })),
                        span,
                    );
                    return Some(dest);
                }
            }
        }

        // `value.field` on a `json::Value` receiver - rewrite to a
        // runtime `gos_rt_json_get(value, "field")` call. The
        // result is itself a `json::Value` that downstream code
        // chains further field access / cast through.
        if self.is_json_value_ty(receiver.ty)
            || self.is_json_value_ty(self.locals[receiver_local.0 as usize].ty)
        {
            return Some(self.emit_json_get(receiver_local, &name.name, span));
        }

        // Runtime-kind-aware field access: stdlib types
        // (`http::Response`, `errors::Error`, …) expose
        // `.status`, `.body`, `.message`, `.cause` as
        // field-style access in source even though they're
        // runtime-helper calls under the hood.
        let runtime_kind = self
            .receiver_local_from_path(receiver)
            .and_then(|l| self.local_runtime_kind.get(&l).copied())
            .or_else(|| self.local_runtime_kind.get(&receiver_local).copied());
        if let Some(rk) = runtime_kind {
            let helper: Option<(&'static str, Ty)> = match (rk, name.name.as_str()) {
                ("http::Response", "status") => Some((
                    "gos_rt_http_response_status",
                    self.tcx.int_ty(gossamer_types::IntTy::I64),
                )),
                ("http::Response", "body") => {
                    Some(("gos_rt_http_response_body", self.tcx.string_ty()))
                }
                ("http::Response", "raw_bytes") => {
                    let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                    Some((
                        "gos_rt_http_response_raw_bytes",
                        self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                    ))
                }
                ("http::Response", "headers") => {
                    let s = self.tcx.string_ty();
                    let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                    Some((
                        "gos_rt_http_response_headers",
                        self.tcx.intern(gossamer_types::TyKind::Vec(tup)),
                    ))
                }
                ("http::Response", "content_type") => {
                    Some(("gos_rt_http_response_content_type", self.tcx.string_ty()))
                }
                ("http::Response", "location") => {
                    Some(("gos_rt_http_response_location", self.tcx.string_ty()))
                }
                ("http::Request", "method") => {
                    Some(("gos_rt_http_request_method", self.tcx.string_ty()))
                }
                ("http::Request", "path") => {
                    Some(("gos_rt_http_request_path", self.tcx.string_ty()))
                }
                ("http::Request", "query") => {
                    Some(("gos_rt_http_request_query", self.tcx.string_ty()))
                }
                ("http::Request", "peer_addr") => {
                    Some(("gos_rt_http_request_peer_addr", self.tcx.string_ty()))
                }
                ("http::Request", "context") => Some((
                    "gos_rt_http_request_context",
                    self.tcx.int_ty(gossamer_types::IntTy::I64),
                )),
                ("http::Request", "body") => {
                    Some(("gos_rt_http_request_body_str", self.tcx.string_ty()))
                }
                ("http::Request", "raw_body") => {
                    let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                    Some((
                        "gos_rt_http_request_raw_body",
                        self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                    ))
                }
                ("http::Request", "headers") => {
                    let s = self.tcx.string_ty();
                    let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                    Some((
                        "gos_rt_http_request_headers",
                        self.tcx.intern(gossamer_types::TyKind::Vec(tup)),
                    ))
                }
                ("errors::Error", "message") => {
                    Some(("gos_rt_error_message", self.tcx.string_ty()))
                }
                ("errors::Error", "cause") => Some((
                    "gos_rt_error_cause",
                    self.tcx.int_ty(gossamer_types::IntTy::I64),
                )),
                _ => None,
            };
            if let Some((rt_name, ret_ty)) = helper {
                let dest = self.fresh(ret_ty);
                // The request's context is an opaque handle in an i64
                // slot, so it is tagged rather than typed: the RC passes
                // must not treat it as an owned aggregate, and its
                // methods still need an identity to dispatch on.
                if rt_name == "gos_rt_http_request_context" {
                    self.local_runtime_kind.insert(dest, "context::Context");
                }
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(rt_name.to_string())),
                    args: vec![Operand::Copy(Place::local(receiver_local))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
        }

        let struct_name = self
            .local_struct
            .get(&receiver_local)
            .cloned()
            .or_else(|| self.struct_name_of(receiver.ty));
        if name.name == "0"
            && (self.is_reverse_i64_ty(receiver.ty)
                || self.is_reverse_i64_ty(self.locals[receiver_local.0 as usize].ty))
        {
            let dest = self.fresh(ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(receiver_local))),
                span,
            );
            return Some(dest);
        }
        let field_order = struct_name
            .as_ref()
            .and_then(|n| self.structs.get(n))
            .cloned();
        let Some(order) = field_order else {
            // Only real json::Value carriers use dynamic field get.
            // Other unresolved receivers indicate a checker/lowering
            // leak and must not silently turn into JSON null.
            if matches!(
                self.tcx.kind_of(receiver.ty),
                gossamer_types::TyKind::JsonValue
            ) || matches!(
                self.tcx.kind_of(self.locals[receiver_local.0 as usize].ty),
                gossamer_types::TyKind::JsonValue
            ) {
                return Some(self.emit_json_get(receiver_local, &name.name, span));
            }
            return None;
        };
        if let Some(sname) = struct_name {
            // Tag the receiver so subsequent field accesses
            // and method calls hit the same fallback.
            self.local_struct.insert(receiver_local, sname);
        }
        let idx = order
            .iter()
            .position(|f| f == &name.name)
            .map(|i| u32::try_from(i).expect("field index fits u32"));
        // The type-checker rejects accesses to unknown field
        // names, so this lookup must succeed for any program that
        // reaches MIR. If a future refactor relaxes that check,
        // decline lowering instead of reading a bogus field.
        let idx = idx?;
        let dest = self.fresh(ty);
        let place = Place {
            local: receiver_local,
            projection: vec![crate::ir::Projection::Field(idx)],
        };
        self.emit_assign(Place::local(dest), Rvalue::Use(Operand::Copy(place)), span);
        Some(dest)
    }

    /// Emits a single runtime-helper call and returns its destination local.
    fn emit_rt_call_local(
        &mut self,
        rt_name: &str,
        args: Vec<Operand>,
        ret_ty: Ty,
        span: Span,
    ) -> Local {
        let dest = self.fresh(ret_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(rt_name.to_string())),
            args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Lowers an `http::Response { status, body, content_type, headers }`
    /// struct literal to the runtime constructor + setter chain the
    /// compiled tiers understand:
    ///
    /// `gos_rt_http_response_text_new(status, body)` →
    /// `gos_rt_http_response_set_body_bytes` (byte-array bodies) →
    /// `gos_rt_http_response_set_content_type` (explicit content_type) →
    /// `gos_rt_http_response_with_header` per header pair (unrolled for
    /// literal arrays, a MIR-level loop for dynamic ones).
    ///
    /// Field subsets mirror the interp's `value_to_response`: every
    /// field is optional - status defaults to 200, body to empty,
    /// content_type to text/plain (via `text_new`), headers to none.
    /// Unknown fields are evaluated and discarded, and a functional
    ///-update `..base` fills omitted fields by reading them back off
    /// the base response through the accessor shims.
    pub(crate) fn lower_http_response_literal(
        &mut self,
        pairs: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let unit_ty = self.tcx.unit();

        let mut status_expr: Option<&HirExpr> = None;
        let mut body_expr: Option<&HirExpr> = None;
        let mut ct_expr: Option<&HirExpr> = None;
        let mut headers_expr: Option<&HirExpr> = None;
        let mut base_expr: Option<&HirExpr> = None;
        for chunk in pairs.chunks_exact(2) {
            let HirExprKind::Literal(HirLiteral::String(field)) = &chunk[0].kind else {
                return None;
            };
            match field.as_str() {
                "status" => status_expr = Some(&chunk[1]),
                "body" => body_expr = Some(&chunk[1]),
                "content_type" => ct_expr = Some(&chunk[1]),
                "headers" => headers_expr = Some(&chunk[1]),
                "__base" => base_expr = Some(&chunk[1]),
                // The interp evaluates then ignores unknown fields.
                _ => {
                    let _ = self.lower_expr(&chunk[1]);
                }
            }
        }

        let base_local: Option<Local> = match base_expr {
            Some(b) => Some(self.lower_expr(b)?),
            None => None,
        };

        let status_local = if let Some(e) = status_expr {
            self.lower_expr(e)?
        } else if let Some(base) = base_local {
            self.emit_rt_call_local(
                "gos_rt_http_response_status",
                vec![Operand::Copy(Place::local(base))],
                i64_ty,
                span,
            )
        } else {
            let l = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(l),
                Rvalue::Use(Operand::Const(ConstValue::Int(200))),
                span,
            );
            l
        };

        // String bodies feed `text_new` directly; byte-array bodies
        // construct with an empty string and route through
        // `set_body_bytes` (the interp's `value_to_response` accepts
        // both shapes).
        let mut body_bytes_local: Option<Local> = None;
        let body_str_local = if let Some(e) = body_expr {
            let v = self.lower_expr(e)?;
            let vt = self.locals[v.0 as usize].ty;
            match self.tcx.kind_of(vt).clone() {
                TyKind::Array { elem, len } if matches!(self.tcx.kind_of(elem), TyKind::Int(_)) => {
                    body_bytes_local = Some(self.coerce_array_to_vec(v, elem, len, span));
                    self.empty_string_local(string_ty, span)
                }
                TyKind::Vec(elem) | TyKind::Slice(elem)
                    if matches!(self.tcx.kind_of(elem), TyKind::Int(_)) =>
                {
                    body_bytes_local = Some(v);
                    self.empty_string_local(string_ty, span)
                }
                _ => v,
            }
        } else if let Some(base) = base_local {
            self.emit_rt_call_local(
                "gos_rt_http_response_body",
                vec![Operand::Copy(Place::local(base))],
                string_ty,
                span,
            )
        } else {
            self.empty_string_local(string_ty, span)
        };

        let resp = self.emit_rt_call_local(
            "gos_rt_http_response_text_new",
            vec![
                Operand::Copy(Place::local(status_local)),
                Operand::Copy(Place::local(body_str_local)),
            ],
            i64_ty,
            span,
        );
        self.local_runtime_kind.insert(resp, "http::Response");

        if let Some(bytes) = body_bytes_local {
            let _ = self.emit_rt_call_local(
                "gos_rt_http_response_set_body_bytes",
                vec![
                    Operand::Copy(Place::local(resp)),
                    Operand::Copy(Place::local(bytes)),
                ],
                unit_ty,
                span,
            );
        }

        let ct_local: Option<Local> = if let Some(e) = ct_expr {
            Some(self.lower_expr(e)?)
        } else {
            // Without a base the `text_new` constructor already
            // records the text/plain default - matching the interp's
            // no-content_type behavior.
            base_local.map(|base| {
                self.emit_rt_call_local(
                    "gos_rt_http_response_content_type",
                    vec![Operand::Copy(Place::local(base))],
                    string_ty,
                    span,
                )
            })
        };
        if let Some(ct) = ct_local {
            let _ = self.emit_rt_call_local(
                "gos_rt_http_response_set_content_type",
                vec![
                    Operand::Copy(Place::local(resp)),
                    Operand::Copy(Place::local(ct)),
                ],
                unit_ty,
                span,
            );
        }

        if let Some(e) = headers_expr {
            // Peel a leading `&` so `headers: &pairs` iterates the
            // underlying array/vec.
            let e = match &e.kind {
                HirExprKind::Unary {
                    op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
                    operand,
                    ..
                } => operand.as_ref(),
                _ => e,
            };
            if let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) = &e.kind {
                // Literal array - unroll one `with_header` per pair.
                for elem in elems {
                    let (k, v) = if let HirExprKind::Tuple(items) = &elem.kind {
                        if items.len() != 2 {
                            continue;
                        }
                        (self.lower_expr(&items[0])?, self.lower_expr(&items[1])?)
                    } else {
                        let t = self.lower_expr(elem)?;
                        let k = self.fresh(string_ty);
                        self.emit_assign(
                            Place::local(k),
                            Rvalue::Use(Operand::Copy(Place {
                                local: t,
                                projection: vec![crate::ir::Projection::Field(0)],
                            })),
                            span,
                        );
                        let v = self.fresh(string_ty);
                        self.emit_assign(
                            Place::local(v),
                            Rvalue::Use(Operand::Copy(Place {
                                local: t,
                                projection: vec![crate::ir::Projection::Field(1)],
                            })),
                            span,
                        );
                        (k, v)
                    };
                    let _ = self.emit_rt_call_local(
                        "gos_rt_http_response_with_header",
                        vec![
                            Operand::Copy(Place::local(resp)),
                            Operand::Copy(Place::local(k)),
                            Operand::Copy(Place::local(v)),
                        ],
                        i64_ty,
                        span,
                    );
                }
            } else {
                // Dynamic header list - loop over the runtime vec.
                let hv = self.lower_expr(e)?;
                let hv_ty = self.locals[hv.0 as usize].ty;
                let hv = if let TyKind::Array { elem, len } = self.tcx.kind_of(hv_ty).clone() {
                    self.coerce_array_to_vec(hv, elem, len, span)
                } else {
                    hv
                };
                self.emit_response_header_copy_loop(resp, hv, span);
            }
        } else if let Some(base) = base_local {
            let s = string_ty;
            let tup = self.tcx.intern(TyKind::Tuple(vec![s, s]));
            let vec_ty = self.tcx.intern(TyKind::Vec(tup));
            let hv = self.emit_rt_call_local(
                "gos_rt_http_response_headers",
                vec![Operand::Copy(Place::local(base))],
                vec_ty,
                span,
            );
            self.emit_response_header_copy_loop(resp, hv, span);
        }

        Some(resp)
    }

    /// Materialises an empty-string constant local.
    fn empty_string_local(&mut self, string_ty: Ty, span: Span) -> Local {
        let l = self.fresh(string_ty);
        self.emit_assign(
            Place::local(l),
            Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
            span,
        );
        l
    }

    /// Emits a counter loop over a `Vec<(String, String)>` of header
    /// pairs, attaching each to `resp` via
    /// `gos_rt_http_response_with_header`. Same element-access recipe
    /// as `lower_for_vec`'s tuple destructure: `gos_rt_vec_get_ptr`
    /// for the 16-byte slot, `gos_load` at word offsets 0 / 8 for the
    /// name / value c-strings (borrowed - `with_header` copies).
    fn emit_response_header_copy_loop(&mut self, resp: Local, headers_vec: Local, span: Span) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let bool_ty = self.tcx.bool_ty();

        let len_local = self.emit_rt_call_local(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(headers_vec))],
            i64_ty,
            span,
        );
        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );

        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        let slot_ptr = self.emit_rt_call_local(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(headers_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            i64_ty,
            span,
        );
        let field_local = |b: &mut Self, offset: i64| -> Local {
            let off = b.fresh(i64_ty);
            b.emit_assign(
                Place::local(off),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(offset)))),
                span,
            );
            b.emit_rt_call_local(
                "gos_load",
                vec![
                    Operand::Copy(Place::local(slot_ptr)),
                    Operand::Copy(Place::local(off)),
                ],
                string_ty,
                span,
            )
        };
        let k = field_local(self, 0);
        let v = field_local(self, 8);
        let _ = self.emit_rt_call_local(
            "gos_rt_http_response_with_header",
            vec![
                Operand::Copy(Place::local(resp)),
                Operand::Copy(Place::local(k)),
                Operand::Copy(Place::local(v)),
            ],
            i64_ty,
            span,
        );
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
    }
}
