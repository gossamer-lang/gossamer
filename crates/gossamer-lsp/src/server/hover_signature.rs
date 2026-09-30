fn empty_semantic_tokens() -> Value {
    let mut out = BTreeMap::new();
    out.insert("data".to_string(), Value::Array(Vec::new()));
    Value::Object(out)
}

#[allow(
    clippy::too_many_lines,
    reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
)]
fn render_hover(doc: &DocumentAnalysis, loc: &Locate) -> String {
    match loc {
        Locate::PathExpr {
            resolution: Some(Resolution::Local(node)),
            name,
            expr_id,
            ..
        } => {
            let mut body = String::new();
            if let Some(info) = doc.index.local(*node) {
                body.push_str("```gos\n");
                if info.mutable {
                    body.push_str("let mut ");
                } else {
                    body.push_str("let ");
                }
                body.push_str(&info.name);
                if let Some(ty) = doc.types.get(*expr_id) {
                    body.push_str(": ");
                    body.push_str(&render_public_ty(&doc.tcx, ty));
                }
                body.push_str("\n```");
            } else {
                body.push_str(name);
            }
            body
        }
        Locate::PathExpr {
            resolution: Some(Resolution::Def { def, .. }),
            expr_id,
            ..
        } => {
            let mut body = String::new();
            if let Some(info) = doc.index.def(*def) {
                body.push_str("```gos\n");
                body.push_str(&info.signature);
                body.push_str("\n```");
                if !info.docs.is_empty() {
                    body.push_str("\n\n");
                    body.push_str(&info.docs);
                }
            }
            if let Some(ty) = doc.types.get(*expr_id) {
                body.push_str("\n\n*type:* `");
                body.push_str(&render_public_ty(&doc.tcx, ty));
                body.push('`');
            }
            body
        }
        Locate::PathExpr {
            resolution: Some(Resolution::Primitive(_)),
            name,
            ..
        } => format!("```gos\n{name}\n```\n\nbuilt-in primitive type"),
        Locate::PathExpr {
            resolution: Some(Resolution::Import { .. }),
            name,
            ..
        } => format!("```gos\nuse {name}\n```\n\nimported name"),
        Locate::PathExpr {
            resolution: Some(Resolution::Err) | None,
            name,
            expr_id,
            ..
        } => {
            let mut body = format!("```\n{name}\n```");
            if let Some(ty) = doc.types.get(*expr_id) {
                body.push_str("\n\n*type:* `");
                body.push_str(&render_public_ty(&doc.tcx, ty));
                body.push('`');
            }
            body
        }
        Locate::TypePath {
            resolution: Some(Resolution::Def { def, .. }),
            ..
        } => doc.index.def(*def).map_or_else(String::new, |info| {
            let mut body = format!("```gos\n{}\n```", info.signature);
            if !info.docs.is_empty() {
                body.push_str("\n\n");
                body.push_str(&info.docs);
            }
            body
        }),
        Locate::TypePath {
            resolution: Some(Resolution::Primitive(_)) | None,
            name,
            ..
        }
        | Locate::TypePath {
            resolution: Some(Resolution::Err),
            name,
            ..
        }
        | Locate::TypePath {
            resolution: Some(Resolution::Import { .. }),
            name,
            ..
        }
        | Locate::TypePath {
            resolution: Some(Resolution::Local(_)),
            name,
            ..
        } => format!("```gos\n{name}\n```"),
        Locate::Binding {
            pattern_id, name, ..
        } => {
            let mut body = format!("```gos\nlet {name}\n```");
            if let Some(ty) = doc.types.get(*pattern_id) {
                body.push_str("\n\n*type:* `");
                body.push_str(&render_public_ty(&doc.tcx, ty));
                body.push('`');
            }
            body
        }
        Locate::Field {
            name,
            access_id: Some(id),
            ..
        } if doc.types.get(*id).is_some() => {
            let ty = doc.types.get(*id).map(|ty| render_public_ty(&doc.tcx, ty));
            format!("```gos\n{name}: {}\n```\n\nfield", ty.unwrap_or_default())
        }
        Locate::Field { name, .. } => format!("```gos\n{name}\n```\n\nfield / method"),
    }
}

/// The word as a hover title: a built-in trait leads with the block an
/// `impl` writes, and every other word is shown as it was typed.
fn builtin_trait_signature(word: &str) -> String {
    match gossamer_types::builtin_trait(word) {
        Some(entry) if !entry.signature.is_empty() => format!("{word} {}", entry.signature),
        _ => word.to_string(),
    }
}

fn word_hover(doc: &DocumentAnalysis, offset: u32) -> Value {
    let Some(word) = doc.word_at(offset) else {
        return Value::Null;
    };
    let mut markdown = format!("```\n{}\n```", builtin_trait_signature(word));
    if doc.top_level_span(word).is_some() {
        markdown.push_str("\n\nDeclared at the top level of this file.");
    } else if let Some(entry) = gossamer_types::builtin_trait(word) {
        markdown.push_str("\n\n");
        markdown.push_str(entry.doc);
        if !entry.instead.is_empty() {
            markdown.push(' ');
            markdown.push_str(entry.instead);
        }
    }
    let mut contents = BTreeMap::new();
    contents.insert("kind".to_string(), Value::String("markdown".to_string()));
    contents.insert("value".to_string(), Value::String(markdown));
    let mut hover = BTreeMap::new();
    hover.insert("contents".to_string(), Value::Object(contents));
    Value::Object(hover)
}

