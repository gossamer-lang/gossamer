//! The two symbols a host loads a binding library by, and the
//! [`ModuleTable`] built from the registry.

#![allow(unsafe_code, reason = "the C entry points the host calls")]

use std::sync::OnceLock;

use gossamer_binding_abi::{
    ABI_VERSION, HostTable, ItemEntry, ModuleEntry, ModuleTable, SIGNATURE_HASH_SEED, StrRef,
    signature_hash,
};

use crate::registry::{REGISTRY, Signature};
use crate::types::Type;

/// The table and the storage its pointers name, built once.
struct Built {
    table: ModuleTable,
    _modules: Vec<ModuleEntry>,
    _items: Vec<Vec<ItemEntry>>,
    _signatures: Vec<String>,
}

// SAFETY: every pointer in `Built` names static text or storage `Built`
// owns, which nothing writes after it is built.
unsafe impl Send for Built {}
// SAFETY: as above.
unsafe impl Sync for Built {}

static BUILT: OnceLock<Built> = OnceLock::new();

fn build() -> Built {
    let mut modules = Vec::new();
    let mut items = Vec::new();
    let mut signatures = Vec::new();
    let mut hash = SIGNATURE_HASH_SEED;
    let mut registered: Vec<_> = REGISTRY.iter().copied().collect();
    // The registry's order follows the link; the table's follows the paths,
    // so its hash does not depend on how the library was linked.
    registered.sort_by_key(|module| module.path);
    for module in registered {
        hash = signature_hash(hash, module.path.as_bytes());
        let mut entries = Vec::with_capacity(module.items.len());
        for item in module.items {
            let signature = signature_json(&item.signature);
            hash = signature_hash(hash, item.name.as_bytes());
            hash = signature_hash(hash, signature.as_bytes());
            entries.push(ItemEntry {
                name: StrRef::new(item.name),
                doc: StrRef::new(item.doc),
                signature: StrRef::new(&signature),
                vm_thunk: item.vm,
                wire_symbol: StrRef::new(item.wire_symbol),
                wire_thunk: item.wire.map_or(std::ptr::null(), |addr| addr().cast()),
                flags: if item.calls_back {
                    gossamer_binding_abi::ITEM_CALLS_BACK
                } else {
                    0
                },
            });
            // The heap text the entry borrows moves with the `String`, so it
            // stays where the entry points.
            signatures.push(signature);
        }
        modules.push(ModuleEntry {
            path: StrRef::new(module.path),
            doc: StrRef::new(module.doc),
            items: entries.as_ptr(),
            item_count: entries.len(),
        });
        items.push(entries);
    }
    let table = ModuleTable {
        abi_version: ABI_VERSION,
        reserved: 0,
        modules: modules.as_ptr(),
        module_count: modules.len(),
        signature_hash: hash,
        free_out: crate::abi::free_out,
    };
    Built {
        table,
        _modules: modules,
        _items: items,
        _signatures: signatures,
    }
}

/// The [`ABI_VERSION`] this library was built against.
#[unsafe(no_mangle)]
pub extern "C" fn gos_binding_abi_version() -> u32 {
    ABI_VERSION
}

/// Installs the host's runtime table and answers this library's modules, or
/// null when the host speaks another major version or its table is
/// incomplete. Calling it again answers the same table.
///
/// # Safety
/// `host` points at a table that lives as long as the process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_binding_init(host: *const HostTable) -> *const ModuleTable {
    // SAFETY: the caller's contract.
    if unsafe { crate::host::install(host) }.is_err() {
        return std::ptr::null();
    }
    match std::panic::catch_unwind(|| &raw const BUILT.get_or_init(build).table) {
        Ok(table) => table,
        Err(_) => std::ptr::null(),
    }
}

