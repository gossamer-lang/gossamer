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
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::let_and_return)]
#![allow(clippy::needless_collect)]

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

pub(crate) fn collect_const_values(
    program: &HirProgram,
) -> HashMap<gossamer_resolve::DefId, ConstValue> {
    let mut out = HashMap::new();
    for item in &program.items {
        collect_item_const_values(item, &mut out);
    }
    out
}

/// Declaration initializer for every top-level `const` / non-`mut` `static`
/// item, keyed by `DefId`. A heap-typed initializer (`Vec`, `Map`, `Set`, an
/// aggregate) has no [`ConstValue`] representation, so [`collect_const_values`]
/// never folds it and a reference to the item finds no entry in `consts`.
/// [`Builder::lower_path`] falls back to re-lowering the stored expression
/// here at each reference site (matching how a scalar const is re-materialised
/// as a literal at every use) instead of misreading the item as a function
/// reference. A `static mut` item is excluded: its reads must observe the
/// live shared cell (`mut_statics`), not a fresh copy of the declaration.
pub(crate) fn collect_const_init_exprs(
    program: &HirProgram,
) -> HashMap<gossamer_resolve::DefId, HirExpr> {
    let mut out = HashMap::new();
    for item in &program.items {
        let Some(def) = item.def else { continue };
        let init = match &item.kind {
            HirItemKind::Const(decl) => &decl.value,
            HirItemKind::Static(decl) if !decl.mutable => &decl.value,
            _ => continue,
        };
        out.insert(def, init.clone());
    }
    out
}

fn collect_item_const_values(
    item: &HirItem,
    out: &mut HashMap<gossamer_resolve::DefId, ConstValue>,
) {
    if let Some(def) = item.def {
        let init = match &item.kind {
            HirItemKind::Const(decl) => Some(&decl.value),
            HirItemKind::Static(decl) => Some(&decl.value),
            _ => None,
        };
        if let Some(init) = init {
            collect_expr_const_values(init, out);
            if let Some(value) = const_value_of_expr(init, out) {
                out.insert(def, value);
            }
        }
    }
    match &item.kind {
        HirItemKind::Fn(decl) => {
            if let Some(body) = &decl.body {
                collect_block_const_values(&body.block, out);
            }
        }
        HirItemKind::Impl(decl) => {
            for method in &decl.methods {
                if let Some(body) = &method.body {
                    collect_block_const_values(&body.block, out);
                }
            }
        }
        HirItemKind::Trait(decl) => {
            for method in &decl.methods {
                if let Some(body) = &method.body {
                    collect_block_const_values(&body.block, out);
                }
            }
        }
        HirItemKind::Const(decl) => collect_expr_const_values(&decl.value, out),
        HirItemKind::Static(decl) => collect_expr_const_values(&decl.value, out),
        HirItemKind::Adt(_) => {}
    }
}

fn collect_block_const_values(
    block: &HirBlock,
    out: &mut HashMap<gossamer_resolve::DefId, ConstValue>,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let { init, .. } => {
                if let Some(init) = init {
                    collect_expr_const_values(init, out);
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                collect_expr_const_values(expr, out);
            }
            HirStmtKind::Item(item) => collect_item_const_values(item, out),
        }
    }
    if let Some(tail) = &block.tail {
        collect_expr_const_values(tail, out);
    }
}

fn collect_expr_const_values(
    expr: &HirExpr,
    out: &mut HashMap<gossamer_resolve::DefId, ConstValue>,
) {
    match &expr.kind {
        HirExprKind::Block(block) => collect_block_const_values(block, out),
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_expr_const_values(condition, out);
            collect_expr_const_values(then_branch, out);
            if let Some(else_branch) = else_branch {
                collect_expr_const_values(else_branch, out);
            }
        }
        HirExprKind::Loop { body, .. } | HirExprKind::While { body, .. } => {
            collect_expr_const_values(body, out);
        }
        HirExprKind::Match { scrutinee, arms } => {
            collect_expr_const_values(scrutinee, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_expr_const_values(guard, out);
                }
                collect_expr_const_values(&arm.body, out);
            }
        }
        HirExprKind::Closure { body, .. } => collect_expr_const_values(body, out),
        HirExprKind::Select { arms } => {
            for arm in arms {
                collect_expr_const_values(&arm.body, out);
            }
        }
        HirExprKind::Call { callee, args } => {
            collect_expr_const_values(callee, out);
            for arg in args {
                collect_expr_const_values(arg, out);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            collect_expr_const_values(receiver, out);
            for arg in args {
                collect_expr_const_values(arg, out);
            }
        }
        HirExprKind::Unary { operand, .. } | HirExprKind::Cast { value: operand, .. } => {
            collect_expr_const_values(operand, out);
        }
        HirExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_const_values(lhs, out);
            collect_expr_const_values(rhs, out);
        }
        HirExprKind::Assign { place, value } => {
            collect_expr_const_values(place, out);
            collect_expr_const_values(value, out);
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            collect_expr_const_values(receiver, out);
        }
        HirExprKind::Index { base, index } => {
            collect_expr_const_values(base, out);
            collect_expr_const_values(index, out);
        }
        HirExprKind::Tuple(elems) => {
            for elem in elems {
                collect_expr_const_values(elem, out);
            }
        }
        HirExprKind::Array(array) => match array {
            gossamer_hir::HirArrayExpr::List(elems) => {
                for elem in elems {
                    collect_expr_const_values(elem, out);
                }
            }
            gossamer_hir::HirArrayExpr::Repeat { value, count } => {
                collect_expr_const_values(value, out);
                collect_expr_const_values(count, out);
            }
        },
        HirExprKind::Return(Some(inner))
        | HirExprKind::Break {
            value: Some(inner), ..
        } => collect_expr_const_values(inner, out),
        HirExprKind::Range { start, end, .. } => {
            if let Some(start) = start {
                collect_expr_const_values(start, out);
            }
            if let Some(end) = end {
                collect_expr_const_values(end, out);
            }
        }
        HirExprKind::Path { .. }
        | HirExprKind::Literal(_)
        | HirExprKind::LiftedClosure { .. }
        | HirExprKind::Return(None)
        | HirExprKind::Break { value: None, .. }
        | HirExprKind::Continue { .. }
        | HirExprKind::Placeholder => {}
    }
}

