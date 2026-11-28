#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

pub const MATH_RAND: StdModule = StdModule {
    path: "std::math::rand",
    summary: "Deterministic pseudo-random number generation.",
    items: &[
        StdItem {
            name: "Rng",
            kind: StdItemKind::Type,
            doc: "SplitMix64-based RNG.",
        },
        StdItem {
            name: "range",
            kind: StdItemKind::Function,
            doc: "`range(rng, low, high) -> i64`: a uniform integer in `low..high`, unbiased (Go's `rand.IntN`, Python's `randrange`).",
        },
        StdItem {
            name: "range_f64",
            kind: StdItemKind::Function,
            doc: "`range_f64(rng, low, high) -> f64`: a uniform float in `low..high`.",
        },
        StdItem {
            name: "chance",
            kind: StdItemKind::Function,
            doc: "`chance(rng, p) -> bool`: true with probability `p`.",
        },
        StdItem {
            name: "choose",
            kind: StdItemKind::Function,
            doc: "`choose(rng, xs) -> Option<T>`: one element at random; `None` for an empty sequence.",
        },
        StdItem {
            name: "shuffle",
            kind: StdItemKind::Function,
            doc: "`shuffle(rng, &mut xs)`: puts `xs` in a uniformly random order (Fisher-Yates).",
        },
        StdItem {
            name: "sample",
            kind: StdItemKind::Function,
            doc: "`sample(rng, xs, k) -> Vec<T>`: `k` distinct elements at random, in the order drawn (Python's `random.sample`).",
        },
        StdItem {
            name: "weighted",
            kind: StdItemKind::Function,
            doc: "`weighted(rng, weights) -> Option<i64>`: an index drawn with probability proportional to its weight; `None` when no weight is positive or one is negative.",
        },
        StdItem {
            name: "normal",
            kind: StdItemKind::Function,
            doc: "`normal(rng, mean, stdev) -> f64`: a normally distributed value (Box-Muller).",
        },
        StdItem {
            name: "exponential",
            kind: StdItemKind::Function,
            doc: "`exponential(rng, rate) -> f64`: an exponentially distributed value with the given rate.",
        },
        StdItem {
            name: "from_os",
            kind: StdItemKind::Function,
            doc: "`from_os() -> Result<Rng, errors::Error>`: a generator seeded from the host's random source.",
        },
    ],
};

