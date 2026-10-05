//! Calls to functions declared in an `unsafe extern "C"` block, bracketed
//! and marshalled exactly as the LLVM backend's `lower/foreign.rs` does.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use cranelift_codegen::ir::{self, AbiParam, ArgumentPurpose, InstBuilder, Signature, types};
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use gossamer_abi::c_aggregate::{ArgPlan, CAbi, CallPlan, Piece, PieceKind, RetPlan};
use gossamer_mir::{
    Body, ForeignCall, ForeignCallback, ForeignParam, ForeignStatic, Local, Operand, Rvalue,
    StatementKind,
};
use gossamer_types::TyCtxt;

use super::{IntrinsicContext, coerce_arg_to, define_var_to, lower_operand, value_type};

/// The Cranelift type of a C class, and whether a narrow integer of the
/// class is sign-extended. `None` for `v`.
fn c_type(class: char) -> Result<Option<(ir::Type, bool)>> {
    Ok(Some(match class {
        'c' => (types::I8, true),
        'C' | 'B' => (types::I8, false),
        'h' => (types::I16, true),
        'H' => (types::I16, false),
        'i' => (types::I32, true),
        'I' => (types::I32, false),
        'l' => (types::I64, true),
        'L' => (types::I64, false),
        'f' => (types::F32, false),
        'd' => (types::F64, false),
        'v' => return Ok(None),
        other => return Err(anyhow!("native codegen: unknown foreign class `{other}`")),
    }))
}

fn abi_param(ty: ir::Type, signed: bool) -> AbiParam {
    let param = AbiParam::new(ty);
    if ty == types::I8 || ty == types::I16 || ty == types::I32 {
        if signed { param.sext() } else { param.uext() }
    } else {
        param
    }
}

/// The calling convention by-value structs follow. This lowering builds the
/// JIT's code, which runs in this process, so the convention is the host's.
fn c_abi() -> Result<CAbi> {
    let arch = std::env::consts::ARCH;
    let os = if cfg!(windows) { "windows" } else { "unix" };
    CAbi::for_target(arch, os)
        .ok_or_else(|| anyhow!("native codegen: no lowering for a struct by value on {arch}"))
}

/// The call plan of `call` when it moves a struct by value or answers one.
pub(crate) fn struct_plan(call: ForeignCall<'_>) -> Result<Option<CallPlan>> {
    let any_struct = call.ret_layout.is_some()
        || call
            .param_list()
            .iter()
            .any(|param| matches!(param, ForeignParam::ByValue(_)));
    if !any_struct {
        return Ok(None);
    }
    Ok(Some(call.plan(c_abi()?)))
}

/// The Cranelift type of a struct piece, and whether a narrow integer
/// sign-extends.
pub(crate) fn piece_type(kind: PieceKind) -> (ir::Type, bool) {
    match kind {
        PieceKind::Int { bytes: 1, signed } => (types::I8, signed),
        PieceKind::Int { bytes: 2, signed } => (types::I16, signed),
        PieceKind::Int { bytes: 4, signed } => (types::I32, signed),
        PieceKind::Int { signed, .. } => (types::I64, signed),
        PieceKind::F32 => (types::F32, false),
        PieceKind::F64 => (types::F64, false),
    }
}

/// The parameters a struct argument travels as under `plan`.
pub(crate) fn struct_params(plan: &ArgPlan, pointer: ir::Type) -> Vec<AbiParam> {
    match plan {
        ArgPlan::Scalar => Vec::new(),
        ArgPlan::Pieces {
            pad_int,
            pad_float,
            pieces,
        } => std::iter::repeat_n(AbiParam::new(types::I64), usize::from(*pad_int))
            .chain(std::iter::repeat_n(
                AbiParam::new(types::F64),
                usize::from(*pad_float),
            ))
            .chain(pieces.iter().map(|piece| {
                let (ty, signed) = piece_type(piece.kind);
                abi_param(ty, signed)
            }))
            .collect(),
        ArgPlan::Memory { size, .. } => vec![AbiParam::special(
            pointer,
            ArgumentPurpose::StructArgument(size.next_multiple_of(8)),
        )],
        ArgPlan::Indirect => vec![AbiParam::new(pointer)],
    }
}