/// Collects the `static mut` items that become real mutable module
/// globals: those whose initializer folds to a scalar (`Int` / `Float` /
/// `Bool` / `Char`). `consts` is the full fold map (from
/// [`collect_const_values`]) so an initializer that references another
/// const resolves. A mutable static with a non-scalar or non-foldable
/// initializer is intentionally omitted - it keeps the inline-the-
/// initial-value fallback through the const map. Known limitation:
/// aggregate `static mut` (Vec / struct / String) therefore has no
/// shared mutable storage - writes are not observable. `static mut`
/// is supported for scalar types; guard a shared aggregate behind a
/// `sync::Mutex` instead.
pub(crate) fn collect_mut_static_defs(
    program: &HirProgram,
    consts: &HashMap<gossamer_resolve::DefId, ConstValue>,
) -> HashMap<gossamer_resolve::DefId, crate::ir::StaticRef> {
    let mut out = HashMap::new();
    for item in &program.items {
        let Some(def) = item.def else { continue };
        let HirItemKind::Static(decl) = &item.kind else {
            continue;
        };
        if !decl.mutable {
            continue;
        }
        // A scalar cell carries its value in the global's own initializer. A
        // cell holding a container or a string names heap storage instead,
        // which only a running program can build: the global starts null and
        // the entry's prologue writes what the declaration says.
        let init = match const_value_of_expr(&decl.value, consts) {
            Some(
                value @ (ConstValue::Int(_)
                | ConstValue::Float(_)
                | ConstValue::Bool(_)
                | ConstValue::Char(_)),
            ) => value,
            _ => ConstValue::Int(0),
        };
        out.insert(
            def,
            crate::ir::StaticRef {
                symbol: format!("gos_static_{}", def.local),
                ty: decl.ty,
                init,
            },
        );
    }
    out
}

pub(crate) fn const_value_of_expr(
    expr: &HirExpr,
    known: &HashMap<gossamer_resolve::DefId, ConstValue>,
) -> Option<ConstValue> {
    match &expr.kind {
        HirExprKind::Literal(lit) => Some(literal_to_const(lit)),
        HirExprKind::Path { def: Some(def), .. } => known.get(def).cloned(),
        HirExprKind::Unary {
            op: HirUnaryOp::Neg,
            operand,
        } => match const_value_of_expr(operand, known)? {
            ConstValue::Int(n) => Some(ConstValue::Int(-n)),
            ConstValue::Float(bits) => {
                let f = f64::from_bits(bits);
                Some(ConstValue::Float((-f).to_bits()))
            }
            _ => None,
        },
        HirExprKind::Binary { op, lhs, rhs } => {
            let l = const_value_of_expr(lhs, known)?;
            let r = const_value_of_expr(rhs, known)?;
            fold_binary_const(*op, &l, &r)
        }
        _ => None,
    }
}

pub(crate) fn fold_binary_const(
    op: gossamer_hir::HirBinaryOp,
    lhs: &ConstValue,
    rhs: &ConstValue,
) -> Option<ConstValue> {
    use gossamer_hir::HirBinaryOp as Op;
    match (lhs, rhs) {
        (ConstValue::Int(a), ConstValue::Int(b)) => match op {
            Op::Add => Some(ConstValue::Int(a.checked_add(*b)?)),
            Op::Sub => Some(ConstValue::Int(a.checked_sub(*b)?)),
            Op::Mul => Some(ConstValue::Int(a.checked_mul(*b)?)),
            Op::Div if *b != 0 => Some(ConstValue::Int(a.checked_div(*b)?)),
            Op::Rem if *b != 0 => Some(ConstValue::Int(a.checked_rem(*b)?)),
            Op::BitAnd => Some(ConstValue::Int(a & b)),
            Op::BitOr => Some(ConstValue::Int(a | b)),
            Op::BitXor => Some(ConstValue::Int(a ^ b)),
            _ => None,
        },
        (ConstValue::Float(a), ConstValue::Float(b)) => {
            let af = f64::from_bits(*a);
            let bf = f64::from_bits(*b);
            let result = match op {
                Op::Add => af + bf,
                Op::Sub => af - bf,
                Op::Mul => af * bf,
                Op::Div => af / bf,
                _ => return None,
            };
            Some(ConstValue::Float(result.to_bits()))
        }
        _ => None,
    }
}

pub(crate) fn collect_impl_methods(program: &HirProgram) -> HashMap<String, Option<Ty>> {
    let mut out: HashMap<String, Option<Ty>> = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Impl(decl) = &item.kind {
            if let Some(prefix) = decl.self_name.as_ref() {
                for method in &decl.methods {
                    let mangled = format!("{}::{}", prefix.name, method.name.name);
                    out.insert(mangled, method.ret);
                }
            }
        }
    }
    out
}

pub(crate) fn collect_impl_method_receivers(program: &HirProgram) -> HashMap<String, Ty> {
    let mut out = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Impl(decl) = &item.kind
            && let Some(prefix) = decl.self_name.as_ref()
        {
            for method in &decl.methods {
                if let Some(receiver) = method.params.first() {
                    out.insert(
                        format!("{}::{}", prefix.name, method.name.name),
                        receiver.ty,
                    );
                }
            }
        }
    }
    out
}

pub(crate) fn collect_impl_method_inputs(program: &HirProgram) -> HashMap<String, Vec<Ty>> {
    let mut out = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Impl(decl) = &item.kind
            && let Some(prefix) = decl.self_name.as_ref()
        {
            for method in &decl.methods {
                out.insert(
                    format!("{}::{}", prefix.name, method.name.name),
                    method.params.iter().map(|param| param.ty).collect(),
                );
            }
        }
    }
    out
}

pub(crate) fn collect_fn_inputs(program: &HirProgram) -> HashMap<gossamer_resolve::DefId, Vec<Ty>> {
    let mut out = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Fn(decl) = &item.kind {
            if let Some(def) = item.def {
                let inputs: Vec<Ty> = decl.params.iter().map(|p| p.ty).collect();
                out.insert(def, inputs);
            }
        }
    }
    out
}

pub(crate) fn collect_fn_returns(
    program: &HirProgram,
    tcx: &mut TyCtxt,
) -> HashMap<gossamer_resolve::DefId, Ty> {
    let mut out = HashMap::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(decl) => {
                if let Some(def) = item.def {
                    if let Some(ret) = decl.ret {
                        // A const-generic array return (`-> [T; N]`) is carried
                        // as a runtime GosVec; record the `Vec<T>` representation
                        // so every consumer (call-site dest typing, return ABI)
                        // agrees with the callee body and never reads the heap
                        // Vec as an inline `[T; N]` aggregate.
                        let ret = const_generic_array_as_vec(tcx, ret).unwrap_or(ret);
                        out.insert(def, ret);
                    }
                }
            }
            HirItemKind::Impl(decl) => {
                for method in &decl.methods {
                    if let Some(ret) = method.ret {
                        // Impl methods' def ids live on the
                        // method's name; use the resolver's id
                        // when available. Fallback to no entry.
                        let _ = method;
                        let _ = ret;
                    }
                }
            }
            HirItemKind::Trait(decl) => {
                let _ = decl;
            }
            _ => {}
        }
    }
    out
}

