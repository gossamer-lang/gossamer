/// The classified body of one struct the synthesizer may emit for.
enum SerdeShape {
    Named(Vec<(String, FieldKind)>, HashSet<String>),
    Tuple(Vec<FieldKind>),
}

impl SerdeShape {
    fn kinds(&self) -> Box<dyn Iterator<Item = &FieldKind> + '_> {
        match self {
            Self::Named(fields, _) => Box::new(fields.iter().map(|(_, kind)| kind)),
            Self::Tuple(fields) => Box::new(fields.iter()),
        }
    }
}

/// True when every user type this field kind reaches has a synthesized
/// serializer of its own, so the emitted body can call it.
fn kind_is_emittable(kind: &FieldKind, emittable: &HashSet<String>) -> bool {
    match kind {
        FieldKind::Struct(ty) => emittable.contains(&ty.symbol),
        FieldKind::Vec(inner) | FieldKind::Option(inner) | FieldKind::Map(inner) => {
            kind_is_emittable(inner, emittable)
        }
        FieldKind::Tuple(elems) => elems.iter().all(|e| kind_is_emittable(e, emittable)),
        FieldKind::Int(_)
        | FieldKind::I64
        | FieldKind::F64
        | FieldKind::Bool
        | FieldKind::String
        | FieldKind::Json => true,
    }
}

/// Walks `parsed` for struct definitions and synthesizes
/// serialization-method source for each eligible struct. Returns the
/// generated source text, ready to be parsed and merged.
#[must_use]
pub fn synthesize_serde_impls(parsed: &SourceFile) -> String {
    let mut out = String::new();
    out.push_str("// Synthesized by `gossamer-parse::autoderive`.\n");
    out.push('\n');

    let struct_names: HashMap<String, TyId> = struct_identities(&parsed.items);
    let aliases = alias_targets(&parsed.items);
    let opaque = opaque_alias_names(&parsed.items);

    let mut classified: Vec<(TyId, SerdeShape)> = Vec::new();
    for (module, item) in flatten_items_with_modules(&parsed.items) {
        let ItemKind::Struct(decl) = &item.kind else {
            continue;
        };
        if !decl.generics.params.is_empty() {
            continue;
        }
        let ty = TyId::new(&module, &decl.name.name);
        match &decl.body {
            StructBody::Named(fields) => {
                let typed: Option<Vec<(String, FieldKind)>> = fields
                    .iter()
                    .map(|f| {
                        FieldKind::from_type(&f.ty, &struct_names, &aliases)
                            .map(|k| (f.name.name.clone(), k))
                    })
                    .collect();
                let opaque_fields: HashSet<String> = fields
                    .iter()
                    .filter(|f| type_names_opaque_alias(&f.ty, &opaque))
                    .map(|f| f.name.name.clone())
                    .collect();
                if let Some(typed) = typed {
                    classified.push((ty, SerdeShape::Named(typed, opaque_fields)));
                }
            }
            StructBody::Tuple(fields) => {
                let typed: Option<Vec<FieldKind>> = fields
                    .iter()
                    .map(|f| FieldKind::from_type(&f.ty, &struct_names, &aliases))
                    .collect();
                if let Some(typed) = typed {
                    classified.push((ty, SerdeShape::Tuple(typed)));
                }
            }
            // A unit struct is the zero-field named shape, so it encodes as
            // the empty object `Unit {}` encodes as.
            StructBody::Unit => {
                classified.push((ty, SerdeShape::Named(Vec::new(), HashSet::new())));
            }
        }
    }

    // A field naming a user struct is emittable only when that struct's own
    // serializer is emitted. Classification answers per type, so the set has
    // to settle: dropping one type can drop the types that reach it, however
    // deep the nesting runs.
    let mut emittable: HashSet<String> =
        classified.iter().map(|(ty, _)| ty.symbol.clone()).collect();
    loop {
        let mut dropped = false;
        for (ty, shape) in &classified {
            if !emittable.contains(&ty.symbol) {
                continue;
            }
            if !shape.kinds().all(|kind| kind_is_emittable(kind, &emittable)) {
                emittable.remove(&ty.symbol);
                dropped = true;
            }
        }
        if !dropped {
            break;
        }
    }

    for (ty, shape) in &classified {
        if !emittable.contains(&ty.symbol) {
            continue;
        }
        match shape {
            SerdeShape::Named(typed, opaque_fields) => {
                emit_impl(&mut out, ty, typed, opaque_fields);
            }
            SerdeShape::Tuple(typed) => emit_tuple_impl(&mut out, ty, typed),
        }
    }
    out
}

/// Emits the serde free functions for a tuple struct: a JSON object keyed
/// by position (`{"0":v0,"1":v1}`), reusing the `to_json`-backed toml/yaml
/// wrappers. Positional access `value.N` and the `Name(..)` constructor are
/// rewritten through the tuple-struct machinery.
fn emit_tuple_impl(out: &mut String, ty: &TyId, fields: &[FieldKind]) {
    emit_tuple_to_json(out, ty, fields);
    emit_tuple_from_json(out, ty, fields);
    emit_to_toml(out, ty);
    emit_from_toml(out, ty);
    emit_to_yaml(out, ty);
    emit_from_yaml(out, ty);
}

fn emit_tuple_to_json(out: &mut String, ty: &TyId, fields: &[FieldKind]) {
    let name = ty.path.as_str();
    out.push_str("// Render a tuple struct as a position-keyed JSON object. Auto-derived.\n");
    out.push_str(&format!(
        "pub fn {}(value: {name}) -> Result<String, errors::Error> {{\n",
        to_json_fn(&ty.symbol)
    ));
    out.push_str("    let mut out = \"\"\n    out += \"{\"\n");
    for (i, kind) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str("    out += \",\"\n");
        }
        out.push_str(&format!("    out += \"\\\"{i}\\\":\"\n"));
        let lit = kind.render_to_json(&format!("value.{i}"));
        out.push_str(&format!("    out += {lit}\n"));
    }
    out.push_str("    out += \"}\"\n    Ok(out)\n}\n\n");
}

fn emit_tuple_from_json(out: &mut String, ty: &TyId, fields: &[FieldKind]) {
    let name = ty.path.as_str();
    out.push_str("// Parse a position-keyed JSON object into a tuple struct. Auto-derived.\n");
    out.push_str(&format!(
        "pub fn {}(text: &String) -> Result<{name}, errors::Error> {{\n",
        from_json_fn(&ty.symbol)
    ));
    out.push_str("    let v = json::parse(text)?\n");
    out.push_str(&format!(
        "    {}(v)\n}}\n\n",
        from_json_value_fn(&ty.symbol)
    ));
    out.push_str(
        "// Parse an already-read JSON node into a tuple struct. Auto-derived.\n",
    );
    out.push_str(&format!(
        "pub fn {}(v: json::Value) -> Result<{name}, errors::Error> {{\n",
        from_json_value_fn(&ty.symbol)
    ));
    for (i, kind) in fields.iter().enumerate() {
        let path = format!("element `{i}`");
        let extract = kind.extract_strict("__child", &path);
        let missing = if kind.tolerates_missing_key() {
            "None".to_string()
        } else {
            format!("return Err(errors::new(\"missing element `{i}`\"))")
        };
        out.push_str(&format!(
            "    let __f{i} = match json::get(v, \"{i}\") {{\n        Some(__child) => {extract},\n        None => {missing},\n    }}\n"
        ));
    }
    let args: Vec<String> = (0..fields.len()).map(|i| format!("__f{i}")).collect();
    out.push_str(&format!(
        "    Ok({}({}))\n}}\n\n",
        ty.bare,
        args.join(", ")
    ));
}

