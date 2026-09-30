//! Rustyline `Helper` that paints Gossamer source with ANSI colour
//! escapes as the user types in the REPL.
//! Lexing is delegated to `gossamer_lex::tokenize`; the helper does
//! not build an AST, so partially-typed input (unterminated strings,
//! dangling punctuation) still paints correctly up to the last
//! lexable boundary.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use gossamer_lex::{Punct, SourceMap, TokenKind, tokenize};
use rustyline::completion::Completer;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, Helper, RepeatCount};

/// ANSI colour escapes used by the REPL. Chosen to read well on both
/// light and dark terminals; dim for comments keeps them present but
/// low-contrast.
const RESET: &str = "\x1b[0m";
const CYAN_BOLD: &str = "\x1b[1;36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const MAGENTA: &str = "\x1b[35m";
const DIM: &str = "\x1b[2m";

/// REPL helper that highlights Gossamer source as the user types.
#[derive(Default)]
pub(crate) struct GosReplHelper {
    binding_surfaces: HashMap<String, crate::repl::BindingSurface>,
    declarations: Vec<String>,
    session_type_names: HashSet<String>,
}

impl GosReplHelper {
    /// Constructs a fresh helper with no per-session state.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Replaces the session's declarations, which fix both the names the
    /// highlighter paints as types and the members a binding of one of
    /// those types completes.
    pub(crate) fn set_declarations(&mut self, declarations: &[String]) {
        self.session_type_names = crate::repl::session_type_names(declarations);
        self.declarations = declarations.to_vec();
    }

    /// Whether `name` names a type here: one the session declared, or one
    /// the language and its standard library provide.
    fn is_type_name(&self, name: &str) -> bool {
        self.session_type_names.contains(name) || is_known_type_name(name)
    }

    /// Records what a persistent binding's name reaches through a dot.
    pub(crate) fn set_binding_surface(
        &mut self,
        name: &str,
        surface: Option<crate::repl::BindingSurface>,
    ) {
        if let Some(surface) = surface {
            self.binding_surfaces.insert(name.to_string(), surface);
        } else {
            self.binding_surfaces.remove(name);
        }
    }

    /// Removes completion metadata for a binding ended by `%drop`.
    pub(crate) fn forget_binding(&mut self, name: &str) {
        self.binding_surfaces.remove(name);
    }

    /// Clears all completion metadata with the rest of the REPL session.
    pub(crate) fn reset_session(&mut self) {
        self.binding_surfaces.clear();
        self.declarations.clear();
        self.session_type_names.clear();
    }
}

/// Type names the language itself provides: the primitives, the core
/// generic containers, and the shapes syntax alone builds. A name outside
/// this set and outside the session's own declarations is left uncoloured,
/// matching the editor, which paints a type only where one resolves.
fn is_known_type_name(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "char"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
            | "f32"
            | "f64"
            | "String"
            | "Vec"
            | "Map"
            | "BTreeMap"
            | "Set"
            | "BTreeSet"
            | "Deque"
            | "Queue"
            | "Stack"
            | "MaxHeap"
            | "MinHeap"
            | "Option"
            | "Result"
            | "Weak"
            | "Sender"
            | "Receiver"
            | "JoinHandle"
            | "Self"
    ) || STDLIB_TYPE_NAMES.with(|names| names.contains(name))
}

thread_local! {
    /// Type names the standard library exports, read once from its manifest.
    static STDLIB_TYPE_NAMES: HashSet<String> = gossamer_std::registry::modules()
        .iter()
        .flat_map(|module| module.items)
        .filter(|item| matches!(item.kind, gossamer_std::registry::StdItemKind::Type))
        .map(|item| item.name.to_string())
        .collect();
}

impl Helper for GosReplHelper {}

/// Adds language-aware indentation when Enter extends incomplete input.
pub(crate) struct ReplEnterHandler;

impl ConditionalEventHandler for ReplEnterHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let input = ctx.line();
        if ctx.pos() != input.len() || incomplete_reason(input).is_none() {
            return None;
        }
        Some(Cmd::Insert(1, format!("\n{}", continuation_indent(input))))
    }
}

