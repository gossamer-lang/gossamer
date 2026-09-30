//! Stable Rust-binding system for Gossamer libraries.
//!
//! A Rust crate that wants to expose Gossamer-callable functions
//! depends on this crate, declares its module(s) with the
//! [`register_module!`] macro, and lands its `Module` in the
//! global [`REGISTRY`] via `linkme`. The Gossamer toolchain
//! statically links the binding and its registry entries become
//! visible to `use` and to the runtime dispatcher.
//!
//! See `~/dev/contexts/lang/ffi.md` for the full design.

// `gossamer-binding` is the only workspace crate that needs
// `unsafe`: the compiled-mode export ABI in `native` materialises
// `*const c_char`, `*mut GosVec`, etc. from raw pointers handed
// in by the codegen. The unsafe is contained inside `native`;
// every other module keeps the workspace `forbid` posture by
// staying pure-safe.
#![deny(unsafe_code)]

pub mod blocking_pool;
pub mod conv;
pub mod error;
mod macros;
pub mod native;
pub mod opaque;
pub mod registry;
mod sig;
pub mod struct_helpers;
pub mod types;

pub use crate::conv::{BindingCallback, Bytes, DynValue, FromGos, PersistentCallback, ToGos};
pub use crate::error::GosError;

// Re-export the attribute / derive proc-macros so binding authors
// only have to add `gossamer-binding` as a dependency. The proc
// macros live in a separate crate (`gossamer-binding-macros`)
// because Rust requires `proc-macro = true` crates to be
// declared standalone.
pub use crate::opaque::Registry;
pub use crate::registry::{ItemFn, Module, NativeCall, REGISTRY, Signature};
pub use crate::sig::SigType;
pub use crate::types::{Type, VariantArm};
pub use gossamer_binding_macros::{GosStruct, gos_blocking, gos_module, gos_opaque};

/// Major.minor version of the binding wire layout.
///
/// Bumped whenever a cross-FFI layout, calling convention, or symbol prefix
/// changes. A binding is always compiled against the toolchain building the
/// program (the runner patches `gossamer-binding` to the toolchain's copy),
/// so the layout is internal: the version names it for diagnostics and for
/// `ABI_0_4.md`, and no binary built by one toolchain is linked by another.
///
/// ABI v2.0 freezes the legacy wire prefix and requires runtime-created
/// ownership carriers for allocation-backed values such as `GosVec`.
/// `ABI_0_4.md`: `GosVec`, `GosVariant`, `GosVariantValue`,
/// `GosTuple`, `GosBytes`, `BindingGosMap`, `GosDynVariant`,
/// `GosCallback`, `GosStruct`, plus the `gos_binding_<...>` symbol
/// scheme. Minor bumps within v1 add new wire shapes or new
/// `BindingAbi` impls; they do NOT reorder existing fields. Major
/// bumps (v2) break compatibility.
pub const ABI_VERSION: (u8, u8) = (2, 0);

/// The wire-layout version this binding was compiled with, exported so a
/// linked binary records which layout its bindings speak.
///
/// `unsafe_code` is permitted on this single static because the
/// `no_mangle` export is the entire mechanism - without it, the
/// runtime has nothing to dlsym for at link time.
#[allow(unsafe_code, reason = "no_mangle is the load-time ABI-version anchor")]
#[unsafe(no_mangle)]
#[used]
pub static __gos_binding_abi_version: [u8; 2] = [ABI_VERSION.0, ABI_VERSION.1];

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

pub use gossamer_interp::value::{NativeDispatch, RuntimeError, RuntimeResult, Value};

#[doc(hidden)]
pub use gossamer_interp::value;

#[doc(hidden)]
pub use linkme;
#[doc(hidden)]
pub use parking_lot;
#[doc(hidden)]
pub use pastey as __paste;

/// Internal: registers a binding's `gos_binding_<...>` C-ABI thunk
/// address with the cranelift JIT's native-symbol table.
///
/// Called from the `force_link()` shim emitted by
/// [`register_module!`] so JIT-compiled bodies can resolve calls
/// into bindings without relying on the dynamic symbol table.
///
/// `addr` must point at the matching `extern "C"` thunk; the macro
/// stamps out `[< gos_binding_ $sym __ $name >] as *const u8` to
/// satisfy this contract. Bindings should not call this directly.
#[doc(hidden)]
pub fn __register_native_symbol(name: &'static str, addr: *const u8) {
    gossamer_codegen_cranelift::register_native_symbol(name, addr);
}

