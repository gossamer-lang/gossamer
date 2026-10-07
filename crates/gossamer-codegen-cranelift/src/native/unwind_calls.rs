//! Landing pads for bodies that register deferred expressions.
//!
//! MIR gives such a body a cleanup pad, which runs the pending deferred
//! expressions of the region a fault left and resumes the fault, and a note
//! pad, which records a panic one of those expressions raised and goes on with
//! the next. Cranelift code reaches them through catching calls: once a body
//! is built, every call in it becomes a call to a runtime shim that runs the
//! callee through a thunk under `catch_unwind` and answers whether a fault
//! unwound out of it, on which the body branches to the pad. The pad's
//! `Resume` hands the held fault back to the unwinder. A catching call works
//! on every target and unwinding scheme alike, since the runtime's own
//! `catch_unwind` is the catch.
//!
//! The new edges carry no block arguments: every local a pad's code names is
//! written to a stack slot after each statement that names it, and read back
//! where a pad begins, so a pad needs no value from the call that reached it.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use cranelift_codegen::Context;
use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    self, AbiParam, Function, InstBuilder, InstructionData, MemFlagsData, Signature, StackSlot,
    StackSlotData, StackSlotKind, UserFuncName, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use gossamer_mir::{BlockId, Body, Local, Terminator, UnwindPads};
use gossamer_types::TyCtxt;

/// The bytes each value takes in a catching call's buffer, enough for any
/// scalar, carrier, or vector a call passes.
const SLOT_BYTES: u32 = 16;

/// What lowering a body with landing pads keeps while it builds the body.
pub(super) struct UnwindLowering {
    pads: UnwindPads,
    /// The stack slot each local a pad names is kept in, with the type of
    /// the local's variable.
    spill: HashMap<Local, (StackSlot, ir::Type)>,
    /// The locals a `Call` terminator ending in a block writes, by the block
    /// the call continues in, where the write is first visible.
    written_on_entry: HashMap<BlockId, Vec<Local>>,
    /// The Cranelift blocks a pad's code was lowered into.
    pad_blocks: HashSet<ir::Block>,
    ptr_ty: ir::Type,
}

/// The landing pads of a built body, for [`rewrite_unwinding_calls`].
pub(crate) struct UnwindRewrite {
    cleanup_pad: ir::Block,
    note_pad: ir::Block,
    pad_blocks: HashSet<ir::Block>,
}