pub const MATH: StdModule = StdModule {
    path: "std::math",
    summary: "Mathematical constants and f64 functions (Go's math package shape).",
    items: &[
        StdItem {
            name: "mean",
            kind: StdItemKind::Function,
            doc: "`mean(xs) -> Option<f64>`: the arithmetic mean, with a compensated sum; `None` for no data.",
        },
        StdItem {
            name: "median",
            kind: StdItemKind::Function,
            doc: "`median(xs) -> Option<f64>`: the middle value, or the mean of the two middle values; `None` for no data.",
        },
        StdItem {
            name: "variance",
            kind: StdItemKind::Function,
            doc: "`variance(xs) -> Option<f64>`: the sample variance (divides by n - 1), two-pass; `None` below two values.",
        },
        StdItem {
            name: "stdev",
            kind: StdItemKind::Function,
            doc: "`stdev(xs) -> Option<f64>`: the sample standard deviation.",
        },
        StdItem {
            name: "pvariance",
            kind: StdItemKind::Function,
            doc: "`pvariance(xs) -> Option<f64>`: the population variance (divides by n).",
        },
        StdItem {
            name: "pstdev",
            kind: StdItemKind::Function,
            doc: "`pstdev(xs) -> Option<f64>`: the population standard deviation.",
        },
        StdItem {
            name: "quantiles",
            kind: StdItemKind::Function,
            doc: "`quantiles(xs, n = 4) -> Option<Vec<f64>>`: the n - 1 cut points dividing the data into n groups, Python's exclusive method; `None` below two values.",
        },
        StdItem {
            name: "erf",
            kind: StdItemKind::Function,
            doc: "`erf(x) -> f64`: the error function.",
        },
        StdItem {
            name: "erfc",
            kind: StdItemKind::Function,
            doc: "`erfc(x) -> f64`: the complementary error function, `1 - erf(x)` without its cancellation.",
        },
        StdItem {
            name: "gamma",
            kind: StdItemKind::Function,
            doc: "`gamma(x) -> f64`: the gamma function; exact for whole numbers up to 21, NaN at zero and the negative integers.",
        },
        StdItem {
            name: "lgamma",
            kind: StdItemKind::Function,
            doc: "`lgamma(x) -> f64`: the natural logarithm of the absolute value of the gamma function.",
        },
        StdItem {
            name: "factorial",
            kind: StdItemKind::Function,
            doc: "`factorial(n) -> i64`: n!, panicking past `i64::MAX`.",
        },
        StdItem {
            name: "comb",
            kind: StdItemKind::Function,
            doc: "`comb(n, k) -> i64`: the number of ways to choose k of n without order.",
        },
        StdItem {
            name: "perm",
            kind: StdItemKind::Function,
            doc: "`perm(n, k) -> i64`: the number of ways to choose k of n in order.",
        },
        StdItem {
            name: "Complex",
            kind: StdItemKind::Type,
            doc: "A complex number of two `f64` parts: `Complex::new(re, im)`, `from_polar(r, theta)`, `+ - * /` and unary `-`, `==`, `abs` (the modulus), `arg`, `conj`, `exp`, `ln`, `sqrt`, `powf`. Prints as `1+2i`.",
        },
        StdItem {
            name: "Decimal",
            kind: StdItemKind::Type,
            doc: "Exact decimal number (`0.1 + 0.2 == 0.3`): `Decimal::parse(text)` (a `Result`), `Decimal::from_i64(n)`, `+ - * /` and unary `-`, comparison and ordering, `round(digits)` and `div_scaled(other, digits)` rounding half to even, `abs`, `is_zero`, `to_f64`; `/` keeps 28 digits after the point. Prints in its shortest exact form.",
        },
        StdItem {
            name: "Rational",
            kind: StdItemKind::Type,
            doc: "Exact rational number: `Rational::new(numerator, denominator)` over `big::Int`s, `from_i64`, `from_int`, `from_decimal`, `from_f64(x)` (the float's exact value; an `Err` for NaN and the infinities), and `Rational::parse(text)`, which reads `\"3/4\"`, `\"-3/4\"`, and decimal text such as `\"1.25\"` (a `Result`). `+ - * /` and unary `-`, comparison and ordering, `numerator()` and `denominator()` (lowest terms, the denominator positive; both grow without bound under arithmetic), `abs`, `recip`, `is_zero`, `is_integer`, and `to_f64`, the nearest float with ties to even. A zero denominator, from `new`, `/`, or `recip`, panics as integer division by zero does. Prints as `n/d`, or `n` when the denominator is 1.",
        },
        StdItem {
            name: "PI",
            kind: StdItemKind::Const,
            doc: "Archimedes' constant π.",
        },
        StdItem {
            name: "E",
            kind: StdItemKind::Const,
            doc: "Euler's number e.",
        },
        StdItem {
            name: "SQRT_2",
            kind: StdItemKind::Const,
            doc: "√2.",
        },
        StdItem {
            name: "LN_2",
            kind: StdItemKind::Const,
            doc: "Natural log of 2.",
        },
        StdItem {
            name: "LN_10",
            kind: StdItemKind::Const,
            doc: "Natural log of 10.",
        },
        StdItem {
            name: "PHI",
            kind: StdItemKind::Const,
            doc: "Golden ratio φ.",
        },
        StdItem {
            name: "INF",
            kind: StdItemKind::Const,
            doc: "Positive infinity.",
        },
        StdItem {
            name: "NAN",
            kind: StdItemKind::Const,
            doc: "Not-a-number value.",
        },
        StdItem {
            name: "abs",
            kind: StdItemKind::Function,
            doc: "Absolute value of x.",
        },
        StdItem {
            name: "sqrt",
            kind: StdItemKind::Function,
            doc: "Square root.",
        },
        StdItem {
            name: "cbrt",
            kind: StdItemKind::Function,
            doc: "Cube root.",
        },
        StdItem {
            name: "floor",
            kind: StdItemKind::Function,
            doc: "Largest integer ≤ x.",
        },
        StdItem {
            name: "ceil",
            kind: StdItemKind::Function,
            doc: "Smallest integer ≥ x.",
        },
        StdItem {
            name: "round",
            kind: StdItemKind::Function,
            doc: "Nearest integer, half away from zero.",
        },
        StdItem {
            name: "trunc",
            kind: StdItemKind::Function,
            doc: "Integer part of x.",
        },
        StdItem {
            name: "sin",
            kind: StdItemKind::Function,
            doc: "Sine (radians).",
        },
        StdItem {
            name: "cos",
            kind: StdItemKind::Function,
            doc: "Cosine (radians).",
        },
        StdItem {
            name: "tan",
            kind: StdItemKind::Function,
            doc: "Tangent (radians).",
        },
        StdItem {
            name: "asin",
            kind: StdItemKind::Function,
            doc: "Arcsine (radians).",
        },
        StdItem {
            name: "acos",
            kind: StdItemKind::Function,
            doc: "Arccosine (radians).",
        },
        StdItem {
            name: "atan",
            kind: StdItemKind::Function,
            doc: "Arctangent (radians).",
        },
        StdItem {
            name: "atan2",
            kind: StdItemKind::Function,
            doc: "Four-quadrant arctangent of y/x.",
        },
        StdItem {
            name: "exp",
            kind: StdItemKind::Function,
            doc: "e^x.",
        },
        StdItem {
            name: "exp2",
            kind: StdItemKind::Function,
            doc: "2^x.",
        },
        StdItem {
            name: "ln",
            kind: StdItemKind::Function,
            doc: "Natural logarithm.",
        },
        StdItem {
            name: "log2",
            kind: StdItemKind::Function,
            doc: "Base-2 logarithm.",
        },
        StdItem {
            name: "log10",
            kind: StdItemKind::Function,
            doc: "Base-10 logarithm.",
        },
        StdItem {
            name: "log",
            kind: StdItemKind::Function,
            doc: "Logarithm with given base.",
        },
        StdItem {
            name: "pow",
            kind: StdItemKind::Function,
            doc: "x raised to the power y.",
        },
        StdItem {
            name: "hypot",
            kind: StdItemKind::Function,
            doc: "Euclidean distance √(x²+y²).",
        },
        StdItem {
            name: "rem",
            kind: StdItemKind::Function,
            doc: "Floating-point remainder x%y.",
        },
        StdItem {
            name: "is_nan",
            kind: StdItemKind::Function,
            doc: "Reports whether x is NaN.",
        },
        StdItem {
            name: "is_inf",
            kind: StdItemKind::Function,
            doc: "Reports whether x is infinite.",
        },
        StdItem {
            name: "copysign",
            kind: StdItemKind::Function,
            doc: "Magnitude of x with sign of y.",
        },
        StdItem {
            name: "positive_diff",
            kind: StdItemKind::Function,
            doc: "max(x-y, 0).",
        },
        StdItem {
            name: "sinh",
            kind: StdItemKind::Function,
            doc: "Hyperbolic sine.",
        },
        StdItem {
            name: "cosh",
            kind: StdItemKind::Function,
            doc: "Hyperbolic cosine.",
        },
        StdItem {
            name: "tanh",
            kind: StdItemKind::Function,
            doc: "Hyperbolic tangent.",
        },
        StdItem {
            name: "min",
            kind: StdItemKind::Function,
            doc: "Lesser of two values.",
        },
        StdItem {
            name: "max",
            kind: StdItemKind::Function,
            doc: "Greater of two values.",
        },
        StdItem {
            name: "clamp",
            kind: StdItemKind::Function,
            doc: "Constrain x to the inclusive range [lo, hi].",
        },
        StdItem {
            name: "LOG2_E",
            kind: StdItemKind::Const,
            doc: "Base-2 logarithm of e.",
        },
        StdItem {
            name: "LOG10_E",
            kind: StdItemKind::Const,
            doc: "Base-10 logarithm of e.",
        },
        StdItem {
            name: "MAX_F64",
            kind: StdItemKind::Const,
            doc: "Largest finite f64 value.",
        },
        StdItem {
            name: "MIN_POSITIVE_F64",
            kind: StdItemKind::Const,
            doc: "Smallest positive normal f64 value.",
        },
        StdItem {
            name: "NEG_INF",
            kind: StdItemKind::Const,
            doc: "Negative infinity.",
        },
    ],
};

