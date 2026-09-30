//! Whether a statement mutates a session binding.

use super::{
    HashSet, format_parse_diags, format_resolve_diags, format_type_diags, render_repl_declarations,
};

pub(super) fn repl_stmt_mutates_binding(
    stmt: &gossamer_ast::Stmt,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    use gossamer_ast::StmtKind;

    match &stmt.kind {
        StmtKind::Let { init, .. } => init
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
            repl_expr_mutates_binding(expr, user_mutating_methods)
        }
        StmtKind::Item(_) => false,
    }
}

pub(super) fn repl_select_op_contains_ref_mut(op: &gossamer_ast::expr::SelectOp) -> bool {
    use gossamer_ast::expr::SelectOp;

    match op {
        SelectOp::Recv { channel, .. } => repl_expr_contains_ref_mut(channel),
        SelectOp::Send { channel, value } => {
            repl_expr_contains_ref_mut(channel) || repl_expr_contains_ref_mut(value)
        }
        SelectOp::Default => false,
    }
}

pub(super) fn repl_stmt_contains_ref_mut(stmt: &gossamer_ast::Stmt) -> bool {
    use gossamer_ast::StmtKind;

    match &stmt.kind {
        StmtKind::Let { init, .. } => init.as_deref().is_some_and(repl_expr_contains_ref_mut),
        StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => repl_expr_contains_ref_mut(expr),
        StmtKind::Item(_) => false,
    }
}

