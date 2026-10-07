//! Parse diagnostics emitted while producing an AST.

#![forbid(unsafe_code)]

use std::fmt;

use gossamer_lex::Span;
use thiserror::Error;

/// Why typed serde declined to synthesize a codec for a turbofish target.
///
/// Typed serde covers a concrete struct whose fields it can classify. Each
/// variant here is a shape outside that, named so the report says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerdeTargetRefusal {
    /// A generic struct or enum. Codecs are synthesized per declared type,
    /// not per instantiation, so there is no single shape to encode.
    Generic,
    /// Not a struct or enum declared in this unit - a scalar, a stdlib type,
    /// or a name that does not resolve to a declared type at all.
    NotAStruct,
    /// A struct the synthesizer declined for a reason the caller could not
    /// attribute to a single field.
    Unsupported,
}

impl SerdeTargetRefusal {
    /// The clause completing "`T` cannot derive `op`: ...".
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Generic => {
                "typed serde covers concrete structs and enums, and this one is generic"
            }
            Self::NotAStruct => "typed serde covers structs and enums, and this does not name one",
            Self::Unsupported => "typed serde has no encoding for this type",
        }
    }

    /// The way forward for this refusal.
    #[must_use]
    pub fn help(self, op: &str) -> String {
        let dynamic = "`json::parse` with `get` / `at` / `as_i64` / `as_str` reads a document \
                       without a declared type";
        match self {
            Self::Generic => format!(
                "wrap the instantiation in a concrete struct (`struct Ids {{ v: Vec<i64> }}`), \
                 or hand-write `{op}`"
            ),
            Self::NotAStruct | Self::Unsupported => {
                format!(
                    "name a struct or enum declared in this program, or hand-write `{op}`; \
                     {dynamic}"
                )
            }
        }
    }
}

impl fmt::Display for SerdeTargetRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.describe())
    }
}

