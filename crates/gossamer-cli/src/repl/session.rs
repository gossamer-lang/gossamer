//! The session index: what the declarations typed so far define, for `%info` and `%explain`.

use super::{BTreeMap, receiver_method_name, symbol_query_matches};

/// Folds the session's facts about a type into the catalog's rendering
/// of it, under the one header both would otherwise print.
///
/// The catalog's leading entry is its header line plus the indented
/// lines describing it; the session's sections belong directly after
/// that, ahead of the method list.
pub(super) fn splice_session_into_catalog(session: &str, catalog: &str) -> String {
    let mut session_lines = session.lines();
    let session_header = session_lines.next().unwrap_or_default();
    let session_body: Vec<&str> = session_lines.collect();
    let mut catalog_lines = catalog.lines();
    let Some(catalog_header) = catalog_lines.next() else {
        return session.to_string();
    };
    if catalog_header != session_header {
        return format!("{session}\n{catalog}");
    }
    let rest: Vec<&str> = catalog_lines.collect();
    let lead = rest
        .iter()
        .take_while(|line| line.starts_with("    "))
        .count();
    let mut out = vec![catalog_header];
    out.extend_from_slice(&rest[..lead]);
    out.extend(session_body);
    out.extend_from_slice(&rest[lead..]);
    out.join("\n")
}

/// `true` when a catalog lookup found no entries. The catalog reports a
/// miss as ordinary text rather than an error, so a caller merging its
/// result has to recognise the sentence.
pub(super) fn is_nothing_found(text: &str) -> bool {
    text.trim_start().starts_with("nothing found for")
}

/// The bare type name inside a rendered type, so `&mut Point` and
/// `Vec<Point>` both look up under `Point`.
pub(super) fn base_type_name(rendered: &str) -> &str {
    rendered
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim()
        .split(['<', '['])
        .next()
        .unwrap_or(rendered)
        .trim()
}

/// `true` when the session declares anything about `name`.
pub(super) fn index_has_facts(declarations: &[String], name: &str) -> bool {
    !session_index(declarations).is_empty_for(name)
}

/// What the session's stored declarations say about the types in it.
///
/// `%info` otherwise answers only from the stdlib catalog, so a struct
/// or trait declared at the prompt has nowhere to be looked up.
#[derive(Debug, Default)]
pub(super) struct SessionIndex {
    /// Struct name to its declared fields, in declaration order.
    pub(super) fields: BTreeMap<String, Vec<(String, String)>>,
    /// Type name to the traits implemented for it in this session.
    pub(super) implements: BTreeMap<String, Vec<String>>,
    /// Trait name to the types implementing it.
    pub(super) implementors: BTreeMap<String, Vec<String>>,
    /// Type name to its methods, each tagged with the trait it came from
    /// or `None` for an inherent `impl`.
    pub(super) methods: BTreeMap<String, Vec<(String, Option<String>)>>,
    /// Trait name to its declared method signatures.
    pub(super) trait_methods: BTreeMap<String, Vec<String>>,
    /// Declared item name to the kind label `%info` prints for it.
    pub(super) kinds: BTreeMap<String, &'static str>,
    /// Type-alias name to the type it stands for, as written.
    pub(super) aliases: BTreeMap<String, String>,
    /// Declared item name to its header as written: the item keyword, the name
    /// with its generic parameters, and a function's signature.
    pub(super) headers: BTreeMap<String, String>,
    /// Declared item name to its generic parameter list as written.
    pub(super) generics: BTreeMap<String, String>,
    /// Declared item name to its generic parameter names, in order.
    pub(super) generic_names: BTreeMap<String, Vec<String>>,
    /// Enum name to its variants as written, in declaration order.
    pub(super) variants: BTreeMap<String, Vec<String>>,
}

impl SessionIndex {
    /// `true` when nothing in the session declares `name`.
    fn is_empty_for(&self, name: &str) -> bool {
        !self.kinds.contains_key(name)
            && !self.headers.contains_key(name)
            && !self.aliases.contains_key(name)
            && !self.fields.contains_key(name)
            && !self.implements.contains_key(name)
            && !self.implementors.contains_key(name)
            && !self.methods.contains_key(name)
    }

    /// Every name the session declares, in sorted order.
    fn declared_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .kinds
            .keys()
            .chain(self.aliases.keys())
            .chain(self.fields.keys())
            .chain(self.implements.keys())
            .chain(self.implementors.keys())
            .chain(self.methods.keys())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// Renders one AST type as source.
pub(super) fn render_type(ty: &gossamer_ast::Type) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type(ty);
    printer.finish()
}

