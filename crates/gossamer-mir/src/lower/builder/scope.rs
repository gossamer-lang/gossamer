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
use gossamer_types::{IntTy, Ty, TyCtxt, TyKind};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

impl<'a> Builder<'a> {
    pub(crate) fn push_local(&mut self, ty: Ty, debug_name: Option<Ident>, mutable: bool) -> Local {
        // `time::Duration` / `time::Instant` are transparent `i64`
        // newtypes: their distinct kinds exist only to steer method-form
        // accessor dispatch on the HIR side. Storage and codegen treat
        // them as an `i64`, so locals never carry those kinds.
        let ty = if matches!(self.tcx.kind_of(ty), TyKind::Duration | TyKind::Instant) {
            self.tcx.int_ty(IntTy::I64)
        } else if let Some(carrier) =
            crate::lower::helpers::const_generic_array_as_vec(self.tcx, ty)
            && !matches!(self.tcx.kind_of(ty), TyKind::Ref { .. })
        {
            // An array whose length is a const generic parameter has no
            // length until the call supplies one, so the body holds it as a
            // runtime-length sequence, as its parameters and return do.
            carrier
        } else {
            ty
        };
        let id = u32::try_from(self.locals.len()).expect("local overflow");
        self.locals.push(LocalDecl {
            ty,
            debug_name,
            mutable,
            region: self.region_depth > 0,
        });
        Local(id)
    }

    pub(crate) fn fresh(&mut self, ty: Ty) -> Local {
        self.push_local(ty, None, false)
    }

