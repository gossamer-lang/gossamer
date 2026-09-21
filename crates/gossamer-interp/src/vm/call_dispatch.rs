#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;
#[cfg(target_arch = "wasm32")]
use crate::jit_stub as jit_backend;
#[cfg(not(target_arch = "wasm32"))]
use gossamer_codegen_cranelift as jit_backend;

/// How a spawned goroutine is named in the diagnostic registry.
struct GoroutineOrigin {
    name: String,
    file: String,
    line: u32,
}

impl Vm {
    /// Invokes a top-level function by name.
    pub fn call(&self, name: &str, args: Vec<Value>) -> RuntimeResult<Value> {
        let callee = self
            .lookup_global(name)
            .ok_or_else(|| self.unresolved(name))?;
        let interned = crate::value::intern_type_name(name);
        self.call_stack.borrow_mut().clear();
        self.call_stack
            .borrow_mut()
            .push(VmCallStackFrame::new(interned));
        let result = self.as_comparator_host(|| self.apply(callee, args));
        if result.is_ok() {
            self.call_stack.borrow_mut().pop();
        }
        result
    }

    /// Snapshot of the in-flight (or last failing) call stack.
    /// Outermost frame first. Returns owned `String`s for API
    /// stability; underlying storage is `&'static str`.
    #[must_use]
    pub fn call_stack_snapshot(&self) -> Vec<String> {
        self.call_stack
            .borrow()
            .iter()
            .map(|frame| frame.function.to_string())
            .collect()
    }

    /// Points the active-VM slot at this VM for the duration of `body`, so a
    /// fault raised inside compiled code can render the interpreted frames
    /// that reached it. Restores the previous value, which is what keeps a
    /// nested run (a goroutine, a REPL replay) from clearing an outer one.
    pub fn with_active_trace<R>(&self, body: impl FnOnce() -> R) -> R {
        install_trace_hook();
        let previous = ACTIVE_VM.with(|slot| slot.replace(std::ptr::from_ref(self).cast()));
        let out = body();
        ACTIVE_VM.with(|slot| slot.set(previous));
        out
    }

    /// A JIT frame found on the machine stack, placed in the source.
    fn jit_call_stack_frame(&self, frame: &jit_backend::JitFrame) -> CallStackFrame {
        let location = frame.span.and_then(|span| {
            self.jit_source_locations
                .borrow()
                .as_ref()
                .and_then(|locations| locations.get(&span).copied())
        });
        CallStackFrame {
            function: frame.function.to_string(),
            file: location.map(|location| location.file.to_string()),
            line: location.map(|location| location.line),
            column: location.map(|location| location.column),
        }
    }

    /// Snapshot of the in-flight (or last failing) call stack with source
    /// positions for frames whose bytecode was compiled with a source map.
    #[must_use]
    pub fn call_stack_frames(&self) -> Vec<CallStackFrame> {
        let rendered =
            |function: &str, location: Option<crate::bytecode::SourceLocation>| CallStackFrame {
                function: function.to_string(),
                file: location.map(|location| location.file.to_string()),
                line: location.map(|location| location.line),
                column: location.map(|location| location.column),
            };
        let mut frames = Vec::new();
        for frame in self.call_stack.borrow().iter() {
            // An inlined call has no frame of its own: the chain of sites the
            // position sits in, outermost first, names each function and the
            // call in it that led inward.
            let mut chain = Vec::new();
            let mut site = frame.inline_site;
            while let Some(index) = site {
                let Some(inline) = frame.inline_sites.get(index as usize) else {
                    break;
                };
                chain.push(inline);
                site = inline.parent;
            }
            chain.reverse();
            let mut function = frame.function;
            for inline in chain {
                frames.push(rendered(function, inline.call));
                function = inline.function;
            }
            frames.push(rendered(function, frame.location));
        }
        frames
    }

    /// Public entry for invoking a function VALUE (not a name): used by
    /// the CLI runner to deliver the main-goroutine panic message to a
    /// `runtime::set_panic_hook` hook.
    pub fn apply_value(&self, callee: Value, args: Vec<Value>) -> RuntimeResult<Value> {
        self.apply(Global::Value(callee), args)
    }

    /// Deliver a panic message to the registered hook, if any. The
    /// stored hook may be a function value or (bytecode tier) the bare
    /// function NAME - resolve through globals in that case. Returns
    /// false when no hook is set or the call failed (caller prints the
    /// default report).
    pub fn invoke_panic_hook(&self, msg: &str) -> bool {
        let Some(hook) = crate::panic_hook_value() else {
            return false;
        };
        let arg = Value::String(SmolStr::from(msg));
        let resolved = match &hook {
            Value::String(name) => match self.lookup_global(name.as_str()) {
                Some(g) => g,
                None => return false,
            },
            other => Global::Value(other.clone()),
        };
        self.apply(resolved, vec![arg]).is_ok()
    }

