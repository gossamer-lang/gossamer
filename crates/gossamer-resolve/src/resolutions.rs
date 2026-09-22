//! Side table mapping `NodeId`s to their resolved meaning.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use gossamer_ast::NodeId;

use crate::def_id::{DefId, DefKind};

/// Where a named reference points after resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Resolution {
    /// A local binding introduced by a `let`, pattern, or parameter. The
    /// `NodeId` identifies the binding occurrence.
    Local(NodeId),
    /// A named item defined in the current crate.
    Def {
        /// Stable identifier of the definition.
        def: DefId,
        /// Kind of definition the id refers to.
        kind: DefKind,
    },
    /// A built-in primitive type (`i32`, `bool`, ...).
    Primitive(PrimitiveTy),
    /// A name imported by a `use` declaration. The leading segment still
    /// needs to be looked up externally; this resolution simply records
    /// that the name is an imported alias.
    Import {
        /// `NodeId` of the `use` declaration that introduced the name.
        use_id: NodeId,
    },
    /// The name could not be resolved; a diagnostic has been produced.
    Err,
}

/// Built-in primitive types recognised directly by the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PrimitiveTy {
    /// `bool`.
    Bool,
    /// `char`.
    Char,
    /// The owned `String` type used for text.
    String,
    /// A signed integer type with width in bits (8, 16, 32, 64, 128) or
    /// `0` to signal `isize`.
    Int(IntWidth),
    /// An unsigned integer type with width in bits (8, 16, 32, 64, 128) or
    /// `0` to signal `usize`.
    UInt(IntWidth),
    /// A floating-point type (`f32` or `f64`).
    Float(FloatWidth),
    /// The never type `!`.
    Never,
    /// The unit type `()` reached via `Self::Unit` is expressed as the
    /// `TypeKind::Unit` variant rather than a primitive path, but the
    /// resolver still emits this variant if user code writes `Unit`.
    Unit,
}

/// Width tag for integer primitive types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IntWidth {
    /// Pointer-sized (`isize` / `usize`).
    Size,
    /// 8-bit (`i8` / `u8`).
    W8,
    /// 16-bit.
    W16,
    /// 32-bit.
    W32,
    /// 64-bit.
    W64,
    /// 128-bit.
    W128,
}

/// Width tag for floating-point primitive types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FloatWidth {
    /// 32-bit IEEE-754 binary32.
    W32,
    /// 64-bit IEEE-754 binary64.
    W64,
}

/// Side table produced by [`crate::resolve_source_file`].
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Resolutions {
    entries: HashMap<NodeId, Resolution>,
    bindings: HashMap<NodeId, DefId>,
    definitions: HashMap<DefId, DefKind>,
    project_aliases: HashMap<String, String>,
    module_aliases: HashMap<String, String>,
    /// The same bindings keyed by the `::`-joined path of the module whose
    /// `use` introduced each, so two modules may bind one name to different
    /// modules.
    scoped_module_aliases: HashMap<String, HashMap<String, String>>,
    import_defs: HashMap<NodeId, DefId>,
}

impl Resolutions {
    /// Returns an empty resolution table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `path_node` resolves to `resolution`.
    pub fn insert(&mut self, path_node: NodeId, resolution: Resolution) {
        self.entries.insert(path_node, resolution);
    }

    /// Records that `use "id" as alias` binds `alias` to the inlined
    /// dependency module named `module`.
    pub fn insert_project_alias(&mut self, alias: String, module: String) {
        self.project_aliases.insert(alias, module);
    }

    /// The inlined dependency module `alias` was bound to, if any. A
    /// path headed by the alias names items registered under the
    /// module's real name, so name-keyed dispatch has to spell it.
    #[must_use]
    pub fn project_alias(&self, alias: &str) -> Option<&str> {
        self.project_aliases.get(alias).map(String::as_str)
    }

    /// Records that a `use path::to::module [as alias]` binds `alias` to
    /// the module at `module`.
    ///
    /// Kept apart from the project aliases because those also say which
    /// module roots are inlined packages, which is what `pub(package)`
    /// reachability is defined against; a module import says nothing
    /// about packages.
    pub fn insert_module_alias(&mut self, scope: &[String], alias: String, module: String) {
        self.scoped_module_aliases
            .entry(scope.join("::"))
            .or_default()
            .insert(alias.clone(), module.clone());
        self.module_aliases.insert(alias, module);
    }

