//! Token-stream formatter: the comment- and macro-preserving engine
//! behind `gos fmt`.
//!
//! The formatter works from the raw lexer token stream - which retains
//! comments and whitespace trivia with exact byte spans - rather than
//! from the parsed AST, so output keeps every comment, macro call, and
//! surface construct exactly as authored. The parser is consulted only
//! to validate the input (unparseable sources are refused). Authored
//! line breaks are preserved; the formatter normalises inter-token
//! spacing and indentation. A final self-check re-lexes the output and
//! refuses to return text whose non-whitespace token stream differs
//! from the input's.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::fmt;

use gossamer_lex::{FileId, Keyword, Punct, TokenKind, tokenize};

use crate::{ParseDiagnostic, parse_source_file};

/// Number of spaces per indentation level.
const INDENT_WIDTH: usize = 4;

/// Opening and closing delimiter of a triple-quoted string literal.
const TRIPLE_QUOTE: &str = "\"\"\"";

/// Maximum run of blank lines preserved between statements.
const MAX_BLANK_LINES: u32 = 2;

/// Why the formatter declined to produce output.
#[derive(Debug)]
pub enum FormatError {
    /// The source failed to lex or parse; the formatter refuses to
    /// touch input it cannot prove it understands.
    Parse(Vec<ParseDiagnostic>),
    /// The no-destruction gate found that the rendered output would
    /// have changed, merged, or dropped a non-whitespace token.
    /// Nothing should be written when this is returned.
    SelfCheck(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(diags) => {
                write!(f, "{} parse error(s); refusing to format", diags.len())
            }
            Self::SelfCheck(detail) => {
                write!(
                    f,
                    "formatter self-check failed ({detail}); refusing to write output"
                )
            }
        }
    }
}

impl std::error::Error for FormatError {}

/// Formats Gossamer source, preserving comments and authored line
/// structure while normalising spacing and indentation.
///
/// Returns `FormatError::Parse` when the input does not parse and
/// `FormatError::SelfCheck` when the rendered output fails the
/// token-equivalence gate; in both cases callers must leave the
/// original file untouched.
pub fn format_source(source: &str, file: FileId) -> Result<String, FormatError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let (_ast, diags) = parse_source_file(source, file);
    if !diags.is_empty() {
        return Err(FormatError::Parse(diags));
    }
    let lines = split_lines(source, file);
    if lines.is_empty() {
        return Ok(String::new());
    }
    let formatted = render(&lines, file);
    verify_equivalence(source, &formatted, file)?;
    Ok(formatted)
}

/// One significant (non-whitespace) token plus its same-line leading
/// trivia.
#[derive(Clone, Copy)]
struct Tok<'src> {
    kind: TokenKind,
    text: &'src str,
    /// Whitespace between the previous token and this one, restricted
    /// to the portion after the last newline (same-line gap).
    gap: &'src str,
}

impl Tok<'_> {
    fn is_comment(&self) -> bool {
        matches!(self.kind, TokenKind::LineComment | TokenKind::BlockComment)
    }
}

/// A source line: its significant tokens and how many blank lines the
/// author left before it.
struct Line<'src> {
    blank_before: u32,
    toks: Vec<Tok<'src>>,
}

/// Splits the token stream into author-defined lines. Line breaks come
/// only from whitespace trivia; newlines inside block comments or raw
/// strings do not split (those tokens stay atomic and are emitted
/// verbatim).
fn split_lines(source: &str, file: FileId) -> Vec<Line<'_>> {
    let (raw, _errs) = tokenize(source, file);
    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut current: Vec<Tok<'_>> = Vec::new();
    let mut current_blank = 0u32;
    let mut pending_newlines = 0u32;
    let mut gap: &str = "";
    for (index, token) in raw.iter().enumerate() {
        let text = &source[token.span.start as usize..token.span.end as usize];
        match token.kind {
            TokenKind::Whitespace => {
                let newlines = u32::try_from(text.matches('\n').count()).unwrap_or(u32::MAX);
                if newlines > 0 {
                    if !current.is_empty() {
                        lines.push(Line {
                            blank_before: current_blank,
                            toks: std::mem::take(&mut current),
                        });
                    }
                    pending_newlines += newlines;
                    gap = text.rsplit('\n').next().unwrap_or("");
                } else {
                    gap = text;
                }
            }
            TokenKind::Eof => break,
            _ => {
                if is_trailing_semicolon(&raw, index, source) {
                    continue;
                }
                if current.is_empty() {
                    current_blank = if lines.is_empty() {
                        0
                    } else {
                        pending_newlines.saturating_sub(1)
                    };
                }
                current.push(Tok {
                    kind: token.kind,
                    text,
                    gap,
                });
                pending_newlines = 0;
                gap = "";
            }
        }
    }
    if !current.is_empty() {
        lines.push(Line {
            blank_before: current_blank,
            toks: current,
        });
    }
    lines
}