fn emit_impl(
    out: &mut String,
    ty: &TyId,
    fields: &[(String, FieldKind)],
    opaque: &HashSet<String>,
) {
    emit_to_json(out, ty, fields);
    emit_from_json(out, ty, fields, opaque);
    emit_to_toml(out, ty);
    emit_from_toml(out, ty);
    emit_to_yaml(out, ty);
    emit_from_yaml(out, ty);
}

fn emit_to_json(out: &mut String, ty: &TyId, fields: &[(String, FieldKind)]) {
    let name = ty.path.as_str();
    out.push_str(
        "// Render a value as a JSON object. Auto-derived; reached via `to_json::<T>(value)`.\n",
    );
    out.push_str(&format!(
        "pub fn {}(value: {name}) -> Result<String, errors::Error> {{\n",
        to_json_fn(&ty.symbol)
    ));
    out.push_str("    let mut out = \"\"\n");
    out.push_str("    out += \"{\"\n");
    for (i, (fname, kind)) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str("    out += \",\"\n");
        }
        out.push_str(&format!("    out += \"\\\"{fname}\\\":\"\n"));
        let lit = kind.render_to_json(&format!("value.{fname}"));
        out.push_str(&format!("    out += {lit}\n"));
    }
    out.push_str("    out += \"}\"\n");
    out.push_str("    Ok(out)\n");
    out.push_str("}\n\n");
}

fn emit_to_toml(out: &mut String, ty: &TyId) {
    let name = ty.path.as_str();
    out.push_str("// Render a value as TOML. Auto-derived; reached via `to_toml::<T>(value)`.\n");
    out.push_str(&format!(
        "pub fn {}(value: {name}) -> Result<String, errors::Error> {{\n",
        serde_fn("to_toml", &ty.symbol)
    ));
    out.push_str(&format!("    let j = {}(value)?\n", to_json_fn(&ty.symbol)));
    out.push_str("    toml::from_json(j)\n");
    out.push_str("}\n\n");
}

fn emit_from_toml(out: &mut String, ty: &TyId) {
    let name = ty.path.as_str();
    out.push_str(
        "// Parse TOML text into a value. Auto-derived; reached via `from_toml::<T>(text)`.\n",
    );
    out.push_str(&format!(
        "pub fn {}(text: &String) -> Result<{name}, errors::Error> {{\n",
        serde_fn("from_toml", &ty.symbol)
    ));
    out.push_str("    let j = toml::to_json(text)?\n");
    out.push_str(&format!("    {}(&j)\n", from_json_fn(&ty.symbol)));
    out.push_str("}\n\n");
}

fn emit_to_yaml(out: &mut String, ty: &TyId) {
    let name = ty.path.as_str();
    out.push_str("// Render a value as YAML. Auto-derived; reached via `to_yaml::<T>(value)`.\n");
    out.push_str(&format!(
        "pub fn {}(value: {name}) -> Result<String, errors::Error> {{\n",
        serde_fn("to_yaml", &ty.symbol)
    ));
    out.push_str(&format!("    let j = {}(value)?\n", to_json_fn(&ty.symbol)));
    out.push_str("    yaml::from_json(j)\n");
    out.push_str("}\n\n");
}

fn emit_from_yaml(out: &mut String, ty: &TyId) {
    let name = ty.path.as_str();
    out.push_str(
        "// Parse YAML text into a value. Auto-derived; reached via `from_yaml::<T>(text)`.\n",
    );
    out.push_str(&format!(
        "pub fn {}(text: &String) -> Result<{name}, errors::Error> {{\n",
        serde_fn("from_yaml", &ty.symbol)
    ));
    out.push_str("    let j = yaml::to_json(text)?\n");
    out.push_str(&format!("    {}(&j)\n", from_json_fn(&ty.symbol)));
    out.push_str("}\n\n");
}

