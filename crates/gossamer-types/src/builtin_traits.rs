//! The catalog of traits the language itself knows: what each one governs,
//! the method an `impl` supplies, and whether writing that `impl` is how the
//! behaviour is chosen at all.
//!
//! One table answers every question about a built-in trait - what an `impl`
//! header may name, what items that block may define, what a bound licenses
//! on a type parameter, and what `%info` prints - so discovery and type
//! checking can never disagree about the surface.

#![forbid(unsafe_code)]

/// How the language decides the behaviour a trait names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinTraitKind {
    /// The behaviour is synthesized, and an `impl` block replaces it.
    Overridable,
    /// The behaviour is an operator's meaning, which exists for a user type
    /// only once an `impl` supplies it.
    Operator,
    /// The language supplies the behaviour outright, so an `impl` block would
    /// name a contract nothing dispatches through. [`BuiltinTrait::instead`]
    /// carries what to write in its place.
    Automatic,
}

/// One trait the language knows by name.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinTrait {
    /// The name an `impl` header and a generic bound spell.
    pub name: &'static str,
    /// How the behaviour is decided.
    pub kind: BuiltinTraitKind,
    /// The manifest module that declares it, for a trait the standard
    /// library exports rather than the language supplying bare.
    pub module: Option<&'static str>,
    /// Items an `impl` block of this trait may define. The rendering in
    /// [`BuiltinTrait::signature`] shows the required one.
    pub impl_items: &'static [&'static str],
    /// Methods a bound naming this trait licenses on a type parameter.
    pub bound_methods: &'static [&'static str],
    /// The contract, rendered as the block an `impl` writes. Empty for a
    /// trait no `impl` supplies.
    pub signature: &'static str,
    /// What the trait governs, in one or two sentences.
    pub doc: &'static str,
    /// What to write instead, for an `Automatic` trait.
    pub instead: &'static str,
    /// A line that exercises the trait, for `%info -d`.
    pub example: &'static str,
}

const EQ_METHODS: &[&str] = &["eq", "ne"];
const ORD_METHODS: &[&str] = &["cmp", "partial_cmp"];
const FMT_METHODS: &[&str] = &["fmt", "to_string"];

/// The `Iterator<T>` method surface available on every execution tier: what a
/// bound naming `Iterator` licenses on a type parameter, and what an iterator
/// receiver accepts.
pub const ITERATOR_BOUND_METHODS: &[&str] = &[
    "next",
    "take",
    "skip",
    "step_by",
    "enumerate",
    "chain",
    "zip",
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "scan",
    "take_while",
    "skip_while",
    "rev",
    "dedup",
    "flatten",
    "pairwise",
    "windows",
    "chunks",
    "collect",
    "count",
    "sum",
    "product",
    "min",
    "max",
    "fold",
    "any",
    "all",
    "find",
    // Terminals and eager-only operations. An iterator argument is legal for
    // these too: the eager ones drain it first, which is what a sequence
    // operation over an iterator has to do anyway.
    "find_map",
    "for_each",
    "position",
    "reduce",
    "partition",
    "unzip",
    "sort_by",
    "sort_by_key",
    "min_by",
    "min_by_key",
    "max_by",
    "max_by_key",
    "sum_by",
    "product_by",
    "chunk_by",
    "count_by",
];