pub const MATH_BITS: StdModule = StdModule {
    path: "std::math::bits",
    summary: "Integer bit-manipulation operations (Go's math/bits shape).",
    items: &[
        StdItem {
            name: "count_ones",
            kind: StdItemKind::Function,
            doc: "Number of set bits (popcount).",
        },
        StdItem {
            name: "count_zeros",
            kind: StdItemKind::Function,
            doc: "Number of clear bits.",
        },
        StdItem {
            name: "leading_zeros",
            kind: StdItemKind::Function,
            doc: "Leading zero bit count.",
        },
        StdItem {
            name: "trailing_zeros",
            kind: StdItemKind::Function,
            doc: "Trailing zero bit count.",
        },
        StdItem {
            name: "rotate_left",
            kind: StdItemKind::Function,
            doc: "Rotates x left by n bits.",
        },
        StdItem {
            name: "rotate_right",
            kind: StdItemKind::Function,
            doc: "Rotates x right by n bits.",
        },
        StdItem {
            name: "reverse_bits",
            kind: StdItemKind::Function,
            doc: "Reverses bit order of x.",
        },
        StdItem {
            name: "reverse_bytes",
            kind: StdItemKind::Function,
            doc: "Reverses byte order of x.",
        },
        StdItem {
            name: "len",
            kind: StdItemKind::Function,
            doc: "Minimum bits required to represent x.",
        },
        StdItem {
            name: "add",
            kind: StdItemKind::Function,
            doc: "x + y + carry; returns (sum, carry_out).",
        },
        StdItem {
            name: "sub",
            kind: StdItemKind::Function,
            doc: "x - y - borrow; returns (diff, borrow_out).",
        },
        StdItem {
            name: "mul",
            kind: StdItemKind::Function,
            doc: "Full 128-bit product; returns (hi, lo).",
        },
        StdItem {
            name: "div",
            kind: StdItemKind::Function,
            doc: "128-bit dividend / 64-bit divisor; returns (quotient, remainder).",
        },
    ],
};

