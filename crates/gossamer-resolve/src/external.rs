//! Binding-module side table consulted by the type checker (and
//! diagnostics) to validate `use` paths and qualified-path
//! expressions that point at Rust-binding items.
//!
//! `gossamer-resolve` cannot depend on `gossamer-binding` because
//! that crate transitively depends on the interpreter. Instead,
//! this module exposes a small, dependency-free table that the
//! `gos` runner populates at startup via
//! [`set_external_modules`]. Any compiler stage that wants to
//! validate an external path (`tuigoose::layout::split`) reads
//! the table through [`lookup_external_item`] /
//! [`lookup_external_module`].

use std::sync::OnceLock;

use parking_lot::RwLock;

/// Type vocabulary advertised by binding signatures.
///
/// Mirrors `gossamer_binding::Type` exactly. Lives here so the
/// resolver and type checker can validate signatures without
/// pulling in the binding/interpreter dependency tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingType {
    /// `()`.
    Unit,
    /// `bool`.
    Bool,
    /// `i64`.
    I64,
    /// `f64`.
    F64,
    /// `char`.
    Char,
    /// `String`.
    String,
    /// `Bytes` - opaque byte buffer (ABI 0.4+).
    Bytes,
    /// `(T1, T2, ...)`.
    Tuple(Vec<BindingType>),
    /// `[T]`.
    Vec(Box<BindingType>),
    /// `Option<T>`.
    Option(Box<BindingType>),
    /// `Result<T, E>`.
    Result(Box<BindingType>, Box<BindingType>),
    /// `Map<K, V>` (ABI 0.4+).
    Map(Box<BindingType>, Box<BindingType>),
    /// `Variant<...>` - tagged-union (ABI 0.4+).
    Variant(Vec<BindingVariantArm>),
    /// `Fn(args...) -> ret` callback (ABI 0.4+).
    Callback(Vec<BindingType>, Box<BindingType>),
    /// User-defined opaque struct/enum, identified by name.
    Opaque(String),
    /// `_` - type checker accepts anything.
    Any,
}

/// One arm of a [`BindingType::Variant`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingVariantArm {
    /// Arm name.
    pub name: String,
    /// Positional payload types.
    pub payload: Vec<BindingType>,
}

impl BindingType {
    /// Renders the type to its Gossamer-source spelling.
    #[must_use]
    pub fn to_source(&self) -> String {
        match self {
            Self::Unit => "()".to_string(),
            Self::Bool => "bool".to_string(),
            Self::I64 => "i64".to_string(),
            Self::F64 => "f64".to_string(),
            Self::Char => "char".to_string(),
            Self::String => "String".to_string(),
            Self::Bytes => "Bytes".to_string(),
            Self::Tuple(ts) => {
                let inner = ts
                    .iter()
                    .map(Self::to_source)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inner})")
            }
            Self::Vec(t) => format!("[{}]", t.to_source()),
            Self::Option(t) => format!("Option<{}>", t.to_source()),
            Self::Result(t, e) => format!("Result<{}, {}>", t.to_source(), e.to_source()),
            Self::Map(k, v) => format!("Map<{}, {}>", k.to_source(), v.to_source()),
            Self::Variant(arms) => {
                let body = arms
                    .iter()
                    .map(|a| {
                        if a.payload.is_empty() {
                            a.name.clone()
                        } else {
                            let payload = a
                                .payload
                                .iter()
                                .map(Self::to_source)
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{}({payload})", a.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                format!("Variant<{body}>")
            }
            Self::Callback(args, ret) => {
                let params = args
                    .iter()
                    .map(Self::to_source)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Fn({params}) -> {}", ret.to_source())
            }
            Self::Opaque(name) => name.clone(),
            Self::Any => "_".to_string(),
        }
    }
}

/// One module exported by a binding crate (resolver-side view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalModule {
    /// Canonical Gossamer-source path (e.g. `"tuigoose::layout"`).
    pub path: String,
    /// One-line documentation rendered by `gos doc`.
    pub doc: String,
    /// Item names exported from this module.
    pub items: Vec<ExternalItem>,
}

