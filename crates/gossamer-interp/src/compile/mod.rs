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
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
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
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! HIR → bytecode compiler.

#![forbid(unsafe_code)]
use std::collections::HashMap;
use std::sync::Arc;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirLiteral, HirPat, HirPatKind, HirStmt,
    HirStmtKind, HirUnaryOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

use crate::bytecode::{ConstIdx, FnChunk, GlobalIdx, ImmArithKind, InstrIdx, Op, Reg};
use crate::value::{RuntimeError, RuntimeResult, SmolStr, Value};

/// Panic message for a `match` that runs off the end without any arm
/// matching. Kept bit-identical to the compiled tiers' constant in
/// `gossamer-mir` so the bytecode VM and LLVM/Cranelift output the same
/// GX panic text on a non-exhaustive match (a checker blind spot such as
/// an unenumerable integer payload).
pub(crate) const NON_EXHAUSTIVE_MATCH_MESSAGE: &str =
    "non-exhaustive match: no pattern matched the value";

/// Kind of a virtual register. Phase-1 typed opcodes target
/// these kinds directly to skip the `Value` enum pack/unpack
/// that dominates numeric kernels. Unknown / aggregate
/// registers stay in [`RegKind::Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegKind {
    /// Boxed [`crate::value::Value`] register (the default,
    /// and the ABI used for calls / returns / aggregates).
    Value,
    /// Unboxed `f64` register.
    F64,
    /// Unboxed `i64` register.
    I64,
}

/// A register plus the file it lives in. Typed opcodes read
/// from / write to one file per operand.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TypedReg {
    pub reg: Reg,
    pub kind: RegKind,
}

/// Struct declaration-order field tables the compiler uses to
/// resolve field-access offsets at compile time.
pub(crate) type StructLayouts = std::collections::HashMap<gossamer_resolve::DefId, Vec<String>>;

/// Returns `true` when `ty` is `&mut Vec<T>` / `&mut [T]` - the
/// parameter / argument shape that rides the write-back cell
/// protocol (`Op::CellNew` / `Op::CellTake` /
/// `FnChunk::mut_ref_params`).
pub(crate) fn is_mut_ref_vec(tcx: &TyCtxt, ty: Ty) -> bool {
    let Some(TyKind::Ref {
        mutability: gossamer_types::Mutbl::Mut,
        inner,
    }) = tcx.kind(ty)
    else {
        return false;
    };
    matches!(tcx.kind(*inner), Some(TyKind::Vec(_) | TyKind::Slice(_)))
}

/// Returns `true` when `ty` is a `&mut T` whose mutation through the
/// reference must be visible to the caller on every tier: `&mut Vec<T>`
/// / `&mut [T]` (the cell-protocol shapes above), `&mut <scalar
/// primitive>` (`i*` / `u*` / `f*` / `bool` / `char`), `&mut String`
/// (a flat `*mut c_char` whose pointer IS the value), and `&mut <struct
/// / enum>` (the compiled tiers pass an aggregate `&mut` by pointer, so
/// a field write or `*p = v` reaches the caller - the VM must match, or
/// a free function mutating a `&mut Struct` param silently no-ops under
/// `gos` while writing back under `gos build`). Used to decide that
/// a call carrying such an argument participates in the `MutCell`
/// write-back + `*p = …` deref-assign protocol. Fixed `[T; N]` arrays
/// participate too: `&mut` aliases always write through.
pub(crate) fn is_mut_ref_writeback(tcx: &TyCtxt, ty: Ty) -> bool {
    if is_mut_ref_vec(tcx, ty) {
        return true;
    }
    let Some(TyKind::Ref {
        mutability: gossamer_types::Mutbl::Mut,
        inner,
    }) = tcx.kind(ty)
    else {
        return false;
    };
    is_writeback_pointee(tcx, *inner)
}

/// Returns `true` when a `&mut` alias to a place of type `ty` rides the
/// write-back protocol. This is the pointee half of
/// [`is_mut_ref_writeback`]: a call site wraps an argument in a cell only
/// for these types, because only for these does the callee's parameter
/// register unwrap one. A container reaches its storage through a handle,
/// but `*p = v` replaces the binding the handle came from, so every
/// container rides the protocol; a channel endpoint or a lazy iterator has
/// no rebinding form and crosses the call boundary as the bare handle.
pub(crate) fn is_writeback_pointee(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind(ty),
        Some(
            TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Duration
                | TyKind::Instant
                | TyKind::Array { .. }
                | TyKind::HashMap { .. }
                | TyKind::Adt { .. }
                | TyKind::Param { .. }
        )
    )
}

/// Trivial-wrapper inlining table: for each user function
/// whose body is `return intrinsic(param)` (a pattern common
/// enough in library code that its call overhead shows up on
/// profiles), we record the target intrinsic's path segments.
/// Calls to the wrapper are rewritten to direct intrinsic
/// calls at compile time - no `Op::Call`, no push/pop of a
/// frame, no boxing across the call boundary.
pub(crate) type InlinableWrappers = std::collections::HashMap<String, Vec<String>>;

/// An owned snapshot of a user function the bytecode compiler may inline
/// at its call sites. Holds clones of the function's parameter patterns
/// and its single tail expression, taken once at load time so the
/// inliner re-compiles the body without re-borrowing the `HirProgram`.
/// Built by [`inline::detect_inlinable_fn`] and consulted by
/// [`FnBuilder::try_inline_user_call`].
#[derive(Clone)]
pub(crate) struct InlinableFn {
    /// Parameter binding patterns in declaration order.
    pub(crate) params: Vec<HirPat>,
    /// Straight-line statements evaluated before [`Self::tail`].
    pub(crate) stmts: Vec<HirStmt>,
    /// The function's tail expression, re-compiled directly into the caller
    /// after [`Self::stmts`].
    pub(crate) tail: HirExpr,
    /// Weighted node count of `tail`, charged against the caller's inline
    /// budget so transitive inlining stays bounded.
    pub(crate) cost: usize,
}