fn continuation_indent(input: &str) -> String {
    const INDENT: &str = "    ";

    let current_line = input.rsplit('\n').next().unwrap_or(input);
    let leading = &current_line[..current_line
        .find(|ch: char| !ch.is_whitespace())
        .unwrap_or(current_line.len())];

    let mut map = SourceMap::new();
    let file = map.add_file("repl-indent.gos", input.to_string());
    let (tokens, _) = tokenize(input, file);
    let opens_block = tokens
        .iter()
        .rev()
        .find(|token| {
            !matches!(
                token.kind,
                TokenKind::Eof
                    | TokenKind::Whitespace
                    | TokenKind::LineComment
                    | TokenKind::BlockComment
            )
        })
        .is_some_and(|token| {
            matches!(
                token.kind,
                TokenKind::Punct(Punct::LBrace | Punct::LParen | Punct::LBracket)
            )
        });

    if opens_block {
        format!("{leading}{INDENT}")
    } else {
        leading.to_string()
    }
}

/// Every Gossamer keyword, completed when the cursor word is unqualified.
/// Every word a completion offers: `gossamer_lex::Keyword` (which exposes
/// no all-variants iterator, so the set is listed here and kept in step
/// with the lexer) plus the contextual words that open a construct
/// without being reserved.
const KEYWORDS: &[&str] = &[
    "arena", "as", "async", "await", "break", "cohort", "comptime", "const", "continue", "crate",
    "defer", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
    "match", "mod", "mut", "newtype", "packed", "package", "pub", "return", "select", "self",
    "spawn", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "yield",
];

impl Completer for GosReplHelper {
    type Candidate = String;

    /// Completes the identifier-or-path word ending at the cursor against
    /// the keyword set and the standard-library surface (`std::registry`):
    /// module paths and their `module::item` members. Returns the byte
    /// offset where the replacement begins plus the prefix-matching
    /// candidates, sorted and de-duplicated.
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(complete_at(
            line,
            pos,
            &self.binding_surfaces,
            &self.declarations,
        ))
    }
}

