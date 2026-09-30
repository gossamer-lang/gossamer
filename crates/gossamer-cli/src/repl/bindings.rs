//! Session bindings: rendering, updating, and reading them back from `let` sources.

use super::{
    CoreMethodEntry, HashSet, ReplValueType, base_type_name,
    build_and_call_with_type_for_inspection, collect_source_file_names, core_methods_for,
    format_parse_diags, index_has_facts, infer_repl_tail_type, lane_type_param_names,
    render_repl_binding_value, render_session_type, rendered_type_args, repl_info_matches,
    session_index, signature_example_arguments, signature_suffix, substitute_generic_names,
    symbol_query_matches,
};

/// The session's declarations as one source block, imports first.
///
/// A file's `use` declarations precede its items, and the prompt accepts
/// them in whatever order the session reached them, so the block the REPL
/// assembles puts them back in the order the grammar states.
pub(super) fn render_repl_declarations(declarations: &[String]) -> String {
    let (imports, items): (Vec<&String>, Vec<&String>) = declarations
        .iter()
        .partition(|declaration| declaration.trim_start().starts_with("use "));
    imports
        .into_iter()
        .chain(items)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn render_repl_setup(lets: &[String]) -> String {
    if lets.is_empty() {
        String::new()
    } else {
        let replay = lets
            .iter()
            .map(|line| suppress_replayed_prints(line))
            .collect::<Vec<_>>()
            .join("\n    ");
        format!("{replay}\n    ")
    }
}

pub(super) fn suppress_replayed_prints(input: &str) -> String {
    input
        .replace("println(", "format(")
        .replace("eprintln(", "format(")
        .replace("print(", "format(")
        .replace("eprint(", "format(")
        .replace("println(", "__repl_discard(")
        .replace("eprintln(", "__repl_discard(")
        .replace("print(", "__repl_discard(")
        .replace("eprint(", "__repl_discard(")
}

#[derive(Clone)]
pub(super) struct ReplBinding {
    pub(super) vars: Vec<ReplBindingVar>,
    pub(super) source_index: usize,
}

#[derive(Clone)]
pub(super) struct ReplBindingVar {
    pub(super) name: String,
    pub(super) mutable: bool,
}

pub(super) struct ReplDropPlan {
    pub(super) lets: Vec<String>,
    pub(super) bindings: Vec<ReplBinding>,
    pub(super) dropped_names: Vec<String>,
}

pub(super) fn update_repl_bindings(bindings: &mut Vec<ReplBinding>, new_binding: ReplBinding) {
    if !new_binding.vars.is_empty() {
        for binding in bindings.iter_mut() {
            binding.vars.retain(|var| {
                !new_binding
                    .vars
                    .iter()
                    .any(|new_var| new_var.name == var.name)
            });
        }
        bindings.retain(|binding| !binding.vars.is_empty());
    }
    bindings.push(new_binding);
}

pub(super) fn prepare_repl_drop(
    lets: &[String],
    bindings: &[ReplBinding],
    name: &str,
) -> Option<ReplDropPlan> {
    let target_index = bindings
        .iter()
        .find(|binding| binding.vars.iter().any(|var| var.name == name))?
        .source_index;

    let mut dropped_names = vec![name.to_string()];
    let mut dropped_set = HashSet::from([name.to_string()]);
    let mut later_bindings = bindings
        .iter()
        .filter(|binding| binding.source_index > target_index)
        .collect::<Vec<_>>();
    later_bindings.sort_by_key(|binding| binding.source_index);
    for binding in later_bindings {
        let source = lets
            .get(binding.source_index)
            .map_or("", std::string::String::as_str);
        if dropped_set
            .iter()
            .any(|dropped| source_mentions_binding(source, dropped))
        {
            for var in &binding.vars {
                if dropped_set.insert(var.name.clone()) {
                    dropped_names.push(var.name.clone());
                }
            }
        }
    }

    let surviving_vars = bindings
        .iter()
        .filter(|binding| binding.source_index >= target_index)
        .flat_map(|binding| binding.vars.iter())
        .filter(|var| !dropped_set.contains(&var.name))
        .cloned()
        .collect::<Vec<_>>();
    let scoped_source = lets[target_index..].join("\n    ");
    let replacement = match surviving_vars.as_slice() {
        [] => format!("let _ = {{\n    {scoped_source}\n    ()\n}}"),
        [var] => format!(
            "let {mutability}{name} = {{\n    {scoped_source}\n    {name}\n}}",
            mutability = if var.mutable { "mut " } else { "" },
            name = var.name,
        ),
        vars => {
            let pattern = vars
                .iter()
                .map(|var| {
                    if var.mutable {
                        format!("mut {}", var.name)
                    } else {
                        var.name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let values = vars
                .iter()
                .map(|var| var.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("let ({pattern}) = {{\n    {scoped_source}\n    ({values})\n}}")
        }
    };

    let mut new_lets = lets[..target_index].to_vec();
    new_lets.push(replacement);
    let mut new_bindings = bindings.to_vec();
    for binding in &mut new_bindings {
        binding.vars.retain(|var| !dropped_set.contains(&var.name));
        if binding.source_index >= target_index {
            binding.source_index = target_index;
        }
    }
    new_bindings.retain(|binding| !binding.vars.is_empty());
    Some(ReplDropPlan {
        lets: new_lets,
        bindings: new_bindings,
        dropped_names,
    })
}

pub(super) fn source_mentions_binding(source: &str, name: &str) -> bool {
    source.match_indices(name).any(|(start, _)| {
        let before = source[..start].chars().next_back();
        let after = source[start + name.len()..].chars().next();
        !before.is_some_and(is_ident_continue) && !after.is_some_and(is_ident_continue)
    })
}

pub(super) fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

pub(super) struct ReplDeclarationDropPlan {
    pub(super) declarations: Vec<String>,
    pub(super) dropped_names: Vec<String>,
}

/// Removes the declaration introducing `name`, reporting every name that goes.
///
/// One entry can introduce several names - an enum and its variants - and the
/// entry is the unit the user typed, so it ends whole. Entries that name a
/// departing item go with it: an `impl Point` written separately from `Point`
/// cannot outlive it, so the set closes over mentions until it stops growing.
pub(super) fn prepare_repl_declaration_drop(
    declarations: &[String],
    name: &str,
) -> Option<ReplDeclarationDropPlan> {
    let target = declarations
        .iter()
        .position(|declaration| declaration_declares_name(declaration, name))?;

    let mut removed = HashSet::from([target]);
    let mut dropped_names = declaration_names(&declarations[target]);
    if let Some(position) = dropped_names.iter().position(|other| other == name) {
        dropped_names.swap(0, position);
    }
    let mut dropped_set = dropped_names.iter().cloned().collect::<HashSet<_>>();
    while let Some((index, declaration)) = declarations
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .find(|(_, declaration)| {
            dropped_set
                .iter()
                .any(|dropped| source_mentions_binding(declaration, dropped))
        })
    {
        removed.insert(index);
        for declared in declaration_names(declaration) {
            if dropped_set.insert(declared.clone()) {
                dropped_names.push(declared);
            }
        }
    }

    let declarations = declarations
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .map(|(_, declaration)| declaration.clone())
        .collect();
    Some(ReplDeclarationDropPlan {
        declarations,
        dropped_names,
    })
}

pub(super) fn render_dropped_declaration_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [name] => format!("`{name}`"),
        [first, rest @ ..] => {
            let also = rest
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{first}` and dependent {also}")
        }
    }
}

pub(super) fn render_dropped_binding_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [name] => format!("`{name}`"),
        [first, rest @ ..] => {
            let also = rest
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{first}` and dependent {also}")
        }
    }
}

pub(super) struct RenderedReplBinding {
    pub(super) name: String,
    pub(super) line: String,
}

pub(super) fn render_repl_bindings(
    declarations: &[String],
    lets: &[String],
    bindings: &[ReplBinding],
) -> Vec<RenderedReplBinding> {
    let let_body = render_repl_setup(lets);
    let mut lines = Vec::new();
    for binding in bindings {
        for var in &binding.vars {
            let entry = format!("__irepl_binding_{}", lines.len());
            let source = format!(
                "{}\nfn {entry}() {{ {lets}{name} }}\n",
                render_repl_declarations(declarations),
                lets = let_body,
                name = var.name,
            );
            let (value, ty) = match build_and_call_with_type_for_inspection(&source, &entry) {
                Ok((value, ty)) => (render_repl_binding_value(&value, &ty), ty.rendered),
                Err(msg) => (
                    format!("<error: {}>", msg.lines().next().unwrap_or("unknown")),
                    "<unknown>".to_string(),
                ),
            };
            let prefix = if var.mutable { "mut " } else { "" };
            lines.push(RenderedReplBinding {
                name: var.name.clone(),
                line: format!("{prefix}{}: {ty} = {value}", var.name),
            });
        }
    }
    lines
}

/// Every session binding whose name the `%explain` query matches, latest
/// declaration of each name first.
pub(super) fn matching_repl_bindings<'a>(
    bindings: &'a [ReplBinding],
    query: &str,
) -> Vec<&'a ReplBindingVar> {
    let mut out: Vec<&ReplBindingVar> = Vec::new();
    for var in bindings.iter().flat_map(|binding| &binding.vars) {
        if !symbol_query_matches(&var.name, query) {
            continue;
        }
        match out.iter().position(|seen| seen.name == var.name) {
            Some(index) => out[index] = var,
            None => out.push(var),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Joins one rendering per matched binding, failing on the first that cannot
/// be resolved.
pub(super) fn render_matched_repl_bindings(
    bindings: &[ReplBinding],
    query: &str,
    mut render: impl FnMut(&ReplBindingVar) -> std::result::Result<String, String>,
) -> Option<std::result::Result<String, String>> {
    let matches = matching_repl_bindings(bindings, query);
    if matches.is_empty() {
        return None;
    }
    let mut sections = Vec::new();
    for var in matches {
        match render(var) {
            Ok(text) => sections.push(text),
            Err(message) => return Some(Err(message)),
        }
    }
    Some(Ok(sections.join("\n")))
}

pub(super) fn repl_binding_info(
    declarations: &[String],
    lets: &[String],
    bindings: &[ReplBinding],
    query: &str,
) -> Option<std::result::Result<String, String>> {
    render_matched_repl_bindings(bindings, query, |var| {
        repl_binding_info_for(declarations, lets, var)
    })
}

pub(super) fn repl_binding_info_for(
    declarations: &[String],
    lets: &[String],
    var: &ReplBindingVar,
) -> std::result::Result<String, String> {
    let name = var.name.as_str();
    resolve_repl_binding(declarations, lets, name).map(|(_, ty)| {
        let can_mutate = binding_can_mutate(var, &ty);
        let capability = match (var.mutable, ty.references.as_slice(), can_mutate) {
            (_, [], true) => "mutable binding",
            (_, [], false) => "immutable binding",
            (_, _, true) => "mutable referent",
            (_, _, false) => "shared referent",
        };
        let mut out = format!("{} [binding]\n  type: {}\n  capability: {capability}\n", var.name, ty.rendered);
        if !ty.tuple_elements.is_empty() {
            out.push_str("  method surface: tuple (positional elements and the methods below)\n");
            for (index, elem) in ty.tuple_elements.iter().enumerate() {
                out.push_str(&format!("  {}.{index}: {elem} [element]\n", var.name));
            }
            let mutability = if can_mutate {
                format!("; assign with {}.0 = ...", var.name)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  Example: let ({}) = {}{mutability}\n",
                (0..ty.tuple_elements.len())
                    .map(|i| format!("e{i}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                var.name
            ));
        }
        if let Some(session) = render_session_type(
            &session_index(declarations),
            base_type_name(&ty.rendered),
            Some(&var.name),
            &rendered_type_args(&ty.rendered),
        ) {
            out.push_str(&session);
            out.push('\n');
        }
        let Some(ref owner) = ty.method_owner else {
            if index_has_facts(declarations, base_type_name(&ty.rendered)) {
                return out.trim_end().to_string();
            }
            out.push_str(&format!(
                "\nNo cataloged methods for this binding's type.\nExample: let copy = {}",
                var.name
            ));
            return out;
        };
        if ty.fixed_array {
            out.push_str(&format!(
                "  method surface: fixed array (array and slice methods only; mutable methods require writable access)\n  Example: let first = {}[0]\n",
                var.name
            ));
        }
        let methods = available_repl_binding_methods(&ty, owner, can_mutate);
        let lane_names = lane_type_param_names(owner);
        let lane_args = rendered_type_args(&ty.rendered);
        let mut found = false;
        for method in methods {
            found = true;
            let signature = substitute_generic_names(&method.signature, &lane_names, &lane_args);
            let signature = signature_suffix(&signature, &method.name);
            out.push_str(&format!(
                "{}.{}{signature} [method]\n    {}\n    Builtin\n    Example: {}.{}({})\n",
                var.name,
                method.name,
                method.doc,
                var.name,
                method.name,
                signature_example_arguments(signature)
            ));
        }
        // A session type's own methods are listed with its declaration above.
        if !found && !index_has_facts(declarations, base_type_name(&ty.rendered)) {
            out.push_str(&format!(
                "\nNo methods are available with this binding's capability.\nExample: let copy = {}",
                var.name
            ));
        }
        out.trim_end().to_string()
    })
}

pub(super) fn repl_binding_listing(
    declarations: &[String],
    lets: &[String],
    bindings: &[ReplBinding],
    query: &str,
) -> Option<std::result::Result<String, String>> {
    render_matched_repl_bindings(bindings, query, |var| {
        repl_binding_listing_for(declarations, lets, var)
    })
}

pub(super) fn repl_binding_listing_for(
    declarations: &[String],
    lets: &[String],
    var: &ReplBindingVar,
) -> std::result::Result<String, String> {
    let name = var.name.as_str();
    resolve_repl_binding(declarations, lets, name).map(|(_value, ty)| {
        let prefix = if var.mutable { "mut " } else { "" };
        let mut out = format!("{prefix}{name}: {} [binding]\n", ty.rendered);
        for (index, elem) in ty.tuple_elements.iter().enumerate() {
            out.push_str(&format!("{name}.{index}: {elem} [element]\n"));
        }
        if let Some(session) = render_session_type(
            &session_index(declarations),
            base_type_name(&ty.rendered),
            Some(name),
            &rendered_type_args(&ty.rendered),
        ) {
            out.push_str(&session);
            out.push('\n');
        }
        let Some(ref owner) = ty.method_owner else {
            return out.trim_end().to_string();
        };
        let can_mutate = binding_can_mutate(var, &ty);
        let lane_names = lane_type_param_names(owner);
        let lane_args = rendered_type_args(&ty.rendered);
        for method in available_repl_binding_methods(&ty, owner, can_mutate) {
            let signature = substitute_generic_names(&method.signature, &lane_names, &lane_args);
            let signature = signature_suffix(&signature, &method.name);
            out.push_str(&format!("{name}.{}{signature} [method]\n", method.name));
        }
        out.trim_end().to_string()
    })
}

pub(super) fn repl_declaration_info(declarations: &[String], query: &str) -> Option<String> {
    let mut sections = Vec::new();
    let mut seen = Vec::new();
    for declaration in declarations.iter().rev() {
        for name in declaration_matching_names(declaration, query) {
            if seen.contains(&name) {
                continue;
            }
            sections.push(format!("{name} [declaration]\n  {declaration}"));
            seen.push(name);
        }
    }
    (!sections.is_empty()).then(|| sections.join("\n"))
}

pub(super) fn declaration_declares_name(declaration: &str, name: &str) -> bool {
    declaration_names(declaration).contains(&name.to_string())
}

pub(super) fn declaration_matching_names(declaration: &str, query: &str) -> Vec<String> {
    declaration_names(declaration)
        .into_iter()
        .filter(|name| symbol_query_matches(name, query))
        .collect()
}

pub(super) fn declaration_names(declaration: &str) -> Vec<String> {
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file(
        "irepl-declaration-explain".to_string(),
        declaration.to_string(),
    );
    let (sf, diags) = gossamer_parse::parse_source_file(declaration, file);
    if diags.is_empty() {
        collect_source_file_names(&sf)
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    }
}

pub(super) fn binding_can_mutate(var: &ReplBindingVar, ty: &ReplValueType) -> bool {
    if ty.references.is_empty() {
        var.mutable
    } else {
        ty.references
            .iter()
            .all(|mutability| *mutability == gossamer_types::Mutbl::Mut)
    }
}

pub(super) fn available_repl_binding_methods(
    ty: &ReplValueType,
    owner: &str,
    can_mutate: bool,
) -> Vec<CoreMethodEntry> {
    core_methods_for(owner, ty.fixed_array, can_mutate)
}

pub(super) fn resolve_repl_binding(
    declarations: &[String],
    lets: &[String],
    name: &str,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    let let_body = render_repl_setup(lets);
    let entry = "__irepl_binding_info";
    let source = format!(
        "{}\nfn {entry}() {{ {lets}{name} }}\n",
        render_repl_declarations(declarations),
        lets = let_body,
    );
    build_and_call_with_type_for_inspection(&source, entry)
}

pub(super) fn infer_repl_binding_type(
    declarations: &[String],
    lets: &[String],
    name: &str,
) -> std::result::Result<ReplValueType, String> {
    let let_body = render_repl_setup(lets);
    let entry = "__irepl_binding_type";
    let source = format!(
        "{}\nfn {entry}() {{ {lets}{name} }}\n",
        render_repl_declarations(declarations),
        lets = let_body,
    );
    infer_repl_tail_type(&source)
}

pub(super) fn repl_binding_from_let_source(
    input: &str,
) -> std::result::Result<ReplBinding, String> {
    use gossamer_ast::{ExprKind, ItemKind, StmtKind};

    // End the input before the synthetic closing brace so a trailing line
    // comment cannot consume it.
    let source = format!("fn __irepl_binding_names() {{ {input}\n}}\n");
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (sf, diags) = gossamer_parse::parse_source_file(&source, file);
    if !diags.is_empty() {
        return Err(format_parse_diags(&diags, &map));
    }
    let Some(item) = sf.items.first() else {
        return Err(repl_let_shape_error());
    };
    let ItemKind::Fn(decl) = &item.kind else {
        return Err(repl_let_shape_error());
    };
    let Some(body) = &decl.body else {
        return Err(repl_let_shape_error());
    };
    let ExprKind::Block(block) = &body.kind else {
        return Err(repl_let_shape_error());
    };
    if block.stmts.is_empty() {
        return Err(repl_let_shape_error());
    }
    let mut vars = Vec::new();
    let mut saw_let = false;
    for stmt in &block.stmts {
        let StmtKind::Let { pattern, init, .. } = &stmt.kind else {
            continue;
        };
        saw_let = true;
        if init.is_none() {
            return Err(repl_let_initializer_error());
        }
        collect_repl_pattern_bindings(pattern, &mut vars);
    }
    if !saw_let {
        return Err(repl_let_shape_error());
    }
    Ok(ReplBinding {
        vars,
        source_index: 0,
    })
}

pub(super) fn repl_let_shape_error() -> String {
    "1 REPL input error:\n  malformed `let` input: expected one or more `let PAT = EXPR` statements"
        .to_string()
}

pub(super) fn repl_let_initializer_error() -> String {
    "1 REPL input error:\n  malformed `let` input: missing `=` initializer; write `let PAT = EXPR`"
        .to_string()
}

pub(super) fn collect_repl_pattern_bindings(
    pattern: &gossamer_ast::Pattern,
    out: &mut Vec<ReplBindingVar>,
) {
    use gossamer_ast::PatternKind;

    match &pattern.kind {
        PatternKind::Ident {
            mutability,
            name,
            subpattern,
        } => {
            out.push(ReplBindingVar {
                name: name.name.clone(),
                mutable: mutability.is_mutable(),
            });
            if let Some(subpattern) = subpattern {
                collect_repl_pattern_bindings(subpattern, out);
            }
        }
        PatternKind::Tuple(patterns) => {
            for pattern in patterns {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Or(patterns) => {
            if let Some(pattern) = patterns.first() {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for pattern in prefix {
                collect_repl_pattern_bindings(pattern, out);
            }
            if let Some(rest) = rest {
                collect_repl_pattern_bindings(rest, out);
            }
            for pattern in suffix {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for field in fields {
                match &field.pattern {
                    Some(pattern) => collect_repl_pattern_bindings(pattern, out),
                    None => out.push(ReplBindingVar {
                        name: field.name.name.clone(),
                        mutable: false,
                    }),
                }
            }
        }
        PatternKind::TupleStruct { elems, .. } => {
            for pattern in elems {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Ref { inner, .. } => collect_repl_pattern_bindings(inner, out),
        _ => {}
    }
}

pub(super) fn split_meta_command(input: &str) -> (&str, &str) {
    input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(command, arg)| (command, arg.trim()))
}

pub(super) fn input_is_declaration(input: &str) -> bool {
    let input = strip_leading_outer_attributes(input);
    let input = input
        .strip_prefix("pub ")
        .or_else(|| input.strip_prefix("pub(crate) "))
        .unwrap_or(input);
    input.starts_with("fn ")
        || input.starts_with("struct ")
        || input.starts_with("enum ")
        || input.starts_with("impl ")
        || input.starts_with("impl<")
        || input.starts_with("trait ")
        || input.starts_with("use ")
        || input.starts_with("const ")
        || input.starts_with("static ")
        || input.starts_with("type ")
}

/// Removes complete outer attributes from the beginning of a REPL input.
///
/// The REPL classifies declarations before rebuilding the accumulated source.
/// An attributed item still starts with `#`, so without this step it is
/// mistaken for an expression and wrapped in the synthetic REPL function.
pub(super) fn strip_leading_outer_attributes(mut input: &str) -> &str {
    loop {
        input = input.trim_start();
        if !input.starts_with("#[") {
            return input;
        }

        let mut depth = 0usize;
        let mut quote = None;
        let mut escaped = false;
        let mut end = None;

        for (offset, ch) in input.char_indices() {
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == delimiter {
                    quote = None;
                }
                continue;
            }

            match ch {
                '"' | '\'' => quote = Some(ch),
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(offset + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end) = end else {
            return input;
        };
        input = &input[end..];
    }
}

pub(super) fn repl_info(arg: &str) -> std::result::Result<String, String> {
    // The catalog listing is the canonical module rendering. Omitting module
    // help here prevents `%i gzip` from printing the same module twice while
    // retaining matching items, methods, and types from the search.
    repl_info_matches(arg, true)
}

pub(super) fn repl_info_listing(arg: &str) -> std::result::Result<String, String> {
    repl_info_matches(arg, false)
}
