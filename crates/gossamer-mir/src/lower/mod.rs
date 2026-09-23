#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! HIR → MIR lowering.
//! Produces a [`Body`] per HIR function. The lowerer is intentionally
//! straightforward: every HIR expression of interest becomes either a
//! sequence of [`StatementKind::Assign`]s targeting fresh temporaries
//! or a [`Terminator`] that closes the current block. Control flow
//! (`if`, `while`, `loop`, `match`) drops into the CFG by allocating
//! join blocks and stitching them with [`Terminator::Goto`] /
//! [`Terminator::SwitchInt`].

#![forbid(unsafe_code)]
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

/// Calling-convention shape of a `r.get(pattern, handler)` handler value
/// at the runtime ABI boundary. Returned by
/// [`builder::Builder::emit_router_handler_abi`] so the caller can pick
/// between the env-carrying and bare entry points.
pub(crate) enum RouterHandlerAbi {
    /// Top-level `fn(http::Request) -> Result<...>` value. Caller routes
    /// to the `_fn` runtime symbol and passes only `fn_addr`.
    Bare(Operand),
    /// Struct or closure value with a `(env, req) -> Result<...>` ABI.
    /// Caller keeps the original runtime symbol and pushes both operands.
    WithEnv {
        /// The handler env operand (struct value pointer or closure env).
        env: Operand,
        /// The handler `fn_addr` operand (`gos_fn_addr("…::serve")`).
        fn_addr: Operand,
    },
}

/// Registers every inline-able user enum (by `DefId.local`) in `tcx` so the
/// codegen renders it as the 2-word by-value `i128` `[disc, payload]` shape
/// instead of a heap node. An enum is inline-able iff every variant has at most
/// one field and that field fits in a single 8-byte slot (scalar / String /
/// Vec / map / ref / fn-pointer / handle). Multi-field variants (e.g. a tree
/// node) keep the heap-node representation.
fn register_inline_enums(program: &HirProgram, tcx: &mut TyCtxt) {
    for item in &program.items {
        let HirItemKind::Adt(adt) = &item.kind else {
            continue;
        };
        let HirAdtKind::Enum(variants) = &adt.kind else {
            continue;
        };
        let inline_able = variants.iter().all(|v| match &v.struct_field_tys {
            None => true,
            Some(tys) => tys.len() <= 1 && tys.iter().all(|t| field_fits_inline(tcx, *t)),
        });
        if !inline_able {
            continue;
        }
        if let gossamer_types::TyKind::Adt { def, .. } = tcx.kind_of(adt.self_ty) {
            let def_local = def.local;
            tcx.register_inline_enum_def(def_local);
        }
    }
}

/// True when a value of `ty` occupies a single 8-byte slot and is never itself
/// an inline (2-word) enum - the safe set for an inline enum payload word.
/// Conservatively excludes `Adt` / `Tuple` / `Array` (which may be multi-slot
/// or themselves inline enums, which would not fit in one payload word).
fn field_fits_inline(tcx: &TyCtxt, ty: gossamer_types::Ty) -> bool {
    use gossamer_types::TyKind;
    matches!(
        tcx.kind_of(ty),
        TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::String
            | TyKind::Slice(_)
            | TyKind::Vec(_)
            | TyKind::HashMap { .. }
            | TyKind::Sender(_)
            | TyKind::Receiver(_)
            | TyKind::JoinHandle(_)
            | TyKind::Ref { .. }
            | TyKind::FnPtr(_)
            | TyKind::JsonValue
            | TyKind::DynError
    )
}

/// Lower an entire HIR program to MIR `Body`s, one per top-level function.
pub fn lower_program(program: &HirProgram, tcx: &mut TyCtxt) -> Vec<Body> {
    crate::opt::intern_row_table_types(tcx);
    let tables = ProgramTables::collect(program, tcx);
    let mut bodies = Vec::new();
    for item in &program.items {
        tables.collect_item(item, tcx, &mut bodies);
    }
    finish_lowered_bodies(&mut bodies, 0, tcx);
    bodies
}