/// Declared return types keyed by callable name: free functions by
/// bare name, impl methods by their `Struct::method` mangled name.
/// Drives the bare-`http::Response` handler thunk lookup at HTTP
/// handler registration sites.
pub(crate) fn collect_fn_ret_names(program: &HirProgram) -> HashMap<String, Ty> {
    let mut out = HashMap::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(decl) => {
                if let Some(ret) = decl.ret {
                    out.insert(decl.name.name.clone(), ret);
                    // Call sites reference inline-module functions by
                    // their canonical `mod::name` spelling.
                    if !item.module_path.is_empty() {
                        out.insert(
                            format!("{}::{}", item.module_path.join("::"), decl.name.name),
                            ret,
                        );
                    }
                }
            }
            HirItemKind::Impl(decl) => {
                if let Some(prefix) = decl.self_name.as_ref() {
                    for method in &decl.methods {
                        if let Some(ret) = method.ret {
                            out.insert(format!("{}::{}", prefix.name, method.name.name), ret);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// True when `ty` is the bare `http::Response` shape - not wrapped in
/// `Result` / `Option`.
pub(crate) fn is_bare_response_ty(tcx: &TyCtxt, ty: Ty) -> bool {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        // Result / Option sentinels: already packed at the C-ABI.
        TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1 => false,
        // The checker pins `http::Response` annotations to the
        // sentinel Response Adt (`u32::MAX - 5`), which renders as a
        // bare `adt#…` placeholder - match the def id directly.
        TyKind::Adt { def, .. } if def.local == u32::MAX - 5 => true,
        // Fallback for paths the checker kept in printable form
        // (e.g. the JsonValue stdlib default). Mirrors the rendered
        // last-segment probe `lower_fn` uses for parameter kinds.
        // This arm depends on the checker's struct-literal pinning:
        // unpinned `http::Response` annotations only reach here in
        // printable form because `check_struct_literal` pins the
        // sentinel Adt (gossamer-types/src/checker.rs,
        // `register_stdlib_struct_fields` + the path-tail probe in
        // the struct-literal arm); a user-defined type that merely
        // ends in `::Response` also matches and gets wrapped.
        _ => {
            let rendered = gossamer_types::printer::render_ty(tcx, ty);
            rendered.rsplit("::").next().unwrap_or(&rendered) == "Response"
        }
    }
}

/// Symbol name of the synthesized Ok-packing handler thunk for `fn_name`.
pub(crate) fn handler_ok_wrap_name(fn_name: &str) -> String {
    format!("{fn_name}::__ok_wrap")
}

/// Symbol name of the synthesized env-ignoring handler thunk for `fn_name`.
pub(crate) fn handler_env_wrap_name(fn_name: &str) -> String {
    format!("{fn_name}::__env_wrap")
}

/// True when `ty` is what an HTTP handler answers - a bare
/// `http::Response` or a `Result<http::Response, E>`.
fn is_handler_ret_ty(tcx: &TyCtxt, ty: Ty) -> bool {
    use gossamer_types::TyKind;
    if is_bare_response_ty(tcx, ty) {
        return true;
    }
    match tcx.kind_of(ty) {
        TyKind::Adt { def, substs } if def.local == u32::MAX => substs
            .types()
            .first()
            .is_some_and(|ok| is_bare_response_ty(tcx, *ok)),
        _ => false,
    }
}

/// Synthesizes the `(env, request) -> Result<Response, Error>` adapter for a
/// handler declared as a plain `fn(http::Request)`.
///
/// Every handler slot in the HTTP runtime - `http::serve`, a middleware's
/// inner handler - invokes its target through the two-argument
/// `(env, request)` C-ABI a `serve` method and a capturing closure already
/// have. A plain function has no environment, so the thunk supplies one the
/// call discards, letting a single dispatch shape serve every handler form.
fn handler_env_wrap_body(
    tcx: &mut TyCtxt,
    wrapped_name: &str,
    req_ty: Ty,
    ret_ty: Ty,
    span: Span,
) -> Body {
    let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    let decl = |ty: Ty| LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    };
    // [RETURN, env, request] - the wrapped handler's own return type is
    // what the thunk answers, so a bare-Response handler keeps reaching
    // its `::__ok_wrap` and the packed-Result ABI stays uniform.
    let inner_name = if is_bare_response_ty(tcx, ret_ty) {
        handler_ok_wrap_name(wrapped_name)
    } else {
        wrapped_name.to_string()
    };
    let thunk_ret = if is_bare_response_ty(tcx, ret_ty) {
        let e = tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([ret_ty, e]);
        tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    } else {
        ret_ty
    };
    // [RETURN, env, request-word, request-slot].
    //
    // The runtime hands the thunk ONE WORD - the request's address - so
    // the incoming parameter is declared as a word. The wrapped handler's
    // own parameter is an aggregate the caller passes by pointer, so the
    // word is copied into a local of that shape and the call passes it.
    // Declaring the parameter with the handler's type instead makes the
    // backend treat the incoming pointer as the aggregate's storage and
    // read the request's first field as if it were the request.
    let locals = vec![decl(thunk_ret), decl(i64_ty), decl(i64_ty), decl(req_ty)];
    Body {
        name: handler_env_wrap_name(wrapped_name),
        def: None,
        arity: 2,
        locals,
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(Local(3)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::local(Local(2)))),
                    },
                    span,
                }],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(inner_name)),
                    args: vec![Operand::Copy(Place::local(Local(3)))],
                    destination: Place::local(Local::RETURN),
                    target: Some(BlockId(1)),
                },
                span,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: Vec::new(),
                terminator: Terminator::Return,
                span,
            },
        ],
        span,
    }
}

/// Pushes the env-ignoring thunk for a free `fn` whose shape is a handler's:
/// one request parameter answering a `Response` or `Result<Response, E>`.
fn maybe_push_handler_env_wrap(decl: &HirFn, tcx: &mut TyCtxt, span: Span, out: &mut Vec<Body>) {
    let Some(ret) = decl.ret else { return };
    if decl.params.len() != 1 || !is_handler_ret_ty(tcx, ret) {
        return;
    }
    let req_ty = decl.params[0].ty;
    out.push(handler_env_wrap_body(
        tcx,
        &decl.name.name,
        req_ty,
        ret,
        span,
    ));
}