fn is_trailing_semicolon(tokens: &[gossamer_lex::Token], index: usize, source: &str) -> bool {
    let token = &tokens[index];
    if token.kind != TokenKind::Punct(Punct::Semi) {
        return false;
    }
    let next = tokens[index + 1..]
        .iter()
        .find(|next| !matches!(next.kind, TokenKind::Whitespace));
    let next_start = next.map_or(source.len(), |next| next.span.start as usize);
    source[token.span.end as usize..next_start].contains('\n')
        || next.is_none_or(|next| {
            matches!(next.kind, TokenKind::Punct(Punct::RBrace) | TokenKind::Eof)
        })
}

/// What a `{` opens: a code block / struct body, or a `use ...::{...}`
/// import list (which keeps its contents tight against the braces).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BraceKind {
    Paren,
    Bracket,
    Block,
    Match,
    DeclList,
    UseList,
}

/// One open bracket: its kind plus the indent level (and continuation
/// status) of the line that opened it. Children indent one level past
/// the opening line, no matter how many brackets that line left open,
/// and a closing line realigns with the opening line.
#[derive(Clone, Copy)]
struct Open {
    kind: BraceKind,
    line_level: usize,
    line_was_cont: bool,
}

/// Operator role resolved from left context, for tokens whose spacing
/// depends on whether they are used in unary or binary position.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Binary,
    Unary,
    OpenPipe,
    ClosePipe,
    Plain,
}

/// The previously emitted code token, with everything spacing rules
/// need to know about it.
#[derive(Clone, Copy)]
struct Emitted<'src> {
    kind: TokenKind,
    text: &'src str,
    class: Class,
    brace: Option<BraceKind>,
}

/// One rendered output line awaiting final indent resolution.
struct Entry {
    blanks: u32,
    indent: EntryIndent,
    body: String,
}

/// Indent state for a rendered line. Comment-only lines borrow the
/// continuation bonus of the next code line so a comment inside a
/// multi-line chain stays aligned with the chain.
enum EntryIndent {
    Code { indent: usize, carry: usize },
    Comment { depth: usize },
}

