//! Reads and writes of `Option` fields a packed struct holds in one word.
//!
//! A packed struct keeps an `Option` of a struct or tuple in one word: the
//! `Some` payload's counted blob, never null, or zero for `None`
//! ([`TyCtxt::is_niche_option`]). Everything else handles the two-word
//! `[disc, payload]` carrier, so every place that reaches such a field is
//! rerouted through a carrier temporary: the field is decoded into it before
//! the statement and, where the statement may write it, encoded back after.
//! A struct literal stores zero for each such field and then encodes the
//! operand into it.

use gossamer_types::{Mutbl, Ty, TyCtxt, TyKind};

use crate::ir::{
    BasicBlock, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Projection, Rvalue,
    Statement, StatementKind, Terminator,
};

const LOAD: &str = "gos_rt_niche_option_load";
const STORE: &str = "gos_rt_niche_option_store";

/// Reroutes every access to a one-word `Option` field in `body`.
pub(crate) fn encode_niche_fields(body: &mut Body, tcx: &mut TyCtxt) {
    if !body_reaches_niche_field(body, tcx) {
        return;
    }
    let unit = tcx.unit();
    let mut rewriter = Rewriter { tcx, unit };
    for bi in 0..body.blocks.len() {
        let stmts = std::mem::take(&mut body.blocks[bi].stmts);
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            let mut access = Access::default();
            let span = stmt.span;
            let inlined = stmt.inlined.clone();
            let kind = rewriter.statement(body, stmt.kind, &mut access);
            let wrap = |kind| Statement {
                kind,
                span,
                inlined: inlined.clone(),
            };
            out.extend(access.before.into_iter().map(wrap));
            out.push(wrap(kind));
            out.extend(access.after.into_iter().map(wrap));
        }
        body.blocks[bi].stmts = out;
        let terminator = std::mem::replace(&mut body.blocks[bi].terminator, Terminator::Return);
        let mut access = Access::default();
        let terminator = rewriter.terminator(body, terminator, &mut access);
        let span = body.blocks[bi].span;
        let inlined = body.blocks[bi].terminator_inlined.clone();
        let wrap = |kind| Statement {
            kind,
            span,
            inlined: inlined.clone(),
        };
        let before: Vec<Statement> = access.before.into_iter().map(wrap).collect();
        body.blocks[bi].stmts.extend(before);
        let terminator = if access.after.is_empty() {
            terminator
        } else {
            // A call's destination is written when the call returns, so its
            // encode runs in a block of its own between the call and where
            // it continues.
            let Terminator::Call {
                callee,
                args,
                destination,
                target: Some(target),
            } = terminator
            else {
                unreachable!("only a returning call writes a place a terminator names")
            };
            let index = body.blocks.len();
            let after: Vec<Statement> = access.after.into_iter().map(wrap).collect();
            body.blocks.push(BasicBlock {
                id: BlockId(u32::try_from(index).expect("block overflow")),
                stmts: after,
                terminator: Terminator::Goto { target },
                span,
                terminator_span: None,
                terminator_inlined: None,
            });
            Terminator::Call {
                callee,
                args,
                destination,
                target: Some(BlockId(u32::try_from(index).expect("block overflow"))),
            }
        };
        body.blocks[bi].terminator = terminator;
    }
}

/// The statements one rewritten statement needs around it.
#[derive(Default)]
struct Access {
    before: Vec<StatementKind>,
    after: Vec<StatementKind>,
}

/// How a statement uses a place: read only, or possibly written through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    Read,
    Write,
}

struct Rewriter<'t> {
    tcx: &'t mut TyCtxt,
    unit: Ty,
}