/// Keeps one word per field for every struct whose layout a narrow-field
/// address or a Rust binding depends on.
///
/// A `&mut` to a field hands its callee an address a whole word is written
/// through, and a binding passes a struct one word per field; either would
/// reach past a narrow field of a packed struct into its neighbour. Runs
/// before any body is lowered, so every layout lowering reads is final.
fn keep_word_fields_where_required(
    program: &HirProgram,
    struct_defs: &HashMap<gossamer_resolve::DefId, String>,
    tcx: &mut TyCtxt,
) {
    let mut kept = Vec::new();
    let mut unresolved = false;
    for item in &program.items {
        walk_item_exprs(item, &mut |expr| match narrow_field_borrow(expr, tcx) {
            NarrowBorrow::None => {}
            NarrowBorrow::Of(def) => kept.push(def),
            NarrowBorrow::Unresolved => unresolved = true,
        });
    }
    let mut binding_names = std::collections::HashSet::new();
    for module in gossamer_resolve::all_external_modules() {
        for item in &module.items {
            for ty in item.params.iter().chain(std::iter::once(&item.ret)) {
                push_opaque_binding_names(ty, &mut binding_names);
            }
        }
    }
    kept.extend(
        struct_defs
            .iter()
            .filter(|(_, name)| unresolved || binding_names.contains(name.as_str()))
            .map(|(def, _)| *def),
    );
    for def in kept {
        tcx.keep_word_fields(def);
    }
}

/// What a `&mut <place>.field` borrow says about the struct holding the field.
enum NarrowBorrow {
    /// The expression borrows no narrow field.
    None,
    /// It borrows a narrow field of this struct.
    Of(gossamer_resolve::DefId),
    /// It borrows a field whose owner the checker left unresolved, so no
    /// struct can be ruled out.
    Unresolved,
}

fn narrow_field_borrow(expr: &HirExpr, tcx: &TyCtxt) -> NarrowBorrow {
    use gossamer_types::{IntTy, TyKind};
    let HirExprKind::Unary {
        op: HirUnaryOp::RefMut,
        operand,
    } = &expr.kind
    else {
        return NarrowBorrow::None;
    };
    let receiver = match &operand.kind {
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => receiver,
        _ => return NarrowBorrow::None,
    };
    let narrow = matches!(
        tcx.kind_of(operand.ty),
        TyKind::Bool
            | TyKind::Char
            | TyKind::Int(
                IntTy::I8 | IntTy::U8 | IntTy::I16 | IntTy::U16 | IntTy::I32 | IntTy::U32
            )
            | TyKind::Var(_)
            | TyKind::Error
    );
    if !narrow {
        return NarrowBorrow::None;
    }
    let mut owner = receiver.ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(owner) {
        owner = *inner;
    }
    match tcx.kind_of(owner) {
        TyKind::Adt { def, .. } => NarrowBorrow::Of(*def),
        TyKind::Var(_) | TyKind::Error => NarrowBorrow::Unresolved,
        _ => NarrowBorrow::None,
    }
}

/// Every struct name a binding signature passes by value.
fn push_opaque_binding_names(
    ty: &gossamer_resolve::BindingType,
    out: &mut std::collections::HashSet<String>,
) {
    use gossamer_resolve::BindingType as B;
    match ty {
        B::Opaque(name) => {
            out.insert(name.clone());
        }
        B::Tuple(items) => {
            for item in items {
                push_opaque_binding_names(item, out);
            }
        }
        B::Vec(inner) | B::Option(inner) => push_opaque_binding_names(inner, out),
        B::Result(first, second) | B::Map(first, second) => {
            push_opaque_binding_names(first, out);
            push_opaque_binding_names(second, out);
        }
        B::Variant(arms) => {
            for arm in arms {
                for payload in &arm.payload {
                    push_opaque_binding_names(payload, out);
                }
            }
        }
        B::Callback(args, ret) => {
            for arg in args {
                push_opaque_binding_names(arg, out);
            }
            push_opaque_binding_names(ret, out);
        }
        B::Unit | B::Bool | B::I64 | B::F64 | B::Char | B::String | B::Bytes | B::Any => {}
    }
}