/// An item's signature as the JSON a host reads:
/// `{"params":[<type>..],"ret":<type>}`, with `"json":<mask>` when a
/// position crosses as text.
#[must_use]
pub fn signature_json(signature: &Signature) -> String {
    let params: Vec<String> = signature.params.iter().map(type_json).collect();
    let json = if signature.json == 0 {
        String::new()
    } else {
        format!(",\"json\":{}", signature.json)
    };
    format!(
        "{{\"params\":[{}],\"ret\":{}{json}}}",
        params.join(","),
        type_json(&signature.ret)
    )
}

/// A type as JSON: `{"kind":..}` plus the kind's parts.
#[must_use]
pub fn type_json(t: &Type) -> String {
    match t {
        Type::Unit => "{\"kind\":\"unit\"}".to_string(),
        Type::Bool => "{\"kind\":\"bool\"}".to_string(),
        Type::I64 => "{\"kind\":\"i64\"}".to_string(),
        Type::F64 => "{\"kind\":\"f64\"}".to_string(),
        Type::Char => "{\"kind\":\"char\"}".to_string(),
        Type::String => "{\"kind\":\"string\"}".to_string(),
        Type::Bytes => "{\"kind\":\"bytes\"}".to_string(),
        Type::Any => "{\"kind\":\"any\"}".to_string(),
        Type::Tuple(ts) => {
            let inner: Vec<String> = ts.iter().map(type_json).collect();
            format!("{{\"kind\":\"tuple\",\"items\":[{}]}}", inner.join(","))
        }
        Type::Vec(inner) => format!("{{\"kind\":\"vec\",\"of\":{}}}", type_json(inner)),
        Type::Option(inner) => format!("{{\"kind\":\"option\",\"of\":{}}}", type_json(inner)),
        Type::Result(ok, err) => format!(
            "{{\"kind\":\"result\",\"ok\":{},\"err\":{}}}",
            type_json(ok),
            type_json(err)
        ),
        Type::Map(k, v) => format!(
            "{{\"kind\":\"map\",\"key\":{},\"value\":{}}}",
            type_json(k),
            type_json(v)
        ),
        Type::Variant(arms) => {
            let arms: Vec<String> = arms
                .iter()
                .map(|arm| {
                    let payload: Vec<String> = arm.payload.iter().map(type_json).collect();
                    format!(
                        "{{\"name\":{},\"payload\":[{}]}}",
                        json_string(arm.name),
                        payload.join(",")
                    )
                })
                .collect();
            format!("{{\"kind\":\"variant\",\"arms\":[{}]}}", arms.join(","))
        }
        Type::Callback(args, ret) => {
            let args: Vec<String> = args.iter().map(type_json).collect();
            format!(
                "{{\"kind\":\"callback\",\"args\":[{}],\"ret\":{}}}",
                args.join(","),
                type_json(ret)
            )
        }
        Type::Opaque(name) => format!("{{\"kind\":\"opaque\",\"name\":{}}}", json_string(name)),
    }
}

fn json_string(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VariantArm;

    #[test]
    fn a_signature_renders_every_type_kind() {
        let sig = Signature {
            params: &[
                Type::Map(&Type::String, &Type::Vec(&Type::I64)),
                Type::Variant(&[VariantArm {
                    name: "Text",
                    payload: &[Type::String],
                }]),
            ],
            ret: Type::Result(&Type::Bytes, &Type::String),
            json: 0,
        };
        assert_eq!(
            signature_json(&sig),
            "{\"params\":[{\"kind\":\"map\",\"key\":{\"kind\":\"string\"},\"value\":\
             {\"kind\":\"vec\",\"of\":{\"kind\":\"i64\"}}},{\"kind\":\"variant\",\"arms\":\
             [{\"name\":\"Text\",\"payload\":[{\"kind\":\"string\"}]}]}],\"ret\":\
             {\"kind\":\"result\",\"ok\":{\"kind\":\"bytes\"},\"err\":{\"kind\":\"string\"}}}"
        );
    }
}
