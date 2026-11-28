//! Types with `impl Drop`: shared handles whose `drop` runs exactly once,
//! when the last reference goes.
//!
//! A drop type `T` keeps each field in a binding of its own, which three
//! closures capture: `__get_N` answers field `N`, `__set_N` replaces it, and
//! `__lend_N` calls an edit with `&mut` of it, so a write changes the field in
//! place rather than a copy. `T` itself becomes `{ __get_0, __set_0,
//! __lend_0, .., __guard }`, so copying a `T`
//! copies its handles and every copy reads and writes the one set of fields.
//! A field has its own binding so that goroutines sharing a `T` touch only the
//! fields they name: reading one field never reads another a goroutine is
//! replacing under a lock. The `runtime::Guard` runs the drop glue: it builds
//! a `T` over the same closures with a guard that does nothing, and calls
//! `drop` on it, so releasing that value cannot run `drop` again. The glue
//! captures only the closures, never `T` or its guard.
//!
//! Every use of `T`'s fields is rewritten: a literal builds the record (a
//! tuple of the declared field types) and calls the type's constructor, a
//! read goes through its field's `__get_N`, a write (an assignment, a method
//! call on a field, `&mut` of a field) runs as an edit `__lend_N` lends the
//! field to, `==` compares records, and a struct pattern matches the record.
//! A write that leaves its expression early (`return`, `break`, `continue`,
//! `?`, or a `defer` inside it) cannot run inside an edit closure, so it reads
//! the field, writes it, and gives it back through `__set_N` instead.

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::{FnSig, Substs, Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    FnOrigin, HirAdtKind, HirBinaryOp, HirBlock, HirBody, HirExpr, HirExprKind, HirFieldPat, HirFn,
    HirItem, HirItemKind, HirLiteral, HirParam, HirPat, HirPatKind, HirProgram, HirStmt,
    HirStmtKind, HirUnaryOp, for_each_child_expr_mut,
};

/// The parts of one drop type the rewrite needs.
#[derive(Clone)]
struct DropType {
    /// The type `T`.
    ty: Ty,
    /// The identity a `T` literal builds values under.
    identity: String,
    /// The declared field names, in order.
    fields: Vec<String>,
    /// The declared field types, in order.
    field_tys: Vec<Ty>,
    /// The record: a tuple of the field types.
    record: Ty,
    /// `Fn() -> F` for each field type `F`, in order.
    get_tys: Vec<Ty>,
    /// `Fn(F)` for each field type `F`, in order.
    set_tys: Vec<Ty>,
    /// `Fn(&mut F)` for each field type `F`, in order: an edit of the field.
    edit_tys: Vec<Ty>,
    /// `Fn(Fn(&mut F))` for each field type `F`, in order.
    lend_tys: Vec<Ty>,
    /// The constructor that wraps a record in a new `T`.
    constructor: String,
}

/// Rewrites every type with an `impl Drop` into a shared handle.
/// The structs an `impl Drop` names, each once, in source order.
fn drop_impl_defs(program: &HirProgram, tcx: &TyCtxt) -> Vec<DefId> {
    let mut drop_defs: Vec<DefId> = Vec::new();
    for item in &program.items {
        if let HirItemKind::Impl(imp) = &item.kind
            && imp
                .trait_name
                .as_ref()
                .is_some_and(|name| name.name == "Drop")
            && let Some(TyKind::Adt { def, substs }) = tcx.kind(imp.self_ty)
            && substs.is_empty()
            && !drop_defs.contains(def)
        {
            drop_defs.push(*def);
        }
    }
    drop_defs
}

