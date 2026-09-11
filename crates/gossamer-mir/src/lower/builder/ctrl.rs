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

/// Panic message for a `match` whose arms did not cover the scrutinee
/// at runtime. The exhaustiveness checker rejects non-exhaustive matches
/// at `check` time; this backstop only fires on a value the checker
/// believed impossible (an exhaustiveness blind spot). Emitting a clean
/// panic instead of `Terminator::Unreachable` keeps the compiled tiers
/// memory-safe - `Unreachable` lowers to undefined behaviour (a segfault
/// on LLVM, a trap on Cranelift) - so "if it builds it runs" holds.
const NON_EXHAUSTIVE_MATCH_MESSAGE: &str = "non-exhaustive match: no pattern matched the value";

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

impl<'a> Builder<'a> {
    pub(crate) fn lower_if(
        &mut self,
        condition: &HirExpr,
        then_branch: &HirExpr,
        else_branch: Option<&HirExpr>,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let cond_local = self.lower_expr(condition)?;
        // 0.7.0 flag::Cell auto-deref for `if flags.verbose { … }`.
        let cond_local = self.auto_deref_cell(cond_local, span);
        let then_block = self.new_block(span);
        let else_block = self.new_block(span);
        let join_block = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cond_local)),
            arms: vec![(0, else_block)],
            default: then_block,
        });

        let result_local = self.fresh(ty);

        self.set_current(then_block);
        if let Some(then_value) = self.lower_expr(then_branch) {
            self.emit_assign(
                Place::local(result_local),
                Rvalue::Use(Operand::Copy(Place::local(then_value))),
                span,
            );
            self.terminate(Terminator::Goto { target: join_block });
        }

        self.set_current(else_block);
        if let Some(else_branch) = else_branch {
            if let Some(else_value) = self.lower_expr(else_branch) {
                self.emit_assign(
                    Place::local(result_local),
                    Rvalue::Use(Operand::Copy(Place::local(else_value))),
                    span,
                );
                self.terminate(Terminator::Goto { target: join_block });
            }
        } else {
            let unit_local = self.lower_unit(span);
            self.emit_assign(
                Place::local(result_local),
                Rvalue::Use(Operand::Copy(Place::local(unit_local))),
                span,
            );
            self.terminate(Terminator::Goto { target: join_block });
        }

        self.set_current(join_block);
        Some(result_local)
    }

    pub(crate) fn lower_match(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[HirMatchArm],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        // A match over one user enum's own variants, binding payloads with
        // plain names and no guards, is what a single `SwitchInt` over the
        // discriminant dispatches: one read, one branch, one arm per
        // variant. The if-chain below would instead re-read the
        // discriminant and re-compare it once per arm.
        if let Some(enum_name) = self.flat_variant_match_enum(scrutinee.ty, arms) {
            return self.lower_flat_variant_match(scrutinee, arms, ty, span, &enum_name);
        }
        // Route guarded arms and any non-flat pattern shape
        // (tuple / or-pattern / nested variant binding) through
        // the if-chain lowering. The original SwitchInt path
        // below stays the fast path for flat int / bool /
        // single-variant matches whose discriminant fits one
        // word.
        // The SwitchInt path below decodes exactly the shapes listed here.
        // Anything else is the if-chain's, which either lowers the pattern
        // or refuses the body: a shape this path does not decode would
        // otherwise be classified as the default arm and swallow every
        // arm written after it.
        let needs_chain = !arms.iter().all(|arm| {
            arm.guard.is_none()
                && match &arm.pattern.kind {
                    HirPatKind::Wildcard | HirPatKind::Binding { .. } => true,
                    HirPatKind::Literal(
                        HirLiteral::Int(_) | HirLiteral::Bool(_) | HirLiteral::Byte(_),
                    ) => true,
                    HirPatKind::Variant { name, .. } => {
                        !matches!(name.name.as_str(), "Ok" | "Err" | "Some" | "None")
                            && self.enums.lookup(std::slice::from_ref(name)).is_none()
                    }
                    _ => false,
                }
        });
        if needs_chain {
            return self.lower_match_with_guards(scrutinee, arms, ty, span);
        }
        let mut switch_arms: Vec<(i128, BlockId)> = Vec::new();
        let mut default_block: Option<BlockId> = None;
        // Per-arm binding: the variant pattern's inner Binding (e.g.
        // `Ok(v)` → `v`) is registered against the scrutinee local
        // when we enter the arm block, so the body can reference it.
        // The scrutinee's static type carries through (e.g. for
        // `Ok(v)` on a `Result<json::Value, _>` the binding gets
        // typed as `json::Value`).
        // Per-arm captured payload binding: (binding name, mutable
        // flag, variant constructor name). The variant name lets
        // the arm-body fixup routine re-pin the scrutinee local's
        // type to the right `substs` slot - `substs[0]` for
        // `Ok`/`Some`, `substs[1]` for `Err` - so subsequent reads
        // of the bound name see the payload type, not the wrapper.
        let mut arm_bindings: Vec<Option<(Ident, bool, Option<String>)>> =
            Vec::with_capacity(arms.len());
        let mut arm_bodies: Vec<(BlockId, &HirExpr)> = Vec::with_capacity(arms.len());
        for arm in arms {
            let arm_block = self.new_block(span);
            arm_bodies.push((arm_block, &arm.body));
            arm_bindings.push(None);
            match &arm.pattern.kind {
                HirPatKind::Literal(HirLiteral::Int(text)) => {
                    let v = parse_int(text).unwrap_or_else(|| {
                        unreachable!("match arm: lexer-validated int literal `{text}` failed parse")
                    });
                    switch_arms.push((v, arm_block));
                }
                HirPatKind::Literal(HirLiteral::Bool(b)) => {
                    switch_arms.push((i128::from(*b), arm_block));
                }
                HirPatKind::Literal(HirLiteral::Byte(b)) => {
                    switch_arms.push((i128::from(*b), arm_block));
                }
                HirPatKind::Wildcard => {
                    // Multiple wildcard arms are accepted; only the
                    // first is reachable. Subsequent wildcard bodies
                    // are emitted into dead blocks the SwitchInt
                    // never targets.
                    if default_block.is_none() {
                        default_block = Some(arm_block);
                    }
                }
                HirPatKind::Binding { name, mutable } => {
                    // A binding arm is the default arm plus a local alias
                    // for the matched scrutinee value.
                    if default_block.is_none() {
                        default_block = Some(arm_block);
                        *arm_bindings.last_mut().expect("arm tracked") =
                            Some((name.clone(), *mutable, None));
                    }
                }
                // Variant patterns (`Ok(x)`, `Err(e)`, `Some(v)`, …)
                // don't yet have runtime discriminants, but we can
                // still produce a well-formed CFG by always taking
                // the first variant arm as a "happy path" default.
                // Bind any inner pattern to the scrutinee local so
                // `let x = foo()?` compiles. Wrong for genuine error
                // cases, but enough for programs whose control flow
                // stays on the Ok/Some path.
                HirPatKind::Variant { name, fields } => {
                    // User-defined enum: dispatch by the variant's
                    // declaration index recorded in `EnumIndex`.
                    // Fall back to `Result`/`Option`'s historical
                    // happy-path encoding (`Ok` / `Some` = 0,
                    // `Err` / `None` = 1) for the stdlib variants
                    // that don't have a Gossamer enum behind them.
                    // Resolve through the scrutinee's own enum first: a
                    // variant name two enums both declare would otherwise
                    // take the other enum's index.
                    let resolved = self
                        .enum_index_name_of(scrutinee.ty)
                        .and_then(|en| self.enums.variant_of_enum(&en, &name.name))
                        .map(|idx| (String::new(), idx))
                        .or_else(|| self.enums.lookup(std::slice::from_ref(name)));
                    let pos: i128 = if let Some((_, idx)) = resolved {
                        idx as i128
                    } else if matches!(name.name.as_str(), "Err" | "None" | "Some" | "Ok") {
                        match name.name.as_str() {
                            "Some" | "Ok" => 0,
                            _ => 1,
                        }
                    } else {
                        switch_arms.len() as i128
                    };
                    switch_arms.push((pos, arm_block));
                    // For `Ok(v)` / `Some(v)` patterns the
                    // payload is structurally identical to the
                    // scrutinee in compiled mode (Result/Option
                    // are flat single-slot values today), so
                    // bind the inner name to the scrutinee local
                    // when entering the arm. Captures only the
                    // first single-Binding inner field - wider
                    // patterns continue through the placeholder.
                    if let Some(first) = fields.first() {
                        if let HirPatKind::Binding {
                            name: bname,
                            mutable,
                        } = &first.kind
                        {
                            *arm_bindings.last_mut().expect("arm tracked") =
                                Some((bname.clone(), *mutable, Some(name.name.clone())));
                        }
                    }
                }
                // Tuple / struct / range / or-pattern shapes that
                // the no-guards SwitchInt path doesn't decode are
                // treated as wildcard arms here: the first one
                // becomes the default, later ones are dead.
                _ => {
                    if default_block.is_none() {
                        default_block = Some(arm_block);
                    }
                }
            }
        }
        let scrutinee_local = self.lower_expr(scrutinee)?;
        let join_block = self.new_block(span);
        let result_local = self.fresh(ty);
        // Save the post-scrutinee block before allocating the
        // default arm; the unreachable_block creation below sets
        // current to that block and then terminates it (leaving
        // current = None), which would otherwise swallow our
        // SwitchInt / Goto terminator below.
        let dispatch_block = self.current;
        let default = default_block.unwrap_or_else(|| {
            let panic_block = self.new_block(span);
            self.set_current(panic_block);
            self.terminate(Terminator::Panic {
                message: NON_EXHAUSTIVE_MATCH_MESSAGE.to_string(),
            });
            panic_block
        });
        if let Some(block) = dispatch_block {
            self.set_current(block);
        }
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(scrutinee_local)),
            arms: switch_arms,
            default,
        });
        for ((arm_block, body), binding) in arm_bodies.into_iter().zip(arm_bindings) {
            self.set_current(arm_block);
            // When the arm pattern was `Ok(v)` / `Some(v)` /
            // `Variant(v)`, register `v` against the scrutinee
            // local so the arm body's references resolve. If the
            // scrutinee is a flat `*mut GosJson` (i.e. its static
            // type is `Result<json::Value, _>` / `Option<json::Value>`
            // / `json::Value`), promote the scrutinee local to
            // `json::Value` so chained `j.field` accesses route
            // through the json runtime helpers.
            if let Some((bname, _mutable, variant_name)) = binding {
                let scrut_ty = self.locals[scrutinee_local.0 as usize].ty;
                if let Some(name) = variant_name.as_deref() {
                    // Generalised happy-path payload pin: for
                    // `Ok(x)` / `Some(x)` the payload is the
                    // wrapper's first generic arg; for `Err(e)`
                    // the second. Without this, downstream code
                    // sees `x` typed as the whole `Result<T, E>`
                    // and routes value-formatting / coercion
                    // through the wrong path.
                    let slot = match name {
                        "Ok" | "Some" => Some(0),
                        "Err" => Some(1),
                        _ => None,
                    };
                    if let Some(idx) = slot {
                        if let Some(payload_ty) = self.adt_generic_at(scrut_ty, idx) {
                            self.locals[scrutinee_local.0 as usize].ty = payload_ty;
                        }
                    }
                }
                self.bind_local(&bname.name, scrutinee_local);
            }
            if let Some(value_local) = self.lower_expr(body) {
                // Pin the match-result local's type to the arm's
                // value type when the HIR type is opaque (Var /
                // Error). Lets chained patterns like `let v =
                // match r { Ok(j) => j, .. }; v.field` flow the
                // concrete `json::Value` (or struct) shape into
                // the surrounding `let`'s field-access lowering.
                use gossamer_types::TyKind;
                let arm_value_ty = self.locals[value_local.0 as usize].ty;
                let result_kind = self.tcx.kind_of(self.locals[result_local.0 as usize].ty);
                let arm_kind = self.tcx.kind_of(arm_value_ty);
                let result_is_loose =
                    matches!(result_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never);
                let arm_is_concrete =
                    !matches!(arm_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never);
                if result_is_loose && arm_is_concrete {
                    self.locals[result_local.0 as usize].ty = arm_value_ty;
                }
                self.emit_assign(
                    Place::local(result_local),
                    Rvalue::Use(Operand::Copy(Place::local(value_local))),
                    span,
                );
                self.terminate(Terminator::Goto { target: join_block });
            }
        }
        self.set_current(join_block);
        Some(result_local)
    }

    /// Names the enum when every arm of `arms` selects on one user enum's
    /// own variants: no guards, each pattern either a variant of that enum
    /// whose payload sub-patterns are plain binders or `_`, or a trailing
    /// wildcard / binding arm. That is the shape one `SwitchInt` over the
    /// discriminant dispatches.
    ///
    /// `Ok` / `Err` / `Some` / `None` are excluded even when a user enum
    /// declares those names: the carrier arms carry payload handling of
    /// their own that the chain lowering owns.
    fn flat_variant_match_enum(&self, scrutinee_ty: Ty, arms: &[HirMatchArm]) -> Option<String> {
        let enum_name = self.enum_index_name_of(scrutinee_ty)?;
        let mut saw_variant = false;
        for arm in arms {
            if arm.guard.is_some() {
                return None;
            }
            match &arm.pattern.kind {
                HirPatKind::Variant { name, fields } => {
                    if matches!(name.name.as_str(), "Ok" | "Err" | "Some" | "None") {
                        return None;
                    }
                    self.enums.variant_of_enum(&enum_name, &name.name)?;
                    // A nested or rest sub-pattern needs the chain's
                    // recursive decode; a `..` in particular would shift
                    // every later field's payload offset.
                    if !fields.iter().all(|f| {
                        matches!(f.kind, HirPatKind::Binding { .. } | HirPatKind::Wildcard)
                    }) {
                        return None;
                    }
                    saw_variant = true;
                }
                HirPatKind::Wildcard | HirPatKind::Binding { .. } => {}
                _ => return None,
            }
        }
        saw_variant.then_some(enum_name)
    }

    /// Lowers a [`Self::flat_variant_match_enum`] match: one discriminant
    /// read, one `SwitchInt` with an arm per matched variant, and each
    /// arm's payload bound inside its own block.
    fn lower_flat_variant_match(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[HirMatchArm],
        ty: Ty,
        span: Span,
        enum_name: &str,
    ) -> Option<Local> {
        let scrutinee_local = self.lower_expr(scrutinee)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);

        // A payload-bearing enum's value is a pointer to `[disc, p0, ...]`,
        // so the discriminant is read out of it; a unit-only enum's value is
        // the discriminant already, and only needs a local the switch can
        // name, since the MIR contract for a `SwitchInt` discriminant is an
        // integer one.
        let disc = self.fresh(i64_ty);
        if self.enums.enum_has_any_payload(enum_name) {
            let intrinsic = if self.enum_repr_tagged(enum_name) {
                "gos_enum_disc_tag"
            } else {
                "gos_enum_disc"
            };
            self.emit_assign(
                Place::local(disc),
                Rvalue::CallIntrinsic {
                    name: intrinsic,
                    args: vec![Operand::Copy(Place::local(scrutinee_local))],
                },
                span,
            );
        } else {
            self.emit_assign(
                Place::local(disc),
                Rvalue::Use(Operand::Copy(Place::local(scrutinee_local))),
                span,
            );
        }

        let join_block = self.new_block(span);
        let result_local = self.fresh(ty);
        let mut switch_arms: Vec<(i128, BlockId)> = Vec::new();
        let mut default_block: Option<BlockId> = None;
        let mut bodies: Vec<(BlockId, &HirMatchArm)> = Vec::with_capacity(arms.len());

        for arm in arms {
            let arm_block = self.new_block(span);
            match &arm.pattern.kind {
                HirPatKind::Variant { name, .. } => {
                    let idx = self.enums.variant_of_enum(enum_name, &name.name)? as i128;
                    // A variant named twice keeps its first arm, the way the
                    // chain lowering's first matching predicate does; a second
                    // key for it would leave the switch with two answers.
                    if switch_arms.iter().all(|&(key, _)| key != idx) {
                        switch_arms.push((idx, arm_block));
                        bodies.push((arm_block, arm));
                    }
                }
                // A catch-all answers every value the arms above it did not
                // name, so it is the default and nothing below it can be
                // reached - including a variant arm, which must not take a
                // switch key away from the catch-all that precedes it.
                _ => {
                    default_block = Some(arm_block);
                    bodies.push((arm_block, arm));
                    break;
                }
            }
        }

        // The dispatch block is the one the discriminant read landed in;
        // creating the panic block below moves `current` and terminates it.
        let dispatch_block = self.current;
        let default = default_block.unwrap_or_else(|| {
            let panic_block = self.new_block(span);
            self.set_current(panic_block);
            self.terminate(Terminator::Panic {
                message: NON_EXHAUSTIVE_MATCH_MESSAGE.to_string(),
            });
            panic_block
        });
        if let Some(block) = dispatch_block {
            self.set_current(block);
        }
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: switch_arms,
            default,
        });

        for (arm_block, arm) in bodies {
            self.set_current(arm_block);
            match &arm.pattern.kind {
                HirPatKind::Variant { name, fields } if !fields.is_empty() => {
                    self.bind_variant_let_payload(scrutinee_local, name, fields, span);
                }
                HirPatKind::Binding { name, .. } => {
                    self.bind_local(&name.name, scrutinee_local);
                }
                _ => {}
            }
            if let Some(value_local) = self.lower_expr(&arm.body) {
                // The arm's own value carries the shape the match answers
                // with when typeck left the match's type open, which is what
                // a following field access or method call reads.
                use gossamer_types::TyKind;
                let arm_value_ty = self.locals[value_local.0 as usize].ty;
                let result_kind = self.tcx.kind_of(self.locals[result_local.0 as usize].ty);
                let arm_kind = self.tcx.kind_of(arm_value_ty);
                let result_is_loose =
                    matches!(result_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never);
                let arm_is_concrete =
                    !matches!(arm_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never);
                if result_is_loose && arm_is_concrete {
                    self.locals[result_local.0 as usize].ty = arm_value_ty;
                }
                if let Some(struct_name) = self.local_struct.get(&value_local).cloned() {
                    self.local_struct.insert(result_local, struct_name);
                }
                if let Some(rk) = self.local_runtime_kind.get(&value_local).copied() {
                    self.local_runtime_kind.insert(result_local, rk);
                }
                if let Some(en) = self.local_elem_struct.get(&value_local).cloned() {
                    self.local_elem_struct.insert(result_local, en);
                }
                self.emit_assign(
                    Place::local(result_local),
                    Rvalue::Use(Operand::Copy(Place::local(value_local))),
                    span,
                );
                self.terminate(Terminator::Goto { target: join_block });
            }
        }
        self.set_current(join_block);
        Some(result_local)
    }

    pub(crate) fn lower_match_with_guards(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[HirMatchArm],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let scrutinee_local = self.lower_expr(scrutinee)?;
        let result_local = self.fresh(ty);
        let join = self.new_block(span);

        for arm in arms {
            let setup_block = self.new_block(span);
            let arm_block = self.new_block(span);
            let next_block = self.new_block(span);

            // Push a binding scope so guard + body see the
            // pattern-bound names. Bindings introduced by the
            // pattern (single-name, tuple-element, variant-payload)
            // are recorded against MIR locals here too.
            self.push_scope();
            // Defer payload extraction for Result/Option arms into
            // setup_block so the payload pointer is only dereferenced
            // on the matching branch. Without this, `gos_rt_result_payload`
            // runs unconditionally in the header block and crashes
            // when the scrutinee is None/Err on the next iteration.
            //
            // Keep setup separate from the user arm body. `while let`
            // lowers through this path, and merging payload setup with
            // the body block made later body emission depend on the exact
            // block state left behind by predicate lowering.
            self.payload_defer_block = Some(setup_block);
            // 0.8.0: no always-matches fallback. When
            // `lower_pattern_predicate` doesn't decode the
            // pattern shape (tuple/struct/range/or-patterns that
            // need richer destructuring than the SwitchInt path
            // covers), MIR refuses to compile rather than silently
            // miscompiling subsequent arms as unreachable. The
            // legacy `GOSSAMER_STRICT_LOWER=1` opt-in is now the
            // only behaviour.
            let pat_match_local =
                match self.lower_pattern_predicate(scrutinee_local, &arm.pattern, span) {
                    Some(local) => local,
                    None => {
                        let kind = pattern_kind_label(&arm.pattern);
                        panic!(
                            "MIR lower: match-guard arm has unsupported pattern shape \
                         ({kind}); add explicit destructuring for this pattern shape \
                         before relying on it in compiled code"
                        );
                    }
                };
            // Clear the defer hint in case the pattern didn't consume it
            // (e.g. wildcard arms have no payload to extract).
            self.payload_defer_block = None;

            self.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::local(pat_match_local)),
                arms: vec![(0, next_block)],
                default: setup_block,
            });

            self.set_current(setup_block);
            // A guard reads the names the pattern bound, and those are
            // extracted in this block rather than in the header, so the guard
            // belongs where they are live. That is also where the language
            // puts it: a guard decides between this arm and the next one only
            // once the pattern has matched, so a guard over a payload names
            // this variant's payload.
            if let Some(guard_expr) = &arm.guard {
                let guard_local = self.lower_expr(guard_expr)?;
                self.terminate(Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(guard_local)),
                    arms: vec![(0, next_block)],
                    default: arm_block,
                });
            } else {
                self.terminate(Terminator::Goto { target: arm_block });
            }

            self.set_current(arm_block);
            if let Some(value_local) = self.lower_expr(&arm.body) {
                use gossamer_types::TyKind;
                let arm_value_ty = self.locals[value_local.0 as usize].ty;
                let result_kind = self.tcx.kind_of(self.locals[result_local.0 as usize].ty);
                let arm_kind = self.tcx.kind_of(arm_value_ty);
                let result_is_loose =
                    matches!(result_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never);
                let arm_is_concrete =
                    !matches!(arm_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never);
                if result_is_loose && arm_is_concrete {
                    self.locals[result_local.0 as usize].ty = arm_value_ty;
                }
                if let Some(struct_name) = self.local_struct.get(&value_local).cloned() {
                    self.local_struct.insert(result_local, struct_name);
                }
                if let Some(rk) = self.local_runtime_kind.get(&value_local).copied() {
                    self.local_runtime_kind.insert(result_local, rk);
                }
                if let Some(en) = self.local_elem_struct.get(&value_local).cloned() {
                    self.local_elem_struct.insert(result_local, en);
                }
                self.emit_assign(
                    Place::local(result_local),
                    Rvalue::Use(Operand::Copy(Place::local(value_local))),
                    span,
                );
                self.terminate(Terminator::Goto { target: join });
            } else if self.current.is_some() {
                // Arm body ended normally (no value to bind, but
                // control still falls through - typical of a loop
                // tail). Connect to join so the codegen doesn't
                // leave a dangling block.
                self.terminate(Terminator::Goto { target: join });
            }
            self.pop_scope();

            self.set_current(next_block);
        }
        // Ran past every guarded arm without matching. The match was
        // assumed exhaustive; a value reaching here is a guard gap the
        // checker could not see. Panic cleanly instead of falling through
        // to `join` with an uninitialised result local, which the
        // compiled tiers would read as garbage (a pointer-typed result
        // becomes a wild pointer).
        self.terminate(Terminator::Panic {
            message: NON_EXHAUSTIVE_MATCH_MESSAGE.to_string(),
        });
        self.set_current(join);
        Some(result_local)
    }

    pub(crate) fn lower_pattern_predicate(
        &mut self,
        scrutinee: Local,
        pattern: &HirPat,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let bool_ty = self.tcx.bool_ty();
        match &pattern.kind {
            HirPatKind::Wildcard => {
                let l = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                    span,
                );
                Some(l)
            }
            HirPatKind::Binding { name, .. } => {
                self.bind_local(&name.name, scrutinee);
                let l = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                    span,
                );
                Some(l)
            }
            HirPatKind::Literal(HirLiteral::Int(text)) => {
                let v = parse_int(text)?;
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                let lit_local = self.fresh(scrut_ty);
                self.emit_assign(
                    Place::local(lit_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(v))),
                    span,
                );
                let cmp = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(cmp),
                    Rvalue::BinaryOp {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(lit_local)),
                    },
                    span,
                );
                Some(cmp)
            }
            HirPatKind::Literal(HirLiteral::Byte(b)) => {
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                let lit_local = self.fresh(scrut_ty);
                self.emit_assign(
                    Place::local(lit_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(*b)))),
                    span,
                );
                let cmp = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(cmp),
                    Rvalue::BinaryOp {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(lit_local)),
                    },
                    span,
                );
                Some(cmp)
            }
            HirPatKind::Literal(HirLiteral::Bool(b)) => {
                let lit_local = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(lit_local),
                    Rvalue::Use(Operand::Const(ConstValue::Bool(*b))),
                    span,
                );
                let cmp = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(cmp),
                    Rvalue::BinaryOp {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(lit_local)),
                    },
                    span,
                );
                Some(cmp)
            }
            HirPatKind::Literal(HirLiteral::String(text)) => {
                // String-literal match arm. Compare via
                // `gos_rt_str_eq` which the runtime exposes; emit
                // a fresh string operand for the literal.
                let str_ty = self.tcx.string_ty();
                let lit_local = self.fresh(str_ty);
                self.emit_assign(
                    Place::local(lit_local),
                    Rvalue::Use(Operand::Const(ConstValue::Str(text.clone()))),
                    span,
                );
                let cmp = self.fresh(bool_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_eq".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(scrutinee)),
                        Operand::Copy(Place::local(lit_local)),
                    ],
                    destination: Place::local(cmp),
                    target: Some(next),
                });
                self.set_current(next);
                Some(cmp)
            }
            HirPatKind::Literal(HirLiteral::Char(c)) => {
                let char_ty = self.tcx.char_ty();
                let lit_local = self.fresh(char_ty);
                self.emit_assign(
                    Place::local(lit_local),
                    Rvalue::Use(Operand::Const(ConstValue::Char(*c))),
                    span,
                );
                let cmp = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(cmp),
                    Rvalue::BinaryOp {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(lit_local)),
                    },
                    span,
                );
                Some(cmp)
            }
            HirPatKind::Literal(HirLiteral::Float(text)) => {
                let value = crate::lower::helpers::parse_float(text.trim());
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                let lit_local = self.fresh(scrut_ty);
                self.emit_assign(
                    Place::local(lit_local),
                    Rvalue::Use(Operand::Const(ConstValue::Float(value.to_bits()))),
                    span,
                );
                let cmp = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(cmp),
                    Rvalue::BinaryOp {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(lit_local)),
                    },
                    span,
                );
                Some(cmp)
            }
            HirPatKind::Literal(HirLiteral::Unit) => {
                // `()` has one inhabitant, so naming it always matches.
                let l = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                    span,
                );
                Some(l)
            }
            HirPatKind::Ref { inner, .. } => {
                // `&pat` patterns: peel through the reference
                // and match the inner pattern. The compiled tier
                // doesn't materialise references separately; the
                // scrutinee local already holds the pointer.
                self.lower_pattern_predicate(scrutinee, inner, span)
            }
            HirPatKind::Rest => {
                // Rest pattern in a non-tuple context - match
                // anything; binds nothing.
                let l = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                    span,
                );
                Some(l)
            }
            HirPatKind::Tuple(sub_pats) => {
                // Conjunction across tuple-element predicates. Each
                // sub-pattern is matched against the corresponding
                // tuple field via a fresh local that holds the
                // projected element value.
                use gossamer_types::TyKind;
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                let elem_tys: Vec<Ty> = match self.tcx.kind_of(scrut_ty) {
                    TyKind::Tuple(elems) => elems.clone(),
                    _ => return None,
                };
                let mut acc = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(acc),
                    Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                    span,
                );
                for (idx, sub_pat) in sub_pats.iter().enumerate() {
                    // Prefer the tuple's recorded element type, but when
                    // inference left it unresolved (`let pair = (10,
                    // "hi")` keeps the binding's tuple type loose) fall
                    // back to the sub-pattern's own type. Without this
                    // the element local defaults to a pointer shape and
                    // the `println!` arg dispatcher routes an `i64`
                    // through `gos_rt_concat_str`, strlen'ing the
                    // integer value and segfaulting.
                    let from_tuple = elem_tys.get(idx).copied();
                    let unresolved = |t: Ty| {
                        matches!(
                            self.tcx.kind_of(t),
                            TyKind::Var(_) | TyKind::Error | TyKind::Never
                        )
                    };
                    let elem_ty = match from_tuple {
                        Some(t) if !unresolved(t) => t,
                        _ if !unresolved(sub_pat.ty) => sub_pat.ty,
                        Some(t) => t,
                        None => return None,
                    };
                    let elem_local = self.fresh(elem_ty);
                    let elem_place = Place {
                        local: scrutinee,
                        projection: vec![crate::ir::Projection::Field(idx as u32)],
                    };
                    self.emit_assign(
                        Place::local(elem_local),
                        Rvalue::Use(Operand::Copy(elem_place)),
                        span,
                    );
                    let sub_pred = self.lower_pattern_predicate(elem_local, sub_pat, span)?;
                    let combined = self.fresh(bool_ty);
                    self.emit_assign(
                        Place::local(combined),
                        Rvalue::BinaryOp {
                            op: BinOp::BitAnd,
                            lhs: Operand::Copy(Place::local(acc)),
                            rhs: Operand::Copy(Place::local(sub_pred)),
                        },
                        span,
                    );
                    acc = combined;
                }
                Some(acc)
            }
            HirPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => self.lower_slice_pattern(scrutinee, prefix, rest.as_deref(), suffix, span),
            HirPatKind::Or(branches) => self.lower_or_pattern_predicate(scrutinee, branches, span),
            HirPatKind::Range { lo, hi, inclusive } => {
                // `lo..hi` and `lo..=hi` arms reduce to
                // `(scrut >= lo) && (scrut <op> hi)` where the
                // upper comparison is `<` for exclusive and `<=`
                // for inclusive. A byte and a character compare as
                // the scalar each one is, which is the same
                // comparison their equality arms already lower to;
                // a float bound has no ordering the discriminant
                // path reads and falls through.
                let lo_v = pattern_range_bound(lo)?;
                let hi_v = pattern_range_bound(hi)?;
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                let lo_local = self.fresh(scrut_ty);
                self.emit_assign(
                    Place::local(lo_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(lo_v))),
                    span,
                );
                let hi_local = self.fresh(scrut_ty);
                self.emit_assign(
                    Place::local(hi_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(hi_v))),
                    span,
                );
                let ge = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(ge),
                    Rvalue::BinaryOp {
                        op: BinOp::Ge,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(lo_local)),
                    },
                    span,
                );
                let upper_op = if *inclusive { BinOp::Le } else { BinOp::Lt };
                let upper = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(upper),
                    Rvalue::BinaryOp {
                        op: upper_op,
                        lhs: Operand::Copy(Place::local(scrutinee)),
                        rhs: Operand::Copy(Place::local(hi_local)),
                    },
                    span,
                );
                let combined = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(combined),
                    Rvalue::BinaryOp {
                        op: BinOp::BitAnd,
                        lhs: Operand::Copy(Place::local(ge)),
                        rhs: Operand::Copy(Place::local(upper)),
                    },
                    span,
                );
                Some(combined)
            }
            HirPatKind::Struct { name, fields, .. } => {
                // Struct pattern matching a value of a known
                // struct type - OR a struct-payload enum variant
                // (`Shape::Rect { w, h }` lowers as
                // `HirPatKind::Struct { name: "Rect", ... }`).
                // For an enum-variant struct, the predicate
                // routes to the variant's discriminant index;
                // for a real struct it's always true (shape
                // verified by the type-checker). Each named-field
                // sub-pattern reads through a
                // `Projection::Field(idx)` of the scrutinee.
                let order = self
                    .structs
                    .get(&name.name)
                    .cloned()
                    .or_else(|| self.enums.variant_fields.get(&name.name).cloned());
                // The scrutinee's own type names the enum, so a variant
                // name two enums both declare resolves to this value's own
                // variant rather than to whichever enum the program-wide
                // map holds.
                let matched = self
                    .enum_index_name_of(self.locals[scrutinee.0 as usize].ty)
                    .and_then(|en| {
                        self.enums
                            .variant_of_enum(&en, &name.name)
                            .map(|idx| (en, idx))
                    })
                    .or_else(|| self.enums.lookup(std::slice::from_ref(name)));
                let matched_enum = matched.clone().map(|(en, _)| en).unwrap_or_default();
                let variant_idx = matched.map(|(_, i)| i);
                // Whether ANY sibling variant of this enum carries
                // a payload - controls whether the runtime layout
                // is `[disc, p0, ...]` (heap aggregate) or just an
                // i64 disc value.
                let any_variant_has_payload = if matched_enum.is_empty() {
                    self.enums.has_any_payload(std::slice::from_ref(name))
                } else {
                    self.enums.enum_has_any_payload(&matched_enum)
                };
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let scrut_is_real_struct = self
                    .struct_name_of(self.locals[scrutinee.0 as usize].ty)
                    .is_some_and(|sn| self.structs.contains_key(&sn));
                let scrut_is_payload_enum =
                    variant_idx.is_some() && (any_variant_has_payload || !fields.is_empty());
                // Predicate seed: for an enum variant, compare the
                // scrutinee's discriminant to the variant index;
                // for a free struct, every value of the scrutinee
                // type matches. For payload-bearing enums the
                // scrutinee is a *ptr* to `[disc, p0, p1, ...]`,
                // so we must load disc from offset 0 first -
                // comparing the bare scrutinee (a heap address) to
                // a small variant index always returns false and
                // the arm body never runs (the cli_args /
                // control_flow / data_structures bug).
                let acc = self.fresh(bool_ty);
                if let Some(idx) = variant_idx {
                    let scrut_for_cmp = if scrut_is_payload_enum {
                        let disc_load = self.fresh(i64_ty);
                        // Tagged repr (<= 4 variants): disc in pointer
                        // bits 1-2; header repr: disc byte at payload-3.
                        let disc_intrinsic = if self.enum_repr_tagged(&matched_enum) {
                            "gos_enum_disc_tag"
                        } else {
                            "gos_enum_disc"
                        };
                        self.emit_assign(
                            Place::local(disc_load),
                            Rvalue::CallIntrinsic {
                                name: disc_intrinsic,
                                args: vec![Operand::Copy(Place::local(scrutinee))],
                            },
                            span,
                        );
                        disc_load
                    } else {
                        scrutinee
                    };
                    let cmp_ty = if scrut_is_payload_enum {
                        i64_ty
                    } else {
                        self.locals[scrutinee.0 as usize].ty
                    };
                    let lit_local = self.fresh(cmp_ty);
                    self.emit_assign(
                        Place::local(lit_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(idx as i128))),
                        span,
                    );
                    self.emit_assign(
                        Place::local(acc),
                        Rvalue::BinaryOp {
                            op: BinOp::Eq,
                            lhs: Operand::Copy(Place::local(scrut_for_cmp)),
                            rhs: Operand::Copy(Place::local(lit_local)),
                        },
                        span,
                    );
                } else {
                    self.emit_assign(
                        Place::local(acc),
                        Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                        span,
                    );
                }
                if let Some(order) = order {
                    let declared_tys = self.enums.field_tys_of(&matched_enum, &name.name);
                    let payload_offsets = self.variant_payload_offsets(
                        declared_tys.as_deref().unwrap_or(&[]),
                        order.len(),
                    );
                    for f in fields {
                        let pos = order.iter().position(|n| n == &f.name.name);
                        let Some(pos) = pos else { continue };
                        let field_idx = u32::try_from(pos).ok()?;
                        // The variant declaration's recorded field type is
                        // authoritative: a named sub-pattern binding
                        // (`Rect { w: __d0 }`) carries a fresh inference
                        // variable that can resolve to the wrong type (e.g.
                        // `String` from a `format!` use site) instead of the
                        // declared field type (e.g. `f64`), which would decode
                        // the I64 load as the wrong shape. Prefer the declared
                        // type when it is resolved, falling back to the HIR
                        // sub-pattern type (free structs have no variant entry)
                        // and finally I64.
                        // A free struct has no enum-variant field-type entry,
                        // so its field type comes from the scrutinee's struct
                        // definition. This is authoritative: a binding
                        // sub-pattern's own inferred type can be left `Var` or
                        // mis-resolved (a `String` from a `format!` use site),
                        // which would decode the I64 slot as the wrong shape.
                        let scrut_field_ty = {
                            let mut t = self.locals[scrutinee.0 as usize].ty;
                            while let gossamer_types::TyKind::Ref { inner, .. } =
                                self.tcx.kind_of(t)
                            {
                                t = *inner;
                            }
                            match self.tcx.kind_of(t) {
                                gossamer_types::TyKind::Adt { def, .. } => self
                                    .tcx
                                    .struct_field_tys(*def)
                                    .and_then(|tys| tys.get(pos).copied()),
                                _ => None,
                            }
                        };
                        let declared_field_ty = declared_tys
                            .as_ref()
                            .and_then(|tys| tys.get(pos).copied())
                            .or(scrut_field_ty)
                            .filter(|&ty| {
                                !matches!(
                                    self.tcx.kind_of(ty),
                                    gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
                                )
                            });
                        let field_ty = declared_field_ty
                            .or_else(|| f.pattern.as_ref().map(|sub| sub.ty))
                            .unwrap_or(i64_ty);
                        let elem = self.fresh(field_ty);
                        if scrut_is_real_struct {
                            // Free struct: real field projection.
                            let field_place = Place {
                                local: scrutinee,
                                projection: vec![crate::ir::Projection::Field(field_idx)],
                            };
                            self.emit_assign(
                                Place::local(elem),
                                Rvalue::Use(Operand::Copy(field_place)),
                                span,
                            );
                        } else if scrut_is_payload_enum {
                            // Enum-variant struct payload: the
                            // scrutinee is a pointer to a heap
                            // aggregate `[disc, p0, p1, …]`.
                            // Load this field from offset
                            // field_idx * 8 (the discriminant lives in
                            // the RC header byte, not the payload).
                            let off_local = self.fresh(i64_ty);
                            self.emit_assign(
                                Place::local(off_local),
                                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                                    payload_offsets
                                        .get(pos)
                                        .copied()
                                        .unwrap_or(i64::from(field_idx) * 8),
                                )))),
                                span,
                            );
                            self.emit_assign(
                                Place::local(elem),
                                Rvalue::CallIntrinsic {
                                    name: "gos_enum_load",
                                    args: vec![
                                        Operand::Copy(Place::local(scrutinee)),
                                        Operand::Copy(Place::local(off_local)),
                                    ],
                                },
                                span,
                            );
                        } else {
                            // Bare-disc enum variant or unknown
                            // shape: bind to zero so the body
                            // compiles. The exhaustiveness
                            // checker is supposed to gate this.
                            self.emit_assign(
                                Place::local(elem),
                                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                                span,
                            );
                        }
                        if let Some(sub) = &f.pattern {
                            let sub_pred = self.lower_pattern_predicate(elem, sub, span)?;
                            // AND into the accumulator. Today we
                            // can't easily re-write `acc`, so we
                            // emit a fresh combined local each
                            // iteration; the optimiser collapses
                            // the chain.
                            let combined = self.fresh(bool_ty);
                            self.emit_assign(
                                Place::local(combined),
                                Rvalue::BinaryOp {
                                    op: BinOp::BitAnd,
                                    lhs: Operand::Copy(Place::local(acc)),
                                    rhs: Operand::Copy(Place::local(sub_pred)),
                                },
                                span,
                            );
                            // Rebind acc to the combined value
                            // via another assign.
                            self.emit_assign(
                                Place::local(acc),
                                Rvalue::Use(Operand::Copy(Place::local(combined))),
                                span,
                            );
                        } else {
                            // Shorthand `{ x, y }` binds the field
                            // name directly to the field local.
                            self.bind_local(&f.name.name, elem);
                        }
                    }
                }
                Some(acc)
            }
            HirPatKind::Variant { name, fields } => {
                // Two encodings hide behind variant patterns in
                // compiled mode:
                //
                //   1. **User-defined enums** (registered in the
                //      `EnumIndex`): the scrutinee holds the
                //      variant's declaration index; predicate is
                //      `scrutinee == idx`.
                //   2. **`Option<T>` / `Result<T, E>` stdlib
                //      variants**: the scrutinee carries the
                //      wrapped value directly (happy-path
                //      encoding - `unwrap` is identity). The
                //      compiled tier can't actually distinguish
                //      `Ok(_)` from `Err(_)` at runtime, so the
                //      `Ok` / `Some` arm becomes the unconditional
                //      always-true predicate and `Err` / `None`
                //      becomes always-false. This compiles `?`
                //      down to "take the success path"; programs
                //      that depend on real error dispatch keep
                //      working under `gos`.
                // A bare variant name resolves through the global
                // `variant_index` map, so `None` / `Some` / `Ok` / `Err`
                // collide with any user enum that reuses one (e.g. an
                // injected `enum { BearerOnly, CookieSession, None }`).
                // When the scrutinee is genuinely a `Result` / `Option`,
                // dispatch on its real discriminant below rather than the
                // colliding user-enum variant index - otherwise the arm
                // compares the whole i128 value to that index and never
                // matches, falling through to a stale local.
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                // The value's own type names the enum: a variant name two
                // enums both declare would otherwise dispatch through
                // whichever one the program-wide map holds, comparing this
                // value against the other enum's index and representation.
                let user_enum_match = if self.is_result_or_option_adt(scrut_ty) {
                    None
                } else {
                    self.enum_index_name_of(scrut_ty)
                        .and_then(|en| {
                            self.enums
                                .variant_of_enum(&en, &name.name)
                                .map(|idx| (en, idx))
                        })
                        .or_else(|| self.enums.lookup(std::slice::from_ref(name)))
                };
                if let Some((matched_enum, idx)) = user_enum_match.clone() {
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    // For payload-bearing variants the scrutinee is a
                    // ptr to `[disc, p0, p1, ...]`; for no-payload
                    // enums the scrutinee IS the variant index. The
                    // ctor at lower_user_enum_ctor only allocates
                    // when args.len() > 0, so we must distinguish at
                    // match time: if the variant has fields OR any
                    // sibling variant has fields, treat scrutinee as
                    // ptr and load disc from offset 0. Otherwise the
                    // scrutinee is the i64 index directly.
                    let any_variant_has_payload = self.enums.enum_has_any_payload(&matched_enum);
                    // Inline-able enum: the scrutinee is the 2-word by-value
                    // `i128` [disc, payload]; the discriminant is its low word.
                    let mut peeled = scrut_ty;
                    while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                        peeled = *inner;
                    }
                    let scrut_is_inline = self.tcx.is_inline_enum_ty(peeled);
                    let scrut_for_cmp = if scrut_is_inline {
                        let disc_load = self.fresh(i64_ty);
                        self.emit_assign(
                            Place::local(disc_load),
                            Rvalue::CallIntrinsic {
                                name: "gos_rt_result_disc",
                                args: vec![Operand::Copy(Place::local(scrutinee))],
                            },
                            span,
                        );
                        disc_load
                    } else if any_variant_has_payload || !fields.is_empty() {
                        let disc_load = self.fresh(i64_ty);
                        // Tagged repr (<= 4 variants): disc in pointer
                        // bits 1-2; header repr: disc byte at payload-3.
                        let disc_intrinsic = if self.enum_repr_tagged(&matched_enum) {
                            "gos_enum_disc_tag"
                        } else {
                            "gos_enum_disc"
                        };
                        self.emit_assign(
                            Place::local(disc_load),
                            Rvalue::CallIntrinsic {
                                name: disc_intrinsic,
                                args: vec![Operand::Copy(Place::local(scrutinee))],
                            },
                            span,
                        );
                        disc_load
                    } else {
                        scrutinee
                    };
                    let lit_local = self.fresh(scrut_ty);
                    self.emit_assign(
                        Place::local(lit_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(idx as i128))),
                        span,
                    );
                    let cmp = self.fresh(bool_ty);
                    self.emit_assign(
                        Place::local(cmp),
                        Rvalue::BinaryOp {
                            op: BinOp::Eq,
                            lhs: Operand::Copy(Place::local(scrut_for_cmp)),
                            rhs: Operand::Copy(Place::local(lit_local)),
                        },
                        span,
                    );
                    // Bind payload fields and check nested patterns by
                    // loading from offsets (i+1)*8 of the scrutinee pointer.
                    let any_payload = self.enums.enum_has_any_payload(&matched_enum);
                    let declared_tys = self.enums.field_tys_of(&matched_enum, &name.name);
                    let payload_offsets = self.variant_payload_offsets(
                        declared_tys.as_deref().unwrap_or(&[]),
                        fields.len(),
                    );
                    let mut acc = cmp;
                    for (i, field) in fields.iter().enumerate() {
                        if let HirPatKind::Binding { name: bname, .. } = &field.kind {
                            // A payload word is only this variant's to read once
                            // the discriminant says so. `payload_defer_block`
                            // names the block the match arm enters after its
                            // test, so the extraction lands there rather than in
                            // the pre-branch header every sibling variant also
                            // runs through. A binding typed as a multi-slot
                            // struct materialises the payload by value, so a
                            // header read would copy from whatever word a
                            // narrower variant left in that slot.
                            let saved_current = self.current;
                            if let Some(defer) = self.payload_defer_block {
                                self.current = Some(defer);
                            }
                            if scrut_is_inline {
                                // 2-word by-value enum: the single field is the
                                // payload high word.
                                let binding_ty = self
                                    .variant_payload_ty(
                                        self.locals[scrutinee.0 as usize].ty,
                                        name.name.as_str(),
                                    )
                                    .or_else(|| {
                                        declared_tys
                                            .as_ref()
                                            .and_then(|tys| tys.get(i).copied())
                                            .filter(|&ty| {
                                                !matches!(
                                                    self.tcx.kind_of(ty),
                                                    gossamer_types::TyKind::Var(_)
                                                        | gossamer_types::TyKind::Error
                                                )
                                            })
                                    })
                                    .unwrap_or(i64_ty);
                                let is_f64 = matches!(
                                    self.tcx.kind_of(binding_ty),
                                    gossamer_types::TyKind::Float(_)
                                );
                                let getter = if is_f64 {
                                    "gos_rt_result_payload_f64"
                                } else if self.is_by_value_enum_ty(binding_ty) {
                                    "gos_rt_result_payload_i128"
                                } else {
                                    "gos_rt_result_payload"
                                };
                                let payload_local = self.fresh(binding_ty);
                                self.emit_assign(
                                    Place::local(payload_local),
                                    Rvalue::CallIntrinsic {
                                        name: getter,
                                        args: vec![Operand::Copy(Place::local(scrutinee))],
                                    },
                                    span,
                                );
                                self.bind_local(&bname.name, payload_local);
                            } else if any_payload || !fields.is_empty() {
                                let off_local = self.fresh(i64_ty);
                                self.emit_assign(
                                    Place::local(off_local),
                                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                                        payload_offsets.get(i).copied().unwrap_or((i * 8) as i64),
                                    )))),
                                    span,
                                );
                                // Use the declared variant field type (e.g. f64) so
                                // that define_var_to_with can bitcast the I64 result
                                // of gos_load to the correct type.
                                let binding_ty = self
                                    .variant_payload_ty(
                                        self.locals[scrutinee.0 as usize].ty,
                                        name.name.as_str(),
                                    )
                                    .or_else(|| {
                                        declared_tys
                                            .as_ref()
                                            .and_then(|tys| tys.get(i).copied())
                                            .filter(|&ty| {
                                                !matches!(
                                                    self.tcx.kind_of(ty),
                                                    gossamer_types::TyKind::Var(_)
                                                        | gossamer_types::TyKind::Error
                                                )
                                            })
                                    })
                                    .unwrap_or(i64_ty);
                                let payload_local = self.fresh(binding_ty);
                                self.emit_assign(
                                    Place::local(payload_local),
                                    Rvalue::CallIntrinsic {
                                        name: "gos_enum_load",
                                        args: vec![
                                            Operand::Copy(Place::local(scrutinee)),
                                            Operand::Copy(Place::local(off_local)),
                                        ],
                                    },
                                    span,
                                );
                                self.bind_local(&bname.name, payload_local);
                            } else {
                                self.bind_local(&bname.name, scrutinee);
                            }
                            self.current = saved_current;
                        } else if !matches!(field.kind, HirPatKind::Wildcard) {
                            // Nested constructor pattern (e.g. Color::Red inside
                            // Shape::Circle): load the field as i64 and recurse.
                            if scrut_is_inline {
                                let field_local = self.fresh(i64_ty);
                                self.emit_assign(
                                    Place::local(field_local),
                                    Rvalue::CallIntrinsic {
                                        name: "gos_rt_result_payload",
                                        args: vec![Operand::Copy(Place::local(scrutinee))],
                                    },
                                    span,
                                );
                                if let Some(sub_pred) =
                                    self.lower_pattern_predicate(field_local, field, span)
                                {
                                    let combined = self.fresh(bool_ty);
                                    self.emit_assign(
                                        Place::local(combined),
                                        Rvalue::BinaryOp {
                                            op: BinOp::BitAnd,
                                            lhs: Operand::Copy(Place::local(acc)),
                                            rhs: Operand::Copy(Place::local(sub_pred)),
                                        },
                                        span,
                                    );
                                    acc = combined;
                                }
                            } else if any_payload || !fields.is_empty() {
                                let off_local = self.fresh(i64_ty);
                                self.emit_assign(
                                    Place::local(off_local),
                                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                                        payload_offsets.get(i).copied().unwrap_or((i * 8) as i64),
                                    )))),
                                    span,
                                );
                                // A multi-slot aggregate field (e.g. the
                                // `Point` of `Shape::Dot(Point { x, y })`) is a
                                // boxed pointer: type the field local as the
                                // aggregate so `gos_enum_load` materialises it
                                // by value, then the nested struct / tuple
                                // pattern reads real field slots.
                                let nested_ty = declared_tys
                                    .as_ref()
                                    .and_then(|tys| tys.get(i).copied())
                                    .filter(|&t| {
                                        !matches!(
                                            self.tcx.kind_of(t),
                                            gossamer_types::TyKind::Var(_)
                                                | gossamer_types::TyKind::Error
                                        )
                                    })
                                    .filter(|&t| self.is_boxable_aggregate_payload(t));
                                let field_local = match nested_ty {
                                    Some(t) => {
                                        let fl = self.fresh(t);
                                        if let Some(sname) = self.struct_name_of(t) {
                                            self.local_struct.insert(fl, sname);
                                        }
                                        fl
                                    }
                                    None => self.fresh(i64_ty),
                                };
                                // Reading a payload word is side-effect-free
                                // whatever variant the value turned out to be,
                                // which is what lets the whole predicate run
                                // ahead of the branch. Materialising an
                                // aggregate is not: it dereferences the word as
                                // the box the payload points at and retains that
                                // box's children, so it belongs behind the
                                // discriminant test that says this variant wrote
                                // the slot. The sub-pattern reads the aggregate,
                                // so it comes along and reports through a local
                                // the header seeds as `false`.
                                let guard = nested_ty.map(|_| {
                                    let guarded = self.new_block(span);
                                    let merge = self.new_block(span);
                                    let answer = self.fresh(bool_ty);
                                    self.emit_assign(
                                        Place::local(answer),
                                        Rvalue::Use(Operand::Const(ConstValue::Bool(false))),
                                        span,
                                    );
                                    self.terminate(Terminator::SwitchInt {
                                        discriminant: Operand::Copy(Place::local(acc)),
                                        arms: vec![(0, merge)],
                                        default: guarded,
                                    });
                                    self.set_current(guarded);
                                    (answer, merge)
                                });
                                self.emit_assign(
                                    Place::local(field_local),
                                    Rvalue::CallIntrinsic {
                                        name: "gos_enum_load",
                                        args: vec![
                                            Operand::Copy(Place::local(scrutinee)),
                                            Operand::Copy(Place::local(off_local)),
                                        ],
                                    },
                                    span,
                                );
                                let sub = self.lower_pattern_predicate(field_local, field, span);
                                if let Some((answer, merge)) = guard {
                                    // Inside the guarded block `acc` is known
                                    // true, so the sub-predicate alone is the
                                    // answer for this field.
                                    match sub {
                                        Some(sub_pred) => self.emit_assign(
                                            Place::local(answer),
                                            Rvalue::Use(Operand::Copy(Place::local(sub_pred))),
                                            span,
                                        ),
                                        None => self.emit_assign(
                                            Place::local(answer),
                                            Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                                            span,
                                        ),
                                    }
                                    self.terminate(Terminator::Goto { target: merge });
                                    self.set_current(merge);
                                    acc = answer;
                                } else if let Some(sub_pred) = sub {
                                    let combined = self.fresh(bool_ty);
                                    self.emit_assign(
                                        Place::local(combined),
                                        Rvalue::BinaryOp {
                                            op: BinOp::BitAnd,
                                            lhs: Operand::Copy(Place::local(acc)),
                                            rhs: Operand::Copy(Place::local(sub_pred)),
                                        },
                                        span,
                                    );
                                    acc = combined;
                                }
                            }
                        }
                    }
                    return Some(acc);
                }
                // Result/Option dispatch picks one of two paths
                // based on the scrutinee's static type:
                //
                //   * Concrete `Result<T, E>` / `Option<T>` (Adt
                //     with our sentinel DefId): the scrutinee is a
                //     `*mut GosResult` carrying a real disc bit.
                //     Compare `gos_rt_result_disc(scrut)` to the
                //     expected arm value.
                //
                //   * Unresolved (`Var` / `Error` / `Never`) or any
                //     other shape: fall back to the happy-path
                //     encoding so legacy producers (`.send()`,
                //     `.map_err()`-chains whose return type the
                //     typer left as `Var`) keep working. `Ok` /
                //     `Some` arms are unconditionally true; `Err`
                //     / `None` arms unconditionally false.
                let expected_disc: i64 = match name.name.as_str() {
                    "Ok" | "Some" => 0,
                    _ => 1,
                };
                let scrut_ty = self.locals[scrutinee.0 as usize].ty;
                // `is_result_or_option_adt` peels any leading `Ref`, so a
                // borrowed `&Result` / `&Option` scrutinee dispatches on its
                // real discriminant (the i128 carried by value through the
                // reference) instead of falling into the happy-path alias that
                // bound the payload to the reference operand.
                let real_disc = self.is_result_or_option_adt(scrut_ty);
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let const_pred = self.fresh(bool_ty);
                if real_disc {
                    let disc_local = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(disc_local),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_disc",
                            args: vec![Operand::Copy(Place::local(scrutinee))],
                        },
                        span,
                    );
                    let lit_local = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(lit_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(expected_disc)))),
                        span,
                    );
                    self.emit_assign(
                        Place::local(const_pred),
                        Rvalue::BinaryOp {
                            op: BinOp::Eq,
                            lhs: Operand::Copy(Place::local(disc_local)),
                            rhs: Operand::Copy(Place::local(lit_local)),
                        },
                        span,
                    );
                } else {
                    let happy_path = expected_disc == 0;
                    self.emit_assign(
                        Place::local(const_pred),
                        Rvalue::Use(Operand::Const(ConstValue::Bool(happy_path))),
                        span,
                    );
                }
                if let Some(first) = fields.first() {
                    // Accept both `Some(x)` (Binding) and `Some((a, b))` (Tuple payload).
                    let is_binding = matches!(first.kind, HirPatKind::Binding { .. });
                    let is_tuple_payload = matches!(first.kind, HirPatKind::Tuple(_));
                    // A payload the arm discards (`Ok(_)`) is still the frame's
                    // to give back: the carrier is two words by value and
                    // releases nothing, so a reference-counted payload nobody
                    // names would outlive every holder. Reading it into a local
                    // of its own puts it under the ordinary release schedule,
                    // which decides ownership from where the carrier came from.
                    if real_disc
                        && !is_binding
                        && !is_tuple_payload
                        && matches!(first.kind, HirPatKind::Wildcard)
                        && let Some(payload_ty) = match name.name.as_str() {
                            "Ok" | "Some" => Some(0),
                            "Err" => Some(1),
                            _ => None,
                        }
                        .and_then(|idx| self.adt_generic_at(scrut_ty, idx))
                        && self.tcx.is_rc_managed(payload_ty)
                        && !self.is_by_value_enum_ty(payload_ty)
                    {
                        let saved_current = self.current;
                        if let Some(defer) = self.payload_defer_block {
                            self.current = Some(defer);
                        }
                        let discarded = self.fresh(payload_ty);
                        self.emit_assign(
                            Place::local(discarded),
                            Rvalue::CallIntrinsic {
                                name: "gos_rt_result_payload",
                                args: vec![Operand::Copy(Place::local(scrutinee))],
                            },
                            span,
                        );
                        self.current = saved_current;
                    }
                    if is_binding || is_tuple_payload {
                        // Bind the payload. With real discriminant
                        // encoding, allocate a fresh local from
                        // `gos_rt_result_payload`. With happy-path
                        // encoding, bind directly to the scrutinee
                        // (legacy: the scrutinee value IS the
                        // payload). Pin the binding's MIR type
                        // from the scrutinee's substs slot.
                        let payload_slot = match name.name.as_str() {
                            "Ok" | "Some" => Some(0),
                            "Err" => Some(1),
                            _ => None,
                        };
                        let payload_ty = payload_slot
                            .and_then(|idx| self.adt_generic_at(scrut_ty, idx))
                            .unwrap_or(i64_ty);
                        // Redirect payload + binding emissions to arm_block when
                        // the match arm loop set `payload_defer_block`. Unconditional
                        // payload extraction in the pre-branch header dereferences a
                        // null pointer when the scrutinee is None/Err on re-entry.
                        // Read it without consuming: every alternative of a binding
                        // or-pattern (`Some(x) | Other(x)`, `Ok(x) | Err(x)`) must
                        // bind its payload into the shared arm block from its own
                        // matched branch. Consuming here left the second alternative
                        // emitting into the pre-branch header, where last-binding-wins
                        // bound the name from an unconditional extraction. The match
                        // arm loop clears the hint once the whole pattern is lowered.
                        let saved_current = self.current;
                        if let Some(defer) = self.payload_defer_block {
                            self.current = Some(defer);
                        }
                        let payload_local = if real_disc {
                            let p = self.fresh(payload_ty);
                            // For Ok(f64) / Some(f64) payloads the
                            // i64 bit-pattern packing must be unpacked
                            // via `bitcast`, not `sitofp`. Route those
                            // through the dedicated `_f64` shim so the
                            // LLVM tier reads the f64 value back as
                            // its source type instead of treating the
                            // bit pattern as an integer.
                            let payload_extractor = if matches!(
                                self.tcx.kind_of(payload_ty),
                                gossamer_types::TyKind::Float(_)
                            ) {
                                "gos_rt_result_payload_f64"
                            } else if self.is_by_value_enum_ty(payload_ty) {
                                "gos_rt_result_payload_i128"
                            } else {
                                "gos_rt_result_payload"
                            };
                            self.emit_assign(
                                Place::local(p),
                                Rvalue::CallIntrinsic {
                                    name: payload_extractor,
                                    args: vec![Operand::Copy(Place::local(scrutinee))],
                                },
                                span,
                            );
                            p
                        } else {
                            // Happy-path: alias the scrutinee.
                            // Repin the scrutinee's MIR type when
                            // the substs gave a concrete payload
                            // type AND the scrutinee's current
                            // type is less specific (unresolved
                            // OR a generic i64 fallback that the
                            // dispatch left behind for a
                            // `Vec<Option<String>>`-style index).
                            // Without this `let row[i] : i64;
                            // match row[i] { Some(k: String) ... }`
                            // would print `k` through the integer
                            // formatter - the regex captures_all
                            // case where strings rendered as raw
                            // pointer ints. The original guard
                            // also kept tagged structs
                            // (`http::Response`/json::Value) safe
                            // by refusing to overwrite - preserve
                            // that by only repinning when the
                            // payload is a *non-Int* concrete
                            // type (String / Bool / Char / Float)
                            // OR the scrutinee is genuinely
                            // unresolved.
                            let payload_kind = self.tcx.kind_of(payload_ty).clone();
                            let scrut_kind_now = self
                                .tcx
                                .kind_of(self.locals[scrutinee.0 as usize].ty)
                                .clone();
                            let scrut_unresolved = matches!(
                                scrut_kind_now,
                                TyKind::Var(_) | TyKind::Error | TyKind::Never,
                            );
                            let payload_concrete = !matches!(
                                payload_kind,
                                TyKind::Var(_) | TyKind::Error | TyKind::Never,
                            );
                            let payload_overrides_int =
                                matches!(
                                    payload_kind,
                                    TyKind::String | TyKind::Bool | TyKind::Char | TyKind::Float(_)
                                ) && matches!(scrut_kind_now, TyKind::Int(_));
                            if payload_concrete && (scrut_unresolved || payload_overrides_int) {
                                self.locals[scrutinee.0 as usize].ty = payload_ty;
                            }
                            scrutinee
                        };
                        if is_binding {
                            let HirPatKind::Binding { name: bname, .. } = &first.kind else {
                                unreachable!()
                            };
                            self.bind_local(&bname.name, payload_local);
                            // Tag the payload local so
                            // `binding.field` / `binding.method` calls
                            // route through the right struct / runtime
                            // dispatch path. The struct/runtime-kind
                            // info is inherited from the wrapper's
                            // generic args (`Result<Opts, _>` →
                            // payload of Ok arm gets Opts).
                            if let Some(sname) = self.struct_name_of(first.ty) {
                                self.local_struct.insert(payload_local, sname);
                            }
                            let scrut_outer_ty = self.locals[scrutinee.0 as usize].ty;
                            let inner_ty = if name.name == "Err" {
                                self.second_generic_of(scrut_outer_ty)
                            } else {
                                self.first_generic_of(scrut_outer_ty)
                            };
                            if let Some(inner) = inner_ty {
                                if let Some(sname) = self.struct_name_of(inner) {
                                    let runtime_kind: Option<&'static str> = match sname.as_str() {
                                        "Error" => Some("errors::Error"),
                                        "Response" => Some("http::Response"),
                                        "Request" => Some("http::Request"),
                                        "Client" => Some("http::Client"),
                                        "Scanner" => Some("bufio::Scanner"),
                                        "Pattern" => Some("regex::Pattern"),
                                        _ => None,
                                    };
                                    self.local_struct.insert(payload_local, sname);
                                    if let Some(rk) = runtime_kind {
                                        self.local_runtime_kind.insert(payload_local, rk);
                                    }
                                }
                            }
                            if let Some(rk) = self.local_runtime_kind.get(&scrutinee).copied() {
                                self.local_runtime_kind.entry(payload_local).or_insert(rk);
                            }
                            if let Some(elem) = self.local_elem_struct.get(&scrutinee).cloned() {
                                self.local_elem_struct.entry(payload_local).or_insert(elem);
                            }
                        } else if let HirPatKind::Tuple(sub_pats) = &first.kind {
                            // `Ok((a, b))` / `Some((a, b))` - unpack the tuple
                            // payload. Carry the scrutinee's runtime kind onto
                            // the tuple payload first so an `accept()` pair
                            // re-tags its stream element (net::TcpStream /
                            // net::UnixStream) for method dispatch.
                            if let Some(rk) = self.local_runtime_kind.get(&scrutinee).copied() {
                                self.local_runtime_kind.entry(payload_local).or_insert(rk);
                            }
                            self.bind_tuple_pattern(payload_local, sub_pats, span);
                        }
                        // Restore the header block so the caller sees
                        // `const_pred` in the right block context.
                        self.current = saved_current;
                    } else if !matches!(first.kind, HirPatKind::Wildcard) {
                        // Concrete payload pattern: literal, range, nested
                        // variant, or-pattern, etc. (e.g. `Ok(1)`, `Ok(1..=5)`).
                        // `gos_rt_result_payload` null-checks internally, so
                        // calling it unconditionally in the pre-branch header
                        // is safe regardless of discriminant. The disc predicate
                        // (`const_pred`) will be false when the variant doesn't
                        // match, making the combined AND false too.
                        let payload_slot = match name.name.as_str() {
                            "Ok" | "Some" => Some(0usize),
                            "Err" => Some(1usize),
                            _ => None,
                        };
                        let payload_ty = payload_slot
                            .and_then(|idx| self.adt_generic_at(scrut_ty, idx))
                            .unwrap_or(i64_ty);
                        let payload_local = if real_disc {
                            let p = self.fresh(payload_ty);
                            // For Ok(f64) / Some(f64) payloads the
                            // i64 bit-pattern packing must be unpacked
                            // via `bitcast`, not `sitofp`. Route those
                            // through the dedicated `_f64` shim so the
                            // LLVM tier reads the f64 value back as
                            // its source type instead of treating the
                            // bit pattern as an integer.
                            let payload_extractor = if matches!(
                                self.tcx.kind_of(payload_ty),
                                gossamer_types::TyKind::Float(_)
                            ) {
                                "gos_rt_result_payload_f64"
                            } else if self.is_by_value_enum_ty(payload_ty) {
                                "gos_rt_result_payload_i128"
                            } else {
                                "gos_rt_result_payload"
                            };
                            self.emit_assign(
                                Place::local(p),
                                Rvalue::CallIntrinsic {
                                    name: payload_extractor,
                                    args: vec![Operand::Copy(Place::local(scrutinee))],
                                },
                                span,
                            );
                            p
                        } else {
                            scrutinee
                        };
                        if let Some(sub_pred) =
                            self.lower_pattern_predicate(payload_local, first, span)
                        {
                            let combined = self.fresh(bool_ty);
                            self.emit_assign(
                                Place::local(combined),
                                Rvalue::BinaryOp {
                                    op: BinOp::BitAnd,
                                    lhs: Operand::Copy(Place::local(const_pred)),
                                    rhs: Operand::Copy(Place::local(sub_pred)),
                                },
                                span,
                            );
                            return Some(combined);
                        }
                    }
                }
                Some(const_pred)
            }
            HirPatKind::Literal(_) => None,
            HirPatKind::At { name, sub, .. } => {
                // `x @ subpat`: bind `x` to the scrutinee, then run
                // the subpattern's filter. Without recursing into
                // `sub` the constraint is dropped - every input
                // matches the arm and the user's intent (e.g.
                // `x @ 1..=3`) is silently widened to `x => …`.
                self.bind_local(&name.name, scrutinee);
                self.lower_pattern_predicate(scrutinee, sub, span)
            }
        }
    }

    /// Desugars a slice pattern `[p.., ..rest, ..q]` matched against a
    /// `Vec`/`Slice`/`Array` scrutinee into a length guard plus
    /// per-element binds. Prefix elements read `xs[i]`, suffix elements
    /// read `xs[len - n_suffix + j]`, and a `..rest` binding captures
    /// `xs.slice(n_prefix, len - n_suffix)`. The returned local holds
    /// the conjoined boolean match predicate.
    fn lower_slice_pattern(
        &mut self,
        scrutinee: Local,
        prefix: &[HirPat],
        rest: Option<&HirPat>,
        suffix: &[HirPat],
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let bool_ty = self.tcx.bool_ty();
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let (elem_ty, fixed_array) = {
            let mut base = self.locals[scrutinee.0 as usize].ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(base) {
                base = *inner;
            }
            match self.tcx.kind_of(base).clone() {
                TyKind::Array { elem, len } => (elem, Some(len)),
                TyKind::Vec(e) | TyKind::Slice(e) => (e, None),
                _ => (i64_ty, None),
            }
        };
        // A fixed-size `[T; N]` scrutinee is a flat stack aggregate, not a
        // GosVec. Materialize it as a heap vector so the length / element /
        // slice helpers below read a real header instead of treating the
        // first stack slot as a vec length and a bogus capacity.
        let scrutinee = if let Some(len) = fixed_array {
            self.coerce_array_to_vec(scrutinee, elem_ty, len, span)
        } else {
            scrutinee
        };
        // len = gos_rt_vec_len(scrutinee)
        let len_local = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(scrutinee))],
            destination: Place::local(len_local),
            target: Some(next),
        });
        self.set_current(next);

        let n_prefix = prefix.len() as i128;
        let n_suffix = suffix.len() as i128;
        // Length predicate: `len >= n_prefix + n_suffix` with a `..`,
        // `len == n_prefix` for a fixed-length slice.
        let mut acc = self.fresh(bool_ty);
        let bound_local = self.fresh(i64_ty);
        if rest.is_some() {
            self.emit_assign(
                Place::local(bound_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(n_prefix + n_suffix))),
                span,
            );
            self.emit_assign(
                Place::local(acc),
                Rvalue::BinaryOp {
                    op: BinOp::Ge,
                    lhs: Operand::Copy(Place::local(len_local)),
                    rhs: Operand::Copy(Place::local(bound_local)),
                },
                span,
            );
        } else {
            self.emit_assign(
                Place::local(bound_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(n_prefix))),
                span,
            );
            self.emit_assign(
                Place::local(acc),
                Rvalue::BinaryOp {
                    op: BinOp::Eq,
                    lhs: Operand::Copy(Place::local(len_local)),
                    rhs: Operand::Copy(Place::local(bound_local)),
                },
                span,
            );
        }
        // Prefix elements: `xs[i]`.
        for (i, sub) in prefix.iter().enumerate() {
            let idx_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(idx_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(i as i128))),
                span,
            );
            let elem_local = self.emit_vec_get(scrutinee, idx_local, elem_ty, span);
            let sub_pred = self.lower_pattern_predicate(elem_local, sub, span)?;
            acc = self.and_bool(acc, sub_pred, span);
        }
        // Suffix elements: `xs[len - n_suffix + j]`.
        for (j, sub) in suffix.iter().enumerate() {
            let offset_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(offset_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(j as i128 - n_suffix))),
                span,
            );
            let idx_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(idx_local),
                Rvalue::BinaryOp {
                    op: BinOp::Add,
                    lhs: Operand::Copy(Place::local(len_local)),
                    rhs: Operand::Copy(Place::local(offset_local)),
                },
                span,
            );
            let elem_local = self.emit_vec_get(scrutinee, idx_local, elem_ty, span);
            let sub_pred = self.lower_pattern_predicate(elem_local, sub, span)?;
            acc = self.and_bool(acc, sub_pred, span);
        }
        // `..rest` binding: `xs.slice(n_prefix, len - n_suffix)`.
        if let Some(rest) = rest {
            if let HirPatKind::Binding { name, mutable } = &rest.kind {
                let lo_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(lo_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(n_prefix))),
                    span,
                );
                let hi_offset = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(hi_offset),
                    Rvalue::Use(Operand::Const(ConstValue::Int(-n_suffix))),
                    span,
                );
                let hi_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(hi_local),
                    Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs: Operand::Copy(Place::local(len_local)),
                        rhs: Operand::Copy(Place::local(hi_offset)),
                    },
                    span,
                );
                let rest_ty = match self.tcx.kind_of(rest.ty) {
                    TyKind::Vec(_) | TyKind::Slice(_) => rest.ty,
                    _ => self.tcx.intern(TyKind::Vec(elem_ty)),
                };
                let rest_local =
                    self.push_local(rest_ty, Some(Ident::new(name.name.as_str())), *mutable);
                let after = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_slice".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(scrutinee)),
                        Operand::Copy(Place::local(lo_local)),
                        Operand::Copy(Place::local(hi_local)),
                    ],
                    destination: Place::local(rest_local),
                    target: Some(after),
                });
                self.set_current(after);
                self.bind_local(name.name.as_str(), rest_local);
            }
        }
        Some(acc)
    }

    /// Reads `xs[index]` for a `Vec`/`Slice` scrutinee, choosing the
    /// runtime accessor that matches the element's slot shape, and
    /// returns the destination local (pinned to `elem_ty`).
    fn emit_vec_get(&mut self, base: Local, index: Local, elem_ty: Ty, span: Span) -> Local {
        use gossamer_types::TyKind;
        let elem_is_result_option = matches!(
            self.tcx.kind_of(elem_ty),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        );
        let elem_is_multislot =
            !elem_is_result_option && self.tcx.elem_is_addressed_aggregate(elem_ty);
        let helper = if elem_is_result_option {
            "gos_rt_vec_get_i128"
        } else if elem_is_multislot {
            "gos_rt_vec_get_ptr"
        } else {
            "gos_rt_vec_get_i64"
        };
        let dest = self.fresh(elem_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args: vec![
                Operand::Copy(Place::local(base)),
                Operand::Copy(Place::local(index)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        if let Some(name) = self.struct_name_of(elem_ty) {
            self.local_struct.insert(dest, name);
        }
        dest
    }

    /// Bitwise-ANDs two boolean predicate locals into a fresh local.
    fn and_bool(&mut self, lhs: Local, rhs: Local, span: Span) -> Local {
        let bool_ty = self.tcx.bool_ty();
        let combined = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(combined),
            Rvalue::BinaryOp {
                op: BinOp::BitAnd,
                lhs: Operand::Copy(Place::local(lhs)),
                rhs: Operand::Copy(Place::local(rhs)),
            },
            span,
        );
        combined
    }

    pub(crate) fn lower_cast(
        &mut self,
        value: &HirExpr,
        target: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let value_local = self.lower_expr(value)?;
        let dest = self.fresh(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Cast {
                operand: Operand::Copy(Place::local(value_local)),
                target,
            },
            span,
        );
        Some(dest)
    }

    pub(crate) fn bind_tuple_pattern(
        &mut self,
        tuple_local: Local,
        sub_patterns: &[HirPat],
        span: Span,
    ) {
        use gossamer_types::TyKind;
        // A sequence taken apart positionally reads its elements through the
        // indexed accessor. `Projection::Field` addresses an aggregate's own
        // slots, which on a `Vec` are its header words rather than its
        // elements.
        let scrutinee_ty = self.peel_ref_ty(self.locals[tuple_local.0 as usize].ty);
        if let TyKind::Vec(elem) | TyKind::Slice(elem) = self.tcx.kind_of(scrutinee_ty).clone() {
            self.bind_sequence_tuple_pattern(tuple_local, sub_patterns, elem, span);
            return;
        }
        // `TcpListener::accept` returns `(TcpStream-handle, peer-addr)`;
        // the call dest is tagged `net::accept_pair`, which flows here
        // through the `Ok(p)`/match-result kind propagation. Re-tag the
        // stream element so `stream.read(..)` dispatches to the runtime
        // helper rather than reading the raw i64 handle.
        let accept_kind = self.local_runtime_kind.get(&tuple_local).copied();
        let accept_pair = matches!(
            accept_kind,
            Some("net::accept_pair" | "net::unix_accept_pair")
        );
        let temp_file_pair = accept_kind == Some("fs::temp_file_pair");
        let accept_stream_kind = if accept_kind == Some("net::unix_accept_pair") {
            "net::UnixStream"
        } else {
            "net::TcpStream"
        };
        let rest_pos = sub_patterns
            .iter()
            .position(|p| matches!(p.kind, HirPatKind::Rest));
        let n_after = rest_pos.map_or(0, |r| sub_patterns.len() - r - 1);
        // Determine the tuple's total field count from its type for
        // correct rest-pattern tail indexing.
        let total_len = if rest_pos.is_some() {
            let tuple_ty = self.locals[tuple_local.0 as usize].ty;
            if let TyKind::Tuple(elems) = self.tcx.kind_of(tuple_ty).clone() {
                elems.len()
            } else {
                sub_patterns.len()
            }
        } else {
            sub_patterns.len()
        };
        for (i, sub) in sub_patterns.iter().enumerate() {
            if matches!(sub.kind, HirPatKind::Rest | HirPatKind::Wildcard) {
                continue;
            }
            let field_idx = if let Some(rest_idx) = rest_pos {
                if i < rest_idx {
                    i
                } else {
                    total_len - n_after + (i - rest_idx - 1)
                }
            } else {
                i
            };
            // Use the sub-pattern's type when concrete; derive from the
            // tuple's field type otherwise. Without this fallback, bindings
            // from `Some((n, m))` get `Var(_)` types and the print dispatcher
            // calls `gos_rt_concat_str` instead of `gos_rt_concat_i64`.
            let pat_ty_unresolved = matches!(
                self.tcx.kind_of(sub.ty).clone(),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            );
            let elem_ty = if pat_ty_unresolved {
                let tuple_ty = self.locals[tuple_local.0 as usize].ty;
                if let TyKind::Tuple(elems) = self.tcx.kind_of(tuple_ty).clone() {
                    elems.get(field_idx).copied().unwrap_or(sub.ty)
                } else {
                    sub.ty
                }
            } else {
                sub.ty
            };
            // A nested destructuring sub-pattern (`(b, c)`, `Point { x, y }`,
            // a tuple-struct variant) extracts its field into a fresh local
            // and recurses; a bare `else { continue }` skipped it, leaving its
            // bindings unmaterialised (`let a, (b, c) = t` lost b and c).
            let (name, mutable) = match &sub.kind {
                HirPatKind::Binding { name, mutable } => (name, *mutable),
                _ => {
                    let field_local = self.push_local(elem_ty, None, false);
                    let place = Place {
                        local: tuple_local,
                        projection: vec![crate::ir::Projection::Field(
                            u32::try_from(field_idx).expect("tuple projection overflow"),
                        )],
                    };
                    self.emit_assign(
                        Place::local(field_local),
                        Rvalue::Use(Operand::Copy(place)),
                        span,
                    );
                    self.bind_aggregate_let_pattern(field_local, sub, span);
                    continue;
                }
            };
            let element_local =
                self.push_local(elem_ty, Some(Ident::new(name.name.as_str())), mutable);
            self.bind_local(name.name.as_str(), element_local);
            if accept_pair && field_idx == 0 {
                self.local_runtime_kind
                    .insert(element_local, accept_stream_kind);
            }
            if temp_file_pair && field_idx == 0 {
                self.local_runtime_kind.insert(element_local, "fs::File");
            }
            let projection = vec![crate::ir::Projection::Field(
                u32::try_from(field_idx).expect("tuple projection overflow"),
            )];
            let place = Place {
                local: tuple_local,
                projection,
            };
            self.emit_assign(
                Place::local(element_local),
                Rvalue::Use(Operand::Copy(place)),
                span,
            );
        }
    }

    /// Binds a positional pattern against a `Vec` / slice scrutinee, reading
    /// each part with the indexed accessor for the element's slot shape.
    ///
    /// A `..` rest counts from the end, so the parts written after it read
    /// the tail. The sequence's length is a runtime property, so a part past
    /// the end reads whatever `xs[i]` reads there - the same contract an
    /// explicit index has.
    fn bind_sequence_tuple_pattern(
        &mut self,
        seq_local: Local,
        sub_patterns: &[HirPat],
        elem_ty: Ty,
        span: Span,
    ) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let rest_pos = sub_patterns
            .iter()
            .position(|p| matches!(p.kind, HirPatKind::Rest));
        let len_local = rest_pos.map(|_| {
            self.emit_combinator_call(
                "gos_rt_vec_len",
                vec![Operand::Copy(Place::local(seq_local))],
                i64_ty,
                span,
            )
        });
        for (i, sub) in sub_patterns.iter().enumerate() {
            if matches!(sub.kind, HirPatKind::Rest | HirPatKind::Wildcard) {
                continue;
            }
            let index = self.fresh(i64_ty);
            match (rest_pos, len_local) {
                (Some(rest_idx), Some(len)) if i > rest_idx => {
                    let from_end = i64::try_from(sub_patterns.len() - i).unwrap_or(0);
                    self.emit_assign(
                        Place::local(index),
                        Rvalue::BinaryOp {
                            op: BinOp::Sub,
                            lhs: Operand::Copy(Place::local(len)),
                            rhs: Operand::Const(ConstValue::Int(i128::from(from_end))),
                        },
                        span,
                    );
                }
                _ => {
                    self.emit_assign(
                        Place::local(index),
                        Rvalue::Use(Operand::Const(ConstValue::Int(
                            i128::try_from(i).unwrap_or(0),
                        ))),
                        span,
                    );
                }
            }
            let element = self.emit_vec_get(seq_local, index, elem_ty, span);
            match &sub.kind {
                HirPatKind::Binding { name, .. } => {
                    self.bind_local(name.name.as_str(), element);
                }
                _ => self.bind_aggregate_let_pattern(element, sub, span),
            }
        }
    }

    /// Bind the locals introduced by an irrefutable struct or tuple-struct
    /// `let` pattern. Each field is read through a `Projection::Field` of the
    /// destructured aggregate and bound to a fresh local; nested patterns
    /// recurse, so `let Nested { p: Point { x, y }, label } = n` binds `x`,
    /// `y`, and `label` to the projected field values.
    pub(crate) fn bind_aggregate_let_pattern(
        &mut self,
        aggregate: Local,
        pattern: &HirPat,
        span: Span,
    ) {
        match &pattern.kind {
            HirPatKind::Binding { name, .. } => {
                self.bind_local(name.name.as_str(), aggregate);
            }
            HirPatKind::At { name, sub, .. } => {
                self.bind_local(name.name.as_str(), aggregate);
                self.bind_aggregate_let_pattern(aggregate, sub, span);
            }
            HirPatKind::Ref { inner, .. } => {
                self.bind_aggregate_let_pattern(aggregate, inner, span);
            }
            HirPatKind::Tuple(sub_patterns) => {
                for (idx, sub) in sub_patterns.iter().enumerate() {
                    self.bind_let_field(aggregate, idx, sub, span);
                }
            }
            HirPatKind::Variant { name, fields } => {
                if self.enums.lookup(std::slice::from_ref(name)).is_some() {
                    // Enum tuple variant: the payload sits after the
                    // discriminant header, so read it discriminant-aware
                    // (the bare `Projection::Field(i)` path reads the wrong
                    // slot on the native tiers).
                    self.bind_variant_let_payload(aggregate, name, fields, span);
                } else {
                    // Tuple struct (the autoderive rewrite usually turns these
                    // into struct patterns): positional fields in order.
                    for (idx, sub) in fields.iter().enumerate() {
                        self.bind_let_field(aggregate, idx, sub, span);
                    }
                }
            }
            HirPatKind::Struct { name, fields, .. } => {
                let order = self.structs.get(&name.name).cloned();
                for f in fields {
                    let Some(idx) = order
                        .as_ref()
                        .and_then(|o| o.iter().position(|n| n == &f.name.name))
                    else {
                        continue;
                    };
                    match &f.pattern {
                        Some(sub) => self.bind_let_field(aggregate, idx, sub, span),
                        None => {
                            // Shorthand `{ x }` binds the field name to its value.
                            let field_ty = self
                                .aggregate_field_ty(aggregate, idx)
                                .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                            let elem = self.push_local(
                                field_ty,
                                Some(Ident::new(f.name.name.as_str())),
                                false,
                            );
                            self.emit_field_copy(aggregate, idx, elem, span);
                            self.bind_local(f.name.name.as_str(), elem);
                        }
                    }
                }
            }
            HirPatKind::Or(branches) => {
                self.bind_or_let_pattern(aggregate, branches, span);
            }
            HirPatKind::Wildcard
            | HirPatKind::Rest
            | HirPatKind::Literal(_)
            | HirPatKind::Range { .. }
            | HirPatKind::Slice { .. } => {}
        }
    }

    /// Binds an enum tuple-variant's payload in an irrefutable `let`
    /// (`let E::P(m, n) = e`), discriminant-aware. A payload-bearing enum is
    /// a pointer to `[disc, p0, p1, ...]`, so field `i` is read with
    /// `gos_enum_load(scrutinee, i*8)` (the helper skips the discriminant
    /// header); a 2-word inline enum reads its single payload with
    /// `gos_rt_result_payload`. Mirrors the match lowering's extraction - the
    /// previous bare `Projection::Field(i)` read the discriminant as the
    /// first field on the native tiers.
    fn bind_variant_let_payload(
        &mut self,
        aggregate: Local,
        name: &Ident,
        fields: &[HirPat],
        span: Span,
    ) {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let scrut_ty = self.locals[aggregate.0 as usize].ty;
        let mut peeled = scrut_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        let scrut_is_inline = self.tcx.is_inline_enum_ty(peeled);
        let any_payload = self.enum_index_name_of(scrut_ty).map_or_else(
            || self.enums.has_any_payload(std::slice::from_ref(name)),
            |en| self.enums.enum_has_any_payload(&en),
        );
        let declared_tys = self
            .enum_index_name_of(scrut_ty)
            .and_then(|en| self.enums.field_tys_of(&en, &name.name))
            .or_else(|| self.enums.variant_field_tys.get(&name.name).cloned());
        let payload_offsets =
            self.variant_payload_offsets(declared_tys.as_deref().unwrap_or(&[]), fields.len());
        for (i, field) in fields.iter().enumerate() {
            if matches!(field.kind, HirPatKind::Wildcard | HirPatKind::Rest) {
                continue;
            }
            let binding_ty = self
                .variant_payload_ty(scrut_ty, name.name.as_str())
                .or_else(|| {
                    declared_tys
                        .as_ref()
                        .and_then(|tys| tys.get(i).copied())
                        .filter(|&ty| {
                            !matches!(self.tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error)
                        })
                })
                .unwrap_or(i64_ty);
            // A payload matched through a `&mut` scrutinee is named in place:
            // the binding takes the address of the payload the enum holds, so
            // a `&mut self` method reaches the enum's own value. A payload
            // matched by value is copied, which is what a read wants and what
            // an immutable binding can promise.
            // A payload matched through a `&mut` scrutinee names the enum's
            // own value, so the binding takes the address of the words the
            // slot stands for. Which address that is - the slot's, or the
            // block it points at - is each back end's own answer, which
            // `gos_enum_slot_ptr` asks for.
            let payload_is_aggregate = matches!(
                self.tcx.kind_of(binding_ty),
                TyKind::Adt { .. } | TyKind::Tuple(_) | TyKind::Array { .. }
            ) && !self.tcx.is_inline_enum_ty(binding_ty)
                && !self.is_by_value_enum_ty(binding_ty)
                && self.type_slot_bytes(binding_ty) >= 8;
            let bind_in_place = !scrut_is_inline
                && payload_is_aggregate
                && matches!(
                    self.tcx.kind_of(scrut_ty),
                    TyKind::Ref {
                        mutability: gossamer_types::Mutbl::Mut,
                        ..
                    }
                );
            let payload_local = if bind_in_place {
                let ref_ty = self.tcx.intern(TyKind::Ref {
                    mutability: gossamer_types::Mutbl::Mut,
                    inner: binding_ty,
                });
                self.fresh(ref_ty)
            } else {
                self.fresh(binding_ty)
            };
            if scrut_is_inline {
                let getter = if matches!(self.tcx.kind_of(binding_ty), TyKind::Float(_)) {
                    "gos_rt_result_payload_f64"
                } else if self.is_by_value_enum_ty(binding_ty) {
                    "gos_rt_result_payload_i128"
                } else {
                    "gos_rt_result_payload"
                };
                self.emit_assign(
                    Place::local(payload_local),
                    Rvalue::CallIntrinsic {
                        name: getter,
                        args: vec![Operand::Copy(Place::local(aggregate))],
                    },
                    span,
                );
            } else if any_payload || !fields.is_empty() {
                let off_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(off_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                        payload_offsets.get(i).copied().unwrap_or((i * 8) as i64),
                    )))),
                    span,
                );
                // A boxed payload's slot holds the block its words live in,
                // which the load answers. A slot-resident payload's words are
                // the slot, whose home each back end names for itself.
                let reader = if bind_in_place && !self.is_boxable_aggregate_payload(binding_ty) {
                    "gos_enum_slot_ptr"
                } else {
                    "gos_enum_load"
                };
                self.emit_assign(
                    Place::local(payload_local),
                    Rvalue::CallIntrinsic {
                        name: reader,
                        args: vec![
                            Operand::Copy(Place::local(aggregate)),
                            Operand::Copy(Place::local(off_local)),
                        ],
                    },
                    span,
                );
            } else {
                continue;
            }
            match &field.kind {
                HirPatKind::Binding { name: bname, .. } => {
                    self.bind_local(&bname.name, payload_local);
                }
                _ => self.bind_aggregate_let_pattern(payload_local, field, span),
            }
        }
    }

    /// Bind the locals introduced by an irrefutable `let` or-pattern
    /// `(A | B | ...)`. Every alternative binds the same set of names (the
    /// language requires consistent bindings), so each alternative's
    /// discriminant predicate selects, at runtime, the projections that
    /// feed one shared result local per name. The matching alternative
    /// writes the bound name's value into that result local; control then
    /// merges and the name resolves to it for the rest of the scope.
    /// Disjunction across alternatives, binding from the one that matched.
    ///
    /// Each alternative names its own payload positions - `A(text)` reads
    /// field 0 where `B(_, text)` reads field 1 - so a name's binding has to
    /// come from the alternative that actually matched. Merging the
    /// predicates alone would leave the last alternative's extraction
    /// standing for every one of them, and every arm would read that
    /// alternative's slot.
    fn lower_or_pattern_predicate(
        &mut self,
        scrutinee: Local,
        branches: &[HirPat],
        span: Span,
    ) -> Option<Local> {
        let bool_ty = self.tcx.bool_ty();
        let mut names: Vec<String> = Vec::new();
        if let Some(first) = branches.first() {
            collect_pattern_binding_names(first, &mut names);
        }
        let matched_flag = self.push_local(bool_ty, None, true);
        self.emit_assign(
            Place::local(matched_flag),
            Rvalue::Use(Operand::Const(ConstValue::Bool(false))),
            span,
        );
        let merge = self.new_block(span);
        let mut results: HashMap<String, Local> = HashMap::new();
        for branch in branches {
            let matched = self.new_block(span);
            let next = self.new_block(span);
            self.push_scope();
            // Extraction is deferred into the matched block so a
            // non-matching alternative never reads the wrong payload.
            let outer_defer = self.payload_defer_block.take();
            self.payload_defer_block = Some(matched);
            let pred = self.lower_pattern_predicate(scrutinee, branch, span)?;
            self.payload_defer_block = outer_defer;
            self.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::local(pred)),
                arms: vec![(0, next)],
                default: matched,
            });
            self.set_current(matched);
            for name in &names {
                let Some(src) = self.lookup_local(name) else {
                    continue;
                };
                let src_ty = self.locals[src.0 as usize].ty;
                let dst = match results.get(name).copied() {
                    Some(existing) => {
                        let existing_ty = self.locals[existing.0 as usize].ty;
                        let loose = matches!(
                            self.tcx.kind_of(existing_ty),
                            gossamer_types::TyKind::Var(_)
                                | gossamer_types::TyKind::Error
                                | gossamer_types::TyKind::Never
                        );
                        let concrete = !matches!(
                            self.tcx.kind_of(src_ty),
                            gossamer_types::TyKind::Var(_)
                                | gossamer_types::TyKind::Error
                                | gossamer_types::TyKind::Never
                        );
                        if loose && concrete {
                            self.locals[existing.0 as usize].ty = src_ty;
                        }
                        existing
                    }
                    None => {
                        let local = self.push_local(src_ty, Some(Ident::new(name.as_str())), true);
                        results.insert(name.clone(), local);
                        local
                    }
                };
                if let Some(sn) = self.local_struct.get(&src).cloned() {
                    self.local_struct.insert(dst, sn);
                }
                if let Some(rk) = self.local_runtime_kind.get(&src).copied() {
                    self.local_runtime_kind.insert(dst, rk);
                }
                if let Some(en) = self.local_elem_struct.get(&src).cloned() {
                    self.local_elem_struct.insert(dst, en);
                }
                self.emit_assign(
                    Place::local(dst),
                    Rvalue::Use(Operand::Copy(Place::local(src))),
                    span,
                );
            }
            self.emit_assign(
                Place::local(matched_flag),
                Rvalue::Use(Operand::Const(ConstValue::Bool(true))),
                span,
            );
            self.terminate(Terminator::Goto { target: merge });
            self.pop_scope();
            self.set_current(next);
        }
        self.terminate(Terminator::Goto { target: merge });
        self.set_current(merge);
        for (name, local) in &results {
            self.bind_local(name, *local);
        }
        Some(matched_flag)
    }

    pub(crate) fn bind_or_let_pattern(
        &mut self,
        scrutinee: Local,
        branches: &[HirPat],
        span: Span,
    ) {
        let mut names: Vec<String> = Vec::new();
        if let Some(first) = branches.first() {
            collect_pattern_binding_names(first, &mut names);
        }
        let mut results: HashMap<String, Local> = HashMap::new();
        let merge = self.new_block(span);
        for branch in branches {
            let matched = self.new_block(span);
            let next = self.new_block(span);
            self.push_scope();
            // Defer Result/Option payload extraction to the matched block so
            // a non-matching alternative never dereferences the wrong payload.
            self.payload_defer_block = Some(matched);
            let Some(pred) = self.lower_pattern_predicate(scrutinee, branch, span) else {
                let kind = pattern_kind_label(branch);
                panic!(
                    "MIR lower: irrefutable let or-pattern has unsupported alternative \
                     shape ({kind}); add explicit destructuring for it"
                );
            };
            self.payload_defer_block = None;
            self.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::local(pred)),
                arms: vec![(0, next)],
                default: matched,
            });
            self.set_current(matched);
            for name in &names {
                let Some(src) = self.lookup_local(name) else {
                    continue;
                };
                let src_ty = self.locals[src.0 as usize].ty;
                let dst = match results.get(name).copied() {
                    Some(existing) => {
                        let existing_ty = self.locals[existing.0 as usize].ty;
                        let existing_loose = matches!(
                            self.tcx.kind_of(existing_ty),
                            gossamer_types::TyKind::Var(_)
                                | gossamer_types::TyKind::Error
                                | gossamer_types::TyKind::Never
                        );
                        let src_concrete = !matches!(
                            self.tcx.kind_of(src_ty),
                            gossamer_types::TyKind::Var(_)
                                | gossamer_types::TyKind::Error
                                | gossamer_types::TyKind::Never
                        );
                        if existing_loose && src_concrete {
                            self.locals[existing.0 as usize].ty = src_ty;
                        }
                        existing
                    }
                    None => {
                        let l = self.push_local(src_ty, Some(Ident::new(name.as_str())), false);
                        results.insert(name.clone(), l);
                        l
                    }
                };
                if let Some(sn) = self.local_struct.get(&src).cloned() {
                    self.local_struct.insert(dst, sn);
                }
                if let Some(rk) = self.local_runtime_kind.get(&src).copied() {
                    self.local_runtime_kind.insert(dst, rk);
                }
                if let Some(en) = self.local_elem_struct.get(&src).cloned() {
                    self.local_elem_struct.insert(dst, en);
                }
                self.emit_assign(
                    Place::local(dst),
                    Rvalue::Use(Operand::Copy(Place::local(src))),
                    span,
                );
            }
            self.terminate(Terminator::Goto { target: merge });
            self.pop_scope();
            self.set_current(next);
        }
        // An exhaustive or-pattern always matches one alternative; the
        // fall-through past the last alternative is unreachable but kept
        // wired so the CFG has no dangling block.
        self.terminate(Terminator::Goto { target: merge });
        self.set_current(merge);
        for (name, local) in &results {
            self.bind_local(name, *local);
        }
    }

    /// Project field `idx` of `aggregate` into a fresh local, then recurse
    /// into `sub` so nested destructure binds its leaf names.
    fn bind_let_field(&mut self, aggregate: Local, idx: usize, sub: &HirPat, span: Span) {
        if matches!(sub.kind, HirPatKind::Wildcard | HirPatKind::Rest) {
            return;
        }
        let field_ty = self.let_field_ty(aggregate, idx, sub);
        let elem = self.push_local(field_ty, param_name(sub), param_mutable(sub));
        self.emit_field_copy(aggregate, idx, elem, span);
        self.bind_aggregate_let_pattern(elem, sub, span);
    }

    /// Type for a destructured field local. The declared aggregate field type
    /// is preferred (it carries the nested Adt / tuple type that lets the
    /// recursion project further); the sub-pattern's own type is the fallback.
    fn let_field_ty(&mut self, aggregate: Local, idx: usize, sub: &HirPat) -> Ty {
        use gossamer_types::TyKind;
        let concrete = |t: Ty| !matches!(self.tcx.kind_of(t), TyKind::Var(_) | TyKind::Error);
        if let Some(ft) = self.aggregate_field_ty(aggregate, idx)
            && concrete(ft)
        {
            return ft;
        }
        if !matches!(
            self.tcx.kind_of(sub.ty),
            TyKind::Var(_) | TyKind::Error | TyKind::Never
        ) {
            return sub.ty;
        }
        self.tcx.int_ty(gossamer_types::IntTy::I64)
    }

    /// Declared type of field `idx` of the aggregate held by `local`, looking
    /// through references. `None` when the local is not a struct / tuple.
    fn aggregate_field_ty(&self, local: Local, idx: usize) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut ty = self.locals[local.0 as usize].ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Adt { def, .. } => self
                .tcx
                .struct_field_tys(*def)
                .and_then(|tys| tys.get(idx).copied()),
            TyKind::Tuple(elems) => elems.get(idx).copied(),
            _ => None,
        }
    }

    /// Emit `dest = aggregate.idx` reading the field by projection.
    fn emit_field_copy(&mut self, aggregate: Local, idx: usize, dest: Local, span: Span) {
        let place = Place {
            local: aggregate,
            projection: vec![crate::ir::Projection::Field(
                u32::try_from(idx).expect("field projection overflow"),
            )],
        };
        self.emit_assign(Place::local(dest), Rvalue::Use(Operand::Copy(place)), span);
    }

    /// Index into `loop_stack` of the loop a `break`/`continue` targets:
    /// the innermost loop carrying a matching label, or the innermost
    /// loop of any label when no label is given. `None` when no such
    /// loop is in scope.
    pub(crate) fn resolve_loop_target(&self, label: Option<&str>) -> Option<usize> {
        match label {
            None => self.loop_stack.len().checked_sub(1),
            Some(name) => self
                .loop_stack
                .iter()
                .rposition(|ctx| ctx.label.as_deref() == Some(name)),
        }
    }

    pub(crate) fn lower_while(&mut self, condition: &HirExpr, body: &HirExpr, span: Span) {
        let label = self.pending_loop_label.take();
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let Some(cond_local) = self.lower_expr(condition) else {
            return;
        };
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cond_local)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        // Auto-region: if the body's allocations provably die at the
        // iteration boundary, wrap it in an arena region so the whole
        // iteration's heap is bulk-freed at the back-edge instead of a
        // per-node refcount teardown. Eligibility is decided on the HIR
        // before lowering so `region_depth` flags the body's locals.
        let regioned = self.begin_loop_region(body, span);
        // `break` jumps to `exit`; `continue` jumps back to the
        // condition test (`header`).
        self.loop_stack.push(LoopContext {
            continue_to: header,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label,
        });
        let _ = self.lower_expr(body);
        self.loop_stack.pop();
        // Eligibility guarantees no early exit, so `current` is the body's
        // fall-through; the pop lands before the back-edge.
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
    }

    /// Recovers a variant payload's type from the scrutinee enum's
    /// substitutions when the declared variant field type is unresolved
    /// (`Var`). For `Result<T, E>` the `Ok`/`Some` payload is the first type
    /// argument and `Err` the second. This pins `let s = f()?` for
    /// `f -> Result<String, _>` inside a function whose own return type left
    /// the extraction local `Var`, so the drop pass sees it is RC-managed and
    /// releases it (otherwise the extracted string leaks).
    fn variant_payload_ty(&self, scrut_ty: Ty, variant: &str) -> Option<Ty> {
        use gossamer_types::TyKind;
        // Only the built-in `Result`/`Option` variants: their declared field
        // types are the generic parameter (often defaulted to `i64`), so the
        // concrete substitution on the scrutinee is the authority. User enum
        // variants keep their declared field types.
        let idx = match variant {
            "Ok" | "Some" => 0,
            "Err" => 1,
            _ => return None,
        };
        let TyKind::Adt { substs, .. } = self.tcx.kind_of(scrut_ty) else {
            return None;
        };
        substs
            .types()
            .get(idx)
            .copied()
            .filter(|&ty| !matches!(self.tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error))
    }

    /// Emits a zero-argument unit-returning call to a runtime region helper
    /// (`gos_rt_arena_push` / `gos_rt_arena_pop`) and continues lowering
    /// in a fresh block.
    pub(crate) fn emit_region_call(&mut self, sym: &str, span: Span) {
        let unit = self.tcx.unit();
        let dest = self.fresh(unit);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(sym.to_string())),
            args: vec![],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
    }

    /// Opens an arena region around an eligible loop body, returning whether
    /// it was regioned. The `for`-loop fast paths (`lower_for_range`,
    /// `_array`, `_vec`, `_enumerate`) call this where `lower_while` inlines
    /// the same logic, so idiomatic `for x in 0..n { build; consume }` gets
    /// the same iteration-scoped bulk-free as the `while` form. Pair with
    /// `end_auto_region` on the body's fall-through. Eligibility rejects any
    /// `break` / `continue` / `return`, so the only body exit is that
    /// fall-through, where the pop is emitted.
    pub(crate) fn begin_loop_region(&mut self, body: &HirExpr, span: Span) -> bool {
        use crate::lower::helpers::{LoopEligibility, RegionDecision};
        let decision = LoopEligibility::new(&*self.tcx, self.region_unsafe).decide(body);
        if std::env::var_os("GOS_ARENA_TRACE").is_some() {
            let b = body.span;
            match decision {
                RegionDecision::Region => eprintln!(
                    "[arena] file {} bytes {}..{}: auto-regioned (iteration heap bulk-freed)",
                    b.file.as_u32(),
                    b.start,
                    b.end
                ),
                RegionDecision::Reject(r) => eprintln!(
                    "[arena] file {} bytes {}..{}: NOT regioned - allocates each iteration on the slow per-node RC path: {}. Wrap the body in `arena {{ }}` to bulk-free it.",
                    b.file.as_u32(),
                    b.start,
                    b.end,
                    r.reason()
                ),
                RegionDecision::NoAlloc => {}
            }
        }
        let regioned = matches!(decision, RegionDecision::Region);
        if regioned {
            self.emit_region_call("gos_rt_arena_push", span);
            self.region_depth += 1;
            self.deferred_auto_region_collections.push(false);
        }
        regioned
    }

    /// Opens an arena region around a lifted closure's whole body, returning
    /// whether it was regioned. A sequence combinator invokes such a body once
    /// per element, so its allocations have the same lifetime as a loop body's
    /// and `for x in xs { .. }` and `xs.map(|x| ..)` get the same bulk-free.
    /// Eligibility rejects a non-Copy tail, so the value handed back to the
    /// caller can never point into the popped region. Pair with
    /// `end_auto_region` on the body's fall-through.
    pub(crate) fn begin_closure_body_region(
        &mut self,
        block: &gossamer_hir::HirBlock,
        span: Span,
    ) -> bool {
        use crate::lower::helpers::{LoopEligibility, RegionDecision};
        let decision =
            LoopEligibility::new(&*self.tcx, self.region_unsafe).decide_lexical_block(block);
        if std::env::var_os("GOS_ARENA_TRACE").is_some() {
            match decision {
                RegionDecision::Region => eprintln!(
                    "[arena] file {} bytes {}..{}: closure body auto-regioned (per-call heap bulk-freed)",
                    span.file.as_u32(),
                    span.start,
                    span.end
                ),
                RegionDecision::Reject(r) => eprintln!(
                    "[arena] file {} bytes {}..{}: closure body NOT regioned - allocates on each call on the slow per-node RC path: {}. Wrap the body in `arena {{ }}` to bulk-free it.",
                    span.file.as_u32(),
                    span.start,
                    span.end,
                    r.reason()
                ),
                RegionDecision::NoAlloc => {}
            }
        }
        let regioned = matches!(decision, RegionDecision::Region);
        if regioned {
            self.emit_region_call("gos_rt_arena_push", span);
            self.region_depth += 1;
            self.deferred_auto_region_collections.push(false);
        }
        regioned
    }

    /// Closes a region opened by `begin_loop_region` or
    /// `begin_closure_body_region`, emitting `arena_pop` on the fall-through
    /// block before the loop's back-edge or the body's return.
    pub(crate) fn end_auto_region(&mut self, regioned: bool, span: Span) {
        if regioned {
            self.region_depth = self.region_depth.saturating_sub(1);
            let collect_after_pop = self
                .deferred_auto_region_collections
                .pop()
                .expect("automatic region collection stack underflow");
            if self.current.is_some() {
                self.emit_region_call("gos_rt_arena_pop", span);
                if collect_after_pop {
                    self.emit_region_call("gos_rt_collect_cycles", span);
                }
            }
        }
    }

    pub(crate) fn lower_loop(&mut self, body: &HirExpr, ty: Ty, span: Span) -> Option<Local> {
        let label = self.pending_loop_label.take();
        if let Some(for_loop) = detect_for_loop(body) {
            // Re-arm the pending label so the for-loop fast path's
            // `LoopContext` (pushed deep inside a `lower_for_*` helper)
            // inherits it; clear it again if no fast path fires.
            self.pending_loop_label.clone_from(&label);
            if let Some(result) = self.try_lower_for_loop(&for_loop, span) {
                return Some(result);
            }
            // A user-defined iterator is executed correctly by the register
            // VM's generic `next()` protocol, but the MIR generic-loop path
            // cannot yet represent that protocol. Leave an explicit marker
            // in the body so Cranelift admission keeps the caller on
            // bytecode instead of promoting the empty MIR back-edge into an
            // infinite native loop.
            if self.for_loop_uses_user_adt(&for_loop) {
                let unit_ty = self.tcx.unit();
                let marker = self.fresh(unit_ty);
                self.emit_assign(
                    Place::local(marker),
                    Rvalue::CallIntrinsic {
                        name: "gos_jit_unsupported_user_iterator",
                        args: Vec::new(),
                    },
                    span,
                );
            }
            self.pending_loop_label = None;
        }
        let header = self.new_block(span);
        let exit = self.new_block(span);
        // Pre-allocate a result local so `break <expr>` has somewhere
        // to write its payload. `loop` is an expression in Gossamer
        // (`let x = loop { ... break v }`), and the typechecker pins
        // `ty` to the unified break-payload type.
        let result_local = self.fresh(ty);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        // Auto-region the body. Eligibility rejects any `break` / `continue` /
        // `return`, so a terminating `loop { ... break }` is never regioned;
        // the gate fires only for a break-free allocating body, whose sole exit
        // is the fall-through to the back-edge where the pop is emitted.
        let regioned = self.begin_loop_region(body, span);
        self.loop_stack.push(LoopContext {
            continue_to: header,
            break_to: exit,
            result: Some(result_local),
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label,
        });
        let _ = self.lower_expr(body);
        let ctx = self.loop_stack.pop().expect("loop stack underflow");
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        if ctx.break_used {
            Some(result_local)
        } else {
            None
        }
    }

    /// Whether an iterable's type is a generic parameter, whose concrete
    /// shape is only known once the call site is monomorphised.
    fn iter_ty_is_generic_param(&mut self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        for _ in 0..8 {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Param { .. } => return true,
                _ => return false,
            }
        }
        false
    }

    fn for_loop_uses_user_adt(&mut self, for_loop: &ForLoopShape<'_>) -> bool {
        use gossamer_types::TyKind;

        let probe_expr = match &for_loop.iter_expr.kind {
            HirExprKind::Unary {
                op: gossamer_hir::HirUnaryOp::RefShared | gossamer_hir::HirUnaryOp::RefMut,
                operand,
                ..
            } => operand.as_ref(),
            _ => for_loop.iter_expr,
        };
        let is_user_adt = |mut ty| {
            for _ in 0..8 {
                match self.tcx.kind_of(ty) {
                    TyKind::Ref { inner, .. } => ty = *inner,
                    TyKind::Adt { def, .. } => return def.local < u32::MAX - 64,
                    // A type parameter's shape varies per instantiation, so
                    // it walks through `.next()`, which its iteration bound
                    // guarantees, rather than an indexed sequence read.
                    TyKind::Param { .. } => return true,
                    _ => return false,
                }
            }
            false
        };
        if is_user_adt(for_loop.iter_expr.ty) {
            return true;
        }
        if let HirExprKind::Path { segments, .. } = &probe_expr.kind
            && let Some(first) = segments.first()
            && let Some(local) = self.lookup_local(&first.name)
        {
            return is_user_adt(self.locals[local.0 as usize].ty);
        }
        false
    }

    /// Best-effort element type for a `for x in <iter>` iterable whose HIR
    /// type the checker left unresolved (stdlib call results are often a
    /// `Var`). Walks the common Vec-producing shapes - a concrete
    /// Vec/Slice/Array type, the String split/lines family, `chars`, and the
    /// element-preserving `rev`/`to_vec`/`clone` adapters (recursing into
    /// their receiver) - so the loop binds the right element type across every
    /// tier instead of defaulting to i64 and iterating heap pointers.
    /// Whether `expr` denotes an `errors::Error` value. The checker leaves
    /// stdlib method results unresolved, so a bound receiver's MIR local
    /// type is the reliable source.
    pub(crate) fn receiver_is_error(&mut self, expr: &HirExpr) -> bool {
        use gossamer_types::TyKind;
        let ty = self
            .receiver_local_from_path(expr)
            .map_or(expr.ty, |l| self.locals[l.0 as usize].ty);
        matches!(self.tcx.kind_of(self.peel_ref_ty(ty)), TyKind::DynError)
    }

    pub(crate) fn for_loop_elem_ty(&mut self, iter: &HirExpr) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut ty = self
            .receiver_local_from_path(iter)
            .map_or(iter.ty, |l| self.locals[l.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                return Some(*elem);
            }
            _ => {}
        }
        // A free stdlib call's own expression type is often an
        // unresolved `Var`, while the lowering pins a real return type
        // on the call. Ask the lowering what it will produce, so
        // `for line in sort::sort_stable(xs)` binds a `String` rather
        // than the word its slot happens to hold.
        if let HirExprKind::Call { callee, args } = &iter.kind
            && let HirExprKind::Path { segments, .. } = &callee.kind
        {
            let joined = Self::stdlib_free_path(segments);
            if let Some((_, ret_ty)) = self.resolve_stdlib_free_call(&joined, args) {
                let mut ty = ret_ty;
                while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
                    ty = *inner;
                }
                if let TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } =
                    self.tcx.kind_of(ty)
                {
                    return Some(*elem);
                }
            }
            return None;
        }
        if let HirExprKind::MethodCall { name, receiver, .. } = &iter.kind {
            return match name.name.as_str() {
                "split" | "splitn" | "split_whitespace" | "lines" => Some(self.tcx.string_ty()),
                "chars" => Some(self.tcx.intern(TyKind::Char)),
                "bytes" | "as_bytes" => Some(self.tcx.int_ty(gossamer_types::IntTy::U8)),
                // `err.fields()` yields the error's structured (key, value)
                // string pairs; `err.chain()` yields error values.
                "fields" if self.receiver_is_error(receiver) => {
                    let text = self.tcx.string_ty();
                    Some(self.tcx.intern(TyKind::Tuple(vec![text, text])))
                }
                "chain" if self.receiver_is_error(receiver) => Some(self.tcx.dyn_error_ty()),
                // `set.to_vec()` / `set.iter()` snapshot a `HashSet<T>` whose
                // handle type is erased to `i64`; recover the element type
                // from the receiver's HIR generic so the loop binds it.
                "to_vec" | "iter"
                    if matches!(
                        self.runtime_kind_from_ty(receiver.ty),
                        Some("collections::HashSet" | "collections::BTreeSet")
                    ) =>
                {
                    Some(match self.set_elem_kind_of(receiver) {
                        MapKeyKind::I64 => self.tcx.int_ty(gossamer_types::IntTy::I64),
                        _ => self.tcx.string_ty(),
                    })
                }
                "rev" | "to_vec" | "clone" => self.for_loop_elem_ty(receiver),
                _ => None,
            };
        }
        None
    }

    pub(crate) fn try_lower_for_loop(
        &mut self,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        // The local-type recovery probes below key off a bare
        // `Path` iter expression. A `for c in &xs` loop lowers the
        // iterand as `Unary { RefShared, Path("xs") }`, so peel a
        // leading `&` / `&mut` wrapper first - otherwise the probe
        // misses the binding, the element type defaults to i64, and a
        // `[String]` iterates as raw heap pointers (atlas_db's schema
        // corruption: `orders.<ptr>` instead of `orders.id`).
        let probe_expr = match &for_loop.iter_expr.kind {
            HirExprKind::Unary {
                op: gossamer_hir::HirUnaryOp::RefShared | gossamer_hir::HirUnaryOp::RefMut,
                operand,
                ..
            } => operand.as_ref(),
            _ => for_loop.iter_expr,
        };
        // A generic parameter's shape is fixed per instantiation, so none of
        // the sequence fast paths below can describe it. It walks through
        // the `next` protocol, which its iteration bound guarantees.
        if self.iter_ty_is_generic_param(for_loop.iter_expr.ty)
            || self.iter_ty_is_generic_param(probe_expr.ty)
        {
            return None;
        }
        // `for x in s` over a `HashSet` value (a bare binding, or a
        // set-returning method like `a.union(&b)`). The set is a sentinel
        // `Adt`, so it would otherwise hit the Adt bail-out below and lower
        // to the iterator `next` protocol the set does not implement (0
        // iterations on the VM, an undefined `@next` on the compiled tier).
        // Snapshot it to a sorted `Vec<T>` - the same order `set.iter()` /
        // `set.to_vec()` yield - and iterate that.
        if matches!(
            self.runtime_kind_from_ty(probe_expr.ty),
            Some("collections::HashSet" | "collections::BTreeSet")
        ) || (matches!(&probe_expr.kind, HirExprKind::Path { .. })
            && self
                .receiver_local_from_path(probe_expr)
                .and_then(|l| self.local_runtime_kind.get(&l).copied())
                .is_some_and(|rk| matches!(rk, "collections::HashSet" | "collections::BTreeSet")))
        {
            let is_i64 = matches!(self.set_elem_kind_of(probe_expr), MapKeyKind::I64);
            // A struct / tuple / array element is content-keyed, and the
            // snapshot rebuilds each one from its slot descriptor. Walking it
            // as text would hand the body a pointer where it reads a field.
            let aggregate_elem = self
                .first_generic_of(probe_expr.ty)
                .map(|elem| self.peel_ref_ty(elem))
                .filter(|elem| self.is_aggregate_key(*elem))
                .and_then(|elem| self.key_descriptor(elem).map(|desc| (elem, desc)));
            let (elem_ty, sym, descriptor) = match (is_i64, aggregate_elem) {
                (true, _) => (
                    self.tcx.int_ty(gossamer_types::IntTy::I64),
                    "gos_rt_set_to_vec_i64",
                    None,
                ),
                (false, Some((elem, desc))) => (elem, "gos_rt_set_to_vec_skey", Some(desc)),
                (false, None) => (self.tcx.string_ty(), "gos_rt_set_to_vec", None),
            };
            return self.lower_for_set(
                probe_expr,
                elem_ty,
                sym,
                descriptor,
                for_loop.loop_pat,
                for_loop.body,
                span,
            );
        }
        // `for (k, v) in m` over a `HashMap` / typed `BTreeMap` should match
        // `for (k, v) in m.iter()`. Recognise it before the user-ADT escape
        // hatch below, because some stdlib collection spellings enter HIR as
        // named ADTs while their MIR/runtime representation is map-shaped.
        if let Some(local) = self.try_lower_for_bare_hashmap_iter(probe_expr, for_loop, span) {
            return Some(local);
        }
        // If the iter expression is a user-defined struct (Adt),
        // the fast-paths below would all misfire and the default
        // fallback would treat it as a runtime Vec (reading `len`
        // off the wrong offset and silently running zero
        // iterations). Bail out so the generic `loop` lowering
        // drives the HIR `match iter.next() { Some => ..., None
        // => break }` desugar against the user's `impl` method.
        let mut iter_ty_probe = for_loop.iter_expr.ty;
        for _ in 0..8 {
            match self.tcx.kind_of(iter_ty_probe) {
                TyKind::Ref { inner, .. } => iter_ty_probe = *inner,
                TyKind::Adt { .. } => return None,
                _ => break,
            }
        }
        // Same probe through the local table - the HIR type for a
        // `Path("c")` iter expression often arrives as `Var(_)`
        // even though the local was pinned to the struct on its
        // `let` statement; the local's MIR-side type is the
        // authoritative source.
        if let HirExprKind::Path { segments, .. } = &probe_expr.kind {
            if let Some(first) = segments.first() {
                if let Some(local) = self.lookup_local(&first.name) {
                    let mut local_ty = self.locals[local.0 as usize].ty;
                    for _ in 0..8 {
                        match self.tcx.kind_of(local_ty) {
                            TyKind::Ref { inner, .. } => local_ty = *inner,
                            TyKind::Adt { .. } => return None,
                            _ => break,
                        }
                    }
                }
            }
        }
        // `for (k, v) in m.iter()` on a HashMap. Snapshot the keys
        // into a fresh `GosVec`, iterate it, and inside each iteration
        // synthesise `v = m.get_or(k, default)` so the tuple pattern
        // bindings see real values.
        if let Some(local) = self.try_lower_for_hashmap_iter(for_loop, span) {
            return Some(local);
        }
        // `for (idx, x) in v.iter().enumerate()` / `for (idx, x) in
        // v.enumerate()`. Strip `.enumerate()` (and a wrapping
        // `.iter()` if present), then drive the standard array /
        // vec loop while binding `idx` to the per-iteration counter
        // and `x` to the element. Without this, the for-loop falls
        // through to the array/vec dispatch with a 2-tuple loop_pat
        // and the body never runs (no fields on a scalar element).
        // The index walk needs a buffer with a readable length, so a lazy
        // source (a range, or an adapter chain over one) keeps its pairs
        // and goes through the iterator snapshot below instead.
        if let Some(inner) = enumerate_inner_expr(for_loop.iter_expr)
            && !matches!(self.tcx.kind_of(inner.ty), TyKind::Iterator(_))
        {
            if let HirPatKind::Tuple(sub_pats) = &for_loop.loop_pat.kind {
                if sub_pats.len() == 2 {
                    return self.lower_for_enumerate(
                        inner,
                        &sub_pats[0],
                        &sub_pats[1],
                        for_loop.body,
                        span,
                    );
                }
            }
        }
        // `for entry in v.iter()` / `for entry in v` where v is a
        // `json::Value` array - synthesise the loop with
        // `gos_rt_json_len` + `gos_rt_json_at`.
        let iter_target = match &for_loop.iter_expr.kind {
            HirExprKind::MethodCall { receiver, name, .. } if name.name == "iter" => {
                Some(receiver.as_ref())
            }
            _ => None,
        };
        let json_iter = iter_target.filter(|recv| {
            let recv_ty = self
                .receiver_local_from_path(recv)
                .map_or(recv.ty, |local| self.locals[local.0 as usize].ty);
            self.is_json_value_ty(recv_ty)
        });
        if let Some(recv) = json_iter {
            return self.lower_for_json(recv, for_loop.loop_pat, for_loop.body, span);
        }
        if self.is_json_value_ty(for_loop.iter_expr.ty) {
            return self.lower_for_json(for_loop.iter_expr, for_loop.loop_pat, for_loop.body, span);
        }
        // `for byte in s.as_bytes()` / `for byte in s.as_bytes().iter()`
        // - `s` is a String. The Vec fallback below would call
        // `gos_rt_vec_len`/`gos_rt_vec_get_ptr` on a `*const c_char`
        // and segfault on the read of the (non-existent) Vec
        // header. Detect the shape and emit a strlen-bound counter
        // loop reading bytes via `gos_rt_str_byte_at`.
        if let Some(string_expr) = self.detect_string_bytes_iter(for_loop.iter_expr) {
            return self.lower_for_string_bytes(
                string_expr,
                for_loop.loop_pat,
                for_loop.body,
                span,
            );
        }
        if let Some((tuple_expr, elems)) = self.detect_tuple_iter(for_loop.iter_expr) {
            return self.lower_for_tuple(
                tuple_expr,
                &elems,
                for_loop.loop_pat,
                for_loop.body,
                span,
            );
        }
        // A lazy iterator stored in a binding is represented by a runtime
        // iterator handle, not a GosVec. Materialise that single-pass state
        // before entering the existing Vec loop. Sending the handle directly
        // to `gos_rt_vec_len` reads an iterator object as a Vec header and can
        // either skip the loop or hard-fault.
        let mut stored_iter_ty = self
            .receiver_local_from_path(probe_expr)
            .map_or(probe_expr.ty, |local| self.locals[local.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(stored_iter_ty) {
            stored_iter_ty = *inner;
        }
        // `c.iter()` over a sequence is the sequence's own walk, which the
        // fallback below takes with nothing materialised. Every other
        // receiver's `iter()` answers a real iterator state, and that is what
        // the protocol below drives - reading one as a `GosVec` would take
        // its first words for a header.
        let collection_iter_method = match &for_loop.iter_expr.kind {
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if name.name == "iter" && args.is_empty() => {
                let mut recv_ty = self
                    .receiver_local_from_path(receiver)
                    .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
                while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
                    recv_ty = *inner;
                }
                // A map's `iter()` answers a real iterator state over its
                // pairs; every other collection's is the collection's own
                // walk, which the fallback below takes with nothing
                // materialised.
                !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. })
            }
            _ => false,
        };
        if let TyKind::Iterator(elem_ty) = self.tcx.kind_of(stored_iter_ty).clone()
            && !matches!(for_loop.iter_expr.kind, HirExprKind::Range { .. })
            && !collection_iter_method
        {
            // Lazy iterator state carries its own position and yields one
            // element per pull, so a bound cursor is advanced through the
            // generic loop's `next()` protocol rather than read out in full.
            // The bound shape is a local (or `&mut` of one), whose
            // re-evaluation in the match scrutinee is a register read.
            let bound_state = match &for_loop.iter_expr.kind {
                HirExprKind::Path { .. } => true,
                HirExprKind::Unary {
                    op: gossamer_hir::HirUnaryOp::RefShared | gossamer_hir::HirUnaryOp::RefMut,
                    operand,
                    ..
                } => matches!(operand.kind, HirExprKind::Path { .. }),
                _ => false,
            };
            let drivable = matches!(
                self.lazy_iter_elem_family(elem_ty),
                Some(
                    crate::lower::builder::method_call::LazyElemFamily::Word
                        | crate::lower::builder::method_call::LazyElemFamily::Ptr
                        | crate::lower::builder::method_call::LazyElemFamily::Float
                )
            );
            if bound_state && drivable {
                return None;
            }
            // A pair element has no `next` shim to advance through, so its
            // elements are read out once and walked in the buffer that holds
            // them. The collect shim takes the state handle, so a `&mut`
            // wrapper is peeled to the local that holds it.
            let handle_expr = match &for_loop.iter_expr.kind {
                HirExprKind::Unary {
                    op: gossamer_hir::HirUnaryOp::RefShared | gossamer_hir::HirUnaryOp::RefMut,
                    operand,
                    ..
                } => operand.as_ref(),
                _ => for_loop.iter_expr,
            };
            let iter_local = self.lower_expr(handle_expr)?;
            let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
            // An address-carrying stream is collected through the helper that
            // copies each element out whole; the word form would keep the
            // addresses themselves as the elements.
            let helper = self.lazy_collect_symbol_for(iter_local, elem_ty);
            let vec_local = self.emit_combinator_call(
                helper,
                vec![Operand::Copy(Place::local(iter_local))],
                vec_ty,
                span,
            );
            return self.lower_for_vec_over_local(
                vec_local,
                elem_ty,
                for_loop.loop_pat,
                for_loop.body,
                span,
                false,
            );
        }
        match &for_loop.iter_expr.kind {
            HirExprKind::Range {
                start: Some(start),
                end,
                inclusive,
            } => self.lower_for_range(
                start,
                end.as_deref(),
                *inclusive,
                for_loop.loop_pat,
                for_loop.body,
                span,
            ),
            HirExprKind::Array(arr) => {
                let len_opt = match arr {
                    gossamer_hir::HirArrayExpr::List(elems) => Some(elems.len() as i64),
                    gossamer_hir::HirArrayExpr::Repeat { count, .. } => self
                        .static_repeat_len(for_loop.iter_expr.ty, count)
                        .and_then(|c| i64::try_from(c).ok()),
                };
                if let Some(len) = len_opt {
                    return self.lower_for_array(
                        for_loop.iter_expr,
                        for_loop.loop_pat,
                        for_loop.body,
                        len,
                        span,
                    );
                }
                // Runtime-sized [val; n]: type is Vec<elem> after typechecking.
                let elem_ty = match self.tcx.kind_of(for_loop.iter_expr.ty) {
                    TyKind::Vec(e) | TyKind::Slice(e) => *e,
                    _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
                };
                self.lower_for_vec(
                    for_loop.iter_expr,
                    elem_ty,
                    for_loop.loop_pat,
                    for_loop.body,
                    span,
                )
            }
            _ => {
                // Fallback chain:
                //   1. fixed-size `[T; N]` (`&[T; N]`) → array iter.
                //   2. runtime `Vec<T>` (or peeled-through-`&`)
                //      → `gos_rt_vec_*` dynamic-length iter.
                //   3. give up (the for-loop fallback re-emits
                //      the original `loop {}` shape).
                let mut cur = for_loop.iter_expr.ty;
                // Also peel `.iter()` method calls - `for x in v.iter()`
                // and `for x in &v` both end up wanting Vec iteration.
                let iter_recv = match &for_loop.iter_expr.kind {
                    // A `HashSet` / `HashMap` receiver must NOT be peeled: its
                    // handle is not a `GosVec`, and a map yields `(K, V)` PAIRS
                    // rather than its receiver's element type. Falling through
                    // lets `s.iter()` / `m.iter()` lower to the runtime
                    // snapshot Vec (`gos_rt_set_to_vec` / the map-iter pair
                    // vec) with the right element type from `iter_expr.ty`.
                    HirExprKind::MethodCall { receiver, name, .. }
                        if name.name == "iter" && !self.receiver_is_map_or_set(receiver) =>
                    {
                        Some(receiver.as_ref())
                    }
                    _ => None,
                };
                if let Some(recv) = iter_recv {
                    let recv_ty = self
                        .receiver_local_from_path(recv)
                        .map_or(recv.ty, |local| self.locals[local.0 as usize].ty);
                    // Also try the receiver's HIR-expression kind:
                    // `[..].iter()` has receiver = Array(...) whose
                    // ty may be unresolved on the MIR side; the AST
                    // shape gives us the literal length directly.
                    if let HirExprKind::Array(arr) = &recv.kind {
                        let len = match arr {
                            gossamer_hir::HirArrayExpr::List(elems) => Some(elems.len() as i64),
                            gossamer_hir::HirArrayExpr::Repeat { count, .. } => self
                                .static_repeat_len(recv.ty, count)
                                .and_then(|c| i64::try_from(c).ok()),
                        };
                        if let Some(len) = len {
                            return self.lower_for_array(
                                recv,
                                for_loop.loop_pat,
                                for_loop.body,
                                len,
                                span,
                            );
                        }
                    }
                    let mut peeled = recv_ty;
                    let mut found_elem: Option<Ty> = None;
                    let mut found_len: Option<i64> = None;
                    loop {
                        match self.tcx.kind_of(peeled) {
                            TyKind::Vec(elem) | TyKind::Slice(elem) => {
                                found_elem = Some(*elem);
                                break;
                            }
                            TyKind::Array { len, elem } => {
                                if let Ok(l) = i64::try_from(len.to_usize()) {
                                    found_len = Some(l);
                                    found_elem = Some(*elem);
                                }
                                break;
                            }
                            TyKind::Ref { inner, .. } => peeled = *inner,
                            _ => break,
                        }
                    }
                    if let Some(len) = found_len {
                        return self.lower_for_array(
                            recv,
                            for_loop.loop_pat,
                            for_loop.body,
                            len,
                            span,
                        );
                    }
                    // For `.iter()` on a receiver whose MIR type
                    // didn't resolve to a Vec/Slice/Array (often
                    // because the receiver is a field projection
                    // through a Var-typed parent), default to
                    // runtime-Vec iteration. The receiver value
                    // is whatever `gos_rt_arr_iter` returns on it
                    // (identity for slices/vecs); the loop reads
                    // each element via `gos_rt_vec_get_ptr` +
                    // `gos_load`. Element type defaults to i64.
                    let elem_ty =
                        found_elem.unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
                    return self.lower_for_vec(
                        recv,
                        elem_ty,
                        for_loop.loop_pat,
                        for_loop.body,
                        span,
                    );
                }
                // If the iter expression is a Path bound to a
                // local, prefer the local's MIR-pinned type to
                // the HIR expression type - the typechecker
                // often leaves stdlib-call results as Var, but
                // the MIR side may have pinned them to a
                // concrete `Vec<T>` via a runtime-helper return
                // type pin.
                if let HirExprKind::Path { segments, .. } = &probe_expr.kind {
                    if let Some(first) = segments.first() {
                        if let Some(local) = self.lookup_local(&first.name) {
                            cur = self.locals[local.0 as usize].ty;
                        }
                    }
                }
                // Check whether the HIR-expression type or the
                // chained method-call return type pins the iter
                // expression to a `Vec<T>`. The MIR-side dispatch
                // for `s.split(...)`, `.lines()`, `.iter()`, etc.
                // already pins their return to `Vec<String>` via
                // `pinned_ret`, so by the time we get here we can
                // look at the call target name + walk the
                // dispatch table to reach the Vec(elem) shape.
                let mut for_vec_elem: Option<Ty> = None;
                // `m.keys()` / `m.values()` carry a `Vec<K>` / `Vec<V>` type, but
                // their runtime snapshot stores struct values as boxed pointers.
                // The map-specific block below binds those as a box-pointer `Ref`
                // so field access derefs the box; the generic `Vec(elem)`
                // extraction here would instead bind the bare struct and read the
                // pointer bits inline. Leave those two to the specialized path.
                let is_map_keys_values = matches!(
                    &for_loop.iter_expr.kind,
                    HirExprKind::MethodCall { name, .. }
                        if matches!(name.name.as_str(), "keys" | "values")
                );
                if !is_map_keys_values && let TyKind::Vec(elem) = self.tcx.kind_of(cur) {
                    for_vec_elem = Some(*elem);
                }
                if for_vec_elem.is_none() {
                    if let HirExprKind::MethodCall { name, .. } = &for_loop.iter_expr.kind {
                        if matches!(
                            name.name.as_str(),
                            "split" | "splitn" | "split_whitespace" | "lines"
                        ) {
                            for_vec_elem = Some(self.tcx.string_ty());
                        } else if name.name.as_str() == "chars" {
                            for_vec_elem = Some(self.tcx.intern(gossamer_types::TyKind::Char));
                        }
                    }
                }
                // Element-preserving Vec adapters (`xs.rev()`,
                // `xs.to_vec()`, `xs.clone()`) consumed directly as the
                // iterable carry a `Var` HIR type; recover the element type
                // from the adapter's receiver so the loop binds it correctly.
                if for_vec_elem.is_none() {
                    if let HirExprKind::MethodCall { name, receiver, .. } = &for_loop.iter_expr.kind
                    {
                        // `set.to_vec()` / `set.iter()` snapshot a `HashSet<T>`
                        // whose handle type is erased to `i64`; recover the
                        // element kind from the receiver's HIR generic.
                        if matches!(name.name.as_str(), "to_vec" | "iter")
                            && matches!(
                                self.runtime_kind_from_ty(receiver.ty),
                                Some("collections::HashSet" | "collections::BTreeSet")
                            )
                        {
                            for_vec_elem = Some(match self.set_elem_kind_of(receiver) {
                                MapKeyKind::I64 => self.tcx.int_ty(gossamer_types::IntTy::I64),
                                // An aggregate element is snapshotted as its
                                // own inline slots, so the binding takes the
                                // element type itself - reading it as one word
                                // would address a fraction of the element.
                                _ => self
                                    .first_generic_of(receiver.ty)
                                    .map(|elem| self.peel_ref_ty(elem))
                                    .filter(|elem| self.is_aggregate_key(*elem))
                                    .unwrap_or_else(|| self.tcx.string_ty()),
                            });
                        } else if matches!(name.name.as_str(), "rev" | "to_vec" | "clone") {
                            for_vec_elem = self.for_loop_elem_ty(receiver);
                        }
                    }
                }
                if for_vec_elem.is_none() {
                    if let HirExprKind::MethodCall { name, receiver, .. } = &for_loop.iter_expr.kind
                    {
                        if matches!(name.name.as_str(), "keys" | "values") {
                            let recv_ty = self
                                .receiver_local_from_path(receiver)
                                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
                            if matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. })
                                || matches!(self.tcx.kind_of(recv_ty), TyKind::Ref { .. })
                                    && self.hash_map_key_kind(recv_ty).is_some()
                            {
                                let elem = if name.name.as_str() == "keys" {
                                    match self.hash_map_key_kind(recv_ty) {
                                        Some(MapKeyKind::String) => self.tcx.string_ty(),
                                        // An aggregate key snapshot stores each
                                        // key as inline flat slots, so the
                                        // binding takes the key type itself -
                                        // reading it as a scalar word would
                                        // address a fraction of the element.
                                        Some(MapKeyKind::Other) => {
                                            self.hash_map_kv_tys(recv_ty).map_or_else(
                                                || self.tcx.int_ty(gossamer_types::IntTy::I64),
                                                |(k, _)| k,
                                            )
                                        }
                                        _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
                                    }
                                } else {
                                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                                    match self.hash_map_value_kind(recv_ty) {
                                        Some(MapValueKind::String) => self.tcx.string_ty(),
                                        // A struct value is stored as a boxed
                                        // pointer; bind `v` as a reference (a
                                        // single box-pointer word) so field
                                        // access derefs the box rather than
                                        // reading the pointer bits inline.
                                        Some(MapValueKind::Other) => {
                                            let value_struct = self
                                                .hash_map_kv_tys(recv_ty)
                                                .map(|(_, v)| v)
                                                .filter(|v| self.struct_name_of(*v).is_some());
                                            match value_struct {
                                                Some(v) => self.tcx.intern(TyKind::Ref {
                                                    mutability: gossamer_types::Mutbl::Not,
                                                    inner: v,
                                                }),
                                                None => i64_ty,
                                            }
                                        }
                                        _ => i64_ty,
                                    }
                                };
                                for_vec_elem = Some(elem);
                            }
                        }
                    }
                }
                if for_vec_elem.is_none() {
                    if let HirExprKind::Call { callee, args } = &for_loop.iter_expr.kind {
                        if let HirExprKind::Path { segments, .. } = &callee.kind {
                            let names: Vec<&str> =
                                segments.iter().map(|s| s.name.as_str()).collect();
                            if let Some((_, _, item)) =
                                self.resolve_external_binding(&names, args.len())
                            {
                                if let gossamer_resolve::BindingType::Vec(inner) = &item.ret {
                                    for_vec_elem = Some(self.binding_type_to_mir(inner));
                                }
                            }
                        }
                    }
                }
                if for_vec_elem.is_none() {
                    if let HirExprKind::Call { callee, .. } = &for_loop.iter_expr.kind {
                        if let HirExprKind::Path { segments, .. } = &callee.kind {
                            let joined = segments
                                .iter()
                                .map(|s| s.name.as_str())
                                .collect::<Vec<_>>()
                                .join("::");
                            // `regex::find_all` returns `Vec<(i64,
                            // i64, String)>` per the public API.
                            // The runtime stores 24-byte tuples, so
                            // pin the element type to a 3-tuple here
                            // - without this the loop body treats
                            // each element as a single i64/String,
                            // crashing on `hit.2` past the end of
                            // a wrongly-strided buffer.
                            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                            let str_ty = self.tcx.string_ty();
                            let tup_ty = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![
                                i64_ty, i64_ty, str_ty,
                            ]));
                            // `captures_all` returns
                            // `Vec<Vec<Option<String>>>`; iterating it
                            // gives a `Vec<Option<String>>` per match.
                            // The element must be `Option<String>` (not
                            // a bare `String`) so `row[i]` indexing types
                            // as the tagged-union and `match row[i] {
                            // Some(k) => …, None => … }` reads the real
                            // discriminant rather than the happy-path
                            // encoding.
                            let opt_str_ty = self.option_string_ty();
                            let opt_str_vec_ty =
                                self.tcx.intern(gossamer_types::TyKind::Vec(opt_str_ty));
                            for_vec_elem = match joined.as_str() {
                                "regex::find_all" | "std::regex::find_all" => Some(tup_ty),
                                "regex::split" | "std::regex::split" => Some(str_ty),
                                "regex::captures_all" | "std::regex::captures_all" => {
                                    Some(opt_str_vec_ty)
                                }
                                _ => None,
                            };
                        }
                    }
                }
                if for_vec_elem.is_none() {
                    // Inline `for … in iter::X(…)`: the typechecker
                    // leaves these stdlib intrinsics' result `Var`, so
                    // pin the element type for the tuple-returning and
                    // nested-vec combinators. Without this the loop
                    // falls to the single-slot i64 shape and either the
                    // tuple destructure has no slot address (panic) or
                    // `w[0]` indexes an i64 as if it were a vec.
                    if let HirExprKind::Call { callee, args } = &for_loop.iter_expr.kind {
                        if let HirExprKind::Path { segments, .. } = &callee.kind {
                            let joined = segments
                                .iter()
                                .map(|s| s.name.as_str())
                                .collect::<Vec<_>>()
                                .join("::");
                            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                            let pair_ty = self
                                .tcx
                                .intern(gossamer_types::TyKind::Tuple(vec![i64_ty, i64_ty]));
                            // `zip` pairs each side's own element, so the
                            // binding's halves carry the sequences' types
                            // rather than the word slot they share.
                            let zip_pair_ty = match args.as_slice() {
                                [a, b] => {
                                    let (a_elem, _) = self.iter_elem_abi(a.ty);
                                    let (b_elem, _) = self.iter_elem_abi(b.ty);
                                    self.tcx
                                        .intern(gossamer_types::TyKind::Tuple(vec![a_elem, b_elem]))
                                }
                                _ => pair_ty,
                            };
                            let vec_i64 = self.tcx.intern(gossamer_types::TyKind::Vec(i64_ty));
                            for_vec_elem = match joined.as_str() {
                                "iter::zip" | "std::iter::zip" => Some(zip_pair_ty),
                                "iter::enumerate"
                                | "std::iter::enumerate"
                                | "iter::pairwise"
                                | "std::iter::pairwise" => Some(pair_ty),
                                "iter::windows" | "std::iter::windows" | "iter::chunks"
                                | "std::iter::chunks" => Some(vec_i64),
                                "iter::flatten" | "std::iter::flatten" | "iter::dedup"
                                | "std::iter::dedup" => Some(i64_ty),
                                _ => None,
                            };
                        }
                    }
                }
                if for_vec_elem.is_none() {
                    // `for (name, value) in resp.headers` /
                    // `r.headers` on an `http::Response` or
                    // `http::Request` receiver. The field lowers to
                    // `gos_rt_http_response_headers` /
                    // `gos_rt_http_request_headers` (a Vec of 16-byte
                    // `(String, String)` tuple slots), but the HIR type
                    // of the projection is often an unresolved Var, so
                    // pin the element type here - without this the loop
                    // falls back to the single-slot i64 shape and the
                    // tuple destructure has no slot address to read.
                    if let HirExprKind::Field { receiver, name } = &for_loop.iter_expr.kind {
                        if name.name == "headers" {
                            let is_header_carrier = matches!(
                                self.receiver_local_from_path(receiver)
                                    .and_then(|l| self.local_runtime_kind.get(&l).copied()),
                                Some("http::Response" | "http::Request")
                            );
                            if is_header_carrier {
                                let s = self.tcx.string_ty();
                                let tup =
                                    self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                                for_vec_elem = Some(tup);
                            }
                        }
                    }
                }
                // Last resort for a method-call iterand only: the shared
                // best-effort element-type probe. It covers the stdlib method
                // results the checker leaves as an unresolved `Var`, so an
                // inline `for x in recv.m()` binds the same element type a
                // `let`-bound receiver does. A concrete sequence type is
                // already handled above, and a fixed array must keep its
                // length-driven lowering below.
                if for_vec_elem.is_none()
                    && matches!(
                        &for_loop.iter_expr.kind,
                        HirExprKind::MethodCall { .. } | HirExprKind::Call { .. }
                    )
                    && !matches!(self.tcx.kind_of(cur), TyKind::Array { .. })
                {
                    for_vec_elem = self.for_loop_elem_ty(for_loop.iter_expr);
                }
                if let Some(elem) = for_vec_elem {
                    return self.lower_for_vec(
                        for_loop.iter_expr,
                        elem,
                        for_loop.loop_pat,
                        for_loop.body,
                        span,
                    );
                }
                let len_opt = loop {
                    match self.tcx.kind_of(cur) {
                        TyKind::Array { len, .. } => {
                            break i64::try_from(len.to_usize()).ok();
                        }
                        TyKind::Vec(elem) | TyKind::Slice(elem) => {
                            let elem = *elem;
                            return self.lower_for_vec(
                                for_loop.iter_expr,
                                elem,
                                for_loop.loop_pat,
                                for_loop.body,
                                span,
                            );
                        }
                        TyKind::Ref { inner, .. } => cur = *inner,
                        _ => break None,
                    }
                };
                if let Some(len) = len_opt {
                    return self.lower_for_array(
                        for_loop.iter_expr,
                        for_loop.loop_pat,
                        for_loop.body,
                        len,
                        span,
                    );
                }
                // Default fallback: treat as a runtime Vec.
                // Element type defaults to i64, which is the
                // pointer width - every slot in a GosVec is
                // 8 bytes regardless of element shape, so
                // method calls on the binding still dispatch.
                let elem_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                self.lower_for_vec(
                    for_loop.iter_expr,
                    elem_ty,
                    for_loop.loop_pat,
                    for_loop.body,
                    span,
                )
            }
        }
    }
}

/// The scalar value of a range-pattern bound: an integer, a byte, or a
/// character, each of which compares as the number it is. `None` for a bound
/// this lowering has no ordering for.
fn pattern_range_bound(literal: &HirLiteral) -> Option<i128> {
    match literal {
        HirLiteral::Int(text) => parse_int(text),
        HirLiteral::Byte(b) => Some(i128::from(*b)),
        HirLiteral::Char(c) => Some(i128::from(u32::from(*c))),
        _ => None,
    }
}
