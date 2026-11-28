/// Returns positional field names when `e` is `Name(args)` and `Name`
/// is a declared tuple struct whose arity matches `args.len()`.
fn struct_ctor_fields(
    e: &gossamer_ast::expr::Expr,
    fields: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    use gossamer_ast::expr::ExprKind;
    let ExprKind::Call { callee, args } = &e.kind else {
        return None;
    };
    let ExprKind::Path(p) = &callee.kind else {
        return None;
    };
    if p.segments.len() != 1 {
        return None;
    }
    let names = fields.get(p.segments[0].name.name.as_str())?;
    (names.len() == args.len()).then(|| names.clone())
}

struct TupleStructCollector<'a> {
    arity: &'a mut HashMap<String, usize>,
    constructors: &'a mut HashMap<String, Vec<String>>,
}

impl gossamer_ast::Visitor for TupleStructCollector<'_> {
    fn visit_item(&mut self, item: &gossamer_ast::Item) {
        if let ItemKind::Struct(decl) = &item.kind
            && let StructBody::Tuple(fields) = &decl.body
        {
            self.arity.insert(decl.name.name.clone(), fields.len());
            self.constructors.insert(
                decl.name.name.clone(),
                (0..fields.len()).map(|index| index.to_string()).collect(),
            );
        }
        gossamer_ast::visitor::walk_item(self, item);
    }
}

/// Rewrites a declared tuple struct constructor call `Pt(a, b)` into the
/// equivalent internal struct literal using `"0".."N-1"` field names.
pub fn rewrite_tuple_struct_ctors(sf: &mut SourceFile) {
    use gossamer_ast::{Visitor, VisitorMut};
    let mut arity: HashMap<String, usize> = HashMap::new();
    let mut constructors: HashMap<String, Vec<String>> = HashMap::new();

    TupleStructCollector {
        arity: &mut arity,
        constructors: &mut constructors,
    }
    .visit_source_file(sf);
    if constructors.is_empty() {
        return;
    }
    TupleCtorRewriter {
        arity: &arity,
        constructors: &constructors,
    }
    .visit_source_file(sf);
}

/// Compatibility hook retained for older tooling that invoked the constructor
/// migration pass. Named structs now use braced construction, so no source
/// rewrite is needed.
pub fn migrate_braced_struct_constructors(
    source: &str,
    file: FileId,
) -> Result<String, Vec<ParseDiagnostic>> {
    let (_, diags) = crate::parse_source_file(source, file);
    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(source.to_string())
}

struct TupleCtorRewriter<'a> {
    arity: &'a HashMap<String, usize>,
    constructors: &'a HashMap<String, Vec<String>>,
}

impl gossamer_ast::VisitorMut for TupleCtorRewriter<'_> {
    fn visit_expr(&mut self, e: &mut gossamer_ast::expr::Expr) {
        use gossamer_ast::expr::{ExprKind, StructExprField};
        gossamer_ast::visitor::walk_expr_mut(self, e);
        // Legacy injected wrappers used `http::Response(status, body,
        // content_type)`. User source now spells the runtime response
        // aggregate as `http::Response { status, body, content_type }`, but
        // keeping this rewrite lets older generated wrappers lower with named
        // fields.
        if let ExprKind::Call { callee, args } = &e.kind
            && let ExprKind::Path(path) = &callee.kind
            && path.segments.len() == 2
            && path.segments[0].name.name == "http"
            && path.segments[1].name.name == "Response"
            && args.len() == 3
        {
            let ExprKind::Call { callee, args } =
                std::mem::replace(&mut e.kind, ExprKind::Error)
            else {
                unreachable!("matched call expression")
            };
            let ExprKind::Path(path) = callee.kind else {
                unreachable!("matched path callee")
            };
            e.kind = ExprKind::Struct {
                path,
                fields: args
                    .into_iter()
                    .zip(["status", "body", "content_type"])
                    .map(|(value, name)| StructExprField {
                        name: gossamer_ast::Ident::new(name),
                        value: Some(value),
                        shorthand: false,
                    })
                    .collect(),
                base: None,
                syntax: gossamer_ast::expr::StructExprSyntax::Parenthesized,
            };
            return;
        }
        let Some(field_names) = struct_ctor_fields(e, self.constructors) else {
            return;
        };
        let ExprKind::Call { callee, args } = std::mem::replace(&mut e.kind, ExprKind::Error)
        else {
            return;
        };
        let ExprKind::Path(path) = callee.kind else {
            return;
        };
        let fields = args
            .into_iter()
            .zip(field_names)
            .map(|(value, name)| StructExprField {
                name: gossamer_ast::Ident::new(name),
                value: Some(value),
                shorthand: false,
            })
            .collect();
        e.kind = ExprKind::Struct {
            path,
            fields,
            base: None,
            syntax: gossamer_ast::expr::StructExprSyntax::Parenthesized,
        };
    }

    fn visit_pattern(&mut self, p: &mut gossamer_ast::pattern::Pattern) {
        use gossamer_ast::pattern::{FieldPattern, PatternKind};
        gossamer_ast::visitor::walk_pattern_mut(self, p);
        let convert = matches!(&p.kind, PatternKind::TupleStruct { path, elems }
            if path.segments.len() == 1
                && self
                    .arity
                    .get(path.segments[0].name.name.as_str())
                    .is_some_and(|&n| n == elems.len()));
        if !convert {
            return;
        }
        let PatternKind::TupleStruct { path, elems } =
            std::mem::replace(&mut p.kind, PatternKind::Wildcard)
        else {
            return;
        };
        let fields = elems
            .into_iter()
            .enumerate()
            .map(|(i, pat)| FieldPattern {
                name: gossamer_ast::Ident::new(i.to_string()),
                pattern: Some(pat),
            })
            .collect();
        p.kind = PatternKind::Struct {
            path,
            fields,
            rest: false,
        };
    }
}

/// Convenience wrapper that augments `source` then parses the
/// result against `file`. Returns the merged `SourceFile` and any
/// parse diagnostics. Callers MUST have already added the augmented
/// source to their source map (see `augment_source`) for span
/// resolution to work.
#[must_use]
pub fn parse_with_autoderive(source: &str, file: FileId) -> (SourceFile, Vec<ParseDiagnostic>) {
    parse_with_autoderive_from(source, file, None)
}

/// Imports for the generated section the standard modules its injected
/// bodies call through and the program left unbound. A `use` has to precede
/// every item in written source, so the imports join the tree rather than the
/// text; each is spanned at the start of the generated section.
fn import_wrapper_modules(source: &str, sf: &mut SourceFile) {
    let marker = GENERATED_SECTION_MARKER;
    let Some(at) = source.rfind(marker) else {
        return;
    };
    let generated = &source[at + marker.len()..];
    let modules = wrapper_module_paths(generated, sf);
    let offset = u32::try_from(at).unwrap_or(u32::MAX);
    let span = gossamer_lex::Span::new(sf.file, offset, offset);
    for module in modules {
        let path = gossamer_ast::ModulePath::from_names(module.split("::"));
        sf.uses
            .push(gossamer_ast::UseDecl::synthetic(span, gossamer_ast::UseTarget::Module(path)));
    }
}

