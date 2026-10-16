//! `Option` and `Result` over a by-value enum, held as a tuple of the
//! discriminant and the payloads instead of a two-word carrier whose payload
//! word points at a heap copy.
//!
//! Every local of such a carrier type becomes the tuple `(disc, ok)` for an
//! `Option`, `(disc, ok, err)` for a `Result`; the arm a value does not hold
//! is zeroes. The carrier operations on those locals become field reads and
//! tuple builds, so the ownership passes that run afterwards account for the
//! payload as they do for any tuple. Where a value meets code that takes the
//! two-word carrier - a runtime helper, a field or container slot, a
//! function whose address escapes - it is boxed into one, and a carrier that
//! code hands back is unboxed.

use std::collections::{HashMap, HashSet};

use gossamer_resolve::DefId;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::ir::{
    AggregateKind, BasicBlock, BinOp, BlockId, Body, ConstValue, InlineChain, Local, LocalDecl,
    Operand, Place, Projection, Rvalue, Statement, StatementKind, Terminator,
};
use crate::lower::FieldRcKind;

/// Which bodies keep the two-word carrier at their boundary.
pub(crate) struct CarrierAbi {
    boxed_names: HashSet<String>,
    boxed_defs: HashSet<DefId>,
    by_def: HashMap<DefId, String>,
    known: HashSet<String>,
}

impl CarrierAbi {
    /// Reads off `bodies` which functions are reached other than by a direct
    /// call: closures, and functions whose address a body takes. Those keep
    /// the carrier at their boundary, since whoever calls them through the
    /// address passes and expects one.
    pub(crate) fn collect(bodies: &[Body]) -> Self {
        let mut boxed_names = HashSet::new();
        let mut boxed_defs = HashSet::new();
        let mut by_def = HashMap::new();
        let mut known = HashSet::new();
        for body in bodies {
            known.insert(body.name.clone());
            if let Some(def) = body.def {
                by_def.insert(def, body.name.clone());
            }
            if body.name.starts_with("__closure") {
                boxed_names.insert(body.name.clone());
            }
            for_each_value_operand(body, &mut |operand| match operand {
                Operand::FnRef { def, .. } => {
                    boxed_defs.insert(*def);
                }
                Operand::Const(_) | Operand::Copy(_) => {}
            });
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign {
                        rvalue:
                            Rvalue::CallIntrinsic {
                                name: "gos_fn_addr",
                                args,
                            },
                        ..
                    } = &stmt.kind
                    {
                        for arg in args {
                            if let Operand::Const(ConstValue::Str(name)) = arg {
                                boxed_names.insert(name.clone());
                            }
                        }
                    }
                }
            }
        }
        Self {
            boxed_names,
            boxed_defs,
            by_def,
            known,
        }
    }

    /// Whether `body` takes and answers flattened carriers.
    fn body_is_flat(&self, body: &Body) -> bool {
        !self.boxed_names.contains(&body.name)
            && !body.def.is_some_and(|def| self.boxed_defs.contains(&def))
    }

    /// Whether a call through `callee` reaches a body that takes and answers
    /// flattened carriers.
    fn callee_is_flat(&self, callee: &Operand) -> bool {
        match callee {
            Operand::FnRef { def, .. } => {
                !self.boxed_defs.contains(def)
                    && self
                        .by_def
                        .get(def)
                        .is_some_and(|name| !self.boxed_names.contains(name))
            }
            Operand::Const(ConstValue::Str(name)) => {
                self.known.contains(name) && !self.boxed_names.contains(name)
            }
            _ => false,
        }
    }
}

/// Every operand `body` uses as a value rather than as a call's callee.
fn for_each_value_operand(body: &Body, f: &mut impl FnMut(&Operand)) {
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                rvalue_operands(rvalue, f);
            }
        }
        if let Terminator::Call { args, .. } = &block.terminator {
            args.iter().for_each(&mut *f);
        }
    }
}

fn rvalue_operands(rvalue: &Rvalue, f: &mut impl FnMut(&Operand)) {
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. } => {
            f(op);
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::Aggregate { operands, .. } => operands.iter().for_each(f),
        Rvalue::CallIntrinsic { args, .. } => args.iter().for_each(f),
        Rvalue::Repeat { value, .. } => f(value),
        _ => {}
    }
}

/// How a carrier local is held once the pass is done with it.
#[derive(Clone, Copy)]
struct Flat {
    /// The carrier type the local had.
    carrier: Ty,
    /// The tuple it is held as.
    tuple: Ty,
}

