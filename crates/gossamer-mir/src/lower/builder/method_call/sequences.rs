//! Sequence combinators and rendering a value through Display.

use super::*;

impl<'a> Builder<'a> {
    /// `xs.map(f)` / `xs.filter(f)` / `xs.sum()` / … - the method form
    /// of the `iter::` combinators on a sequence receiver. Routes
    /// through `try_lower_iter_call` with the receiver threading in as
    /// the data-last argument, so both surfaces share one lowering.
    /// Non-sequence receivers pass through: `Result::map`,
    /// `Option::map`, `HashMap` accessors, and the String surface keep
    /// their own dispatch.
    pub(super) fn lower_seq_combinator_method(
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
            ("pairwise", 0) => Some("iter::pairwise"),
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
            "take" | "skip" | "step_by" | "collect" | "rev"
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
    pub(super) fn free_form_method_path(
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

    pub(super) fn lower_tuple_get_method(
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
    /// Whether the sequence `ty` names (through any references) holds floats.
    pub(super) fn sequence_elem_is_float(&self, ty: Ty) -> bool {
        let mut ty = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                matches!(self.tcx.kind_of(*elem), TyKind::Float(_))
            }
            _ => false,
        }
    }

    pub(super) fn vec_join_symbol(&self, receiver_ty: Ty) -> Option<&'static str> {
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
            TyKind::Float(gossamer_types::FloatTy::F32) => Some("gos_rt_vec_join_f32"),
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
    pub(super) fn element_fmt_symbol(&mut self, elem_ty: Ty, method: &str) -> Option<String> {
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
    pub(super) fn receiver_is_enum(&self, receiver_ty: Ty) -> bool {
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
    pub(super) fn read_display_element(
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
    /// points at. `errors::Error` renders its colon-joined chain through
    /// `gos_rt_error_display`, which `{}` already reaches, and a
    /// `bytes::Buffer` renders its bytes as text through its own
    /// `to_string`.
    pub(super) fn renders_through_own_shim(
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
        matches!(kind, Some("errors::Error" | "bytes::Buffer"))
    }

    /// Whether the receiver is a set, whose local carries the bare handle
    /// word rather than the set's own type. The width tells the renderer
    /// nothing, so the display path is what reaches the set formatter -
    /// the word's own `to_string` would answer its decimal.
    pub(super) fn receiver_is_set_handle(&self, receiver: &HirExpr) -> bool {
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

    pub(super) fn display_to_string_receiver(&mut self, receiver_ty: Ty) -> bool {
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
                | TyKind::JsonValue
        )
    }

    /// `x.to_string()` as the one-piece concatenation `format!("{}", x)`
    /// lowers to, so every element type renders through one formatter.
    pub(super) fn lower_display_to_string(
        &mut self,
        receiver: &HirExpr,
        span: Span,
    ) -> Option<Local> {
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
    pub(super) fn lower_display_join(
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
    pub(super) fn shared_elem_ty(&self, ty: Ty) -> Option<Ty> {
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
    pub(super) fn ty_is_shared_handle(&self, ty: Ty) -> bool {
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(
            self.tcx.kind_of(cur),
            TyKind::Adt { def, .. } if self.tcx.def_name(*def) == Some("sync::Shared")
        )
    }

    pub(super) fn seq_search_shim_fits(&self, elem: Ty) -> bool {
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
    pub(super) fn seq_elem_ty(&self, receiver_ty: Ty) -> Option<Ty> {
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
    pub(super) fn seq_elem_is_inline_aggregate(&self, elem_ty: Ty) -> bool {
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
    pub(super) fn lower_seq_eq_scan(
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
    pub(super) fn seq_str_method_from_lowered(
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
            "keys" if matches!(kind, TyKind::HashMap { .. }) => {
                Some(if self.map_keys_unsigned(ty) {
                    "gos_rt_map_keys_vec_u64"
                } else {
                    "gos_rt_map_keys_vec"
                })
            }
            "values" if matches!(kind, TyKind::HashMap { .. }) && self.map_value_is_carrier(ty) => {
                Some(if self.map_keys_unsigned(ty) {
                    "gos_rt_map_values_carrier_u64"
                } else {
                    "gos_rt_map_values_carrier"
                })
            }
            "values" if matches!(kind, TyKind::HashMap { .. }) => {
                Some(if self.map_keys_unsigned(ty) {
                    "gos_rt_map_values_vec_u64"
                } else {
                    "gos_rt_map_values_vec"
                })
            }
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
