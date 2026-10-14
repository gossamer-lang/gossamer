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

/// Preprocesses a Gossamer source string by appending synthesized
/// `from_json` / `to_json` impl blocks for every eligible struct.
/// Returns the augmented source. Callers should put the augmented
/// source into the source map before invoking `parse_source_file`.
#[must_use]
pub fn augment_source(source: &str) -> String {
    augment(source).0
}

/// A program's own text, parsed cleanly while autoderive read it, kept for
/// [`parse_with_autoderive_from`] so the frontend parses only what
/// autoderive appended.
#[derive(Debug)]
pub struct ParsedProgram {
    tree: SourceFile,
    len: usize,
}

/// [`augment_source`], with the tree of `source` it parsed when the parse
/// was clean. The tree's spans are in the first file of a fresh
/// [`SourceMap`], where every toolchain command registers its unit.
#[must_use]
pub fn augment_source_with_program(source: &str) -> (String, Option<ParsedProgram>) {
    let (augmented, tree) = augment(source);
    let program = tree.map(|tree| ParsedProgram {
        tree,
        len: source.len(),
    });
    (augmented, program)
}

fn augment(source: &str) -> (String, Option<SourceFile>) {
    // The `codegen(..)` splice's `comptime fn` backer.
    let validators = synthesize_validators(source);
    // Every synthesis below reads this one tree or this one token pass.
    let (mut parsed, lexed) = clean_parse_with_tokens(source);
    // The parser's tokens are in the coordinates of the text after a byte
    // order mark, which every scan below slices by.
    let tokens = if source.starts_with('\u{feff}') {
        SourceTokens::new(source)
    } else {
        SourceTokens::lexed(source, lexed)
    };
    // `char` and integer methods written in Gossamer, for the names the
    // program calls.
    let mut primitive_surface = synthesize_primitive_surface(source, &tokens);
    primitive_surface.push_str(&synthesize_format_helpers(source, &tokens));
    primitive_surface.push_str(&synthesize_collect_helpers(source));
    let (serde, derives, type_info) = if source_may_need_ast_synthesis(&tokens) {
        // Synthesis reads item names, fields, and types out of the tree and
        // splices the result back as source. A tree built through error
        // recovery carries placeholder names and dropped items, so anything
        // derived from it would be reported against spans in the appended
        // text rather than anything the user wrote. The parse diagnostics
        // are the actionable report in that case.
        let Some(tree) = parsed.as_mut() else {
            return (source.to_string(), None);
        };
        // An item `#[cfg]` leaves out of this build is no item at all, so
        // nothing is synthesized for it: two cfg variants of one struct
        // would otherwise each get the same helpers. A foreign type has no
        // Gossamer value to format, compare, or serialize, so nothing is
        // synthesized for it either. The stdlib wrappers below read the
        // whole tree, so both are set aside only while synthesis reads it.
        let set_aside = gossamer_ast::cfg::SetAside::take(&mut tree.items, &|item| {
            gossamer_ast::cfg::item_is_active(&item.attrs)
                && !item.attrs.has_word(gossamer_ast::FOREIGN_TYPE_ATTR)
        });
        let parsed = &*tree;
        let serde = synthesize_serde_impls(parsed);
        let mut derives = synthesize_derive_impls(parsed);
        derives.push_str(&synthesize_iterator_adapters(parsed, source));
        // Field-reflection functions for `typeInfo::<T>()`, emitted only
        // when the source reflects (keeps non-reflecting programs lean).
        let type_info = if source.contains("typeInfo") {
            synthesize_type_info(parsed)
        } else {
            String::new()
        };
        set_aside.restore(&mut tree.items);
        (serde, derives, type_info)
    } else {
        (String::new(), String::new(), String::new())
    };
    // Stdlib structs (pem::Block, …) are real Gossamer structs +
    // wrapper functions injected here; the wrappers call leaf
    // `gos_rt_*` intrinsics that return tuples/bytes, so the same
    // code compiles + runs on every tier. `rewrite_stdlib_struct_surface`
    // (in parse_with_autoderive) redirects the user's
    // `encoding::pem::*` call / literal / type sites onto these.
    let stdlib_wrappers = synthesize_stdlib_wrappers(source, parsed.as_ref());
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
        return (closed, parsed);
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
    (combined, parsed)
}

/// Returns true when an AST walk could synthesize source. Most files contain
/// only functions and imports, and those need no tree synthesis.
fn source_may_need_ast_synthesis(tokens: &SourceTokens<'_>) -> bool {
    tokens.any(|kind| {
        matches!(
            kind,
            TokenKind::Keyword(Keyword::Struct | Keyword::Enum) | TokenKind::Punct(Punct::Hash)
        )
    })
}