/// Renders the line model back to text.
fn render(lines: &[Line<'_>], file: FileId) -> String {
    let mut entries: Vec<Entry> = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut prev_sig: Option<TokenKind> = None;
    let mut cont_after_prev_line = false;
    let mut prev_code_level = 0usize;
    let mut prev_code_was_cont = false;
    // One blank line separates the leading top-level `use` block from
    // whatever follows it; authored line choices are otherwise kept.
    let mut prev_was_top_use = false;
    for line in lines {
        let child_level = stack.last().map_or(0, |open| open.line_level + 1);
        if line.toks.iter().all(Tok::is_comment) {
            let mut blanks = line.blank_before;
            if prev_was_top_use && blanks == 0 {
                blanks = 1;
            }
            prev_was_top_use = false;
            entries.push(Entry {
                blanks,
                indent: EntryIndent::Comment { depth: child_level },
                body: comment_only_body(&line.toks),
            });
            continue;
        }
        let lead = line.toks.iter().take_while(|t| is_closer(t.kind)).count();
        // First non-comment token decides line-start continuation.
        let starts_cont = line
            .toks
            .iter()
            .find(|t| !t.is_comment())
            .is_some_and(|t| starts_continuation(t.kind));
        let inside_decl_list = stack
            .last()
            .is_some_and(|open| open.kind == BraceKind::DeclList);
        let is_cont = lead == 0 && !inside_decl_list && (starts_cont || cont_after_prev_line);
        let (line_level, line_was_cont) = if lead > 0 {
            // A closing line realigns with the shallowest opener in its
            // leading closer run (`})` aligns with the statement, not
            // the inner brace), and carries that line's continuation
            // status forward so a chain can resume after the closer.
            let from = stack.len().saturating_sub(lead);
            stack[from..]
                .iter()
                .min_by_key(|open| open.line_level)
                .map_or((0, false), |open| (open.line_level, open.line_was_cont))
        } else if is_cont {
            // A continuation indents one level past the line that
            // started the statement; consecutive continuation lines
            // stay at that level.
            if prev_code_was_cont {
                (prev_code_level, true)
            } else {
                (child_level.max(prev_code_level + 1), true)
            }
        } else {
            (child_level, false)
        };
        let starts_in_top_use_list = stack
            .iter()
            .any(|open| open.kind == BraceKind::UseList && open.line_level == 0);
        let body = render_code_line(
            line,
            file,
            line_level,
            line_was_cont,
            &mut stack,
            &mut prev_sig,
        );
        cont_after_prev_line = line_has_trailing_continuation(line);
        prev_code_level = line_level;
        prev_code_was_cont = line_was_cont;
        let is_top_use = line_level == 0
            && !line_was_cont
            && line
                .toks
                .iter()
                .find(|t| !t.is_comment())
                .is_some_and(|t| t.text == "use");
        let is_top_use_part = is_top_use || starts_in_top_use_list;
        let mut blanks = line.blank_before;
        if prev_was_top_use && !is_top_use_part && blanks == 0 {
            blanks = 1;
        }
        prev_was_top_use = is_top_use_part;
        entries.push(Entry {
            blanks,
            indent: EntryIndent::Code {
                indent: INDENT_WIDTH * line_level,
                carry: line_level.saturating_sub(child_level),
            },
            body,
        });
    }
    assemble(&entries)
}

/// Joins a comment-only line's tokens, preserving authored gaps
/// between multiple comments on the same line.
fn comment_only_body(toks: &[Tok<'_>]) -> String {
    let mut body = String::new();
    for (index, tok) in toks.iter().enumerate() {
        if index > 0 {
            if tok.gap.is_empty() {
                body.push(' ');
            } else {
                body.push_str(tok.gap);
            }
        }
        body.push_str(tok.text);
    }
    body
}

/// Renders one code line, updating the bracket stack and significant
/// previous-token state as it walks.
fn render_code_line<'src>(
    line: &Line<'src>,
    file: FileId,
    line_level: usize,
    line_was_cont: bool,
    stack: &mut Vec<Open>,
    prev_sig: &mut Option<TokenKind>,
) -> String {
    let mut body = String::new();
    let mut closure_pipe: Option<usize> = None;
    let mut prev: Option<Emitted<'src>> = None;
    let mut prev_was_comment = false;
    let mut first_code = true;
    for (tok_index, tok) in line.toks.iter().enumerate() {
        if tok.is_comment() {
            if !body.is_empty() {
                if tok.gap.is_empty() {
                    body.push(' ');
                } else {
                    body.push_str(tok.gap);
                }
            }
            body.push_str(tok.text);
            prev_was_comment = true;
            continue;
        }
        // Every delimited list separates with a comma on one line and a
        // newline when multiline, so a comma that is the last thing on its
        // line inside ANY delimiter is a separator the newline already
        // provides. That covers arguments, parameters, struct and enum
        // fields, tuples, `Vec` / array / `Map` / `Set` literals, patterns,
        // generic lists, and `use` lists - one rule rather than a list of
        // shapes to keep in step. A plain code block is included with
        // them: a statement cannot end in a bare comma, so the only comma
        // that can be last on a line inside one is a list's.
        let in_optional_comma_list = !stack.is_empty();
        if in_optional_comma_list
            && tok.kind == TokenKind::Punct(Punct::Comma)
            && line.toks[tok_index + 1..].iter().all(Tok::is_comment)
        {
            *prev_sig = Some(tok.kind);
            continue;
        }
        let mut class = classify(tok.kind, *prev_sig, &mut closure_pipe, stack.len());
        // SPEC: a newline followed by a leading `&`, `*`, or `-`
        // always starts a new statement, so those read as unary here.
        if first_code
            && matches!(
                tok.kind,
                TokenKind::Punct(Punct::Minus | Punct::Star | Punct::Amp)
            )
        {
            class = Class::Unary;
        }
        first_code = false;
        let cur_brace = if tok.kind == TokenKind::Punct(Punct::RBrace) {
            stack.last().map(|open| open.kind)
        } else {
            None
        };
        if !body.is_empty() {
            let space = if prev_was_comment {
                !tok.gap.is_empty()
            } else if let Some(prev) = &prev {
                let mut space = decide_space(prev, tok, class, cur_brace);
                if !space && pair_merges(file, prev, tok) {
                    space = true;
                }
                space
            } else {
                // Leading block comment already filled `body`.
                !tok.gap.is_empty()
            };
            if space {
                body.push(' ');
            }
        }
        if tok.kind == TokenKind::TripleStringLit {
            body.push_str(&reindent_triple_string(tok.text, INDENT_WIDTH * line_level));
        } else {
            body.push_str(tok.text);
        }
        let declaration_brace =
            line_opens_brace_after(line, tok_index, &[Keyword::Struct, Keyword::Enum]);
        let match_brace = line_opens_brace_after(line, tok_index, &[Keyword::Match]);
        let brace = update_stack(
            tok.kind,
            *prev_sig,
            line_level,
            line_was_cont,
            declaration_brace,
            match_brace,
            stack,
        );
        if matches!(
            tok.kind,
            TokenKind::Punct(Punct::LBrace | Punct::RBrace | Punct::FatArrow | Punct::Semi)
        ) {
            closure_pipe = None;
        }
        prev = Some(Emitted {
            kind: tok.kind,
            text: tok.text,
            class,
            brace,
        });
        *prev_sig = Some(tok.kind);
        prev_was_comment = false;
    }
    body
}

/// Re-renders a multi-line triple-quoted literal with `indent` spaces
/// before every content line and before its closing delimiter.
///
/// Each line keeps its offset from the literal's measured indentation,
/// so the value the parser reads is the same before and after, and the
/// block travels with the line that opens it.
fn reindent_triple_string(raw: &str, indent: usize) -> String {
    let parsed = gossamer_lex::triple_string(raw);
    if !parsed.multiline {
        return raw.to_string();
    }
    let prefix = " ".repeat(indent);
    let mut out = String::with_capacity(raw.len() + indent * parsed.lines.len());
    out.push_str(TRIPLE_QUOTE);
    for line in &parsed.lines {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(&prefix);
            out.push_str(line);
        }
    }
    if parsed.closer_on_own_line {
        out.push('\n');
        out.push_str(&prefix);
    }
    out.push_str(TRIPLE_QUOTE);
    out
}