impl Rewriter<'_> {
    fn statement(
        &mut self,
        body: &mut Body,
        kind: StatementKind,
        access: &mut Access,
    ) -> StatementKind {
        match kind {
            StatementKind::Assign { place, rvalue } => {
                let (rvalue, fills) = self.split_struct_literal(body, &place, rvalue);
                let rvalue = self.rvalue(body, rvalue, access);
                let place = self.place(body, place, Use::Write, access);
                for (index, value) in fills {
                    let value = self.operand(body, value, Use::Read, access);
                    let mut field = place.clone();
                    field.projection.push(Projection::Field(index));
                    let field_ty = crate::uniqueness::place_ty(self.tcx, body, &field)
                        .expect("a struct literal's field has its declared type");
                    let addr = self.address_of(body, field, field_ty, access, Order::After);
                    access.after.push(self.store(body, addr, value));
                }
                StatementKind::Assign { place, rvalue }
            }
            StatementKind::SetDiscriminant { place, variant } => StatementKind::SetDiscriminant {
                place: self.place(body, place, Use::Write, access),
                variant,
            },
            StatementKind::StaticStore { target, value } => StatementKind::StaticStore {
                target,
                value: self.operand(body, value, Use::Read, access),
            },
            StatementKind::IterSource {
                dst,
                source_kind,
                source,
                item_ty,
                ownership,
            } => StatementKind::IterSource {
                source: self.operand(body, source, Use::Read, access),
                dst: self.place(body, dst, Use::Write, access),
                source_kind,
                item_ty,
                ownership,
            },
            StatementKind::IterAdapter {
                dst,
                adapter_kind,
                upstream,
                closure_or_arg,
                item_ty,
            } => StatementKind::IterAdapter {
                upstream: self.place(body, upstream, Use::Write, access),
                closure_or_arg: closure_or_arg
                    .map(|arg| self.operand(body, arg, Use::Read, access)),
                dst: self.place(body, dst, Use::Write, access),
                adapter_kind,
                item_ty,
            },
            StatementKind::IterNext {
                dst_option,
                iter_place,
                item_ty,
            } => StatementKind::IterNext {
                iter_place: self.place(body, iter_place, Use::Write, access),
                dst_option: self.place(body, dst_option, Use::Write, access),
                item_ty,
            },
            other @ (StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop) => other,
        }
    }

    fn terminator(
        &mut self,
        body: &mut Body,
        terminator: Terminator,
        access: &mut Access,
    ) -> Terminator {
        match terminator {
            Terminator::SwitchInt {
                discriminant,
                arms,
                default,
            } => Terminator::SwitchInt {
                discriminant: self.operand(body, discriminant, Use::Read, access),
                arms,
                default,
            },
            Terminator::Call {
                callee,
                args,
                destination,
                target,
            } => {
                let args = args
                    .into_iter()
                    .map(|arg| self.operand(body, arg, Use::Read, access))
                    .collect();
                let destination = if target.is_some() {
                    self.place(body, destination, Use::Write, access)
                } else {
                    destination
                };
                Terminator::Call {
                    callee: self.operand(body, callee, Use::Read, access),
                    args,
                    destination,
                    target,
                }
            }
            Terminator::Assert {
                cond,
                expected,
                msg,
                target,
            } => Terminator::Assert {
                cond: self.operand(body, cond, Use::Read, access),
                expected,
                msg,
                target,
            },
            Terminator::Drop { place, target } => Terminator::Drop {
                place: self.place(body, place, Use::Read, access),
                target,
            },
            other @ (Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Resume
            | Terminator::Unreachable
            | Terminator::Panic { .. }) => other,
        }
    }

    fn rvalue(&mut self, body: &mut Body, rvalue: Rvalue, access: &mut Access) -> Rvalue {
        let mut read = |this: &mut Self, op| this.operand(body, op, Use::Read, access);
        match rvalue {
            Rvalue::Use(op) => Rvalue::Use(read(self, op)),
            Rvalue::BinaryOp { op, lhs, rhs } => Rvalue::BinaryOp {
                op,
                lhs: read(self, lhs),
                rhs: read(self, rhs),
            },
            Rvalue::UnaryOp { op, operand } => Rvalue::UnaryOp {
                op,
                operand: read(self, operand),
            },
            Rvalue::Cast { operand, target } => Rvalue::Cast {
                operand: read(self, operand),
                target,
            },
            Rvalue::Aggregate { kind, operands } => Rvalue::Aggregate {
                kind,
                operands: operands.into_iter().map(|op| read(self, op)).collect(),
            },
            Rvalue::Repeat { value, count } => Rvalue::Repeat {
                value: read(self, value),
                count,
            },
            Rvalue::Len(place) => Rvalue::Len(self.place(body, place, Use::Read, access)),
            // A reference may be written through, and an intrinsic may take
            // a carrier by its address and write it.
            Rvalue::Ref { mutable, place } => Rvalue::Ref {
                mutable,
                place: self.place(body, place, Use::Write, access),
            },
            Rvalue::CallIntrinsic { name, args } => Rvalue::CallIntrinsic {
                name,
                args: args
                    .into_iter()
                    .map(|op| self.operand(body, op, Use::Write, access))
                    .collect(),
            },
            Rvalue::StaticLoad(_) => rvalue,
        }
    }

    fn operand(&mut self, body: &mut Body, op: Operand, how: Use, access: &mut Access) -> Operand {
        match op {
            Operand::Copy(place) => Operand::Copy(self.place(body, place, how, access)),
            other => other,
        }
    }

    /// `place`, rerouted through a carrier temporary when it reaches a
    /// one-word `Option` field.
    fn place(&mut self, body: &mut Body, place: Place, how: Use, access: &mut Access) -> Place {
        let Some((split, field_ty)) = niche_prefix(self.tcx, body, &place) else {
            return place;
        };
        let prefix = Place {
            local: place.local,
            projection: place.projection[..split].to_vec(),
        };
        let addr = self.address_of(body, prefix, field_ty, access, Order::Before);
        let temp = new_local(body, field_ty);
        access.before.push(StatementKind::Assign {
            place: Place::local(temp),
            rvalue: Rvalue::CallIntrinsic {
                name: LOAD,
                args: vec![Operand::Copy(Place::local(addr))],
            },
        });
        if how == Use::Write {
            let store = self.store(body, addr, Operand::Copy(Place::local(temp)));
            access.after.push(store);
        }
        let rest = Place {
            local: temp,
            projection: place.projection[split..].to_vec(),
        };
        // The carrier's own words may reach a further one-word field.
        self.place(body, rest, how, access)
    }

    /// A local holding the address of `place`, an `Option` field held in one
    /// word, taken before or after the statement.
    fn address_of(
        &mut self,
        body: &mut Body,
        place: Place,
        field_ty: Ty,
        access: &mut Access,
        order: Order,
    ) -> Local {
        let addr_ty = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: field_ty,
        });
        let addr = new_local(body, addr_ty);
        let take = StatementKind::Assign {
            place: Place::local(addr),
            rvalue: Rvalue::Ref {
                mutable: true,
                place,
            },
        };
        match order {
            Order::Before => access.before.push(take),
            Order::After => access.after.push(take),
        }
        addr
    }

    fn store(&mut self, body: &mut Body, addr: Local, value: Operand) -> StatementKind {
        let sink = new_local(body, self.unit);
        StatementKind::Assign {
            place: Place::local(sink),
            rvalue: Rvalue::CallIntrinsic {
                name: STORE,
                args: vec![Operand::Copy(Place::local(addr)), value],
            },
        }
    }

    /// A struct literal building into `place`, with each operand of a
    /// one-word `Option` field taken out and replaced by zero, paired with
    /// its field index for the encode after the literal.
    fn split_struct_literal(
        &mut self,
        body: &Body,
        place: &Place,
        rvalue: Rvalue,
    ) -> (Rvalue, Vec<(u32, Operand)>) {
        let Rvalue::Aggregate { kind, mut operands } = rvalue else {
            return (rvalue, Vec::new());
        };
        let Some(ty) = crate::uniqueness::place_ty(self.tcx, body, place) else {
            return (Rvalue::Aggregate { kind, operands }, Vec::new());
        };
        let mut fills = Vec::new();
        for (index, operand) in operands.iter_mut().enumerate() {
            if self.tcx.is_niche_field(ty, index) {
                let value = std::mem::replace(operand, Operand::Const(ConstValue::Int(0)));
                fills.push((u32::try_from(index).expect("field index overflow"), value));
            }
        }
        (Rvalue::Aggregate { kind, operands }, fills)
    }
}

