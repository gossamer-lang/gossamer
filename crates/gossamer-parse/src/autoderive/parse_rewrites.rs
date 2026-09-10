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

/// Rewrites `open_range.take(n)` into an equivalent finite range. The runtime
/// currently materializes range values eagerly, so retaining an unbounded end
/// would either allocate forever or silently collapse to an empty array. This
/// preserves lazy-looking bounded consumption without changing the range ABI.
pub fn rewrite_open_range_take(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    OpenRangeTakeRewriter.visit_source_file(sf);
}

struct OpenRangeTakeRewriter;

impl gossamer_ast::VisitorMut for OpenRangeTakeRewriter {
    fn visit_expr(&mut self, expr: &mut gossamer_ast::expr::Expr) {
        use gossamer_ast::common::BinaryOp;
        use gossamer_ast::expr::{Expr, ExprKind, Literal};
        use gossamer_ast::NodeId;

        gossamer_ast::visitor::walk_expr_mut(self, expr);
        let span = expr.span;
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return;
        };
        if name.name != "take" || args.len() != 1 {
            return;
        }
        let ExprKind::Range {
            start,
            end: None,
            kind,
        } = &receiver.kind
        else {
            return;
        };

        let start = start.as_deref().cloned().unwrap_or_else(|| Expr {
            id: NodeId::DUMMY,
            span,
            kind: ExprKind::Literal(Literal::Int("0".to_string())),
        });
        let plus_count = Expr {
            id: NodeId::DUMMY,
            span,
            kind: ExprKind::Binary {
                op: BinaryOp::Add,
                lhs: Box::new(start.clone()),
                rhs: Box::new(args[0].clone()),
            },
        };
        let end = if *kind == gossamer_ast::RangeKind::Inclusive {
            Expr {
                id: NodeId::DUMMY,
                span,
                kind: ExprKind::Binary {
                    op: BinaryOp::Sub,
                    lhs: Box::new(plus_count),
                    rhs: Box::new(Expr {
                        id: NodeId::DUMMY,
                        span,
                        kind: ExprKind::Literal(Literal::Int("1".to_string())),
                    }),
                },
            }
        } else {
            plus_count
        };
        expr.kind = ExprKind::Range {
            start: Some(Box::new(start)),
            end: Some(Box::new(end)),
            kind: *kind,
        };
    }
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
    let (mut sf, mut diags) = crate::parse_source_file(source, file);
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
    initialize_heap_mut_statics(&mut sf);
    rewrite_open_range_take(&mut sf);
    infer_serde_turbofish(&mut sf);
    desugar_sort_by_key(&mut sf);
    hoist_associated_consts(&mut sf);
    // Runs on the un-mangled AST: `rewrite_serde_generic_calls` below turns a
    // serde turbofish into a bare mangled name, erasing the type argument the
    // check keys on.
    diags.extend(serde_unsupported_field_diags(&sf));
    rewrite_serde_generic_calls(&mut sf);
    specialize_inline_for_generics(&mut sf);
    expand_typeinfo_loops(&mut sf);
    rewrite_type_info_calls(&mut sf);
    rewrite_json_set_mutators(&mut sf);
    rewrite_stdlib_struct_surface(&mut sf);
    // `rewrite_stdlib_struct_surface` turns public qualified wrapper names
    // such as `sql::Rows` and `encoding::pem::Block` into the injected local
    // structs. Run constructor conversion again now that those paths are
    // local, so their canonical `Type(args...)` spelling becomes the field
    // aggregate expected by every lowering tier.
    rewrite_tuple_struct_ctors(&mut sf);
    inject_synthetic_uses(&mut sf, file);
    (sf, diags)
}

