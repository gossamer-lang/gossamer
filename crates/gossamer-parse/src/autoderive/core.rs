// Compile-time codegen for serialization (serde / kotlinx shape).
// For every user struct whose fields the synthesizer can classify
// (primitives, growable arrays of supported types, nested user
// structs), we synthesize per-type free functions
// `__gos_serde_to_json_<T>` / `__gos_serde_from_json_<T>` (plus the
// toml / yaml variants) as real Gossamer source, parse it, and merge
// it into the program. The public surface is the generic call form
// `to_json::<T>(value)` / `from_json::<T>(text)`, which
// `rewrite_serde_generic_calls` rewrites into those names. There are
// no `Type::to_json` methods - one spelling only.
//
// Because the synthesized functions are ordinary Gossamer code, they
// compile through every tier (VM + Cranelift + LLVM) automatically.
// There is no VM-only intercept; no runtime schema registry; no
// per-call dispatch overhead.


use std::collections::{HashMap, HashSet};

use gossamer_ast::{
    BINARY_SEARCH_PREFIX, EnumDecl, EnumVariant, GenericArg, Item, ItemKind, ModBody,
    ModulePath, NodeId, PARTITION_POINT_PREFIX, SourceFile, STRUCTURAL_COMPARATOR_PREFIX,
    StructBody, StructDecl, TypeKind, USER_COMPARATOR_PREFIX, UseDecl, UseTarget,
};
use gossamer_lex::{FileId, Keyword, Lexer, Punct, SourceMap, Span, TokenKind};

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
    I64,
    F64,
    Bool,
    String,
    /// `[T]` / `Vec<T>` of any supported kind.
    Vec(Box<FieldKind>),
    /// Nested user struct, referenced by source-level name.
    Struct(TyId),
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
    fn from_type(
        ty: &gossamer_ast::Type,
        structs: &HashMap<String, TyId>,
        aliases: &HashMap<String, gossamer_ast::Type>,
    ) -> Option<Self> {
        Self::from_type_within(ty, structs, aliases, 0)
    }

    /// `depth` bounds alias expansion, per [`MAX_ALIAS_DEPTH`].
    fn from_type_within(
        ty: &gossamer_ast::Type,
        structs: &HashMap<String, TyId>,
        aliases: &HashMap<String, gossamer_ast::Type>,
        depth: u32,
    ) -> Option<Self> {
        // A generic argument that must itself be a supported field kind.
        let arg_kind = |g: &GenericArg, structs: &HashMap<String, TyId>| -> Option<Self> {
            let GenericArg::Type(inner) = g else {
                return None;
            };
            Self::from_type_within(inner, structs, aliases, depth)
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
                    return None;
                }
                let seg = &segs[0];
                let name = seg.name.name.as_str();
                if seg.generics.is_empty() {
                    return match name {
                        "i64" => Some(Self::I64),
                        "i8" => Some(Self::Int("i8")),
                        "i16" => Some(Self::Int("i16")),
                        "i32" => Some(Self::Int("i32")),
                        "u8" => Some(Self::Int("u8")),
                        "u16" => Some(Self::Int("u16")),
                        "u32" => Some(Self::Int("u32")),
                        "f64" => Some(Self::F64),
                        "f32" => Some(Self::F64),
                        "bool" => Some(Self::Bool),
                        "String" => Some(Self::String),
                        other => {
                            if let Some(target) = aliases.get(other) {
                                if depth >= MAX_ALIAS_DEPTH {
                                    return None;
                                }
                                return Self::from_type_within(
                                    target,
                                    structs,
                                    aliases,
                                    depth + 1,
                                );
                            }
                            structs.get(other).cloned().map(Self::Struct)
                        }
                    };
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
                inner, structs, aliases, depth,
            )?))),
            TypeKind::Tuple(elems) => {
                let mut kinds = Vec::with_capacity(elems.len());
                for e in elems {
                    kinds.push(Self::from_type_within(e, structs, aliases, depth)?);
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
            Self::I64 | Self::Int(_) => "0".to_string(),
            Self::F64 => "0.0".to_string(),
            Self::Bool => "false".to_string(),
            Self::String => "\"\"".to_string(),
            Self::Vec(_) => "Vec::from([])".to_string(),
            Self::Struct(ty) => format!("{}::default()", ty.path),
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

    /// Source-level type spelling for a `let mut acc: ... = Vec::from([])`
    /// declaration used while accumulating a Vec field.
    fn type_spelling(&self) -> String {
        match self {
            Self::I64 => "i64".to_string(),
            Self::Int(name) => (*name).to_string(),
            Self::F64 => "f64".to_string(),
            Self::Bool => "bool".to_string(),
            Self::String => "String".to_string(),
            Self::Vec(inner) => format!("Vec<{}>", inner.type_spelling()),
            Self::Struct(ty) => ty.path.clone(),
            Self::Option(inner) => format!("Option<{}>", inner.type_spelling()),
            Self::Tuple(elems) => format!(
                "({})",
                elems
                    .iter()
                    .map(Self::type_spelling)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Map(inner) => format!("Map<String, {}>", inner.type_spelling()),
            Self::Json => "json::Value".to_string(),
        }
    }

    /// Gossamer source fragment that renders a single value bound to
    /// `expr` to JSON-syntax text. Returns an expression of type
    /// `String` - or, for nested structs, a `?`-propagating call.
    fn render_to_json(&self, expr: &str) -> String {
        match self {
            Self::I64 | Self::Int(_) | Self::F64 | Self::Bool => {
                format!("format(\"{{}}\", {expr})")
            }
            Self::String => format!("format(\"\\\"{{}}\\\"\", {expr})"),
            Self::Vec(inner) => render_vec_to_json(expr, inner),
            Self::Struct(ty) => format!("{}({expr})?", to_json_fn(&ty.symbol)),
            Self::Option(inner) => {
                let some_render = inner.render_to_json("__inner");
                format!(
                    "match {expr} {{ Some(__inner) => {some_render}, None => \"null\" }}"
                )
            }
            Self::Tuple(elems) => render_tuple_to_json(expr, elems),
            Self::Map(inner) => render_map_to_json(expr, inner),
            Self::Json => format!("json::render({expr})"),
        }
    }

    /// Gossamer source for a strict match-extract block. `value_expr`
    /// is the `json::Value` bound name; the block evaluates to the
    /// typed Gossamer value or `return`s a structured `errors::Error`
    /// with `path` (e.g. "field `tags`: ...") on mismatch.
    fn extract_strict(&self, value_expr: &str, path: &str) -> String {
        match self {
            Self::I64 => format!(
                "match json::as_i64({value_expr}) {{ Some(__v) => __v, None => return Err(errors::new(\"{path}: expected i64\")) }}"
            ),
            Self::Int(width) => format!(
                "match json::as_i64({value_expr}) {{ Some(__v) => __v as {width}, None => return Err(errors::new(\"{path}: expected {width}\")) }}"
            ),
            Self::F64 => format!(
                "match json::as_f64({value_expr}) {{ Some(__v) => __v, None => return Err(errors::new(\"{path}: expected f64\")) }}"
            ),
            Self::Bool => format!(
                "match json::as_bool({value_expr}) {{ Some(__v) => __v, None => return Err(errors::new(\"{path}: expected bool\")) }}"
            ),
            Self::String => format!(
                "match json::as_str({value_expr}) {{ Some(__v) => __v, None => return Err(errors::new(\"{path}: expected string\")) }}"
            ),
            Self::Vec(inner) => extract_vec_strict(value_expr, inner, path),
            Self::Struct(ty) => format!(
                "match {}({value_expr}) {{ Ok(__v) => __v, Err(__e) => return Err(errors::wrap(__e, \"{path}\")) }}",
                from_json_value_fn(&ty.symbol)
            ),
            Self::Option(inner) => {
                let some_extract = inner.extract_strict(value_expr, path);
                format!("if json::is_null({value_expr}) {{ None }} else {{ Some({some_extract}) }}")
            }
            Self::Tuple(elems) => extract_tuple_strict(value_expr, elems, path),
            Self::Map(inner) => extract_map_strict(value_expr, inner, path),
            // The field already holds a parsed dynamic JSON value.
            Self::Json => value_expr.to_string(),
        }
    }

    /// `true` if this kind decodes a missing object key to a value (rather than
    /// erroring). Only `Option` does - a missing optional field is `None`.
    fn tolerates_missing_key(&self) -> bool {
        matches!(self, Self::Option(_))
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
/// synthesized function rather than one of them answering a dynamic value.
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
fn to_json_fn(ty: &str) -> String {
    serde_fn("to_json", ty)
}
fn from_json_fn(ty: &str) -> String {
    serde_fn("from_json", ty)
}

/// Name of the decoder that takes an already-parsed `json::Value`.
///
/// A nested value the walk is standing on is a handle onto the document it
/// came from, so the member decodes from that node directly. The text-taking
/// decoder is the same body behind one `json::parse`.
pub(crate) fn from_json_value_fn(ty: &str) -> String {
    serde_fn("from_json_value", ty)
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
            };
        }
        Self {
            path: format!("{module}::{name}"),
            symbol: format!("{}__{name}", module.replace("::", "__")),
            bare: name.to_string(),
        }
    }
}

/// Every named / tuple struct in the tree, indexed by its declared name and
/// carrying the identity its synthesized functions use. A name two modules
/// share resolves to the first declaration, matching how the resolver and
/// type checker break the same tie.
pub(crate) fn struct_identities(items: &[Item]) -> HashMap<String, TyId> {
    let mut out: HashMap<String, TyId> = HashMap::new();
    for (module, item) in flatten_items_with_modules(items) {
        let ItemKind::Struct(decl) = &item.kind else {
            continue;
        };
        if !matches!(&decl.body, StructBody::Named(_) | StructBody::Tuple(_)) {
            continue;
        }
        out.entry(decl.name.name.clone())
            .or_insert_with(|| TyId::new(&module, &decl.name.name));
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

/// Names of the non-generic opaque aliases (`type X = new T`).
///
/// Serialization reads and writes the representation, so a field typed
/// by one needs an explicit `.into()` where the synthesized code builds
/// the value back; formatting needs none, since an opaque alias renders
/// as its representation.
pub(crate) fn opaque_alias_names(items: &[Item]) -> HashSet<String> {
    let mut out = HashSet::new();
    for (_, item) in flatten_items_with_modules(items) {
        let ItemKind::TypeAlias(decl) = &item.kind else {
            continue;
        };
        if decl.nominal && decl.generics.params.is_empty() {
            out.insert(decl.name.name.clone());
        }
    }
    out
}

/// Every item in the tree paired with the `::`-joined path of the module
/// that declares it (empty at the unit root).
pub(crate) fn flatten_items_with_modules(items: &[Item]) -> Vec<(String, &Item)> {
    let mut out = Vec::new();
    collect_flat_items_with_modules(items, &mut String::new(), &mut out);
    out
}

fn collect_flat_items_with_modules<'a>(
    items: &'a [Item],
    module: &mut String,
    out: &mut Vec<(String, &'a Item)>,
) {
    for item in items {
        out.push((module.clone(), item));
        if let ItemKind::Mod(decl) = &item.kind
            && let ModBody::Inline(inner) = &decl.body
        {
            let restore = module.len();
            if !module.is_empty() {
                module.push_str("::");
            }
            module.push_str(&decl.name.name);
            collect_flat_items_with_modules(inner, module, out);
            module.truncate(restore);
        }
    }
}

fn render_vec_to_json(expr: &str, inner: &FieldKind) -> String {
    // Inline a tiny per-Vec assembly. We emit a block expression so
    // the surrounding `out += ...` lands a single concatenated String.
    let elem_render = inner.render_to_json("__item");
    format!(
        "{{ let mut __buf = \"[\"\n            let mut __first = true\n            for __item in {expr} {{\n                if !__first {{ __buf += \",\" }}\n                __first = false\n                __buf += {elem_render}\n            }}\n            __buf += \"]\"\n            __buf }}"
    )
}

fn extract_vec_strict(value_expr: &str, inner: &FieldKind, path: &str) -> String {
    let elem_path = format!("{path}[index]");
    let inner_extract = inner.extract_strict("__elem", &elem_path);
    let elem_ty = inner.type_spelling();
    format!(
        "match json::as_array({value_expr}) {{\n                Some(__arr) => {{\n                    let mut __out: Vec<{elem_ty}> = Vec::from([])\n                    for __elem in __arr {{\n                        let __converted = {inner_extract}\n                        __out.push(__converted)\n                    }}\n                    __out\n                }}\n                None => return Err(errors::new(\"{path}: expected array\")),\n            }}"
    )
}

fn render_tuple_to_json(expr: &str, elems: &[FieldKind]) -> String {
    let mut out = String::from("{ let mut __buf = \"[\"\n");
    for (i, k) in elems.iter().enumerate() {
        if i > 0 {
            out.push_str("            __buf += \",\"\n");
        }
        let er = k.render_to_json(&format!("{expr}.{i}"));
        out.push_str(&format!("            __buf += {er}\n"));
    }
    out.push_str("            __buf += \"]\"\n            __buf }");
    out
}

fn extract_tuple_strict(value_expr: &str, elems: &[FieldKind], path: &str) -> String {
    let n = elems.len();
    let mut out = format!(
        "match json::as_array({value_expr}) {{\n                Some(__arr) => {{\n                    if __arr.len() != {n} {{ return Err(errors::new(\"{path}: expected {n}-element array\")) }}\n"
    );
    let mut names = Vec::with_capacity(n);
    for (i, k) in elems.iter().enumerate() {
        let ex = k.extract_strict(&format!("__arr[{i}]"), &format!("{path}.{i}"));
        out.push_str(&format!("                    let __e{i} = {ex}\n"));
        names.push(format!("__e{i}"));
    }
    out.push_str(&format!("                    ({})\n", names.join(", ")));
    out.push_str(&format!(
        "                }}\n                None => return Err(errors::new(\"{path}: expected array\")),\n            }}"
    ));
    out
}

fn render_map_to_json(expr: &str, inner: &FieldKind) -> String {
    // Sort keys so the object text is deterministic across tiers (a HashMap's
    // iteration order is not stable and differs interp-vs-compiled).
    let vr = inner.render_to_json("__v");
    format!(
        "{{ let mut __ks = {expr}.keys()\n            __ks.sort()\n            let mut __buf = \"{{\"\n            let mut __first = true\n            for __k in __ks {{\n                if !__first {{ __buf += \",\" }}\n                __first = false\n                __buf += format(\"\\\"{{}}\\\":\", __k)\n                if let Some(__v) = {expr}.get(__k) {{ __buf += {vr} }}\n            }}\n            __buf += \"}}\"\n            __buf }}"
    )
}

fn extract_map_strict(value_expr: &str, inner: &FieldKind, path: &str) -> String {
    // Unique bind names (`__map*`) so nested maps and the enclosing field's own
    // `__child` binding never collide - the compiled tier resolves shadows
    // differently from the VM, so an ambiguous reuse silently miscompiles.
    let vt = inner.type_spelling();
    let ve = inner.extract_strict("__mapval", &format!("{path}[key]"));
    format!(
        "match json::keys({value_expr}) {{\n                Some(__mapkeys) => {{\n                    let mut __map: Map<String, {vt}> = Map::new()\n                    for __mapk in __mapkeys {{\n                        let __mapval = match json::get({value_expr}, __mapk) {{ Some(__mc) => __mc, None => return Err(errors::new(\"{path}: missing key\")) }}\n                        let __mapentry = {ve}\n                        __map.insert(__mapk, __mapentry)\n                    }}\n                    __map\n                }}\n                None => return Err(errors::new(\"{path}: expected object\")),\n            }}"
    )
}

/// Mangled name of the synthesized field-reflection function for a
/// struct, reached via `typeInfo::<Type>()`.
#[must_use]
pub(crate) fn type_info_fn(ty: &str) -> String {
    format!("__gos_typeinfo_{ty}")
}

/// Synthesizes the `comptime fn` backers for the compile-time macros:
/// the `regex::compile("…")` / `sql::statement("…")` validators, and the `codegen(…)`
/// source-emitter. Emitted only when the source uses the matching macro,
/// so programs that use none carry no extra items. Each validator returns
/// its input on success and `panic!`s on malformed input - a comptime
/// panic fails the build. `__gos_codegen` is the identity passthrough the
/// comptime pass keys on to splice a result as raw source rather than as a
/// quoted literal.
/// Every string literal `source` hands to `head` as a first argument,
/// in source spelling (escapes included), ignoring the whitespace a call
/// may carry after the `(`.
fn literal_call_arguments(source: &str, head: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(found) = source[at..].find(head) {
        let after_head = at + found + head.len();
        let trimmed = source[after_head..].trim_start();
        let start = source.len() - trimmed.len();
        at = after_head;
        if !trimmed.starts_with('"') {
            continue;
        }
        // Walk to the closing quote, stepping over an escaped one.
        let bytes = source.as_bytes();
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        if i < bytes.len() {
            out.push(source[start..=i].to_string());
            at = i + 1;
        }
    }
    out
}

/// Whether `source` calls `head` with a string literal as its first
/// argument.
fn contains_literal_call(source: &str, head: &str) -> bool {
    !literal_call_arguments(source, head).is_empty()
}

fn synthesize_validators(source: &str) -> String {
    let mut out = String::new();
    if source.contains("codegen(") {
        out.push_str("comptime fn __gos_codegen(__src: String) -> String { __src }\n");
    }
    // Only a literal argument is validated while the program is compiled,
    // so only a literal call site needs the validator spliced in. A
    // pattern built at run time reaches `regex::compile` directly, and a
    // body the unit never calls would still cost every pass that walks it.
    let regex_literals = literal_call_arguments(source, "regex::compile(");
    if source.contains("regex!") || !regex_literals.is_empty() {
        out.push_str(
            "comptime fn __gos_regex_validate(p: String) -> String {\n\
             \tmatch regex::compile(p) {\n\
             \t\tOk(_) => p,\n\
             \t\tErr(__e) => panic(\"invalid regex `{}`: {}\", p, __e),\n\
             \t}\n\
             }\n",
        );
        // One `const` per literal pattern. The initialiser folds while the
        // program is compiled, which is where the validation happens, and
        // the call site keeps the ordinary `regex::compile` it was written
        // as - so nothing of the check survives into the running program.
        for (i, literal) in regex_literals.iter().enumerate() {
            out.push_str(&format!(
                "const __GOS_REGEX_CHECK_{i}: String = __gos_regex_validate({literal})\n"
            ));
        }
    }
    if source.contains("sql!") || contains_literal_call(source, "sql::statement(") {
        out.push_str(
            "comptime fn __gos_sql_validate(q: String) -> String {\n\
             \tif q.len() == 0 { panic(\"empty SQL statement\") }\n\
             \tlet mut depth = 0\n\
             \tlet mut i = 0\n\
             \twhile i < q.len() {\n\
             \t\tlet b = q.byte_at(i)\n\
             \t\tif b == 40 { depth += 1 }\n\
             \t\tif b == 41 { depth -= 1 }\n\
             \t\tif depth < 0 { panic(\"unbalanced parentheses in SQL: {}\", q) }\n\
             \t\ti += 1\n\
             \t}\n\
             \tif depth != 0 { panic(\"unbalanced parentheses in SQL: {}\", q) }\n\
             \tq\n\
             }\n",
        );
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