impl UnwindLowering {
    /// The landing-pad lowering of `body`, when it has pads. Declares a
    /// variable and a stack slot for every local a pad names, so each pad
    /// reads them however early it is lowered.
    pub(super) fn of(
        body: &Body,
        tcx: &TyCtxt,
        module: &dyn Module,
        builder: &mut FunctionBuilder<'_>,
        locals: &mut HashMap<Local, Variable>,
        body_cl_types: &[Option<ir::Type>],
    ) -> Option<Self> {
        let pads = UnwindPads::of(body)?;
        let mut named: Vec<Local> = pads.pad_locals(body).into_iter().collect();
        named.sort_by_key(|local| local.0);
        let mut spill = HashMap::with_capacity(named.len());
        for local in named {
            let var = super::ensure_var(builder, locals, body, tcx, module, body_cl_types, local);
            let current = builder.use_var(var);
            let ty = builder.func.dfg.value_type(current);
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                SLOT_BYTES,
                4,
            ));
            spill.insert(local, (slot, ty));
        }
        let mut written_on_entry: HashMap<BlockId, Vec<Local>> = HashMap::new();
        for block in &body.blocks {
            if let Terminator::Call {
                destination,
                target: Some(target),
                ..
            } = &block.terminator
                && spill.contains_key(&destination.local)
            {
                written_on_entry
                    .entry(*target)
                    .or_default()
                    .push(destination.local);
            }
        }
        Some(Self {
            pads,
            spill,
            written_on_entry,
            pad_blocks: HashSet::new(),
            ptr_ty: module.target_config().pointer_type(),
        })
    }

    /// Whether `block` is one of the pads' own code.
    pub(super) fn is_pad_code(&self, block: BlockId) -> bool {
        self.pads.pad_code.contains(&block)
    }

    /// Records the Cranelift blocks lowering `block` produced: its own and
    /// the ones created from index `first_new` on.
    pub(super) fn note_lowered(
        &mut self,
        block: BlockId,
        own: ir::Block,
        first_new: usize,
        func: &Function,
    ) {
        if !self.is_pad_code(block) {
            return;
        }
        self.pad_blocks.insert(own);
        let created = func.dfg.num_blocks();
        self.pad_blocks
            .extend((first_new..created).map(|index| ir::Block::from_u32(index as u32)));
    }

    /// Keeps the current value of each local in `written` that a pad names.
    pub(super) fn spill<'l>(
        &self,
        builder: &mut FunctionBuilder<'_>,
        locals: &HashMap<Local, Variable>,
        written: impl IntoIterator<Item = &'l Local>,
    ) {
        for local in written {
            let (Some(&(slot, _)), Some(&var)) = (self.spill.get(local), locals.get(local)) else {
                continue;
            };
            let value = builder.use_var(var);
            let base = builder.ins().stack_addr(self.ptr_ty, slot, 0);
            store_at(builder, value, base, 0);
        }
    }

    /// Keeps the locals the call ending a predecessor of `block` wrote.
    pub(super) fn spill_on_entry(
        &self,
        builder: &mut FunctionBuilder<'_>,
        locals: &HashMap<Local, Variable>,
        block: BlockId,
    ) {
        if let Some(written) = self.written_on_entry.get(&block) {
            self.spill(builder, locals, written);
        }
    }

    /// Keeps every local a pad names, for the body's entry.
    pub(super) fn spill_all(
        &self,
        builder: &mut FunctionBuilder<'_>,
        locals: &HashMap<Local, Variable>,
    ) {
        let mut named: Vec<&Local> = self.spill.keys().collect();
        named.sort_by_key(|local| local.0);
        self.spill(builder, locals, named);
    }

    /// Opens a pad when `block` is one: tells the runtime a pad is running
    /// or notes the panic it caught, then reads back every local a pad names.
    pub(super) fn open_pad(
        &self,
        module: &mut dyn Module,
        builder: &mut FunctionBuilder<'_>,
        locals: &HashMap<Local, Variable>,
        intrinsics: &mut super::IntrinsicContext,
        block: BlockId,
    ) -> Result<()> {
        let entry = if block == self.pads.cleanup_pad {
            "gos_rt_unwind_begin"
        } else if block == self.pads.note_pad {
            "gos_rt_unwind_note"
        } else {
            return Ok(());
        };
        let helper = intrinsics.extern_fn_by_name(module, entry)?;
        let helper = module.declare_func_in_func(helper, builder.func);
        builder.ins().call(helper, &[]);
        let mut named: Vec<(&Local, &(StackSlot, ir::Type))> = self.spill.iter().collect();
        named.sort_by_key(|(local, _)| local.0);
        for (local, &(slot, ty)) in named {
            let Some(&var) = locals.get(local) else {
                continue;
            };
            let base = builder.ins().stack_addr(self.ptr_ty, slot, 0);
            let value = load_at(builder, ty, base, 0);
            builder.def_var(var, value);
        }
        Ok(())
    }

    /// Lowers `Resume`: the pad has run its deferred expressions, and the
    /// fault it caught continues to the callers.
    pub(super) fn lower_resume(
        module: &mut dyn Module,
        builder: &mut FunctionBuilder<'_>,
        intrinsics: &mut super::IntrinsicContext,
    ) -> Result<()> {
        for helper in ["gos_rt_unwind_end", "gos_rt_unwind_resume"] {
            let helper = intrinsics.extern_fn_by_name(module, helper)?;
            let helper = module.declare_func_in_func(helper, builder.func);
            builder.ins().call(helper, &[]);
        }
        builder
            .ins()
            .trap(ir::TrapCode::user(1).ok_or_else(|| anyhow!("trap code"))?);
        Ok(())
    }

    /// What the post-build rewrite needs, once every block is lowered.
    pub(super) fn finish(self, blocks: &HashMap<u32, ir::Block>) -> Result<UnwindRewrite> {
        let pad = |block: BlockId| {
            blocks
                .get(&block.as_u32())
                .copied()
                .ok_or_else(|| anyhow!("landing pad bb{} was not lowered", block.as_u32()))
        };
        Ok(UnwindRewrite {
            cleanup_pad: pad(self.pads.cleanup_pad)?,
            note_pad: pad(self.pads.note_pad)?,
            pad_blocks: self.pad_blocks,
        })
    }
}

