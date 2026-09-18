use super::*;
use crate::value::NativeDispatch;

/// VM-backed [`NativeDispatch`] adapter. Higher-order stdlib builtins
/// (`iter::map`, `sort_by`, `http::serve`, `result::map`, …) receive a
/// `&mut dyn NativeDispatch` and invoke the user callables passed to
/// them through it. This adapter drives the bytecode VM's own call
/// machinery, so those callbacks execute on the VM.
///
/// The trait's methods take `&mut self`, but the adapter only holds a
/// shared `&Vm`: the VM's call path is `&self` over `RefCell`/`Arc`
/// interior state, so re-entering it from a builtin callback needs no
/// exclusive borrow.
pub(crate) struct VmDispatch<'a> {
    vm: &'a Vm,
}

impl<'a> VmDispatch<'a> {
    /// Wraps a borrowed VM so it can back the `NativeDispatch` trait.
    pub(crate) fn new(vm: &'a Vm) -> Self {
        Self { vm }
    }
}

impl NativeDispatch for VmDispatch<'_> {
    fn call_fn(&mut self, name: &str, args: Vec<Value>) -> RuntimeResult<Value> {
        // Resolve and invoke without clearing the call stack: this
        // callback runs inside an in-flight VM frame, so the chain
        // above it must stay intact for diagnostics. (`Vm::call`
        // clears the stack because it is the top-level entry point.)
        let callee = self
            .vm
            .lookup_global(name)
            .ok_or_else(|| self.vm.unresolved(name))?;
        self.vm.apply(callee, args)
    }

    fn has_fn(&self, name: &str) -> bool {
        self.vm.lookup_global(name).is_some()
    }

    fn call_value(&mut self, callee: &Value, args: Vec<Value>) -> RuntimeResult<Value> {
        self.vm.dispatch_call(callee, args)
    }

    fn spawn_callable(&mut self, callable: Value, args: Vec<Value>) -> RuntimeResult<()> {
        self.vm.spawn_goroutine_native(callable, args);
        Ok(())
    }

    fn spawn_join(&mut self, callable: Value, args: Vec<Value>) -> RuntimeResult<Value> {
        self.vm.spawn_join_native(callable, args, String::new())
    }

    fn spawn_join_labelled(
        &mut self,
        callable: Value,
        args: Vec<Value>,
        reason: String,
    ) -> RuntimeResult<Value> {
        self.vm.spawn_join_native(callable, args, reason)
    }

    fn spawn_with_outcome(
        &mut self,
        target: crate::value::SpawnTarget,
        args: Vec<Value>,
        sink: Box<dyn FnOnce(RuntimeResult<Value>) + Send>,
    ) {
        self.vm.spawn_with_outcome_native(target, args, sink);
    }

    fn spawn_task(&mut self, task: crate::value::NativeTask) -> bool {
        self.vm.spawn_native_task(task);
        true
    }
}