/// Rewrites the carriers of `body` over a by-value enum into tuples.
pub(crate) fn flatten_carriers(body: &mut Body, abi: &CarrierAbi, tcx: &mut TyCtxt) {
    let n = body.locals.len();
    let mut flat: Vec<Option<Flat>> = vec![None; n];
    let mut any = false;
    for (index, slot) in flat.iter_mut().enumerate() {
        let carrier = body.locals[index].ty;
        if let Some(tuple) = tcx.flat_carrier_ty(carrier) {
            *slot = Some(Flat { carrier, tuple });
            any = true;
        }
    }
    if !any {
        return;
    }
    let boundary_flat = abi.body_is_flat(body);
    let arity = body.arity as usize;
    // The return slot and the parameters follow the body's boundary: held
    // flat in a body whose callers pass flat values, boxed otherwise.
    if !boundary_flat {
        for slot in flat.iter_mut().take(arity + 1) {
            *slot = None;
        }
    }
    // A carrier read through a projection, borrowed, or handed to a
    // statement the rewrite does not know stays boxed. A parameter or the
    // return slot the boundary fixes as flat is then mirrored by a boxed
    // shadow that carries those uses.
    let mut shadow: HashMap<Local, Local> = HashMap::new();
    for index in 0..n {
        if flat[index].is_none() || !needs_boxed_storage(body, Local(index as u32)) {
            continue;
        }
        if index <= arity && boundary_flat {
            let carrier = body.locals[index].ty;
            let boxed = push_local(body, carrier);
            shadow.insert(Local(index as u32), boxed);
        } else {
            flat[index] = None;
        }
    }
    for (index, slot) in flat.iter().enumerate() {
        if let Some(f) = slot {
            body.locals[index].ty = f.tuple;
        }
    }
    flat.resize(body.locals.len(), None);
    let mut rw = Rewriter {
        body,
        tcx,
        abi,
        flat,
        cur: BlockId(0),
    };
    if !shadow.is_empty() {
        rw.substitute_shadows(&shadow);
    }
    rw.rewrite_blocks(&shadow);
}