/// User-function inlining table, keyed by bare function name (mirroring
/// [`InlinableWrappers`]). A call whose callee is a single-segment path
/// naming an entry here may be inlined in place of an `Op::Call`. The
/// map is owned by the VM load frame and plumbed by `&` exactly like
/// `wrappers` / `module_consts`, so its lifetime never entangles with
/// the `HirProgram` borrow.
pub(crate) type InlinableFns = std::collections::HashMap<String, InlinableFn>;

/// Per-parameter "the callee only reads this" flags by callable name. Shared
/// with the compiled tiers so a by-value container argument is copied on the
/// same rule everywhere.
pub(crate) type FnParamShareable = std::collections::HashMap<String, Vec<bool>>;

/// User-function parameter types keyed by the source path used at call sites.
/// The bytecode compiler uses these to select the explicit mutable-reference
/// write-back protocol consistently with the callee signature.
pub(crate) type FnParamTypes = std::collections::HashMap<String, Vec<Ty>>;

/// Top-level `const` items, keyed by name, with their already-
/// evaluated `Value`. The bytecode compiler inlines a path that
/// resolves to one of these into a `LoadConst` (constant-pool
/// fetch - single index) instead of a `LoadGlobal` (string-keyed
/// `HashMap` lookup). The win shows up on hot loops that close
/// over constants - fasta's `(state*IA+IC) % IM` LCG step would
/// otherwise pay three name lookups per iteration.
pub(crate) type ConstValues = std::collections::HashMap<gossamer_resolve::DefId, Value>;

/// Qualified names (`Type::method`) of every user `impl` method whose
/// receiver is `&mut self`. A method call on a writeback place
/// (`obj.bump()`, `(&mut __for_iter).next()`) lowers through the
/// write-back cell protocol so the method's mutation of `self`
/// persists in the caller's binding - matching the by-pointer receiver
/// the compiled tiers pass. The bytecode compiler reconstructs the
/// `Type::method` key from the receiver's resolved type at each call
/// site and consults this set to decide whether the writeback fires.
pub(crate) type MutSelfMethods = std::collections::HashSet<String>;

/// Every name under which an `impl` or `trait` block's method is filed in
/// [`FnParamTypes`] - the bare `method`, `Type::method`, and both of those
/// under the declaring module. A method call whose receiver type is open
/// binds to a uniquely-named candidate from this set, which is what keeps
/// the guess from reaching a module's free function of the same name.
pub(crate) type ImplMethodNames = std::collections::HashSet<String>;

/// Bare names of every `static mut` item in the program. The bytecode
/// compiler consults this set to lower an assignment whose place is
/// rooted at a mutable static into an [`Op::StoreStatic`] against the
/// shared `Global::MutStatic` cell, rather than treating the path as a
/// local. Reads need no entry here: a mutable static is excluded from
/// the const-inlining snapshot, so its path already lowers to a
/// `LoadGlobal` that resolves the cell at runtime.
pub(crate) type MutStatics = std::collections::HashSet<String>;

/// Collects the [`MutStatics`] set: the name of every `static mut` item.
pub fn collect_mut_statics(program: &gossamer_hir::HirProgram) -> MutStatics {
    let mut out = MutStatics::new();
    for item in &program.items {
        if let gossamer_hir::HirItemKind::Static(decl) = &item.kind
            && decl.mutable
        {
            out.insert(decl.name.name.clone());
        }
    }
    out
}

/// Collects the [`MutSelfMethods`] set from a program's `impl` blocks:
/// every method whose first parameter is a `&mut self` receiver. Built
/// once at load time and threaded into [`compile_fn`].
pub fn collect_mut_self_methods(program: &gossamer_hir::HirProgram) -> MutSelfMethods {
    let mut out = MutSelfMethods::new();
    for item in &program.items {
        let gossamer_hir::HirItemKind::Impl(decl) = &item.kind else {
            continue;
        };
        let Some(type_name) = &decl.self_name else {
            continue;
        };
        for method in &decl.methods {
            if method_has_mut_self(method) {
                out.insert(format!("{}::{}", type_name.name, method.name.name));
            }
        }
    }
    out
}

/// Collects the [`ImplMethodNames`] set, mirroring every key the loader
/// files an `impl` or `trait` method under.
pub fn collect_impl_method_names(program: &gossamer_hir::HirProgram) -> ImplMethodNames {
    let mut out = ImplMethodNames::new();
    for item in &program.items {
        let prefix = (!item.module_path.is_empty()).then(|| item.module_path.join("::"));
        let methods = match &item.kind {
            gossamer_hir::HirItemKind::Impl(decl) => {
                for method in &decl.methods {
                    if let Some(type_name) = &decl.self_name {
                        let qualified = format!("{}::{}", type_name.name, method.name.name);
                        if let Some(prefix) = &prefix {
                            out.insert(format!("{prefix}::{qualified}"));
                        }
                        out.insert(qualified);
                    }
                }
                continue;
            }
            gossamer_hir::HirItemKind::Trait(decl) => &decl.methods,
            _ => continue,
        };
        for method in methods {
            if let Some(prefix) = &prefix {
                out.insert(format!("{prefix}::{}", method.name.name));
            }
            out.insert(method.name.name.clone());
        }
    }
    out
}

/// `true` when `decl`'s first parameter is a `&mut self` receiver - the
/// shape (`fn next(&mut self)`, `fn bump(&mut self)`) whose mutation
/// must flow back to the caller's binding.
fn method_has_mut_self(decl: &HirFn) -> bool {
    if !decl.has_self {
        return false;
    }
    matches!(
        decl.params.first().map(|p| &p.pattern.kind),
        Some(HirPatKind::Binding {
            name,
            mutable: true,
        }) if name.name == "self"
    )
}