/// Every trait an `impl` header or a generic bound may name without a
/// declaration in the checked source.
pub const BUILTIN_TRAITS: &[BuiltinTrait] = &[
    BuiltinTrait {
        name: "Display",
        kind: BuiltinTraitKind::Overridable,
        module: Some("std::fmt"),
        // Post-normalization: `impl Display`'s `fn fmt` is renamed to the
        // name the `{}` channel dispatches on before the checker sees it.
        impl_items: &["to_string"],
        bound_methods: FMT_METHODS,
        signature: "{ fn fmt(&self) -> String }",
        doc: "How a value renders through `{}`. Every type renders without one; \
              an `impl` replaces that rendering everywhere the value is shown, \
              including inside a `Vec`, `Map`, tuple, `Option`, or struct field, \
              and `x.to_string()` reaches it.",
        instead: "",
        example: "impl Display for Point { fn fmt(&self) -> String { format(\"({}, {})\", self.x, self.y) } }",
    },
    BuiltinTrait {
        name: "Debug",
        kind: BuiltinTraitKind::Overridable,
        module: Some("std::fmt"),
        impl_items: &["fmt"],
        bound_methods: FMT_METHODS,
        signature: "{ fn fmt(&self) -> String }",
        doc: "How a value renders through `{:?}`. Independent of `Display`: a \
              type that implements one keeps the synthesized rendering on the \
              other channel.",
        instead: "",
        example: "impl Debug for Point { fn fmt(&self) -> String { format(\"Point[{}]\", self.x) } }",
    },
    BuiltinTrait {
        name: "PartialEq",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: EQ_METHODS,
        bound_methods: EQ_METHODS,
        signature: "{ fn eq(&self, other: Self) -> bool }",
        doc: "What `==` and `!=` answer. Structs, enums, tuples, and sequences \
              compare field by field with no `impl`; one written here replaces \
              that comparison.",
        instead: "",
        example: "impl PartialEq for Point { fn eq(&self, other: Point) -> bool { self.x == other.x } }",
    },
    BuiltinTrait {
        name: "Eq",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: EQ_METHODS,
        bound_methods: EQ_METHODS,
        signature: "{ fn eq(&self, other: Self) -> bool }",
        doc: "The `PartialEq` contract under its total-equality spelling; both \
              names reach the same `eq`. Usable as a bound where a key or an \
              element has to compare.",
        instead: "",
        example: "fn first_of<T: Eq>(xs: Vec<T>, needle: T) -> Option<i64> { xs.position(|v| v == needle) }",
    },
    BuiltinTrait {
        name: "PartialOrd",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: ORD_METHODS,
        bound_methods: ORD_METHODS,
        signature: "{ fn cmp(&self, other: Self) -> i64 }",
        doc: "What `<`, `<=`, `>`, and `>=` answer: negative when the receiver \
              orders first, zero when the two tie, positive otherwise. Values \
              compare lexicographically by declaration order with no `impl`.",
        instead: "",
        example: "impl PartialOrd for Point { fn cmp(&self, other: Point) -> i64 { self.x - other.x } }",
    },
    BuiltinTrait {
        name: "Ord",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: ORD_METHODS,
        bound_methods: ORD_METHODS,
        signature: "{ fn cmp(&self, other: Self) -> i64 }",
        doc: "The `PartialOrd` contract under its total-order spelling; both \
              names reach the same `cmp`, and a sequence's `sort`, `min`, \
              `max`, and sorted-sequence searches all read it, and a \
              `BTreeSet` or `BTreeMap` seats its entries by it. A heap \
              orders as it stores, with no comparator to call, so it \
              declines such an element (GT0085).",
        instead: "",
        example: "impl Ord for Point { fn cmp(&self, other: Point) -> i64 { self.x - other.x } }",
    },
    BuiltinTrait {
        name: "Clone",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: &["clone"],
        bound_methods: &["clone"],
        signature: "{ fn clone(&self) -> Self }",
        doc: "What `x.clone()` answers. Every value already clones field by \
              field; an `impl` replaces that copy for the type.",
        instead: "",
        example: "impl Clone for Point { fn clone(&self) -> Point { Point { x: self.x, y: self.y } } }",
    },
    BuiltinTrait {
        name: "Default",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: &["default"],
        bound_methods: &["default"],
        signature: "{ fn default() -> Self }",
        doc: "The value `T::default()` answers. `#[derive(Default)]` synthesizes \
              one from the fields' own defaults, with `#[default]` picking an \
              enum's variant; an `impl` writes it directly.",
        instead: "",
        example: "impl Default for Point { fn default() -> Point { Point { x: 0, y: 0 } } }",
    },
    BuiltinTrait {
        name: "Iterator",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: ITERATOR_BOUND_METHODS,
        bound_methods: ITERATOR_BOUND_METHODS,
        signature: "{ fn next(&mut self) -> Option<T> }",
        doc: "Makes a type walkable: `for v in value` drives `next` until it \
              answers `None`. Any type with that method works in a `for`, and a \
              bound naming `Iterator` licenses the adapter surface.",
        instead: "",
        example: "impl Iterator for Countdown { fn next(&mut self) -> Option<i64> { if self.n == 0 { None } else { self.n -= 1; Some(self.n) } } }",
    },
    BuiltinTrait {
        name: "From",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: &["from"],
        bound_methods: &["from"],
        signature: "{ fn from(value: T) -> Self }",
        doc: "How a value of another type becomes this one. `x.into()` reads \
              the `From` impl on the type the use site expects, and `?` converts \
              an error through it.",
        instead: "",
        example: "impl From<i64> for Point { fn from(v: i64) -> Point { Point { x: v, y: 0 } } }",
    },
    BuiltinTrait {
        name: "TryFrom",
        kind: BuiltinTraitKind::Overridable,
        module: None,
        impl_items: &["try_from"],
        bound_methods: &["try_from"],
        signature: "{ fn try_from(value: T) -> Result<Self, E> }",
        doc: "The fallible conversion into this type. `x.try_into()` reads the \
              `TryFrom` impl on the `Ok` payload the use site expects.",
        instead: "",
        example: "impl TryFrom<i64> for Even { fn try_from(v: i64) -> Result<Even, String> { if v % 2 == 0 { Ok(Even { v }) } else { Err(\"odd\") } } }",
    },
    BuiltinTrait {
        name: "Add",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["add"],
        bound_methods: &["add"],
        signature: "{ fn add(&self, other: Self) -> Self }",
        doc: "What `a + b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no arithmetic of its own.",
        instead: "",
        example: "impl Add for Point { fn add(&self, other: Point) -> Point { Point { x: self.x + other.x, y: self.y + other.y } } }",
    },
    BuiltinTrait {
        name: "Sub",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["sub"],
        bound_methods: &["sub"],
        signature: "{ fn sub(&self, other: Self) -> Self }",
        doc: "What `a - b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no arithmetic of its own.",
        instead: "",
        example: "impl Sub for Point { fn sub(&self, other: Point) -> Point { Point { x: self.x - other.x, y: self.y - other.y } } }",
    },
    BuiltinTrait {
        name: "Mul",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["mul"],
        bound_methods: &["mul"],
        signature: "{ fn mul(&self, other: Self) -> Self }",
        doc: "What `a * b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no arithmetic of its own.",
        instead: "",
        example: "impl Mul for Point { fn mul(&self, other: Point) -> Point { Point { x: self.x * other.x, y: self.y * other.y } } }",
    },
    BuiltinTrait {
        name: "Div",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["div"],
        bound_methods: &["div"],
        signature: "{ fn div(&self, other: Self) -> Self }",
        doc: "What `a / b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no arithmetic of its own.",
        instead: "",
        example: "impl Div for Point { fn div(&self, other: Point) -> Point { Point { x: self.x / other.x, y: self.y / other.y } } }",
    },
    BuiltinTrait {
        name: "Rem",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["rem"],
        bound_methods: &["rem"],
        signature: "{ fn rem(&self, other: Self) -> Self }",
        doc: "What `a % b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no arithmetic of its own.",
        instead: "",
        example: "impl Rem for Point { fn rem(&self, other: Point) -> Point { Point { x: self.x % other.x, y: self.y % other.y } } }",
    },
    BuiltinTrait {
        name: "Neg",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["neg"],
        bound_methods: &["neg"],
        signature: "{ fn neg(&self) -> Self }",
        doc: "What unary `-value` answers for this type. Without an `impl` the \
              operator is rejected: a struct has no arithmetic of its own.",
        instead: "",
        example: "impl Neg for Point { fn neg(&self) -> Point { Point { x: -self.x, y: -self.y } } }",
    },
    BuiltinTrait {
        name: "Not",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["not"],
        bound_methods: &["not"],
        signature: "{ fn not(&self) -> Self }",
        doc: "What unary `!value` answers for this type. Without an `impl` the \
              operator is rejected: a struct has no negation of its own.",
        instead: "",
        example: "impl Not for Mask { fn not(&self) -> Mask { Mask { bits: !self.bits } } }",
    },
    BuiltinTrait {
        name: "BitAnd",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["bitand"],
        bound_methods: &["bitand"],
        signature: "{ fn bitand(&self, other: Self) -> Self }",
        doc: "What `a & b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no bitwise meaning of its own.",
        instead: "",
        example: "impl BitAnd for Mask { fn bitand(&self, other: Mask) -> Mask { Mask { bits: self.bits & other.bits } } }",
    },
    BuiltinTrait {
        name: "BitOr",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["bitor"],
        bound_methods: &["bitor"],
        signature: "{ fn bitor(&self, other: Self) -> Self }",
        doc: "What `a | b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no bitwise meaning of its own.",
        instead: "",
        example: "impl BitOr for Mask { fn bitor(&self, other: Mask) -> Mask { Mask { bits: self.bits | other.bits } } }",
    },
    BuiltinTrait {
        name: "BitXor",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["bitxor"],
        bound_methods: &["bitxor"],
        signature: "{ fn bitxor(&self, other: Self) -> Self }",
        doc: "What `a ^ b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no bitwise meaning of its own.",
        instead: "",
        example: "impl BitXor for Mask { fn bitxor(&self, other: Mask) -> Mask { Mask { bits: self.bits ^ other.bits } } }",
    },
    BuiltinTrait {
        name: "Shl",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["shl"],
        bound_methods: &["shl"],
        signature: "{ fn shl(&self, other: Self) -> Self }",
        doc: "What `a << b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no shift of its own.",
        instead: "",
        example: "impl Shl for Mask { fn shl(&self, other: Mask) -> Mask { Mask { bits: self.bits << other.bits } } }",
    },
    BuiltinTrait {
        name: "Shr",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["shr"],
        bound_methods: &["shr"],
        signature: "{ fn shr(&self, other: Self) -> Self }",
        doc: "What `a >> b` answers for this type. Without an `impl` the operator \
              is rejected: a struct carries no shift of its own.",
        instead: "",
        example: "impl Shr for Mask { fn shr(&self, other: Mask) -> Mask { Mask { bits: self.bits >> other.bits } } }",
    },
    BuiltinTrait {
        name: "Index",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["index"],
        bound_methods: &["index"],
        signature: "{ fn index(&self, index: I) -> T }",
        doc: "What `value[i]` answers for this type. The index may be any type \
              the method takes, and the result is whatever it returns.",
        instead: "",
        example: "impl Index for Grid { fn index(&self, i: i64) -> i64 { self.cells[i] } }",
    },
    BuiltinTrait {
        name: "IndexMut",
        kind: BuiltinTraitKind::Operator,
        module: None,
        impl_items: &["index"],
        bound_methods: &["index"],
        signature: "{ fn index(&self, index: I) -> T }",
        doc: "The `Index` contract under its writable spelling; both names reach \
              the same `index`.",
        instead: "",
        example: "impl IndexMut for Grid { fn index(&self, i: i64) -> i64 { self.cells[i] } }",
    },
    automatic(
        "Hash",
        &["hash"],
        "Hashing is structural and automatic: any hashable value keys a `Map` \
         or a `Set`, and equal keys built at different allocations reach the \
         same slot.",
        "Remove the block; to key on part of a value, build the key yourself and \
         store the value beside it.",
        "let m = {Point { x: 1, y: 2 }: \"origin-ish\"}",
    ),
    automatic(
        "Hashable",
        &["hash"],
        "An older spelling of `Hash`. Hashing is structural and automatic.",
        "Remove the block.",
        "let s = #{Point { x: 1, y: 2 }}",
    ),
    automatic(
        "Copy",
        &[],
        "Every value is passed, assigned, and captured by value already, and \
         no parameter asks for a `&` to avoid a copy.",
        "Remove the block.",
        "let b = a",
    ),
    automatic(
        "Sized",
        &[],
        "Every type has a known size; there is no unsized value to bound \
         against.",
        "Remove the block.",
        "fn f<T>(value: T) { }",
    ),
    automatic(
        "Send",
        &[],
        "Every value crosses a `spawn` and a channel already: memory is \
         reference-counted and the runtime owns the synchronization.",
        "Remove the block.",
        "cohort { spawn(|| work(value)) }",
    ),
    automatic(
        "Sync",
        &[],
        "Shared access is the runtime's business, not a marker's: reach for \
         `sync::Mutex` or a channel when goroutines share state.",
        "Remove the block.",
        "let guard = sync::Mutex::new(0)",
    ),
    automatic(
        "Drop",
        &["drop"],
        "Values are released deterministically by reference counting, with no \
         destructor hook to run at the release.",
        "Write `defer expr`, which runs when control leaves the enclosing block \
         by any edge the compiler sees.",
        "defer file.close()",
    ),
    automatic(
        "Into",
        &["into"],
        "`x.into()` reads the `From` impl on the type the use site expects, so \
         the conversion is written once, on the target.",
        "Write `impl From<Source> for Target { fn from(value: Source) -> Target }`.",
        "let p: Point = 5.into()",
    ),
    automatic(
        "TryInto",
        &["try_into"],
        "`x.try_into()` reads the `TryFrom` impl on the `Ok` payload the use \
         site expects, so the conversion is written once, on the target.",
        "Write `impl TryFrom<Source> for Target { fn try_from(value: Source) -> Result<Target, E> }`.",
        "let p: Result<Even, String> = 5.try_into()",
    ),
    automatic(
        "IntoIterator",
        &["into_iter"],
        "`for v in value` drives `Iterator::next` directly; there is no \
         separate conversion step to implement.",
        "Write `impl Iterator for Type { fn next(&mut self) -> Option<T> }`.",
        "for v in countdown { println(\"{}\", v) }",
    ),
    automatic(
        "FromIterator",
        &["from_iter"],
        "`collect` ends an iterator chain with a `Vec`; building any other \
         type from a sequence is an ordinary function or an associated `from`.",
        "Write `impl From<Vec<T>> for Type`, or a plain `Type::from_values(xs)`.",
        "let xs = (1..5).map(|i| i * i).collect()",
    ),
    automatic(
        "AsRef",
        &["as_ref"],
        "A parameter is `T` or `&mut T` and nothing else, so there is no \
         shared-reference form to convert into.",
        "Write an inherent method on the type: `impl Type { fn as_slice(&self) -> [T] }`.",
        "fn total(xs: [i64]) -> i64 { xs.iter().sum() }",
    ),
    automatic(
        "AsMut",
        &["as_mut"],
        "A callee writes through a `&mut T` parameter spelled at the call site, \
         so there is no conversion into a mutable view to implement.",
        "Write `fn extend(xs: &mut Vec<i64>)`, called as `extend(&mut items)`.",
        "extend(&mut items)",
    ),
    automatic(
        "Read",
        &["read"],
        "Byte input is the standard library's `io::Reader` contract, which a \
         type implements the way it implements any declared trait.",
        "Write `use std::io` then `impl Reader for Type`.",
        "let text = io::read_to_string(source)?",
    ),
    automatic(
        "Write",
        &["write"],
        "Byte output is the standard library's `io::Writer` contract, which a \
         type implements the way it implements any declared trait.",
        "Write `use std::io` then `impl Writer for Type`.",
        "writer.write(bytes)?",
    ),
    automatic(
        "Error",
        &[],
        "`errors::Error` is a concrete type, not a contract: a fallible \
         function answers `Result<T, errors::Error>` and `?` converts into it.",
        "Use `errors::new(msg)` / `errors::wrap(cause, msg)`, or an error type \
         of your own with `impl From<Yours> for errors::Error`.",
        "fn load(p: String) -> Result<String, errors::Error> { fs::read_to_string(p) }",
    ),
    automatic(
        "Future",
        &[],
        "There is no `async` / `await`: concurrency is goroutines under a \
         `cohort { }`, and a `JoinHandle` is what a pending result is held by.",
        "Write `cohort { let h = spawn(|| work()) }` then `h.join()`.",
        "cohort { let h = spawn(|| fetch(url)); println(\"{}\", h.join()??) }",
    ),
    callable("Fn"),
    callable("FnMut"),
    callable("FnOnce"),
];

