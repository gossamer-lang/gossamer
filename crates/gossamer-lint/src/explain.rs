//! Long-form explanations shown by `gos lint --explain <id>`.

/// Returns a multi-line explanation of `id`, or `None` when the lint
/// is unknown.
#[must_use]
pub fn lint_explanation(id: &str) -> Option<&'static str> {
    Some(match id {
        "unused_variable" => {
            "Declares a `let` binding whose name is never read.\n\
            Prefix the name with `_` to silence explicitly (e.g. `_tmp`)\n\
            when the binding is intentional but unused."
        }
        "unused_import" => {
            "A `use` declaration whose imported name is never referenced.\n\
            Remove the import or reference the name in the file."
        }
        "needless_return" => {
            "`return expr` at the tail of a block is the same as writing `expr`\n\
            by itself. Prefer the tail form for symmetry with the rest of\n\
            the expression language."
        }
        "needless_bool" => {
            "`if cond { true } else { false }` is the same as `cond`. The\n\
            inverted form is `!cond`."
        }
        "comparison_to_bool_literal" => {
            "`x == true` reads worse than `x`, and `x == false` reads worse\n\
            than `!x`. Drop the literal."
        }
        "single_match" => {
            "A `match` with a single arm reads better as `if let PATTERN = ...`.\n\
            Single-arm `match` is almost always a half-written exhaustive match."
        }
        "shadowed_binding" => {
            "A `let` binding in the same block shadows an earlier one. Rename\n\
            one of them to make the data flow obvious."
        }
        "unchecked_result" => {
            "`let _ = expr?` silently discards an error. Either handle the\n\
            `Err` branch explicitly or propagate with `?` so the caller sees it."
        }
        "string_concat_in_loop" => {
            "Repeated `s += ...` inside a loop allocates a fresh buffer on\n\
            each iteration. Collect into `bytes::Builder` (or pre-size with\n\
            `String::with_capacity`) and commit once at the end."
        }
        "empty_block" => {
            "An empty `{}` block is almost always a mistake. Add an explicit\n\
            `()` tail if the block is intentional."
        }
        "panic_in_main" => {
            "`panic!` inside `main` aborts without a clean exit code. Return a\n\
            `Result` from `main` and use `?` so the error propagates."
        }
        "redundant_clone" => {
            "Calling `.clone()` on a literal or already-copied value is\n\
            redundant. Drop the call."
        }
        "double_negation" => {
            "`!!x` collapses to `x` when `x: bool`. If the double negation is\n\
            intentional for truthiness coercion, use an explicit cast."
        }
        "self_assignment" => {
            "Assigning a variable to itself does nothing. The statement is\n\
            usually the residue of a refactor - remove it."
        }
        "unused_mut_variable" => {
            "A binding marked `mut` that is never reassigned. Drop the `mut`\n\
            keyword."
        }
        "todo_macro" => {
            "`todo()` and `unimplemented()` are placeholders, not shippable\n\
            expressions. Implement the branch before merging."
        }
        "bool_literal_in_condition" => {
            "`if true { ... }` / `while false { ... }` - the branch is\n\
            decided at compile time. Drop the control-flow construct."
        }
        "let_and_return" => {
            "`let x = expr; x` at the tail of a block is just `expr`. Drop\n\
            the needless binding."
        }
        "collapsible_if" => {
            "`if a { if b { ... } }` can be combined into\n\
            `if a && b { ... }`. Easier to scan."
        }
        "if_same_then_else" => {
            "Both branches of the `if` are identical. Drop the branch and\n\
            keep the body once."
        }
        "needless_else_after_return" => {
            "`if cond { return X } else { Y }` - the `else` is unreachable\n\
            fall-through. Un-nest the `else` body."
        }
        "self_compare" => {
            "Comparing a value to itself is always `true` (for `==`, `<=`,\n\
            `>=`) or `false` (for `!=`, `<`, `>`). Use the constant."
        }
        "identity_op" => {
            "`x + 0`, `x - 0`, `x * 1`, `x / 1` all equal `x`. The operation\n\
            adds nothing but noise."
        }
        "unit_let" => {
            "`let x = ()` binds the unit value, which is almost never useful.\n\
            Drop the `let`."
        }
        "float_eq_zero" => {
            "Equality against a float literal is almost never what you want -\n\
            floating-point arithmetic rarely produces the exact bit pattern.\n\
            Compare `(x - y).abs() < eps` with an explicit tolerance."
        }
        "empty_else" => {
            "`else {}` adds no information. Drop the else and let the `if`\n\
            stand alone."
        }
        "match_bool" => {
            "`match b { true => ... false => ... }` is an `if` in disguise.\n\
            Rewrite as `if b { ... } else { ... }`."
        }
        "needless_parens" => {
            "`(x)` without a trailing comma is a needless pair of parens -\n\
            `x` reads the same. `(x,)` is a one-tuple and means something\n\
            different. Closure parameters follow the same rule: write `|t|`,\n\
            not `|(t)|`."
        }
        "manual_not_equal" => "`!(a == b)` is just `a != b`. Prefer the direct operator.",
        "nested_ternary_if" => {
            "Three or more `if / else if` layers that all test one value\n\
            against a pattern are a `match` written the long way. Rewrite\n\
            them as `match` on that value. A chain whose arms test\n\
            unrelated conditions, or compare with `<` and `>`, is left\n\
            alone: `match` would only move those into guards."
        }
        "absurd_range" => {
            "A literal range whose lower bound exceeds its upper bound is\n\
            empty. Swap the bounds or double-check the intent."
        }
        "string_literal_concat" => {
            "`\"a\" + \"b\"` can be written directly as `\"ab\"`. Let the\n\
            source reflect the final value."
        }
        "chained_negation_literals" => "`-(-x)` is `x`. The extra unary does nothing.",
        "if_not_else" => {
            "`if !cond { A } else { B }` scans better as `if cond { B }\n\
            else { A }`. Flip the branches and drop the `!`."
        }
        "empty_string_concat" => {
            "Concatenating an empty string literal is a no-op. Drop the\n\
            `\"\" +` or `+ \"\"`."
        }
        "println_newline_only" => {
            "`println(\"\")` already writes a newline. Don't pass `\"\\n\"`\n\
            and don't call it twice to emit a blank line."
        }
        "match_same_arms" => {
            "Two match arms share the same body. Either collapse them with\n\
            `|` alternation or extract the shared body into a helper."
        }
        "manual_swap" => {
            "Three consecutive statements `let tmp = a; a = b; b = tmp` swap\n\
            two bindings via a temporary. Prefer a destructuring assignment\n\
            once the language supports it, or at minimum document why the\n\
            swap is needed."
        }
        "consecutive_assignment" => {
            "Two back-to-back assignments to the same place - the earlier\n\
            value is dead before it's read. Drop the first or consolidate\n\
            the logic into one statement."
        }
        "large_unreadable_literal" => {
            "Integer literals of five or more digits are easier to scan with\n\
            `_` as thousands separators: `1_000_000` instead of `1000000`."
        }
        "redundant_closure" => {
            "`|x| f(x)` is a closure that just forwards to `f`. Pass `f`\n\
            directly."
        }
        "empty_if_body" => {
            "An `if cond { } else { body }` is the same as `if !cond { body }`.\n\
            Invert the condition and drop the empty branch."
        }
        "bool_to_int_match" => {
            "`match b { true => 1, false => 0 }` is an `if` in disguise that\n\
            happens to return an integer. Prefer `if b { 1 } else { 0 }`."
        }
        "fn_returns_unit_explicit" => {
            "`fn f() -> () { ... }` is the same as `fn f() { ... }`. The\n\
            explicit `-> ()` is noise."
        }
        "let_with_unit_type" => {
            "`let _: () = expr` annotates the binding with the unit type. If\n\
            `expr` was going to return `()` anyway, the annotation is noise.\n\
            If it wasn't, the annotation forces a coercion - use a plain\n\
            statement instead."
        }
        "useless_default_only_match" => {
            "`match x { _ => expr }` always runs `expr` - the `match` adds\n\
            nothing. Drop the `match` (and add `let _ = x` if evaluating\n\
            the scrutinee has side effects)."
        }
        "unnecessary_parens_in_condition" => {
            "`if (cond) { ... }` wraps the condition in a single-tuple\n\
            expression. Drop the parens: `if cond { ... }`."
        }
        "pattern_matching_unit" => {
            "`match () { ... }` has exactly one reachable arm. Drop the match\n\
            and run the body directly."
        }
        "panic_without_message" => {
            "`panic()` with no argument leaves the post-mortem with nothing\n\
            to render. Always pass a brief explanation."
        }
        "empty_loop" => {
            "`loop {}` with no body busy-waits forever at 100% CPU. Add a\n\
            `break`, a `continue`, or replace with a real wait primitive."
        }
        "fill_loop" => {
            "A `for _ in 0..n { xs.push(value) }` loop that pushes the same\n\
            literal every iteration is the repeat literal spelled long:\n\
            `let xs = [value; n]` builds the identical vec in one expression\n\
            (byte-packed for `bool` / `u8` elements)."
        }
        "substring_byte_scan" => {
            "`s.substring(i, i + 1)` allocates a String per step of a scan.\n\
            `substring` takes byte offsets, so `s.byte_at(i)` reads the same\n\
            position directly as an `i64`: compare with byte literals\n\
            (`s.byte_at(i) >= b'0'`), do arithmetic on it\n\
            (`s.byte_at(i) - b'0'`), and render with `s.byte_at(i) as char`.\n\
            `s[i]` is not that byte: indexing counts Unicode scalars, so it\n\
            yields the `char` at character index `i`."
        }
        "i64_only_container_family" => {
            "`std::collections::{queue,stack,deque,heap,ordered_*}` hold\n\
            `i64` only and return a new container from every mutator, which\n\
            duplicates `Queue`, `Stack`, `Deque`, `MinHeap`, `BTreeSet`, and\n\
            `BTreeMap` at a narrower element type.\n\
            Deliberately not auto-fixable: the two families differ in more\n\
            than spelling. `peek` answers `0` on an empty container where the\n\
            type answers `None`, so a mechanical rewrite would keep\n\
            type-checking and change what an empty container means."
        }
        "no_effect_statement" => {
            "An expression statement that only computes - a path, a literal,\n\
            arithmetic over them - and hands its value to nothing.\n\
            A newline followed by `-`, `*`, or `&` starts a NEW statement, so\n\
            a continuation meant as part of the line above becomes one of\n\
            these. Join it to that line (end the previous line with the\n\
            operator, or parenthesise), or bind what it computes."
        }
        _ => return None,
    })
}