/// Compiles an [`HirFn`] body into a [`FnChunk`]. The caller owns the
/// resulting chunk; the compiler itself has no shared state.
pub fn compile_fn(
    decl: &HirFn,
    tcx: &TyCtxt,
    layouts: &StructLayouts,
    wrappers: &InlinableWrappers,
    inline_fns: &InlinableFns,
    fn_param_shareable: &FnParamShareable,
    fn_param_tys: &FnParamTypes,
    consts: &ConstValues,
    method_muts: &MutSelfMethods,
    impl_methods: &ImplMethodNames,
    mut_statics: &MutStatics,
    source_map: Option<&gossamer_lex::SourceMap>,
    cov: Option<&gossamer_lex::SourceMap>,
) -> RuntimeResult<FnChunk> {
    let name = crate::value::intern_type_name(&decl.name.name);
    let Some(body) = decl.body.as_ref() else {
        return Ok(FnChunk {
            name,
            arity: u16::try_from(decl.params.len()).unwrap_or(u16::MAX),
            register_count: 0,
            float_count: 0,
            int_count: 0,
            instrs: Vec::new(),
            instruction_locations: Vec::new(),
            consts: Vec::new(),
            f64_consts: Vec::new(),
            i64_consts: Vec::new(),
            globals: Vec::new(),
            shape_names: Vec::new(),
            wide_ops: Vec::new(),
            call_cache_count: 0,
            arith_cache_count: 0,
            field_cache_count: 0,
            mut_ref_params: Vec::new(),
            i64_params: Vec::new(),
            closure_protos: Vec::new(),
            select_arms: Vec::new(),
        });
    };
    let mut builder = FnBuilder::new(
        name,
        tcx,
        layouts,
        wrappers,
        inline_fns,
        fn_param_shareable,
        fn_param_tys,
        consts,
        method_muts,
        impl_methods,
        mut_statics,
        source_map,
        cov,
    );
    builder.consumable = consume::consumable_locals(decl);
    let (captured_locals, mutated_locals) =
        consume::closure_captured_locals(&decl.params, &body.block);
    builder.capture_cell_names = capture_cell_names(tcx, &captured_locals, &mutated_locals);
    let mut pending_cells: Vec<(String, Reg, gossamer_types::Ty)> = Vec::new();
    for (idx, param) in decl.params.iter().enumerate() {
        let reg = builder.alloc_reg();
        if matches!(tcx.kind(param.ty), Some(TyKind::Int(_))) {
            let int_reg = builder.alloc_int();
            builder.bind_typed_param(&param.pattern, int_reg, RegKind::I64);
            builder.i64_params.push((reg, int_reg));
        } else {
            builder.bind_param(&param.pattern, reg)?;
        }
        if matches!(
            tcx.kind(builder.unwrap_ref(param.ty)),
            Some(TyKind::Iterator(_))
        ) {
            builder.lazy_iterator_locals.insert(reg);
        }
        // `&mut Vec<T>` / `&mut [T]` / `&mut <scalar>` parameters
        // participate in the write-back cell protocol - the callee
        // unwraps an incoming `MutCell` into the param register and
        // publishes its final value back on return. See
        // `FnChunk::mut_ref_params`. A `&mut self` receiver (an
        // aggregate, which `is_mut_ref_writeback` deliberately excludes
        // for general args) rides the same protocol: the compiled tiers
        // pass `self` by pointer, so its mutation must reach the
        // caller's binding. Marking the receiver register here lets the
        // call-site cell wrapping in `compile_method_call` publish the
        // post-call `self` back to the receiver place.
        if is_mut_ref_writeback(tcx, param.ty)
            || (idx == 0 && method_has_mut_self(decl) && builder.is_adt_ref(param.ty))
        {
            builder.mut_ref_params.push(reg);
        }
        // Numeric Vec and slice parameters normally arrive in the flat storage
        // produced by their constructors. Typed indexed ops retain a boxed-array
        // fallback for values created by a generic collection operation.
        if let Some(elem) = builder.array_elem_ty(param.ty) {
            match tcx.kind(elem) {
                Some(TyKind::Float(FloatTy::F64)) => {
                    builder.flat_float_locals.insert(reg);
                }
                Some(TyKind::Int(IntTy::I64 | IntTy::Isize | IntTy::Usize)) => {
                    builder.flat_int_locals.insert(reg);
                }
                _ => {}
            }
        }
        if let HirPatKind::Binding { name, .. } = &param.pattern.kind {
            pending_cells.push((name.name.clone(), reg, param.ty));
        }
    }
    // Capture cells claim registers of their own, so they are installed
    // only once every parameter holds its register: the VM fills the
    // leading `arity` registers with the incoming arguments, and a cell
    // allocated between two parameters would take the slot the later one
    // is called with.
    for (name, reg, ty) in pending_cells {
        let typed = TypedReg {
            reg,
            kind: RegKind::Value,
        };
        builder.install_capture_cell(&name, typed, ty);
    }
    let result = builder.compile_block(&body.block)?;
    if matches!(result, BlockResult::ValueIn(_)) {
        let BlockResult::ValueIn(reg) = result else {
            unreachable!()
        };
        let reg = match body.block.tail.as_deref() {
            Some(tail) => builder.cloned_returned_field_container(tail, reg),
            None => reg,
        };
        builder.emit(Op::Return { value: reg });
    } else {
        builder.emit(Op::ReturnUnit);
    }
    let arity = u16::try_from(decl.params.len()).unwrap_or(u16::MAX);
    Ok(builder.finish(arity))
}