/// A trait the language supplies itself, so an `impl` naming it would
/// declare a contract nothing dispatches through.
const fn automatic(
    name: &'static str,
    items: &'static [&'static str],
    doc: &'static str,
    instead: &'static str,
    example: &'static str,
) -> BuiltinTrait {
    BuiltinTrait {
        name,
        kind: BuiltinTraitKind::Automatic,
        module: None,
        impl_items: items,
        bound_methods: items,
        signature: "",
        doc,
        instead,
        example,
    }
}

/// A closure's type, written in a parameter rather than implemented.
const fn callable(name: &'static str) -> BuiltinTrait {
    BuiltinTrait {
        name,
        kind: BuiltinTraitKind::Automatic,
        module: None,
        impl_items: &[],
        bound_methods: &[],
        signature: "",
        doc: "The type of a closure or a function passed as a value, written \
              with its parameters in parentheses. Capture is automatic, and \
              there is no owned-versus-borrowed distinction between the three \
              spellings.",
        instead: "Write `Fn(A) -> B` in the parameter's type, as in `fn each(xs: Vec<i64>, f: Fn(i64) -> ())`.",
        example: "fn each(xs: Vec<i64>, f: Fn(i64) -> ()) { for v in xs { f(v) } }",
    }
}

impl BuiltinTrait {
    /// True when an `impl` block is how the behaviour is chosen.
    #[must_use]
    pub const fn is_implementable(&self) -> bool {
        matches!(
            self.kind,
            BuiltinTraitKind::Overridable | BuiltinTraitKind::Operator
        )
    }
}