/// The C signature of `call` on `module`'s target.
fn signature(module: &dyn Module, call: ForeignCall<'_>) -> Result<Signature> {
    let pointer = module.target_config().pointer_type();
    let plan = struct_plan(call)?;
    let mut sig = module.make_signature();
    if plan.as_ref().is_some_and(CallPlan::has_sret) {
        sig.params
            .push(AbiParam::special(pointer, ArgumentPurpose::StructReturn));
    }
    for (index, param) in call.param_list().into_iter().enumerate() {
        match param {
            ForeignParam::Scalar(class) => {
                let (ty, signed) =
                    c_type(class)?.ok_or_else(|| anyhow!("`void` is not a parameter class"))?;
                sig.params.push(abi_param(ty, signed));
            }
            ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => {
                sig.params.push(AbiParam::new(pointer));
            }
            ForeignParam::ByValue(_) => {
                let arg_plan = plan
                    .as_ref()
                    .and_then(|plan| plan.params.get(index))
                    .ok_or_else(|| anyhow!("native codegen: struct argument without a plan"))?;
                sig.params.extend(struct_params(arg_plan, pointer));
            }
        }
    }
    match plan.as_ref().map(|plan| &plan.ret) {
        Some(RetPlan::Pieces(pieces)) => {
            for piece in pieces {
                let (ty, signed) = piece_type(piece.kind);
                sig.returns.push(abi_param(ty, signed));
            }
        }
        Some(RetPlan::Sret) => {}
        _ => {
            if call.ret != 's'
                && let Some((ty, signed)) = c_type(call.ret)?
            {
                sig.returns.push(abi_param(ty, signed));
            }
        }
    }
    Ok(sig)
}

/// The values a struct at `buffer` travels as under `plan`.
pub(crate) fn struct_values(
    builder: &mut FunctionBuilder<'_>,
    plan: &ArgPlan,
    buffer: ir::Value,
) -> Vec<ir::Value> {
    match plan {
        ArgPlan::Scalar => Vec::new(),
        ArgPlan::Pieces {
            pad_int,
            pad_float,
            pieces,
        } => {
            let mut values = Vec::new();
            // Dummy arguments use up the registers the convention retires
            // for a struct that does not fit in what is left.
            for _ in 0..*pad_int {
                values.push(builder.ins().iconst(types::I64, 0));
            }
            for _ in 0..*pad_float {
                values.push(builder.ins().f64const(0.0));
            }
            for piece in pieces {
                let (ty, _) = piece_type(piece.kind);
                let offset = i32::try_from(piece.offset).unwrap_or(0);
                values.push(
                    builder
                        .ins()
                        .load(ty, ir::MemFlagsData::new(), buffer, offset),
                );
            }
            values
        }
        ArgPlan::Memory { .. } | ArgPlan::Indirect => vec![buffer],
    }
}

/// Stores a result returned as `pieces` into the struct buffer `holder`.
pub(crate) fn store_ret_pieces(
    builder: &mut FunctionBuilder<'_>,
    pieces: &[Piece],
    results: &[ir::Value],
    holder: ir::Value,
) {
    for (piece, value) in pieces.iter().zip(results) {
        let offset = i32::try_from(piece.offset).unwrap_or(0);
        builder
            .ins()
            .store(ir::MemFlagsData::new(), *value, holder, offset);
    }
}

/// A declaration of `symbol` the module already holds whose parameter and
/// result types are `sig`'s, such as a runtime export the ABI registry
/// declared without the extension attributes a foreign signature carries.
/// The call passes values of those types either way, so it reuses that one.
fn existing_declaration(module: &dyn Module, symbol: &str, sig: &Signature) -> Option<FuncId> {
    let cranelift_module::FuncOrDataId::Func(id) = module.declarations().get_name(symbol)? else {
        return None;
    };
    let declared = &module.declarations().get_function_decl(id).signature;
    let types = |params: &[AbiParam]| params.iter().map(|p| p.value_type).collect::<Vec<_>>();
    (types(&declared.params) == types(&sig.params)
        && types(&declared.returns) == types(&sig.returns))
    .then_some(id)
}