/// Re-export of the cranelift-side link-time symbol registry. The
/// `register_module!` macro lands one [`NativeSymbolEntry`] per
/// binding item into this slice via `linkme::distributed_slice`,
/// so the JIT can resolve calls into bindings without any runtime
/// registration call.
#[doc(hidden)]
pub use gossamer_codegen_cranelift::{NATIVE_SYMBOLS, NativeSymbolEntry};

/// Link-time slice of every registered module's `force_link` fn.
///
/// The `register_module!` macro publishes one entry per call;
/// [`run_all_force_links`] walks the slice and invokes each. This
/// replaces the per-crate `__bindings_force_link()` shim - the
/// runner template just calls `run_all_force_links()` once and
/// every linked binding's `linkme` registry entries become
/// reachable. Binding crates with multiple modules contribute one
/// entry per module, all discovered automatically.
#[linkme::distributed_slice]
pub static FORCE_LINK_FNS: [fn()] = [..];

/// Invokes every `force_link()` entry registered via
/// [`FORCE_LINK_FNS`]. Call this once at runner startup, before
/// [`install_all`]. Idempotent.
pub fn run_all_force_links() {
    for f in FORCE_LINK_FNS.iter() {
        f();
    }
}

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

/// Compiled-mode counterpart to [`install_all`].
///
/// Compiled binaries call binding items directly through the C-ABI
/// thunks emitted by `register_module!` - they don't go through
/// the interpreter's external-natives table or the resolver. This
/// function exists so the compiled-mode entry point has a single,
/// stable symbol to call. It's deliberately a no-op aside from
/// touching every `Module` to keep the `linkme` distributed-slice
/// entries alive across LTO.
pub fn install_all_for_compiled() {
    for module in REGISTRY.iter().copied() {
        let _ = module.path;
    }
}

/// Installs every registered binding into the interpreter's
/// external-natives table.
///
/// Each item is registered under its fully-qualified
/// `module::item` spelling. Call this exactly once at runtime
/// startup, before constructing the first VM.
///
/// Returns the number of items installed (sum of `module.items.len()`
/// across [`REGISTRY`]).
///
/// Side effects:
/// - registers each item with the interpreter as a `Value::Native`
///   global under its fully-qualified `module::item` spelling.
/// - mirrors the registry into `gossamer_resolve::external` so the
///   resolver / type checker / `gos doc` can see binding metadata
///   without depending on this crate.
#[must_use]
pub fn install_all() -> usize {
    let mut count = 0;
    let mut leaf_groups: rustc_hash::FxHashMap<&'static str, Vec<&'static ItemFn>> =
        rustc_hash::FxHashMap::default();
    for module in REGISTRY.iter().copied() {
        for item in module.items {
            leaf_groups.entry(item.name).or_default().push(item);
        }
    }
    for module in REGISTRY.iter().copied() {
        for item in module.items {
            let qualified: &'static str =
                Box::leak(format!("{}::{}", module.path, item.name).into_boxed_str());
            gossamer_interp::register_external_native(qualified, item.call);
            count += 1;
        }
    }
    for (leaf, group) in &leaf_groups {
        if group.len() == 1 {
            // Unambiguous leaf - install the direct thunk.
            gossamer_interp::register_external_native(leaf, group[0].call);
        } else {
            // Ambiguous leaf - install an arity-aware dispatcher
            // that picks a candidate matching the call's argc.
            // Falls back to the first candidate when no arity
            // matches, so the binding's own arity check produces
            // the standard error message.
            let dispatcher = assign_ambig_dispatcher(group.clone());
            gossamer_interp::register_external_native(leaf, dispatcher);
        }
    }
    populate_resolve_table();
    count
}

fn populate_resolve_table() {
    let modules = REGISTRY
        .iter()
        .copied()
        .map(|m| {
            let items = m
                .items
                .iter()
                .map(|item| gossamer_resolve::ExternalItem {
                    name: item.name.to_string(),
                    params: item.signature.params.iter().map(lower_type).collect(),
                    ret: lower_type(&item.signature.ret),
                    doc: item.doc.to_string(),
                })
                .collect();
            gossamer_resolve::ExternalModule {
                path: m.path.to_string(),
                doc: m.doc.to_string(),
                items,
            }
        })
        .collect();
    gossamer_resolve::set_external_modules(modules);
}