/// Filters the captured bindings a closure analysis reported down to the
/// names this function stores in capture cells: those whose type the
/// compiled tiers name through a shared heap buffer.
fn capture_cell_names(
    tcx: &TyCtxt,
    captured: &[(String, gossamer_types::Ty)],
    mutated: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    captured
        .iter()
        .filter(|(name, ty)| {
            // A cell exists so both sides of a capture name one buffer once
            // either writes it. A binding nothing writes has no such reader
            // to serve, and the cell's read - which empties the cell for the
            // length of the instruction - is what two goroutines sharing one
            // capture race on.
            mutated.contains(name)
                && matches!(tcx.kind(*ty), Some(TyKind::Vec(_) | TyKind::Slice(_)))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Compiles a single `const`/`static` initializer expression into a
/// synthetic nullary [`FnChunk`]. Running the chunk on the VM yields the
/// item's value, so const/static evaluation reuses the ordinary
/// compile-and-run path - every initializer shape the VM can lower
/// (literals, arithmetic, aggregates, prelude/const-fn calls) is
/// evaluated by the same machinery, with no separate evaluator.
pub fn compile_initializer(
    expr: &HirExpr,
    tcx: &TyCtxt,
    layouts: &StructLayouts,
    wrappers: &InlinableWrappers,
    inline_fns: &InlinableFns,
    fn_param_shareable: &FnParamShareable,
    fn_param_tys: &FnParamTypes,
    consts: &ConstValues,
    method_muts: &MutSelfMethods,
    impl_methods: &ImplMethodNames,
    mut_statics: &MutStatics,
    source_map: Option<&gossamer_lex::SourceMap>,
    cov: Option<&gossamer_lex::SourceMap>,
) -> RuntimeResult<FnChunk> {
    let name = crate::value::intern_type_name("__init");
    let mut builder = FnBuilder::new(
        name,
        tcx,
        layouts,
        wrappers,
        inline_fns,
        fn_param_shareable,
        fn_param_tys,
        consts,
        method_muts,
        impl_methods,
        mut_statics,
        source_map,
        cov,
    );
    let reg = builder.compile_expr(expr)?;
    builder.emit(Op::Return { value: reg });
    Ok(builder.finish(0))
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BlockResult {
    Unit,
    ValueIn(Reg),
    Diverges,
}

pub(crate) struct FnBuilder<'tcx> {
    pub(crate) name: &'static str,
    pub(crate) tcx: &'tcx TyCtxt,
    pub(crate) layouts: &'tcx StructLayouts,
    pub(crate) wrappers: &'tcx InlinableWrappers,
    /// User functions eligible for call-site inlining (see
    /// [`InlinableFns`]). Borrowed from the VM load frame for the
    /// duration of the compile, like `wrappers`.
    pub(crate) inline_fns: &'tcx InlinableFns,
    /// See [`FnParamShareable`].
    pub(crate) fn_param_shareable: &'tcx FnParamShareable,
    /// Parameter types for user functions, used to lower explicit `&mut`
    /// call arguments through the write-back protocol.
    pub(crate) fn_param_tys: &'tcx FnParamTypes,
    /// Names of the functions whose bodies are currently being inlined
    /// into this builder, innermost last. Consulted before each inline to
    /// reject direct / mutual / transitive recursion; pushed before
    /// compiling a callee body and popped after.
    pub(crate) inlining: Vec<&'static str>,
    /// Running total of inlined tail nodes spliced into this builder.
    /// Once it would exceed the per-caller budget, further calls stay
    /// real `Op::Call`s, bounding code growth.
    pub(crate) inlined_nodes: usize,
    /// Pre-evaluated values for `const` items. A path
    /// expression that resolves to one of these inlines as a
    /// `LoadConst` instead of a `LoadGlobal` so the bytecode VM
    /// fetches it via constant-pool index instead of a runtime
    /// name lookup.
    pub(crate) module_consts: &'tcx ConstValues,
    /// Value registers that are compile-time-proven to hold
    /// `Value::FloatArray` - populated by `BuildFloatArray`
    /// emission and cleared whenever the register is
    /// reassigned. When a read/write op's base is one of
    /// these, we emit `FlatGetF64` / `FlatSetF64` instead of
    /// the discriminant-checking `IndexedFieldGetF64ByOffset`.
    pub(crate) flat_locals: std::collections::HashMap<Reg, u16>,
    /// Value registers compile-time-proven to hold a
    /// `Value::IntArray` (a primitive `[i64; N]` literal). Reads
    /// against one of these registers route through
    /// [`Op::IntArrayGetI64`] into a typed `i64` register.
    pub(crate) flat_int_locals: std::collections::HashSet<Reg>,
    /// Mirror of [`Self::flat_int_locals`] for `Value::FloatVec` -
    /// `[f64; N]` literals built via [`Self::try_build_float_vec`].
    /// Lets indexed reads / writes route through the typed-`f64`
    /// fast path that skips the `Value::Float` round-trip.
    pub(crate) flat_float_locals: std::collections::HashSet<Reg>,
    /// Value or integer registers whose source expression is known to be an
    /// `as u64` / `as usize` cast. Renderer calls box these as `Value::Uint`
    /// so VM output matches the compiled tiers' unsigned printer choice.
    pub(crate) uint_display_locals: std::collections::HashSet<Reg>,
    /// Registers shared by a local reference binding and its source place.
    /// Last-use clearing is disabled for these registers because the
    /// consume analysis tracks names, while either name may still access the
    /// same storage.
    pub(crate) reference_alias_regs: std::collections::HashSet<Reg>,
    /// Lowest `next_reg` value that must survive discarded-statement
    /// register restores because a temporary value register became the
    /// referent of a local reference binding.
    pub(crate) escaped_reference_reg_floor: Reg,
    /// Local names this function stores in a capture cell: a closure in
    /// the body reads them from an enclosing binding and their type is
    /// one the compiled tiers share by pointer.
    pub(crate) capture_cell_names: std::collections::HashSet<String>,
    /// Active capture-cell bindings as `(home register, cell register)`,
    /// innermost scope last. Every emitted instruction that names a home
    /// register is bracketed with the cell's load / store.
    pub(crate) capture_cells: Vec<(Reg, Reg)>,
    /// `true` once any binding in this function has been cell-backed.
    /// Peepholes that assume a call's argument moves sit in one
    /// uninterrupted run are skipped, since cell traffic separates them.
    pub(crate) capture_cells_used: bool,
    /// Registers bound by a pattern to an array / vec / slice value
    /// (`Some(arr)`, `(head, tail)`, …). A pattern binding's `Path`
    /// reference carries the binding's *declared* type only when the
    /// frontend resolved it; for an inferred binding the HIR `ty` stays
    /// an unresolved var, so the for-loop fast path can't tell `for x in
    /// arr` iterates a collection. Tracking the binding register here
    /// lets `try_compile_for_loop_vec_iter` drive it by index instead of
    /// deferring. Populated from the pattern's resolved type at bind time.
    pub(crate) collection_locals: std::collections::HashSet<Reg>,
    /// Registers proven to hold a stateful, single-pass `Iterator<T>`.
    /// Parameter and inferred-local paths can retain an unresolved HIR type,
    /// so for-loop lowering consults this provenance before considering the
    /// indexable collection fast path.
    pub(crate) lazy_iterator_locals: std::collections::HashSet<Reg>,
    /// Registers holding an `Iterator<T>` that is a materialized
    /// sequence - a collection's `iter()` / `enumerate()` result - rather
    /// than a live cursor such as a range. A loop drives these by index.
    /// Registers bound to a `flag::Set` handle (`flag::Set::new(...)`),
    /// so a chained `set.duration(...)` is recognised as constructing a
    /// duration-flag cell rather than calling a same-named user method.
    pub(crate) flag_set_locals: std::collections::HashSet<Reg>,
    /// Registers bound to a `flag::Set` duration cell
    /// (`set.duration(...)`). A duration cell's element type is the
    /// transparent `time::Duration` newtype, but the typechecker leaves
    /// the cell an inference var, so the method-form accessors
    /// (`cell.as_millis()`) dispatch on this tag instead of the receiver
    /// type, routing to the `time::Duration::<accessor>` global with the
    /// cell auto-deref'd to its backing `i64`-of-ms value.
    pub(crate) duration_cell_locals: std::collections::HashSet<Reg>,
    pub(crate) instrs: Vec<Op>,
    pub(crate) consts: Vec<Value>,
    pub(crate) const_cache: HashMap<ConstKey, ConstIdx>,
    pub(crate) f64_consts: Vec<f64>,
    pub(crate) f64_const_cache: HashMap<u64, ConstIdx>,
    pub(crate) i64_consts: Vec<i64>,
    pub(crate) i64_const_cache: HashMap<i64, ConstIdx>,
    pub(crate) globals: Vec<Box<str>>,
    pub(crate) shape_names: Vec<&'static str>,
    pub(crate) global_cache: HashMap<String, GlobalIdx>,
    pub(crate) next_reg: u16,
    pub(crate) next_float_reg: u16,
    pub(crate) next_int_reg: u16,
    /// Largest register-file cursors reached before dead temporary spans were
    /// recycled. `finish` sizes frames from these high-water marks while the
    /// `next_*` cursors remain free to reuse physical slots.
    pub(crate) max_reg: u16,
    pub(crate) max_float_reg: u16,
    pub(crate) max_int_reg: u16,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) loop_stack: Vec<LoopCtx>,
    /// The label of the loop currently being compiled, set by the
    /// `Loop` / `While` compilation site just before it descends into a
    /// loop emitter. Each emitter takes it at entry and records it on
    /// the `LoopCtx` it pushes, so labelled `break`/`continue` can
    /// target the right loop. `None` for an unlabelled loop.
    pub(crate) pending_loop_label: Option<String>,
    /// Per-block frames of `defer`red expressions, mirroring the MIR
    /// builder's `defer_stack`. `compile_block` pushes a frame on entry
    /// and emits it LIFO on a normal exit; `return` / `break` /
    /// `continue` emit the pending frames at their exit edge before the
    /// jump. The same block-scoped LIFO contract the compiled tiers use.
    pub(crate) defer_stack: Vec<Vec<HirExpr>>,
    pub(crate) closure_protos: Vec<crate::bytecode::ClosureProto>,
    pub(crate) select_arms: Vec<crate::bytecode::SelectArmMeta>,
    pub(crate) wide_ops: Vec<crate::bytecode::WideOp>,
    /// Counter incremented every time we emit a dispatch op
    /// (`Op::Call` / `Op::MethodCall`) so each call site gets a
    /// unique inline-cache slot index. The `FnChunk` allocates a
    /// `Vec<CacheSlot>` of this size at finish time.
    pub(crate) next_cache_idx: u16,
    /// Counter for `Op::FieldGet` IC slots (T2.5).
    pub(crate) next_field_cache_idx: u16,
    /// Counter incremented every time we emit a generic-`Value`
    /// arith op (`Op::AddInt` / `Op::SubInt` / `Op::MulInt` /
    /// `Op::DivInt` / `Op::RemInt`) so each site gets its own
    /// `arith_caches` slot. The `FnChunk` allocates the cache
    /// vector to this size at `finish` time. Tier C2.
    pub(crate) next_arith_cache_idx: u16,
    /// Parameter registers declared `&mut Vec<T>` / `&mut [T]`;
    /// copied into [`FnChunk::mut_ref_params`] at `finish` time.
    pub(crate) mut_ref_params: Vec<Reg>,
    pub(crate) i64_params: Vec<(Reg, Reg)>,
    /// Qualified names (`Type::method`) of user `&mut self` methods, used
    /// by `compile_method_call` to route a place-receiver call through
    /// the write-back cell protocol so the receiver's mutation persists.
    pub(crate) method_muts: &'tcx MutSelfMethods,
    /// Names an `impl` or `trait` method is filed under; see [`ImplMethodNames`].
    pub(crate) impl_methods: &'tcx ImplMethodNames,
    /// Names of `static mut` items. Used to lower a static-rooted
    /// assignment into an [`Op::StoreStatic`] instead of deferring it.
    pub(crate) mut_statics: &'tcx MutStatics,
    /// Source map used to resolve bytecode instruction positions for runtime
    /// tracebacks. This is independent of coverage instrumentation.
    pub(crate) source_map: Option<&'tcx gossamer_lex::SourceMap>,
    /// Source map for `gos test --coverage`. `Some` only when the VM
    /// loaded the program with coverage active (see
    /// [`crate::vm::Vm::coverage_active`]); each `compile_stmt` then
    /// resolves the statement span to `(file, line)`, registers a
    /// counter slot, and emits [`Op::CovHit`]. `None` everywhere else,
    /// so non-coverage compiles pay nothing.
    pub(crate) cov: Option<&'tcx gossamer_lex::SourceMap>,
    /// Source position for each emitted instruction while the chunk is being
    /// built. [`Self::finish`](FnBuilder::finish) run-length encodes it.
    pub(crate) instruction_locations: Vec<Option<crate::bytecode::SourceLocation>>,
    /// Local names this function may consume (move) at their single
    /// use - see [`consume::consumable_locals`]. Read at the
    /// consuming sites to emit `*Consume` ops in place of the cloning
    /// `Move` / `VariantField` / `IndexGet` / `TupleIndex`. Empty for
    /// closure-body sub-builders and during call inlining, so those
    /// contexts never consume.
    pub(crate) consumable: std::collections::HashSet<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Scope {
    pub(crate) locals: HashMap<String, TypedReg>,
    /// Local names whose storage is a direct reference alias rather than an
    /// independent value register.
    pub(crate) reference_bindings: std::collections::HashSet<String>,
    /// `FnBuilder::capture_cells` length when this scope opened. Cells
    /// installed inside the scope stop applying when it closes, which is
    /// also when their home register becomes reusable.
    pub(crate) capture_cell_mark: usize,
}

#[derive(Debug)]
pub(crate) struct LoopCtx {
    pub(crate) break_patches: Vec<InstrIdx>,
    /// Forward jumps emitted for `continue` inside this loop. Each
    /// entry is the index of an `Op::Jump { target: 0 }` waiting to
    /// be patched to the loop's per-iteration step op (or back to
    /// the re-entry header for shapes whose step happens at the
    /// top). The per-loop emitter resolves these once it has
    /// emitted its step / re-entry op so `continue` skips the
    /// rest of the body without bypassing the iteration counter.
    pub(crate) continue_patches: Vec<InstrIdx>,
    pub(crate) result_reg: Reg,
    /// `defer_stack` length at loop entry. `break` / `continue` emit the
    /// defer frames at indices `>= defer_depth` - the blocks nested
    /// inside the loop body - before jumping, so per-iteration cleanup
    /// runs on every exit edge while the loop's enclosing frames stay
    /// pending. Mirrors `gossamer-mir`'s `LoopCtx::defer_depth`.
    pub(crate) defer_depth: usize,
    /// Loop label (without the leading apostrophe), or `None` for an
    /// unlabelled loop. Labelled `break`/`continue` scan the stack from
    /// the innermost outward for a matching label.
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ConstKey {
    Unit,
    Bool(bool),
    Int(i64),
    Float(u64),
    Char(char),
    String(String),
    Variant(String),
    /// A struct's field-name list, interned as `Value::Array` of names. Its
    /// own key space keeps a single-field list from sharing a slot with the
    /// plain string const of that one field's name.
    FieldNames(Vec<String>),
}

mod block;
mod call_expr;
mod closure;
mod compile_expr;
mod consume;
mod control_flow;
mod emit_const;
mod fast_paths;
mod inline;
mod lifecycle;
mod op_expr;
mod reg_scope;
mod stmt;
mod type_helpers;

pub(crate) use inline::detect_inlinable_fn;

fn expr_diverges(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::Return(_) | HirExprKind::Break { .. } | HirExprKind::Continue { .. }
    )
}

/// Returns `true` when `expr` is a bare single-segment path.
/// Used by `let` binding to detect the aliasing case - binding
/// a local to the reg of an existing local would share storage
/// and propagate future writes through the alias. Every other
/// expression produces a freshly-allocated reg we can bind
/// directly.
fn is_path_expr(expr: &HirExpr) -> bool {
    matches!(&expr.kind, HirExprKind::Path { .. })
}

/// Whether `expr` names storage the program still reaches by another route -
/// a local, or a field of one. The value such an expression answers is an
/// `Arc` share of that storage, so a binding taken from it copies the way a
/// binding from a bare local does.
fn is_place_expr(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Path { .. } => true,
        // An index names storage its container owns exactly as a field names
        // storage its struct owns, so `xs[i]` and `xs[i].m` are places the way
        // `s.m` and `t.0` are.
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            is_place_expr(receiver)
        }
        HirExprKind::Index { base, .. } => is_place_expr(base),
        _ => false,
    }
}

fn reference_operand_local_name(expr: &HirExpr) -> Option<&str> {
    let HirExprKind::Path { segments, .. } = &expr.kind else {
        return None;
    };
    let [segment] = segments.as_slice() else {
        return None;
    };
    Some(segment.name.as_str())
}

/// Strips leading module-relative prefix segments (`super`, `crate`,
/// `self`) from a path while more than one segment remains. The global
/// table is keyed by unqualified or module-joined names, so a
/// `super::foo` reference inside an inline `#[cfg(test)] mod tests`
/// resolves the flat parent-module item - matching the resolver's
/// flat-lookup strip (`resolver.rs`) and the walker's `eval_path`.
pub(crate) fn strip_module_relative(segments: &[Ident]) -> &[Ident] {
    let mut tail = segments;
    while tail.len() > 1 && matches!(tail[0].name.as_str(), "super" | "crate" | "self") {
        tail = &tail[1..];
    }
    tail
}

/// Maps a runtime [`Value`] back into the [`ConstKey`] used to
/// dedupe entries in the per-chunk constant pool. Falls back to a
/// disambiguating string key for shapes the pool doesn't model
/// (arrays, maps, etc.) - those still get pooled, but no two
/// inserts will ever collide because each call uses a fresh
/// formatter output.
fn const_key_for_value(value: &Value) -> ConstKey {
    match value {
        Value::Unit | Value::Void => ConstKey::Unit,
        Value::Bool(b) => ConstKey::Bool(*b),
        Value::Int(n) => ConstKey::Int(*n),
        Value::Float(f) => ConstKey::Float(f.to_bits()),
        Value::Char(c) => ConstKey::Char(*c),
        Value::String(s) => ConstKey::String(s.as_ref().to_string()),
        other => ConstKey::String(format!("__const_value_{other:?}")),
    }
}

fn literal_const(lit: &HirLiteral) -> (ConstKey, Value) {
    match lit {
        HirLiteral::Unit => (ConstKey::Unit, Value::Unit),
        HirLiteral::Bool(b) => (ConstKey::Bool(*b), Value::Bool(*b)),
        HirLiteral::Int(text) => {
            let value = parse_int(text).unwrap_or(0);
            (ConstKey::Int(value), Value::Int(value))
        }
        HirLiteral::Float(text) => {
            let parsed = float_literal_value(text);
            (ConstKey::Float(parsed.to_bits()), Value::Float(parsed))
        }
        HirLiteral::Char(c) => (ConstKey::Char(*c), Value::Char(*c)),
        HirLiteral::String(text) => (
            ConstKey::String(text.clone()),
            Value::String(SmolStr::from(std::sync::Arc::new(text.clone()))),
        ),
        HirLiteral::Byte(b) => (ConstKey::Int(i64::from(*b)), Value::Int(i64::from(*b))),
        HirLiteral::ByteString(bytes) => {
            let parts = bytes.iter().map(|b| Value::Int(i64::from(*b))).collect();
            (
                ConstKey::String(format!("bytes:{bytes:?}")),
                Value::Array(std::sync::Arc::new(parts)),
            )
        }
    }
}

/// `true` when `pat` introduces any name binding (a plain binding,
/// an `@`-binding, or a struct-field shorthand). The or-pattern
/// lowering uses this to decide whether its alternatives need a
/// shared destination-register set or are pure tests.
fn pattern_has_binding(pat: &HirPat) -> bool {
    match &pat.kind {
        HirPatKind::Wildcard
        | HirPatKind::Rest
        | HirPatKind::Literal(_)
        | HirPatKind::Range { .. } => false,
        HirPatKind::Binding { .. } | HirPatKind::At { .. } => true,
        HirPatKind::Tuple(ps) | HirPatKind::Variant { fields: ps, .. } => {
            ps.iter().any(pattern_has_binding)
        }
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            prefix.iter().any(pattern_has_binding)
                || rest.as_deref().is_some_and(pattern_has_binding)
                || suffix.iter().any(pattern_has_binding)
        }
        HirPatKind::Struct { fields, .. } => fields
            .iter()
            .any(|f| f.pattern.as_ref().is_none_or(pattern_has_binding)),
        HirPatKind::Ref { inner, .. } => pattern_has_binding(inner),
        HirPatKind::Or(alts) => alts.iter().any(pattern_has_binding),
    }
}

