#![allow(
    clippy::too_many_lines,
    reason = "VM dispatch loop - see vm/run.rs roadmap for arm-group decomp"
)]
use super::*;
use crate::bytecode::InstrIdx;
use std::fmt::Write as _;

const VM_PREEMPT_INTERVAL: u16 = 1024;

#[inline]
fn poll_vm_backedge(countdown: &mut u16) {
    *countdown -= 1;
    if *countdown == 0 {
        // The scheduler phase/pressure poll is cheap and only yields when a
        // watchdog or peer requested preemption. The old unconditional
        // `yield_now` handed every tight VM loop to the kernel once per 1024
        // backedges even when this was the only runnable work.
        gossamer_runtime::preempt::gos_rt_preempt_check_and_yield();
        *countdown = VM_PREEMPT_INTERVAL;
    }
}

/// Deep-clones a `Map` / `IntMap` / `StrIntMap` / `Set` / `BTreeSet` value
/// into a fresh, independent backing table; any other value clones like
/// `Value::clone` (a cheap `Arc`/scalar copy). Shared by [`Op::CloneMapLike`]
/// (a `let` binding or by-value call argument), [`Op::BuildArrayRepeat`]
/// (`#[map_value; n]`), and the `clone` builtin - each needs independent
/// storage, not a second alias of the same `Arc<Mutex<_>>` / `SET_REGISTRY`
/// entry.
pub(crate) fn map_like_deep_clone(v: &Value) -> Value {
    match v {
        Value::Map(m) => Value::Map(Arc::new(parking_lot::Mutex::new(m.lock().clone()))),
        Value::IntMap(m) => Value::IntMap(Arc::new(parking_lot::Mutex::new(m.lock().clone()))),
        Value::StrIntMap(m) => {
            Value::StrIntMap(Arc::new(parking_lot::Mutex::new(m.lock().clone())))
        }
        // A tuple or fixed array reaches its elements the way a struct
        // reaches its fields, so an element holding a table takes storage of
        // its own rather than a second handle on the source's.
        Value::Tuple(elems) if elems.iter().any(holds_shared_storage) => {
            Value::Tuple(Arc::new(elems.iter().map(map_like_deep_clone).collect()))
        }
        Value::Array(elems) if elems.iter().any(holds_shared_storage) => {
            Value::Array(Arc::new(elems.iter().map(map_like_deep_clone).collect()))
        }
        Value::Struct(inner) => match inner.name.as_str() {
            // The slot containers reach their elements through a registry
            // handle exactly as a `Set` does, so a binding taken from one
            // copies the elements rather than the handle.
            "Deque" | "Queue" | "Stack" => crate::stdlib_builtins::deque::deque_deep_clone(v),
            "MinHeap" | "MaxHeap" => {
                crate::stdlib_builtins::container_heap::binary_heap_deep_clone(v)
            }
            "Set" | "BTreeSet" => crate::stdlib_builtins::set::set_deep_clone(v),
            // A user struct: rebuild it with every container field
            // deep-cloned, any depth of plain structs down, so a struct
            // binding never shares a table or an element store with its
            // source. A `Map`, a `Set`, and the slot containers all reach
            // their storage through a handle that carries no count of its
            // holders, so a copied field takes storage of its own - which is
            // what the compiled tiers give the same field through the drop
            // pass's per-field clone.
            _ => {
                if inner.fields.iter().any(|(_, f)| holds_shared_storage(f)) {
                    let mut rebuilt = (**inner).clone();
                    for (_, f) in rebuilt.fields.iter_mut() {
                        if holds_shared_storage(f) {
                            *f = map_like_deep_clone(f);
                        }
                    }
                    return Value::Struct(std::sync::Arc::new(rebuilt));
                }
                v.clone()
            }
        },
        other => other.clone(),
    }
}

/// True when a value reaches storage a plain `Value::clone` would leave shared:
/// a map's table, a set's or a slot container's element store, or a struct
/// holding one of those at any depth.
fn holds_shared_storage(v: &Value) -> bool {
    match v {
        Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_) => true,
        Value::Tuple(elems) | Value::Array(elems) => elems.iter().any(holds_shared_storage),
        Value::Struct(inner) => {
            matches!(
                inner.name.as_str(),
                "Set" | "BTreeSet" | "Deque" | "Queue" | "Stack" | "MinHeap" | "MaxHeap"
            ) || inner.fields.iter().any(|(_, f)| holds_shared_storage(f))
        }
        _ => false,
    }
}

fn incompatible_type_error(value: &Value, peer: Option<&Value>, expected: &str) -> RuntimeError {
    let message = match peer {
        Some(peer) => format!(
            "incompatible types: `{}` ({}) and `{}` ({})",
            peer.type_name(),
            peer.repr(),
            value.type_name(),
            value.repr()
        ),
        None => format!(
            "incompatible types: `{expected}` and `{}` ({})",
            value.type_name(),
            value.repr()
        ),
    };
    RuntimeError::Type(message)
}

/// The bounds panic every `swap` opcode raises. One wording across the
/// general and the flat-register paths, so the same source line reports the
/// same fault whichever specialization the compiler picked.
#[cold]
fn swap_out_of_bounds(a: i64, b: i64, len: usize) -> RuntimeError {
    RuntimeError::Panic(format!(
        "vec swap index out of bounds: the len is {len} but the index is {}",
        if a < 0 || a as usize >= len { a } else { b }
    ))
}

/// Element count of a receiver holding the flat-swap invariant, for the
/// cold path that reports a negative index before the receiver is matched.
#[cold]
fn flat_receiver_len(value: &Value) -> usize {
    match value {
        Value::Array(values) => values.len(),
        Value::IntArray(values) => values.len(),
        Value::ByteArray(values) => values.len(),
        Value::InlineByteArray(values) => values.len(),
        Value::ByteVec(values) => values.len(),
        Value::FloatVec(values) => values.len(),
        _ => 0,
    }
}

#[inline(always)]
fn checked_integer_arithmetic(
    lhs: i64,
    rhs: i64,
    ty: gossamer_types::IntTy,
    op: ImmArithKind,
) -> RuntimeResult<i64> {
    use gossamer_types::IntTy;

    let label = match op {
        ImmArithKind::Add => "add",
        ImmArithKind::Sub => "subtract",
        ImmArithKind::Mul => "multiply",
        ImmArithKind::Div | ImmArithKind::Rem => unreachable!("only additive ops are checked here"),
    };
    let overflow = || RuntimeError::Panic(format!("attempt to {label} with overflow"));
    match ty {
        IntTy::U64 | IntTy::U128 | IntTy::Usize => {
            let lhs = lhs as u64;
            let rhs = rhs as u64;
            let value = match op {
                ImmArithKind::Add => lhs.checked_add(rhs),
                ImmArithKind::Sub => lhs.checked_sub(rhs),
                ImmArithKind::Mul => lhs.checked_mul(rhs),
                ImmArithKind::Div | ImmArithKind::Rem => unreachable!(),
            }
            .ok_or_else(overflow)?;
            Ok(value as i64)
        }
        IntTy::U8 | IntTy::U16 | IntTy::U32 => {
            let max = match ty {
                IntTy::U8 => u8::MAX as u128,
                IntTy::U16 => u16::MAX as u128,
                IntTy::U32 => u32::MAX as u128,
                _ => unreachable!(),
            };
            let lhs = lhs as u64 as u128;
            let rhs = rhs as u64 as u128;
            let value = match op {
                ImmArithKind::Add => lhs.checked_add(rhs),
                ImmArithKind::Sub => lhs.checked_sub(rhs),
                ImmArithKind::Mul => lhs.checked_mul(rhs),
                ImmArithKind::Div | ImmArithKind::Rem => unreachable!(),
            }
            .filter(|value| *value <= max)
            .ok_or_else(overflow)?;
            Ok(value as u64 as i64)
        }
        IntTy::I64 | IntTy::I128 | IntTy::Isize => {
            let value = match op {
                ImmArithKind::Add => lhs.checked_add(rhs),
                ImmArithKind::Sub => lhs.checked_sub(rhs),
                ImmArithKind::Mul => lhs.checked_mul(rhs),
                ImmArithKind::Div | ImmArithKind::Rem => unreachable!(),
            }
            .ok_or_else(overflow)?;
            Ok(value)
        }
        IntTy::I8 | IntTy::I16 | IntTy::I32 => {
            let (min, max) = match ty {
                IntTy::I8 => (i8::MIN as i128, i8::MAX as i128),
                IntTy::I16 => (i16::MIN as i128, i16::MAX as i128),
                IntTy::I32 => (i32::MIN as i128, i32::MAX as i128),
                _ => unreachable!(),
            };
            let lhs = i128::from(lhs);
            let rhs = i128::from(rhs);
            let value = match op {
                ImmArithKind::Add => lhs + rhs,
                ImmArithKind::Sub => lhs - rhs,
                ImmArithKind::Mul => lhs * rhs,
                ImmArithKind::Div | ImmArithKind::Rem => unreachable!(),
            };
            if !(min..=max).contains(&value) {
                return Err(overflow());
            }
            Ok(value as i64)
        }
    }
}

/// Defers publishing a bytecode frame's current source position until the
/// frame exits or suspends. Tracebacks only observe the call stack after an
/// error has left the dispatch loop, so mutably borrowing the shared stack at
/// every source-expression boundary was pure hot-path overhead.
struct TracebackLocationGuard<'a> {
    call_stack: &'a RefCell<Vec<VmCallStackFrame>>,
    /// Index of this bytecode frame in the logical call stack. A nested
    /// direct call may leave its own failing frame on top before this guard
    /// drops, so updating `last_mut()` would overwrite the callee's location.
    frame_index: usize,
    locations: &'a [crate::bytecode::InstructionLocation],
    /// Address of the dispatch loop's program counter. `pc` is declared
    /// before this guard and therefore outlives it. Every opcode increments
    /// `pc` before execution, so `pc - 1` is the failing or suspending opcode.
    next_instruction: *const InstrIdx,
}

impl Drop for TracebackLocationGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: `next_instruction` points to `run_with_frame`'s local `pc`.
        // That local is declared before this guard, so Rust drops the guard
        // first. The VM is single-threaded and reads it only during Drop.
        let next_instruction = unsafe { *self.next_instruction };
        let location = next_instruction.checked_sub(1).and_then(|instruction| {
            let after = self
                .locations
                .partition_point(|entry| entry.instruction <= instruction);
            after
                .checked_sub(1)
                .and_then(|idx| self.locations[idx].location)
        });
        if let Some(frame) = self.call_stack.borrow_mut().get_mut(self.frame_index) {
            frame.location = location;
        }
    }
}

/// How a bytecode frame completed.
///
/// A tail call is deliberately returned to [`Vm::apply`] rather than invoked
/// from this dispatch loop.  That lets the caller discard this frame before
/// entering the next one, so direct and mutual tail recursion use a constant
/// amount of native Rust stack.
pub(crate) enum RunControl {
    Return(Value),
    TailCall {
        chunk: Arc<FnChunk>,
        args: Vec<Value>,
    },
    /// A direct bytecode call whose caller has been moved out of the dispatch
    /// loop. `Vm::apply` drives the callee and resumes `parent` without
    /// growing the Rust call stack.
    Call {
        chunk: Arc<FnChunk>,
        args: Vec<Value>,
        dst: u16,
        parent: SuspendedFrame,
    },
}

/// A paused bytecode frame. Register files are moved, rather than cloned,
/// out of `FrameGuard` at a direct bytecode call and re-wrapped on resume.
pub(crate) struct SuspendedFrame {
    pub(crate) chunk: Arc<FnChunk>,
    pub(crate) registers: Vec<Value>,
    pub(crate) floats: Vec<f64>,
    pub(crate) ints: Vec<i64>,
    pub(crate) ref_cells: Vec<(usize, Arc<ThreadConfinedCell>)>,
    pub(crate) pc: u32,
    #[cfg(feature = "fuel")]
    pub(crate) prev_pc: u32,
}

impl Vm {
    pub(crate) fn run(
        &self,
        chunk: Arc<FnChunk>,
        state: &ChunkState,
        args: Vec<Value>,
    ) -> RuntimeResult<RunControl> {
        self.run_with_frame(chunk, state, args, None, true)
    }

    /// Executes a short-lived chunk whose `ChunkState` is not installed in
    /// the VM arena (currently compile-time initializers). Such a chunk cannot
    /// safely install its state in the VM arena because its chunk is dropped
    /// immediately after evaluation. It still uses explicit frames: each
    /// bytecode child is driven by the ordinary trampoline, then the local
    /// initializer frame is resumed from its moved register files.
    pub(crate) fn run_local(
        &self,
        chunk: Arc<FnChunk>,
        state: &ChunkState,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        let mut control = self.run_with_frame(chunk, state, args, None, true)?;
        loop {
            match control {
                RunControl::Return(value) => return Ok(value),
                RunControl::TailCall { chunk, args } => {
                    // The initializer's frame has been discarded at this
                    // point. The tail body owns the rest of the execution.
                    return self.apply(Global::Fn(chunk), args);
                }
                RunControl::Call {
                    chunk,
                    args,
                    dst,
                    mut parent,
                } => {
                    // `apply` drives arbitrarily deep bytecode descendants
                    // from its explicit frame stack. Only this local parent
                    // remains on the host stack, and it is resumed by moving
                    // its register files back into `run_with_frame`.
                    let value = self.apply(Global::Fn(chunk), args)?;
                    parent.registers[dst as usize] = value;
                    let parent_chunk = Arc::clone(&parent.chunk);
                    control =
                        self.run_with_frame(parent_chunk, state, Vec::new(), Some(parent), true)?;
                }
            }
        }
    }

    pub(crate) fn resume(&self, frame: SuspendedFrame) -> RuntimeResult<RunControl> {
        let chunk = Arc::clone(&frame.chunk);
        let state = self.chunk_state_for(&chunk);
        self.run_with_frame(chunk, state, Vec::new(), Some(frame), true)
    }

