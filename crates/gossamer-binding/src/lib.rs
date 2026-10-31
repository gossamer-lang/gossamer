//! Stable Rust-binding system for Gossamer libraries.
//!
//! A Rust crate that wants to expose Gossamer-callable functions
//! depends on this crate and declares its module(s) with
//! [`register_module!`] or [`gos_module`]; each lands its [`Module`] in
//! the link-time [`REGISTRY`] via `linkme`.
//!
//! The crate depends on no part of the Gossamer toolchain. `gos run`
//! loads a binding as a shared library and calls it through the C ABI in
//! `gossamer-binding-abi`: `gos_binding_init` takes the host's runtime
//! table and answers the module table, whose items the VM calls through
//! their VM thunks. Compiled programs link the binding statically and call
//! its typed wire thunks (`gos_binding_<module>__<item>`) directly.

// The C entry points, the host table, and the wire thunks need `unsafe`;
// each module that holds them opts in, and every other module stays safe.
#![deny(unsafe_code)]

mod abi;
pub mod blocking_pool;
pub mod conv;
pub mod error;
mod heap;
mod host;
mod macros;
pub mod native;
pub mod opaque;
pub mod registry;
mod sig;
pub mod struct_helpers;
mod table;
pub mod testing;
pub mod types;
pub mod value;
pub mod wire_json;

pub use crate::abi::VmDispatch;
pub use crate::conv::{BindingCallback, Bytes, DynValue, FromGos, PersistentCallback, ToGos};
pub use crate::error::GosError;
pub use crate::heap::RuntimeAllocator;
pub use crate::host::mark_thread_idle;
pub use crate::opaque::Registry;
pub use crate::registry::{ItemFn, Module, REGISTRY, Signature, TextConversion, WireText};
pub use crate::sig::SigType;
pub use crate::table::{gos_binding_abi_version, gos_binding_init, signature_json, type_json};
pub use crate::types::{Type, VariantArm};
pub use crate::value::{
    CallbackOrigin, CallbackRef, NativeDispatch, RuntimeError, RuntimeResult, Str, Value,
};
// The proc macros live in `gossamer-binding-macros`, as Rust requires of a
// `proc-macro = true` crate; binding authors depend on this crate alone.
pub use gossamer_binding_macros::{GosStruct, gos_blocking, gos_module, gos_opaque};

/// Major and minor version of the binding ABI this crate speaks, as
/// `gossamer_binding_abi::ABI_VERSION` encodes them.
pub const ABI_VERSION: (u16, u16) = (
    gossamer_binding_abi::abi_major(gossamer_binding_abi::ABI_VERSION) as u16,
    gossamer_binding_abi::abi_minor(gossamer_binding_abi::ABI_VERSION) as u16,
);

/// Renders the C-ABI export symbol for a binding item.
///
/// Mirrors what the `register_module!` macro emits via the
/// `symbol_prefix:` parameter - `path::to::module` segments get
/// joined with `__`, and the item is appended after a final
/// `__`. Both the codegen and the macro use this scheme so the
/// codegen-emitted call resolves to the macro-emitted thunk at
/// link time.
///
/// Example:
/// `mangle_binding_symbol("tuigoose::layout", "rect")` →
/// `"gos_binding_tuigoose__layout__rect"`.
#[must_use]
pub fn mangle_binding_symbol(module_path: &str, item_name: &str) -> String {
    let mangled_path = module_path.replace("::", "__");
    format!("gos_binding_{mangled_path}__{item_name}")
}

#[doc(hidden)]
pub use crate::abi::vm_call as __vm_call;
#[doc(hidden)]
pub use crate::abi::{
    entry_to_value as __entry_to_value, json_mask as __json_mask,
    value_to_entry as __value_to_entry, vm_arg as __vm_arg, vm_ret as __vm_ret,
};
#[doc(hidden)]
pub use gossamer_binding_abi as __abi;
#[doc(hidden)]
pub use linkme;
#[doc(hidden)]
pub use parking_lot;
#[doc(hidden)]
pub use pastey as __paste;

/// Returns every module registered via [`register_module!`].
///
/// The slice is populated at link time by `linkme`; it is empty
/// only when no binding crate is in the link graph.
#[must_use]
pub fn modules() -> Vec<&'static Module> {
    REGISTRY.iter().copied().collect()
}

/// Looks up a module by its declared path.
#[must_use]
pub fn module(path: &str) -> Option<&'static Module> {
    REGISTRY.iter().find(|m| m.path == path).copied()
}

/// Resolves an item by `module::name`.
#[must_use]
pub fn item(qualified: &str) -> Option<(&'static Module, &'static ItemFn)> {
    let (mod_path, item_name) = qualified.rsplit_once("::")?;
    let module = module(mod_path)?;
    module
        .items
        .iter()
        .find(|i| i.name == item_name)
        .map(|i| (module, i))
}
