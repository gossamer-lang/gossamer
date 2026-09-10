//! Centralized registry of every diagnostic code Gossamer emits.
//!
//! Each entry pairs a stable code (e.g. `"GT0001"`) with a
//! one-paragraph explanation rendered by `gos explain CODE`. Front-end
//! crates emit the code; this module owns the explanation text.
//!
//! Invariants enforced by `tests/registry.rs`:
//! - every code emitted by the compiler appears here exactly once,
//! - codes are sorted alphabetically,
//! - explanation text is non-empty.

/// Stable code-to-explanation table consumed by `gos explain CODE` and
/// by `gossamer_lint::lint_explanation`. Sorted alphabetically by code.
pub const REGISTRY: &[(&str, &str)] = &[
    (
        "GL0001",
        "Declares a `let` binding whose name is never read.\n\
            Prefix the name with `_` to silence explicitly (e.g. `_tmp`)\n\
            when the binding is intentional but unused.",
    ),
    (
        "GL0002",
        "A `use` declaration whose imported name is never referenced.\n\
            Remove the import or reference the name in the file.",
    ),
    (
        "GL0003",
        "A binding marked `mut` that is never reassigned. Drop the `mut`\n\
            keyword.",
    ),
    (
        "GL0004",
        "`return expr` at the tail of a block is the same as writing `expr`\n\
            by itself. Prefer the tail form for symmetry with the rest of\n\
            the expression language.",
    ),
    (
        "GL0005",
        "`if cond { true } else { false }` is the same as `cond`. The\n\
            inverted form is `!cond`.",
    ),
    (
        "GL0006",
        "`x == true` reads worse than `x`, and `x == false` reads worse\n\
            than `!x`. Drop the literal.",
    ),
    (
        "GL0007",
        "A `match` with a single arm reads better as `if let PATTERN = ...`.\n\
            Single-arm `match` is almost always a half-written exhaustive match.",
    ),
    (
        "GL0008",
        "A `let` binding in the same block shadows an earlier one. Rename\n\
            one of them to make the data flow obvious.",
    ),
    (
        "GL0009",
        "`let _ = expr?` silently discards an error. Either handle the\n\
            `Err` branch explicitly or propagate with `?` so the caller sees it.",
    ),
    (
        "GL0010",
        "An empty `{}` block is almost always a mistake. Add an explicit\n\
            `()` tail if the block is intentional.",
    ),
    (
        "GL0011",
        "`panic!` inside `main` aborts without a clean exit code. Return a\n\
            `Result` from `main` and use `?` so the error propagates.",
    ),
    (
        "GL0012",
        "Calling `.clone()` on a literal or already-copied value is\n\
            redundant. Drop the call.",
    ),
    (
        "GL0013",
        "`!!x` collapses to `x` when `x: bool`. If the double negation is\n\
            intentional for truthiness coercion, use an explicit cast.",
    ),
    (
        "GL0014",
        "Assigning a variable to itself does nothing. The statement is\n\
            usually the residue of a refactor - remove it.",
    ),
    (
        "GL0015",
        "`todo()` and `unimplemented()` are placeholders, not shippable\n\
            expressions. Implement the branch before merging.",
    ),
    (
        "GL0016",
        "`if true { ... }` / `while false { ... }` - the branch is\n\
            decided at compile time. Drop the control-flow construct.",
    ),
    (
        "GL0017",
        "`let x = expr; x` at the tail of a block is just `expr`. Drop\n\
            the needless binding.",
    ),
    (
        "GL0018",
        "`if a { if b { ... } }` can be combined into\n\
            `if a && b { ... }`. Easier to scan.",
    ),
    (
        "GL0019",
        "Both branches of the `if` are identical. Drop the branch and\n\
            keep the body once.",
    ),
    (
        "GL0021",
        "`if cond { return X } else { Y }` - the `else` is unreachable\n\
            fall-through. Un-nest the `else` body.",
    ),
    (
        "GL0022",
        "Comparing a value to itself is always `true` (for `==`, `<=`,\n\
            `>=`) or `false` (for `!=`, `<`, `>`). Use the constant.",
    ),
    (
        "GL0023",
        "`x + 0`, `x - 0`, `x * 1`, `x / 1` all equal `x`. The operation\n\
            adds nothing but noise.",
    ),
    (
        "GL0024",
        "`let x = ()` binds the unit value, which is almost never useful.\n\
            Drop the `let`.",
    ),
    (
        "GL0025",
        "Equality against a float literal is almost never what you want -\n\
            floating-point arithmetic rarely produces the exact bit pattern.\n\
            Compare `(x - y).abs() < eps` with an explicit tolerance.",
    ),
    (
        "GL0026",
        "`else {}` adds no information. Drop the else and let the `if`\n\
            stand alone.",
    ),
    (
        "GL0027",
        "`match b { true => ... false => ... }` is an `if` in disguise.\n\
            Rewrite as `if b { ... } else { ... }`.",
    ),
    (
        "GL0028",
        "`(x)` without a trailing comma is a needless pair of parens -\n\
            `x` reads the same. `(x,)` is a one-tuple and means something\n\
            different. Closure parameters follow the same rule: write `|t|`,\n\
            not `|(t)|`.",
    ),
    (
        "GL0029",
        "`!(a == b)` is just `a != b`. Prefer the direct operator.",
    ),
    (
        "GL0030",
        "Three or more nested `if / else if` layers are hard to skim.\n\
            Rewrite as `match` on the discriminant.",
    ),
    (
        "GL0031",
        "A literal range whose lower bound exceeds its upper bound is\n\
            empty. Swap the bounds or double-check the intent.",
    ),
    (
        "GL0032",
        "`\"a\" + \"b\"` can be written directly as `\"ab\"`. Let the\n\
            source reflect the final value.",
    ),
    ("GL0033", "`-(-x)` is `x`. The extra unary does nothing."),
    (
        "GL0034",
        "`if !cond { A } else { B }` scans better as `if cond { B }\n\
            else { A }`. Flip the branches and drop the `!`.",
    ),
    (
        "GL0035",
        "Concatenating an empty string literal is a no-op. Drop the\n\
            `\"\" +` or `+ \"\"`.",
    ),
    (
        "GL0036",
        "`println(\"\")` already writes a newline. Don't pass `\"\\n\"`\n\
            and don't call it twice to emit a blank line.",
    ),
    (
        "GL0037",
        "Two match arms share the same body. Either collapse them with\n\
            `|` alternation or extract the shared body into a helper.",
    ),
    (
        "GL0038",
        "Three consecutive statements `let tmp = a; a = b; b = tmp` swap\n\
            two bindings via a temporary. Prefer a destructuring assignment\n\
            once the language supports it, or at minimum document why the\n\
            swap is needed.",
    ),
    (
        "GL0039",
        "Two back-to-back assignments to the same place - the earlier\n\
            value is dead before it's read. Drop the first or consolidate\n\
            the logic into one statement.",
    ),
    (
        "GL0040",
        "Integer literals of five or more digits are easier to scan with\n\
            `_` as thousands separators: `1_000_000` instead of `1000000`.",
    ),
    (
        "GL0041",
        "`|x| f(x)` is a closure that just forwards to `f`. Pass `f`\n\
            directly.",
    ),
    (
        "GL0042",
        "An `if cond { } else { body }` is the same as `if !cond { body }`.\n\
            Invert the condition and drop the empty branch.",
    ),
    (
        "GL0043",
        "`match b { true => 1, false => 0 }` is an `if` in disguise that\n\
            happens to return an integer. Prefer `if b { 1 } else { 0 }`.",
    ),
    (
        "GL0044",
        "`fn f() -> () { ... }` is the same as `fn f() { ... }`. The\n\
            explicit `-> ()` is noise.",
    ),
    (
        "GL0045",
        "`let _: () = expr` annotates the binding with the unit type. If\n\
            `expr` was going to return `()` anyway, the annotation is noise.\n\
            If it wasn't, the annotation forces a coercion - use a plain\n\
            statement instead.",
    ),
    (
        "GL0046",
        "`match x { _ => expr }` always runs `expr` - the `match` adds\n\
            nothing. Drop the `match` (and add `let _ = x` if evaluating\n\
            the scrutinee has side effects).",
    ),
    (
        "GL0047",
        "`if (cond) { ... }` wraps the condition in a single-tuple\n\
            expression. Drop the parens: `if cond { ... }`.",
    ),
    (
        "GL0048",
        "`match () { ... }` has exactly one reachable arm. Drop the match\n\
            and run the body directly.",
    ),
    (
        "GL0049",
        "`panic()` with no argument leaves the post-mortem with nothing\n\
            to render. Always pass a brief explanation.",
    ),
    (
        "GL0050",
        "`loop {}` with no body busy-waits forever at 100% CPU. Add a\n\
            `break`, a `continue`, or replace with a real wait primitive.",
    ),
    (
        "GL0051",
        "A counted loop only assigns the same value to every element of a\n\
            sequence. Use `.fill(value)` to state the operation directly.",
    ),
    (
        "GL0052",
        "A scan calls `s.substring(i, i + 1)`, allocating a String per step.\n\
            `substring` takes byte offsets, so `s.byte_at(i)` reads the same\n\
            position directly as an `i64`. Note that `s[i]` is not that byte:\n\
            indexing counts Unicode scalars and yields a `char`.",
    ),
    (
        "GL0055",
        "A signature with no return type answers a unit, so the value the\n\
            body's tail expression produces is discarded and the caller reads\n\
            the unit the signature promises. Return it with `-> T`, or write\n\
            `-> ()` to say the discard is deliberate.",
    ),
    (
        "GL0056",
        "An expression statement that only computes - a path, a literal,\n\
            arithmetic over them - and hands its value to nothing. A newline\n\
            followed by `-`, `*`, or `&` starts a NEW statement, so a\n\
            continuation meant as part of the line above becomes one of these.\n\
            Join it to that line (end the previous line with the operator, or\n\
            parenthesise), or bind what it computes.",
    ),
    (
        "GM0001",
        "Generic monomorphization received a type substitution that the\n\
                     compiler does not yet support - typically a generic parameter\n\
                     instantiated with a non-scalar (Vec, HashMap, struct). Track\n\
                     A's P8 widens this; in the meantime, instantiate the generic\n\
                     with a scalar (i64 / bool / f64) or write a non-generic\n\
                     specialisation.",
    ),
    (
        "GM0002",
        "A `match` arm is unreachable because an earlier arm already\n\
                     covers every value its pattern would match. Drop the dead\n\
                     arm or refine the earlier pattern so the later one becomes\n\
                     reachable.",
    ),
    (
        "GM0003",
        "A value allocated inside an `arena { }` block is used after the\n\
                     block exits. The arena frees its memory in one shot at the\n\
                     closing brace, so the reference would dangle - a use-after-free.\n\
                     Keep only a scalar or already-outside summary (assigning to an\n\
                     outer binding, pushing into an outer container, sending on a\n\
                     channel, returning, or capturing in a goroutine/closure are the\n\
                     escapes the check rejects), or allocate the value before the\n\
                     block. The raw `runtime::arena_push/pop` primitive is unchecked.",
    ),
    (
        "GP0001",
        "The parser saw a token where it expected a different one.\n\
                     Check for missing punctuation, an unmatched delimiter, or an \n\
                     out-of-place keyword.",
    ),
    (
        "GP0002",
        "The parser reached end-of-file in the middle of a construct.\n\
                     Finish the expression, statement, or item - or remove it.",
    ),
    (
        "GP0003",
        "A balanced construct (block, tuple, array, string literal) was\n\
                     left unterminated. Add the matching closing delimiter.",
    ),
    (
        "GP0004",
        "Comparison operators like `==` / `!=` / `<` are not associative.\n\
                     Parenthesise the operands: `(a == b) && (b == c)`.",
    ),
    (
        "GP0005",
        "Range operators (`..`, `..=`) are not associative.\n\
                     Parenthesise the operands: `(a..b)..c`.",
    ),
    (
        "GP0006",
        "A braced struct literal in the scrutinee of `if`/`while`/`match`\n\
                     is ambiguous with the block that follows.\n\
                     Wrap the literal in `(...)`.",
    ),
    (
        "GP0007",
        "The right-hand side of `|>` must be a callable: a function\n\
                     reference, a method call (which receives the piped value as\n\
                     its last positional argument), or a closure.",
    ),
    (
        "GP0008",
        "Assignment (`=`, `+=`, …) only appears at statement position.\n\
                     If you need an expression, return the right-hand side\n\
                     directly.",
    ),
    ("GP0009", "An integer literal is required at this position."),
    ("GP0010", "A string literal is required at this position."),
    (
        "GP0011",
        "A tuple index must be a plain decimal integer (`p.0`, `p.1`).\n\
                     Hex, binary, or octal indices are not accepted.",
    ),
    (
        "GP0012",
        "A label identifier is required after the leading `'`.",
    ),
    (
        "GP0013",
        "An attribute is malformed. Accepted forms are `#[attr]`,\n\
                     `#[attr(args)]`, and `#[attr = value]`.",
    ),
    (
        "GP0014",
        "A `use` declaration could not be parsed. Check the path for\n\
                     stray punctuation or an unfinished brace list.",
    ),
    (
        "GP0015",
        "Two consecutive tokens formed something the parser does not\n\
                     recognise. Most often a missing operator or comma.",
    ),
    (
        "GP0016",
        "The `extern` keyword is reserved but has no\n\
                     source-level item form. Gossamer's FFI surface is the\n\
                     `[rust-bindings]` section of `project.toml` plus the\n\
                     `gossamer-binding` crate (see https://gossamer-lang.org/docs/libraries/).\n\
                     Remove the `extern \"C\" { ... }` block or rewrite the\n\
                     binding as a Rust crate consumed via `[rust-bindings]`.",
    ),
    (
        "GP0017",
        "An expression nested past the parser's hard recursion limit.\n\
                     Surfaced as a guard against adversarial input; split the\n\
                     expression into smaller helpers so the parse tree stays\n\
                     within the limit.",
    ),
    (
        "GP0018",
        "The lexer rejected a token before parsing could continue. The diagnostic identifies the malformed string, comment, escape, or token spelling.",
    ),
    (
        "GP0019",
        "Executable statements are allowed only in the entry file. Module bodies contain declarations; move the statement into a function.",
    ),
    (
        "GP0020",
        "An entry file cannot contain both bare top-level statements and an explicit fn main. Move the statements into main, or use the implicit entry form.",
    ),
    (
        "GP0021",
        "A format placeholder must be a binding name, a format specification, or an explicit positional placeholder. Bind complex expressions first.",
    ),
    (
        "GP0022",
        "Automatic serialization cannot be generated because a named field has an unsupported type. Change that field to a serializable type or provide a supported representation.",
    ),
    (
        "GP0023",
        "The number of positional format arguments differs from the number of positional placeholders. Add or remove placeholders to make them match.",
    ),
    (
        "GP0024",
        "Format macros require a literal template so placeholders can be checked at compile time. Replace the computed template with a string literal.",
    ),
    (
        "GP0026",
        "The inclusive range operator always needs an upper bound. Supply the bound, or use an exclusive open range.",
    ),
    (
        "GP0027",
        "`$` was written where an expression belongs. It used to spell the\n\
            slot a `|>` step filled; a step that needs the value in a\n\
            particular slot is now a closure, as in `x |> |v| f(a, v)`. A\n\
            method already chains, as in `x.trim()`, and a callback is a\n\
            closure or a function named in value position.",
    ),
    (
        "GP0029",
        "Every match arm needs an arrow after the pattern and optional guard.",
    ),
    (
        "GP0030",
        "A match arm has an arrow but no result expression. Add the value or block that the arm should produce.",
    ),
    (
        "GP0031",
        "Expression-bodied match arms on the same line require a comma boundary. Add a comma, or begin the next arm on a new line.",
    ),
    (
        "GP0032",
        "A bracket spelling that used to build a container is no longer syntax. Construct the container through its type instead: `Type::new()` for an empty one, or `Type::from([a, b, c])`.",
    ),
    (
        "GP0033",
        "A triple-quoted string spanning several lines carried text on the\n\
                     same line as its opening `\"\"\"`. The body starts on the next\n\
                     line, and the indentation it shares with the closing `\"\"\"`\n\
                     is stripped from every line.",
    ),
    (
        "GP0034",
        "A `const` or `static` item was declared without a type annotation.\n\
                     These items are never inferred from their initialiser, so the type\n\
                     is written after the name: `const EPS: f64 = 1e-12`.",
    ),
    (
        "GP0035",
        "A slice pattern wrote more than one `..`. One rest binding splits the\n\
            elements into a prefix and a suffix, as in `[first, ..rest, last]`;\n\
            a second `..` has no elements left to describe.",
    ),
    (
        "GP0036",
        "A struct literal wrote more than one `..base` functional update. Keep a\n\
            single base value and list every field you want to override\n\
            explicitly.",
    ),
    (
        "GP0037",
        "A `let` whose pattern can fail to match was written without an `else`\n\
            block. Give the failure a diverging path: `let Some(x) = opt else\n\
            { return }`.",
    ),
    (
        "GP0038",
        "A visibility was written with a restriction Gossamer does not have.\n\
            The three visibilities are private (no annotation),\n\
            `pub(package)`, and `pub`.",
    ),
    (
        "GP0039",
        "A serde turbofish named a type typed serde does not cover.\n\
            A codec is synthesized per concrete struct whose fields the\n\
            synthesizer can classify, so a generic struct, an enum, or a name\n\
            that is not a struct has none. Exchange a concrete struct, read\n\
            the document dynamically with `json::parse`, or hand-write the\n\
            function.",
    ),
    (
        "GP0040",
        "A `use` path was written with the hyphens a package name may carry.\n\
            `-` is subtraction, never part of an identifier, so the path stops\n\
            at the first one and the rest reads as an expression. A\n\
            dependency's module name is its package name with each `-`\n\
            replaced by `_`, so `pgsql-gos` is imported as `use pgsql_gos`.",
    ),
    (
        "GP0041",
        "A `|>` step wrote its own arguments, so the piped value has no slot\n\
            left. The pipe composes free functions; a step that writes other\n\
            arguments is a closure whose parameter is that slot, as in\n\
            `x |> |v| f(a, v)`. No argument order is assumed, so a data-first\n\
            callee such as `|v| strings::split(v, sep)` reads the same as a\n\
            data-last one. `--fix` puts the parameter in the trailing slot,\n\
            which a data-first callee does not want, so confirm the slot is\n\
            the one the call needs.",
    ),
    (
        "GP0042",
        "A binding or an assignment wrote its targets as a parenthesised\n\
            tuple. The comma-separated list is the one spelling: `let a, b =\n\
            value` and `a, b = x, y`. Parentheses group a pattern only where\n\
            one sits beside others - a `match` arm, a `for` binding, a\n\
            parameter, or a nested element of the list itself, as in\n\
            `let a, (b, c) = 1, (2, 3)`. `--fix` drops the outer pair.",
    ),
    (
        "GP0043",
        "A `mod` declaration ended in `;`. A statement ends at its newline\n\
            here, and a trailing semicolon is invalid everywhere else in the\n\
            language; this was the one form that asked for one. Write\n\
            `mod name`. `--fix` drops the semicolon.",
    ),
    (
        "GP0044",
        "A callable type was written as `fn`, `FnMut`, or `FnOnce`. The\n\
            language has one callable type, `Fn(args) -> ret`: there is no raw\n\
            function-pointer shape and no `FnMut` / `FnOnce` distinction to\n\
            draw. `--fix` rewrites the spelling.",
    ),
    (
        "GP0045",
        "An argument label was bound with `=`. Every keyed form in the\n\
            language binds with `:` - a struct field, a map entry, a `cohort`\n\
            header setting - and `=` at a call site reads as the assignment it\n\
            is elsewhere. Write `name: value`. `--fix` rewrites the separator.",
    ),
    (
        "GP0046",
        "`unsafe` was written. No operation is withheld outside an `unsafe`\n\
            block or from a safe `fn`, so the keyword marked a boundary the\n\
            language does not draw. It stays reserved for a future raw-FFI\n\
            story. `--fix` drops it.",
    ),
    (
        "GP0047",
        "An opaque alias was written `type X = new T`. `new` reads as\n\
            allocation to a reader arriving from any other language, and the\n\
            concept has a standard name: write `newtype X = T`.",
    ),
    (
        "GP0048",
        "`go` was written. Every goroutine is a `spawn` attached to the\n\
            enclosing cohort, and `main` is an implicit root cohort, so\n\
            nothing a program starts is left unaccounted for: a failure is\n\
            reported, and no goroutine outlives the block that started it.\n\
            `go` evaluated its operands at the spawn site while a closure\n\
            body runs in the child, so `--fix` hoists an operand that\n\
            computes a value into a temporary before the `spawn`.",
    ),
    (
        "GP0049",
        "A compiler-known call was written with a `!`. The set of such names\n\
            is closed and recognised at the `(`, so there is nothing a sigil\n\
            disambiguates: `println(..)`, `format(..)`, `matches(..)`, and the\n\
            rest are ordinary calls. The first argument is still the template\n\
            when it is a string literal, and a lone argument renders on its\n\
            own. `--fix` drops the `!`.",
    ),
    (
        "GP0050",
        "An `enum Name : R` named a representation that is not an unsigned\n\
            width. A discriminant is a count, so its store is unsigned: write\n\
            `enum Name : u8 { .. }`, with a width between 1 and 64 bits.",
    ),
    (
        "GP0051",
        "`regex!` or `sql!` was written. Both moved into their modules:\n\
            `regex::compile(\"…\")` and `sql::statement(\"…\")`. A literal\n\
            argument is still validated while the program is compiled, and\n\
            the module keeps its name rather than becoming a global.",
    ),
    (
        "GP0052",
        "`sql::statement` was handed something other than a literal. The\n\
            statement is checked while the program is compiled, so it has to\n\
            be there to check; a statement built at run time is an ordinary\n\
            `String` and needs no wrapper.",
    ),
    (
        "GP0053",
        "A `Display` implementation declared its rendering as `to_string`.\n\
            Both rendering contracts declare one method that answers a\n\
            `String`, so both are written `fn fmt`; the `impl` header is what\n\
            decides which channel a value reaches, `{}` taking `Display` and\n\
            `{:?}` taking `Debug`. `x.to_string()` still renders through\n\
            `Display`.",
    ),
    (
        "GP0054",
        "A parameter was declared with a shared reference type. An argument\n\
            is passed without copying whatever its type, and a callee cannot\n\
            write to the caller's variable unless the parameter says `&mut`,\n\
            so `f(m: &Map)` and `f(m: Map)` have the same cost and the same\n\
            guarantee. A sequence view is written `[T]`, and `&mut [T]` is\n\
            the form that writes through. `--fix` drops the `&`.",
    ),
    (
        "GP0055",
        "A call argument or a binary operand was written as a shared\n\
            reference. There is no shared-reference type left for the sigil\n\
            to name: an argument and an operand are both read without\n\
            copying either way, and a callee writes to the caller's variable\n\
            only through a `&mut` parameter, which is spelled `&mut` at the\n\
            call too. `--fix` drops the `&`, except where dropping it would\n\
            let the argument bind as a call on the one before it.",
    ),
    (
        "GP0056",
        "A cohort header used the retired `context:` isolation spelling.\n\
            `context::Context` is the cancellation type a cohort may one\n\
            day inherit; this setting decides whether a child gets an OS\n\
            thread of its own, so it is `isolation: Isolation::Shared` or\n\
            `isolation: Isolation::Thread`. `--fix` rewrites it.",
    ),
    (
        "GR0001",
        "A name used in source could not be resolved to a declaration.\n\
                     Check the spelling, whether a `use` brings the name into scope,\n\
                     and whether the item is visible at this location.",
    ),
    (
        "GR0002",
        "A path was found in the wrong namespace - for example a value\n\
                     where a type was expected, or a module where a value was\n\
                     expected. Re-check the import target.",
    ),
    (
        "GR0003",
        "Two items in the same module share a name. Rename one of them\n\
                     or move it into a distinct `mod`.",
    ),
    (
        "GR0004",
        "A `use` declaration imported the same name twice. Drop the\n\
                     duplicate or rename one of the imports with `use ... as ...`.",
    ),
    (
        "GR0005",
        "The `use` names a module path that does not exist.\n\
                     A `std::` module has exactly one canonical path (e.g. JSON\n\
                     lives at `std::encoding::json`); check `gos doc` or the\n\
                     stdlib reference for it. A `crate::` / `super::` / `self::`\n\
                     path names a module of this package or an item one declares,\n\
                     and a file is a module: add the file that declares it, or\n\
                     correct the path.",
    ),
    (
        "GR0006",
        "A container spelling that a canonical name replaced. Each container has exactly one name - import and write that one.",
    ),
    (
        "GR0007",
        "The `use` names a module that exists but an item that module does\n\
                     not export. Check the item spelling; `gos doc std::<module>`\n\
                     lists every name a module exports.",
    ),
    (
        "GR0008",
        "An item declared without `pub` is visible only inside the module\n\
                     that declares it and that module's descendants. Write `pub` on\n\
                     the declaration to let other modules name it.",
    ),
    (
        "GR0009",
        "A bare enum-variant name that two or more enums declare. Variant\n\
                     dispatch identifies a variant by name, so write the enum out -\n\
                     `Shape::Circle` rather than `Circle`.",
    ),
    (
        "GR0010",
        "A `mod name;` declaration whose module source was never supplied.\n\
                     Out-of-line modules are filled in from the project layout\n\
                     (`name.gos` or `name/mod.gos` beside the entry), so this\n\
                     names a file the build did not find - or a file outside any\n\
                     project, where the layout is not read at all.",
    ),
    (
        "GR0011",
        "A bare name that some module in this unit declares but which is not\n\
                     in this scope. A module's items are reached through a path\n\
                     (`util::add`) or an import (`use util::add`); the file layout\n\
                     declares the module, and the import brings its names in.",
    ),
    (
        "GR0012",
        "A `typeInfo::<T>()` named a type with nothing to reflect. The\n\
                     reflection surface describes a struct's fields or an enum's\n\
                     variants, so the type has to be one this program declares,\n\
                     and a unit struct - which has no fields - has nothing to\n\
                     return.",
    ),
    (
        "GR0013",
        "A call gave an argument a name its callee does not declare, gave\n\
                     the same one twice, wrote a positional argument after a named\n\
                     one, or named an argument on a method more than one type\n\
                     declares differently. A name selects a parameter, so it has to\n\
                     name one, name it once, and - because the positions after a\n\
                     name are no longer in written order - be followed only by\n\
                     further names.",
    ),
    (
        "GR0014",
        "A parameter default was not a constant. The default is spliced\n\
                     into every call that leaves the parameter out, so it has to be\n\
                     a literal - `10`, `-1`, `true`, `\"\"` - rather than an\n\
                     expression that would have to be resolved separately at each\n\
                     of those call sites.",
    ),
    (
        "GR0015",
        "A call left a parameter with neither an argument nor a default.\n\
            Once names and defaults are in play the argument count is a poor\n\
            description of the problem - a call can supply the declared\n\
            number of arguments and still leave a parameter unfilled - so\n\
            this names the parameters instead. Give each one a value,\n\
            positionally or by name, or declare a default for it.",
    ),
    (
        "GR0016",
        "A path named a dependency package that this file does not import.\n\
            A dependency's module is reached only through the import that\n\
            names the package it comes from, so `use \"example.com/lib\"`\n\
            states the provenance the bare path leaves implicit. Add the\n\
            import, or alias it with `use \"example.com/lib\" as name`.",
    ),
    (
        "GR0017",
        "A `break` or `continue` has no loop to act on: either none encloses\n\
                     it, or the label it names is not on any enclosing loop. A closure\n\
                     body is a separate function, so a loop outside it is not a target.",
    ),
    (
        "GR0018",
        "A path named one of the compiler-known calls - `println`,\n\
            `format`, `panic`, and the rest of the fixed set. Each expands\n\
            where it is written and the runtime binds no callable for it,\n\
            so the path has nothing to call or pass as a value. Write it\n\
            as `name(..)`; it needs no import.",
    ),
    (
        "GR0019",
        "Two dependency packages are reached under one module name. A `-` is\n\
            not part of an identifier, so a package name carrying one is\n\
            reached from source as the same name with `_` in its place -\n\
            which two packages can share (`a/pgsql-gos` and `b/pgsql_gos`\n\
            both become `pgsql_gos`). Every path headed by that name would be\n\
            ambiguous. Give one of them a name of its own in\n\
            `[dependencies]`, or import each through `use \"id\" as name`.",
    ),
    (
        "GR0020",
        "A `fn` or a binding was declared under one of the compiler-known\n\
            call names - `println`, `format`, `matches`, and the rest of the\n\
            fixed set. Each expands where it is written, so the declaration\n\
            could never be reached. Give it a name of its own.",
    ),
    (
        "GR0021",
        "A std free function was called with its data argument in the slot\n\
            it occupied before every module took its data first. The call\n\
            still means what it did, so nothing else in the body is\n\
            affected; write the data argument first, which is what makes\n\
            `iter::map(xs, f)` read as the `xs.map(f)` it stands for.",
    ),
    (
        "GT0001",
        "The type checker could not reconcile two types it expected to\n\
                     match. The primary label shows the location of the mismatch;\n\
                     the `note:` line names the conflicting types.",
    ),
    (
        "GT0002",
        "The type checker could not find a method with the supplied\n\
                     name on the receiver type. Check for a typo, a missing `use`,\n\
                     or a trait impl that lives in an unreachable module.",
    ),
    (
        "GT0003",
        "An operator (`+`, `*`, `==`, …) was applied to a type that does\n\
                     not implement it. Either change the operand type or implement\n\
                     the trait that backs the operator.",
    ),
    (
        "GT0004",
        "A `match` expression does not cover every possible value. Add\n\
                     an arm for the pattern(s) listed under `help:`.",
    ),
    (
        "GT0005",
        "The `as` cast is restricted to a whitelist of conversions:\n\
                     numeric <-> numeric, `bool`/`char` -> integer, `u8` -> `char`,\n\
                     and same-type no-ops. Struct / enum / String sources are\n\
                     rejected. Use a conversion method when you need serialisation;\n\
                     `as` does not run code.",
    ),
    (
        "GT0006",
        "A struct field access (`x.field`) referenced a name that the\n\
                     receiver type does not declare. Check the field name or the\n\
                     receiver's actual type - generics and inference often resolve\n\
                     this once the surrounding code is more constrained.",
    ),
    (
        "GT0007",
        "A `Result<T, E>` expression was used as a statement without its\n\
                     value being handled. If the operation failed the error is\n\
                     silently ignored, which is almost always a bug.\n\n\
                     Three ways to fix this:\n\
                     - Propagate with `?`: `do_something()?` (requires the\n\
                       enclosing function to return `Result`).\n\
                     - Match explicitly: `match do_something() { Ok(v) => …, Err(e) => … }`.\n\
                     - Acknowledge and discard: `let _ = do_something()` - this\n\
                       silences GT0007 but leaves the error unhandled; only\n\
                       appropriate when the operation is best-effort.\n\n\
                     SPEC §9 requires every `Result` value to be handled.",
    ),
    (
        "GT0008",
        "An expression, type, or pattern nested past the type-checker's\n\
                     hard recursion limit. Emitted on adversarial input that\n\
                     survives parsing; rewrite the construct with smaller helpers\n\
                     so the typechecker stays within its guard.",
    ),
    (
        "GT0009",
        "An integer literal overflows the value range of its declared\n\
                     type-suffix (e.g. `300i8`, `99999999999999999999i64`). The\n\
                     literal is treated as `TyKind::Error` so downstream typing\n\
                     does not cascade; pick a wider suffix or shrink the literal.",
    ),
    (
        "GT0010",
        "A string escape that the parser accepted but cannot be validly\n\
                     decoded (out-of-range `\\u{...}`, surrogate code point,\n\
                     non-ASCII `\\x..`). Fix the escape so the resulting Unicode\n\
                     scalar value is in range.",
    ),
    (
        "GT0011",
        "A `<T: Bound>` clause names a trait the resolver does not know\n\
                     about. Check the spelling, or bring the trait into scope so\n\
                     the bound can be enforced.",
    ),
    (
        "GT0012",
        "An enum declares more variants than the one-byte heap\n\
                     discriminant can index (256). Split the enum or group\n\
                     variants into nested enums.",
    ),
    (
        "GT0013",
        "A closure was passed to a std combinator the checker has no\n\
                     signature row for, so its parameter type cannot be inferred.\n\
                     Annotate the parameter (`|x: String| ...`) or bind the\n\
                     payload through a typed `match`.",
    ),
    (
        "GT0014",
        "`i128` / `u128` have no 128-bit runtime representation on any\n\
                     tier. Use `i64` / `u64`, or split the value into two 64-bit\n\
                     halves.",
    ),
    (
        "GT0015",
        "A std free function was used as a first-class value, but the\n\
                     signature catalogue carries no fixed parameter list for it, so\n\
                     it cannot be rewritten into the closure that calls it. Write\n\
                     the closure yourself: `|x| f(x)`. A compiler-known call is not\n\
                     a function and reports GR0018 instead.",
    ),
    (
        "GT0016",
        "`json::render` / `json::encode` was handed an enum value (often\n\
                     a `Result` missing its `?`). Enums have no JSON form; unwrap\n\
                     first, or use `to_json::<T>` for a struct.",
    ),
    (
        "GT0017",
        "A generic call instantiates a type parameter with a concrete\n\
                     type that does not implement a required trait bound. Add the\n\
                     `impl Trait for Type` or pass a type that already does.",
    ),
    (
        "GT0018",
        "A call supplied the wrong number of arguments for the callee's\n\
                     declared arity. The VM aborts on this and the native backend\n\
                     drops or zero-fills the mismatched arguments, so it is\n\
                     rejected at check time. Pass exactly as many arguments as the\n\
                     function declares parameters.",
    ),
    (
        "GT0019",
        "A path `Enum::Variant` named a variant the enum does not\n\
                     declare. The resolver leaves the path unresolved and the\n\
                     program faults at runtime; check the variant spelling against\n\
                     the enum declaration.",
    ),
    (
        "GT0020",
        "A method reached through a generic bound resolves only through a\n\
                     supertrait of that bound (e.g. `fn f<T: Pet>(p: T)` calling a\n\
                     method declared on `Animal` where `trait Pet: Animal`). The\n\
                     compiled tiers cannot lower supertrait-through-bound dispatch\n\
                     (SPEC §3.8); add the method to the named bound, or bound the\n\
                     parameter on the supertrait directly.",
    ),
    (
        "GT0021",
        "`value[index]` was used on a type that cannot be indexed. Only\n\
                     `[T]`, `[T; N]`, `Vec<T>`, and `String` support indexing. The\n\
                     VM faults (GX0001) and the compiled tier reads through the\n\
                     value as a base pointer (segfault), so it is rejected at check.",
    ),
    (
        "GT0022",
        "`value(args)` was used on a type that is not callable. Only `fn`\n\
                     items, `fn(..)` pointers, and `Fn(..)` values can be called.\n\
                     The VM faults (GX0001) and the compiled tier emits a call\n\
                     through a non-function symbol, so it is rejected at check.",
    ),
    (
        "GT0023",
        "`value.N` positional access was used on a non-tuple, or `N` is\n\
                     past the tuple's arity. The VM faults (GX0004) and the compiled\n\
                     tier reads out-of-object memory, so it is rejected at check.",
    ),
    (
        "GT0024",
        "A `type` alias expands to itself through a cycle (`type A = B;\n\
                     type B = A`), so it has no underlying type. Every use is\n\
                     ill-typed, so it is rejected at check.",
    ),
    (
        "GT0025",
        "A `#[derive(...)]` named a trait that synthesizes nothing.\n\
                     Gossamer value types compare, order, hash, and copy by value\n\
                     automatically, so only Debug, Default, PartialEq, Eq,\n\
                     PartialOrd, and Ord are derivable; everything else is either\n\
                     automatic or implemented with `impl Trait for T`.",
    ),
    (
        "GT0027",
        "A `match` / `if let` arm patterns a `json::Value` scrutinee with a\n\
                     `json::Value::Object(..)` / `::Array(..)` / `::Int(..)` (etc.)\n\
                     constructor. `json::Value` is an opaque dynamic-document handle\n\
                     with no matchable discriminant, so the pattern silently falls\n\
                     through on the VM and faults on the compiled tiers. Rejected at\n\
                     check; read the document with the dynamic accessors instead\n\
                     (`json::as_i64` / `json::as_str` / `json::get` / `json::keys`).",
    ),
    (
        "GT0028",
        "`.downgrade()` was called on a by-value type with no runtime RC\n\
                     header - a scalar (`i64` / `bool` / ...), an `Option` /\n\
                     `Result`, or another packed value. `Weak<T>` is a non-owning\n\
                     pointer into a reference-counted allocation, so the compiled\n\
                     tiers read a header off the value's bits and fault (SIGSEGV),\n\
                     while the VM returns a bogus handle. Rejected at check;\n\
                     downgrade a heap aggregate (struct / payload enum) instead.",
    ),
    (
        "GT0029",
        "An `option::*` or `result::*` combinator received its data argument\n\
                     in the wrong position. These functions are data-last: pass the\n\
                     closure first and the `Option` or `Result` value last.",
    ),
    (
        "GT0030",
        "An assignment targeted a binding that was not declared `mut`.\n\
                     `let` and parameter bindings are immutable by default, so a\n\
                     place rooted at one cannot be written with `=` or a compound\n\
                     `+=` / `-=` / ... . Declare it `let mut name` (or `mut name`\n\
                     in the parameter list). Writing through a `&mut T` reference\n\
                     stays allowed regardless of the pointer binding's own\n\
                     mutability.",
    ),
    (
        "GT0031",
        "An assignment targeted a place through a shared `&T` reference.\n\
                     Shared references permit reads but not writes, regardless of\n\
                     whether the reference binding itself is declared `mut`. Create\n\
                     the reference with `&mut` from a mutable place to write through it.",
    ),
    (
        "GT0032",
        "A mutable reference was requested for a place rooted at an immutable binding.\n\
                     Declare the source binding `mut` before taking `&mut`.",
    ),
    (
        "GT0033",
        "A nominal struct or enum value was destructured without naming its\n\
                     declared struct or variant. Use the named pattern so field layout\n\
                     cannot be bypassed by an anonymous tuple pattern.",
    ),
    (
        "GT0034",
        "A struct constructor used syntax that does not match its declaration.\n\
                     Named structs use braces and named fields; tuple structs use\n\
                     parentheses and positional fields.",
    ),
    (
        "GT0035",
        "A struct initializer omitted a required field. Supply every declared\n\
                     field, or use an explicit constructor that provides defaults.",
    ),
    (
        "GT0036",
        "A struct initializer specified the same field more than once. Remove\n\
                     the duplicate so each field has exactly one value.",
    ),
    (
        "GT0037",
        "A struct initializer supplied more positional values than the type has\n\
                     fields. Remove the extra values or use the correct constructor.",
    ),
    (
        "GT0041",
        "A lazy iterator was formatted or printed directly. Consume it with a\n\
                     terminal such as `.collect()`, `.count()`, or `.fold(...)` first.",
    ),
    (
        "GT0042",
        "A lazy iterator binding was used after an adapter or terminal consumed\n\
                     its state. Build a fresh iterator for the second traversal.",
    ),
    (
        "GT0043",
        "A second named mutable reference would overlap an active `&mut` binding.\n\
                     Named mutable references are exclusive for their lexical scope.\n\
                     End or narrow the earlier borrow before taking another `&mut` to the\n\
                     same root binding.",
    ),
    (
        "GT0044",
        "A generic function's return type parameter could not be inferred from\n\
                     its arguments or expected result. Add an explicit type annotation\n\
                     or turbofish type argument.",
    ),
    (
        "GT0045",
        "The `?` operator was used on a value that is not `Option` or `Result`,\n\
                     or in a function whose return type cannot propagate that family.\n\
                     Handle the value explicitly or change the enclosing return type.",
    ),
    (
        "GT0046",
        "A call omitted `&mut` for a parameter that can modify its argument.\n\
                     Ensure the source binding uses `let mut`, then pass its place as\n\
                     `&mut value`. An existing `&mut T` value can be forwarded directly.",
    ),
    (
        "GT0047",
        "A plain `let` used an irrefutable assignment position with a literal or\n\
                     another pattern that may fail. Use `if let` or `match` for a\n\
                     refutable pattern, or bind the value to a name.",
    ),
    (
        "GT0048",
        "A `let &...` or `let &mut ...` pattern was applied to a non-reference\n\
                     initializer. Borrow the initializer explicitly, or remove the\n\
                     reference marker to bind the value directly.",
    ),
    (
        "GT0049",
        "A bare `[T]` names an unsized slice and cannot be stored as an owned local, field, parameter, or return value. Use `[T; N]` for an owned fixed-size array, `Vec<T>` for an owned growable sequence, or borrow a sequence as `&[T]` or `&mut [T]`.",
    ),
    (
        "GT0050",
        "A method that changes sequence length or capacity was called on a fixed array or slice. Those operations require `Vec<T>`. Arrays and mutable slices can still mutate existing elements and use non-resizing methods.",
    ),
    (
        "GT0051",
        "A fixed array length was not known at compile time. Array lengths are part of `[T; N]`, so `[value; N]` requires a constant `N`. Use an explicit `Vec<T>` construction when the length is only known at runtime.",
    ),
    (
        "GT0052",
        "A reference would escape the lexical storage boundary that keeps its\n\
                     referent valid, such as a return, aggregate field, closure,\n\
                     channel, or goroutine. Keep the reference local or pass an owned\n\
                     value across that boundary.",
    ),
    (
        "GT0053",
        "An access conflicts with an active lexical reference to the same root\n\
                     binding. End or narrow the existing borrow before reading,\n\
                     mutating, resizing, or borrowing that place incompatibly.",
    ),
    (
        "GT0054",
        "A reference pattern attempted to copy an aggregate referent by value.\n\
                     Bind the reference itself, or destructure the owned aggregate\n\
                     without a reference pattern.",
    ),
    (
        "GT0055",
        "A by-value aggregate would cross a concurrency boundary whose compiled\n\
                     publication ABI cannot preserve that layout and all nested child\n\
                     ownership. Direct goroutine arguments therefore accept scalar and\n\
                     supported top-level runtime containers, not inline structs, tuples,\n\
                     or arrays. Channels also reject aggregates containing nested Vec\n\
                     storage. Publish supported fields separately and reconstruct the\n\
                     aggregate in the receiver.",
    ),
    (
        "GT0056",
        "A method was called on a generic parameter that none of its trait\n\
                     bounds declares. A parameter stands for every type a caller\n\
                     may supply, so its bounds are the whole of what it can do.\n\
                     Bound the parameter by a trait that declares the method.",
    ),
    (
        "GT0057",
        "A built-in iterator was passed to a parameter bound by an iteration\n\
                     trait. Only a type with an impl block can specialise such a\n\
                     call, so name the iterator type on the parameter directly.",
    ),
    (
        "GT0058",
        "A trait impl leaves out a method the trait declares without a\n\
                     default body. A call through the trait lowers to a direct\n\
                     call to each declared method, so every one needs a body.\n\
                     Add the missing method to the impl, or give it a default\n\
                     body in the trait.",
    ),
    (
        "GT0059",
        "A trait impl leaves out an associated type or constant the trait\n\
                     declares without a default. A projection through the trait\n\
                     has to land on a concrete item, so every impl supplies one.\n\
                     Add it to the impl, or give the trait a default.",
    ),
    (
        "GT0060",
        "A path projected an associated item that nothing in scope declares.\n\
                     Check the spelling against the trait's associated types and\n\
                     constants, or bound the parameter by the trait that declares\n\
                     the item.",
    ),
    (
        "GT0061",
        "An associated item reached through a trait has several candidate\n\
                     impls and none is singled out. Pin an associated type with\n\
                     an equality constraint on the bound, as in\n\
                     `T: Iterator<Item = i64>`; for an associated constant, name\n\
                     the concrete type or give the trait a default.",
    ),
    (
        "GT0062",
        "A value with no textual form was passed to a format macro: a\n\
                     runtime handle (`sync::Map`, `http::Client`, a middleware\n\
                     `Handler`), a function or closure, or a channel endpoint.\n\
                     Format an accessor, a call result, or the values that pass\n\
                     through the endpoint instead.",
    ),
    (
        "GT0063",
        "A method was called from outside the module whose `impl` block\n\
                     declares it, and the method is not `pub`. A method's\n\
                     visibility is declared on the method itself inside the\n\
                     `impl`, independently of the type's visibility: a `pub`\n\
                     type may keep private helpers. Add `pub` (or\n\
                     `pub(package)`) to the method, or call it through a public\n\
                     one.",
    ),
    (
        "GT0064",
        "A value from a `#[must_use]` function, or of a `#[must_use]`\n\
                     type, was used as a statement and discarded. The attribute\n\
                     marks values whose whole point is the value: dropping one\n\
                     means the call did nothing observable, or a guard was\n\
                     released immediately.\n\n\
                     Bind it (`let guard = acquire()`), consume it, or discard\n\
                     it deliberately with `let _ = expr`.",
    ),
    (
        "GT0065",
        "A struct field was read or written from outside the module that\n\
                     declares the struct, and the field is not `pub`. A field's\n\
                     visibility is declared on the field itself, so a `pub`\n\
                     struct may keep private ones: the type is part of the API\n\
                     while its representation is not.\n\n\
                     Add `pub(package)` or `pub` to the field, or reach it\n\
                     through a method the declaring module provides.",
    ),
    (
        "GT0066",
        "`.into()` was written between two types with no conversion\n\
                     behind it. An opaque alias (`type Id = new i64`) converts\n\
                     to and from its own representation for free, because the\n\
                     two share one runtime value. A fixed array converts into\n\
                     the `Vec` of its element. Any other pair - including two\n\
                     distinct aliases over the same representation - needs an\n\
                     explicit `impl From<Source> for Target`.\n\n\
                     The same code covers `.into()` / `.try_into()` with no\n\
                     target at all. A conversion's target comes from the use\n\
                     site, never from the receiver, so a call nothing\n\
                     annotates has nothing to convert to. Annotate the\n\
                     binding, pass the call to a typed parameter, or return\n\
                     it.",
    ),
    (
        "GT0067",
        "A `for` loop's subject was a `Result` or an `Option`. Neither is a\n\
            sequence: it holds at most one value and carries no element type,\n\
            so the loop binds nothing and its body runs zero times. Take the\n\
            value out first - with `?`, a `match`, `if let Some(v) = ..`, or\n\
            `unwrap_or(..)` - and iterate that.",
    ),
    (
        "GT0068",
        "A `Deque`, `Queue`, `Stack`, `MaxHeap`, or `MinHeap` named an\n\
            element it cannot hold. Each holds one 8-byte slot per element, so\n\
            the element is a scalar: an integer of any width, `f32` / `f64`,\n\
            `bool`, or `char`. A `String`, a container, or an aggregate is\n\
            held in a `Vec`, or reached through a key in a `Map`. A heap also\n\
            orders its elements, comparing a slot as a signed 64-bit value, so\n\
            it declines `u64` / `usize`, whose range runs past what that\n\
            comparison orders.",
    ),
    (
        "GT0069",
        "A parameter wrote `&` in its pattern rather than in its type.\n\
            `fn f(&m: Map<String, i64>)` declares a parameter that takes a\n\
            map by value and then destructures it as a reference, where\n\
            `fn f(m: Map<String, i64>)` was meant. A parameter's type is\n\
            what declares whether the call passes a reference; `&` in the\n\
            pattern destructures a reference the type already names, so over\n\
            a non-reference type it has no referent to bind. Move the `&`\n\
            into the type, or drop it to take the value.",
    ),
    (
        "GT0070",
        "An `impl` header named a trait nothing declares. `impl Bogus for S`\n\
            reads as a promise to satisfy `Bogus`, but with no declaration to\n\
            check against, the block's methods are only inherent methods under\n\
            a misleading header - a misspelled trait name compiles clean and\n\
            nothing dispatches through it. Declare the trait, correct the\n\
            spelling, or drop the trait name to write a plain `impl S` block.\n\
            The built-in traits an `impl` may name are the operator traits\n\
            (`Add`, `Index`, `Neg`, ...), the conversions (`From`, `TryFrom`,\n\
            `Into`, `TryInto`), and `Display` / `Debug`.",
    ),
    (
        "GT0071",
        "A trait name was written where a type belongs. `fn f(x: Display)`\n\
            names behaviour, not a value's shape, and Gossamer has no `dyn`,\n\
            so there is no value whose type a bare trait is. Bound a generic\n\
            parameter by the trait instead - `fn f<T: Display>(x: T)` - or\n\
            name the concrete type the parameter takes.",
    ),
    (
        "GT0072",
        "An `impl Trait for Type` block defined an item the trait does not\n\
            declare. The header promises exactly the trait's contract, so an\n\
            extra `fn` would become an inherent method under a misleading\n\
            heading - `impl Display for Point { fn show(..) }` reads as part\n\
            of `Display` while nothing dispatches to it through the trait.\n\
            Move the item into an inherent `impl Point { .. }` block, or\n\
            declare it in the trait so every implementor supplies it.",
    ),
    (
        "GT0073",
        "Two implementations supply the same trait for the same type, so a\n\
            call through the trait has two bodies to reach and no rule picks\n\
            one. A `#[derive(Debug)]` counts as one of them: the derive\n\
            synthesizes the same rendering a written `impl Debug for T`\n\
            supplies. Keep one - merge the blocks, or drop the derive.",
    ),
    (
        "GT0075",
        "A `&` or `&mut` applied to a range index. The index answers a fresh\n\
            copy of that range, so borrowing it would hand out a reference to\n\
            a temporary nothing owns; a window aliasing part of a sequence's\n\
            buffer has no value shape yet. Read a copy through the bare index\n\
            or `slice`, or edit in place with `copy_within`,\n\
            `copy_from_slice`, or an indexed write.",
    ),
    (
        "GT0076",
        "A goroutine's closure reads a binding whose type holds nested\n\
            growable storage - a struct or tuple carrying a Vec, Map, or Set.\n\
            Both goroutines would reach that storage through the same handle\n\
            with nothing serialising the access, and no compiled concurrency\n\
            ABI has an ownership descriptor for it. Build the value inside the\n\
            goroutine, send it over a channel, or guard it with\n\
            `sync::Shared` and reach it through `with`, which takes the lock\n\
            for the duration of the access.",
    ),
    (
        "GT0077",
        "`sync::Shared` guards one word, and only a scalar or a `String` is\n\
            read the same way from that word by the bytecode VM and by\n\
            compiled code. A collection or a struct would need a shape both\n\
            sides agree on before it can be guarded; publish one through a\n\
            channel instead, or keep a `Shared` per scalar field.",
    ),
    (
        "GT0078",
        "An assignment targets an expression that names no place. Only a\n\
            binding, a field, an index, or a dereference is writable - a\n\
            literal, a call result, and any other temporary name nothing to\n\
            write through. A destructuring assignment applies the same rule\n\
            to every element of its tuple; write `_` for an element to\n\
            discard.",
    ),
    (
        "GT0079",
        "A `Box<T>` / `Rc<T>` / `Arc<T>` wrapper was written. Every value is\n\
            heap-shared and reference-counted already, so the wrapper names no\n\
            choice the writer has to make. Write the inner type; `--fix`\n\
            strips the wrapper.",
    ),
    (
        "GT0080",
        "`String::parse` was called. The parse surface is `to_i64`,\n\
            `to_f64`, and `to_bool`: each parses the whole string and answers\n\
            an `Option<T>`. Add `.ok_or(..)` where a `Result` is wanted.\n\
            `--fix` rewrites the call.",
    ),
    (
        "GT0081",
        "An `enum Name : uN` named a width too narrow for its variants. A\n\
            declared width is exact - it is what the discriminant is stored\n\
            in - so it is never widened silently. Write a width that holds\n\
            every variant, or drop it and let the compiler pick.",
    ),
    (
        "GT0082",
        "A reference was passed where the parameter takes the value. A\n\
            parameter that is not `&mut` takes the value, and a reference\n\
            names one rather than being one, so the compiled tiers would\n\
            hand the callee the address. Write `*x`.",
    ),
    (
        "GT0083",
        "An assignment wrote through `*x` where `x` is not a reference. `*`\n\
            reaches the place a reference names, and a value has no such\n\
            place, so the write would reach nothing. Write the binding\n\
            directly, or make the parameter `&mut T` and pass `&mut` at the\n\
            call site.",
    ),
    (
        "GT0084",
        "An `impl` header named a trait the language supplies itself. Hashing,\n\
            copying, release, marker safety, and the `Into` / `TryInto` /\n\
            `IntoIterator` directions are decided by the language, not chosen\n\
            per type, so the block would declare a contract nothing dispatches\n\
            through and the behaviour would stay as it was. The diagnostic\n\
            names what the language does and what to write in the block's\n\
            place - a `From` impl on the target, a `defer`, an inherent\n\
            method - or remove the block.",
    ),
    (
        "GT0085",
        "A container that orders its elements as it stores them - a heap, a\n\
            `BTreeSet`, a `BTreeMap` - named an element or key whose type\n\
            writes its own `cmp`. Such a container keeps its elements in the\n\
            order they went in and reads them back with no comparator to call,\n\
            so the order the type declares would silently not be the order it\n\
            answers. A sequence orders on demand and does route through the\n\
            type's `cmp`: sort a `Vec<T>`, or key the container on a value\n\
            that carries the order.",
    ),
    (
        "GT0086",
        "A `spawn` was written in a function with no `cohort { }` of its own.\n\
            `spawn` attaches its child to the cohort the goroutine is inside at\n\
            that moment, which is dynamic state a signature says nothing\n\
            about: the child joins whatever cohort the caller happens to be\n\
            in, and the root cohort - whose extent is the process - when there\n\
            is none. Open a `cohort { }` around the spawns and the code that\n\
            collects from them. `main` is exempt: the root cohort's extent is\n\
            main's own, so a child there does not outlive the scope that owns\n\
            it.",
    ),
    (
        "GX0001",
        "An operation received a value of an incompatible type. The\n\
                     diagnostic names the type that was required and the type\n\
                     and value that were supplied. Add an explicit conversion\n\
                     or use operands of the same type.",
    ),
    (
        "GX0002",
        "A name resolved at parse/resolve time to nothing callable at\n\
                     runtime. Usually means a stdlib builtin is not wired into the\n\
                     execution path that reached the call.",
    ),
    (
        "GX0003",
        "A call supplied the wrong number of arguments for the callee's\n\
                     declared arity. Fix the call site or update the declaration.",
    ),
    (
        "GX0004",
        "A checked runtime bounds operation or numeric conversion could\n\
                     not produce a valid result. Integer division and modulo\n\
                     by zero are panics and use GX0005 instead.",
    ),
    (
        "GX0005",
        "Explicit `panic(...)` or an assertion failure aborted the\n\
                     program. Wrap the fallible operation in a `Result` path if the\n\
                     failure is recoverable.",
    ),
    (
        "GX0006",
        "A `match` expression failed to match any arm at runtime. The\n\
                     exhaustiveness checker catches most of these statically; a\n\
                     `GX0006` at runtime means a refinement check slipped through.",
    ),
    (
        "GX0007",
        "The execution path (interpreter or native) does not yet\n\
                     implement the construct reached. File the example and use\n\
                     the other path in the meantime.",
    ),
    (
        "GX0008",
        "The goroutine exceeded the VM's maximum call depth (40 frames).\n\
                     Each interpreted Gossamer frame adds a large pair of Rust stack\n\
                     frames (apply + run); the 8 MB OS thread stack can safely hold\n\
                     around 40 such pairs in a debug build before overflowing.\n\
                     Direct or mutual recursion without a reachable base case is the\n\
                     most common cause. Add a terminating condition, convert to an\n\
                     iterative loop, or use `gos build` where the native codegen\n\
                     produces standard call instructions the OS can grow to handle.",
    ),
    (
        "GX0009",
        "The configured execution budget was exhausted. This limit is used by\n\
                     fuel-enabled hosts such as the playground to stop unbounded\n\
                     programs. Reduce the work or fix the non-terminating loop.",
    ),
    (
        "GX0010",
        "A `comptime` region reached a capability its compile-time policy\n\
                     withholds. Compile-time evaluation runs with the privileges of\n\
                     whoever started the compile, so `--comptime-io` bounds what it\n\
                     may touch: `none` denies all I/O, `confined` (the default)\n\
                     permits reads under the source tree and denies writes, process\n\
                     spawn, network, and environment mutation, and `full` restores\n\
                     unrestricted evaluation. A project pins its own posture with\n\
                     `project.comptime-io` in `project.toml`; the toolchain takes\n\
                     the more restrictive of the manifest and the command line, so a\n\
                     manifest can tighten the posture and can never loosen it.\n\
                     Move the work out of the comptime region, read a file from\n\
                     inside the project, or pass `--comptime-io=full` deliberately.",
    ),
    (
        "GX0011",
        "A wait was entered that nothing can end on this build. The browser\n\
                     playground runs the VM on a single thread and settles every\n\
                     goroutine at its spawn, so a channel receive, a `WaitGroup`, or\n\
                     a `Barrier` that expects a goroutine still to run waits for one\n\
                     that has already finished. Send before receiving, size a barrier\n\
                     for the goroutines that actually arrive, or run the program with\n\
                     `gos`, where goroutines run concurrently.",
    ),
    (
        "GX0012",
        "The program called `process::exit`. A wasm module has no process of\n\
                     its own to end, so the browser playground stops the run and\n\
                     reports the status the program asked for. Native builds end the\n\
                     process and never raise this.",
    ),
];

/// Returns the explanation text for `code`, or `None` when the code
/// is not in the registry. Lookup is case-sensitive on the canonical
/// upper-case form; callers should pre-uppercase user input.
#[must_use]
pub fn explain(code: &str) -> Option<&'static str> {
    REGISTRY
        .binary_search_by_key(&code, |(k, _)| *k)
        .ok()
        .map(|i| REGISTRY[i].1)
}

/// Returns every code currently registered, in registry order.
pub fn codes() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|(k, _)| *k)
}