    pub(crate) fn apply(&self, global: Global, args: Vec<Value>) -> RuntimeResult<Value> {
        match global {
            Global::Fn(chunk) => {
                let mut chunk = chunk;
                let mut args = args;
                // Direct named bytecode calls suspend their caller here rather
                // than recursively entering `apply -> run`. The destination
                // register belongs to the suspended frame; the child result
                // is written just before that frame resumes.
                let mut suspended: Vec<(u16, crate::vm::run::SuspendedFrame)> = Vec::new();
                let mut resumed: Option<crate::vm::run::SuspendedFrame> = None;
                // Byte-precise native-stack guard, consulted before the frame
                // count. A JIT-compiled body recurses on the real OS stack
                // (not the heap frame pool `MAX_CALL_DEPTH` bounds), so a
                // frame count alone cannot stop it before the guard page; the
                // armed byte budget catches that recursion at the boundary and
                // raises a clean stack-overflow, ending only the current
                // goroutine instead of aborting the whole process. No-op when
                // unarmed.
                if gossamer_coro::stack_guard_tripped() {
                    return Err(RuntimeError::StackOverflow(MAX_CALL_DEPTH));
                }
                // Refuse non-tail calls beyond the goroutine call-depth cap.
                // Every ordinary call still adds an `apply()` + `run()` pair
                // to the native stack; direct named tail calls are replaced
                // by the loop below after their old frame is dropped.
                let depth = self.call_depth.get();
                if depth >= MAX_CALL_DEPTH {
                    return Err(RuntimeError::StackOverflow(MAX_CALL_DEPTH));
                }
                self.call_depth.set(depth + 1);

                // Push a frame on the call stack so a runtime error
                // mid-body reports the chain. Pop on success only;
                // a propagating error keeps the frame so callers
                // can read it via `call_stack_snapshot`.
                let pushed_frame = self
                    .call_stack
                    .borrow()
                    .last()
                    .is_none_or(|top| top.function != chunk.name);
                if pushed_frame {
                    self.call_stack
                        .borrow_mut()
                        .push(VmCallStackFrame::new(chunk.name));
                }
                // Tail calls reuse the native frame but retain their logical
                // frames for panic diagnostics. Successful completion removes
                // this many entries at once; an error intentionally leaves
                // them available to the caller's traceback renderer.
                let mut tail_frames = 0usize;
                loop {
                    // A resumed frame has already crossed its entry boundary:
                    // it must neither consume another JIT hot-count tick nor
                    // attempt to marshal an empty `args` vector into native
                    // code. Its live arguments remain in the suspended
                    // register file.
                    let resuming = resumed.is_some();
                    let state = self.chunk_state_for(&chunk);
                    // Tier D2 - decrement the per-`Vm` hot counter and
                    // trigger a deferred JIT compile when the budget is
                    // spent. The counter is per-thread (in `ChunkState`),
                    // so each goroutine independently warms up. `main`
                    // participates like any body: sret covers every
                    // aggregate-return shape, so a hot loop living
                    // directly in the implicit top-level `main` promotes.
                    // A loop-bearing `main` sits in the eager set (counter
                    // starts at 1) and compiles on its single call; a
                    // loop-free `main` never trips its size-scaled
                    // threshold on one call, keeping short scripts off
                    // the Cranelift compile pass entirely.
                    let hot = state.hot_counter.get();
                    if !resuming && hot > 0 && hot != crate::bytecode::HOT_DISABLED {
                        state.jit_observed_work.set(
                            state
                                .jit_observed_work
                                .get()
                                .saturating_add(state.instr_count.max(1)),
                        );
                        let next = hot - 1;
                        state.hot_counter.set(next);
                        if next == 0 {
                            self.jit_counters.tier_up_requested();
                            if state.jit_observed_work.get() >= state.jit_min_work {
                                self.try_compile_jit_lazy(chunk.name);
                            } else {
                                self.jit_counters.work_floor_deferred();
                                state.hot_counter.set(1);
                            }
                        }
                    }
                    // Tier D1 - if the deferred compile produced a
                    // native entry for this chunk, route through the
                    // trampoline first. The override map is shared
                    // across goroutines via `Arc<RwLock<JitState>>`, so
                    // a child goroutine that tripped the hot counter
                    // installs entries every other thread sees.
                    //
                    // Fast path: skip the RwLock probe entirely when no
                    // overrides are installed. The atomic load is a
                    // single ~1 ns instruction; the RwLock read costs
                    // ~6-8 ns of CAS that compounds across recursive
                    // call chains where every leaf fires through `apply`.
                    let jit_opt =
                        if resuming || self.jit_override_count.load(Ordering::Acquire) == 0 {
                            None
                        } else {
                            // Resolve once per ChunkState, then read a plain
                            // field - no lock, no string hash. Sound because
                            // JIT install is one-shot and the map only shrinks.
                            let mut slot = state.jit_resolve.borrow_mut();
                            if matches!(&*slot, crate::vm::JitResolve::Unresolved) {
                                let chunk_key = Arc::as_ptr(&chunk) as usize;
                                let override_jit = {
                                    let jit = self.jit.read();
                                    jit.chunk_overrides.get(&chunk_key).cloned()
                                };
                                let resolved = match override_jit {
                                    Some(j) => match jit_call::prepare(j) {
                                        Some(p) => crate::vm::JitResolve::Some(std::rc::Rc::new(p)),
                                        None => crate::vm::JitResolve::None,
                                    },
                                    None => crate::vm::JitResolve::None,
                                };
                                *slot = resolved;
                            }
                            match &*slot {
                                crate::vm::JitResolve::Some(p) => Some(p.clone()),
                                _ => None,
                            }
                        };
                    if let Some(prepared) = jit_opt {
                        match jit_call::invoke_prepared(&prepared, &args, &self.jit_graph_cache) {
                            jit_call::Dispatch::Ok(value) => {
                                prepared.record_hit();
                                // This is an intentionally conservative
                                // payback signal: one direct native dispatch
                                // bypasses this chunk's bytecode body. Native
                                // intra-JIT calls may avoid additional VM
                                // instructions, but we do not estimate them.
                                self.jit_counters.saved_vm_instructions(state.instr_count);
                                if jit_call::jit_trace() {
                                    eprintln!("jit: native hit {}", prepared.jit.name);
                                }
                                if let Some((dst, mut parent)) = suspended.pop() {
                                    // A nested native body completes exactly
                                    // like `RunControl::Return`: publish its
                                    // result into the suspended bytecode
                                    // caller, retire the child's logical
                                    // frame, and continue the trampoline.
                                    self.call_depth.set(self.call_depth.get().saturating_sub(1));
                                    let mut stack = self.call_stack.borrow_mut();
                                    for _ in 0..=tail_frames {
                                        stack.pop();
                                    }
                                    drop(stack);
                                    tail_frames = 0;
                                    parent.registers[dst as usize] = value;
                                    chunk = Arc::clone(&parent.chunk);
                                    resumed = Some(parent);
                                    args = Vec::new();
                                    continue;
                                }
                                self.call_depth.set(self.call_depth.get().saturating_sub(1));
                                let cleanup_frames = tail_frames + usize::from(pushed_frame);
                                if cleanup_frames > 0 {
                                    let mut stack = self.call_stack.borrow_mut();
                                    for _ in 0..cleanup_frames {
                                        stack.pop();
                                    }
                                }
                                return Ok(value);
                            }
                            jit_call::Dispatch::Fallback => {
                                if jit_call::jit_trace() {
                                    eprintln!(
                                        "jit: fallback to bytecode for {}",
                                        prepared.jit.name
                                    );
                                }
                                // A body whose args never marshal (enum values
                                // arriving as bytecode `Value::Variant`) wastes a
                                // marshal attempt on every call. After enough
                                // consecutive misses with no native hit, demote
                                // the ChunkState slot to bytecode-only so the
                                // attempt stops.
                                if prepared.record_fallback_should_demote() {
                                    *state.jit_resolve.borrow_mut() = crate::vm::JitResolve::None;
                                    if jit_call::jit_trace() {
                                        eprintln!(
                                            "jit: demote {} (bytecode-only)",
                                            prepared.jit.name
                                        );
                                    }
                                }
                            }
                        }
                    }
                    let run_result = match resumed.take() {
                        Some(frame) => self.resume(frame),
                        None => self.run(Arc::clone(&chunk), state, args),
                    };
                    let control = match run_result {
                        Ok(control) => control,
                        Err(err) => {
                            // The explicit parents will not get individual
                            // Rust unwinds. Retire all their logical depth
                            // slots here while intentionally retaining the
                            // call-stack names for the traceback.
                            let released = suspended.len().saturating_add(1);
                            self.call_depth
                                .set(self.call_depth.get().saturating_sub(released));
                            // Preserve the failing frame for traceback parity
                            // with the non-trampolined call path.
                            return Err(err);
                        }
                    };
                    match control {
                        crate::vm::run::RunControl::Return(value) => {
                            if let Some((dst, mut parent)) = suspended.pop() {
                                // The child completed successfully. Its
                                // logical frame (and any tail frames it grew)
                                // can disappear before the parent resumes.
                                self.call_depth.set(self.call_depth.get().saturating_sub(1));
                                let mut stack = self.call_stack.borrow_mut();
                                for _ in 0..=tail_frames {
                                    stack.pop();
                                }
                                drop(stack);
                                tail_frames = 0;
                                parent.registers[dst as usize] = value;
                                chunk = Arc::clone(&parent.chunk);
                                resumed = Some(parent);
                                args = Vec::new();
                                continue;
                            }
                            // Decrement depth unconditionally on completion so
                            // ordinary recursion and early-return paths both
                            // release their slot. A tail-call chain shares this
                            // one logical slot because its old frame is gone.
                            self.call_depth.set(self.call_depth.get().saturating_sub(1));
                            let cleanup_frames = tail_frames + usize::from(pushed_frame);
                            if cleanup_frames > 0 {
                                let mut stack = self.call_stack.borrow_mut();
                                for _ in 0..cleanup_frames {
                                    stack.pop();
                                }
                            }
                            return Ok(value);
                        }
                        crate::vm::run::RunControl::TailCall {
                            chunk: next_chunk,
                            args: tail_args,
                        } => {
                            // The dispatch loop resolved this chunk before it
                            // discarded the old frame, so a tail position is
                            // equally safe for a named function and a closure.
                            // Keep the logical frame for diagnostics while
                            // reusing this trampoline iteration.
                            if tail_frames >= MAX_TAIL_CALL_DEPTH {
                                // Tail calls share one physical depth slot,
                                // so the ordinary call-depth guard cannot
                                // stop an unbounded `fn f() { f() }` loop.
                                // Retire the active explicit frames exactly
                                // as the run-error path does, but retain the
                                // logical names for the diagnostic snapshot.
                                let released = suspended.len().saturating_add(1);
                                self.call_depth
                                    .set(self.call_depth.get().saturating_sub(released));
                                return Err(RuntimeError::StackOverflow(MAX_TAIL_CALL_DEPTH));
                            }
                            self.call_stack
                                .borrow_mut()
                                .push(VmCallStackFrame::new(next_chunk.name));
                            tail_frames += 1;
                            chunk = next_chunk;
                            args = tail_args;
                        }
                        crate::vm::run::RunControl::Call {
                            chunk: next_chunk,
                            args: call_args,
                            dst,
                            parent,
                        } => {
                            let depth = self.call_depth.get();
                            if depth >= MAX_CALL_DEPTH {
                                let released = suspended.len().saturating_add(1);
                                self.call_depth
                                    .set(self.call_depth.get().saturating_sub(released));
                                return Err(RuntimeError::StackOverflow(MAX_CALL_DEPTH));
                            }
                            self.call_depth.set(depth + 1);
                            self.call_stack
                                .borrow_mut()
                                .push(VmCallStackFrame::new(next_chunk.name));
                            suspended.push((dst, parent));
                            chunk = next_chunk;
                            args = call_args;
                            tail_frames = 0;
                        }
                    }
                }
            }
            Global::Value(value) => match value {
                // Builtins / natives take aggregates by value -
                // unwrap any `&mut` write-back cell so they see the
                // plain aggregate (the unchanged value flows back to
                // the caller through the cell afterwards).
                Value::Builtin(inner) => {
                    let args = crate::value::unwrap_mut_cells_unless_writer(inner.name, args);
                    (inner.call)(&args)
                }
                Value::Native(inner) => {
                    let args = crate::value::unwrap_mut_cells_unless_writer(inner.name, args);
                    let mut dispatch = super::native_dispatch::VmDispatch::new(self);
                    (inner.call)(&mut dispatch, &args)
                }
                Value::Closure(closure) => self.invoke_closure(&closure, args),
                Value::Variant(inner) if inner.fields.is_empty() => Ok(Value::variant(
                    inner.name.clone(),
                    Arc::unwrap_or_clone(std::sync::Arc::new(args)),
                )),
                _ => Err(RuntimeError::Type(
                    "global is not callable at this call site".to_string(),
                )),
            },
            // A `static mut` holding a callable value - load the current
            // value and dispatch it as a plain value.
            Global::MutStatic(cell) => {
                let value = cell.lock().clone();
                self.apply(Global::Value(value), args)
            }
        }
    }

