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
    pub(crate) fn lower_block(&mut self, block: &HirBlock) -> Option<Local> {
        // Function bodies begin with Builder's one root scope. Only nested
        // blocks may acquire an implicit lexical region: a function's result
        // is an externally visible escape boundary, while a nested block is
        // accepted only when its tail is Copy (checked by the analysis).
        // Never layer an automatic region inside a source-visible one.
        let lexical_region = self.scopes.len() > 1
            && self.region_depth == 0
            && matches!(
                crate::lower::helpers::LoopEligibility::new(&*self.tcx, self.region_unsafe)
                    .decide_lexical_block(block),
                crate::lower::helpers::RegionDecision::Region
            );
        if lexical_region {
            self.emit_region_call("gos_rt_arena_push", block.span);
            self.region_depth += 1;
            self.deferred_auto_region_collections.push(false);
        }
        self.push_scope();
        self.defer_stack.push(Vec::new());
        for stmt in &block.stmts {
            self.lower_stmt(stmt);
            if self.current.is_none() {
                // Diverged mid-block (a `return` / `break` / `continue` inside
                // a statement). That construct already emitted the defers it
                // needed; drop this frame without re-emitting.
                self.defer_stack.pop();
                self.pop_scope();
                // Eligibility rejects all early exits, so this is defensive
                // only; keeping the pop here preserves the region stack if a
                // future lowering rule gains another diverging expression.
                self.end_auto_region(lexical_region, block.span);
                return None;
            }
        }
        let result = match block.tail.as_ref() {
            Some(tail) => self.lower_expr(tail),
            // Tail-less block whose flow didn't diverge yields the
            // unit value. Without this the caller (e.g. `lower_if`'s
            // else arm) sees `None` and skips the join `Goto`,
            // leaving the post-statement block with the default
            // `Unreachable` terminator - `let _ = fn_call()` as
            // the last statement of an else block crashed
            // compiled binaries with `ud2`.
            None => {
                if self.current.is_some() {
                    Some(self.lower_unit(block.span))
                } else {
                    None
                }
            }
        };
        // Block-scoped `defer`: on a normal (non-diverging) exit, run this
        // block's deferred expressions LIFO after the block's value is
        // computed. A diverging tail (e.g. `return`) leaves `current` None and
        // has already emitted the frames itself.
        let frame = self.defer_stack.pop().unwrap_or_default();
        // Snapshot the block's value into a fresh local before the deferred
        // expressions run, so a defer that mutates a binding the tail names
        // (`{ defer t += 1; t }`) cannot change the value the block yields -
        // the value is the binding's state at the tail, not after the defer.
        let result = if self.current.is_some() && !frame.is_empty() {
            result.map(|r| {
                let ty = self.locals[r.0 as usize].ty;
                let snap = self.fresh(ty);
                self.emit_assign(
                    Place::local(snap),
                    Rvalue::Use(Operand::Copy(Place::local(r))),
                    block.span,
                );
                snap
            })
        } else {
            result
        };
        if self.current.is_some() {
            self.emit_defer_frame(&frame);
        }
        self.pop_scope();
        self.end_auto_region(lexical_region, block.span);
        if self.current.is_none() { None } else { result }
    }

    /// If `value` is the freshly-produced result of a call or inline enum
    /// constructor, rewrite that producer to write `binding` directly. The
    /// result temporary has no source-language identity and its only use is
    /// this binding, so copying it cannot be observed. This is ordinary return
    /// value copy elision, not ownership transfer between user bindings.
    ///
    /// Besides avoiding a redundant scalar or aggregate copy, direct binding
    /// is essential for aggregates containing Vec fields. Deep-cloning the Vec
    /// out of a fresh function result allocated a new non-region buffer on
    /// every loop iteration, while the dead temporary's region cleanup could
    /// not release that detached buffer.
    fn try_rebind_ctor_call(&mut self, value: Local, binding: Local) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        // Only a fresh temporary may be re-pointed. Once a local is published
        // under a name it is that binding's own storage, and stealing the call
        // that defines it leaves the earlier name reading a slot nothing ever
        // writes - `let a = Vec::new()` followed by `let b = a` would hand `b`
        // the constructor and leave `a` uninitialised.
        if self.named_locals.contains(&value) {
            return false;
        }
        // Calls lower to a terminator in a prior block whose continuation is
        // the current block. Their destination temporary is fresh by
        // construction and is consumed immediately by this let binding.
        for blk in &mut self.blocks {
            if let Terminator::Call {
                callee,
                destination,
                target: Some(t),
                ..
            } = &mut blk.terminator
                && *t == cur
                && destination.local == value
                && destination.projection.is_empty()
                && matches!(callee, Operand::Const(ConstValue::Str(name)) if is_container_ctor(name))
            {
                *destination = Place::local(binding);
                return true;
            }
        }
        // `Some(..)` / `Ok(..)` / `Err(..)` lower to a `gos_rt_result_new`
        // `CallIntrinsic` assignment - the last statement of the current block
        // (the binding copy has not been emitted yet).
        let cur_idx = cur.0 as usize;
        if cur_idx < self.blocks.len()
            && let Some(last) = self.blocks[cur_idx].stmts.last_mut()
            && let StatementKind::Assign {
                place,
                rvalue: Rvalue::CallIntrinsic { name, .. },
            } = &mut last.kind
            && *name == "gos_rt_result_new"
            && place.local == value
            && place.projection.is_empty()
        {
            place.local = binding;
            return true;
        }
        false
    }

    fn is_fresh_user_call_result(&self, value: Local) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        self.blocks.iter().any(|block| {
            matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::FnRef { .. },
                    destination,
                    target: Some(target),
                    ..
                } if *target == cur
                    && destination.local == value
                    && destination.projection.is_empty()
            )
        })
    }

    /// `true` when `value` is the payload of a carrier one of this frame's own
    /// calls answered - what `let x = f(..)?` leaves behind. The carrier hands
    /// its payload over with it, so the value has no source-language identity
    /// of its own any more than a call result has one: the binding takes the
    /// handles as they are, and the RC schedule gives the caller's share back.
    pub(crate) fn is_owned_carrier_payload(&self, value: Local) -> bool {
        let Some(carrier) = self
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .find_map(|stmt| {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    return None;
                };
                if place.local != value || !place.projection.is_empty() {
                    return None;
                }
                let Rvalue::CallIntrinsic { name, args } = rvalue else {
                    return None;
                };
                if !matches!(
                    *name,
                    "gos_rt_result_payload"
                        | "gos_rt_result_payload_f64"
                        | "gos_enum_load"
                        | "gos_enum_slot_ptr"
                ) {
                    return None;
                }
                match args.first() {
                    Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local),
                    _ => None,
                }
            })
        else {
            return false;
        };
        self.blocks.iter().any(|block| {
            matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::FnRef { .. },
                    destination,
                    ..
                } if destination.local == carrier && destination.projection.is_empty()
            )
        })
    }

    /// `true` when `local` is a binding that took a carrier payload this frame
    /// owns. Such a binding holds the one reference the carrier handed over, so
    /// a by-value use of it is a move exactly as a use of a fresh call result
    /// is - copying it would leave the original with no owner.
    pub(crate) fn holds_owned_carrier_payload(&self, local: Local) -> bool {
        self.blocks.iter().flat_map(|b| b.stmts.iter()).any(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } if place.local == local
                    && place.projection.is_empty()
                    && src.projection.is_empty()
                    && self.is_owned_carrier_payload(src.local)
            )
        })
    }

    fn is_vec_like_ty(&self, ty: gossamer_types::Ty) -> bool {
        matches!(self.tcx.kind_of(ty), gossamer_types::TyKind::Vec(_))
    }

    /// Runtime clone symbol for a `Map`/`BTreeMap` (`gos_rt_map_clone`) or
    /// `Set`/`BTreeSet` (`gos_rt_set_clone`) type, or `None` for anything
    /// else. Neither `GosMap` nor `GosSet` carries a refcount of its own
    /// (unlike `GosVec` / strings), so a plain pointer copy at a `let`
    /// binding or by-value argument either double-frees the table once both
    /// bindings' drop points run, or leaves both bindings mutating the same
    /// live table - the same hazard `is_vec_like_ty` closes for `Vec`.
    pub(crate) fn map_or_set_clone_symbol(&self, ty: gossamer_types::Ty) -> Option<&'static str> {
        const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
        const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
        const VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
        const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
        const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
        const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
        const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;
        match self.tcx.kind_of(ty) {
            gossamer_types::TyKind::HashMap { .. } => Some("gos_rt_map_clone"),
            gossamer_types::TyKind::Adt { def, .. }
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
            {
                Some("gos_rt_set_clone")
            }
            // A slot container reaches its storage through a handle, so a
            // binding taken from one copies the storage rather than the
            // handle. A heap IS a `GosVec`; a deque, a queue, and a stack
            // share the `GosDeque` header.
            gossamer_types::TyKind::Adt { def, .. }
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                Some("gos_rt_vec_clone")
            }
            gossamer_types::TyKind::Adt { def, .. } if def.local == VEC_DEQUE_DEF_LOCAL => {
                Some("gos_rt_deque_clone")
            }
            gossamer_types::TyKind::Adt { def, .. } if def.local == VEC_QUEUE_DEF_LOCAL => {
                Some("gos_rt_queue_clone")
            }
            gossamer_types::TyKind::Adt { def, .. } if def.local == VEC_STACK_DEF_LOCAL => {
                Some("gos_rt_stack_clone")
            }
            _ => None,
        }
    }

    /// Same as [`Self::map_or_set_clone_symbol`], but for a local whose MIR
    /// type has already been normalized to `i64` - the canonical
    /// representation every `Set`/`BTreeSet`-producing constructor gives its
    /// destination local (`lower_collections_free`, `lower_set_from_array`,
    /// `lower_set_from_sequence`), tracking the real element/ownership shape
    /// only through the `local_runtime_kind` side table so method dispatch
    /// still resolves. A path expression reading that local inherits the
    /// same `i64` type, so [`Self::map_or_set_clone_symbol`]'s `Ty`-based
    /// check can never see a `Set` there - this checks the tag instead.
    pub(crate) fn set_clone_symbol_for_local(&self, local: Local) -> Option<&'static str> {
        match self.local_runtime_kind.get(&local) {
            Some(&("collections::HashSet" | "collections::BTreeSet")) => Some("gos_rt_set_clone"),
            // The slot containers reach their storage through a handle the
            // same way a `Set` does, so a binding taken from one copies the
            // storage rather than the handle. A heap IS a `GosVec`; a deque,
            // a queue, and a stack share the `GosDeque` header.
            Some(&("collections::MinHeap" | "collections::MaxHeap")) => Some("gos_rt_vec_clone"),
            Some(&"collections::VecDeque") => Some("gos_rt_deque_clone"),
            Some(&"collections::VecQueue") => Some("gos_rt_queue_clone"),
            Some(&"collections::VecStack") => Some("gos_rt_stack_clone"),
            _ => None,
        }
    }

    pub(crate) fn emit_vec_clone_binding(&mut self, value: Local, binding: Local, span: Span) {
        self.emit_clone_call_binding(value, binding, span, "gos_rt_vec_clone");
    }

    /// Emits `binding = <symbol>(value)` and continues in the fallthrough
    /// block. Shared by [`Self::emit_vec_clone_binding`] and the `Map` /
    /// `Set` clone path in [`Self::emit_owned_clone_binding`].
    fn emit_clone_call_binding(
        &mut self,
        value: Local,
        binding: Local,
        span: Span,
        symbol: &'static str,
    ) {
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(symbol.to_string())),
            args: vec![Operand::Copy(Place::local(value))],
            destination: Place::local(binding),
            target: Some(next),
        });
        self.set_current(next);
    }

    /// Copies an owned value into `binding`, cloning every growable vector
    /// header nested in a by-value struct or tuple. A flat aggregate memcpy is
    /// sufficient for fixed storage and RC-managed scalar children, but it
    /// would otherwise let a later `push` through the copy replace the source
    /// value's vector buffer in the LLVM and Cranelift tiers.
    pub(crate) fn emit_owned_clone_binding(&mut self, value: Local, binding: Local, span: Span) {
        use gossamer_types::TyKind;

        let ty = self.locals[binding.0 as usize].ty;
        if matches!(self.tcx.kind_of(ty), TyKind::Vec(_)) {
            self.emit_vec_clone_binding(value, binding, span);
            return;
        }
        if let Some(symbol) = self
            .map_or_set_clone_symbol(ty)
            .or_else(|| self.set_clone_symbol_for_local(value))
        {
            self.emit_clone_call_binding(value, binding, span, symbol);
            // A `Set`/`BTreeSet` local's MIR type is the opaque-pointer
            // `i64` every constructor gives it (see
            // `Self::set_clone_symbol_for_local`), not the checker's `Adt`
            // sentinel - `{:?}` / method dispatch on the clone recover its
            // real shape only through this tag, which a fresh destination
            // local does not otherwise inherit from its source.
            if let Some(rk) = self.local_runtime_kind.get(&value).copied() {
                self.local_runtime_kind.insert(binding, rk);
            }
            return;
        }

        self.emit_assign(
            Place::local(binding),
            Rvalue::Use(Operand::Copy(Place::local(value))),
            span,
        );

        for (path, kind) in crate::lower::aggregate_rc_field_paths(self.tcx, ty) {
            // A map field is not cloned here: the drop pass books one
            // `gos_rt_map_field_clone` per aggregate copy, and a second swap
            // would orphan the first clone.
            if kind != crate::lower::FieldRcKind::Vec {
                continue;
            }
            let mut field_ty = ty;
            let mut place = Place::local(binding);
            let mut valid = true;
            for index in path {
                field_ty = match self.tcx.kind_of(field_ty) {
                    TyKind::Adt { def, substs } => self
                        .tcx
                        .adt_field_tys(*def, substs)
                        .and_then(|fields| fields.get(index as usize).copied())
                        .unwrap_or_else(|| {
                            valid = false;
                            field_ty
                        }),
                    TyKind::Tuple(fields) => {
                        fields.get(index as usize).copied().unwrap_or_else(|| {
                            valid = false;
                            field_ty
                        })
                    }
                    TyKind::Array { elem, len } if (index as usize) < len.to_usize() => *elem,
                    _ => {
                        valid = false;
                        field_ty
                    }
                };
                place.projection.push(crate::ir::Projection::Field(index));
            }
            if !valid
                || !matches!(
                    self.tcx.kind_of(field_ty),
                    TyKind::Vec(_) | TyKind::Slice(_)
                )
            {
                continue;
            }
            let cloned = self.fresh(field_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_clone".to_string())),
                args: vec![Operand::Copy(place.clone())],
                destination: Place::local(cloned),
                target: Some(next),
            });
            self.set_current(next);
            self.emit_assign(
                place,
                Rvalue::Use(Operand::Copy(Place::local(cloned))),
                span,
            );
        }
    }

    pub(crate) fn lower_stmt(&mut self, stmt: &HirStmt) {
        match &stmt.kind {
            HirStmtKind::Let { pattern, ty, init } => {
                let local = self.push_local(*ty, param_name(pattern), param_mutable(pattern));
                // NOTE: do NOT bind the name yet. `let x = expr`
                // must evaluate `expr` in the *outer* scope so a
                // shadowing form like `let x = x + 1` reads the
                // previous binding. We `bind_local` only after
                // `lower_expr(init)` has resolved every name.
                // `lower_let_array_as_vec` (the Vec annotation
                // shortcut) below also defers the bind to its own
                // post-init point.
                {
                    use gossamer_types::TyKind;
                    let binding_wants_vec = matches!(self.tcx.kind_of(*ty), TyKind::Vec(_));
                    if binding_wants_vec {
                        if let Some(init_expr) = init.as_ref() {
                            if let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) =
                                &init_expr.kind
                            {
                                if self.lower_let_array_as_vec(local, elems, stmt.span) {
                                    if let HirPatKind::Binding { name, .. } = &pattern.kind {
                                        self.bind_local(&name.name, local);
                                    }
                                    return;
                                }
                            }
                        }
                    }
                }
                // A non-rebindable direct reference binding aliases its source
                // place. Do not materialise a copied fixed array or scalar in
                // `local`: bind the user name to the source local so reads and
                // projected stores share the same storage.
                //
                // A mutable binding of an aggregate reference is different:
                // `let mut cursor = &head; cursor = next` rebinds the pointer,
                // it does not assign through the reference. Aggregate values
                // already have pointer representation, so keep that pointer in
                // the binding's own `&T` local. Aliasing `cursor` directly to
                // the owning `head` local made the later rebind release and
                // overwrite `head`, corrupting recursive-list ownership in
                // compiled tiers.
                if let HirPatKind::Binding { name, .. } = &pattern.kind
                    && let Some(HirExpr {
                        kind: HirExprKind::Unary { op, operand },
                        ..
                    }) = init
                    && matches!(op, HirUnaryOp::RefShared | HirUnaryOp::RefMut)
                    && let HirExprKind::Path { segments, .. } = &operand.kind
                    && let [source] = segments.as_slice()
                    && let Some(source_local) = self.lookup_local(&source.name)
                {
                    if param_mutable(pattern)
                        && !matches!(
                            self.tcx.kind_of(self.locals[source_local.0 as usize].ty),
                            gossamer_types::TyKind::Int(_)
                                | gossamer_types::TyKind::Float(_)
                                | gossamer_types::TyKind::Bool
                                | gossamer_types::TyKind::Char
                                | gossamer_types::TyKind::String
                        )
                    {
                        self.emit_assign(
                            Place::local(local),
                            Rvalue::Ref {
                                mutable: matches!(op, HirUnaryOp::RefMut),
                                place: Place::local(source_local),
                            },
                            stmt.span,
                        );
                        self.bind_local(&name.name, local);
                    } else {
                        self.bind_local(&name.name, source_local);
                        self.bind_reference_alias(&name.name, source_local);
                    }
                    return;
                }
                if let Some(init) = init {
                    // A runtime-sized repeat (`let a = [value; n]`) is a heap
                    // Vec. Build it directly in the binding. Lowering it as a
                    // general expression first produced a temporary Vec and
                    // then applied ordinary Vec value semantics, deep-cloning
                    // the complete buffer into `a`. Large numeric buffers
                    // therefore used twice their required memory and paid an
                    // avoidable full-buffer copy before useful work began.
                    if let HirExprKind::Array(gossamer_hir::HirArrayExpr::Repeat { value, count }) =
                        &init.kind
                        && (matches!(self.tcx.kind_of(init.ty), gossamer_types::TyKind::Vec(_))
                            || self.static_repeat_len(init.ty, count).is_none())
                        && self
                            .lower_array_repeat_into(value, count, init.ty, stmt.span, Some(local))
                            .is_some()
                    {
                        if let HirPatKind::Binding { name, .. } = &pattern.kind {
                            self.bind_local(&name.name, local);
                        }
                        return;
                    }
                    if let Some(mut value) = self.lower_expr(init) {
                        // Coerce a `json::Value`-typed initialiser
                        // when the binding has an explicit primitive
                        // / String annotation. `let low: i64 =
                        // root.latency.low_ms` becomes
                        // `gos_rt_json_as_i64(root.get("latency").get("low_ms"))`
                        // - keeps the user's natural notation while
                        // funnelling the dynamic-shape tax through
                        // the runtime helpers.
                        let value_ty = self.locals[value.0 as usize].ty;
                        if self.is_json_value_ty(value_ty) && !self.is_json_value_ty(*ty) {
                            if let Some(coerced) =
                                self.maybe_coerce_json_value(value, *ty, stmt.span)
                            {
                                value = coerced;
                            }
                        }
                        // Callable-shape coercion: when `let f:
                        // fn(...) -> ... = bare_fn` (or
                        // `Fn(...) -> ...`) is written, wrap the
                        // bare fn item in the env+code blob so
                        // every callable slot in the program
                        // uniformly carries an env_ptr. Without
                        // this, a later `f(...)` call site would
                        // see the raw fn address and skip the
                        // env-load step, segfaulting on access.
                        {
                            use gossamer_types::TyKind;
                            let value_ty_now = self.locals[value.0 as usize].ty;
                            let dest_callable = matches!(
                                self.tcx.kind_of(*ty),
                                TyKind::FnPtr(_) | TyKind::FnTrait(_)
                            );
                            let src_is_fn_def =
                                matches!(self.tcx.kind_of(value_ty_now), TyKind::FnDef { .. });
                            // Lift-closed produces a Path lowered
                            // to `Const(Str(name))` whose local is
                            // marked in `local_fn_name`. Treat it
                            // the same as a bare fn item for
                            // callable-slot wrapping.
                            let src_names_fn = self.local_fn_name.contains_key(&value);
                            if dest_callable && (src_is_fn_def || src_names_fn) {
                                value = self.coerce_to_fn_trait_if_needed(value, *ty, stmt.span);
                            }
                        }
                        // When the HIR-recorded type is an
                        // unresolved inference variable, pin the
                        // binding's MIR type to whatever the lowered
                        // initialiser settled on - keeps downstream
                        // passes (string-concat, codegen cl-type
                        // inference) grounded on concrete kinds.
                        let init_ty = self.locals[value.0 as usize].ty;
                        {
                            use gossamer_types::TyKind;
                            let binding_kind = self.tcx.kind_of(self.locals[local.0 as usize].ty);
                            let init_kind = self.tcx.kind_of(init_ty);
                            // When the binding's annotation is an
                            // Adt wrapper but the initialiser
                            // settled on a concrete scalar / String
                            // (typical of `let v = r.unwrap()` for
                            // `r: Result<T, E>` where the compiled
                            // tier flattens the wrapper), promote
                            // the binding to the scalar type so
                            // downstream printing + arithmetic find
                            // the right kind. The other concrete
                            // annotations (struct, vec, tuple, …)
                            // are kept verbatim because they
                            // typically come from explicit user
                            // annotations the typechecker has
                            // already validated against the value.
                            let promote_inner = matches!(binding_kind, TyKind::Adt { .. })
                                && matches!(
                                    init_kind,
                                    TyKind::Bool
                                        | TyKind::Char
                                        | TyKind::Int(_)
                                        | TyKind::Float(_)
                                        | TyKind::String
                                );
                            // The initialiser's runtime kind tells us
                            // when the binding is a runtime handle
                            // (flag cell, http response, regex
                            // pattern, …) that the typechecker has
                            // collapsed to a primitive. The cell
                            // value is pointer-shaped at runtime, so
                            // the binding's MIR type must be widened
                            // to i64 - keeping it as bool/i8 makes
                            // cranelift store the byte-truncated
                            // pointer value, and later loads return
                            // garbage. This is the cli_args / flag
                            // bool reproducer the test suite caught.
                            let init_rk = self.local_runtime_kind.get(&value).copied();
                            let promote_handle = init_rk.is_some_and(|rk| {
                                rk.starts_with("flag::Cell::")
                                    || rk.starts_with("http::")
                                    || rk.starts_with("regex::")
                                    || rk.starts_with("bufio::")
                                    || rk == "errors::Error"
                                    || rk == "flag::Set"
                            }) && matches!(
                                binding_kind,
                                TyKind::Bool
                                    | TyKind::Char
                                    | TyKind::Int(_)
                                    | TyKind::Float(_)
                                    | TyKind::String
                            );
                            // Promote `Array { elem: Var/Error, len }`
                            // bindings to the init's resolved Array
                            // type so {:?} dispatch can classify the
                            // elem (otherwise the print path falls
                            // back to the `<value>` placeholder).
                            let promote_array_elem = match (binding_kind, init_kind) {
                                (
                                    TyKind::Array { elem: be, .. },
                                    TyKind::Array { elem: ie, .. },
                                ) => {
                                    let be_unresolved = matches!(
                                        self.tcx.kind_of(*be),
                                        TyKind::Var(_) | TyKind::Error
                                    );
                                    let ie_resolved = !matches!(
                                        self.tcx.kind_of(*ie),
                                        TyKind::Var(_) | TyKind::Error
                                    );
                                    be_unresolved && ie_resolved
                                }
                                _ => false,
                            };
                            let promote_vec_elem = match (binding_kind, init_kind) {
                                (TyKind::Vec(be), TyKind::Vec(ie)) => {
                                    let be_unresolved = matches!(
                                        self.tcx.kind_of(*be),
                                        TyKind::Var(_) | TyKind::Error
                                    );
                                    let ie_resolved = !matches!(
                                        self.tcx.kind_of(*ie),
                                        TyKind::Var(_) | TyKind::Error
                                    );
                                    be_unresolved && ie_resolved
                                }
                                _ => false,
                            };
                            if !matches!(
                                binding_kind,
                                TyKind::Bool
                                    | TyKind::Char
                                    | TyKind::Int(_)
                                    | TyKind::Float(_)
                                    | TyKind::String
                                    | TyKind::Vec(_)
                                    | TyKind::Array { .. }
                                    | TyKind::Slice(_)
                                    | TyKind::Adt { .. }
                                    | TyKind::Tuple(_)
                                    | TyKind::Ref { .. }
                                    | TyKind::HashMap { .. }
                            ) || promote_inner
                                || promote_handle
                                || promote_array_elem
                                || promote_vec_elem
                            {
                                self.locals[local.0 as usize].ty = init_ty;
                            }
                        }
                        if let Some(struct_name) = self.local_struct.get(&value).cloned() {
                            self.local_struct.insert(local, struct_name);
                        }
                        if let Some(elem) = self.local_elem_struct.get(&value).cloned() {
                            self.local_elem_struct.insert(local, elem);
                        }
                        if let Some(closure_name) = self.local_closure.get(&value).cloned() {
                            self.local_closure.insert(local, closure_name);
                        }
                        if let Some(fn_name) = self.local_fn_name.get(&value).cloned() {
                            self.local_fn_name.insert(local, fn_name);
                        }
                        if let Some(rk) = self.local_runtime_kind.get(&value).copied() {
                            self.local_runtime_kind.insert(local, rk);
                        }
                        if self
                            .local_runtime_kind
                            .get(&value)
                            .is_some_and(|rk| *rk == "collections::BinaryHeap")
                            && self
                                .binary_heap_elem_is_reverse_i64(self.locals[local.0 as usize].ty)
                        {
                            self.local_binary_heap_min_i64.insert(value);
                            self.local_binary_heap_min_i64.insert(local);
                        } else if self.local_binary_heap_min_i64.contains(&value) {
                            self.local_binary_heap_min_i64.insert(local);
                        }
                        if let Some(layout) = self.local_define_layout.get(&value).cloned() {
                            self.local_define_layout.insert(local, layout);
                        }
                        // Bind fresh call and constructor results directly.
                        if !self.try_rebind_ctor_call(value, local) {
                            let init_ty = self.locals[value.0 as usize].ty;
                            let binding_ty = self.locals[local.0 as usize].ty;
                            if gossamer_hir::is_capture_env_load(init) {
                                // A closure capture aliases the enclosing
                                // scope's value, so a heap type mutated
                                // through it reaches the original.
                                self.emit_assign(
                                    Place::local(local),
                                    Rvalue::Use(Operand::Copy(Place::local(value))),
                                    stmt.span,
                                );
                            } else if gossamer_hir::is_capture_env_load(init)
                                || self.is_fresh_user_call_result(value)
                                || self.is_owned_carrier_payload(value)
                                || self.holds_owned_carrier_payload(value)
                                // A `.clone()` result already took its deep
                                // per-field copy at the dispatch, so the
                                // binding takes the handles as they are - a
                                // second walk would clone once per hop.
                                || matches!(
                                    &init.kind,
                                    gossamer_hir::HirExprKind::MethodCall { name, .. }
                                        if name.name.as_str() == "clone"
                                )
                            {
                                // Neither shape owns a fresh value. A closure's
                                // capture prologue reads the environment slot,
                                // and the closure observes the captured value
                                // rather than a copy of it; a call-result
                                // temporary has no source-language identity.
                                // Copy the aggregate words and managed child
                                // handles so RC insertion retains the children,
                                // rather than deep-cloning a nested Vec into a
                                // detached buffer whose element metadata no
                                // longer describes the original.
                                self.emit_assign(
                                    Place::local(local),
                                    Rvalue::Use(Operand::Copy(Place::local(value))),
                                    stmt.span,
                                );
                            } else if self.is_vec_like_ty(init_ty)
                                && self.is_vec_like_ty(binding_ty)
                                || matches!(
                                    self.tcx.kind_of(binding_ty),
                                    gossamer_types::TyKind::Adt { .. }
                                        | gossamer_types::TyKind::Tuple(_)
                                        | gossamer_types::TyKind::Array { .. }
                                        | gossamer_types::TyKind::HashMap { .. }
                                )
                                || self.set_clone_symbol_for_local(value).is_some()
                            {
                                self.emit_owned_clone_binding(value, local, stmt.span);
                            } else {
                                self.emit_assign(
                                    Place::local(local),
                                    Rvalue::Use(Operand::Copy(Place::local(value))),
                                    stmt.span,
                                );
                            }
                        }
                        match &pattern.kind {
                            HirPatKind::Tuple(sub_patterns) => {
                                self.bind_tuple_pattern(local, sub_patterns, stmt.span);
                            }
                            HirPatKind::Struct { .. } | HirPatKind::Variant { .. } => {
                                self.bind_aggregate_let_pattern(local, pattern, stmt.span);
                            }
                            HirPatKind::Or(branches) => {
                                self.bind_or_let_pattern(local, branches, stmt.span);
                            }
                            _ => {}
                        }
                    }
                }
                // Bind the user-name AFTER the init has been
                // lowered, so a shadowing form like
                // `let x = x + 1` reads the previous `x` while
                // evaluating the RHS.
                if let HirPatKind::Binding { name, .. } = &pattern.kind {
                    self.bind_local(&name.name, local);
                }
            }
            HirStmtKind::Expr { expr, .. } => {
                let _ = self.lower_expr(expr);
            }
            HirStmtKind::Defer(expr) => {
                // Register for block-scoped execution: the expression runs
                // (LIFO) when control leaves the enclosing block, emitted by
                // `lower_block` (normal exit) or by `return` / `break` /
                // `continue` (the exit edges).
                if let Some(frame) = self.defer_stack.last_mut() {
                    frame.push(expr.clone());
                }
            }
            HirStmtKind::Item(_) => {}
        }
    }
}