/// The catalog entry for `name`, or `None` when the language does not know
/// a trait by that name.
#[must_use]
pub fn builtin_trait(name: &str) -> Option<&'static BuiltinTrait> {
    BUILTIN_TRAITS.iter().find(|entry| entry.name == name)
}

/// Renders the docs-site page cataloguing every trait the language knows.
///
/// The page is generated from [`BUILTIN_TRAITS`] so the site and the checker
/// cannot disagree about which traits exist, what an `impl` of one must
/// define, or what to write instead of one the language supplies outright.
#[must_use]
pub fn render_builtin_traits_markdown() -> String {
    let mut out = String::with_capacity(8192);
    out.push_str("# Built-in traits\n\n");
    out.push_str(
        "Traits the language knows by name: an `impl` header or a generic bound \
         may name one without declaring it. Source is \
         `crates/gossamer-types/src/builtin_traits.rs`; this page is regenerated \
         from `BUILTIN_TRAITS` by `gos doc --emit-stdlib`.\n\n",
    );
    out.push_str("| Trait | Kind | An `impl` writes |\n|---|---|---|\n");
    for entry in BUILTIN_TRAITS {
        let kind = match entry.kind {
            BuiltinTraitKind::Overridable => "overridable",
            BuiltinTraitKind::Operator => "operator",
            BuiltinTraitKind::Automatic => "automatic",
        };
        let writes = if entry.signature.is_empty() {
            "nothing - the language supplies it".to_string()
        } else {
            format!("`{}`", entry.signature)
        };
        out.push_str(&format!(
            "| [`{name}`](#{anchor}) | {kind} | {writes} |\n",
            name = entry.name,
            anchor = entry.name.to_lowercase(),
        ));
    }
    push_kind_section(
        &mut out,
        BuiltinTraitKind::Overridable,
        "Overridable",
        "Every type already has this behaviour; an `impl` replaces the \
         synthesized one. `#[derive(..)]` asks for the synthesized behaviour \
         explicitly where a type needs it named.",
    );
    push_kind_section(
        &mut out,
        BuiltinTraitKind::Operator,
        "Operators",
        "The operator has no meaning for a user type until an `impl` supplies \
         one. Each block defines the one method named below, and the operator \
         dispatches to it.",
    );
    push_kind_section(
        &mut out,
        BuiltinTraitKind::Automatic,
        "Supplied by the language",
        "The language provides this behaviour outright, so an `impl` block \
         would name a contract nothing dispatches through and is rejected. \
         Each entry says what to write in its place.",
    );
    out
}

