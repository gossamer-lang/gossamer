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
    let (structs, struct_defs) = collect_struct_fields(program);
    let enums = collect_enum_variants(program);
    register_inline_enums(program, tcx);
    let impl_methods = collect_impl_methods(program);
    let impl_method_receivers = collect_impl_method_receivers(program);
    let impl_method_inputs = collect_impl_method_inputs(program);
    let fn_ret_names = collect_fn_ret_names(program);
    let fn_returns = collect_fn_returns(program, tcx);
    let fn_inputs = collect_fn_inputs(program);
    let mut consts = collect_const_values(program);
    // Scalar `static mut` items become real mutable module globals;
    // remove them from the const-inline map so their reads load the
    // live cell instead of freezing at the declaration value.
    let mut_statics = collect_mut_static_defs(program, &consts);
    for def in mut_statics.keys() {
        consts.remove(def);
    }
    let const_inits = collect_const_init_exprs(program);
    // Conservative escape summary driving automatic arena regions.
    let region_unsafe = collect_region_unsafe_fns(program, tcx);
    let fn_param_shareable = collect_shareable_params(program, tcx);
    let mut bodies = Vec::new();
    for item in &program.items {
        collect_item(
            item,
            tcx,
            &structs,
            &struct_defs,
            &enums,
            &impl_methods,
            &impl_method_receivers,
            &impl_method_inputs,
            &fn_ret_names,
            &fn_returns,
            &fn_inputs,
            &fn_param_shareable,
            &consts,
            &mut_statics,
            &const_inits,
            &region_unsafe,
            &mut bodies,
        );
    }
    for body in &mut bodies {
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
        crate::opt::reserve_hashmaps_for_counted_insert_loops(body, tcx);
        // Fuse `seq.substring(i, i+k)` + `m.inc(kmer)` into the borrowed-slice
        // probe before the RC passes, so the eliminated scratch String gets no
        // retain/release.
        fuse_substring_map_inc(body);
        release_mapped_payloads(body, tcx);
        record_channel_elem_kind(body, tcx);
        drop_confined_channels(body);
        clear_region_on_call_results(body);
        free_overwritten_ctor_values(body, &container_ctor_free);
        insert_drops_at_returns(body, tcx);
        insert_rc_releases(body, tcx);
        insert_aggr_copy_drops(body, tcx);
        insert_json_frees(body, tcx);
        insert_vec_elem_metas(body, tcx);
        insert_early_releases(body, tcx);
        insert_copied_key_releases(body, tcx);
        drop_unread_map_insert_results(body, tcx);
        hoist_loop_carried_releases(body, tcx);
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
    load_reference_receiver_reads(&mut bodies, tcx);
    // RC retain/release last-use elision (item 3). Runs after the drop
    // passes so the teardown releases they insert are visible to the
    // pass's post-release liveness check; a value moved into a surviving
    // holder and dead afterward keeps only the holder's reference. The
    // gate excludes goroutine-shared values, whose count is adjusted
    // concurrently under the atomic protocol. `GOS_RC_NO_ELIDE` disables
    // the pass for differential measurement and as a safety escape hatch.
    if std::env::var_os("GOS_RC_NO_ELIDE").is_none() {
        for body in &mut bodies {
            crate::opt::elide_null_rc_accounting(body);
            crate::opt::elide_redundant_rc_pairs(body, tcx);
            crate::opt::elide_borrowed_holder_rc(body, tcx);
        }
    }
    // Perceus reuse: recycle a uniquely-owned block being released into a
    // same-type constructor in place. Runs last so it sees the final release
    // set; `GOS_RC_NO_REUSE` disables it for differential measurement and as a
    // safety escape hatch. Compiled-tier only (the bytecode VM does not consume
    // this MIR), and a runtime refcount check makes any mis-pairing fall back to
    // a fresh allocation rather than corrupt.
    if std::env::var_os("GOS_RC_NO_REUSE").is_none() {
        for body in &mut bodies {
            crate::opt::insert_rc_reuse(body, tcx);
        }
    }
    #[cfg(debug_assertions)]
    crate::verify::debug_verify_program(&bodies, tcx);
    bodies
}

pub mod builder;
pub(crate) mod carrier_ref;
pub mod helpers;

pub(crate) use builder::Builder;
pub use helpers::*;