    /// Enqueues `task` on the process-wide [`crate::vm::goroutine::pool`],
    /// running it against the worker thread's reused `Vm`. The pool
    /// keeps `num_cpus()` worker threads, each owning a thread-local
    /// `Vm` lazily built on first task and reused across every
    /// goroutine that lands on it - chunk caches stay warm, the frame
    /// pool stays populated, and there is no per-spawn `HashMap::clone`
    /// of globals. After `task` returns, the worker `Vm` is trimmed
    /// back toward steady state so bursty workloads do not leave every
    /// worker holding the union of every task's high-water mark.
    ///
    /// Tasks queue if every worker is busy; the spawning thread does
    /// not block. Programs that mix `gos` invocations within one
    /// process would see stale per-worker state here; the bench-game
    /// shape (one program per process) does not.
    fn spawn_on_pool<F>(&self, origin: GoroutineOrigin, task: F)
    where
        F: FnOnce(&mut Vm) + Send + 'static,
    {
        // Every goroutine is published to the process-wide diagnostic
        // registry, so `pprof::goroutine_profile` and the goroutine dump
        // describe a VM goroutine exactly as they describe one the compiled
        // scheduler runs. Ids come from that registry's own counter, which
        // is its single allocator.
        let gid = gossamer_runtime::sigquit::next_id();
        gossamer_runtime::sigquit::register(gid, origin.name);
        if !origin.file.is_empty() {
            gossamer_runtime::sigquit::set_position(gid, origin.file, origin.line);
        }
        let globals = Arc::clone(&self.globals);
        let mir_bodies = self.mir_bodies.borrow().clone();
        let tcx_snapshot = self.tcx_snapshot.borrow().clone();
        let enum_shape_defs = self.enum_shape_defs.borrow().clone();
        let enum_shape_handles = self.enum_shape_handles.borrow().clone();
        let struct_shape_defs = self.struct_shape_defs.borrow().clone();
        let struct_shape_handles = self.struct_shape_handles.borrow().clone();
        let jit_eager_names = Arc::clone(&self.jit_eager_names.borrow());
        let jit_cache_key = self.jit_cache_key.borrow().clone();
        let comptime_gate = self.comptime_gate.get();
        let comptime_root = gossamer_runtime::comptime_policy::Confined::root();
        crate::vm::goroutine::GoroutinePool::spawn(
            crate::vm::goroutine::pool(),
            Box::new(move || {
                // The entry describes a live goroutine, so it is retired on
                // every path out of the task, a panic included, and the
                // worker is left bound to no goroutine for the next one.
                struct Registered(u32);
                impl Drop for Registered {
                    fn drop(&mut self) {
                        gossamer_runtime::sigquit::set_active_gid(u32::MAX);
                        gossamer_runtime::sigquit::unregister(self.0);
                    }
                }
                let _registered = Registered(gid);
                gossamer_runtime::sigquit::set_active_gid(gid);
                thread_local! {
                    static THREAD_VM: std::cell::OnceCell<std::cell::RefCell<Option<Vm>>> =
                        const { std::cell::OnceCell::new() };
                }
                THREAD_VM.with(|cell| {
                    let vm_cell = cell.get_or_init(|| std::cell::RefCell::new(None));
                    // A goroutine spawned from inside one running on this
                    // thread finds the cached worker `Vm` still bound to the
                    // outer task, so the inner task runs on a `Vm` of its own.
                    let mut cached = vm_cell.try_borrow_mut().ok();
                    // The cached `Vm` is only valid for the program whose
                    // globals it was built from. A thread can outlive one
                    // program (wasm runs every task on the main thread; an
                    // embedding may load several programs in one process),
                    // so key reuse on the globals `Arc` identity.
                    let reusable = cached
                        .as_deref()
                        .and_then(Option::as_ref)
                        .is_some_and(|vm| Arc::ptr_eq(&vm.globals, &globals));
                    // The worker `Vm` shares the parent's loaded globals
                    // (user fns, consts, statics, ADT ctors) via the
                    // `Arc`, so every callable a Native builtin resolves
                    // off-main is already present.
                    let mut fresh = (!reusable).then(|| {
                        Vm::with_globals(
                            globals,
                            mir_bodies,
                            tcx_snapshot,
                            enum_shape_defs,
                            enum_shape_handles,
                            struct_shape_defs,
                            struct_shape_handles,
                            jit_eager_names,
                            jit_cache_key,
                        )
                    });
                    if let Some(slot) = cached.as_deref_mut()
                        && let Some(built) = fresh.take()
                    {
                        *slot = Some(built);
                    }
                    // One of the two holds a `Vm`: an unheld cache was just
                    // filled or already matched, and a held one left `fresh`
                    // owning the task's own.
                    let vm = match cached.as_deref_mut() {
                        Some(slot) => slot.as_mut(),
                        None => fresh.as_mut(),
                    }
                    .expect("the task's worker Vm");
                    // Re-stamped per task: a pooled worker `Vm` outlives
                    // the program it was built for, so the compile-time
                    // policy travels with the spawn rather than the thread.
                    vm.set_comptime_gate(comptime_gate);
                    // The confinement root travels with the spawn, so a
                    // compile-time goroutine reads under the same tree
                    // the folding thread is held to.
                    let _confinement = comptime_root
                        .clone()
                        .map(gossamer_runtime::comptime_policy::Confined::at_root);
                    task(vm);
                    vm.reset_after_task();
                });
            }),
        );
    }

