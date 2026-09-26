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
    pub(crate) fn lower_external_binding_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        if segments.is_empty() {
            return None;
        }
        let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let (module_path, item_name, item) = self.resolve_external_binding(&names, args.len())?;

        let mangled_module = module_path.replace("::", "__");
        let mangled = format!("gos_binding_{mangled_module}__{item_name}");

        // `Bytes`, `Map`, and a tuple arrive as a wire pointer the
        // runtime converts after the call, so the call's own
        // destination is that pointer rather than the value's type.
        let ret_ty = if Self::binding_ret_needs_wire(&item.ret) {
            self.tcx.int_ty(gossamer_types::IntTy::I64)
        } else {
            self.binding_type_to_mir(&item.ret)
        };
        let mut arg_locals: Vec<Local> = Vec::with_capacity(args.len());
        let mut callbacks: Vec<Local> = Vec::new();
        for (idx, arg) in args.iter().enumerate() {
            let raw = self.lower_expr(arg)?;
            let param_ty = item.params.get(idx);
            if matches!(param_ty, Some(gossamer_resolve::BindingType::Callback(..))) {
                let handle = self.register_binding_callback(raw, arg.ty, span);
                callbacks.push(handle);
                arg_locals.push(handle);
                continue;
            }
            let coerced = self.coerce_arg_for_binding(raw, param_ty, span);
            arg_locals.push(coerced);
        }
        let dest = self.fresh(ret_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(mangled)),
            args: arg_locals
                .into_iter()
                .map(|l| Operand::Copy(Place::local(l)))
                .collect(),
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        // A callback is valid for the binding call that received it.
        let unit_ty = self.tcx.unit();
        for handle in callbacks {
            let _ = self.emit_runtime_call(
                "gos_rt_binding_callback_release",
                vec![Operand::Copy(Place::local(handle))],
                unit_ty,
                span,
            );
        }
        // Result / Option returns arrive as a binding-ABI
        // `*mut GosVariant`; convert to the runtime's packed i128
        // result (string payloads become runtime strings) and type
        // the converted local as the real Result / Option Adt so
        // match / `?` / fmt downstream see the standard shape.
        let converted = self.convert_binding_variant_return(dest, &item.ret, span);
        if let Some(local) = converted {
            return Some(local);
        }
        Some(self.convert_binding_wire_return(dest, &item.ret, span))
    }

    /// Registers the closure in `closure` as a callback for one binding call
    /// and answers the handle the binding receives. The registration records
    /// the register class of each parameter and of the return, from the
    /// closure's type, so the runtime can call the compiled code.
    fn register_binding_callback(&mut self, closure: Local, closure_ty: Ty, span: Span) -> Local {
        use gossamer_types::TyKind;
        let class = |tcx: &TyCtxt, ty: Ty| -> char {
            match tcx.kind_of(ty) {
                TyKind::Float(_) => 'f',
                TyKind::Bool => 'b',
                TyKind::Char => 'c',
                TyKind::String => 's',
                TyKind::Unit | TyKind::Never => 'u',
                _ => 'i',
            }
        };
        let mut ty = closure_ty;
        if !matches!(self.tcx.kind_of(ty), TyKind::FnPtr(_) | TyKind::FnTrait(_)) {
            ty = self.locals[closure.0 as usize].ty;
        }
        let sig = match self.tcx.kind_of(ty).clone() {
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => Some(sig),
            _ => None,
        };
        // A named function arrives as its name; the runtime calls an
        // environment, so it is wrapped in one as any callable sink wraps it.
        let closure = match &sig {
            Some(sig) => {
                let callable = self.tcx.intern(TyKind::FnTrait(sig.clone()));
                self.coerce_to_fn_trait_if_needed(closure, callable, span)
            }
            None => closure,
        };
        let signature = match sig {
            Some(sig) => {
                let mut text: String = sig.inputs.iter().map(|t| class(self.tcx, *t)).collect();
                text.push('>');
                text.push(class(self.tcx, sig.output));
                text
            }
            // No signature to call through: the runtime refuses the
            // registration, and the binding's call through it reports that.
            _ => String::new(),
        };
        let string_ty = self.tcx.string_ty();
        let signature_local = self.fresh(string_ty);
        self.emit_assign(
            Place::local(signature_local),
            Rvalue::Use(Operand::Const(ConstValue::Str(signature))),
            span,
        );
        let handle_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        self.emit_runtime_call(
            "gos_rt_binding_callback_register",
            vec![
                Operand::Copy(Place::local(closure)),
                Operand::Copy(Place::local(signature_local)),
            ],
            handle_ty,
            span,
        )
    }

    /// Whether a binding return crosses as a wire pointer the runtime
    /// converts into the value's own shape.
    fn binding_ret_needs_wire(ret: &gossamer_resolve::BindingType) -> bool {
        use gossamer_resolve::BindingType as B;
        match ret {
            B::Bytes | B::Map(_, _) | B::Opaque(_) | B::Variant(_) => true,
            B::Tuple(elems) => Self::packed_wire_tags(elems).is_some(),
            _ => false,
        }
    }

    /// The Gossamer enum a binding's declared arm set names: the one whose
    /// variants spell exactly those arms. A binding's table is an ABI input;
    /// the type the program matches on is an ordinary enum it declared.
    /// `None` when no declared enum matches, which leaves the value open.
    fn enum_for_binding_arms(
        &mut self,
        arms: &[gossamer_resolve::BindingVariantArm],
    ) -> Option<(gossamer_resolve::DefId, Vec<String>)> {
        let wanted: std::collections::BTreeSet<&str> =
            arms.iter().map(|arm| arm.name.as_str()).collect();
        let candidates = self.tcx.enums_with_variant_names();
        let found = candidates.into_iter().find(|(_, names)| {
            names.len() == wanted.len() && names.iter().all(|name| wanted.contains(name.as_str()))
        })?;
        Some((found.0, found.1.to_vec()))
    }

    /// The Gossamer struct a binding's `Opaque(name)` shape names, with
    /// its field types in declaration order.
    fn binding_struct_shape(
        &mut self,
        name: &str,
    ) -> Option<(gossamer_types::Ty, Vec<gossamer_types::Ty>)> {
        let def = *self
            .struct_defs
            .iter()
            .find_map(|(def, declared)| (declared == name).then_some(def))?;
        let ty = self.tcx.intern(gossamer_types::TyKind::Adt {
            def,
            substs: gossamer_types::Substs::from_types([]),
        });
        let fields = self.inline_field_tys(ty)?;
        Some((ty, fields))
    }

    /// One wire tag per slot-shaped field, packed a byte apiece.
    fn packed_slot_tags(&self, fields: &[gossamer_types::Ty]) -> Option<i128> {
        if fields.is_empty() || fields.len() > 8 {
            return None;
        }
        let mut packed: i128 = 0;
        for (index, ty) in fields.iter().enumerate() {
            packed |= i128::from(self.slot_wire_tag(*ty)?) << (index * 8);
        }
        Some(packed)
    }

    /// The binding ABI's field tag for a slot-shaped type, or `None`
    /// when the type does not fit one tagged word.
    fn slot_wire_tag(&self, ty: gossamer_types::Ty) -> Option<u8> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Int(_) => return Some(0),
                TyKind::Float(_) => return Some(1),
                TyKind::Bool => return Some(2),
                TyKind::Char => return Some(3),
                TyKind::String => return Some(4),
                _ => return None,
            }
        }
    }

    /// One wire tag per element, packed a byte apiece, or `None` when
    /// an element is not a shape the tuple wire carries in one slot.
    fn packed_wire_tags(elems: &[gossamer_resolve::BindingType]) -> Option<i128> {
        use gossamer_resolve::BindingType as B;
        if elems.is_empty() || elems.len() > 8 {
            return None;
        }
        let mut packed: i128 = 0;
        for (index, elem) in elems.iter().enumerate() {
            let tag: i128 = match elem {
                B::I64 | B::Unit => 0,
                B::F64 => 1,
                B::Bool => 2,
                B::Char => 3,
                B::String => 4,
                _ => return None,
            };
            packed |= tag << (index * 8);
        }
        Some(packed)
    }

    /// The map-side kind selector the runtime converters read: `1` for a
    /// `String`, `0` for every kind it stores as one word.
    fn binding_map_kind(t: &gossamer_resolve::BindingType) -> i128 {
        i128::from(matches!(t, gossamer_resolve::BindingType::String))
    }

    /// Converts a `Bytes`, `Map`, or tuple return from its wire pointer
    /// into the runtime value the rest of the body reads, or hands back
    /// `raw` unchanged for every other return shape.
    fn convert_binding_wire_return(
        &mut self,
        raw: Local,
        ret: &gossamer_resolve::BindingType,
        span: Span,
    ) -> Local {
        use gossamer_resolve::BindingType as B;
        use gossamer_types::TyKind;
        match ret {
            B::Bytes => {
                // `Bytes` is `[u8]` at the source level, so the converted
                // value carries the slice type the caller wrote - the same
                // runtime object a `Vec` carries, under the spelling the
                // declared type is written in.
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let slice_ty = self.tcx.intern(TyKind::Slice(i64_ty));
                self.emit_runtime_call(
                    "gos_rt_binding_bytes_to_vec",
                    vec![Operand::Copy(Place::local(raw))],
                    slice_ty,
                    span,
                )
            }
            B::Map(key, value) => {
                let key_ty = self.binding_type_to_mir(key);
                let value_ty = self.binding_type_to_mir(value);
                let map_ty = self.tcx.intern(TyKind::HashMap {
                    key: key_ty,
                    value: value_ty,
                    ordered: false,
                });
                let key_kind = self.const_i64_local(Self::binding_map_kind(key), span);
                let value_kind = self.const_i64_local(Self::binding_map_kind(value), span);
                self.emit_runtime_call(
                    "gos_rt_binding_map_to_map",
                    vec![
                        Operand::Copy(Place::local(raw)),
                        Operand::Copy(Place::local(key_kind)),
                        Operand::Copy(Place::local(value_kind)),
                    ],
                    map_ty,
                    span,
                )
            }
            // The open dynamic value: the wire variant carries the whole
            // value, so the caller reads the value rather than the pointer
            // that reached it.
            B::Variant(arms) if arms.is_empty() => {
                let dyn_ty = self.tcx.intern(TyKind::DynValue);
                self.emit_runtime_call(
                    "gos_rt_dyn_from_binding_variant",
                    vec![Operand::Copy(Place::local(raw))],
                    dyn_ty,
                    span,
                )
            }
            // A declared arm set names a Gossamer enum, so the value the
            // caller reads is that enum: the wire's runtime arm name selects
            // the discriminant and the payload fills the variant's fields,
            // and every tier matches on it the same way.
            B::Variant(arms) if !arms.is_empty() => self
                .convert_binding_arms_to_enum(raw, arms, span)
                .unwrap_or(raw),
            B::Opaque(name) => {
                let Some((struct_ty, field_tys)) = self.binding_struct_shape(name) else {
                    return raw;
                };
                let Some(tags) = self.packed_slot_tags(&field_tys) else {
                    return raw;
                };
                let dest = self.fresh(struct_ty);
                let ptr_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let slots = self.fresh(ptr_ty);
                self.emit_assign(
                    Place::local(slots),
                    Rvalue::Ref {
                        mutable: true,
                        place: Place::local(dest),
                    },
                    span,
                );
                let count = self.const_i64_local(field_tys.len() as i128, span);
                let tags_local = self.const_i64_local(tags, span);
                let unit_ty = self.tcx.unit();
                let _ = self.emit_runtime_call(
                    "gos_rt_binding_struct_to_slots",
                    vec![
                        Operand::Copy(Place::local(raw)),
                        Operand::Copy(Place::local(slots)),
                        Operand::Copy(Place::local(count)),
                        Operand::Copy(Place::local(tags_local)),
                    ],
                    unit_ty,
                    span,
                );
                dest
            }
            B::Tuple(elems) => {
                let Some(tags) = Self::packed_wire_tags(elems) else {
                    return raw;
                };
                let elem_tys: Vec<gossamer_types::Ty> =
                    elems.iter().map(|e| self.binding_type_to_mir(e)).collect();
                let tuple_ty = self.tcx.intern(TyKind::Tuple(elem_tys));
                let dest = self.fresh(tuple_ty);
                let ptr_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let slots = self.fresh(ptr_ty);
                self.emit_assign(
                    Place::local(slots),
                    Rvalue::Ref {
                        mutable: true,
                        place: Place::local(dest),
                    },
                    span,
                );
                let count = self.const_i64_local(elems.len() as i128, span);
                let tags_local = self.const_i64_local(tags, span);
                let unit_ty = self.tcx.unit();
                let _ = self.emit_runtime_call(
                    "gos_rt_binding_tuple_to_slots",
                    vec![
                        Operand::Copy(Place::local(raw)),
                        Operand::Copy(Place::local(slots)),
                        Operand::Copy(Place::local(count)),
                        Operand::Copy(Place::local(tags_local)),
                    ],
                    unit_ty,
                    span,
                );
                dest
            }
            _ => raw,
        }
    }

    /// Lowers a `DynValue::<name>(..)` constructor to the runtime helper that
    /// builds that shape. `None` when the path is not one.
    pub(crate) fn lower_dyn_value_ctor(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let ["DynValue", ctor] = names.as_slice() else {
            return None;
        };
        let symbol = match (*ctor, args.len()) {
            ("nil", 0) => "gos_rt_dyn_nil",
            ("bool", 1) => "gos_rt_dyn_bool",
            ("int", 1) => "gos_rt_dyn_int",
            ("float", 1) => "gos_rt_dyn_float",
            ("char", 1) => "gos_rt_dyn_char",
            ("string", 1) => "gos_rt_dyn_string",
            ("bytes", 1) => "gos_rt_dyn_bytes",
            ("list", 1) => "gos_rt_dyn_list",
            ("map", 2) => "gos_rt_dyn_map",
            ("tagged", 2) => "gos_rt_dyn_tagged",
            _ => return None,
        };
        let mut arg_operands = Vec::with_capacity(args.len());
        for arg in args {
            let local = self.lower_expr(arg)?;
            arg_operands.push(Operand::Copy(Place::local(local)));
        }
        let dyn_ty = self.tcx.intern(gossamer_types::TyKind::DynValue);
        Some(self.emit_runtime_call(symbol, arg_operands, dyn_ty, span))
    }

    /// Builds the Gossamer enum a binding's declared arm set names from the
    /// wire value the call returned.
    fn convert_binding_arms_to_enum(
        &mut self,
        raw: Local,
        arms: &[gossamer_resolve::BindingVariantArm],
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let (def, names) = self.enum_for_binding_arms(arms)?;
        let variant_tys = self.tcx.enum_variant_tys(def)?.to_vec();
        if variant_tys.len() != names.len() {
            return None;
        }
        let enum_name = self.tcx.def_name(def)?.to_string();
        let enum_ty = self.tcx.intern(TyKind::Adt {
            def,
            substs: gossamer_types::Substs::from_types([]),
        });
        let dyn_ty = self.tcx.intern(TyKind::DynValue);
        let value = self.emit_runtime_call(
            "gos_rt_dyn_from_binding_variant",
            vec![Operand::Copy(Place::local(raw))],
            dyn_ty,
            span,
        );
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let names_local = {
            let string_ty = self.tcx.string_ty();
            let local = self.fresh(string_ty);
            self.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Str(names.join("|")))),
                span,
            );
            local
        };
        let index = self.emit_runtime_call(
            "gos_rt_dyn_arm_index",
            vec![
                Operand::Copy(Place::local(value)),
                Operand::Copy(Place::local(names_local)),
            ],
            i64_ty,
            span,
        );
        let dest = self.fresh(enum_ty);
        let join = self.new_block(span);
        // An arm the declared set does not carry is a broken binding
        // contract, not a value: the default block says so rather than
        // building some other variant's shape.
        let fallback = self.new_block(span);
        let mut arms_blocks: Vec<(i128, BlockId)> = Vec::new();
        let mut bodies: Vec<(BlockId, u32, Vec<gossamer_types::Ty>)> = Vec::new();
        for (variant, fields) in variant_tys.iter().enumerate() {
            let block = self.new_block(span);
            arms_blocks.push((variant as i128, block));
            bodies.push((block, variant as u32, fields.clone()));
        }
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(index)),
            arms: arms_blocks
                .iter()
                .map(|(value, block)| (*value, *block))
                .collect(),
            default: fallback,
        });
        for (block, variant, fields) in bodies {
            self.set_current(block);
            let mut payload = Vec::with_capacity(fields.len());
            for (position, field_ty) in fields.iter().enumerate() {
                payload.push(self.emit_binding_arm_field(value, position, *field_ty, span));
            }
            // The same construction a written `Enum::Arm(..)` lowers to, so
            // the node the caller matches on is the enum's own.
            let built = self
                .lower_user_enum_ctor_from_locals(&enum_name, variant, &payload, enum_ty, span)?;
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(built))),
                span,
            );
            self.terminate(Terminator::Goto { target: join });
        }
        self.set_current(fallback);
        let string_ty = self.tcx.string_ty();
        let message = self.fresh(string_ty);
        self.emit_assign(
            Place::local(message),
            Rvalue::Use(Operand::Const(ConstValue::Str(format!(
                "binding returned an arm outside its declared set ({})",
                names.join("|")
            )))),
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_runtime_call(
            "gos_rt_panic",
            vec![Operand::Copy(Place::local(message))],
            unit_ty,
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
        Some(dest)
    }

    /// Reads one payload field of a wire arm as the type the enum's variant
    /// declares for it.
    fn emit_binding_arm_field(
        &mut self,
        value: Local,
        position: usize,
        field_ty: gossamer_types::Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let position_local = self.const_i64_local(position as i128, span);
        let symbol = match self.tcx.kind_of(field_ty) {
            TyKind::Float(_) => "gos_rt_dyn_field_f64",
            TyKind::String => "gos_rt_dyn_field_str",
            TyKind::DynValue => "gos_rt_dyn_field_dyn",
            _ => "gos_rt_dyn_field_i64",
        };
        self.emit_runtime_call(
            symbol,
            vec![
                Operand::Copy(Place::local(value)),
                Operand::Copy(Place::local(position_local)),
            ],
            field_ty,
            span,
        )
    }

    /// Binds `value` into a fresh `i64` local.
    fn const_i64_local(&mut self, value: i128, span: Span) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(local),
            Rvalue::Use(Operand::Const(ConstValue::Int(value))),
            span,
        );
        local
    }

    /// Calls a runtime helper by name and binds its result.
    fn emit_runtime_call(
        &mut self,
        name: &str,
        args: Vec<Operand>,
        ret_ty: gossamer_types::Ty,
        span: Span,
    ) -> Local {
        let dest = self.fresh(ret_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name.to_string())),
            args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Emits the `gos_rt_binding_variant_to_result` conversion for a
    /// binding call whose declared return is `Result` / `Option`.
    /// Returns `None` for every other return shape.
    fn convert_binding_variant_return(
        &mut self,
        raw: Local,
        ret: &gossamer_resolve::BindingType,
        span: Span,
    ) -> Option<Local> {
        use gossamer_resolve::BindingType as B;
        use gossamer_types::TyKind;
        let adt_ty = match ret {
            B::Result(ok, err) => {
                let ok_ty = self.binding_type_to_mir(ok);
                let err_ty = self.binding_type_to_mir(err);
                let substs = gossamer_types::Substs::from_types([ok_ty, err_ty]);
                self.tcx.intern(TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                })
            }
            B::Option(inner) => {
                let inner_ty = self.binding_type_to_mir(inner);
                let substs = gossamer_types::Substs::from_types([inner_ty]);
                self.tcx.intern(TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                })
            }
            _ => return None,
        };
        let dest = self.fresh(adt_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(
                "gos_rt_binding_variant_to_result".to_string(),
            )),
            args: vec![Operand::Copy(Place::local(raw))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn coerce_arg_for_binding(
        &mut self,
        raw: Local,
        param_ty: Option<&gossamer_resolve::BindingType>,
        span: Span,
    ) -> Local {
        use gossamer_resolve::BindingType as B;
        use gossamer_types::TyKind;
        match param_ty {
            Some(B::Bytes) => {
                let ptr_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                return self.emit_runtime_call(
                    "gos_rt_binding_bytes_from_vec",
                    vec![Operand::Copy(Place::local(raw))],
                    ptr_ty,
                    span,
                );
            }
            Some(B::Map(key, value)) => {
                let key_kind = self.const_i64_local(Self::binding_map_kind(key), span);
                let value_kind = self.const_i64_local(Self::binding_map_kind(value), span);
                let ptr_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                return self.emit_runtime_call(
                    "gos_rt_binding_map_from_map",
                    vec![
                        Operand::Copy(Place::local(raw)),
                        Operand::Copy(Place::local(key_kind)),
                        Operand::Copy(Place::local(value_kind)),
                    ],
                    ptr_ty,
                    span,
                );
            }
            Some(B::Opaque(_)) => {
                let raw_ty = self.locals[raw.0 as usize].ty;
                let Some(name) = self.struct_name_of(raw_ty) else {
                    return raw;
                };
                let Some(fields) = self.inline_field_tys(raw_ty) else {
                    return raw;
                };
                let Some(tags) = self.packed_slot_tags(&fields) else {
                    return raw;
                };
                let ptr_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let string_ty = self.tcx.string_ty();
                let name_local = self.fresh(string_ty);
                self.emit_assign(
                    Place::local(name_local),
                    Rvalue::Use(Operand::Const(ConstValue::Str(name))),
                    span,
                );
                let slots = self.fresh(ptr_ty);
                self.emit_assign(
                    Place::local(slots),
                    Rvalue::Ref {
                        mutable: false,
                        place: Place::local(raw),
                    },
                    span,
                );
                let count_local = self.const_i64_local(fields.len() as i128, span);
                let tags_local = self.const_i64_local(tags, span);
                return self.emit_runtime_call(
                    "gos_rt_binding_struct_from_slots",
                    vec![
                        Operand::Copy(Place::local(name_local)),
                        Operand::Copy(Place::local(slots)),
                        Operand::Copy(Place::local(count_local)),
                        Operand::Copy(Place::local(tags_local)),
                    ],
                    ptr_ty,
                    span,
                );
            }
            Some(B::Tuple(elems)) => {
                let Some(tags) = Self::packed_wire_tags(elems) else {
                    return raw;
                };
                let count = elems.len();
                let ptr_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let slots = self.fresh(ptr_ty);
                self.emit_assign(
                    Place::local(slots),
                    Rvalue::Ref {
                        mutable: false,
                        place: Place::local(raw),
                    },
                    span,
                );
                let count_local = self.const_i64_local(count as i128, span);
                let tags_local = self.const_i64_local(tags, span);
                return self.emit_runtime_call(
                    "gos_rt_binding_tuple_from_slots",
                    vec![
                        Operand::Copy(Place::local(slots)),
                        Operand::Copy(Place::local(count_local)),
                        Operand::Copy(Place::local(tags_local)),
                    ],
                    ptr_ty,
                    span,
                );
            }
            _ => {}
        }
        let Some(B::Vec(_)) = param_ty else {
            return raw;
        };
        let raw_ty = self.locals[raw.0 as usize].ty;
        let TyKind::Array { elem, len } = self.tcx.kind_of(raw_ty) else {
            return raw;
        };
        let elem_ty = *elem;
        let len_val = len.to_usize();
        let elem_bytes = self.elem_bytes_of(elem_ty);
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let elem_bytes_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(elem_bytes)))),
            span,
        );
        let len_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(len_val as i128))),
            span,
        );
        let vec_local = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_from_arr".to_string())),
            args: vec![
                Operand::Copy(Place::local(elem_bytes_local)),
                Operand::Copy(Place::local(raw)),
                Operand::Copy(Place::local(len_local)),
            ],
            destination: Place::local(vec_local),
            target: Some(next),
        });
        self.set_current(next);
        vec_local
    }

    pub(crate) fn coerce_array_to_vec(
        &mut self,
        raw: Local,
        elem_ty: Ty,
        len: gossamer_types::ArrayLen,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let len = len.to_usize();
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);

        // Nested array: `Array{Array{T,N},M}` → `Vec<Vec<T>>`.
        // Each inner flat array must become a heap GosVec pointer so that
        // `gos_rt_vec_get_i64` on the outer Vec returns a valid *mut GosVec.
        if let TyKind::Array {
            elem: inner_elem,
            len: inner_len,
        } = self.tcx.kind_of(elem_ty).clone()
        {
            let inner_len = inner_len.to_usize();
            let inner_elem_bytes = self.elem_bytes_of(inner_elem);
            let inner_elem_bytes_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(inner_elem_bytes_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                    inner_elem_bytes,
                )))),
                span,
            );
            let inner_len_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(inner_len_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(inner_len as i128))),
                span,
            );
            let outer_len_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(outer_len_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(len as i128))),
                span,
            );
            let inner_vec_ty = self.tcx.intern(TyKind::Vec(inner_elem));
            let outer_vec_ty = self.tcx.intern(TyKind::Vec(inner_vec_ty));
            let dest = self.fresh(outer_vec_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_nested_arr_to_vec".to_string())),
                args: vec![
                    Operand::Copy(Place::local(inner_elem_bytes_local)),
                    Operand::Copy(Place::local(inner_len_local)),
                    Operand::Copy(Place::local(raw)),
                    Operand::Copy(Place::local(outer_len_local)),
                ],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return dest;
        }

        let elem_bytes = self.elem_bytes_of(elem_ty);
        let elem_bytes_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(elem_bytes)))),
            span,
        );
        let len_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(len as i128))),
            span,
        );
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let dest = self.fresh(vec_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_from_arr".to_string())),
            args: vec![
                Operand::Copy(Place::local(elem_bytes_local)),
                Operand::Copy(Place::local(raw)),
                Operand::Copy(Place::local(len_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        if let Some(name) = self.local_elem_struct.get(&raw).cloned() {
            self.local_elem_struct.insert(dest, name);
        }
        dest
    }

    /// Converts only the outer fixed array to a Vec. Nested fixed-array
    /// elements retain their inline `[T; N]` layout.
    pub(crate) fn fixed_array_to_vec(
        &mut self,
        raw: Local,
        elem_ty: Ty,
        len: gossamer_types::ArrayLen,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let elem_bytes_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                self.elem_bytes_of(elem_ty),
            )))),
            span,
        );
        let len_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(len.to_usize() as i128))),
            span,
        );
        let vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(elem_ty));
        let dest = self.fresh(vec_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_from_arr".to_string())),
            args: vec![
                Operand::Copy(Place::local(elem_bytes_local)),
                Operand::Copy(Place::local(raw)),
                Operand::Copy(Place::local(len_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Coerces a borrowed array (`&[T; N]` reaching a `&[T]` / `&Vec<T>`
    /// parameter) into a borrowing GosVec view. Identical buffer to
    /// `coerce_array_to_vec`, but built via `gos_rt_vec_borrow_arr` so the
    /// drop pass leaves it non-owning: a borrow must not free the element
    /// children the source array still owns. Nested-array borrows fall back
    /// to the owning conversion (exotic; the inner vecs need real headers).
    pub(crate) fn coerce_borrow_array_to_vec(
        &mut self,
        raw: Local,
        elem_ty: Ty,
        len: gossamer_types::ArrayLen,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        if matches!(self.tcx.kind_of(elem_ty), TyKind::Array { .. }) {
            return self.coerce_array_to_vec(raw, elem_ty, len, span);
        }
        let len = len.to_usize();
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let elem_bytes = self.elem_bytes_of(elem_ty);
        let elem_bytes_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(elem_bytes)))),
            span,
        );
        let len_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(len as i128))),
            span,
        );
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let dest = self.fresh(vec_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_borrow_arr".to_string())),
            args: vec![
                Operand::Copy(Place::local(elem_bytes_local)),
                Operand::Copy(Place::local(raw)),
                Operand::Copy(Place::local(len_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    pub(crate) fn resolve_external_binding(
        &self,
        names: &[&str],
        argc: usize,
    ) -> Option<(String, String, gossamer_resolve::ExternalItem)> {
        if names.len() >= 2 {
            let qualified = names.join("::");
            if let Some(item) = gossamer_resolve::lookup_external_item(&qualified) {
                let (module_path, item_name) = qualified.rsplit_once("::")?;
                return Some((module_path.to_string(), item_name.to_string(), item));
            }
            // Module-prefixed lookup: try the leaf segment against
            // every module whose path ends in the leading segment.
            // E.g. `echo::shout` matches the `echo` module.
            let leading = names[0];
            let leaf = *names.last()?;
            for m in gossamer_resolve::all_external_modules() {
                let path_segs: Vec<&str> = m.path.split("::").collect();
                if path_segs.last().copied() == Some(leading)
                    && let Some(item) = m.items.iter().find(|i| i.name == leaf)
                {
                    return Some((m.path.clone(), item.name.clone(), item.clone()));
                }
            }
            return None;
        }
        // Bare-leaf lookup: walk every module's items looking for
        // the unique candidate matching arity.
        let leaf = names[0];
        let mut matches: Vec<(String, gossamer_resolve::ExternalItem)> = Vec::new();
        for m in gossamer_resolve::all_external_modules() {
            for item in &m.items {
                if item.name == leaf && item.params.len() == argc {
                    matches.push((m.path.clone(), item.clone()));
                }
            }
        }
        if matches.len() == 1 {
            let (module_path, item) = matches.pop()?;
            return Some((module_path, item.name.clone(), item));
        }
        None
    }
}