/// Every class of error the parser may emit.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// An unexpected token appeared where the grammar requires something else.
    /// `{found}` already includes its own backticks for keyword/punct
    /// tokens (`token_text` formats them), so the outer format string
    /// does not double-wrap.
    #[error("unexpected {found}, expected {expected}")]
    Unexpected {
        /// Human-readable description of what was expected. Reads as the
        /// tail of "expected ...", so it stays a noun phrase; guidance
        /// belongs in `help`.
        expected: String,
        /// Source text of the token that was actually seen.
        found: String,
        /// Optional guidance rendered as the diagnostic's help line.
        help: Option<String>,
    },
    /// End of file encountered while parsing a construct.
    #[error("unexpected end of input while parsing {construct}")]
    UnexpectedEof {
        /// Name of the construct being parsed.
        construct: String,
    },
    /// A construct required to be terminated was not.
    #[error("unterminated {construct} - expected `{delimiter}`")]
    Unterminated {
        /// Name of the construct (e.g. `block`, `tuple`).
        construct: String,
        /// Expected closing delimiter.
        delimiter: String,
    },
    /// A comparison operator was chained without parentheses, e.g. `a == b == c`.
    #[error("comparison operator `{op}` is non-associative - parenthesise the operands")]
    NonAssociativeCompare {
        /// Operator spelling.
        op: String,
    },
    /// A range operator was chained without parentheses, e.g. `1..2..3`.
    #[error("range operator `{op}` is non-associative - parenthesise the operands")]
    NonAssociativeRange {
        /// Operator spelling.
        op: String,
    },
    /// An inclusive range operator appeared without its required upper bound.
    #[error("inclusive range operator `..=` requires an upper bound")]
    InclusiveRangeMissingEnd,
    /// `#[derive(Default)]` on a struct with a field whose type has no zero
    /// value to fill it with.
    #[error("`#[derive(Default)]` on `{ty}` cannot fill field `{field}`")]
    DeriveDefaultField {
        /// The deriving struct.
        ty: String,
        /// The field without a zero value.
        field: String,
        /// The field's type as written.
        field_ty: String,
    },
    /// A feature the language declines (SPEC §17.5), written where it would
    /// start.
    #[error("{feature} are not part of Gossamer")]
    DeclinedFeature {
        /// The feature, as a plural noun phrase (`` `async` functions ``).
        feature: String,
        /// What a program writes instead.
        instead: String,
        /// Source text that replaces the construct's span, when the rewrite
        /// is mechanical.
        replacement: Option<String>,
    },
    /// A struct literal written unparenthesised in an `if` / `while` /
    /// `match` / `for` head, where `{` opens the body.
    #[error("struct literal `{name} {{ .. }}` must be parenthesized here")]
    StructLiteralInHead {
        /// The struct's path as written.
        name: String,
    },
    /// A range pattern bound was written as a path other than a primitive limit.
    #[error("range pattern bound `{text}` is not a literal")]
    RangePatternBoundNotLiteral {
        /// The path as written.
        text: String,
    },
    /// A match arm pattern was not followed by its arrow.
    #[error("expected `=>` after match arm pattern, found {found}")]
    MatchArmMissingArrow {
        /// Token encountered where `=>` was required.
        found: String,
    },
    /// A match arm arrow was not followed by a result expression.
    #[error("expected an expression after `=>` in match arm")]
    MatchArmMissingBody,
    /// Adjacent expression-bodied match arms lacked a clear boundary.
    #[error("match arms on the same line must be separated by a comma")]
    MatchArmMissingSeparator,
    /// A braced struct literal appeared directly in the scrutinee of an
    /// `if`, `while`, or `match`, where it is ambiguous with the block start.
    #[error("struct literal must be parenthesised in `if`/`while`/`match` scrutinee")]
    StructLiteralNeedsParens,
    /// The right-hand side of `|>` did not match any of the forms in SPEC §4.6.
    #[error("E0601: right-hand side of `|>` must be a callable")]
    PipeRhsInvalid,
    /// `$` was written where an expression belongs. The pipe placeholder it
    /// used to spell is retired in favour of a closure step.
    #[error("`$` is not part of the language")]
    PipePlaceholderRetired,
    /// A validating macro was written; it lives in its module now.
    #[error("`{name}!` moved into its module")]
    ValidatingMacroMoved {
        /// The macro name as written.
        name: String,
    },
    /// A build-time validated call handed something other than a literal.
    #[error("`{call}` takes a literal")]
    ValidatedCallNeedsLiteral {
        /// The call as written, `regex::compile` or `sql::statement`.
        call: &'static str,
    },
    /// A literal handed to `regex::compile` is not a pattern it compiles.
    #[error("invalid regex: {reason}")]
    InvalidRegexLiteral {
        /// The regex engine's reason.
        reason: String,
    },
    /// A literal handed to `sql::statement` is not a well-formed statement.
    #[error("invalid SQL statement: {reason}")]
    InvalidSqlStatement {
        /// What is wrong with the statement.
        reason: String,
    },
    /// An `enum Name : R` named something that is not an unsigned width.
    #[error("an enum representation is an unsigned width, not `{written}`")]
    EnumReprWidth {
        /// The representation as written.
        written: String,
    },
    /// A compiler-known call was written with a `!`.
    #[error("`{name}` is an ordinary call; drop the `!`")]
    MacroSigilRetired {
        /// The name as written, without the bang.
        name: String,
    },
    /// An opaque alias was written `type X = new T`.
    #[error("an opaque alias is written `newtype X = T`")]
    OpaqueAliasSpelling,
    /// A `Display` implementation declared the rendering as `to_string`.
    #[error("the `Display` contract is `fn fmt`")]
    DisplayContractMethod,
    /// A parameter was declared with a shared reference type.
    #[error("a parameter is `T` or `&mut T`; a shared `&` names no choice")]
    SharedReferenceParameter,
    /// A call argument or a binary operand was written as a shared
    /// reference.
    #[error("a shared `&` names no choice here; drop it")]
    SharedReferenceArgument,
    /// A shared `&` on an argument whose removal would let it bind as a
    /// call on the argument before it. Reported without a fix.
    #[error(
        "a shared `&` names no choice here; drop it, and separate this argument \
         from the one before it"
    )]
    SharedReferenceArgumentJoins,
    /// `unsafe fn`, `unsafe impl`, or `unsafe trait` was written. Only an
    /// `unsafe { }` block grants anything: a call to a foreign function.
    #[error("`unsafe` grants nothing; drop it")]
    UnsafeGrantsNothing,
    /// A cohort header used the retired `context:` isolation spelling.
    #[error("a cohort's isolation setting is `isolation:`, not `context:`")]
    CohortIsolationSpelling {
        /// The whole setting as it should be written.
        replacement: String,
    },
    /// An argument label was bound with `=` instead of `:`.
    #[error("an argument label binds with `:`, not `=`")]
    ArgumentLabelSeparator {
        /// The label as written.
        name: String,
    },
    /// A callable type was written with a spelling other than `Fn`.
    #[error("`{written}` is not a callable type; the language has one, `Fn`")]
    CallableTypeSpelling {
        /// The spelling as written.
        written: String,
    },
    /// A `mod` declaration ended in `;`. A statement ends at its newline.
    #[error("a `mod` declaration ends at its newline, not at a `;`")]
    ModDeclSemicolon,
    /// A `let` binding or an assignment wrote its targets as a parenthesised
    /// tuple. The comma-separated list is the one spelling.
    #[error("a binding lists its targets without parentheses")]
    LetPatternParens {
        /// The paren-free spelling of the same targets.
        replacement: String,
    },
    /// A `|>` step takes arguments, so it cannot also take the piped value.
    #[error("a `|>` step that takes arguments must be a closure")]
    PipeStepNeedsClosure {
        /// The same step written as the closure it stands for.
        replacement: Option<String>,
    },
    /// A binary operator other than `|>` began a line, where it cannot
    /// continue the expression above.
    #[error("a line cannot start with `{op}`")]
    LeadingBinaryOperator {
        /// The operator as written.
        op: String,
        /// The operator moved to the end of the line above, replacing
        /// `fix_span`.
        replacement: String,
        /// From the end of the line above's last token through the operator
        /// and the space after it.
        fix_span: Span,
    },
    /// A closure is a statement of its own, so nothing reads it: a line
    /// starting with `||` or `|` begins a closure rather than continuing the
    /// expression above.
    #[error("this closure is a statement of its own, so its value is unused")]
    DiscardedClosure {
        /// When the closure takes no parameters and follows an expression,
        /// its `||` moved to the end of the line above, replacing the span:
        /// the logical or that line most likely meant.
        continuation: Option<(String, Span)>,
    },
    /// An assignment appeared in a non-statement expression position.
    #[error("assignment is only valid at statement position")]
    AssignmentNotAllowed,
    /// An integer literal is required by the grammar at this position.
    #[error("expected an integer literal")]
    ExpectedInt,
    /// A string literal is required by the grammar at this position.
    #[error("expected a string literal")]
    ExpectedString,
    /// A multi-line triple-quoted literal carried text on the line that
    /// opens it, where only the delimiter and whitespace belong.
    #[error("text follows the opening `\"\"\"` of a multi-line string")]
    TripleStringOpeningLine,
    /// A trailing integer produced an invalid tuple index (`foo.0xff`, etc.).
    #[error("invalid tuple index")]
    InvalidTupleIndex,
    /// A label token is malformed (missing identifier after `'`).
    #[error("expected a label identifier after `'`")]
    MalformedLabel,
    /// An unsupported or malformed attribute.
    #[error("malformed attribute")]
    MalformedAttribute,
    /// A use declaration target could not be parsed.
    #[error("malformed `use` declaration")]
    MalformedUse,
    /// A `use` path was written with the hyphens a package name may carry.
    /// `-` is subtraction, never part of an identifier, so the path would
    /// stop at the first one and the rest would read as an expression.
    #[error("`-` is not part of an identifier, so `{written}` is not a module path")]
    HyphenInUsePath {
        /// The path as written, hyphens included.
        written: String,
        /// The module name the package is reached by: the same path with
        /// each `-` replaced by `_`.
        module: String,
    },
    /// Two consecutive tokens formed something the parser does not recognise.
    #[error("unexpected construct")]
    UnexpectedConstruct,
    /// An `extern` form other than `unsafe extern "C" { fn ...; }`: an
    /// extern block without `unsafe`, or an ABI other than `"C"` /
    /// `"system"`.
    #[error("foreign functions are declared in an `unsafe extern \"C\" {{ ... }}` block")]
    ExternReserved,
    /// `extern "C" fn name(..) { .. }`, Rust's spelling of a function C
    /// calls, which Gossamer writes `#[export] fn`.
    #[error("a function C calls is written `#[export] fn`")]
    ExternFnDefinition,
    /// A declaration inside an `unsafe extern "C"` block that is not a
    /// bodyless, non-generic `fn`, a `type Name`, or a `static NAME: T`.
    #[error(
        "an `unsafe extern \"C\"` block holds only `fn` signatures without bodies, `type` names, and `static` declarations"
    )]
    ExternItemShape,
    /// An expression, type, or pattern nested past the parser's hard
    /// recursion limit. Emitted to keep adversarial inputs from
    /// blowing the C stack while still letting the parser recover.
    #[error("expression nests beyond {limit} levels (consider rewriting with a helper)")]
    RecursionLimit {
        /// Configured recursion limit at which this error was raised.
        limit: u32,
    },
    /// A tokenization error (unterminated comment/string, bad escape,
    /// ...) surfaced through the parse diagnostics so it reaches the
    /// driver instead of being dropped with the lexer.
    #[error("{message}")]
    Lex {
        /// Rendered lexer diagnostic.
        message: String,
    },
    /// A bare statement appeared inside a `mod { }` body, where only items
    /// are allowed. Top-level statements belong only to the entry file's
    /// implicit `fn main`.
    #[error("statements are only allowed at the top level of the entry file")]
    StatementOutsideEntry,
    /// The entry file mixed bare top-level statements with an explicit
    /// `fn main`; an entry file uses exactly one entry form.
    #[error("cannot mix top-level statements with an explicit `fn main`")]
    MixedEntryForms,
    /// A format-macro placeholder `{...}` whose contents are neither a
    /// binding name nor a format spec - typically an expression like
    /// `{age + 1}`, which the macros do not interpolate.
    #[error("malformed format placeholder `{{{text}}}`")]
    MalformedFormatPlaceholder {
        /// The placeholder's inner text (without the braces).
        text: String,
    },
    /// A format placeholder naming its argument by position (`{0}`), which
    /// templates do not take: arguments fill `{}` in order, and a value used
    /// twice is bound and named.
    #[error("format placeholder `{{{text}}}` names an argument by position")]
    FormatArgumentIndex {
        /// The placeholder's inner text (without the braces).
        text: String,
    },
    /// A Rust-style formatting macro received a positional argument count
    /// different from the number of positional placeholders in its template.
    #[error("format string requires {expected} positional argument(s), but {found} were supplied")]
    FormatArgumentCount {
        /// Number of positional placeholders in the template.
        expected: usize,
        /// Number of explicit positional arguments after the template.
        found: usize,
    },
    /// A Rust-style formatting macro requires a literal template so its
    /// placeholders can be checked during parsing.
    #[error("format argument must be a string literal")]
    FormatStringMustBeLiteral,
    /// `vec![..]`, Rust's Vec macro, where the Vec literal is `#[..]`.
    #[error("`vec!` is not part of Gossamer; a Vec literal is `#[...]`")]
    VecMacroRetired,
    /// An `f"..."` string holds a `{}` placeholder, which names nothing: an
    /// interpolated string has no arguments to fill it in order.
    #[error("interpolated string placeholder `{{{text}}}` names no value")]
    InterpolatedPositional {
        /// The placeholder's inner text (without the braces).
        text: String,
    },
    /// An `f"..."` placeholder's spec is not one the format grammar takes.
    #[error("malformed format spec `:{spec}`")]
    MalformedInterpolationSpec {
        /// The spec's text after the `:`.
        spec: String,
    },
    /// An `f"..."` string holds a brace that opens or closes no placeholder.
    #[error("unmatched `{brace}` in an interpolated string")]
    UnmatchedInterpolationBrace {
        /// The lone brace.
        brace: char,
    },
    /// A `const` or `static` declaration whose name was followed directly by
    /// `=`. These items carry no inference, so the type annotation is part of
    /// the grammar rather than an option.
    #[error("{kind} `{name}` needs a type annotation")]
    MissingItemType {
        /// Item keyword, `constant` or `static`.
        kind: &'static str,
        /// The item's declared name.
        name: String,
        /// Type spelling inferred from the initialiser, when it is a literal
        /// whose type is known without inference.
        inferred: Option<&'static str>,
    },
    /// A bracket literal spelling for a container that is now constructed
    /// through its type: `<[..]`, `[..]>`, `^[..]`, `_[..]`.
    #[error("`{spelling}` literals are not valid syntax - construct a `{container}` instead")]
    RemovedCollectionLiteral {
        /// The literal spelling that was written.
        spelling: String,
        /// The container the spelling used to build.
        container: String,
    },
    /// A struct used in a `to_json` / `from_json` (or toml/yaml) call has a
    /// field whose type the serde synthesizer cannot handle. Without this the
    /// whole struct's serde was silently dropped and the call surfaced only as
    /// an opaque unknown-name error.
    #[error(
        "`{ty}` cannot derive `{op}`: field `{field}` has type `{field_ty}`, which is not serializable"
    )]
    SerdeUnserializableField {
        /// The struct being serialized.
        ty: String,
        /// The offending field's name.
        field: String,
        /// The offending field's type spelling.
        field_ty: String,
        /// The serde operation requested (`to_json`, `from_json`, ...).
        op: String,
    },
    /// A `to_json` / `from_json` (or toml/yaml) call named a type typed serde
    /// does not synthesize for at all, as opposed to a struct with one
    /// unserializable field. The synthesized function is absent either way, so
    /// without this the call surfaces only as an unresolved internal name.
    #[error("`{ty}` cannot derive `{op}`: {reason}")]
    SerdeUnsupportedTarget {
        /// The type named in the turbofish, as written.
        ty: String,
        /// The serde operation requested (`to_json`, `from_json`, ...).
        op: String,
        /// Why the synthesizer declined this target.
        reason: SerdeTargetRefusal,
    },
    /// A slice pattern wrote a second `..`. One rest binding splits the
    /// elements into a prefix and a suffix; a second one has no meaning.
    #[error("a slice pattern may contain at most one `..`")]
    SlicePatternExtraRest,
    /// A struct literal wrote a second `..base` functional update.
    #[error("a struct literal may contain at most one `..base` spread")]
    StructLiteralExtraSpread,
    /// A `let` whose pattern can fail to match was written without the
    /// `else` block that gives the failure a diverging path.
    #[error("a refutable `let` pattern requires an `else` block")]
    RefutableLetNeedsElse,
    /// A `pub(..)` restriction other than `pub(package)`.
    #[error("`pub({written})` is not a visibility Gossamer has")]
    UnsupportedVisibilityRestriction {
        /// Restriction as written, without the parentheses.
        written: String,
    },
}

