//! Features the language declines (SPEC §17.5), reported where their
//! spelling starts together with what a program writes instead.

use gossamer_lex::Span;

use crate::diagnostic::ParseError;
use crate::parser::Parser;

/// One declined feature: what it is, and what replaces it.
pub(crate) type Declined = (&'static str, &'static str);

pub(crate) const ASYNC: Declined = (
    "`async` functions and `.await`",
    "start concurrent work with `spawn` inside a `cohort { }` and send its result over a channel",
);
pub(crate) const GENERATORS: Declined = (
    "generators (`yield`, `gen fn`)",
    "write a type with `impl Iterator` and `fn next(&mut self) -> Option<T>`; pulling stops early",
);
pub(crate) const EXCEPTIONS: Declined = (
    "exceptions (`try`, `catch`, `throw`)",
    "return `Result<T, E>` and propagate a failure with `?`",
);
pub(crate) const LIFETIMES: Declined = (
    "lifetimes",
    "drop the lifetime: a reference is managed and lives as long as it is used",
);
pub(crate) const MOVE_CLOSURES: Declined = (
    "`move` closures",
    "drop `move`: a closure captures what it names by itself",
);
pub(crate) const TRAIT_OBJECTS: Declined = (
    "`dyn` trait objects",
    "take a generic parameter `<T: Trait>`, or an enum with a variant per concrete type",
);
pub(crate) const IMPL_TRAIT_TYPES: Declined = (
    "`impl Trait` types",
    "answer `Iterator<T>` for a lazy sequence or `Fn(..) -> R` for a callable, take a generic parameter `<T: Trait>`, or name the concrete type",
);
pub(crate) const GENERIC_ASSOCIATED_TYPES: Declined = (
    "generic associated types",
    "declare the associated type without parameters (`type Item`)",
);
pub(crate) const SPECIALIZATION: Declined = (
    "specialized impls (`default fn`)",
    "write one impl per concrete type",
);
pub(crate) const UNIONS: Declined = ("union types", "declare an enum with a variant per case");
pub(crate) const CLASSES: Declined = (
    "classes",
    "declare a `struct`, give it methods in an `impl` block, and share behaviour through a trait",
);
pub(crate) const USER_MACROS: Declined = (
    "user-defined macros",
    "write a function, or compute with a `comptime fn` and splice its source with `codegen(..)`",
);
pub(crate) const PROC_MACROS: Declined = (
    "procedural macros",
    "compute with a `comptime fn` and splice its source with `codegen(..)`",
);
pub(crate) const DETACHED_GO: Declined = (
    "detached goroutines (`go expr`)",
    "write `spawn(|| expr)`, which a `cohort { }` (or `main`) joins; the closure body runs in \
     the goroutine, so bind an argument that must be evaluated at the spawn site first",
);
pub(crate) const COMPREHENSIONS: Declined = (
    "comprehensions",
    "chain the iterator: `xs.iter().filter(..).map(..).collect()`",
);

impl Parser<'_> {
    /// Whether the token after the next one starts on a later source line.
    pub(crate) fn newline_after_peek(&self) -> bool {
        let here = self.peek_span();
        let after = self.peek_nth(1).span;
        if after.start < here.end {
            return false;
        }
        let between = Span {
            file: here.file,
            start: here.end,
            end: after.start,
        };
        self.slice(between).contains('\n')
    }

    /// `span` widened to the start of the next token when only spaces on the
    /// same line separate them, so deleting it leaves no gap.
    pub(crate) fn through_trailing_space(&self, span: Span) -> Span {
        let next = self.peek_span();
        if next.start < span.end {
            return span;
        }
        let gap = Span {
            file: span.file,
            start: span.end,
            end: next.start,
        };
        if self.slice(gap).chars().all(|c| c == ' ' || c == '\t') {
            Span {
                file: span.file,
                start: span.start,
                end: next.start,
            }
        } else {
            span
        }
    }

    /// Records `feature` as declined at `span`.
    pub(crate) fn report_declined(&mut self, (feature, instead): Declined, span: Span) {
        self.record(
            ParseError::DeclinedFeature {
                feature: feature.to_string(),
                instead: instead.to_string(),
                replacement: None,
            },
            span,
        );
    }
}