pub(crate) fn desugar_drop_types(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
    identities: &HashMap<DefId, String>,
) {
    let drop_defs = drop_impl_defs(program, tcx);
    if drop_defs.is_empty() {
        return;
    }
    let unit = tcx.unit();
    let guard_ty = tcx.intern(TyKind::Adt {
        def: DefId::local(u32::MAX - 49),
        substs: Substs::new(),
    });
    let mut types: HashMap<Ty, DropType> = HashMap::new();
    for item in &mut program.items {
        let HirItemKind::Adt(adt) = &mut item.kind else {
            continue;
        };
        // The item's own `self_ty` is not filled for a struct; its def is.
        let Some(def) = item.def.filter(|def| drop_defs.contains(def)) else {
            continue;
        };
        let self_ty = tcx.intern(TyKind::Adt {
            def,
            substs: Substs::new(),
        });
        let HirAdtKind::Struct(names) = &adt.kind else {
            continue;
        };
        let Some(field_tys) = tcx.struct_field_tys(def).map(<[Ty]>::to_vec) else {
            continue;
        };
        if field_tys.len() != names.len() {
            continue;
        }
        let record = tcx.intern(TyKind::Tuple(field_tys.clone()));
        let AccessorTys {
            get: get_tys,
            set: set_tys,
            edit: edit_tys,
            lend: lend_tys,
        } = accessor_tys(tcx, &field_tys, unit);
        let fields: Vec<String> = names.iter().map(|name| name.name.clone()).collect();
        let mut handle_names = Vec::with_capacity(3 * fields.len() + 1);
        let mut handle_tys = Vec::with_capacity(3 * fields.len() + 1);
        for index in 0..fields.len() {
            handle_names.push(Ident::new(getter_name(index)));
            handle_names.push(Ident::new(setter_name(index)));
            handle_names.push(Ident::new(lender_name(index)));
            handle_tys.push(get_tys[index]);
            handle_tys.push(set_tys[index]);
            handle_tys.push(lend_tys[index]);
        }
        handle_names.push(Ident::new("__guard"));
        handle_tys.push(guard_ty);
        adt.kind = HirAdtKind::Struct(handle_names);
        tcx.register_struct_fields(def, handle_tys);
        tcx.register_drop_type(def);
        let identity = identities
            .get(&def)
            .cloned()
            .unwrap_or_else(|| adt.name.name.clone());
        types.insert(
            self_ty,
            DropType {
                ty: self_ty,
                identity,
                fields,
                field_tys,
                record,
                get_tys,
                set_tys,
                edit_tys,
                lend_tys,
                constructor: format!("__drop_new_{}", def.local),
            },
        );
    }
    if types.is_empty() {
        return;
    }
    let mut pass = DropPass {
        tcx,
        ids,
        types,
        guard_ty,
        unit,
        next: 0,
    };
    for item in &mut program.items {
        match &mut item.kind {
            HirItemKind::Fn(decl) => pass.visit_fn(decl),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|m| pass.visit_fn(m)),
            HirItemKind::Trait(tr) => tr.methods.iter_mut().for_each(|m| pass.visit_fn(m)),
            HirItemKind::Const(c) => pass.visit_expr(&mut c.value),
            HirItemKind::Static(s) => pass.visit_expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }
    let mut drop_types: Vec<DropType> = pass.types.values().cloned().collect();
    drop_types.sort_by(|a, b| a.constructor.cmp(&b.constructor));
    for drop_type in drop_types {
        let item = pass.constructor(&drop_type);
        program.items.push(item);
    }
}

struct DropPass<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    types: HashMap<Ty, DropType>,
    guard_ty: Ty,
    unit: Ty,
    /// Counter naming the bindings the rewrite introduces.
    next: u32,
}

