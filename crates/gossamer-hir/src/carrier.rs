//! Lowers the `Option` and `Result` combinators that only test or fold a
//! payload onto a `match` over the receiver.
//!
//! `opt.is_some_and(f)`, `opt.is_none_or(f)`, `opt.map_or(d, f)`,
//! `opt.map_or_else(df, f)`, `res.is_ok_and(f)`, `res.is_err_and(f)`,
//! `res.map_or(d, f)`, and `res.map_or_else(ef, f)`, and their free forms
//! (`option::is_some_and(opt, f)`, whose data argument reaches this pass
//! last), each become a `match`
//! whose arms call the callback on the payload or answer the fallback. The
//! receiver and the arguments are bound first, in the order the call names
//! them, so every argument is evaluated once whichever arm runs. Every tier
//! then runs the same `match`.

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirLiteral, HirMatchArm, HirPat,
    HirPatKind, HirProgram, HirStmt, HirStmtKind, for_each_child_expr_mut,
};

/// Rewrites every payload-folding combinator call in `program` onto a `match`.
pub(crate) fn desugar_carrier_combinators(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut pass = Desugarer { tcx, ids, temp: 0 };
    for item in &mut program.items {
        pass.visit_item(item);
    }
}

/// Which variant an arm matches and what it answers.
#[derive(Clone, Copy)]
enum Answer {
    /// The callback argument at this index, applied to the payload.
    Apply(usize),
    /// The callback argument at this index, applied to nothing.
    Call(usize),
    /// The argument at this index as it is.
    Value(usize),
    /// A boolean constant.
    Bool(bool),
}

/// A combinator's two arms: the one for `Some` / `Ok`, then `None` / `Err`.
struct Shape {
    present: Answer,
    absent: Answer,
}

/// The arms `method` takes on an `Option` (`option`) or a `Result`, with
/// the number of arguments it is written with.
fn shape(option: bool, method: &str) -> Option<(Shape, usize)> {
    use Answer::{Apply, Bool, Call, Value};
    let (present, absent, arity) = match (option, method) {
        (true, "is_some_and") | (false, "is_ok_and") => (Apply(0), Bool(false), 1),
        (true, "is_none_or") => (Apply(0), Bool(true), 1),
        (false, "is_err_and") => (Bool(false), Apply(0), 1),
        (_, "map_or") => (Apply(1), Value(0), 2),
        (true, "map_or_else") => (Apply(1), Call(0), 2),
        (false, "map_or_else") => (Apply(1), Apply(0), 2),
        _ => return None,
    };
    Some((Shape { present, absent }, arity))
}

struct Desugarer<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    temp: u32,
}

