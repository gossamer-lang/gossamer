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
//!   `align_of`, `offset_of`, `fn_from_ptr`), the `ffi::Handle` methods, and
//!   the `ffi::Union` operations become runtime calls or constants over the C
//!   layout of their type, which is known here and not in their generic
//!   declarations;
//! - `ffi::addr_of(NAME)` of a C global declared `static NAME: T` becomes the
//!   symbol's address, and the placeholder static itself is dropped.

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::{ArrayLen, FloatTy, FnSig, IntTy, Mutbl, Ty, TyCtxt, TyKind};

use crate::foreign::{
    CValue, FOREIGN_DISPATCHER, HANDLE_TYPE, PTR_TYPE, aggregate_layout, aggregate_slice_elem,
    classify_value, handle_value, option_payload, out_param_word, ptr_elem, struct_result,
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
/// `__gos_ffi_export(adapter, signature, symbol)`: defines the external
/// C-ABI entry `symbol` running `adapter`.
pub const EXPORT: &str = "__gos_ffi_export";
/// The generated function holding one [`EXPORT`] per `#[export]` function.
/// Nothing calls it; it roots the entries a native build defines.
pub const EXPORTS_FN: &str = "__gos_ffi_exports";
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
/// `__gos_ffi_symbol(symbol, library) -> u64`: the address of a C global.
pub const SYMBOL: &str = "__gos_ffi_symbol";
/// `__gos_ffi_load_int(address, class) -> i64`: a C integer in memory.
pub const LOAD_INT: &str = "__gos_ffi_load_int";
/// `__gos_ffi_load_float(address, class) -> f64`: a C float in memory.
pub const LOAD_FLOAT: &str = "__gos_ffi_load_float";
/// `__gos_ffi_store_int(address, class, value)`: stores a C integer.
pub const STORE_INT: &str = "__gos_ffi_store_int";
/// `__gos_ffi_store_float(address, class, value)`: stores a C float.
pub const STORE_FLOAT: &str = "__gos_ffi_store_float";
/// `__gos_ffi_view_check(index, len)`: an `ffi::View` element's bounds.
pub const VIEW_CHECK: &str = "__gos_ffi_view_check";
/// `__gos_ffi_view_range_check(lo, hi, len)`: an `ffi::View` sub-view's.
pub const VIEW_RANGE_CHECK: &str = "__gos_ffi_view_range_check";
/// `__gos_ffi_view_len_check(len, found)`: `ffi::View::copy_from`'s.
pub const VIEW_LEN_CHECK: &str = "__gos_ffi_view_len_check";
/// `__gos_ffi_atomic_rmw(address, op, width, value) -> i64`.
pub const ATOMIC_RMW: &str = "__gos_ffi_atomic_rmw";
/// `__gos_ffi_atomic_cas(address, width, expected, new) -> i64`.
pub const ATOMIC_CAS: &str = "__gos_ffi_atomic_cas";

/// A function declared in an extern block, as its call sites need it.
struct ForeignDecl {
    params: Vec<Ty>,
    ret: Option<Ty>,
    symbol: String,
}

/// A slice-of-structs argument copied into C memory for a call.
struct SliceCopy {
    /// The local bound to the argument.
    slice: String,
    slice_ty: Ty,
    elem: Ty,
    /// The local holding the slice's length.
    len: String,
    /// The local holding the copy's address.
    base: String,
    size: u32,
    /// The layout of a one-element array of `elem`.
    layout: String,
    /// Whether the callee's writes come back.
    writable: bool,
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
    let mut statics = HashMap::new();
    let mut exports = Vec::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Static(decl) => {
                if let (Some(def), Some(foreign)) = (item.def, &decl.foreign) {
                    statics.insert(def, foreign.clone());
                }
            }
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
                if let Some(symbol) = &decl.export_symbol {
                    exports.push((def, symbol.clone(), item.span));
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
        statics,
        adapters: HashMap::new(),
        generated: Vec::new(),
        temp: 0,
    };
    for item in &mut program.items {
        pass.visit_item(item);
    }
    if !exports.is_empty() {
        let item = pass.exports_fn(&exports);
        pass.generated.push(item);
    }
    program.items.append(&mut pass.generated);
    // A foreign static's placeholder initializer has nothing to run.
    program
        .items
        .retain(|item| !matches!(&item.kind, HirItemKind::Static(decl) if decl.foreign.is_some()));
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
    AlignOf,
    OffsetOf,
    AddrOf,
    FnAddr,
    FnFromPtr,
    /// An atomic read-modify-write: its runtime op code (0 load .. 7 xor).
    Atomic(i64),
    AtomicCompareExchange,
}

fn operation_named(name: &str) -> Option<Operation> {
    Some(match name {
        "__gos_ffi_read" => Operation::Read,
        "__gos_ffi_read_at" => Operation::ReadAt,
        "__gos_ffi_write" => Operation::Write,
        "__gos_ffi_write_at" => Operation::WriteAt,
        "__gos_ffi_alloc" => Operation::Alloc,
        "__gos_ffi_size_of" => Operation::SizeOf,
        "__gos_ffi_align_of" => Operation::AlignOf,
        "__gos_ffi_offset_of" => Operation::OffsetOf,
        "__gos_ffi_addr_of" => Operation::AddrOf,
        "__gos_ffi_fn_addr" => Operation::FnAddr,
        "__gos_ffi_atomic_load" => Operation::Atomic(0),
        "__gos_ffi_atomic_store" => Operation::Atomic(1),
        "__gos_ffi_atomic_swap" => Operation::Atomic(2),
        "__gos_ffi_atomic_fetch_add" => Operation::Atomic(3),
        "__gos_ffi_atomic_fetch_sub" => Operation::Atomic(4),
        "__gos_ffi_atomic_fetch_and" => Operation::Atomic(5),
        "__gos_ffi_atomic_fetch_or" => Operation::Atomic(6),
        "__gos_ffi_atomic_fetch_xor" => Operation::Atomic(7),
        "__gos_ffi_atomic_compare_exchange" => Operation::AtomicCompareExchange,
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
    /// C globals declared in extern blocks: symbol and library.
    statics: HashMap<DefId, (String, String)>,
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
            HirExprKind::Index { base, index }
                if crate::foreign::view_elem(self.tcx, Self::referent(self.tcx, base.ty))
                    .is_some() =>
            {
                self.rewrite_view_method(expr, base, "get", std::slice::from_ref(index))
            }
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if crate::foreign::view_elem(self.tcx, Self::referent(self.tcx, receiver.ty))
                .is_some() =>
            {
                self.rewrite_view_method(expr, receiver, &name.name, args)
            }
            HirExprKind::Call { callee, args } => self.rewrite_call(expr, callee, args),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if self.union_size(receiver.ty).is_some() => {
                self.rewrite_union_method(expr, receiver, &name.name, args)
            }
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
        if self.union_size(call.ty).is_some() {
            match (last, args) {
                ("zeroed", []) => return self.union_zeroed(call.ty, call.span),
                ("new", [value]) => return self.union_new(call.ty, value, call.span),
                _ => {}
            }
        }
        if let Some((receiver, rest)) = args.split_first()
            && crate::foreign::view_elem(self.tcx, Self::referent(self.tcx, receiver.ty)).is_some()
            && matches!(
                last,
                "get" | "index" | "set" | "slice" | "to_vec" | "copy_from" | "fill"
            )
        {
            return self.rewrite_view_method(call, receiver, last, rest);
        }
        // A method generic over its own parameter reaches here as a path
        // call with the receiver first.
        if matches!(last, "get" | "set")
            && let Some((receiver, rest)) = args.split_first()
            && self
                .union_size(Self::referent(self.tcx, receiver.ty))
                .is_some()
        {
            return self.rewrite_union_method(call, receiver, last, rest);
        }
        None
    }

    // ---- foreign calls ------------------------------------------------

    /// Stages `arg` when `param` is an out-parameter (a `&mut` scalar or
    /// pointer) in a one-element array the call writes through, and queues
    /// the write back to `arg`'s place. Answers the call argument.
    fn stage_out_param(
        &mut self,
        arg: &HirExpr,
        param: Ty,
        symbol: &str,
        stmts: &mut Vec<HirStmt>,
        after: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<HirExpr> {
        let (word, value) = out_param_word(self.tcx, param)?;
        let reference = self.fresh("r");
        stmts.push(self.let_stmt(&reference, false, param, arg.clone(), span));
        let current = {
            let staged_expr = self.local(&reference, param, span);
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
        let staged_arg = self.unary(HirUnaryOp::RefMut, staged_local, staged_ref_ty, span);
        let element = {
            let staged_expr = self.local(&staged, staged_ty, span);
            self.index0(staged_expr, word, span)
        };
        let target_ty = match self.tcx.kind_of(param) {
            TyKind::Ref { inner, .. } => *inner,
            _ => param,
        };
        let back = self.value_out(element, &value, target_ty, symbol, span);
        let place = {
            let staged_expr = self.local(&reference, param, span);
            self.deref(staged_expr, span)
        };
        after.push(self.assign_stmt(place, back, span));
        Some(staged_arg)
    }

    /// The one-element array a struct result comes back into, bound in
    /// `stmts` and passed as the call's final argument.
    fn struct_result_holder(
        &mut self,
        result: Ty,
        stmts: &mut Vec<HirStmt>,
        call_args: &mut Vec<HirExpr>,
        span: Span,
    ) -> Option<(String, Ty, Ty)> {
        let holder_ty = self.array_ty(result, 1);
        let holder = self.fresh("ret");
        let zero = self.zero(result, span)?;
        let initial = self.array(vec![zero], holder_ty, span);
        stmts.push(self.let_stmt(&holder, true, holder_ty, initial, span));
        let holder_ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: holder_ty,
        });
        let local = self.local(&holder, holder_ty, span);
        call_args.push(self.unary(HirUnaryOp::RefMut, local, holder_ref_ty, span));
        Some((holder, holder_ty, result))
    }

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
        let mut copies = Vec::new();
        for (arg, param) in args.iter().zip(&params) {
            if let Some((elem, writable)) = aggregate_slice_elem(self.tcx, *param) {
                changed = true;
                let address =
                    self.copy_slice_in(arg, elem, writable, &mut stmts, &mut copies, span)?;
                call_args.push(address);
                continue;
            }
            if let Some(staged) =
                self.stage_out_param(arg, *param, &symbol, &mut stmts, &mut after, span)
            {
                changed = true;
                call_args.push(staged);
                continue;
            }
            // A pointer or a callback crosses as the address word.
            let word = match classify_value(self.tcx, *param) {
                Some(value @ (CValue::Ptr | CValue::OptPtr)) => {
                    Some(self.value_in(arg.clone(), &value, span))
                }
                Some(CValue::Callback(sig)) => Some(self.callback_address(arg, &sig, span)?),
                _ => None,
            };
            if let Some(word) = word {
                changed = true;
                let bound = self.fresh("a");
                stmts.push(self.let_stmt(&bound, false, u64_ty, word, span));
                call_args.push(self.local(&bound, u64_ty, span));
            } else {
                call_args.push(arg.clone());
            }
        }
        let ret_value = ret.and_then(|ret| match classify_value(self.tcx, ret)? {
            value @ (CValue::Ptr | CValue::OptPtr) => Some(value),
            _ => None,
        });
        if !changed && ret_value.is_none() && struct_result(self.tcx, ret).is_none() {
            return None;
        }
        for copy in copies {
            self.copy_slice_out(copy, &mut after, span)?;
        }
        // A struct result comes back into a one-element array of it, the
        // call's final argument.
        let holder = match struct_result(self.tcx, ret) {
            Some(result) => {
                Some(self.struct_result_holder(result, &mut stmts, &mut call_args, span)?)
            }
            None => None,
        };
        if let Some((holder, holder_ty, result)) = holder {
            let unit = self.tcx.unit();
            let inner = HirExpr {
                id: self.ids.next(),
                span,
                ty: unit,
                kind: HirExprKind::Call {
                    callee: Box::new(callee.clone()),
                    args: call_args,
                },
            };
            stmts.push(self.expr_stmt(inner, span));
            stmts.extend(after);
            let local = self.local(&holder, holder_ty, span);
            let tail = self.index0(local, result, span);
            return Some(self.block(stmts, Some(tail), call.ty, span));
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

    /// Binds a slice-of-structs argument and copies its elements into C
    /// memory, answering that memory's address. The copy is read back (when
    /// the parameter is `&mut`) and freed by [`Self::copy_slice_out`].
    fn copy_slice_in(
        &mut self,
        arg: &HirExpr,
        elem: Ty,
        writable: bool,
        stmts: &mut Vec<HirStmt>,
        copies: &mut Vec<SliceCopy>,
        span: Span,
    ) -> Option<HirExpr> {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let (size, layout) = self.layout_of_one(elem)?;
        let slice = self.fresh("xs");
        stmts.push(self.let_stmt(&slice, false, arg.ty, arg.clone(), span));
        let len = self.fresh("n");
        let len_value = {
            let local = self.local(&slice, arg.ty, span);
            self.method(local, "len", Vec::new(), i64_ty, span)
        };
        stmts.push(self.let_stmt(&len, false, i64_ty, len_value, span));
        // `malloc(0)` may answer NULL; one byte keeps every copy addressable.
        let bytes = {
            let n = self.local(&len, i64_ty, span);
            let n = self.cast(n, u64_ty, span);
            let size_lit = self.int_lit(i64::from(size), u64_ty, span);
            let product = self.binary(HirBinaryOp::Mul, n, size_lit, u64_ty, span);
            let one = self.int_lit(1, u64_ty, span);
            self.binary(HirBinaryOp::Add, product, one, u64_ty, span)
        };
        let raw = self.routed("gos_rt_ffi_alloc", "L>L", vec![bytes], u64_ty, span);
        let base = self.fresh("buf");
        stmts.push(self.let_stmt(&base, false, u64_ty, raw, span));
        // while i < n { write(base + i * size, xs[i]); i += 1 }
        let index = self.fresh("i");
        let mut body = Vec::new();
        let element = {
            let local = self.local(&slice, arg.ty, span);
            let at = self.local(&index, i64_ty, span);
            self.index(local, at, elem, span)
        };
        let staged_ty = self.array_ty(elem, 1);
        let staged = self.fresh("in");
        let initial = self.array(vec![element], staged_ty, span);
        body.push(self.let_stmt(&staged, false, staged_ty, initial, span));
        let address = self.element_at(&base, &index, size, span);
        let staged_local = self.local(&staged, staged_ty, span);
        let len_lit = self.int_lit(i64::from(size), u64_ty, span);
        let write = self.routed_with_layouts(
            "gos_rt_ffi_write",
            "LrL>v",
            &format!("|{layout}|"),
            vec![address, staged_local, len_lit],
            unit,
            span,
        );
        body.push(self.expr_stmt(write, span));
        stmts.push(self.counted_loop(&index, &len, body, span));
        copies.push(SliceCopy {
            slice,
            slice_ty: arg.ty,
            elem,
            len,
            base: base.clone(),
            size,
            layout,
            writable,
        });
        Some(self.local(&base, u64_ty, span))
    }

    /// Reads a slice copy back into the slice when its parameter is `&mut`,
    /// then frees it.
    fn copy_slice_out(
        &mut self,
        copy: SliceCopy,
        after: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<()> {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        if copy.writable {
            let index = self.fresh("i");
            let mut body = Vec::new();
            let staged_ty = self.array_ty(copy.elem, 1);
            let staged = self.fresh("out");
            let zero = self.zero(copy.elem, span)?;
            let initial = self.array(vec![zero], staged_ty, span);
            body.push(self.let_stmt(&staged, true, staged_ty, initial, span));
            let staged_ref_ty = self.tcx.intern(TyKind::Ref {
                mutability: Mutbl::Mut,
                inner: staged_ty,
            });
            let staged_ref = {
                let local = self.local(&staged, staged_ty, span);
                self.unary(HirUnaryOp::RefMut, local, staged_ref_ty, span)
            };
            let address = self.element_at(&copy.base, &index, copy.size, span);
            let len_lit = self.int_lit(i64::from(copy.size), u64_ty, span);
            let read = self.routed_with_layouts(
                "gos_rt_ffi_read",
                "RLL>v",
                &format!("{}||", copy.layout),
                vec![staged_ref, address, len_lit],
                unit,
                span,
            );
            body.push(self.expr_stmt(read, span));
            let target = {
                let local = self.local(&copy.slice, copy.slice_ty, span);
                let at = self.local(&index, i64_ty, span);
                self.index(local, at, copy.elem, span)
            };
            let value = {
                let local = self.local(&staged, staged_ty, span);
                self.index0(local, copy.elem, span)
            };
            body.push(self.assign_stmt(target, value, span));
            after.push(self.counted_loop(&index, &copy.len, body, span));
        }
        let base = self.local(&copy.base, u64_ty, span);
        let free = self.routed("gos_rt_ffi_free", "L>v", vec![base], unit, span);
        after.push(self.expr_stmt(free, span));
        Some(())
    }

    /// `base + index * size`, the address of element `index` of a C array.
    fn element_at(&mut self, base: &str, index: &str, size: u32, span: Span) -> HirExpr {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let base = self.local(base, u64_ty, span);
        let at = {
            let local = self.local(index, i64_ty, span);
            self.cast(local, u64_ty, span)
        };
        let size = self.int_lit(i64::from(size), u64_ty, span);
        let offset = self.binary(HirBinaryOp::Mul, at, size, u64_ty, span);
        self.binary(HirBinaryOp::Add, base, offset, u64_ty, span)
    }

    /// `{ let mut index = 0; while index < len { body; index = index + 1 } }`.
    fn counted_loop(&mut self, index: &str, len: &str, body: Vec<HirStmt>, span: Span) -> HirStmt {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let zero = self.int_lit(0, i64_ty, span);
        let start = self.let_stmt(index, true, i64_ty, zero, span);
        let condition = {
            let at = self.local(index, i64_ty, span);
            let end = self.local(len, i64_ty, span);
            self.binary(HirBinaryOp::Lt, at, end, bool_ty, span)
        };
        let mut body = body;
        let step = {
            let at = self.local(index, i64_ty, span);
            let one = self.int_lit(1, i64_ty, span);
            let next = self.binary(HirBinaryOp::Add, at, one, i64_ty, span);
            let place = self.local(index, i64_ty, span);
            self.assign_stmt(place, next, span)
        };
        body.push(step);
        let body = self.block(body, None, unit, span);
        let looped = self.expr(
            unit,
            span,
            HirExprKind::While {
                condition: Box::new(condition),
                body: Box::new(body),
                label: None,
            },
        );
        let looped = self.expr_stmt(looped, span);
        let whole = self.block(vec![start, looped], None, unit, span);
        self.expr_stmt(whole, span)
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
        let signature = self.callback_signature(sig)?;
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

    /// `fn __gos_ffi_exports() { __gos_ffi_export(adapter, signature, symbol) .. }`
    /// over every `#[export]` function: each is reached from C through an
    /// entry running the same adapter a callback to it runs.
    fn exports_fn(&mut self, exports: &[(DefId, String, Span)]) -> HirItem {
        let unit = self.tcx.unit();
        let mut stmts = Vec::new();
        let mut first_span = None;
        for (def, symbol, span) in exports {
            let span = *span;
            first_span.get_or_insert(span);
            let Some(target) = self.functions.get(def) else {
                continue;
            };
            let sig = FnSig {
                inputs: target.params.clone(),
                output: target.ret.unwrap_or(unit),
            };
            let name = target.name.clone();
            let fn_ty = self.tcx.intern(TyKind::FnDef {
                def: *def,
                substs: gossamer_types::Substs::default(),
            });
            let target_path = self.expr(
                fn_ty,
                span,
                HirExprKind::Path {
                    segments: vec![Ident::new(&name)],
                    def: Some(*def),
                },
            );
            let Some(address) = self.callback_address(&target_path, &sig, span) else {
                continue;
            };
            // The callback form names the adapter and signature; the export
            // keeps both and names the symbol in place of the function.
            let HirExprKind::Call { mut args, .. } = address.kind else {
                continue;
            };
            args.truncate(2);
            args.push(self.string_lit(symbol, span));
            let u64_ty = self.tcx.int_ty(IntTy::U64);
            let call = self.call_named(EXPORT, args, u64_ty, span);
            stmts.push(self.expr_stmt(call, span));
        }
        let span = first_span.unwrap_or_default();
        let body = HirBody {
            block: HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: None,
                ty: unit,
                is_comptime: false,
            },
        };
        HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(HirFn {
                name: Ident::new(EXPORTS_FN),
                params: Vec::new(),
                ret: None,
                body: Some(body),
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

    /// The C signature of a callback or function pointer of type `sig`: a
    /// class per scalar, `s(layout)` per struct by value.
    fn callback_signature(&self, sig: &FnSig) -> Option<String> {
        let spell = |ty: Ty| -> Option<String> {
            match crate::foreign::value_class(self.tcx, ty) {
                Some(class) => Some(class.to_string()),
                None => crate::foreign::by_value_spelling(self.tcx, ty),
            }
        };
        let mut signature = String::new();
        for input in &sig.inputs {
            signature.push_str(&spell(*input)?);
        }
        signature.push('>');
        if matches!(self.tcx.kind_of(sig.output), TyKind::Unit) {
            signature.push('v');
        } else {
            signature.push_str(&spell(sig.output)?);
        }
        Some(signature)
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
            let index = i64::try_from(index).ok()?;
            let decoded = if let Some(value) = classify_value(self.tcx, *param) {
                self.callback_argument(&words, index, &value, *param, &target_name, span)?
            } else {
                // A struct argument's word is the address of its bytes.
                let address = self.callback_word(&words, index, span);
                self.read_struct_at(address, *param, &mut stmts, span)?
            };
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
        let tail = if crate::foreign::struct_result(self.tcx, Some(ret_ty)).is_some() {
            // A struct result is written to the buffer whose address is the
            // word after the arguments'.
            let index = i64::try_from(params.len()).ok()?;
            let address = self.callback_word(&words, index, span);
            self.write_struct_at(address, call, &mut stmts, span)?;
            self.int_lit(0, i64_ty, span)
        } else {
            self.callback_result(call, ret_ty, &mut stmts, span)?
        };
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
            export_symbol: None,
        };
        Some(HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(decl),
        })
    }

    /// The callback argument word at `index` of the words at `words`.
    fn callback_word(&mut self, words: &str, index: i64, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let index_lit = self.int_lit(index, u64_ty, span);
        let base = {
            let local = self.local(words, i64_ty, span);
            self.cast(local, u64_ty, span)
        };
        self.routed(
            "gos_rt_ffi_word",
            "LL>L",
            vec![base, index_lit],
            u64_ty,
            span,
        )
    }

    /// The `ty` copied out of the C bytes at `address`.
    fn read_struct_at(
        &mut self,
        address: HirExpr,
        ty: Ty,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<HirExpr> {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let (size, layout) = self.layout_of_one(ty)?;
        let staged_ty = self.array_ty(ty, 1);
        let zero = self.zero(ty, span)?;
        let initial = self.array(vec![zero], staged_ty, span);
        let staged = self.fresh("arg");
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
        let local = self.local(&staged, staged_ty, span);
        Some(self.index0(local, ty, span))
    }

    /// Copies `value` into the C bytes at `address`.
    fn write_struct_at(
        &mut self,
        address: HirExpr,
        value: HirExpr,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<()> {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let (size, layout) = self.layout_of_one(value.ty)?;
        let staged_ty = self.array_ty(value.ty, 1);
        let staged = self.fresh("res");
        let initial = self.array(vec![value], staged_ty, span);
        stmts.push(self.let_stmt(&staged, false, staged_ty, initial, span));
        let local = self.local(&staged, staged_ty, span);
        let len = self.int_lit(i64::from(size), u64_ty, span);
        let copy = self.routed_with_layouts(
            "gos_rt_ffi_write",
            "LrL>v",
            &format!("|{layout}|"),
            vec![address, local, len],
            unit,
            span,
        );
        stmts.push(self.expr_stmt(copy, span));
        Some(())
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
            Operation::Read | Operation::ReadAt => self.rewrite_read(call, op, args),
            Operation::Write | Operation::WriteAt => self.rewrite_write(call, op, args),
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
            Operation::AlignOf => {
                let elem = self.generic_arg(callee, 0)?;
                let align = self.tcx.plain_layout(elem)?.align;
                Some(self.int_lit(i64::from(align), i64_ty, span))
            }
            Operation::OffsetOf => {
                let target = self.generic_arg(callee, 0)?;
                let HirExprKind::Literal(HirLiteral::String(path)) = &args.first()?.kind else {
                    return None;
                };
                let offset = self.field_offset(target, path)?;
                Some(self.int_lit(i64::from(offset), i64_ty, span))
            }
            Operation::AddrOf => {
                let HirExprKind::Path { def: Some(def), .. } = &args.first()?.kind else {
                    return None;
                };
                let (symbol, library) = self.statics.get(def)?.clone();
                let symbol = self.string_lit(&symbol, span);
                let library = self.string_lit(&library, span);
                let address = self.call_named(SYMBOL, vec![symbol, library], u64_ty, span);
                Some(self.struct_lit(PTR_TYPE, vec![("__addr", address)], call.ty, span))
            }
            Operation::FnFromPtr => self.fn_from_ptr(call, args.first()?, span),
            Operation::FnAddr => {
                let target = args.first()?;
                let HirExprKind::Path { def: Some(def), .. } = &target.kind else {
                    return None;
                };
                let function = self.functions.get(def)?;
                let sig = FnSig {
                    inputs: function.params.clone(),
                    output: function.ret.unwrap_or(unit),
                };
                let address = self.callback_address(target, &sig, span)?;
                Some(self.struct_lit(PTR_TYPE, vec![("__addr", address)], call.ty, span))
            }
            Operation::Atomic(code) => self.rewrite_atomic(call, code, args),
            Operation::AtomicCompareExchange => self.rewrite_compare_exchange(call, args),
        }
    }

    /// `ffi::read(p)` and `ffi::read_at(p, i)`: a load for a scalar, a copy through a staging array otherwise.
    fn rewrite_read(&mut self, call: &HirExpr, op: Operation, args: &[HirExpr]) -> Option<HirExpr> {
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let elem = call.ty;
        let (size, layout) = self.layout_of_one(elem)?;
        let mut stmts = Vec::new();
        let address =
            self.element_address(&mut stmts, args, op == Operation::ReadAt, size, span)?;
        if let Some(value) = self.load_scalar(address.clone(), elem, span) {
            return Some(self.block(stmts, Some(value), elem, span));
        }
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

    /// `ffi::write(p, v)` and `ffi::write_at(p, i, v)`: a store for a scalar, a copy through a staging array otherwise.
    fn rewrite_write(
        &mut self,
        call: &HirExpr,
        op: Operation,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let value = args.last()?;
        let elem = value.ty;
        let (size, layout) = self.layout_of_one(elem)?;
        let mut stmts = Vec::new();
        let address =
            self.element_address(&mut stmts, args, op == Operation::WriteAt, size, span)?;
        if let Some(store) = self.store_scalar(address.clone(), value.clone(), span) {
            stmts.push(self.expr_stmt(store, span));
            return Some(self.block(stmts, None, unit, span));
        }
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

    /// An atomic read-modify-write of the integer a `Ptr` addresses, by operation `code`.
    fn rewrite_atomic(&mut self, call: &HirExpr, code: i64, args: &[HirExpr]) -> Option<HirExpr> {
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let pointer = args.first()?;
        let elem = ptr_elem(self.tcx, pointer.ty)?;
        let width = self.c_size(elem)?;
        let address = self.field(pointer.clone(), "__addr", u64_ty, span);
        let operand = match args.get(1) {
            Some(value) => self.cast(value.clone(), i64_ty, span),
            None => self.int_lit(0, i64_ty, span),
        };
        let code = self.int_lit(code, i64_ty, span);
        let width = self.int_lit(i64::from(width), i64_ty, span);
        let raw = self.call_named(
            ATOMIC_RMW,
            vec![address, code, width, operand],
            i64_ty,
            span,
        );
        if matches!(self.tcx.kind_of(call.ty), TyKind::Unit) {
            let stmt = self.expr_stmt(raw, span);
            return Some(self.block(vec![stmt], None, unit, span));
        }
        Some(self.cast(raw, call.ty, span))
    }

    /// `ffi::atomic_compare_exchange(p, current, new)`: `Ok` with the old value when it
    /// held `current`, `Err` with the value found otherwise.
    fn rewrite_compare_exchange(&mut self, call: &HirExpr, args: &[HirExpr]) -> Option<HirExpr> {
        let span = call.span;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let [pointer, current, new] = args else {
            return None;
        };
        let elem = ptr_elem(self.tcx, pointer.ty)?;
        let width = self.c_size(elem)?;
        let mut stmts = Vec::new();
        let expected = self.fresh("cur");
        stmts.push(self.let_stmt(&expected, false, elem, current.clone(), span));
        let address = self.field(pointer.clone(), "__addr", u64_ty, span);
        let width = self.int_lit(i64::from(width), i64_ty, span);
        let expected_word = {
            let local = self.local(&expected, elem, span);
            self.cast(local, i64_ty, span)
        };
        let new_word = self.cast(new.clone(), i64_ty, span);
        let raw = self.call_named(
            ATOMIC_CAS,
            vec![address, width, expected_word, new_word],
            i64_ty,
            span,
        );
        let previous = self.fresh("prev");
        let value = self.cast(raw, elem, span);
        stmts.push(self.let_stmt(&previous, false, elem, value, span));
        let bool_ty = self.tcx.bool_ty();
        let swapped = {
            let prev = self.local(&previous, elem, span);
            let cur = self.local(&expected, elem, span);
            self.binary(HirBinaryOp::Eq, prev, cur, bool_ty, span)
        };
        let ok = {
            let prev = self.local(&previous, elem, span);
            self.variant("Ok", prev, call.ty, span)
        };
        let err = {
            let prev = self.local(&previous, elem, span);
            self.variant("Err", prev, call.ty, span)
        };
        let choice = self.if_else(swapped, ok, err, call.ty, span);
        Some(self.block(stmts, Some(choice), call.ty, span))
    }

    /// The scalar `ty` loaded from the C memory at `address`, without a
    /// foreign call; `None` when `ty` is not a scalar or pointer form.
    fn load_scalar(&mut self, address: HirExpr, ty: Ty, span: Span) -> Option<HirExpr> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let value = classify_value(self.tcx, ty)?;
        let class = match &value {
            CValue::Scalar(class) => *class,
            CValue::Ptr | CValue::OptPtr => 'L',
            CValue::Callback(_) => return None,
        };
        let class_lit = self.int_lit(i64::from(u32::from(class)), i64_ty, span);
        Some(match value {
            CValue::Scalar('f' | 'd') => {
                let f64_ty = self.tcx.float_ty(FloatTy::F64);
                let raw = self.call_named(LOAD_FLOAT, vec![address, class_lit], f64_ty, span);
                self.cast(raw, ty, span)
            }
            CValue::Scalar('B') => {
                let raw = self.call_named(LOAD_INT, vec![address, class_lit], i64_ty, span);
                let zero = self.int_lit(0, i64_ty, span);
                let bool_ty = self.tcx.bool_ty();
                self.binary(HirBinaryOp::Ne, raw, zero, bool_ty, span)
            }
            CValue::Scalar(_) => {
                let raw = self.call_named(LOAD_INT, vec![address, class_lit], i64_ty, span);
                self.cast(raw, ty, span)
            }
            // A pointer copied out of memory carries whatever address C
            // stored there, NULL included.
            CValue::Ptr => {
                let raw = self.call_named(LOAD_INT, vec![address, class_lit], i64_ty, span);
                let word = self.cast(raw, u64_ty, span);
                self.struct_lit(PTR_TYPE, vec![("__addr", word)], ty, span)
            }
            ref pointer => {
                let raw = self.call_named(LOAD_INT, vec![address, class_lit], i64_ty, span);
                let word = self.cast(raw, u64_ty, span);
                self.value_out(word, pointer, ty, "ffi::read", span)
            }
        })
    }

    /// A store of the scalar `value` into the C memory at `address`, without
    /// a foreign call; `None` when its type is not a scalar or pointer form.
    fn store_scalar(&mut self, address: HirExpr, value: HirExpr, span: Span) -> Option<HirExpr> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let kind = classify_value(self.tcx, value.ty)?;
        let class = match &kind {
            CValue::Scalar(class) => *class,
            CValue::Ptr | CValue::OptPtr => 'L',
            CValue::Callback(_) => return None,
        };
        let class_lit = self.int_lit(i64::from(u32::from(class)), i64_ty, span);
        Some(match kind {
            CValue::Scalar('f' | 'd') => {
                let f64_ty = self.tcx.float_ty(FloatTy::F64);
                let wide = self.cast(value, f64_ty, span);
                self.call_named(STORE_FLOAT, vec![address, class_lit, wide], unit, span)
            }
            CValue::Scalar(_) => {
                let word = self.cast(value, i64_ty, span);
                self.call_named(STORE_INT, vec![address, class_lit, word], unit, span)
            }
            ref pointer => {
                let word = self.value_in(value, pointer, span);
                let word = self.cast(word, i64_ty, span);
                self.call_named(STORE_INT, vec![address, class_lit, word], unit, span)
            }
        })
    }

    /// `Variant(value)` of the enum `ty`.
    fn variant(&mut self, name: &str, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        let callee = self.path(name, ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![value],
            },
        )
    }

    // ---- views ----------------------------------------------------------

    /// The methods of `ffi::View<T>` that touch its memory.
    fn rewrite_view_method(
        &mut self,
        call: &HirExpr,
        receiver: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let span = call.span;
        let view_ty = Self::referent(self.tcx, receiver.ty);
        let elem = crate::foreign::view_elem(self.tcx, view_ty)?;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let size = self.c_size(elem)?;
        let receiver = match &receiver.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut | HirUnaryOp::RefShared,
                operand,
            } => (**operand).clone(),
            _ => receiver.clone(),
        };
        let mut stmts = Vec::new();
        let view = self.fresh("view");
        stmts.push(self.let_stmt(&view, false, view_ty, receiver, span));
        let base = self.fresh("base");
        let base_value = {
            let local = self.local(&view, view_ty, span);
            self.field(local, "__addr", u64_ty, span)
        };
        stmts.push(self.let_stmt(&base, false, u64_ty, base_value, span));
        let len = self.fresh("len");
        let len_value = {
            let local = self.local(&view, view_ty, span);
            self.field(local, "__len", i64_ty, span)
        };
        stmts.push(self.let_stmt(&len, false, i64_ty, len_value, span));
        let parts = ViewParts {
            view_ty,
            elem,
            size,
            base: base.clone(),
            len: len.clone(),
        };
        match (method, args) {
            ("get" | "index", [index]) => {
                let at = self.fresh("i");
                stmts.push(self.let_stmt(&at, false, i64_ty, index.clone(), span));
                self.view_check(&at, &len, &mut stmts, span);
                let address = self.element_at(&base, &at, size, span);
                let value = self.load_element(address, elem, &mut stmts, span)?;
                Some(self.block(stmts, Some(value), elem, span))
            }
            ("set", [index, value]) => {
                let at = self.fresh("i");
                stmts.push(self.let_stmt(&at, false, i64_ty, index.clone(), span));
                self.view_check(&at, &len, &mut stmts, span);
                let address = self.element_at(&base, &at, size, span);
                self.store_element(address, value.clone(), &mut stmts, span)?;
                Some(self.block(stmts, None, unit, span))
            }
            ("slice", [lo, hi]) => self.view_slice(&parts, stmts, lo, hi, span),
            ("to_vec", []) => self.view_to_vec(&parts, stmts, span),
            ("copy_from", [values]) => self.view_copy_from(&parts, stmts, values, span),
            ("fill", [value]) => self.view_fill(&parts, stmts, value, span),
            _ => None,
        }
    }

    /// `v.slice(lo, hi)`: a view of the elements from `lo` up to `hi`, range-checked.
    fn view_slice(
        &mut self,
        parts: &ViewParts,
        mut stmts: Vec<HirStmt>,
        lo: &HirExpr,
        hi: &HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        let ViewParts {
            view_ty,
            size,
            ref base,
            ref len,
            ..
        } = *parts;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let low = self.fresh("lo");
        stmts.push(self.let_stmt(&low, false, i64_ty, lo.clone(), span));
        let high = self.fresh("hi");
        stmts.push(self.let_stmt(&high, false, i64_ty, hi.clone(), span));
        let check = {
            let lo = self.local(&low, i64_ty, span);
            let hi = self.local(&high, i64_ty, span);
            let len = self.local(len, i64_ty, span);
            self.call_named(VIEW_RANGE_CHECK, vec![lo, hi, len], unit, span)
        };
        stmts.push(self.expr_stmt(check, span));
        let address = self.element_at(base, &low, size, span);
        let count = {
            let hi = self.local(&high, i64_ty, span);
            let lo = self.local(&low, i64_ty, span);
            self.binary(HirBinaryOp::Sub, hi, lo, i64_ty, span)
        };
        let name = self
            .tcx
            .def_name(match self.tcx.kind_of(view_ty) {
                TyKind::Adt { def, .. } => *def,
                _ => return None,
            })?
            .to_string();
        let sub = self.struct_lit(
            &name,
            vec![("__addr", address), ("__len", count)],
            view_ty,
            span,
        );
        Some(self.block(stmts, Some(sub), view_ty, span))
    }

    /// `v.to_vec()`: every element copied into a `Vec`.
    fn view_to_vec(
        &mut self,
        parts: &ViewParts,
        mut stmts: Vec<HirStmt>,
        span: Span,
    ) -> Option<HirExpr> {
        let ViewParts {
            elem,
            size,
            ref base,
            ref len,
            ..
        } = *parts;
        let unit = self.tcx.unit();
        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
        let out = self.fresh("out");
        let empty = self.array(Vec::new(), vec_ty, span);
        stmts.push(self.let_stmt(&out, true, vec_ty, empty, span));
        let at = self.fresh("i");
        let mut body = Vec::new();
        let address = self.element_at(base, &at, size, span);
        let value = self.load_element(address, elem, &mut body, span)?;
        let push = {
            let local = self.local(&out, vec_ty, span);
            self.method(local, "push", vec![value], unit, span)
        };
        body.push(self.expr_stmt(push, span));
        stmts.push(self.counted_loop(&at, len, body, span));
        let result = self.local(&out, vec_ty, span);
        Some(self.block(stmts, Some(result), vec_ty, span))
    }

    /// `v.copy_from(values)`: every element written from a sequence of the same length.
    fn view_copy_from(
        &mut self,
        parts: &ViewParts,
        mut stmts: Vec<HirStmt>,
        values: &HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        let ViewParts {
            elem,
            size,
            ref base,
            ref len,
            ..
        } = *parts;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let source = self.fresh("src");
        stmts.push(self.let_stmt(&source, false, values.ty, values.clone(), span));
        let count = self.fresh("n");
        let count_value = {
            let local = self.local(&source, values.ty, span);
            self.method(local, "len", Vec::new(), i64_ty, span)
        };
        stmts.push(self.let_stmt(&count, false, i64_ty, count_value, span));
        let check = {
            let len = self.local(len, i64_ty, span);
            let found = self.local(&count, i64_ty, span);
            self.call_named(VIEW_LEN_CHECK, vec![len, found], unit, span)
        };
        stmts.push(self.expr_stmt(check, span));
        let at = self.fresh("i");
        let mut body = Vec::new();
        let element = {
            let local = self.local(&source, values.ty, span);
            let index = self.local(&at, i64_ty, span);
            self.index(local, index, elem, span)
        };
        let address = self.element_at(base, &at, size, span);
        self.store_element(address, element, &mut body, span)?;
        stmts.push(self.counted_loop(&at, len, body, span));
        Some(self.block(stmts, None, unit, span))
    }

    /// `v.fill(value)`: every element written with one value.
    fn view_fill(
        &mut self,
        parts: &ViewParts,
        mut stmts: Vec<HirStmt>,
        value: &HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        let ViewParts {
            elem,
            size,
            ref base,
            ref len,
            ..
        } = *parts;
        let unit = self.tcx.unit();
        let filler = self.fresh("v");
        stmts.push(self.let_stmt(&filler, false, elem, value.clone(), span));
        let at = self.fresh("i");
        let mut body = Vec::new();
        let address = self.element_at(base, &at, size, span);
        let local = self.local(&filler, elem, span);
        self.store_element(address, local, &mut body, span)?;
        stmts.push(self.counted_loop(&at, len, body, span));
        Some(self.block(stmts, None, unit, span))
    }

    /// `__gos_ffi_view_check(index, len)` as a statement.
    fn view_check(&mut self, index: &str, len: &str, stmts: &mut Vec<HirStmt>, span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let at = self.local(index, i64_ty, span);
        let len = self.local(len, i64_ty, span);
        let check = self.call_named(VIEW_CHECK, vec![at, len], unit, span);
        stmts.push(self.expr_stmt(check, span));
    }

    /// The `ty` at `address`: a direct load for a scalar, a copy for a
    /// struct.
    fn load_element(
        &mut self,
        address: HirExpr,
        ty: Ty,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<HirExpr> {
        if let Some(value) = self.load_scalar(address.clone(), ty, span) {
            return Some(value);
        }
        self.read_struct_at(address, ty, stmts, span)
    }

    /// Stores `value` at `address`: directly for a scalar, by copy for a
    /// struct.
    fn store_element(
        &mut self,
        address: HirExpr,
        value: HirExpr,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> Option<()> {
        if let Some(store) = self.store_scalar(address.clone(), value.clone(), span) {
            stmts.push(self.expr_stmt(store, span));
            return Some(());
        }
        self.write_struct_at(address, value, stmts, span)
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

    /// The byte offset of the field `path` (`.`-separated) inside the
    /// `#[repr(C)]` struct `target`.
    fn field_offset(&mut self, target: Ty, path: &str) -> Option<u32> {
        let mut offset = 0u32;
        let mut current = target;
        for segment in path.split('.') {
            let TyKind::Adt { def, substs } = self.tcx.kind_of(current).clone() else {
                return None;
            };
            let names = self.fields.get(&def)?;
            let index = names.iter().position(|name| name.name == segment)?;
            let layout = self.tcx.plain_layout(current)?;
            offset += layout.field_offsets.get(index)?;
            current = *self.tcx.adt_field_tys(def, &substs)?.get(index)?;
        }
        Some(offset)
    }

    // ---- unions ---------------------------------------------------------

    /// The type under one reference, or `ty` itself.
    fn referent(tcx: &TyCtxt, ty: Ty) -> Ty {
        match tcx.kind_of(ty) {
            TyKind::Ref { inner, .. } => *inner,
            _ => ty,
        }
    }

    /// The C size of `ty` when it is an `ffi::Union<M>`.
    fn union_size(&self, ty: Ty) -> Option<u32> {
        let TyKind::Adt { def, substs } = self.tcx.kind_of(ty) else {
            return None;
        };
        self.tcx.union_members(*def, substs)?;
        Some(self.tcx.plain_layout(ty)?.size)
    }

    /// `Union { __bytes: [0; size] }`.
    fn union_zeroed(&mut self, union: Ty, span: Span) -> Option<HirExpr> {
        let TyKind::Adt { def, substs } = self.tcx.kind_of(union).clone() else {
            return None;
        };
        let bytes_ty = self.tcx.union_bytes_ty(def, &substs)?;
        let zero = self.zero(bytes_ty, span)?;
        let name = self.tcx.def_name(def)?.to_string();
        Some(self.struct_lit(&name, vec![("__bytes", zero)], union, span))
    }

    /// Copies the leading `size_of::<T>()` bytes of `src` (a `T` or a
    /// union) over those of a staged copy of `dst_value`, answering the
    /// staged value: the one reinterpretation every union operation is.
    fn reinterpret(
        &mut self,
        stmts: &mut Vec<HirStmt>,
        dst_value: HirExpr,
        src: HirExpr,
        len: u32,
        span: Span,
    ) -> Option<HirExpr> {
        let dst_ty = dst_value.ty;
        let src_ty = src.ty;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let unit = self.tcx.unit();
        let staged_ty = self.array_ty(dst_ty, 1);
        let staged = self.fresh("dst");
        let initial = self.array(vec![dst_value], staged_ty, span);
        stmts.push(self.let_stmt(&staged, true, staged_ty, initial, span));
        let source_ty = self.array_ty(src_ty, 1);
        let source = self.fresh("src");
        let source_init = self.array(vec![src], source_ty, span);
        stmts.push(self.let_stmt(&source, false, source_ty, source_init, span));
        let dst_layout = aggregate_layout(self.tcx, staged_ty)?;
        let src_layout = aggregate_layout(self.tcx, source_ty)?;
        let staged_ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: staged_ty,
        });
        let staged_ref = {
            let local = self.local(&staged, staged_ty, span);
            self.unary(HirUnaryOp::RefMut, local, staged_ref_ty, span)
        };
        let source_local = self.local(&source, source_ty, span);
        let len = self.int_lit(i64::from(len), u64_ty, span);
        let copy = self.routed_with_layouts(
            "gos_rt_ffi_copy",
            "RrL>v",
            &format!("{dst_layout}|{src_layout}|"),
            vec![staged_ref, source_local, len],
            unit,
            span,
        );
        stmts.push(self.expr_stmt(copy, span));
        let staged_expr = self.local(&staged, staged_ty, span);
        Some(self.index0(staged_expr, dst_ty, span))
    }

    /// `Union::new(value)`: a zeroed union whose leading bytes are `value`'s.
    fn union_new(&mut self, union: Ty, value: &HirExpr, span: Span) -> Option<HirExpr> {
        let len = self.c_size(value.ty)?;
        let zeroed = self.union_zeroed(union, span)?;
        let mut stmts = Vec::new();
        let result = self.reinterpret(&mut stmts, zeroed, value.clone(), len, span)?;
        Some(self.block(stmts, Some(result), union, span))
    }

    /// `u.get::<T>()` and `u.set(value)`.
    fn rewrite_union_method(
        &mut self,
        call: &HirExpr,
        receiver: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> Option<HirExpr> {
        let span = call.span;
        // A receiver taken by reference: the union is the place under it.
        let place = match &receiver.kind {
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut | HirUnaryOp::RefShared,
                operand,
            } => (**operand).clone(),
            _ => receiver.clone(),
        };
        match (method, args) {
            ("get", []) => {
                let member = call.ty;
                let len = self.c_size(member)?;
                let zero = self.zero(member, span)?;
                let mut stmts = Vec::new();
                let result = self.reinterpret(&mut stmts, zero, place, len, span)?;
                Some(self.block(stmts, Some(result), member, span))
            }
            ("set", [value]) => {
                let len = self.c_size(value.ty)?;
                let unit = self.tcx.unit();
                let mut stmts = Vec::new();
                let updated =
                    self.reinterpret(&mut stmts, place.clone(), value.clone(), len, span)?;
                stmts.push(self.assign_stmt(place, updated, span));
                Some(self.block(stmts, None, unit, span))
            }
            _ => None,
        }
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
        // The address takes the first layout slot.
        let mut layouts = vec![String::new()];
        let signature = self.callback_signature(&sig)?;
        for input in &sig.inputs {
            let name = self.fresh("x");
            params.push(self.param(&name, *input, span));
            let local = self.local(&name, *input, span);
            if let Some(value) = classify_value(self.tcx, *input) {
                call_args.push(self.value_in(local, &value, span));
                layouts.push(String::new());
            } else {
                // A struct crosses by value, packed by its layout.
                call_args.push(local);
                layouts.push(aggregate_layout(self.tcx, *input)?);
            }
        }
        let output = sig.output;
        let struct_ret = crate::foreign::struct_result(self.tcx, Some(output));
        let ret_value = if matches!(self.tcx.kind_of(output), TyKind::Unit) || struct_ret.is_some()
        {
            None
        } else {
            Some(classify_value(self.tcx, output)?)
        };
        let raw_ty = match &ret_value {
            Some(CValue::Ptr | CValue::OptPtr) => u64_ty,
            _ if struct_ret.is_some() => self.tcx.unit(),
            _ => output,
        };
        let mut stmts = Vec::new();
        let holder = match struct_ret {
            Some(result) => {
                let holder_ty = self.array_ty(result, 1);
                let holder = self.fresh("ret");
                let zero = self.zero(result, span)?;
                let initial = self.array(vec![zero], holder_ty, span);
                stmts.push(self.let_stmt(&holder, true, holder_ty, initial, span));
                let holder_ref_ty = self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: holder_ty,
                });
                let local = self.local(&holder, holder_ty, span);
                call_args.push(self.unary(HirUnaryOp::RefMut, local, holder_ref_ty, span));
                layouts.push(aggregate_layout(self.tcx, holder_ty)?);
                Some((holder, holder_ty, result))
            }
            None => None,
        };
        let raw = self.routed_with_layouts(
            INDIRECT_SYMBOL,
            &signature,
            &layouts.join("|"),
            call_args,
            raw_ty,
            span,
        );
        let body = match (&ret_value, holder) {
            (Some(value @ (CValue::Ptr | CValue::OptPtr)), _) => {
                self.value_out(raw, value, output, "ffi::fn_from_ptr", span)
            }
            (_, Some((holder, holder_ty, result))) => {
                stmts.push(self.expr_stmt(raw, span));
                let local = self.local(&holder, holder_ty, span);
                let tail = self.index0(local, result, span);
                self.block(stmts, Some(tail), output, span)
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
                if self.tcx.union_members(def, &substs).is_some() {
                    return self.union_zeroed(ty, span);
                }
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

    fn index(&mut self, base: HirExpr, index: HirExpr, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        )
    }

    fn method(
        &mut self,
        receiver: HirExpr,
        name: &str,
        args: Vec<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(receiver),
                name: Ident::new(name),
                args,
                owner: None,
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

/// An `ffi::View<T>` receiver bound for one method: its type, element type
/// and C size, and the bindings holding its base address and length.
struct ViewParts {
    view_ty: Ty,
    elem: Ty,
    size: u32,
    base: String,
    len: String,
}
