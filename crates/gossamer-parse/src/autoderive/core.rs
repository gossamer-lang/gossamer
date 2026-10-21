// Serialization surface (serde / kotlinx shape).
// For every user type a serde turbofish names whose fields classify
// (primitives, growable arrays of supported types, nested user types),
// autoderive declares per-type stubs `__gos_serde_to_json_<T>` /
// `__gos_serde_from_json_<T>` (plus the toml / yaml variants), so the
// calls check like any other. The public surface is the generic call form
// `to_json::<T>(value)` / `from_json::<T>(text)`, which
// `rewrite_serde_generic_calls` rewrites into those names. There are no
// `Type::to_json` methods - one spelling only.
//
// The stubs' bodies come after checking: `gossamer-hir`'s serde pass writes
// them from the checked field types, over the runtime's typed reader and
// writer, and every tier compiles them like any other code.


use std::collections::{HashMap, HashSet};

use gossamer_ast::{
    BINARY_SEARCH_PREFIX, EnumDecl, EnumVariant, GenericArg, Item, ItemKind, ModBody,
    ModulePath, NodeId, PARTITION_POINT_PREFIX, SourceFile, STRUCTURAL_COMPARATOR_PREFIX,
    StructBody, StructDecl, TypeKind, USER_COMPARATOR_PREFIX, UseDecl, UseTarget,
};
use gossamer_lex::{FileId, Keyword, Punct, SourceMap, Span, TokenKind};

use crate::{ParseDiagnostic, SerdeTargetRefusal};