pub const ITER: StdModule = StdModule {
    path: "std::iter",
    summary: "Sequence adapters: map, filter, fold, zip, enumerate, chain, etc. \
                A `Vec` argument is traversed eagerly; an `Iterator` argument keeps \
                the adapter lazy and answers with another iterator.",
    items: &[
        StdItem {
            name: "count",
            kind: StdItemKind::Function,
            doc: "Number of elements.",
        },
        StdItem {
            name: "collect",
            kind: StdItemKind::Function,
            doc: "Materializes a sequence into a Vec.",
        },
        StdItem {
            name: "once",
            kind: StdItemKind::Function,
            doc: "Single-element Vec containing value.",
        },
        StdItem {
            name: "empty",
            kind: StdItemKind::Function,
            doc: "Empty Vec.",
        },
        StdItem {
            name: "take",
            kind: StdItemKind::Function,
            doc: "First n elements.",
        },
        StdItem {
            name: "skip",
            kind: StdItemKind::Function,
            doc: "All elements after the first n.",
        },
        StdItem {
            name: "step_by",
            kind: StdItemKind::Function,
            doc: "Every step-th element, starting at index 0.",
        },
        StdItem {
            name: "zip",
            kind: StdItemKind::Function,
            doc: "Pairs elements from two sequences.",
        },
        StdItem {
            name: "enumerate",
            kind: StdItemKind::Function,
            doc: "Pairs each element with its index.",
        },
        StdItem {
            name: "chain",
            kind: StdItemKind::Function,
            doc: "Concatenates two sequences.",
        },
        StdItem {
            name: "flatten",
            kind: StdItemKind::Function,
            doc: "Flattens a Vec<Vec<T>> into Vec<T>.",
        },
        StdItem {
            name: "rev",
            kind: StdItemKind::Function,
            doc: "Returns the elements in reverse order.",
        },
        StdItem {
            name: "dedup",
            kind: StdItemKind::Function,
            doc: "Removes consecutive duplicate elements.",
        },
        StdItem {
            name: "map",
            kind: StdItemKind::Function,
            doc: "Applies f to each element. Returns a Vec for a Vec argument, \
                   and another iterator for an iterator argument.",
        },
        StdItem {
            name: "filter",
            kind: StdItemKind::Function,
            doc: "Returns elements where f is true.",
        },
        StdItem {
            name: "fold",
            kind: StdItemKind::Function,
            doc: "Reduces a sequence with an accumulator.",
        },
        StdItem {
            name: "flat_map",
            kind: StdItemKind::Function,
            doc: "Maps f and flattens one level.",
        },
        StdItem {
            name: "any",
            kind: StdItemKind::Function,
            doc: "True if any element satisfies f.",
        },
        StdItem {
            name: "all",
            kind: StdItemKind::Function,
            doc: "True if every element satisfies f.",
        },
        StdItem {
            name: "sum",
            kind: StdItemKind::Function,
            doc: "Sum of i64 or f64 elements.",
        },
        StdItem {
            name: "product",
            kind: StdItemKind::Function,
            doc: "Product of i64 or f64 elements.",
        },
        StdItem {
            name: "min",
            kind: StdItemKind::Function,
            doc: "Smallest element, or None when empty.",
        },
        StdItem {
            name: "max",
            kind: StdItemKind::Function,
            doc: "Largest element, or None when empty.",
        },
        StdItem {
            name: "range",
            kind: StdItemKind::Function,
            doc: "Half-open integer sequence [start, end).",
        },
        StdItem {
            name: "range_inclusive",
            kind: StdItemKind::Function,
            doc: "Closed integer sequence [start, end].",
        },
        StdItem {
            name: "repeat",
            kind: StdItemKind::Function,
            doc: "A value repeated n times.",
        },
        StdItem {
            name: "unzip",
            kind: StdItemKind::Function,
            doc: "Splits a sequence of pairs into two Vecs.",
        },
        StdItem {
            name: "windows",
            kind: StdItemKind::Function,
            doc: "Overlapping windows of width n.",
        },
        StdItem {
            name: "pairwise",
            kind: StdItemKind::Function,
            doc: "Consecutive overlapping pairs.",
        },
        StdItem {
            name: "chunks",
            kind: StdItemKind::Function,
            doc: "Non-overlapping chunks of length n.",
        },
        StdItem {
            name: "for_each",
            kind: StdItemKind::Function,
            doc: "Applies f to each element for its side effect.",
        },
        StdItem {
            name: "filter_map",
            kind: StdItemKind::Function,
            doc: "Maps each element and keeps the Some results.",
        },
        StdItem {
            name: "reduce",
            kind: StdItemKind::Function,
            doc: "Folds with the first element as the initial accumulator.",
        },
        StdItem {
            name: "scan",
            kind: StdItemKind::Function,
            doc: "Folds while yielding each intermediate accumulator.",
        },
        StdItem {
            name: "sum_by",
            kind: StdItemKind::Function,
            doc: "Sum of f(element) over the sequence.",
        },
        StdItem {
            name: "product_by",
            kind: StdItemKind::Function,
            doc: "Product of f(element) over the sequence.",
        },
        StdItem {
            name: "find",
            kind: StdItemKind::Function,
            doc: "First element satisfying f, or None.",
        },
        StdItem {
            name: "position",
            kind: StdItemKind::Function,
            doc: "Index of the first element satisfying f, or None.",
        },
        StdItem {
            name: "find_map",
            kind: StdItemKind::Function,
            doc: "First Some result of f over the sequence.",
        },
        StdItem {
            name: "take_while",
            kind: StdItemKind::Function,
            doc: "Leading run of elements satisfying f.",
        },
        StdItem {
            name: "skip_while",
            kind: StdItemKind::Function,
            doc: "Elements after the leading run satisfying f.",
        },
        StdItem {
            name: "partition",
            kind: StdItemKind::Function,
            doc: "Splits into (matching, non-matching) by f.",
        },
        StdItem {
            name: "sort_by",
            kind: StdItemKind::Function,
            doc: "Sorted copy ordered by the comparison closure.",
        },
        StdItem {
            name: "sort_by_key",
            kind: StdItemKind::Function,
            doc: "Sorted copy ordered by a derived key.",
        },
        StdItem {
            name: "min_by",
            kind: StdItemKind::Function,
            doc: "Smallest element by the comparison closure.",
        },
        StdItem {
            name: "max_by",
            kind: StdItemKind::Function,
            doc: "Largest element by the comparison closure.",
        },
        StdItem {
            name: "min_by_key",
            kind: StdItemKind::Function,
            doc: "Element with the smallest derived key.",
        },
        StdItem {
            name: "max_by_key",
            kind: StdItemKind::Function,
            doc: "Element with the largest derived key.",
        },
        StdItem {
            name: "chunk_by",
            kind: StdItemKind::Function,
            doc: "Groups elements into a map keyed by f.",
        },
        StdItem {
            name: "count_by",
            kind: StdItemKind::Function,
            doc: "Counts elements per key derived by f.",
        },
    ],
};