/// Convenience accessors so server.rs doesn't reach into private
/// index types directly.
impl DocumentAnalysis {
    pub(crate) fn index_pairs(
        &self,
    ) -> impl Iterator<Item = (gossamer_resolve::DefId, &DefinitionInfo)> {
        self.index.def_iter()
    }

    pub(crate) fn binding_pairs(
        &self,
    ) -> impl Iterator<Item = (gossamer_ast::NodeId, &BindingInfo)> {
        self.index.local_iter()
    }
}

fn locate_name(loc: &Locate) -> String {
    match loc {
        Locate::PathExpr { name, .. }
        | Locate::TypePath { name, .. }
        | Locate::Binding { name, .. }
        | Locate::Field { name, .. } => name.clone(),
    }
}

fn locate_span(loc: &Locate) -> Span {
    match loc {
        Locate::PathExpr { segment_span, .. }
        | Locate::TypePath { segment_span, .. }
        | Locate::Binding {
            name_span: segment_span,
            ..
        }
        | Locate::Field {
            name_span: segment_span,
            ..
        } => *segment_span,
    }
}

fn location(doc: &DocumentAnalysis, span: Span) -> Value {
    let mut out = BTreeMap::new();
    out.insert("uri".to_string(), Value::String(doc.uri.clone()));
    out.insert("range".to_string(), span_to_range(doc, span));
    Value::Object(out)
}

fn signature_help_for(info: &DefinitionInfo, active_param: u32) -> Value {
    let mut signature = BTreeMap::new();
    signature.insert("label".to_string(), Value::String(info.signature.clone()));
    if !info.docs.is_empty() {
        let mut docs = BTreeMap::new();
        docs.insert("kind".to_string(), Value::String("markdown".to_string()));
        docs.insert("value".to_string(), Value::String(info.docs.clone()));
        signature.insert("documentation".to_string(), Value::Object(docs));
    }
    // Build the parameters array by re-parsing `(args)` out of the
    // signature text.
    let params = parse_signature_params(&info.signature);
    let parameters: Vec<Value> = params
        .iter()
        .map(|p| {
            let mut entry = BTreeMap::new();
            entry.insert("label".to_string(), Value::String(p.clone()));
            Value::Object(entry)
        })
        .collect();
    signature.insert("parameters".to_string(), Value::Array(parameters));
    let mut help = BTreeMap::new();
    help.insert(
        "signatures".to_string(),
        Value::Array(vec![Value::Object(signature)]),
    );
    help.insert("activeSignature".to_string(), Value::Number(0.0));
    help.insert(
        "activeParameter".to_string(),
        Value::Number(f64::from(active_param)),
    );
    Value::Object(help)
}

fn parse_signature_params(sig: &str) -> Vec<String> {
    let Some(open) = sig.find('(') else {
        return Vec::new();
    };
    let Some(close) = sig.rfind(')') else {
        return Vec::new();
    };
    if close <= open + 1 {
        return Vec::new();
    }
    let inner = &sig[open + 1..close];
    let mut depth = 0i32;
    let mut current = String::new();
    let mut out: Vec<String> = Vec::new();
    for ch in inner.chars() {
        match ch {
            '<' | '(' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '>' | ')' | ']' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn enclosing_call(source: &str, offset: u32) -> Option<(String, u32)> {
    let bytes = source.as_bytes();
    let cap = std::cmp::min(offset as usize, bytes.len());
    let mut depth = 0i32;
    let mut commas = 0u32;
    // Walk backwards looking for the most recent unbalanced `(`.
    for i in (0..cap).rev() {
        match bytes[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                depth -= 1;
                if depth < 0 && bytes[i] == b'(' {
                    // Found an open paren without a matching close.
                    let name = preceding_identifier(bytes, i);
                    return name.map(|n| (n, commas));
                }
            }
            b',' if depth == 0 => commas += 1,
            _ => {}
        }
    }
    None
}

fn preceding_identifier(bytes: &[u8], paren_pos: usize) -> Option<String> {
    let mut end = paren_pos;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = end;
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    std::str::from_utf8(&bytes[start..end])
        .ok()
        .map(str::to_string)
}

/// Encodes one inlay hint into the LSP wire shape.
fn inlay_to_lsp(hint: InlayHint) -> Value {
    let mut position = BTreeMap::new();
    position.insert("line".to_string(), Value::Number(f64::from(hint.line)));
    position.insert(
        "character".to_string(),
        Value::Number(f64::from(hint.character)),
    );
    let mut out = BTreeMap::new();
    out.insert("position".to_string(), Value::Object(position));
    out.insert("label".to_string(), Value::String(hint.label));
    out.insert("kind".to_string(), Value::Number(1.0));
    out.insert("paddingLeft".to_string(), Value::Bool(false));
    out.insert("paddingRight".to_string(), Value::Bool(false));
    Value::Object(out)
}

/// Validates that `name` is a legal Gossamer identifier.
///
/// Follows the same `XID_Start` / `XID_Continue` rules the lexer
/// applies (matches Rust 2024). Underscores are allowed as a start
/// character in addition to `XID_Start`. Empty strings and reserved
/// keywords are rejected.
fn is_valid_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Only the lexer's reserved words are refused; a contextual word such as
    // `defer` or `select` is an ordinary name everywhere its construct does
    // not start.
    if gossamer_lex::Keyword::from_ident(name).is_some() {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !unicode_ident::is_xid_start(first) {
        return false;
    }
    chars.all(unicode_ident::is_xid_continue)
}