/// Synthesizes the `(args…) -> Result<Response, Error>` adapter body for a
/// handler that declares a bare `http::Response` return. The HTTP runtime
/// invokes every registered handler through the packed-Result i128 C-ABI
/// (`extract_response_into` reads the `gos_rt_result_new` encoding), so a
/// bare-Response handler's pointer return would be misread as a Result
/// discriminant. The thunk calls the user function and packs its return
/// into `Ok`, keeping the handler ABI uniformly Result-shaped.
fn handler_ok_wrap_body(
    tcx: &mut TyCtxt,
    wrapped_name: &str,
    param_tys: &[Ty],
    ret_ty: Ty,
    span: Span,
) -> Body {
    let e = tcx.dyn_error_ty();
    let substs = gossamer_types::Substs::from_types([ret_ty, e]);
    let result_ty = tcx.intern(gossamer_types::TyKind::Adt {
        def: gossamer_resolve::DefId::local(u32::MAX),
        substs,
    });
    let decl = |ty: Ty| LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    };
    let mut locals = vec![decl(result_ty)];
    locals.extend(param_tys.iter().map(|ty| decl(*ty)));
    let resp_local = Local(u32::try_from(locals.len()).expect("local overflow"));
    locals.push(decl(ret_ty));
    let args = (1..=param_tys.len())
        .map(|i| Operand::Copy(Place::local(Local(i as u32))))
        .collect();
    let call_block = BasicBlock {
        id: BlockId(0),
        stmts: Vec::new(),
        terminator: Terminator::Call {
            callee: Operand::Const(ConstValue::Str(wrapped_name.to_string())),
            args,
            destination: Place::local(resp_local),
            target: Some(BlockId(1)),
        },
        span,
    };
    let pack_block = BasicBlock {
        id: BlockId(1),
        stmts: vec![Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local::RETURN),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_result_new",
                    args: vec![
                        Operand::Const(ConstValue::Int(0)),
                        Operand::Copy(Place::local(resp_local)),
                    ],
                },
            },
            span,
        }],
        terminator: Terminator::Return,
        span,
    };
    Body {
        name: handler_ok_wrap_name(wrapped_name),
        def: None,
        arity: u32::try_from(param_tys.len()).expect("arity overflow"),
        locals,
        blocks: vec![call_block, pack_block],
        span,
    }
}

/// Pushes the Ok-packing handler thunk for `decl` when its declared
/// return is a bare `http::Response` and its arity matches a handler
/// shape (`fn(Request)` free fn or `fn(&self, Request)` serve method).
/// A capturing closure reaches this as its lifted `__closure_N(env, req)`
/// item, so it is scanned on the two-argument arity a `serve` method uses,
/// and a capturing closure on a router route as `__closure_N(env, req,
/// params)` on the three-argument one.
fn maybe_push_handler_ok_wrap(
    decl: &HirFn,
    expected_arity: usize,
    tcx: &mut TyCtxt,
    span: Span,
    out: &mut Vec<Body>,
) {
    let Some(ret) = decl.ret else { return };
    if decl.params.len() != expected_arity || !is_bare_response_ty(tcx, ret) {
        return;
    }
    let param_tys: Vec<Ty> = decl.params.iter().map(|p| p.ty).collect();
    out.push(handler_ok_wrap_body(
        tcx,
        &decl.name.name,
        &param_tys,
        ret,
        span,
    ));
}

pub(crate) fn collect_struct_fields(
    program: &HirProgram,
) -> (
    HashMap<String, Vec<String>>,
    HashMap<gossamer_resolve::DefId, String>,
) {
    let mut by_name = HashMap::new();
    let mut by_def = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Adt(adt) = &item.kind {
            if let HirAdtKind::Struct(fields) = &adt.kind {
                by_name.insert(
                    adt.name.name.clone(),
                    fields.iter().map(|f| f.name.clone()).collect(),
                );
                if let Some(def) = item.def {
                    by_def.insert(def, adt.name.name.clone());
                }
            }
        }
    }
    for (name, fields) in stdlib_struct_shapes() {
        by_name
            .entry((*name).to_string())
            .or_insert_with(|| fields.iter().map(|f| (*f).to_string()).collect());
    }
    // Mirror the typechecker's sentinel-DefId minting for stdlib
    // structs (see `gossamer-types::checker::stdlib_struct_layout`).
    // Keeps `Adt { def, .. }`-shaped receivers from stdlib paths
    // (e.g. `&fs::DirInfo`) routable through `struct_defs[def] →
    // struct_name → field-name table`.
    for (name, offset) in stdlib_struct_def_offsets() {
        by_def.insert(
            gossamer_resolve::DefId::local(u32::MAX - offset),
            (*name).to_string(),
        );
    }
    (by_name, by_def)
}

pub(crate) fn stdlib_struct_def_offsets() -> &'static [(&'static str, u32)] {
    &[
        ("DirInfo", 2),
        ("Output", 3),
        ("ResponseStream", 4),
        ("Response", 5),
        ("Reverse", 29),
    ]
}

pub(crate) fn stdlib_struct_shapes() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("Reverse", &["0"]),
        ("Output", &["stdout", "stderr", "code"]),
        ("ExitStatus", &["code"]),
        (
            "DirEntry",
            &["path", "name", "is_dir", "is_file", "is_symlink"],
        ),
        // `fs::list_dir` returns these - same field order as the
        // interp builtin's `Value::struct_("DirInfo", ...)` and
        // the runtime's `gos_rt_fs_list_dir` blob layout.
        (
            "DirInfo",
            &[
                "name",
                "path",
                "is_file",
                "is_dir",
                "is_symlink",
                "size",
                "modified_ms",
            ],
        ),
        (
            "Civil",
            &[
                "year",
                "month",
                "day",
                "hour",
                "minute",
                "second",
                "offset_seconds",
                "weekday",
            ],
        ),
        ("TestResult", &["name", "passed", "failure_message"]),
        ("Headers", &["pairs"]),
        ("StatusCode", &["code"]),
        ("FetchOptions", &["offline"]),
        ("IoError", &["kind", "message", "context"]),
        // `http::stream` returns these - same field order as the
        // interp's `builtin_http_stream` and the runtime's
        // `gos_rt_http_stream` blob layout.
        ("ResponseStream", &["__handle", "status", "content_type"]),
        // `http::get` / `http::post` return shape - fields are
        // accessed via per-name `gos_rt_http_response_*` helpers,
        // not flat-blob indexing, so adding `raw_bytes` here is
        // purely for source-level field-name lookup.
        (
            "Response",
            &[
                "status",
                "body",
                "raw_bytes",
                "content_type",
                "location",
                "headers",
            ],
        ),
    ]
}