/// The callee of a call instruction.
enum Callee {
    Direct(ir::FuncRef),
    Indirect(ir::Value),
}

/// One call to make through a catching shim.
struct CatchingSite {
    inst: ir::Inst,
    in_pad: bool,
}

/// Every call in `func` that can unwind into a landing pad, in layout order,
/// with whether it sits in a pad's own code.
fn catching_sites(
    func: &Function,
    module: &dyn Module,
    rewrite: &UnwindRewrite,
) -> Vec<CatchingSite> {
    let mut sites = Vec::new();
    for block in func.layout.blocks() {
        let in_pad = rewrite.pad_blocks.contains(&block);
        for inst in func.layout.block_insts(block) {
            let is_call = match &func.dfg.insts[inst] {
                InstructionData::Call { func_ref, .. } => {
                    !is_unwind_helper(func, module, *func_ref)
                }
                InstructionData::CallIndirect { .. } => true,
                _ => false,
            };
            if is_call {
                sites.push(CatchingSite { inst, in_pad });
            }
        }
    }
    sites
}

/// Whether `func_ref` names one of the runtime's own unwinding entries,
/// which a pad calls directly.
fn is_unwind_helper(func: &Function, module: &dyn Module, func_ref: ir::FuncRef) -> bool {
    let ir::ExternalName::User(name) = &func.dfg.ext_funcs[func_ref].name else {
        return false;
    };
    let index = func.params.user_named_funcs()[*name].index;
    module
        .declarations()
        .get_function_decl(FuncId::from_u32(index))
        .name
        .as_deref()
        .is_some_and(|name| name.starts_with("gos_rt_unwind_"))
}

/// The catching thunks a module has defined, by the signature they call.
#[derive(Default)]
pub(crate) struct CatchingThunks {
    by_signature: HashMap<Signature, FuncId>,
    /// Thunks defined since the frames were last taken, with the compiled
    /// size and unwind information the unwinder needs to cross them.
    defined: Vec<crate::jit_frames::FunctionFrames>,
}

impl CatchingThunks {
    /// The thunks defined since the last call, for frame registration.
    pub(crate) fn take_frames(&mut self) -> Vec<crate::jit_frames::FunctionFrames> {
        std::mem::take(&mut self.defined)
    }