/// Computes completion candidates for the word ending at `pos`. Split out
/// from the trait method so it is testable without a rustyline `Context`.
fn complete_at(
    line: &str,
    pos: usize,
    binding_surfaces: &HashMap<String, crate::repl::BindingSurface>,
    declarations: &[String],
) -> (usize, Vec<String>) {
    let start = word_start(line, pos);
    let word = &line[start..pos];
    // A cursor sitting straight after the dot has no word yet, and that is
    // exactly when the whole member surface is worth offering. Only the
    // keyword and module completions below need something to match against.
    if start > 0
        && line.as_bytes()[start - 1] == b'.'
        && let Some(receiver_start) = receiver_start(line, start - 1)
        && let Some(surface) = binding_surfaces.get(&line[receiver_start..start - 1])
    {
        return (
            start,
            crate::repl::binding_member_names(surface, declarations)
                .into_iter()
                .filter(|member| member.starts_with(word))
                .collect(),
        );
    }
    if word.is_empty() {
        return (start, Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    // A qualified word may name a type rather than a module, and a type's
    // own surface is what `%info` reports for it.
    if let Some((owner, member)) = word.rsplit_once("::") {
        out.extend(
            crate::repl::qualified_member_names(owner, declarations)
                .into_iter()
                .filter(|name| name.starts_with(member))
                .map(|name| format!("{owner}::{name}")),
        );
    }
    if !word.contains(':') {
        out.extend(
            KEYWORDS
                .iter()
                .filter(|kw| kw.starts_with(word))
                .map(|kw| (*kw).to_string()),
        );
        // A trait is named bare in the `impl` header and the bound that reach
        // for it, so that is the spelling completion has to offer.
        out.extend(
            gossamer_types::BUILTIN_TRAITS
                .iter()
                .filter(|entry| entry.name.starts_with(word))
                .map(|entry| entry.name.to_string()),
        );
    }
    for module in gossamer_std::registry::modules() {
        for prefix in module_prefixes(module.path) {
            if prefix.starts_with(word) {
                out.push(prefix.to_string());
            }
            for item in module.items {
                let qualified = format!("{prefix}::{}", item.name);
                if qualified.starts_with(word) {
                    out.push(qualified);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    (start, out)
}

/// Byte offset where the identifier ending at `dot` begins, or `None` when
/// the dot follows something that is not a name. Identifiers are Unicode
/// (UAX #31), so the scan walks characters rather than ASCII bytes.
fn receiver_start(line: &str, dot: usize) -> Option<usize> {
    let start = identifier_start(&line[..dot]);
    (start < dot).then_some(start)
}

/// Byte offset where the identifier ending `text` begins.
fn identifier_start(text: &str) -> usize {
    text.char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_alphanumeric() || *ch == '_')
        .last()
        .map_or(text.len(), |(offset, _)| offset)
}

/// The module path forms a user might type to reach a module: the canonical
/// path (`std::strings`), the `std::`-stripped form used after `use std::…`
/// (`strings`, `encoding::json`), and the bare last segment (`json`).
fn module_prefixes(path: &'static str) -> Vec<&'static str> {
    let mut forms = vec![path];
    if let Some(stripped) = path.strip_prefix("std::") {
        if !forms.contains(&stripped) {
            forms.push(stripped);
        }
    }
    if let Some(last) = path.rsplit("::").next() {
        if !forms.contains(&last) {
            forms.push(last);
        }
    }
    forms
}

/// Byte offset of the start of the identifier-or-path word ending at `pos`.
/// Walks back over identifier characters and `:` path separators so a
/// partially-typed `strings::sp` completes as a single unit.
fn word_start(line: &str, pos: usize) -> usize {
    line[..pos]
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_alphanumeric() || *ch == '_' || *ch == ':')
        .last()
        .map_or(pos, |(offset, _)| offset)
}

impl Hinter for GosReplHelper {
    type Hint = String;
}

impl Validator for GosReplHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        if input.is_empty() {
            return Ok(ValidationResult::Valid(None));
        }
        if incomplete_reason(input).is_some() {
            return Ok(ValidationResult::Incomplete);
        }
        Ok(ValidationResult::Valid(None))
    }
}

/// Returns `Some(_)` when `input` is syntactically incomplete - an
/// unclosed brace/paren/bracket, or a trailing unterminated block
/// comment. In that case the REPL keeps reading subsequent lines as
/// a continuation of the same expression.
fn incomplete_reason(input: &str) -> Option<&'static str> {
    // Meta-command arguments may themselves be incomplete Gossamer syntax,
    // especially regexes such as `[` or `foo(`. Submit them immediately so
    // the command can either use the pattern or report its regex error.
    if input.trim_start().starts_with('%') {
        return None;
    }
    let mut map = SourceMap::new();
    let file = map.add_file("repl.gos", input.to_string());
    let (tokens, lex_errors) = tokenize(input, file);
    for err in &lex_errors {
        let message = format!("{err:?}");
        if message.contains("Unterminated") {
            return Some("unterminated literal or comment");
        }
    }
    let mut depth_brace: i32 = 0;
    let mut depth_paren: i32 = 0;
    let mut depth_bracket: i32 = 0;
    for token in tokens {
        match token.kind {
            TokenKind::Punct(Punct::LBrace) => depth_brace += 1,
            TokenKind::Punct(Punct::RBrace) => depth_brace -= 1,
            TokenKind::Punct(Punct::LParen) => depth_paren += 1,
            TokenKind::Punct(Punct::RParen) => depth_paren -= 1,
            TokenKind::Punct(Punct::LBracket) => depth_bracket += 1,
            TokenKind::Punct(Punct::RBracket) => depth_bracket -= 1,
            _ => {}
        }
    }
    if depth_brace > 0 {
        return Some("unbalanced `{`");
    }
    if depth_paren > 0 {
        return Some("unbalanced `(`");
    }
    if depth_bracket > 0 {
        return Some("unbalanced `[`");
    }
    None
}

impl Highlighter for GosReplHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            return Cow::Borrowed(line);
        }
        let mut map = SourceMap::new();
        let file = map.add_file("repl.gos", line.to_string());
        let (tokens, _) = tokenize(line, file);
        let mut out = String::with_capacity(line.len() + tokens.len() * 6);
        let mut cursor = 0usize;
        for token in tokens {
            let start = token.span.start as usize;
            let end = token.span.end as usize;
            if start > cursor {
                out.push_str(&line[cursor..start]);
            }
            if end <= start || end > line.len() {
                cursor = start.max(cursor);
                continue;
            }
            let text = &line[start..end];
            match token.kind {
                TokenKind::Eof => {}
                TokenKind::Keyword(_) => {
                    out.push_str(CYAN_BOLD);
                    out.push_str(text);
                    out.push_str(RESET);
                }
                TokenKind::StringLit
                | TokenKind::RawStringLit { .. }
                | TokenKind::TripleStringLit
                | TokenKind::FStringLit
                | TokenKind::FTripleStringLit
                | TokenKind::ByteStringLit
                | TokenKind::RawByteStringLit { .. }
                | TokenKind::CharLit
                | TokenKind::ByteLit => {
                    out.push_str(GREEN);
                    out.push_str(text);
                    out.push_str(RESET);
                }
                TokenKind::IntLit | TokenKind::FloatLit => {
                    out.push_str(YELLOW);
                    out.push_str(text);
                    out.push_str(RESET);
                }
                TokenKind::LineComment | TokenKind::BlockComment => {
                    out.push_str(DIM);
                    out.push_str(text);
                    out.push_str(RESET);
                }
                TokenKind::Ident => {
                    if self.is_type_name(text) {
                        out.push_str(MAGENTA);
                        out.push_str(text);
                        out.push_str(RESET);
                    } else {
                        out.push_str(text);
                    }
                }
                TokenKind::Label => {
                    out.push_str(CYAN_BOLD);
                    out.push_str(text);
                    out.push_str(RESET);
                }
                TokenKind::Punct(_) | TokenKind::Whitespace | TokenKind::Invalid => {
                    out.push_str(text);
                }
            }
            cursor = end.max(cursor);
        }
        if cursor < line.len() {
            out.push_str(&line[cursor..]);
        }
        Cow::Owned(out)
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        true
    }
}