fn walk_item_exprs(item: &HirItem, visit: &mut dyn FnMut(&HirExpr)) {
    match &item.kind {
        HirItemKind::Fn(func) => walk_fn_exprs(func, visit),
        HirItemKind::Const(konst) => walk_exprs(&konst.value, visit),
        HirItemKind::Static(stat) => walk_exprs(&stat.value, visit),
        HirItemKind::Impl(imp) => {
            for method in &imp.methods {
                walk_fn_exprs(method, visit);
            }
        }
        HirItemKind::Trait(tr) => {
            for method in &tr.methods {
                walk_fn_exprs(method, visit);
            }
        }
        HirItemKind::Adt(_) => {}
    }
}

fn walk_fn_exprs(func: &HirFn, visit: &mut dyn FnMut(&HirExpr)) {
    if let Some(body) = &func.body {
        walk_block_exprs(&body.block, visit);
    }
}

fn walk_block_exprs(block: &HirBlock, visit: &mut dyn FnMut(&HirExpr)) {
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let { init, .. } => {
                if let Some(init) = init {
                    walk_exprs(init, visit);
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => walk_exprs(expr, visit),
            HirStmtKind::Item(item) => walk_item_exprs(item, visit),
        }
    }
    if let Some(tail) = &block.tail {
        walk_exprs(tail, visit);
    }
}

/// Hands `expr` and every expression inside it to `visit`. Every kind is
/// named, so a new expression kind does not compile until it is walked.
fn walk_exprs(expr: &HirExpr, visit: &mut dyn FnMut(&HirExpr)) {
    visit(expr);
    match &expr.kind {
        HirExprKind::Literal(_)
        | HirExprKind::Path { .. }
        | HirExprKind::Continue { .. }
        | HirExprKind::Placeholder => {}
        HirExprKind::Call { callee, args } => {
            walk_exprs(callee, visit);
            for arg in args {
                walk_exprs(arg, visit);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            walk_exprs(receiver, visit);
            for arg in args {
                walk_exprs(arg, visit);
            }
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            walk_exprs(receiver, visit);
        }
        HirExprKind::Index { base, index } => {
            walk_exprs(base, visit);
            walk_exprs(index, visit);
        }
        HirExprKind::Unary { operand, .. } => walk_exprs(operand, visit),
        HirExprKind::Binary { lhs, rhs, .. } => {
            walk_exprs(lhs, visit);
            walk_exprs(rhs, visit);
        }
        HirExprKind::Assign { place, value } => {
            walk_exprs(place, visit);
            walk_exprs(value, visit);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_exprs(condition, visit);
            walk_exprs(then_branch, visit);
            if let Some(otherwise) = else_branch {
                walk_exprs(otherwise, visit);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            walk_exprs(scrutinee, visit);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    walk_exprs(guard, visit);
                }
                walk_exprs(&arm.body, visit);
            }
        }
        HirExprKind::Loop { body, .. } | HirExprKind::Closure { body, .. } => {
            walk_exprs(body, visit);
        }
        HirExprKind::While {
            condition, body, ..
        } => {
            walk_exprs(condition, visit);
            walk_exprs(body, visit);
        }
        HirExprKind::Block(block) => walk_block_exprs(block, visit),
        HirExprKind::LiftedClosure { captures, .. } | HirExprKind::Tuple(captures) => {
            for capture in captures {
                walk_exprs(capture, visit);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &arm.op {
                    gossamer_hir::HirSelectOp::Recv { channel, .. } => walk_exprs(channel, visit),
                    gossamer_hir::HirSelectOp::Send { channel, value } => {
                        walk_exprs(channel, visit);
                        walk_exprs(value, visit);
                    }
                    gossamer_hir::HirSelectOp::Default => {}
                }
                walk_exprs(&arm.body, visit);
            }
        }
        HirExprKind::Return(value) | HirExprKind::Break { value, .. } => {
            if let Some(value) = value {
                walk_exprs(value, visit);
            }
        }
        HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) => {
            for elem in elems {
                walk_exprs(elem, visit);
            }
        }
        HirExprKind::Array(gossamer_hir::HirArrayExpr::Repeat { value, count }) => {
            walk_exprs(value, visit);
            walk_exprs(count, visit);
        }
        HirExprKind::Cast { value, .. } => walk_exprs(value, visit),
        HirExprKind::Range { start, end, .. } => {
            if let Some(start) = start {
                walk_exprs(start, visit);
            }
            if let Some(end) = end {
                walk_exprs(end, visit);
            }
        }
    }
}