/// A diagnostic with its source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostic {
    /// Classification of the error.
    pub error: ParseError,
    /// Source range the diagnostic refers to.
    pub span: Span,
}

impl ParseDiagnostic {
    /// Builds a diagnostic from an error and span.
    #[must_use]
    pub const fn new(error: ParseError, span: Span) -> Self {
        Self { error, span }
    }
}

impl fmt::Display for ParseDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "{}..{}: {}",
            self.span.start, self.span.end, self.error
        )
    }
}

impl std::error::Error for ParseDiagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl ParseDiagnostic {
    /// Renders this parse diagnostic as a structured
    /// [`gossamer_diagnostics::Diagnostic`].
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location, Suggestion};
        let location = Location::new(self.span.file, self.span);
        let (code, title, help) = self.error.code_title_help();
        let mut out = Diagnostic::error(Code(code), title.clone()).with_primary(location, title);
        if let Some(help) = help {
            out = out.with_help(help);
        }
        if let Some((label, replacement)) = self.error.rewrite() {
            let target = match &self.error {
                ParseError::LeadingBinaryOperator { fix_span, .. }
                | ParseError::DiscardedClosure {
                    continuation: Some((_, fix_span)),
                } => Location::new(fix_span.file, *fix_span),
                _ => location,
            };
            out = out.with_suggestion(Suggestion::replacement(target, label, replacement));
        }
        out
    }
}