pub(crate) fn collect_enum_variants(program: &HirProgram) -> EnumIndex {
    let mut by_enum: HashMap<String, Vec<String>> = HashMap::new();
    let mut variant_index: HashMap<String, (String, usize)> = HashMap::new();
    let mut variant_fields: HashMap<String, Vec<String>> = HashMap::new();
    let mut variant_field_tys: HashMap<String, Vec<Ty>> = HashMap::new();
    let mut variant_field_tys_owned: HashMap<String, Vec<Ty>> = HashMap::new();
    let mut variant_has_payload: HashMap<String, bool> = HashMap::new();
    let mut enum_any_payload: HashMap<String, bool> = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Adt(adt) = &item.kind {
            if let HirAdtKind::Enum(variants) = &adt.kind {
                let names: Vec<String> = variants.iter().map(|v| v.name.name.clone()).collect();
                for (idx, vname) in names.iter().enumerate() {
                    variant_index.insert(vname.clone(), (adt.name.name.clone(), idx));
                    variant_has_payload.entry(vname.clone()).or_insert(false);
                }
                for v in variants {
                    if let Some(fields) = &v.struct_fields {
                        let field_names: Vec<String> =
                            fields.iter().map(|f| f.name.clone()).collect();
                        variant_fields.insert(v.name.name.clone(), field_names);
                    }
                    if let Some(tys) = &v.struct_field_tys {
                        // Both named and tuple payloads carry their field
                        // types here, so the declaration alone decides the
                        // payload question - the body scan below only
                        // supplements constructors of enums declared in
                        // other modules.
                        if !tys.is_empty() {
                            variant_has_payload.insert(v.name.name.clone(), true);
                        }
                        variant_field_tys.insert(v.name.name.clone(), tys.clone());
                        variant_field_tys_owned
                            .insert(format!("{}::{}", adt.name.name, v.name.name), tys.clone());
                    }
                }
                let declared_payload = variants
                    .iter()
                    .any(|v| v.struct_field_tys.as_ref().is_some_and(|t| !t.is_empty()));
                enum_any_payload.insert(adt.name.name.clone(), declared_payload);
                by_enum.insert(adt.name.name.clone(), names);
            }
        }
    }
    // Walk every fn body to discover tuple-payload variants. The
    // HIR strips tuple arity from `HirEnumVariant`, so we infer
    // it by counting args at every variant-constructor call site.
    #[allow(clippy::items_after_statements)]
    pub(crate) fn scan_expr(
        e: &gossamer_hir::HirExpr,
        idx: &HashMap<String, (String, usize)>,
        has_payload: &mut HashMap<String, bool>,
    ) {
        use gossamer_hir::{HirExpr, HirExprKind};
        let recurse = |e: &HirExpr, hp: &mut HashMap<String, bool>| {
            scan_expr(e, idx, hp);
        };
        match &e.kind {
            HirExprKind::Call { callee, args } => {
                recurse(callee, has_payload);
                for a in args {
                    recurse(a, has_payload);
                }
                if let HirExprKind::Path { segments, .. } = &callee.kind {
                    let last = segments.last().map(|s| s.name.as_str());
                    if let Some(name) = last {
                        if idx.contains_key(name) && !args.is_empty() {
                            has_payload.insert(name.to_string(), true);
                        }
                    }
                }
            }
            HirExprKind::Block(b) => {
                for s in &b.stmts {
                    if let gossamer_hir::HirStmtKind::Let { init: Some(i), .. } = &s.kind {
                        recurse(i, has_payload);
                    }
                    if let gossamer_hir::HirStmtKind::Expr { expr, .. } = &s.kind {
                        recurse(expr, has_payload);
                    }
                }
                if let Some(t) = &b.tail {
                    recurse(t, has_payload);
                }
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                recurse(condition, has_payload);
                recurse(then_branch, has_payload);
                if let Some(e) = else_branch {
                    recurse(e, has_payload);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                recurse(scrutinee, has_payload);
                for a in arms {
                    recurse(&a.body, has_payload);
                }
            }
            HirExprKind::Loop { body, .. } => recurse(body, has_payload),
            HirExprKind::While {
                condition, body, ..
            } => {
                recurse(condition, has_payload);
                recurse(body, has_payload);
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                recurse(lhs, has_payload);
                recurse(rhs, has_payload);
            }
            HirExprKind::Unary { operand, .. } => recurse(operand, has_payload),
            HirExprKind::Assign { place, value } => {
                recurse(place, has_payload);
                recurse(value, has_payload);
            }
            HirExprKind::MethodCall { receiver, args, .. } => {
                recurse(receiver, has_payload);
                for a in args {
                    recurse(a, has_payload);
                }
            }
            HirExprKind::Field { receiver, .. } => recurse(receiver, has_payload),
            HirExprKind::Index { base, index } => {
                recurse(base, has_payload);
                recurse(index, has_payload);
            }
            HirExprKind::TupleIndex { receiver, .. } => recurse(receiver, has_payload),
            HirExprKind::Tuple(elems) => {
                for e in elems {
                    recurse(e, has_payload);
                }
            }
            HirExprKind::Array(arr) => {
                use gossamer_hir::HirArrayExpr;
                match arr {
                    HirArrayExpr::List(elems) => {
                        for e in elems {
                            recurse(e, has_payload);
                        }
                    }
                    HirArrayExpr::Repeat { value, count } => {
                        recurse(value, has_payload);
                        recurse(count, has_payload);
                    }
                }
            }
            HirExprKind::Cast { value, .. } => recurse(value, has_payload),
            HirExprKind::Return(Some(v)) | HirExprKind::Break { value: Some(v), .. } => {
                recurse(v, has_payload)
            }
            _ => {}
        }
    }
    #[allow(
        clippy::items_after_statements,
        reason = "the nested helper belongs beside the walk that uses it"
    )]
    pub(crate) fn scan_block(
        b: &gossamer_hir::HirBlock,
        idx: &HashMap<String, (String, usize)>,
        hp: &mut HashMap<String, bool>,
    ) {
        for s in &b.stmts {
            if let gossamer_hir::HirStmtKind::Let { init: Some(i), .. } = &s.kind {
                scan_expr(i, idx, hp);
            }
            if let gossamer_hir::HirStmtKind::Expr { expr, .. } = &s.kind {
                scan_expr(expr, idx, hp);
            }
        }
        if let Some(t) = &b.tail {
            scan_expr(t, idx, hp);
        }
    }
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(decl) => {
                if let Some(body) = &decl.body {
                    scan_block(&body.block, &variant_index, &mut variant_has_payload);
                }
            }
            HirItemKind::Impl(impl_decl) => {
                for m in &impl_decl.methods {
                    if let Some(body) = &m.body {
                        scan_block(&body.block, &variant_index, &mut variant_has_payload);
                    }
                }
            }
            _ => {}
        }
    }
    // A variant name may belong to more than one enum, so the per-variant
    // flags cannot answer "does THIS enum carry a payload". The declaration
    // answers it directly; a tuple payload the declaration did not spell out
    // is credited through the variant map, which names one owner per variant.
    // How many enums declare each variant name. A name more than one enum
    // declares cannot be attributed by name alone, so only its declaration
    // decides whether its enum carries a payload.
    let mut declarers: HashMap<&str, usize> = HashMap::new();
    for variants in by_enum.values() {
        for v in variants {
            *declarers.entry(v.as_str()).or_insert(0) += 1;
        }
    }
    for (enum_name, variants) in &by_enum {
        let scanned = variants.iter().any(|v| {
            declarers.get(v.as_str()).copied().unwrap_or(0) == 1
                && variant_has_payload.get(v).copied().unwrap_or(false)
                && variant_index
                    .get(v)
                    .is_some_and(|(owner, _)| owner == enum_name)
        });
        let entry = enum_any_payload.entry(enum_name.clone()).or_insert(false);
        *entry = *entry || scanned;
    }
    EnumIndex {
        by_enum,
        variant_index,
        variant_fields,
        variant_field_tys,
        variant_field_tys_owned,
        variant_has_payload,
        enum_any_payload,
    }
}