impl DropPass<'_> {
    fn fresh_name(&mut self, stem: &str) -> String {
        let name = format!("__drop_{stem}_{}", self.next);
        self.next += 1;
        name
    }

    /// The drop type `ty` names, through references.
    fn drop_type(&self, ty: Ty) -> Option<DropType> {
        let mut ty = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        self.types.get(&ty).cloned()
    }

    fn visit_fn(&mut self, decl: &mut HirFn) {
        for param in &mut decl.params {
            self.visit_pat(&mut param.pattern);
        }
        if let Some(body) = decl.body.as_mut() {
            self.visit_block(&mut body.block);
        }
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { pattern, ty, init } => {
                    if let Some(init) = init.as_mut() {
                        self.visit_expr(init);
                    }
                    // `let T { a, b } = value` destructures the record.
                    if matches!(pattern.kind, HirPatKind::Struct { .. })
                        && let Some(drop_type) = self.drop_type(pattern.ty)
                        && let Some(value) = init.take()
                    {
                        let span = value.span;
                        *init = Some(self.read_record(value, &drop_type, span));
                        self.record_pattern(pattern, &drop_type);
                        *ty = drop_type.record;
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.visit_expr(expr),
                HirStmtKind::Item(item) => match &mut item.kind {
                    HirItemKind::Fn(decl) => self.visit_fn(decl),
                    HirItemKind::Impl(imp) => {
                        imp.methods.iter_mut().for_each(|m| self.visit_fn(m));
                    }
                    _ => {}
                },
            }
        }
        if let Some(tail) = block.tail.as_mut() {
            self.visit_expr(tail);
        }
    }

    fn visit_pat(&mut self, _pat: &mut HirPat) {}

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        // A write through a field of a drop type reads the record, writes
        // it, and gives it back; the rewrite visits what it wraps.
        if let Some(rewritten) = self.rewrite_write(expr) {
            *expr = rewritten;
            return;
        }
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.visit_block(block);
            return;
        }
        for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child));
        self.rewrite_read(expr);
    }

    /// A read of a field, a literal, an equality, or a match of a drop type,
    /// after its parts have been rewritten.
    fn rewrite_read(&mut self, expr: &mut HirExpr) {
        let span = expr.span;
        match &mut expr.kind {
            HirExprKind::Field { receiver, name } => {
                let Some(drop_type) = self.drop_type(receiver.ty) else {
                    return;
                };
                let Some(index) = drop_type.fields.iter().position(|f| *f == name.name) else {
                    return;
                };
                let receiver = std::mem::replace(receiver.as_mut(), placeholder(span, self.unit));
                expr.kind = self.read_field(receiver, &drop_type, index, span).kind;
            }
            HirExprKind::Call { callee, args } if is_struct_literal(callee) => {
                let Some(drop_type) = self.types.get(&expr.ty).cloned() else {
                    return;
                };
                let args = std::mem::take(args);
                if let Some(built) = self.construct(args, &drop_type, span) {
                    expr.kind = built.kind;
                }
            }
            HirExprKind::Binary {
                op: HirBinaryOp::Eq | HirBinaryOp::Ne,
                lhs,
                rhs,
            } => {
                let Some(drop_type) = self.drop_type(lhs.ty) else {
                    return;
                };
                let left = std::mem::replace(lhs.as_mut(), placeholder(span, self.unit));
                let right = std::mem::replace(rhs.as_mut(), placeholder(span, self.unit));
                **lhs = self.read_record(left, &drop_type, span);
                **rhs = self.read_record(right, &drop_type, span);
            }
            HirExprKind::Match { scrutinee, arms } => {
                let Some(drop_type) = self.drop_type(scrutinee.ty) else {
                    return;
                };
                let structural = arms
                    .iter()
                    .any(|arm| matches!(arm.pattern.kind, HirPatKind::Struct { .. }))
                    && arms.iter().all(|arm| {
                        matches!(
                            arm.pattern.kind,
                            HirPatKind::Struct { .. } | HirPatKind::Wildcard
                        )
                    });
                if !structural {
                    return;
                }
                let held = std::mem::replace(scrutinee.as_mut(), placeholder(span, self.unit));
                **scrutinee = self.read_record(held, &drop_type, span);
                for arm in arms.iter_mut() {
                    self.record_pattern(&mut arm.pattern, &drop_type);
                }
            }
            _ => {}
        }
    }

    /// `(value.__get_N)()`: field `index` of a drop value.
    fn read_field(
        &mut self,
        value: HirExpr,
        drop_type: &DropType,
        index: usize,
        span: Span,
    ) -> HirExpr {
        let getter = HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.get_tys[index],
            kind: HirExprKind::Field {
                receiver: Box::new(value),
                name: Ident::new(getter_name(index)),
            },
        };
        HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.field_tys[index],
            kind: HirExprKind::Call {
                callee: Box::new(getter),
                args: Vec::new(),
            },
        }
    }

    /// `(value.__set_N)(field)`: replaces field `index` of a drop value.
    fn write_field(
        &mut self,
        value: HirExpr,
        field: HirExpr,
        drop_type: &DropType,
        index: usize,
        span: Span,
    ) -> HirExpr {
        let setter = HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.set_tys[index],
            kind: HirExprKind::Field {
                receiver: Box::new(value),
                name: Ident::new(setter_name(index)),
            },
        };
        let unit = self.tcx.unit();
        HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Call {
                callee: Box::new(setter),
                args: vec![field],
            },
        }
    }

    /// The record a drop value holds: a tuple of every field, the value read
    /// once.
    fn read_record(&mut self, value: HirExpr, drop_type: &DropType, span: Span) -> HirExpr {
        let handle_name = self.fresh_name("whole");
        let handle_ty = value.ty;
        let bind = self.let_stmt(&handle_name, handle_ty, value, false, span);
        let elems = (0..drop_type.fields.len())
            .map(|index| {
                let handle = self.path(&handle_name, handle_ty, span);
                self.read_field(handle, drop_type, index, span)
            })
            .collect();
        let record = HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.record,
            kind: HirExprKind::Tuple(elems),
        };
        self.block(vec![bind], Some(record), drop_type.record, span)
    }

    /// A struct pattern on a drop type, as the tuple pattern on its record.
    fn record_pattern(&mut self, pat: &mut HirPat, drop_type: &DropType) {
        let HirPatKind::Struct { fields, .. } = &mut pat.kind else {
            pat.ty = drop_type.record;
            return;
        };
        let mut elems = Vec::with_capacity(drop_type.fields.len());
        for (name, ty) in drop_type.fields.iter().zip(&drop_type.field_tys) {
            let given = fields.iter_mut().find(|field| field.name.name == *name);
            let elem = match given {
                Some(HirFieldPat {
                    pattern: Some(sub), ..
                }) => std::mem::replace(
                    sub,
                    HirPat {
                        id: self.ids.next(),
                        span: pat.span,
                        ty: *ty,
                        kind: HirPatKind::Wildcard,
                    },
                ),
                Some(HirFieldPat {
                    name: field,
                    pattern: None,
                }) => HirPat {
                    id: self.ids.next(),
                    span: pat.span,
                    ty: *ty,
                    kind: HirPatKind::Binding {
                        name: field.clone(),
                        mutable: false,
                    },
                },
                None => HirPat {
                    id: self.ids.next(),
                    span: pat.span,
                    ty: *ty,
                    kind: HirPatKind::Wildcard,
                },
            };
            elems.push(elem);
        }
        pat.kind = HirPatKind::Tuple(elems);
        pat.ty = drop_type.record;
    }

    /// `__struct("T", "f", v, …)` of a drop type: the constructor called on
    /// the record, its fields in declared order. `None` when the literal
    /// builds the handle itself.
    fn construct(
        &mut self,
        args: Vec<HirExpr>,
        drop_type: &DropType,
        span: Span,
    ) -> Option<HirExpr> {
        let mut given: HashMap<String, HirExpr> = HashMap::new();
        let mut rest = args.into_iter().skip(1);
        while let (Some(name), Some(value)) = (rest.next(), rest.next()) {
            let HirExprKind::Literal(HirLiteral::String(name)) = name.kind else {
                return None;
            };
            if name == getter_name(0) || name == "__guard" {
                return None;
            }
            given.insert(name, value);
        }
        let mut stmts = Vec::new();
        let base = given.remove("__base").map(|base| {
            let name = self.fresh_name("base");
            let record = self.read_record(base, drop_type, span);
            stmts.push(self.let_stmt(&name, drop_type.record, record, false, span));
            name
        });
        let mut elems = Vec::with_capacity(drop_type.fields.len());
        for (index, (name, ty)) in drop_type
            .fields
            .iter()
            .zip(&drop_type.field_tys)
            .enumerate()
        {
            let value = match (given.remove(name), &base) {
                (Some(value), _) => value,
                (None, Some(base)) => {
                    let record = self.path(base, drop_type.record, span);
                    HirExpr {
                        id: self.ids.next(),
                        span,
                        ty: *ty,
                        kind: HirExprKind::TupleIndex {
                            receiver: Box::new(record),
                            index: u32::try_from(index).unwrap_or(0),
                        },
                    }
                }
                (None, None) => return None,
            };
            elems.push(value);
        }
        let record = HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.record,
            kind: HirExprKind::Tuple(elems),
        };
        let callee = self.path(&drop_type.constructor, drop_type.ty, span);
        let call = HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![record],
            },
        };
        if stmts.is_empty() {
            return Some(call);
        }
        Some(self.block(stmts, Some(call), drop_type.ty, span))
    }

    /// The rewrite of `expr` when it writes through a field of a drop type:
    /// an assignment to such a place, a method call on one, or a call that
    /// lends one with `&mut`.
    fn rewrite_write(&mut self, expr: &mut HirExpr) -> Option<HirExpr> {
        let in_place = !leaves_early(expr);
        let place: &mut HirExpr = match &mut expr.kind {
            HirExprKind::Assign { place, .. } => place,
            HirExprKind::MethodCall { receiver, .. } => receiver,
            HirExprKind::Call { args, .. } => {
                args.iter_mut().find_map(|arg| match &mut arg.kind {
                    HirExprKind::Unary {
                        op: HirUnaryOp::RefMut,
                        operand,
                    } if self.written_field(operand).is_some() => Some(operand.as_mut()),
                    _ => None,
                })?
            }
            _ => return None,
        };
        let field = self.written_field(place)?;
        let span = expr.span;
        let (drop_type, index) = field;
        // Take the drop value the field is read from out of the place.
        let handle_slot = written_field_slot(place, &self.types, self.tcx)?;
        let HirExprKind::Field { receiver, .. } = &mut handle_slot.kind else {
            return None;
        };
        let mut handle = std::mem::replace(receiver.as_mut(), placeholder(span, self.unit));
        self.visit_expr(&mut handle);
        let field_name = self.fresh_name("field");
        let mut stmts = Vec::new();
        let handle_name = self.fresh_name("handle");
        let handle_ty = handle.ty;
        stmts.push(self.let_stmt(&handle_name, handle_ty, handle, false, span));
        let field_index = index as usize;
        let field_ty = drop_type.field_tys[field_index];
        if in_place {
            let lent_ty = self.tcx.intern(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Mut,
                inner: field_ty,
            });
            let lent = self.path(&field_name, lent_ty, span);
            handle_slot.kind = HirExprKind::Unary {
                op: HirUnaryOp::Deref,
                operand: Box::new(lent),
            };
            let site = LentField {
                drop_type,
                index: field_index,
                handle_name,
                handle_ty,
                field_name,
            };
            return Some(self.lend_write(expr, &site, stmts, span));
        }
        let handle_path = self.path(&handle_name, handle_ty, span);
        let read = self.read_field(handle_path, &drop_type, field_index, span);
        stmts.push(self.let_stmt(&field_name, field_ty, read, true, span));
        handle_slot.kind = self.path(&field_name, field_ty, span).kind;
        // The rest of the write (its value, its arguments, the place below
        // the record) is rewritten as any other expression is.
        let mut write = std::mem::replace(expr, placeholder(span, self.unit));
        for_each_child_expr_mut(&mut write, &mut |child| self.visit_expr(child));
        let value_ty = write.ty;
        let value_name = self.fresh_name("value");
        let unit = self.tcx.unit();
        let answers = !matches!(self.tcx.kind(value_ty), Some(TyKind::Unit | TyKind::Never));
        if answers {
            stmts.push(self.let_stmt(&value_name, value_ty, write, false, span));
        } else {
            stmts.push(self.expr_stmt(write));
        }
        let handle_path = self.path(&handle_name, handle_ty, span);
        let field_path = self.path(&field_name, field_ty, span);
        let store = self.write_field(handle_path, field_path, &drop_type, field_index, span);
        stmts.push(self.expr_stmt(store));
        let tail = answers.then(|| self.path(&value_name, value_ty, span));
        let ty = if answers { value_ty } else { unit };
        Some(self.block(stmts, tail, ty, span))
    }

    /// `(handle.__lend_N)(|field: &mut F| write)`: `write`, whose place
    /// already names `*field`, run as an edit of the field in place. A value
    /// the write answers leaves the edit through an `Option` binding.
    fn lend_write(
        &mut self,
        expr: &mut HirExpr,
        site: &LentField,
        mut stmts: Vec<HirStmt>,
        span: Span,
    ) -> HirExpr {
        let LentField {
            drop_type,
            index,
            handle_name,
            handle_ty,
            field_name,
        } = site;
        let (index, handle_ty) = (*index, *handle_ty);
        let unit = self.tcx.unit();
        let mut write = std::mem::replace(expr, placeholder(span, self.unit));
        for_each_child_expr_mut(&mut write, &mut |child| self.visit_expr(child));
        let value_ty = write.ty;
        let answers = !matches!(self.tcx.kind(value_ty), Some(TyKind::Unit | TyKind::Never));
        let answer_name = self.fresh_name("answer");
        let answer_ty = self.option_ty(value_ty);
        let edit_body = if answers {
            let none = self.path("None", answer_ty, span);
            stmts.push(self.let_stmt(&answer_name, answer_ty, none, true, span));
            let store = self.store_answer(&answer_name, answer_ty, write, span);
            self.block(vec![store], None, unit, span)
        } else {
            let write = self.expr_stmt(write);
            self.block(vec![write], None, unit, span)
        };
        let lent_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner: drop_type.field_tys[index],
        });
        let field_param = HirParam {
            pattern: HirPat {
                id: self.ids.next(),
                span,
                ty: lent_ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(field_name),
                    mutable: false,
                },
            },
            ty: lent_ty,
            is_comptime: false,
        };
        let edit = self.closure(
            vec![field_param],
            unit,
            edit_body,
            drop_type.edit_tys[index],
            span,
        );
        let handle = self.path(handle_name, handle_ty, span);
        let lender = HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.lend_tys[index],
            kind: HirExprKind::Field {
                receiver: Box::new(handle),
                name: Ident::new(lender_name(index)),
            },
        };
        let lend = HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Call {
                callee: Box::new(lender),
                args: vec![edit],
            },
        };
        stmts.push(self.expr_stmt(lend));
        if !answers {
            return self.block(stmts, None, unit, span);
        }
        let answer = self.path(&answer_name, answer_ty, span);
        let unwrapped = HirExpr {
            id: self.ids.next(),
            span,
            ty: value_ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(answer),
                name: Ident::new("unwrap"),
                args: Vec::new(),
                owner: None,
            },
        };
        self.block(stmts, Some(unwrapped), value_ty, span)
    }

    /// `answer = Some(value)`.
    fn store_answer(
        &mut self,
        answer_name: &str,
        answer_ty: Ty,
        value: HirExpr,
        span: Span,
    ) -> HirStmt {
        let unit = self.tcx.unit();
        let some = self.path("Some", answer_ty, span);
        let wrapped = HirExpr {
            id: self.ids.next(),
            span,
            ty: answer_ty,
            kind: HirExprKind::Call {
                callee: Box::new(some),
                args: vec![value],
            },
        };
        let answer = self.path(answer_name, answer_ty, span);
        let store = HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Assign {
                place: Box::new(answer),
                value: Box::new(wrapped),
            },
        };
        self.expr_stmt(store)
    }

    /// `Option<payload>`.
    fn option_ty(&mut self, payload: Ty) -> Ty {
        let def = DefId::local(u32::MAX - 1);
        self.tcx.register_def_name(def, "Option");
        self.tcx.intern(TyKind::Adt {
            def,
            substs: Substs::from_types([payload]),
        })
    }

    /// The drop type and field index of the field of a drop value a write to
    /// `place` goes through: the one nearest the write.
    fn written_field(&self, place: &HirExpr) -> Option<(DropType, u32)> {
        let mut current = place;
        loop {
            match &current.kind {
                HirExprKind::Field { receiver, name } => {
                    if let Some(drop_type) = self.drop_type(receiver.ty)
                        && let Some(index) = drop_type.fields.iter().position(|f| *f == name.name)
                    {
                        return Some((drop_type, u32::try_from(index).unwrap_or(0)));
                    }
                    current = receiver;
                }
                HirExprKind::TupleIndex { receiver, .. }
                | HirExprKind::Index { base: receiver, .. } => current = receiver,
                HirExprKind::Unary {
                    op: HirUnaryOp::Deref,
                    operand,
                } => current = operand,
                _ => return None,
            }
        }
    }

    fn path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    fn let_stmt(
        &mut self,
        name: &str,
        ty: Ty,
        init: HirExpr,
        mutable: bool,
        span: Span,
    ) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.ids.next(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable,
                    },
                },
                ty,
                init: Some(init),
            },
        }
    }

    fn expr_stmt(&mut self, expr: HirExpr) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span: expr.span,
            kind: HirStmtKind::Expr {
                expr,
                has_semi: true,
            },
        }
    }

    fn block(&mut self, stmts: Vec<HirStmt>, tail: Option<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: tail.map(Box::new),
                ty,
                is_comptime: false,
            }),
        }
    }

    fn closure(
        &mut self,
        params: Vec<HirParam>,
        ret: Ty,
        body: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Closure {
                params,
                ret: Some(ret),
                body: Box::new(body),
            },
        }
    }

    /// `T { __get_0, __set_0, __lend_0, .., __guard }` over the closures
    /// `access` names, in field order.
    fn handle_literal(
        &mut self,
        drop_type: &DropType,
        access: &[Access],
        guard: HirExpr,
        span: Span,
    ) -> HirExpr {
        let string_ty = self.tcx.string_ty();
        let text = |pass: &mut Self, text: &str| HirExpr {
            id: pass.ids.next(),
            span,
            ty: string_ty,
            kind: HirExprKind::Literal(HirLiteral::String(text.to_string())),
        };
        let error = self.tcx.error_ty();
        let callee = self.path("__struct", error, span);
        let mut args = vec![text(self, &drop_type.identity)];
        for (index, closures) in access.iter().enumerate() {
            args.push(text(self, &getter_name(index)));
            args.push(self.path(&closures.get, drop_type.get_tys[index], span));
            args.push(text(self, &setter_name(index)));
            args.push(self.path(&closures.set, drop_type.set_tys[index], span));
            args.push(text(self, &lender_name(index)));
            args.push(self.path(&closures.lend, drop_type.lend_tys[index], span));
        }
        args.push(text(self, "__guard"));
        args.push(guard);
        HirExpr {
            id: self.ids.next(),
            span,
            ty: drop_type.ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        }
    }

    fn guard_new(&mut self, cleanup: HirExpr, span: Span) -> HirExpr {
        let error = self.tcx.error_ty();
        let callee = HirExpr {
            id: self.ids.next(),
            span,
            ty: error,
            kind: HirExprKind::Path {
                segments: vec![
                    Ident::new("runtime"),
                    Ident::new("Guard"),
                    Ident::new("new"),
                ],
                def: None,
            },
        };
        HirExpr {
            id: self.ids.next(),
            span,
            ty: self.guard_ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![cleanup],
            },
        }
    }

    /// `fn __drop_new_N(record) -> T`: a binding per field, the closures
    /// that read and replace it, and the guard whose cleanup runs `drop`.
    fn constructor(&mut self, drop_type: &DropType) -> HirItem {
        let span = Span::default();
        let guard = "__drop_guard";
        let mut stmts = Vec::new();
        let mut access = Vec::with_capacity(drop_type.fields.len());
        for (index, &field_ty) in drop_type.field_tys.iter().enumerate() {
            let cell = format!("__drop_cell_{index}");
            let get = format!("__drop_get_{index}");
            let set = format!("__drop_set_{index}");
            let given = self.path("__drop_record", drop_type.record, span);
            let initial = HirExpr {
                id: self.ids.next(),
                span,
                ty: field_ty,
                kind: HirExprKind::TupleIndex {
                    receiver: Box::new(given),
                    index: u32::try_from(index).unwrap_or(0),
                },
            };
            stmts.push(self.let_stmt(&cell, field_ty, initial, true, span));
            let read_cell = self.path(&cell, field_ty, span);
            let get_ty = drop_type.get_tys[index];
            let getter = self.closure(Vec::new(), field_ty, read_cell, get_ty, span);
            stmts.push(self.let_stmt(&get, get_ty, getter, false, span));
            let setter = self.setter_closure(field_ty, drop_type.set_tys[index], &cell, span);
            stmts.push(self.let_stmt(&set, drop_type.set_tys[index], setter, false, span));
            let lend = format!("__drop_lend_{index}");
            let lender = self.lender_closure(drop_type, index, &cell, span);
            stmts.push(self.let_stmt(&lend, drop_type.lend_tys[index], lender, false, span));
            access.push(Access { get, set, lend });
        }
        let guard_value = self.drop_glue_guard(drop_type, &access, span);
        stmts.push(self.let_stmt(guard, self.guard_ty, guard_value, false, span));
        let handle_guard = self.path(guard, self.guard_ty, span);
        let handle = self.handle_literal(drop_type, &access, handle_guard, span);
        self.constructor_item(drop_type, stmts, handle, span)
    }

    /// `|edit| edit(&mut cell)`: the closure that lends the field in `cell` to
    /// an edit.
    fn lender_closure(
        &mut self,
        drop_type: &DropType,
        index: usize,
        cell: &str,
        span: Span,
    ) -> HirExpr {
        let unit = self.tcx.unit();
        let field_ty = drop_type.field_tys[index];
        let edit_ty = drop_type.edit_tys[index];
        let edit = "__drop_edit";
        let edit_param = HirParam {
            pattern: HirPat {
                id: self.ids.next(),
                span,
                ty: edit_ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(edit),
                    mutable: false,
                },
            },
            ty: edit_ty,
            is_comptime: false,
        };
        let lent_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner: field_ty,
        });
        let cell_place = self.path(cell, field_ty, span);
        let lent = HirExpr {
            id: self.ids.next(),
            span,
            ty: lent_ty,
            kind: HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand: Box::new(cell_place),
            },
        };
        let callee = self.path(edit, edit_ty, span);
        let run_edit = HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![lent],
            },
        };
        let run_edit = self.expr_stmt(run_edit);
        let body = self.block(vec![run_edit], None, unit, span);
        self.closure(
            vec![edit_param],
            unit,
            body,
            drop_type.lend_tys[index],
            span,
        )
    }

    /// The closure that replaces the field in `cell` with its argument.
    fn setter_closure(&mut self, field_ty: Ty, set_ty: Ty, cell: &str, span: Span) -> HirExpr {
        let unit = self.tcx.unit();
        let next = "__drop_next";
        let next_param = HirParam {
            pattern: HirPat {
                id: self.ids.next(),
                span,
                ty: field_ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(next),
                    mutable: false,
                },
            },
            ty: field_ty,
            is_comptime: false,
        };
        let cell_place = self.path(cell, field_ty, span);
        let next_value = self.path(next, field_ty, span);
        let store = HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Assign {
                place: Box::new(cell_place),
                value: Box::new(next_value),
            },
        };
        let store = self.expr_stmt(store);
        let set_body = self.block(vec![store], None, unit, span);
        self.closure(vec![next_param], unit, set_body, set_ty, span)
    }

    /// The guard whose cleanup runs the type's `drop`: a value over the same
    /// fields, through the closures `access` names, whose own guard does
    /// nothing.
    fn drop_glue_guard(&mut self, drop_type: &DropType, access: &[Access], span: Span) -> HirExpr {
        let unit = self.tcx.unit();
        let inert_body = self.block(Vec::new(), None, unit, span);
        let no_cleanup = FnSig {
            inputs: Vec::new(),
            output: unit,
        };
        let cleanup_ty = self.tcx.intern(TyKind::FnTrait(no_cleanup));
        let inert_cleanup = self.closure(Vec::new(), unit, inert_body, cleanup_ty, span);
        let inert_guard = self.guard_new(inert_cleanup, span);
        let last_value = self.handle_literal(drop_type, access, inert_guard, span);
        let last = "__drop_last";
        let bind_last = self.let_stmt(last, drop_type.ty, last_value, true, span);
        let mut_ref = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner: drop_type.ty,
        });
        let receiver = self.path(last, drop_type.ty, span);
        let receiver = HirExpr {
            id: self.ids.next(),
            span,
            ty: mut_ref,
            kind: HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand: Box::new(receiver),
            },
        };
        let callee = self.path(&format!("{}::drop", drop_type.identity), unit, span);
        let run_drop = HirExpr {
            id: self.ids.next(),
            span,
            ty: unit,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![receiver],
            },
        };
        let run_drop = self.expr_stmt(run_drop);
        let glue_body = self.block(vec![bind_last, run_drop], None, unit, span);
        let glue = self.closure(Vec::new(), unit, glue_body, cleanup_ty, span);
        self.guard_new(glue, span)
    }

    /// The constructor function of `drop_type`, taking its record and
    /// answering the handle `stmts` build and `handle` names.
    fn constructor_item(
        &mut self,
        drop_type: &DropType,
        stmts: Vec<HirStmt>,
        handle: HirExpr,
        span: Span,
    ) -> HirItem {
        let block = HirBlock {
            id: self.ids.next(),
            span,
            stmts,
            tail: Some(Box::new(handle)),
            ty: drop_type.ty,
            is_comptime: false,
        };
        HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(HirFn {
                name: Ident::new(&drop_type.constructor),
                params: vec![HirParam {
                    pattern: HirPat {
                        id: self.ids.next(),
                        span,
                        ty: drop_type.record,
                        kind: HirPatKind::Binding {
                            name: Ident::new("__drop_record"),
                            mutable: false,
                        },
                    },
                    ty: drop_type.record,
                    is_comptime: false,
                }],
                ret: Some(drop_type.ty),
                body: Some(HirBody { block }),
                is_unsafe: false,
                is_comptime: false,
                has_self: false,
                origin: FnOrigin::Declared,
                foreign_link: None,
                foreign_symbol: None,
                export_symbol: None,
            }),
        }
    }
}