/// Capacity of the ambiguous-leaf dispatcher pool. Each ambiguous
/// leaf consumes one slot; collisions across more than this many
/// distinct leaves panic. The number is generous given typical
/// binding-crate sizes (tuigoose has ~50 items across 7 modules).
/// Maximum number of ambiguous-leaf dispatch groups.
///
/// The dispatch table at the bottom of this file is a fixed-size
/// array of distinct `extern fn` pointers (each is a separate
/// monomorphic instantiation of `ambig_call::<N>`), so growing
/// the pool at runtime would require regenerating the table -
/// which is a build-time concern, not a runtime one. 0.6.0
/// bumped the cap from 64 to 256 (and improved the
/// exhaustion diagnostic) so practical binding-crate sizes never
/// hit the limit. Linking a binding crate that exposes more than
/// 256 ambiguous-leaf groups produces a clear panic at startup.
const AMBIG_POOL_SIZE: usize = 256;

type AmbigGroup = Vec<&'static ItemFn>;

static AMBIG_SLOTS: parking_lot::RwLock<Vec<Option<AmbigGroup>>> =
    parking_lot::RwLock::new(Vec::new());

fn ambig_slots() -> parking_lot::RwLockReadGuard<'static, Vec<Option<AmbigGroup>>> {
    AMBIG_SLOTS.read()
}

fn ambig_slots_mut() -> parking_lot::RwLockWriteGuard<'static, Vec<Option<AmbigGroup>>> {
    let mut guard = AMBIG_SLOTS.write();
    if guard.len() < AMBIG_POOL_SIZE {
        guard.resize(AMBIG_POOL_SIZE, None);
    }
    guard
}

fn assign_ambig_dispatcher(group: AmbigGroup) -> gossamer_interp::value::NativeCall {
    let idx = {
        let mut guard = ambig_slots_mut();
        let mut chosen: Option<usize> = None;
        for (i, slot) in guard.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(group);
                chosen = Some(i);
                break;
            }
        }
        chosen.unwrap_or_else(|| {
            panic!(
                "gossamer-binding: ambiguous-leaf dispatch pool exhausted ({AMBIG_POOL_SIZE} slots in use). \
                 More than {AMBIG_POOL_SIZE} distinct ambiguous leaf names are registered \
                 across the linked binding crates. Raise `AMBIG_POOL_SIZE` in \
                 `gossamer-binding/src/lib.rs` and regenerate `AMBIG_DISPATCH_TABLE` \
                 - the table must remain a fixed-size const array so each entry is \
                 a distinct `extern fn` pointer."
            )
        })
    };
    AMBIG_DISPATCH_TABLE[idx]
}

fn ambig_call<const N: usize>(
    dispatch: &mut dyn gossamer_interp::value::NativeDispatch,
    args: &[gossamer_interp::value::Value],
) -> gossamer_interp::value::RuntimeResult<gossamer_interp::value::Value> {
    let Some(group) = ambig_slots().get(N).cloned().flatten() else {
        return Err(gossamer_interp::value::RuntimeError::Arity {
            expected: 0,
            found: args.len(),
        });
    };
    for item in &group {
        if item.signature.params.len() == args.len() {
            return (item.call)(dispatch, args);
        }
    }
    let first = group
        .first()
        .copied()
        .expect("ambig group must be non-empty");
    (first.call)(dispatch, args)
}