#[derive(Clone, Copy)]
enum Order {
    Before,
    After,
}

/// How many projection steps of `place` reach a one-word `Option` field,
/// with that field's type, or `None` when it reaches none.
fn niche_prefix(tcx: &TyCtxt, body: &Body, place: &Place) -> Option<(usize, Ty)> {
    let mut ty = body.locals.get(place.local.0 as usize)?.ty;
    let mut variant: Option<u32> = None;
    for (step_index, step) in place.projection.iter().enumerate() {
        let mut base = ty;
        if !matches!(step, Projection::Deref) {
            while let TyKind::Ref { inner, .. } = tcx.kind_of(base) {
                base = *inner;
            }
        }
        ty = match (step, tcx.kind_of(base)) {
            (Projection::Deref, TyKind::Ref { inner, .. }) => *inner,
            (Projection::Downcast(v), TyKind::Adt { .. }) => {
                variant = Some(*v);
                continue;
            }
            (Projection::Field(i), TyKind::Adt { def, substs }) => {
                if let Some(v) = variant.take() {
                    *tcx.enum_variant_tys(*def)?
                        .get(v as usize)?
                        .get(*i as usize)?
                } else {
                    if tcx.is_niche_field(base, *i as usize) {
                        let field = *tcx.adt_field_tys(*def, substs)?.get(*i as usize)?;
                        return Some((step_index + 1, field));
                    }
                    *tcx.adt_field_tys(*def, substs)?.get(*i as usize)?
                }
            }
            (Projection::Field(i), TyKind::Tuple(elems)) => *elems.get(*i as usize)?,
            (
                Projection::Index(_),
                TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. },
            ) => *elem,
            _ => return None,
        };
    }
    None
}