    /// The thunk that calls a callee of signature `sig`, defining it first.
    fn for_signature(&mut self, module: &mut dyn Module, sig: &Signature) -> Result<FuncId> {
        if let Some(id) = self.by_signature.get(sig) {
            return Ok(*id);
        }
        let ptr_ty = module.target_config().pointer_type();
        let mut thunk_sig = module.make_signature();
        thunk_sig.params.push(AbiParam::new(ptr_ty));
        let name: &'static str =
            Box::leak(format!("__gos_unwind_thunk_{}", self.by_signature.len()).into_boxed_str());
        let id = module
            .declare_function(name, Linkage::Local, &thunk_sig)
            .map_err(|e| anyhow!("declare {name}: {e}"))?;
        let mut func = Function::with_name_signature(UserFuncName::user(0, id.as_u32()), thunk_sig);
        let mut fb_ctx = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let ctx = builder.block_params(entry)[0];
            let flags = MemFlagsData::trusted();
            let callee = builder.ins().load(ptr_ty, flags, ctx, 0);
            let args: Vec<ir::Value> = sig
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    load_at(&mut builder, param.value_type, ctx, word_offset(1 + index))
                })
                .collect();
            let callee_sig = builder.import_signature(sig.clone());
            let call = builder.ins().call_indirect(callee_sig, callee, &args);
            let results = builder.inst_results(call).to_vec();
            for (index, value) in results.into_iter().enumerate() {
                store_at(
                    &mut builder,
                    value,
                    ctx,
                    word_offset(1 + sig.params.len() + index),
                );
            }
            builder.ins().return_(&[]);
            builder.seal_all_blocks();
            builder.finalize(module.target_config());
        }
        let mut ctx = Context::for_function(func);
        module
            .define_function(id, &mut ctx)
            .map_err(|e| anyhow!("define {name}: {e}"))?;
        if let Some(code) = ctx.compiled_code() {
            self.defined.push(crate::jit_frames::FunctionFrames {
                id,
                name: std::sync::Arc::from(""),
                code_len: code.code_info().total_size,
                unwind: code.create_unwind_info(module.isa()).ok().flatten(),
                positions: Vec::new(),
                spans: Vec::new(),
            });
        }
        self.by_signature.insert(sig.clone(), id);
        Ok(id)
    }
}

/// The byte offset of word `index` in a catching call's buffer.
fn word_offset(index: usize) -> i32 {
    i32::try_from(index).map_or(i32::MAX, |index| index.saturating_mul(SLOT_BYTES as i32))
}

/// Loads a `ty` at `base + offset`, as two words for a carrier.
fn load_at(
    builder: &mut FunctionBuilder<'_>,
    ty: ir::Type,
    base: ir::Value,
    offset: i32,
) -> ir::Value {
    let flags = MemFlagsData::trusted();
    if ty == types::I128 {
        let low = builder.ins().load(types::I64, flags, base, offset);
        let high = builder.ins().load(types::I64, flags, base, offset + 8);
        builder.ins().iconcat(low, high)
    } else {
        builder.ins().load(ty, flags, base, offset)
    }
}

/// Stores `value` at `base + offset`, as two words for a carrier.
fn store_at(builder: &mut FunctionBuilder<'_>, value: ir::Value, base: ir::Value, offset: i32) {
    let flags = MemFlagsData::trusted();
    if builder.func.dfg.value_type(value) == types::I128 {
        let (low, high) = builder.ins().isplit(value);
        builder.ins().store(flags, low, base, offset);
        builder.ins().store(flags, high, base, offset + 8);
    } else {
        builder.ins().store(flags, value, base, offset);
    }
}

