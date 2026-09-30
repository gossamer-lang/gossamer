//! Building and running an input, and typing its tail expression.

use super::ReplValueType;

pub(super) fn build_and_call(
    source: &str,
    entry: &str,
) -> std::result::Result<gossamer_interp::Value, String> {
    build_and_call_with_type_inner(source, entry, false).map(|(value, _)| value)
}

pub(super) fn build_and_call_with_type(
    source: &str,
    entry: &str,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    build_and_call_with_type_inner(source, entry, false)
}

pub(super) fn build_and_call_with_type_for_inspection(
    source: &str,
    entry: &str,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    build_and_call_with_type_inner(source, entry, true)
}

pub(super) fn infer_repl_tail_type(source: &str) -> std::result::Result<ReplValueType, String> {
    let source = gossamer_parse::autoderive::augment_source(source);
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&source, file);
    if !parse_diags.is_empty() {
        return Err(format_parse_diags(&parse_diags, &map));
    }
    let (res, resolve_diags) = gossamer_resolve::resolve_source_file(&sf);
    if !resolve_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &resolve_diags, &map));
    }
    // A labelled or defaulted argument and a std function named in value
    // position are caller-side spellings, rewritten into the one shape the
    // checker and every tier lower. The REPL drives the front-end phase by
    // phase rather than through `check_frontend`, so it runs the shared
    // normalisation itself to see the same calls a file does.
    let named_arg_diags = gossamer_types::normalize_caller_side_spellings(&mut sf, &res);
    if !named_arg_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &named_arg_diags, &map));
    }
    let mut tcx = gossamer_types::TyCtxt::new();
    let (tbl, type_diags) =
        gossamer_types::typecheck_source_file_for_repl_inspection(&sf, &res, &mut tcx);
    let tail_spans = repl_tail_diag_spans(&sf);
    let user_type_diags: Vec<_> = type_diags
        .iter()
        .filter(|diag| !is_implicit_repl_tail_diag(diag, &tail_spans))
        .collect();
    if !user_type_diags.is_empty() {
        return Err(format_type_diags(&user_type_diags, &map));
    }
    let tail_ty_id = repl_generated_tail_expr(&sf).and_then(|expr| tbl.get(expr.id));
    Ok(tail_ty_id.map_or_else(ReplValueType::unknown, |ty| {
        ReplValueType::from_ty(&tcx, ty)
    }))
}

pub(super) fn build_and_call_with_type_inner(
    source: &str,
    entry: &str,
    inspection: bool,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    let source = gossamer_parse::autoderive::augment_source(source);
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&source, file);
    if !parse_diags.is_empty() {
        return Err(format_parse_diags(&parse_diags, &map));
    }
    let (res, resolve_diags) = gossamer_resolve::resolve_source_file(&sf);
    if !resolve_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &resolve_diags, &map));
    }
    // A labelled or defaulted argument and a std function named in value
    // position are caller-side spellings, rewritten into the one shape the
    // checker and every tier lower. The REPL drives the front-end phase by
    // phase rather than through `check_frontend`, so it runs the shared
    // normalisation itself to see the same calls a file does.
    let named_arg_diags = gossamer_types::normalize_caller_side_spellings(&mut sf, &res);
    if !named_arg_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &named_arg_diags, &map));
    }
    let mut tcx = gossamer_types::TyCtxt::new();
    let (tbl, type_diags) = if inspection {
        gossamer_types::typecheck_source_file_for_repl_inspection(&sf, &res, &mut tcx)
    } else {
        gossamer_types::typecheck_source_file(&sf, &res, &mut tcx)
    };
    // REPL expressions are installed as the tail of a generated function with
    // no written return annotation. The REPL deliberately returns that value
    // as REPL output, so its tail is neither discarded nor a user error.
    // Suppress only that exact generated-body diagnostic, never one from the
    // submitted expression's children or declarations.
    let tail_spans = repl_tail_diag_spans(&sf);
    let user_type_diags: Vec<_> = type_diags
        .iter()
        .filter(|diag| !is_implicit_repl_tail_diag(diag, &tail_spans))
        .collect();
    if !user_type_diags.is_empty() {
        return Err(format_type_diags(&user_type_diags, &map));
    }
    let tail_ty_id = repl_generated_tail_expr(&sf).and_then(|expr| tbl.get(expr.id));
    let tail_ty = tail_ty_id.map_or_else(ReplValueType::unknown, |ty| {
        ReplValueType::from_ty(&tcx, ty)
    });
    let mut program = gossamer_hir::lower_source_file(&sf, &res, &tbl, &mut tcx);
    // Generated REPL functions intentionally return their tail even though
    // the user did not write a return annotation. Keep the HIR signature in
    // sync with that inferred tail type so non-inlined calls return aggregate
    // values instead of applying the ordinary implicit-unit ABI.
    if let Some(tail_ty_id) = tail_ty_id {
        for item in &mut program.items {
            if let gossamer_hir::HirItemKind::Fn(function) = &mut item.kind
                && function.name.name == entry
            {
                function.ret = Some(tail_ty_id);
                break;
            }
        }
    }
    let mut vm = gossamer_interp::Vm::new();
    // A session's next call is whatever the user types next, so every body
    // stays promotable.
    vm.set_entry_points(&[]);
    vm.load(&program, tcx, true).map_err(|e| format!("{e}"))?;
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.call(entry, Vec::new())))
            .map_err(repl_panic_message)?;
    gossamer_interp::flush_runtime_stdout();
    result
        .map(|value| (value, tail_ty))
        .map_err(|e| format!("{e}"))
}