    /// The module a `use`-imported name was bound to, if any. Its items
    /// register under the module's own path, so name-keyed dispatch has
    /// to spell that rather than the name the import introduced.
    #[must_use]
    pub fn module_alias(&self, alias: &str) -> Option<&str> {
        self.module_aliases.get(alias).map(String::as_str)
    }

    /// The module `alias` leads to for a path written in module `scope`: the
    /// innermost enclosing module that imported the name decides, and a name
    /// no enclosing module imported falls back to [`Self::module_alias`].
    #[must_use]
    pub fn module_alias_in(&self, scope: &[String], alias: &str) -> Option<&str> {
        (0..=scope.len())
            .rev()
            .find_map(|depth| {
                self.scoped_module_aliases
                    .get(&scope[..depth].join("::"))
                    .and_then(|aliases| aliases.get(alias))
            })
            .map(String::as_str)
            .or_else(|| self.module_alias(alias))
    }

    /// Records that the `use`-imported name at `node` ultimately refers
    /// to `def`, an item of this same compilation unit.
    ///
    /// The node keeps its [`Resolution::Import`] so HIR lowering still
    /// qualifies the name by the import's path; this side table is what
    /// lets the type checker read the target's signature.
    pub fn insert_import_def(&mut self, node: NodeId, def: DefId) {
        self.import_defs.insert(node, def);
    }

    /// Definition a `use`-imported name at `node` refers to, when the
    /// target is an item of this compilation unit.
    #[must_use]
    pub fn import_def(&self, node: NodeId) -> Option<DefId> {
        self.import_defs.get(&node).copied()
    }

    /// Root module segment naming the inlined package `module` belongs
    /// to, or `None` when it belongs to the package being compiled.
    ///
    /// A path dependency's source is inlined under a module named for its
    /// project id, so that root segment is what separates one package's
    /// modules from another's inside a single compilation unit. This is
    /// the rule `pub(package)` reachability is defined against.
    #[must_use]
    pub fn package_root<'m>(&self, module: &'m [String]) -> Option<&'m str> {
        let first = module.first()?;
        self.project_aliases
            .values()
            .any(|inlined| inlined == first)
            .then_some(first.as_str())
    }

    /// True when `a` and `b` belong to the same package.
    #[must_use]
    pub fn same_package(&self, a: &[String], b: &[String]) -> bool {
        self.package_root(a) == self.package_root(b)
    }

    /// Looks up the resolution for a path node. Returns `None` if the
    /// node was never touched by the resolver (e.g. because it appears
    /// outside of a resolved position) or has no resolution recorded.
    #[must_use]
    pub fn get(&self, path_node: NodeId) -> Option<Resolution> {
        self.entries.get(&path_node).copied()
    }

    /// Returns `true` when at least one resolution has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of recorded resolutions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Records the [`DefId`] allocated for an item or binding node.
    pub fn insert_definition(&mut self, node: NodeId, def: DefId, kind: DefKind) {
        self.bindings.insert(node, def);
        self.definitions.insert(def, kind);
    }

    /// Returns the [`DefId`] assigned to a definition node, if any.
    #[must_use]
    pub fn definition_of(&self, node: NodeId) -> Option<DefId> {
        self.bindings.get(&node).copied()
    }

    /// Returns the [`DefKind`] associated with a definition id, if any.
    #[must_use]
    pub fn kind_of(&self, def: DefId) -> Option<DefKind> {
        self.definitions.get(&def).copied()
    }

    /// Returns every `(NodeId, Resolution)` pair in a deterministic order
    /// (sorted by node id). Useful for snapshot comparisons.
    #[must_use]
    pub fn sorted_entries(&self) -> Vec<(NodeId, Resolution)> {
        let mut pairs: Vec<(NodeId, Resolution)> =
            self.entries.iter().map(|(k, v)| (*k, *v)).collect();
        pairs.sort_by_key(|(node, _)| node.as_u32());
        pairs
    }
}