pub(crate) fn collect_item(
    item: &HirItem,
    tcx: &mut TyCtxt,
    structs: &HashMap<String, Vec<String>>,
    struct_defs: &HashMap<gossamer_resolve::DefId, String>,
    enums: &EnumIndex,
    impl_methods: &HashMap<String, Option<Ty>>,
    impl_method_receivers: &HashMap<String, Ty>,
    impl_method_inputs: &HashMap<String, Vec<Ty>>,
    fn_ret_names: &HashMap<String, Ty>,
    fn_returns: &HashMap<gossamer_resolve::DefId, Ty>,
    fn_inputs: &HashMap<gossamer_resolve::DefId, Vec<Ty>>,
    fn_param_shareable: &HashMap<gossamer_resolve::DefId, Vec<bool>>,
    consts: &HashMap<gossamer_resolve::DefId, ConstValue>,
    mut_statics: &HashMap<gossamer_resolve::DefId, crate::ir::StaticRef>,
    const_inits: &HashMap<gossamer_resolve::DefId, HirExpr>,
    region_unsafe: &std::collections::HashSet<gossamer_resolve::DefId>,
    out: &mut Vec<Body>,
) {
    match &item.kind {
        HirItemKind::Fn(decl) => {
            // An inline-module function's body carries its canonical
            // `mod::name` symbol (mirroring the `Struct::method`
            // mangle below), so two modules may define the same
            // function name without emitting two identically-named
            // native symbols. Call sites agree: the HIR lowering
            // rewrites every reference to the qualified spelling and
            // `Operand::FnRef` resolution is `DefId`-keyed.
            let mangled: HirFn = if item.module_path.is_empty() {
                decl.clone()
            } else {
                let mut renamed = decl.clone();
                renamed.name = Ident::new(format!(
                    "{}::{}",
                    item.module_path.join("::"),
                    decl.name.name
                ));
                renamed
            };
            if let Some(body) = lower_fn(
                &mangled,
                item.def,
                item.span,
                tcx,
                structs,
                struct_defs,
                enums,
                impl_methods,
                impl_method_receivers,
                impl_method_inputs,
                fn_ret_names,
                fn_returns,
                fn_inputs,
                fn_param_shareable,
                consts,
                mut_statics,
                const_inits,
                region_unsafe,
            ) {
                out.push(body);
                maybe_push_handler_ok_wrap(&mangled, 1, tcx, item.span, out);
                // The other handler shapes are two-word: a router handler
                // `fn(Request, Params)`, and a capturing closure lifted to
                // `__closure_N(env, req)`. A declaration has one arity, so at
                // most one of these fires; a thunk nothing references is
                // dropped with the rest of the unreachable code, while a
                // missing one is an undefined symbol at link time.
                maybe_push_handler_ok_wrap(&mangled, 2, tcx, item.span, out);
                // A capturing closure registered as a router route carries
                // both: it lifts to `__closure_N(env, req, params)`, which is
                // neither of the two-word shapes above.
                maybe_push_handler_ok_wrap(&mangled, 3, tcx, item.span, out);
                maybe_push_handler_env_wrap(&mangled, tcx, item.span, out);
            }
        }
        HirItemKind::Impl(decl) => {
            // Mangle each method name to `Struct::method` so calls
            // from `c.bump()` (where `c: Counter`) can dispatch via
            // a stable name without colliding with another impl's
            // identically-named method on a different struct.
            let prefix = decl.self_name.as_ref().map(|n| n.name.clone());
            for method in &decl.methods {
                let mangled: HirFn = if let Some(p) = prefix.clone() {
                    let mut renamed = method.clone();
                    renamed.name = Ident::new(format!("{}::{}", p, method.name.name));
                    renamed
                } else {
                    method.clone()
                };
                if let Some(body) = lower_fn(
                    &mangled,
                    None,
                    item.span,
                    tcx,
                    structs,
                    struct_defs,
                    enums,
                    impl_methods,
                    impl_method_receivers,
                    impl_method_inputs,
                    fn_ret_names,
                    fn_returns,
                    fn_inputs,
                    fn_param_shareable,
                    consts,
                    mut_statics,
                    const_inits,
                    region_unsafe,
                ) {
                    out.push(body);
                    if method.name.name == "serve" {
                        maybe_push_handler_ok_wrap(&mangled, 2, tcx, item.span, out);
                    }
                }
            }
        }
        HirItemKind::Trait(decl) => {
            for method in &decl.methods {
                if method.body.is_some() {
                    if let Some(body) = lower_fn(
                        method,
                        None,
                        item.span,
                        tcx,
                        structs,
                        struct_defs,
                        enums,
                        impl_methods,
                        impl_method_receivers,
                        impl_method_inputs,
                        fn_ret_names,
                        fn_returns,
                        fn_inputs,
                        fn_param_shareable,
                        consts,
                        mut_statics,
                        const_inits,
                        region_unsafe,
                    ) {
                        out.push(body);
                    }
                }
            }
        }
        HirItemKind::Adt(_) | HirItemKind::Const(_) | HirItemKind::Static(_) => {}
    }
}