impl ParseError {
    /// The label and replacement text of the mechanical rewrite that
    /// resolves this error over its span, when it has one.
    fn rewrite(&self) -> Option<(String, String)> {
        let write = |text: &str| (format!("write `{text}`"), text.to_string());
        let drop = |what: &str| (format!("drop {what}"), String::new());
        match self {
            // The span of a missing-type diagnostic is the item's name, so
            // the annotated name is a drop-in replacement.
            ParseError::MissingItemType {
                name,
                inferred: Some(ty),
                ..
            } => Some((
                format!("annotate the type: `{name}: {ty}`"),
                format!("{name}: {ty}"),
            )),
            // The span is the `!`, so `::compile` in its place spells the call.
            ParseError::ValidatingMacroMoved { name } => Some(if name == "regex" {
                (
                    "write `regex::compile`".to_string(),
                    "::compile".to_string(),
                )
            } else {
                (
                    "write `sql::statement`".to_string(),
                    "::statement".to_string(),
                )
            }),
            ParseError::VecMacroRetired => Some(("write `#[...]`".to_string(), "#".to_string())),
            ParseError::ExternFnDefinition => Some(write("#[export]")),
            ParseError::MacroSigilRetired { .. } => Some(drop("the `!`")),
            ParseError::SharedReferenceArgument | ParseError::SharedReferenceParameter => {
                Some(drop("the `&`"))
            }
            ParseError::UnsafeGrantsNothing => Some(drop("`unsafe`")),
            ParseError::ModDeclSemicolon => Some(drop("the `;`")),
            ParseError::DisplayContractMethod => Some(write("fmt")),
            ParseError::CallableTypeSpelling { .. } => Some(write("Fn")),
            ParseError::OpaqueAliasSpelling => Some((
                "drop `new` and write `newtype` in place of `type`".to_string(),
                String::new(),
            )),
            ParseError::ArgumentLabelSeparator { name } => {
                Some((format!("write `{name}:`"), ":".to_string()))
            }
            ParseError::LeadingBinaryOperator {
                op, replacement, ..
            } => Some((
                format!("move `{op}` to the end of the line above"),
                replacement.clone(),
            )),
            ParseError::DiscardedClosure {
                continuation: Some((replacement, _)),
            } => Some((
                "move `||` to the end of the line above".to_string(),
                replacement.clone(),
            )),
            // A pipe step's rewrite covers the step alone, so every step of a
            // chain converges in a single `--fix` pass.
            ParseError::CohortIsolationSpelling { replacement }
            | ParseError::LetPatternParens { replacement }
            | ParseError::DeclinedFeature {
                replacement: Some(replacement),
                ..
            }
            | ParseError::PipeStepNeedsClosure {
                replacement: Some(replacement),
            } => Some(write(replacement)),
            _ => None,
        }
    }
}

impl ParseError {
    /// Builds an unexpected-token error carrying no extra guidance.
    pub(crate) fn unexpected(expected: impl Into<String>, found: String) -> Self {
        ParseError::Unexpected {
            expected: expected.into(),
            found,
            help: None,
        }
    }

    /// Builds an unexpected-token error whose `help` line carries the
    /// guidance, keeping `expected` a bare noun phrase.
    pub(crate) fn unexpected_help(
        expected: impl Into<String>,
        found: String,
        help: impl Into<String>,
    ) -> Self {
        ParseError::Unexpected {
            expected: expected.into(),
            found,
            help: Some(help.into()),
        }
    }

