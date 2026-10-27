//! The signatures of a binding library, as its module table states them and
//! as `signatures.json` beside the library caches them, so `gos check` and
//! the first call's stubs know every item without loading the library.

use std::path::Path;

use gossamer_resolve::{BindingType, BindingVariantArm, ExternalItem, ExternalModule};
use serde::{Deserialize, Serialize};

use crate::HostError;

/// Every module and item of one library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signatures {
    /// The ABI version the library was built against.
    pub abi_version: u32,
    /// The library's hash of its paths, names, and signatures, as 16 hex
    /// digits; a library that answers another one has changed.
    pub signature_hash: String,
    /// The modules, by path.
    pub modules: Vec<ModuleSignature>,
}

/// One module of a library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleSignature {
    /// The module's path as Gossamer code names it.
    pub path: String,
    /// Its documentation.
    pub doc: String,
    /// Its items.
    pub items: Vec<ItemSignature>,
}

/// One item of a module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemSignature {
    /// The item's name.
    pub name: String,
    /// Its documentation.
    pub doc: String,
    /// Parameter types.
    pub params: Vec<SigType>,
    /// Return type.
    pub ret: SigType,
    /// The wire thunk compiled code calls, empty when the item has none.
    #[serde(default)]
    pub wire_symbol: String,
    /// Whether the item calls Gossamer callables back, which only a VM call
    /// can serve for a callable of either tier.
    #[serde(default)]
    pub calls_back: bool,
    /// The positions that cross as JSON text: bit 0 the return, bit `i + 1`
    /// parameter `i`.
    #[serde(default)]
    pub json: u64,
}

/// An item's types as the library's JSON states them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ItemTypes {
    pub(crate) params: Vec<SigType>,
    pub(crate) ret: SigType,
    #[serde(default)]
    pub(crate) json: u64,
}

/// A type in a signature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SigType {
    /// `()`.
    Unit,
    /// `bool`.
    Bool,
    /// An integer.
    I64,
    /// `f64`.
    F64,
    /// `char`.
    Char,
    /// `String`.
    String,
    /// A byte buffer.
    Bytes,
    /// A tuple.
    Tuple {
        /// Element types.
        items: Vec<SigType>,
    },
    /// A sequence.
    Vec {
        /// Element type.
        of: Box<SigType>,
    },
    /// `Option<T>`.
    Option {
        /// Inner type.
        of: Box<SigType>,
    },
    /// `Result<T, E>`.
    Result {
        /// `Ok` payload.
        ok: Box<SigType>,
        /// `Err` payload.
        err: Box<SigType>,
    },
    /// `Map<K, V>`.
    Map {
        /// Key type.
        key: Box<SigType>,
        /// Value type.
        value: Box<SigType>,
    },
    /// A closed set of arms the program matches as the enum spelling them.
    Variant {
        /// The arms.
        arms: Vec<ArmSignature>,
    },
    /// A callable.
    Callback {
        /// Argument types.
        args: Vec<SigType>,
        /// Return type.
        ret: Box<SigType>,
    },
    /// A struct or handle by name.
    Opaque {
        /// The type's name.
        name: String,
    },
    /// Any value.
    Any,
}

/// One arm of a [`SigType::Variant`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmSignature {
    /// The arm's name.
    pub name: String,
    /// Its payload types.
    pub payload: Vec<SigType>,
}

impl SigType {
    /// Whether a callable appears anywhere in this type.
    #[must_use]
    pub fn holds_callable(&self) -> bool {
        match self {
            Self::Callback { .. } => true,
            Self::Tuple { items } => items.iter().any(Self::holds_callable),
            Self::Vec { of } | Self::Option { of } => of.holds_callable(),
            Self::Result { ok, err } => ok.holds_callable() || err.holds_callable(),
            Self::Map { key, value } => key.holds_callable() || value.holds_callable(),
            Self::Variant { arms } => arms
                .iter()
                .any(|arm| arm.payload.iter().any(Self::holds_callable)),
            Self::Unit
            | Self::Bool
            | Self::I64
            | Self::F64
            | Self::Char
            | Self::String
            | Self::Bytes
            | Self::Opaque { .. }
            | Self::Any => false,
        }
    }