    fn run_with_frame(
        &self,
        chunk: Arc<FnChunk>,
        state: &ChunkState,
        args: Vec<Value>,
        frame: Option<SuspendedFrame>,
        allow_explicit_frames: bool,
    ) -> RuntimeResult<RunControl> {
        let is_resume = frame.is_some();
        if !is_resume && chunk.arity as usize != args.len() {
            return Err(RuntimeError::Arity {
                expected: chunk.arity as usize,
                found: args.len(),
            });
        }
        // Pool guard: takes the three register-file `Vec`s on
        // entry and returns them on Drop, so `?` and early
        // returns inside the dispatch loop don't leak buffers.
        #[cfg(feature = "fuel")]
        let mut prev_pc = 0;
        let (mut guard, mut ref_cells, mut pc) = if let Some(frame) = frame {
            #[cfg(feature = "fuel")]
            {
                prev_pc = frame.prev_pc;
            }
            (
                FrameGuard::from_parts(&self.pool, frame.registers, frame.floats, frame.ints),
                frame.ref_cells,
                frame.pc,
            )
        } else {
            (
                FrameGuard::take(
                    &self.pool,
                    chunk.register_count as usize,
                    chunk.float_count as usize,
                    chunk.int_count as usize,
                ),
                Vec::new(),
                0,
            )
        };
        let registers = &mut guard.registers;
        let floats = &mut guard.floats;
        let ints = &mut guard.ints;
        // Drain (not consume) so the empty Vec can go back to
        // the pool's `args` free list - most arg Vecs are
        // pool-borrowed in `Op::Call`, and reclaiming them here
        // closes the loop without an extra allocation per call.
        if !is_resume {
            let mut args = args;
            for (i, arg) in args.drain(..).enumerate() {
                registers[i] = arg;
            }
            self.pool.borrow_mut().give_args(args);
            for &(param, dst_i) in &chunk.i64_params {
                let value = match &registers[param as usize] {
                    Value::Int(value) => *value,
                    Value::Uint(value) => *value as i64,
                    other => {
                        return Err(RuntimeError::Type(format!(
                            "integer parameter received `{other}`"
                        )));
                    }
                };
                ints[dst_i as usize] = value;
            }
        }
        // Write-back cell protocol for `&mut Vec<T>` / `&mut [T]`
        // parameters: unwrap each incoming `MutCell` into its param
        // register and remember the cell so every return path below
        // publishes the final register value back to the caller.
        if !is_resume && !chunk.mut_ref_params.is_empty() {
            for &idx in &chunk.mut_ref_params {
                let slot = idx as usize;
                if let Value::MutCell(cell) = &registers[slot] {
                    let cell = Arc::clone(cell);
                    // Move the inner value out of the cell into the param
                    // register. With a `CellNewMove`-created cell the caller's
                    // home register was already emptied, so this keeps the
                    // value's refcount at one and the first field write mutates
                    // in place. `publish_ref_cells` repopulates the cell on
                    // return.
                    registers[slot] = std::mem::replace(&mut *cell.lock(), Value::Unit);
                    ref_cells.push((slot, cell));
                }
            }
        }
        if !is_resume {
            crate::profile::enter_frame();
        }
        #[cfg(feature = "profile")]
        struct ProfDump(&'static str);
        #[cfg(feature = "profile")]
        impl Drop for ProfDump {
            fn drop(&mut self) {
                if self.0 == "main" && std::env::var_os("GOS_VM_PROFILE").is_some() {
                    eprint!("{}", crate::profile::dump_report());
                }
            }
        }
        #[cfg(feature = "profile")]
        let _prof_dump = ProfDump(chunk.name);
        let instrs: &[Op] = &chunk.instrs;
        let instr_count = instrs.len();
        let _traceback_location = TracebackLocationGuard {
            call_stack: &self.call_stack,
            frame_index: self.call_stack.borrow().len().saturating_sub(1),
            locations: &chunk.instruction_locations,
            next_instruction: std::ptr::addr_of!(pc),
        };
        let mut preempt_countdown = VM_PREEMPT_INTERVAL;
        // Several specialized opcodes have a generic method-call fallback.
        // A user-defined method resolves to `Global::Fn`; it must use the
        // same suspended-frame protocol as `Op::Call`, not recurse through
        // `apply` from this dispatch frame.
        macro_rules! apply_or_suspend_bytecode {
            ($dst:expr, $target:expr, $call_args:expr) => {
                match $target {
                    Global::Fn(next_chunk) if allow_explicit_frames => {
                        if self.call_depth.get() < crate::vm::DIRECT_BYTECODE_CALL_DEPTH {
                            self.apply(Global::Fn(next_chunk), $call_args)?
                        } else {
                            guard.suspended = true;
                            return Ok(RunControl::Call {
                                chunk: next_chunk,
                                args: $call_args,
                                dst: $dst,
                                parent: SuspendedFrame {
                                    chunk: Arc::clone(&chunk),
                                    registers: std::mem::take(registers),
                                    floats: std::mem::take(floats),
                                    ints: std::mem::take(ints),
                                    ref_cells,
                                    pc,
                                    #[cfg(feature = "fuel")]
                                    prev_pc,
                                },
                            });
                        }
                    }
                    target => self.apply(target, $call_args)?,
                }
            };
        }
        loop {
            // Execution budget: a backward (or self) jump is a loop iteration.
            // Counting them bounds total iterations so an unbounded loop aborts
            // cleanly instead of hanging. Compiled in only under the `fuel`
            // feature (the wasm playground); native builds carry none of this.
            #[cfg(feature = "fuel")]
            {
                if pc <= prev_pc && crate::fuel::consume() {
                    return Err(RuntimeError::FuelExhausted);
                }
                prev_pc = pc;
            }
            // SAFETY: every chunk emitted by `compile.rs` ends
            // with a `Return` / `ReturnUnit`, and every jump /
            // branch target is computed from the same emit-
            // counter that placed the op - so `pc` can never
            // exceed `instr_count` at this point. We keep a
            // `debug_assert!` so a corrupted chunk fails loudly
            // in debug builds, but skip the runtime branch in
            // release. `Op` is `Copy`, so dereferencing gives
            // us a by-value copy of the enum for destructuring
            // without invoking `<Op as Clone>::clone`.
            debug_assert!((pc as usize) < instr_count, "fell off end of bytecode");
            let _ = instr_count;
            let op = unsafe { *instrs.get_unchecked(pc as usize) };
            crate::profile::record_op(op);
            pc += 1;
            match op {
                Op::LoadConst { dst, idx } => {
                    registers[dst as usize] = chunk.consts[idx as usize].clone();
                }
                Op::LoadGlobal { dst, idx } => {
                    let name: &str = &chunk.globals[idx as usize];
                    let value = match self.lookup_global_ref(name) {
                        Some(Global::Value(v)) => v.clone(),
                        Some(Global::MutStatic(cell)) => cell.lock().clone(),
                        Some(Global::Fn(_)) => Value::String(SmolStr::from(name)),
                        None => return Err(RuntimeError::UnresolvedName(name.to_string())),
                    };
                    registers[dst as usize] = value;
                }
                Op::StoreStatic { name_idx, src } => {
                    let name: &str = &chunk.globals[name_idx as usize];
                    match self.lookup_global_ref(name) {
                        Some(Global::MutStatic(cell)) => {
                            *cell.lock() = registers[src as usize].clone();
                        }
                        _ => return Err(RuntimeError::UnresolvedName(name.to_string())),
                    }
                }
                Op::Move { dst, src } => {
                    registers[dst as usize] = registers[src as usize].clone();
                }
                Op::CloneMapLike { dst, src } => {
                    registers[dst as usize] = map_like_deep_clone(&registers[src as usize]);
                }
                Op::MoveConsume { dst, src } => {
                    // Hand the value over instead of cloning: the source
                    // local is read exactly once at this point (proven by
                    // `compile::consume`), so the emptied slot is never read.
                    let v = std::mem::replace(&mut registers[src as usize], Value::Void);
                    registers[dst as usize] = v;
                }
                Op::Deref { dst, src } => {
                    let v = registers[src as usize].clone();
                    let resolved = if let Value::Struct(inner) = &v {
                        if inner.name == "__Cell" {
                            let mut set_id: u64 = 0;
                            let mut flag_name = String::new();
                            for (ident, val) in &inner.fields {
                                if (*ident) == "__set_id"
                                    && let Value::Int(n) = val
                                {
                                    set_id = *n as u64;
                                }
                                if (*ident) == "__flag_name"
                                    && let Value::String(s) = val
                                {
                                    flag_name = s.as_str().to_string();
                                }
                            }
                            crate::builtins::resolve_cell(set_id, &flag_name)
                                .unwrap_or_else(|| v.clone())
                        } else {
                            v
                        }
                    } else {
                        v
                    };
                    registers[dst as usize] = resolved;
                }
                Op::AddInt {
                    dst,
                    lhs,
                    rhs,
                    cache_idx,
                } => {
                    let a = &registers[lhs as usize];
                    let b = &registers[rhs as usize];
                    let shape = state.arith_caches[cache_idx as usize].shape.get();
                    registers[dst as usize] = adaptive_add(state, cache_idx, shape, a, b)?;
                }
                Op::SubInt {
                    dst,
                    lhs,
                    rhs,
                    cache_idx,
                } => {
                    let a = &registers[lhs as usize];
                    let b = &registers[rhs as usize];
                    let shape = state.arith_caches[cache_idx as usize].shape.get();
                    registers[dst as usize] = adaptive_arith(
                        state,
                        cache_idx,
                        shape,
                        a,
                        b,
                        i64::wrapping_sub,
                        |x, y| x - y,
                        "subtraction",
                    )?;
                }
                Op::MulInt {
                    dst,
                    lhs,
                    rhs,
                    cache_idx,
                } => {
                    let a = &registers[lhs as usize];
                    let b = &registers[rhs as usize];
                    let shape = state.arith_caches[cache_idx as usize].shape.get();
                    registers[dst as usize] = adaptive_arith(
                        state,
                        cache_idx,
                        shape,
                        a,
                        b,
                        i64::wrapping_mul,
                        |x, y| x * y,
                        "multiplication",
                    )?;
                }
                Op::DivInt {
                    dst,
                    lhs,
                    rhs,
                    cache_idx,
                } => {
                    let a = &registers[lhs as usize];
                    let b = &registers[rhs as usize];
                    let shape = state.arith_caches[cache_idx as usize].shape.get();
                    registers[dst as usize] = adaptive_div(state, cache_idx, shape, a, b)?;
                }
                Op::RemInt {
                    dst,
                    lhs,
                    rhs,
                    cache_idx,
                } => {
                    let a = &registers[lhs as usize];
                    let b = &registers[rhs as usize];
                    let shape = state.arith_caches[cache_idx as usize].shape.get();
                    registers[dst as usize] = adaptive_rem(state, cache_idx, shape, a, b)?;
                }
                Op::Neg { dst, operand } => {
                    registers[dst as usize] = neg(&registers[operand as usize])?;
                }
                Op::Not { dst, operand } => {
                    registers[dst as usize] = not(&registers[operand as usize])?;
                }
                Op::Eq { dst, lhs, rhs } => {
                    // `values_equal` already auto-derefs `__Cell`.
                    registers[dst as usize] = Value::Bool(values_equal(
                        &registers[lhs as usize],
                        &registers[rhs as usize],
                    ));
                }
                Op::Ne { dst, lhs, rhs } => {
                    registers[dst as usize] = Value::Bool(!values_equal(
                        &registers[lhs as usize],
                        &registers[rhs as usize],
                    ));
                }
                Op::Lt { dst, lhs, rhs } => {
                    registers[dst as usize] = compare(
                        &registers[lhs as usize],
                        &registers[rhs as usize],
                        std::cmp::Ordering::Less,
                        false,
                    )?;
                }
                Op::Le { dst, lhs, rhs } => {
                    registers[dst as usize] = compare(
                        &registers[lhs as usize],
                        &registers[rhs as usize],
                        std::cmp::Ordering::Less,
                        true,
                    )?;
                }
                Op::Gt { dst, lhs, rhs } => {
                    registers[dst as usize] = compare(
                        &registers[lhs as usize],
                        &registers[rhs as usize],
                        std::cmp::Ordering::Greater,
                        false,
                    )?;
                }
                Op::Ge { dst, lhs, rhs } => {
                    registers[dst as usize] = compare(
                        &registers[lhs as usize],
                        &registers[rhs as usize],
                        std::cmp::Ordering::Greater,
                        true,
                    )?;
                }
                Op::Jump { target } => {
                    if target < pc {
                        poll_vm_backedge(&mut preempt_countdown);
                    }
                    pc = target;
                }
                Op::BranchIf { cond, target } => {
                    if truthy(&registers[cond as usize])? {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                }
                Op::BranchIfNot { cond, target } => {
                    if !truthy(&registers[cond as usize])? {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                }
                op @ (Op::Call { .. } | Op::CallGlobal { .. }) => {
                    let (dst, callee, direct_global, args, argc, cache_idx, may_have_cells) =
                        match op {
                            Op::Call {
                                dst,
                                callee,
                                args,
                                argc,
                                cache_idx,
                                may_have_cells,
                            } => (
                                dst,
                                Some(callee),
                                None,
                                args,
                                argc,
                                cache_idx,
                                may_have_cells,
                            ),
                            Op::CallGlobal {
                                dst,
                                global_idx,
                                args,
                                argc,
                                cache_idx,
                                may_have_cells,
                            } => (
                                dst,
                                None,
                                Some(global_idx),
                                args,
                                argc,
                                cache_idx,
                                may_have_cells,
                            ),
                            _ => unreachable!(),
                        };
                    // `Call; Return <same destination>` is a tail position.
                    // Do not re-enter `apply` from this already-large Rust
                    // dispatch frame. The outer trampoline resolves the
                    // bytecode callee after this frame (and its pooled
                    // registers) has been dropped.
                    // `main` is the stable root of user-visible panic traces.
                    // Do not replace it with its final callee: a failing
                    // `main() { inner() }` must still report both frames.
                    let tail_position = chunk.name != "main"
                        && ref_cells.is_empty()
                        && matches!(instrs.get(pc as usize), Some(Op::Return { value }) if *value == dst);
                    let argc_usz = argc as usize;
                    if let Some(callee) = callee
                        && let Value::Variant(inner) = &registers[callee as usize]
                        && inner.fields.is_empty()
                    {
                        let take_arg = |registers: &mut [Value], offset: usize| {
                            let raw = std::mem::replace(
                                &mut registers[args as usize + offset],
                                Value::Void,
                            );
                            if may_have_cells {
                                auto_deref_cell(&raw).unwrap_or(raw)
                            } else {
                                raw
                            }
                        };
                        registers[dst as usize] = match argc_usz {
                            // A nullary constructor's callee sentinel is already
                            // the exact immutable value the call would create.
                            // Reuse it instead of hashing through the small-value
                            // cache on every invocation.
                            0 => registers[callee as usize].clone(),
                            1 => {
                                let variant_name = inner.name.clone();
                                let field = take_arg(registers, 0);
                                Value::variant_with_tag_1(variant_name, field)
                            }
                            2 => {
                                let variant_name = inner.name.clone();
                                let first = take_arg(registers, 0);
                                let second = take_arg(registers, 1);
                                Value::variant_with_tag_2(variant_name, first, second)
                            }
                            _ => {
                                let variant_name = inner.name.clone();
                                let mut fields = Vec::with_capacity(argc_usz);
                                for i in 0..argc_usz {
                                    fields.push(take_arg(registers, i));
                                }
                                Value::variant_with_tag(variant_name, fields)
                            }
                        };
                        continue;
                    }
                    let mut arg_values = self.pool.borrow_mut().take_args(argc_usz);
                    for i in 0..argc_usz {
                        // Move the argument out of its scratch slot rather
                        // than cloning: the contiguous `args` region is
                        // written once by the call's arg setup and read
                        // only here, so handing the value to the callee
                        // gives it unique ownership (the basis for
                        // move-on-last-use draining the input) and saves a
                        // refcount bump per argument. A `&mut` write-back
                        // cell lives in a separate register that `CellTake`
                        // reads after the call, so emptying the slot is
                        // safe.
                        //
                        // 0.7.0 flag::Cell auto-deref at the call boundary:
                        // `f(flags.output)` passes the current backing
                        // value instead of the `__Cell` handle. Matches
                        // Rust's `Deref` coercion on fn-arg sites. Skipped
                        // when the compiler proved every argument is scalar.
                        let raw = std::mem::replace(&mut registers[args as usize + i], Value::Void);
                        let v = if may_have_cells {
                            auto_deref_cell(&raw).unwrap_or(raw)
                        } else {
                            raw
                        };
                        arg_values.push(v);
                    }
                    let callee_val = callee.map(|callee| &registers[callee as usize]);
                    let direct_name = direct_global.map(|idx| &*chunk.globals[idx as usize]);
                    let token = if direct_name.is_some() {
                        NAMED_CALL_TOKEN
                    } else {
                        callee_val.map_or(0, call_token)
                    };
                    let callee_name = direct_name.or_else(|| match callee_val {
                        Some(Value::String(name)) => Some(name.as_str()),
                        _ => None,
                    });
                    let live_generation = self.globals_generation();
                    // Resolve every bytecode callable before falling through
                    // to the synchronous dispatcher. In particular, closure
                    // bodies are bytecode chunks too: calling one from a VM
                    // frame must suspend that frame rather than grow the Rust
                    // stack through `invoke_closure -> apply`.
                    let bytecode_target = if let Some(name) = callee_name {
                        let cached = {
                            let cache = state.call_caches.borrow();
                            let slot = &cache[cache_idx as usize];
                            (slot.type_token == token
                                && slot.callee_name.as_deref() == Some(name)
                                && slot.generation == live_generation)
                                .then(|| slot.fn_chunk.as_ref().map(Arc::clone))
                                .flatten()
                        };
                        let resolved = cached.or_else(|| {
                            let global = self.lookup_global(name);
                            if let Some(ref global) = global {
                                let mut cache = state.call_caches.borrow_mut();
                                cache[cache_idx as usize] = fill_cache_slot(
                                    token,
                                    live_generation,
                                    Some(SmolStr::from(name)),
                                    global,
                                );
                            }
                            match global {
                                Some(Global::Fn(chunk)) => Some(chunk),
                                _ => None,
                            }
                        });
                        resolved.map(|chunk| (chunk, None))
                    } else if let Some(Value::Closure(closure)) = callee_val {
                        Some((Arc::clone(&closure.chunk), Some(closure)))
                    } else {
                        None
                    };
                    if let Some((next_chunk, closure)) = bytecode_target {
                        let closure_call = closure.is_some();
                        let next_args = if let Some(closure) = closure {
                            let expected = next_chunk.arity as usize - closure.capture_values.len();
                            if expected != arg_values.len() {
                                return Err(RuntimeError::Arity {
                                    expected,
                                    found: arg_values.len(),
                                });
                            }
                            // Keep the call-argument pool in the ownership
                            // loop: the child receives `full`, which its
                            // entry drains and returns, while the now-empty
                            // source buffer is immediately reusable here.
                            let mut full = self
                                .pool
                                .borrow_mut()
                                .take_args(closure.capture_values.len() + arg_values.len());
                            full.extend(closure.capture_values.iter().cloned());
                            full.append(&mut arg_values);
                            self.pool.borrow_mut().give_args(arg_values);
                            full
                        } else {
                            arg_values
                        };
                        if tail_position {
                            publish_ref_cells(&ref_cells, registers);
                            return Ok(RunControl::TailCall {
                                chunk: next_chunk,
                                args: next_args,
                            });
                        }
                        if !allow_explicit_frames {
                            // Kept for callers that deliberately request the
                            // old direct path; local initializers now opt in.
                            let result = self.apply(Global::Fn(next_chunk), next_args)?;
                            registers[dst as usize] = result;
                            continue;
                        }
                        if !closure_call
                            && self.call_depth.get() < crate::vm::DIRECT_BYTECODE_CALL_DEPTH
                        {
                            let result = self.apply(Global::Fn(next_chunk), next_args)?;
                            registers[dst as usize] = result;
                            continue;
                        }
                        let parent = SuspendedFrame {
                            chunk: Arc::clone(&chunk),
                            registers: std::mem::take(registers),
                            floats: std::mem::take(floats),
                            ints: std::mem::take(ints),
                            ref_cells,
                            pc,
                            #[cfg(feature = "fuel")]
                            prev_pc,
                        };
                        guard.suspended = true;
                        return Ok(RunControl::Call {
                            chunk: next_chunk,
                            args: next_args,
                            dst,
                            parent,
                        });
                    }
                    // Inline-cache probe. The slot is keyed by the
                    // *callee* identity (the resolved name for a
                    // `Value::String(SmolStr::from("foo"))` callee). Cache hit
                    // skips the `self.globals.get(name)` HashMap
                    // probe - typically the dominant cost in tight
                    // loops calling small helper functions.
                    // Two-tier IC probe (same shape as MethodCall): a
                    // resolved builtin is returned as a raw `fn` pointer so
                    // the hit path calls it directly - no per-call
                    // `Arc<BuiltinInner>` allocation just to thread it
                    // through `apply`.
                    type BuiltinFn = fn(&[Value]) -> RuntimeResult<Value>;
                    let (cached_builtin, cached): (Option<BuiltinFn>, Option<Global>) =
                        if token != 0 {
                            // Read-only borrow on the IC hit path; the
                            // fill (miss) path below takes borrow_mut.
                            // Splitting avoids serialising every cached
                            // hit against any concurrent borrow on the
                            // same RefCell.
                            let cache = state.call_caches.borrow();
                            let slot = &cache[cache_idx as usize];
                            if slot.type_token == token
                                && slot.callee_name.as_deref() == callee_name
                                && slot.generation == live_generation
                            {
                                if let Some(call_fn) = slot.builtin_fn {
                                    (Some(call_fn), None)
                                } else {
                                    (
                                        None,
                                        slot.fn_chunk.as_ref().map(|c| Global::Fn(Arc::clone(c))),
                                    )
                                }
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        };
                    let result = if let Some(call_fn) = cached_builtin {
                        // Hottest hit: direct fn-ptr call. Unwrap any `&mut`
                        // write-back cells (free builtins take aggregates by
                        // value) and return the pooled arg buffer - both
                        // things `apply`'s builtin arm does, minus the alloc.
                        let call_args = crate::value::unwrap_mut_cells_unless_writer(
                            callee_name.unwrap_or_default(),
                            arg_values,
                        );
                        let r = call_fn(&call_args)?;
                        self.pool.borrow_mut().give_args(call_args);
                        r
                    } else if let Some(g) = cached {
                        self.apply(g, arg_values)?
                    } else if token != 0 {
                        // Miss: do the full dispatch and write back.
                        let resolved_global = callee_name.and_then(|name| self.lookup_global(name));
                        if let Some(ref g) = resolved_global {
                            let mut cache = state.call_caches.borrow_mut();
                            cache[cache_idx as usize] = fill_cache_slot(
                                token,
                                live_generation,
                                callee_name.map(SmolStr::from),
                                g,
                            );
                        }
                        match resolved_global {
                            Some(g) => self.apply(g, arg_values)?,
                            None => {
                                if let Some(name) = direct_name {
                                    return Err(self.unresolved(name));
                                }
                                self.dispatch_call(
                                    callee_val.expect("dynamic unresolved call has a callee"),
                                    arg_values,
                                )?
                            }
                        }
                    } else {
                        // Non-cacheable callee shape (Builtin,
                        // Closure, Native, …): straight to the
                        // existing slow-path dispatcher.
                        self.dispatch_call(
                            callee_val.expect("non-cacheable call has a callee"),
                            arg_values,
                        )?
                    };
                    registers[dst as usize] = result;
                }
                Op::Return { value } => {
                    // Capture the return value before publishing: a function
                    // may return one of its own `&mut` params, and publishing
                    // moves that register's value into the cell.
                    let ret = registers[value as usize].clone();
                    publish_ref_cells(&ref_cells, registers);
                    return Ok(RunControl::Return(ret));
                }
                Op::ReturnUnit => {
                    publish_ref_cells(&ref_cells, registers);
                    return Ok(RunControl::Return(Value::Unit));
                }
                Op::ClearRegs { start, count } => {
                    let from = start as usize;
                    let to = from + count as usize;
                    for slot in &mut registers[from..to] {
                        *slot = Value::Void;
                    }
                }
                Op::Panic { msg } => {
                    let message = match &chunk.consts[msg as usize] {
                        Value::String(s) => s.as_str().to_string(),
                        _ => "panic".to_string(),
                    };
                    return Err(RuntimeError::Panic(message));
                }
                Op::TypeError { msg } => {
                    let message = match &chunk.consts[msg as usize] {
                        Value::String(s) => s.as_str().to_string(),
                        _ => "type error".to_string(),
                    };
                    return Err(RuntimeError::Type(message));
                }
                Op::MethodCall {
                    dst,
                    receiver,
                    name_idx,
                    args,
                    argc,
                    cache_idx,
                } => {
                    // Inline-cache probe. We key the slot on the
                    // *receiver* type (interned struct-name pointer
                    // or a per-variant constant). Hit returns the
                    // resolved `Global` directly, skipping the
                    // qualified-key build + double `HashMap::get`
                    // lookup chain that dominates tight per-element
                    // method call loops.
                    let name = &*chunk.globals[name_idx as usize];
                    let argc_usz = argc as usize;
                    let total = argc_usz + 1;
                    let recv_token = type_token(&registers[receiver as usize]);
                    let method_name = Some(SmolStr::from_str(name));
                    let live_generation = self.globals_generation();
                    // Two-tier IC probe. The hottest hit is the
                    // builtin fn-pointer fast path: the slot's
                    // `builtin_fn` is the resolved
                    // `fn(&[Value]) -> RuntimeResult<Value>`,
                    // called directly with no `match Global { … }`
                    // chain. Closures / JIT-promoted bodies fall to
                    // the slower `resolved` field.
                    type BuiltinFn = fn(&[Value]) -> RuntimeResult<Value>;
                    let (cached_builtin, cached_general): (Option<BuiltinFn>, Option<Global>) =
                        if recv_token != 0 {
                            // Read-only borrow on the IC hit path
                            // (>99 % of calls in steady state).
                            // Miss-and-fill below takes borrow_mut.
                            let cache = state.call_caches.borrow();
                            let slot = &cache[cache_idx as usize];
                            if slot.type_token == recv_token
                                && slot.callee_name.as_ref() == method_name.as_ref()
                                && slot.generation == live_generation
                            {
                                let general =
                                    slot.fn_chunk.as_ref().map(|c| Global::Fn(Arc::clone(c)));
                                (slot.builtin_fn, general)
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        };
                    let cached = cached_general;

                    // Materialise call args. Stack buffer for argc
                    // ≤ 7 (recv + 7 args fits 8 slots).
                    const SMALL: usize = 8;
                    let result = if total <= SMALL {
                        let mut buf: [Value; SMALL] = [
                            Value::Void,
                            Value::Void,
                            Value::Void,
                            Value::Void,
                            Value::Void,
                            Value::Void,
                            Value::Void,
                            Value::Void,
                        ];
                        buf[0] = if name == "next" {
                            registers[receiver as usize].clone()
                        } else {
                            crate::stdlib_builtins::iter::fork_lazy_iter_value(
                                &registers[receiver as usize],
                            )
                        };
                        for i in 0..argc_usz {
                            // 0.7.0 flag::Cell auto-deref at the
                            // call boundary - same rule as `Op::Call`.
                            let raw = registers[args as usize + i].clone();
                            buf[i + 1] = auto_deref_cell(&raw).unwrap_or(raw);
                        }
                        if let Some(call_fn) = cached_builtin {
                            // Hottest hit path: direct fn ptr call,
                            // no enum match.
                            call_fn(&buf[..total])?
                        } else if let Some(g) = cached {
                            // Cached non-builtin (closure / JIT). The buffer
                            // dies here, so the callee takes the values
                            // outright rather than sharing them.
                            let mut v = self.pool.borrow_mut().take_args(total);
                            for slot in buf.iter_mut().take(total) {
                                v.push(std::mem::replace(slot, Value::Void));
                            }
                            apply_or_suspend_bytecode!(dst, g, v)
                        } else {
                            // Miss: full resolution + cache fill.
                            let r = self
                                .qualified_key(&buf[0], name)
                                .and_then(|qual| self.lookup_global(qual.as_ref()))
                                .or_else(|| self.lookup_builtin_method(name));
                            if recv_token != 0 {
                                if let Some(ref g) = r {
                                    let mut cache = state.call_caches.borrow_mut();
                                    cache[cache_idx as usize] = fill_cache_slot(
                                        recv_token,
                                        live_generation,
                                        method_name.clone(),
                                        g,
                                    );
                                }
                            }
                            match r {
                                Some(Global::Value(Value::Builtin(builtin_inner))) => {
                                    (builtin_inner.call)(&buf[..total])?
                                }
                                Some(g) => {
                                    let mut v = self.pool.borrow_mut().take_args(total);
                                    for slot in buf.iter_mut().take(total) {
                                        v.push(std::mem::replace(slot, Value::Void));
                                    }
                                    apply_or_suspend_bytecode!(dst, g, v)
                                }
                                None => {
                                    return Err(self.unresolved(name));
                                }
                            }
                        }
                    } else {
                        let recv = if name == "next" {
                            registers[receiver as usize].clone()
                        } else {
                            crate::stdlib_builtins::iter::fork_lazy_iter_value(
                                &registers[receiver as usize],
                            )
                        };
                        let mut call_args = self.pool.borrow_mut().take_args(total);
                        call_args.push(recv);
                        for i in 0..argc_usz {
                            // 0.7.0 flag::Cell auto-deref at the
                            // call boundary - same rule as `Op::Call`.
                            let raw = registers[args as usize + i].clone();
                            call_args.push(auto_deref_cell(&raw).unwrap_or(raw));
                        }
                        if let Some(call_fn) = cached_builtin {
                            let out = call_fn(&call_args)?;
                            self.pool.borrow_mut().give_args(call_args);
                            out
                        } else if let Some(g) = cached {
                            apply_or_suspend_bytecode!(dst, g, call_args)
                        } else {
                            let r = self
                                .qualified_key(&call_args[0], name)
                                .and_then(|qual| self.lookup_global(qual.as_ref()))
                                .or_else(|| self.lookup_builtin_method(name));
                            if recv_token != 0 {
                                if let Some(ref g) = r {
                                    let mut cache = state.call_caches.borrow_mut();
                                    cache[cache_idx as usize] = fill_cache_slot(
                                        recv_token,
                                        live_generation,
                                        method_name.clone(),
                                        g,
                                    );
                                }
                            }
                            match r {
                                Some(Global::Value(Value::Builtin(builtin_inner))) => {
                                    (builtin_inner.call)(&call_args)?
                                }
                                Some(g) => apply_or_suspend_bytecode!(dst, g, call_args),
                                None => {
                                    return Err(self.unresolved(name));
                                }
                            }
                        }
                    };
                    registers[dst as usize] = result;
                }
                Op::StreamWriteByte {
                    dst,
                    stream_reg,
                    byte_reg,
                } => {
                    // Super-instruction for `<stream>.write_byte(<b>)`.
                    // Hot path: receiver is a `Value::Struct{name="Stream",
                    // fields=[("fd", Int(fd))]}`, byte is a
                    // `Value::Int`. Inline the same work
                    // `builtins::builtin_stream_write_byte` does
                    // but without going through the
                    // MethodCall + IC + Vec-args path.
                    let recv = &registers[stream_reg as usize];
                    let byte_val = &registers[byte_reg as usize];
                    let stream_match = matches!(
                        recv,
                        Value::Struct(inner) if inner.name == "Stream"
                    );
                    let byte_match = matches!(byte_val, Value::Int(_));
                    if stream_match && byte_match {
                        let fd = match recv {
                            Value::Struct(inner) => {
                                let mut fd = 1i64;
                                for (n, v) in &inner.fields {
                                    if (*n) == "fd" {
                                        if let Value::Int(f) = v {
                                            fd = *f;
                                            break;
                                        }
                                    }
                                }
                                fd
                            }
                            _ => 1,
                        };
                        let b = match byte_val {
                            Value::Int(n) => *n,
                            _ => unreachable!(),
                        };
                        crate::builtins::stream_write_one_byte(fd, b);
                        registers[dst as usize] = Value::Unit;
                    } else {
                        // Fallback: full method dispatch through
                        // the regular qualified-key path. Keeps
                        // the op correct for any user-defined
                        // `write_byte` method on a non-Stream
                        // receiver, at the cost of one extra
                        // hash lookup per call (uncached for the
                        // miss case, since this op doesn't carry
                        // an IC slot).
                        let recv_clone = recv.clone();
                        let byte_clone = byte_val.clone();
                        let resolved = match &recv_clone {
                            Value::Struct(_) | Value::Channel(_) => self
                                .qualified_key(&recv_clone, "write_byte")
                                .and_then(|q| self.lookup_global(q.as_ref())),
                            _ => None,
                        }
                        .or_else(|| self.lookup_builtin_method("write_byte"));
                        let args = vec![recv_clone, byte_clone];
                        let result = match resolved {
                            Some(Global::Value(Value::Builtin(builtin_inner))) => {
                                (builtin_inner.call)(&args)?
                            }
                            Some(g) => apply_or_suspend_bytecode!(dst, g, args),
                            None => {
                                return Err(self.unresolved("write_byte"));
                            }
                        };
                        registers[dst as usize] = result;
                    }
                }
                Op::U8VecSetByte {
                    dst,
                    u8vec_reg,
                    idx_reg,
                    byte_reg,
                } => {
                    // Super-instruction for `<u8vec>.set_byte(<idx>, <byte>)`.
                    // Same fast-path / fallback shape as
                    // [`Op::StreamWriteByte`]. The inline helper
                    // returns `false` when the handle has been
                    // dropped (extremely rare - U8Vec is held by
                    // the user-side struct for its full lifetime),
                    // letting us fall through to the generic
                    // dispatch path for correctness.
                    let recv = &registers[u8vec_reg as usize];
                    let idx_val = &registers[idx_reg as usize];
                    let byte_val = &registers[byte_reg as usize];
                    let fast = matches!(
                        recv,
                        Value::Struct(inner) if inner.name == "U8Vec"
                    ) && matches!(idx_val, Value::Int(_))
                        && matches!(byte_val, Value::Int(_));
                    if fast {
                        let handle = match recv {
                            Value::Struct(inner) => {
                                let mut h = 0i64;
                                for (n, v) in &inner.fields {
                                    if (*n) == "handle" {
                                        if let Value::Int(x) = v {
                                            h = *x;
                                            break;
                                        }
                                    }
                                }
                                h
                            }
                            _ => unreachable!(),
                        };
                        let idx = match idx_val {
                            Value::Int(n) => *n,
                            _ => unreachable!(),
                        };
                        let byte = match byte_val {
                            Value::Int(n) => *n,
                            _ => unreachable!(),
                        };
                        if crate::builtins::u8vec_set_byte_inline(handle, idx, byte) {
                            registers[dst as usize] = Value::Unit;
                            continue;
                        }
                    }
                    // Fallback: same generic-dispatch shape as
                    // `StreamWriteByte`'s miss path.
                    let recv_clone = recv.clone();
                    let idx_clone = idx_val.clone();
                    let byte_clone = byte_val.clone();
                    let resolved = match &recv_clone {
                        Value::Struct(_) => self
                            .qualified_key(&recv_clone, "set_byte")
                            .and_then(|q| self.lookup_global(q.as_ref())),
                        _ => None,
                    }
                    .or_else(|| self.lookup_builtin_method("set_byte"));
                    let args = vec![recv_clone, idx_clone, byte_clone];
                    let result = match resolved {
                        Some(Global::Value(Value::Builtin(builtin_inner))) => {
                            (builtin_inner.call)(&args)?
                        }
                        Some(g) => apply_or_suspend_bytecode!(dst, g, args),
                        None => {
                            return Err(self.unresolved("set_byte"));
                        }
                    };
                    registers[dst as usize] = result;
                }
                Op::U8VecGetByte {
                    dst_i,
                    u8vec_reg,
                    idx_reg,
                } => {
                    let recv = &registers[u8vec_reg as usize];
                    let idx_val = &registers[idx_reg as usize];
                    let fast = matches!(
                        recv,
                        Value::Struct(inner) if inner.name == "U8Vec"
                    ) && matches!(idx_val, Value::Int(_));
                    if fast {
                        let handle = match recv {
                            Value::Struct(inner) => {
                                let mut h = 0i64;
                                for (n, v) in &inner.fields {
                                    if (*n) == "handle" {
                                        if let Value::Int(x) = v {
                                            h = *x;
                                            break;
                                        }
                                    }
                                }
                                h
                            }
                            _ => unreachable!(),
                        };
                        let idx = match idx_val {
                            Value::Int(n) => *n,
                            _ => unreachable!(),
                        };
                        if let Some(b) = crate::builtins::u8vec_get_byte_inline(handle, idx) {
                            // SAFETY: `dst_i` is a compile-allocated
                            // i64 register slot.
                            unsafe {
                                *ints.get_unchecked_mut(dst_i as usize) = b;
                            }
                            continue;
                        }
                    }
                    // Fallback: dispatch through the generic
                    // `get_byte` builtin, then unbox the resulting
                    // `Value::Int` into the typed register.
                    let recv_clone = recv.clone();
                    let idx_clone = idx_val.clone();
                    let resolved = match &recv_clone {
                        Value::Struct(_) => self
                            .qualified_key(&recv_clone, "get_byte")
                            .and_then(|q| self.lookup_global(q.as_ref())),
                        _ => None,
                    }
                    .or_else(|| self.lookup_builtin_method("get_byte"));
                    let args = vec![recv_clone, idx_clone];
                    let result = match resolved {
                        Some(Global::Value(Value::Builtin(builtin_inner))) => {
                            (builtin_inner.call)(&args)?
                        }
                        Some(g) => self.apply(g, args)?,
                        None => {
                            return Err(self.unresolved("get_byte"));
                        }
                    };
                    let n = match result {
                        Value::Int(n) => n,
                        _ => 0,
                    };
                    unsafe {
                        *ints.get_unchecked_mut(dst_i as usize) = n;
                    }
                }
                Op::StrSubstring {
                    dst,
                    recv_reg,
                    start_reg,
                    end_reg,
                } => {
                    // Fast path: string receiver + integer bounds. The
                    // borrow of `registers` ends with the match, since it
                    // yields an owned `SmolStr`, so the destination write
                    // below does not alias it.
                    let bounds =
                        match (&registers[start_reg as usize], &registers[end_reg as usize]) {
                            (Value::Int(a), Value::Int(b)) => Some((*a, *b)),
                            _ => None,
                        };
                    let fast = match (&registers[recv_reg as usize], bounds) {
                        (Value::String(s), Some((a, b))) => {
                            Some(crate::builtins::str_substring_inline(s, a, b))
                        }
                        _ => None,
                    };
                    if let Some(out) = fast {
                        registers[dst as usize] = Value::String(out);
                        continue;
                    }
                    // Fallback: generic `substring` dispatch, same shape as
                    // `U8VecGetByte`'s miss path.
                    let recv_clone = registers[recv_reg as usize].clone();
                    let start_clone = registers[start_reg as usize].clone();
                    let end_clone = registers[end_reg as usize].clone();
                    let resolved = match &recv_clone {
                        Value::Struct(_) => self
                            .qualified_key(&recv_clone, "substring")
                            .and_then(|q| self.lookup_global(q.as_ref())),
                        _ => None,
                    }
                    .or_else(|| self.lookup_builtin_method("substring"));
                    let args = vec![recv_clone, start_clone, end_clone];
                    let result = match resolved {
                        Some(Global::Value(Value::Builtin(builtin_inner))) => {
                            (builtin_inner.call)(&args)?
                        }
                        Some(g) => apply_or_suspend_bytecode!(dst, g, args),
                        None => {
                            return Err(self.unresolved("substring"));
                        }
                    };
                    registers[dst as usize] = result;
                }
                Op::MapIncMethod {
                    dst,
                    map_reg,
                    key_reg,
                    by_reg,
                } => {
                    let by = match &registers[by_reg as usize] {
                        Value::Int(n) => *n,
                        _ => 1,
                    };
                    // Each arm holds the map lock only within the match and
                    // yields an owned `Value::Int`, so the borrow of
                    // `registers` is released before the destination write.
                    let result = match &registers[map_reg as usize] {
                        Value::StrIntMap(map) => {
                            if let Value::String(k) = &registers[key_reg as usize] {
                                let mut guard = map.lock();
                                let new_val = if let Some(slot) = guard.get_mut(k) {
                                    *slot += by;
                                    *slot
                                } else {
                                    guard.insert(k.clone(), by);
                                    by
                                };
                                Some(Value::Int(new_val))
                            } else {
                                None
                            }
                        }
                        Value::IntMap(map) => {
                            if let Value::Int(k) = &registers[key_reg as usize] {
                                let mut guard = map.lock();
                                let new_val = guard.get(k).copied().unwrap_or(0) + by;
                                guard.insert(*k, new_val);
                                Some(Value::Int(new_val))
                            } else {
                                None
                            }
                        }
                        Value::Map(map) => {
                            let key = MapKey::from_value(&registers[key_reg as usize]);
                            let mut guard = map.lock();
                            let new_val = match guard.get(&key) {
                                Some(Value::Int(v)) => v + by,
                                _ => by,
                            };
                            guard.insert(key, Value::Int(new_val));
                            Some(Value::Int(new_val))
                        }
                        _ => None,
                    };
                    if let Some(v) = result {
                        registers[dst as usize] = v;
                        continue;
                    }
                    // Fallback: generic `inc` dispatch (non-map receiver or
                    // key-type mismatch), same shape as `StrSubstring`'s miss.
                    let map_clone = registers[map_reg as usize].clone();
                    let key_clone = registers[key_reg as usize].clone();
                    let by_clone = registers[by_reg as usize].clone();
                    let resolved = match &map_clone {
                        Value::Struct(_) => self
                            .qualified_key(&map_clone, "inc")
                            .and_then(|q| self.lookup_global(q.as_ref())),
                        _ => None,
                    }
                    .or_else(|| self.lookup_builtin_method("inc"));
                    let args = vec![map_clone, key_clone, by_clone];
                    let result = match resolved {
                        Some(Global::Value(Value::Builtin(builtin_inner))) => {
                            (builtin_inner.call)(&args)?
                        }
                        Some(g) => apply_or_suspend_bytecode!(dst, g, args),
                        None => {
                            return Err(self.unresolved("inc"));
                        }
                    };
                    registers[dst as usize] = result;
                }
                Op::MapInsert {
                    dst,
                    map_reg,
                    key_reg,
                    value_reg,
                } => {
                    let Value::Map(map) = &registers[map_reg as usize] else {
                        return Err(RuntimeError::Type(
                            "MapInsert: receiver lost map invariant".to_string(),
                        ));
                    };
                    let key = MapKey::from_value(&registers[key_reg as usize]);
                    let value = registers[value_reg as usize].clone();
                    map.lock().insert(key, value);
                    registers[dst as usize] = Value::Map(Arc::clone(map));
                }
                Op::Wide { idx } => {
                    // Side-table indirection for the rare
                    // 6-payload-field ops. Reads the actual
                    // operation out of `chunk.wide_ops` and
                    // dispatches inline. Adding a new wide op
                    // means adding a new arm to the inner match
                    // here AND a new variant to `WideOp` in
                    // bytecode.rs (and a matching emit site in
                    // compile.rs). The hot path stays unchanged
                    // for every non-wide op.
                    let wide = unsafe { chunk.wide_ops.get_unchecked(idx as usize) };
                    match wide {
                        crate::bytecode::WideOp::StrConcatPadI64 {
                            dst,
                            prefix,
                            value,
                            width,
                            fill,
                            align,
                        } => {
                            let prefix = match &registers[*prefix as usize] {
                                Value::String(value) => value.as_str(),
                                _ => "",
                            };
                            let width = match &registers[*width as usize] {
                                Value::Int(value) if *value >= 0 => *value as usize,
                                _ => 0,
                            };
                            let fill = match &registers[*fill as usize] {
                                Value::Char(value) => *value,
                                Value::Int(value) => char::from_u32(*value as u32).unwrap_or(' '),
                                _ => ' ',
                            };
                            let align = match &registers[*align as usize] {
                                Value::Int(value) => *value,
                                _ => 0,
                            };
                            let value = match &registers[*value as usize] {
                                Value::Int(value) => *value,
                                _ => 0,
                            };
                            let mut number = itoa::Buffer::new();
                            let rendered = number.format(value);
                            let total = width.saturating_sub(rendered.len());
                            let mut out = String::with_capacity(
                                prefix.len()
                                    + rendered.len()
                                    + total.saturating_mul(fill.len_utf8()),
                            );
                            out.push_str(prefix);
                            if align == gossamer_ast::PAD_ALIGN_SIGN_AWARE_ZERO {
                                let split = gossamer_ast::sign_aware_prefix_len(rendered);
                                out.push_str(&rendered[..split]);
                                out.extend(std::iter::repeat_n('0', total));
                                out.push_str(&rendered[split..]);
                            } else {
                                let (left, right) = match align {
                                    1 => (0, total),
                                    2 => (total / 2, total - total / 2),
                                    _ => (total, 0),
                                };
                                out.extend(std::iter::repeat_n(fill, left));
                                out.push_str(rendered);
                                out.extend(std::iter::repeat_n(fill, right));
                            }
                            registers[*dst as usize] = Value::String(out.into());
                        }
                        crate::bytecode::WideOp::MapIncAt {
                            dst,
                            map_reg,
                            seq_reg,
                            start_reg,
                            len_reg,
                            by_reg,
                        } => {
                            let dst = *dst;
                            let map_reg = *map_reg;
                            let seq_reg = *seq_reg;
                            let start_reg = *start_reg;
                            let len_reg = *len_reg;
                            let by_reg = *by_reg;
                            // Zero-copy slice-hash counter that mirrors
                            // `*m.entry(&seq[start..start+len]).or_insert(0) += by`.
                            let result_int: i64 =
                                if let Value::Map(map) = &registers[map_reg as usize] {
                                    let seq_bytes: &[u8] = match &registers[seq_reg as usize] {
                                        Value::String(s) => s.as_bytes(),
                                        _ => &[],
                                    };
                                    let start = match &registers[start_reg as usize] {
                                        Value::Int(n) if *n >= 0 => *n as usize,
                                        _ => 0,
                                    };
                                    let len = match &registers[len_reg as usize] {
                                        Value::Int(n) if *n >= 0 => *n as usize,
                                        _ => 0,
                                    };
                                    let by = match &registers[by_reg as usize] {
                                        Value::Int(n) => *n,
                                        _ => 1,
                                    };
                                    if len == 0 || start + len > seq_bytes.len() {
                                        0
                                    } else {
                                        let key_bytes = &seq_bytes[start..start + len];
                                        // SAFETY: `seq_bytes` came from a
                                        // UTF-8 `String`, so any sub-slice
                                        // on a char boundary is also UTF-8.
                                        // ASCII inputs are always safe.
                                        let key_str =
                                            unsafe { std::str::from_utf8_unchecked(key_bytes) };
                                        let key = MapKey::Str(crate::value::SmolStr::from(
                                            key_str.to_string(),
                                        ));
                                        let mut guard = map.lock();
                                        let entry = guard.entry(key).or_insert(Value::Int(0));
                                        let new_val = match entry {
                                            Value::Int(cur) => *cur + by,
                                            _ => by,
                                        };
                                        *entry = Value::Int(new_val);
                                        new_val
                                    }
                                } else {
                                    0
                                };
                            registers[dst as usize] = Value::Int(result_int);
                        }
                        crate::bytecode::WideOp::BuildFloatArray {
                            dst_v,
                            name_idx,
                            fields_idx,
                            stride,
                            elem_count,
                            first_f,
                        } => {
                            let dst_v = *dst_v;
                            let name_idx = *name_idx;
                            let fields_idx = *fields_idx;
                            let stride = *stride;
                            let elem_count = *elem_count;
                            let first_f = *first_f;
                            let Value::String(name_arc) = &chunk.consts[name_idx as usize] else {
                                return Err(RuntimeError::Panic(
                                    "BuildFloatArray: name must be string const".to_string(),
                                ));
                            };
                            let name = name_arc.as_str().to_string();
                            let Value::Array(field_names_arr) = &chunk.consts[fields_idx as usize]
                            else {
                                return Err(RuntimeError::Panic(
                                    "BuildFloatArray: fields must be array of strings".to_string(),
                                ));
                            };
                            let field_names: Vec<String> = field_names_arr
                                .iter()
                                .filter_map(|v| match v {
                                    Value::String(s) => Some(s.as_str().to_string()),
                                    _ => None,
                                })
                                .collect();
                            let total = stride as usize * elem_count as usize;
                            let start = first_f as usize;
                            let end = start + total;
                            let data: Vec<f64> = floats[start..end].to_vec();
                            registers[dst_v as usize] = Value::float_array(
                                name,
                                stride,
                                Arc::new(field_names),
                                Arc::new(data),
                            );
                        }
                        crate::bytecode::WideOp::BuildFloatArrayFromStructs {
                            dst_v,
                            first_v,
                            elem_count,
                            name_idx,
                            fields_idx,
                        } => {
                            let Value::String(name) = &chunk.consts[*name_idx as usize] else {
                                return Err(RuntimeError::Panic(
                                    "BuildFloatArrayFromStructs: name must be string const"
                                        .to_string(),
                                ));
                            };
                            let Value::Array(field_names_values) =
                                &chunk.consts[*fields_idx as usize]
                            else {
                                return Err(RuntimeError::Panic(
                                    "BuildFloatArrayFromStructs: fields must be string array"
                                        .to_string(),
                                ));
                            };
                            let field_names: Vec<String> = field_names_values
                                .iter()
                                .map(|value| match value {
                                    Value::String(field) => Ok(field.as_str().to_owned()),
                                    _ => Err(RuntimeError::Panic(
                                        "BuildFloatArrayFromStructs: invalid field name"
                                            .to_string(),
                                    )),
                                })
                                .collect::<RuntimeResult<_>>()?;
                            let mut data =
                                Vec::with_capacity(usize::from(*elem_count) * field_names.len());
                            for index in 0..*elem_count {
                                let value = &registers[usize::from(*first_v + index)];
                                let Value::Struct(inner) = value else {
                                    return Err(RuntimeError::Panic(
                                        "BuildFloatArrayFromStructs: element must be struct"
                                            .to_string(),
                                    ));
                                };
                                for field_name in &field_names {
                                    let Some(offset) = inner.fields.position(field_name) else {
                                        return Err(RuntimeError::Panic(
                                            "BuildFloatArrayFromStructs: missing field".to_string(),
                                        ));
                                    };
                                    let Value::Float(field) = inner.fields[offset] else {
                                        return Err(RuntimeError::Panic(
                                            "BuildFloatArrayFromStructs: field must be f64"
                                                .to_string(),
                                        ));
                                    };
                                    data.push(field);
                                }
                            }
                            registers[*dst_v as usize] = Value::float_array(
                                name.as_str(),
                                u16::try_from(field_names.len()).map_err(|_| {
                                    RuntimeError::Panic(
                                        "BuildFloatArrayFromStructs: too many fields".to_string(),
                                    )
                                })?,
                                Arc::new(field_names),
                                Arc::new(data),
                            );
                        }
                    }
                }
                Op::MapInc {
                    dst,
                    map_reg,
                    key_reg,
                    by_reg,
                } => {
                    // Fused `m.insert(k, m.get_or(k, 0) + by)`. The
                    // compiler only emits this op for receivers
                    // statically typed `HashMap`, so the fast arm
                    // is the only one that runs in practice. The
                    // generic arm handles polymorphic-by-promotion
                    // value shapes (i.e. a slot already holding
                    // something other than `Value::Int`) by going
                    // through the normal `bin_arith` path.
                    let result = if let Value::Map(map) = &registers[map_reg as usize] {
                        let key = MapKey::from_value(&registers[key_reg as usize]);
                        let by_val = &registers[by_reg as usize];
                        let mut guard = map.lock();
                        let entry = guard.entry(key).or_insert(Value::Int(0));
                        if let (Value::Int(cur), Value::Int(b)) = (&*entry, by_val) {
                            *entry = Value::Int(*cur + *b);
                        } else {
                            let cur = entry.clone();
                            let sum =
                                bin_arith(&cur, by_val, i64::wrapping_add, |a, b| a + b, "+")?;
                            *entry = sum;
                        }
                        let cloned = Arc::clone(map);
                        drop(guard);
                        Value::Map(cloned)
                    } else {
                        // Receiver isn't a Map (shouldn't happen for
                        // a HashMap-typed receiver, but stay total).
                        registers[map_reg as usize].clone()
                    };
                    registers[dst as usize] = result;
                }
                Op::IndexGet { dst, base, index } => {
                    let b = &registers[base as usize];
                    let i = &registers[index as usize];
                    registers[dst as usize] = index_get(b, i)?;
                }
                Op::StrByteAt { dst, recv, idx } => {
                    // Matches `builtin_str_byte_at`: non-string receiver,
                    // non-integer index, or out-of-range index all yield 0.
                    let byte = if let Value::String(s) = &registers[recv as usize] {
                        let i = match &registers[idx as usize] {
                            Value::Int(n) => *n,
                            other => match auto_deref_cell(other) {
                                Some(Value::Int(n)) => n,
                                _ => -1,
                            },
                        };
                        let bytes = s.as_str().as_bytes();
                        if i < 0 || (i as usize) >= bytes.len() {
                            0
                        } else {
                            i64::from(bytes[i as usize])
                        }
                    } else {
                        0
                    };
                    registers[dst as usize] = Value::Int(byte);
                }
                Op::StrByteAtI64 { dst_i, recv, idx_i } => unsafe {
                    // Static typing and bytecode validation guarantee the
                    // register files. Keep the hot path free of bounds checks
                    // and `Value::Int` allocation.
                    let i = *ints.get_unchecked(idx_i as usize);
                    let byte = match registers.get_unchecked(recv as usize) {
                        Value::String(s) => {
                            let bytes = s.as_str().as_bytes();
                            if i < 0 || (i as usize) >= bytes.len() {
                                0
                            } else {
                                i64::from(*bytes.get_unchecked(i as usize))
                            }
                        }
                        _ => 0,
                    };
                    *ints.get_unchecked_mut(dst_i as usize) = byte;
                },
                Op::StrByteAtAddI64 {
                    dst_i,
                    lhs_i,
                    recv,
                    idx_i,
                } => unsafe {
                    let i = *ints.get_unchecked(idx_i as usize);
                    let byte = match registers.get_unchecked(recv as usize) {
                        Value::String(s) => {
                            let bytes = s.as_str().as_bytes();
                            if i < 0 || (i as usize) >= bytes.len() {
                                0
                            } else {
                                i64::from(*bytes.get_unchecked(i as usize))
                            }
                        }
                        _ => 0,
                    };
                    *ints.get_unchecked_mut(dst_i as usize) =
                        ints.get_unchecked(lhs_i as usize).wrapping_add(byte);
                },
                Op::StrLenI64 { dst_i, recv } => unsafe {
                    let len = match registers.get_unchecked(recv as usize) {
                        Value::String(value) => value.len() as i64,
                        _ => 0,
                    };
                    *ints.get_unchecked_mut(dst_i as usize) = len;
                },
                Op::IndexGetChecked { dst, base, index } => {
                    let b = &registers[base as usize];
                    let i = &registers[index as usize];
                    registers[dst as usize] = index_get_checked(b, i)?;
                }
                Op::IndexSet { base, index, value } => {
                    let new_value = registers[value as usize].clone();
                    let i = &registers[index as usize];
                    let raw = super::index_value(i)?;
                    crate::stdlib_builtins::iter::note_vec_element_replacement(
                        &registers[base as usize],
                        raw,
                        &new_value,
                    );
                    let b = &mut registers[base as usize];
                    // An `[f64]` vec whose elements so far were all
                    // integer-valued sits in `IntArray` storage (an `[i64]`
                    // can never receive a float store); widen it to flat
                    // float storage before the store below.
                    if let (Value::IntArray(data), Value::Float(_)) = (&*b, &new_value) {
                        *b = Value::FloatVec(Arc::new(data.iter().map(|n| *n as f64).collect()));
                    }
                    match b {
                        Value::Array(items) | Value::Tuple(items) => {
                            if raw < 0 || raw as usize >= items.len() {
                                return Err(crate::vm::index_oob_panic(raw, items.len()));
                            }
                            Arc::make_mut(items)[raw as usize] = new_value;
                        }
                        Value::IntArray(data) => {
                            if raw < 0 || raw as usize >= data.len() {
                                return Err(crate::vm::index_oob_panic(raw, data.len()));
                            }
                            match new_value {
                                Value::Int(n) => Arc::make_mut(data)[raw as usize] = n,
                                _ => {
                                    return Err(RuntimeError::Type(
                                        "IndexSet on IntArray expects i64 value".to_string(),
                                    ));
                                }
                            }
                        }
                        Value::ByteArray(data) => {
                            if raw < 0 || raw as usize >= data.len() {
                                return Err(crate::vm::index_oob_panic(raw, data.len()));
                            }
                            match new_value {
                                Value::Int(n) => Arc::make_mut(data)[raw as usize] = n as u8,
                                _ => {
                                    return Err(RuntimeError::Type(
                                        "IndexSet on ByteArray expects u8 value".to_string(),
                                    ));
                                }
                            }
                        }
                        Value::InlineByteArray(data) => {
                            if raw < 0 || raw as usize >= data.len() {
                                return Err(crate::vm::index_oob_panic(raw, data.len()));
                            }
                            match new_value {
                                Value::Int(n) => Arc::make_mut(data)[raw as usize] = n as u8,
                                _ => {
                                    return Err(RuntimeError::Type(
                                        "IndexSet on byte array expects u8 value".to_string(),
                                    ));
                                }
                            }
                        }
                        Value::ByteVec(data) => {
                            if raw < 0 || raw as usize >= data.len() {
                                return Err(crate::vm::index_oob_panic(raw, data.len()));
                            }
                            match new_value {
                                Value::Int(n) => {
                                    Arc::make_mut(data)[raw as usize] =
                                        u8::try_from(n).map_err(|_| {
                                            RuntimeError::Type(
                                                "IndexSet on byte vector expects u8 value"
                                                    .to_string(),
                                            )
                                        })?;
                                }
                                _ => {
                                    return Err(RuntimeError::Type(
                                        "IndexSet on byte vector expects u8 value".to_string(),
                                    ));
                                }
                            }
                        }
                        Value::FloatVec(data) => {
                            if raw < 0 || raw as usize >= data.len() {
                                return Err(crate::vm::index_oob_panic(raw, data.len()));
                            }
                            match new_value {
                                Value::Float(f) => Arc::make_mut(data)[raw as usize] = f,
                                Value::Int(n) => Arc::make_mut(data)[raw as usize] = n as f64,
                                _ => {
                                    return Err(RuntimeError::Type(
                                        "IndexSet on FloatVec expects f64 value".to_string(),
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{b}` is not indexable"
                            )));
                        }
                    }
                }
                Op::FieldGet {
                    dst,
                    receiver,
                    name_idx,
                    cache_idx,
                } => {
                    // PEP 659-style inline cache. Fast path: when the
                    // observed receiver shape (struct-name interned
                    // pointer) matches the slot's `type_token`, jump
                    // straight to `inner.fields[offset].1.clone()` -
                    // skipping the linear name-scan in `field_get`.
                    let recv = &registers[receiver as usize];
                    if let Value::Struct(inner) = recv {
                        // `inner.name` is a globally-interned `&'static str`
                        // (canonical, pointer-stable across every clone of
                        // any instance of this type), so its address is a
                        // ready-made guard token - no second hash needed.
                        let token = u64::from(inner.name.id());
                        let slot = &state.field_caches[cache_idx as usize];
                        if slot.type_token.get() == token {
                            let off = slot.offset.get() as usize;
                            if off < inner.fields.len() {
                                registers[dst as usize] = inner.fields[off].clone();
                                continue;
                            }
                        }
                        // Miss: linear-scan, then refill the slot for next
                        // time. Every instance of a struct shares one
                        // globally-interned `name`, so the token compare
                        // above is `O(1)` after fill.
                        let field_name = match &chunk.consts[name_idx as usize] {
                            Value::String(s) => s.as_str(),
                            _ => {
                                return Err(RuntimeError::Panic(
                                    "FieldGet: name must be string const".to_string(),
                                ));
                            }
                        };
                        if let Some(pos) = inner.fields.position(field_name) {
                            slot.type_token.set(token);
                            slot.offset.set(pos as u16);
                            let value = inner.fields[pos].clone();
                            registers[dst as usize] = value;
                        } else {
                            registers[dst as usize] = Value::Unit;
                        }
                    } else {
                        let field_name = match &chunk.consts[name_idx as usize] {
                            Value::String(s) => s.clone(),
                            _ => {
                                return Err(RuntimeError::Panic(
                                    "FieldGet: name must be string const".to_string(),
                                ));
                            }
                        };
                        let v = field_get(recv, field_name.as_str())?;
                        registers[dst as usize] = v;
                    }
                }
                Op::FieldSet {
                    receiver,
                    name_idx,
                    value,
                } => {
                    let field_name = match &chunk.consts[name_idx as usize] {
                        Value::String(s) => s.clone(),
                        _ => {
                            return Err(RuntimeError::Panic(
                                "FieldSet: name must be string const".to_string(),
                            ));
                        }
                    };
                    let new_value = registers[value as usize].clone();
                    let recv = &mut registers[receiver as usize];
                    field_set(recv, field_name.as_str(), new_value)?;
                }
                Op::FieldSetI64ByOffset {
                    receiver,
                    offset,
                    value_i,
                } => {
                    let recv = &mut registers[receiver as usize];
                    let Value::Struct(inner) = recv else {
                        return Err(RuntimeError::Type(format!(
                            "field assignment on non-struct `{recv}`"
                        )));
                    };
                    let inner = Arc::make_mut(inner);
                    let Some((_, field)) = inner.fields.get_mut(offset as usize) else {
                        return Err(RuntimeError::Panic(
                            "struct field offset out of bounds".to_string(),
                        ));
                    };
                    *field = Value::Int(ints[value_i as usize]);
                }
                Op::VecPush { receiver, value } => {
                    let new_value = registers[value as usize].clone();
                    crate::stdlib_builtins::iter::note_vec_structural_mutation(
                        &registers[receiver as usize],
                    );
                    let recv = &mut registers[receiver as usize];
                    vec_push_value(recv, new_value);
                }
                Op::StrAppend { receiver, value } => {
                    // Read the RHS first (a cheap SmolStr clone: an Arc
                    // bump or inline copy), so the receiver can then be
                    // borrowed mutably without aliasing the value register
                    // (and `s += s` stays correct).
                    let rhs = match &registers[value as usize] {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    };
                    if let Some(rhs) = rhs {
                        if let Value::String(dst) = &mut registers[receiver as usize] {
                            dst.push_str(rhs.as_str());
                        }
                    }
                }
                Op::StrPush {
                    receiver,
                    value,
                    byte,
                } => {
                    let ch = if byte {
                        match &registers[value as usize] {
                            Value::Int(n) => Some(char::from(*n as u8)),
                            _ => None,
                        }
                    } else {
                        match &registers[value as usize] {
                            Value::Char(ch) => Some(*ch),
                            Value::Int(n) => char::from_u32(*n as u32),
                            _ => None,
                        }
                    };
                    if let Some(ch) = ch
                        && let Value::String(dst) = &mut registers[receiver as usize]
                    {
                        dst.push(ch);
                    }
                }
                Op::StrConcatI64 {
                    dst,
                    prefix,
                    value_i,
                } => {
                    let prefix = match &registers[prefix as usize] {
                        Value::String(value) => value.as_str(),
                        _ => "",
                    };
                    let mut out = String::with_capacity(prefix.len() + 20);
                    out.push_str(prefix);
                    let _ = write!(out, "{}", ints[value_i as usize]);
                    registers[dst as usize] = Value::String(out.into());
                }
                Op::VecPop { dst, receiver } => {
                    let has_item = match &registers[receiver as usize] {
                        Value::Array(items) => !items.is_empty(),
                        Value::IntArray(data) => !data.is_empty(),
                        Value::ByteArray(data) => !data.is_empty(),
                        Value::InlineByteArray(data) => !data.is_empty(),
                        Value::ByteVec(data) => !data.is_empty(),
                        Value::FloatVec(data) => !data.is_empty(),
                        _ => false,
                    };
                    if has_item {
                        crate::stdlib_builtins::iter::note_vec_structural_mutation(
                            &registers[receiver as usize],
                        );
                    }
                    let popped = match &mut registers[receiver as usize] {
                        Value::Array(items) => Arc::make_mut(items).pop(),
                        Value::IntArray(data) => Arc::make_mut(data).pop().map(Value::Int),
                        Value::ByteArray(data) => {
                            let mut values = data.to_vec();
                            let popped = values.pop().map(|value| Value::Int(i64::from(value)));
                            registers[receiver as usize] = Value::ByteVec(Arc::new(values));
                            popped
                        }
                        Value::InlineByteArray(data) => {
                            let mut values = data.to_vec();
                            let popped = values.pop().map(|value| Value::Int(i64::from(value)));
                            registers[receiver as usize] = Value::ByteVec(Arc::new(values));
                            popped
                        }
                        Value::ByteVec(data) => Arc::make_mut(data)
                            .pop()
                            .map(|value| Value::Int(i64::from(value))),
                        Value::FloatVec(data) => Arc::make_mut(data).pop().map(Value::Float),
                        _ => None,
                    };
                    registers[dst as usize] = match popped {
                        Some(v) => Value::variant("Some", vec![v]),
                        None => Value::variant("None", vec![]),
                    };
                }
                Op::VecInsert {
                    dst,
                    receiver,
                    index,
                    value,
                } => {
                    let idx = super::index_value(&registers[index as usize])?;
                    let len = match &registers[receiver as usize] {
                        Value::Array(items) => items.len() as i64,
                        Value::IntArray(data) => data.len() as i64,
                        Value::ByteArray(data) => data.len() as i64,
                        Value::InlineByteArray(data) => data.len() as i64,
                        Value::ByteVec(data) => data.len() as i64,
                        Value::FloatVec(data) => data.len() as i64,
                        _ => 0,
                    };
                    if idx < 0 || idx > len {
                        registers[dst as usize] = crate::builtins::slice_err(format!(
                            "insert: index {idx} out of bounds for length {len}"
                        ));
                        continue;
                    }
                    crate::stdlib_builtins::iter::note_vec_structural_mutation(
                        &registers[receiver as usize],
                    );
                    let new_value = registers[value as usize].clone();
                    let recv = &mut registers[receiver as usize];
                    // Same typed-storage routing as `Op::VecPush`: a scalar
                    // insert into an empty generic array switches it to flat
                    // storage, and a float insert widens an integer-valued
                    // `[f64]` out of `IntArray` storage first.
                    match (&*recv, &new_value) {
                        (Value::Array(items), Value::Int(_)) if items.is_empty() => {
                            *recv = Value::IntArray(Arc::new(Vec::new()));
                        }
                        (Value::Array(items), Value::Float(_)) if items.is_empty() => {
                            *recv = Value::FloatVec(Arc::new(Vec::new()));
                        }
                        (Value::IntArray(data), Value::Float(_)) => {
                            *recv =
                                Value::FloatVec(Arc::new(data.iter().map(|n| *n as f64).collect()));
                        }
                        _ => {}
                    }
                    match recv {
                        Value::Array(items) => {
                            let v = Arc::make_mut(items);
                            v.insert(idx as usize, new_value);
                        }
                        Value::IntArray(data) => {
                            if let Value::Int(n) = new_value {
                                let v = Arc::make_mut(data);
                                v.insert(idx as usize, n);
                            }
                        }
                        Value::ByteArray(data) => {
                            if let Value::Int(n) = new_value {
                                let mut values = data.to_vec();
                                values.insert(idx as usize, n as u8);
                                *recv = Value::ByteVec(Arc::new(values));
                            }
                        }
                        Value::InlineByteArray(data) => {
                            if let Value::Int(n) = new_value {
                                let mut values = data.to_vec();
                                values.insert(idx as usize, n as u8);
                                *recv = Value::ByteVec(Arc::new(values));
                            }
                        }
                        Value::ByteVec(data) => {
                            if let Value::Int(n) = new_value {
                                Arc::make_mut(data).insert(idx as usize, n as u8);
                            }
                        }
                        Value::FloatVec(data) => {
                            let f = match new_value {
                                Value::Float(f) => Some(f),
                                Value::Int(n) => Some(n as f64),
                                _ => None,
                            };
                            if let Some(f) = f {
                                let v = Arc::make_mut(data);
                                v.insert(idx as usize, f);
                            }
                        }
                        _ => {}
                    }
                    registers[dst as usize] = Value::variant("Ok", vec![Value::Unit]);
                }
                Op::VecSwap {
                    dst,
                    receiver,
                    a,
                    b,
                } => {
                    let a = super::index_value(&registers[a as usize])?;
                    let b = super::index_value(&registers[b as usize])?;
                    let len = match &registers[receiver as usize] {
                        Value::Array(values) => values.len(),
                        Value::IntArray(values) => values.len(),
                        Value::ByteArray(values) => values.len(),
                        Value::InlineByteArray(values) => values.len(),
                        Value::ByteVec(values) => values.len(),
                        Value::FloatVec(values) => values.len(),
                        _ => 0,
                    };
                    let valid = a >= 0
                        && b >= 0
                        && usize::try_from(a).is_ok_and(|index| index < len)
                        && usize::try_from(b).is_ok_and(|index| index < len);
                    if !valid {
                        return Err(RuntimeError::Panic(format!(
                            "vec swap index out of bounds: the len is {len} but the index is {}",
                            if a < 0 || a as usize >= len { a } else { b }
                        )));
                    }
                    let (a, b) = (a as usize, b as usize);
                    match &mut registers[receiver as usize] {
                        Value::Array(values) => Arc::make_mut(values).swap(a, b),
                        Value::IntArray(values) => Arc::make_mut(values).swap(a, b),
                        Value::ByteArray(values) => {
                            let mut owned = values.to_vec();
                            owned.swap(a, b);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(owned));
                        }
                        Value::InlineByteArray(values) => {
                            let mut owned = values.to_vec();
                            owned.swap(a, b);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(owned));
                        }
                        Value::ByteVec(values) => Arc::make_mut(values).swap(a, b),
                        Value::FloatVec(values) => Arc::make_mut(values).swap(a, b),
                        _ => {}
                    }
                    registers[dst as usize] = Value::Unit;
                }
                Op::VecSwapDiscard { receiver, a, b } => {
                    let a = super::index_value(&registers[a as usize])?;
                    let b = super::index_value(&registers[b as usize])?;
                    let len = match &registers[receiver as usize] {
                        Value::Array(values) => values.len(),
                        Value::IntArray(values) => values.len(),
                        Value::ByteArray(values) => values.len(),
                        Value::InlineByteArray(values) => values.len(),
                        Value::ByteVec(values) => values.len(),
                        Value::FloatVec(values) => values.len(),
                        _ => {
                            return Err(RuntimeError::Type(
                                "VecSwapDiscard: unsupported receiver".to_string(),
                            ));
                        }
                    };
                    if a < 0 || b < 0 || a as usize >= len || b as usize >= len {
                        return Err(RuntimeError::Panic(format!(
                            "vec swap index out of bounds: the len is {len} but the index is {}",
                            if a < 0 || a as usize >= len { a } else { b }
                        )));
                    }
                    let (a, b) = (a as usize, b as usize);
                    match &mut registers[receiver as usize] {
                        Value::Array(values) => Arc::make_mut(values).swap(a, b),
                        Value::IntArray(values) => Arc::make_mut(values).swap(a, b),
                        Value::ByteArray(values) => {
                            let mut owned = values.to_vec();
                            owned.swap(a, b);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(owned));
                        }
                        Value::InlineByteArray(values) => {
                            let mut owned = values.to_vec();
                            owned.swap(a, b);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(owned));
                        }
                        Value::ByteVec(values) => Arc::make_mut(values).swap(a, b),
                        Value::FloatVec(values) => Arc::make_mut(values).swap(a, b),
                        _ => unreachable!("validated Vec swap receiver"),
                    }
                }
                Op::VecRemove { receiver, index } => {
                    let idx = super::index_value(&registers[index as usize])?;
                    let len = match &registers[receiver as usize] {
                        Value::Array(items) => items.len() as i64,
                        Value::IntArray(data) => data.len() as i64,
                        Value::ByteArray(data) => data.len() as i64,
                        Value::InlineByteArray(data) => data.len() as i64,
                        Value::ByteVec(data) => data.len() as i64,
                        Value::FloatVec(data) => data.len() as i64,
                        _ => 0,
                    };
                    if idx < 0 || idx >= len {
                        return Err(RuntimeError::Panic(format!(
                            "remove: index {idx} out of bounds for length {len}"
                        )));
                    }
                    crate::stdlib_builtins::iter::note_vec_structural_mutation(
                        &registers[receiver as usize],
                    );
                    let idx = idx as usize;
                    match &mut registers[receiver as usize] {
                        Value::Array(items) => {
                            let v = Arc::make_mut(items);
                            v.remove(idx);
                        }
                        Value::IntArray(data) => {
                            let v = Arc::make_mut(data);
                            v.remove(idx);
                        }
                        Value::ByteArray(data) => {
                            let mut values = data.to_vec();
                            values.remove(idx);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(values));
                        }
                        Value::InlineByteArray(data) => {
                            let mut values = data.to_vec();
                            values.remove(idx);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(values));
                        }
                        Value::ByteVec(data) => {
                            Arc::make_mut(data).remove(idx);
                        }
                        Value::FloatVec(data) => {
                            let v = Arc::make_mut(data);
                            v.remove(idx);
                        }
                        _ => {}
                    }
                }
                Op::VecRemoveAt {
                    dst,
                    receiver,
                    index,
                } => {
                    let idx = super::index_value(&registers[index as usize])?;
                    let len = match &registers[receiver as usize] {
                        Value::Array(items) => items.len() as i64,
                        Value::IntArray(data) => data.len() as i64,
                        Value::ByteArray(data) => data.len() as i64,
                        Value::InlineByteArray(data) => data.len() as i64,
                        Value::ByteVec(data) => data.len() as i64,
                        Value::FloatVec(data) => data.len() as i64,
                        _ => 0,
                    };
                    if idx < 0 || idx >= len {
                        registers[dst as usize] = crate::builtins::slice_err(format!(
                            "remove: index {idx} out of bounds for length {len}"
                        ));
                        continue;
                    }
                    crate::stdlib_builtins::iter::note_vec_structural_mutation(
                        &registers[receiver as usize],
                    );
                    let removed = match &mut registers[receiver as usize] {
                        Value::Array(items) => {
                            let v = Arc::make_mut(items);
                            v.remove(idx as usize)
                        }
                        Value::IntArray(data) => {
                            let v = Arc::make_mut(data);
                            Value::Int(v.remove(idx as usize))
                        }
                        Value::ByteArray(data) => {
                            let mut values = data.to_vec();
                            let value = values.remove(idx as usize);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(values));
                            Value::Int(i64::from(value))
                        }
                        Value::InlineByteArray(data) => {
                            let mut values = data.to_vec();
                            let value = values.remove(idx as usize);
                            registers[receiver as usize] = Value::ByteVec(Arc::new(values));
                            Value::Int(i64::from(value))
                        }
                        Value::ByteVec(data) => {
                            let value = Arc::make_mut(data).remove(idx as usize);
                            Value::Int(i64::from(value))
                        }
                        Value::FloatVec(data) => {
                            let v = Arc::make_mut(data);
                            Value::Float(v.remove(idx as usize))
                        }
                        _ => {
                            return Err(RuntimeError::Type(
                                "remove expects a Vec receiver".to_string(),
                            ));
                        }
                    };
                    registers[dst as usize] = Value::variant("Ok", vec![removed]);
                }
                Op::TupleIndex {
                    dst,
                    receiver,
                    index,
                } => {
                    let recv = &registers[receiver as usize];
                    let idx = index as usize;
                    registers[dst as usize] = match recv {
                        Value::Tuple(items) => items.get(idx).cloned().ok_or_else(|| {
                            RuntimeError::Arithmetic("tuple index out of bounds".to_string())
                        })?,
                        Value::Array(items) => items.get(idx).cloned().ok_or_else(|| {
                            RuntimeError::Arithmetic("tuple index out of bounds".to_string())
                        })?,
                        // A tuple struct stores its positional fields as named
                        // "0".."N-1"; `.N` projects field N, matching the
                        // compiled tiers' offset load.
                        Value::Struct(inner) => inner
                            .fields
                            .get(idx)
                            .map(|(_, v)| v.clone())
                            .ok_or_else(|| {
                                RuntimeError::Arithmetic("tuple index out of bounds".to_string())
                            })?,
                        _ => {
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{recv}` has no tuple fields"
                            )));
                        }
                    };
                }
                Op::TupleSet {
                    receiver,
                    index,
                    value,
                } => {
                    let new_value = registers[value as usize].clone();
                    let idx = index as usize;
                    let oob = || RuntimeError::Arithmetic("tuple index out of bounds".to_string());
                    match &mut registers[receiver as usize] {
                        Value::Tuple(items) | Value::Array(items) => {
                            let items = Arc::make_mut(items);
                            *items.get_mut(idx).ok_or_else(oob)? = new_value;
                        }
                        // A tuple struct stores its positional fields as named
                        // "0".."N-1"; `.N` writes field N, matching the
                        // compiled tiers' offset store.
                        Value::Struct(inner) => {
                            let inner = Arc::make_mut(inner);
                            *inner.fields.get_mut(idx).ok_or_else(oob)?.1 = new_value;
                        }
                        other => {
                            let kind = other.type_name();
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{kind}` has no tuple fields"
                            )));
                        }
                    }
                }
                Op::TupleTailIndex {
                    dst,
                    receiver,
                    offset_from_end,
                } => {
                    let recv = &registers[receiver as usize];
                    registers[dst as usize] = match recv.as_value_slice() {
                        Some(items) => {
                            let len = items.len();
                            let idx = len.saturating_sub(offset_from_end as usize + 1);
                            items.get(idx).cloned().ok_or_else(|| {
                                RuntimeError::Arithmetic(
                                    "tuple tail index out of bounds".to_string(),
                                )
                            })?
                        }
                        None => {
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{recv}` has no tuple fields"
                            )));
                        }
                    };
                }
                Op::IndexedFieldSet {
                    base,
                    index,
                    name_idx,
                    value,
                } => {
                    let idx = super::sequence_index(&registers[index as usize])?;
                    let field_name_arc = match &chunk.consts[name_idx as usize] {
                        Value::String(s) => s.clone(),
                        _ => {
                            return Err(RuntimeError::Panic(
                                "IndexedFieldSet: name must be string const".to_string(),
                            ));
                        }
                    };
                    let field_name: &str = &field_name_arc;
                    let new_value = registers[value as usize].clone();
                    let b = &mut registers[base as usize];
                    let (Value::Array(items) | Value::Tuple(items)) = b else {
                        return Err(RuntimeError::Type(format!(
                            "value of kind `{b}` is not indexable"
                        )));
                    };
                    let slots = Arc::make_mut(items);
                    let slot_count = slots.len();
                    let slot = slots.get_mut(idx).ok_or_else(|| {
                        // `v[i].field = x` out of range panics on the compiled
                        // tier (the place-form bounds assert); match it here.
                        crate::vm::index_oob_panic(idx as i64, slot_count)
                    })?;
                    let Value::Struct(struct_arc) = slot else {
                        return Err(RuntimeError::Type(format!(
                            "cannot assign to field `{field_name}` on non-struct"
                        )));
                    };
                    let struct_inner = Arc::make_mut(struct_arc);
                    let field_slots = &mut struct_inner.fields;
                    let pos = field_slots
                        .iter()
                        .position(|(ident, _)| (*ident) == field_name);
                    if let Some(p) = pos {
                        field_slots[p] = new_value;
                    } else {
                        // Dynamic field add (e.g. `json::Object`): the
                        // fixed-arity slice grows by one, rebuilt once.
                        let mut grown = std::mem::take(field_slots).into_vec();
                        grown.push((crate::value::intern_type_name(field_name), new_value));
                        *field_slots = crate::value::StructFields::new(grown);
                    }
                }
                Op::MakeClosure { dst, proto } => {
                    let proto = &chunk.closure_protos[proto as usize];
                    // Snapshot the captured upvalue registers. A `Value`
                    // clone is a by-value snapshot for scalars and an
                    // `Arc` refcount bump for aggregates, so a captured
                    // aggregate mutated through the closure stays visible
                    // to the original binding.
                    let capture_values: Vec<Value> = proto
                        .capture_regs
                        .iter()
                        .map(|r| registers[*r as usize].clone())
                        .collect();
                    registers[dst as usize] = Value::Closure(Arc::new(crate::value::Closure {
                        chunk: Arc::clone(&proto.chunk),
                        capture_values,
                    }));
                }
                Op::Select { first, count } => {
                    let start = first as usize;
                    let arms = &chunk.select_arms[start..start + count as usize];
                    pc = select_dispatch(arms, &mut registers[..])?;
                }
                Op::CovHit { slot } => {
                    gossamer_runtime::coverage::bump(slot as usize);
                }

                // ----- Phase 1 typed ops -----
                //
                // All float/int register accesses use
                // `get_unchecked` - the register slot index is
                // always less than `chunk.float_count` /
                // `chunk.int_count` by construction of the
                // bytecode (the compiler emits a fresh index for
                // every destination and carries it through
                // compile_expr_ex).
                Op::LoadConstF64 { dst_f, idx } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) =
                        *chunk.f64_consts.get_unchecked(idx as usize);
                },
                Op::AddF64 {
                    dst_f,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) = *floats
                        .get_unchecked(lhs_f as usize)
                        + *floats.get_unchecked(rhs_f as usize);
                },
                Op::SubF64 {
                    dst_f,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) = *floats
                        .get_unchecked(lhs_f as usize)
                        - *floats.get_unchecked(rhs_f as usize);
                },
                Op::MulF64 {
                    dst_f,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) = *floats
                        .get_unchecked(lhs_f as usize)
                        * *floats.get_unchecked(rhs_f as usize);
                },
                Op::DivF64 {
                    dst_f,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) = *floats
                        .get_unchecked(lhs_f as usize)
                        / *floats.get_unchecked(rhs_f as usize);
                },
                Op::DivF64ByI64 {
                    dst_f,
                    lhs_f,
                    rhs_i,
                } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) = *floats
                        .get_unchecked(lhs_f as usize)
                        / (*ints.get_unchecked(rhs_i as usize) as f64);
                },
                Op::NegF64 { dst_f, src_f } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) =
                        -*floats.get_unchecked(src_f as usize);
                },
                Op::LtF64 {
                    dst_v,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *floats.get_unchecked(lhs_f as usize)
                            < *floats.get_unchecked(rhs_f as usize),
                    );
                },
                Op::LeF64 {
                    dst_v,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *floats.get_unchecked(lhs_f as usize)
                            <= *floats.get_unchecked(rhs_f as usize),
                    );
                },
                Op::GtF64 {
                    dst_v,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *floats.get_unchecked(lhs_f as usize)
                            > *floats.get_unchecked(rhs_f as usize),
                    );
                },
                Op::GeF64 {
                    dst_v,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *floats.get_unchecked(lhs_f as usize)
                            >= *floats.get_unchecked(rhs_f as usize),
                    );
                },
                Op::EqF64 {
                    dst_v,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *floats.get_unchecked(lhs_f as usize)
                            == *floats.get_unchecked(rhs_f as usize),
                    );
                },
                Op::NeF64 {
                    dst_v,
                    lhs_f,
                    rhs_f,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *floats.get_unchecked(lhs_f as usize)
                            != *floats.get_unchecked(rhs_f as usize),
                    );
                },
                Op::UnboxF64 {
                    dst_f,
                    src_v,
                    peer_v,
                } => {
                    let v = &registers[src_v as usize];
                    let peer = peer_v.map(|peer_v| &registers[peer_v as usize]);
                    let f = match v {
                        Value::Float(f) => *f,
                        Value::Int(n) => *n as f64,
                        // 0.7.0 flag::Cell auto-deref for `Cell<f64>`
                        // / `Cell<i64>` operand mixing.
                        Value::Struct(inner) if inner.name == "__Cell" => {
                            match auto_deref_cell(v) {
                                Some(Value::Float(f)) => f,
                                Some(Value::Int(n)) => n as f64,
                                _ => {
                                    return Err(incompatible_type_error(v, peer, "f64"));
                                }
                            }
                        }
                        _ => {
                            return Err(incompatible_type_error(v, peer, "f64"));
                        }
                    };
                    floats[dst_f as usize] = f;
                }
                Op::BoxF64 { dst_v, src_f } => {
                    registers[dst_v as usize] = Value::Float(floats[src_f as usize]);
                }
                Op::SqrtF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].sqrt();
                }
                Op::SinF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].sin();
                }
                Op::CosF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].cos();
                }
                Op::AbsF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].abs();
                }
                Op::FloorF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].floor();
                }
                Op::CeilF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].ceil();
                }
                Op::ExpF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].exp();
                }
                Op::LnF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize].ln();
                }
                Op::MulAddF64 {
                    dst_f,
                    a_f,
                    b_f,
                    c_f,
                } => unsafe {
                    // Two roundings, as `a * b + c` is written. The fusion is
                    // over dispatch, not over arithmetic: a single-rounded
                    // `mul_add` would give this expression a different value
                    // here than the compiled tiers' separate multiply and add.
                    let product =
                        *floats.get_unchecked(a_f as usize) * *floats.get_unchecked(b_f as usize);
                    *floats.get_unchecked_mut(dst_f as usize) =
                        product + *floats.get_unchecked(c_f as usize);
                },
                Op::MulSubF64 {
                    dst_f,
                    a_f,
                    b_f,
                    c_f,
                } => unsafe {
                    let product =
                        *floats.get_unchecked(a_f as usize) * *floats.get_unchecked(b_f as usize);
                    *floats.get_unchecked_mut(dst_f as usize) =
                        *floats.get_unchecked(c_f as usize) - product;
                },

                Op::LoadConstI64 { dst_i, idx } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) =
                        *chunk.i64_consts.get_unchecked(idx as usize);
                },
                Op::AddI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) = ints
                        .get_unchecked(lhs_i as usize)
                        .wrapping_add(*ints.get_unchecked(rhs_i as usize));
                },
                Op::CheckedAddI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                    overflow_ty,
                } => unsafe {
                    let lhs = *ints.get_unchecked(lhs_i as usize);
                    let rhs = *ints.get_unchecked(rhs_i as usize);
                    *ints.get_unchecked_mut(dst_i as usize) = if matches!(
                        overflow_ty,
                        gossamer_types::IntTy::I64 | gossamer_types::IntTy::Isize
                    ) {
                        match lhs.checked_add(rhs) {
                            Some(value) => value,
                            None => {
                                return Err(RuntimeError::Panic(
                                    "attempt to add with overflow".to_string(),
                                ));
                            }
                        }
                    } else {
                        checked_integer_arithmetic(lhs, rhs, overflow_ty, ImmArithKind::Add)?
                    };
                },
                Op::SubI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) = ints
                        .get_unchecked(lhs_i as usize)
                        .wrapping_sub(*ints.get_unchecked(rhs_i as usize));
                },
                Op::CheckedSubI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                    overflow_ty,
                } => unsafe {
                    let lhs = *ints.get_unchecked(lhs_i as usize);
                    let rhs = *ints.get_unchecked(rhs_i as usize);
                    *ints.get_unchecked_mut(dst_i as usize) = if matches!(
                        overflow_ty,
                        gossamer_types::IntTy::I64 | gossamer_types::IntTy::Isize
                    ) {
                        match lhs.checked_sub(rhs) {
                            Some(value) => value,
                            None => {
                                return Err(RuntimeError::Panic(
                                    "attempt to subtract with overflow".to_string(),
                                ));
                            }
                        }
                    } else {
                        checked_integer_arithmetic(lhs, rhs, overflow_ty, ImmArithKind::Sub)?
                    };
                },
                Op::MulI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) = ints
                        .get_unchecked(lhs_i as usize)
                        .wrapping_mul(*ints.get_unchecked(rhs_i as usize));
                },
                Op::CheckedMulI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                    overflow_ty,
                } => unsafe {
                    let lhs = *ints.get_unchecked(lhs_i as usize);
                    let rhs = *ints.get_unchecked(rhs_i as usize);
                    *ints.get_unchecked_mut(dst_i as usize) = if matches!(
                        overflow_ty,
                        gossamer_types::IntTy::I64 | gossamer_types::IntTy::Isize
                    ) {
                        match lhs.checked_mul(rhs) {
                            Some(value) => value,
                            None => {
                                return Err(RuntimeError::Panic(
                                    "attempt to multiply with overflow".to_string(),
                                ));
                            }
                        }
                    } else {
                        checked_integer_arithmetic(lhs, rhs, overflow_ty, ImmArithKind::Mul)?
                    };
                },
                Op::DivI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => {
                    let r = ints[rhs_i as usize];
                    if r == 0 {
                        return Err(RuntimeError::Panic("divide by zero".to_string()));
                    }
                    ints[dst_i as usize] = ints[lhs_i as usize].wrapping_div(r);
                }
                Op::RemI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => {
                    let r = ints[rhs_i as usize];
                    if r == 0 {
                        return Err(RuntimeError::Panic("divide by zero".to_string()));
                    }
                    ints[dst_i as usize] = ints[lhs_i as usize].wrapping_rem(r);
                }
                Op::DivU64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => {
                    let r = ints[rhs_i as usize] as u64;
                    if r == 0 {
                        return Err(RuntimeError::Panic("divide by zero".to_string()));
                    }
                    ints[dst_i as usize] = ((ints[lhs_i as usize] as u64) / r) as i64;
                }
                Op::RemU64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => {
                    let r = ints[rhs_i as usize] as u64;
                    if r == 0 {
                        return Err(RuntimeError::Panic("divide by zero".to_string()));
                    }
                    ints[dst_i as usize] = ((ints[lhs_i as usize] as u64) % r) as i64;
                }
                Op::ArithImmI64 {
                    kind,
                    dst_i,
                    lhs_i,
                    imm,
                } => unsafe {
                    let lhs = *ints.get_unchecked(lhs_i as usize);
                    let imm = imm as i64;
                    // Div/Rem immediates are non-zero by the emitter's
                    // contract, so no zero check is paid here.
                    *ints.get_unchecked_mut(dst_i as usize) = match kind {
                        ImmArithKind::Add => lhs.wrapping_add(imm),
                        ImmArithKind::Sub => lhs.wrapping_sub(imm),
                        ImmArithKind::Mul => lhs.wrapping_mul(imm),
                        ImmArithKind::Div => lhs.wrapping_div(imm),
                        ImmArithKind::Rem => lhs.wrapping_rem(imm),
                    };
                },
                Op::NegI64 { dst_i, src_i } => {
                    ints[dst_i as usize] = ints[src_i as usize].wrapping_neg();
                }
                Op::BitAndI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) =
                        ints.get_unchecked(lhs_i as usize) & ints.get_unchecked(rhs_i as usize);
                },
                Op::BitOrI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) =
                        ints.get_unchecked(lhs_i as usize) | ints.get_unchecked(rhs_i as usize);
                },
                Op::BitXorI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) =
                        ints.get_unchecked(lhs_i as usize) ^ ints.get_unchecked(rhs_i as usize);
                },
                Op::ShlI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    let shift = (*ints.get_unchecked(rhs_i as usize) & 63) as u32;
                    *ints.get_unchecked_mut(dst_i as usize) =
                        ints.get_unchecked(lhs_i as usize).wrapping_shl(shift);
                },
                Op::ShrI64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    let shift = (*ints.get_unchecked(rhs_i as usize) & 63) as u32;
                    *ints.get_unchecked_mut(dst_i as usize) =
                        ints.get_unchecked(lhs_i as usize).wrapping_shr(shift);
                },
                Op::ShrU64 {
                    dst_i,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    let shift = (*ints.get_unchecked(rhs_i as usize) & 63) as u32;
                    *ints.get_unchecked_mut(dst_i as usize) =
                        (*ints.get_unchecked(lhs_i as usize) as u64).wrapping_shr(shift) as i64;
                },
                Op::LtI64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *ints.get_unchecked(lhs_i as usize) < *ints.get_unchecked(rhs_i as usize),
                    );
                },
                Op::LeI64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *ints.get_unchecked(lhs_i as usize) <= *ints.get_unchecked(rhs_i as usize),
                    );
                },
                Op::GtI64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *ints.get_unchecked(lhs_i as usize) > *ints.get_unchecked(rhs_i as usize),
                    );
                },
                Op::GeI64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *ints.get_unchecked(lhs_i as usize) >= *ints.get_unchecked(rhs_i as usize),
                    );
                },
                Op::EqI64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *ints.get_unchecked(lhs_i as usize) == *ints.get_unchecked(rhs_i as usize),
                    );
                },
                Op::NeI64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        *ints.get_unchecked(lhs_i as usize) != *ints.get_unchecked(rhs_i as usize),
                    );
                },
                Op::LtU64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        (*ints.get_unchecked(lhs_i as usize) as u64)
                            < (*ints.get_unchecked(rhs_i as usize) as u64),
                    );
                },
                Op::LeU64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        (*ints.get_unchecked(lhs_i as usize) as u64)
                            <= (*ints.get_unchecked(rhs_i as usize) as u64),
                    );
                },
                Op::GtU64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        (*ints.get_unchecked(lhs_i as usize) as u64)
                            > (*ints.get_unchecked(rhs_i as usize) as u64),
                    );
                },
                Op::GeU64 {
                    dst_v,
                    lhs_i,
                    rhs_i,
                } => unsafe {
                    *registers.get_unchecked_mut(dst_v as usize) = Value::Bool(
                        (*ints.get_unchecked(lhs_i as usize) as u64)
                            >= (*ints.get_unchecked(rhs_i as usize) as u64),
                    );
                },
                Op::UnboxI64 {
                    dst_i,
                    src_v,
                    peer_v,
                } => {
                    let v = &registers[src_v as usize];
                    let peer = peer_v.map(|peer_v| &registers[peer_v as usize]);
                    // 0.7.0 flag::Cell auto-deref. The typechecker
                    // pins `flags.number` as `Cell<i64>` and emits
                    // an UnboxI64 hoping to find a `Value::Int`;
                    // without this branch the cell handle hits the
                    // type-error arm and aborts.
                    let n = match v {
                        Value::Int(n) => *n,
                        // `as usize` / `as u64` produce a `Uint` (for unsigned
                        // display); every ≤64-bit integer shares i64 arithmetic,
                        // so unboxing it into the typed i64 register is correct
                        // (and lets `(x as usize) < (y as usize)` take the typed
                        // comparison path instead of type-erroring).
                        Value::Uint(n) => *n as i64,
                        Value::Struct(inner) if inner.name == "__Cell" => {
                            match auto_deref_cell(v) {
                                Some(Value::Int(n)) => n,
                                Some(Value::Uint(n)) => n as i64,
                                _ => {
                                    return Err(incompatible_type_error(v, peer, "i64"));
                                }
                            }
                        }
                        _ => {
                            return Err(incompatible_type_error(v, peer, "i64"));
                        }
                    };
                    ints[dst_i as usize] = n;
                }
                Op::BoxI64 { dst_v, src_i } => {
                    registers[dst_v as usize] = Value::Int(ints[src_i as usize]);
                }
                Op::MoveF64 { dst_f, src_f } => {
                    floats[dst_f as usize] = floats[src_f as usize];
                }
                Op::MoveI64 { dst_i, src_i } => {
                    ints[dst_i as usize] = ints[src_i as usize];
                }
                Op::Struct2I64 {
                    dst,
                    type_name,
                    field0,
                    field1,
                    first_i,
                    second_i,
                } => {
                    registers[dst as usize] = Value::struct_2_i64(
                        chunk.shape_names[type_name as usize],
                        chunk.shape_names[field0 as usize],
                        ints[first_i as usize],
                        chunk.shape_names[field1 as usize],
                        ints[second_i as usize],
                    );
                }

                // ----- Phase 2 fused / typed field access -----
                Op::FieldGetF64 {
                    dst_f,
                    receiver,
                    name_idx,
                } => {
                    let Value::String(field_name) = &chunk.consts[name_idx as usize] else {
                        return Err(RuntimeError::Panic(
                            "FieldGetF64: name must be string const".to_string(),
                        ));
                    };
                    let recv = &registers[receiver as usize];
                    let Value::Struct(struct_inner) = recv else {
                        return Err(RuntimeError::Type(format!(
                            "field access on non-struct `{recv}`"
                        )));
                    };
                    let mut val = 0.0f64;
                    for (ident, v) in &struct_inner.fields {
                        if (*ident) == field_name.as_str() {
                            val = match v {
                                Value::Float(f) => *f,
                                Value::Int(n) => *n as f64,
                                _ => 0.0,
                            };
                            break;
                        }
                    }
                    floats[dst_f as usize] = val;
                }
                Op::FieldGetI64 {
                    dst_i,
                    receiver,
                    name_idx,
                } => {
                    let Value::String(field_name) = &chunk.consts[name_idx as usize] else {
                        return Err(RuntimeError::Panic(
                            "FieldGetI64: name must be string const".to_string(),
                        ));
                    };
                    let recv = &registers[receiver as usize];
                    let Value::Struct(struct_inner) = recv else {
                        return Err(RuntimeError::Type(format!(
                            "field access on non-struct `{recv}`"
                        )));
                    };
                    let mut val = 0i64;
                    for (ident, v) in &struct_inner.fields {
                        if (*ident) == field_name.as_str() {
                            val = match v {
                                Value::Int(n) => *n,
                                Value::Uint(n) => *n as i64,
                                _ => 0,
                            };
                            break;
                        }
                    }
                    ints[dst_i as usize] = val;
                }
                Op::IndexedFieldGet {
                    dst,
                    base,
                    index,
                    name_idx,
                } => {
                    let idx = super::sequence_index(&registers[index as usize])?;
                    let Value::String(field_name) = &chunk.consts[name_idx as usize] else {
                        return Err(RuntimeError::Panic(
                            "IndexedFieldGet: name must be string const".to_string(),
                        ));
                    };
                    let b = &registers[base as usize];
                    let Some(items) = b.as_value_slice() else {
                        return Err(RuntimeError::Type(format!(
                            "value of kind `{b}` is not indexable"
                        )));
                    };
                    let slot = items
                        .get(idx)
                        .ok_or_else(|| crate::vm::index_oob_panic(idx as i64, items.len()))?;
                    let Value::Struct(struct_inner) = slot else {
                        return Err(RuntimeError::Type(
                            "value at index is not a struct".to_string(),
                        ));
                    };
                    let mut found = None;
                    for (ident, v) in &struct_inner.fields {
                        if (*ident) == field_name.as_str() {
                            found = Some(v);
                            break;
                        }
                    }
                    registers[dst as usize] = found.cloned().unwrap_or(Value::Unit);
                }
                Op::IndexedFieldGetF64 {
                    dst_f,
                    base,
                    index,
                    name_idx,
                } => {
                    let idx = super::sequence_index(&registers[index as usize])?;
                    let Value::String(field_name) = &chunk.consts[name_idx as usize] else {
                        return Err(RuntimeError::Panic(
                            "IndexedFieldGetF64: name must be string const".to_string(),
                        ));
                    };
                    let b = &registers[base as usize];
                    // FloatArray fast path - resolve the field
                    // name against the stored declaration order
                    // and pull the f64 directly out of flat data.
                    if let Value::FloatArray(fa_inner) = b {
                        let off = fa_inner
                            .field_names
                            .iter()
                            .position(|n| n.as_str() == field_name.as_str())
                            .unwrap_or(0);
                        let stride = fa_inner.stride as usize;
                        let pos = idx * stride + off;
                        floats[dst_f as usize] = *fa_inner.data.get(pos).unwrap_or(&0.0);
                        continue;
                    }
                    let Some(items) = b.as_value_slice() else {
                        return Err(RuntimeError::Type(format!(
                            "value of kind `{b}` is not indexable"
                        )));
                    };
                    let slot = items
                        .get(idx)
                        .ok_or_else(|| crate::vm::index_oob_panic(idx as i64, items.len()))?;
                    let Value::Struct(struct_inner) = slot else {
                        return Err(RuntimeError::Type(
                            "value at index is not a struct".to_string(),
                        ));
                    };
                    let mut val = 0.0f64;
                    for (ident, v) in &struct_inner.fields {
                        if (*ident) == field_name.as_str() {
                            val = match v {
                                Value::Float(f) => *f,
                                Value::Int(n) => *n as f64,
                                _ => 0.0,
                            };
                            break;
                        }
                    }
                    floats[dst_f as usize] = val;
                }
                Op::IndexedFieldSetF64 {
                    base,
                    index,
                    name_idx,
                    value_f,
                } => {
                    let idx = super::sequence_index(&registers[index as usize])?;
                    let field_name_arc = match &chunk.consts[name_idx as usize] {
                        Value::String(s) => s.clone(),
                        _ => {
                            return Err(RuntimeError::Panic(
                                "IndexedFieldSetF64: name must be string const".to_string(),
                            ));
                        }
                    };
                    let field_name = field_name_arc.as_str();
                    let new_value = Value::Float(floats[value_f as usize]);
                    let b = &mut registers[base as usize];
                    let (Value::Array(items) | Value::Tuple(items)) = b else {
                        return Err(RuntimeError::Type(format!(
                            "value of kind `{b}` is not indexable"
                        )));
                    };
                    let slots = Arc::make_mut(items);
                    let slot_count = slots.len();
                    let slot = slots.get_mut(idx).ok_or_else(|| {
                        // `v[i].field = x` out of range panics on the compiled
                        // tier (the place-form bounds assert); match it here.
                        crate::vm::index_oob_panic(idx as i64, slot_count)
                    })?;
                    let Value::Struct(struct_arc) = slot else {
                        return Err(RuntimeError::Type(format!(
                            "cannot assign to field `{field_name}` on non-struct"
                        )));
                    };
                    let struct_inner = Arc::make_mut(struct_arc);
                    let field_slots = &mut struct_inner.fields;
                    let pos = field_slots
                        .iter()
                        .position(|(ident, _)| (*ident) == field_name);
                    if let Some(p) = pos {
                        field_slots[p] = new_value;
                    } else {
                        // Dynamic field add (e.g. `json::Object`): the
                        // fixed-arity slice grows by one, rebuilt once.
                        let mut grown = std::mem::take(field_slots).into_vec();
                        grown.push((crate::value::intern_type_name(field_name), new_value));
                        *field_slots = crate::value::StructFields::new(grown);
                    }
                }

                // ----- Phase 2 offset-resolved ops -----
                Op::IndexedFieldGetF64ByOffset {
                    dst_f,
                    base,
                    index,
                    offset,
                } => {
                    // SAFETY: `index`, `base`, `dst_f` are
                    // compile-time allocated register slots,
                    // so the indexed accesses into `registers`
                    // and `floats` are always in bounds.
                    let idx =
                        super::sequence_index(unsafe { registers.get_unchecked(index as usize) })?;
                    let b = unsafe { registers.get_unchecked(base as usize) };
                    // Flat-f64 fast path: direct f64 load out
                    // of the flat data buffer, no `Value`
                    // discriminant, no `Arc::clone`.
                    if let Value::FloatArray(fa_inner) = b {
                        let pos = idx * (fa_inner.stride as usize) + offset as usize;
                        // SAFETY: the FloatArray was built with
                        // `data.len() == stride * elem_count`, and
                        // `offset < stride` by construction
                        // (compile-time checked). `idx` is the
                        // caller's responsibility; we bounds-check
                        // it once here.
                        if pos >= fa_inner.data.len() {
                            return Err(crate::vm::index_oob_panic(
                                pos as i64,
                                fa_inner.data.len(),
                            ));
                        }
                        let f = unsafe { *fa_inner.data.get_unchecked(pos) };
                        unsafe {
                            *floats.get_unchecked_mut(dst_f as usize) = f;
                        }
                    } else {
                        let Some(items) = b.as_value_slice() else {
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{b}` is not indexable"
                            )));
                        };
                        let slot = items
                            .get(idx)
                            .ok_or_else(|| crate::vm::index_oob_panic(idx as i64, items.len()))?;
                        let Value::Struct(struct_inner) = slot else {
                            return Err(RuntimeError::Type(
                                "value at index is not a struct".to_string(),
                            ));
                        };
                        let f = match struct_inner.fields.get(offset as usize).map(|(_, v)| v) {
                            Some(Value::Float(f)) => *f,
                            Some(Value::Int(n)) => *n as f64,
                            _ => 0.0,
                        };
                        floats[dst_f as usize] = f;
                    }
                }
                Op::IndexedFieldSetF64ByOffset {
                    base,
                    index,
                    offset,
                    value_f,
                } => {
                    let idx =
                        super::sequence_index(unsafe { registers.get_unchecked(index as usize) })?;
                    // SAFETY: `value_f` and `base` are
                    // compile-allocated register slots.
                    let new_f = unsafe { *floats.get_unchecked(value_f as usize) };
                    let b = unsafe { registers.get_unchecked_mut(base as usize) };
                    // Flat-f64 fast path: one `Arc::make_mut`
                    // plus a direct memory store. The common
                    // case is a refcount-1 Arc, so `make_mut`
                    // returns the inner mut ref without cloning
                    // - still one acquire-load per write, but no
                    // struct clone and no field scan.
                    if let Value::FloatArray(fa_arc) = b {
                        let fa_inner = Arc::make_mut(fa_arc);
                        let stride = fa_inner.stride as usize;
                        let pos = idx * stride + offset as usize;
                        let buf = Arc::make_mut(&mut fa_inner.data);
                        // SAFETY: `pos < stride * elem_count == buf.len()`
                        // when `idx < elem_count`; we verify that.
                        if pos < buf.len() {
                            unsafe {
                                *buf.get_unchecked_mut(pos) = new_f;
                            }
                        } else {
                            // `v[i].field = x` out of range panics on the
                            // compiled tier; match it here.
                            return Err(crate::vm::index_oob_panic(pos as i64, buf.len()));
                        }
                    } else {
                        let new_value = Value::Float(new_f);
                        let (Value::Array(items) | Value::Tuple(items)) = b else {
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{b}` is not indexable"
                            )));
                        };
                        let slots = Arc::make_mut(items);
                        let slot_count = slots.len();
                        let slot = slots
                            .get_mut(idx)
                            .ok_or_else(|| crate::vm::index_oob_panic(idx as i64, slot_count))?;
                        let Value::Struct(struct_arc) = slot else {
                            return Err(RuntimeError::Type(
                                "cannot assign to field on non-struct".to_string(),
                            ));
                        };
                        let struct_inner = Arc::make_mut(struct_arc);
                        let field_slots = &mut struct_inner.fields;
                        if let Some(entry) = field_slots.get_mut(offset as usize) {
                            *entry.1 = new_value;
                        }
                    }
                }
                Op::BranchIfLtI64 {
                    lhs_i,
                    rhs_i,
                    target,
                } => unsafe {
                    if *ints.get_unchecked(lhs_i as usize) < *ints.get_unchecked(rhs_i as usize) {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                },
                Op::BranchIfGeI64 {
                    lhs_i,
                    rhs_i,
                    target,
                } => unsafe {
                    if *ints.get_unchecked(lhs_i as usize) >= *ints.get_unchecked(rhs_i as usize) {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                },
                Op::BranchIfGtI64 {
                    lhs_i,
                    rhs_i,
                    target,
                } => unsafe {
                    if *ints.get_unchecked(lhs_i as usize) > *ints.get_unchecked(rhs_i as usize) {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                },
                Op::BranchIfLtF64 {
                    lhs_f,
                    rhs_f,
                    target,
                } => unsafe {
                    if *floats.get_unchecked(lhs_f as usize) < *floats.get_unchecked(rhs_f as usize)
                    {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                },
                Op::BranchIfGeF64 {
                    lhs_f,
                    rhs_f,
                    target,
                } => unsafe {
                    if *floats.get_unchecked(lhs_f as usize)
                        >= *floats.get_unchecked(rhs_f as usize)
                    {
                        if target < pc {
                            poll_vm_backedge(&mut preempt_countdown);
                        }
                        pc = target;
                    }
                },
                Op::IncJumpIfLtI64 {
                    counter_i,
                    end_i,
                    target,
                } => unsafe {
                    // Bottom-of-loop fused tick for `for i in a..b`.
                    // SAFETY: counter_i and end_i are typed-i64 regs
                    // allocated by `try_compile_for_loop_range`; the
                    // counter_i slot is the same one tracked by the
                    // pre-loop bounds check, and the int register
                    // file size is sized to hold both.
                    let next = (*ints.get_unchecked(counter_i as usize)).wrapping_add(1);
                    *ints.get_unchecked_mut(counter_i as usize) = next;
                    if next < *ints.get_unchecked(end_i as usize) {
                        poll_vm_backedge(&mut preempt_countdown);
                        pc = target;
                    }
                },
                Op::IncJumpIfLeI64 {
                    counter_i,
                    end_i,
                    target,
                } => unsafe {
                    let next = (*ints.get_unchecked(counter_i as usize)).wrapping_add(1);
                    *ints.get_unchecked_mut(counter_i as usize) = next;
                    if next <= *ints.get_unchecked(end_i as usize) {
                        poll_vm_backedge(&mut preempt_countdown);
                        pc = target;
                    }
                },

                Op::FieldGetF64ByOffset {
                    dst_f,
                    receiver,
                    offset,
                } => {
                    let recv = &registers[receiver as usize];
                    let Value::Struct(struct_inner) = recv else {
                        return Err(RuntimeError::Type(format!(
                            "field access on non-struct `{recv}`"
                        )));
                    };
                    let f = match struct_inner.fields.get(offset as usize).map(|(_, v)| v) {
                        Some(Value::Float(f)) => *f,
                        Some(Value::Int(n)) => *n as f64,
                        _ => 0.0,
                    };
                    floats[dst_f as usize] = f;
                }
                Op::FieldGetI64ByOffset {
                    dst_i,
                    receiver,
                    offset,
                } => {
                    let recv = &registers[receiver as usize];
                    let Value::Struct(struct_inner) = recv else {
                        return Err(RuntimeError::Type(format!(
                            "field access on non-struct `{recv}`"
                        )));
                    };
                    let n = match struct_inner.fields.get(offset as usize).map(|(_, v)| v) {
                        Some(Value::Int(n)) => *n,
                        Some(Value::Uint(n)) => *n as i64,
                        _ => 0,
                    };
                    ints[dst_i as usize] = n;
                }
                Op::FlatGetF64 {
                    dst_f,
                    base,
                    index,
                    stride,
                    offset,
                } => unsafe {
                    let idx = super::sequence_index(registers.get_unchecked(index as usize))?;
                    let b = registers.get_unchecked(base as usize);
                    // Compiler-proven FloatArray: skip discriminant match.
                    let Value::FloatArray(fa_inner) = b else {
                        return Err(RuntimeError::Type(
                            "FlatGetF64: receiver lost flat invariant".to_string(),
                        ));
                    };
                    let pos = idx * stride as usize + offset as usize;
                    if pos >= fa_inner.data.len() {
                        return Err(crate::vm::index_oob_panic(pos as i64, fa_inner.data.len()));
                    }
                    *floats.get_unchecked_mut(dst_f as usize) = *fa_inner.data.get_unchecked(pos);
                },
                Op::FlatSetF64 {
                    base,
                    index,
                    stride,
                    offset,
                    value_f,
                } => unsafe {
                    let idx = super::sequence_index(registers.get_unchecked(index as usize))?;
                    let new_f = *floats.get_unchecked(value_f as usize);
                    let b = registers.get_unchecked_mut(base as usize);
                    let Value::FloatArray(fa_arc) = b else {
                        return Err(RuntimeError::Type(
                            "FlatSetF64: receiver lost flat invariant".to_string(),
                        ));
                    };
                    let fa_inner = Arc::make_mut(fa_arc);
                    let pos = idx * stride as usize + offset as usize;
                    let buf = Arc::make_mut(&mut fa_inner.data);
                    if pos < buf.len() {
                        *buf.get_unchecked_mut(pos) = new_f;
                    } else {
                        // `v[i].field = x` out of range panics on the compiled
                        // tier (place-form bounds assert); match it here.
                        return Err(crate::vm::index_oob_panic(pos as i64, buf.len()));
                    }
                },
                Op::FlatGetF64I {
                    dst_f,
                    base,
                    index_i,
                    stride,
                    offset,
                } => unsafe {
                    let raw = *ints.get_unchecked(index_i as usize);
                    if raw < 0 {
                        return Err(RuntimeError::Arithmetic(
                            "negative index into sequence".to_string(),
                        ));
                    }
                    let idx = raw as usize;
                    let b = registers.get_unchecked(base as usize);
                    let Value::FloatArray(fa_inner) = b else {
                        return Err(RuntimeError::Type(
                            "FlatGetF64I: receiver lost flat invariant".to_string(),
                        ));
                    };
                    let pos = idx * stride as usize + offset as usize;
                    if pos >= fa_inner.data.len() {
                        return Err(crate::vm::index_oob_panic(pos as i64, fa_inner.data.len()));
                    }
                    *floats.get_unchecked_mut(dst_f as usize) = *fa_inner.data.get_unchecked(pos);
                },
                Op::FlatSetF64I {
                    base,
                    index_i,
                    stride,
                    offset,
                    value_f,
                } => unsafe {
                    let raw = *ints.get_unchecked(index_i as usize);
                    if raw < 0 {
                        return Err(RuntimeError::Arithmetic(
                            "negative index into sequence".to_string(),
                        ));
                    }
                    let idx = raw as usize;
                    let new_f = *floats.get_unchecked(value_f as usize);
                    let b = registers.get_unchecked_mut(base as usize);
                    let Value::FloatArray(fa_arc) = b else {
                        return Err(RuntimeError::Type(
                            "FlatSetF64I: receiver lost flat invariant".to_string(),
                        ));
                    };
                    let fa_inner = Arc::make_mut(fa_arc);
                    let pos = idx * stride as usize + offset as usize;
                    let buf = Arc::make_mut(&mut fa_inner.data);
                    if pos < buf.len() {
                        *buf.get_unchecked_mut(pos) = new_f;
                    } else {
                        // `v[i].field = x` out of range panics on the compiled
                        // tier (place-form bounds assert); match it here.
                        return Err(crate::vm::index_oob_panic(pos as i64, buf.len()));
                    }
                },

                Op::BuildIntArray {
                    dst_v,
                    first_i,
                    count,
                } => {
                    let start = first_i as usize;
                    let end = start + count as usize;
                    let data: Vec<i64> = ints[start..end].to_vec();
                    registers[dst_v as usize] = Value::IntArray(Arc::new(data));
                }
                Op::BuildByteArray {
                    dst_v,
                    first_i,
                    count,
                } => {
                    let start = first_i as usize;
                    let end = start + count as usize;
                    let data: Vec<u8> = ints[start..end].iter().map(|value| *value as u8).collect();
                    registers[dst_v as usize] =
                        Value::ByteArray(Arc::new(crate::value::PackedBytes::from(data)));
                }
                Op::BuildByteArrayRepeat {
                    dst_v,
                    value_i,
                    count_v,
                } => {
                    let count = match &registers[count_v as usize] {
                        Value::Int(count) if *count >= 0 => *count as usize,
                        Value::Int(_) => {
                            return Err(RuntimeError::Type("negative repeat count".to_string()));
                        }
                        _ => {
                            return Err(RuntimeError::Type("repeat count must be int".to_string()));
                        }
                    };
                    let value = ints[value_i as usize] as u8;
                    registers[dst_v as usize] =
                        Value::ByteArray(Arc::new(crate::value::PackedBytes::from(vec![
                            value;
                            count
                        ])));
                }
                Op::CheckNonNegativeCapacity { capacity_i } => unsafe {
                    let capacity = *ints.get_unchecked(capacity_i as usize);
                    if capacity < 0 {
                        return Err(RuntimeError::Type(
                            "Vec::with_capacity: capacity must be non-negative".to_string(),
                        ));
                    }
                    if capacity as u64 > (isize::MAX as u64) / (std::mem::size_of::<i64>() as u64) {
                        return Err(RuntimeError::Panic("capacity overflow".to_string()));
                    }
                },
                Op::IntToFloatF64 { dst_f, src_i } => unsafe {
                    *floats.get_unchecked_mut(dst_f as usize) =
                        *ints.get_unchecked(src_i as usize) as f64;
                },
                Op::FloatToIntI64 { dst_i, src_f } => unsafe {
                    *ints.get_unchecked_mut(dst_i as usize) =
                        *floats.get_unchecked(src_f as usize) as i64;
                },
                Op::TruncCastI64 {
                    dst_i,
                    src_i,
                    shift,
                    signed,
                } => unsafe {
                    let v = *ints.get_unchecked(src_i as usize);
                    let result = if signed {
                        // Arithmetic: shift left to fill MSB with sign bit,
                        // then arithmetic right shift back.
                        v.wrapping_shl(u32::from(shift))
                            .wrapping_shr(u32::from(shift))
                    } else {
                        // Logical: zero-extend by masking upper bits.
                        ((v as u64)
                            .wrapping_shl(u32::from(shift))
                            .wrapping_shr(u32::from(shift))) as i64
                    };
                    *ints.get_unchecked_mut(dst_i as usize) = result;
                },
                Op::UintLeaves { dst, src, desc_idx } => {
                    let desc = match &chunk.consts[desc_idx as usize] {
                        Value::String(text) => text.as_str().as_bytes().to_vec(),
                        _ => Vec::new(),
                    };
                    registers[dst as usize] =
                        crate::value::uint_leaves(&registers[src as usize], &desc);
                }
                Op::I64ToUint { dst_v, src_i } => unsafe {
                    registers[dst_v as usize] =
                        Value::Uint(*ints.get_unchecked(src_i as usize) as u64);
                },
                Op::CastScalar { dst, src, target } => {
                    let v = &registers[src as usize];
                    let Some(result) = crate::cast::cast_scalar(v, target) else {
                        return Err(RuntimeError::Type(format!(
                            "cannot cast value of kind `{v}` to {target:?}"
                        )));
                    };
                    registers[dst as usize] = result;
                }
                Op::CellNew { dst, src } => {
                    let inner = registers[src as usize].clone();
                    registers[dst as usize] =
                        Value::MutCell(Arc::new(ThreadConfinedCell::new(inner)));
                }
                Op::CellNewMove { dst, src } => {
                    let inner = std::mem::replace(&mut registers[src as usize], Value::Unit);
                    registers[dst as usize] =
                        Value::MutCell(Arc::new(ThreadConfinedCell::new(inner)));
                }
                Op::CaptureCellNew { dst, src } => {
                    let inner = std::mem::replace(&mut registers[src as usize], Value::Unit);
                    registers[dst as usize] =
                        Value::CaptureCell(Arc::new(parking_lot::Mutex::new(inner)));
                }
                Op::CaptureCellGet { dst, cell } => {
                    let Value::CaptureCell(c) = &registers[cell as usize] else {
                        return Err(capture_cell_expected("CaptureCellGet"));
                    };
                    let loaded = c.lock().clone();
                    registers[dst as usize] = loaded;
                }
                Op::CaptureCellTake { dst, cell } => {
                    // Exclusive borrow for one instruction: the value moves
                    // out so an in-place mutation sees a refcount of one, and
                    // the paired `CaptureCellSet` returns it.
                    let Value::CaptureCell(c) = &registers[cell as usize] else {
                        return Err(capture_cell_expected("CaptureCellTake"));
                    };
                    let taken = std::mem::replace(&mut *c.lock(), Value::Unit);
                    registers[dst as usize] = taken;
                }
                Op::CaptureCellSet { cell, src } => {
                    // Move (not clone) the value home: leaving the register a
                    // second owner would copy-on-write the whole aggregate on
                    // the next mutation. Every later read of the binding is
                    // preceded by its own load.
                    let Value::CaptureCell(c) = &registers[cell as usize] else {
                        return Err(capture_cell_expected("CaptureCellSet"));
                    };
                    let c = Arc::clone(c);
                    *c.lock() = std::mem::replace(&mut registers[src as usize], Value::Unit);
                }
                Op::CellTake { dst, cell } => {
                    // Last use of the cell, so move its inner out rather than
                    // clone - the caller re-homes it through this register.
                    let taken = match &registers[cell as usize] {
                        Value::MutCell(c) => std::mem::replace(&mut *c.lock(), Value::Unit),
                        other => other.clone(),
                    };
                    registers[dst as usize] = taken;
                }
                Op::BuildTuple { dst, first, count } => {
                    // Clones each value register into a fresh
                    // `Vec<Value>`, wraps in Arc, drops into
                    // `Value::Tuple`.
                    let n = count as usize;
                    let start = first as usize;
                    let mut items: Vec<Value> = Vec::with_capacity(n);
                    for i in 0..n {
                        items.push(registers[start + i].clone());
                    }
                    registers[dst as usize] = Value::Tuple(Arc::from(items));
                }
                Op::BuildArray { dst, first, count } => {
                    let n = count as usize;
                    let start = first as usize;
                    let mut items: Vec<Value> = Vec::with_capacity(n);
                    for i in 0..n {
                        items.push(registers[start + i].clone());
                    }
                    registers[dst as usize] = Value::Array(Arc::new(items));
                }
                Op::BuildArrayRepeat { dst, value, count } => {
                    let n = match &registers[count as usize] {
                        Value::Int(c) if *c >= 0 => *c as usize,
                        Value::Int(_) => {
                            return Err(RuntimeError::Type("negative repeat count".to_string()));
                        }
                        _ => {
                            return Err(RuntimeError::Type("repeat count must be int".to_string()));
                        }
                    };
                    // A scalar repeat lands in flat typed storage - 8 bytes
                    // per element instead of a 16-byte boxed `Value` - the
                    // same routing `Op::BuildRange` and integer array
                    // literals use. Every consumer (indexing, `len`,
                    // iteration, the collection helpers) handles `IntArray` /
                    // `FloatVec` identically to a boxed array of the same
                    // scalars, so this is a pure representation change.
                    registers[dst as usize] = match &registers[value as usize] {
                        Value::Int(elem) => Value::IntArray(Arc::new(vec![*elem; n])),
                        Value::Float(elem) => Value::FloatVec(Arc::new(vec![*elem; n])),
                        // `vec![elem.clone(); n]` calls `Clone::clone` on one
                        // shared value, which for a `Map`/`Set` element is an
                        // `Arc`/handle-id copy - every repeated slot would
                        // alias the same backing table. Deep-clone each slot
                        // independently instead, same as `Op::CloneMapLike`.
                        elem => Value::Array(Arc::new(
                            (0..n).map(|_| map_like_deep_clone(elem)).collect(),
                        )),
                    };
                }
                Op::BuildRange {
                    dst,
                    start,
                    end,
                    inclusive,
                    start_open,
                    end_open,
                } => {
                    let start_val = match &registers[start as usize] {
                        Value::Int(n) => *n,
                        other => {
                            return Err(RuntimeError::Type(format!(
                                "range lower bound must be i64, found `{other}`"
                            )));
                        }
                    };
                    let end_val = match &registers[end as usize] {
                        Value::Int(n) => *n,
                        other => {
                            return Err(RuntimeError::Type(format!(
                                "range upper bound must be i64, found `{other}`"
                            )));
                        }
                    };
                    registers[dst as usize] = crate::stdlib_builtins::iter::new_range_iter(
                        start_val, end_val, inclusive, start_open, end_open,
                    );
                }
                Op::BuildVariant1 {
                    dst,
                    name_idx,
                    field,
                    take_field,
                } => {
                    let Value::Variant(sentinel) = &chunk.consts[name_idx as usize] else {
                        return Err(RuntimeError::Type(
                            "variant constructor constant must be a variant".to_string(),
                        ));
                    };
                    let field = if take_field {
                        std::mem::replace(&mut registers[field as usize], Value::Void)
                    } else {
                        registers[field as usize].clone()
                    };
                    registers[dst as usize] =
                        Value::variant_with_tag_1(sentinel.name.clone(), field);
                }
                Op::BuildVariant2 {
                    dst,
                    name_idx,
                    first,
                    second,
                    take_first,
                    take_second,
                } => {
                    let Value::Variant(sentinel) = &chunk.consts[name_idx as usize] else {
                        return Err(RuntimeError::Type(
                            "variant constructor constant must be a variant".to_string(),
                        ));
                    };
                    let first = if take_first {
                        std::mem::replace(&mut registers[first as usize], Value::Void)
                    } else {
                        registers[first as usize].clone()
                    };
                    let second = if take_second {
                        std::mem::replace(&mut registers[second as usize], Value::Void)
                    } else {
                        registers[second as usize].clone()
                    };
                    registers[dst as usize] =
                        Value::variant_with_tag_2(sentinel.name.clone(), first, second);
                }
                Op::VariantIs {
                    dst,
                    src,
                    name_idx,
                    arity,
                } => {
                    // Both names come from `intern_type_name`: one
                    // pointer compare replaces string content equality.
                    let expected: &'static str = chunk.shape_names[name_idx as usize];
                    let matches = match &registers[src as usize] {
                        Value::Variant(inner) => {
                            inner.name.as_str() == expected && inner.fields.len() == arity as usize
                        }
                        Value::NativeEnum(owner) => {
                            let disc = crate::value::native_enum_disc(owner.ptr, &owner.shape);
                            owner.shape.variants.get(disc).is_some_and(|v| {
                                std::ptr::eq(v.name, expected) && v.fields.len() == arity as usize
                            })
                        }
                        _ => false,
                    };
                    registers[dst as usize] = Value::Bool(matches);
                }
                Op::VariantField { dst, src, idx } => {
                    registers[dst as usize] = match &registers[src as usize] {
                        Value::Variant(inner) => inner
                            .fields
                            .get(idx as usize)
                            .cloned()
                            .unwrap_or(Value::Unit),
                        Value::NativeEnum(owner) => {
                            crate::value::native_enum_field(owner, idx as usize)
                        }
                        _ => Value::Unit,
                    };
                }
                Op::VariantFieldSet { receiver, idx, src } => {
                    let value = registers[src as usize].clone();
                    if let Value::Variant(arc) = &mut registers[receiver as usize] {
                        let inner = Arc::make_mut(arc);
                        if let Some(slot) = inner.fields.get_mut(idx as usize) {
                            *slot = value;
                        }
                    }
                }
                Op::StructIs { dst, src, name_idx } => {
                    let expected: &'static str = chunk.shape_names[name_idx as usize];
                    let matches = matches!(
                        &registers[src as usize],
                        Value::Struct(inner) if inner.name.as_str() == expected
                    );
                    registers[dst as usize] = Value::Bool(matches);
                }
                Op::VariantFieldConsume { dst, src, idx } => {
                    // Drain the payload when the scrutinee is uniquely
                    // owned; clone (matching `VariantField`) when shared
                    // or when the value is not a `Variant`.
                    let result = match &mut registers[src as usize] {
                        Value::Variant(arc) => match Arc::get_mut(arc) {
                            Some(inner) => inner
                                .fields
                                .get_mut(idx as usize)
                                .map(|slot| std::mem::replace(slot, Value::Void))
                                .unwrap_or(Value::Unit),
                            None => arc.fields.get(idx as usize).cloned().unwrap_or(Value::Unit),
                        },
                        Value::NativeEnum(arc) => match Arc::get_mut(arc) {
                            Some(owner) => {
                                crate::value::native_enum_field_consume(owner, idx as usize)
                                    .unwrap_or_else(|| {
                                        crate::value::native_enum_field(owner, idx as usize)
                                    })
                            }
                            None => crate::value::native_enum_field(arc, idx as usize),
                        },
                        _ => Value::Unit,
                    };
                    registers[dst as usize] = result;
                }
                Op::IndexGetConsume { dst, base, index } => {
                    // Read the integer index before the mutable base
                    // borrow so the two register accesses don't overlap.
                    let idx_int = match &registers[index as usize] {
                        Value::Int(n) => Some(*n),
                        Value::Uint(n) => Some(*n as i64),
                        _ => None,
                    };
                    let result = match idx_int {
                        Some(raw) => index_get_consume(&mut registers[base as usize], raw)?,
                        None => {
                            let idx_val = registers[index as usize].clone();
                            index_get(&registers[base as usize], &idx_val)?
                        }
                    };
                    registers[dst as usize] = result;
                }
                Op::TupleIndexConsume {
                    dst,
                    receiver,
                    index,
                } => {
                    // Drain a uniquely-owned tuple / array field; clone
                    // (matching `TupleIndex`) when the aggregate is shared,
                    // and keep `TupleIndex`'s error semantics otherwise.
                    let idx = index as usize;
                    let oob = || RuntimeError::Arithmetic("tuple index out of bounds".to_string());
                    let result = match &mut registers[receiver as usize] {
                        Value::Tuple(arc) | Value::Array(arc) => match Arc::get_mut(arc) {
                            Some(items) => items
                                .get_mut(idx)
                                .map(|slot| std::mem::replace(slot, Value::Void))
                                .ok_or_else(oob)?,
                            None => arc.get(idx).cloned().ok_or_else(oob)?,
                        },
                        Value::Struct(inner) => inner
                            .fields
                            .get(idx)
                            .map(|(_, v)| v.clone())
                            .ok_or_else(oob)?,
                        other => {
                            return Err(RuntimeError::Type(format!(
                                "value of kind `{other}` has no tuple fields"
                            )));
                        }
                    };
                    registers[dst as usize] = result;
                }
                Op::IntArrayGetI64 {
                    dst_i,
                    base,
                    index_i,
                } => unsafe {
                    let idx = *ints.get_unchecked(index_i as usize);
                    let b = registers.get_unchecked(base as usize);
                    let i = usize::try_from(idx).map_err(|_| crate::vm::index_oob_panic(idx, 0))?;
                    let value = match b {
                        Value::IntArray(data) => data
                            .get(i)
                            .copied()
                            .ok_or_else(|| crate::vm::index_oob_panic(i as i64, data.len()))?,
                        Value::ByteArray(data) => data
                            .get(i)
                            .copied()
                            .map(i64::from)
                            .ok_or_else(|| crate::vm::index_oob_panic(i as i64, data.len()))?,
                        Value::InlineByteArray(data) => data
                            .get(i)
                            .copied()
                            .map(i64::from)
                            .ok_or_else(|| crate::vm::index_oob_panic(i as i64, data.len()))?,
                        Value::ByteVec(data) => data
                            .get(i)
                            .copied()
                            .map(i64::from)
                            .ok_or_else(|| crate::vm::index_oob_panic(i as i64, data.len()))?,
                        Value::Array(data) => match data.get(i) {
                            Some(Value::Int(value)) => *value,
                            Some(other) => {
                                return Err(RuntimeError::Type(format!(
                                    "expected i64 array element, found `{other}`"
                                )));
                            }
                            None => {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                        },
                        Value::FloatVec(_) | Value::FloatArray(_) => {
                            return Err(RuntimeError::Type(
                                "IntArrayGetI64: receiver is a float array".to_string(),
                            ));
                        }
                        _ => {
                            return Err(RuntimeError::Type(
                                "IntArrayGetI64: receiver lost flat invariant".to_string(),
                            ));
                        }
                    };
                    *ints.get_unchecked_mut(dst_i as usize) = value;
                },
                Op::IntArraySetI64 {
                    base,
                    index_i,
                    value_i,
                } => unsafe {
                    let idx = *ints.get_unchecked(index_i as usize);
                    let new_val = *ints.get_unchecked(value_i as usize);
                    crate::stdlib_builtins::iter::note_vec_element_replacement(
                        registers.get_unchecked(base as usize),
                        idx,
                        &Value::Int(new_val),
                    );
                    let b = registers.get_unchecked_mut(base as usize);
                    let i = usize::try_from(idx).map_err(|_| crate::vm::index_oob_panic(idx, 0))?;
                    match b {
                        Value::IntArray(data) => {
                            if i >= data.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                            *Arc::make_mut(data).get_unchecked_mut(i) = new_val;
                        }
                        Value::ByteArray(data) => {
                            if i >= data.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                            *Arc::make_mut(data).get_unchecked_mut(i) = new_val as u8;
                        }
                        Value::InlineByteArray(data) => {
                            if i >= data.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                            *Arc::make_mut(data).get_unchecked_mut(i) = new_val as u8;
                        }
                        Value::ByteVec(data) => {
                            if i >= data.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                            *Arc::make_mut(data).get_unchecked_mut(i) = new_val as u8;
                        }
                        Value::Array(data) => {
                            if i >= data.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                            *Arc::make_mut(data).get_unchecked_mut(i) = Value::Int(new_val);
                        }
                        _ => {
                            return Err(RuntimeError::Type(
                                "IntArraySetI64: receiver lost flat invariant".to_string(),
                            ));
                        }
                    }
                },
                Op::IntArraySwap { base, i_i, j_i } => unsafe {
                    let i_idx = *ints.get_unchecked(i_i as usize);
                    let j_idx = *ints.get_unchecked(j_i as usize);
                    if i_idx < 0 || j_idx < 0 {
                        let len = flat_receiver_len(registers.get_unchecked(base as usize));
                        return Err(swap_out_of_bounds(i_idx, j_idx, len));
                    }
                    let i = i_idx as usize;
                    let j = j_idx as usize;
                    let b = registers.get_unchecked_mut(base as usize);
                    match b {
                        Value::IntArray(data) => {
                            let values = Arc::make_mut(data);
                            if i >= values.len() || j >= values.len() {
                                return Err(swap_out_of_bounds(i_idx, j_idx, values.len()));
                            }
                            values.swap(i, j);
                        }
                        Value::ByteArray(data) => {
                            let values = Arc::make_mut(data);
                            if i >= values.len() || j >= values.len() {
                                return Err(swap_out_of_bounds(i_idx, j_idx, values.len()));
                            }
                            values.swap(i, j);
                        }
                        Value::InlineByteArray(data) => {
                            let values = Arc::make_mut(data);
                            if i >= values.len() || j >= values.len() {
                                return Err(swap_out_of_bounds(i_idx, j_idx, values.len()));
                            }
                            values.swap(i, j);
                        }
                        Value::ByteVec(data) => {
                            let values = Arc::make_mut(data);
                            if i >= values.len() || j >= values.len() {
                                return Err(swap_out_of_bounds(i_idx, j_idx, values.len()));
                            }
                            values.swap(i, j);
                        }
                        Value::Array(data) => {
                            let values = Arc::make_mut(data);
                            if i >= values.len() || j >= values.len() {
                                return Err(swap_out_of_bounds(i_idx, j_idx, values.len()));
                            }
                            values.swap(i, j);
                        }
                        _ => {
                            return Err(RuntimeError::Type(
                                "IntArraySwap: receiver lost flat invariant".to_string(),
                            ));
                        }
                    }
                },
                Op::FloatVecSwap { base, i_i, j_i } => unsafe {
                    let i_idx = *ints.get_unchecked(i_i as usize);
                    let j_idx = *ints.get_unchecked(j_i as usize);
                    if i_idx < 0 || j_idx < 0 {
                        let len = flat_receiver_len(registers.get_unchecked(base as usize));
                        return Err(swap_out_of_bounds(i_idx, j_idx, len));
                    }
                    let i = i_idx as usize;
                    let j = j_idx as usize;
                    let b = registers.get_unchecked_mut(base as usize);
                    let Value::FloatVec(data) = b else {
                        return Err(RuntimeError::Type(
                            "FloatVecSwap: receiver lost flat invariant".to_string(),
                        ));
                    };
                    let v = Arc::make_mut(data);
                    if i >= v.len() || j >= v.len() {
                        return Err(swap_out_of_bounds(i_idx, j_idx, v.len()));
                    }
                    v.swap(i, j);
                },
                Op::BuildFloatVec {
                    dst_v,
                    first_f,
                    count,
                } => {
                    let n = count as usize;
                    let start = first_f as usize;
                    let mut data: Vec<f64> = Vec::with_capacity(n);
                    // SAFETY: `first_f .. first_f + count` is a
                    // compile-allocated span in the float register
                    // file (mirrors `BuildIntArray`).
                    unsafe {
                        for i in 0..n {
                            data.push(*floats.get_unchecked(start + i));
                        }
                    }
                    registers[dst_v as usize] = Value::FloatVec(Arc::new(data));
                }
                Op::FloatVecGetF64 {
                    dst_f,
                    base,
                    index_i,
                } => unsafe {
                    let idx = *ints.get_unchecked(index_i as usize);
                    let b = registers.get_unchecked(base as usize);
                    let i = usize::try_from(idx).map_err(|_| crate::vm::index_oob_panic(idx, 0))?;
                    let value = match b {
                        Value::FloatVec(data) => data
                            .get(i)
                            .copied()
                            .ok_or_else(|| crate::vm::index_oob_panic(i as i64, data.len()))?,
                        Value::Array(data) => match data.get(i) {
                            Some(Value::Float(value)) => *value,
                            Some(other) => {
                                return Err(RuntimeError::Type(format!(
                                    "expected f64 array element, found `{other}`"
                                )));
                            }
                            None => {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                        },
                        Value::FloatArray(data) if data.stride == 1 => {
                            data.data.get(i).copied().ok_or_else(|| {
                                crate::vm::index_oob_panic(i as i64, data.data.len())
                            })?
                        }
                        _ => {
                            return Err(RuntimeError::Type(
                                "FloatVecGetF64: receiver lost flat invariant".to_string(),
                            ));
                        }
                    };
                    *floats.get_unchecked_mut(dst_f as usize) = value;
                },
                Op::FloatVecSetF64 {
                    base,
                    index_i,
                    value_f,
                } => unsafe {
                    let idx = *ints.get_unchecked(index_i as usize);
                    let new_f = *floats.get_unchecked(value_f as usize);
                    crate::stdlib_builtins::iter::note_vec_element_replacement(
                        registers.get_unchecked(base as usize),
                        idx,
                        &Value::Float(new_f),
                    );
                    let b = registers.get_unchecked_mut(base as usize);
                    let i = usize::try_from(idx).map_err(|_| crate::vm::index_oob_panic(idx, 0))?;
                    match b {
                        Value::FloatVec(data_arc) => {
                            let buf = Arc::make_mut(data_arc);
                            if i >= buf.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, buf.len()));
                            }
                            *buf.get_unchecked_mut(i) = new_f;
                        }
                        Value::Array(data) => {
                            if i >= data.len() {
                                return Err(crate::vm::index_oob_panic(i as i64, data.len()));
                            }
                            *Arc::make_mut(data).get_unchecked_mut(i) = Value::Float(new_f);
                        }
                        _ => {
                            return Err(RuntimeError::Type(
                                "FloatVecSetF64: receiver is not a float vector".to_string(),
                            ));
                        }
                    }
                },
                Op::BuildIntMap { dst_v } => {
                    registers[dst_v as usize] = Value::IntMap(Arc::new(parking_lot::Mutex::new(
                        crate::value::dense_map_with_capacity(16),
                    )));
                }
                Op::BuildStrIntMap { dst_v } => {
                    registers[dst_v as usize] = Value::StrIntMap(Arc::new(
                        parking_lot::Mutex::new(crate::value::dense_map_with_capacity(16)),
                    ));
                }
                Op::IntMapInc {
                    dst_i,
                    map_reg,
                    key_i,
                    by_i,
                } => unsafe {
                    let key = *ints.get_unchecked(key_i as usize);
                    let by = *ints.get_unchecked(by_i as usize);
                    let m = registers.get_unchecked(map_reg as usize);
                    let Value::IntMap(map) = m else {
                        return Err(RuntimeError::Type(
                            "IntMapInc: receiver lost typed invariant".to_string(),
                        ));
                    };
                    let mut guard = map.lock();
                    let entry = guard.entry(key).or_insert(0);
                    *entry += by;
                    let post = *entry;
                    drop(guard);
                    *ints.get_unchecked_mut(dst_i as usize) = post;
                },
                Op::IntMapGetOr {
                    dst_i,
                    map_reg,
                    key_i,
                    default_i,
                } => unsafe {
                    let key = *ints.get_unchecked(key_i as usize);
                    let default = *ints.get_unchecked(default_i as usize);
                    let m = registers.get_unchecked(map_reg as usize);
                    let Value::IntMap(map) = m else {
                        return Err(RuntimeError::Type(
                            "IntMapGetOr: receiver lost typed invariant".to_string(),
                        ));
                    };
                    let v = map.lock().get(&key).copied().unwrap_or(default);
                    *ints.get_unchecked_mut(dst_i as usize) = v;
                },
                Op::IntMapInsert {
                    dst_v,
                    map_reg,
                    key_i,
                    value_i,
                } => unsafe {
                    let key = *ints.get_unchecked(key_i as usize);
                    let val = *ints.get_unchecked(value_i as usize);
                    let m = registers.get_unchecked(map_reg as usize);
                    let Value::IntMap(map) = m else {
                        return Err(RuntimeError::Type(
                            "IntMapInsert: receiver lost typed invariant".to_string(),
                        ));
                    };
                    let previous = map.lock().insert(key, val);
                    registers[dst_v as usize] = match previous {
                        Some(value) => Value::variant("Some", vec![Value::Int(value)]),
                        None => Value::variant("None", Vec::new()),
                    };
                },
                Op::IntMapLen { dst_i, map_reg } => unsafe {
                    let m = registers.get_unchecked(map_reg as usize);
                    let Value::IntMap(map) = m else {
                        return Err(RuntimeError::Type(
                            "IntMapLen: receiver lost typed invariant".to_string(),
                        ));
                    };
                    let n = map.lock().len() as i64;
                    *ints.get_unchecked_mut(dst_i as usize) = n;
                },
                Op::IntMapContainsKey {
                    dst_v,
                    map_reg,
                    key_i,
                } => unsafe {
                    let key = *ints.get_unchecked(key_i as usize);
                    let m = registers.get_unchecked(map_reg as usize);
                    let Value::IntMap(map) = m else {
                        return Err(RuntimeError::Type(
                            "IntMapContainsKey: receiver lost typed invariant".to_string(),
                        ));
                    };
                    let has = map.lock().contains_key(&key);
                    registers[dst_v as usize] = Value::Bool(has);
                },
            }
        }
    }
}

/// Publishes the final values of `&mut Vec<T>` / `&mut [T]` parameter
/// registers back into their caller-provided write-back cells. Runs
/// on every successful return path of [`Vm::run`]; error unwinds skip
/// it (the caller's aggregate keeps its pre-call value, matching the
/// compiled tiers' no-partial-publish behaviour for panics).
/// Typed-storage promotion for a scalar push. A first `i64` / `f64` push
/// into an empty generic `Array` switches the vector to flat typed storage
/// (`IntArray` / `FloatVec`, 8 bytes per element instead of a 16-byte boxed
/// `Value`), so a push-built scalar vec costs the same as a literal-built
/// one. A float push onto an `IntArray` widens it to `FloatVec`: an `[i64]`
/// can never receive a float, so the receiver is an `[f64]` whose elements
/// so far were integer-valued. Returns the replacement value, or `None`
/// when the ordinary push applies.
fn promote_scalar_push(recv: &Value, new_value: &Value) -> Option<Value> {
    match (recv, new_value) {
        (Value::Array(items), Value::Int(n)) if items.is_empty() => {
            Some(Value::IntArray(Arc::new(vec![*n])))
        }
        (Value::Array(items), Value::Float(f)) if items.is_empty() => {
            Some(Value::FloatVec(Arc::new(vec![*f])))
        }
        (Value::IntArray(data), Value::Float(f)) => {
            let mut wide: Vec<f64> = data.iter().map(|n| *n as f64).collect();
            wide.push(*f);
            Some(Value::FloatVec(Arc::new(wide)))
        }
        _ => None,
    }
}

pub(super) fn vec_push_value(recv: &mut Value, new_value: Value) {
    if let Some(promoted) = promote_scalar_push(recv, &new_value) {
        *recv = promoted;
        return;
    }
    match recv {
        Value::Array(items) => Arc::make_mut(items).push(new_value),
        Value::IntArray(data) => {
            if let Value::Int(n) = new_value {
                Arc::make_mut(data).push(n);
            }
        }
        Value::ByteArray(data) => {
            if let Value::Int(n) = new_value {
                let mut values = data.to_vec();
                values.push(n as u8);
                *recv = Value::ByteVec(Arc::new(values));
            }
        }
        Value::InlineByteArray(data) => {
            if let Value::Int(n) = new_value {
                let mut values = data.to_vec();
                values.push(n as u8);
                *recv = Value::ByteVec(Arc::new(values));
            }
        }
        Value::ByteVec(data) => {
            if let Value::Int(n) = new_value {
                Arc::make_mut(data).push(n as u8);
            }
        }
        Value::FloatVec(data) => match new_value {
            Value::Float(f) => Arc::make_mut(data).push(f),
            Value::Int(n) => Arc::make_mut(data).push(n as f64),
            _ => {}
        },
        _ => {}
    }
}

/// The compiler brackets a capture-cell binding's instructions with cell
/// traffic keyed on the binding's own cell register, so an operand that
/// is not a cell means the bracketing itself is malformed.
fn capture_cell_expected(op: &str) -> RuntimeError {
    RuntimeError::Panic(format!("{op}: operand does not hold a capture cell"))
}

fn publish_ref_cells(cells: &[(usize, Arc<ThreadConfinedCell>)], registers: &mut [Value]) {
    // Move (not clone) each `&mut` param's final value back into its cell.
    // Cloning would leave the value referenced by both the cell and the
    // returning frame's register, so the caller's `CellTake` would receive
    // a shared value - and a subsequent in-place mutation (`v.push` /
    // `*s += …`) would copy-on-write the whole collection on every call,
    // turning a build loop into O(n^2). The frame is exiting, so emptying
    // the register is safe.
    for (slot, cell) in cells {
        *cell.lock() = std::mem::replace(&mut registers[*slot], Value::Unit);
    }
}

fn shuffled_select_order(n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if n <= 1 {
        return order;
    }
    #[cfg(not(target_arch = "wasm32"))]
    let mut x = {
        let nanos = gossamer_runtime::platform::system_time_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_u64, |d| d.as_nanos() as u64);
        nanos
            ^ ((std::thread::current().name().map_or(0, str::len) as u64) << 32)
            ^ 0x9E37_79B9_7F4A_7C15
    };
    #[cfg(target_arch = "wasm32")]
    let mut x = {
        static SELECT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        SELECT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) ^ 0x9E37_79B9_7F4A_7C15
    };
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = (x as usize) % (i + 1);
        order.swap(i, j);
    }
    order
}

/// One non-blocking poll over every `select` arm in pseudo-random order.
/// Returns the chosen arm's `body_block` - writing a received value
/// into the recv arm's `bind_reg`, or completing a send - or `None`
/// when no arm (including `default`) is ready. The two-pass scan
/// (recv/send first, then `default`) chooses a `default` arm only once
/// every recv/send arm has been found not-ready.
fn select_try_once(
    arms: &[crate::bytecode::SelectArmMeta],
    registers: &mut [Value],
) -> Option<crate::bytecode::InstrIdx> {
    use crate::bytecode::SelectArmKind;
    for index in shuffled_select_order(arms.len()) {
        let arm = &arms[index];
        match arm.kind {
            SelectArmKind::Recv => {
                let Value::Channel(ch) = &registers[arm.channel_reg as usize] else {
                    continue;
                };
                let ch = ch.clone();
                if let Some(value) = ch.try_recv() {
                    registers[arm.bind_reg as usize] = value;
                    return Some(arm.body_block);
                }
                // Go semantics: a recv arm on a closed (and drained)
                // channel is always ready, binding the element-type
                // zero value. The 8-byte select payload contract is
                // i64-shaped on every tier, so the VM mirrors the
                // compiled tier's `last_value = 0`.
                if ch.is_closed() {
                    registers[arm.bind_reg as usize] = Value::Int(0);
                    return Some(arm.body_block);
                }
            }
            SelectArmKind::Send => {
                let Value::Channel(ch) = &registers[arm.channel_reg as usize] else {
                    continue;
                };
                let ch = ch.clone();
                let value = registers[arm.value_reg as usize].clone();
                // A bounded channel at capacity makes this send arm
                // not-ready; fall through to the next arm rather than
                // blocking inside the non-blocking probe.
                // A closed channel makes this arm not-ready rather than
                // panicking inside the readiness probe: the blocking send
                // the arm stands for is what raises that.
                if ch.try_send(value) == crate::value::SendOutcome::Sent {
                    return Some(arm.body_block);
                }
            }
            SelectArmKind::Default => {}
        }
    }
    for arm in arms {
        if arm.kind == SelectArmKind::Default {
            return Some(arm.body_block);
        }
    }
    None
}

/// Polls every `select` arm, registering one waiter across every
/// channel arm when nothing is ready and no `default` exists. Any
/// send, recv, close, or receiver-arrival event wakes the waiter and
/// the VM re-polls all arms in a fresh pseudo-random order.
/// The arm a select resolves to once its cohort is cancelled: the
/// `default` arm when one is written, else the first arm, whose recv
/// yields the element-type zero the way a closed channel does.
fn select_cancelled_target(
    arms: &[crate::bytecode::SelectArmMeta],
    registers: &mut [Value],
) -> Option<crate::bytecode::InstrIdx> {
    use crate::bytecode::SelectArmKind;
    let arm = arms
        .iter()
        .find(|a| a.kind == SelectArmKind::Default)
        .or_else(|| arms.first())?;
    if arm.kind == SelectArmKind::Recv {
        // The same element-type zero a closed channel's recv arm binds.
        registers[arm.bind_reg as usize] = Value::Int(0);
    }
    Some(arm.body_block)
}

fn select_dispatch(
    arms: &[crate::bytecode::SelectArmMeta],
    registers: &mut [Value],
) -> RuntimeResult<crate::bytecode::InstrIdx> {
    use crate::bytecode::SelectArmKind;
    if let Some(target) = select_try_once(arms, registers) {
        return Ok(target);
    }
    let channels: Vec<crate::value::Channel> = arms
        .iter()
        .filter(|a| a.kind != SelectArmKind::Default)
        .filter_map(|a| match &registers[a.channel_reg as usize] {
            Value::Channel(ch) => Some(ch.clone()),
            _ => None,
        })
        .collect();
    loop {
        if let Some(target) = select_try_once(arms, registers) {
            return Ok(target);
        }
        // A cancelled cohort makes every blocking arm behave the way a
        // closed channel does, so a select under cancellation resolves
        // instead of waiting for a partner that will never come.
        if crate::stdlib_builtins::cohort::current_is_cancelled()
            && let Some(target) = select_cancelled_target(arms, registers)
        {
            return Ok(target);
        }
        // No arm can become ready after this point on the browser build:
        // every goroutine that could reach one of these channels has
        // already run to completion.
        if !gossamer_runtime::platform::CAN_BLOCK {
            return Err(RuntimeError::WouldNeverWake("select"));
        }
        if channels.is_empty() {
            gossamer_runtime::platform::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        let waiter = crate::value::Channel::select_waiter();
        for ch in &channels {
            ch.register_select_waiter(&waiter);
        }
        if let Some(target) = select_try_once(arms, registers) {
            for ch in &channels {
                ch.unregister_select_waiter(&waiter);
            }
            return Ok(target);
        }
        crate::value::Channel::wait_select(&waiter);
        for ch in &channels {
            ch.unregister_select_waiter(&waiter);
        }
    }
}