    pub(crate) fn bind_local(&mut self, name: &str, local: Local) {
        self.named_locals.insert(local);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), local);
        }
    }

    pub(crate) fn lookup_local(&self, name: &str) -> Option<Local> {
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.get(name) {
                return Some(*local);
            }
        }
        None
    }

    pub(crate) fn bind_reference_alias(&mut self, name: &str, local: Local) {
        if let Some(scope) = self.reference_aliases.last_mut() {
            scope.insert(name.to_string(), local);
        }
    }

    pub(crate) fn reference_alias_local(&self, name: &str) -> Option<Local> {
        for (scope, aliases) in self
            .scopes
            .iter()
            .rev()
            .zip(self.reference_aliases.iter().rev())
        {
            if scope.contains_key(name) {
                return aliases.get(name).copied();
            }
        }
        None
    }

    pub(crate) fn rebind_reference_alias(&mut self, name: &str, local: Local) -> bool {
        for (scope, aliases) in self
            .scopes
            .iter_mut()
            .rev()
            .zip(self.reference_aliases.iter_mut().rev())
        {
            if scope.contains_key(name) {
                if aliases.contains_key(name) {
                    scope.insert(name.to_string(), local);
                    aliases.insert(name.to_string(), local);
                    return true;
                }
                return false;
            }
        }
        false
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.reference_aliases.push(HashMap::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
        self.reference_aliases.pop();
    }

    /// Lowers the expressions in a defer frame in LIFO (reverse-registration)
    /// order for their side effects. A frame is emitted at every edge that
    /// leaves its block.
    pub(crate) fn emit_defer_frame(&mut self, frame: &[HirExpr]) {
        for expr in frame.iter().rev() {
            if self.current.is_none() {
                break;
            }
            let _ = self.lower_expr(expr);
        }
    }

    /// Emits every defer frame at index `>= from_depth`, innermost block first,
    /// without removing them - the owning `lower_block` calls pop their frames
    /// as control unwinds. `return` passes `0` (all frames); `break`/`continue`
    /// pass the target loop's `defer_depth` (only the frames inside the loop).
    pub(crate) fn emit_defers_above(&mut self, from_depth: usize) {
        let depth = self.defer_stack.len();
        for i in (from_depth..depth).rev() {
            let frame = self.defer_stack[i].clone();
            self.emit_defer_frame(&frame);
        }
    }

    pub(crate) fn new_block(&mut self, span: Span) -> BlockId {
        let id = BlockId(u32::try_from(self.blocks.len()).expect("block overflow"));
        self.blocks.push(BasicBlock {
            id,
            stmts: Vec::new(),
            terminator: Terminator::Unreachable,
            span,
            terminator_span: None,
            terminator_inlined: None,
        });
        id
    }

    pub(crate) fn set_current(&mut self, block: BlockId) {
        self.current = Some(block);
    }

    pub(crate) fn current_block(&mut self) -> &mut BasicBlock {
        let id = self.current.expect("no current block").0 as usize;
        &mut self.blocks[id]
    }

    pub(crate) fn emit_assign(&mut self, place: Place, rvalue: Rvalue, span: Span) {
        if self.current.is_none() {
            return;
        }
        let stmt = Statement {
            kind: StatementKind::Assign { place, rvalue },
            span,
            inlined: None,
        };
        self.current_block().stmts.push(stmt);
    }

    /// Emits a [`StatementKind::StaticStore`] writing `value` into the
    /// `static mut` cell `target`.
    pub(crate) fn emit_static_store(
        &mut self,
        target: crate::ir::StaticRef,
        value: Operand,
        span: Span,
    ) {
        if self.current.is_none() {
            return;
        }
        let stmt = Statement {
            kind: StatementKind::StaticStore { target, value },
            span,
            inlined: None,
        };
        self.current_block().stmts.push(stmt);
    }

    pub(crate) fn auto_deref_cell(&mut self, local: Local, span: Span) -> Local {
        let Some(kind) = self.local_runtime_kind.get(&local).copied() else {
            return local;
        };
        let (helper, dest_ty): (&'static str, Ty) = match kind {
            "flag::Cell::String" => ("gos_rt_flag_cell_load_str", self.tcx.string_ty()),
            "flag::Cell::Int" | "flag::Cell::Uint" => (
                "gos_rt_flag_cell_load_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // A duration cell's backing repr is the same `i64`-of-ms, but
            // its element type is the transparent `time::Duration` newtype
            // so the method-form accessors (`cell.as_millis()`) dispatch on
            // the receiver's static type. Duration normalizes back to i64
            // in `push_local` and unifies with `Int`, so arithmetic and
            // comparison on a deref'd duration cell are unaffected.
            "flag::Cell::Duration" => ("gos_rt_flag_cell_load_i64", self.tcx.duration_ty()),
            "flag::Cell::Bool" => ("gos_rt_flag_cell_load_bool", self.tcx.bool_ty()),
            "flag::Cell::Float" => (
                "gos_rt_flag_cell_load_f64",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            _ => return local,
        };
        let dest = self.fresh(dest_ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: helper,
                args: vec![Operand::Copy(Place::local(local))],
            },
            span,
        );
        dest
    }

    /// Convert a `char`-typed string-method needle to a one-char
    /// `String` via `gos_rt_char_to_str`. The `gos_rt_str_*` helpers
    /// read the needle as a `*const c_char`; a bare `char` lowers to an
    /// i32 codepoint, so passing it raw makes the helper dereference
    /// the codepoint as a pointer. Non-`char` args pass through.
    pub(crate) fn coerce_char_arg_to_str(&mut self, local: Local, span: Span) -> Local {
        let mut t = self.locals[local.0 as usize].ty;
        while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        if !matches!(self.tcx.kind_of(t), gossamer_types::TyKind::Char) {
            return local;
        }
        let string_ty = self.tcx.string_ty();
        let dest = self.fresh(string_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_char_to_str".to_string())),
            args: vec![Operand::Copy(Place::local(local))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    pub(crate) fn terminate(&mut self, terminator: Terminator) {
        if self.current.is_some() {
            let span = self.fn_span;
            // A `&mut self` call on a scalar receiver borrowed a slot the
            // backend materialised for it. The callee wrote through that
            // slot, so the receiver's place is reloaded from it on the way
            // out - the block the call continues into is this call's own and
            // still empty, so the reload lands ahead of everything that reads
            // the receiver again.
            let reload = match &terminator {
                Terminator::Call {
                    args,
                    target: Some(target),
                    ..
                } => match args.first() {
                    Some(Operand::Copy(place)) if place.projection.is_empty() => self
                        .mut_receiver_reloads
                        .remove(&place.local)
                        .map(|dest| (*target, dest, place.local)),
                    _ => None,
                },
                _ => None,
            };
            let terminator_span = self.expr_span;
            let block = self.current_block();
            block.terminator = terminator;
            block.terminator_span = Some(terminator_span);
            let _ = span;
            if let Some((target, dest, ref_local)) = reload {
                let stmt = crate::ir::Statement {
                    kind: crate::ir::StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::Use(Operand::Copy(Place {
                            local: ref_local,
                            projection: vec![crate::ir::Projection::Deref],
                        })),
                    },
                    span,
                    inlined: None,
                };
                self.blocks[target.0 as usize].stmts.insert(0, stmt);
            }
        }
        self.current = None;
    }

    /// Registers the child layout of a lifted closure's environment and
    /// answers the codegen symbol naming it.
    ///
    /// Word 0 holds the lifted body's address and is never a child, so
    /// capture `i` is word `i + 1`. The record shape is the one an enum
    /// variant uses, so release walks it with the machinery already in
    /// place.
    fn register_closure_env_meta(&mut self, name: &str, child_entries: &[i64]) -> String {
        use gossamer_abi::rc::RC_KIND_STRUCT;
        let sanitised: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let symbol = format!("gos_rc_meta_closure_{sanitised}");
        let mut blob = vec![RC_KIND_STRUCT, 1, 0, child_entries.len() as i64];
        blob.extend_from_slice(child_entries);
        self.tcx.register_rc_meta(symbol.clone(), blob);
        symbol
    }

    pub(crate) fn lower_lifted_closure(
        &mut self,
        name: &Ident,
        captures: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_abi::rc::{RC_CHILD_KIND_SHIFT, RC_CHILD_VEC};

        // Lower the captures first: the environment owns a share of every
        // RC-managed one, so its child layout is decided by their types.
        let mut capture_locals = Vec::with_capacity(captures.len());
        let mut vec_captures = vec![false; captures.len()];
        let mut child_entries: Vec<i64> = Vec::new();
        for (i, cap) in captures.iter().enumerate() {
            let value_local = self.lower_expr(cap)?;
            let value_ty = self.locals[value_local.0 as usize].ty;
            let word = i as i64 + 1;
            if self.tcx.is_rc_managed(value_ty) {
                child_entries.push(word);
            } else if matches!(
                self.tcx.kind_of(value_ty),
                gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
            ) {
                child_entries.push(word | (RC_CHILD_VEC << RC_CHILD_KIND_SHIFT));
                vec_captures[i] = true;
            }
            capture_locals.push(value_local);
        }
        // A closure capturing nothing the environment must release takes the
        // empty symbol, which lowers to a null meta pointer the runtime
        // treats as a leaf.
        let meta_symbol = if child_entries.is_empty() {
            String::new()
        } else {
            self.register_closure_env_meta(&name.name, &child_entries)
        };

        // Only the environment carries the closure's own type, so that type
        // says "owns a reference-counted environment" and the drop passes can
        // reclaim it from that alone. The sizes, offsets, body address, and
        // store sinks around it are plain words and say so.
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let unit_ty = self.tcx.unit();
        let size = i128::from((captures.len() + 1) as i64 * 8);
        let size_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(size_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(size))),
            span,
        );
        // The environment is the callable's carrying form, so it takes the
        // type that says an environment is owned here. A bare `fn` pointer
        // type names an address with nothing to reclaim, and reading this
        // allocation as one would leave it to leak.
        let env_ty = match self.tcx.kind_of(ty) {
            gossamer_types::TyKind::FnPtr(sig) => {
                let sig = sig.clone();
                self.tcx.intern(gossamer_types::TyKind::FnTrait(sig))
            }
            _ => ty,
        };
        let env_local = self.fresh(env_ty);
        self.emit_assign(
            Place::local(env_local),
            Rvalue::CallIntrinsic {
                name: "gos_rc_alloc",
                args: vec![
                    Operand::Copy(Place::local(size_local)),
                    Operand::Const(ConstValue::Str(meta_symbol)),
                ],
            },
            span,
        );
        let fn_addr_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(fn_addr_local),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(name.name.clone()))],
            },
            span,
        );
        let zero_offset_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_offset_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let sink = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(sink),
            Rvalue::CallIntrinsic {
                name: "gos_store",
                args: vec![
                    Operand::Copy(Place::local(env_local)),
                    Operand::Copy(Place::local(zero_offset_local)),
                    Operand::Copy(Place::local(fn_addr_local)),
                ],
            },
            span,
        );
        for (i, value_local) in capture_locals.into_iter().enumerate() {
            let offset = (i as i64 + 1) * 8;
            let offset_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(offset_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(offset)))),
                span,
            );
            // The environment's own share of a Vec capture. An RC-managed
            // capture is retained by the store, which the drop pass reads as
            // an acquisition; a Vec carries its count outside the RC header,
            // so its share is taken here, exactly as an enum payload's is.
            if vec_captures[i] {
                let retain_dest = self.fresh(unit_ty);
                self.emit_assign(
                    Place::local(retain_dest),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_vec_retain",
                        args: vec![Operand::Copy(Place::local(value_local))],
                    },
                    span,
                );
            }
            let sink = self.fresh(unit_ty);
            self.emit_assign(
                Place::local(sink),
                Rvalue::CallIntrinsic {
                    name: "gos_store",
                    args: vec![
                        Operand::Copy(Place::local(env_local)),
                        Operand::Copy(Place::local(offset_local)),
                        Operand::Copy(Place::local(value_local)),
                    ],
                },
                span,
            );
        }
        self.local_closure.insert(env_local, name.name.clone());
        Some(env_local)
    }

    pub(crate) fn receiver_local_from_path(&self, expr: &HirExpr) -> Option<Local> {
        if let HirExprKind::Path { segments, .. } = &expr.kind {
            let first = segments.first()?;
            return self.lookup_local(&first.name);
        }
        None
    }

    pub(crate) fn peek_collection_type(&self, expr: &HirExpr) -> Option<Ty> {
        let mut ty = self.peek_struct_type(expr)?;
        while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            gossamer_types::TyKind::Vec(_)
            | gossamer_types::TyKind::Slice(_)
            | gossamer_types::TyKind::Array { .. } => Some(ty),
            _ => None,
        }
    }

    pub(crate) fn peek_struct_type(&self, expr: &HirExpr) -> Option<Ty> {
        self.peek_struct_type_with_depth(expr, 0)
    }

    pub(crate) fn peek_struct_type_with_depth(&self, expr: &HirExpr, depth: u32) -> Option<Ty> {
        const MAX_PEEK_DEPTH: u32 = 16;
        if depth >= MAX_PEEK_DEPTH {
            // Surface the cap during compiler development so a
            // genuine depth-bomb HIR shape doesn't silently fall
            // through to the runtime-dispatch path.
            eprintln!(
                "gossamer-mir: peek_struct_type recursion cap reached ({MAX_PEEK_DEPTH}) - falling back to runtime dispatch"
            );
            return None;
        }
        if let Some(local) = self.receiver_local_from_path(expr) {
            return Some(self.locals[local.0 as usize].ty);
        }
        if !matches!(self.tcx.kind_of(expr.ty), gossamer_types::TyKind::Var(_)) {
            return Some(expr.ty);
        }
        if let HirExprKind::Field {
            receiver: parent,
            name,
        } = &expr.kind
        {
            let parent_ty = self.peek_struct_type_with_depth(parent, depth + 1)?;
            let mut peeled = parent_ty;
            while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            if let gossamer_types::TyKind::Adt { def, .. } = self.tcx.kind_of(peeled) {
                let sname = self.struct_defs.get(def)?;
                let order = self.structs.get(sname)?;
                let pos = order.iter().position(|f| f == &name.name)?;
                let tys = self.tcx.struct_field_tys(*def)?;
                return tys.get(pos).copied();
            }
        }
        None
    }

    pub(crate) fn peek_method_chain_kind(&self, expr: &HirExpr) -> Option<gossamer_types::TyKind> {
        use gossamer_types::TyKind;
        match &expr.kind {
            HirExprKind::MethodCall { name, .. } => match name.name.as_str() {
                "len" | "count" | "find" | "byte_at" | "as_i64" | "to_int" | "abs" | "pow"
                | "signum" => Some(TyKind::Int(gossamer_types::IntTy::I64)),
                "to_string" | "trim" | "to_lowercase" | "to_uppercase" | "replace" | "repeat"
                | "as_str" | "clone_str" | "message" => Some(TyKind::String),
                "is_empty" | "contains" | "starts_with" | "ends_with" | "is_some" | "is_none"
                | "is_ok" | "is_err" => Some(TyKind::Bool),
                _ => None,
            },
            // `args[i].method()` - the receiver is an `Index`
            // projection whose HIR type can be `Var(_)` when
            // multi-module typeck loses contact with the element
            // kind (single-file builds resolve `args[i]` to
            // `String` and never hit this arm). Pull the base
            // local's MIR-pinned type and unwrap one Vec / Slice /
            // Array layer to surface the concrete element kind.
            // Without this, `args[i].len()` falls through the
            // `len` dispatch's default arm to `gos_rt_arr_len`,
            // which reads a Vec header out of a `*const c_char`
            // string pointer and crashes inside `mov (%rdi),%rax`.
            HirExprKind::Index { base, .. } => {
                let base_local = self.receiver_local_from_path(base)?;
                let mut base_ty = self.locals[base_local.0 as usize].ty;
                while let TyKind::Ref { inner, .. } = self.tcx.kind_of(base_ty) {
                    base_ty = *inner;
                }
                let elem_ty = match self.tcx.kind_of(base_ty) {
                    TyKind::Vec(elem) | TyKind::Slice(elem) => *elem,
                    TyKind::Array { elem, .. } => *elem,
                    _ => return None,
                };
                let mut elem_kind = self.tcx.kind_of(elem_ty).clone();
                while let TyKind::Ref { inner, .. } = elem_kind {
                    elem_kind = self.tcx.kind_of(inner).clone();
                }
                if matches!(elem_kind, TyKind::Var(_)) {
                    return None;
                }
                Some(elem_kind)
            }
            // `<receiver>.<field>.method()` - the field's type is
            // resolvable from the receiver's local_struct (a known
            // stdlib or user struct registered in `self.structs`).
            // The HIR-level type on the field expression itself is
            // often a `Var` that lost contact with the field
            // declaration, so the typechecker route is
            // insufficient - but the structural lookup is.
            HirExprKind::Field { receiver, name } => {
                let recv_local = self.receiver_local_from_path(receiver)?;
                let struct_name = self.local_struct.get(&recv_local).cloned()?;
                let order = self.structs.get(&struct_name)?;
                let idx = order.iter().position(|f| f == name.name.as_str())?;
                // `stdlib_struct_shapes` only ships field names
                // today; tag the i-th field's `TyKind` from the
                // typechecker's struct-fields registry. Without
                // a typed registry we fall back to a default
                // i64 for the well-known counter / size fields
                // and String for the rest. Good enough for the
                // method-chain dispatch (`.to_string()` /
                // `.is_empty()` decisions).
                let bool_fields: &[&str] = &["is_dir", "is_file", "is_symlink", "is_empty"];
                let int_fields: &[&str] = &[
                    "size",
                    "modified_ms",
                    "len",
                    "code",
                    "status",
                    "year",
                    "month",
                    "day",
                    "hour",
                    "minute",
                    "second",
                ];
                let _ = idx;
                if bool_fields.contains(&name.name.as_str()) {
                    Some(TyKind::Bool)
                } else if int_fields.contains(&name.name.as_str()) {
                    Some(TyKind::Int(gossamer_types::IntTy::I64))
                } else {
                    Some(TyKind::String)
                }
            }
            _ => None,
        }
    }

    pub(crate) fn peek_define_deref_kind(&self, expr: &HirExpr) -> Option<gossamer_types::TyKind> {
        use gossamer_types::TyKind;
        let HirExprKind::Unary {
            op: HirUnaryOp::Deref,
            operand,
        } = &expr.kind
        else {
            return None;
        };
        let HirExprKind::Field {
            receiver,
            name: field_name,
        } = &operand.kind
        else {
            return None;
        };
        let receiver_local = self.receiver_local_from_path(receiver)?;
        let layout = self.local_define_layout.get(&receiver_local)?;
        let (_, cell_kind) = layout
            .iter()
            .find(|(long, _)| long == field_name.name.as_str())?;
        match *cell_kind {
            "flag::Cell::Int" | "flag::Cell::Uint" => Some(TyKind::Int(gossamer_types::IntTy::I64)),
            // The transparent Duration newtype keeps the accessor dispatch
            // on the cell's element type; its repr is still `i64`-of-ms.
            "flag::Cell::Duration" => Some(TyKind::Duration),
            "flag::Cell::Float" => Some(TyKind::Float(gossamer_types::FloatTy::F64)),
            "flag::Cell::Bool" => Some(TyKind::Bool),
            "flag::Cell::String" => Some(TyKind::String),
            _ => None,
        }
    }
}