    /// The GP0016 report for a foreign declaration written in a form other
    /// than a body-less `fn` in an `unsafe extern "C"` block.
    fn foreign_report(&self) -> (&'static str, String, Option<String>) {
        match self {
            Self::ExternReserved => (
                "GP0016",
                "foreign functions are declared in an `unsafe extern \"C\" { ... }` block"
                    .to_string(),
                Some(
                    "write `unsafe extern \"C\" { fn name(params) -> ret }` at module level \
                     and call the function inside `unsafe { }`; a Gossamer function is given to C \
                     with `#[export]` on a plain `fn`, and only the `\"C\"` and `\"system\"` ABIs \
                     are recognised"
                        .to_string(),
                ),
            ),
            Self::ExternFnDefinition => (
                "GP0016",
                "a function C calls is written `#[export] fn`".to_string(),
                Some(
                    "`#[export]` defines the function under its own name for C, and \
                     `#[export(\"symbol\")]` under another; `gos check --fix` rewrites it"
                        .to_string(),
                ),
            ),
            Self::ExternItemShape => (
                "GP0016",
                "an `unsafe extern \"C\"` block holds only `fn` signatures without bodies, \
                 `type` names, and `static` declarations"
                    .to_string(),
                Some(
                    "declare each foreign function as `fn name(params) -> ret`, with no body \
                     and no generic parameters; its body is in the native library. A C global \
                     is `static NAME: T` with no initializer, reached through `ffi::addr_of`"
                        .to_string(),
                ),
            ),
            _ => unreachable!("only the foreign-declaration errors reach here"),
        }
    }

    /// Diagnostic code, title, and optional help text for this error.
    fn code_title_help(&self) -> (&'static str, String, Option<String>) {
        match self {
            ParseError::Unexpected {
                expected,
                found,
                help,
            } => (
                "GP0001",
                format!("unexpected {found}, expected {expected}"),
                help.clone(),
            ),
            ParseError::UnexpectedEof { construct } => (
                "GP0002",
                format!("unexpected end of input while parsing {construct}"),
                Some(format!("finish the {construct} or remove it")),
            ),
            ParseError::Unterminated {
                construct,
                delimiter,
            } => (
                "GP0003",
                format!("unterminated {construct} - expected `{delimiter}`"),
                Some(format!("add `{delimiter}` to close the {construct}")),
            ),
            ParseError::NonAssociativeCompare { op } => (
                "GP0004",
                format!("comparison operator `{op}` is non-associative"),
                Some("parenthesise the operands".to_string()),
            ),
            ParseError::NonAssociativeRange { op } => (
                "GP0005",
                format!("range operator `{op}` is non-associative"),
                Some("parenthesise the operands".to_string()),
            ),
            ParseError::StructLiteralNeedsParens => (
                "GP0006",
                "struct literal must be parenthesised in an `if`/`while`/`match` scrutinee"
                    .to_string(),
                Some("wrap the struct literal in `(...)`".to_string()),
            ),
            ParseError::PipeRhsInvalid => (
                "GP0007",
                "right-hand side of `|>` must be a callable".to_string(),
                Some(
                    "pipe into a function name, method, closure, or call expression such as \
                     `value |> parse` or `value |> clamp(0, 10, $)`"
                        .to_string(),
                ),
            ),
            ParseError::AssignmentNotAllowed => (
                "GP0008",
                "assignment is only valid at statement position".to_string(),
                Some(
                    "move the assignment into its own statement before using the assigned value"
                        .to_string(),
                ),
            ),
            ParseError::ExpectedInt => ("GP0009", "expected an integer literal".to_string(), None),
            ParseError::ExpectedString => ("GP0010", "expected a string literal".to_string(), None),
            ParseError::TripleStringOpeningLine => (
                "GP0033",
                "text follows the opening `\"\"\"` of a multi-line string".to_string(),
                Some(
                    "start the body on the next line; the indentation the body shares with \
                     the closing `\"\"\"` is stripped from every line"
                        .to_string(),
                ),
            ),
            ParseError::InvalidTupleIndex => (
                "GP0011",
                "invalid tuple index".to_string(),
                Some("tuple indices must be plain decimal integers".to_string()),
            ),
            ParseError::ExternReserved
            | ParseError::ExternFnDefinition
            | ParseError::ExternItemShape => self.foreign_report(),
            ParseError::RecursionLimit { limit } => (
                "GP0017",
                format!("expression nests beyond {limit} levels"),
                Some("split the expression into smaller helpers".to_string()),
            ),
            ParseError::Lex { message } => ("GP0018", message.clone(), None),
            other => other.code_title_help_malformed(),
        }
    }

    fn code_title_help_malformed(&self) -> (&'static str, String, Option<String>) {
        match self {
            ParseError::MalformedLabel => (
                "GP0012",
                "expected a label identifier after `'`".to_string(),
                Some("write a label such as `'outer` before the loop".to_string()),
            ),
            ParseError::MalformedAttribute => (
                "GP0013",
                "malformed attribute".to_string(),
                Some(
                    "write an attribute as `#[name]` or `#[name(value)]` immediately before its item"
                        .to_string(),
                ),
            ),
            ParseError::HyphenInUsePath { written, module } => (
                "GP0040",
                format!("`-` is not part of an identifier, so `{written}` is not a module path"),
                Some(format!(
                    "a package's module name replaces each `-` with `_`: write `use {module}`"
                )),
            ),
            ParseError::MalformedUse => (
                "GP0014",
                "malformed `use` declaration".to_string(),
                Some(
                    "write `use std::module`, `use path::item`, or a grouped import such as \
                     `use std::{env, fs}`"
                        .to_string(),
                ),
            ),
            ParseError::UnexpectedConstruct => (
                "GP0015",
                "these adjacent tokens do not form a valid expression or declaration".to_string(),
                Some(
                    "check for a missing operator, comma, delimiter, or statement separator"
                        .to_string(),
                ),
            ),
            other => other.code_title_help_syntax(),
        }
    }