/// [`parse_with_autoderive`] over `source`, the text
/// [`augment_source_with_program`] answered with `program`: the program's
/// own text is taken from that tree and only what autoderive appended is
/// parsed. The tree answered is the one a parse of the whole of `source`
/// gives. A `program` from another file, or `None`, parses everything.
#[must_use]
pub fn parse_with_autoderive_from(
    source: &str,
    file: FileId,
    program: Option<ParsedProgram>,
) -> (SourceFile, Vec<ParseDiagnostic>) {
    let (mut sf, mut diags) = match program {
        Some(ParsedProgram { tree, len }) if tree.file == file && source.is_char_boundary(len) => {
            crate::parse_appended(source, file, tree, len)
        }
        _ => crate::parse_source_file(source, file),
    };
    import_wrapper_modules(source, &mut sf);
    // A field default is a function of its own, and a type body is the
    // `impl` blocks it stands for; every pass below reads those.
    crate::synthesize_field_default_fns(&mut sf);
    diags.extend(crate::desugar_type_bodies(&mut sf));
    // `augment_source` synthesizes nothing from a tree that did not parse, so
    // a missing serde codec there says nothing about the type it names.
    let parsed_cleanly = diags.is_empty();
    splice_iterator_methods(&mut sf);
    rewrite_operator_impls(&mut sf);
    rewrite_fallible_collect(&mut sf);
    // The entry file is implicitly `fn main`: fold its bare top-level
    // statements into one (or report a conflict with an explicit `fn main`)
    // before the rewrites below, so the synthesized body receives the same
    // serde-turbofish and synthetic-use treatment as any function body. This
    // is the single compile/analysis parse entry - every codegen tier, the
    // REPL compiler path, and the LSP reach the implicit main through here.
    // Source-preserving `gos fmt`/`doc`/`lint` paths use raw
    // `parse_source_file` and are unaffected.
    diags.extend(crate::entry_main::synthesize_entry_main(&mut sf));
    rewrite_tuple_struct_ctors(&mut sf);
    materialize_trait_defaults(&mut sf);
    rewrite_trait_qualified_calls(&mut sf);
    initialize_heap_mut_statics(&mut sf);
    infer_serde_turbofish(&mut sf);
    desugar_sort_by_key(&mut sf);
    hoist_associated_consts(&mut sf);
    // After the associated-constant reads are hoisted, the `Self` heads left in
    // an impl name the implementing type.
    rewrite_self_paths(&mut sf);
    // Runs on the un-mangled AST: `rewrite_serde_generic_calls` below turns a
    // serde turbofish into a bare mangled name, erasing the type argument the
    // check keys on.
    if parsed_cleanly {
        diags.extend(serde_unsupported_field_diags(&sf));
        diags.extend(serde_attribute_diags(&sf));
    }
    diags.extend(derive_default_diags(&sf));
    rewrite_duplicate_keys_args(&mut sf);
    rewrite_serve_tls_config(&mut sf);
    rewrite_serde_generic_calls(&mut sf);
    specialize_inline_for_generics(&mut sf);
    expand_typeinfo_loops(&mut sf);
    rewrite_type_info_calls(&mut sf);
    rewrite_json_set_mutators(&mut sf);
    rewrite_range_contains(&mut sf);
    rewrite_stdlib_struct_surface(&mut sf);
    // `rewrite_stdlib_struct_surface` turns public qualified wrapper names
    // such as `sql::Rows` and `encoding::pem::Block` into the injected local
    // structs. Run constructor conversion again now that those paths are
    // local, so their canonical `Type(args...)` spelling becomes the field
    // aggregate expected by every lowering tier.
    rewrite_tuple_struct_ctors(&mut sf);
    inject_synthetic_uses(&mut sf, file);
    number_synthesized_nodes(&mut sf);
    (sf, diags)
}

/// Gives every node a rewrite built without an identifier one of its own.
///
/// The checker's per-node tables are keyed by identifier, so nodes sharing
/// the placeholder would share one entry, and the type recorded for one would
/// answer for all of them.
fn number_synthesized_nodes(sf: &mut SourceFile) {
    use gossamer_ast::visitor::{
        VisitorMut, walk_expr_mut, walk_item_mut, walk_pattern_mut, walk_stmt_mut, walk_type_mut,
    };

    struct Numberer {
        next: u32,
    }
    impl Numberer {
        fn assign(&mut self, id: &mut NodeId) {
            if *id == NodeId::DUMMY {
                *id = NodeId::from_raw(self.next);
                self.next = self.next.saturating_add(1);
            }
        }
    }
    impl VisitorMut for Numberer {
        fn visit_expr(&mut self, expr: &mut gossamer_ast::Expr) {
            self.assign(&mut expr.id);
            walk_expr_mut(self, expr);
        }
        fn visit_stmt(&mut self, stmt: &mut gossamer_ast::Stmt) {
            self.assign(&mut stmt.id);
            walk_stmt_mut(self, stmt);
        }
        fn visit_pattern(&mut self, pattern: &mut gossamer_ast::Pattern) {
            self.assign(&mut pattern.id);
            walk_pattern_mut(self, pattern);
        }
        fn visit_type(&mut self, ty: &mut gossamer_ast::Type) {
            self.assign(&mut ty.id);
            walk_type_mut(self, ty);
        }
        fn visit_item(&mut self, item: &mut gossamer_ast::Item) {
            self.assign(&mut item.id);
            walk_item_mut(self, item);
        }
    }
    let mut numberer = Numberer {
        next: sf.next_node_id,
    };
    numberer.visit_source_file(sf);
    sf.next_node_id = numberer.next;
}

/// Reports serde turbofish calls (`to_json::<T>(v)`, `from_json::<T>(s)`, and
/// the toml/yaml forms) whose struct `T` has a field serde cannot classify.
/// Such a struct gets no stub from `synthesize_serde_impls`, so without this
/// the call would surface only as an opaque unknown-name error.
/// Gated on use - a struct with an unsupported field is fine until it actually
/// flows through a serde call - and deduplicated to one diagnostic per struct,
/// pointing at the first offending field.
fn serde_unsupported_field_diags(sf: &SourceFile) -> Vec<ParseDiagnostic> {
    let struct_names = struct_identities(&sf.items);
    let aliases = alias_targets(&sf.items);
    // Keyed by both the module-folded symbol and the bare name; a bare name
    // two modules share keeps its first declaration.
    let mut decls: HashMap<String, (String, &StructDecl)> = HashMap::new();
    for (module, item) in flatten_items_with_modules(&sf.items) {
        let ItemKind::Struct(decl) = &item.kind else {
            continue;
        };
        if !decl.generics.params.is_empty() {
            continue;
        }
        let symbol = TyId::new(&module, &decl.name.name).symbol;
        decls
            .entry(symbol)
            .or_insert_with(|| (module.clone(), decl));
        decls
            .entry(decl.name.name.clone())
            .or_insert_with(|| (module, decl));
    }
    // `augment_source` has already declared a `__gos_serde_to_json_<T>` for every
    // named type serde accepts (and the user may hand-provide one), so its
    // presence means the type is serializable - only its absence is a dropped
    // struct worth diagnosing.
    let synthesized: HashSet<&str> = sf
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Fn(f) => f.name.name.strip_prefix("__gos_serde_to_json_"),
            _ => None,
        })
        .collect();

    // The rewriter maps a written spelling onto the symbol its synthesized
    // functions carry - through a module path, an import, or an alias. Asking
    // the same index here is what keeps the two from disagreeing: a spelling
    // the rewriter can resolve is never reported, and one it cannot is never
    // left to surface as the mangled name.
    let symbols = serde_symbol_index(sf);
    let resolved = |written: &str, module: &str| serde_symbol_for(&symbols, module, written);

    let mut diags = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    for (op, target, call_span, module) in collect_serde_turbofish_calls(sf) {
        let ty_name = match target {
            SerdeCallTarget::Named(written) | SerdeCallTarget::Inner(written) => written,
            SerdeCallTarget::TypeParameter(param) => {
                if reported.insert(param.clone()) {
                    diags.push(ParseDiagnostic::new(
                        crate::ParseError::SerdeUnsupportedTarget {
                            ty: param,
                            op,
                            reason: SerdeTargetRefusal::TypeParameter,
                        },
                        call_span,
                    ));
                }
                continue;
            }
        };
        let symbol = resolved(&ty_name, &module);
        if reported.contains(&ty_name) || synthesized.contains(symbol.as_str()) {
            continue;
        }
        let Some((module, decl)) = decls.get(symbol.as_str()).or_else(|| decls.get(ty_name.as_str()))
        else {
            // No concrete struct behind the spelling. Naming which shape it is
            // beats the alternative, which is the absent synthesized function
            // surfacing as an internal name the user never wrote.
            reported.insert(ty_name.clone());
            diags.push(ParseDiagnostic::new(
                crate::ParseError::SerdeUnsupportedTarget {
                    ty: ty_name,
                    op,
                    reason: refusal_for(sf, &symbol),
                },
                call_span,
            ));
            continue;
        };
        let offending = match &decl.body {
            StructBody::Named(fields) => fields.iter().find_map(|f| {
                FieldKind::from_type(&f.ty, module, &struct_names, &aliases)
                    .is_none()
                    .then(|| (f.name.name.clone(), ty_to_string(&f.ty), f.ty.span))
            }),
            StructBody::Tuple(fields) => fields.iter().enumerate().find_map(|(i, f)| {
                FieldKind::from_type(&f.ty, module, &struct_names, &aliases)
                    .is_none()
                    .then(|| (i.to_string(), ty_to_string(&f.ty), f.ty.span))
            }),
            StructBody::Unit => None,
        };
        reported.insert(ty_name.clone());
        match offending {
            Some((field, field_ty, span)) => diags.push(ParseDiagnostic::new(
                crate::ParseError::SerdeUnserializableField {
                    ty: ty_name,
                    field,
                    field_ty,
                    op,
                },
                span,
            )),
            // Every field classified, yet no function was synthesized. The
            // shape is unattributable to one field, so the report says only
            // what is certain rather than pointing somewhere arbitrary.
            None => diags.push(ParseDiagnostic::new(
                crate::ParseError::SerdeUnsupportedTarget {
                    ty: ty_name,
                    op,
                    reason: SerdeTargetRefusal::Unsupported,
                },
                call_span,
            )),
        }
    }
    diags
}