    /// The resolver's spelling of this type.
    #[must_use]
    pub fn to_binding_type(&self) -> BindingType {
        match self {
            Self::Unit => BindingType::Unit,
            Self::Bool => BindingType::Bool,
            Self::I64 => BindingType::I64,
            Self::F64 => BindingType::F64,
            Self::Char => BindingType::Char,
            Self::String => BindingType::String,
            Self::Bytes => BindingType::Bytes,
            Self::Tuple { items } => {
                BindingType::Tuple(items.iter().map(Self::to_binding_type).collect())
            }
            Self::Vec { of } => BindingType::Vec(Box::new(of.to_binding_type())),
            Self::Option { of } => BindingType::Option(Box::new(of.to_binding_type())),
            Self::Result { ok, err } => BindingType::Result(
                Box::new(ok.to_binding_type()),
                Box::new(err.to_binding_type()),
            ),
            Self::Map { key, value } => BindingType::Map(
                Box::new(key.to_binding_type()),
                Box::new(value.to_binding_type()),
            ),
            Self::Variant { arms } => BindingType::Variant(
                arms.iter()
                    .map(|arm| BindingVariantArm {
                        name: arm.name.clone(),
                        payload: arm.payload.iter().map(Self::to_binding_type).collect(),
                    })
                    .collect(),
            ),
            Self::Callback { args, ret } => BindingType::Callback(
                args.iter().map(Self::to_binding_type).collect(),
                Box::new(ret.to_binding_type()),
            ),
            Self::Opaque { name } => BindingType::Opaque(name.clone()),
            Self::Any => BindingType::Any,
        }
    }
}

impl Signatures {
    /// The resolver's view of these modules.
    #[must_use]
    pub fn external_modules(&self) -> Vec<ExternalModule> {
        self.modules
            .iter()
            .map(|module| ExternalModule {
                path: module.path.clone(),
                doc: module.doc.clone(),
                items: module
                    .items
                    .iter()
                    .map(|item| ExternalItem {
                        name: item.name.clone(),
                        params: item.params.iter().map(SigType::to_binding_type).collect(),
                        ret: item.ret.to_binding_type(),
                        doc: item.doc.clone(),
                        json: item.json,
                    })
                    .collect(),
            })
            .collect()
    }

    /// The hash as the library's table states it.
    pub(crate) fn hash(&self) -> Option<u64> {
        u64::from_str_radix(&self.signature_hash, 16).ok()
    }

    /// Reads `signatures.json` at `path`.
    pub fn read(path: &Path) -> Result<Self, HostError> {
        let text = std::fs::read_to_string(path).map_err(|e| HostError::Signatures {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        serde_json::from_str(&text).map_err(|e| HostError::Signatures {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    /// Writes these signatures to `path`, replacing it in one step.
    pub fn write(&self, path: &Path) -> Result<(), HostError> {
        let fail = |e: std::io::Error| HostError::Signatures {
            path: path.to_path_buf(),
            reason: e.to_string(),
        };
        let text = serde_json::to_string_pretty(self).map_err(|e| HostError::Signatures {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let staged = path.with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&staged, text).map_err(fail)?;
        std::fs::rename(&staged, path).map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_signature_reads_as_the_resolver_types() {
        let json = r#"{"params":[{"kind":"map","key":{"kind":"string"},"value":{"kind":"vec","of":{"kind":"i64"}}}],"ret":{"kind":"result","ok":{"kind":"bytes"},"err":{"kind":"string"}}}"#;
        let types: ItemTypes = serde_json::from_str(json).unwrap();
        assert_eq!(
            types.params[0].to_binding_type(),
            BindingType::Map(
                Box::new(BindingType::String),
                Box::new(BindingType::Vec(Box::new(BindingType::I64)))
            )
        );
        assert_eq!(
            types.ret.to_binding_type(),
            BindingType::Result(Box::new(BindingType::Bytes), Box::new(BindingType::String))
        );
    }
}
