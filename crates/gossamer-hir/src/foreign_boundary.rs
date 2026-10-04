//! Converts values where they cross the C boundary, after type checking.
//!
//! Every tier reaches a foreign function through one dispatcher that knows
//! scalars, slices, and plain-data aggregates. This pass rewrites the forms
//! built on top of those into them, in HIR, so the bytecode VM, the JIT, and
//! native builds all run the same conversion code:
//!
//! - an `ffi::Ptr<T>` argument passes its address, and an `Option<Ptr<T>>`
//!   its address or zero; a result is wrapped back, with a null answered
//!   where the declaration promised `Ptr` raising GX0013;
//! - an out-parameter (`&mut` a scalar or a pointer form) is staged in a
//!   one-element array the call writes, then stored through the reference;
//! - a named function passed as a C function pointer becomes the address of
//!   a C-ABI entry that runs a generated adapter, which decodes the
//!   arguments from words and encodes the result as one;
//! - the generic `ffi` operations (`read`, `write`, `alloc`, `size_of`,
//!   `fn_from_ptr`) and the `ffi::Handle` methods become runtime calls over
//!   the C layout of their type, which is known here and not in their
//!   generic declarations.

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::{ArrayLen, FloatTy, FnSig, IntTy, Mutbl, Ty, TyCtxt, TyKind};

use crate::foreign::{
    CValue, FOREIGN_DISPATCHER, HANDLE_TYPE, PTR_TYPE, aggregate_layout, classify_value,
    handle_value, option_payload, out_param_word, ptr_elem,
};
use crate::ids::HirIdGenerator;
use crate::tree::{
    FnOrigin, HirArrayExpr, HirBinaryOp, HirBlock, HirBody, HirExpr, HirExprKind, HirFn, HirItem,
    HirItemKind, HirLiteral, HirMatchArm, HirParam, HirPat, HirPatKind, HirProgram, HirStmt,
    HirStmtKind, HirUnaryOp,
};

/// Raises GX0013 naming its argument: a null where `Ptr` was promised.
pub const NULL_RESULT: &str = "__gos_ffi_null_result";
/// `__gos_ffi_callback(adapter, signature, name)`: the address of a C-ABI
/// entry for `adapter` with the C signature `signature`.
pub const CALLBACK: &str = "__gos_ffi_callback";
/// `__gos_ffi_handle_pin(cell) -> u64`: registers a handle's cell.
pub const HANDLE_PIN: &str = "__gos_ffi_handle_pin";
/// `__gos_ffi_handle_cell(id) -> Vec<T>`: a live handle's cell.
pub const HANDLE_CELL: &str = "__gos_ffi_handle_cell";
/// `__gos_ffi_handle_store(id, cell)`: replaces a live handle's cell.
pub const HANDLE_STORE: &str = "__gos_ffi_handle_store";
/// `__gos_ffi_handle_release(id)`: drops a live handle.
pub const HANDLE_RELEASE: &str = "__gos_ffi_handle_release";
/// The symbol a routed call names to call through an address: the address
/// is the first argument after the layouts.
pub const INDIRECT_SYMBOL: &str = "@";

/// A function declared in an extern block, as its call sites need it.
struct ForeignDecl {
    params: Vec<Ty>,
    ret: Option<Ty>,
    symbol: String,
}

/// A function a callback argument names.
struct CallbackTarget {
    name: String,
    params: Vec<Ty>,
    ret: Option<Ty>,
}

/// Rewrites every foreign-boundary form in `program` (see the module docs)
/// and appends the callback adapters it generates.
pub(crate) fn desugar_foreign_boundaries(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut foreign = HashMap::new();
    let mut functions = HashMap::new();
    let mut operations = HashMap::new();
    let mut fields = HashMap::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(decl) => {
                let Some(def) = item.def else {
                    continue;
                };
                if decl.origin == FnOrigin::Foreign {
                    foreign.insert(
                        def,
                        ForeignDecl {
                            params: decl.params.iter().map(|p| p.ty).collect(),
                            ret: decl.ret,
                            symbol: decl
                                .foreign_symbol
                                .clone()
                                .unwrap_or_else(|| decl.name.name.clone()),
                        },
                    );
                    continue;
                }
                if let Some(op) = operation_named(&decl.name.name) {
                    operations.insert(def, op);
                }
                functions.insert(
                    def,
                    CallbackTarget {
                        name: decl.name.name.clone(),
                        params: decl.params.iter().map(|p| p.ty).collect(),
                        ret: decl.ret,
                    },
                );
            }
            HirItemKind::Adt(adt) => {
                if let (Some(def), crate::tree::HirAdtKind::Struct(names)) = (item.def, &adt.kind) {
                    fields.insert(def, names.clone());
                }
            }
            _ => {}
        }
    }
    let mut pass = Boundary {
        tcx,
        ids,
        foreign,
        functions,
        operations,
        fields,
        adapters: HashMap::new(),
        generated: Vec::new(),
        temp: 0,
    };
    for item in &mut program.items {
        pass.visit_item(item);
    }
    program.items.append(&mut pass.generated);
}

/// The generic `ffi` operations this pass lowers, by injected name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Read,
    ReadAt,
    Write,
    WriteAt,
    Alloc,
    SizeOf,
    FnFromPtr,
}