pub(super) fn repl_expr_contains_ref_mut(expr: &gossamer_ast::Expr) -> bool {
    use gossamer_ast::ExprKind;
    use gossamer_ast::common::UnaryOp;

    match &expr.kind {
        ExprKind::Unary {
            op: UnaryOp::RefMut,
            ..
        } => true,
        ExprKind::Call { callee, args } => {
            repl_expr_contains_ref_mut(callee) || args.iter().any(repl_expr_contains_ref_mut)
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            repl_expr_contains_ref_mut(receiver) || args.iter().any(repl_expr_contains_ref_mut)
        }
        ExprKind::FieldAccess { receiver, .. } => repl_expr_contains_ref_mut(receiver),
        ExprKind::Index { base, index } => {
            repl_expr_contains_ref_mut(base) || repl_expr_contains_ref_mut(index)
        }
        ExprKind::Unary { operand, .. } => repl_expr_contains_ref_mut(operand),
        ExprKind::Binary { lhs, rhs, .. } => {
            repl_expr_contains_ref_mut(lhs) || repl_expr_contains_ref_mut(rhs)
        }
        ExprKind::Assign { place, value, .. } => {
            repl_expr_contains_ref_mut(place) || repl_expr_contains_ref_mut(value)
        }
        ExprKind::Cast { value, .. } | ExprKind::Try(value) => repl_expr_contains_ref_mut(value),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            repl_expr_contains_ref_mut(condition)
                || repl_expr_contains_ref_mut(then_branch)
                || else_branch
                    .as_deref()
                    .is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Match { scrutinee, arms } => {
            repl_expr_contains_ref_mut(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(repl_expr_contains_ref_mut)
                        || repl_expr_contains_ref_mut(&arm.body)
                })
        }
        ExprKind::Loop { body, .. } => repl_expr_contains_ref_mut(body),
        ExprKind::While {
            condition, body, ..
        } => repl_expr_contains_ref_mut(condition) || repl_expr_contains_ref_mut(body),
        ExprKind::For { iter, body, .. } => {
            repl_expr_contains_ref_mut(iter) || repl_expr_contains_ref_mut(body)
        }
        ExprKind::Block(block) | ExprKind::Unsafe(block) => {
            block.stmts.iter().any(repl_stmt_contains_ref_mut)
                || block
                    .tail
                    .as_deref()
                    .is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Closure { body, .. } => repl_expr_contains_ref_mut(body),
        ExprKind::Return(value) => value.as_deref().is_some_and(repl_expr_contains_ref_mut),
        ExprKind::Break { value, .. } => value.as_deref().is_some_and(repl_expr_contains_ref_mut),
        ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => {
            elems.iter().any(repl_expr_contains_ref_mut)
        }
        ExprKind::Struct { fields, base, .. } => {
            fields
                .iter()
                .any(|field| field.value.as_ref().is_some_and(repl_expr_contains_ref_mut))
                || base.as_deref().is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Array(array) | ExprKind::FixedArray(array) => {
            repl_array_expr_contains_ref_mut(array)
        }
        ExprKind::Range { start, end, .. } => {
            start.as_deref().is_some_and(repl_expr_contains_ref_mut)
                || end.as_deref().is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Select(arms) => arms.iter().any(|arm| {
            repl_select_op_contains_ref_mut(&arm.op) || repl_expr_contains_ref_mut(&arm.body)
        }),
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Continue { .. } | ExprKind::Error => {
            false
        }
    }
}

pub(super) fn repl_array_expr_contains_ref_mut(array: &gossamer_ast::expr::ArrayExpr) -> bool {
    match array {
        gossamer_ast::expr::ArrayExpr::List(elems) => elems.iter().any(repl_expr_contains_ref_mut),
        gossamer_ast::expr::ArrayExpr::Repeat { value, count } => {
            repl_expr_contains_ref_mut(value) || repl_expr_contains_ref_mut(count)
        }
    }
}

pub(super) fn repl_expr_mutates_binding(
    expr: &gossamer_ast::Expr,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    use gossamer_ast::ExprKind;

    match &expr.kind {
        ExprKind::Assign { .. } => true,
        ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } => {
            gossamer_types::is_mutating_method_name(&name.name)
                || user_mutating_methods.contains(&name.name)
                || repl_expr_mutates_binding(receiver, user_mutating_methods)
                || args
                    .iter()
                    .any(|arg| repl_expr_mutates_binding(arg, user_mutating_methods))
        }
        ExprKind::Call { callee, args } => {
            repl_callee_is_mutating_name(callee)
                || repl_expr_mutates_binding(callee, user_mutating_methods)
                || args
                    .iter()
                    .any(|arg| repl_expr_mutates_binding(arg, user_mutating_methods))
        }
        ExprKind::For { iter, body, .. } => {
            repl_expr_contains_ref_mut(iter)
                || repl_expr_mutates_binding(body, user_mutating_methods)
        }
        ExprKind::Block(block) | ExprKind::Unsafe(block) => {
            repl_block_mutates_binding(block, user_mutating_methods)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => repl_if_mutates_binding(
            condition,
            then_branch,
            else_branch.as_deref(),
            user_mutating_methods,
        ),
        ExprKind::Match { scrutinee, arms } => {
            repl_match_mutates_binding(scrutinee, arms, user_mutating_methods)
        }
        ExprKind::Loop { body, .. } => repl_expr_mutates_binding(body, user_mutating_methods),
        ExprKind::While {
            condition, body, ..
        } => repl_pair_mutates_binding(condition, body, user_mutating_methods),
        ExprKind::FieldAccess { receiver, .. } => {
            repl_expr_mutates_binding(receiver, user_mutating_methods)
        }
        ExprKind::Index { base, index } => {
            repl_expr_mutates_binding(base, user_mutating_methods)
                || repl_expr_mutates_binding(index, user_mutating_methods)
        }
        ExprKind::Unary { operand, .. } => {
            repl_expr_mutates_binding(operand, user_mutating_methods)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            repl_expr_mutates_binding(lhs, user_mutating_methods)
                || repl_expr_mutates_binding(rhs, user_mutating_methods)
        }
        ExprKind::Cast { value, .. } | ExprKind::Try(value) => {
            repl_expr_mutates_binding(value, user_mutating_methods)
        }
        ExprKind::Closure { body, .. } => repl_expr_mutates_binding(body, user_mutating_methods),
        ExprKind::Return(value) => value
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        ExprKind::Break { value, .. } => value
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => elems
            .iter()
            .any(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        ExprKind::Struct { fields, base, .. } => {
            repl_struct_expr_mutates_binding(fields, base.as_deref(), user_mutating_methods)
        }
        ExprKind::Array(array) | ExprKind::FixedArray(array) => {
            repl_array_expr_mutates_binding(array, user_mutating_methods)
        }
        ExprKind::Range { start, end, .. } => repl_optional_pair_mutates_binding(
            start.as_deref(),
            end.as_deref(),
            user_mutating_methods,
        ),
        ExprKind::Select(arms) => arms
            .iter()
            .any(|arm| repl_expr_mutates_binding(&arm.body, user_mutating_methods)),
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Continue { .. } | ExprKind::Error => {
            false
        }
    }
}

pub(super) fn repl_block_mutates_binding(
    block: &gossamer_ast::expr::Block,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    block
        .stmts
        .iter()
        .any(|stmt| repl_stmt_mutates_binding(stmt, user_mutating_methods))
        || block
            .tail
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

pub(super) fn repl_if_mutates_binding(
    condition: &gossamer_ast::Expr,
    then_branch: &gossamer_ast::Expr,
    else_branch: Option<&gossamer_ast::Expr>,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    repl_pair_mutates_binding(condition, then_branch, user_mutating_methods)
        || else_branch.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

pub(super) fn repl_match_mutates_binding(
    scrutinee: &gossamer_ast::Expr,
    arms: &[gossamer_ast::expr::MatchArm],
    user_mutating_methods: &HashSet<String>,
) -> bool {
    repl_expr_mutates_binding(scrutinee, user_mutating_methods)
        || arms.iter().any(|arm| {
            arm.guard
                .as_ref()
                .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
                || repl_expr_mutates_binding(&arm.body, user_mutating_methods)
        })
}

pub(super) fn repl_pair_mutates_binding(
    left: &gossamer_ast::Expr,
    right: &gossamer_ast::Expr,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    repl_expr_mutates_binding(left, user_mutating_methods)
        || repl_expr_mutates_binding(right, user_mutating_methods)
}

pub(super) fn repl_optional_pair_mutates_binding(
    left: Option<&gossamer_ast::Expr>,
    right: Option<&gossamer_ast::Expr>,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    left.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
        || right.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

pub(super) fn repl_struct_expr_mutates_binding(
    fields: &[gossamer_ast::expr::StructExprField],
    base: Option<&gossamer_ast::Expr>,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    fields.iter().any(|field| {
        field
            .value
            .as_ref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
    }) || base.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

pub(super) fn repl_array_expr_mutates_binding(
    array: &gossamer_ast::expr::ArrayExpr,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    match array {
        gossamer_ast::expr::ArrayExpr::List(elems) => elems
            .iter()
            .any(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        gossamer_ast::expr::ArrayExpr::Repeat { value, count } => {
            repl_expr_mutates_binding(value, user_mutating_methods)
                || repl_expr_mutates_binding(count, user_mutating_methods)
        }
    }
}

pub(super) fn repl_callee_is_mutating_name(callee: &gossamer_ast::Expr) -> bool {
    let gossamer_ast::ExprKind::Path(path) = &callee.kind else {
        return false;
    };
    path.segments
        .last()
        .is_some_and(|segment| gossamer_types::is_mutating_method_name(&segment.name.name))
}

/// Validates that the accumulated declarations parse, resolve, and
/// compile onto the VM. The built `Vm` is discarded - the REPL keeps
/// declarations as source strings and full-recompiles each input - so
/// this is purely a probe: `Ok(())` means the declaration set is
/// loadable, `Err` rolls back the just-added declaration.
pub(super) fn rebuild_session(declarations: &[String]) -> std::result::Result<(), String> {
    // Parse declarations before appending the synthetic probe function. A
    // missing item body must point at the user's end of input, not at the
    // generated `fn __irepl_probe` that follows it.
    let declarations_source = render_repl_declarations(declarations);
    let mut declarations_map = gossamer_lex::SourceMap::new();
    let declarations_file =
        declarations_map.add_file("<repl>".to_string(), declarations_source.clone());
    let (_, declaration_diags) =
        gossamer_parse::parse_source_file(&declarations_source, declarations_file);
    if !declaration_diags.is_empty() {
        return Err(format_parse_diags(&declaration_diags, &declarations_map));
    }

    let source = render_repl_declarations(declarations) + "\nfn __irepl_probe() { }\n";
    let source = gossamer_parse::autoderive::augment_source(&source);
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
    let (tbl, type_diags) = gossamer_types::typecheck_source_file(&sf, &res, &mut tcx);
    if !type_diags.is_empty() {
        return Err(format_type_diags(&type_diags, &map));
    }
    let program = gossamer_hir::lower_source_file(&sf, &res, &tbl, &mut tcx);
    let mut vm = gossamer_interp::Vm::new();
    // A session's next call is whatever the user types next, so every body
    // stays promotable.
    vm.set_entry_points(&[]);
    vm.load(&program, tcx, true).map_err(|e| format!("{e}"))?;
    Ok(())
}