fn line_opens_brace_after(line: &Line<'_>, index: usize, keywords: &[Keyword]) -> bool {
    line.toks[index].kind == TokenKind::Punct(Punct::LBrace)
        && line.toks[..index].iter().any(
            |prior| matches!(prior.kind, TokenKind::Keyword(keyword) if keywords.contains(&keyword)),
        )
}

/// Pushes or pops the bracket stack for `kind`; returns the brace kind
/// when `kind` opens or closes a brace pair.
fn update_stack(
    kind: TokenKind,
    prev_sig: Option<TokenKind>,
    line_level: usize,
    line_was_cont: bool,
    declaration_brace: bool,
    match_brace: bool,
    stack: &mut Vec<Open>,
) -> Option<BraceKind> {
    let open = |kind: BraceKind| Open {
        kind,
        line_level,
        line_was_cont,
    };
    match kind {
        TokenKind::Punct(Punct::LParen) => {
            stack.push(open(BraceKind::Paren));
            Some(BraceKind::Paren)
        }
        TokenKind::Punct(Punct::LBracket) => {
            stack.push(open(BraceKind::Bracket));
            Some(BraceKind::Bracket)
        }
        TokenKind::Punct(Punct::LBrace) => {
            let brace = if declaration_brace {
                BraceKind::DeclList
            } else if match_brace {
                BraceKind::Match
            } else if prev_sig == Some(TokenKind::Punct(Punct::ColonColon)) {
                BraceKind::UseList
            } else {
                BraceKind::Block
            };
            // A block brace anchors to its statement context: a `{`
            // at the end of a multi-line header (`for ... in [...,
            // ...] {`) indents its body one past the header's first
            // line, not one past the physical line.
            let anchor = line_level.min(stack.last().map_or(0, |top| top.line_level + 1));
            stack.push(Open {
                kind: brace,
                line_level: anchor,
                line_was_cont: line_was_cont && anchor == line_level,
            });
            Some(brace)
        }
        TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
            stack.pop().map(|open| open.kind)
        }
        _ => None,
    }
}

/// Resolves the unary/binary/closure-pipe role of an operator token
/// from what precedes it.
fn classify(
    kind: TokenKind,
    prev_sig: Option<TokenKind>,
    closure_pipe: &mut Option<usize>,
    depth: usize,
) -> Class {
    let prev_ends = prev_sig.is_some_and(ends_expr);
    match kind {
        TokenKind::Punct(Punct::Minus | Punct::Star | Punct::Amp) => {
            if prev_ends {
                Class::Binary
            } else {
                Class::Unary
            }
        }
        TokenKind::Punct(Punct::Pipe) => {
            if *closure_pipe == Some(depth) {
                *closure_pipe = None;
                Class::ClosePipe
            } else if prev_ends {
                Class::Binary
            } else {
                *closure_pipe = Some(depth);
                Class::OpenPipe
            }
        }
        _ => Class::Plain,
    }
}

/// `true` when a token can end an expression (so an operator after it
/// reads as binary, and `(`/`[` after it read as call/index).
fn ends_expr(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::IntLit
            | TokenKind::FloatLit
            | TokenKind::StringLit
            | TokenKind::RawStringLit { .. }
            | TokenKind::TripleStringLit
            | TokenKind::CharLit
            | TokenKind::ByteLit
            | TokenKind::ByteStringLit
            | TokenKind::RawByteStringLit { .. }
            | TokenKind::Keyword(
                Keyword::True | Keyword::False | Keyword::SelfLower | Keyword::SelfUpper
            )
            | TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace | Punct::Question)
    )
}

/// `true` for keywords that behave like operands (`true`, `self`, ...)
/// rather than spacing keywords.
fn operand_keyword(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::True | Keyword::False | Keyword::SelfLower | Keyword::SelfUpper
    )
}

/// `true` for `<`, `>`, `<<`, `>>` - ambiguous between generics and
/// comparison/shift, so spacing around them is preserved as authored.
fn angle_like(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Punct(Punct::Lt | Punct::Gt | Punct::ShiftL | Punct::ShiftR)
    )
}

/// Operators that always take a space on their right.
fn spaced_after(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Punct(
            Punct::Plus
                | Punct::Slash
                | Punct::Percent
                | Punct::PlusPercent
                | Punct::MinusPercent
                | Punct::StarPercent
                | Punct::Caret
                | Punct::AmpAmp
                | Punct::PipePipe
                | Punct::PipeGt
                | Punct::EqEq
                | Punct::NotEq
                | Punct::LtEq
                | Punct::GtEq
                | Punct::Eq
                | Punct::PlusEq
                | Punct::MinusEq
                | Punct::StarEq
                | Punct::SlashEq
                | Punct::PercentEq
                | Punct::PlusPercentEq
                | Punct::MinusPercentEq
                | Punct::StarPercentEq
                | Punct::AmpEq
                | Punct::PipeEq
                | Punct::CaretEq
                | Punct::ShiftLEq
                | Punct::ShiftREq
                | Punct::Arrow
                | Punct::FatArrow
                | Punct::At
                | Punct::Comma
                | Punct::Semi
                | Punct::Colon
        )
    )
}