/// Resolves a `[value; count]` count expression to an integer at
/// compile time. Only matches plain `i64` / `usize` literals so the
/// bytecode emitter can pre-allocate exactly `count` registers.
/// Other shapes (`const`-folded path, function call) fall back to
/// the deferred path.
fn resolve_const_count(expr: &HirExpr) -> Option<i64> {
    use gossamer_hir::{HirExprKind as H, HirLiteral as L};
    if let H::Literal(L::Int(s)) = &expr.kind {
        // The HIR preserves source-form integer literals; strip the
        // optional type suffix and underscore separators before parsing.
        let trimmed = s
            .trim_end_matches("i64")
            .trim_end_matches("usize")
            .trim_end_matches("u64")
            .trim_end_matches("isize")
            .trim_end_matches("u32")
            .trim_end_matches("i32");
        let cleaned: String = trimmed.chars().filter(|c| *c != '_').collect();
        if let Some(stripped) = cleaned.strip_prefix("0x") {
            return i64::from_str_radix(stripped, 16).ok();
        }
        if let Some(stripped) = cleaned.strip_prefix("0o") {
            return i64::from_str_radix(stripped, 8).ok();
        }
        if let Some(stripped) = cleaned.strip_prefix("0b") {
            return i64::from_str_radix(stripped, 2).ok();
        }
        return cleaned.parse::<i64>().ok();
    }
    None
}