/// Which shape a serde turbofish target is, once it is known that no
/// synthesized codec exists for it. A named target is a declared struct or
/// enum, so the only question is whether it was named without its arguments.
fn refusal_for(sf: &SourceFile, symbol: &str) -> SerdeTargetRefusal {
    let generic = flatten_items_with_modules(&sf.items).iter().any(|(module, item)| {
        let (name, generics) = match &item.kind {
            ItemKind::Struct(decl) => (&decl.name.name, &decl.generics),
            ItemKind::Enum(decl) => (&decl.name.name, &decl.generics),
            _ => return false,
        };
        (TyId::new(module, name).symbol == symbol || name == symbol)
            && !generics.params.is_empty()
    });
    if generic {
        SerdeTargetRefusal::Generic
    } else {
        SerdeTargetRefusal::Unsupported
    }
}

/// What a serde turbofish call asks for, as the diagnostics and the stub
/// synthesis read it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SerdeCallTarget {
    /// A user type named on its own: the spelling as written.
    Named(String),
    /// A user type named inside a compound target (`Vec<Shape>`), which the
    /// call's own stub codes through this type's codec.
    Inner(String),
    /// A type parameter of the enclosing function.
    TypeParameter(String),
}

/// Collects `(op, target, span, module)` for every serde turbofish call in
/// `sf` (`to_json::<T>` / `from_json::<T>`, the toml/yaml forms, and
/// `json::from_value::<T>`, bare or format-module-qualified), on the
/// un-mangled AST. A compound target contributes each user type it names.
fn collect_serde_turbofish_calls(sf: &SourceFile) -> Vec<(String, SerdeCallTarget, Span, String)> {
    use gossamer_ast::Visitor;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr;

    struct Collector<'a> {
        types: &'a SerdeTypes,
        calls: Vec<(String, SerdeCallTarget, Span, String)>,
        module: Vec<String>,
        params: Vec<Vec<String>>,
    }
    impl Visitor for Collector<'_> {
        fn visit_item(&mut self, item: &gossamer_ast::Item) {
            // Synthesis reads only the items this build compiles, so a call
            // inside an inactive `#[cfg]` item has no codec to be missing.
            if !gossamer_ast::cfg::item_is_active(&item.attrs) {
                return;
            }
            let name = match &item.kind {
                ItemKind::Mod(decl) if matches!(decl.body, gossamer_ast::ModBody::Inline(_)) => {
                    Some(decl.name.name.clone())
                }
                _ => None,
            };
            self.module.extend(name.clone());
            self.params.push(item_type_params(item));
            gossamer_ast::visitor::walk_item(self, item);
            self.params.pop();
            if name.is_some() {
                self.module.pop();
            }
        }

        fn visit_expr(&mut self, expr: &Expr) {
            walk_expr(self, expr);
            let ExprKind::Call { callee, .. } = &expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &callee.kind else {
                return;
            };
            if self.types.names_own_module(&path.segments, &self.module) {
                return;
            }
            let Some((qualified_op, seg_idx)) = serde_call_op(&path.segments) else {
                return;
            };
            let seg = &path.segments[seg_idx];
            let op = qualified_op.unwrap_or(seg.name.name.as_str()).to_string();
            if !matches!(
                op.as_str(),
                "to_json" | "from_json" | "to_toml" | "from_toml" | "to_yaml" | "from_yaml"
                    | "from_value"
            ) || seg.generics.len() != 1
            {
                return;
            }
            let GenericArg::Type(ty) = &seg.generics[0] else {
                return;
            };
            let module = self.module.join("::");
            let scope: Vec<&str> = self.params.iter().flatten().map(String::as_str).collect();
            if let Some(param) = mentioned_type_param(ty, &scope) {
                self.calls.push((op, SerdeCallTarget::TypeParameter(param), callee.span, module));
                return;
            }
            match self.types.target(ty, &module) {
                SerdeTarget::Named(_) | SerdeTarget::GenericBare => {
                    if let Some(written) = written_path(ty) {
                        self.calls.push((op, SerdeCallTarget::Named(written), callee.span, module));
                    }
                }
                SerdeTarget::Site => {
                    for inner in type_paths(ty) {
                        let written = inner.join("::");
                        let symbol = serde_symbol_for(&self.types.symbols, &module, &written);
                        if self.types.user.contains(&symbol) && !self.types.generic.contains(&symbol)
                        {
                            self.calls.push((
                                op.clone(),
                                SerdeCallTarget::Inner(written),
                                callee.span,
                                module.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }

    let types = SerdeTypes::of(sf);
    let mut collector = Collector {
        types: &types,
        calls: Vec::new(),
        module: Vec::new(),
        params: Vec::new(),
    };
    collector.visit_source_file(sf);
    collector.calls
}

/// Each stdlib `module::item` (matched on the last two segment names) and the
/// mangled name of the injected wrapper or struct it reaches, so both
/// `encoding::pem::decode` and the bare `pem::decode` map.
const MANGLED_STDLIB_NAMES: &[(&str, &str, &str)] = &[
    ("pem", "decode", "__gos_pem_decode"),
    ("pem", "decode_all", "__gos_pem_decode_all"),
    ("pem", "encode", "__gos_pem_encode"),
    ("pem", "Block", "__gos_pem_Block"),
    ("x509", "parse_pem", "__gos_x509_parse_pem"),
    ("x509", "CertInfo", "__gos_x509_CertInfo"),
    ("x509", "Extension", "__gos_x509_Extension"),
    ("x509", "self_signed", "__gos_x509_self_signed"),
    ("x509", "issue", "__gos_x509_issue"),
    ("x509", "extensions", "__gos_x509_extensions"),
    ("fs", "metadata", "__gos_fs_metadata"),
    ("fs", "Metadata", "__gos_fs_Metadata"),
    ("fs", "temp_dir", "__gos_fs_temp_dir"),
    ("fs", "temp_file", "__gos_fs_temp_file"),
    ("fs", "chown", "__gos_fs_chown"),
    ("tls", "ServerConfig", "__gos_tls_ServerConfig"),
    ("net", "lookup_mx", "__gos_net_lookup_mx"),
    ("net", "lookup_txt", "__gos_net_lookup_txt"),
    ("net", "lookup_srv", "__gos_net_lookup_srv"),
    ("net", "lookup_cname", "__gos_net_lookup_cname"),
    ("net", "lookup_addr", "__gos_net_lookup_addr"),
    ("net", "Mx", "__gos_net_Mx"),
    ("net", "Srv", "__gos_net_Srv"),
    ("fs", "disk_space", "__gos_fs_disk_space"),
    ("fs", "DiskSpace", "__gos_fs_DiskSpace"),
    ("json", "entries", "__gos_json_entries"),
    ("json", "Decoder", "__gos_json_Decoder"),
    ("ecdh", "x25519_keypair", "__gos_ecdh_x25519_keypair"),
    ("ecdh", "x25519", "__gos_ecdh_x25519"),
    ("ecdh", "p256_keypair", "__gos_ecdh_p256_keypair"),
    ("ecdh", "p256", "__gos_ecdh_p256"),
    ("ecdh", "p384_keypair", "__gos_ecdh_p384_keypair"),
    ("ecdh", "p384", "__gos_ecdh_p384"),
    ("mlkem", "keypair_768", "__gos_mlkem_keypair_768"),
    ("mlkem", "encapsulate_768", "__gos_mlkem_encapsulate_768"),
    ("mlkem", "decapsulate_768", "__gos_mlkem_decapsulate_768"),
    ("mlkem", "keypair_1024", "__gos_mlkem_keypair_1024"),
    ("mlkem", "encapsulate_1024", "__gos_mlkem_encapsulate_1024"),
    ("mlkem", "decapsulate_1024", "__gos_mlkem_decapsulate_1024"),
    ("ecdh", "SharedSecret", "__gos_crypto_SharedSecret"),
    ("mlkem", "SharedSecret", "__gos_crypto_SharedSecret"),
    ("text_template", "Template", "__gos_text_template_Template"),
    ("html_template", "Template", "__gos_html_template_Template"),
    ("text_template", "MissingKey", "__gos_template_MissingKey"),
    ("html_template", "MissingKey", "__gos_template_MissingKey"),
    ("text_template", "render", "__gos_text_template_render"),
    ("html_template", "render", "__gos_html_template_render"),
    ("netip", "Prefix", "__gos_netip_Prefix"),
    ("image", "decode", "__gos_image_decode"),
    ("image", "set_max_pixels", "__gos_image_set_max_pixels"),
    ("image", "Image", "__gos_image_Image"),
    ("image", "Rgba", "__gos_image_Rgba"),
    ("mail", "parse_address", "__gos_mail_parse_address"),
    ("mail", "parse_address_list", "__gos_mail_parse_address_list"),
    ("mail", "Address", "__gos_mail_Address"),
    ("json", "DuplicateKeys", "__gos_json_DuplicateKeys"),
    ("yaml", "DuplicateKeys", "__gos_json_DuplicateKeys"),
    ("fs", "read_dir", "__gos_fs_read_dir"),
    ("fs", "DirInfo", "__gos_fs_DirInfo"),
    ("fs", "walk_dir", "__gos_fs_walk_dir"),
    ("path", "walk", "__gos_fs_walk_dir"),
    ("process", "run", "__gos_process_run"),
    ("process", "run_in", "__gos_process_run_in"),
    ("process", "Output", "__gos_process_Output"),
    ("process", "pipeline_run", "__gos_process_pipeline_run"),
    ("process", "Command", "__gos_process_Command"),
    ("process", "Stdio", "__gos_process_Stdio"),
    ("process", "Child", "__gos_process_Child"),
    ("process", "spawn_piped", "__gos_process_spawn_piped"),
    ("path", "Path", "__gos_path_Path"),
    ("collections", "IndexMap", "__gos_collections_IndexMap"),
    ("collections", "BitSet", "__gos_collections_BitSet"),
    ("bufio", "Reader", "__gos_bufio_Reader"),
    ("bufio", "Writer", "__gos_bufio_Writer"),
    ("math", "Decimal", "__gos_math_Decimal"),
    ("math", "Rational", "__gos_math_Rational"),
    ("strings", "shell_split", "__gos_strings_shell_split"),
    ("strings", "shell_quote", "__gos_strings_shell_quote"),
    ("strings", "shell_join", "__gos_strings_shell_join"),
    ("math", "mean", "__gos_math_mean"),
    ("math", "median", "__gos_math_median"),
    ("math", "variance", "__gos_math_variance"),
    ("math", "stdev", "__gos_math_stdev"),
    ("math", "pvariance", "__gos_math_pvariance"),
    ("math", "pstdev", "__gos_math_pstdev"),
    ("math", "quantiles", "__gos_math_quantiles"),
    ("math", "erf", "__gos_math_erf"),
    ("math", "erfc", "__gos_math_erfc"),
    ("math", "gamma", "__gos_math_gamma"),
    ("math", "lgamma", "__gos_math_lgamma"),
    ("math", "factorial", "__gos_math_factorial"),
    ("math", "comb", "__gos_math_comb"),
    ("math", "perm", "__gos_math_perm"),
    ("math", "Complex", "__gos_math_Complex"),
    ("rand", "range", "__gos_rand_range"),
    ("rand", "range_f64", "__gos_rand_range_f64"),
    ("rand", "chance", "__gos_rand_chance"),
    ("rand", "choose", "__gos_rand_choose"),
    ("rand", "shuffle", "__gos_rand_shuffle"),
    ("rand", "sample", "__gos_rand_sample"),
    ("rand", "weighted", "__gos_rand_weighted"),
    ("rand", "normal", "__gos_rand_normal"),
    ("rand", "exponential", "__gos_rand_exponential"),
    ("rand", "from_os", "__gos_rand_from_os"),
    ("fs", "TempDir", "__gos_fs_TempDir"),
    ("fs", "TempFile", "__gos_fs_TempFile"),
    ("http", "Http2Config", "__gos_http_Http2Config"),
    ("Http2Config", "default", "__gos_http_Http2Config_default"),
    ("time", "Location", "__gos_time_Location"),
    ("time", "Time", "__gos_time_Time"),
    ("time", "CivilTime", "__gos_time_CivilTime"),
    ("time", "CivilResolution", "__gos_time_CivilResolution"),
    ("time", "format_in", "__gos_time_format_in"),
    ("time", "parse", "__gos_time_parse"),
    ("time", "add_date", "__gos_time_add_date"),
    // tar/zip `read` route through the struct wrapper; `write`
    // lowers directly (no struct), so it is NOT rewritten.
    ("tar", "read", "__gos_tar_read"),
    ("tar", "read_limited", "__gos_tar_read_limited"),
    ("tar", "TarEntry", "__gos_tar_TarEntry"),
    ("zip", "read", "__gos_zip_read"),
    ("zip", "read_limited", "__gos_zip_read_limited"),
    ("zip", "ZipEntry", "__gos_zip_ZipEntry"),
    ("sql", "open", "__gos_sql_open"),
    ("sql", "drivers", "__gos_sql_drivers"),
    ("sql", "Conn", "__gos_sql_Conn"),
    ("sql", "Rows", "__gos_sql_Rows"),
    ("sql", "Row", "__gos_sql_Row"),
    ("sql", "Tx", "__gos_sql_Tx"),
    ("sql", "Value", "__gos_sql_Value"),
    ("sql", "IsolationLevel", "__gos_sql_IsolationLevel"),
    ("sql", "Stmt", "__gos_sql_Stmt"),
    ("sql", "Pool", "__gos_sql_Pool"),
    ("sql", "Notification", "__gos_sql_Notification"),
    ("sql", "Select", "__gos_sql_Select"),
    ("sql", "pool_open", "__gos_sql_pool_open"),
    ("sql", "pool_open_with", "__gos_sql_pool_open_with"),
    ("sql", "migrate_up", "__gos_sql_migrate_up"),
    // Gossamer-native driver dispatch: `register_native` captures
    // the driver's env + dispatch fn-address (custom MIR lowering,
    // hooked on the mangled leaf name); the `native_*` /
    // `value_*` helpers are the side-channel a `.gos` driver reads
    // and writes through.
    ("sql", "register_native", "__gos_sql_register_native"),
    ("sql", "native_url", "__gos_sql_native_url"),
    ("sql", "native_sql", "__gos_sql_native_sql"),
    ("sql", "native_parent", "__gos_sql_native_parent"),
    ("sql", "native_out_handle", "__gos_sql_native_out_handle"),
    ("sql", "native_iso", "__gos_sql_native_iso"),
    ("sql", "native_timeout", "__gos_sql_native_timeout"),
    ("sql", "native_channel", "__gos_sql_native_channel"),
    ("sql", "native_param_count", "__gos_sql_native_param_count"),
    ("sql", "native_param", "__gos_sql_native_param"),
    ("sql", "native_data", "__gos_sql_native_data"),
    ("sql", "native_push_column", "__gos_sql_native_push_column"),
    ("sql", "native_push_value", "__gos_sql_native_push_value"),
    ("sql", "native_row_ready", "__gos_sql_native_row_ready"),
    ("sql", "native_set_error", "__gos_sql_native_set_error"),
    ("sql", "native_emit_bytes", "__gos_sql_native_emit_bytes"),
    ("sql", "native_set_notification", "__gos_sql_native_set_notification"),
    ("sql", "native_set_handle", "__gos_sql_native_set_handle"),
    ("sql", "native_handle", "__gos_sql_native_handle"),
    ("sql", "value_null", "__gos_sql_native_value_null"),
    ("sql", "value_bool", "__gos_sql_native_value_bool"),
    ("sql", "value_int", "__gos_sql_native_value_int"),
    ("sql", "value_float", "__gos_sql_native_value_float"),
    ("sql", "value_text", "__gos_sql_native_value_text"),
    ("sql", "value_blob", "__gos_sql_native_value_blob"),
    ("sql", "value_kind", "__gos_sql_native_value_kind"),
    ("sql", "value_int_of", "__gos_sql_native_value_int_of"),
    ("sql", "value_float_of", "__gos_sql_native_value_float_of"),
    ("sql", "value_text_of", "__gos_sql_native_value_text_of"),
    ("sql", "value_blob_of", "__gos_sql_native_value_blob_of"),
    // Channel-returning timer: `time::after(d)` fires on a goroutine that
    // sleeps then sends, so the result is usable in `select` / `while let`.
    ("time", "after", "__gos_time_after"),
    // Cancellation-shielded and time-bounded work, written with
    // `cohort` + `spawn` because that is what the two primitives
    // already express (SPEC 8.6's addition test).
    ("sync", "shield", "__gos_sync_shield"),
    ("sync", "with_timeout", "__gos_sync_with_timeout"),
    ("sync", "Semaphore", "__gos_sync_Semaphore"),
    ("time", "Ticker", "__gos_time_Ticker"),
    ("sync", "Permit", "__gos_sync_Permit"),
    // std::http::form url-encoded parser.
    ("form", "Form", "__gos_http_form_Form"),
    ("form", "parse", "__gos_http_form_parse"),
    ("form", "get", "__gos_http_form_get"),
    ("form", "get_all", "__gos_http_form_get_all"),
    ("form", "has", "__gos_http_form_has"),
    ("form", "count", "__gos_http_form_count"),
    // std::http::multipart (multipart/form-data) parser.
    ("multipart", "Part", "__gos_http_multipart_Part"),
    ("multipart", "parse", "__gos_http_multipart_parse"),
    ("multipart", "boundary", "__gos_http_multipart_boundary"),
    // std::ffi C type names and C string conversions.
    ("ffi", "c_char", "__gos_ffi_c_char"),
    ("ffi", "c_schar", "__gos_ffi_c_schar"),
    ("ffi", "c_uchar", "__gos_ffi_c_uchar"),
    ("ffi", "c_short", "__gos_ffi_c_short"),
    ("ffi", "c_ushort", "__gos_ffi_c_ushort"),
    ("ffi", "c_int", "__gos_ffi_c_int"),
    ("ffi", "c_uint", "__gos_ffi_c_uint"),
    ("ffi", "c_long", "__gos_ffi_c_long"),
    ("ffi", "c_ulong", "__gos_ffi_c_ulong"),
    ("ffi", "c_longlong", "__gos_ffi_c_longlong"),
    ("ffi", "c_ulonglong", "__gos_ffi_c_ulonglong"),
    ("ffi", "size_t", "__gos_ffi_size_t"),
    ("ffi", "ssize_t", "__gos_ffi_ssize_t"),
    ("ffi", "c_float", "__gos_ffi_c_float"),
    ("ffi", "c_double", "__gos_ffi_c_double"),
    ("ffi", "cstring", "__gos_ffi_cstring"),
    ("ffi", "from_cstr", "__gos_ffi_from_cstr"),
    ("ffi", "c_void", "__gos_ffi_c_void"),
    ("ffi", "Ptr", "__gos_ffi_Ptr"),
    ("ffi", "Handle", "__gos_ffi_Handle"),
    ("ffi", "read", "__gos_ffi_read"),
    ("ffi", "read_at", "__gos_ffi_read_at"),
    ("ffi", "write", "__gos_ffi_write"),
    ("ffi", "write_at", "__gos_ffi_write_at"),
    ("ffi", "alloc", "__gos_ffi_alloc"),
    ("ffi", "size_of", "__gos_ffi_size_of"),
    ("ffi", "align_of", "__gos_ffi_align_of"),
    ("ffi", "offset_of", "__gos_ffi_offset_of"),
    ("ffi", "addr_of", "__gos_ffi_addr_of"),
    ("ffi", "Union", "__gos_ffi_Union"),
    ("ffi", "View", "__gos_ffi_View"),
    ("ffi", "atomic_load", "__gos_ffi_atomic_load"),
    ("ffi", "atomic_store", "__gos_ffi_atomic_store"),
    ("ffi", "atomic_swap", "__gos_ffi_atomic_swap"),
    ("ffi", "atomic_compare_exchange", "__gos_ffi_atomic_compare_exchange"),
    ("ffi", "atomic_fetch_add", "__gos_ffi_atomic_fetch_add"),
    ("ffi", "atomic_fetch_sub", "__gos_ffi_atomic_fetch_sub"),
    ("ffi", "atomic_fetch_and", "__gos_ffi_atomic_fetch_and"),
    ("ffi", "atomic_fetch_or", "__gos_ffi_atomic_fetch_or"),
    ("ffi", "atomic_fetch_xor", "__gos_ffi_atomic_fetch_xor"),
    ("ffi", "free", "__gos_ffi_free"),
    ("ffi", "read_bytes", "__gos_ffi_read_bytes"),
    ("ffi", "read_cstr", "__gos_ffi_read_cstr"),
    ("ffi", "write_bytes", "__gos_ffi_write_bytes"),
    ("ffi", "to_c_bytes", "__gos_ffi_to_c_bytes"),
    ("ffi", "fn_from_ptr", "__gos_ffi_fn_from_ptr"),
    ("ffi", "fn_addr", "__gos_ffi_fn_addr"),
    // std::os::fd readiness waits.
    ("fd", "Descriptor", "__gos_fd_Descriptor"),
    ("fd", "Borrowed", "__gos_fd_Borrowed"),
    ("fd", "wait_readable", "__gos_fd_wait_readable"),
    ("fd", "wait_writable", "__gos_fd_wait_writable"),
    // std::os::signal numbers, per target.
    ("signal", "SIGHUP", "__gos_signal_SIGHUP"),
    ("signal", "SIGINT", "__gos_signal_SIGINT"),
    ("signal", "SIGQUIT", "__gos_signal_SIGQUIT"),
    ("signal", "SIGTERM", "__gos_signal_SIGTERM"),
    ("signal", "SIGUSR1", "__gos_signal_SIGUSR1"),
    ("signal", "SIGUSR2", "__gos_signal_SIGUSR2"),
    ("signal", "SIGWINCH", "__gos_signal_SIGWINCH"),
    ("signal", "SIGTSTP", "__gos_signal_SIGTSTP"),
    ("signal", "SIGCONT", "__gos_signal_SIGCONT"),
    // std::term, written in Gossamer over the platform C library.
    ("term", "STDIN", "__gos_term_STDIN"),
    ("term", "STDOUT", "__gos_term_STDOUT"),
    ("term", "STDERR", "__gos_term_STDERR"),
    ("term", "RawMode", "__gos_term_RawMode"),
    ("term", "is_terminal", "__gos_term_is_terminal"),
    ("term", "size", "__gos_term_size"),
    ("term", "enter_raw", "__gos_term_enter_raw"),
    ("term", "read_input", "__gos_term_read_input"),
    ("term", "read_password", "__gos_term_read_password"),
    ("term", "resized", "__gos_term_resized"),
];

/// The mangled name of the injected wrapper or struct a stdlib `parent::item`
/// reaches, if any. See [`MANGLED_STDLIB_NAMES`].
fn mangled_stdlib_name(parent: &str, item: &str) -> Option<&'static str> {
    MANGLED_STDLIB_NAMES
        .iter()
        .find(|(p, i, _)| *p == parent && *i == item)
        .map(|(_, _, name)| *name)
}

/// The `parent::item` a program writes for the injected stdlib wrapper or
/// type `mangled` (`term::is_terminal` for `__gos_term_is_terminal`), for a
/// diagnostic that names it.
#[must_use]
pub fn stdlib_wrapper_spelling(mangled: &str) -> Option<String> {
    MANGLED_STDLIB_NAMES
        .iter()
        .find(|(_, _, name)| *name == mangled)
        .map(|(parent, item, _)| format!("{}::{item}", spelled_module(parent)))
}

/// The name a program spells the module a rewrite-table key stands for:
/// both template modules are spelled `template`.
fn spelled_module(key: &str) -> &str {
    match key {
        "text_template" | "html_template" => "template",
        other => other,
    }
}

/// The declaration of the standard type `module_path::name` that is written
/// in Gossamer (`std::time` and `CivilTime`), as a program spells it: its
/// fields with their defaults or its variants, then its public methods.
/// `None` for a type that is not written in Gossamer.
#[must_use]
pub fn stdlib_wrapper_type_outline(module_path: &str, name: &str) -> Option<String> {
    let import = module_path.strip_prefix("std::")?;
    let mut segments = import.rsplit("::");
    let parent = segments.next()?;
    let key = nested_module_key(segments.next(), parent);
    let mangled = mangled_stdlib_name(&key, name)?;
    // A program that imports the module and names the type, which is what
    // brings its wrappers in.
    let probe = format!("use std::{import}\n\nfn probe(value: {parent}::{name}) {{}}\n");
    let source = stdlib_wrapper_source(&probe);
    let parsed = clean_parse(&source)?;
    let text = |span: Span| {
        let end = (span.end as usize).min(source.len());
        source.get(span.start as usize..end).unwrap_or_default().trim().to_string()
    };
    let shape = |body: &StructBody| match body {
        StructBody::Named(fields) => {
            let fields: Vec<String> = fields
                .iter()
                .map(|f| {
                    let default = f.default.as_ref().map(|d| format!(" = {}", text(d.span)));
                    format!("{}: {}{}", f.name.name, text(f.ty.span), default.unwrap_or_default())
                })
                .collect();
            Some(fields)
        }
        StructBody::Tuple(fields) => {
            Some(vec![fields.iter().map(|f| text(f.ty.span)).collect::<Vec<_>>().join(", ")])
        }
        StructBody::Unit => None,
    };
    let mut out = String::new();
    for item in &parsed.items {
        let name = format!("{name}{}", declared_generics(&text(item.span), mangled));
        match &item.kind {
            ItemKind::Struct(decl) if decl.name.name == mangled => {
                match (&decl.body, shape(&decl.body)) {
                    (StructBody::Tuple(_), Some(fields)) => {
                        out.push_str(&format!("struct {name}({})\n", fields.join(", ")));
                        continue;
                    }
                    (StructBody::Unit, _) => {
                        out.push_str(&format!("struct {name}\n"));
                        continue;
                    }
                    _ => {}
                }
                out.push_str(&format!("struct {name} {{\n"));
                // A field whose name starts `__` is the type's own state, which
                // a program reaches through the methods.
                let fields = shape(&decl.body).unwrap_or_default();
                let public: Vec<&String> =
                    fields.iter().filter(|field| !field.starts_with("__")).collect();
                for field in &public {
                    out.push_str(&format!("    {field}\n"));
                }
                if public.len() < fields.len() {
                    out.push_str("    ..\n");
                }
                out.push_str("}\n");
            }
            ItemKind::Enum(decl) if decl.name.name == mangled => {
                out.push_str(&format!("enum {name} {{\n"));
                for variant in &decl.variants {
                    let payload = match (&variant.body, shape(&variant.body)) {
                        (StructBody::Named(_), Some(fields)) => format!(" {{ {} }}", fields.join(", ")),
                        (StructBody::Tuple(_), Some(fields)) => format!("({})", fields.join(", ")),
                        _ => String::new(),
                    };
                    out.push_str(&format!("    {}{payload}\n", variant.name.name));
                }
                out.push_str("}\n");
            }
            _ => {}
        }
    }
    if out.is_empty() {
        return None;
    }
    let methods = wrapper_type_methods(&parsed, mangled, &text);
    if !methods.is_empty() {
        out.push('\n');
        for method in methods {
            out.push_str(&format!("{method}\n"));
        }
    }
    Some(demangle_wrapper_names(&out))
}

/// The generic parameters a declaration's text `decl` writes after the name
/// `mangled`, or an empty string when it declares none.
fn declared_generics(decl: &str, mangled: &str) -> String {
    let after = decl.find(mangled).map_or("", |at| &decl[at + mangled.len()..]);
    if !after.starts_with('<') {
        return String::new();
    }
    let mut depth = 0usize;
    for (i, c) in after.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return after[..=i].to_string();
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// The signatures of the inherent methods the wrapper source declares on
/// the type `mangled`, its own `__`-named helpers left out.
fn wrapper_type_methods(
    parsed: &SourceFile,
    mangled: &str,
    text: &impl Fn(Span) -> String,
) -> Vec<String> {
    parsed
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Impl(decl)
                if decl.trait_ref.is_none()
                    && text(decl.self_ty.span)
                        .strip_prefix(mangled)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('<')) =>
            {
                Some(decl)
            }
            _ => None,
        })
        .flat_map(|decl| decl.items.iter())
        .filter_map(|member| match member {
            ImplItem::Fn(f) if !f.name.name.starts_with("__") => {
                let signature = text(f.span);
                let cut = signature.find('{').unwrap_or(signature.len());
                Some(signature[..cut].trim().to_string())
            }
            _ => None,
        })
        .collect()
}

/// `text` with each injected stdlib name spelled as a program writes it
/// (`time::CivilTime` for `__gos_time_CivilTime`).
fn demangle_wrapper_names(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("__gos_") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        let mangled = &tail[..end];
        match stdlib_wrapper_spelling(mangled) {
            Some(spelling) => out.push_str(&spelling),
            None => out.push_str(mangled),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The spelling a program writes for the injected stdlib type `mangled`
/// (`fs::Metadata` for `__gos_fs_Metadata`), which its rendering shows.
fn public_stdlib_type_spelling(mangled: &str) -> Option<String> {
    MANGLED_STDLIB_NAMES
        .iter()
        .find(|(_, item, name)| *name == mangled && item.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|(parent, item, _)| format!("{}::{item}", spelled_module(parent)))
}

/// Collapses the `form::Form::parse` associated-function path onto its
/// injected name, guarded on the `form` head so a user's own `Form::parse`
/// is left alone. Returns true when it rewrote `path`.
fn collapse_http_security_path(path: &mut gossamer_ast::PathExpr) -> bool {
    let n = path.segments.len();
    if n < 3 {
        return false;
    }
    if path.segments[n - 3].name.name.as_str() == "form"
        && path.segments[n - 2].name.name.as_str() == "Form"
        && path.segments[n - 1].name.name.as_str() == "parse"
    {
        path.segments = vec![gossamer_ast::PathSegment::new("__gos_http_form_parse")];
        return true;
    }
    false
}

/// Rewrites `recv.form_file(name)` into a call of the injected
/// `__gos_http_request_form_file(recv, name)` free wrapper. The
/// `form_file` source marker is what pulled the multipart wrappers in,
/// so the rewrite only ever fires when they are present.
fn rewrite_form_file_method(expr: &mut gossamer_ast::expr::Expr) {
    use gossamer_ast::expr::{Expr, ExprKind};
    let span = expr.span;
    let ExprKind::MethodCall {
        receiver, mut args, ..
    } = std::mem::replace(&mut expr.kind, ExprKind::Tuple(Vec::new()))
    else {
        return;
    };
    let mut call_args = Vec::with_capacity(2);
    call_args.push(*receiver);
    call_args.append(&mut args);
    let callee = Expr {
        id: NodeId::DUMMY,
        span,
        kind: ExprKind::Path(gossamer_ast::PathExpr {
            segments: vec![gossamer_ast::PathSegment::new(
                "__gos_http_request_form_file",
            )],
        }),
    };
    expr.kind = ExprKind::Call {
        callee: Box::new(callee),
        args: call_args,
    };
}

/// Reports whether `e` is a place expression cheap and side-effect-free
/// to evaluate more than once (a name, field, or constant-indexed chain).
fn is_reevaluable_place(e: &gossamer_ast::expr::Expr) -> bool {
    use gossamer_ast::expr::ExprKind;
    match &e.kind {
        ExprKind::Path(_) => true,
        ExprKind::FieldAccess { receiver, .. } => is_reevaluable_place(receiver),
        ExprKind::Index { base, index } => {
            is_reevaluable_place(base)
                && matches!(index.kind, ExprKind::Literal(_) | ExprKind::Path(_))
        }
        _ => false,
    }
}

/// Wraps `place = set_call; place` into a value-yielding block so a
/// mutator rewrite both persists the update and stays usable in
/// expression position.
fn writeback_block(
    place: gossamer_ast::expr::Expr,
    set_call: gossamer_ast::expr::Expr,
    span: gossamer_lex::Span,
) -> gossamer_ast::expr::ExprKind {
    use gossamer_ast::common::AssignOp;
    use gossamer_ast::expr::{Block, Expr, ExprKind};
    use gossamer_ast::stmt::{Stmt, StmtKind};
    let assign = Expr {
        id: NodeId::DUMMY,
        span,
        kind: ExprKind::Assign {
            op: AssignOp::Assign,
            place: Box::new(place.clone()),
            value: Box::new(set_call),
        },
    };
    ExprKind::Block(Block {
        stmts: vec![Stmt::new(
            NodeId::DUMMY,
            span,
            StmtKind::Expr {
                expr: Box::new(assign),
                has_semi: true,
            },
        )],
        tail: Some(Box::new(place)),
        synthetic: true,
        kind: gossamer_ast::BlockKind::Plain,
    })
}

/// Rewrites a `json::set(&mut place, key, value)` mutator call into
/// `{ place = json::set(place, key, value); place }` so the in-place
/// update persists. `json::set` is a functional helper (it returns a
/// new `json::Value`); the `&mut place` spelling reads as a mutation,
/// so the returned value must be written back to `place`. The block
/// also yields the updated value, keeping the call usable in
/// expression position. The functional form (`let x = json::set(obj,
/// k, v)`, no `&mut`) is left untouched. Returns `true` when it fired.
fn rewrite_json_set_mutator(expr: &mut gossamer_ast::expr::Expr) -> bool {
    use gossamer_ast::common::UnaryOp;
    use gossamer_ast::expr::{Expr, ExprKind};

    let ExprKind::Call { callee, args } = &expr.kind else {
        return false;
    };
    if args.len() != 3 {
        return false;
    }
    let ExprKind::Path(path) = &callee.kind else {
        return false;
    };
    let segs: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    let is_json_set = matches!(
        segs.as_slice(),
        ["json", "set"] | ["encoding", "json", "set"] | ["std", "encoding", "json", "set"]
    );
    if !is_json_set {
        return false;
    }
    // First arg must be `&mut place` / `&place` over a place expression
    // that is safe to re-evaluate (a name, field, or index chain - no
    // calls). Anything else keeps the functional semantics.
    let ExprKind::Unary {
        op: UnaryOp::RefMut | UnaryOp::RefShared,
        operand,
    } = &args[0].kind
    else {
        return false;
    };
    if !is_reevaluable_place(operand) {
        return false;
    }
    let place = (**operand).clone();
    let span = expr.span;
    let ExprKind::Call { callee, mut args } =
        std::mem::replace(&mut expr.kind, ExprKind::Tuple(Vec::new()))
    else {
        unreachable!("matched Call above");
    };
    // Replace the `&mut place` first argument with the bare place so
    // the inner call lowers through the ordinary functional path.
    args[0] = place.clone();
    let set_call = Expr {
        id: NodeId::DUMMY,
        span,
        kind: ExprKind::Call { callee, args },
    };
    expr.kind = writeback_block(place, set_call, span);
    true
}

/// Rewrites a trait-qualified call `Trait::method(recv, args..)` of a method
/// the program's trait declares with a receiver into `recv.method(args..)`,
/// with a leading `&` / `&mut` on `recv` dropped since a method call borrows
/// its receiver itself. A type defines each method name once (GT0096), so the
/// method call reaches exactly the body the qualified path names.
pub fn rewrite_trait_qualified_calls(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::common::UnaryOp;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr_mut;

    struct Rewriter {
        receiver_methods: HashMap<String, HashSet<String>>,
    }
    impl VisitorMut for Rewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            let ExprKind::Call { callee, args } = &mut expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &callee.kind else {
                return;
            };
            let [.., trait_segment, method_segment] = path.segments.as_slice() else {
                return;
            };
            let qualifies = self
                .receiver_methods
                .get(&trait_segment.name.name)
                .is_some_and(|methods| methods.contains(&method_segment.name.name));
            if !qualifies || args.is_empty() || !method_segment.generics.is_empty() {
                return;
            }
            let name = method_segment.name.clone();
            let name_span = callee.span;
            let mut rest = std::mem::take(args);
            let first = rest.remove(0);
            let receiver = match first.kind {
                ExprKind::Unary {
                    op: UnaryOp::RefShared | UnaryOp::RefMut,
                    operand,
                } => *operand,
                _ => first,
            };
            expr.kind = ExprKind::MethodCall {
                receiver: Box::new(receiver),
                name,
                name_span,
                desugared_from: None,
                generics: Vec::new(),
                args: rest,
            };
        }
    }
    let mut receiver_methods: HashMap<String, HashSet<String>> = HashMap::new();
    for (_, item) in flatten_items_with_modules(&sf.items) {
        let ItemKind::Trait(decl) = &item.kind else {
            continue;
        };
        let methods = receiver_methods.entry(decl.name.name.clone()).or_default();
        for trait_item in &decl.items {
            if let gossamer_ast::TraitItem::Fn(f) = trait_item
                && matches!(f.params.first(), Some(gossamer_ast::FnParam::Receiver(_)))
            {
                methods.insert(f.name.name.clone());
            }
        }
    }
    if receiver_methods.values().all(HashSet::is_empty) {
        return;
    }

    Rewriter { receiver_methods }.visit_source_file(sf);
}

/// Rewrites `(lo..hi).contains(x)` into comparisons against its bounds,
/// evaluated once each in source order: a range written in place has no
/// other use for its bounds, and the comparison is the same code on every
/// tier. A range held in a binding is iteration state and has no `contains`.
pub fn rewrite_range_contains(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::common::{BinaryOp, RangeKind};
    use gossamer_ast::expr::{Block, Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr_mut;

    use range_contains_nodes::{bind, binary, name_expr};

    struct Rewriter;
    impl VisitorMut for Rewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            let ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } = &expr.kind
            else {
                return;
            };
            if name.name != "contains" || args.len() != 1 {
                return;
            }
            let ExprKind::Range { start, end, kind } = &receiver.kind else {
                return;
            };
            if start.is_none() && end.is_none() {
                return;
            }
            let span = expr.span;
            let mut stmts = Vec::new();
            let mut tests = Vec::new();
            if let Some(start) = start {
                stmts.push(bind("__gos_range_lo", (**start).clone(), span));
            }
            if let Some(end) = end {
                stmts.push(bind("__gos_range_hi", (**end).clone(), span));
            }
            stmts.push(bind("__gos_range_x", args[0].clone(), span));
            if start.is_some() {
                tests.push(binary(
                    BinaryOp::Ge,
                    name_expr("__gos_range_x", span),
                    name_expr("__gos_range_lo", span),
                    span,
                ));
            }
            if end.is_some() {
                let op = if matches!(kind, RangeKind::Inclusive) {
                    BinaryOp::Le
                } else {
                    BinaryOp::Lt
                };
                tests.push(binary(
                    op,
                    name_expr("__gos_range_x", span),
                    name_expr("__gos_range_hi", span),
                    span,
                ));
            }
            let mut tests = tests.into_iter();
            let Some(first) = tests.next() else {
                return;
            };
            let test = tests.fold(first, |all, next| binary(BinaryOp::And, all, next, span));
            expr.kind = ExprKind::Block(Block {
                stmts,
                tail: Some(Box::new(test)),
                synthetic: true,
                kind: gossamer_ast::BlockKind::Plain,
            });
        }
    }
    Rewriter.visit_source_file(sf);
}