/// Runtime constructors whose results are fresh owned values. Other runtime
/// calls may return a borrowed or write-back-related handle and must keep their
/// original temporary so the drop pass can honor that ABI contract.
fn is_container_ctor(name: &str) -> bool {
    matches!(
        name,
        "Vec::new"
            | "Vec::with_capacity"
            | "gos_rt_vec_new"
            | "gos_rt_vec_new_typed"
            | "gos_rt_vec_with_capacity"
            | "gos_rt_vec_with_capacity_typed"
            | "gos_rt_vec_from_arr"
            | "gos_rt_nested_arr_to_vec"
            | "gos_rt_vec_clone"
            | "gos_rt_str_chars"
            | "gos_rt_i64_chars"
            | "gos_rt_arr_iter"
            | "gos_rt_lazy_iter_collect_i64"
            | "gos_rt_lazy_iter_collect_pair_i64"
            | "Map::new"
            | "Map::with_capacity"
            | "HashMap::new"
            | "HashMap::with_capacity"
            | "collections::Map::new"
            | "collections::Map::with_capacity"
            | "collections::HashMap::new"
            | "Set::new"
            | "collections::Set::new"
            | "HashSet::new"
            | "collections::HashSet::new"
            | "BTreeSet::new"
            | "collections::BTreeSet::new"
            | "BTreeMap::new"
            | "collections::BTreeMap::new"
            | "gos_rt_map_new"
            | "gos_rt_map_new_with_capacity"
            | "gos_rt_set_new"
    )
}