    /// How a spawned goroutine is filed in the diagnostic registry:
    /// the callee's own source-level name and definition site, which is what
    /// the compiled tier's profile shows for the same spawn.
    fn goroutine_origin(callee: &Value) -> GoroutineOrigin {
        match callee {
            Value::Closure(closure) => {
                let site = closure
                    .chunk
                    .instruction_locations
                    .iter()
                    .find_map(|entry| entry.location);
                GoroutineOrigin {
                    name: closure.chunk.name.to_string(),
                    file: site.map(|loc| loc.file.to_string()).unwrap_or_default(),
                    line: site.map_or(0, |loc| loc.line),
                }
            }
            other => GoroutineOrigin {
                name: other.type_name(),
                file: String::new(),
                line: 0,
            },
        }
    }

    /// Spawns a goroutine that runs `callee(args)` through the
    /// bytecode VM. A panic in the spawned callee is isolated to its
    /// worker: it is delivered to the panic hook (if any) or reported
    /// on stderr, and never propagates to the spawning thread.
    pub(crate) fn spawn_goroutine_native(&self, callee: Value, args: Vec<Value>) {
        let origin = Self::goroutine_origin(&callee);
        self.spawn_on_pool(origin, move |vm| {
            if let Err(err) = vm.as_comparator_host(|| vm.dispatch_call(&callee, args)) {
                if !vm.invoke_panic_hook(&crate::panic_message(&err)) {
                    // Report an unobserved goroutine panic with the same single
                    // `error[GX0005]: panic: ...` line the compiled runtime
                    // emits (`gos_rt_panic`), so stderr is identical whether the
                    // goroutine ran on the VM or native code.
                    eprintln!("{err}");
                }
            }
        });
    }