/// Declares the C function behind every foreign-call intrinsic in `bodies`,
/// which the parallel lowering phase cannot do.
pub(super) fn declare_foreign_calls(
    module: &mut dyn Module,
    bodies: &[Body],
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    for body in bodies {
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                    ..
                } = &stmt.kind
                else {
                    continue;
                };
                if let Some(global) = ForeignStatic::parse(name) {
                    if !intrinsics.foreign_data.contains_key(name) {
                        let id = module
                            .declare_data(global.symbol, Linkage::Import, true, false)
                            .map_err(|e| {
                                anyhow!("declare foreign static `{}`: {e}", global.symbol)
                            })?;
                        intrinsics.foreign_data.insert(name, id);
                    }
                    continue;
                }
                if let Some(callback) = ForeignCallback::parse(name) {
                    if !intrinsics.foreign.contains_key(name) {
                        let id = define_callback_shim(module, callback, intrinsics)?;
                        intrinsics.foreign.insert(name, id);
                    }
                    continue;
                }
                let Some(call) = ForeignCall::parse(name) else {
                    continue;
                };
                if intrinsics.foreign.contains_key(name) || call.is_indirect() {
                    continue;
                }
                let sig = signature(module, call)?;
                let id = match existing_declaration(module, call.symbol, &sig) {
                    Some(id) => id,
                    None => module
                        .declare_function(call.symbol, Linkage::Import, &sig)
                        .map_err(|e| anyhow!("declare foreign function `{}`: {e}", call.symbol))?,
                };
                intrinsics.foreign.insert(name, id);
            }
        }
    }
    Ok(())
}

