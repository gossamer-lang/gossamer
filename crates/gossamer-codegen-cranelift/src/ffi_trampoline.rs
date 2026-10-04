//! Call trampolines for foreign functions the bytecode VM calls.
//!
//! The VM holds a foreign call's arguments as 64-bit words and the callee as
//! an address, so it needs one routine per C signature that loads each word
//! into the register class its parameter takes, calls the address with the
//! target's C calling convention, and widens the result back to a word.
//! Cranelift generates that routine once per signature; the compiled tiers
//! call foreign functions directly and never use these.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use cranelift_codegen::ir::{self, AbiParam, InstBuilder, Signature, Type, types};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;

/// The C class of one parameter or of the result of a foreign function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FfiKind {
    /// `int8_t` (a signed byte).
    I8,
    /// `uint8_t` or `_Bool`.
    U8,
    /// `int16_t`.
    I16,
    /// `uint16_t`.
    U16,
    /// `int32_t` / `int`.
    I32,
    /// `uint32_t` / `unsigned`.
    U32,
    /// `int64_t` / `long` on LP64.
    I64,
    /// `uint64_t` / `size_t`.
    U64,
    /// `float`, whose word holds its bits in the low half.
    F32,
    /// `double`.
    F64,
    /// A data pointer.
    Ptr,
    /// No value (a result only).
    Void,
}

impl FfiKind {
    fn ir_type(self, pointer: Type) -> Option<Type> {
        match self {
            Self::I8 | Self::U8 => Some(types::I8),
            Self::I16 | Self::U16 => Some(types::I16),
            Self::I32 | Self::U32 => Some(types::I32),
            Self::I64 | Self::U64 => Some(types::I64),
            Self::F32 => Some(types::F32),
            Self::F64 => Some(types::F64),
            Self::Ptr => Some(pointer),
            Self::Void => None,
        }
    }
}

/// `trampoline(target, args) -> result`: calls the C function at `target`
/// with the words at `args` and answers its result as a word (an integer
/// sign- or zero-extended by its kind, a `double` by its bits, a `float` by
/// its bits in the low half, `0` for void).
pub type Trampoline = unsafe extern "C" fn(target: *const u8, args: *const u64) -> u64;

type Key = (Vec<FfiKind>, FfiKind);

/// Generated trampolines by signature. Each lives in a module kept for the
/// life of the process.
static TRAMPOLINES: Mutex<Option<HashMap<Key, usize>>> = Mutex::new(None);

/// The trampoline for a C function taking `params` and answering `ret`.
///
/// # Errors
///
/// Fails when the host ISA cannot be built or the routine does not compile.
pub fn trampoline(params: &[FfiKind], ret: FfiKind) -> Result<Trampoline> {
    let key = (params.to_vec(), ret);
    let mut cache = TRAMPOLINES
        .lock()
        .map_err(|_| anyhow!("trampoline cache poisoned"))?;
    let cache = cache.get_or_insert_with(HashMap::new);
    let addr = match cache.entry(key) {
        Entry::Occupied(found) => *found.get(),
        Entry::Vacant(slot) => *slot.insert(build(params, ret)?),
    };
    // SAFETY: `addr` is a finalised routine with the `Trampoline` signature,
    // in a module that is never freed.
    Ok(unsafe { std::mem::transmute::<usize, Trampoline>(addr) })
}

impl FfiKind {
    /// This kind as a C parameter or result, extended the way C promotes a
    /// narrow integer, as the platform ABIs that require it expect.
    fn abi_param(self, ty: Type) -> AbiParam {
        let param = AbiParam::new(ty);
        match self {
            Self::I8 | Self::I16 | Self::I32 => param.sext(),
            Self::U8 | Self::U16 | Self::U32 => param.uext(),
            _ => param,
        }
    }
}

/// The C signature of the function a trampoline calls.
fn callee_signature(
    call_conv: CallConv,
    pointer: Type,
    params: &[FfiKind],
    ret: FfiKind,
) -> Result<Signature> {
    let mut callee = Signature::new(call_conv);
    for kind in params {
        let ty = kind
            .ir_type(pointer)
            .ok_or_else(|| anyhow!("`void` is not a parameter type"))?;
        callee.params.push(kind.abi_param(ty));
    }
    if let Some(ty) = ret.ir_type(pointer) {
        callee.returns.push(ret.abi_param(ty));
    }
    Ok(callee)
}