// ---------------------------------------------------------------------------
// 0.4.0 surface - HTTP/2, websocket, sse, router, middleware, static files,
// proxy, native client, chunked transfer, archives, extended compress,
// XML / base32 / ascii85, FNV, big-int, insecure / cipher crypto.
// ---------------------------------------------------------------------------

pub const MATH_BIG: StdModule = StdModule {
    path: "std::math::big",
    summary: "Arbitrary-precision integers (num-bigint).",
    items: &[
        StdItem {
            name: "Int",
            kind: StdItemKind::Type,
            doc: "Arbitrary-precision signed integer.",
        },
        StdItem {
            name: "Uint",
            kind: StdItemKind::Type,
            doc: "Arbitrary-precision unsigned integer.",
        },
        StdItem {
            name: "factorial",
            kind: StdItemKind::Function,
            doc: "Computes n! as an Int.",
        },
        StdItem {
            name: "int_from_str",
            kind: StdItemKind::Function,
            doc: "Parses a decimal string into an Int.",
        },
        StdItem {
            name: "int_from_i64",
            kind: StdItemKind::Function,
            doc: "Converts an i64 into an Int.",
        },
        StdItem {
            name: "int_to_str",
            kind: StdItemKind::Function,
            doc: "Decimal string form of an Int.",
        },
        StdItem {
            name: "int_to_hex",
            kind: StdItemKind::Function,
            doc: "Hexadecimal string form of an Int.",
        },
        StdItem {
            name: "int_to_i64",
            kind: StdItemKind::Function,
            doc: "Narrows an Int to i64 where it fits.",
        },
        StdItem {
            name: "int_is_zero",
            kind: StdItemKind::Function,
            doc: "Reports whether the Int is zero.",
        },
        StdItem {
            name: "int_is_positive",
            kind: StdItemKind::Function,
            doc: "Reports whether the Int is greater than zero.",
        },
        StdItem {
            name: "int_is_negative",
            kind: StdItemKind::Function,
            doc: "Reports whether the Int is less than zero.",
        },
        StdItem {
            name: "int_add",
            kind: StdItemKind::Function,
            doc: "Sum of two Ints.",
        },
        StdItem {
            name: "int_sub",
            kind: StdItemKind::Function,
            doc: "Difference of two Ints.",
        },
        StdItem {
            name: "int_mul",
            kind: StdItemKind::Function,
            doc: "Product of two Ints.",
        },
        StdItem {
            name: "int_div",
            kind: StdItemKind::Function,
            doc: "Truncated quotient of two Ints.",
        },
        StdItem {
            name: "int_rem",
            kind: StdItemKind::Function,
            doc: "Remainder of two Ints.",
        },
        StdItem {
            name: "int_pow",
            kind: StdItemKind::Function,
            doc: "Int raised to a non-negative power.",
        },
        StdItem {
            name: "int_abs",
            kind: StdItemKind::Function,
            doc: "Absolute value of an Int.",
        },
        StdItem {
            name: "int_neg",
            kind: StdItemKind::Function,
            doc: "Negation of an Int.",
        },
        StdItem {
            name: "int_gcd",
            kind: StdItemKind::Function,
            doc: "Greatest common divisor of two Ints.",
        },
        StdItem {
            name: "int_lcm",
            kind: StdItemKind::Function,
            doc: "Least common multiple of two Ints.",
        },
        StdItem {
            name: "int_cmp",
            kind: StdItemKind::Function,
            doc: "Three-way comparison of two Ints (-1, 0, 1).",
        },
        StdItem {
            name: "uint_from_str",
            kind: StdItemKind::Function,
            doc: "Parses a decimal string into a Uint.",
        },
        StdItem {
            name: "uint_from_u64",
            kind: StdItemKind::Function,
            doc: "Converts a u64 into a Uint.",
        },
        StdItem {
            name: "uint_to_str",
            kind: StdItemKind::Function,
            doc: "Decimal string form of a Uint.",
        },
        StdItem {
            name: "uint_to_hex",
            kind: StdItemKind::Function,
            doc: "Hexadecimal string form of a Uint.",
        },
        StdItem {
            name: "uint_to_u64",
            kind: StdItemKind::Function,
            doc: "Narrows a Uint to u64 where it fits.",
        },
        StdItem {
            name: "uint_is_zero",
            kind: StdItemKind::Function,
            doc: "Reports whether the Uint is zero.",
        },
        StdItem {
            name: "uint_add",
            kind: StdItemKind::Function,
            doc: "Sum of two Uints.",
        },
        StdItem {
            name: "uint_mul",
            kind: StdItemKind::Function,
            doc: "Product of two Uints.",
        },
        StdItem {
            name: "uint_pow",
            kind: StdItemKind::Function,
            doc: "Uint raised to a non-negative power.",
        },
        StdItem {
            name: "uint_pow_mod",
            kind: StdItemKind::Function,
            doc: "Modular exponentiation of a Uint.",
        },
        StdItem {
            name: "uint_bit_len",
            kind: StdItemKind::Function,
            doc: "Number of significant bits in a Uint.",
        },
    ],
};