/// Classification of a struct field for the synthesizer. Anything
/// outside this set causes the struct to be skipped - we don't want
/// to emit code that won't compile.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FieldKind {
    /// Any signed or unsigned narrow integer (i8/i16/i32/u8/u16/u32),
    /// stored as its source-level spelling. JSON round-trips through
    /// `i64` then casts back at extract time.
    Int(&'static str),
    /// `u64` / `usize`, stored as its spelling. JSON reads it through
    /// `json::as_u64`, which holds the whole unsigned range.
    U64(&'static str),
    I64,
    /// `f64` / `f32`, stored as its spelling. JSON reads it as `f64`.
    Float(&'static str),
    Bool,
    String,
    /// `[T]` / `Vec<T>` of any supported kind.
    Vec(Box<FieldKind>),
    /// Nested user struct, referenced by source-level name.
    Struct(TyId),
    /// A user enum, externally tagged: a unit variant is its name, and a
    /// variant with a payload is a one-key object naming it.
    Enum(TyId),
    /// `Option<T>` - JSON `null` for `None`, else the inner value. A missing
    /// object key also decodes to `None`.
    Option(Box<FieldKind>),
    /// A tuple `(A, B, ...)` - a JSON array of heterogeneous elements.
    Tuple(Vec<FieldKind>),
    /// `Map<String, V>` - a JSON object. Keys are sorted on encode so the
    /// text is deterministic across tiers.
    Map(Box<FieldKind>),
    /// `json::Value` - a dynamic JSON document, passed through unchanged.
    Json,
}

/// Bounds how far a chain of type aliases is followed. A cyclic alias
/// reaches the autoderive passes before the checker reports `GT0024`, so
/// every alias walk terminates on its own rather than relying on that
/// report.
pub(crate) const MAX_ALIAS_DEPTH: u32 = 32;

impl FieldKind {
    /// Classifies `ty` as written in a field of a type declared in `module`.
    fn from_type(
        ty: &gossamer_ast::Type,
        module: &str,
        structs: &StructIdentities,
        aliases: &HashMap<String, gossamer_ast::Type>,
    ) -> Option<Self> {
        Self::from_type_within(ty, module, structs, aliases, 0)
    }

    /// The kind a primitive type name serializes as.
    fn scalar(name: &str) -> Option<Self> {
        Some(match name {
            "i64" => Self::I64,
            "i8" => Self::Int("i8"),
            "i16" => Self::Int("i16"),
            "i32" => Self::Int("i32"),
            "u8" => Self::Int("u8"),
            "u16" => Self::Int("u16"),
            "u32" => Self::Int("u32"),
            "u64" => Self::U64("u64"),
            "usize" => Self::U64("usize"),
            "f64" => Self::Float("f64"),
            "f32" => Self::Float("f32"),
            "bool" => Self::Bool,
            "String" => Self::String,
            _ => return None,
        })
    }

    /// The kind a declared struct, enum, or alias named `name` serializes as.
    fn named(
        name: &str,
        module: &str,
        structs: &StructIdentities,
        aliases: &HashMap<String, gossamer_ast::Type>,
        depth: u32,
    ) -> Option<Self> {
        // An alias declared elsewhere names its target where it is declared.
        let alias = match structs.local(module, name) {
            Some(DeclaredType::Struct(ty)) => return Some(Self::Struct(ty.clone())),
            Some(DeclaredType::Enum(ty)) => return Some(Self::Enum(ty.clone())),
            Some(DeclaredType::Alias(target)) => Some((module, target)),
            Some(DeclaredType::Other) => return None,
            None => match structs.first_alias(name) {
                Some((home, target)) => Some((home.as_str(), target)),
                None => aliases.get(name).map(|target| (module, target)),
            },
        };
        if let Some((home, target)) = alias {
            if depth >= MAX_ALIAS_DEPTH {
                return None;
            }
            return Self::from_type_within(target, home, structs, aliases, depth + 1);
        }
        structs
            .first(name)
            .cloned()
            .map(Self::Struct)
            .or_else(|| structs.first_enum(name).cloned().map(Self::Enum))
    }

    /// The kind of a type written as a module path (`crate::m::T`,
    /// `super::T`, `m::T`), read where the declaring module declares it.
    fn qualified(
        segs: &[gossamer_ast::TypePathSegment],
        module: &str,
        structs: &StructIdentities,
        aliases: &HashMap<String, gossamer_ast::Type>,
        depth: u32,
    ) -> Option<Self> {
        let (last, prefix) = segs.split_last()?;
        if !last.generics.is_empty() || prefix.iter().any(|seg| !seg.generics.is_empty()) {
            return None;
        }
        let mut path: Vec<&str> = Vec::new();
        let mut anchored = false;
        for (i, seg) in prefix.iter().enumerate() {
            match seg.name.name.as_str() {
                "crate" if i == 0 => anchored = true,
                "self" if i == 0 => {
                    path.extend(module.split("::").filter(|m| !m.is_empty()));
                    anchored = true;
                }
                "super" if i == 0 || anchored => {
                    if !anchored {
                        path.extend(module.split("::").filter(|m| !m.is_empty()));
                        anchored = true;
                    }
                    path.pop()?;
                }
                name => path.push(name),
            }
        }
        let absolute = path.join("::");
        // An unanchored path names a child of the writing module first, as
        // the resolver reads it, and otherwise a module from the crate root.
        let candidates = if anchored || module.is_empty() {
            vec![absolute]
        } else {
            vec![format!("{module}::{absolute}"), absolute]
        };
        let name = last.name.name.as_str();
        let (target_module, declared) = candidates
            .into_iter()
            .find_map(|m| structs.local(&m, name).map(|declared| (m, declared)))?;
        match declared {
            DeclaredType::Struct(ty) => Some(Self::Struct(ty.clone())),
            DeclaredType::Enum(ty) => Some(Self::Enum(ty.clone())),
            DeclaredType::Alias(target) if depth < MAX_ALIAS_DEPTH => {
                Self::from_type_within(target, &target_module, structs, aliases, depth + 1)
            }
            DeclaredType::Alias(_) | DeclaredType::Other => None,
        }
    }

    /// `depth` bounds alias expansion, per [`MAX_ALIAS_DEPTH`].
    fn from_type_within(
        ty: &gossamer_ast::Type,
        module: &str,
        structs: &StructIdentities,
        aliases: &HashMap<String, gossamer_ast::Type>,
        depth: u32,
    ) -> Option<Self> {
        // A generic argument that must itself be a supported field kind.
        let arg_kind = |g: &GenericArg, structs: &StructIdentities| -> Option<Self> {
            let GenericArg::Type(inner) = g else {
                return None;
            };
            Self::from_type_within(inner, module, structs, aliases, depth)
        };
        match &ty.kind {
            TypeKind::Path(path) => {
                // `json::Value` (any path ending in `json::Value`) is a dynamic
                // JSON document, serialized by pass-through.
                let segs = &path.segments;
                if segs.len() >= 2
                    && segs[segs.len() - 1].name.name == "Value"
                    && segs[segs.len() - 2].name.name == "json"
                {
                    return Some(Self::Json);
                }
                if segs.len() != 1 {
                    return Self::qualified(segs, module, structs, aliases, depth);
                }
                let seg = &segs[0];
                let name = seg.name.name.as_str();
                if seg.generics.is_empty() {
                    return Self::scalar(name)
                        .or_else(|| Self::named(name, module, structs, aliases, depth));
                }
                match name {
                    "Vec" if seg.generics.len() == 1 => {
                        Some(Self::Vec(Box::new(arg_kind(&seg.generics[0], structs)?)))
                    }
                    "Option" if seg.generics.len() == 1 => {
                        Some(Self::Option(Box::new(arg_kind(&seg.generics[0], structs)?)))
                    }
                    "Map" if seg.generics.len() == 2 => {
                        // Only `String`-keyed maps map cleanly to a JSON object.
                        let GenericArg::Type(key) = &seg.generics[0] else {
                            return None;
                        };
                        let string_key = matches!(
                            &key.kind,
                            TypeKind::Path(kp)
                                if kp.segments.len() == 1 && kp.segments[0].name.name == "String"
                        );
                        if !string_key {
                            return None;
                        }
                        Some(Self::Map(Box::new(arg_kind(&seg.generics[1], structs)?)))
                    }
                    _ => None,
                }
            }
            TypeKind::Slice(inner) => Some(Self::Vec(Box::new(Self::from_type_within(
                inner, module, structs, aliases, depth,
            )?))),
            TypeKind::Tuple(elems) => {
                let mut kinds = Vec::with_capacity(elems.len());
                for e in elems {
                    kinds.push(Self::from_type_within(e, module, structs, aliases, depth)?);
                }
                Some(Self::Tuple(kinds))
            }
            _ => None,
        }
    }

    /// Source-level expression for this field's `Default::default()`
    /// value, used by `#[derive(Default)]` synthesis.
    fn default_literal(&self) -> String {
        match self {
            Self::I64 | Self::Int(_) | Self::U64(_) => "0".to_string(),
            Self::Float(_) => "0.0".to_string(),
            Self::Bool => "false".to_string(),
            Self::String => "\"\"".to_string(),
            Self::Vec(_) => "Vec::from([])".to_string(),
            Self::Struct(ty) | Self::Enum(ty) => format!("{}::default()", ty.path),
            Self::Option(_) => "None".to_string(),
            Self::Tuple(elems) => format!(
                "({})",
                elems
                    .iter()
                    .map(Self::default_literal)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Map(_) => "Map::new()".to_string(),
            Self::Json => "json::Value::Null".to_string(),
        }
    }

    /// The user types this kind reaches, at any depth.
    fn user_types(&self) -> Vec<&TyId> {
        match self {
            Self::Struct(ty) | Self::Enum(ty) => vec![ty],
            Self::Vec(inner) | Self::Option(inner) | Self::Map(inner) => inner.user_types(),
            Self::Tuple(elems) => elems.iter().flat_map(Self::user_types).collect(),
            Self::I64
            | Self::Int(_)
            | Self::U64(_)
            | Self::Float(_)
            | Self::Bool
            | Self::String
            | Self::Json => Vec::new(),
        }
    }

    /// Whether this kind's zero value calls a user type's `default()`: the
    /// type itself, or a tuple holding one. A sequence, map, or `Option` is
    /// empty and calls nothing.
    fn reaches_user_type(&self) -> bool {
        match self {
            Self::Struct(_) | Self::Enum(_) => true,
            Self::Tuple(elems) => elems.iter().any(Self::reaches_user_type),
            _ => false,
        }
    }
}

/// Mangled name of the synthesized serializer free function for a
/// given operation and type. The public surface is the generic call
/// form `to_json::<T>(v)` / `from_json::<T>(s)`, which the parse-time
/// `rewrite_serde_generic_calls` pass rewrites into these names.
pub(crate) fn serde_fn(op: &str, ty: &str) -> String {
    format!("__gos_serde_{op}_{ty}")
}

/// The canonical serde operation a `module::item` turbofish spelling names,
/// or `None` when the pair is not one. `json::decode::<T>` and
/// `from_json::<T>` name the same typed decode, so both reach the same
/// declared function rather than one of them answering a dynamic value.
pub(crate) const fn qualified_serde_op(head: &str, tail: &str) -> Option<&'static str> {
    match (head.as_bytes(), tail.as_bytes()) {
        (b"json", b"decode" | b"from_json") => Some("from_json"),
        (b"json", b"encode" | b"to_json") => Some("to_json"),
        (b"yaml", b"from_yaml") => Some("from_yaml"),
        (b"yaml", b"to_yaml") => Some("to_yaml"),
        (b"toml", b"from_toml") => Some("from_toml"),
        (b"toml", b"to_toml") => Some("to_toml"),
        _ => None,
    }
}

/// How a synthesized body names one user type: `path` is what the emitted
/// source writes (`a::Point` for a type inside `mod a`), and `symbol` is the
/// per-type suffix its free functions carry. Folding the module into the
/// symbol is what lets two modules each declare a `Point`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TyId {
    pub(crate) path: String,
    pub(crate) symbol: String,
    /// The name as declared. Constructors and patterns resolve against the
    /// declaring item, so they spell this rather than the module path.
    pub(crate) bare: String,
    /// The `::`-joined module that declares the type, empty at the unit root.
    pub(crate) module: String,
}

impl TyId {
    /// Builds the identity of `name` as declared in `module` (`::`-joined,
    /// empty at the unit root).
    pub(crate) fn new(module: &str, name: &str) -> Self {
        if module.is_empty() {
            return Self {
                path: name.to_string(),
                symbol: name.to_string(),
                bare: name.to_string(),
                module: String::new(),
            };
        }
        Self {
            path: format!("{module}::{name}"),
            symbol: format!("{}__{name}", module.replace("::", "__")),
            bare: name.to_string(),
            module: module.to_string(),
        }
    }
}

/// What a module declares under a type name, as far as classifying a field
/// needs to know.
#[derive(Debug, Clone)]
pub(crate) enum DeclaredType {
    /// A named or tuple struct, with the identity its synthesized functions use.
    Struct(TyId),
    /// A non-generic enum, with the identity its synthesized functions use.
    Enum(TyId),
    /// A non-generic `type X = T` alias, transparent or opaque.
    Alias(gossamer_ast::Type),
    /// Any other type-namespace item: an enum, a trait, a unit struct, a
    /// generic alias.
    Other,
}

/// Every type name in the tree, per declaring module, with the first
/// declaration of each named / tuple struct anywhere in the tree.
#[derive(Debug, Default)]
pub(crate) struct StructIdentities {
    scoped: HashMap<String, HashMap<String, DeclaredType>>,
    first: HashMap<String, TyId>,
    first_enum: HashMap<String, TyId>,
    first_newtype: HashMap<String, gossamer_ast::Type>,
    newtypes_in: HashMap<String, HashMap<String, gossamer_ast::Type>>,
    first_alias: HashMap<String, (String, gossamer_ast::Type)>,
}

impl StructIdentities {
    /// What `module` itself declares under `name`. A module's own declaration
    /// shadows a same-named type declared elsewhere, as it does for the
    /// resolver, whatever kind of type either one is.
    pub(crate) fn local(&self, module: &str, name: &str) -> Option<&DeclaredType> {
        self.scoped.get(module).and_then(|names| names.get(name))
    }

    /// The first named / tuple struct declared as `name` anywhere in the tree,
    /// for a name the writing module does not declare itself.
    pub(crate) fn first(&self, name: &str) -> Option<&TyId> {
        self.first.get(name)
    }

    /// The first non-generic alias declared as `name` anywhere in the tree,
    /// with the module that declares it, which its target is read in.
    pub(crate) fn first_alias(&self, name: &str) -> Option<&(String, gossamer_ast::Type)> {
        self.first_alias.get(name)
    }

    /// The first non-generic enum declared as `name` anywhere in the tree.
    pub(crate) fn first_enum(&self, name: &str) -> Option<&TyId> {
        self.first_enum.get(name)
    }

    /// The representation of the newtype `name` names in `module`: its own
    /// declaration, or else the first one anywhere in the tree. `None` when
    /// the name is not a newtype there.
    pub(crate) fn newtype(&self, module: &str, name: &str) -> Option<&gossamer_ast::Type> {
        match self.local(module, name) {
            Some(DeclaredType::Alias(_)) => self
                .newtypes_in
                .get(module)
                .and_then(|names| names.get(name)),
            Some(_) => None,
            None => self.first_newtype.get(name),
        }
    }
}

/// Indexes every type-namespace item in the tree by its declaring module and
/// declared name.
pub(crate) fn struct_identities(items: &[Item]) -> StructIdentities {
    let mut out = StructIdentities::default();
    for (module, item) in flatten_items_with_modules(items) {
        let (name, declared) = match &item.kind {
            ItemKind::Struct(decl)
                if matches!(&decl.body, StructBody::Named(_) | StructBody::Tuple(_)) =>
            {
                let ty = TyId::new(&module, &decl.name.name);
                out.first
                    .entry(decl.name.name.clone())
                    .or_insert_with(|| ty.clone());
                (&decl.name.name, DeclaredType::Struct(ty))
            }
            ItemKind::Struct(decl) => (&decl.name.name, DeclaredType::Other),
            ItemKind::TypeAlias(decl) if decl.generics.params.is_empty() => {
                out.first_alias
                    .entry(decl.name.name.clone())
                    .or_insert_with(|| (module.clone(), decl.ty.clone()));
                if decl.nominal {
                    out.first_newtype
                        .entry(decl.name.name.clone())
                        .or_insert_with(|| decl.ty.clone());
                    out.newtypes_in
                        .entry(module.clone())
                        .or_default()
                        .entry(decl.name.name.clone())
                        .or_insert_with(|| decl.ty.clone());
                }
                (&decl.name.name, DeclaredType::Alias(decl.ty.clone()))
            }
            ItemKind::TypeAlias(decl) => (&decl.name.name, DeclaredType::Other),
            ItemKind::Enum(decl) if decl.generics.params.is_empty() => {
                let ty = TyId::new(&module, &decl.name.name);
                out.first_enum
                    .entry(decl.name.name.clone())
                    .or_insert_with(|| ty.clone());
                (&decl.name.name, DeclaredType::Enum(ty))
            }
            ItemKind::Enum(decl) => (&decl.name.name, DeclaredType::Other),
            ItemKind::Trait(decl) => (&decl.name.name, DeclaredType::Other),
            _ => continue,
        };
        out.scoped
            .entry(module)
            .or_default()
            .entry(name.clone())
            .or_insert(declared);
    }
    out
}

/// Right-hand side of every non-generic `type X = T` alias, transparent
/// or opaque, keyed by the alias's name.
///
/// Both forms carry the representation's runtime value, so a field typed
/// by one serializes exactly as the representation does. Generic aliases
/// are excluded: substituting their parameters is the type checker's job,
/// not this pass's.
pub(crate) fn alias_targets(items: &[Item]) -> HashMap<String, gossamer_ast::Type> {
    let mut out: HashMap<String, gossamer_ast::Type> = HashMap::new();
    for (_, item) in flatten_items_with_modules(items) {
        let ItemKind::TypeAlias(decl) = &item.kind else {
            continue;
        };
        if !decl.generics.params.is_empty() {
            continue;
        }
        out.entry(decl.name.name.clone())
            .or_insert_with(|| decl.ty.clone());
    }
    out
}

/// Every item in the tree paired with the `::`-joined path of the module
/// that declares it (empty at the unit root).
pub(crate) fn flatten_items_with_modules(items: &[Item]) -> Vec<(String, &Item)> {
    let mut out = Vec::new();
    collect_flat_items_with_modules(items, &mut String::new(), &mut out, false);
    out
}

/// [`flatten_items_with_modules`] limited to the items this build compiles:
/// an inactive `#[cfg(..)]` item, and everything in an inactive module, is left out.
pub(crate) fn flatten_active_items_with_modules(items: &[Item]) -> Vec<(String, &Item)> {
    let mut out = Vec::new();
    collect_flat_items_with_modules(items, &mut String::new(), &mut out, true);
    out
}

fn collect_flat_items_with_modules<'a>(
    items: &'a [Item],
    module: &mut String,
    out: &mut Vec<(String, &'a Item)>,
    active_only: bool,
) {
    for item in items {
        if active_only && !gossamer_ast::cfg::item_is_active(&item.attrs) {
            continue;
        }
        out.push((module.clone(), item));
        if let ItemKind::Mod(decl) = &item.kind
            && let ModBody::Inline(inner) = &decl.body
        {
            let restore = module.len();
            if !module.is_empty() {
                module.push_str("::");
            }
            module.push_str(&decl.name.name);
            collect_flat_items_with_modules(inner, module, out, active_only);
            module.truncate(restore);
        }
    }
}

/// Mangled name of the synthesized field-reflection function for a
/// struct, reached via `typeInfo::<Type>()`.
#[must_use]
pub(crate) fn type_info_fn(ty: &str) -> String {
    format!("__gos_typeinfo_{ty}")
}

/// Synthesizes the `comptime fn` backer for `codegen(…)`, emitted only when
/// the source calls it. `__gos_codegen` is the identity passthrough the
/// comptime pass keys on to splice a result as raw source rather than as a
/// quoted literal.
fn synthesize_validators(source: &str) -> String {
    let mut out = String::new();
    if source.contains("codegen(") {
        out.push_str("comptime fn __gos_codegen(__src: String) -> String { __src }\n");
    }
    out
}

/// Renders an AST type as a compact source-like string for reflection
/// (`typeInfo`) and for diagnostics that quote a field's written type.
///
/// The match is exhaustive so a type shape added later is a compile error
/// here rather than a silent `_` in a user-facing message.
fn ty_to_string(ty: &gossamer_ast::ty::Type) -> String {
    use gossamer_ast::ty::TypeKind;
    match &ty.kind {
        TypeKind::Unit => "()".to_string(),
        TypeKind::Never => "!".to_string(),
        TypeKind::Infer => "_".to_string(),
        TypeKind::Fn { kind, params, ret } => {
            let params = params
                .iter()
                .map(ty_to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let head = format!("{}({params})", kind.as_str());
            match ret {
                Some(ret) => format!("{head} -> {}", ty_to_string(ret)),
                None => head,
            }
        }
        TypeKind::Slice(inner) => format!("[{}]", ty_to_string(inner)),
        TypeKind::Array { elem, .. } => format!("[{}]", ty_to_string(elem)),
        TypeKind::Ref { inner, .. } => ty_to_string(inner),
        TypeKind::Tuple(elems) => format!(
            "({})",
            elems
                .iter()
                .map(ty_to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeKind::Path(tp) => match tp.segments.last() {
            Some(seg) if !seg.generics.is_empty() => {
                let args: Vec<String> = seg
                    .generics
                    .iter()
                    .map(|g| match g {
                        GenericArg::Type(t) => ty_to_string(t),
                        GenericArg::Const(_) => "_".to_string(),
                    })
                    .collect();
                format!("{}<{}>", seg.name.name, args.join(", "))
            }
            Some(seg) => seg.name.name.clone(),
            None => "_".to_string(),
        },
    }
}

/// Every item in `items`, descending into inline module bodies so a
/// declaration nested in a `mod name { ... }` is visible to the
/// whole-program synthesis passes. A multi-file package is auto-bundled
/// into one source by wrapping each sibling file in `mod <stem> { ... }`
/// (see `gossamer-cli`'s sibling auto-bundle), so a struct declared in
/// another file lives one module level deep; the synthesizers must reach
/// it exactly as the resolver does when it flattens the inline module
/// tree for name resolution.
fn flatten_items(items: &[Item]) -> Vec<&Item> {
    let mut out = Vec::new();
    collect_flat_items(items, &mut out);
    out
}

fn collect_flat_items<'a>(items: &'a [Item], out: &mut Vec<&'a Item>) {
    for item in items {
        out.push(item);
        if let ItemKind::Mod(decl) = &item.kind
            && let ModBody::Inline(inner) = &decl.body
        {
            collect_flat_items(inner, out);
        }
    }
}