const AMBIG_DISPATCH_TABLE: [gossamer_interp::value::NativeCall; AMBIG_POOL_SIZE] = [
    ambig_call::<0>,
    ambig_call::<1>,
    ambig_call::<2>,
    ambig_call::<3>,
    ambig_call::<4>,
    ambig_call::<5>,
    ambig_call::<6>,
    ambig_call::<7>,
    ambig_call::<8>,
    ambig_call::<9>,
    ambig_call::<10>,
    ambig_call::<11>,
    ambig_call::<12>,
    ambig_call::<13>,
    ambig_call::<14>,
    ambig_call::<15>,
    ambig_call::<16>,
    ambig_call::<17>,
    ambig_call::<18>,
    ambig_call::<19>,
    ambig_call::<20>,
    ambig_call::<21>,
    ambig_call::<22>,
    ambig_call::<23>,
    ambig_call::<24>,
    ambig_call::<25>,
    ambig_call::<26>,
    ambig_call::<27>,
    ambig_call::<28>,
    ambig_call::<29>,
    ambig_call::<30>,
    ambig_call::<31>,
    ambig_call::<32>,
    ambig_call::<33>,
    ambig_call::<34>,
    ambig_call::<35>,
    ambig_call::<36>,
    ambig_call::<37>,
    ambig_call::<38>,
    ambig_call::<39>,
    ambig_call::<40>,
    ambig_call::<41>,
    ambig_call::<42>,
    ambig_call::<43>,
    ambig_call::<44>,
    ambig_call::<45>,
    ambig_call::<46>,
    ambig_call::<47>,
    ambig_call::<48>,
    ambig_call::<49>,
    ambig_call::<50>,
    ambig_call::<51>,
    ambig_call::<52>,
    ambig_call::<53>,
    ambig_call::<54>,
    ambig_call::<55>,
    ambig_call::<56>,
    ambig_call::<57>,
    ambig_call::<58>,
    ambig_call::<59>,
    ambig_call::<60>,
    ambig_call::<61>,
    ambig_call::<62>,
    ambig_call::<63>,
    ambig_call::<64>,
    ambig_call::<65>,
    ambig_call::<66>,
    ambig_call::<67>,
    ambig_call::<68>,
    ambig_call::<69>,
    ambig_call::<70>,
    ambig_call::<71>,
    ambig_call::<72>,
    ambig_call::<73>,
    ambig_call::<74>,
    ambig_call::<75>,
    ambig_call::<76>,
    ambig_call::<77>,
    ambig_call::<78>,
    ambig_call::<79>,
    ambig_call::<80>,
    ambig_call::<81>,
    ambig_call::<82>,
    ambig_call::<83>,
    ambig_call::<84>,
    ambig_call::<85>,
    ambig_call::<86>,
    ambig_call::<87>,
    ambig_call::<88>,
    ambig_call::<89>,
    ambig_call::<90>,
    ambig_call::<91>,
    ambig_call::<92>,
    ambig_call::<93>,
    ambig_call::<94>,
    ambig_call::<95>,
    ambig_call::<96>,
    ambig_call::<97>,
    ambig_call::<98>,
    ambig_call::<99>,
    ambig_call::<100>,
    ambig_call::<101>,
    ambig_call::<102>,
    ambig_call::<103>,
    ambig_call::<104>,
    ambig_call::<105>,
    ambig_call::<106>,
    ambig_call::<107>,
    ambig_call::<108>,
    ambig_call::<109>,
    ambig_call::<110>,
    ambig_call::<111>,
    ambig_call::<112>,
    ambig_call::<113>,
    ambig_call::<114>,
    ambig_call::<115>,
    ambig_call::<116>,
    ambig_call::<117>,
    ambig_call::<118>,
    ambig_call::<119>,
    ambig_call::<120>,
    ambig_call::<121>,
    ambig_call::<122>,
    ambig_call::<123>,
    ambig_call::<124>,
    ambig_call::<125>,
    ambig_call::<126>,
    ambig_call::<127>,
    ambig_call::<128>,
    ambig_call::<129>,
    ambig_call::<130>,
    ambig_call::<131>,
    ambig_call::<132>,
    ambig_call::<133>,
    ambig_call::<134>,
    ambig_call::<135>,
    ambig_call::<136>,
    ambig_call::<137>,
    ambig_call::<138>,
    ambig_call::<139>,
    ambig_call::<140>,
    ambig_call::<141>,
    ambig_call::<142>,
    ambig_call::<143>,
    ambig_call::<144>,
    ambig_call::<145>,
    ambig_call::<146>,
    ambig_call::<147>,
    ambig_call::<148>,
    ambig_call::<149>,
    ambig_call::<150>,
    ambig_call::<151>,
    ambig_call::<152>,
    ambig_call::<153>,
    ambig_call::<154>,
    ambig_call::<155>,
    ambig_call::<156>,
    ambig_call::<157>,
    ambig_call::<158>,
    ambig_call::<159>,
    ambig_call::<160>,
    ambig_call::<161>,
    ambig_call::<162>,
    ambig_call::<163>,
    ambig_call::<164>,
    ambig_call::<165>,
    ambig_call::<166>,
    ambig_call::<167>,
    ambig_call::<168>,
    ambig_call::<169>,
    ambig_call::<170>,
    ambig_call::<171>,
    ambig_call::<172>,
    ambig_call::<173>,
    ambig_call::<174>,
    ambig_call::<175>,
    ambig_call::<176>,
    ambig_call::<177>,
    ambig_call::<178>,
    ambig_call::<179>,
    ambig_call::<180>,
    ambig_call::<181>,
    ambig_call::<182>,
    ambig_call::<183>,
    ambig_call::<184>,
    ambig_call::<185>,
    ambig_call::<186>,
    ambig_call::<187>,
    ambig_call::<188>,
    ambig_call::<189>,
    ambig_call::<190>,
    ambig_call::<191>,
    ambig_call::<192>,
    ambig_call::<193>,
    ambig_call::<194>,
    ambig_call::<195>,
    ambig_call::<196>,
    ambig_call::<197>,
    ambig_call::<198>,
    ambig_call::<199>,
    ambig_call::<200>,
    ambig_call::<201>,
    ambig_call::<202>,
    ambig_call::<203>,
    ambig_call::<204>,
    ambig_call::<205>,
    ambig_call::<206>,
    ambig_call::<207>,
    ambig_call::<208>,
    ambig_call::<209>,
    ambig_call::<210>,
    ambig_call::<211>,
    ambig_call::<212>,
    ambig_call::<213>,
    ambig_call::<214>,
    ambig_call::<215>,
    ambig_call::<216>,
    ambig_call::<217>,
    ambig_call::<218>,
    ambig_call::<219>,
    ambig_call::<220>,
    ambig_call::<221>,
    ambig_call::<222>,
    ambig_call::<223>,
    ambig_call::<224>,
    ambig_call::<225>,
    ambig_call::<226>,
    ambig_call::<227>,
    ambig_call::<228>,
    ambig_call::<229>,
    ambig_call::<230>,
    ambig_call::<231>,
    ambig_call::<232>,
    ambig_call::<233>,
    ambig_call::<234>,
    ambig_call::<235>,
    ambig_call::<236>,
    ambig_call::<237>,
    ambig_call::<238>,
    ambig_call::<239>,
    ambig_call::<240>,
    ambig_call::<241>,
    ambig_call::<242>,
    ambig_call::<243>,
    ambig_call::<244>,
    ambig_call::<245>,
    ambig_call::<246>,
    ambig_call::<247>,
    ambig_call::<248>,
    ambig_call::<249>,
    ambig_call::<250>,
    ambig_call::<251>,
    ambig_call::<252>,
    ambig_call::<253>,
    ambig_call::<254>,
    ambig_call::<255>,
];