/// The nodes `rewrite_range_contains` builds.
mod range_contains_nodes {
    use gossamer_ast::NodeId;
    use gossamer_ast::common::{BinaryOp, Mutability};
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::pattern::{Pattern, PatternKind};
    use gossamer_ast::stmt::{Stmt, StmtKind};

    pub(super) fn name_expr(name: &str, span: gossamer_lex::Span) -> Expr {
        Expr {
            id: NodeId::DUMMY,
            span,
            kind: ExprKind::Path(gossamer_ast::PathExpr {
                segments: vec![gossamer_ast::PathSegment::new(name)],
            }),
        }
    }

    pub(super) fn binary(op: BinaryOp, lhs: Expr, rhs: Expr, span: gossamer_lex::Span) -> Expr {
        Expr {
            id: NodeId::DUMMY,
            span,
            kind: ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        }
    }

    pub(super) fn bind(name: &str, init: Expr, span: gossamer_lex::Span) -> Stmt {
        Stmt::new(
            NodeId::DUMMY,
            span,
            StmtKind::Let {
                pattern: Pattern {
                    id: NodeId::DUMMY,
                    span,
                    kind: PatternKind::Ident {
                        mutability: Mutability::Immutable,
                        name: gossamer_ast::Ident::new(name),
                        subpattern: None,
                    },
                },
                ty: None,
                init: Some(Box::new(init)),
            },
        )
    }
}

