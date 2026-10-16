//! Construction, matching, and binding of enums stored by value.
//!
//! Such an enum is a struct of its discriminant and the regions its variants'
//! fields occupy (see [`gossamer_types::FlatEnumLayout`]), so every read and
//! write is a field projection of that struct, and copies and drops follow
//! the struct's own ownership walk.

use gossamer_hir::{HirPat, HirPatKind};
use gossamer_lex::Span;
use gossamer_types::{FlatEnumLayout, Ty, TyKind};

use crate::ir::{
    AggregateKind, BinOp, ConstValue, Local, Operand, Place, Projection, Rvalue, Terminator,
};

use super::Builder;

/// One sub-pattern of a variant pattern over a by-value enum.
pub(crate) struct FlatFieldPat<'p> {
    /// The field's position in its variant.
    pub(crate) position: usize,
    /// The pattern the field is matched against, or `None` for a shorthand
    /// binding of `name`.
    pub(crate) pattern: Option<&'p HirPat>,
    /// The name a shorthand binding introduces.
    pub(crate) name: Option<&'p str>,
}

impl Builder<'_> {
    /// The place holding the by-value enum `local` names - the local itself,
    /// or what it refers to - with the enum's type, or `None` when `local`
    /// holds no such enum.
    pub(crate) fn flat_enum_place(&self, local: Local) -> Option<(Place, Ty, bool)> {
        let ty = self.locals[local.0 as usize].ty;
        if self.tcx.is_flat_enum_ty(ty) {
            return Some((Place::local(local), ty, false));
        }
        let TyKind::Ref { inner, mutability } = self.tcx.kind_of(ty) else {
            return None;
        };
        let (inner, mutable) = (*inner, *mutability == gossamer_types::Mutbl::Mut);
        self.tcx.is_flat_enum_ty(inner).then(|| {
            let place = Place {
                local,
                projection: vec![Projection::Deref],
            };
            (place, inner, mutable)
        })
    }

    fn flat_layout(&self, ty: Ty) -> Option<FlatEnumLayout> {
        self.tcx.flat_enum_layout(ty).cloned()
    }

    /// Builds variant `variant` of the by-value enum `ty` from `payload`, its
    /// fields in declaration order.
    pub(crate) fn lower_flat_enum_ctor(
        &mut self,
        variant: u32,
        payload: &[Local],
        field_tys: &[Ty],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let layout = self.flat_layout(ty)?;
        let regions = layout.variant_regions.get(variant as usize)?.clone();
        let declared = layout.variant_field_tys.get(variant as usize)?.clone();
        let mut filled: Vec<Option<Local>> = vec![None; layout.regions.len()];
        for (i, value) in payload.iter().enumerate() {
            let region = *regions.get(i)? as usize;
            let local = self.coerce_enum_payload(*value, field_tys.get(i).copied(), span);
            let field_ty = declared.get(i).copied()?;
            filled[region] =
                Some(self.field_into_storage(local, field_ty, layout.regions[region], span));
        }
        let mut operands = Vec::with_capacity(layout.regions.len() + 1);
        operands.push(Operand::Const(ConstValue::Int(i128::from(variant))));
        for (region, value) in filled.into_iter().enumerate() {
            operands.push(match value {
                Some(local) => Operand::Copy(Place::local(local)),
                None => self.zero_operand(layout.regions[region], span),
            });
        }
        let dest = self.fresh(ty);
        let _ = self.ensure_aggr_copy_meta(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: AggregateKind::Tuple,
                operands,
            },
            span,
        );
        Some(dest)
    }

    /// An operand holding the all-zero value of `ty`: what a region the
    /// current variant does not use holds, so its share-counted words are
    /// null and a copy or a drop of the enum passes over them.
    fn zero_operand(&mut self, ty: Ty, span: Span) -> Operand {
        if crate::lower::carrier_ref::is_two_word_carrier(self.tcx, ty) {
            let zero = self.fresh(ty);
            self.emit_assign(
                Place::local(zero),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_new",
                    args: vec![
                        Operand::Const(ConstValue::Int(0)),
                        Operand::Const(ConstValue::Int(0)),
                    ],
                },
                span,
            );
            return Operand::Copy(Place::local(zero));
        }
        let fields: Option<Vec<Ty>> = match self.tcx.kind_of(ty) {
            TyKind::Tuple(elems) => Some(elems.clone()),
            TyKind::Array { elem, len } => Some(vec![*elem; len.to_usize()]),
            TyKind::Adt { def, .. } if def.local < u32::MAX - 16 => {
                self.tcx.struct_field_tys(*def).map(<[Ty]>::to_vec)
            }
            _ => None,
        };
        let Some(fields) = fields else {
            return Operand::Const(ConstValue::Int(0));
        };
        let kind = if matches!(self.tcx.kind_of(ty), TyKind::Array { .. }) {
            AggregateKind::Array
        } else {
            AggregateKind::Tuple
        };
        let operands = fields.iter().map(|f| self.zero_operand(*f, span)).collect();
        let zero = self.fresh(ty);
        self.emit_assign(
            Place::local(zero),
            Rvalue::Aggregate { kind, operands },
            span,
        );
        Operand::Copy(Place::local(zero))
    }

    /// `value`, a field of type `field_ty`, as the region of type `storage`
    /// that holds it: itself, or a one-word scalar widened into the shared
    /// word.
    fn field_into_storage(&mut self, value: Local, field_ty: Ty, storage: Ty, span: Span) -> Local {
        if field_ty == storage {
            return value;
        }
        let word = self.fresh(storage);
        let rvalue = if crate::lower::flat_enums::is_unit_only_enum(self.tcx, field_ty) {
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![Operand::Copy(Place::local(value))],
            }
        } else {
            Rvalue::Cast {
                operand: Operand::Copy(Place::local(value)),
                target: storage,
            }
        };
        self.emit_assign(Place::local(word), rvalue, span);
        word
    }

    /// A field of type `field_ty` read back out of the shared word `word`.
    fn field_out_of_storage(&mut self, word: Local, field_ty: Ty, span: Span) -> Local {
        let value = self.fresh(field_ty);
        let rvalue = if crate::lower::flat_enums::is_unit_only_enum(self.tcx, field_ty) {
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Copy(Place::local(word)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            }
        } else if matches!(self.tcx.kind_of(field_ty), TyKind::Bool) {
            Rvalue::BinaryOp {
                op: BinOp::Ne,
                lhs: Operand::Copy(Place::local(word)),
                rhs: Operand::Const(ConstValue::Int(0)),
            }
        } else {
            Rvalue::Cast {
                operand: Operand::Copy(Place::local(word)),
                target: field_ty,
            }
        };
        self.emit_assign(Place::local(value), rvalue, span);
        value
    }

    /// The discriminant of the by-value enum `scrutinee` names.
    pub(crate) fn flat_enum_disc(&mut self, scrutinee: Local, span: Span) -> Option<Local> {
        let (mut place, _, _) = self.flat_enum_place(scrutinee)?;
        place.projection.push(Projection::Field(0));
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let disc = self.fresh(i64_ty);
        self.emit_assign(Place::local(disc), Rvalue::Use(Operand::Copy(place)), span);
        Some(disc)
    }

    /// A local holding field `position` of variant `variant` of the by-value
    /// enum `scrutinee` names, and that field's type. Through a `&mut` a
    /// field a write reaches in place - a string, a container, an aggregate -
    /// is named by its address; any other field is copied.
    fn flat_enum_field(
        &mut self,
        scrutinee: Local,
        variant: u32,
        position: usize,
        span: Span,
    ) -> Option<(Local, Ty)> {
        let (base, enum_ty, mutable) = self.flat_enum_place(scrutinee)?;
        let layout = self.flat_layout(enum_ty)?;
        let field = layout.field_of(variant, u32::try_from(position).ok()?)?;
        let storage = layout.regions[field as usize - 1];
        let field_ty = *layout
            .variant_field_tys
            .get(variant as usize)?
            .get(position)?;
        let mut place = base;
        place.projection.push(Projection::Field(field));
        if storage != field_ty {
            let word = self.fresh(storage);
            self.emit_assign(Place::local(word), Rvalue::Use(Operand::Copy(place)), span);
            return Some((self.field_out_of_storage(word, field_ty, span), field_ty));
        }
        // Through a `&mut` a string or a heap enum is named by the address of
        // the word holding it, as a `&mut` parameter of either names its
        // caller's; a container is shared by its handle, and a scalar copied.
        let by_word = matches!(self.tcx.kind_of(field_ty), TyKind::String)
            || self.tcx.is_payload_enum(field_ty);
        if mutable && by_word {
            let regions = &layout.regions[..field as usize - 1];
            let words: u32 = 1 + regions
                .iter()
                .map(|t| self.type_slot_bytes(*t).div_ceil(8).max(1))
                .sum::<u32>();
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let ref_ty = self.tcx.intern(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Mut,
                inner: field_ty,
            });
            let local = self.fresh(ref_ty);
            let base_ptr = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(base_ptr),
                Rvalue::Use(Operand::Copy(Place::local(scrutinee))),
                span,
            );
            self.emit_assign(
                Place::local(local),
                Rvalue::CallIntrinsic {
                    name: "gos_enum_word_addr",
                    args: vec![
                        Operand::Copy(Place::local(base_ptr)),
                        Operand::Const(ConstValue::Int(i128::from(words) * 8)),
                    ],
                },
                span,
            );
            return Some((local, field_ty));
        }
        let in_place = mutable
            && !by_word
            && !matches!(
                self.tcx.kind_of(field_ty),
                TyKind::Int(_)
                    | TyKind::Float(_)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::Vec(_)
                    | TyKind::Slice(_)
                    | TyKind::HashMap { .. }
            );
        if in_place {
            let ref_ty = self.tcx.intern(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Mut,
                inner: field_ty,
            });
            let local = self.fresh(ref_ty);
            self.emit_assign(
                Place::local(local),
                Rvalue::Ref {
                    mutable: true,
                    place,
                },
                span,
            );
            return Some((local, field_ty));
        }
        let local = self.fresh(field_ty);
        self.emit_assign(Place::local(local), Rvalue::Use(Operand::Copy(place)), span);
        Some((local, field_ty))
    }

    /// The fields a variant pattern names, by position: a tuple pattern's
    /// elements around any `..`, or a struct pattern's fields by name.
    pub(crate) fn flat_variant_positions<'p>(
        &self,
        enum_name: &str,
        variant: &str,
        tuple_fields: Option<&'p [HirPat]>,
        struct_fields: Option<&'p [gossamer_hir::HirFieldPat]>,
    ) -> Option<Vec<FlatFieldPat<'p>>> {
        if let Some(fields) = tuple_fields {
            let arity = self
                .enums
                .field_tys_of(enum_name, variant)
                .map_or(fields.len(), |tys| tys.len());
            let rest = fields
                .iter()
                .position(|f| matches!(f.kind, HirPatKind::Rest));
            let mut out = Vec::with_capacity(fields.len());
            for (i, field) in fields.iter().enumerate() {
                if matches!(field.kind, HirPatKind::Rest | HirPatKind::Wildcard) {
                    continue;
                }
                let position = match rest {
                    Some(r) if i > r => arity - (fields.len() - i),
                    _ => i,
                };
                out.push(FlatFieldPat {
                    position,
                    pattern: Some(field),
                    name: None,
                });
            }
            return Some(out);
        }
        let order = self.enums.field_names_of(enum_name, variant)?;
        let mut out = Vec::new();
        for field in struct_fields.unwrap_or_default() {
            let position = order.iter().position(|n| *n == field.name.name)?;
            if matches!(
                field.pattern.as_ref().map(|p| &p.kind),
                Some(HirPatKind::Wildcard)
            ) {
                continue;
            }
            out.push(FlatFieldPat {
                position,
                pattern: field.pattern.as_ref(),
                name: Some(field.name.name.as_str()),
            });
        }
        Some(out)
    }

    /// The variant a variant or struct pattern over the by-value enum
    /// `scrutinee` names, and the fields it matches, or `None` when
    /// `scrutinee` holds no such enum or `pattern` is not a variant pattern.
    pub(crate) fn flat_pattern_fields<'p>(
        &self,
        scrutinee: Local,
        pattern: &'p HirPat,
    ) -> Option<(u32, Vec<FlatFieldPat<'p>>)> {
        let (_, enum_ty, _) = self.flat_enum_place(scrutinee)?;
        let enum_name = self.enum_index_name_of(enum_ty)?;
        let (name, tuple_fields, struct_fields) = match &pattern.kind {
            HirPatKind::Variant { name, fields } => (name, Some(fields.as_slice()), None),
            HirPatKind::Struct { name, fields, .. } => (name, None, Some(fields.as_slice())),
            _ => return None,
        };
        let variant = self.enums.variant_of_enum(&enum_name, &name.name)?;
        let fields =
            self.flat_variant_positions(&enum_name, &name.name, tuple_fields, struct_fields)?;
        Some((u32::try_from(variant).ok()?, fields))
    }

    /// The predicate of a variant pattern over the by-value enum `scrutinee`
    /// names: its discriminant is `variant`, and every field matches its
    /// sub-pattern. Fields are read, and their bindings made, only once the
    /// discriminant has matched.
    pub(crate) fn lower_flat_variant_predicate(
        &mut self,
        scrutinee: Local,
        variant: u32,
        fields: &[FlatFieldPat<'_>],
        span: Span,
    ) -> Option<Local> {
        let bool_ty = self.tcx.bool_ty();
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let disc = self.flat_enum_disc(scrutinee, span)?;
        let expected = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(expected),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(variant)))),
            span,
        );
        let matched = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(matched),
            Rvalue::BinaryOp {
                op: BinOp::Eq,
                lhs: Operand::Copy(Place::local(disc)),
                rhs: Operand::Copy(Place::local(expected)),
            },
            span,
        );
        if fields.is_empty() {
            return Some(matched);
        }
        let answer = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(answer),
            Rvalue::Use(Operand::Const(ConstValue::Bool(false))),
            span,
        );
        let guarded = self.new_block(span);
        let merge = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(matched)),
            arms: vec![(0, merge)],
            default: guarded,
        });
        self.set_current(guarded);
        let acc = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
            span,
        );
        for field in fields {
            let (local, _) = self.flat_enum_field(scrutinee, variant, field.position, span)?;
            match field.pattern {
                Some(pattern) => {
                    let sub = self.lower_pattern_predicate(local, pattern, span)?;
                    self.emit_assign(
                        Place::local(acc),
                        Rvalue::BinaryOp {
                            op: BinOp::BitAnd,
                            lhs: Operand::Copy(Place::local(acc)),
                            rhs: Operand::Copy(Place::local(sub)),
                        },
                        span,
                    );
                }
                None => {
                    if let Some(name) = field.name {
                        self.bind_local(name, local);
                    }
                }
            }
        }
        self.emit_assign(
            Place::local(answer),
            Rvalue::Use(Operand::Copy(Place::local(acc))),
            span,
        );
        self.terminate(Terminator::Goto { target: merge });
        self.set_current(merge);
        Some(answer)
    }

    /// Binds the fields of variant `variant` of the by-value enum `scrutinee`
    /// names, for a pattern already known to match.
    pub(crate) fn bind_flat_variant_fields(
        &mut self,
        scrutinee: Local,
        variant: u32,
        fields: &[FlatFieldPat<'_>],
        span: Span,
    ) {
        for field in fields {
            let Some((local, _)) = self.flat_enum_field(scrutinee, variant, field.position, span)
            else {
                continue;
            };
            match field.pattern {
                Some(pattern) => match &pattern.kind {
                    HirPatKind::Binding { name, .. } => self.bind_local(&name.name, local),
                    _ => self.bind_aggregate_let_pattern(local, pattern, span),
                },
                None => {
                    if let Some(name) = field.name {
                        self.bind_local(name, local);
                    }
                }
            }
        }
    }
}