    /// Code/title/help for range, match-arm, and pipe syntax errors.
    #[allow(clippy::too_many_lines, reason = "one arm per diagnostic code")]
    fn code_title_help_syntax(&self) -> (&'static str, String, Option<String>) {
        match self {
            ParseError::DeriveDefaultField {
                ty,
                field,
                field_ty,
            } => (
                "GP0062",
                format!("`#[derive(Default)]` on `{ty}` cannot fill field `{field}`"),
                Some(format!(
                    "`{field_ty}` has no `default()`: derive it with `#[derive(Default)]`, write \
                     `impl Default for {field_ty}`, or implement `default` for `{ty}` by hand"
                )),
            ),
            ParseError::VecMacroRetired => (
                "GP0064",
                "`vec!` is not part of Gossamer; a Vec literal is `#[...]`".to_string(),
                Some(
                    "`#[1, 2]` builds a `Vec`, `[1, 2]` a fixed array, and `#[0; n]` a `Vec` \
                     of `n` zeros"
                        .to_string(),
                ),
            ),
            ParseError::DeclinedFeature {
                feature, instead, ..
            } => (
                "GP0061",
                format!("{feature} are not part of Gossamer"),
                Some(instead.clone()),
            ),
            ParseError::StructLiteralInHead { name } => (
                "GP0060",
                format!("struct literal `{name} {{ .. }}` must be parenthesized here"),
                Some(format!(
                    "in an `if`, `while`, `match`, or `for` head a `{{` opens the body; write \
                     `({name} {{ .. }})`"
                )),
            ),
            ParseError::RangePatternBoundNotLiteral { text } => (
                "GP0059",
                format!("range pattern bound `{text}` is not a literal"),
                Some(
                    "a range pattern bound is a literal or a primitive integer limit such as \
                     `i64::MIN`; compare against a named constant with a guard \
                     (`n if n >= LOW => ..`)"
                        .to_string(),
                ),
            ),
            ParseError::InclusiveRangeMissingEnd => (
                "GP0026",
                "inclusive range operator `..=` requires an upper bound".to_string(),
                Some(
                    "provide an upper bound, or use `..` for a range open at the upper end"
                        .to_string(),
                ),
            ),
            ParseError::PipePlaceholderRetired => (
                "GP0027",
                "`$` is not part of the language".to_string(),
                Some(
                    "a `|>` step that needs the value in a particular slot is a closure, \
                     as in `x |> |v| f(a, v)`; a method already chains, as in `x.trim()`, \
                     and a callback is a closure or a function named in value position"
                        .to_string(),
                ),
            ),
            ParseError::ValidatingMacroMoved { name } => {
                let replacement = if name == "regex" {
                    "regex::compile"
                } else {
                    "sql::statement"
                };
                (
                    "GP0051",
                    format!("`{name}!` moved into its module"),
                    Some(format!(
                        "write `{replacement}(\"…\")`; a literal argument is still \
                         validated while the program is compiled, and `{name}` stays a \
                         module name rather than becoming a global"
                    )),
                )
            }
            ParseError::ValidatedCallNeedsLiteral { call } => (
                "GP0052",
                format!("`{call}` takes a literal"),
                Some(if *call == "regex::compile" {
                    "the pattern is checked while the program is compiled, so it answers the \
                     `Pattern` itself; a pattern built at run time goes to `regex::new`, \
                     which answers a `Result`"
                        .to_string()
                } else {
                    "the statement is checked while the program is compiled, so it has to \
                     be there to check; a statement built at run time is an ordinary \
                     `String` and needs no wrapper"
                        .to_string()
                }),
            ),
            ParseError::InvalidRegexLiteral { reason } => (
                "GP0057",
                format!("invalid regex: {reason}"),
                Some(
                    "a literal pattern is compiled while the program is parsed, with the \
                     engine `regex::compile` uses at run time, so a pattern that would \
                     answer `Err` there is reported here"
                        .to_string(),
                ),
            ),
            ParseError::InvalidSqlStatement { reason } => (
                "GP0058",
                format!("invalid SQL statement: {reason}"),
                Some(
                    "a statement is checked while the program is parsed: it must not be \
                     empty and its parentheses must balance"
                        .to_string(),
                ),
            ),
            ParseError::EnumReprWidth { written } => (
                "GP0050",
                format!("an enum representation is an unsigned width, not `{written}`"),
                Some(
                    "write `enum Name : u8 { .. }` - a discriminant is a count, so its \
                     store is unsigned, and the width is between 1 and 64 bits"
                        .to_string(),
                ),
            ),
            ParseError::MacroSigilRetired { name } => (
                "GP0049",
                format!("`{name}` is an ordinary call; drop the `!`"),
                Some(
                    "the set of compiler-known names is closed and recognised at the \
                     `(`, so there is nothing a sigil disambiguates. The first argument \
                     is still the template when it is a string literal"
                        .to_string(),
                ),
            ),
            ParseError::CohortIsolationSpelling { replacement } => (
                "GP0056",
                format!("write `{replacement}`"),
                Some(
                    "`context::Context` is the cancellation type a cohort may one day \
                     inherit; this setting decides whether a child gets an OS thread of \
                     its own, which is what `isolation:` names"
                        .to_string(),
                ),
            ),
            ParseError::SharedReferenceArgumentJoins => (
                "GP0055",
                "a shared `&` names no choice here; drop it, and separate this \
                 argument from the one before it"
                    .to_string(),
                Some(
                    "this argument opens with `(` and a newline is all that separates \
                     it from the one before, so deleting the `&` alone would let the \
                     two bind as one call. Parenthesise the previous argument, or put \
                     a comma between them, and then drop the sigil. `gos check --fix` \
                     leaves this one alone for that reason"
                        .to_string(),
                ),
            ),
            ParseError::SharedReferenceArgument => (
                "GP0055",
                "a shared `&` names no choice here; drop it".to_string(),
                Some(
                    "there is no shared-reference type left for the sigil to name. An \
                     argument and an operand are both read without copying either way, \
                     and a callee writes to the caller's variable only through a \
                     `&mut` parameter, which is spelled `&mut` at the call too"
                        .to_string(),
                ),
            ),
            ParseError::SharedReferenceParameter => (
                "GP0054",
                "a parameter is `T` or `&mut T`; a shared `&` names no choice".to_string(),
                Some(
                    "an argument is passed without copying whatever its type, and a \
                     callee cannot write to the caller's variable unless the parameter \
                     says `&mut`, so `f(m: &Map)` and `f(m: Map)` have the same cost \
                     and the same guarantee. A sequence view is written `[T]`, and \
                     `&mut [T]` is the form that writes through"
                        .to_string(),
                ),
            ),
            ParseError::DisplayContractMethod => (
                "GP0053",
                "the `Display` contract is `fn fmt`".to_string(),
                Some(
                    "`Display` and `Debug` each declare one method that answers a \
                     `String`, so both are written `fn fmt`; which one a value \
                     reaches is decided by the `impl` header, `{}` taking `Display` \
                     and `{:?}` taking `Debug`. `x.to_string()` still renders through \
                     `Display`"
                        .to_string(),
                ),
            ),
            ParseError::OpaqueAliasSpelling => (
                "GP0047",
                "an opaque alias is written `newtype X = T`".to_string(),
                Some(
                    "`new` reads as allocation to a reader arriving from any other \
                     language; `newtype` is the standard name for a distinct type over \
                     one representation"
                        .to_string(),
                ),
            ),
            ParseError::UnsafeGrantsNothing => (
                "GP0046",
                "`unsafe` grants nothing; drop it".to_string(),
                Some(
                    "an `unsafe fn`, `unsafe impl`, or `unsafe trait` withholds nothing; the \
                     one operation `unsafe` permits, calling a foreign function, is written \
                     inside an `unsafe { }` block"
                        .to_string(),
                ),
            ),
            ParseError::ArgumentLabelSeparator { name } => (
                "GP0045",
                "an argument label binds with `:`, not `=`".to_string(),
                Some(format!(
                    "write `{name}: value`; every keyed form in the language - a struct \
                     field, a map entry, a `cohort` header setting - binds with `:`, and \
                     `=` at a call site reads as the assignment it is elsewhere"
                )),
            ),
            ParseError::CallableTypeSpelling { written } => (
                "GP0044",
                format!("`{written}` is not a callable type; the language has one, `Fn`"),
                Some(
                    "write `Fn(args) -> ret`. There is no raw function-pointer shape \
                     and no `FnMut` / `FnOnce` distinction to draw"
                        .to_string(),
                ),
            ),
            ParseError::ModDeclSemicolon => (
                "GP0043",
                "a `mod` declaration ends at its newline, not at a `;`".to_string(),
                Some(
                    "write `mod name`; a trailing semicolon is invalid everywhere else \
                     in the language, and this was the one form that asked for one"
                        .to_string(),
                ),
            ),
            ParseError::LetPatternParens { .. } => (
                "GP0042",
                "a binding lists its targets without parentheses".to_string(),
                Some(
                    "write `let a, b = value` and `a, b = x, y`; parentheses group a \
                     pattern that sits beside others - a `match` arm, a `for` binding, \
                     a parameter, or a nested element of the list"
                        .to_string(),
                ),
            ),
            ParseError::PipeStepNeedsClosure { .. } => (
                "GP0041",
                "a `|>` step that takes arguments must be a closure".to_string(),
                Some(
                    "write the step as a closure whose parameter is the piped value, as in \
                     `x |> |v| f(v, a)`. Every std free function takes its data first, \
                     which is the slot `--fix` writes; check it is the slot this call \
                     needs"
                        .to_string(),
                ),
            ),
            ParseError::MatchArmMissingArrow { found } => (
                "GP0029",
                format!("expected `=>` after match arm pattern, found {found}"),
                Some("write `pattern => expression` for each match arm".to_string()),
            ),
            ParseError::MatchArmMissingBody => (
                "GP0030",
                "expected an expression after `=>` in match arm".to_string(),
                Some("provide the value or block produced by this match arm".to_string()),
            ),
            ParseError::MatchArmMissingSeparator => (
                "GP0031",
                "match arms on the same line must be separated by a comma".to_string(),
                Some("add a comma, or put the next match arm on a new line".to_string()),
            ),
            other => other.code_title_help_entry(),
        }
    }