/// `std::option` - data-last combinators that thread through `|>`.
pub const OPTION: StdModule = StdModule {
    path: "std::option",
    summary: "Option combinators as data-first free functions for pipelines: map, filter, unwrap_or, and_then, etc.",
    items: &[
        StdItem {
            name: "and_then",
            kind: StdItemKind::Function,
            doc: "Chains a fallible step: Some(v) -> f(v), None stays None.",
        },
        StdItem {
            name: "unwrap",
            kind: StdItemKind::Function,
            doc: "Returns the payload, panicking when the value is None.",
        },
        StdItem {
            name: "expect",
            kind: StdItemKind::Function,
            doc: "Returns the payload, panicking with the message when the value is None.",
        },
        StdItem {
            name: "unwrap_or",
            kind: StdItemKind::Function,
            doc: "Unwraps with a fallback value for None.",
        },
        StdItem {
            name: "unwrap_or_else",
            kind: StdItemKind::Function,
            doc: "Unwraps with a lazily computed fallback for None.",
        },
        StdItem {
            name: "filter",
            kind: StdItemKind::Function,
            doc: "Keeps Some(v) only when the predicate holds.",
        },
        StdItem {
            name: "flatten",
            kind: StdItemKind::Function,
            doc: "Collapses Option<Option<T>> one level.",
        },
        StdItem {
            name: "is_none",
            kind: StdItemKind::Function,
            doc: "True for None.",
        },
        StdItem {
            name: "is_some",
            kind: StdItemKind::Function,
            doc: "True for Some.",
        },
        StdItem {
            name: "ok_or",
            kind: StdItemKind::Function,
            doc: "Converts Some to Ok, or None to Err with the provided error.",
        },
        StdItem {
            name: "ok_or_else",
            kind: StdItemKind::Function,
            doc: "Converts Some to Ok, or None to Err from a fallback closure.",
        },
        StdItem {
            name: "iter",
            kind: StdItemKind::Function,
            doc: "Zero-or-one element sequence view.",
        },
        StdItem {
            name: "map",
            kind: StdItemKind::Function,
            doc: "Transforms the Some payload, None stays None.",
        },
        StdItem {
            name: "or",
            kind: StdItemKind::Function,
            doc: "First Some of self and the alternative.",
        },
        StdItem {
            name: "or_else",
            kind: StdItemKind::Function,
            doc: "First Some of self and a lazily built alternative.",
        },
        StdItem {
            name: "zip",
            kind: StdItemKind::Function,
            doc: "Pairs two Somes into Some((a, b)).",
        },
        StdItem {
            name: "is_some_and",
            kind: StdItemKind::Function,
            doc: "Whether the option is Some and its payload passes the predicate.",
        },
        StdItem {
            name: "is_none_or",
            kind: StdItemKind::Function,
            doc: "Whether the option is None or its payload passes the predicate.",
        },
        StdItem {
            name: "map_or",
            kind: StdItemKind::Function,
            doc: "The payload mapped by f, or the default for None.",
        },
        StdItem {
            name: "map_or_else",
            kind: StdItemKind::Function,
            doc: "The payload mapped by f, or the default built by a callback for None.",
        },
    ],
};