/// Operators that always take a space on their left.
fn spaced_before(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Punct(
            Punct::Plus
                | Punct::Slash
                | Punct::Percent
                | Punct::PlusPercent
                | Punct::MinusPercent
                | Punct::StarPercent
                | Punct::Caret
                | Punct::AmpAmp
                | Punct::PipePipe
                | Punct::PipeGt
                | Punct::EqEq
                | Punct::NotEq
                | Punct::LtEq
                | Punct::GtEq
                | Punct::Eq
                | Punct::PlusEq
                | Punct::MinusEq
                | Punct::StarEq
                | Punct::SlashEq
                | Punct::PercentEq
                | Punct::PlusPercentEq
                | Punct::MinusPercentEq
                | Punct::StarPercentEq
                | Punct::AmpEq
                | Punct::PipeEq
                | Punct::CaretEq
                | Punct::ShiftLEq
                | Punct::ShiftREq
                | Punct::Arrow
                | Punct::FatArrow
                | Punct::At
        )
    )
}

/// Decides whether a single space separates `prev` and `cur`.
/// Rules are ordered; the first match wins. The fall-through preserves
/// the author's choice (any gap becomes one space, no gap stays none).
fn decide_space(
    prev: &Emitted<'_>,
    cur: &Tok<'_>,
    cur_class: Class,
    cur_brace: Option<BraceKind>,
) -> bool {
    use Punct as P;
    use TokenKind as TK;
    let had_gap = !cur.gap.is_empty();
    // Empty brace pairs follow the author: `=> {}` and `struct S { }`
    // are both established styles in the corpus.
    if prev.kind == TK::Punct(P::LBrace) && cur.kind == TK::Punct(P::RBrace) {
        return had_gap;
    }
    // Block braces space their contents; use-list braces stay tight.
    if prev.kind == TK::Punct(P::LBrace) {
        return !matches!(prev.brace, Some(BraceKind::UseList)) || had_gap;
    }
    if cur.kind == TK::Punct(P::RBrace) {
        return !matches!(cur_brace, Some(BraceKind::UseList)) || had_gap;
    }
    // Hard no-space-before.
    if matches!(
        cur.kind,
        TK::Punct(
            P::Comma
                | P::Semi
                | P::RParen
                | P::RBracket
                | P::Dot
                | P::ColonColon
                | P::Question
                | P::Colon
        )
    ) {
        return false;
    }
    if cur.kind == TK::Punct(P::Bang) && ends_expr(prev.kind) {
        return false;
    }
    // Anonymous function expressions and `fn(...)` types hug the paren.
    if prev.kind == TK::Keyword(Keyword::Fn) && cur.kind == TK::Punct(P::LParen) {
        return false;
    }
    if matches!(cur.kind, TK::Punct(P::DotDot | P::DotDotEq | P::DotDotDot)) && ends_expr(prev.kind)
    {
        return false;
    }
    // Hard no-space-after.
    if matches!(
        prev.kind,
        TK::Punct(
            P::LParen
                | P::LBracket
                | P::Dot
                | P::ColonColon
                | P::Hash
                | P::Bang
                | P::DotDot
                | P::DotDotEq
                | P::DotDotDot
        )
    ) {
        return false;
    }
    if matches!(prev.class, Class::Unary | Class::OpenPipe) {
        return false;
    }
    if cur_class == Class::ClosePipe {
        return false;
    }
    // Generic-vs-comparison ambiguity: keep the author's spacing.
    if angle_like(prev.kind) || angle_like(cur.kind) {
        return had_gap;
    }
    // Spaced operators and separators.
    if matches!(prev.class, Class::Binary | Class::ClosePipe) || spaced_after(prev.kind) {
        return true;
    }
    if cur_class == Class::Binary || spaced_before(cur.kind) {
        return true;
    }
    // Keywords keep a space on both sides.
    if let TK::Keyword(kw) = prev.kind
        && !operand_keyword(kw)
    {
        return true;
    }
    if let TK::Keyword(kw) = cur.kind
        && !operand_keyword(kw)
    {
        return true;
    }
    // Blocks and struct literals open with a space; calls and indexing
    // hug their callee.
    if cur.kind == TK::Punct(P::LBrace) {
        return true;
    }
    if matches!(cur.kind, TK::Punct(P::LParen | P::LBracket)) && ends_expr(prev.kind) {
        return false;
    }
    had_gap
}

/// `true` when joining `prev` and `cur` with no space would re-lex as
/// something other than the same two tokens (e.g. `|` + `|` → `||`).
fn pair_merges(file: FileId, prev: &Emitted<'_>, cur: &Tok<'_>) -> bool {
    let mut joined = String::with_capacity(prev.text.len() + cur.text.len());
    joined.push_str(prev.text);
    joined.push_str(cur.text);
    let (tokens, errs) = tokenize(&joined, file);
    if !errs.is_empty() {
        return true;
    }
    let sig: Vec<_> = tokens
        .iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Eof))
        .collect();
    sig.len() != 2
        || sig[0].kind != prev.kind
        || sig[1].kind != cur.kind
        || sig[0].span.len() as usize != prev.text.len()
}