    /// Runs a builtin's share of work on a pool goroutine, handing it a
    /// dispatch over that goroutine's own `Vm`.
    pub(crate) fn spawn_native_task(&self, task: crate::value::NativeTask) {
        let origin = GoroutineOrigin {
            name: "parallel adapter".to_string(),
            file: String::new(),
            line: 0,
        };
        self.spawn_on_pool(origin, move |vm| {
            task(&mut super::native_dispatch::VmDispatch::new(vm));
        });
    }

    /// Spawns `callee(args)` on a goroutine and hands the outcome to
    /// `sink` on that goroutine.
    ///
    /// Unlike [`Self::spawn_join_native`] the caller keeps no handle, so
    /// nothing on the spawning goroutine waits: a native server places a
    /// request and takes the next one while the handler runs. The
    /// goroutine has its own `Vm` sharing the program's globals, so N
    /// handlers run on N workers.
    pub(crate) fn spawn_with_outcome_native(
        &self,
        target: crate::value::SpawnTarget,
        args: Vec<Value>,
        sink: Box<dyn FnOnce(RuntimeResult<Value>) + Send>,
    ) {
        use crate::value::SpawnTarget;
        let origin = match &target {
            SpawnTarget::Named(name) => GoroutineOrigin {
                name: name.clone(),
                file: String::new(),
                line: 0,
            },
            SpawnTarget::Callable(callee) => Self::goroutine_origin(callee),
        };
        self.spawn_on_pool(origin, move |vm| {
            let outcome = match target {
                SpawnTarget::Named(name) => match vm.lookup_global(&name) {
                    Some(global) => vm.apply(global, args),
                    None => Err(vm.unresolved(&name)),
                },
                SpawnTarget::Callable(callee) => vm.dispatch_call(&callee, args),
            };
            sink(outcome);
        });
    }