/// Reports serde turbofish calls (`to_json::<T>(v)`, `from_json::<T>(s)`, and
/// the toml/yaml forms) whose struct `T` has a field the synthesizer cannot
/// classify. Such a struct is silently skipped by `synthesize_serde_impls`, so
/// without this the call would surface only as an opaque unknown-name error.
/// Gated on use - a struct with an unsupported field is fine until it actually
/// flows through a serde call - and deduplicated to one diagnostic per struct,
/// pointing at the first offending field.
fn serde_unsupported_field_diags(sf: &SourceFile) -> Vec<ParseDiagnostic> {
    let struct_names: HashMap<String, TyId> = struct_identities(&sf.items);
    let aliases = alias_targets(&sf.items);
    let decls: HashMap<&str, &StructDecl> = flatten_items(&sf.items)
        .into_iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Struct(decl) if decl.generics.params.is_empty() => {
                Some((decl.name.name.as_str(), decl))
            }
            _ => None,
        })
        .collect();
    // `augment_source` has already appended a `__gos_serde_to_json_<T>` for every
    // struct the synthesizer accepted (and the user may hand-provide one), so its
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
    let resolved = |written: &str| -> String {
        symbols
            .get(written)
            .cloned()
            .unwrap_or_else(|| written.to_string())
    };

    let mut diags = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    for (op, ty_name, call_span) in collect_serde_turbofish_calls(sf) {
        let symbol = resolved(&ty_name);
        if reported.contains(&ty_name) || synthesized.contains(symbol.as_str()) {
            continue;
        }
        let Some(decl) = decls.get(symbol.as_str()).or_else(|| decls.get(ty_name.as_str())) else {
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
                FieldKind::from_type(&f.ty, &struct_names, &aliases)
                    .is_none()
                    .then(|| (f.name.name.clone(), ty_to_string(&f.ty), f.ty.span))
            }),
            StructBody::Tuple(fields) => fields.iter().enumerate().find_map(|(i, f)| {
                FieldKind::from_type(&f.ty, &struct_names, &aliases)
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
/// synthesized codec exists for it.
fn refusal_for(sf: &SourceFile, symbol: &str) -> SerdeTargetRefusal {
    for (module, item) in flatten_items_with_modules(&sf.items) {
        let (name, generic, is_enum) = match &item.kind {
            ItemKind::Struct(decl) => (&decl.name.name, !decl.generics.params.is_empty(), false),
            ItemKind::Enum(decl) => (&decl.name.name, !decl.generics.params.is_empty(), true),
            _ => continue,
        };
        if TyId::new(&module, name).symbol != symbol && name != symbol {
            continue;
        }
        return match (is_enum, generic) {
            (true, _) => SerdeTargetRefusal::Enum,
            (false, true) => SerdeTargetRefusal::Generic,
            (false, false) => SerdeTargetRefusal::Unsupported,
        };
    }
    SerdeTargetRefusal::NotAStruct
}

/// Collects `(op, type_name)` for every serde turbofish call in `sf`
/// (`to_json::<T>` / `from_json::<T>` and the toml/yaml forms, bare or
/// format-module-qualified), on the un-mangled AST.
fn collect_serde_turbofish_calls(sf: &SourceFile) -> Vec<(String, String, Span)> {
    use gossamer_ast::Visitor;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr;

    struct Collector {
        calls: Vec<(String, String, Span)>,
    }
    impl Visitor for Collector {
        fn visit_expr(&mut self, expr: &Expr) {
            walk_expr(self, expr);
            let ExprKind::Call { callee, .. } = &expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &callee.kind else {
                return;
            };
            let (op, seg) = match path.segments.len() {
                1 => {
                    let seg = &path.segments[0];
                    (seg.name.name.clone(), seg)
                }
                2 => {
                    let head = path.segments[0].name.name.as_str();
                    let tail = path.segments[1].name.name.as_str();
                    let Some(op) = crate::autoderive::qualified_serde_op(head, tail) else {
                        return;
                    };
                    (op.to_string(), &path.segments[1])
                }
                _ => return,
            };
            if !matches!(
                op.as_str(),
                "to_json" | "from_json" | "to_toml" | "from_toml" | "to_yaml" | "from_yaml"
            ) || seg.generics.len() != 1
            {
                return;
            }
            let GenericArg::Type(ty) = &seg.generics[0] else {
                return;
            };
            let TypeKind::Path(tp) = &ty.kind else {
                return;
            };
            // The spelling as written, which is what the symbol index keys a
            // module-qualified type by. Two modules may each declare a `Cell`,
            // and the leaf alone names neither.
            let written: Vec<&str> = tp
                .segments
                .iter()
                .map(|segment| segment.name.name.as_str())
                .filter(|segment| !matches!(*segment, "crate" | "self" | "super" | "root"))
                .collect();
            if written.is_empty() {
                return;
            }
            self.calls.push((op, written.join("::"), callee.span));
        }
    }

    let mut collector = Collector { calls: Vec::new() };
    collector.visit_source_file(sf);
    collector.calls
}

/// Maps a stdlib `module::item` (matched on the last two segment
/// names) to the mangled name of the injected wrapper / struct, so
/// both `encoding::pem::decode` and the bare `pem::decode` map.
fn mangled_stdlib_name(parent: &str, item: &str) -> Option<&'static str> {
    match (parent, item) {
        ("pem", "decode") => Some("__gos_pem_decode"),
        ("pem", "decode_all") => Some("__gos_pem_decode_all"),
        ("pem", "encode") => Some("__gos_pem_encode"),
        ("pem", "Block") => Some("__gos_pem_Block"),
        ("x509", "parse_pem") => Some("__gos_x509_parse_pem"),
        ("x509", "CertInfo") => Some("__gos_x509_CertInfo"),
        ("fs", "metadata") => Some("__gos_fs_metadata"),
        ("fs", "Metadata") => Some("__gos_fs_Metadata"),
        ("path", "Path") => Some("__gos_path_Path"),
        ("http", "Http2Config") => Some("__gos_http_Http2Config"),
        ("Http2Config", "default") => Some("__gos_http_Http2Config_default"),
        ("time", "Location") => Some("__gos_time_Location"),
        ("time", "CivilTime") => Some("__gos_time_CivilTime"),
        ("time", "CivilResolution") => Some("__gos_time_CivilResolution"),
        ("time", "format_in") => Some("__gos_time_format_in"),
        ("time", "add_date") => Some("__gos_time_add_date"),
        // tar/zip `read` route through the struct wrapper; `write`
        // lowers directly (no struct), so it is NOT rewritten.
        ("tar", "read") => Some("__gos_tar_read"),
        ("tar", "TarEntry") => Some("__gos_tar_TarEntry"),
        ("zip", "read") => Some("__gos_zip_read"),
        ("zip", "ZipEntry") => Some("__gos_zip_ZipEntry"),
        ("sql", "open") => Some("__gos_sql_open"),
        ("sql", "drivers") => Some("__gos_sql_drivers"),
        ("sql", "Conn") => Some("__gos_sql_Conn"),
        ("sql", "Rows") => Some("__gos_sql_Rows"),
        ("sql", "Row") => Some("__gos_sql_Row"),
        ("sql", "Tx") => Some("__gos_sql_Tx"),
        ("sql", "Value") => Some("__gos_sql_Value"),
        ("sql", "IsolationLevel") => Some("__gos_sql_IsolationLevel"),
        ("sql", "Stmt") => Some("__gos_sql_Stmt"),
        ("sql", "Pool") => Some("__gos_sql_Pool"),
        ("sql", "Notification") => Some("__gos_sql_Notification"),
        ("sql", "Select") => Some("__gos_sql_Select"),
        ("sql", "pool_open") => Some("__gos_sql_pool_open"),
        ("sql", "pool_open_with") => Some("__gos_sql_pool_open_with"),
        ("sql", "migrate_up") => Some("__gos_sql_migrate_up"),
        // Gossamer-native driver dispatch: `register_native` captures
        // the driver's env + dispatch fn-address (custom MIR lowering,
        // hooked on the mangled leaf name); the `native_*` /
        // `value_*` helpers are the side-channel a `.gos` driver reads
        // and writes through.
        ("sql", "register_native") => Some("__gos_sql_register_native"),
        ("sql", "native_url") => Some("__gos_sql_native_url"),
        ("sql", "native_sql") => Some("__gos_sql_native_sql"),
        ("sql", "native_parent") => Some("__gos_sql_native_parent"),
        ("sql", "native_out_handle") => Some("__gos_sql_native_out_handle"),
        ("sql", "native_iso") => Some("__gos_sql_native_iso"),
        ("sql", "native_timeout") => Some("__gos_sql_native_timeout"),
        ("sql", "native_channel") => Some("__gos_sql_native_channel"),
        ("sql", "native_param_count") => Some("__gos_sql_native_param_count"),
        ("sql", "native_param") => Some("__gos_sql_native_param"),
        ("sql", "native_data") => Some("__gos_sql_native_data"),
        ("sql", "native_push_column") => Some("__gos_sql_native_push_column"),
        ("sql", "native_push_value") => Some("__gos_sql_native_push_value"),
        ("sql", "native_row_ready") => Some("__gos_sql_native_row_ready"),
        ("sql", "native_set_error") => Some("__gos_sql_native_set_error"),
        ("sql", "native_emit_bytes") => Some("__gos_sql_native_emit_bytes"),
        ("sql", "native_set_notification") => Some("__gos_sql_native_set_notification"),
        ("sql", "native_set_handle") => Some("__gos_sql_native_set_handle"),
        ("sql", "native_handle") => Some("__gos_sql_native_handle"),
        ("sql", "value_null") => Some("__gos_sql_native_value_null"),
        ("sql", "value_bool") => Some("__gos_sql_native_value_bool"),
        ("sql", "value_int") => Some("__gos_sql_native_value_int"),
        ("sql", "value_float") => Some("__gos_sql_native_value_float"),
        ("sql", "value_text") => Some("__gos_sql_native_value_text"),
        ("sql", "value_blob") => Some("__gos_sql_native_value_blob"),
        ("sql", "value_kind") => Some("__gos_sql_native_value_kind"),
        ("sql", "value_int_of") => Some("__gos_sql_native_value_int_of"),
        ("sql", "value_float_of") => Some("__gos_sql_native_value_float_of"),
        ("sql", "value_text_of") => Some("__gos_sql_native_value_text_of"),
        ("sql", "value_blob_of") => Some("__gos_sql_native_value_blob_of"),
        // Channel-returning timer: `time::after(d)` fires on a goroutine that
        // sleeps then sends, so the result is usable in `select` / `while let`.
        ("time", "after") => Some("__gos_time_after"),
        // Cancellation-shielded and time-bounded work, written with
        // `cohort` + `spawn` because that is what the two primitives
        // already express (SPEC 8.6's addition test).
        ("sync", "shield") => Some("__gos_sync_shield"),
        ("sync", "with_timeout") => Some("__gos_sync_with_timeout"),
        // std::http::csrf request/response-integrated surface.
        ("csrf", "Config") => Some("__gos_http_csrf_Config"),
        ("csrf", "config") => Some("__gos_http_csrf_config"),
        ("csrf", "RouteAuth") => Some("__gos_http_csrf_RouteAuth"),
        ("csrf", "extract_token") => Some("__gos_http_csrf_extract_token"),
        ("csrf", "origin_allowed") => Some("__gos_http_csrf_origin_allowed"),
        ("csrf", "check") => Some("__gos_http_csrf_check"),
        ("csrf", "attach_cookie") => Some("__gos_http_csrf_attach_cookie"),
        // std::http::session signed + AES-GCM store surface.
        ("session", "Store") => Some("__gos_http_session_Store"),
        ("session", "signed") => Some("__gos_http_session_signed"),
        ("session", "encrypted") => Some("__gos_http_session_encrypted"),
        ("session", "save") => Some("__gos_http_session_save"),
        ("session", "load") => Some("__gos_http_session_load"),
        ("session", "with_session") => Some("__gos_http_session_with_session"),
        // std::http::form url-encoded parser.
        ("form", "Form") => Some("__gos_http_form_Form"),
        ("form", "parse") => Some("__gos_http_form_parse"),
        ("form", "get") => Some("__gos_http_form_get"),
        ("form", "get_all") => Some("__gos_http_form_get_all"),
        ("form", "has") => Some("__gos_http_form_has"),
        ("form", "count") => Some("__gos_http_form_count"),
        // std::http::multipart (multipart/form-data) parser.
        ("multipart", "Part") => Some("__gos_http_multipart_Part"),
        ("multipart", "parse") => Some("__gos_http_multipart_parse"),
        ("multipart", "boundary") => Some("__gos_http_multipart_boundary"),
        _ => None,
    }
}

/// Collapses the `csrf::RouteAuth::X` enum-variant and the
/// `form::Form::parse` associated-function paths onto their injected
/// names, guarded on the `csrf` / `form` head so a user's own
/// `RouteAuth::X` / `Form::parse` is left alone. Returns true when it
/// rewrote `path`.
fn collapse_http_security_path(path: &mut gossamer_ast::PathExpr) -> bool {
    let n = path.segments.len();
    if n < 3 {
        return false;
    }
    if path.segments[n - 3].name.name.as_str() == "csrf"
        && path.segments[n - 2].name.name.as_str() == "RouteAuth"
    {
        let variant = std::mem::replace(
            &mut path.segments[n - 1],
            gossamer_ast::PathSegment::new(""),
        );
        path.segments = vec![
            gossamer_ast::PathSegment::new("__gos_http_csrf_RouteAuth"),
            variant,
        ];
        return true;
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