/// `true` for tokens that close a bracket pair.
fn is_closer(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace)
    )
}

/// `true` when a line starting with this token continues the previous
/// expression (`|>` chains, method chains). Lines starting with `&`,
/// `*`, or `-` begin a new statement per SPEC and get no bonus.
fn starts_continuation(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Punct(Punct::PipeGt | Punct::Dot))
}

/// `true` when a line ends in an operator that continues onto the next
/// line. Generic type closers are excluded even though the lexer uses
/// the same `>` and `>>` tokens for them.
fn line_has_trailing_continuation(line: &Line<'_>) -> bool {
    let Some((index, token)) = line
        .toks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, token)| !token.is_comment())
    else {
        return false;
    };
    if matches!(token.kind, TokenKind::Punct(Punct::Gt | Punct::ShiftR))
        && closes_generic_type(line, index)
    {
        return false;
    }
    trailing_continuation(token.kind)
}

/// Recognises a final `>` / `>>` as the balanced close of a type
/// argument list in a type annotation. Requiring a type-context token
/// before the matching `<` keeps compact comparisons such as `x<y>`
/// in the operator path.
fn closes_generic_type(line: &Line<'_>, close_index: usize) -> bool {
    let mut depth = match line.toks[close_index].kind {
        TokenKind::Punct(Punct::Gt) => 1,
        TokenKind::Punct(Punct::ShiftR) => 2,
        _ => return false,
    };

    for (index, token) in line.toks[..close_index].iter().enumerate().rev() {
        match token.kind {
            TokenKind::Punct(Punct::Gt) => depth += 1,
            TokenKind::Punct(Punct::ShiftR) => depth += 2,
            TokenKind::Punct(Punct::Lt) if depth == 1 => {
                let context = &line.toks[..index];
                let Some(context_index) = context.iter().rposition(|candidate| {
                    matches!(
                        candidate.kind,
                        TokenKind::Punct(Punct::Colon | Punct::Arrow)
                            | TokenKind::Keyword(Keyword::Fn)
                    )
                }) else {
                    return false;
                };
                return !context[context_index + 1..].iter().any(|candidate| {
                    matches!(
                        candidate.kind,
                        TokenKind::Punct(Punct::Eq | Punct::FatArrow)
                    )
                });
            }
            TokenKind::Punct(Punct::Lt) => depth -= 1,
            _ => {}
        }
    }
    false
}

/// `true` when a token can trail a continued expression.
fn trailing_continuation(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Punct(
            Punct::Eq
                | Punct::PlusEq
                | Punct::MinusEq
                | Punct::StarEq
                | Punct::SlashEq
                | Punct::PercentEq
                | Punct::PlusPercentEq
                | Punct::MinusPercentEq
                | Punct::StarPercentEq
                | Punct::AmpEq
                | Punct::PipeEq
                | Punct::CaretEq
                | Punct::ShiftLEq
                | Punct::ShiftREq
                | Punct::Plus
                | Punct::Minus
                | Punct::Star
                | Punct::Slash
                | Punct::Percent
                | Punct::PlusPercent
                | Punct::MinusPercent
                | Punct::StarPercent
                | Punct::Amp
                | Punct::Caret
                | Punct::AmpAmp
                | Punct::PipePipe
                | Punct::PipeGt
                | Punct::EqEq
                | Punct::NotEq
                | Punct::Lt
                | Punct::LtEq
                | Punct::Gt
                | Punct::GtEq
                | Punct::ShiftL
                | Punct::ShiftR
                | Punct::Arrow
                | Punct::FatArrow
                | Punct::Dot
                | Punct::ColonColon
        )
    ) || kind == TokenKind::Keyword(Keyword::As)
}

/// Resolves comment-line indents and joins entries into final text.
fn assemble(entries: &[Entry]) -> String {
    let mut resolved: Vec<usize> = vec![0; entries.len()];
    let mut next_carry = 0usize;
    for (index, entry) in entries.iter().enumerate().rev() {
        match &entry.indent {
            EntryIndent::Code { indent, carry } => {
                resolved[index] = *indent;
                next_carry = *carry;
            }
            EntryIndent::Comment { depth } => {
                resolved[index] = INDENT_WIDTH * (depth + next_carry);
            }
        }
    }
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
            for _ in 0..entry.blanks.min(MAX_BLANK_LINES) {
                out.push('\n');
            }
        }
        for _ in 0..resolved[index] {
            out.push(' ');
        }
        out.push_str(&entry.body);
    }
    out.push('\n');
    out
}

/// The no-destruction gate: every non-whitespace token (comments
/// included) of `before` must reappear in `after` with identical kind
/// and value.
fn verify_equivalence(before: &str, after: &str, file: FileId) -> Result<(), FormatError> {
    let original = significant_tokens(before, file);
    let (_tokens, errs) = tokenize(after, file);
    if !errs.is_empty() {
        return Err(FormatError::SelfCheck(format!(
            "output no longer lexes cleanly ({} error(s))",
            errs.len()
        )));
    }
    let rendered = significant_tokens(after, file);
    if original.len() != rendered.len() {
        return Err(FormatError::SelfCheck(format!(
            "token count changed from {} to {}",
            original.len(),
            rendered.len()
        )));
    }
    for (index, (a, b)) in original.iter().zip(&rendered).enumerate() {
        if a != b {
            return Err(FormatError::SelfCheck(format!(
                "token {index} changed from `{}` to `{}`",
                a.1, b.1
            )));
        }
    }
    Ok(())
}