pub(super) fn repl_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("panic: {message}")
}

pub(super) fn repl_generated_tail_expr(
    sf: &gossamer_ast::SourceFile,
) -> Option<&gossamer_ast::Expr> {
    use gossamer_ast::{ExprKind, ItemKind};

    sf.items.iter().find_map(|item| {
        let ItemKind::Fn(decl) = &item.kind else {
            return None;
        };
        if !decl.name.name.starts_with("__irepl_") {
            return None;
        }
        let body = decl.body.as_ref()?;
        let ExprKind::Block(block) = &body.kind else {
            return None;
        };
        block.tail.as_deref()
    })
}

/// Span a diagnostic about the generated function's implicit tail carries.
///
/// The tail expression is the REPL's result rather than a discarded value,
/// so a diagnostic anchored to it is suppressed. Diagnostics reach it either
/// through the expression itself or through the enclosing body.
pub(super) fn repl_tail_diag_spans(sf: &gossamer_ast::SourceFile) -> Vec<gossamer_lex::Span> {
    repl_generated_tail_expr(sf)
        .map(|expr| expr.span)
        .into_iter()
        .chain(repl_generated_body_span(sf))
        .collect()
}

pub(super) fn repl_generated_body_span(
    sf: &gossamer_ast::SourceFile,
) -> Option<gossamer_lex::Span> {
    use gossamer_ast::ItemKind;

    sf.items.iter().find_map(|item| {
        let ItemKind::Fn(decl) = &item.kind else {
            return None;
        };
        if !decl.name.name.starts_with("__irepl_") {
            return None;
        }
        decl.body.as_ref().map(|body| body.span)
    })
}

pub(super) fn is_implicit_repl_tail_diag(
    diag: &gossamer_types::TypeDiagnostic,
    tail_spans: &[gossamer_lex::Span],
) -> bool {
    let at_tail = tail_spans.contains(&diag.span);
    match &diag.error {
        gossamer_types::TypeError::TypeMismatch { expected, .. } => expected == "()" && at_tail,
        gossamer_types::TypeError::DiscardedResult => at_tail,
        _ => false,
    }
}

/// Renders a batch of diagnostics with the frame `gos check` and the LSP
/// produce, so one mistake reads the same wherever it is reported: stable
/// code, primary span, notes, helps, and fix suggestions.
pub(super) fn format_diagnostics(
    diags: &[gossamer_diagnostics::Diagnostic],
    map: &gossamer_lex::SourceMap,
) -> String {
    let options = gossamer_diagnostics::RenderOptions { colour: false };
    let mut out = String::new();
    for diag in diags {
        out.push_str(&gossamer_diagnostics::render(diag, map, options));
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Renders hard type-checker failures before the REPL can lower a program.
/// Keeping this gate here is essential: lowering after a rejected call used
/// to let missing or wrongly typed arguments reach permissive runtime shims,
/// which then silently substituted defaults.
pub(super) fn format_type_diags<D>(diags: &[D], map: &gossamer_lex::SourceMap) -> String
where
    D: std::borrow::Borrow<gossamer_types::TypeDiagnostic>,
{
    let structured = diags
        .iter()
        .map(|diag| diag.borrow().to_diagnostic())
        .collect::<Vec<_>>();
    format_diagnostics(&structured, map)
}

pub(super) fn format_resolve_diags(
    sf: &gossamer_ast::SourceFile,
    diags: &[gossamer_resolve::ResolveDiagnostic],
    map: &gossamer_lex::SourceMap,
) -> String {
    let in_scope = collect_source_file_names(sf);
    let structured = diags
        .iter()
        .map(|diag| diag.to_diagnostic(&in_scope))
        .collect::<Vec<_>>();
    format_diagnostics(&structured, map)
}

pub(super) fn collect_source_file_names(sf: &gossamer_ast::SourceFile) -> Vec<&str> {
    use gossamer_ast::ItemKind;

    let mut out = Vec::new();
    for item in &sf.items {
        let name = match &item.kind {
            ItemKind::Fn(decl) => decl.name.name.as_str(),
            ItemKind::Struct(decl) => decl.name.name.as_str(),
            ItemKind::Enum(decl) => decl.name.name.as_str(),
            ItemKind::Trait(decl) => decl.name.name.as_str(),
            ItemKind::TypeAlias(decl) => decl.name.name.as_str(),
            ItemKind::Const(decl) => decl.name.name.as_str(),
            ItemKind::Static(decl) => decl.name.name.as_str(),
            ItemKind::Mod(decl) => decl.name.name.as_str(),
            ItemKind::Impl(_) | ItemKind::AttrItem(_) => continue,
        };
        out.push(name);
    }
    out
}

/// Renders a parse-diagnostic batch through the shared frame, so the REPL
/// reports the same code, span, and fix as `gos check` and the LSP.
pub(super) fn format_parse_diags(
    diags: &[gossamer_parse::ParseDiagnostic],
    map: &gossamer_lex::SourceMap,
) -> String {
    let structured = diags
        .iter()
        .map(gossamer_parse::ParseDiagnostic::to_diagnostic)
        .collect::<Vec<_>>();
    format_diagnostics(&structured, map)
}