fn body_reaches_niche_field(body: &Body, tcx: &TyCtxt) -> bool {
    let reaches = |place: &Place| niche_prefix(tcx, body, place).is_some();
    let operand = |op: &Operand| matches!(op, Operand::Copy(place) if reaches(place));
    let rvalue = |rv: &Rvalue| match rv {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. } => operand(op),
        Rvalue::BinaryOp { lhs, rhs, .. } => operand(lhs) || operand(rhs),
        Rvalue::Aggregate { operands, .. } => operands.iter().any(operand),
        Rvalue::CallIntrinsic { args, .. } => args.iter().any(operand),
        Rvalue::Repeat { value, .. } => operand(value),
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => reaches(place),
        Rvalue::StaticLoad(_) => false,
    };
    body.blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| match &stmt.kind {
            StatementKind::Assign { place, rvalue: rv } => {
                reaches(place)
                    || rvalue(rv)
                    || matches!(rv, Rvalue::Aggregate { .. })
                        && crate::uniqueness::place_ty(tcx, body, place)
                            .is_some_and(|ty| has_niche_field(tcx, ty))
            }
            StatementKind::SetDiscriminant { place, .. } => reaches(place),
            StatementKind::StaticStore { value, .. } => operand(value),
            StatementKind::IterSource { dst, source, .. } => reaches(dst) || operand(source),
            StatementKind::IterAdapter {
                dst,
                upstream,
                closure_or_arg,
                ..
            } => reaches(dst) || reaches(upstream) || closure_or_arg.as_ref().is_some_and(operand),
            StatementKind::IterNext {
                dst_option,
                iter_place,
                ..
            } => reaches(dst_option) || reaches(iter_place),
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {
                false
            }
        }) || match &block.terminator {
            Terminator::SwitchInt { discriminant, .. } => operand(discriminant),
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => operand(callee) || args.iter().any(operand) || reaches(destination),
            Terminator::Assert { cond, .. } => operand(cond),
            Terminator::Drop { place, .. } => reaches(place),
            _ => false,
        }
    })
}

/// Whether the struct `ty` holds an `Option` in one word.
fn has_niche_field(tcx: &TyCtxt, ty: Ty) -> bool {
    let TyKind::Adt { def, substs } = tcx.kind_of(ty) else {
        return false;
    };
    tcx.adt_field_tys(*def, substs)
        .is_some_and(|fields| (0..fields.len()).any(|index| tcx.is_niche_field(ty, index)))
}

fn new_local(body: &mut Body, ty: Ty) -> Local {
    let local = Local(u32::try_from(body.locals.len()).expect("local overflow"));
    body.locals.push(LocalDecl {
        ty,
        debug_name: None,
        mutable: true,
        region: false,
    });
    local
}