/// Lowers the foreign-call intrinsic `name` into `destination`.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_foreign_call(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    name: &str,
    destination: Local,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    let call = ForeignCall::parse(name)
        .ok_or_else(|| anyhow!("native codegen: malformed foreign call `{name}`"))?;
    let pointer = module.target_config().pointer_type();
    // A call through an address takes its target from the first operand.
    let (target, args) = if call.is_indirect() {
        let (address, rest) = args
            .split_first()
            .ok_or_else(|| anyhow!("native codegen: indirect foreign call without a target"))?;
        let value = lower_operand(
            module,
            builder,
            locals,
            body,
            tcx,
            address,
            Some(pointer),
            intrinsics,
        )?;
        (Some(coerce_arg_to(builder, value, pointer)?), rest)
    } else {
        (None, args)
    };
    let plan = struct_plan(call)?;
    // A struct result comes back through the final operand, the buffer it is
    // written to.
    let (args, holder) = if call.ret_layout.is_some() {
        let (holder, rest) = args
            .split_last()
            .ok_or_else(|| anyhow!("native codegen: struct result without its buffer"))?;
        let value = lower_operand(
            module,
            builder,
            locals,
            body,
            tcx,
            holder,
            Some(pointer),
            intrinsics,
        )?;
        (rest, Some(coerce_arg_to(builder, value, pointer)?))
    } else {
        (args, None)
    };
    let mut values = Vec::with_capacity(args.len());
    if plan.as_ref().is_some_and(CallPlan::has_sret)
        && let Some(holder) = holder
    {
        values.push(holder);
    }
    let mut buffers = Vec::new();
    for (index, (param, arg)) in call.param_list().into_iter().zip(args).enumerate() {
        let value = match param {
            ForeignParam::Scalar(class) => {
                let (ty, _) =
                    c_type(class)?.ok_or_else(|| anyhow!("`void` is not a parameter class"))?;
                let value = lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(ty),
                    intrinsics,
                )?;
                narrow(builder, value, ty)?
            }
            ForeignParam::Slice { elem, writable } => {
                let value = lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(pointer),
                    intrinsics,
                )?;
                let vec = coerce_arg_to(builder, value, pointer)?;
                let class = builder.ins().iconst(types::I64, i64::from(u32::from(elem)));
                let begin = intrinsics.extern_fn_by_name(module, "gos_rt_ffi_buf_begin")?;
                let begin = module.declare_func_in_func(begin, builder.func);
                let inst = builder.ins().call(begin, &[vec, class]);
                let elements = builder.inst_results(inst)[0];
                buffers.push((vec, elements, i64::from(writable), class));
                elements
            }
            ForeignParam::Struct { .. } => {
                let value = lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(pointer),
                    intrinsics,
                )?;
                coerce_arg_to(builder, value, pointer)?
            }
            ForeignParam::ByValue(_) => {
                let value = lower_operand(
                    module,
                    builder,
                    locals,
                    body,
                    tcx,
                    arg,
                    Some(pointer),
                    intrinsics,
                )?;
                let buffer = coerce_arg_to(builder, value, pointer)?;
                let arg_plan = plan
                    .as_ref()
                    .and_then(|plan| plan.params.get(index))
                    .ok_or_else(|| anyhow!("native codegen: struct argument without a plan"))?;
                values.extend(struct_values(builder, arg_plan, buffer));
                continue;
            }
        };
        values.push(value);
    }
    let ret = if call.ret == 's' {
        None
    } else {
        c_type(call.ret)?
    };
    let enter = intrinsics.extern_fn_by_name(module, "gos_rt_ffi_enter")?;
    let enter = module.declare_func_in_func(enter, builder.func);
    builder.ins().call(enter, &[]);
    let inst = match target {
        Some(target) => {
            let sig = signature(module, call)?;
            let sig_ref = builder.import_signature(sig);
            builder.ins().call_indirect(sig_ref, target, &values)
        }
        None => {
            let callee = *intrinsics.foreign.get(name).ok_or_else(|| {
                anyhow!(
                    "native codegen: foreign function `{}` not declared",
                    call.symbol
                )
            })?;
            let callee = module.declare_func_in_func(callee, builder.func);
            builder.ins().call(callee, &values)
        }
    };
    let result = ret.map(|_| builder.inst_results(inst)[0]);
    let returned: Vec<ir::Value> = builder.inst_results(inst).to_vec();
    let leave = intrinsics.extern_fn_by_name(module, "gos_rt_ffi_leave")?;
    let leave = module.declare_func_in_func(leave, builder.func);
    builder.ins().call(leave, &[]);
    if let (Some(RetPlan::Pieces(pieces)), Some(holder)) =
        (plan.as_ref().map(|plan| &plan.ret), holder)
    {
        store_ret_pieces(builder, pieces, &returned, holder);
    }
    for (vec, elements, writable, class) in buffers {
        let end = intrinsics.extern_fn_by_name(module, "gos_rt_ffi_buf_end")?;
        let end = module.declare_func_in_func(end, builder.func);
        let writable = builder.ins().iconst(types::I64, writable);
        builder.ins().call(end, &[vec, elements, writable, class]);
    }

    let value = match (result, ret) {
        (None, _) | (_, None) => builder.ins().iconst(types::I64, 0),
        (Some(value), Some((ty, signed))) => {
            let want = intrinsics
                .body_cl_types
                .get(destination.0 as usize)
                .copied()
                .flatten()
                .unwrap_or(types::I64);
            widen(builder, value, ty, want, signed)
        }
    };
    define_var_to(
        builder,
        locals,
        &intrinsics.body_cl_types,
        destination,
        value,
    );
    Ok(())
}

/// `value` narrowed to the C parameter type `ty`.
fn narrow(builder: &mut FunctionBuilder<'_>, value: ir::Value, ty: ir::Type) -> Result<ir::Value> {
    let have = value_type(value, builder);
    if have == types::F64 && ty == types::F32 {
        return Ok(builder.ins().fdemote(types::F32, value));
    }
    if have.is_int() && ty.is_int() && have.bits() > ty.bits() {
        return Ok(builder.ins().ireduce(ty, value));
    }
    if have.is_int() && ty.is_int() && have.bits() < ty.bits() {
        return Ok(builder.ins().uextend(ty, value));
    }
    coerce_arg_to(builder, value, ty)
}

/// A C result of type `from` as the destination's type `want`.
fn widen(
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    from: ir::Type,
    want: ir::Type,
    signed: bool,
) -> ir::Value {
    if from == types::F32 && want == types::F64 {
        return builder.ins().fpromote(types::F64, value);
    }
    if from == want || !from.is_int() || !want.is_int() {
        return value;
    }
    if want.bits() < from.bits() {
        if want == types::I8 && from == types::I8 {
            return value;
        }
        return builder.ins().ireduce(want, value);
    }
    if signed {
        builder.ins().sextend(want, value)
    } else {
        builder.ins().uextend(want, value)
    }
}