/// `std::result` - data-last combinators that thread through `|>`.
pub const RESULT: StdModule = StdModule {
    path: "std::result",
    summary: "Result combinators as data-first free functions for pipelines: map, map_err, unwrap_or_else, etc.",
    items: &[
        StdItem {
            name: "and_then",
            kind: StdItemKind::Function,
            doc: "Chains a fallible step on the Ok payload.",
        },
        StdItem {
            name: "unwrap_or",
            kind: StdItemKind::Function,
            doc: "Unwraps Ok with a fallback value for Err.",
        },
        StdItem {
            name: "unwrap_or_else",
            kind: StdItemKind::Function,
            doc: "Consumes the result, handling Err with a callback.",
        },
        StdItem {
            name: "err",
            kind: StdItemKind::Function,
            doc: "Err payload as an Option.",
        },
        StdItem {
            name: "is_err",
            kind: StdItemKind::Function,
            doc: "True for Err.",
        },
        StdItem {
            name: "is_ok",
            kind: StdItemKind::Function,
            doc: "True for Ok.",
        },
        StdItem {
            name: "map",
            kind: StdItemKind::Function,
            doc: "Transforms the Ok payload, Err passes through.",
        },
        StdItem {
            name: "map_err",
            kind: StdItemKind::Function,
            doc: "Transforms the Err payload, Ok passes through.",
        },
        StdItem {
            name: "ok",
            kind: StdItemKind::Function,
            doc: "Ok payload as an Option.",
        },
        StdItem {
            name: "or_else",
            kind: StdItemKind::Function,
            doc: "Recovers from Err with a fallback computation.",
        },
        StdItem {
            name: "is_ok_and",
            kind: StdItemKind::Function,
            doc: "Whether the result is Ok and its value passes the predicate.",
        },
        StdItem {
            name: "is_err_and",
            kind: StdItemKind::Function,
            doc: "Whether the result is Err and its error passes the predicate.",
        },
        StdItem {
            name: "map_or",
            kind: StdItemKind::Function,
            doc: "The Ok value mapped by f, or the default for Err.",
        },
        StdItem {
            name: "map_or_else",
            kind: StdItemKind::Function,
            doc: "The Ok value mapped by f, or the error mapped by the default callback.",
        },
    ],
};
