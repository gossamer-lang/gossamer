/// The line `augment_source` writes between the program and everything it
/// appends. Nothing the program wrote follows its last occurrence, so the
/// code after it is the toolchain's own: see [`program_source_end`].
pub const GENERATED_SECTION_MARKER: &str = "// gossamer: generated below\n";

/// The byte offset where the program's own text ends in augmented `source`:
/// the start of the last [`GENERATED_SECTION_MARKER`], or the whole length
/// when nothing was appended. A program cannot place text after the marker
/// the toolchain appends, so an item before this offset is the program's.
#[must_use]
pub fn program_source_end(source: &str) -> usize {
    source.rfind(GENERATED_SECTION_MARKER).unwrap_or(source.len())
}

/// `items` without the foreign types `type Name` declares in extern blocks,
/// at any module depth.
fn without_foreign_types(items: &mut Vec<gossamer_ast::Item>) {
    items.retain(|item| !item.attrs.has_word(gossamer_ast::FOREIGN_TYPE_ATTR));
    for item in items {
        if let gossamer_ast::ItemKind::Mod(gossamer_ast::ModDecl {
            body: gossamer_ast::ModBody::Inline(inner),
            ..
        }) = &mut item.kind
        {
            without_foreign_types(inner);
        }
    }
}

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
    // `char` and integer methods written in Gossamer, for the names the
    // program calls.
    let mut primitive_surface = synthesize_primitive_surface(source);
    primitive_surface.push_str(&synthesize_format_helpers(source));
    primitive_surface.push_str(&synthesize_collect_helpers(source));
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
        // An item `#[cfg]` leaves out of this build is no item at all, so
        // nothing is synthesized for it: two cfg variants of one struct
        // would otherwise each get the same helpers.
        let mut parsed = gossamer_ast::cfg::without_inactive_items(&parsed).unwrap_or(parsed);
        // A foreign type has no Gossamer value to format, compare, or
        // serialize, so nothing is synthesized for it.
        without_foreign_types(&mut parsed.items);
        let serde = synthesize_serde_impls(&parsed);
        let mut derives = synthesize_derive_impls(&parsed);
        derives.push_str(&synthesize_iterator_adapters(&parsed, source));
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
        && primitive_surface.is_empty()
        && derives.is_empty()
        && type_info.is_empty()
        && validators.is_empty()
    {
        // The marker still closes the program, so text the program writes
        // can never be read as the toolchain's.
        let mut closed = String::with_capacity(source.len() + GENERATED_SECTION_MARKER.len() + 2);
        closed.push_str(source);
        if !closed.ends_with('\n') {
            closed.push('\n');
        }
        closed.push_str(GENERATED_SECTION_MARKER);
        return closed;
    }
    if std::env::var_os("GOS_AUTODERIVE_DEBUG").is_some() {
        eprintln!(
            "=== autoderive synth ===\n{serde}{derives}{stdlib_wrappers}{primitive_surface}=== /autoderive ==="
        );
    }
    let mut combined = String::with_capacity(
        source.len()
            + serde.len()
            + derives.len()
            + stdlib_wrappers.len()
            + primitive_surface.len()
            + 2,
    );
    combined.push_str(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push('\n');
    combined.push_str(GENERATED_SECTION_MARKER);
    if !synth_is_empty(&serde) {
        combined.push_str(&serde);
    }
    combined.push_str(&derives);
    combined.push_str(&stdlib_wrappers);
    combined.push_str(&primitive_surface);
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