/// Whether a use of `local` needs the two-word carrier in storage.
fn needs_boxed_storage(body: &Body, local: Local) -> bool {
    let projected = |place: &Place| place.local == local && !place.projection.is_empty();
    let operand_projected = |op: &Operand| matches!(op, Operand::Copy(p) if projected(p));
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if projected(place) {
                        return true;
                    }
                    let mut hit = false;
                    rvalue_operands(rvalue, &mut |op| hit |= operand_projected(op));
                    if hit {
                        return true;
                    }
                    if let Rvalue::Ref { place, .. } | Rvalue::Len(place) = rvalue
                        && place.local == local
                    {
                        return true;
                    }
                }
                StatementKind::Nop
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_) => {}
                _ => {
                    if crate::opt::stmt_mentions_local(stmt, local) {
                        return true;
                    }
                }
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                if projected(destination)
                    || args.iter().any(operand_projected)
                    || matches!(callee, Operand::Copy(p) if p.local == local)
                {
                    return true;
                }
            }
            Terminator::Return | Terminator::Goto { .. } | Terminator::Unreachable => {}
            other => {
                if crate::opt::term_mentions_local(other, local)
                    || matches!(other, Terminator::Drop { place, .. } if place.local == local)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn push_local(body: &mut Body, ty: Ty) -> Local {
    let local = Local(u32::try_from(body.locals.len()).expect("local index fits in u32"));
    body.locals.push(LocalDecl {
        ty,
        debug_name: None,
        mutable: true,
        region: false,
    });
    local
}

struct Rewriter<'a, 'b> {
    body: &'a mut Body,
    tcx: &'b mut TyCtxt,
    abi: &'b CarrierAbi,
    flat: Vec<Option<Flat>>,
    cur: BlockId,
}

impl Rewriter<'_, '_> {
    fn flat_of(&self, local: Local) -> Option<Flat> {
        self.flat.get(local.0 as usize).copied().flatten()
    }

    /// The flattened local `operand` names whole, if any.
    fn flat_operand(&self, operand: &Operand) -> Option<(Local, Flat)> {
        match operand {
            Operand::Copy(place) if place.projection.is_empty() => {
                self.flat_of(place.local).map(|f| (place.local, f))
            }
            _ => None,
        }
    }

    fn local(&mut self, ty: Ty) -> Local {
        let local = push_local(self.body, ty);
        self.flat.push(None);
        local
    }

    fn emit(&mut self, kind: StatementKind, span: gossamer_lex::Span) {
        let block = &mut self.body.blocks[self.cur.0 as usize];
        block.stmts.push(Statement {
            kind,
            span,
            inlined: InlineChain::default(),
        });
    }

    fn assign(&mut self, place: Place, rvalue: Rvalue, span: gossamer_lex::Span) {
        self.emit(StatementKind::Assign { place, rvalue }, span);
    }

    fn new_block(&mut self, span: gossamer_lex::Span) -> BlockId {
        let id = BlockId(u32::try_from(self.body.blocks.len()).expect("block index fits in u32"));
        self.body.blocks.push(BasicBlock {
            id,
            stmts: Vec::new(),
            terminator: Terminator::Unreachable,
            span,
            terminator_span: None,
            terminator_inlined: InlineChain::default(),
        });
        id
    }

    fn terminate(&mut self, terminator: Terminator) {
        self.body.blocks[self.cur.0 as usize].terminator = terminator;
    }

    /// Branches on whether `disc` names the first arm: `then` fills the
    /// first arm's block, `otherwise` the other's, and both continue in a
    /// block the cursor is left at.
    fn branch_on_disc(
        &mut self,
        disc: Operand,
        span: gossamer_lex::Span,
        then: impl FnOnce(&mut Self),
        otherwise: impl FnOnce(&mut Self),
    ) {
        let first = self.new_block(span);
        let second = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: disc,
            arms: vec![(0, first)],
            default: second,
        });
        self.cur = first;
        then(self);
        self.terminate(Terminator::Goto { target: join });
        self.cur = second;
        otherwise(self);
        self.terminate(Terminator::Goto { target: join });
        self.cur = join;
    }

    /// An operand holding the all-zero value of `ty`.
    fn zero(&mut self, ty: Ty, span: gossamer_lex::Span) -> Operand {
        if crate::lower::carrier_ref::is_two_word_carrier(self.tcx, ty) {
            let zero = self.local(ty);
            self.assign(
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
            TyKind::Adt { def, substs } if def.local < u32::MAX - 16 => {
                let (def, substs) = (*def, substs.clone());
                self.tcx.adt_field_tys(def, &substs).map(<[Ty]>::to_vec)
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
        let operands = fields.iter().map(|f| self.zero(*f, span)).collect();
        let zero = self.local(ty);
        self.assign(
            Place::local(zero),
            Rvalue::Aggregate { kind, operands },
            span,
        );
        Operand::Copy(Place::local(zero))
    }

    /// The payload types of the carrier `f` stands for: `Ok` / `Some`, then
    /// `Err` for a `Result`.
    /// `op`, or the integer constant it names when it is a local whose one
    /// definition is that constant, as a discriminant often reaches here.
    fn constant_operand(&self, op: &Operand) -> Operand {
        let Operand::Copy(place) = op else {
            return op.clone();
        };
        if !place.projection.is_empty() {
            return op.clone();
        }
        let mut value = None;
        for block in &self.body.blocks {
            for stmt in &block.stmts {
                match &stmt.kind {
                    StatementKind::Assign {
                        place: dest,
                        rvalue,
                    } if dest.local == place.local => {
                        match (value, dest.projection.is_empty(), rvalue) {
                            (None, true, Rvalue::Use(Operand::Const(ConstValue::Int(v)))) => {
                                value = Some(*v);
                            }
                            _ => return op.clone(),
                        }
                    }
                    StatementKind::Nop
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Assign { .. } => {}
                    _ => {
                        if crate::opt::stmt_mentions_local(stmt, place.local) {
                            return op.clone();
                        }
                    }
                }
            }
            if let Terminator::Call { destination, .. } = &block.terminator
                && destination.local == place.local
            {
                return op.clone();
            }
        }
        value.map_or_else(|| op.clone(), |v| Operand::Const(ConstValue::Int(v)))
    }

    fn arms(&self, f: Flat) -> Vec<Ty> {
        match self.tcx.kind_of(f.tuple) {
            TyKind::Tuple(elems) => elems[1..].to_vec(),
            _ => Vec::new(),
        }
    }

    /// Writes into `dest`, a flattened local, the tuple of discriminant
    /// `disc` holding `payload` in arm `arm`.
    fn build(
        &mut self,
        dest: Local,
        f: Flat,
        disc: Operand,
        arm: usize,
        payload: Operand,
        span: gossamer_lex::Span,
    ) {
        let arms = self.arms(f);
        let mut operands = vec![disc];
        for (index, ty) in arms.iter().enumerate() {
            operands.push(if index == arm {
                payload.clone()
            } else {
                self.zero(*ty, span)
            });
        }
        self.assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: AggregateKind::Tuple,
                operands,
            },
            span,
        );
    }

    /// The arm a payload of type `ty` belongs to in the carrier `f`.
    fn arm_of(&self, f: Flat, ty: Ty) -> Option<usize> {
        let ty = self.tcx.peel_nominal(ty);
        self.arms(f).iter().position(|arm| *arm == ty)
    }

    /// A boxed carrier holding what the flattened local `local` holds.
    fn boxed(&mut self, local: Local, f: Flat, span: gossamer_lex::Span) -> Local {
        let out = self.local(f.carrier);
        let arms = self.arms(f);
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let disc = self.local(i64_ty);
        self.assign(
            Place::local(disc),
            Rvalue::Use(Operand::Copy(field(local, 0))),
            span,
        );
        let ok = arms[0];
        let err = arms.get(1).copied();
        self.branch_on_disc(
            Operand::Copy(Place::local(disc)),
            span,
            |rw| {
                let payload = rw.local(ok);
                rw.assign(
                    Place::local(payload),
                    Rvalue::Use(Operand::Copy(field(local, 1))),
                    span,
                );
                rw.assign(
                    Place::local(out),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![
                            Operand::Const(ConstValue::Int(0)),
                            Operand::Copy(Place::local(payload)),
                        ],
                    },
                    span,
                );
            },
            |rw| {
                let payload = match err {
                    Some(err) => {
                        let payload = rw.local(err);
                        rw.assign(
                            Place::local(payload),
                            Rvalue::Use(Operand::Copy(field(local, 2))),
                            span,
                        );
                        Operand::Copy(Place::local(payload))
                    }
                    None => Operand::Const(ConstValue::Int(0)),
                };
                rw.assign(
                    Place::local(out),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![Operand::Const(ConstValue::Int(1)), payload],
                    },
                    span,
                );
            },
        );
        out
    }

    /// Fills the flattened local `dest` from `source`, a boxed carrier.
    fn unbox_into(&mut self, dest: Local, f: Flat, source: Operand, span: gossamer_lex::Span) {
        let arms = self.arms(f);
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let disc = self.local(i64_ty);
        self.assign(
            Place::local(disc),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![source.clone()],
            },
            span,
        );
        let extract = |rw: &mut Self, ty: Ty| -> Operand {
            let payload = rw.local(ty);
            let name = rw.payload_reader(ty);
            rw.assign(
                Place::local(payload),
                Rvalue::CallIntrinsic {
                    name,
                    args: vec![source.clone()],
                },
                span,
            );
            Operand::Copy(Place::local(payload))
        };
        let ok = arms[0];
        let err = arms.get(1).copied();
        self.branch_on_disc(
            Operand::Copy(Place::local(disc)),
            span,
            |rw| {
                let payload = extract(rw, ok);
                rw.build(
                    dest,
                    f,
                    Operand::Const(ConstValue::Int(0)),
                    0,
                    payload,
                    span,
                );
            },
            |rw| {
                let payload = match err {
                    Some(err) => extract(rw, err),
                    None => Operand::Const(ConstValue::Int(0)),
                };
                let arm = if err.is_some() { 1 } else { usize::MAX };
                rw.build(
                    dest,
                    f,
                    Operand::Const(ConstValue::Int(1)),
                    arm,
                    payload,
                    span,
                );
            },
        );
    }

    /// The intrinsic reading a payload of type `ty` out of a boxed carrier.
    fn payload_reader(&self, ty: Ty) -> &'static str {
        if matches!(self.tcx.kind_of(ty), TyKind::Float(_)) {
            "gos_rt_result_payload_f64"
        } else if crate::lower::carrier_ref::is_two_word_carrier(self.tcx, ty) {
            "gos_rt_result_payload_i128"
        } else {
            "gos_rt_result_payload"
        }
    }

    /// Replaces each use of a parameter or the return slot that needs a boxed
    /// carrier with its shadow, which entry fills from the parameter and each
    /// return reads back into the slot.
    fn substitute_shadows(&mut self, shadow: &HashMap<Local, Local>) {
        for block in &mut self.body.blocks {
            for stmt in &mut block.stmts {
                rename_in_stmt(stmt, shadow);
            }
            rename_in_terminator(&mut block.terminator, shadow);
        }
    }

    fn rewrite_blocks(&mut self, shadow: &HashMap<Local, Local>) {
        let originals: Vec<(Vec<Statement>, Terminator)> = self
            .body
            .blocks
            .iter_mut()
            .map(|block| {
                (
                    std::mem::take(&mut block.stmts),
                    std::mem::replace(&mut block.terminator, Terminator::Unreachable),
                )
            })
            .collect();
        for (index, (stmts, terminator)) in originals.into_iter().enumerate() {
            self.cur = BlockId(index as u32);
            let span = self.body.blocks[index].span;
            if index == 0 {
                // Parameters arrive flat; a shadowed one is boxed at entry.
                let mut params: Vec<(Local, Local)> = shadow
                    .iter()
                    .filter(|(param, _)| param.0 != 0)
                    .map(|(param, boxed)| (*param, *boxed))
                    .collect();
                params.sort_by_key(|(param, _)| param.0);
                for (param, boxed) in params {
                    if let Some(f) = self.flat_of(param) {
                        let made = self.boxed(param, f, span);
                        self.assign(
                            Place::local(boxed),
                            Rvalue::Use(Operand::Copy(Place::local(made))),
                            span,
                        );
                    }
                }
            }
            for stmt in stmts {
                self.rewrite_statement(stmt);
            }
            if matches!(terminator, Terminator::Return)
                && let Some(&boxed) = shadow.get(&Local::RETURN)
                && let Some(f) = self.flat_of(Local::RETURN)
            {
                self.unbox_into(Local::RETURN, f, Operand::Copy(Place::local(boxed)), span);
            }
            self.rewrite_terminator(terminator, span);
        }
    }

    fn rewrite_statement(&mut self, stmt: Statement) {
        let span = stmt.span;
        let StatementKind::Assign { place, rvalue } = stmt.kind else {
            self.body.blocks[self.cur.0 as usize].stmts.push(stmt);
            return;
        };
        let dest = (place.projection.is_empty())
            .then(|| self.flat_of(place.local).map(|f| (place.local, f)))
            .flatten();
        if let Some((dest, f)) = dest {
            self.rewrite_into_flat(dest, f, rvalue, span);
            return;
        }
        if let Some(rewritten) = self.rewrite_read(&place, &rvalue, span) {
            if rewritten {
                return;
            }
        }
        // Any other mention of a flattened carrier hands it to code that
        // takes the two-word form.
        let rvalue = self.box_operands(rvalue, span);
        self.assign(place, rvalue, span);
    }

    /// Fills the flattened local `dest` from `rvalue`.
    fn rewrite_into_flat(
        &mut self,
        dest: Local,
        f: Flat,
        rvalue: Rvalue,
        span: gossamer_lex::Span,
    ) {
        match rvalue {
            Rvalue::CallIntrinsic { name, args }
                if matches!(name, "gos_rt_result_new" | "gos_rt_result_new_owned")
                    && args.len() == 2 =>
            {
                let disc = self.constant_operand(&args[0]);
                let payload = args[1].clone();
                let arms = self.arms(f).len();
                // `None` carries nothing, so it fills no arm.
                let arm = match &disc {
                    Operand::Const(ConstValue::Int(1)) if arms == 1 => Some(usize::MAX),
                    Operand::Const(ConstValue::Int(d)) => {
                        usize::try_from(*d).ok().filter(|d| *d < arms)
                    }
                    _ => self.operand_ty(&payload).and_then(|ty| self.arm_of(f, ty)),
                };
                match arm {
                    Some(usize::MAX) => {
                        self.build(
                            dest,
                            f,
                            disc,
                            usize::MAX,
                            Operand::Const(ConstValue::Int(0)),
                            span,
                        );
                    }
                    Some(arm) => self.build(dest, f, disc, arm, payload, span),
                    None => {
                        let boxed = self.local(f.carrier);
                        self.assign(
                            Place::local(boxed),
                            Rvalue::CallIntrinsic { name, args },
                            span,
                        );
                        self.unbox_into(dest, f, Operand::Copy(Place::local(boxed)), span);
                    }
                }
            }
            Rvalue::Use(Operand::Copy(source))
                if source.projection.is_empty() && self.flat_of(source.local).is_some() =>
            {
                self.assign(Place::local(dest), Rvalue::Use(Operand::Copy(source)), span);
            }
            Rvalue::Use(Operand::Const(_)) => {
                let tuple = f.tuple;
                let zero = self.zero(tuple, span);
                self.assign(Place::local(dest), Rvalue::Use(zero), span);
            }
            other => {
                let other = self.box_operands(other, span);
                let boxed = self.local(f.carrier);
                self.assign(Place::local(boxed), other, span);
                self.unbox_into(dest, f, Operand::Copy(Place::local(boxed)), span);
            }
        }
    }

    /// Rewrites a statement that reads a flattened carrier through one of
    /// the carrier operations. `Some(true)` when the statement was replaced.
    fn rewrite_read(
        &mut self,
        place: &Place,
        rvalue: &Rvalue,
        span: gossamer_lex::Span,
    ) -> Option<bool> {
        let Rvalue::CallIntrinsic { name, args } = rvalue else {
            return None;
        };
        let (local, f) = self.flat_operand(args.first()?)?;
        match *name {
            "gos_rt_result_disc" => {
                self.assign(
                    place.clone(),
                    Rvalue::Use(Operand::Copy(field(local, 0))),
                    span,
                );
            }
            "gos_rt_result_payload"
            | "gos_rt_result_payload_i128"
            | "gos_rt_result_payload_f64" => {
                let dest_ty = self.place_ty(place)?;
                let arm = self.arm_of(f, dest_ty).unwrap_or(0);
                self.assign(
                    place.clone(),
                    Rvalue::Use(Operand::Copy(field(local, arm as u32 + 1))),
                    span,
                );
            }
            "gos_rt_option_is_some"
            | "gos_rt_result_is_ok"
            | "gos_rt_option_is_none"
            | "gos_rt_result_is_err" => {
                let op = if matches!(*name, "gos_rt_option_is_some" | "gos_rt_result_is_ok") {
                    BinOp::Eq
                } else {
                    BinOp::Ne
                };
                self.assign(
                    place.clone(),
                    Rvalue::BinaryOp {
                        op,
                        lhs: Operand::Copy(field(local, 0)),
                        rhs: Operand::Const(ConstValue::Int(0)),
                    },
                    span,
                );
            }
            "gos_rt_option_slot_retain" | "gos_rt_result_payload_retain" => {
                self.count_fields(local, f, true, None, span);
            }
            "gos_rt_option_slot_retain_ok" => self.count_fields(local, f, true, Some(1), span),
            "gos_rt_option_slot_release" | "gos_rt_result_payload_release" => {
                self.count_fields(local, f, false, None, span);
            }
            "gos_rt_result_ok_payload_release" => self.count_fields(local, f, false, Some(1), span),
            _ => return None,
        }
        Some(true)
    }

    /// The share counts a carrier operation took or gave on the payloads of
    /// `local`, as per-field calls on the tuple; `only` limits them to the
    /// fields under one arm.
    fn count_fields(
        &mut self,
        local: Local,
        f: Flat,
        retain: bool,
        only: Option<u32>,
        span: gossamer_lex::Span,
    ) {
        for (path, kind) in crate::lower::aggregate_rc_field_paths(self.tcx, f.tuple) {
            if only.is_some_and(|arm| path.first() != Some(&arm)) {
                continue;
            }
            let field_ty = field_ty_at(self.tcx, f.tuple, &path);
            let string = matches!(kind, FieldRcKind::Rc)
                && field_ty.is_some_and(|t| matches!(self.tcx.kind_of(t), TyKind::String));
            let name = match (retain, kind) {
                (true, FieldRcKind::Rc) if string => "gos_rt_str_retain_typed",
                (false, FieldRcKind::Rc) if string => "gos_rt_str_free_typed",
                (true, FieldRcKind::Carrier { .. }) => "gos_rt_result_payload_retain",
                (false, FieldRcKind::Carrier { .. }) => "gos_rt_result_payload_release",
                (true, other) => other.helpers().0,
                (false, other) => other.helpers().1,
            };
            let mut args = vec![Operand::Copy(Place {
                local,
                projection: path.iter().map(|i| Projection::Field(*i)).collect(),
            })];
            if let FieldRcKind::Carrier { ok, err } = kind {
                args.push(Operand::Const(ConstValue::Int(i128::from(ok))));
                args.push(Operand::Const(ConstValue::Int(i128::from(err))));
            }
            let unit = self.tcx.unit();
            let sink = self.local(unit);
            self.assign(
                Place::local(sink),
                Rvalue::CallIntrinsic { name, args },
                span,
            );
        }
    }

    /// `rvalue` with every flattened carrier it names whole boxed first.
    fn box_operands(&mut self, rvalue: Rvalue, span: gossamer_lex::Span) -> Rvalue {
        let mut rvalue = rvalue;
        let mut mentions: Vec<Local> = Vec::new();
        rvalue_operands(&rvalue, &mut |op| {
            if let Operand::Copy(place) = op
                && place.projection.is_empty()
            {
                mentions.push(place.local);
            }
        });
        let mut boxed: HashMap<Local, Local> = HashMap::new();
        for local in mentions {
            if boxed.contains_key(&local) {
                continue;
            }
            if let Some(f) = self.flat_of(local) {
                let b = self.boxed(local, f, span);
                boxed.insert(local, b);
            }
        }
        if !boxed.is_empty() {
            rename_in_rvalue(&mut rvalue, &boxed);
        }
        rvalue
    }

    fn rewrite_terminator(&mut self, terminator: Terminator, span: gossamer_lex::Span) {
        let Terminator::Call {
            callee,
            mut args,
            destination,
            target,
        } = terminator
        else {
            let terminator = self.box_switch_operand(terminator, span);
            self.terminate(terminator);
            return;
        };
        // A discriminant test reached as a call is a comparison.
        if let Operand::Const(ConstValue::Str(name)) = &callee
            && matches!(
                name.as_str(),
                "gos_rt_result_is_ok"
                    | "gos_rt_result_is_err"
                    | "gos_rt_option_is_some"
                    | "gos_rt_option_is_none"
            )
            && let Some((local, _)) = args.first().and_then(|a| self.flat_operand(a))
            && let Some(target) = target
        {
            let op = if matches!(
                name.as_str(),
                "gos_rt_result_is_ok" | "gos_rt_option_is_some"
            ) {
                BinOp::Eq
            } else {
                BinOp::Ne
            };
            self.assign(
                destination,
                Rvalue::BinaryOp {
                    op,
                    lhs: Operand::Copy(field(local, 0)),
                    rhs: Operand::Const(ConstValue::Int(0)),
                },
                span,
            );
            self.terminate(Terminator::Goto { target });
            return;
        }
        let callee_flat = self.abi.callee_is_flat(&callee);
        for arg in &mut args {
            if let Some((local, f)) = self.flat_operand(arg) {
                if !callee_flat {
                    let b = self.boxed(local, f, span);
                    *arg = Operand::Copy(Place::local(b));
                }
            } else if callee_flat
                && let Some(carrier) = self.operand_ty(arg)
                && let Some(tuple) = self.tcx.flat_carrier_ty(carrier)
            {
                let f = Flat { carrier, tuple };
                let temp = self.local(tuple);
                self.flat[temp.0 as usize] = Some(f);
                self.unbox_into(temp, f, arg.clone(), span);
                *arg = Operand::Copy(Place::local(temp));
            }
        }
        let dest_flat = destination
            .projection
            .is_empty()
            .then(|| self.flat_of(destination.local))
            .flatten();
        let dest_carrier = if dest_flat.is_none() {
            self.place_ty(&destination)
                .filter(|ty| self.tcx.flat_carrier_ty(*ty).is_some())
        } else {
            None
        };
        match (dest_flat, dest_carrier, target) {
            (Some(f), _, Some(target)) if !callee_flat => {
                // The callee answers a boxed carrier, unboxed on the way out.
                let boxed = self.local(f.carrier);
                let after = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee,
                    args,
                    destination: Place::local(boxed),
                    target: Some(after),
                });
                self.cur = after;
                self.unbox_into(
                    destination.local,
                    f,
                    Operand::Copy(Place::local(boxed)),
                    span,
                );
                self.terminate(Terminator::Goto { target });
            }
            (None, Some(carrier), Some(target)) if callee_flat => {
                // The callee answers flat into a slot that holds the boxed form.
                let tuple = self.tcx.flat_carrier_ty(carrier).expect("checked above");
                let f = Flat { carrier, tuple };
                let temp = self.local(tuple);
                self.flat[temp.0 as usize] = Some(f);
                let after = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee,
                    args,
                    destination: Place::local(temp),
                    target: Some(after),
                });
                self.cur = after;
                let boxed = self.boxed(temp, f, span);
                self.assign(
                    destination,
                    Rvalue::Use(Operand::Copy(Place::local(boxed))),
                    span,
                );
                self.terminate(Terminator::Goto { target });
            }
            _ => self.terminate(Terminator::Call {
                callee,
                args,
                destination,
                target,
            }),
        }
    }

    /// A terminator other than a call, with any flattened carrier it reads
    /// whole boxed first.
    fn box_switch_operand(
        &mut self,
        terminator: Terminator,
        span: gossamer_lex::Span,
    ) -> Terminator {
        match terminator {
            Terminator::SwitchInt {
                discriminant,
                arms,
                default,
            } => {
                let discriminant = match self.flat_operand(&discriminant) {
                    Some((local, f)) => Operand::Copy(Place::local(self.boxed(local, f, span))),
                    None => discriminant,
                };
                Terminator::SwitchInt {
                    discriminant,
                    arms,
                    default,
                }
            }
            other => other,
        }
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty> {
        match operand {
            Operand::Copy(place) => self.place_ty(place),
            _ => None,
        }
    }

    /// The type `place` holds, through field and dereference steps.
    fn place_ty(&self, place: &Place) -> Option<Ty> {
        let mut ty = self.body.locals.get(place.local.0 as usize)?.ty;
        for step in &place.projection {
            ty = match step {
                Projection::Deref => match self.tcx.kind_of(ty) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => return None,
                },
                Projection::Field(index) => field_ty_at(self.tcx, ty, &[*index])?,
                Projection::Index(_) => match self.tcx.kind_of(ty) {
                    TyKind::Array { elem, .. } | TyKind::Vec(elem) | TyKind::Slice(elem) => *elem,
                    _ => return None,
                },
                Projection::Downcast(_) | Projection::Discriminant => return None,
            };
        }
        Some(ty)
    }
}