/// The types of the closures a handle carries for each field, in field order.
struct AccessorTys {
    /// `Fn() -> F`.
    get: Vec<Ty>,
    /// `Fn(F)`.
    set: Vec<Ty>,
    /// `Fn(&mut F)`: an edit of the field.
    edit: Vec<Ty>,
    /// `Fn(Fn(&mut F))`.
    lend: Vec<Ty>,
}

fn accessor_tys(tcx: &mut TyCtxt, field_tys: &[Ty], unit: Ty) -> AccessorTys {
    let mut tys = AccessorTys {
        get: Vec::with_capacity(field_tys.len()),
        set: Vec::with_capacity(field_tys.len()),
        edit: Vec::with_capacity(field_tys.len()),
        lend: Vec::with_capacity(field_tys.len()),
    };
    for &field in field_tys {
        let fn_ty = |tcx: &mut TyCtxt, inputs: Vec<Ty>, output: Ty| {
            tcx.intern(TyKind::FnTrait(FnSig { inputs, output }))
        };
        let lent = tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner: field,
        });
        let edit = fn_ty(tcx, vec![lent], unit);
        tys.get.push(fn_ty(tcx, Vec::new(), field));
        tys.set.push(fn_ty(tcx, vec![field], unit));
        tys.edit.push(edit);
        tys.lend.push(fn_ty(tcx, vec![edit], unit));
    }
    tys
}