/// Appends the section for one trait kind, with an entry per trait.
fn push_kind_section(out: &mut String, kind: BuiltinTraitKind, title: &str, blurb: &str) {
    out.push_str(&format!("\n## {title}\n\n{blurb}\n"));
    for entry in BUILTIN_TRAITS.iter().filter(|e| e.kind == kind) {
        out.push_str(&format!("\n### `{}`\n\n", entry.name));
        if let Some(module) = entry.module {
            out.push_str(&format!("Declared by `{module}`.\n\n"));
        }
        out.push_str(&format!("{}\n", entry.doc));
        if !entry.signature.is_empty() {
            out.push_str(&format!(
                "\n```gossamer\nimpl {} for Type {}\n```\n",
                entry.name, entry.signature
            ));
        }
        if !entry.instead.is_empty() {
            out.push_str(&format!("\nInstead: {}\n", entry.instead));
        }
        if !entry.example.is_empty() {
            out.push_str(&format!("\n```gossamer\n{}\n```\n", entry.example));
        }
        if !entry.bound_methods.is_empty() {
            let methods: Vec<String> = entry
                .bound_methods
                .iter()
                .map(|m| format!("`{m}`"))
                .collect();
            out.push_str(&format!(
                "\nA bound naming `{}` licenses {} on a type parameter.\n",
                entry.name,
                methods.join(", ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILTIN_TRAITS, BuiltinTraitKind, builtin_trait};

    #[test]
    fn every_catalog_name_is_unique() {
        let mut names: Vec<&str> = BUILTIN_TRAITS.iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate trait names in the catalog");
    }

    #[test]
    fn an_implementable_trait_renders_the_method_its_impl_supplies() {
        for entry in BUILTIN_TRAITS.iter().filter(|e| e.is_implementable()) {
            assert!(
                entry.signature.starts_with("{ fn "),
                "{} has no rendered contract",
                entry.name
            );
            assert!(
                !entry.impl_items.is_empty(),
                "{} names no item an impl supplies",
                entry.name
            );
        }
    }

    #[test]
    fn an_automatic_trait_names_what_to_write_instead() {
        for entry in BUILTIN_TRAITS
            .iter()
            .filter(|e| e.kind == BuiltinTraitKind::Automatic)
        {
            assert!(
                !entry.instead.is_empty(),
                "{} rejects an impl without saying what to write",
                entry.name
            );
        }
    }

    #[test]
    fn operator_entries_carry_their_own_method_name() {
        for name in ["Add", "Sub", "Mul", "Div", "Rem", "Shl", "Shr"] {
            let entry = builtin_trait(name).expect("operator is cataloged");
            let method = entry.impl_items[0];
            assert!(
                entry.signature.contains(&format!("fn {method}(")),
                "{name} renders `{}` for method `{method}`",
                entry.signature
            );
        }
    }
}