    /// Spawns `callee(args)` through the bytecode VM and returns a
    /// one-shot channel handle that `.join()` blocks on for the
    /// outcome. Backs `spawn(f)`: the outcome rides the channel as the
    /// final `Result<T, String>` variant - `Ok(value)`, or
    /// `Err(message)` carrying the bare panic text, matching the
    /// compiled tier's `gos_rt_join`.
    pub(crate) fn spawn_join_native(
        &self,
        callee: Value,
        args: Vec<Value>,
        reason: String,
    ) -> RuntimeResult<Value> {
        let channel = crate::value::Channel::unbounded();
        let worker_channel = channel.clone();
        // The cohort is read on the spawning goroutine, before the task
        // is queued, so children take their positional index in call
        // order and a cohort cannot finish joining while one of its
        // children is still on its way to a worker.
        let cohort = crate::stdlib_builtins::cohort::current_cohort();
        let index = if cohort == 0 {
            -1
        } else {
            crate::stdlib_builtins::cohort::register_child(cohort, reason)
        };
        crate::stdlib_builtins::cohort::note_child_handle(channel.identity(), cohort, index);
        let origin = Self::goroutine_origin(&callee);
        self.spawn_on_pool(origin, move |vm| {
            crate::stdlib_builtins::cohort::enter_child(cohort);
            let result = vm.as_comparator_host(|| vm.dispatch_call(&callee, args));
            // A child fails by panicking or by answering `Err`, the same
            // two ways the compiled tier reports one.
            let failure = match &result {
                Ok(Value::Variant(v)) if v.name == "Err" => Some(child_err_message(&v.fields)),
                Ok(_) => None,
                Err(RuntimeError::Panic(msg)) => Some(msg.clone()),
                Err(other) => Some(format!("{other}")),
            };
            // A panic hook observes every panic. Being joinable decides
            // whether the DEFAULT report is printed - the outcome carries
            // the message to `join()` instead - and says nothing about a
            // hook the program installed.
            if let Err(RuntimeError::Panic(msg)) = &result {
                let _ = vm.invoke_panic_hook(msg);
            }
            let outcome = match result {
                Ok(v) => Value::variant("Ok", vec![v]),
                Err(RuntimeError::Panic(msg)) => {
                    Value::variant("Err", vec![Value::String(msg.into())])
                }
                Err(other) => Value::variant("Err", vec![Value::String(format!("{other}").into())]),
            };
            // The outcome reaches the handle before the cohort is told.
            // Reporting a failure under a fail-fast policy cancels the
            // cohort, and cancelling wakes a joiner parked on this very
            // handle - so telling the cohort first would hand that joiner a
            // cancellation in place of the outcome it was waiting for. The
            // send is what the joiner is waiting on, so it can never be the
            // last thing running.
            let _delivered = worker_channel.send(outcome);
            if cohort != 0 {
                // A joiner may already have read the outcome and marked the
                // child observed; `leave_child` picks that up, so a failure
                // read through `join()` is not reported as orphaned at exit.
                crate::stdlib_builtins::cohort::leave_child(cohort, index, failure);
            }
            crate::stdlib_builtins::cohort::unwind_open_cohorts();
        });
        Ok(Value::Channel(channel))
    }