    /// Code/title/help for the entry-form and serde-derive errors.
    /// Split out of [`Self::code_title_help`] to keep each match small.
    fn code_title_help_entry(&self) -> (&'static str, String, Option<String>) {
        match self {
            ParseError::StatementOutsideEntry => (
                "GP0019",
                "statements are only allowed at the top level of the entry file".to_string(),
                Some(
                    "a module body contains items only; move executable code into a function, \
                     or into the entry file's top level (its implicit `fn main`)"
                        .to_string(),
                ),
            ),
            ParseError::MixedEntryForms => (
                "GP0020",
                "cannot mix top-level statements with an explicit `fn main`".to_string(),
                Some(
                    "the entry file is already implicitly `fn main` when it carries top-level \
                     statements; move the statements into your `fn main`, or remove the explicit \
                     `fn main`"
                        .to_string(),
                ),
            ),
            ParseError::SerdeUnserializableField {
                ty,
                field,
                field_ty,
                op,
            } => (
                "GP0022",
                format!(
                    "`{ty}` cannot derive `{op}`: field `{field}` has type `{field_ty}`, which is not serializable"
                ),
                Some(format!(
                    "give `{field}` a serializable type (scalar, String, Vec, Option, tuple, \
                     Map<String, _>, json::Value, or a nested struct), or hand-write `{op}`"
                )),
            ),
            ParseError::SerdeUnsupportedTarget { ty, op, reason } => (
                "GP0039",
                format!("`{ty}` cannot derive `{op}`: {}", reason.describe()),
                Some(reason.help(op)),
            ),
            other => other.code_title_help_format(),
        }
    }