#[cfg(test)]
mod repl_helper_tests {
    use super::{GosReplHelper, MAGENTA, complete_at, continuation_indent, incomplete_reason};
    use crate::repl::BindingSurface;
    use rustyline::highlight::Highlighter;
    use std::collections::HashMap;

    fn surfaces(entries: &[(&str, &str)]) -> HashMap<String, BindingSurface> {
        entries
            .iter()
            .map(|(name, owner)| ((*name).to_string(), BindingSurface::owned_by(owner)))
            .collect()
    }

    /// The editor paints a type where one resolves, so the REPL does too: a
    /// capitalised word nothing declares is an ordinary identifier, and
    /// colouring it would claim a resolution the session does not have.
    #[test]
    fn only_a_name_that_resolves_to_a_type_is_painted_as_one() {
        let mut helper = GosReplHelper::new();
        helper.set_declarations(&["struct Point { x: i64 }".to_string()]);

        let painted = helper.highlight("Point", 0);
        assert!(
            painted.contains(MAGENTA),
            "a session-declared type is painted: {painted:?}"
        );

        let builtin = helper.highlight("Vec", 0);
        assert!(
            builtin.contains(MAGENTA),
            "a language type is painted: {builtin:?}"
        );

        let unknown = helper.highlight("Bogus", 0);
        assert!(
            !unknown.contains(MAGENTA),
            "a capitalised name nothing declares is left alone: {unknown:?}"
        );

        helper.reset_session();
        let dropped = helper.highlight("Point", 0);
        assert!(
            !dropped.contains(MAGENTA),
            "clearing the session forgets its types: {dropped:?}"
        );
    }