fn emit_from_json(
    out: &mut String,
    ty: &TyId,
    fields: &[(String, FieldKind)],
    opaque: &HashSet<String>,
) {
    let name = ty.path.as_str();
    out.push_str(
        "// Parse a JSON object into a value. Auto-derived; reached via `from_json::<T>(text)`.\n// Returns `Err` when a required field is missing or a field's value\n// type does not match the declaration; the error names the field.\n",
    );
    out.push_str(&format!(
        "pub fn {}(text: &String) -> Result<{name}, errors::Error> {{\n",
        from_json_fn(&ty.symbol)
    ));
    out.push_str("    let v = json::parse(text)?\n");
    out.push_str(&format!(
        "    {}(v)\n}}\n\n",
        from_json_value_fn(&ty.symbol)
    ));
    out.push_str(
        "// Parse an already-read JSON node into a value. Auto-derived; a\n// nested member decodes from the node the walk stands on.\n",
    );
    out.push_str(&format!(
        "pub fn {}(v: json::Value) -> Result<{name}, errors::Error> {{\n",
        from_json_value_fn(&ty.symbol)
    ));
    // The local a field decodes into is named by position, not by the
    // field. A field is free to carry any name the language allows -
    // including one a compiler-known call already has (`format`,
    // `panic`, `matches`) - and a generated binding named after it would
    // make the compiler emit code that fails its own check, reported
    // against a line of the user's file that does not exist.
    for (index, (fname, kind)) in fields.iter().enumerate() {
        let path = format!("field `{fname}`");
        let extract = kind.extract_strict("__child", &path);
        // A missing `Option` field decodes to `None` rather than erroring.
        let missing = if kind.tolerates_missing_key() {
            "None".to_string()
        } else {
            format!("return Err(errors::new(\"missing field `{fname}`\"))")
        };
        out.push_str(&format!(
            "    let __f{index} = match json::get(v, \"{fname}\") {{\n        Some(__child) => {extract},\n        None => {missing},\n    }}\n"
        ));
    }
    // The extracted local carries the representation; an opaque alias
    // field takes it back across the boundary, and the field's declared
    // type is what fixes `.into()`'s target.
    let values: Vec<String> = fields
        .iter()
        .enumerate()
        .map(|(index, (field, _))| {
            if opaque.contains(field) {
                format!("__f{index}.into()")
            } else {
                format!("__f{index}")
            }
        })
        .collect();
    let fields = fields
        .iter()
        .zip(values.iter())
        .map(|((field, _), value)| (field.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    out.push_str(&format!("    Ok({})\n", named_struct_literal(name, &fields)));
    out.push_str("}\n\n");
}

/// Whether `ty` is written as the bare name of an opaque alias.
fn type_names_opaque_alias(ty: &gossamer_ast::Type, opaque: &HashSet<String>) -> bool {
    let TypeKind::Path(path) = &ty.kind else {
        return false;
    };
    path.segments.len() == 1
        && path.segments[0].generics.is_empty()
        && opaque.contains(&path.segments[0].name.name)
}

fn named_struct_literal(name: &str, fields: &[(&str, &str)]) -> String {
    let parts = fields
        .iter()
        .map(|(field, value)| format!("{field}: {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name} {{ {parts} }}")
}

/// Extracts the trait names listed in an item's `#[derive(...)]`
/// attributes (e.g. `["Clone", "PartialEq"]`). Multiple `#[derive(...)]`
/// attributes accumulate.
fn derive_list(attrs: &gossamer_ast::Attrs) -> Vec<String> {
    let mut out = Vec::new();
    for attr in &attrs.outer {
        let is_derive =
            attr.path.segments.len() == 1 && attr.path.segments[0].name.name == "derive";
        if !is_derive {
            continue;
        }
        if let Some(tokens) = &attr.tokens {
            for tok in tokens.split(',') {
                let name = tok.trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// Head name of a `Type` that is a single-segment path (`Point` ->
/// `"Point"`), used to attach an `impl` block to its target type.
fn type_head_name(ty: &gossamer_ast::Type) -> Option<&str> {
    match &ty.kind {
        TypeKind::Path(path) if path.segments.len() == 1 => {
            Some(path.segments[0].name.name.as_str())
        }
        _ => None,
    }
}

/// The `cmp` lines that order one field of a derived comparison.
///
/// `<` on a field recurses into that type's own `cmp`, which every shape has
/// except `Option`: nothing declares `Option::cmp`, so a type carrying one
/// derived a body naming a function that does not exist. The arms are spelled
/// out here instead, following the language's own rule for an enum - by
/// variant rank, then payload. `Some` is declared first, so it orders before
/// `None`, which is the order a sort of bare `Option`s already gives.
fn derived_cmp_field(ty: &gossamer_ast::Type, mine: &str, theirs: &str) -> String {
    if type_head_name(ty) != Some("Option") {
        return format!(
            "        if {mine} < {theirs} {{ return -1 }}\n        if {theirs} < {mine} {{ return 1 }}\n"
        );
    }
    format!(
        "        if {mine}.is_some() && {theirs}.is_none() {{ return -1 }}\n\
         \x20       if {mine}.is_none() && {theirs}.is_some() {{ return 1 }}\n\
         \x20       if {mine}.is_some() && {theirs}.is_some() {{\n\
         \x20           if {mine}.unwrap() < {theirs}.unwrap() {{ return -1 }}\n\
         \x20           if {theirs}.unwrap() < {mine}.unwrap() {{ return 1 }}\n\
         \x20       }}\n"
    )
}

/// Scalar field types a synthesized `fmt` can render directly via
/// `format!("{}", field)` on every tier.
fn is_scalar_fmt_name(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "String"
    )
}

/// Whether a field type renders inside a synthesized `fmt` on the compiled
/// tiers: a scalar, a struct / enum that itself ends up with a `fmt` (tracked
/// in `formattable`), or a sequence, `Option`, or tuple over those - each
/// renders through the element's own descriptor. A `Map`, a `Set`, a channel,
/// and a function type are excluded, so a type carrying one keeps the
/// runtime's default render and gets no implicit `fmt`.
/// Type-parameter names a declaration introduces.
fn param_name_set(generics: &gossamer_ast::Generics) -> HashSet<String> {
    generics
        .params
        .iter()
        .filter_map(|p| match p {
            gossamer_ast::GenericParam::Type { name, .. } => Some(name.name.clone()),
            _ => None,
        })
        .collect()
}

fn ty_is_renderable(
    ty: &gossamer_ast::Type,
    formattable: &HashSet<String>,
    params: &HashSet<String>,
    aliases: &HashMap<String, gossamer_ast::Type>,
) -> bool {
    ty_is_renderable_within(ty, formattable, params, aliases, 0)
}

/// [`ty_is_renderable`] with `depth` bounding alias expansion, per
/// [`MAX_ALIAS_DEPTH`].
fn ty_is_renderable_within(
    ty: &gossamer_ast::Type,
    formattable: &HashSet<String>,
    params: &HashSet<String>,
    aliases: &HashMap<String, gossamer_ast::Type>,
    depth: u32,
) -> bool {
    match &ty.kind {
        TypeKind::Path(path) if path.segments.len() == 1 => {
            let seg = &path.segments[0];
            let name = seg.name.name.as_str();
            // Whether a field typed by one of the declaration's own
            // parameters renders depends on the argument each instantiation
            // supplies, which is not known here. A generic type asks for its
            // `fmt` with `#[derive(Debug)]`, where the author has the
            // instantiations in view.
            if params.contains(name) {
                return false;
            }
            // `Box` / `Arc` / `Rc` are transparent, so a value inside one
            // renders exactly as the value does. This is what lets a
            // recursive type reach its own `fmt`.
            if matches!(name, "Box" | "Arc" | "Rc") {
                return match seg.generics.as_slice() {
                    [gossamer_ast::GenericArg::Type(inner)] => {
                        ty_is_renderable_within(inner, formattable, params, aliases, depth)
                    }
                    _ => false,
                };
            }
            // A sequence or an `Option` renders element by element, so it
            // renders exactly when its element does.
            if matches!(name, "Vec" | "Option") {
                return match seg.generics.as_slice() {
                    [gossamer_ast::GenericArg::Type(inner)] => {
                        ty_is_renderable_within(inner, formattable, params, aliases, depth)
                    }
                    _ => false,
                };
            }
            // A `Result` renders through whichever arm it holds, so it
            // renders when both of them do.
            if name == "Result" {
                return seg.generics.len() == 2
                    && seg.generics.iter().all(|arg| match arg {
                        gossamer_ast::GenericArg::Type(inner) => {
                            ty_is_renderable_within(inner, formattable, params, aliases, depth)
                        }
                        gossamer_ast::GenericArg::Const(_) => false,
                    });
            }
            // A keyed, ordered, or slot-backed container renders through
            // the runtime, element by element, so it renders exactly when
            // every type it holds does.
            if matches!(
                name,
                "Map"
                    | "BTreeMap"
                    | "Set"
                    | "BTreeSet"
                    | "Deque"
                    | "Queue"
                    | "Stack"
                    | "MaxHeap"
                    | "MinHeap"
            ) {
                return !seg.generics.is_empty()
                    && seg.generics.iter().all(|arg| match arg {
                        gossamer_ast::GenericArg::Type(inner) => {
                            ty_is_renderable_within(inner, formattable, params, aliases, depth)
                        }
                        gossamer_ast::GenericArg::Const(_) => false,
                    });
            }
            if !seg.generics.is_empty() {
                return false;
            }
            if is_scalar_fmt_name(name) || formattable.contains(name) {
                return true;
            }
            // A transparent alias stands for its target, so a field typed by
            // one renders exactly as a field of the target does.
            match aliases.get(name) {
                Some(target) if depth < MAX_ALIAS_DEPTH => {
                    ty_is_renderable_within(target, formattable, params, aliases, depth + 1)
                }
                _ => false,
            }
        }
        TypeKind::Ref { inner, .. } => {
            ty_is_renderable_within(inner, formattable, params, aliases, depth)
        }
        // A slice and a fixed array render like the sequence they are; a
        // tuple renders field by field.
        TypeKind::Slice(inner) | TypeKind::Array { elem: inner, .. } => {
            ty_is_renderable_within(inner, formattable, params, aliases, depth)
        }
        TypeKind::Tuple(elems) => elems
            .iter()
            .all(|e| ty_is_renderable_within(e, formattable, params, aliases, depth)),
        _ => false,
    }
}

/// Type heads whose values carry no meaningful equality / ordering, so a
/// synthesized `self.f == other.f` over a field of one would not typecheck.
/// A struct carrying one of these gets no automatic comparison (comparing it
/// is then a clean check error, never a miscompile).
fn is_noncomparable_head(name: &str) -> bool {
    matches!(
        name,
        "Sender"
            | "Receiver"
            | "Mutex"
            | "RwLock"
            | "JoinHandle"
            | "WaitGroup"
            | "Once"
            | "Context"
            | "AtomicBool"
            | "AtomicI8"
            | "AtomicI16"
            | "AtomicI32"
            | "AtomicI64"
            | "AtomicU8"
            | "AtomicU16"
            | "AtomicU32"
            | "AtomicU64"
            | "AtomicUsize"
            | "AtomicIsize"
    )
}

/// A field type over which a synthesized `eq` and `cmp` are correct and
/// lower identically on every tier: a scalar / `String` leaf, or a nested
/// struct / enum already proven comparable. Containers, tuples, generic
/// parameters, and channel / fn types are deliberately excluded - those need
/// an explicit `#[derive(PartialEq)]` / `#[derive(Ord)]`, which force the
/// synthesis without the by-value guarantee.
fn ty_is_comparable(ty: &gossamer_ast::Type, comparable: &HashSet<String>) -> bool {
    match &ty.kind {
        TypeKind::Ref { inner, .. } => ty_is_comparable(inner, comparable),
        TypeKind::Path(path) => {
            let Some(seg) = path.segments.last() else {
                return false;
            };
            if !seg.generics.is_empty() {
                return false;
            }
            let name = seg.name.name.as_str();
            !is_noncomparable_head(name) && (is_scalar_fmt_name(name) || comparable.contains(name))
        }
        _ => false,
    }
}

/// Like [`ty_is_comparable`] but for ordering (`cmp`), which additionally
/// excludes `bool`: `<` on a `bool` does not lower on the compiled tiers, so a
/// struct carrying a `bool` is equatable (`==`) but not auto-orderable - it
/// gets `eq` but not `cmp`. (Equality on a `bool` field lowers fine.)
fn ty_is_orderable(ty: &gossamer_ast::Type, orderable: &HashSet<String>) -> bool {
    match &ty.kind {
        TypeKind::Ref { inner, .. } => ty_is_orderable(inner, orderable),
        TypeKind::Path(path) => {
            let Some(seg) = path.segments.last() else {
                return false;
            };
            if !seg.generics.is_empty() {
                return false;
            }
            let name = seg.name.name.as_str();
            name != "bool"
                && !is_noncomparable_head(name)
                && (is_scalar_fmt_name(name) || orderable.contains(name))
        }
        _ => false,
    }
}

/// Marker the synthesizer pushes for the `Display` spelling of the
/// structural rendering. `#[derive(..)]` never names it, so it cannot
/// collide with a written attribute.
const IMPLICIT_DISPLAY: &str = "__ImplicitDisplay";

/// Method names the structural rendering is emitted under for this derive
/// set: `fmt` is the `Debug` channel (`{:?}`) and `to_string` the `Display`
/// one (`{}`). Both render the same text.
fn rendering_method_names(derives: &[String]) -> Vec<&'static str> {
    let mut names = Vec::new();
    if derives.iter().any(|d| d == "Debug") {
        names.push("fmt");
    }
    if derives.iter().any(|d| d == IMPLICIT_DISPLAY) {
        names.push("to_string");
    }
    names
}

/// Placeholder a synthesized rendering uses for one field of type `ty` on
/// `method`'s channel.
///
/// A scalar keeps `{:?}` on both channels, which is what makes a float field
/// carry its fractional part and a `char` its quoting. A struct or enum field
/// shows through its own rendering for the channel, so `{}` reaches the
/// field's `impl Display` and `{:?}` its `impl Debug`.
fn field_placeholder(
    method: &str,
    ty: &gossamer_ast::Type,
    aliases: &HashMap<String, gossamer_ast::Type>,
) -> &'static str {
    if method == "fmt" || ty_head_is_scalar(ty, aliases, 0) {
        "{:?}"
    } else {
        "{}"
    }
}

/// Whether `ty`'s head names a scalar, expanding transparent aliases so a
/// `type Meters = f64` field reads as the `f64` it stands for. `depth`
/// bounds that expansion, per [`MAX_ALIAS_DEPTH`].
fn ty_head_is_scalar(
    ty: &gossamer_ast::Type,
    aliases: &HashMap<String, gossamer_ast::Type>,
    depth: u32,
) -> bool {
    let Some(name) = type_head_name(ty) else {
        return false;
    };
    if is_scalar_fmt_name(name) {
        return true;
    }
    match aliases.get(name) {
        Some(target) if depth < MAX_ALIAS_DEPTH => ty_head_is_scalar(target, aliases, depth + 1),
        _ => false,
    }
}

/// Types for which the user already wrote a method named `method` (in an
/// inherent or trait `impl`), so the synthesizer must not emit a conflicting
/// structural one.
fn types_with_user_method(parsed: &SourceFile, method: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for item in flatten_items(&parsed.items) {
        if let ItemKind::Impl(decl) = &item.kind
            && let Some(name) = type_head_name(&decl.self_ty)
            && decl
                .items
                .iter()
                .any(|i| matches!(i, gossamer_ast::ImplItem::Fn(f) if f.name.name == method))
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// Trait an `impl` header names, if it names one.
fn impl_trait_head(decl: &gossamer_ast::ImplDecl) -> Option<&str> {
    decl.trait_ref
        .as_ref()
        .and_then(|bound| bound.path.segments.last())
        .map(|segment| segment.name.name.as_str())
}

/// Types whose `{}` rendering the source already supplies. Both rendering
/// contracts declare `fn fmt`, so the `impl` header is what tells the two
/// apart: a `fmt` under `impl Display for T` is the `Display` channel, and
/// a `to_string` in any block is the same channel under the name every
/// caller reaches it by.
fn types_with_user_display(parsed: &SourceFile) -> HashSet<String> {
    let mut out = types_with_user_method(parsed, "to_string");
    for item in flatten_items(&parsed.items) {
        if let ItemKind::Impl(decl) = &item.kind
            && impl_trait_head(decl) == Some("Display")
            && let Some(name) = type_head_name(&decl.self_ty)
            && decl
                .items
                .iter()
                .any(|i| matches!(i, gossamer_ast::ImplItem::Fn(f) if f.name.name == "fmt"))
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// Types whose `{:?}` rendering the source already supplies: a `fmt` in
/// any block other than the one that names `Display`.
fn types_with_user_debug(parsed: &SourceFile) -> HashSet<String> {
    let mut out = HashSet::new();
    for item in flatten_items(&parsed.items) {
        if let ItemKind::Impl(decl) = &item.kind
            && impl_trait_head(decl) != Some("Display")
            && let Some(name) = type_head_name(&decl.self_ty)
            && decl
                .items
                .iter()
                .any(|i| matches!(i, gossamer_ast::ImplItem::Fn(f) if f.name.name == "fmt"))
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// Synthesizes `impl` blocks for the `#[derive(...)]` traits, plus a
/// structural `fmt` for every struct / enum that is formattable but has no
/// `fmt` of its own, so `{}` / `{:?}` lowers on the compiled tiers exactly as
/// it renders on the VM. Returns the appended source.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "linear orchestration: collect names, fields, formattable + comparable sets, then emit"
)]
pub fn synthesize_derive_impls(parsed: &SourceFile) -> String {
    let struct_names: HashMap<String, TyId> = struct_identities(&parsed.items);
    let aliases = alias_targets(&parsed.items);
    let user_fmt = types_with_user_debug(parsed);
    let user_to_string = types_with_user_display(parsed);
    let user_eq = types_with_user_method(parsed, "eq");
    let user_cmp = types_with_user_method(parsed, "cmp");

    // Field types per struct / enum, used to grow the `formattable` set,
    // paired with the declaration's own type-parameter names.
    let mut field_tys: HashMap<String, Vec<&gossamer_ast::Type>> = HashMap::new();
    let mut type_params: HashMap<String, HashSet<String>> = HashMap::new();
    for item in flatten_items(&parsed.items) {
        match &item.kind {
            ItemKind::Struct(decl) => {
                let tys: Vec<&gossamer_ast::Type> = match &decl.body {
                    StructBody::Named(fields) => fields.iter().map(|f| &f.ty).collect(),
                    StructBody::Tuple(fields) => fields.iter().map(|f| &f.ty).collect(),
                    StructBody::Unit => Vec::new(),
                };
                field_tys.insert(decl.name.name.clone(), tys);
                type_params.insert(decl.name.name.clone(), param_name_set(&decl.generics));
            }
            ItemKind::Enum(decl) => {
                field_tys.insert(
                    decl.name.name.clone(),
                    decl.variants.iter().flat_map(variant_fields).collect(),
                );
                type_params.insert(decl.name.name.clone(), param_name_set(&decl.generics));
            }
            _ => {}
        }
    }
    // A type ends up with a `fmt` if the user wrote one or a `#[derive(Debug)]`
    // requests one; seed the formattable set with those, then grow it to the
    // fixpoint of types whose every field is a scalar or an already-formattable
    // type. A struct/enum reaches a `fmt` only if all its fields actually
    // render - so a field referencing a non-formattable type (or a container)
    // never produces a `format!("{}", field)` the compiled tiers cannot lower.
    let mut formattable: HashSet<String> = HashSet::new();
    for item in flatten_items(&parsed.items) {
        let derives = derive_list(&item.attrs);
        let name = match &item.kind {
            ItemKind::Struct(d)
                if matches!(&d.body, StructBody::Named(_) | StructBody::Tuple(_)) =>
            {
                Some(&d.name.name)
            }
            ItemKind::Enum(d) => Some(&d.name.name),
            _ => None,
        };
        if let Some(n) = name
            && (derives.iter().any(|d| d == "Debug")
                || user_fmt.contains(n)
                || user_to_string.contains(n))
        {
            formattable.insert(n.clone());
        }
    }
    // A type reaches its own `fmt` through the cycle it sits in, and the
    // cycle is as often between two declarations as within one: an
    // expression that holds a query and a query that holds expressions each
    // render only if the other does. Admitting one name at a time, with only
    // that name assumed, settles a self-reference and deadlocks a mutual one
    // - each half waits for the half that is waiting for it. So every
    // candidate starts admitted and the pass takes away the ones a field
    // disproves, which reaches the same answer for a self-cycle and the right
    // one for a cycle of any width.
    let mut candidates: HashSet<String> = field_tys.keys().cloned().collect();
    for name in &formattable {
        candidates.remove(name);
    }
    loop {
        let mut reachable = formattable.clone();
        reachable.extend(candidates.iter().cloned());
        let disproven: Vec<String> = candidates
            .iter()
            .filter(|name| {
                let params = type_params.get(*name).cloned().unwrap_or_default();
                !field_tys.get(*name).is_some_and(|tys| {
                    tys.iter()
                        .all(|ty| ty_is_renderable(ty, &reachable, &params, &aliases))
                })
            })
            .cloned()
            .collect();
        if disproven.is_empty() {
            break;
        }
        for name in disproven {
            candidates.remove(&name);
        }
    }
    formattable.extend(candidates);

    // Grow the set of structs / enums that compare by value structurally on
    // every tier: a type is comparable once every field is a scalar / String
    // or an already-comparable nested type. This drives automatic `eq` / `cmp`
    // synthesis, so `==` / `<` work on a plain `struct Point { x, y }` with no
    // `#[derive(...)]` - exactly as they already do on tuples.
    let mut comparable: HashSet<String> = HashSet::new();
    loop {
        let mut changed = false;
        for (name, tys) in &field_tys {
            if comparable.contains(name) {
                continue;
            }
            if tys.iter().all(|ty| ty_is_comparable(ty, &comparable)) {
                comparable.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Orderable types (drives `cmp`): comparable, minus any `bool` field, since
    // `<` on a `bool` does not lower on the compiled tiers. A bool-bearing type
    // is still in `comparable` (it gets `eq`), just not here.
    let mut orderable: HashSet<String> = HashSet::new();
    loop {
        let mut changed = false;
        for (name, tys) in &field_tys {
            if orderable.contains(name) {
                continue;
            }
            if tys.iter().all(|ty| ty_is_orderable(ty, &orderable)) {
                orderable.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut out = String::new();
    for (module, item) in flatten_items_with_modules(&parsed.items) {
        let mut derives = derive_list(&item.attrs);
        // Synthesize a structural `fmt` for every formattable struct / enum that
        // lacks one, so `{}` / `{:?}` lowers on the compiled tiers exactly as it
        // renders on the VM.
        let implicit_target = match &item.kind {
            ItemKind::Struct(d) => Some(&d.name.name),
            ItemKind::Enum(d) => Some(&d.name.name),
            _ => None,
        };
        if let Some(tn) = implicit_target
            && formattable.contains(tn)
            && !derives.iter().any(|d| d == "Debug")
        {
            derives.push("Debug".to_string());
        }
        // `Display` and `Debug` are separate channels: `{}` reaches
        // `to_string` and `{:?}` reaches `fmt`. A type with no `impl Display`
        // still shows through `{}`, so it carries the structural rendering
        // under that spelling too.
        if let Some(tn) = implicit_target
            && formattable.contains(tn)
        {
            derives.push(IMPLICIT_DISPLAY.to_string());
        }
        // A method the source already supplies is never re-emitted, whatever
        // a `#[derive(..)]` asks for: one name, one body.
        if let Some(tn) = implicit_target {
            if user_fmt.contains(tn) {
                derives.retain(|d| d != "Debug");
            }
            if user_to_string.contains(tn) {
                derives.retain(|d| d != IMPLICIT_DISPLAY);
            }
        }
        // Synthesize `eq` / `cmp` for every by-value-comparable struct / enum
        // that has no user-written one, so structural `==` and `<` work with no
        // `#[derive(...)]`. The synthesized methods key off the same `PartialEq`
        // / `Ord` markers an explicit derive uses, so the two paths never
        // double-emit.
        if let Some(tn) = implicit_target {
            if comparable.contains(tn)
                && !user_eq.contains(tn)
                && !derives.iter().any(|d| d == "PartialEq" || d == "Eq")
            {
                derives.push("PartialEq".to_string());
            }
            if orderable.contains(tn)
                && !user_cmp.contains(tn)
                && !derives.iter().any(|d| d == "Ord" || d == "PartialOrd")
            {
                derives.push("Ord".to_string());
            }
        }
        if derives.is_empty() {
            continue;
        }
        match &item.kind {
            ItemKind::Struct(decl) => match &decl.body {
                StructBody::Named(fields) => {
                    let ty = TyId::new(&module, &decl.name.name);
                    emit_struct_derive_impl(
                        &mut out, decl, &ty, fields, &derives, &struct_names, &aliases,
                    );
                }
                StructBody::Tuple(fields) => {
                    let ty = TyId::new(&module, &decl.name.name);
                    emit_tuple_struct_derive_impl(
                        &mut out,
                        decl,
                        &ty,
                        fields,
                        &derives,
                        &struct_names,
                        &aliases,
                    );
                }
                // A unit struct carries no fields, so it takes the same
                // derived bodies an empty named struct takes: every pair of
                // values is equal, and every rendering is the bare shape.
                StructBody::Unit => {
                    let ty = TyId::new(&module, &decl.name.name);
                    emit_struct_derive_impl(
                        &mut out, decl, &ty, &[], &derives, &struct_names, &aliases,
                    );
                }
            },
            ItemKind::Enum(decl) => {
                let ty = TyId::new(&module, &decl.name.name);
                emit_enum_derive_impl(&mut out, decl, &ty, &derives, &aliases);
            }
            _ => {}
        }
    }
    emit_comparators(&mut out, parsed, &user_cmp, &orderable);
    out
}

/// Emits one comparator per ordered type, under the prefix that says
/// whether the source wrote the order or the compiler synthesized it.
fn emit_comparators(
    out: &mut String,
    parsed: &SourceFile,
    user_cmp: &HashSet<String>,
    orderable: &HashSet<String>,
) {
    let searches = searches_sorted_sequences(parsed);
    for (module, item) in flatten_items_with_modules(&parsed.items) {
        let (name, generics) = match &item.kind {
            ItemKind::Struct(decl) => (&decl.name.name, &decl.generics),
            ItemKind::Enum(decl) => (&decl.name.name, &decl.generics),
            _ => continue,
        };
        let prefix = if user_cmp.contains(name) {
            USER_COMPARATOR_PREFIX
        } else if orderable.contains(name) {
            STRUCTURAL_COMPARATOR_PREFIX
        } else {
            continue;
        };
        let ty = TyId::new(&module, name);
        let params: Vec<&str> = generics
            .params
            .iter()
            .filter_map(|p| match p {
                gossamer_ast::GenericParam::Type { name, .. } => Some(name.name.as_str()),
                _ => None,
            })
            .collect();
        let (gen_decl, self_ty) = if params.is_empty() {
            (String::new(), ty.path.clone())
        } else {
            let args = format!("<{}>", params.join(", "));
            (args.clone(), format!("{}{args}", ty.path))
        };
        out.push_str(&format!(
            "// Comparator for {}, reaching the order the type carries.\n\
             fn {prefix}{}{gen_decl}(a: {self_ty}, b: {self_ty}) -> i64 {{ a.cmp(b) }}\n",
            ty.path, ty.symbol,
        ));
        // A search over a sorted sequence compares against a target rather
        // than ranking a pair, so it needs its own body. Monomorphic, since
        // the ordering call names it directly, and emitted only where the
        // program searches at all.
        if searches && params.is_empty() {
            let cmp = format!("{prefix}{}", ty.symbol);
            out.push_str(&format!(
                "\
fn {PARTITION_POINT_PREFIX}{sym}(xs: Vec<{self_ty}>, pivot: {self_ty}) -> i64 {{
    let mut lo = 0
    let mut hi = xs.len()
    while lo < hi {{
        let mid = (lo + hi) / 2
        if {cmp}(xs[mid], pivot) < 0 {{ lo = mid + 1 }} else {{ hi = mid }}
    }}
    lo
}}

fn {BINARY_SEARCH_PREFIX}{sym}(xs: Vec<{self_ty}>, target: {self_ty}) -> Option<i64> {{
    let at = {PARTITION_POINT_PREFIX}{sym}(xs, target)
    if at < xs.len() && {cmp}(xs[at], target) == 0 {{ Some(at) }} else {{ None }}
}}
",
                sym = ty.symbol,
            ));
        }
    }
}

/// True when the source searches a sorted sequence, so the per-type search
/// bodies are worth emitting.
fn searches_sorted_sequences(parsed: &SourceFile) -> bool {
    struct Search {
        found: bool,
    }
    impl gossamer_ast::visitor::Visitor for Search {
        fn visit_expr(&mut self, expr: &gossamer_ast::Expr) {
            if let gossamer_ast::ExprKind::Call { callee, .. } = &expr.kind
                && let gossamer_ast::ExprKind::Path(path) = &callee.kind
                && path.segments.last().is_some_and(|s| {
                    matches!(s.name.name.as_str(), "binary_search" | "partition_point")
                })
            {
                self.found = true;
            }
            gossamer_ast::visitor::walk_expr(self, expr);
        }
    }
    let mut search = Search { found: false };
    for item in &parsed.items {
        gossamer_ast::visitor::walk_item(&mut search, item);
    }
    search.found
}



/// Iterator over the payload field types of an enum variant (empty for unit
/// variants), for the implicit-`fmt` formattability check.
fn variant_fields(v: &EnumVariant) -> impl Iterator<Item = &gossamer_ast::Type> {
    let tys: Vec<&gossamer_ast::Type> = match &v.body {
        StructBody::Unit => Vec::new(),
        StructBody::Tuple(fields) => fields.iter().map(|f| &f.ty).collect(),
        StructBody::Named(fields) => fields.iter().map(|f| &f.ty).collect(),
    };
    tys.into_iter()
}

/// The match pattern and the value-reconstruction for one enum variant,
/// binding each payload field to `{prefix}{i}` - e.g. for `V(a, b)` with prefix
/// `__s`: `("E::V(__s0, __s1)", "E::V(__s0, __s1)", ["__s0", "__s1"])`.
/// `(match pattern, construction expression, bindings)` for one variant.
///
/// A pattern names the type through the module path so it resolves from the
/// unit root where these bodies are spliced; a construction spells the type
/// as declared, which is the name every tier's constructor dispatch carries.
fn variant_shape(ty: &TyId, v: &EnumVariant, prefix: &str) -> (String, String, Vec<String>) {
    let vn = &v.name.name;
    let enum_name = ty.path.as_str();
    let ctor_name = ty.bare.as_str();
    match &v.body {
        StructBody::Unit => (
            format!("{enum_name}::{vn}"),
            format!("{ctor_name}::{vn}"),
            Vec::new(),
        ),
        StructBody::Tuple(fields) => {
            let binds: Vec<String> = (0..fields.len()).map(|i| format!("{prefix}{i}")).collect();
            let joined = binds.join(", ");
            (
                format!("{enum_name}::{vn}({joined})"),
                format!("{ctor_name}::{vn}({joined})"),
                binds,
            )
        }
        StructBody::Named(fields) => {
            let binds: Vec<String> = (0..fields.len()).map(|i| format!("{prefix}{i}")).collect();
            let pat: Vec<String> = fields
                .iter()
                .zip(&binds)
                .map(|(f, b)| format!("{}: {b}", f.name.name))
                .collect();
            (
                format!("{enum_name}::{vn} {{ {} }}", pat.join(", ")),
                format!("{ctor_name}::{vn} {{ {} }}", pat.join(", ")),
                binds,
            )
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one block per derived trait (clone/eq/cmp/debug/default); splitting scatters the emit"
)]
fn emit_enum_derive_impl(
    out: &mut String,
    decl: &EnumDecl,
    ty: &TyId,
    derives: &[String],
    aliases: &HashMap<String, gossamer_ast::Type>,
) {
    let name = ty.path.as_str();
    let has = |t: &str| derives.iter().any(|d| d == t);
    let want_clone = has("Clone");
    let want_eq = has("PartialEq") || has("Eq");
    let want_cmp = has("PartialOrd") || has("Ord");
    let want_default = has("Default");
    let render_methods = rendering_method_names(derives);
    let want_rendering = !render_methods.is_empty();
    if !(want_clone || want_eq || want_cmp || want_default || want_rendering) {
        return;
    }
    // A generic enum's derived methods live on `impl<T> Name<T>`, the same
    // shape a generic struct's do. Emitted without the parameters, the
    // methods would not be found on any instantiation.
    let (gen_decl, self_ty) = enum_generics(decl, name);
    out.push_str(&format!(
        "// Auto-derived from #[derive(...)] for {name}.\n#[gos_synthesized]\nimpl{gen_decl} {self_ty} {{\n"
    ));
    let name = self_ty.as_str();
    if want_clone {
        out.push_str(&format!(
            "    fn clone(&self) -> {name} {{\n        match self {{\n"
        ));
        for v in &decl.variants {
            let (pat, recon, _) = variant_shape(ty, v, "__c");
            out.push_str(&format!("            {pat} => {recon},\n"));
        }
        out.push_str("        }\n    }\n");
    }
    if want_eq {
        // Nested single matches (a tuple `match (self, other)` over enum
        // variant patterns isn't reliably matched): match `self`'s variant,
        // then match `other` against the same variant inside the arm.
        out.push_str(&format!(
            "    fn eq(&self, other: &{name}) -> bool {{\n        match self {{\n"
        ));
        for v in &decl.variants {
            let (lpat, _, lbinds) = variant_shape(ty, v, "__a");
            let (rpat, _, rbinds) = variant_shape(ty, v, "__b");
            let cond = if lbinds.is_empty() {
                "true".to_string()
            } else {
                lbinds
                    .iter()
                    .zip(&rbinds)
                    .map(|(a, b)| format!("{a} == {b}"))
                    .collect::<Vec<_>>()
                    .join(" && ")
            };
            out.push_str(&format!(
                "            {lpat} => match other {{ {rpat} => {cond}, _ => false }},\n"
            ));
        }
        out.push_str("        }\n    }\n");
    }
    if want_cmp {
        // Order by variant declaration position first (rank), then compare
        // payloads of a same-rank pair lexicographically. Returns -1 / 0 / 1;
        // the operator routing tests `Type::cmp(a, b) <op> 0`.
        out.push_str(&format!("    fn cmp(&self, other: &{name}) -> i64 {{\n"));
        for (side, var) in [("self", "__rs"), ("other", "__ro")] {
            out.push_str(&format!("        let {var} = match {side} {{\n"));
            for (i, v) in decl.variants.iter().enumerate() {
                let vn = &v.name.name;
                let pat = match &v.body {
                    StructBody::Unit => format!("{name}::{vn}"),
                    StructBody::Tuple(fields) => {
                        let wilds = vec!["_"; fields.len()].join(", ");
                        format!("{name}::{vn}({wilds})")
                    }
                    StructBody::Named(_) => format!("{name}::{vn} {{ .. }}"),
                };
                out.push_str(&format!("            {pat} => {i},\n"));
            }
            out.push_str("        }\n");
        }
        out.push_str("        if __rs < __ro { return -1 }\n        if __rs > __ro { return 1 }\n");
        out.push_str("        match self {\n");
        for v in &decl.variants {
            let (lpat, _, lbinds) = variant_shape(ty, v, "__a");
            let (rpat, _, rbinds) = variant_shape(ty, v, "__b");
            if lbinds.is_empty() {
                out.push_str(&format!("            {lpat} => 0,\n"));
            } else {
                let mut body = String::new();
                for (a, b) in lbinds.iter().zip(&rbinds) {
                    body.push_str(&format!(
                        "if {a} < {b} {{ return -1 }}\n                if {b} < {a} {{ return 1 }}\n                "
                    ));
                }
                body.push('0');
                out.push_str(&format!(
                    "            {lpat} => match other {{\n                {rpat} => {{\n                {body}\n                }},\n                _ => 0,\n            }},\n"
                ));
            }
        }
        out.push_str("        }\n    }\n");
    }
    for render_method in &render_methods {
        out.push_str(&format!(
            "    fn {render_method}(&self) -> String {{\n        match self {{\n"
        ));
        for v in &decl.variants {
            let (pat, _, binds) = variant_shape(ty, v, "__d");
            let vn = &v.name.name;
            let arm = match &v.body {
                StructBody::Unit => format!("\"{vn}\""),
                StructBody::Tuple(payload) => {
                    let holes = payload
                        .iter()
                        .map(|f| field_placeholder(render_method, &f.ty, aliases))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("format(\"{vn}({holes})\", {})", binds.join(", "))
                }
                StructBody::Named(fields) => {
                    let parts: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            format!(
                                "{}: {}",
                                f.name.name,
                                field_placeholder(render_method, &f.ty, aliases)
                            )
                        })
                        .collect();
                    format!(
                        "format(\"{vn} {{{{ {} }}}}\", {})",
                        parts.join(", "),
                        binds.join(", ")
                    )
                }
            };
            out.push_str(&format!("            {pat} => {arm},\n"));
        }
        out.push_str("        }\n    }\n");
    }
    if want_default {
        // Rust requires `#[default]` on exactly one (unit) variant.
        let default_variant = decl.variants.iter().find(|v| {
            v.attrs
                .outer
                .iter()
                .any(|a| a.path.segments.len() == 1 && a.path.segments[0].name.name == "default")
        });
        if let Some(v) = default_variant {
            if matches!(v.body, StructBody::Unit) {
                out.push_str(&format!(
                    "    fn default() -> {name} {{ {name}::{} }}\n",
                    v.name.name
                ));
            }
        }
    }
    out.push_str("}\n\n");
}

fn emit_named_struct_fmt_impl(
    out: &mut String,
    name: &str,
    method: &str,
    field_names: &[&str],
    fields: &[gossamer_ast::StructField],
    aliases: &HashMap<String, gossamer_ast::Type>,
) {
    let mut tmpl = String::new();
    tmpl.push_str(name);
    tmpl.push_str(" {{ ");
    for (i, f) in field_names.iter().enumerate() {
        if i > 0 {
            tmpl.push_str(", ");
        }
        tmpl.push_str(f);
        tmpl.push_str(": ");
        tmpl.push_str(fields.get(i).map_or("{:?}", |field| {
            field_placeholder(method, &field.ty, aliases)
        }));
    }
    tmpl.push_str(" }}");
    let argvals: Vec<String> = fields
        .iter()
        .map(|f| {
            if type_head_name(&f.ty) == Some("String") {
                format!("__gos_strconv_quote(self.{})", f.name.name)
            } else {
                format!("self.{}", f.name.name)
            }
        })
        .collect();
    if field_names.is_empty() {
        out.push_str(&format!(
            "    fn {method}(&self) -> String {{ format(\"{tmpl}\") }}\n"
        ));
    } else {
        out.push_str(&format!(
            "    fn {method}(&self) -> String {{ format(\"{tmpl}\", {}) }}\n",
            argvals.join(", ")
        ));
    }
}

/// `("<T, U>", "Name<T, U>")` for a generic struct, or `("", "Name")` for a
/// non-generic one. Lifetime / const params are skipped (rare in derives).
/// `(<T, U>, Name<T, U>)` for a generic enum, or `("", Name)` when it has no
/// parameters - the declaration and self-type spellings a derived `impl`
/// needs.
fn enum_generics(decl: &EnumDecl, qualified: &str) -> (String, String) {
    let names: Vec<&str> = decl
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            gossamer_ast::GenericParam::Type { name, .. } => Some(name.name.as_str()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        (String::new(), qualified.to_string())
    } else {
        let args = format!("<{}>", names.join(", "));
        (args.clone(), format!("{qualified}{args}"))
    }
}

fn struct_generics(decl: &StructDecl, qualified: &str) -> (String, String) {
    let names: Vec<&str> = decl
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            gossamer_ast::GenericParam::Type { name, .. } => Some(name.name.as_str()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        (String::new(), qualified.to_string())
    } else {
        let args = format!("<{}>", names.join(", "));
        (args.clone(), format!("{qualified}{args}"))
    }
}

/// Emits `Clone` / `PartialEq` / `Default` / `Debug` impls for a tuple
/// struct, using positional access `self.N` and positional construction
/// `Name(..)` (rewritten to the struct-literal form by
/// `rewrite_tuple_struct_ctors`). Debug renders `Name(v0, v1)`.
#[allow(
    clippy::too_many_lines,
    reason = "one block per derived trait; splitting scatters the emit"
)]
fn emit_tuple_struct_derive_impl(
    out: &mut String,
    decl: &StructDecl,
    ty: &TyId,
    fields: &[gossamer_ast::TupleField],
    derives: &[String],
    structs: &HashMap<String, TyId>,
    aliases: &HashMap<String, gossamer_ast::Type>,
) {
    let name = ty.path.as_str();
    // `{:?}` renders the type the user declared, not the path used to
    // reach it from the unit root.
    let bare = decl.name.name.as_str();
    let has = |t: &str| derives.iter().any(|d| d == t);
    let want_clone = has("Clone");
    let want_eq = has("PartialEq") || has("Eq");
    let want_cmp = has("PartialOrd") || has("Ord");
    let want_default = has("Default");
    let render_methods = rendering_method_names(derives);
    let want_rendering = !render_methods.is_empty();
    if !(want_clone || want_eq || want_cmp || want_default || want_rendering) {
        return;
    }
    let (gen_decl, self_ty) = struct_generics(decl, name);
    let n = fields.len();
    out.push_str(&format!(
        "// Auto-derived from #[derive(...)] for {name}.\n#[gos_synthesized]\nimpl{gen_decl} {self_ty} {{\n"
    ));
    if want_clone {
        let init: Vec<String> = (0..n).map(|i| format!("self.{i}")).collect();
        out.push_str(&format!(
            "    fn clone(&self) -> {self_ty} {{ {bare}({}) }}\n",
            init.join(", ")
        ));
    }
    if want_eq {
        if n == 0 {
            out.push_str(&format!(
                "    fn eq(&self, other: &{self_ty}) -> bool {{ true }}\n"
            ));
        } else {
            let conds: Vec<String> = (0..n).map(|i| format!("self.{i} == other.{i}")).collect();
            out.push_str(&format!(
                "    fn eq(&self, other: &{self_ty}) -> bool {{ {} }}\n",
                conds.join(" && ")
            ));
        }
    }
    if want_cmp {
        out.push_str(&format!("    fn cmp(&self, other: &{self_ty}) -> i64 {{\n"));
        for (i, field) in fields.iter().enumerate() {
            out.push_str(&derived_cmp_field(
                &field.ty,
                &format!("self.{i}"),
                &format!("other.{i}"),
            ));
        }
        out.push_str("        0\n    }\n");
    }
    if want_default {
        let typed: Option<Vec<FieldKind>> = fields
            .iter()
            .map(|f| FieldKind::from_type(&f.ty, structs, aliases))
            .collect();
        if let Some(typed) = typed {
            let init: Vec<String> = typed.iter().map(FieldKind::default_literal).collect();
            out.push_str(&format!(
                "    fn default() -> {self_ty} {{ {bare}({}) }}\n",
                init.join(", ")
            ));
        }
    }
    for render_method in &render_methods {
        let placeholders: Vec<&str> = fields
            .iter()
            .map(|f| field_placeholder(render_method, &f.ty, aliases))
            .collect();
        let argvals: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if type_head_name(&f.ty) == Some("String") {
                    format!("__gos_strconv_quote(self.{i})")
                } else {
                    format!("self.{i}")
                }
            })
            .collect();
        out.push_str(&format!(
            "    fn {render_method}(&self) -> String {{ format(\"{bare}({})\", {}) }}\n",
            placeholders.join(", "),
            argvals.join(", ")
        ));
    }
    out.push_str("}\n");
}

fn emit_struct_derive_impl(
    out: &mut String,
    decl: &StructDecl,
    ty: &TyId,
    fields: &[gossamer_ast::StructField],
    derives: &[String],
    structs: &HashMap<String, TyId>,
    aliases: &HashMap<String, gossamer_ast::Type>,
) {
    let name = ty.path.as_str();
    let has = |t: &str| derives.iter().any(|d| d == t);
    let want_clone = has("Clone");
    let want_eq = has("PartialEq") || has("Eq");
    let want_cmp = has("PartialOrd") || has("Ord");
    let want_default = has("Default");
    let render_methods = rendering_method_names(derives);
    let want_rendering = !render_methods.is_empty();
    if !(want_clone || want_eq || want_cmp || want_default || want_rendering) {
        return;
    }
    // `(gen_decl, self_ty)` = ("<T>", "Pair<T>") for a generic struct, else
    // ("", "Pair"). Named structs reconstruct with braced literals.
    let (gen_decl, self_ty) = struct_generics(decl, name);
    let field_names: Vec<&str> = fields.iter().map(|f| f.name.name.as_str()).collect();
    out.push_str(&format!(
        "// Auto-derived from #[derive(...)] for {name}.\n#[gos_synthesized]\nimpl{gen_decl} {self_ty} {{\n"
    ));
    if want_clone {
        // Reconstruct with a field-by-field copy. In the GC model a value
        // struct's fields are shared by copy; this avoids a per-field
        // `.clone()` call (which the VM's name-global method dispatch would
        // misroute back to `Type::clone`).
        let init = field_names
            .iter()
            .map(|field| (*field, format!("self.{field}")))
            .collect::<Vec<_>>();
        let init_refs = init
            .iter()
            .map(|(field, value)| (*field, value.as_str()))
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "    fn clone(&self) -> {self_ty} {{ {} }}\n",
            named_struct_literal(name, &init_refs)
        ));
    }
    if want_eq {
        if field_names.is_empty() {
            out.push_str(&format!(
                "    fn eq(&self, other: &{self_ty}) -> bool {{ true }}\n"
            ));
        } else {
            let conds: Vec<String> = field_names
                .iter()
                .map(|f| format!("self.{f} == other.{f}"))
                .collect();
            out.push_str(&format!(
                "    fn eq(&self, other: &{self_ty}) -> bool {{ {} }}\n",
                conds.join(" && ")
            ));
        }
    }
    if want_cmp {
        // Lexicographic field-by-field ordering returning -1 / 0 / 1; the
        // operator routing tests `Type::cmp(a, b) <op> 0`. Each `<` recurses:
        // scalars / String compare natively, a nested struct routes to its own
        // `cmp`.
        out.push_str(&format!("    fn cmp(&self, other: &{self_ty}) -> i64 {{\n"));
        for (f, field) in field_names.iter().zip(fields.iter()) {
            out.push_str(&derived_cmp_field(
                &field.ty,
                &format!("self.{f}"),
                &format!("other.{f}"),
            ));
        }
        out.push_str("        0\n    }\n");
    }
    if want_default {
        // Per-field default literal needs each field classified; if any
        // field type is outside the supported set, skip Default rather
        // than emit code that won't compile.
        let typed: Option<Vec<(String, FieldKind)>> = fields
            .iter()
            .map(|f| FieldKind::from_type(&f.ty, structs, aliases).map(|k| (f.name.name.clone(), k)))
            .collect();
        if let Some(typed) = typed {
            let init: Vec<(String, String)> = typed
                .iter()
                .map(|(field, k)| (field.clone(), k.default_literal()))
                .collect();
            let init_refs = init
                .iter()
                .map(|(field, value)| (field.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            out.push_str(&format!(
                "    fn default() -> {self_ty} {{ {} }}\n",
                named_struct_literal(name, &init_refs)
            ));
        }
    }
    for render_method in &render_methods {
        emit_named_struct_fmt_impl(
            out,
            &decl.name.name,
            render_method,
            &field_names,
            fields,
            aliases,
        );
    }
    out.push_str("}\n\n");
}