/// The program-wide tables the lowering of every body reads.
pub(crate) struct ProgramTables {
    structs: HashMap<String, Vec<String>>,
    struct_defs: HashMap<gossamer_resolve::DefId, String>,
    enums: EnumIndex,
    impl_methods: HashMap<String, Option<Ty>>,
    impl_method_receivers: HashMap<String, Ty>,
    impl_method_inputs: HashMap<String, Vec<Ty>>,
    fn_ret_names: HashMap<String, Ty>,
    fn_returns: HashMap<gossamer_resolve::DefId, Ty>,
    fn_inputs: HashMap<gossamer_resolve::DefId, Vec<Ty>>,
    fn_param_shareable:
        HashMap<gossamer_resolve::DefId, Vec<crate::lower::helpers::escape::ParamShare>>,
    consts: HashMap<gossamer_resolve::DefId, ConstValue>,
    mut_statics: HashMap<gossamer_resolve::DefId, crate::ir::StaticRef>,
    const_inits: HashMap<gossamer_resolve::DefId, HirExpr>,
    region_unsafe: std::collections::HashSet<gossamer_resolve::DefId>,
    effect_free_pair_keys: HashMap<String, bool>,
}

impl ProgramTables {
    /// Reads every table off `program`, registering its inline enums in `tcx`.
    pub(crate) fn collect(program: &HirProgram, tcx: &mut TyCtxt) -> Self {
        let (structs, struct_defs) = collect_struct_fields(program);
        keep_word_fields_where_required(program, &struct_defs, tcx);
        let enums = collect_enum_variants(program, tcx);
        register_inline_enums(program, tcx);
        let mut consts = collect_const_values(program);
        // Scalar `static mut` items become real mutable module globals;
        // remove them from the const-inline map so their reads load the
        // live cell instead of freezing at the declaration value.
        let mut_statics = collect_mut_static_defs(program, &consts);
        for def in mut_statics.keys() {
            consts.remove(def);
        }
        Self {
            structs,
            struct_defs,
            enums,
            impl_methods: collect_impl_methods(program),
            impl_method_receivers: collect_impl_method_receivers(program),
            impl_method_inputs: collect_impl_method_inputs(program),
            fn_ret_names: collect_fn_ret_names(program),
            fn_returns: collect_fn_returns(program, tcx),
            fn_inputs: collect_fn_inputs(program),
            consts,
            mut_statics,
            const_inits: collect_const_init_exprs(program),
            // Conservative escape summary driving automatic arena regions.
            region_unsafe: collect_region_unsafe_fns(program, tcx),
            fn_param_shareable: collect_shareable_params(program, tcx),
            effect_free_pair_keys: collect_effect_free_pair_keys(program),
        }
    }

    /// Whether the program declares a method body under `name` (`Type::method`).
    pub(crate) fn declares_method(&self, name: &str) -> bool {
        self.impl_methods.contains_key(name)
    }

    /// Lowers every body `item` declares into `out`.
    pub(crate) fn collect_item(&self, item: &HirItem, tcx: &mut TyCtxt, out: &mut Vec<Body>) {
        collect_item(
            item,
            tcx,
            &self.structs,
            &self.struct_defs,
            &self.enums,
            &self.impl_methods,
            &self.impl_method_receivers,
            &self.impl_method_inputs,
            &self.fn_ret_names,
            &self.fn_returns,
            &self.fn_inputs,
            &self.fn_param_shareable,
            &self.consts,
            &self.mut_statics,
            &self.const_inits,
            &self.region_unsafe,
            &self.effect_free_pair_keys,
            out,
        );
    }

    /// Lowers one function declaration.
    pub(crate) fn lower_fn(
        &self,
        decl: &HirFn,
        def: Option<gossamer_resolve::DefId>,
        span: Span,
        tcx: &mut TyCtxt,
    ) -> Option<Body> {
        lower_fn(
            decl,
            def,
            span,
            tcx,
            &self.structs,
            &self.struct_defs,
            &self.enums,
            &self.impl_methods,
            &self.impl_method_receivers,
            &self.impl_method_inputs,
            &self.fn_ret_names,
            &self.fn_returns,
            &self.fn_inputs,
            &self.fn_param_shareable,
            &self.consts,
            &self.mut_statics,
            &self.const_inits,
            &self.region_unsafe,
            &self.effect_free_pair_keys,
        )
    }
}