/// One item inside an [`ExternalModule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalItem {
    /// Item name (un-prefixed).
    pub name: String,
    /// Declared positional parameter types.
    pub params: Vec<BindingType>,
    /// Declared return type.
    pub ret: BindingType,
    /// One-line documentation.
    pub doc: String,
    /// The positions that cross as JSON text, which the program's generated
    /// walk writes and reads: bit 0 the return, bit `i + 1` parameter `i`.
    pub json: u64,
}

impl ExternalItem {
    /// Whether the return crosses as JSON text.
    #[must_use]
    pub fn json_ret(&self) -> bool {
        self.json & 1 != 0
    }

    /// Whether parameter `index` crosses as JSON text.
    #[must_use]
    pub fn json_param(&self, index: usize) -> bool {
        index < 63 && self.json & (1 << (index + 1)) != 0
    }
}

/// Process-wide table populated once at runner startup.
static TABLE: OnceLock<RwLock<Vec<ExternalModule>>> = OnceLock::new();

fn table() -> &'static RwLock<Vec<ExternalModule>> {
    TABLE.get_or_init(|| RwLock::new(Vec::new()))
}

/// Replaces the external-module table with `modules`. Idempotent;
/// the per-project runner calls this exactly once before parsing.
pub fn set_external_modules(modules: Vec<ExternalModule>) {
    *table().write() = modules;
}

/// Returns the module declared at `path`, if any.
#[must_use]
pub fn lookup_external_module(path: &str) -> Option<ExternalModule> {
    table().read().iter().find(|m| m.path == path).cloned()
}

/// Resolves `module::item` to its declared item.
#[must_use]
pub fn lookup_external_item(qualified: &str) -> Option<ExternalItem> {
    let (path, name) = qualified.rsplit_once("::")?;
    table()
        .read()
        .iter()
        .find(|m| m.path == path)
        .and_then(|m| m.items.iter().find(|i| i.name == name).cloned())
}

/// Returns every registered module path.
#[must_use]
pub fn all_external_module_paths() -> Vec<String> {
    table().read().iter().map(|m| m.path.clone()).collect()
}

/// Snapshot of the entire table for tooling (`gos doc`).
#[must_use]
pub fn all_external_modules() -> Vec<ExternalModule> {
    table().read().clone()
}

/// Test-only helper: clears the table.
#[doc(hidden)]
pub fn clear_for_test() {
    *table().write() = Vec::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests in this module mutate a process-global module table
    /// (see [`set_external_modules`] / [`clear_for_test`]).
    /// `cargo test` runs unit tests in parallel by default, so two
    /// tests interleaving `clear → set → lookup` will silently
    /// blow each other's state away. This Mutex serialises every
    /// test that touches the global.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn fixture() -> Vec<ExternalModule> {
        vec![ExternalModule {
            path: "tuigoose::layout".to_string(),
            doc: String::new(),
            items: vec![ExternalItem {
                name: "split".to_string(),
                params: vec![BindingType::Vec(Box::new(BindingType::I64))],
                ret: BindingType::Vec(Box::new(BindingType::Vec(Box::new(BindingType::I64)))),
                doc: String::new(),
                json: 0,
            }],
        }]
    }

    #[test]
    fn lookup_module_returns_the_registered_module() {
        let _guard = TEST_LOCK.lock();
        clear_for_test();
        set_external_modules(fixture());
        let m = lookup_external_module("tuigoose::layout").unwrap();
        assert_eq!(m.items.len(), 1);
        clear_for_test();
    }

    #[test]
    fn lookup_item_walks_module_then_item() {
        let _guard = TEST_LOCK.lock();
        clear_for_test();
        set_external_modules(fixture());
        let i = lookup_external_item("tuigoose::layout::split").unwrap();
        assert!(matches!(i.ret, BindingType::Vec(_)));
        assert!(lookup_external_item("tuigoose::layout::missing").is_none());
        clear_for_test();
    }

    #[test]
    fn unknown_module_returns_none() {
        let _guard = TEST_LOCK.lock();
        clear_for_test();
        let m = lookup_external_module("does::not::exist");
        assert!(m.is_none());
    }

    #[test]
    fn binding_type_to_source_round_trips() {
        let t = BindingType::Vec(Box::new(BindingType::Tuple(vec![
            BindingType::I64,
            BindingType::String,
        ])));
        assert_eq!(t.to_source(), "[(i64, String)]");
    }
}