fn field(local: Local, index: u32) -> Place {
    Place {
        local,
        projection: vec![Projection::Field(index)],
    }
}

/// The type at `path` inside the aggregate `ty`.
fn field_ty_at(tcx: &TyCtxt, ty: Ty, path: &[u32]) -> Option<Ty> {
    let mut ty = ty;
    for &index in path {
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        ty = match tcx.kind_of(ty) {
            TyKind::Adt { def, substs } => *tcx.adt_field_tys(*def, substs)?.get(index as usize)?,
            TyKind::Tuple(elems) => *elems.get(index as usize)?,
            TyKind::Array { elem, len } if (index as usize) < len.to_usize() => *elem,
            _ => return None,
        };
    }
    Some(ty)
}

fn rename_place(place: &mut Place, map: &HashMap<Local, Local>) {
    if let Some(to) = map.get(&place.local) {
        place.local = *to;
    }
}

fn rename_operand(op: &mut Operand, map: &HashMap<Local, Local>) {
    if let Operand::Copy(place) = op {
        rename_place(place, map);
    }
}

fn rename_in_rvalue(rvalue: &mut Rvalue, map: &HashMap<Local, Local>) {
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. } => {
            rename_operand(op, map);
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            rename_operand(lhs, map);
            rename_operand(rhs, map);
        }
        Rvalue::Aggregate { operands, .. } => {
            for op in operands {
                rename_operand(op, map);
            }
        }
        Rvalue::CallIntrinsic { args, .. } => {
            for op in args {
                rename_operand(op, map);
            }
        }
        Rvalue::Repeat { value, .. } => rename_operand(value, map),
        Rvalue::Ref { place, .. } | Rvalue::Len(place) => rename_place(place, map),
        Rvalue::StaticLoad(_) => {}
    }
}

fn rename_in_stmt(stmt: &mut Statement, map: &HashMap<Local, Local>) {
    match &mut stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            rename_place(place, map);
            rename_in_rvalue(rvalue, map);
        }
        StatementKind::SetDiscriminant { place, .. } => rename_place(place, map),
        StatementKind::StaticStore { value, .. } => rename_operand(value, map),
        StatementKind::IterSource { dst, source, .. } => {
            rename_place(dst, map);
            rename_operand(source, map);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            rename_place(dst, map);
            rename_place(upstream, map);
            if let Some(arg) = closure_or_arg {
                rename_operand(arg, map);
            }
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            rename_place(dst_option, map);
            rename_place(iter_place, map);
        }
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
    }
}

fn rename_in_terminator(terminator: &mut Terminator, map: &HashMap<Local, Local>) {
    match terminator {
        Terminator::SwitchInt { discriminant, .. } => rename_operand(discriminant, map),
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            rename_operand(callee, map);
            for op in args {
                rename_operand(op, map);
            }
            rename_place(destination, map);
        }
        Terminator::Assert { cond, .. } => rename_operand(cond, map),
        Terminator::Drop { place, .. } => rename_place(place, map),
        _ => {}
    }
}