/// Method names that grow or destructively reshape a sequence in
/// place. A binding that receives any of these somewhere in the
/// function body genuinely wants a heap `Vec`; one that doesn't can
/// stay a fixed inline `[T; N]` array.
pub(crate) fn lower_fn(
    decl: &HirFn,
    def: Option<gossamer_resolve::DefId>,
    span: Span,
    tcx: &mut TyCtxt,
    structs: &HashMap<String, Vec<String>>,
    struct_defs: &HashMap<gossamer_resolve::DefId, String>,
    enums: &EnumIndex,
    impl_methods: &HashMap<String, Option<Ty>>,
    impl_method_receivers: &HashMap<String, Ty>,
    impl_method_inputs: &HashMap<String, Vec<Ty>>,
    fn_ret_names: &HashMap<String, Ty>,
    fn_returns: &HashMap<gossamer_resolve::DefId, Ty>,
    fn_inputs: &HashMap<gossamer_resolve::DefId, Vec<Ty>>,
    fn_param_shareable: &HashMap<gossamer_resolve::DefId, Vec<bool>>,
    consts: &HashMap<gossamer_resolve::DefId, ConstValue>,
    mut_statics: &HashMap<gossamer_resolve::DefId, crate::ir::StaticRef>,
    const_inits: &HashMap<gossamer_resolve::DefId, HirExpr>,
    region_unsafe: &std::collections::HashSet<gossamer_resolve::DefId>,
) -> Option<Body> {
    let body = decl.body.as_ref()?;
    let mut builder = Builder::new(
        decl.name.name.clone(),
        span,
        tcx,
        structs,
        struct_defs,
        enums,
        impl_methods,
        impl_method_receivers,
        impl_method_inputs,
        fn_ret_names,
        fn_returns,
        fn_inputs,
        fn_param_shareable,
        consts,
        mut_statics,
        const_inits,
        region_unsafe,
    );
    // A const generic array return (`-> [T; N]`) is carried as a runtime
    // GosVec exactly like the `[T; N]` parameter it is derived from, so the
    // return local takes the `Vec<T>` representation. Without this the body
    // returns a Vec pointer through an inline-array return ABI and the caller
    // reads a struct-return slot as a sequence.
    let return_ty = decl.ret.unwrap_or_else(|| builder.tcx.unit());
    let return_ty = const_generic_array_as_vec(builder.tcx, return_ty).unwrap_or(return_ty);
    builder.push_local(return_ty, None, false);
    let arity = u32::try_from(decl.params.len()).expect("arity overflow");
    let mut param_patterns = Vec::with_capacity(decl.params.len());
    for param in &decl.params {
        // A const generic array parameter (`xs: [T; N]`) has a length that is
        // unknown in the generic body, so it is carried as a runtime-length
        // sequence: the body iterates / indexes / measures it through the
        // `Vec` paths, and the caller hands over a GosVec built from the
        // concrete-length argument. This keeps every tier reading the real
        // length instead of a length baked from the symbolic parameter.
        let param_ty = const_generic_array_as_vec(builder.tcx, param.ty).unwrap_or(param.ty);
        let local = builder.push_local(
            param_ty,
            param_name(&param.pattern),
            param_mutable(&param.pattern),
        );
        builder.param_locals.insert(local);
        param_patterns.push((local, &param.pattern));
        if let HirPatKind::Binding { name, .. } = &param.pattern.kind {
            builder.bind_local(&name.name, local);
            // First-priority signal for the runtime-kind tag: the
            // rendered type of the parameter. Stdlib types like
            // `http::Request` resolve to a `TyKind` whose printer
            // form retains the path, even when the type isn't a
            // user-declared struct (so `struct_name_of` below
            // can't pick it up). Match on the last `::`-segment
            // so the fully-qualified `http::Request` and the
            // short-name `Request` both light up.
            let rendered = gossamer_types::printer::render_ty(builder.tcx, param.ty);
            let last_segment = rendered.rsplit("::").next().unwrap_or(&rendered);
            let runtime_kind_from_type: Option<&'static str> = match last_segment {
                "Request" => Some("http::Request"),
                "Response" => Some("http::Response"),
                "Scanner" => Some("bufio::Scanner"),
                "Client" => Some("http::Client"),
                // A server, its router, and a streamed body all reach a
                // goroutine as parameters - `go run(server, app)` - where
                // there is no construction to tag, so the named type is
                // what identifies them.
                "Router" => Some("http::Router"),
                "Server" => Some("http::Server"),
                "ResponseStream" => Some("http::ResponseStream"),
                // A `Set`, a `Deque`, a `Queue`, a `Stack`, and a heap reach
                // codegen as bare handles, so a parameter holding one - the
                // `self` of an `impl` for it, most of all - has no
                // construction to carry the kind and its type is the only
                // record. Without this, `self.iter()` inside
                // `impl Display for Set` dispatched on a handle whose shape
                // nothing named.
                "Set" | "HashSet" => Some("collections::HashSet"),
                "BTreeSet" => Some("collections::BTreeSet"),
                "Deque" | "VecDeque" => Some("collections::VecDeque"),
                "Queue" | "VecQueue" => Some("collections::VecQueue"),
                "Stack" | "VecStack" => Some("collections::VecStack"),
                "MaxHeap" => Some("collections::MaxHeap"),
                "MinHeap" => Some("collections::MinHeap"),
                _ => None,
            };
            // Secondary fallback: parameters named with stdlib-
            // shape-identifying names get the same tag - but only
            // when inference left the type unresolved. A parameter
            // whose type resolved to anything concrete (e.g. a user
            // struct received through a binding named `r`) keeps its
            // real RC semantics, not opaque-handle semantics.
            let ty_unresolved = matches!(
                builder.tcx.kind_of(param.ty),
                gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
            );
            let runtime_kind_from_name: Option<&'static str> = if ty_unresolved {
                match name.name.as_str() {
                    "request" | "req" | "r" | "rq" => Some("http::Request"),
                    "response" | "resp" | "rsp" => Some("http::Response"),
                    "scanner" => Some("bufio::Scanner"),
                    "client" => Some("http::Client"),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(rk) = runtime_kind_from_type.or(runtime_kind_from_name) {
                builder.local_runtime_kind.insert(local, rk);
            }
        }
        // Pre-populate `local_struct` for parameters whose static
        // type resolves to a known named struct so `self.field`
        // (and other `param.field`) accesses inside the body find
        // the struct name without falling through to the
        // unsupported placeholder. The HIR lowerer leaves `self`'s
        // type as Error today, so we also try the impl receiver
        // by inspecting parameter names: a binding called `self`
        // gets the receiver type when `param.ty` doesn't already
        // resolve to one.
        if let Some(struct_name) = builder.struct_name_of(param.ty) {
            // Tag well-known stdlib types via the runtime-kind
            // map so method dispatch on parameters picks the
            // right helper. Maps by struct name; any user struct
            // sharing one of these names overrides this - out
            // of scope for now.
            let runtime_kind: Option<&'static str> = match struct_name.as_str() {
                "Error" => Some("errors::Error"),
                "Response" => Some("http::Response"),
                "Request" => Some("http::Request"),
                "Client" => Some("http::Client"),
                "Scanner" => Some("bufio::Scanner"),
                "Pattern" => Some("regex::Pattern"),
                _ => None,
            };
            builder.local_struct.insert(local, struct_name);
            if let Some(rk) = runtime_kind {
                builder.local_runtime_kind.insert(local, rk);
            }
        }
    }
    let entry = builder.new_block(span);
    builder.set_current(entry);
    for (local, pattern) in param_patterns {
        if !matches!(pattern.kind, HirPatKind::Binding { .. }) {
            builder.bind_aggregate_let_pattern(local, pattern, span);
        }
    }
    // A lifted closure body runs once per element under a sequence
    // combinator, so it owns a region on the same terms a loop body does.
    // The return value is read into RETURN before the pop, and eligibility
    // admits only a Copy tail, so nothing handed back points into the
    // popped region.
    let regioned = matches!(decl.origin, gossamer_hir::FnOrigin::LiftedClosure)
        && builder.begin_closure_body_region(&body.block, span);
    let result_local = builder.lower_block(&body.block);
    if let Some(result) = result_local {
        if builder.current.is_some() {
            let ret_ty = builder.locals[Local::RETURN.0 as usize].ty;
            let result = builder.coerce_return_value(result, ret_ty, span);
            builder.emit_assign(
                Place::local(Local::RETURN),
                Rvalue::Use(Operand::Copy(Place::local(result))),
                span,
            );
        }
    } else if builder.current.is_some() {
        // The body produced no value. A tail that never yields one - a `loop`
        // with no `break`, an early exit - leaves RETURN unwritten by design.
        // A tail whose own type carries the function's result must have
        // lowered to something, so refuse that shape here rather than hand the
        // backends a function whose result slot is never stored.
        let ret_ty = builder.locals[Local::RETURN.0 as usize].ty;
        let tail_yields_value = body.block.tail.as_ref().is_some_and(|tail| {
            !matches!(
                builder.tcx.kind_of(tail.ty),
                gossamer_types::TyKind::Unit
                    | gossamer_types::TyKind::Never
                    | gossamer_types::TyKind::Error
            )
        });
        assert!(
            !tail_yields_value
                || matches!(builder.tcx.kind_of(ret_ty), gossamer_types::TyKind::Unit),
            "MIR lower: `{}` returns a value but its tail expression has no \
             lowering; the expression's type did not reach the builder",
            decl.name.name,
        );
    }
    builder.end_auto_region(regioned, span);
    builder.terminate(Terminator::Return);
    let mut body = Body {
        name: decl.name.name.clone(),
        def,
        arity,
        locals: builder.locals,
        blocks: builder.blocks,
        span,
    };
    crate::lower::carrier_ref::deref_carrier_reads(&mut body, builder.tcx);
    crate::lower::carrier_ref::split_carrier_writebacks(&mut body, builder.tcx);
    Some(body)
}