/// Returns `true` when the array-shaped `array_ty`'s element type -
/// or, when typeck left the array's elem as a still-bound
/// `TyKind::Var`, the optional `value_ty` of the literal element -
/// matches `pred`. Used by every typed-storage `try_build_*`
/// builder to gate the fast path. Without the value-side fallback,
/// `let mut perm: [i64; 16] = [0; 16]` and `let mut u: [f64; 6000] =
/// [1.0; 6000]` failed to specialise: typeck records the element
/// type on the binding annotation, but the array literal's
/// `Ty::Array { elem }` keeps a fresh inference var that
/// `default_unresolved_int_vars` resolves only inside the `InferCtxt` -
/// never substituted back into the HIR handle compile.rs reads.
fn is_array_elem_kind(
    tcx: &TyCtxt,
    array_ty: Ty,
    value_ty: Option<Ty>,
    pred: impl Fn(&TyKind) -> bool,
) -> bool {
    let array_elem = match tcx.kind(array_ty) {
        Some(TyKind::Array { elem, .. } | TyKind::Vec(elem) | TyKind::Slice(elem)) => Some(*elem),
        _ => None,
    };
    if let Some(t) = array_elem {
        if let Some(k) = tcx.kind(t) {
            if pred(k) {
                return true;
            }
        }
    }
    if let Some(t) = value_ty {
        if let Some(k) = tcx.kind(t) {
            if pred(k) {
                return true;
            }
        }
    }
    false
}