/// Renders a trait bound's path as source.
pub(super) fn render_trait_path(bound: &gossamer_ast::TraitBound) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type_path(&bound.path);
    printer.finish()
}

/// Renders a method's parameter list and return type, without its body.
pub(super) fn render_fn_signature(decl: &gossamer_ast::FnDecl) -> String {
    use gossamer_ast::{FnParam, Receiver};

    let params = decl
        .params
        .iter()
        .map(|param| match param {
            FnParam::Receiver(Receiver::Owned) => "self".to_string(),
            FnParam::Receiver(Receiver::RefShared) => "&self".to_string(),
            FnParam::Receiver(Receiver::RefMut) => "&mut self".to_string(),
            FnParam::Typed { pattern, ty, .. } => {
                let mut printer = gossamer_ast::Printer::new();
                printer.print_pattern(pattern);
                format!("{}: {}", printer.finish(), render_type(ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let ret = decl
        .ret
        .as_ref()
        .map_or_else(String::new, |ty| format!(" -> {}", render_type(ty)));
    format!("({params}){ret}")
}

/// Renders a generic parameter list as source, or nothing when there is none.
pub(super) fn render_generics(generics: &gossamer_ast::Generics) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_generics(generics);
    printer.finish()
}

/// The names a generic parameter list declares, in order.
pub(super) fn generic_param_names(generics: &gossamer_ast::Generics) -> Vec<String> {
    generics
        .params
        .iter()
        .map(|param| match param {
            gossamer_ast::GenericParam::Lifetime { name } => format!("'{name}"),
            gossamer_ast::GenericParam::Type { name, .. }
            | gossamer_ast::GenericParam::Const { name, .. } => name.name.clone(),
        })
        .collect()
}

/// An enum variant as written: its name and the shape of its payload.
pub(super) fn render_variant(variant: &gossamer_ast::EnumVariant) -> String {
    use gossamer_ast::StructBody;
    let name = &variant.name.name;
    match &variant.body {
        StructBody::Unit => name.clone(),
        StructBody::Tuple(fields) => format!(
            "{name}({})",
            fields
                .iter()
                .map(|field| render_type(&field.ty))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        StructBody::Named(fields) => format!(
            "{name} {{ {} }}",
            fields
                .iter()
                .map(|field| format!("{}: {}", field.name.name, render_type(&field.ty)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Records a declared item's header and generic parameters.
pub(super) fn index_item_header(
    index: &mut SessionIndex,
    name: &str,
    header: String,
    generics: &gossamer_ast::Generics,
) {
    index.headers.insert(name.to_string(), header);
    if !generics.is_empty() {
        index
            .generics
            .insert(name.to_string(), render_generics(generics));
        index
            .generic_names
            .insert(name.to_string(), generic_param_names(generics));
    }
}

/// The parameter names a lane vector's catalog rows are written over, which a
/// binding's own type `Simd<f64, 4>` or `Mask<4>` supplies in order.
pub(super) fn lane_type_param_names(owner: &str) -> Vec<String> {
    match owner {
        "Simd" => vec!["T".to_string(), "N".to_string()],
        "Mask" => vec!["N".to_string()],
        _ => Vec::new(),
    }
}

/// The type arguments a rendered type spells: `Ring<3>` answers `["3"]`.
pub(super) fn rendered_type_args(rendered: &str) -> Vec<String> {
    let Some(open) = rendered.find('<') else {
        return Vec::new();
    };
    let Some(inner) = rendered[open + 1..].strip_suffix('>') else {
        return Vec::new();
    };
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in inner.chars() {
        match ch {
            '<' | '(' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '>' | ')' | ']' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        args.push(current.trim().to_string());
    }
    args
}

/// `text` with each whole-word generic parameter name replaced by the argument
/// an instance gives it, so a binding's fields read as that instance's types.
pub(super) fn substitute_generic_names(text: &str, names: &[String], args: &[String]) -> String {
    if names.is_empty() || names.len() != args.len() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        match names.iter().position(|name| name == word) {
            Some(index) => out.push_str(&args[index]),
            None => out.push_str(word),
        }
        word.clear();
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
        } else {
            flush(&mut word, &mut out);
            out.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The base name of a self type, so `impl Trait for Wrapper<T>` indexes
/// under `Wrapper`.
pub(super) fn self_type_name(ty: &gossamer_ast::Type) -> String {
    let rendered = render_type(ty);
    rendered
        .split(['<', '['])
        .next()
        .unwrap_or(&rendered)
        .trim()
        .trim_start_matches('&')
        .trim()
        .to_string()
}

/// Builds the session's type index from the declarations replayed into
/// every REPL evaluation.
pub(super) fn session_index(declarations: &[String]) -> SessionIndex {
    use gossamer_ast::{ItemKind, StructBody, TraitItem};

    let mut index = SessionIndex::default();
    for declaration in declarations {
        let mut map = gossamer_lex::SourceMap::new();
        let file = map.add_file("irepl-session-index".to_string(), declaration.clone());
        let (source_file, diags) = gossamer_parse::parse_source_file(declaration, file);
        if !diags.is_empty() {
            continue;
        }
        for item in &source_file.items {
            match &item.kind {
                ItemKind::Struct(decl) => {
                    let name = decl.name.name.clone();
                    index.kinds.insert(name.clone(), "struct");
                    let header = format!("struct {name}{}", render_generics(&decl.generics));
                    index_item_header(&mut index, &name, header, &decl.generics);
                    let fields = match &decl.body {
                        StructBody::Named(fields) => fields
                            .iter()
                            .map(|field| (field.name.name.clone(), render_type(&field.ty)))
                            .collect(),
                        StructBody::Tuple(fields) => fields
                            .iter()
                            .enumerate()
                            .map(|(position, field)| (position.to_string(), render_type(&field.ty)))
                            .collect(),
                        StructBody::Unit => Vec::new(),
                    };
                    index.fields.insert(name, fields);
                }
                ItemKind::Enum(decl) => {
                    let name = decl.name.name.clone();
                    index.kinds.insert(name.clone(), "enum");
                    let header = format!("enum {name}{}", render_generics(&decl.generics));
                    index_item_header(&mut index, &name, header, &decl.generics);
                    index
                        .variants
                        .insert(name, decl.variants.iter().map(render_variant).collect());
                }
                ItemKind::Trait(decl) => {
                    let name = decl.name.name.clone();
                    index.kinds.insert(name.clone(), "trait");
                    let header = format!("trait {name}{}", render_generics(&decl.generics));
                    index_item_header(&mut index, &name, header, &decl.generics);
                    let signatures = decl
                        .items
                        .iter()
                        .filter_map(|trait_item| match trait_item {
                            TraitItem::Fn(fn_decl) => Some(format!(
                                "fn {}{}",
                                fn_decl.name.name,
                                render_fn_signature(fn_decl)
                            )),
                            _ => None,
                        })
                        .collect();
                    index.trait_methods.insert(name, signatures);
                }
                ItemKind::Fn(decl) => {
                    let name = decl.name.name.clone();
                    index.kinds.insert(name.clone(), "fn");
                    let header = format!(
                        "fn {name}{}{}",
                        render_generics(&decl.generics),
                        render_fn_signature(decl)
                    );
                    index_item_header(&mut index, &name, header, &decl.generics);
                }
                ItemKind::TypeAlias(decl) => {
                    index.kinds.insert(decl.name.name.clone(), "type");
                    let header = format!(
                        "type {}{} = {}",
                        decl.name.name,
                        render_generics(&decl.generics),
                        render_type(&decl.ty)
                    );
                    index_item_header(&mut index, &decl.name.name, header, &decl.generics);
                    index
                        .aliases
                        .insert(decl.name.name.clone(), render_type(&decl.ty));
                }
                ItemKind::Impl(decl) => index_session_impl(&mut index, decl),
                _ => {}
            }
        }
    }
    index
}

/// Records an `impl` block's methods under its owner, and a trait impl's
/// pairing in both directions.
pub(super) fn index_session_impl(index: &mut SessionIndex, decl: &gossamer_ast::ImplDecl) {
    use gossamer_ast::ImplItem;

    let owner = self_type_name(&decl.self_ty);
    let trait_name = decl.trait_ref.as_ref().map(render_trait_path);
    if let Some(trait_name) = &trait_name {
        push_unique(
            index.implements.entry(owner.clone()).or_default(),
            trait_name.clone(),
        );
        push_unique(
            index.implementors.entry(trait_name.clone()).or_default(),
            owner.clone(),
        );
        // A name reached only through an `impl X for Y` header
        // is a trait; without this it would fall to the
        // default kind and be listed as a type.
        index.kinds.entry(trait_name.clone()).or_insert("trait");
    }
    let methods = index.methods.entry(owner).or_default();
    for impl_item in &decl.items {
        if let ImplItem::Fn(fn_decl) = impl_item {
            methods.push((
                format!("{}{}", fn_decl.name.name, render_fn_signature(fn_decl)),
                trait_name.clone(),
            ));
        }
    }
}

/// Names the session declares that a type position may name, for the
/// highlighter: structs, enums, traits, and aliases.
pub(crate) fn session_type_names(declarations: &[String]) -> std::collections::HashSet<String> {
    session_index(declarations)
        .kinds
        .into_iter()
        .filter(|(_, kind)| matches!(*kind, "struct" | "enum" | "trait" | "type"))
        .map(|(name, _)| name)
        .collect()
}

/// Appends `value` unless the list already carries it.
pub(super) fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Renders what the session knows about `name`: a struct's fields, the
/// traits implemented for a type, a trait's methods and implementors.
///
/// `receiver` names the binding a method would be called on, so
/// `%explain p` shows `p.area()` where `%info Point` shows
/// `Point::area(&self)`. Returns `None` when the session declares
/// nothing under `name`.
pub(super) fn render_session_type(
    index: &SessionIndex,
    name: &str,
    receiver: Option<&str>,
    type_args: &[String],
) -> Option<String> {
    if index.is_empty_for(name) {
        return None;
    }
    let names = index.generic_names.get(name).map_or(&[][..], Vec::as_slice);
    let mut out = String::new();
    if let Some(header) = index.headers.get(name) {
        out.push_str(&format!("  declared as\n    {header}\n"));
    }
    if let Some(target) = index.aliases.get(name) {
        out.push_str(&format!("  alias of\n    {target}\n"));
    }
    if let Some(fields) = index.fields.get(name)
        && !fields.is_empty()
    {
        out.push_str("  fields\n");
        for (field, ty) in fields {
            let ty = substitute_generic_names(ty, names, type_args);
            out.push_str(&format!("    {field}: {ty}\n"));
        }
    }
    if let Some(variants) = index.variants.get(name)
        && !variants.is_empty()
    {
        out.push_str("  variants\n");
        for variant in variants {
            let variant = substitute_generic_names(variant, names, type_args);
            out.push_str(&format!("    {variant}\n"));
        }
    }
    if let Some(traits) = index.implements.get(name)
        && !traits.is_empty()
    {
        out.push_str("  implements\n");
        for trait_name in traits {
            out.push_str(&format!("    {trait_name}\n"));
        }
    }
    if let Some(signatures) = index.trait_methods.get(name)
        && !signatures.is_empty()
    {
        out.push_str("  methods\n");
        for signature in signatures {
            out.push_str(&format!("    {signature}\n"));
        }
    }
    if let Some(methods) = index.methods.get(name)
        && !methods.is_empty()
    {
        out.push_str("  methods\n");
        for (signature, from_trait) in methods {
            let origin = from_trait
                .as_ref()
                .map_or_else(|| "[inherent]".to_string(), |t| format!("[{t}]"));
            // An associated function has no receiver to be called through, so
            // it keeps its qualified spelling even where a binding is named.
            match receiver.filter(|_| receiver_method_name(signature).is_some()) {
                Some(binding) => {
                    let call = signature.replacen("(&mut self, ", "(", 1);
                    let call = call.replacen("(&mut self)", "()", 1);
                    let call = call.replacen("(&self, ", "(", 1);
                    let call = call.replacen("(&self)", "()", 1);
                    let call = call.replacen("(self, ", "(", 1);
                    let call = call.replacen("(self)", "()", 1);
                    let call = substitute_generic_names(&call, names, type_args);
                    out.push_str(&format!("    {binding}.{call} {origin}\n"));
                }
                None => out.push_str(&format!("    {name}::{signature} {origin}\n")),
            }
        }
    }
    if let Some(types) = index.implementors.get(name)
        && !types.is_empty()
    {
        out.push_str("  implemented by\n");
        for ty in types {
            out.push_str(&format!("    {ty}\n"));
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.trim_end().to_string())
}

/// The `%info` rendering for each name the session declares that `query`
/// matches.
pub(super) fn repl_session_info(index: &SessionIndex, query: &str) -> Option<String> {
    let mut sections = Vec::new();
    for name in index.declared_names() {
        if !symbol_query_matches(name, query) {
            continue;
        }
        let Some(body) = render_session_type(index, name, None, &[]) else {
            continue;
        };
        let kind = index.kinds.get(name).copied().unwrap_or("type");
        // A session function is inspected by `%explain`, which shows the
        // declaration as written; `%info` answers the types a session adds.
        if kind == "fn" {
            continue;
        }
        let generics = index.generics.get(name).map_or("", String::as_str);
        sections.push(format!("{name}{generics} [{kind}]\n{body}"));
    }
    (!sections.is_empty()).then(|| sections.join("\n"))
}