/// Lowers the foreign-static intrinsic `name`: the address of the C global
/// [`declare_foreign_calls`] declared for it.
pub(super) fn lower_foreign_static(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    name: &str,
    destination: Local,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    let data = *intrinsics
        .foreign_data
        .get(name)
        .ok_or_else(|| anyhow!("native codegen: foreign static `{name}` not declared"))?;
    let pointer = module.target_config().pointer_type();
    let global = module.declare_data_in_func(data, builder.func);
    let address = builder.ins().symbol_value(pointer, global);
    let want = intrinsics
        .body_cl_types
        .get(destination.0 as usize)
        .copied()
        .flatten()
        .unwrap_or(types::I64);
    let value = coerce_arg_to(builder, address, want)?;
    define_var_to(
        builder,
        locals,
        &intrinsics.body_cl_types,
        destination,
        value,
    );
    Ok(())
}

/// Lowers the callback intrinsic `name`: the address of the C-ABI entry
/// [`define_callback_shim`] defined for it.
pub(super) fn lower_foreign_callback(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    name: &str,
    destination: Local,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    let shim = *intrinsics
        .foreign
        .get(name)
        .ok_or_else(|| anyhow!("native codegen: callback entry `{name}` not defined"))?;
    let pointer = module.target_config().pointer_type();
    let shim = module.declare_func_in_func(shim, builder.func);
    let address = builder.ins().func_addr(pointer, shim);
    let want = intrinsics
        .body_cl_types
        .get(destination.0 as usize)
        .copied()
        .flatten()
        .unwrap_or(types::I64);
    let value = coerce_arg_to(builder, address, want)?;
    define_var_to(
        builder,
        locals,
        &intrinsics.body_cl_types,
        destination,
        value,
    );
    Ok(())
}

/// The C signature of a callback entry under `plan` (when it moves a struct
/// by value) on `module`'s target.
fn callback_signature(
    module: &dyn Module,
    callback: ForeignCallback<'_>,
    plan: Option<&CallPlan>,
) -> Result<Signature> {
    let pointer = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    if plan.is_some_and(CallPlan::has_sret) {
        sig.params
            .push(AbiParam::special(pointer, ArgumentPurpose::StructReturn));
    }
    for (index, param) in callback.param_list().into_iter().enumerate() {
        match param {
            ForeignParam::ByValue(_) => {
                let arg_plan = plan
                    .and_then(|plan| plan.params.get(index))
                    .ok_or_else(|| {
                        anyhow!("native codegen: struct callback argument without a plan")
                    })?;
                sig.params.extend(struct_params(arg_plan, pointer));
            }
            ForeignParam::Scalar(class) => {
                let (ty, signed) =
                    c_type(class)?.ok_or_else(|| anyhow!("`void` is not a parameter class"))?;
                sig.params.push(abi_param(ty, signed));
            }
            ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => {
                return Err(anyhow!(
                    "native codegen: a callback takes scalars and structs"
                ));
            }
        }
    }
    match plan.map(|plan| &plan.ret) {
        Some(RetPlan::Pieces(pieces)) => {
            for piece in pieces {
                let (ty, signed) = piece_type(piece.kind);
                sig.returns.push(abi_param(ty, signed));
            }
        }
        Some(RetPlan::Sret) => {}
        _ => {
            if let Some((ty, signed)) = c_type(callback.ret)? {
                sig.returns.push(abi_param(ty, signed));
            }
        }
    }
    Ok(sig)
}