impl Desugarer<'_> {
    fn visit_item(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(f) => self.visit_fn(f),
            HirItemKind::Impl(imp) => {
                for m in &mut imp.methods {
                    self.visit_fn(m);
                }
            }
            HirItemKind::Trait(t) => {
                for m in &mut t.methods {
                    self.visit_fn(m);
                }
            }
            HirItemKind::Const(c) => self.visit_expr(&mut c.value),
            HirItemKind::Static(s) => self.visit_expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }

    fn visit_fn(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            self.visit_block(&mut body.block);
        }
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init, .. } => {
                    if let Some(e) = init {
                        self.visit_expr(e);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.visit_expr(expr),
                HirStmtKind::Item(item) => self.visit_item(item),
            }
        }
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
        }
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.visit_block(block);
        } else {
            for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child));
        }
        if let Some(lowered) = self.lower_combinator(expr) {
            *expr = lowered;
        }
    }

    /// The `match` standing in for `expr`, when it is a payload-folding
    /// combinator call on an `Option` or `Result`.
    fn lower_combinator(&mut self, expr: &HirExpr) -> Option<HirExpr> {
        let (receiver, name, args): (&HirExpr, &str, &[HirExpr]) = match &expr.kind {
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => (receiver, name.name.as_str(), args),
            HirExprKind::Call { callee, args } => {
                let HirExprKind::Path {
                    segments,
                    def: None,
                } = &callee.kind
                else {
                    return None;
                };
                let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
                let (["option" | "result", name] | ["std", "option" | "result", name]) =
                    names.as_slice()
                else {
                    return None;
                };
                let (receiver, rest) = args.split_last()?;
                (receiver, *name, rest)
            }
            _ => return None,
        };
        let TyKind::Adt { def, substs } = self.tcx.kind_of(receiver.ty).clone() else {
            return None;
        };
        let option = match def.local {
            d if d == u32::MAX - 1 => true,
            d if d == u32::MAX => false,
            _ => return None,
        };
        let (shape, arity) = shape(option, name)?;
        if args.len() != arity {
            return None;
        }
        let payloads = substs.types();
        let present_ty = *payloads.first()?;
        let absent_ty = if option {
            None
        } else {
            Some(*payloads.get(1)?)
        };
        let span = expr.span;
        let mut stmts = Vec::new();
        let scrutinee = self.bound(&mut stmts, receiver.clone(), span);
        let bound_args: Vec<HirExpr> = args
            .iter()
            .map(|arg| self.bound(&mut stmts, arg.clone(), span))
            .collect();
        let (present_name, absent_name) = if option {
            ("Some", "None")
        } else {
            ("Ok", "Err")
        };
        let present = self.arm(
            present_name,
            Some(present_ty),
            scrutinee.ty,
            shape.present,
            &bound_args,
            expr.ty,
            span,
        );
        let absent = self.arm(
            absent_name,
            absent_ty,
            scrutinee.ty,
            shape.absent,
            &bound_args,
            expr.ty,
            span,
        );
        let matched = self.expr(
            expr.ty,
            span,
            HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![present, absent],
            },
        );
        Some(self.block(stmts, Some(matched), expr.ty, span))
    }

    /// One arm of the `match`: the variant `variant`, binding its payload of
    /// type `payload` when it carries one, answering `answer`.
    #[allow(clippy::too_many_arguments, reason = "one arm, spelled out")]
    fn arm(
        &mut self,
        variant: &str,
        payload: Option<Ty>,
        scrutinee_ty: Ty,
        answer: Answer,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> HirMatchArm {
        let bound = self.temp_name("x");
        let fields = match payload {
            Some(payload_ty) => {
                let kind = if matches!(answer, Answer::Apply(_)) {
                    HirPatKind::Binding {
                        name: Ident::new(&bound),
                        mutable: false,
                    }
                } else {
                    HirPatKind::Wildcard
                };
                vec![HirPat {
                    id: self.ids.next(),
                    span,
                    ty: payload_ty,
                    kind,
                }]
            }
            None => Vec::new(),
        };
        let pattern = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new(variant),
                fields,
            },
        };
        let body = match answer {
            Answer::Apply(index) => {
                let value = self.path(&bound, payload.unwrap_or(ty), span);
                self.call(&args[index], vec![value], ty, span)
            }
            Answer::Call(index) => self.call(&args[index], Vec::new(), ty, span),
            Answer::Value(index) => self.reclone(&args[index]),
            Answer::Bool(value) => {
                self.expr(ty, span, HirExprKind::Literal(HirLiteral::Bool(value)))
            }
        };
        HirMatchArm {
            pattern,
            guard: None,
            body,
        }
    }

    /// `value` held in a fresh local declared in `stmts`, answered as a read
    /// of it. A named function is answered as written: it names itself.
    fn bound(&mut self, stmts: &mut Vec<HirStmt>, value: HirExpr, span: Span) -> HirExpr {
        if let HirExprKind::Path { segments, def } = &value.kind
            && (def.is_some() || segments.len() > 1)
        {
            return value;
        }
        let name = self.temp_name("v");
        let ty = value.ty;
        let pattern = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(&name),
                mutable: false,
            },
        };
        stmts.push(HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty,
                init: Some(value),
            },
        });
        self.path(&name, ty, span)
    }

    /// `callee(args..)`, typed `ty`.
    fn call(&mut self, callee: &HirExpr, args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let mut callee = self.reclone(callee);
        if let TyKind::FnTrait(sig) = self.tcx.kind_of(callee.ty).clone() {
            callee.ty = self.tcx.intern(TyKind::FnPtr(sig));
        }
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    fn expr(&mut self, ty: Ty, span: Span, kind: HirExprKind) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind,
        }
    }

    fn path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        )
    }

    fn block(&mut self, stmts: Vec<HirStmt>, tail: Option<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let block = HirBlock {
            id: self.ids.next(),
            span,
            stmts,
            tail: tail.map(Box::new),
            ty,
            is_comptime: false,
        };
        self.expr(ty, span, HirExprKind::Block(block))
    }

    fn temp_name(&mut self, tag: &str) -> String {
        let n = self.temp;
        self.temp += 1;
        format!("__carrier_{tag}_{n}")
    }

    fn reclone(&mut self, e: &HirExpr) -> HirExpr {
        let mut c = e.clone();
        c.id = self.ids.next();
        c
    }
}
