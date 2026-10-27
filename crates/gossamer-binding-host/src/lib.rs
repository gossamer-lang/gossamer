//! Loads Rust binding libraries into `gos` and calls them from the VM and
//! the JIT.
//!
//! A binding library is a shared library built from a binding crate. `gos`
//! reads the library's `signatures.json` to type-check and to register a VM
//! native per item, and loads the library itself on the first call: the VM
//! calls an item through its VM thunk, and JIT-compiled code calls its wire
//! thunk directly, resolved when a body calling it is compiled. A library
//! is checked against the ABI version `gos` speaks and against the hash of
//! its cached signatures before any of its items runs.

#![deny(unsafe_code)]

mod dl;
mod library;
mod marshal;
mod signatures;
mod vm;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

pub use crate::signatures::{ArmSignature, ItemSignature, ModuleSignature, SigType, Signatures};

/// Why a binding library could not be used.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The library would not open, or is not a binding library.
    #[error("cannot load the binding library at {path}: {reason}")]
    Load {
        /// The library.
        path: PathBuf,
        /// What the loader or the library said.
        reason: String,
    },
    /// The library speaks another version of the binding ABI.
    #[error(
        "the binding library at {path} was built for binding ABI {}.{}, and this gos speaks {}.{}; rebuild the binding against a gossamer-binding of the same major version",
        gossamer_binding_abi::abi_major(*library),
        gossamer_binding_abi::abi_minor(*library),
        gossamer_binding_abi::abi_major(*host),
        gossamer_binding_abi::abi_minor(*host)
    )]
    Abi {
        /// The library.
        path: PathBuf,
        /// The version it was built for.
        library: u32,
        /// The version this host speaks.
        host: u32,
    },
    /// The library's signatures differ from the ones cached beside it.
    #[error(
        "the binding library at {path} changed since its signatures were recorded; rebuild it so they match"
    )]
    SignatureMismatch {
        /// The library.
        path: PathBuf,
    },
    /// `signatures.json` could not be read or written.
    #[error("the binding signatures at {path}: {reason}")]
    Signatures {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
}

/// Loads the library at `path` into this process and answers its
/// signatures, for a build to cache beside it.
pub fn read_library_signatures(path: &Path) -> Result<Signatures, HostError> {
    library::load(path, &vm::HOST_TABLE).map(|loaded| loaded.signatures)
}

/// The libraries [`install`] registered, with their wire symbols.
static LIBRARIES: RwLock<Vec<(Arc<library::LazyLibrary>, Vec<String>)>> = RwLock::new(Vec::new());

/// Registers `libraries` for this process: their modules become visible to
/// the resolver and the type checker, each item becomes a native of every
/// VM built afterwards, and the JIT resolves their wire thunks. No library
/// loads until one of its items is called. Answers the number of modules.
pub fn install(libraries: &[(PathBuf, Signatures)]) -> usize {
    let mut modules = gossamer_resolve::all_external_modules();
    let mut leaves = Vec::new();
    let mut wire_names = Vec::new();
    for (path, signatures) in libraries {
        let library = Arc::new(library::LazyLibrary::new(path.clone(), signatures));
        leaves.extend(vm::register(Arc::clone(&library), signatures));
        // Compiled code calls an item through its wire thunk, where a
        // callable it calls back runs on the VM active on this thread. An
        // item taking a callable keeps the body calling it on the VM, which
        // passes the callable as the handle the item calls back through.
        let names: Vec<String> = signatures
            .modules
            .iter()
            .flat_map(|module| module.items.iter())
            .filter(|item| !item.wire_symbol.is_empty())
            .filter(|item| !(item.calls_back && item.params.iter().any(SigType::holds_callable)))
            .map(|item| item.wire_symbol.clone())
            .collect();
        for name in &names {
            wire_names.push(&*Box::leak(name.clone().into_boxed_str()));
        }
        LIBRARIES.write().push((library, names));
        modules.extend(signatures.external_modules());
    }
    vm::register_leaves(leaves);
    gossamer_codegen_cranelift::register_lazy_binding_symbols(wire_names, resolve_wire_symbol);
    let count = modules.len();
    gossamer_resolve::set_external_modules(modules);
    count
}

/// The address of the wire thunk `name`, loading its library on first use.
fn resolve_wire_symbol(name: &str) -> Option<*const u8> {
    let library = LIBRARIES
        .read()
        .iter()
        .find(|(_, names)| names.iter().any(|n| n == name))
        .map(|(library, _)| Arc::clone(library))?;
    match library.get() {
        Ok(loaded) => loaded.wire.get(name).map(|addr| addr.cast::<u8>()),
        Err(message) => {
            // The program's call through the symbol raises the load error,
            // as a VM call of the same item does.
            UNAVAILABLE.write().get_or_insert(message);
            Some(unavailable_binding as *const u8)
        }
    }
}

/// Why the library of a JIT-compiled binding call did not load.
static UNAVAILABLE: RwLock<Option<String>> = RwLock::new(None);

/// Stands in for a wire thunk whose library did not load: it takes no
/// argument it reads and never returns, so it fits every thunk's signature.
extern "C-unwind" fn unavailable_binding() {
    let message = UNAVAILABLE
        .read()
        .clone()
        .unwrap_or_else(|| "a binding library did not load".to_string());
    gossamer_runtime::c_abi::binding_host::raise_now(&message);
}