/// Rebuilds struct argument `plan` from the entry block's parameters at
/// `params` (advancing past them) and answers the address of its bytes: the
/// pieces are stored into a stack slot, a stack or indirect copy is used
/// where it lies.
pub(crate) fn receive_struct(
    builder: &mut FunctionBuilder<'_>,
    plan: &ArgPlan,
    size: u32,
    params: &mut impl Iterator<Item = ir::Value>,
    pointer: ir::Type,
) -> ir::Value {
    match plan {
        ArgPlan::Pieces {
            pad_int,
            pad_float,
            pieces,
        } => {
            for _ in 0..usize::from(*pad_int) + usize::from(*pad_float) {
                params.next();
            }
            let slot = builder.create_sized_stack_slot(ir::StackSlotData::new(
                ir::StackSlotKind::ExplicitSlot,
                size.max(8).next_multiple_of(8),
                3,
            ));
            for piece in pieces {
                if let Some(value) = params.next() {
                    let offset = i32::try_from(piece.offset).unwrap_or(0);
                    builder.ins().stack_store(pointer, value, slot, offset);
                }
            }
            builder.ins().stack_addr(pointer, slot, 0)
        }
        ArgPlan::Memory { .. } | ArgPlan::Indirect | ArgPlan::Scalar => params
            .next()
            .unwrap_or_else(|| builder.ins().iconst(pointer, 0)),
    }
}