fn operation_named(name: &str) -> Option<Operation> {
    Some(match name {
        "__gos_ffi_read" => Operation::Read,
        "__gos_ffi_read_at" => Operation::ReadAt,
        "__gos_ffi_write" => Operation::Write,
        "__gos_ffi_write_at" => Operation::WriteAt,
        "__gos_ffi_alloc" => Operation::Alloc,
        "__gos_ffi_size_of" => Operation::SizeOf,
        "__gos_ffi_fn_from_ptr" => Operation::FnFromPtr,
        _ => return None,
    })
}

struct Boundary<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    foreign: HashMap<DefId, ForeignDecl>,
    functions: HashMap<DefId, CallbackTarget>,
    operations: HashMap<DefId, Operation>,
    fields: HashMap<DefId, Vec<Ident>>,
    /// The adapter generated for each function passed as a callback.
    adapters: HashMap<DefId, String>,
    generated: Vec<HirItem>,
    temp: u32,
}

impl Boundary<'_> {
    fn visit_item(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(f) => self.visit_fn(f),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|f| self.visit_fn(f)),
            HirItemKind::Trait(t) => t.methods.iter_mut().for_each(|f| self.visit_fn(f)),
            HirItemKind::Const(c) => self.visit_expr(&mut c.value),
            HirItemKind::Static(s) => self.visit_expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }

    fn visit_fn(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            for stmt in &mut body.block.stmts {
                self.visit_stmt(stmt);
            }
            if let Some(tail) = &mut body.block.tail {
                self.visit_expr(tail);
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &mut HirStmt) {
        match &mut stmt.kind {
            HirStmtKind::Let { init: Some(e), .. }
            | HirStmtKind::Expr { expr: e, .. }
            | HirStmtKind::Defer(e) => self.visit_expr(e),
            HirStmtKind::Item(item) => self.visit_item(item),
            HirStmtKind::Let { init: None, .. } => {}
        }
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        if let HirExprKind::Block(block) = &mut expr.kind {
            for stmt in &mut block.stmts {
                self.visit_stmt(stmt);
            }
            if let Some(tail) = &mut block.tail {
                self.visit_expr(tail);
            }
            return;
        }
        crate::tree::for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child));
        let replacement = match &expr.kind {
            HirExprKind::Call { callee, args } => self.rewrite_call(expr, callee, args),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => self.rewrite_handle_method(expr, receiver, &name.name, args),
            _ => None,
        };
        if let Some(replacement) = replacement {
            *expr = replacement;
        }
    }

    fn rewrite_call(
        &mut self,
        call: &HirExpr,
        callee: &HirExpr,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let HirExprKind::Path { segments, def } = &callee.kind else {
            return None;
        };
        if let Some(def) = def {
            if self.foreign.contains_key(def) {
                return self.rewrite_foreign_call(call, callee, *def, args);
            }
            if let Some(op) = self.operations.get(def).copied() {
                return self.rewrite_operation(call, callee, op, args);
            }
        }
        let last = segments.last()?.name.as_str();
        if last == "new" && handle_value(self.tcx, call.ty).is_some() && args.len() == 1 {
            return Some(self.handle_new(call, &args[0]));
        }
        None
    }

    // ---- foreign calls ------------------------------------------------

    fn rewrite_foreign_call(
        &mut self,
        call: &HirExpr,
        callee: &HirExpr,
        def: DefId,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let decl = self.foreign.get(&def)?;
        let params = decl.params.clone();
        let ret = decl.ret;
        let symbol = decl.symbol.clone();
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let mut stmts = Vec::new();
        let mut after = Vec::new();
        let mut call_args = Vec::with_capacity(args.len());
        let mut changed = false;
        for (arg, param) in args.iter().zip(&params) {
            if let Some((word, value)) = out_param_word(self.tcx, *param) {
                changed = true;
                let reference = self.fresh("r");
                stmts.push(self.let_stmt(&reference, false, *param, arg.clone(), span));
                let current = {
                    let staged_expr = self.local(&reference, *param, span);
                    self.deref(staged_expr, span)
                };
                let staged_value = self.value_in(current, &value, span);
                let staged_ty = self.array_ty(word, 1);
                let staged = self.fresh("t");
                let initial = self.array(vec![staged_value], staged_ty, span);
                stmts.push(self.let_stmt(&staged, true, staged_ty, initial, span));
                let staged_ref_ty = self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: staged_ty,
                });
                let staged_local = self.local(&staged, staged_ty, span);
                call_args.push(self.unary(HirUnaryOp::RefMut, staged_local, staged_ref_ty, span));
                let element = {
                    let staged_expr = self.local(&staged, staged_ty, span);
                    self.index0(staged_expr, word, span)
                };
                let target_ty = match self.tcx.kind_of(*param) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => *param,
                };
                let back = self.value_out(element, &value, target_ty, &symbol, span);
                let place = {
                    let staged_expr = self.local(&reference, *param, span);
                    self.deref(staged_expr, span)
                };
                after.push(self.assign_stmt(place, back, span));
                continue;
            }
            match classify_value(self.tcx, *param) {
                Some(CValue::Ptr | CValue::OptPtr) => {
                    changed = true;
                    let value = classify_value(self.tcx, *param)?;
                    let converted = self.value_in(arg.clone(), &value, span);
                    let bound = self.fresh("a");
                    stmts.push(self.let_stmt(&bound, false, u64_ty, converted, span));
                    call_args.push(self.local(&bound, u64_ty, span));
                }
                Some(CValue::Callback(sig)) => {
                    changed = true;
                    let address = self.callback_address(arg, &sig, span)?;
                    let bound = self.fresh("a");
                    stmts.push(self.let_stmt(&bound, false, u64_ty, address, span));
                    call_args.push(self.local(&bound, u64_ty, span));
                }
                _ => call_args.push(arg.clone()),
            }
        }
        let ret_value = ret.and_then(|ret| match classify_value(self.tcx, ret)? {
            value @ (CValue::Ptr | CValue::OptPtr) => Some(value),
            _ => None,
        });
        if !changed && ret_value.is_none() {
            return None;
        }
        let raw_ty = if ret_value.is_some() { u64_ty } else { call.ty };
        let inner = HirExpr {
            id: self.ids.next(),
            span,
            ty: raw_ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee.clone()),
                args: call_args,
            },
        };
        if after.is_empty() && ret_value.is_none() {
            return Some(self.block(stmts, Some(inner), call.ty, span));
        }
        let result = self.fresh("ret");
        stmts.push(self.let_stmt(&result, false, raw_ty, inner, span));
        stmts.extend(after);
        let tail = match ret_value {
            Some(value) => {
                let raw = self.local(&result, raw_ty, span);
                self.value_out(raw, &value, call.ty, &symbol, span)
            }
            None => self.local(&result, raw_ty, span),
        };
        Some(self.block(stmts, Some(tail), call.ty, span))
    }

    /// `value` (of the Gossamer type `kind` describes) as the word or
    /// scalar the C call takes.
    fn value_in(&mut self, value: HirExpr, kind: &CValue, span: Span) -> HirExpr {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        match kind {
            CValue::Scalar(_) | CValue::Callback(_) => value,
            CValue::Ptr => self.field(value, "__addr", u64_ty, span),
            CValue::OptPtr => {
                let ptr_ty = option_payload(self.tcx, value.ty).unwrap_or(value.ty);
                let bound = self.fresh("p");
                let some_body = {
                    let local = self.local(&bound, ptr_ty, span);
                    self.field(local, "__addr", u64_ty, span)
                };
                let none_body = self.int_lit(0, u64_ty, span);
                self.match_option(value, &bound, ptr_ty, some_body, none_body, u64_ty, span)
            }
        }
    }

    /// The Gossamer value of type `ty` (described by `kind`) a C word or
    /// scalar `raw` reads back as. A null where `Ptr` was promised raises
    /// GX0013 naming `symbol`.
    fn value_out(
        &mut self,
        raw: HirExpr,
        kind: &CValue,
        ty: Ty,
        symbol: &str,
        span: Span,
    ) -> HirExpr {
        match kind {
            CValue::Scalar(_) | CValue::Callback(_) => raw,
            CValue::Ptr => {
                let ptr_ty = ty;
                let u64_ty = raw.ty;
                let bound = self.fresh("w");
                let bind = self.let_stmt(&bound, false, u64_ty, raw, span);
                let check = self.null_check(&bound, u64_ty, symbol, span);
                let address = self.local(&bound, u64_ty, span);
                let ptr = self.struct_lit(PTR_TYPE, vec![("__addr", address)], ptr_ty, span);
                self.block(vec![bind, check], Some(ptr), ptr_ty, span)
            }
            CValue::OptPtr => {
                let opt_ty = ty;
                let ptr_ty = option_payload(self.tcx, ty).unwrap_or(ty);
                let u64_ty = raw.ty;
                let bound = self.fresh("w");
                let bind = self.let_stmt(&bound, false, u64_ty, raw, span);
                let is_null = {
                    let local = self.local(&bound, u64_ty, span);
                    let zero = self.int_lit(0, u64_ty, span);
                    let bool_ty = self.tcx.bool_ty();
                    self.binary(HirBinaryOp::Eq, local, zero, bool_ty, span)
                };
                let none = self.path("None", opt_ty, span);
                let some = {
                    let address = self.local(&bound, u64_ty, span);
                    let ptr = self.struct_lit(PTR_TYPE, vec![("__addr", address)], ptr_ty, span);
                    self.some(ptr, opt_ty, span)
                };
                let choice = self.if_else(is_null, none, some, opt_ty, span);
                self.block(vec![bind], Some(choice), opt_ty, span)
            }
        }
    }

    /// `if <word> == 0 { __gos_ffi_null_result("<symbol>") }`.
    fn null_check(&mut self, word: &str, u64_ty: Ty, symbol: &str, span: Span) -> HirStmt {
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let local = self.local(word, u64_ty, span);
        let zero = self.int_lit(0, u64_ty, span);
        let is_null = self.binary(HirBinaryOp::Eq, local, zero, bool_ty, span);
        let name = self.string_lit(symbol, span);
        let raise = self.call_named(NULL_RESULT, vec![name], unit, span);
        let then = {
            let staged_expr = self.expr_stmt(raise, span);
            self.block(vec![staged_expr], None, unit, span)
        };
        let check = self.expr(
            unit,
            span,
            HirExprKind::If {
                condition: Box::new(is_null),
                then_branch: Box::new(then),
                else_branch: None,
            },
        );
        self.expr_stmt(check, span)
    }

    // ---- callbacks ----------------------------------------------------

    /// The address of a C-ABI entry for the named function `arg`, whose C
    /// signature is `sig`.
    fn callback_address(&mut self, arg: &HirExpr, sig: &FnSig, span: Span) -> Option<HirExpr> {
        let HirExprKind::Path { def: Some(def), .. } = &arg.kind else {
            return None;
        };
        let target = self.functions.get(def)?;
        let name = target.name.clone();
        let adapter = if let Some(adapter) = self.adapters.get(def) {
            adapter.clone()
        } else {
            let adapter = format!("__gos_ffi_cb_{}", def.local);
            let item = self.adapter(&adapter, arg, *def, span)?;
            self.generated.push(item);
            self.adapters.insert(*def, adapter.clone());
            adapter
        };
        let mut signature = String::new();
        for input in &sig.inputs {
            signature.push(crate::foreign::value_class(self.tcx, *input)?);
        }
        signature.push('>');
        if matches!(self.tcx.kind_of(sig.output), TyKind::Unit) {
            signature.push('v');
        } else {
            signature.push(crate::foreign::value_class(self.tcx, sig.output)?);
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let adapter_sig = FnSig {
            inputs: vec![i64_ty],
            output: i64_ty,
        };
        let adapter_ty = self.tcx.intern(TyKind::FnTrait(adapter_sig));
        let adapter_path = self.path(&adapter, adapter_ty, span);
        let signature = self.string_lit(&signature, span);
        let name = self.string_lit(&name, span);
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        Some(self.call_named(CALLBACK, vec![adapter_path, signature, name], u64_ty, span))
    }

    /// `fn <adapter>(words: i64) -> i64`: decodes the target's arguments
    /// from the words a C-ABI entry stored, calls it, and encodes its
    /// result as a word.
    fn adapter(
        &mut self,
        adapter: &str,
        target_path: &HirExpr,
        def: DefId,
        span: Span,
    ) -> Option<HirItem> {
        let target = self.functions.get(&def)?;
        let (params, ret, target_name) = (target.params.clone(), target.ret, target.name.clone());
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let words = "__ffi_words".to_string();
        let mut stmts = Vec::new();
        let mut call_args = Vec::with_capacity(params.len());
        for (index, param) in params.iter().enumerate() {
            let value = classify_value(self.tcx, *param)?;
            let index = i64::try_from(index).ok()?;
            let decoded =
                self.callback_argument(&words, index, &value, *param, &target_name, span)?;
            let bound = self.fresh("arg");
            stmts.push(self.let_stmt(&bound, false, *param, decoded, span));
            call_args.push(self.local(&bound, *param, span));
        }
        let unit = self.tcx.unit();
        let ret_ty = ret.unwrap_or(unit);
        let call = HirExpr {
            id: self.ids.next(),
            span,
            ty: ret_ty,
            kind: HirExprKind::Call {
                callee: Box::new(target_path.clone()),
                args: call_args,
            },
        };
        let tail = self.callback_result(call, ret_ty, &mut stmts, span)?;
        let body = HirBody {
            block: HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: Some(Box::new(tail)),
                ty: i64_ty,
                is_comptime: false,
            },
        };
        let decl = HirFn {
            name: Ident::new(adapter),
            params: vec![self.param(&words, i64_ty, span)],
            ret: Some(i64_ty),
            body: Some(body),
            is_unsafe: false,
            is_comptime: false,
            has_self: false,
            origin: FnOrigin::Declared,
            foreign_link: None,
            foreign_symbol: None,
        };
        Some(HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(decl),
        })
    }

    // ---- generic ffi operations ---------------------------------------

    fn rewrite_operation(
        &mut self,
        call: &HirExpr,
        callee: &HirExpr,
        op: Operation,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        match op {
            Operation::Read | Operation::ReadAt => {
                let elem = call.ty;
                let (size, layout) = self.layout_of_one(elem)?;
                let mut stmts = Vec::new();
                let address =
                    self.element_address(&mut stmts, args, op == Operation::ReadAt, size, span)?;
                let staged_ty = self.array_ty(elem, 1);
                let zero = self.zero(elem, span)?;
                let initial = self.array(vec![zero], staged_ty, span);
                let staged = self.fresh("out");
                stmts.push(self.let_stmt(&staged, true, staged_ty, initial, span));
                let staged_ref_ty = self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: staged_ty,
                });
                let staged_ref = {
                    let local = self.local(&staged, staged_ty, span);
                    self.unary(HirUnaryOp::RefMut, local, staged_ref_ty, span)
                };
                let len = self.int_lit(i64::from(size), u64_ty, span);
                let copy = self.routed_with_layouts(
                    "gos_rt_ffi_read",
                    "RLL>v",
                    &format!("{layout}||"),
                    vec![staged_ref, address, len],
                    unit,
                    span,
                );
                stmts.push(self.expr_stmt(copy, span));
                let value = {
                    let staged_expr = self.local(&staged, staged_ty, span);
                    self.index0(staged_expr, elem, span)
                };
                Some(self.block(stmts, Some(value), elem, span))
            }
            Operation::Write | Operation::WriteAt => {
                let value = args.last()?;
                let elem = value.ty;
                let (size, layout) = self.layout_of_one(elem)?;
                let mut stmts = Vec::new();
                let address =
                    self.element_address(&mut stmts, args, op == Operation::WriteAt, size, span)?;
                let staged_ty = self.array_ty(elem, 1);
                let staged = self.fresh("in");
                let initial = self.array(vec![value.clone()], staged_ty, span);
                stmts.push(self.let_stmt(&staged, false, staged_ty, initial, span));
                let len = self.int_lit(i64::from(size), u64_ty, span);
                let staged_local = self.local(&staged, staged_ty, span);
                let copy = self.routed_with_layouts(
                    "gos_rt_ffi_write",
                    "LrL>v",
                    &format!("|{layout}|"),
                    vec![address, staged_local, len],
                    unit,
                    span,
                );
                stmts.push(self.expr_stmt(copy, span));
                Some(self.block(stmts, None, unit, span))
            }
            Operation::Alloc => {
                let elem = ptr_elem(self.tcx, call.ty)?;
                let size = self.c_size(elem)?;
                let count = {
                    let count = args.first()?.clone();
                    self.cast(count, u64_ty, span)
                };
                let size_lit = self.int_lit(i64::from(size), u64_ty, span);
                let bytes = self.binary(HirBinaryOp::Mul, count, size_lit, u64_ty, span);
                let raw = self.routed("gos_rt_ffi_alloc", "L>L", vec![bytes], u64_ty, span);
                Some(self.value_out(raw, &CValue::Ptr, call.ty, "ffi::alloc", span))
            }
            Operation::SizeOf => {
                let elem = self.generic_arg(callee, 0)?;
                let size = self.c_size(elem)?;
                Some(self.int_lit(i64::from(size), i64_ty, span))
            }
            Operation::FnFromPtr => self.fn_from_ptr(call, args.first()?, span),
        }
    }

    /// The type argument at `index` of the generic function `callee` calls.
    fn generic_arg(&self, callee: &HirExpr, index: usize) -> Option<Ty> {
        match self.tcx.kind_of(callee.ty) {
            TyKind::FnDef { substs, .. } => substs.types().get(index).copied(),
            _ => None,
        }
    }

    /// Binds the pointer (and index) arguments of a memory operation and
    /// answers the element's address: `p.__addr + i * size`.
    fn element_address(
        &mut self,
        stmts: &mut Vec<HirStmt>,
        args: &[HirExpr],
        indexed: bool,
        size: u32,
        span: Span,
    ) -> Option<HirExpr> {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let pointer = args.first()?.clone();
        let base = self.field(pointer, "__addr", u64_ty, span);
        let bound = self.fresh("addr");
        stmts.push(self.let_stmt(&bound, false, u64_ty, base, span));
        let mut address = self.local(&bound, u64_ty, span);
        if indexed {
            let index = self.cast(args.get(1)?.clone(), u64_ty, span);
            let size = self.int_lit(i64::from(size), u64_ty, span);
            let offset = self.binary(HirBinaryOp::Mul, index, size, u64_ty, span);
            address = self.binary(HirBinaryOp::Add, address, offset, u64_ty, span);
        }
        Some(address)
    }

    /// The C size of `elem` and the layout of a one-element array of it.
    fn layout_of_one(&mut self, elem: Ty) -> Option<(u32, String)> {
        let staged = self.array_ty(elem, 1);
        let size = self.c_size(elem)?;
        Some((size, aggregate_layout(self.tcx, staged)?))
    }

    /// The bytes a `T` occupies in C layout.
    fn c_size(&mut self, elem: Ty) -> Option<u32> {
        let staged = self.array_ty(elem, 1);
        self.tcx.c_leaves(staged).map(|(size, _)| size)
    }

    /// `ffi::fn_from_ptr::<Fn(A..) -> R>(p)`: a closure that calls the
    /// native function at `p` with that C signature.
    fn fn_from_ptr(&mut self, call: &HirExpr, pointer: &HirExpr, span: Span) -> Option<HirExpr> {
        let (TyKind::FnTrait(sig) | TyKind::FnPtr(sig)) = self.tcx.kind_of(call.ty).clone() else {
            return None;
        };
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let target = self.fresh("fp");
        let address = self.field(pointer.clone(), "__addr", u64_ty, span);
        let bind = self.let_stmt(&target, false, u64_ty, address, span);
        let mut params = Vec::with_capacity(sig.inputs.len());
        let mut call_args = vec![self.local(&target, u64_ty, span)];
        let mut signature = String::new();
        for input in &sig.inputs {
            let value = classify_value(self.tcx, *input)?;
            let name = self.fresh("x");
            params.push(self.param(&name, *input, span));
            let local = self.local(&name, *input, span);
            call_args.push(self.value_in(local, &value, span));
            signature.push(crate::foreign::value_class(self.tcx, *input)?);
        }
        signature.push('>');
        let output = sig.output;
        let ret_value = if matches!(self.tcx.kind_of(output), TyKind::Unit) {
            signature.push('v');
            None
        } else {
            signature.push(crate::foreign::value_class(self.tcx, output)?);
            Some(classify_value(self.tcx, output)?)
        };
        let raw_ty = match &ret_value {
            Some(CValue::Ptr | CValue::OptPtr) => u64_ty,
            _ => output,
        };
        let layouts = vec![""; sig.inputs.len() + 1].join("|");
        let raw = self.routed_with_layouts(
            INDIRECT_SYMBOL,
            &signature,
            &layouts,
            call_args,
            raw_ty,
            span,
        );
        let body = match &ret_value {
            Some(value @ (CValue::Ptr | CValue::OptPtr)) => {
                self.value_out(raw, value, output, "ffi::fn_from_ptr", span)
            }
            _ => raw,
        };
        let closure = self.expr(
            call.ty,
            span,
            HirExprKind::Closure {
                params,
                ret: Some(output),
                body: Box::new(body),
            },
        );
        Some(self.block(vec![bind], Some(closure), call.ty, span))
    }

    /// Argument `index` of a callback, decoded from the word a C-ABI entry
    /// stored at `words`.
    fn callback_argument(
        &mut self,
        words: &str,
        index: i64,
        value: &CValue,
        param: Ty,
        target_name: &str,
        span: Span,
    ) -> Option<HirExpr> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let index_lit = self.int_lit(index, u64_ty, span);
        let base = {
            let local = self.local(words, i64_ty, span);
            self.cast(local, u64_ty, span)
        };
        if let CValue::Scalar(class @ ('f' | 'd')) = value {
            let f64_ty = self.tcx.float_ty(FloatTy::F64);
            let wide = self.routed(
                "gos_rt_ffi_word_f64",
                "LL>d",
                vec![base, index_lit],
                f64_ty,
                span,
            );
            return Some(if *class == 'f' {
                self.cast(wide, param, span)
            } else {
                wide
            });
        }
        let word = self.routed(
            "gos_rt_ffi_word",
            "LL>L",
            vec![base, index_lit],
            u64_ty,
            span,
        );
        match value {
            CValue::Scalar('B') => {
                let zero = self.int_lit(0, u64_ty, span);
                let bool_ty = self.tcx.bool_ty();
                Some(self.binary(HirBinaryOp::Ne, word, zero, bool_ty, span))
            }
            CValue::Scalar(_) => Some(self.cast(word, param, span)),
            CValue::Ptr | CValue::OptPtr => Some(self.value_out(
                word,
                value,
                param,
                &format!("{target_name} (callback argument)"),
                span,
            )),
            CValue::Callback(_) => None,
        }
    }

    /// The word a callback adapter answers for the target's result `call`.
    fn callback_result(
        &mut self,
        call: HirExpr,
        ret_ty: Ty,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<HirExpr> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        if matches!(self.tcx.kind_of(ret_ty), TyKind::Unit) {
            stmts.push(self.expr_stmt(call, span));
            return Some(self.int_lit(0, i64_ty, span));
        }
        match classify_value(self.tcx, ret_ty)? {
            CValue::Scalar(class @ ('f' | 'd')) => {
                let wide = if class == 'f' {
                    let f64_ty = self.tcx.float_ty(FloatTy::F64);
                    self.cast(call, f64_ty, span)
                } else {
                    call
                };
                let u64_ty = self.tcx.int_ty(IntTy::U64);
                let bits = self.routed("gos_rt_ffi_f64_bits", "d>L", vec![wide], u64_ty, span);
                Some(self.cast(bits, i64_ty, span))
            }
            CValue::Scalar(_) => Some(self.cast(call, i64_ty, span)),
            value @ (CValue::Ptr | CValue::OptPtr) => {
                let word = self.value_in(call, &value, span);
                Some(self.cast(word, i64_ty, span))
            }
            CValue::Callback(_) => None,
        }
    }

    // ---- handles --------------------------------------------------------

    fn handle_new(&mut self, call: &HirExpr, value: &HirExpr) -> HirExpr {
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let inner = handle_value(self.tcx, call.ty).unwrap_or(value.ty);
        let cell_ty = self.tcx.intern(TyKind::Vec(inner));
        let slot = self.fresh("slot");
        let initial = self.array(vec![value.clone()], cell_ty, span);
        let bind = self.let_stmt(&slot, false, cell_ty, initial, span);
        let slot_local = self.local(&slot, cell_ty, span);
        let id = self.call_named(HANDLE_PIN, vec![slot_local], u64_ty, span);
        let handle = self.struct_lit(HANDLE_TYPE, vec![("__id", id)], call.ty, span);
        self.block(vec![bind], Some(handle), call.ty, span)
    }

    fn rewrite_handle_method(
        &mut self,
        call: &HirExpr,
        receiver: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let inner = handle_value(self.tcx, receiver.ty)?;
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let cell_ty = self.tcx.intern(TyKind::Vec(inner));
        let handle = self.fresh("h");
        let mut stmts = vec![self.let_stmt(&handle, false, receiver.ty, receiver.clone(), span)];
        let id = |this: &mut Self| {
            let local = this.local(&handle, receiver.ty, span);
            this.field(local, "__id", u64_ty, span)
        };
        let read_cell = |this: &mut Self, stmts: &mut Vec<HirStmt>| -> String {
            let id_expr = id(this);
            let fetched = this.call_named(HANDLE_CELL, vec![id_expr], cell_ty, span);
            let slot = this.fresh("slot");
            stmts.push(this.let_stmt(&slot, false, cell_ty, fetched, span));
            slot
        };
        match method {
            "get" if args.is_empty() => {
                let slot = read_cell(self, &mut stmts);
                let value = {
                    let staged_expr = self.local(&slot, cell_ty, span);
                    self.index0(staged_expr, inner, span)
                };
                Some(self.block(stmts, Some(value), inner, span))
            }
            "set" if args.len() == 1 => {
                let fresh = self.array(vec![args[0].clone()], cell_ty, span);
                let id_expr = id(self);
                let store = self.call_named(HANDLE_STORE, vec![id_expr, fresh], unit, span);
                stmts.push(self.expr_stmt(store, span));
                Some(self.block(stmts, None, unit, span))
            }
            "update" if args.len() == 1 => {
                let update = self.fresh("f");
                stmts.push(self.let_stmt(&update, false, args[0].ty, args[0].clone(), span));
                let slot = read_cell(self, &mut stmts);
                let current = {
                    let staged_expr = self.local(&slot, cell_ty, span);
                    self.index0(staged_expr, inner, span)
                };
                let value = self.fresh("v");
                stmts.push(self.let_stmt(&value, true, inner, current, span));
                let ref_ty = self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner,
                });
                let value_ref = {
                    let local = self.local(&value, inner, span);
                    self.unary(HirUnaryOp::RefMut, local, ref_ty, span)
                };
                let apply = HirExpr {
                    id: self.ids.next(),
                    span,
                    ty: unit,
                    kind: HirExprKind::Call {
                        callee: Box::new(self.local(&update, args[0].ty, span)),
                        args: vec![value_ref],
                    },
                };
                stmts.push(self.expr_stmt(apply, span));
                let updated = self.local(&value, inner, span);
                let fresh = self.array(vec![updated], cell_ty, span);
                let id_expr = id(self);
                let store = self.call_named(HANDLE_STORE, vec![id_expr, fresh], unit, span);
                stmts.push(self.expr_stmt(store, span));
                Some(self.block(stmts, None, unit, span))
            }
            "take" if args.is_empty() => {
                let slot = read_cell(self, &mut stmts);
                let id_expr = id(self);
                let release = self.call_named(HANDLE_RELEASE, vec![id_expr], unit, span);
                stmts.push(self.expr_stmt(release, span));
                let value = {
                    let staged_expr = self.local(&slot, cell_ty, span);
                    self.index0(staged_expr, inner, span)
                };
                Some(self.block(stmts, Some(value), inner, span))
            }
            "release" if args.is_empty() => {
                let id_expr = id(self);
                let release = self.call_named(HANDLE_RELEASE, vec![id_expr], unit, span);
                stmts.push(self.expr_stmt(release, span));
                Some(self.block(stmts, None, unit, span))
            }
            _ => None,
        }
    }

    // ---- values ---------------------------------------------------------

    /// A zero value of the plain-data `ty`, to stage a read into.
    fn zero(&mut self, ty: Ty, span: Span) -> Option<HirExpr> {
        let kind = self.tcx.kind_of(ty).clone();
        Some(match kind {
            TyKind::Bool => self.expr(ty, span, HirExprKind::Literal(HirLiteral::Bool(false))),
            TyKind::Int(_) => self.int_lit(0, ty, span),
            TyKind::Float(_) => self.expr(
                ty,
                span,
                HirExprKind::Literal(HirLiteral::Float("0.0".to_string())),
            ),
            TyKind::Array { elem, len } => {
                let value = self.zero(elem, span)?;
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let count = self.int_lit(i64::try_from(len.to_usize()).ok()?, i64_ty, span);
                self.expr(
                    ty,
                    span,
                    HirExprKind::Array(HirArrayExpr::Repeat {
                        value: Box::new(value),
                        count: Box::new(count),
                    }),
                )
            }
            TyKind::Adt { def, substs } => {
                if ptr_elem(self.tcx, ty).is_some() {
                    let u64_ty = self.tcx.int_ty(IntTy::U64);
                    let zero = self.int_lit(0, u64_ty, span);
                    return Some(self.struct_lit(PTR_TYPE, vec![("__addr", zero)], ty, span));
                }
                let name = self.tcx.def_name(def)?.to_string();
                let names = self.fields.get(&def)?.clone();
                let field_tys = self.tcx.adt_field_tys(def, &substs)?.to_vec();
                let mut values = Vec::with_capacity(names.len());
                for (field, field_ty) in names.iter().zip(field_tys) {
                    values.push((field.name.clone(), self.zero(field_ty, span)?));
                }
                let pairs: Vec<(&str, HirExpr)> = values
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.clone()))
                    .collect();
                self.struct_lit(&name, pairs, ty, span)
            }
            _ => return None,
        })
    }

    // ---- builders -------------------------------------------------------

    fn fresh(&mut self, role: &str) -> String {
        self.temp += 1;
        format!("__ffi_{role}{}", self.temp)
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

    fn local(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.path(name, ty, span)
    }

    fn int_lit(&mut self, value: i64, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Int(value.to_string())),
        )
    }

    fn string_lit(&mut self, text: &str, span: Span) -> HirExpr {
        let string = self.tcx.string_ty();
        self.expr(
            string,
            span,
            HirExprKind::Literal(HirLiteral::String(text.to_string())),
        )
    }

    fn field(&mut self, receiver: HirExpr, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Field {
                receiver: Box::new(receiver),
                name: Ident::new(name),
            },
        )
    }

    fn index0(&mut self, base: HirExpr, ty: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let zero = self.int_lit(0, i64_ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Index {
                base: Box::new(base),
                index: Box::new(zero),
            },
        )
    }

    fn cast(&mut self, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        if value.ty == ty {
            return value;
        }
        self.expr(
            ty,
            span,
            HirExprKind::Cast {
                value: Box::new(value),
                ty,
            },
        )
    }

    fn unary(&mut self, op: HirUnaryOp, operand: HirExpr, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
        )
    }

    /// `*reference`, of the referent's type.
    fn deref(&mut self, reference: HirExpr, span: Span) -> HirExpr {
        let inner = match self.tcx.kind_of(reference.ty) {
            TyKind::Ref { inner, .. } => *inner,
            _ => reference.ty,
        };
        self.unary(HirUnaryOp::Deref, reference, inner, span)
    }

    fn binary(
        &mut self,
        op: HirBinaryOp,
        lhs: HirExpr,
        rhs: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        )
    }

    fn array(&mut self, items: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        self.expr(ty, span, HirExprKind::Array(HirArrayExpr::List(items)))
    }

    fn array_ty(&mut self, elem: Ty, len: usize) -> Ty {
        self.tcx.intern(TyKind::Array {
            elem,
            len: ArrayLen::Concrete(len),
        })
    }

    fn call_named(&mut self, name: &str, args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let unit = self.tcx.unit();
        let callee = self.path(name, unit, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    /// A routed call to the runtime export `symbol` with no aggregate
    /// parameters.
    fn routed(
        &mut self,
        symbol: &str,
        signature: &str,
        args: Vec<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let layouts = vec![""; args.len()].join("|");
        self.routed_with_layouts(symbol, signature, &layouts, args, ty, span)
    }

    fn routed_with_layouts(
        &mut self,
        symbol: &str,
        signature: &str,
        layouts: &str,
        args: Vec<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let mut routed = vec![
            self.string_lit(symbol, span),
            self.string_lit(signature, span),
            self.string_lit("", span),
            self.string_lit(layouts, span),
        ];
        routed.extend(args);
        self.call_named(FOREIGN_DISPATCHER, routed, ty, span)
    }

    fn struct_lit(
        &mut self,
        name: &str,
        fields: Vec<(&str, HirExpr)>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let unit = self.tcx.unit();
        let mut args = vec![self.string_lit(name, span)];
        for (field, value) in fields {
            args.push(self.string_lit(field, span));
            args.push(value);
        }
        let callee = self.path("__struct", unit, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    fn some(&mut self, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        let callee = self.path("Some", ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![value],
            },
        )
    }

    fn if_else(
        &mut self,
        condition: HirExpr,
        then: HirExpr,
        otherwise: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then),
                else_branch: Some(Box::new(otherwise)),
            },
        )
    }

    #[allow(clippy::too_many_arguments, reason = "one match, spelled out")]
    fn match_option(
        &mut self,
        scrutinee: HirExpr,
        bound: &str,
        payload: Ty,
        some_body: HirExpr,
        none_body: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let scrutinee_ty = scrutinee.ty;
        let binding = HirPat {
            id: self.ids.next(),
            span,
            ty: payload,
            kind: HirPatKind::Binding {
                name: Ident::new(bound),
                mutable: false,
            },
        };
        let some_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Some"),
                fields: vec![binding],
            },
        };
        let none_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        self.expr(
            ty,
            span,
            HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    HirMatchArm {
                        pattern: some_pat,
                        guard: None,
                        body: some_body,
                    },
                    HirMatchArm {
                        pattern: none_pat,
                        guard: None,
                        body: none_body,
                    },
                ],
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

    fn let_stmt(
        &mut self,
        name: &str,
        mutable: bool,
        ty: Ty,
        init: HirExpr,
        span: Span,
    ) -> HirStmt {
        let pattern = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable,
            },
        };
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty,
                init: Some(init),
            },
        }
    }

    fn expr_stmt(&mut self, expr: HirExpr, span: Span) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Expr {
                expr,
                has_semi: true,
            },
        }
    }

    fn assign_stmt(&mut self, place: HirExpr, value: HirExpr, span: Span) -> HirStmt {
        let unit = self.tcx.unit();
        let assign = self.expr(
            unit,
            span,
            HirExprKind::Assign {
                place: Box::new(place),
                value: Box::new(value),
            },
        );
        self.expr_stmt(assign, span)
    }

    fn param(&mut self, name: &str, ty: Ty, span: Span) -> HirParam {
        HirParam {
            pattern: HirPat {
                id: self.ids.next(),
                span,
                ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(name),
                    mutable: false,
                },
            },
            ty,
            is_comptime: false,
        }
    }
}
