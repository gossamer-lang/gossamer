//! Process-wide registry of binding-installed native functions.
//!
//! `gossamer-binding-host::install` populates this table once at
//! startup; [`crate::Vm::new`] merges the snapshot into its
//! per-instance globals so qualified binding paths (e.g.
//! `tuigoose::layout::rect`) resolve through the same `Value::Native`
//! lookup the VM uses for built-in stdlib entries.

use parking_lot::Mutex;

use crate::value::{NativeCall, Value};

static EXTERNAL_NATIVES: Mutex<Vec<(&'static str, Value)>> = Mutex::new(Vec::new());

/// Registers a binding-supplied native function under `name`.
///
/// Both segments of the name are stored: the binding crate
/// pre-leaks the qualified spelling so this entry point never
/// touches lifetime concerns.
pub fn register_external_native(name: &'static str, call: NativeCall) {
    register_external_value(name, Value::native(name, call));
}

/// Registers `value` as the global `name` of every VM built afterwards.
pub fn register_external_value(name: &'static str, value: Value) {
    EXTERNAL_NATIVES.lock().push((name, value));
}

/// Returns a clone of every (name, native-value) pair currently
/// registered. Each `Value::Native` payload is `Arc`-backed, so
/// cloning is a refcount bump.
#[must_use]
pub fn external_natives_snapshot() -> Vec<(&'static str, Value)> {
    EXTERNAL_NATIVES.lock().clone()
}

/// Clears the registry. Used by tests that need to assert from
/// a fresh state.
pub fn clear_external_natives_for_test() {
    EXTERNAL_NATIVES.lock().clear();
}