    /// A cursor sitting straight after the dot is the moment the whole member
    /// surface is worth offering, so an empty prefix lists every member the
    /// receiver has rather than nothing.
    #[test]
    fn a_bare_dot_offers_the_receivers_methods() {
        let owners = surfaces(&[("x", "String")]);
        let (start, candidates) = complete_at("x.", 2, &owners, &[]);
        assert_eq!(start, 2, "the replacement begins after the dot");
        assert!(
            !candidates.is_empty(),
            "a bare dot on a String binding offers its methods"
        );
        assert!(
            candidates.iter().any(|m| m == "len"),
            "String methods are offered: {candidates:?}"
        );
        // A prefix after the dot still narrows the same surface.
        let (narrow_start, narrowed) = complete_at("x.le", 4, &owners, &[]);
        assert_eq!(narrow_start, 2);
        assert!(narrowed.iter().any(|m| m == "len"), "{narrowed:?}");
        assert!(
            narrowed.len() < candidates.len(),
            "a prefix narrows the candidate list"
        );
        // An unknown receiver still completes nothing rather than guessing.
        let (_, unknown) = complete_at("zzz.", 4, &owners, &[]);
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    /// Every receiver `%explain` reports a method surface for completes it.
    /// A fixed array is the case that reads like a `Vec` and is a type of
    /// its own, so it is the one worth naming here.
    #[test]
    fn every_owner_with_a_method_surface_completes() {
        for owner in [
            "Array", "Slice", "Vec", "String", "Map", "Set", "Deque", "Queue", "Stack", "MinHeap",
            "MaxHeap", "Iterator", "Option", "Result", "Tuple",
        ] {
            let owners = surfaces(&[("x", owner)]);
            let (_, candidates) = complete_at("x.", 2, &owners, &[]);
            assert!(!candidates.is_empty(), "{owner} offers no completions");
        }
        let array = surfaces(&[("a", "Array")]);
        let (_, candidates) = complete_at("a.", 2, &array, &[]);
        for expected in ["len", "iter", "to_vec", "map"] {
            assert!(
                candidates.iter().any(|m| m == expected),
                "a fixed array completes {expected}: {candidates:?}"
            );
        }
        assert!(
            !candidates.iter().any(|m| m == "push"),
            "a fixed array does not resize: {candidates:?}"
        );
    }

    /// A binding of a session-declared type reaches its fields and the
    /// methods its `impl` blocks give it, which is what `%explain` reports
    /// for one.
    #[test]
    fn a_session_type_completes_its_fields_and_methods() {
        let declarations = vec![
            "struct Point { x: i64, y: i64 }".to_string(),
            "impl Point { fn norm(&self) -> i64 { self.x } fn origin() -> Point { Point { x: 0, y: 0 } } }"
                .to_string(),
        ];
        let owners = surfaces(&[("p", "Point")]);
        let (start, candidates) = complete_at("p.", 2, &owners, &declarations);
        assert_eq!(start, 2);
        assert_eq!(candidates, vec!["norm", "x", "y"], "{candidates:?}");
    }

    /// A tuple is read by position, so its positions complete alongside the
    /// methods every tuple has.
    #[test]
    fn a_tuple_completes_its_positions() {
        let mut surface = BindingSurface::owned_by("Tuple");
        surface.set_tuple_arity(2);
        let owners = HashMap::from([("t".to_string(), surface)]);
        let (_, candidates) = complete_at("t.", 2, &owners, &[]);
        assert!(candidates.iter().any(|m| m == "0"), "{candidates:?}");
        assert!(candidates.iter().any(|m| m == "1"), "{candidates:?}");
        assert!(candidates.iter().any(|m| m == "len"), "{candidates:?}");
    }

    /// Identifiers are Unicode, so the word the cursor sits in is found by
    /// walking characters rather than ASCII bytes.
    #[test]
    fn a_unicode_binding_name_completes_its_members() {
        let owners = surfaces(&[("café", "String")]);
        let (start, candidates) = complete_at("café.le", "café.le".len(), &owners, &[]);
        assert_eq!(start, "café.".len());
        assert!(candidates.iter().any(|m| m == "len"), "{candidates:?}");
    }

    /// A type's own name reaches its surface too: `%info` reports a
    /// constructor under the type that declares it, so the qualified
    /// spelling completes it.
    #[test]
    fn a_type_name_completes_its_own_surface() {
        let (start, candidates) = complete_at("Vec::in", 7, &HashMap::new(), &[]);
        assert_eq!(start, 0);
        assert!(
            candidates.iter().any(|c| c == "Vec::insert"),
            "{candidates:?}"
        );

        let declarations = vec![
            "struct Point { x: i64 }".to_string(),
            "impl Point { fn origin() -> Point { Point { x: 0 } } }".to_string(),
        ];
        let (_, candidates) = complete_at("Point::or", 9, &HashMap::new(), &declarations);
        assert!(
            candidates.iter().any(|c| c == "Point::origin"),
            "{candidates:?}"
        );
    }

    #[test]
    fn keyword_prefix_completes() {
        let (start, cands) = complete_at("le", 2, &HashMap::new(), &[]);
        assert_eq!(start, 0);
        assert!(cands.iter().any(|c| c == "let"));
    }

    #[test]
    fn empty_word_yields_nothing() {
        let (_, cands) = complete_at("let x = ", 8, &HashMap::new(), &[]);
        assert!(cands.is_empty());
    }

    #[test]
    fn qualified_path_completes_member() {
        let (start, cands) = complete_at("println(strings::jo", 19, &HashMap::new(), &[]);
        assert_eq!(start, 8);
        assert!(
            cands.iter().any(|c| c == "strings::join"),
            "expected strings::join in {cands:?}"
        );
        // A qualified word must not pull in keyword candidates.
        assert!(cands.iter().all(|c| c.contains("::")));
    }

    #[test]
    fn completion_offset_is_word_start_not_line_start() {
        let (start, _) = complete_at("    fo", 6, &HashMap::new(), &[]);
        assert_eq!(start, 4);
    }

    #[test]
    fn set_binding_completes_methods_after_dot() {
        let mut helper = GosReplHelper::new();
        helper.set_binding_surface("set", Some(BindingSurface::owned_by("Set")));

        let (start, cands) = complete_at("set.ins", 7, &helper.binding_surfaces, &[]);
        assert_eq!(start, 4);
        assert_eq!(cands, vec!["insert"]);

        let (_, cands) = complete_at("set.insecure", 12, &helper.binding_surfaces, &[]);
        assert!(cands.is_empty());
    }

    #[test]
    fn stack_binding_completes_methods_after_dot() {
        let mut helper = GosReplHelper::new();
        helper.set_binding_surface("x", Some(BindingSurface::owned_by("Stack")));

        let (start, cands) = complete_at("x.l", 3, &helper.binding_surfaces, &[]);
        assert_eq!(start, 2);
        assert_eq!(cands, vec!["len"]);
    }

    #[test]
    fn meta_command_regex_is_never_treated_as_incomplete_source() {
        assert_eq!(incomplete_reason("%info ["), None);
        assert_eq!(incomplete_reason("%bindings foo("), None);
    }

    #[test]
    fn continuation_indent_steps_in_after_an_opening_delimiter() {
        assert_eq!(continuation_indent("fn main() {"), "    ");
        assert_eq!(continuation_indent("    let values = ["), "        ");
    }

    #[test]
    fn continuation_indent_preserves_the_current_level() {
        assert_eq!(continuation_indent("fn main() {\n    let x = 1"), "    ");
        assert_eq!(
            continuation_indent("fn main() { // setup"),
            "    ",
            "a trailing comment should not hide the opening brace"
        );
    }
}