/// A write through one field of a drop value, as the edit that runs it sees
/// it: the field, the binding holding the handle, and the parameter naming
/// the lent field.
struct LentField {
    drop_type: DropType,
    index: usize,
    handle_name: String,
    handle_ty: Ty,
    field_name: String,
}

/// The closures of one field a handle carries, by their binding names.
struct Access {
    get: String,
    set: String,
    lend: String,
}

/// The handle field that lends field `index` to an edit.
fn lender_name(index: usize) -> String {
    format!("__lend_{index}")
}

/// Whether `expr` leaves itself other than by finishing: a `return`,
/// `break`, or `continue` (a `?` is a `return` here), or a `defer` that
/// belongs to the block around it. A closure inside it leaves only itself.
fn leaves_early(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Return(_) | HirExprKind::Break { .. } | HirExprKind::Continue { .. } => true,
        HirExprKind::Closure { .. } => false,
        HirExprKind::Block(block)
            if block
                .stmts
                .iter()
                .any(|stmt| matches!(stmt.kind, HirStmtKind::Defer(_))) =>
        {
            true
        }
        _ => {
            let mut leaves = false;
            crate::tree::for_each_child_expr(expr, &mut |child| leaves |= leaves_early(child));
            leaves
        }
    }
}

/// The handle field that reads field `index`.
fn getter_name(index: usize) -> String {
    format!("__get_{index}")
}