    /// Code/title/help for the format-template and interpolated-string errors.
    fn code_title_help_format(&self) -> (&'static str, String, Option<String>) {
        match self {
            ParseError::MalformedFormatPlaceholder { text } => (
                "GP0021",
                format!("malformed format placeholder `{{{text}}}`"),
                Some(
                    "a `format` placeholder is `{}` or a binding or field path, with an optional \
                     `:spec` of fill, alignment, sign, zero-pad, width, precision, and radix or \
                     exponent (`{:>8}`, `{:+08.3}`, `{:#x}`, `{:.2e}`) or `?`; pass a computed \
                     value as an argument, or write the string as `f\"...\"`, whose placeholders \
                     hold any expression"
                        .to_string(),
                ),
            ),
            ParseError::FormatArgumentIndex { text } => (
                "GP0021",
                format!("format placeholder `{{{text}}}` names an argument by position"),
                Some(
                    "arguments fill `{}` placeholders in order; to use a value twice or out of \
                     order, bind it and name it, as in `{total}` or `{total:>8}`"
                        .to_string(),
                ),
            ),
            ParseError::FormatArgumentCount { expected, found } => (
                "GP0023",
                format!(
                    "format string requires {expected} positional argument(s), but {found} were supplied"
                ),
                Some(
                    "add or remove `{}` placeholders so every positional argument is used exactly once"
                        .to_string(),
                ),
            ),
            ParseError::MalformedInterpolationSpec { spec } => (
                "GP0021",
                format!("malformed format spec `:{spec}`"),
                Some(
                    "a spec is fill, alignment, sign, zero-pad, width, precision, and radix or \
                     exponent (`:>8`, `:+08.3`, `:#x`, `:.2e`), or `?`; an expression that needs \
                     a `:` of its own is written in parentheses"
                        .to_string(),
                ),
            ),
            ParseError::LeadingBinaryOperator { op, .. } => (
                "GP0066",
                format!("a line cannot start with `{op}`"),
                Some(format!(
                    "end the line above with `{op}` to continue the expression across lines; \
                     `|>` is the one operator that may begin a line, and a line starting \
                     with `||`, `|`, `-`, `&`, or `*` begins a new statement"
                )),
            ),
            ParseError::DiscardedClosure { .. } => (
                "GP0067",
                "this closure is a statement of its own, so its value is unused".to_string(),
                Some(
                    "a line starting with `||` or `|` begins a closure; to continue a logical \
                     or across lines, end the line above with `||`, and to keep the closure, \
                     bind it with `let`"
                        .to_string(),
                ),
            ),
            ParseError::UnmatchedInterpolationBrace { brace } => (
                "GP0065",
                format!("unmatched `{brace}` in an interpolated string"),
                Some(
                    "a placeholder is `{expr}` or `{expr:spec}`; write a literal brace twice, as \
                     `{{` or `}}`"
                        .to_string(),
                ),
            ),
            ParseError::InterpolatedPositional { text } => (
                "GP0063",
                format!("interpolated string placeholder `{{{text}}}` names no value"),
                Some(
                    "an `f\"...\"` string fills each placeholder from the expression inside \
                     it, as in `f\"{total}\"` or `f\"{a + b:>8}\"`; to fill `{}` placeholders in \
                     order, call `format(\"...\", a, b)`"
                        .to_string(),
                ),
            ),
            ParseError::FormatStringMustBeLiteral => (
                "GP0024",
                "format argument must be a string literal".to_string(),
                Some(
                    "the first argument is the template when it is a string literal, and a \
                     lone argument renders on its own; two or more arguments need a \
                     template, as in `format(\"value: {}\", value)`"
                        .to_string(),
                ),
            ),
            other => other.code_title_help_shape(),
        }
    }

    /// Code/title/help for the item-annotation, literal-spelling, and
    /// construct-shape errors.
    fn code_title_help_shape(&self) -> (&'static str, String, Option<String>) {
        match self {
            ParseError::MissingItemType {
                kind,
                name,
                inferred,
            } => (
                "GP0034",
                format!("{kind} `{name}` needs a type annotation"),
                // With a type inferred from the initialiser the suggestion
                // below carries the exact edit, so a help would repeat it.
                match inferred {
                    Some(_) => None,
                    None => Some(format!(
                        "write the type after the name, as in `{name}: i64`; a {kind} is never \
                         inferred from its value"
                    )),
                },
            ),
            ParseError::RemovedCollectionLiteral {
                spelling,
                container,
            } => (
                "GP0032",
                format!("`{spelling}` literals are not valid syntax"),
                Some(format!(
                    "build the container through its type: `{container}::new()` for an empty one, \
                     or `{container}::from([a, b, c])` from a Vec literal"
                )),
            ),
            ParseError::SlicePatternExtraRest => (
                "GP0035",
                "a slice pattern may contain at most one `..`".to_string(),
                Some(
                    "keep a single `..`; elements before it form the prefix and those after it \
                     the suffix, as in `[first, ..rest, last]`"
                        .to_string(),
                ),
            ),
            ParseError::StructLiteralExtraSpread => (
                "GP0036",
                "a struct literal may contain at most one `..base` spread".to_string(),
                Some(
                    "keep a single `..base` and list every field you want to override explicitly"
                        .to_string(),
                ),
            ),
            ParseError::RefutableLetNeedsElse => (
                "GP0037",
                "a refutable `let` pattern requires an `else` block".to_string(),
                Some(
                    "write `let Some(x) = opt else { return }`; the `else` block must diverge"
                        .to_string(),
                ),
            ),
            ParseError::UnsupportedVisibilityRestriction { written } => (
                "GP0038",
                format!("`pub({written})` is not a visibility Gossamer has"),
                Some(
                    "the three visibilities are private (no annotation), \
                     `pub(package)`, and `pub`"
                        .to_string(),
                ),
            ),
            // Every other variant is handled earlier in the chain; this
            // split exists only to keep each match under the line cap.
            _ => unreachable!("code_title_help dispatches every other variant"),
        }
    }
}