    pub(crate) fn dispatch_call(&self, callee: &Value, args: Vec<Value>) -> RuntimeResult<Value> {
        match callee {
            Value::Builtin(inner) => {
                let args = crate::value::unwrap_mut_cells_unless_writer(inner.name, args);
                (inner.call)(&args)
            }
            Value::String(name) => {
                let entry = self
                    .globals
                    .get(name.as_str())
                    .cloned()
                    .ok_or_else(|| self.unresolved(name))?;
                self.apply(entry, args)
            }
            // Closures run natively via their compiled body chunk.
            Value::Closure(closure) => self.invoke_closure(closure, args),
            // Native-dispatch hooks (`&mut self` builtins) re-enter the
            // VM's own call machinery through `VmDispatch`, so the user
            // callables they invoke run on the VM.
            Value::Native(inner) => {
                let args = crate::value::unwrap_mut_cells_unless_writer(inner.name, args);
                let mut dispatch = super::native_dispatch::VmDispatch::new(self);
                (inner.call)(&mut dispatch, &args)
            }
            // Calling a zero-field variant value acts as that variant's
            // constructor: `Circle(1.5)` produces
            // `Value::variant("Circle", [1.5])`.
            Value::Variant(inner) if inner.fields.is_empty() => Ok(Value::variant(
                inner.name.clone(),
                Arc::unwrap_or_clone(std::sync::Arc::new(args)),
            )),
            // A tuple-struct constructor is registered as an empty struct
            // sentinel; calling it fills positional fields named "0".."N-1".
            Value::Struct(inner) if inner.fields.is_empty() => {
                let fields = args
                    .into_iter()
                    .enumerate()
                    .map(|(idx, value)| (positional_field_name(idx), value))
                    .collect();
                Ok(Value::struct_with_tag(inner.name.clone(), fields))
            }
            other => Err(RuntimeError::Type(format!(
                "value of kind `{other}` is not callable"
            ))),
        }
    }

    /// Invokes a [`Value::Closure`] by running its native body chunk
    /// with `capture_values ++ args` in the leading registers. Every
    /// closure the VM builds carries a compiled chunk; a missing one is
    /// an internal lowering invariant violation.
    pub(crate) fn invoke_closure(
        &self,
        closure: &Arc<crate::value::Closure>,
        args: Vec<Value>,
    ) -> RuntimeResult<Value> {
        let chunk = &closure.chunk;
        // The chunk's leading parameters are the captured upvalues, so
        // its declared-parameter count is the arity minus the captures.
        let expected = chunk.arity as usize - closure.capture_values.len();
        if expected != args.len() {
            return Err(RuntimeError::Arity {
                expected,
                found: args.len(),
            });
        }
        // Take the argument buffer from the frame pool, as `Op::Call`
        // does, so the buffer `run` donates on frame entry is the one
        // the next callback reuses.
        let mut full = self
            .pool
            .borrow_mut()
            .take_args(closure.capture_values.len() + args.len());
        full.extend(closure.capture_values.iter().cloned());
        full.extend(args);
        self.apply(Global::Fn(Arc::clone(chunk)), full)
    }
}