/// The handle field that replaces field `index`.
fn setter_name(index: usize) -> String {
    format!("__set_{index}")
}

fn placeholder(span: Span, ty: Ty) -> HirExpr {
    HirExpr {
        id: crate::ids::HirId(u32::MAX),
        span,
        ty,
        kind: HirExprKind::Placeholder,
    }
}

fn is_struct_literal(callee: &HirExpr) -> bool {
    matches!(&callee.kind, HirExprKind::Path { segments, .. }
        if matches!(segments.as_slice(), [only] if only.name == "__struct"))
}

/// The field node, inside `place`, that [`DropPass::written_field`] names.
fn written_field_slot<'p>(
    place: &'p mut HirExpr,
    types: &HashMap<Ty, DropType>,
    tcx: &TyCtxt,
) -> Option<&'p mut HirExpr> {
    let is_drop_field = |expr: &HirExpr| match &expr.kind {
        HirExprKind::Field { receiver, name } => {
            let mut ty = receiver.ty;
            while let Some(TyKind::Ref { inner, .. }) = tcx.kind(ty) {
                ty = *inner;
            }
            types
                .get(&ty)
                .is_some_and(|drop_type| drop_type.fields.contains(&name.name))
        }
        _ => false,
    };
    if is_drop_field(place) {
        return Some(place);
    }
    match &mut place.kind {
        HirExprKind::Field { receiver, .. }
        | HirExprKind::TupleIndex { receiver, .. }
        | HirExprKind::Index { base: receiver, .. }
        | HirExprKind::Unary {
            op: HirUnaryOp::Deref,
            operand: receiver,
        } => written_field_slot(receiver, types, tcx),
        _ => None,
    }
}
