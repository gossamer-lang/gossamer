/// Preprocesses a Gossamer source string by appending synthesized
/// `from_json` / `to_json` impl blocks for every eligible struct.
/// Returns the augmented source. Callers should put the augmented
/// source into the source map before invoking `parse_source_file`.
#[must_use]
pub fn augment_source(source: &str) -> String {
    // The `codegen(..)` splice's `comptime fn` backer.
    let validators = synthesize_validators(source);
    // Stdlib structs (pem::Block, …) are real Gossamer structs +
    // wrapper functions injected here; the wrappers call leaf
    // `gos_rt_*` intrinsics that return tuples/bytes, so the same
    // code compiles + runs on every tier. `rewrite_stdlib_struct_surface`
    // (in parse_with_autoderive) redirects the user's
    // `encoding::pem::*` call / literal / type sites onto these.
    let stdlib_wrappers = synthesize_stdlib_wrappers(source);
    let (serde, derives, type_info) = if source_may_need_ast_synthesis(source) {
        let mut probe_map = SourceMap::new();
        let probe_file = probe_map.add_file("<autoderive-probe>", source.to_string());
        let (parsed, probe_diags) = crate::parse_source_file(source, probe_file);
        // Synthesis reads item names, fields, and types out of the tree and
        // splices the result back as source. A tree built through error
        // recovery carries placeholder names and dropped items, so anything
        // derived from it would be reported against spans in the appended
        // text rather than anything the user wrote. The parse diagnostics
        // are the actionable report in that case.
        if !probe_diags.is_empty() {
            return source.to_string();
        }
        let serde = synthesize_serde_impls(&parsed);
        let derives = synthesize_derive_impls(&parsed);
        // Field-reflection functions for `typeInfo::<T>()`, emitted only
        // when the source reflects (keeps non-reflecting programs lean).
        let type_info = if source.contains("typeInfo") {
            synthesize_type_info(&parsed)
        } else {
            String::new()
        };
        (serde, derives, type_info)
    } else {
        (String::new(), String::new(), String::new())
    };
    if synth_is_empty(&serde)
        && stdlib_wrappers.is_empty()
        && derives.is_empty()
        && type_info.is_empty()
        && validators.is_empty()
    {
        return source.to_string();
    }
    if std::env::var_os("GOS_AUTODERIVE_DEBUG").is_some() {
        eprintln!("=== autoderive synth ===\n{serde}{derives}{stdlib_wrappers}=== /autoderive ===");
    }
    let mut combined = String::with_capacity(
        source.len() + serde.len() + derives.len() + stdlib_wrappers.len() + 2,
    );
    combined.push_str(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push('\n');
    if !synth_is_empty(&serde) {
        combined.push_str(&serde);
    }
    combined.push_str(&derives);
    combined.push_str(&stdlib_wrappers);
    combined.push_str(&type_info);
    combined.push_str(&validators);
    combined
}

/// Returns true when an AST walk could synthesize source. Most files contain
/// only functions and imports; for those, avoid the probe parse and let the
/// later authoritative frontend parse handle normal rewrites.
fn source_may_need_ast_synthesis(source: &str) -> bool {
    let mut map = SourceMap::new();
    let file = map.add_file("<autoderive-prescan>", String::new());
    let mut lexer = Lexer::new(source, file);
    loop {
        let token = lexer.next_token();
        match token.kind {
            TokenKind::Keyword(Keyword::Struct | Keyword::Enum) => return true,
            TokenKind::Punct(Punct::Hash) => return true,
            TokenKind::Eof => return false,
            _ => {}
        }
    }
}

