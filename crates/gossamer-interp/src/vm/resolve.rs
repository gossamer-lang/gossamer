#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl Vm {
    pub(crate) fn resolve_global(&self, name: &str) -> RuntimeResult<Value> {
        let entry = self
            .globals
            .get(name)
            .ok_or_else(|| self.unresolved(name))?;
        match entry {
            Global::Value(value) => Ok(value.clone()),
            Global::MutStatic(cell) => Ok(cell.get()),
            Global::Fn(_) => {
                // Bytecode chunk (possibly with a deferred JIT
                // override) - surface as the function's name
                // string so `dispatch_call` looks the entry up
                // again and routes through `apply`, which checks
                // the JIT override map before falling back to
                // bytecode.
                Ok(Value::String(SmolStr::from(name.to_string())))
            }
        }
    }

    // Cognitive-complexity is intentionally high: this is the
    // single dispatch loop covering every `Op` variant (~80
    // arms today). Splitting into per-op handler fns is the
    // Tier-A3 work in `interp_wow_plan.md` and will land
    // separately. The `items_after_statements` allow covers
    // per-arm `type` and `const` definitions (e.g. `BuiltinFn`
    // in `Op::MethodCall`); hoisting them out of their match

    /// Returns the per-`Vm` [`ChunkState`] for `chunk`, allocating
    /// it on first lookup. The returned reference is tied to
    /// `&self` and stays valid for the lifetime of the `Vm`.
    ///
    /// SAFETY (the localized `unsafe { &*ptr }` below): the arena
    /// is append-only - entries are inserted on first encounter
    /// and never removed, so the `Box<ChunkState>` stays at a
    /// stable heap address. The `Vec<Box<...>>` may reallocate
    /// when growing, but only the `Box` slots in the spine move;
    /// the heap-allocated `ChunkState` each `Box` points to does
    /// not. `&self` outlives every reference we hand out, so the
    /// `'static` cast in the side-index map is collapsed back to
    /// `&'a self::ChunkState` at the call site. Single-thread
    /// access (each `Vm` is owned by one goroutine) means no
    /// cross-thread aliasing concerns.
    pub(crate) fn chunk_state_for(&self, chunk: &Arc<FnChunk>) -> &ChunkState {
        let key = Arc::as_ptr(chunk) as usize;
        // Single-slot cache: mutual recursion keeps hitting the
        // same chunk on many adjacent calls, saving ~10 ns of
        // hash + comparison per `apply` entry.
        if let Some((last_key, last_state)) = self.chunk_state_last.get() {
            if last_key == key {
                return last_state;
            }
        }
        // Fast path: shared borrow of the side index.
        if let Some(state) = self.chunk_state_map.borrow().get(&key).copied() {
            self.chunk_state_last.set(Some((key, state)));
            return state;
        }
        // Miss: allocate a fresh `ChunkState`, pin it in the
        // arena, register the side-index reference. `jit_disabled`
        // is read once per chunk and frozen - the JIT can't be
        // toggled mid-run.
        let jit_disabled = !jit_call::jit_enabled();
        let eager = self.jit_eager_names.borrow().contains(chunk.name);
        let state_box = Box::new(ChunkState::new(
            chunk.call_cache_count,
            chunk.arith_cache_count,
            chunk.field_cache_count,
            chunk.instrs.len(),
            jit_disabled,
            eager,
        ));
        let mut arena = self.chunk_state_arena.borrow_mut();
        arena.push(state_box);
        // SAFETY: see the doc-comment above. Arena is append-only,
        // boxed entries are heap-pinned, single-thread access.
        let state_ref: &'static ChunkState =
            unsafe { &*std::ptr::from_ref(arena.last().unwrap().as_ref()) };
        drop(arena);
        self.chunk_state_map.borrow_mut().insert(key, state_ref);
        self.chunk_state_last.set(Some((key, state_ref)));
        state_ref
    }
}
