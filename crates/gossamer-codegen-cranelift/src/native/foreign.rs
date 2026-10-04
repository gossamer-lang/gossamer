//! Calls to functions declared in an `unsafe extern "C"` block, bracketed
//! and marshalled exactly as the LLVM backend's `lower/foreign.rs` does.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use cranelift_codegen::ir::{self, AbiParam, InstBuilder, Signature, types};
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use gossamer_mir::{
    Body, ForeignCall, ForeignCallback, ForeignParam, Local, Operand, Rvalue, StatementKind,
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

/// The C signature of `call` on `module`'s target.
fn signature(module: &dyn Module, call: ForeignCall<'_>) -> Result<Signature> {
    let pointer = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    for param in call.param_list() {
        sig.params.push(match param {
            ForeignParam::Scalar(class) => {
                let (ty, signed) =
                    c_type(class)?.ok_or_else(|| anyhow!("`void` is not a parameter class"))?;
                abi_param(ty, signed)
            }
            ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => AbiParam::new(pointer),
        });
    }
    if let Some((ty, signed)) = c_type(call.ret)? {
        sig.returns.push(abi_param(ty, signed));
    }
    Ok(sig)
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
    let mut values = Vec::with_capacity(args.len());
    let mut buffers = Vec::new();
    for (param, arg) in call.param_list().into_iter().zip(args) {
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
        };
        values.push(value);
    }
    let ret = c_type(call.ret)?;
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
    let leave = intrinsics.extern_fn_by_name(module, "gos_rt_ffi_leave")?;
    let leave = module.declare_func_in_func(leave, builder.func);
    builder.ins().call(leave, &[]);
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

/// Defines the C-ABI entry for `callback`: a function with its C signature
/// that stores each argument as a word (an integer extended by its class, a
/// float as the bits of a `double`), runs the adapter through
/// `gos_rt_ffi_callback_run`, and answers the result word narrowed to the C
/// result.
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
    let mut shim_sig = module.make_signature();
    let classes: Vec<char> = callback.params.chars().collect();
    for class in &classes {
        let (ty, signed) =
            c_type(*class)?.ok_or_else(|| anyhow!("`void` is not a parameter class"))?;
        shim_sig.params.push(abi_param(ty, signed));
    }
    let ret = c_type(callback.ret)?;
    if let Some((ty, signed)) = ret {
        shim_sig.returns.push(abi_param(ty, signed));
    }
    let shim_name = format!(
        "__gos_ffi_shim_{}_{}_{}",
        callback.adapter, callback.params, callback.ret
    );
    let shim = module
        .declare_function(&shim_name, Linkage::Local, &shim_sig)
        .map_err(|e| anyhow!("declare {shim_name}: {e}"))?;
    let label_name = format!("__gos_ffi_cbname_{}_{}", callback.adapter, callback.params);
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
        let size = u32::try_from(classes.len().max(1) * 8)
            .map_err(|_| anyhow!("too many callback parameters"))?;
        let words = b.create_sized_stack_slot(ir::StackSlotData::new(
            ir::StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        let args: Vec<ir::Value> = b.block_params(block).to_vec();
        for (index, (value, class)) in args.iter().zip(&classes).enumerate() {
            let word = match class {
                'f' => {
                    let wide = b.ins().fpromote(types::F64, *value);
                    b.ins().bitcast(types::I64, ir::MemFlagsData::new(), wide)
                }
                'd' => b.ins().bitcast(types::I64, ir::MemFlagsData::new(), *value),
                'l' | 'L' => *value,
                class if gossamer_types::c_class_signed(*class) => {
                    b.ins().sextend(types::I64, *value)
                }
                _ => b.ins().uextend(types::I64, *value),
            };
            let offset =
                i32::try_from(index * 8).map_err(|_| anyhow!("too many callback parameters"))?;
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
        match ret {
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
        }
        b.finalize(module.target_config());
    }
    let mut ctx = Context::for_function(func);
    module
        .define_function(shim, &mut ctx)
        .map_err(|e| anyhow!("define {shim_name}: {e}"))?;
    Ok(shim)
}