/// Collects every non-whitespace token of `source` as `(kind, value)`.
///
/// A triple-quoted literal's indentation is layout the formatter owns,
/// so it is compared by the contents it decodes to rather than by the
/// text that spells it.
fn significant_tokens(source: &str, file: FileId) -> Vec<(TokenKind, Cow<'_, str>)> {
    let (tokens, _errs) = tokenize(source, file);
    tokens
        .iter()
        .enumerate()
        .filter(|(index, token)| {
            if matches!(token.kind, TokenKind::Whitespace | TokenKind::Eof) {
                return false;
            }
            if is_trailing_semicolon(&tokens, *index, source) {
                return false;
            }
            if token.kind != TokenKind::Punct(Punct::Comma) {
                return true;
            }
            let next_start = tokens[index + 1..]
                .iter()
                .find(|next| {
                    !matches!(
                        next.kind,
                        TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
                    )
                })
                .map_or(source.len(), |next| next.span.start as usize);
            !source[token.span.end as usize..next_start].contains('\n')
        })
        .map(|(_, token)| {
            let text = &source[token.span.start as usize..token.span.end as usize];
            let value = if token.kind == TokenKind::TripleStringLit {
                Cow::Owned(crate::patterns::string_literal_value(text))
            } else {
                Cow::Borrowed(text)
            };
            (token.kind, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use gossamer_lex::SourceMap;

    use super::*;

    fn fmt(source: &str) -> String {
        let mut map = SourceMap::new();
        let file = map.add_file("test.gos", source);
        format_source(source, file).expect("format")
    }

    fn fmt_err(source: &str) -> FormatError {
        let mut map = SourceMap::new();
        let file = map.add_file("test.gos", source);
        format_source(source, file).expect_err("expected failure")
    }

    #[test]
    fn normalizes_spacing_in_misformatted_signature() {
        assert_eq!(fmt("fn    main(  )   {   }\n"), "fn main() { }\n");
    }

    #[test]
    fn idempotent_on_canonical_snippet() {
        let source = "fn double(x: i64) -> i64 { x * 2 }\n";
        let once = fmt(source);
        assert_eq!(once, source);
        assert_eq!(fmt(&once), once);
    }

    #[test]
    fn generic_parameter_type_does_not_indent_the_next_parameter() {
        let source = "fn many_params(\n    one: Vec<i64>\n    two: i64\n    three: Vec<Vec<String>>\n    four: i64\n) {\n    one[0] + two + four\n}\n";
        let once = fmt(source);
        assert_eq!(once, source);
        assert_eq!(fmt(&once), once);
    }

    #[test]
    fn compact_shift_operator_still_continues_the_expression() {
        let source = "fn shifted(value: i64) -> i64 {\n    value>>\n    1\n}\n";
        let expected = "fn shifted(value: i64) -> i64 {\n    value>>\n        1\n}\n";
        assert_eq!(fmt(source), expected);
    }

    #[test]
    fn macro_calls_never_rewritten() {
        let source = "fn main() {\n    let n = 3\n    println(\"value: {} / {n}\", n)\n}\n";
        let out = fmt(source);
        assert_eq!(out, source);
        assert!(!out.contains("__concat"));
    }

    #[test]
    fn trailing_comment_stays_trailing_with_authored_gap() {
        let source = "fn main() {\n    let x = 1   // aligned note\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn own_line_comment_keeps_position_and_indent() {
        let source = "fn main() {\n    // explain the next call\n    work()\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn comment_inside_pipe_chain_aligns_with_chain() {
        let source =
            "fn main() {\n    let x = input\n        // drop empties\n        |> filter\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn block_comment_preserved_verbatim() {
        let source = "/*\n * Multi-line header.\n * Stays byte-identical.\n */\nfn main() { }\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn pipe_chain_continuation_indent() {
        let source = "fn main() {\n    let n = input |> double\n    let m = input\n        |> double\n        |> triple\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn use_list_braces_stay_tight() {
        let source = "use std::{iter, os, strings}\n\nfn main() { }\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn multiline_top_use_list_does_not_gain_blank_after_opener() {
        // The opener stays tight and the continuation keeps its indent.
        // The comma before the newline is optional - the newline is the
        // separator - so it goes, like every other multiline list's.
        let source = "use std::{iter, os,\n    strings}\n\nfn main() { }\n";
        let expected = "use std::{iter, os\n    strings}\n\nfn main() { }\n";
        assert_eq!(fmt(source), expected);
        assert_eq!(fmt(expected), expected);
    }

    #[test]
    fn removes_commas_from_multiline_sequence_and_set_literals() {
        let source = "\
let defs = [
    Some(1),
    Some(2),
]

let v = #[
    1,
    2,
]

let s = #{
    1,
    2,
}
";
        let expected = "\
let defs = [
    Some(1)
    Some(2)
]

let v = #[
    1
    2
]