/// The argument word at `offset` as the register class `kind` takes.
fn load_argument(
    b: &mut FunctionBuilder<'_>,
    args_ptr: ir::Value,
    offset: i32,
    kind: FfiKind,
    pointer: Type,
) -> ir::Value {
    let word = b
        .ins()
        .load(types::I64, ir::MemFlagsData::trusted(), args_ptr, offset);
    match kind {
        FfiKind::I8 | FfiKind::U8 => b.ins().ireduce(types::I8, word),
        FfiKind::I16 | FfiKind::U16 => b.ins().ireduce(types::I16, word),
        FfiKind::I32 | FfiKind::U32 => b.ins().ireduce(types::I32, word),
        FfiKind::F32 => {
            let bits = b.ins().ireduce(types::I32, word);
            b.ins().bitcast(types::F32, ir::MemFlagsData::new(), bits)
        }
        FfiKind::F64 => b.ins().bitcast(types::F64, ir::MemFlagsData::new(), word),
        FfiKind::Ptr if pointer != types::I64 => b.ins().ireduce(pointer, word),
        _ => word,
    }
}

/// The call's result widened to the word a trampoline answers.
fn result_word(
    b: &mut FunctionBuilder<'_>,
    call: ir::Inst,
    ret: FfiKind,
    pointer: Type,
) -> ir::Value {
    if ret == FfiKind::Void {
        return b.ins().iconst(types::I64, 0);
    }
    let value = b.inst_results(call)[0];
    match ret {
        FfiKind::I8 | FfiKind::I16 | FfiKind::I32 => b.ins().sextend(types::I64, value),
        FfiKind::U8 | FfiKind::U16 | FfiKind::U32 => b.ins().uextend(types::I64, value),
        FfiKind::F32 => {
            let bits = b.ins().bitcast(types::I32, ir::MemFlagsData::new(), value);
            b.ins().uextend(types::I64, bits)
        }
        FfiKind::F64 => b.ins().bitcast(types::I64, ir::MemFlagsData::new(), value),
        FfiKind::Ptr if pointer != types::I64 => b.ins().uextend(types::I64, value),
        _ => value,
    }
}

fn build(params: &[FfiKind], ret: FfiKind) -> Result<usize> {
    let isa = crate::native::build_native_isa(false, false)?;
    let pointer = isa.pointer_type();
    let call_conv = isa.default_call_conv();
    let mut module = JITModule::new(JITBuilder::with_isa(
        isa,
        cranelift_module::default_libcall_names(),
    ));

    let mut outer = Signature::new(call_conv);
    outer.params.push(AbiParam::new(pointer));
    outer.params.push(AbiParam::new(pointer));
    outer.returns.push(AbiParam::new(types::I64));
    let callee = callee_signature(call_conv, pointer, params, ret)?;

    let id = module
        .declare_anonymous_function(&outer)
        .map_err(|e| anyhow!("declare trampoline: {e}"))?;
    let mut ctx = module.make_context();
    ctx.func.signature = outer;
    let mut fctx = FunctionBuilderContext::new();
    let target_config = module.target_config();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        let target = b.block_params(entry)[0];
        let args_ptr = b.block_params(entry)[1];
        let mut args = Vec::with_capacity(params.len());
        for (index, kind) in params.iter().enumerate() {
            let offset = i32::try_from(index * 8).map_err(|_| anyhow!("too many arguments"))?;
            args.push(load_argument(&mut b, args_ptr, offset, *kind, pointer));
        }
        let sig_ref = b.import_signature(callee);
        let call = b.ins().call_indirect(sig_ref, target, &args);
        let result = result_word(&mut b, call, ret, pointer);
        b.ins().return_(&[result]);
        b.finalize(target_config);
    }
    module
        .define_function(id, &mut ctx)
        .map_err(|e| anyhow!("define trampoline: {e}"))?;
    module.clear_context(&mut ctx);
    module
        .finalize_definitions()
        .map_err(|e| anyhow!("finalize trampoline: {e}"))?;
    let addr = module.get_finalized_function(id) as usize;
    // The routine is called for the life of the process.
    std::mem::forget(module);
    Ok(addr)
}

/// `entry(slot, words) -> word`: what a reverse trampoline calls with the
/// callback's slot and the argument words it stored.
pub type CallbackEntry = extern "C" fn(slot: u64, words: *const u64) -> u64;

type ReverseKey = (Vec<FfiKind>, FfiKind, usize, u64);

/// Generated reverse trampolines by signature, entry, and slot.
static REVERSE: Mutex<Option<HashMap<ReverseKey, usize>>> = Mutex::new(None);

/// A C-ABI function taking `params` and answering `ret` that stores each
/// argument as a word (an integer extended by its kind, a float as the bits
/// of a `double`), calls `entry(slot, words)`, and answers the result word
/// narrowed to `ret` (a float result as the bits of a `double`). Native code
/// calls it as the callback it was handed.
///
/// # Errors
///
/// Fails when the host ISA cannot be built or the routine does not compile.
pub fn reverse_trampoline(
    params: &[FfiKind],
    ret: FfiKind,
    entry: CallbackEntry,
    slot: u64,
) -> Result<usize> {
    let key = (params.to_vec(), ret, entry as usize, slot);
    let mut cache = REVERSE
        .lock()
        .map_err(|_| anyhow!("reverse trampoline cache poisoned"))?;
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(addr) = cache.get(&key) {
        return Ok(*addr);
    }
    let addr = build_reverse(params, ret, entry as usize, slot)?;
    cache.insert(key, addr);
    Ok(addr)
}