fn parse_int(text: &str) -> Option<i64> {
    let cleaned = strip_int_suffix(text).replace('_', "");
    if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        // Try signed first; fall back to unsigned reinterpret for
        // bit patterns that overflow i64 (e.g. 0xFFFFFFFFFFFFFFFF).
        return i64::from_str_radix(rest, 16)
            .ok()
            .or_else(|| u64::from_str_radix(rest, 16).ok().map(|n| n as i64));
    }
    if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        return i64::from_str_radix(rest, 2)
            .ok()
            .or_else(|| u64::from_str_radix(rest, 2).ok().map(|n| n as i64));
    }
    if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        return i64::from_str_radix(rest, 8)
            .ok()
            .or_else(|| u64::from_str_radix(rest, 8).ok().map(|n| n as i64));
    }
    // For decimal, try signed parse first, then unsigned reinterpret
    // for values in [2^63, 2^64) such as u64::MAX or i64::MIN's
    // magnitude (9223372036854775808).
    cleaned
        .parse::<i64>()
        .ok()
        .or_else(|| cleaned.parse::<u64>().ok().map(|n| n as i64))
}

fn strip_int_suffix(text: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "i128", "u128", "isize", "usize", "i64", "u64", "i32", "u32", "i16", "u16", "i8", "u8",
    ];
    for suffix in SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    text.to_string()
}