/// Renders a failed cohort child's `Err` payload for the cohort's
/// report: an `errors::Error` shows its message, anything else renders
/// the way `{}` would show it.
fn child_err_message(fields: &[Value]) -> String {
    match fields.first() {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(Value::Struct(inner)) if inner.name == "errors::Error" => {
            for (name, value) in &inner.fields {
                if (*name) == "message" {
                    return match value {
                        Value::String(s) => s.as_str().to_string(),
                        other => format!("{other}"),
                    };
                }
            }
            "cohort child failed".to_string()
        }
        Some(other) => format!("{other}"),
        None => "cohort child failed".to_string(),
    }
}

/// Returns the canonical positional field name without re-interning the hot
/// small arities used by ordinary source constructors.
fn positional_field_name(index: usize) -> &'static str {
    const HOT_ARITIES: usize = 16;
    static NAMES: std::sync::LazyLock<[&'static str; HOT_ARITIES]> =
        std::sync::LazyLock::new(|| {
            std::array::from_fn(|index| crate::value::intern_type_name(&index.to_string()))
        });
    NAMES
        .get(index)
        .copied()
        .unwrap_or_else(|| crate::value::intern_type_name(&index.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_constructor_preserves_layout_with_cached_names_and_tag() {
        let vm = Vm::new();
        let sentinel = Value::struct_("Pair", Vec::new());
        let result = vm
            .dispatch_call(&sentinel, vec![Value::Int(3), Value::Int(5)])
            .expect("construct Pair");

        let (Value::Struct(sentinel), Value::Struct(result)) = (&sentinel, result) else {
            panic!("expected struct constructor result");
        };
        assert_eq!(result.name, sentinel.name);
        assert_eq!(result.fields.len(), 2);
        assert_eq!(result.fields.get(0).unwrap().0, positional_field_name(0));
        assert!(matches!(result.fields[0], Value::Int(3)));
        assert_eq!(result.fields.get(1).unwrap().0, positional_field_name(1));
        assert!(matches!(result.fields[1], Value::Int(5)));
    }
}

thread_local! {
    /// The VM whose frames a compiled-code fault reports. Null outside a run.
    static ACTIVE_VM: std::cell::Cell<*const Vm> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// Registers [`render_active_vm_trace`] with the runtime once per process.
fn install_trace_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: the hook has the C-ABI signature the runtime declares for
        // `TraceHookFn` and answers an owned runtime string the runtime frees.
        unsafe {
            gossamer_runtime::c_abi::install_trace_hook(render_active_vm_trace);
        }
    });
}

/// The active VM's call stack in the same shape `gos` prints for an
/// interpreted fault, as an owned runtime string. Null when no VM is running.
extern "C" fn render_active_vm_trace() -> *mut std::ffi::c_char {
    let ptr = ACTIVE_VM.with(std::cell::Cell::get);
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the slot holds a borrow of a VM live for the whole of
    // `with_active_trace`, which is the only window this hook runs in.
    let vm = unsafe { &*ptr };
    let mut frames = vm.call_stack_frames();
    // A JIT-compiled body runs on the machine stack beneath the bytecode frame
    // that dispatched into it, so the frames found there continue the chain;
    // the dispatching frame itself, which never ran bytecode, is the first of
    // them.
    let mut native: Vec<CallStackFrame> = jit_backend::active_jit_frames()
        .iter()
        .rev()
        .map(|frame| vm.jit_call_stack_frame(frame))
        .collect();
    if let (Some(last), Some(first)) = (frames.last(), native.first())
        && last.function == first.function
        && last.line.is_none()
    {
        frames.pop();
    }
    frames.append(&mut native);
    if frames.is_empty() {
        return std::ptr::null_mut();
    }
    let mut out = String::from("  call stack (outermost first):\n");
    let mut index = 0usize;
    while index < frames.len() {
        let frame = &frames[index];
        let mut repeats = 1usize;
        while index + repeats < frames.len() && frames[index + repeats] == *frame {
            repeats += 1;
        }
        out.push_str("    at ");
        out.push_str(&frame.function);
        if let (Some(file), Some(line)) = (&frame.file, frame.line) {
            match frame.column {
                Some(column) => out.push_str(&format!(" ({file}:{line}:{column})")),
                None => out.push_str(&format!(" ({file}:{line})")),
            }
        }
        if repeats > 1 {
            out.push_str(&format!(" x{repeats}"));
        }
        out.push('\n');
        index += repeats;
    }
    gossamer_runtime::c_abi::string::alloc_cstring(out.as_bytes())
}