fn lower_type(t: &crate::types::Type) -> gossamer_resolve::BindingType {
    use crate::types::Type;
    use gossamer_resolve::{BindingType as R, BindingVariantArm};
    match t {
        Type::Unit => R::Unit,
        Type::Bool => R::Bool,
        Type::I64 => R::I64,
        Type::F64 => R::F64,
        Type::Char => R::Char,
        Type::String => R::String,
        Type::Bytes => R::Bytes,
        Type::Tuple(ts) => R::Tuple(ts.iter().map(lower_type).collect()),
        Type::Vec(inner) => R::Vec(Box::new(lower_type(inner))),
        Type::Option(inner) => R::Option(Box::new(lower_type(inner))),
        Type::Result(ok, err) => R::Result(Box::new(lower_type(ok)), Box::new(lower_type(err))),
        Type::Map(k, v) => R::Map(Box::new(lower_type(k)), Box::new(lower_type(v))),
        Type::Variant(arms) => R::Variant(
            arms.iter()
                .map(|a| BindingVariantArm {
                    name: a.name.to_string(),
                    payload: a.payload.iter().map(lower_type).collect(),
                })
                .collect(),
        ),
        Type::Callback(args, ret) => R::Callback(
            args.iter().map(lower_type).collect(),
            Box::new(lower_type(ret)),
        ),
        Type::Opaque(name) => R::Opaque((*name).to_string()),
        Type::Any => R::Any,
    }
}