/// Rewrites the `duplicate_keys:` argument of `json::parse`, `yaml::parse`,
/// and their typed `decode::<T>` forms into the policy's index, which the
/// runtime reads on every tier: `json::parse(t, duplicate_keys: d)` becomes
/// `json::__parse_with(t, __gos_json_duplicate_index(d))`, and a decode keeps
/// its label for its stub's `duplicate_keys` parameter.
pub fn rewrite_duplicate_keys_args(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr_mut;

    struct Rewriter {
        named: std::collections::HashMap<NodeId, Vec<gossamer_ast::NamedArg>>,
    }
    impl VisitorMut for Rewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            let ExprKind::Call { callee, args } = &mut expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &mut callee.kind else {
                return;
            };
            let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
            let (module, op) = match names.as_slice() {
                [.., module @ ("json" | "yaml"), op] => (Some(*module), *op),
                [op @ ("from_json" | "from_yaml")] => (None, *op),
                _ => return,
            };
            let parses = op == "parse";
            let decodes = matches!(
                (module, op),
                (Some(_), "decode") | (Some("json") | None, "from_json") | (Some("yaml") | None, "from_yaml")
            );
            if !(parses || decodes) || args.len() != 2 {
                return;
            }
            let labels = self.named.get_mut(&expr.id);
            let labelled = labels
                .as_ref()
                .and_then(|labels| labels.iter().position(|l| l.name.name == "duplicate_keys"));
            let index = match labelled {
                Some(at) => labels.as_ref().map_or(1, |labels| labels[at].index),
                None => 1,
            };
            if parses {
                if let Some(last) = path.segments.last_mut() {
                    last.name.name = "__parse_with".to_string();
                }
                // The policy goes second, unlabelled, as the entry takes it.
                if let (Some(labels), Some(at)) = (labels, labelled) {
                    labels.remove(at);
                }
                if index == 0 {
                    args.swap(0, 1);
                }
            }
            let slot = if parses { 1 } else { index };
            let span = args[slot].span;
            let value = std::mem::replace(&mut args[slot], Expr::new(NodeId::DUMMY, span, ExprKind::Tuple(Vec::new())));
            let helper = Expr::new(
                NodeId::DUMMY,
                span,
                ExprKind::Path(gossamer_ast::PathExpr {
                    segments: vec![gossamer_ast::PathSegment::new("__gos_json_duplicate_index")],
                }),
            );
            args[slot] = Expr::new(
                NodeId::DUMMY,
                span,
                ExprKind::Call {
                    callee: Box::new(helper),
                    args: vec![value],
                },
            );
        }
    }
    let mut rewriter = Rewriter {
        named: std::mem::take(&mut sf.named_args),
    };
    rewriter.visit_source_file(sf);
    rewriter.named.retain(|_, labels| !labels.is_empty());
    sf.named_args = rewriter.named;
}