/// Runs the ownership, reuse, and canonicalisation passes over
/// `bodies[start..]`. The whole list is read for what each callee declares,
/// so bodies lowered after the first round see the same program the first
/// round did.
pub(crate) fn finish_lowered_bodies(bodies: &mut [Body], start: usize, tcx: &mut TyCtxt) {
    // A pass reading a field of a generic struct instance reads the
    // instance's own field types, not the declaration's parameters.
    crate::monomorph::register_struct_instantiations(&bodies[start..], tcx);
    let user_fn_names: std::collections::HashSet<String> =
        bodies.iter().map(|body| body.name.clone()).collect();
    // Which callees borrow a `json::Value` they are handed, so the carrier a
    // combinator reads the handle out of can be reclaimed after the call.
    let json_borrowing_fns = collect_json_borrowing_fns(bodies, tcx);
    for body in &mut bodies[start..] {
        // Rewrite `s = s + frag` to the in-place `gos_rt_str_concat_drop_a`
        // BEFORE inserting RC retain/release statements. The rewrite matches a
        // copy-back pattern (the concat's result copied straight back into the
        // accumulator); the RC pass inserts statements into that gap, so
        // running it first would hide the pattern and leave every append on the
        // fresh-allocation path (O(n^2) string building).
        // Forward-propagate concrete types through `B = Copy(A)` chains before
        // the RC passes. A `?`/`unwrap` extraction is typed from the scrutinee's
        // substs, but the let-binding it is copied into can be left `Var` by the
        // checker (e.g. `let s = f()?` in a function whose own return type is
        // unrelated). Without the binding's concrete (RC-managed) type the drop
        // pass cannot tell it owns a `String` and never releases it (a leak).
        propagate_copy_types(body, tcx);
        rewrite_str_concat_consuming(body);
        crate::opt::elide_vec_clone_in_three_way_swaps(body);
        crate::opt::reserve_vecs_for_counted_push_loops(body);
        // After the reservation, so a vector sized for the loop keeps that size.
        crate::opt::fuse_byte_append_loops(body, tcx);
        crate::opt::reserve_hashmaps_for_counted_insert_loops(body, tcx);
        // Fuse `seq.substring(i, i+k)` + `m.inc(kmer)` into the borrowed-slice
        // probe before the RC passes, so the eliminated scratch String gets no
        // retain/release.
        fuse_substring_map_inc(body);
        release_mapped_payloads(body, tcx);
        own_carrier_payloads(body, tcx);
        record_channel_elem_kind(body, tcx);
        drop_confined_channels(body);
        clear_region_on_call_results(body);
        free_overwritten_ctor_values(body, tcx, &container_ctor_free);
        own_returned_map_payloads(body, tcx);
        insert_drops_at_returns(body, tcx);
        complete_ok_or_err_kind(body, tcx);
        insert_rc_releases(body, tcx);
        insert_aggr_copy_drops(body, tcx);
        insert_json_frees(body, tcx, &json_borrowing_fns);
        insert_vec_elem_metas(body, tcx, &user_fn_names);
        insert_early_releases(body, tcx);
        insert_copied_key_releases(body, tcx);
        drop_unread_map_insert_results(body, tcx);
        hoist_loop_carried_releases(body, tcx);
        release_displaced_enum_targets(body, tcx);
        own_rebound_enum_parameters(body, tcx);
        release_rebound_rc_locals(body, tcx);
        pair_holder_err_arm_calls(body, tcx);
        lead_passthrough_shares(body);
        // `insert_*` calls are ownership-acquiring operations: the drop pass
        // emitted a retain for the container's share immediately before the
        // call, and it must retain the source binding's ordinary release.
        // Keeping both sides balanced covers overwrite, removal, early return,
        // and container teardown without a post-hoc "suppress the drop"
        // escape hatch (which used to turn every inserted value into a leak).
        crate::opt::fuse_slice_parse_ranges(body);
        // Copy/type and ownership lowering can expose the canonical
        // three-way swap only after their temporary assignments settle.
        crate::opt::elide_vec_clone_in_three_way_swaps(body);
        crate::opt::elide_vec_clone_of_fresh_temporary(body, tcx);
        crate::opt::elide_vec_clone_of_dead_aggregate_source(body, &user_fn_names);
        crate::opt::move_vec_clone_of_dead_local(body, tcx, &user_fn_names);
        crate::opt::share_read_only_vec_bindings(body, tcx);
        // Follows the drop passes, so the carrier releases they place are part
        // of what it accounts for.
        crate::opt::pop_scalar_aggregates_in_place(body, tcx);
        if std::env::var("GOS_DUMP_MIR_RC").is_ok() {
            eprintln!("=== MIR(post-rc) {} ===", body.name);
            for block in &body.blocks {
                eprintln!("  bb{}:", block.id.as_u32());
                for stmt in &block.stmts {
                    eprintln!("    {:?}", stmt.kind);
                }
                eprintln!("    term: {:?}", block.terminator);
            }
        }
    }
    // `&self` names the same value `self` does, so both spellings of a read of
    // the receiver reach the backends as the same load. Runs before the RC and
    // reuse passes walk these bodies, so what they account for is what the
    // backends are handed.
    load_reference_receiver_reads(bodies, start, tcx);
    // RC retain/release last-use elision (item 3). Runs after the drop
    // passes so the teardown releases they insert are visible to the
    // pass's post-release liveness check; a value moved into a surviving
    // holder and dead afterward keeps only the holder's reference. The
    // gate excludes goroutine-shared values, whose count is adjusted
    // concurrently under the atomic protocol. `GOS_RC_NO_ELIDE` disables
    // the pass for differential measurement and as a safety escape hatch.
    if std::env::var_os("GOS_RC_NO_ELIDE").is_none() {
        // Uniqueness-driven transfers run first, so the null sources they
        // leave are what the null-accounting elision below cleans up.
        // `GOS_RC_NO_UNIQUENESS` disables them for differential measurement.
        if std::env::var_os("GOS_RC_NO_UNIQUENESS").is_none() {
            let summaries = crate::uniqueness::CallSummaries::compute(bodies, tcx);
            for body in &mut bodies[start..] {
                let report = crate::opt::UniquenessReport {
                    shares_transferred: crate::opt::transfer_last_use_shares(body, tcx, &summaries),
                    clones_moved: crate::opt::move_unique_clones(body, tcx, &summaries),
                };
                crate::opt::record_uniqueness(&body.name, report);
            }
        }
        let callees = crate::opt::CalleeParams::compute(bodies, tcx);
        for body in &mut bodies[start..] {
            crate::opt::elide_null_rc_accounting(body);
            crate::opt::elide_redundant_rc_pairs(body, tcx);
            crate::opt::elide_borrowed_holder_rc(body, tcx, &callees);
            crate::opt::elide_moved_aggregate_shares(body, tcx);
            crate::opt::move_stored_rc_shares(body, tcx);
            crate::opt::elide_settled_guarded_walks(body);
            crate::opt::reduce_materialised_counts(body);
        }
    }
    // Perceus reuse: recycle a uniquely-owned block being released into a
    // same-type constructor in place. Runs last so it sees the final release
    // set; `GOS_RC_NO_REUSE` disables it for differential measurement and as a
    // safety escape hatch. Compiled-tier only (the bytecode VM does not consume
    // this MIR), and a runtime refcount check makes any mis-pairing fall back to
    // a fresh allocation rather than corrupt.
    if std::env::var_os("GOS_RC_NO_REUSE").is_none() {
        for body in &mut bodies[start..] {
            crate::opt::insert_rc_reuse(body, tcx);
        }
    }
    #[cfg(debug_assertions)]
    crate::verify::debug_verify_program(bodies, tcx);
}

pub mod builder;
pub(crate) mod carrier_ref;
pub mod helpers;
pub(crate) mod instantiate;

pub(crate) use builder::Builder;
pub use helpers::*;