/// The argument `value` of kind `kind` widened to the word it is stored as.
fn argument_word(
    b: &mut FunctionBuilder<'_>,
    value: ir::Value,
    kind: FfiKind,
    pointer: Type,
) -> ir::Value {
    match kind {
        FfiKind::I8 | FfiKind::I16 | FfiKind::I32 => b.ins().sextend(types::I64, value),
        FfiKind::U8 | FfiKind::U16 | FfiKind::U32 => b.ins().uextend(types::I64, value),
        FfiKind::F32 => {
            let wide = b.ins().fpromote(types::F64, value);
            b.ins().bitcast(types::I64, ir::MemFlagsData::new(), wide)
        }
        FfiKind::F64 => b.ins().bitcast(types::I64, ir::MemFlagsData::new(), value),
        FfiKind::Ptr if pointer != types::I64 => b.ins().uextend(types::I64, value),
        _ => value,
    }
}

/// The result `word` narrowed to the C result of kind `kind`.
fn result_from_word(
    b: &mut FunctionBuilder<'_>,
    word: ir::Value,
    kind: FfiKind,
    pointer: Type,
) -> Option<ir::Value> {
    Some(match kind {
        FfiKind::Void => return None,
        FfiKind::I8 | FfiKind::U8 => b.ins().ireduce(types::I8, word),
        FfiKind::I16 | FfiKind::U16 => b.ins().ireduce(types::I16, word),
        FfiKind::I32 | FfiKind::U32 => b.ins().ireduce(types::I32, word),
        FfiKind::F32 => {
            let wide = b.ins().bitcast(types::F64, ir::MemFlagsData::new(), word);
            b.ins().fdemote(types::F32, wide)
        }
        FfiKind::F64 => b.ins().bitcast(types::F64, ir::MemFlagsData::new(), word),
        FfiKind::Ptr if pointer != types::I64 => b.ins().ireduce(pointer, word),
        FfiKind::I64 | FfiKind::U64 | FfiKind::Ptr => word,
    })
}

fn build_reverse(params: &[FfiKind], ret: FfiKind, entry: usize, slot: u64) -> Result<usize> {
    let isa = crate::native::build_native_isa(false, false)?;
    let pointer = isa.pointer_type();
    let call_conv = isa.default_call_conv();
    let mut module = JITModule::new(JITBuilder::with_isa(
        isa,
        cranelift_module::default_libcall_names(),
    ));
    let outer = callee_signature(call_conv, pointer, params, ret)?;
    let mut entry_sig = Signature::new(call_conv);
    entry_sig.params.push(AbiParam::new(types::I64));
    entry_sig.params.push(AbiParam::new(pointer));
    entry_sig.returns.push(AbiParam::new(types::I64));

    let id = module
        .declare_anonymous_function(&outer)
        .map_err(|e| anyhow!("declare reverse trampoline: {e}"))?;
    let mut ctx = module.make_context();
    ctx.func.signature = outer;
    let mut fctx = FunctionBuilderContext::new();
    let target_config = module.target_config();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        b.seal_block(block);
        let size =
            u32::try_from(params.len().max(1) * 8).map_err(|_| anyhow!("too many arguments"))?;
        let words = b.create_sized_stack_slot(ir::StackSlotData::new(
            ir::StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        let args: Vec<ir::Value> = b.block_params(block).to_vec();
        for (index, (value, kind)) in args.iter().zip(params).enumerate() {
            let word = argument_word(&mut b, *value, *kind, pointer);
            let offset = i32::try_from(index * 8).map_err(|_| anyhow!("too many arguments"))?;
            b.ins().stack_store(pointer, word, words, offset);
        }
        let words_addr = b.ins().stack_addr(pointer, words, 0);
        let slot_value = b.ins().iconst(types::I64, slot as i64);
        let entry_addr = b.ins().iconst(pointer, entry as i64);
        let sig_ref = b.import_signature(entry_sig);
        let call = b
            .ins()
            .call_indirect(sig_ref, entry_addr, &[slot_value, words_addr]);
        let word = b.inst_results(call)[0];
        match result_from_word(&mut b, word, ret, pointer) {
            Some(value) => b.ins().return_(&[value]),
            None => b.ins().return_(&[]),
        };
        b.finalize(target_config);
    }
    module
        .define_function(id, &mut ctx)
        .map_err(|e| anyhow!("define reverse trampoline: {e}"))?;
    module.clear_context(&mut ctx);
    module
        .finalize_definitions()
        .map_err(|e| anyhow!("finalize reverse trampoline: {e}"))?;
    let addr = module.get_finalized_function(id) as usize;
    // Native code may call the routine for the life of the process.
    std::mem::forget(module);
    Ok(addr)
}