/// Numeric value of a float literal's source text, with its width suffix
/// and any `_` visual separators removed.
pub(crate) fn float_literal_value(text: &str) -> f64 {
    let mut body = text;
    for suffix in &["f32", "f64"] {
        if let Some(stripped) = text.strip_suffix(suffix) {
            body = stripped;
            break;
        }
    }
    body.replace('_', "").parse::<f64>().unwrap_or(0.0)
}

/// Detects `m.insert(k, m.get_or(k, 0) + by)`. Returns `(key, by)`
/// (borrowed from the original HIR) when the surrounding insert
/// receiver and key match the inner `get_or`'s receiver and key
/// structurally and the inner default arg is literal `0`.
pub(crate) fn match_map_inc_pattern<'a>(
    receiver: &'a HirExpr,
    insert_key: &'a HirExpr,
    insert_value: &'a HirExpr,
) -> Option<(&'a HirExpr, &'a HirExpr)> {
    let HirExprKind::Binary { op, lhs, rhs } = &insert_value.kind else {
        return None;
    };
    if !matches!(op, HirBinaryOp::Add) {
        return None;
    }
    if is_get_or_zero(receiver, insert_key, lhs) {
        return Some((insert_key, rhs));
    }
    if is_get_or_zero(receiver, insert_key, rhs) {
        return Some((insert_key, lhs));
    }
    None
}

fn is_get_or_zero(receiver: &HirExpr, key: &HirExpr, candidate: &HirExpr) -> bool {
    let HirExprKind::MethodCall {
        receiver: inner_recv,
        name: inner_name,
        args: inner_args,
        owner: None,
    } = &candidate.kind
    else {
        return false;
    };
    inner_name.name == "get_or"
        && inner_args.len() == 2
        && exprs_equiv(receiver, inner_recv)
        && exprs_equiv(key, &inner_args[0])
        && is_zero_literal(&inner_args[1])
}

/// Structural equivalence over the HIR shapes that can safely be
/// re-evaluated zero times (i.e. compiled once and reused for both
/// the outer `insert` and the elided inner `get_or`). Limited to
/// pure single-segment `Path` reads and primitive literals so we
/// never elide a side-effecting expression.
fn exprs_equiv(a: &HirExpr, b: &HirExpr) -> bool {
    match (&a.kind, &b.kind) {
        (HirExprKind::Path { segments: sa, .. }, HirExprKind::Path { segments: sb, .. }) => {
            sa.len() == sb.len() && sa.iter().zip(sb).all(|(x, y)| x.name == y.name)
        }
        (HirExprKind::Literal(la), HirExprKind::Literal(lb)) => literals_equal(la, lb),
        _ => false,
    }
}

fn literals_equal(a: &HirLiteral, b: &HirLiteral) -> bool {
    match (a, b) {
        (HirLiteral::Int(x), HirLiteral::Int(y)) => x == y,
        (HirLiteral::Float(x), HirLiteral::Float(y)) => x == y,
        (HirLiteral::String(x), HirLiteral::String(y)) => x == y,
        (HirLiteral::Char(x), HirLiteral::Char(y)) => x == y,
        (HirLiteral::Byte(x), HirLiteral::Byte(y)) => x == y,
        (HirLiteral::ByteString(x), HirLiteral::ByteString(y)) => x == y,
        (HirLiteral::Bool(x), HirLiteral::Bool(y)) => x == y,
        (HirLiteral::Unit, HirLiteral::Unit) => true,
        _ => false,
    }
}

fn is_zero_literal(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Literal(HirLiteral::Int(text)) => parse_int(text) == Some(0),
        _ => false,
    }
}