/// Returns `Vec<T>` for a const generic array parameter `[T; N]`
/// (peeling any leading reference), or `None` for every other type.
/// The body then treats the parameter as a runtime-length sequence.
pub(crate) fn const_generic_array_as_vec(tcx: &mut TyCtxt, ty: Ty) -> Option<Ty> {
    use gossamer_types::{ArrayLen, TyKind};
    let mut peeled = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
        peeled = *inner;
    }
    if let TyKind::Array {
        elem,
        len: ArrayLen::Param(_),
    } = tcx.kind_of(peeled)
    {
        let elem = *elem;
        return Some(tcx.intern(TyKind::Vec(elem)));
    }
    None
}

pub(crate) fn param_name(pattern: &HirPat) -> Option<Ident> {
    match &pattern.kind {
        HirPatKind::Binding { name, .. } => Some(name.clone()),
        _ => None,
    }
}

pub(crate) fn param_mutable(pattern: &HirPat) -> bool {
    matches!(&pattern.kind, HirPatKind::Binding { mutable: true, .. })
}

/// Collects the binding names a pattern introduces, in source order,
/// skipping duplicates. Used to find the consistent variable set shared
/// across an or-pattern's alternatives.
pub(crate) fn collect_pattern_binding_names(pattern: &HirPat, out: &mut Vec<String>) {
    let push = |name: &str, out: &mut Vec<String>| {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    };
    match &pattern.kind {
        HirPatKind::Binding { name, .. } => push(name.name.as_str(), out),
        HirPatKind::At { name, sub, .. } => {
            push(name.name.as_str(), out);
            collect_pattern_binding_names(sub, out);
        }
        HirPatKind::Ref { inner, .. } => collect_pattern_binding_names(inner, out),
        HirPatKind::Tuple(subs) | HirPatKind::Variant { fields: subs, .. } => {
            for sub in subs {
                collect_pattern_binding_names(sub, out);
            }
        }
        HirPatKind::Struct { fields, .. } => {
            for field in fields {
                match &field.pattern {
                    Some(sub) => collect_pattern_binding_names(sub, out),
                    None => push(field.name.name.as_str(), out),
                }
            }
        }
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for sub in prefix {
                collect_pattern_binding_names(sub, out);
            }
            if let Some(rest) = rest {
                collect_pattern_binding_names(rest, out);
            }
            for sub in suffix {
                collect_pattern_binding_names(sub, out);
            }
        }
        HirPatKind::Or(branches) => {
            if let Some(first) = branches.first() {
                collect_pattern_binding_names(first, out);
            }
        }
        HirPatKind::Wildcard
        | HirPatKind::Rest
        | HirPatKind::Literal(_)
        | HirPatKind::Range { .. } => {}
    }
}

pub(crate) fn pattern_kind_label(pattern: &HirPat) -> &'static str {
    match &pattern.kind {
        HirPatKind::Wildcard => "wildcard",
        HirPatKind::Binding { .. } => "binding",
        HirPatKind::Literal(_) => "literal",
        HirPatKind::Tuple(_) => "tuple",
        HirPatKind::Slice { .. } => "slice",
        HirPatKind::Or(_) => "or-pattern",
        HirPatKind::Range { .. } => "range",
        HirPatKind::Struct { .. } => "struct",
        HirPatKind::Variant { .. } => "variant",
        HirPatKind::Ref { .. } => "reference",
        HirPatKind::Rest => "rest",
        HirPatKind::At { .. } => "at-binding",
    }
}
