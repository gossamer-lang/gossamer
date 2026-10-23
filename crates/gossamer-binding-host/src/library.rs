//! Loading a binding library: its ABI version, its init with the host's
//! table, and the items its module table answers.

#![allow(unsafe_code, reason = "calls the library's C entry points")]

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use gossamer_binding_abi::{
    ABI_VERSION, ABI_VERSION_SYMBOL, HostTable, INIT_SYMBOL, ModuleTable, SIGNATURE_HASH_SEED,
    VmThunk, abi_compatible, signature_hash,
};

use crate::HostError;
use crate::signatures::{ItemSignature, ItemTypes, ModuleSignature, Signatures};

/// An item of a loaded library.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LoadedItem {
    pub(crate) vm: VmThunk,
}

/// A loaded library: its signatures and its items by module and name.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) signatures: Signatures,
    pub(crate) items: HashMap<(String, String), LoadedItem>,
    pub(crate) wire: HashMap<String, *const c_void>,
    pub(crate) free_out: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: every pointer names code or static data of a library that stays
// loaded for the life of the process.
unsafe impl Send for Loaded {}
// SAFETY: as above.
unsafe impl Sync for Loaded {}

/// Opens the library at `path`, hands it `host`, and reads its module table.
pub(crate) fn load(path: &Path, host: &'static HostTable) -> Result<Loaded, HostError> {
    let fail = |reason: String| HostError::Load {
        path: path.to_path_buf(),
        reason,
    };
    let handle = crate::dl::open(path).map_err(&fail)?;
    let version_addr = crate::dl::symbol(handle, ABI_VERSION_SYMBOL).ok_or_else(|| {
        fail(format!(
            "it exports no `{ABI_VERSION_SYMBOL}`; it is not a binding library"
        ))
    })?;
    // SAFETY: the symbol is the library's `extern "C" fn() -> u32`.
    let version_fn: extern "C" fn() -> u32 = unsafe { std::mem::transmute(version_addr) };
    let version = version_fn();
    if !abi_compatible(ABI_VERSION, version) {
        return Err(HostError::Abi {
            path: path.to_path_buf(),
            library: version,
            host: ABI_VERSION,
        });
    }
    let init_addr = crate::dl::symbol(handle, INIT_SYMBOL)
        .ok_or_else(|| fail(format!("it exports no `{INIT_SYMBOL}`")))?;
    // SAFETY: the symbol is the library's `gos_binding_init`, whose type the
    // ABI fixes.
    let init: unsafe extern "C" fn(*const HostTable) -> *const ModuleTable =
        unsafe { std::mem::transmute(init_addr) };
    // SAFETY: `host` lives as long as the process, as `init` requires.
    let table = unsafe { init(host) };
    if table.is_null() {
        return Err(fail(
            "its `gos_binding_init` refused the host's table".to_string(),
        ));
    }
    // SAFETY: a non-null table lives as long as the library, which stays
    // loaded.
    let table = unsafe { &*table };
    // SAFETY: as above, for every entry the table points at.
    unsafe { read(path, table) }
}

/// Reads `table`'s modules and checks its hash.
///
/// # Safety
/// `table` and everything it points at stay alive for the process.
unsafe fn read(path: &Path, table: &'static ModuleTable) -> Result<Loaded, HostError> {
    let fail = |reason: String| HostError::Load {
        path: path.to_path_buf(),
        reason,
    };
    let modules = if table.modules.is_null() {
        &[][..]
    } else {
        // SAFETY: the caller's contract.
        unsafe { std::slice::from_raw_parts(table.modules, table.module_count) }
    };
    let mut hash = SIGNATURE_HASH_SEED;
    let mut items = HashMap::new();
    let mut wire = HashMap::new();
    let mut signatures = Vec::with_capacity(modules.len());
    for module in modules {
        // SAFETY: the caller's contract covers every string the table names.
        let module_path = unsafe { module.path.to_string_lossy() };
        hash = signature_hash(hash, module_path.as_bytes());
        let entries = if module.items.is_null() {
            &[][..]
        } else {
            // SAFETY: as above.
            unsafe { std::slice::from_raw_parts(module.items, module.item_count) }
        };
        let mut item_signatures = Vec::with_capacity(entries.len());
        for entry in entries {
            // SAFETY: as above.
            let (name, doc, signature, wire_symbol) = unsafe {
                (
                    entry.name.to_string_lossy(),
                    entry.doc.to_string_lossy(),
                    entry.signature.to_string_lossy(),
                    entry.wire_symbol.to_string_lossy(),
                )
            };
            hash = signature_hash(hash, name.as_bytes());
            hash = signature_hash(hash, signature.as_bytes());
            let types: ItemTypes = serde_json::from_str(&signature).map_err(|e| {
                fail(format!(
                    "the signature of {module_path}::{name} is unreadable: {e}"
                ))
            })?;
            if !wire_symbol.is_empty() && !entry.wire_thunk.is_null() {
                wire.insert(wire_symbol.clone(), entry.wire_thunk);
            }
            items.insert(
                (module_path.clone(), name.clone()),
                LoadedItem { vm: entry.vm_thunk },
            );
            item_signatures.push(ItemSignature {
                name,
                doc,
                params: types.params,
                json: types.json,
                ret: types.ret,
                wire_symbol,
                calls_back: entry.flags & gossamer_binding_abi::ITEM_CALLS_BACK != 0,
            });
        }
        signatures.push(ModuleSignature {
            path: module_path,
            // SAFETY: as above.
            doc: unsafe { module.doc.to_string_lossy() },
            items: item_signatures,
        });
    }
    if hash != table.signature_hash {
        return Err(fail(format!(
            "its table states signature hash {:016x} but holds {hash:016x}",
            table.signature_hash
        )));
    }
    Ok(Loaded {
        signatures: Signatures {
            abi_version: table.abi_version,
            signature_hash: format!("{hash:016x}"),
            modules: signatures,
        },
        items,
        wire,
        free_out: table.free_out,
    })
}

/// A library registered from its cached signatures and loaded on its first
/// call.
#[derive(Debug)]
pub(crate) struct LazyLibrary {
    pub(crate) path: PathBuf,
    expected_hash: Option<u64>,
    loaded: std::sync::OnceLock<Result<Loaded, String>>,
}

impl LazyLibrary {
    pub(crate) fn new(path: PathBuf, signatures: &Signatures) -> Self {
        Self {
            path,
            expected_hash: signatures.hash(),
            loaded: std::sync::OnceLock::new(),
        }
    }

    /// The loaded library, loading it the first time; a library whose
    /// signatures changed since they were cached is refused before any call.
    pub(crate) fn get(&self) -> Result<&Loaded, String> {
        self.loaded
            .get_or_init(|| {
                let loaded = load(&self.path, &crate::vm::HOST_TABLE).map_err(|e| e.to_string())?;
                let found = loaded.signatures.hash();
                if found != self.expected_hash {
                    return Err(HostError::SignatureMismatch {
                        path: self.path.clone(),
                    }
                    .to_string());
                }
                Ok(loaded)
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}