/// Defines the C-ABI entry for `callback`: a function with its C signature
/// that stores each argument as a word (an integer extended by its class, a
/// float as the bits of a `double`, a struct as the address of its bytes),
/// runs the adapter through `gos_rt_ffi_callback_run`, and answers the result
/// word narrowed to the C result, or a struct result the adapter wrote.
fn define_callback_shim(
    module: &mut dyn Module,
    callback: ForeignCallback<'_>,
    intrinsics: &mut IntrinsicContext,
) -> Result<FuncId> {
    use cranelift_codegen::Context;
    use cranelift_frontend::FunctionBuilderContext;
    use cranelift_module::DataDescription;

    let adapter = *intrinsics.functions.get(callback.adapter).ok_or_else(|| {
        anyhow!(
            "native codegen: callback adapter `{}` is not in this module",
            callback.adapter
        )
    })?;
    let pointer = module.target_config().pointer_type();
    let plan = if callback.has_struct() {
        Some(callback.plan(c_abi()?))
    } else {
        None
    };
    let shim_sig = callback_signature(module, callback, plan.as_ref())?;
    let param_list = callback.param_list();
    let ret_struct = callback
        .ret_layout
        .and_then(gossamer_abi::c_aggregate::CLayout::parse);
    let safe = |text: &str| -> String {
        text.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    let suffix = format!(
        "{}_{}{}",
        safe(callback.params),
        callback.ret,
        safe(callback.ret_layout.unwrap_or_default())
    );
    let shim_name = format!("__gos_ffi_shim_{}_{suffix}", callback.adapter);
    let shim = module
        .declare_function(&shim_name, Linkage::Local, &shim_sig)
        .map_err(|e| anyhow!("declare {shim_name}: {e}"))?;
    let label_name = format!("__gos_ffi_cbname_{}_{suffix}", callback.adapter);
    let label = module
        .declare_data(&label_name, Linkage::Local, false, false)
        .map_err(|e| anyhow!("declare {label_name}: {e}"))?;
    let mut text = callback.name.as_bytes().to_vec();
    text.push(0);
    let mut description = DataDescription::new();
    description.define(text.into_boxed_slice());
    module
        .define_data(label, &description)
        .map_err(|e| anyhow!("define {label_name}: {e}"))?;
    let run = intrinsics.extern_fn_by_name(module, "gos_rt_ffi_callback_run")?;

    let mut func = cranelift_codegen::ir::Function::with_name_signature(
        cranelift_codegen::ir::UserFuncName::user(0, shim.as_u32()),
        shim_sig,
    );
    let mut fctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut func, &mut fctx);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        b.seal_block(block);
        let word_count = param_list.len() + usize::from(ret_struct.is_some());
        let size = u32::try_from(word_count.max(1) * 8)
            .map_err(|_| anyhow!("too many callback parameters"))?;
        let words = b.create_sized_stack_slot(ir::StackSlotData::new(
            ir::StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        let mut incoming = b.block_params(block).to_vec().into_iter();
        let sret = if plan.as_ref().is_some_and(CallPlan::has_sret) {
            incoming.next()
        } else {
            None
        };
        for (index, param) in param_list.iter().enumerate() {
            let word = match param {
                ForeignParam::ByValue(layout) => {
                    let arg_plan = plan
                        .as_ref()
                        .and_then(|plan| plan.params.get(index))
                        .ok_or_else(|| anyhow!("struct callback argument without a plan"))?;
                    let address =
                        receive_struct(&mut b, arg_plan, layout.size, &mut incoming, pointer);
                    if pointer == types::I64 {
                        address
                    } else {
                        b.ins().uextend(types::I64, address)
                    }
                }
                ForeignParam::Scalar(class) => {
                    let value = incoming
                        .next()
                        .ok_or_else(|| anyhow!("callback parameter missing"))?;
                    match class {
                        'f' => {
                            let wide = b.ins().fpromote(types::F64, value);
                            b.ins().bitcast(types::I64, ir::MemFlagsData::new(), wide)
                        }
                        'd' => b.ins().bitcast(types::I64, ir::MemFlagsData::new(), value),
                        'l' | 'L' => value,
                        class if gossamer_types::c_class_signed(*class) => {
                            b.ins().sextend(types::I64, value)
                        }
                        _ => b.ins().uextend(types::I64, value),
                    }
                }
                ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => {
                    return Err(anyhow!("a callback takes scalars and structs"));
                }
            };
            let offset =
                i32::try_from(index * 8).map_err(|_| anyhow!("too many callback parameters"))?;
            b.ins().stack_store(pointer, word, words, offset);
        }
        // A struct result is written to a buffer: the caller's for `sret`,
        // otherwise a stack slot here whose pieces are then returned.
        let result_buffer = match (&ret_struct, sret) {
            (Some(_), Some(sret)) => Some(sret),
            (Some(layout), None) => {
                let slot = b.create_sized_stack_slot(ir::StackSlotData::new(
                    ir::StackSlotKind::ExplicitSlot,
                    layout.size.max(8).next_multiple_of(8),
                    3,
                ));
                Some(b.ins().stack_addr(pointer, slot, 0))
            }
            (None, _) => None,
        };
        if let Some(buffer) = result_buffer {
            let word = if pointer == types::I64 {
                buffer
            } else {
                b.ins().uextend(types::I64, buffer)
            };
            let offset = i32::try_from(param_list.len() * 8)
                .map_err(|_| anyhow!("too many callback parameters"))?;
            b.ins().stack_store(pointer, word, words, offset);
        }
        let words_addr = b.ins().stack_addr(pointer, words, 0);
        let adapter_ref = module.declare_func_in_func(adapter, b.func);
        let adapter_addr = b.ins().func_addr(pointer, adapter_ref);
        let label_ref = module.declare_data_in_func(label, b.func);
        let label_addr = b.ins().symbol_value(pointer, label_ref);
        let run_ref = module.declare_func_in_func(run, b.func);
        let call = b
            .ins()
            .call(run_ref, &[adapter_addr, words_addr, label_addr]);
        let word = b.inst_results(call)[0];
        match (plan.as_ref().map(|plan| &plan.ret), result_buffer) {
            (Some(RetPlan::Pieces(pieces)), Some(buffer)) => {
                let mut values = Vec::with_capacity(pieces.len());
                for piece in pieces {
                    let (ty, _) = piece_type(piece.kind);
                    let offset = i32::try_from(piece.offset).unwrap_or(0);
                    values.push(b.ins().load(ty, ir::MemFlagsData::new(), buffer, offset));
                }
                b.ins().return_(&values);
            }
            (Some(RetPlan::Sret), _) => {
                b.ins().return_(&[]);
            }
            _ => match c_type(callback.ret)? {
                None => {
                    b.ins().return_(&[]);
                }
                Some((ty, _)) => {
                    let value = if ty == types::F64 {
                        b.ins().bitcast(types::F64, ir::MemFlagsData::new(), word)
                    } else if ty == types::F32 {
                        let wide = b.ins().bitcast(types::F64, ir::MemFlagsData::new(), word);
                        b.ins().fdemote(types::F32, wide)
                    } else if ty == types::I64 {
                        word
                    } else {
                        b.ins().ireduce(ty, word)
                    };
                    b.ins().return_(&[value]);
                }
            },
        }
        b.finalize(module.target_config());
    }
    let mut ctx = Context::for_function(func);
    module
        .define_function(shim, &mut ctx)
        .map_err(|e| anyhow!("define {shim_name}: {e}"))?;
    Ok(shim)
}