let s = #{
    1
    2
}
";
        assert_eq!(fmt(source), expected);
        assert_eq!(fmt(expected), expected);
    }

    #[test]
    fn keeps_commas_that_separate_on_one_line() {
        // Only the comma a newline already replaces is optional.
        let source = "let v = #[\n    1, 2\n    3, 4\n]\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn closure_pipes_format_tight() {
        let source = "fn main() {\n    let f = xs |> |v| filter(|n: i64| n % 2 == 0, v)\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn unary_operators_hug_their_operand() {
        assert_eq!(
            fmt("fn main() {\n    let x = - 1\n    let y = & x\n    let z = ! true\n}\n"),
            "fn main() {\n    let x = -1\n    let y = &x\n    let z = !true\n}\n"
        );
    }

    #[test]
    fn match_arms_drop_optional_trailing_commas() {
        let source = "fn main() {\n    let p = Point { x: 1.0, y: 2.0 }\n    match shape {\n        Shape::Circle(r) => r * r,\n        Shape::Rect { w, h } => w * h,\n    }\n}\n";
        let expected = "fn main() {\n    let p = Point { x: 1.0, y: 2.0 }\n    match shape {\n        Shape::Circle(r) => r * r\n        Shape::Rect { w, h } => w * h\n    }\n}\n";
        assert_eq!(fmt(source), expected);
    }

    #[test]
    fn match_arms_drop_commas_before_trailing_comments() {
        let source =
            "match a {\n    1 => a + 1, // line comment\n    2 => a + 2, /* block comment */\n}\n";
        let expected =
            "match a {\n    1 => a + 1 // line comment\n    2 => a + 2 /* block comment */\n}\n";
        assert_eq!(fmt(source), expected);
    }

    #[test]
    fn attributes_stay_tight() {
        let source = "#[derive(Clone, PartialEq)]\nstruct Point {\n    x: f64,\n}\n";
        assert_eq!(
            fmt(source),
            "#[derive(Clone, PartialEq)]\nstruct Point {\n    x: f64\n}\n"
        );
    }

    #[test]
    fn generics_and_turbofish_preserved() {
        let source = "fn parse(text: String) -> Result<Config, errors::Error> {\n    from_json::<Config>(text)\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn hashmap_literal_spacing_is_preserved_as_map_syntax() {
        let source =
            "fn main() { let map: HashMap<String, i64> = HashMap::from({\"one\":1,\"two\":2}) }\n";
        let expected = "fn main() { let map: HashMap<String, i64> = HashMap::from({ \"one\": 1, \"two\": 2 }) }\n";
        assert_eq!(fmt(source), expected);
    }

    #[test]
    fn refuses_unparseable_input() {
        assert!(matches!(fmt_err("fn main( {\n"), FormatError::Parse(_)));
    }

    #[test]
    fn excess_blank_lines_collapse_to_two() {
        assert_eq!(
            fmt("fn a() { }\n\n\n\n\n\nfn b() { }\n"),
            "fn a() { }\n\n\nfn b() { }\n"
        );
    }

    #[test]
    fn pair_guard_keeps_adjacent_pipes_apart() {
        // `(| |)`-style inputs cannot parse, so exercise the guard at
        // the unit level instead.
        let mut map = SourceMap::new();
        let file = map.add_file("g.gos", "");
        let prev = Emitted {
            kind: TokenKind::Punct(Punct::Pipe),
            text: "|",
            class: Class::OpenPipe,
            brace: None,
        };
        let cur = Tok {
            kind: TokenKind::Punct(Punct::Pipe),
            text: "|",
            gap: " ",
        };
        assert!(pair_merges(file, &prev, &cur));
    }

    #[test]
    fn newline_separated_statements_are_kept() {
        let source = "fn main() {\n    let x = 1\n    let y = 2\n}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn removes_commas_from_multiline_struct_fields() {
        let source = "struct Point {\n    x: i64,\n    y: i64,\n}\n";
        assert_eq!(fmt(source), "struct Point {\n    x: i64\n    y: i64\n}\n");
    }

    #[test]
    fn removes_commas_from_all_multiline_delimited_lists() {
        let source = "\
struct Point {
    x: i64,
    y: i64,
}

let p = Point {
    y: 12,
    x: 16,
}

struct Coord(
    i64,
    i64,
)

let c = Coord(
    12,
    16,
)
";
        let expected = "\
struct Point {
    x: i64
    y: i64
}

let p = Point {
    y: 12
    x: 16
}

struct Coord(
    i64
    i64
)

let c = Coord(
    12
    16
)
";
        assert_eq!(fmt(source), expected);
    }

    #[test]
    fn removes_optional_comma_after_match_break_arm() {
        let source = "match pos {\n    7 => break,\n    _ => pos += 3\n}\n";
        let expected = "match pos {\n    7 => break\n    _ => pos += 3\n}\n";
        assert_eq!(fmt(source), expected);
    }
}
