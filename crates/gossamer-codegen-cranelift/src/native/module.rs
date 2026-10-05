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
//! Real Cranelift-backed native codegen.
//! Lowers a slice of MIR [`Body`]s into a `cranelift-object` module
//! and serialises the result as ELF (or the host's equivalent object
//! format). Supported today:
//! - `fn main() -> i64` with integer arithmetic (`+`, `-`, `*`, `/`,
//!   `%`, `&`, `|`, `^`, `<<`, `>>`, unary `-`, `!`),
//! - integer constants,
//! - direct calls between lowered functions,
//! - `return` of an `i64`.
//!
//! A C-ABI shim `main(argc, argv) -> i32` is emitted automatically:
//! it calls the Gossamer `main` and truncates the `i64` result into
//! the process exit code, so the object file links through a
//! standard `cc` invocation.
//! Aggregates (tuples/arrays/structs), strings, closures, and
//! anything that needs a GC heap are not yet lowered - those
//! constructs fall back to [`crate::emit::emit_module`] for
//! inspection.

// Allow patterns the Cranelift lowering deliberately uses:
//   - `similar_names` fires on `print_str`/`print_i64`/etc.
//     intrinsic-name shadowing within the same arm. The
//     parallel naming makes the dispatch table readable.
//   - `many_single_char_names` fires on hot inner-loop locals
//     (`a`, `b`, `n`, `m`, `k`) where longer names would
//     overflow the 100-col limit.
//   - `items_after_statements` flags inline `extern "C"` decls
//     localised to the one helper that uses them. Hoisting them
//     to module scope spreads the FFI surface; localised wins.
//   - `too_many_lines` / `cognitive_complexity` fire on the
//     intrinsic-dispatch arm and the `lower_intrinsic_call`
//     match. Splitting either hides the one-arm-per-symbol
//     structure that makes the table grep-able.
//   - `unnecessary_wraps` flags helpers whose `Result` exists
//     so call sites can still `?` them once a future lowering
//     can fail.
//   - `if_chain_can_be_rewritten_with_match` would flatten
//     short `if let Some(x) = .. else if let Some(y) = ..`
//     chains into match-on-tuple-of-options that's strictly
//     uglier here.
//   - `doc_markdown` flags identifiers like `i64`, `f64`,
//     etc. in plain-prose docs. Backticking every numeric
//     type name in every comment is noise.
//   - `manual_debug_impl` flags `JitModule`'s `Debug` impl
//     (which deliberately omits the JIT module pointer to keep
//     debug output stable across runs).
#![forbid(unsafe_code)]
#![allow(clippy::comparison_chain)]

use std::collections::HashMap;

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use cranelift_codegen::ir::{
    AbiParam, ExtFuncData, Function, GlobalValueData, InstBuilder, MemFlagsData, Signature,
    StackSlotData, StackSlotKind, UserExternalName, UserFuncName, condcodes::IntCC,
    immediates::Imm64, types,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, ir};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, ModuleDeclarations};
use cranelift_object::{ObjectBuilder, ObjectModule};
use gossamer_mir::{
    BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, StatementKind, Terminator,
    UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use rayon::prelude::*;

use super::*;

#[derive(Clone)]
pub(super) struct OfflineModule {
    pub(super) frontend_config: TargetFrontendConfig,
    pub(super) default_call_conv: CallConv,
    /// FuncId.as_u32() → (signature, colocated) snapshot from the real module.
    pub(super) func_sigs: HashMap<u32, (Signature, bool)>,
    /// DataId.as_u32() → (colocated, tls) snapshot from the real module.
    pub(super) data_info: HashMap<u32, (bool, bool)>,
}

impl Module for OfflineModule {
    fn isa(&self) -> &dyn cranelift_codegen::isa::TargetIsa {
        unreachable!("OfflineModule: isa() must not be called in parallel IR phase")
    }
    fn declarations(&self) -> &ModuleDeclarations {
        unreachable!("OfflineModule: declarations() must not be called in parallel IR phase")
    }
    fn target_config(&self) -> TargetFrontendConfig {
        self.frontend_config
    }
    fn make_signature(&self) -> Signature {
        Signature::new(self.default_call_conv)
    }
    fn declare_function(
        &mut self,
        _name: &str,
        _linkage: Linkage,
        _sig: &Signature,
    ) -> cranelift_module::ModuleResult<FuncId> {
        unreachable!("OfflineModule: declare_function called in parallel phase - pre-declare first")
    }
    fn declare_anonymous_function(
        &mut self,
        _sig: &Signature,
    ) -> cranelift_module::ModuleResult<FuncId> {
        unreachable!("OfflineModule: declare_anonymous_function called in parallel phase")
    }
    fn declare_data(
        &mut self,
        _name: &str,
        _linkage: Linkage,
        _writable: bool,
        _tls: bool,
    ) -> cranelift_module::ModuleResult<DataId> {
        unreachable!(
            "OfflineModule: declare_data called in parallel phase - pre-intern strings first"
        )
    }
    fn declare_anonymous_data(
        &mut self,
        _writable: bool,
        _tls: bool,
    ) -> cranelift_module::ModuleResult<DataId> {
        unreachable!("OfflineModule: declare_anonymous_data called in parallel phase")
    }
    fn define_function_with_control_plane(
        &mut self,
        _func: FuncId,
        _ctx: &mut Context,
        _ctrl_plane: &mut cranelift_codegen::control::ControlPlane,
    ) -> cranelift_module::ModuleResult<()> {
        unreachable!("OfflineModule: define_function called in parallel phase")
    }
    fn define_function_bytes(
        &mut self,
        _func_id: FuncId,
        _alignment: u64,
        _bytes: &[u8],
        _relocs: &[cranelift_module::ModuleReloc],
    ) -> cranelift_module::ModuleResult<()> {
        unreachable!("OfflineModule: define_function_bytes called in parallel phase")
    }
    fn define_data(
        &mut self,
        _data_id: DataId,
        _ctx: &DataDescription,
    ) -> cranelift_module::ModuleResult<()> {
        unreachable!("OfflineModule: define_data called in parallel phase")
    }
    /// Override the default implementation so we never call `declarations()`.
    fn declare_func_in_func(&mut self, func_id: FuncId, func: &mut ir::Function) -> ir::FuncRef {
        let (sig, colocated) = self.func_sigs.get(&func_id.as_u32()).unwrap_or_else(|| {
            panic!(
                "OfflineModule: FuncId {} not pre-declared",
                func_id.as_u32()
            )
        });
        let signature = func.import_signature(sig.clone());
        let user_name_ref = func.declare_imported_user_function(UserExternalName {
            namespace: 0,
            index: func_id.as_u32(),
        });
        func.import_function(ExtFuncData {
            name: ir::ExternalName::user(user_name_ref),
            signature,
            colocated: *colocated,
            patchable: false,
        })
    }
    /// Override the default implementation so we never call `declarations()`.
    fn declare_data_in_func(&self, data_id: DataId, func: &mut ir::Function) -> ir::GlobalValue {
        // A far reference reaches any symbol; a near one only data this
        // module defines, so a symbol the snapshot does not describe is far.
        let (colocated, tls) = self
            .data_info
            .get(&data_id.as_u32())
            .copied()
            .unwrap_or((false, false));
        let user_name_ref = func.declare_imported_user_function(UserExternalName {
            namespace: 1,
            index: data_id.as_u32(),
        });
        func.create_global_value(GlobalValueData::Symbol {
            name: ir::ExternalName::user(user_name_ref),
            offset: Imm64::new(0),
            colocated,
            tls,
        })
    }
}