/// Makes every call in a built body a catching call that branches to the
/// body's cleanup pad, or from a pad's own code to its note pad, when a
/// fault unwinds out of the callee.
pub(crate) fn rewrite_unwinding_calls(
    module: &mut dyn Module,
    func: &mut Function,
    rewrite: &UnwindRewrite,
    thunks: &mut CatchingThunks,
) -> Result<()> {
    let ptr_ty = module.target_config().pointer_type();
    let mut shim_sig = module.make_signature();
    shim_sig.params.push(AbiParam::new(ptr_ty));
    shim_sig.params.push(AbiParam::new(ptr_ty));
    shim_sig.returns.push(AbiParam::new(types::I32));
    let mut shims = HashMap::new();
    for name in ["gos_rt_unwind_try_call", "gos_rt_unwind_try_call_in_pad"] {
        let id = module
            .declare_function(name, Linkage::Import, &shim_sig)
            .map_err(|e| anyhow!("declare {name}: {e}"))?;
        shims.insert(name, module.declare_func_in_func(id, func));
    }
    for site in catching_sites(func, module, rewrite) {
        let inst = site.inst;
        let (callee, sig_ref) = match &func.dfg.insts[inst] {
            InstructionData::Call { func_ref, .. } => (
                Callee::Direct(*func_ref),
                func.dfg.ext_funcs[*func_ref].signature,
            ),
            InstructionData::CallIndirect { sig_ref, .. } => {
                (Callee::Indirect(func.dfg.inst_args(inst)[0]), *sig_ref)
            }
            _ => continue,
        };
        let sig = func.dfg.signatures[sig_ref].clone();
        let args: Vec<ir::Value> = match callee {
            Callee::Direct(_) => func.dfg.inst_args(inst).to_vec(),
            Callee::Indirect(_) => func.dfg.inst_args(inst)[1..].to_vec(),
        };
        let results: Vec<ir::Value> = func.dfg.inst_results(inst).to_vec();
        let thunk = thunks.for_signature(module, &sig)?;
        let thunk = module.declare_func_in_func(thunk, func);
        let words = 1 + args.len() + results.len();
        let slot = func.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            u32::try_from(words)?.saturating_mul(SLOT_BYTES),
            4,
        ));
        let srcloc = func.srcloc(inst);
        let block = func
            .layout
            .inst_block(inst)
            .ok_or_else(|| anyhow!("call outside the layout"))?;
        let next = func
            .layout
            .next_inst(inst)
            .ok_or_else(|| anyhow!("call ends its block"))?;
        let flags = MemFlagsData::trusted();
        let mut pos = FuncCursor::new(func).at_inst(inst);
        pos.set_srcloc(srcloc);
        let base = pos.ins().stack_addr(ptr_ty, slot, 0);
        let target = match callee {
            Callee::Direct(func_ref) => pos.ins().func_addr(ptr_ty, func_ref),
            Callee::Indirect(value) => value,
        };
        pos.ins().store(flags, target, base, 0);
        for (index, arg) in args.iter().enumerate() {
            let offset = word_offset(1 + index);
            if pos.func.dfg.value_type(*arg) == types::I128 {
                let (low, high) = pos.ins().isplit(*arg);
                pos.ins().store(flags, low, base, offset);
                pos.ins().store(flags, high, base, offset + 8);
            } else {
                pos.ins().store(flags, *arg, base, offset);
            }
        }
        let thunk_addr = pos.ins().func_addr(ptr_ty, thunk);
        let shim = if site.in_pad {
            shims["gos_rt_unwind_try_call_in_pad"]
        } else {
            shims["gos_rt_unwind_try_call"]
        };
        let status_call = pos.ins().call(shim, &[thunk_addr, base]);
        let status = pos.func.dfg.inst_results(status_call)[0];
        let landing = if site.in_pad {
            rewrite.note_pad
        } else {
            rewrite.cleanup_pad
        };
        let resumed = pos.func.dfg.make_block();
        pos.func.layout.split_block(resumed, next);
        pos.func.layout.remove_inst(inst);
        pos.func.dfg.detach_inst_results(inst);
        let mut pos = FuncCursor::new(func).at_bottom(block);
        pos.set_srcloc(srcloc);
        pos.ins().brif(status, landing, &[], resumed, &[]);
        let mut pos = FuncCursor::new(func).at_first_insertion_point(resumed);
        pos.set_srcloc(srcloc);
        for (index, result) in results.iter().enumerate() {
            let ty = pos.func.dfg.value_type(*result);
            let offset = word_offset(1 + args.len() + index);
            let loaded = if ty == types::I128 {
                let low = pos.ins().load(types::I64, flags, base, offset);
                let high = pos.ins().load(types::I64, flags, base, offset + 8);
                pos.ins().iconcat(low, high)
            } else {
                pos.ins().load(ty, flags, base, offset)
            };
            pos.func.dfg.change_to_alias(*result, loaded);
        }
    }
    Ok(())
}