/// Rewrites `http::serve_tls_config(addr, config, handler)` into
/// `http::__serve_tls_live(addr, config.id(), handler)`: the server takes the
/// live configuration by its registry id, as the runtime holds it.
pub fn rewrite_serve_tls_config(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr_mut;

    struct Rewriter;
    impl VisitorMut for Rewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            let ExprKind::Call { callee, args } = &mut expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &mut callee.kind else {
                return;
            };
            let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
            if !matches!(names.as_slice(), [.., "http", "serve_tls_config"]) || args.len() != 3 {
                return;
            }
            if let Some(last) = path.segments.last_mut() {
                last.name.name = "__serve_tls_live".to_string();
            }
            let span = args[1].span;
            let config =
                std::mem::replace(&mut args[1], Expr::new(NodeId::DUMMY, span, ExprKind::Tuple(Vec::new())));
            args[1] = Expr::new(
                NodeId::DUMMY,
                span,
                ExprKind::MethodCall {
                    receiver: Box::new(config),
                    name: gossamer_ast::Ident::new("id"),
                    name_span: span,
                    desugared_from: None,
                    generics: Vec::new(),
                    args: Vec::new(),
                },
            );
        }
    }
    Rewriter.visit_source_file(sf);
}

/// Walks the program rewriting every `json::set(&mut place, k, v)`
/// mutator call into a value-yielding write-back block (see
/// `rewrite_json_set_mutator`).
pub fn rewrite_json_set_mutators(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::Expr;
    use gossamer_ast::visitor::walk_expr_mut;

    struct Rewriter;
    impl VisitorMut for Rewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            rewrite_json_set_mutator(expr);
        }
    }
    Rewriter.visit_source_file(sf);
}
