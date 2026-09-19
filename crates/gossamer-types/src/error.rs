//! Diagnostics emitted by the type checker.

#![forbid(unsafe_code)]

use std::fmt;

use gossamer_lex::Span;
use thiserror::Error;

/// One type-checker diagnostic paired with its source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDiagnostic {
    /// Specific error variant.
    pub error: TypeError,
    /// Where in the source the error was detected.
    pub span: Span,
}

impl TypeDiagnostic {
    /// Whether this finding is advisory rather than fatal: a lint the
    /// checker is the only pass with enough type information to make, so it
    /// reports at warning severity and never moves the exit code.
    #[must_use]
    pub const fn is_advisory(&self) -> bool {
        matches!(self.error, TypeError::UndeclaredReturnValue { .. })
    }

    /// Constructs a diagnostic from its error and span.
    #[must_use]
    pub const fn new(error: TypeError, span: Span) -> Self {
        Self { error, span }
    }
}

impl fmt::Display for TypeDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.error)
    }
}

/// What kind of value was formatted, for [`TypeError::ValueNotDisplayable`].
/// Each class gets its own help line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotDisplayableClass {
    /// A runtime-owned handle: a pointer whose bits differ run to run.
    Handle,
    /// A function, method, or closure value.
    Callable,
    /// A channel endpoint or join handle.
    Concurrency,
    /// A generic type with no `fmt`: whether its fields render depends on
    /// the arguments each instantiation supplies, so the declaration has to
    /// ask for one.
    GenericWithoutDebug,
}

fn not_displayable_message(ty: &str, class: NotDisplayableClass) -> String {
    match class {
        NotDisplayableClass::Handle => {
            format!("`{ty}` is a runtime handle and has no display representation")
        }
        NotDisplayableClass::Callable => {
            format!("a {ty} has no display representation and cannot be formatted")
        }
        NotDisplayableClass::Concurrency => {
            format!("`{ty}` is runtime state and has no display representation")
        }
        NotDisplayableClass::GenericWithoutDebug => {
            format!("`{ty}` is generic and has no `fmt` to format it with")
        }
    }
}

/// Every failure mode the type checker can report.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TypeError {
    /// Two concrete types that should be equal are not.
    #[error("type mismatch: expected `{expected}`, found `{found}`")]
    TypeMismatch {
        /// Expected type, rendered via [`crate::render_ty`].
        expected: String,
        /// Found type, rendered via [`crate::render_ty`].
        found: String,
    },
    /// An operator mixed an integer operand with a float operand.
    /// Gossamer never widens across that boundary implicitly, so the fix
    /// is a written cast on the integer operand.
    #[error("type mismatch: expected `{expected}`, found `{found}`")]
    NumericOperandMismatch {
        /// Left operand type, rendered via [`crate::render_ty`].
        expected: String,
        /// Right operand type, rendered via [`crate::render_ty`].
        found: String,
        /// The whole expression rewritten with the cast in place.
        cast: String,
    },
    /// An `Option<T>` value was used where the payload `T` is required.
    #[error("type mismatch: expected `{expected}`, found `{found}`")]
    OptionValueMismatch {
        /// Payload type the site requires.
        expected: String,
        /// The `Option<T>` type found there.
        found: String,
        /// Source spelling of the expression producing the `Option`.
        actual: String,
        /// Name to bind the payload to in the `if let` form.
        binding: String,
        /// Spelling of a `T` value for the `unwrap_or` form.
        default: String,
    },
    /// A named call parameter received a value of an incompatible type.
    #[error(
        "type mismatch for parameter `{parameter}` of `{callee}`: expected `{expected}`, found `{found}` (value `{actual}`)"
    )]
    ArgumentTypeMismatch {
        /// Fully-qualified callable name.
        callee: String,
        /// Source-level parameter name.
        parameter: String,
        /// Expected type, rendered via [`crate::render_ty`].
        expected: String,
        /// Found type, rendered via [`crate::render_ty`].
        found: String,
        /// Source-level rendering of the supplied value or expression.
        actual: String,
    },
    /// A method call could not be resolved to any known definition.
    #[error("no method named `{name}` found for type `{ty}`")]
    UnresolvedMethod {
        /// Receiver type.
        ty: String,
        /// Method name.
        name: String,
        /// Method names the receiver does have, in canonical order.
        /// Empty when the checker has no surface list for the receiver.
        available: Vec<String>,
        /// The receiver declares a field under this name, so the call
        /// spelling is what missed, not the name.
        field_of_same_name: bool,
        /// The program declares a free function under this name, so the
        /// call spelling is what missed, not the name.
        free_fn_of_same_name: bool,
    },
    /// A heap (`MaxHeap`, `MinHeap`) named an element the language has no
    /// ordering for, so the heap has nothing to order it by.
    #[error("`{owner}` cannot hold an element of type `{found}`")]
    SlotCollectionElement {
        /// Container spelling as written.
        owner: String,
        /// The element type the annotation named.
        found: String,
    },
    /// A `spawn` that no `cohort { }` in the same function body encloses.
    /// The child would attach to whatever cohort the caller happens to be
    /// inside, which the callee's signature says nothing about.
    #[error(
        "`spawn` in `{function}` is not inside a `cohort` block, so its child would attach to the caller's cohort and outlive this call"
    )]
    SpawnOutsideCohort {
        /// Name of the function the spawn was written in.
        function: String,
    },
    /// A container that orders its elements internally named an element
    /// whose type writes its own `cmp`. The container keeps its elements in
    /// its own order and never calls a comparator, so the order the type
    /// declares would not be the order the container reads.
    #[error("`{owner}` cannot order an element of type `{elem}`, which writes its own `cmp`")]
    ContainerIgnoresUserOrder {
        /// Container spelling as written.
        owner: String,
        /// Element or key type that declares its own order.
        elem: String,
    },
    /// A method reached from outside the module its `impl` was written
    /// in, without `pub`.
    #[error("method `{name}` on `{ty}` is private to module `{module}`")]
    PrivateMethod {
        /// Receiver type.
        ty: String,
        /// Method name.
        name: String,
        /// Module the `impl` was written in.
        module: String,
    },
    /// A field reached from outside the module declaring its struct,
    /// without `pub`.
    #[error("field `{name}` on `{ty}` is private to module `{module}`")]
    PrivateField {
        /// Struct that declares the field.
        ty: String,
        /// Field name.
        name: String,
        /// Module the struct was declared in.
        module: String,
    },
    /// A binary or unary operator could not be resolved for the given
    /// operand types.
    #[error("cannot apply `{op}` to `{lhs}` and `{rhs}`")]
    UnresolvedOp {
        /// Operator symbol.
        op: String,
        /// Left-hand type.
        lhs: String,
        /// Right-hand type (for unary ops this is the operand).
        rhs: String,
    },
    /// An overloadable operator was applied to a user struct / enum that
    /// carries no impl of the operator's backing trait.
    #[error("cannot apply `{op}` to `{ty}`")]
    UnresolvedOpImpl {
        /// Operator symbol (`+`, `-`, `+=`, ...).
        op: String,
        /// Operator trait that would provide the operation (`Add`).
        trait_name: String,
        /// Impl method the operator dispatches to (`add`).
        method: String,
        /// The ADT operand's type.
        ty: String,
    },
    /// A `match` expression lacks coverage for one or more patterns.
    #[error("non-exhaustive patterns: {missing}")]
    NonExhaustiveMatch {
        /// Human-readable description of the missing patterns.
        missing: String,
    },
    /// A plain `let` tried to assign a value to a literal.
    #[error("cannot assign to a literal")]
    CannotAssignToLiteral,
    /// A plain `let` used a pattern that might not match its value.
    #[error("this `let` pattern might not match its value")]
    LetPatternMayNotMatch,
    /// A `let &...` pattern tried to destructure a non-reference value.
    #[error(
        "`let {pattern} name = value` requires an `{pattern}` initializer; write `let name = {pattern} value` to borrow, or remove `{pattern}` to bind the value directly"
    )]
    ReferencePatternRequiresReference {
        /// Source spelling of the reference pattern.
        pattern: &'static str,
    },
    /// A parameter wrote `&` in its pattern rather than in its type.
    #[error(
        "parameter pattern `{pattern}` requires an `{pattern}` type, but `{binding}` is declared `{ty}`"
    )]
    ReferenceParameterPatternPosition {
        /// Source spelling of the reference pattern.
        pattern: &'static str,
        /// Name the pattern binds.
        binding: String,
        /// Rendered declared parameter type.
        ty: String,
        /// Declared type with the pattern's reference applied.
        reference_ty: String,
    },
    /// A bare `[T]` slice was used where a sized owned value is required.
    #[error("slice type `[{element}]` is unsized and cannot be stored by value")]
    UnsizedSliceValue {
        /// Rendered element type.
        element: String,
    },
    /// A length- or capacity-changing `Vec` method was called on an array or slice.
    #[error(
        "method `{method}` changes sequence length or capacity and requires `Vec<T>`, but the receiver has type `{ty}`"
    )]
    SequenceResizeRequiresVec {
        /// Receiver type.
        ty: String,
        /// Attempted method.
        method: String,
    },
    /// A fixed array length could not be evaluated at compile time.
    #[error("array length must be a compile-time constant")]
    ArrayLengthNotConstant,
    /// A nominal struct or enum was destructured with an anonymous tuple
    /// pattern, which would bypass its declared name and field labels.
    #[error("destructuring `{ty}` requires its struct or variant name")]
    StructPatternNameRequired {
        /// The nominal type being destructured.
        ty: String,
    },
    /// A named struct was constructed with tuple-struct call syntax.
    #[error("struct `{name}` must be constructed with braces")]
    StructConstructorBracesRequired {
        /// The nominal struct name.
        name: String,
    },
    /// A tuple struct was constructed with named-field literal syntax.
    #[error("tuple struct `{name}` must be constructed with parentheses")]
    TupleStructConstructorParenthesesRequired {
        /// The nominal struct name.
        name: String,
    },
    /// A named struct was initialized with positional braced entries.
    #[error("named struct `{name}` requires field names in its initializer")]
    NamedStructFieldsRequired {
        /// Struct name.
        name: String,
    },
    /// A struct literal omitted a required field.
    #[error("missing field `{field}` in initializer of `{name}`")]
    MissingStructField {
        /// Struct name.
        name: String,
        /// Missing field name.
        field: String,
    },
    /// A struct literal initialized the same field more than once.
    #[error("field `{field}` specified more than once in initializer of `{name}`")]
    DuplicateStructField {
        /// Struct name.
        name: String,
        /// Duplicate field name.
        field: String,
    },
    /// A positional named-struct literal supplied more values than fields.
    #[error("too many positional fields in initializer of `{name}`")]
    TooManyStructFields {
        /// Struct name.
        name: String,
        /// Declared field count.
        expected: usize,
        /// Supplied positional-plus-keyed field count.
        found: usize,
    },
    /// `value as T` was requested between two types that are not in
    /// the `as`-cast whitelist (non-primitive source, struct source,
    /// etc.).
    #[error("non-primitive cast: `{from}` as `{to}`")]
    InvalidCast {
        /// Source type.
        from: String,
        /// Target type.
        to: String,
    },
    /// `.into()` was written across an opaque alias boundary that has no
    /// conversion behind it.
    #[error("no conversion from `{from}` to `{to}`")]
    NoConversion {
        /// Source type.
        from: String,
        /// Target type.
        to: String,
        /// `true` when the source is a borrowed fixed-length sequence and
        /// the target the owned `Vec` of the same element, which copies
        /// with `to_vec()` rather than through a `From` impl.
        borrowed_sequence: bool,
    },
    /// `.into()` / `.try_into()` written where nothing says what to
    /// convert to, so the target stays an inference variable.
    #[error("cannot infer the target type of `{method}`")]
    ConversionTargetUnknown {
        /// The conversion method as written, `into` or `try_into`.
        method: String,
    },
    /// Field access (`value.field`) on a type that has no such field.
    /// Splits two failure modes: `opaque` is true when the receiver's
    /// type is known but the checker has no field map for it (typical
    /// of dynamic stdlib types like `json::Value`); `opaque` is false
    /// when the type does have fields but `field` isn't one of them.
    #[error("type `{ty}` has no field `{field}`")]
    UnknownField {
        /// Receiver type.
        ty: String,
        /// Field name attempted.
        field: String,
        /// `true` when the receiver is opaque to the checker.
        opaque: bool,
        /// Field names `ty` declares, in declaration order. Empty when
        /// the receiver is opaque.
        declared: Vec<String>,
        /// Span covering the field name alone, when the site can point at
        /// it. Carries the machine-applicable rename.
        field_span: Option<Span>,
        /// The receiver has a method under this name, so the read is
        /// missing its call parentheses.
        method_of_same_name: bool,
    },
    /// A `Result<T, E>` expression was used as a statement without
    /// binding or propagating the value. SPEC §9: discarded Results
    /// are a compile error unless explicitly suppressed with `let _ =`.
    #[error("unused `Result` value - the `Err` variant may go unhandled")]
    DiscardedResult,
    /// A value produced by a `#[must_use]` function, or of a
    /// `#[must_use]` type, was used as a statement without binding or
    /// consuming it.
    #[error("unused {what} - `{name}` is marked `#[must_use]`")]
    DiscardedMustUse {
        /// Shape of the discarded value, for the message.
        what: &'static str,
        /// Name of the annotated function or type.
        name: String,
    },
    /// A `for` loop whose subject is a `Result` or an `Option` rather
    /// than a sequence. The value inside has to be taken first.
    #[error("`{name}` is not a sequence - a `for` over it binds nothing and runs zero times")]
    IterableWrapper {
        /// `Result` or `Option`.
        name: String,
        /// How to take the value, for the help line.
        taken: &'static str,
    },
    /// An expression, type, or pattern nested past the type-checker's
    /// hard recursion limit. Emitted on adversarial input that
    /// survives parsing (rare, but the typechecker has its own
    /// guard to keep `cargo check`-style probes from crashing).
    #[error("expression nests beyond {limit} levels (consider rewriting with a helper)")]
    RecursionLimit {
        /// Recursion limit at which the error was raised.
        limit: u32,
    },
    /// A `type` alias expands to itself through a cycle (`type A = B;
    /// type B = A`), so it has no underlying type. Treated as
    /// `TyKind::Error` so downstream typing does not cascade.
    #[error("type alias `{name}` is cyclic - it expands to itself")]
    CyclicTypeAlias {
        /// Name of the alias at the point the cycle was detected.
        name: String,
    },
    /// A `#[derive(...)]` names a trait that synthesizes nothing: it is
    /// either automatic for value types (comparison / hashing / serde) or
    /// is implemented with `impl Trait for T`, not derived.
    #[error("`#[derive({name})]` is not supported")]
    UnsupportedDerive {
        /// The rejected derive name.
        name: String,
        /// Why it is unsupported and what to do instead.
        hint: String,
    },
    /// An integer literal overflows the value range of its declared
    /// type-suffix (e.g. `300i8`, `99999999999999999999i64`). Treated
    /// as `TyKind::Error` so downstream typing does not cascade.
    #[error("integer literal `{literal}` does not fit in `{ty}`")]
    IntLiteralOverflow {
        /// Source spelling of the literal, including suffix.
        literal: String,
        /// Name of the suffix-derived target type.
        ty: String,
    },
    /// A string escape that the parser accepted but cannot be
    /// validly decoded (out-of-range `\u{...}`, surrogate code
    /// point, non-ASCII `\x..`). Surfaced from the AST-to-typechecker
    /// boundary so that downstream lowering sees no malformed
    /// strings.
    #[error("invalid escape `{escape}` in string literal: {reason}")]
    InvalidEscape {
        /// Verbatim text of the rejected escape sequence.
        escape: String,
        /// Human-readable reason the escape is invalid.
        reason: String,
    },
    /// A trait bound `<T: Bound>` on a generic parameter names a
    /// trait the resolver does not know about. Catches typos
    /// (`Hashabel` for `Hashable`) at function declaration before
    /// the call site stumbles into a runtime "no method" error.
    #[error("unknown trait `{name}` in bound for parameter `{param}`")]
    UnknownTraitBound {
        /// Generic parameter the bound was attached to.
        param: String,
        /// Trait name as written.
        name: String,
    },
    /// A trait name was written where a type belongs. A trait names
    /// behaviour a type supplies, and Gossamer has no `dyn`, so there is no
    /// value whose type it is.
    #[error("`{name}` is a trait, not a type")]
    TraitInTypePosition {
        /// Trait name as written.
        name: String,
    },
    /// An `impl` header names a trait no declaration and no built-in
    /// provides. Nothing checks the block's contents against a contract that
    /// does not exist, so a misspelled trait name would otherwise compile to
    /// a set of inherent methods nobody calls.
    #[error("unknown trait `{name}` in `impl {name} for {ty}`")]
    UnknownImplTrait {
        /// Trait name as written in the header.
        name: String,
        /// Type the block implements it for.
        ty: String,
    },
    /// An `impl` header names a trait the language supplies itself. The
    /// block would declare a contract nothing dispatches through, so the
    /// behaviour it means to change would stay exactly as it was.
    #[error("`{name}` is supplied by the language, so `impl {name} for {ty}` changes nothing")]
    AutomaticTraitImpl {
        /// Trait name as written in the header.
        name: String,
        /// Type the block implements it for.
        ty: String,
        /// What the language does instead, from the built-in trait catalog.
        reason: String,
        /// What to write in the block's place.
        instead: String,
    },
    /// A generic call instantiates a type parameter with a concrete type
    /// that does not implement a required trait bound.
    #[error("the trait bound `{ty}: {bound}` is not satisfied")]
    TraitBoundNotSatisfied {
        /// Concrete type supplied at the call site.
        ty: String,
        /// Trait the parameter is bound by.
        bound: String,
    },
    /// A method was called on a generic parameter that none of its trait
    /// bounds declares. The parameter stands for every type a caller may
    /// supply, so only its bounds say what it can do.
    #[error("no method named `{method}` on type parameter `{param}`")]
    MethodNotOnBound {
        /// Source-level name of the type parameter (`T`, `U`, ...).
        param: String,
        /// Method the receiver was asked for.
        method: String,
        /// Traits the parameter is bound by, in source order.
        bounds: Vec<String>,
    },
    /// An operator was applied to a generic parameter that none of its
    /// trait bounds gives the operator's method. The parameter stands for
    /// every type a caller may supply, so a bound has to license the
    /// operation.
    #[error("`{op}` cannot be applied to type parameter `{param}`")]
    OperatorNotOnBound {
        /// Source-level name of the type parameter (`T`, `U`, ...).
        param: String,
        /// Operator as written.
        op: String,
        /// Trait that licenses the operator (`Add`, `Neg`, ...).
        trait_name: String,
        /// Method the operator dispatches to (`add`, `neg`, ...).
        method: String,
        /// Traits the parameter is bound by, in source order.
        bounds: Vec<String>,
    },
    /// A trait impl leaves out a method the trait declares without a
    /// default body. Every call through the trait lowers to a direct call
    /// to that method, so the impl has to define it.
    #[error(
        "`impl {trait_name} for {ty}` is missing {}: {}",
        if missing.len() == 1 { "method" } else { "methods" },
        missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")
    )]
    MissingTraitImplMethods {
        /// Trait being implemented.
        trait_name: String,
        /// Type the impl attaches to.
        ty: String,
        /// Trait methods with no body in this impl and no default.
        missing: Vec<String>,
    },
    /// A trait impl leaves out an associated type or constant the trait
    /// declares without a default. Every projection through the trait has
    /// to land on a concrete item, so the impl has to supply it.
    #[error(
        "`impl {trait_name} for {ty}` is missing associated {}: {}",
        if missing.len() == 1 { "item" } else { "items" },
        missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")
    )]
    MissingTraitImplAssocItems {
        /// Trait being implemented.
        trait_name: String,
        /// Type the impl attaches to.
        ty: String,
        /// Missing items rendered as `type Item` / `const MAX`.
        missing: Vec<String>,
    },
    /// A trait impl defines an item the trait does not declare. The block
    /// promises one contract, so anything outside it would silently become
    /// an inherent method under a misleading header.
    #[error("`{item}` is not a member of trait `{trait_name}`")]
    ImplItemNotInTrait {
        /// Trait being implemented.
        trait_name: String,
        /// Type the impl attaches to.
        ty: String,
        /// Item the block defines that the trait does not declare.
        item: String,
        /// Item names the trait does declare, in declaration order.
        declared: Vec<String>,
    },
    /// Two `impl` blocks supply the same trait for the same type, so every
    /// call through the trait has two bodies to reach and no rule picks one.
    #[error("conflicting implementations of trait `{trait_name}` for type `{ty}`")]
    ConflictingTraitImpl {
        /// Trait implemented more than once.
        trait_name: String,
        /// Type both blocks attach to.
        ty: String,
        /// Whether the first implementation is a `#[derive(...)]`.
        derived: bool,
    },
    /// A function body's tail answers a value through a signature that
    /// declares no return type, so the value is discarded and the caller
    /// reads the unit the signature promises.
    #[error("the tail value of `{name}` is discarded; it declares no return type")]
    UndeclaredReturnValue {
        /// Function or method name as written.
        name: String,
        /// Type the body's answer has, for the suggested `-> T`.
        found: String,
    },
    /// A `&` / `&mut` applied to a range index. A window into part of a
    /// sequence is not a value this release can produce, and the copy the
    /// index would otherwise make is not the alias the borrow asks for.
    #[error("`&{mutability}{base}[{range}]` would borrow a window into part of a sequence")]
    RangeBorrow {
        /// `mut ` for a mutable borrow, empty otherwise.
        mutability: &'static str,
        /// Base expression as written, for the message.
        base: String,
        /// Range as written, for the message.
        range: String,
    },
    /// A path projected an associated item that nothing in scope declares.
    #[error("no associated item named `{name}` on `{base}`")]
    UnknownAssocItem {
        /// Base the projection was written against (`T`, `Self`, a type).
        base: String,
        /// Associated item name as written.
        name: String,
        /// Associated item names the base's traits do declare.
        declared: Vec<String>,
    },
    /// An associated item reached through a trait has more than one
    /// candidate impl and no equality constraint picking one.
    #[error("associated {kind} `{name}` of trait `{trait_name}` is ambiguous through `{base}`")]
    AmbiguousAssocItem {
        /// Base the projection was written against.
        base: String,
        /// Trait declaring the associated item.
        trait_name: String,
        /// Associated item name.
        name: String,
        /// `"type"` or `"const"` - an equality constraint pins a type,
        /// while a constant needs a concrete base or a trait default.
        kind: &'static str,
    },
    /// A built-in iterator was passed to a parameter bound by an iteration
    /// trait. Only a type with an impl block can specialise such a call.
    #[error("`{ty}` cannot instantiate a parameter bound by an iteration trait")]
    BuiltinIteratorNotGeneric {
        /// Rendered iterator type supplied at the call site.
        ty: String,
    },
    /// An enum declares more variants than the heap representation's
    /// one-byte discriminant can index.
    #[error("enum `{name}` has {count} variants; the maximum is 256")]
    TooManyVariants {
        /// Enum name.
        name: String,
        /// Declared variant count.
        count: usize,
    },
    /// A closure was passed to a std combinator whose signature the
    /// checker has no row for, leaving the closure's parameter types
    /// uninferrable. Without a concrete type the compiled tiers pin
    /// the parameter to i64 and a String/Error payload formats as a
    /// raw pointer, so this is a hard error instead of silent garbage.
    #[error(
        "cannot infer the parameter types of this closure passed to `{combinator}`; \
         annotate the parameter (e.g. `|x: String| ...`) or bind the payload through a typed `match`"
    )]
    ClosureParamUninferred {
        /// Qualified combinator path, e.g. `iter::map`.
        combinator: String,
    },
    /// A generic call returns a type parameter that cannot be inferred from
    /// arguments, an expected `Result<T, E>`, or explicit turbofish syntax.
    #[error(
        "cannot infer type parameter `{param}` for `{callable}`; \
         write `{callable}::<{param}>(...)` or assign to `Result<{param}, errors::Error>`"
    )]
    GenericReturnTypeUninferred {
        /// Callable name shown in diagnostics.
        callable: String,
        /// Generic type parameter name.
        param: String,
    },
    /// The `?` operator was applied to a value that is not a
    /// `Result` or `Option`, or appeared outside a function returning
    /// the same propagation family.
    #[error("the `?` operator cannot be used with `{ty}` here: {reason}")]
    QuestionMarkUnsupported {
        /// Rendered type of the operand or enclosing return type.
        ty: String,
        /// Why the operator is unsupported at this position.
        reason: String,
    },
    /// `i128` / `u128` appeared in a type position, a literal
    /// suffix, or a cast target. The runtime's i64 value model has
    /// no 128-bit representation on any tier, so the checker
    /// rejects the types uniformly instead of letting the VM run
    /// them at silent 64-bit width.
    #[error("`{ty}` is unsupported because all integer tiers are limited to 64 bits")]
    Int128Unsupported {
        /// Which of the two 128-bit spellings appeared.
        ty: String,
    },
    /// A std free function was used as a first-class value
    /// (`r.map_err(errors::new)`) but is not in the supported-set
    /// signature catalogue, so the pass that rewrites a std function
    /// named in value position into the closure calling it has no arity
    /// to write. Rejected uniformly on every tier rather than letting
    /// the VM accept what a native build cannot link.
    #[error("std function `{path}` has no fixed parameter list to pass as a value")]
    StdFnValueUnsupported {
        /// Qualified std path as written, e.g. `strings::repeat`.
        path: String,
    },
    /// A lazy iterator state was formatted or printed directly instead of
    /// being consumed by a terminal.
    #[error("a lazy iterator cannot be formatted before it is collected or consumed")]
    IteratorStateFormatted,
    /// A value with no textual representation - a runtime handle, a
    /// callable, or a concurrency endpoint - was passed to a format
    /// macro.
    #[error("{}", not_displayable_message(ty, *class))]
    ValueNotDisplayable {
        /// Type as the checker knows it, e.g. `sync::Map`, or the word
        /// for a callable that has no useful type to name.
        ty: String,
        /// What the value is, used to pick the help text.
        class: NotDisplayableClass,
    },
    /// A lazy iterator state was used after an adapter or terminal consumed it.
    #[error("iterator `{name}` was already consumed by `{operation}`")]
    IteratorStateConsumed {
        /// Local binding name.
        name: String,
        /// Operation that consumed it.
        operation: String,
    },
    /// `json::render` / `json::encode` was handed an enum value - most
    /// often a `Result` from `json::parse(..)` that is missing its `?`,
    /// or an `Option`. Enums have no JSON serialization; the VM
    /// tolerated the misuse while the compiled tiers silently produced
    /// an empty string, so the checker now rejects it uniformly.
    #[error(
        "`json::{op}` cannot serialize the enum type `{ty}`; unwrap it first \
         (e.g. with `?` or `match`), or use `to_json::<T>` for a struct"
    )]
    JsonNotSerializable {
        /// The `render` / `encode` function name as written.
        op: String,
        /// The rendered enum type, e.g. `Result<json::Value, errors::Error>`.
        ty: String,
    },
    /// A call supplied the wrong number of arguments for the callee's
    /// declared arity. The VM aborts on this and the LLVM backend
    /// silently drops or zero-fills the mismatched args, so the checker
    /// rejects it statically on every tier.
    #[error("`{callee}` takes {expected} argument(s) but {found} were supplied")]
    CallArityMismatch {
        /// Callee name as written.
        callee: String,
        /// Declared parameter count.
        expected: usize,
        /// Argument count at the call site.
        found: usize,
    },
    /// A path `Enum::Variant` named a variant the enum does not declare.
    /// The resolver leaves the path unresolved and the VM faults at
    /// runtime (GX0002); the checker rejects it where the enum is known.
    #[error("enum `{enum_name}` has no variant `{variant}`")]
    UnknownVariant {
        /// Enum type name.
        enum_name: String,
        /// Variant name as written.
        variant: String,
        /// Variant names the enum declares, sorted for a stable listing.
        declared: Vec<String>,
    },
    /// A method reached through a generic bound (`fn f<T: Pet>(p: &T)`)
    /// resolves only through a supertrait of the bound (`trait Pet:
    /// Animal`, `name` declared on `Animal`). The compiled tiers cannot
    /// lower supertrait-through-bound dispatch (SPEC §3.8), so it is
    /// rejected uniformly instead of miscompiling on the native tier.
    #[error(
        "method `{method}` on `{param}` comes from supertrait `{supertrait}` of bound `{bound}`; \
         supertrait methods through a generic bound are not supported"
    )]
    SupertraitMethodThroughBound {
        /// Generic parameter the receiver binds to.
        param: String,
        /// Method name as called.
        method: String,
        /// The directly-named bound trait.
        bound: String,
        /// The supertrait that actually declares the method.
        supertrait: String,
    },
    /// `value[index]` where `value`'s type cannot be indexed (only
    /// `[T]` / `[T; N]` / `Vec<T>` / `String` are). The VM faults at
    /// runtime (GX0001) and the compiled tier reads through the value
    /// as a base pointer (SIGSEGV), so it is rejected at check.
    #[error("type `{ty}` cannot be indexed")]
    NotIndexable {
        /// Receiver type as rendered.
        ty: String,
    },
    /// `value(args)` where `value`'s type is not callable (not a `fn`
    /// item, `fn(..)` pointer, or `Fn(..)` value). The VM faults
    /// (GX0001) and the compiled tier emits a call through a
    /// non-function symbol (build failure), so it is rejected at check.
    #[error("type `{ty}` is not callable")]
    NotCallable {
        /// Callee type as rendered.
        ty: String,
    },
    /// `value.N` where `value` is not a tuple, or `N` is past the
    /// tuple's arity. The VM faults (GX0004) and the compiled tier
    /// reads out-of-object memory (garbage / info leak), so it is
    /// rejected at check.
    #[error("type `{ty}` has no tuple field `.{index}`")]
    NoTupleField {
        /// Receiver type as rendered.
        ty: String,
        /// Positional index attempted.
        index: u64,
    },
    /// A `match` / `if let` arm patterns a `json::Value` scrutinee with a
    /// `json::Value::Object(..)` / `::Array(..)` / `::Int(..)` etc.
    /// constructor. `json::Value` is an opaque dynamic-document handle
    /// with no matchable discriminant across the tiers, so such a pattern
    /// silently falls through on the VM and faults on the compiled tiers;
    /// rejected at check so the surface stays sound and the dynamic
    /// accessor API is used instead.
    #[error("`json::Value::{variant}` cannot be used as a pattern")]
    JsonValuePatternUnsupported {
        /// The json variant named in the pattern (`Object`, `Int`, ...).
        variant: String,
    },
    /// `.downgrade()` was called on a by-value type with no runtime RC
    /// header (a scalar, `Option`/`Result`, or other packed value).
    /// `Weak<T>` is a non-owning pointer into a reference-counted
    /// allocation, so the compiled tiers read a header off the value's
    /// bits and fault (SIGSEGV); rejected at check.
    #[error("`.downgrade()` is not valid on `{ty}`")]
    WeakDowngradeNonRc {
        /// Rendered receiver type.
        ty: String,
    },
    /// A data-last `option::*` / `result::*` combinator was called at
    /// full arity with its trailing data argument not shaped as the
    /// module's payload type - most often the `Option`/`Result` passed
    /// first and the closure last. The runtime reads the closure slot
    /// as the data value and silently returns the `None`/`Err`
    /// fallback, so the checker rejects the call.
    #[error("`{combinator}` takes its `{shape}` argument last")]
    CombinatorDataArgMismatch {
        /// Qualified combinator path, e.g. `option::and_then`.
        combinator: String,
        /// The payload shape the data slot requires (`Option`/`Result`).
        shape: String,
    },
    /// An assignment (`=` or a compound `+=` / ...) targets a binding
    /// that was not declared `mut`. `let`/parameter bindings are
    /// immutable by default; a place rooted at one cannot be written.
    #[error("cannot assign to immutable binding `{name}`")]
    AssignToImmutable {
        /// Name of the immutable root binding.
        name: String,
    },
    /// `*x = v` where `x` is not a reference, so the write reaches no place.
    #[error("cannot write through `{name}`: it is a value, not a reference")]
    DerefWriteToNonReference {
        /// Name of the dereferenced binding.
        name: String,
    },
    /// A reference was passed where the parameter is a value.
    #[error("`{argument}` is a reference; the parameter is the value it names")]
    ReferenceArgumentNeedsDeref {
        /// The argument as written.
        argument: String,
    },
    /// A wrapping arithmetic operation was written as an integer method.
    /// The operator is the one spelling.
    #[error(
        "`{method}` is not an integer method; wrapping arithmetic is the `{operator}` operator"
    )]
    WrappingMethodRetired {
        /// The method as written.
        method: String,
        /// The operator that spells the operation.
        operator: String,
        /// The whole call rewritten to the operator, when both operands have
        /// a short spelling.
        replacement: Option<String>,
    },
    /// A `Simd` / `Mask` type or operation outside the supported lane set:
    /// an element type or lane count the vector type does not take, or an
    /// operator its lanes do not define.
    #[error("unsupported vector: {reason}")]
    SimdShape {
        /// What the vector type or operation does not support.
        reason: String,
    },
    /// `m.range(r)` on a `BTreeMap` given something other than a range
    /// written in place: the bounds are keys, so they are read from the
    /// range expression itself.
    #[error("`range` takes a range written in place over `{key}` keys, such as `lo..hi`")]
    OrderedRangeArgument {
        /// Rendered key type of the map.
        key: String,
    },
    /// A call to a function with a const generic parameter gave no value
    /// for it: no turbofish names it and no array argument's length does.
    #[error("cannot infer the const generic argument of `{callee}`")]
    ConstGenericNotInferred {
        /// The callee as written.
        callee: String,
        /// The type a struct literal or an enum variant builds, which names
        /// the argument where the value is bound; `None` for a function call.
        literal_ty: Option<String>,
        /// The type an associated function is called through, whose turbofish
        /// names the argument; `None` for any other call.
        assoc_owner: Option<String>,
    },
    /// `String::parse` was called. The strict `to_T()` family is the one
    /// parse surface, and it answers an `Option<T>`.
    #[error("`parse` is not a string method; the parse family is `to_i64` / `to_f64` / `to_bool`")]
    StringParseRetired {
        /// The `to_T()` spelling this site wants.
        suggestion: String,
    },
    /// An `enum Name : uN` named a width too narrow for its variants.
    #[error("`{name}` declares {count} variants, which `u{bits}` cannot represent")]
    EnumReprTooNarrow {
        /// The enum's name.
        name: String,
        /// The declared width, in bits.
        bits: u32,
        /// How many variants the declaration has.
        count: usize,
    },
    /// A `Box<T>` / `Arc<T>` / `Rc<T>` wrapper was written. Every value is
    /// already heap-shared and reference-counted, so the wrapper names no
    /// distinction the language draws.
    #[error("`{wrapper}<{inner}>` is `{inner}`; the language has no pointer wrappers")]
    TransparentWrapper {
        /// The wrapper as written.
        wrapper: String,
        /// The inner type it wrapped.
        inner: String,
    },
    /// An assignment targets an expression that names no place: a
    /// literal, a call result, or another temporary. Only a binding, a
    /// field, an index, or a dereference is writable.
    #[error("cannot assign to `{target}`")]
    InvalidAssignTarget {
        /// Rendering of the offending target expression.
        target: String,
    },
    /// An assignment targets a place reached through a shared `&T`
    /// reference. The reference binding's own `mut` qualifier cannot make
    /// its referent writable.
    #[error("cannot assign through shared reference `{name}`")]
    AssignThroughSharedReference {
        /// Name of the shared reference at the root of the place.
        name: String,
    },
    /// A mutable reference was requested for a place rooted at an immutable
    /// binding. Mutable references require a writable source place.
    #[error("cannot take a mutable reference to immutable binding `{name}`")]
    MutableReferenceToImmutable {
        /// Name of the immutable root binding.
        name: String,
    },
    /// A second named mutable reference would overlap an existing one.
    #[error("cannot take mutable reference to `{root}` while `{borrower}` is active")]
    MutableReferenceConflict {
        /// Root binding being borrowed again.
        root: String,
        /// Earlier named mutable-reference binding.
        borrower: String,
    },
    /// A reference was placed in storage or crossed a boundary where the
    /// current runtime cannot keep its referent valid.
    #[error("reference cannot {context}")]
    ReferenceEscapeUnsupported {
        /// Source-level boundary that would let the reference escape.
        context: String,
    },
    /// An access conflicts with a lexically active reference.
    #[error("cannot {action} `{root}` while reference `{borrower}` is active")]
    BorrowedPlaceConflict {
        /// Root binding protected by the active reference.
        root: String,
        /// Binding holding the active reference.
        borrower: String,
        /// Rejected operation, such as read or mutate.
        action: &'static str,
    },
    /// A reference pattern attempted to copy an aggregate referent.
    #[error("reference pattern cannot bind aggregate value `{ty}` by value")]
    ReferencePatternAggregateUnsupported {
        /// Aggregate referent type.
        ty: String,
    },
    /// A concurrency boundary attempted a by-value aggregate shape whose
    /// compiled publication ABI cannot preserve all child ownership.
    #[error(
        "by-value aggregate `{ty}` cannot {boundary} with the current compiled concurrency ABI"
    )]
    ConcurrentAggregateUnsupported {
        /// Aggregate type whose nested Vec storage cannot be published safely.
        ty: String,
        /// Concurrency boundary being crossed.
        boundary: &'static str,
    },
    /// A goroutine closure captured a binding whose type has no
    /// representation that crosses the boundary, so both sides would reach
    /// the same nested storage with no synchronisation behind it.
    #[error(
        "`{name}` is a `{ty}`, which cannot be captured by a goroutine: its nested storage has no shared representation"
    )]
    ConcurrentCaptureUnsupported {
        /// The captured binding's name, as written.
        name: String,
        /// The binding's type.
        ty: String,
    },
    /// `sync::Shared::new` was handed a value whose type the guarded slot
    /// cannot carry on every tier.
    #[error("`sync::Shared` cannot guard a `{ty}`")]
    SharedPayloadUnsupported {
        /// The type offered to `Shared::new`.
        ty: String,
    },
    /// A call omitted `&mut` for a parameter that may modify its argument.
    /// Mutable access must be visible at the call site as `&mut place`.
    #[error(
        "argument `{argument}` may be modified by this call; ensure its source uses `let mut` and pass it as `&mut {argument}`"
    )]
    MutableArgumentRequiresReference {
        /// Source-like spelling of the argument place.
        argument: String,
    },
}

impl TypeError {
    /// Returns a short stable tag useful for snapshot tests.
    // A stable tag per variant is a data table, not control flow: the lint
    // counts its rows as function length, and splitting the table would cost
    // the exhaustiveness that makes a new variant fail to compile here.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::TypeMismatch { .. } => "type-mismatch",
            Self::NumericOperandMismatch { .. } => "numeric-operand-mismatch",
            Self::OptionValueMismatch { .. } => "option-value-mismatch",
            Self::ArgumentTypeMismatch { .. } => "argument-type-mismatch",
            Self::SlotCollectionElement { .. } => "slot-collection-element",
            Self::ContainerIgnoresUserOrder { .. } => "container-ignores-user-order",
            Self::SpawnOutsideCohort { .. } => "spawn-outside-cohort",
            Self::UnresolvedMethod { .. } => "unresolved-method",
            Self::PrivateMethod { .. } => "private-method",
            Self::PrivateField { .. } => "private-field",
            Self::UnresolvedOp { .. } => "unresolved-op",
            Self::UnresolvedOpImpl { .. } => "unresolved-op-impl",
            Self::NonExhaustiveMatch { .. } => "non-exhaustive-match",
            Self::CannotAssignToLiteral => "cannot-assign-to-literal",
            Self::LetPatternMayNotMatch => "let-pattern-may-not-match",
            Self::ReferencePatternRequiresReference { .. } => {
                "reference-pattern-requires-reference"
            }
            Self::ReferenceParameterPatternPosition { .. } => {
                "reference-parameter-pattern-position"
            }
            Self::UnsizedSliceValue { .. } => "unsized-slice-value",
            Self::SequenceResizeRequiresVec { .. } => "sequence-resize-requires-vec",
            Self::ArrayLengthNotConstant => "array-length-not-constant",
            Self::StructPatternNameRequired { .. } => "struct-pattern-name-required",
            Self::StructConstructorBracesRequired { .. } => "struct-constructor-braces-required",
            Self::TupleStructConstructorParenthesesRequired { .. } => {
                "tuple-struct-constructor-parentheses-required"
            }
            Self::NamedStructFieldsRequired { .. } => "named-struct-fields-required",
            Self::MissingStructField { .. } => "missing-struct-field",
            Self::DuplicateStructField { .. } => "duplicate-struct-field",
            Self::TooManyStructFields { .. } => "too-many-struct-fields",
            Self::InvalidCast { .. } => "invalid-cast",
            Self::NoConversion { .. } => "no-conversion",
            Self::ConversionTargetUnknown { .. } => "conversion-target-unknown",
            Self::UnknownField { .. } => "unknown-field",
            Self::DiscardedResult => "discarded-result",
            Self::DiscardedMustUse { .. } => "discarded-must-use",
            Self::IterableWrapper { .. } => "iterable-wrapper",
            Self::RecursionLimit { .. } => "recursion-limit",
            Self::CyclicTypeAlias { .. } => "cyclic-type-alias",
            Self::UnsupportedDerive { .. } => "unsupported-derive",
            Self::IntLiteralOverflow { .. } => "int-literal-overflow",
            Self::InvalidEscape { .. } => "invalid-escape",
            Self::UnknownTraitBound { .. } => "unknown-trait-bound",
            Self::UnknownImplTrait { .. } => "unknown-impl-trait",
            Self::AutomaticTraitImpl { .. } => "automatic-trait-impl",
            Self::TraitInTypePosition { .. } => "trait-in-type-position",
            Self::TraitBoundNotSatisfied { .. } => "trait-bound-not-satisfied",
            Self::MethodNotOnBound { .. } => "method-not-on-bound",
            Self::OperatorNotOnBound { .. } => "operator-not-on-bound",
            Self::MissingTraitImplMethods { .. } => "missing-trait-impl-methods",
            Self::MissingTraitImplAssocItems { .. } => "missing-trait-impl-assoc-items",
            Self::ImplItemNotInTrait { .. } => "impl-item-not-in-trait",
            Self::ConflictingTraitImpl { .. } => "conflicting-trait-impl",
            Self::UndeclaredReturnValue { .. } => "undeclared-return-value",
            Self::RangeBorrow { .. } => "range-borrow",
            Self::UnknownAssocItem { .. } => "unknown-assoc-item",
            Self::AmbiguousAssocItem { .. } => "ambiguous-assoc-item",
            Self::BuiltinIteratorNotGeneric { .. } => "builtin-iterator-not-generic",
            Self::TooManyVariants { .. } => "too-many-variants",
            Self::ClosureParamUninferred { .. } => "closure-param-uninferred",
            Self::GenericReturnTypeUninferred { .. } => "generic-return-type-uninferred",
            Self::QuestionMarkUnsupported { .. } => "question-mark-unsupported",
            Self::Int128Unsupported { .. } => "int128-unsupported",
            Self::StdFnValueUnsupported { .. } => "std-fn-value-unsupported",
            Self::IteratorStateFormatted => "iterator-state-formatted",
            Self::ValueNotDisplayable { .. } => "value-not-displayable",
            Self::IteratorStateConsumed { .. } => "iterator-state-consumed",
            Self::JsonNotSerializable { .. } => "json-not-serializable",
            Self::CallArityMismatch { .. } => "call-arity-mismatch",
            Self::UnknownVariant { .. } => "unknown-variant",
            Self::SupertraitMethodThroughBound { .. } => "supertrait-method-through-bound",
            Self::NotIndexable { .. } => "not-indexable",
            Self::NotCallable { .. } => "not-callable",
            Self::NoTupleField { .. } => "no-tuple-field",
            Self::TransparentWrapper { .. } => "transparent-wrapper",
            Self::StringParseRetired { .. } => "string-parse-retired",
            Self::WrappingMethodRetired { .. } => "wrapping-method-retired",
            Self::ConstGenericNotInferred { .. } => "const-generic-not-inferred",
            Self::SimdShape { .. } => "simd-shape",
            Self::OrderedRangeArgument { .. } => "ordered-range-argument",
            Self::ReferenceArgumentNeedsDeref { .. } => "reference-argument-needs-deref",
            Self::DerefWriteToNonReference { .. } => "deref-write-to-non-reference",
            Self::EnumReprTooNarrow { .. } => "enum-repr-too-narrow",
            Self::JsonValuePatternUnsupported { .. } => "json-value-pattern-unsupported",
            Self::WeakDowngradeNonRc { .. } => "weak-downgrade-non-rc",
            Self::CombinatorDataArgMismatch { .. } => "combinator-data-arg-mismatch",
            Self::AssignToImmutable { .. } => "assign-to-immutable",
            Self::AssignThroughSharedReference { .. } => "assign-through-shared-reference",
            Self::InvalidAssignTarget { .. } => "invalid-assign-target",
            Self::MutableReferenceToImmutable { .. } => "mutable-reference-to-immutable",
            Self::MutableReferenceConflict { .. } => "mutable-reference-conflict",
            Self::ReferenceEscapeUnsupported { .. } => "reference-escape-unsupported",
            Self::BorrowedPlaceConflict { .. } => "borrowed-place-conflict",
            Self::ReferencePatternAggregateUnsupported { .. } => {
                "reference-pattern-aggregate-unsupported"
            }
            Self::ConcurrentAggregateUnsupported { .. } => "concurrent-aggregate-unsupported",
            Self::ConcurrentCaptureUnsupported { .. } => "concurrent-capture-unsupported",
            Self::SharedPayloadUnsupported { .. } => "shared-payload-unsupported",
            Self::MutableArgumentRequiresReference { .. } => "mutable-argument-requires-reference",
        }
    }

    /// Whether this error names a type that already failed to check.
    ///
    /// Such a type renders as `<error>`, a spelling that appears nowhere in
    /// the source, so a diagnostic carrying it is a follow-on report of a
    /// failure already described.
    #[must_use]
    pub fn mentions_error_type(&self) -> bool {
        format!("{self}").contains("<error>")
    }

    /// Stable error code used by the diagnostics framework.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TypeMismatch { .. } => "GT0001",
            Self::NumericOperandMismatch { .. } => "GT0001",
            Self::OptionValueMismatch { .. } => "GT0001",
            Self::ArgumentTypeMismatch { .. } => "GT0001",
            Self::SlotCollectionElement { .. } => "GT0068",
            Self::ContainerIgnoresUserOrder { .. } => "GT0085",
            Self::SpawnOutsideCohort { .. } => "GT0086",
            Self::WrappingMethodRetired { .. } => "GT0087",
            Self::ConstGenericNotInferred { .. } => "GT0088",
            Self::SimdShape { .. } => "GT0089",
            Self::OrderedRangeArgument { .. } => "GT0091",
            Self::UnresolvedMethod { .. } => "GT0002",
            Self::UnresolvedOp { .. } | Self::UnresolvedOpImpl { .. } => "GT0003",
            Self::NonExhaustiveMatch { .. } => "GT0004",
            Self::CannotAssignToLiteral | Self::LetPatternMayNotMatch => "GT0047",
            Self::ReferencePatternRequiresReference { .. } => "GT0048",
            Self::ReferenceParameterPatternPosition { .. } => "GT0069",
            Self::UnsizedSliceValue { .. } => "GT0049",
            Self::SequenceResizeRequiresVec { .. } => "GT0050",
            Self::ArrayLengthNotConstant => "GT0051",
            Self::StructPatternNameRequired { .. } => "GT0033",
            Self::StructConstructorBracesRequired { .. }
            | Self::TupleStructConstructorParenthesesRequired { .. }
            | Self::NamedStructFieldsRequired { .. } => "GT0034",
            Self::MissingStructField { .. } => "GT0035",
            Self::DuplicateStructField { .. } => "GT0036",
            Self::TooManyStructFields { .. } => "GT0037",
            Self::InvalidCast { .. } => "GT0005",
            Self::NoConversion { .. } => "GT0066",
            Self::ConversionTargetUnknown { .. } => "GT0066",
            Self::UnknownField { .. } => "GT0006",
            Self::DiscardedResult => "GT0007",
            Self::DiscardedMustUse { .. } => "GT0064",
            Self::IterableWrapper { .. } => "GT0067",
            Self::RecursionLimit { .. } => "GT0008",
            Self::CyclicTypeAlias { .. } => "GT0024",
            Self::UnsupportedDerive { .. } => "GT0025",
            Self::IntLiteralOverflow { .. } => "GT0009",
            Self::InvalidEscape { .. } => "GT0010",
            Self::UnknownTraitBound { .. } => "GT0011",
            Self::UnknownImplTrait { .. } => "GT0070",
            Self::AutomaticTraitImpl { .. } => "GT0084",
            Self::TraitInTypePosition { .. } => "GT0071",
            Self::TooManyVariants { .. } => "GT0012",
            Self::ClosureParamUninferred { .. } => "GT0013",
            Self::GenericReturnTypeUninferred { .. } => "GT0044",
            Self::QuestionMarkUnsupported { .. } => "GT0045",
            Self::Int128Unsupported { .. } => "GT0014",
            Self::StdFnValueUnsupported { .. } => "GT0015",
            Self::IteratorStateFormatted => "GT0041",
            Self::ValueNotDisplayable { .. } => "GT0062",
            Self::PrivateMethod { .. } => "GT0063",
            Self::PrivateField { .. } => "GT0065",
            Self::IteratorStateConsumed { .. } => "GT0042",
            Self::JsonNotSerializable { .. } => "GT0016",
            Self::TraitBoundNotSatisfied { .. } => "GT0017",
            Self::MethodNotOnBound { .. } | Self::OperatorNotOnBound { .. } => "GT0056",
            Self::MissingTraitImplMethods { .. } => "GT0058",
            Self::MissingTraitImplAssocItems { .. } => "GT0059",
            Self::ImplItemNotInTrait { .. } => "GT0072",
            Self::ConflictingTraitImpl { .. } => "GT0073",
            Self::UndeclaredReturnValue { .. } => "GL0055",
            Self::RangeBorrow { .. } => "GT0075",
            Self::UnknownAssocItem { .. } => "GT0060",
            Self::AmbiguousAssocItem { .. } => "GT0061",
            Self::BuiltinIteratorNotGeneric { .. } => "GT0057",
            Self::CallArityMismatch { .. } => "GT0018",
            Self::UnknownVariant { .. } => "GT0019",
            Self::SupertraitMethodThroughBound { .. } => "GT0020",
            Self::NotIndexable { .. } => "GT0021",
            Self::NotCallable { .. } => "GT0022",
            Self::NoTupleField { .. } => "GT0023",
            Self::JsonValuePatternUnsupported { .. } => "GT0027",
            Self::WeakDowngradeNonRc { .. } => "GT0028",
            Self::CombinatorDataArgMismatch { .. } => "GT0029",
            Self::AssignToImmutable { .. } => "GT0030",
            Self::AssignThroughSharedReference { .. } => "GT0031",
            Self::InvalidAssignTarget { .. } => "GT0078",
            Self::TransparentWrapper { .. } => "GT0079",
            Self::StringParseRetired { .. } => "GT0080",
            Self::ReferenceArgumentNeedsDeref { .. } => "GT0082",
            Self::DerefWriteToNonReference { .. } => "GT0083",
            Self::EnumReprTooNarrow { .. } => "GT0081",
            Self::MutableReferenceToImmutable { .. } => "GT0032",
            Self::MutableReferenceConflict { .. } => "GT0043",
            Self::ReferenceEscapeUnsupported { .. } => "GT0052",
            Self::BorrowedPlaceConflict { .. } => "GT0053",
            Self::ReferencePatternAggregateUnsupported { .. } => "GT0054",
            Self::ConcurrentAggregateUnsupported { .. } => "GT0055",
            Self::ConcurrentCaptureUnsupported { .. } => "GT0076",
            Self::SharedPayloadUnsupported { .. } => "GT0077",
            Self::MutableArgumentRequiresReference { .. } => "GT0046",
        }
    }
}

/// Maps the most common `expected X, found Y` pairs to a one-line
/// "did you mean" hint. Pure string compare on the rendered types -
/// keeps the table small and avoids re-deriving structure here.
/// Attaches the GT0014 help + note to an `Int128Unsupported`
/// diagnostic. Split out of `to_diagnostic` to keep that match
/// within the line-count lint budget.
fn int128_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    ty: &str,
) -> gossamer_diagnostics::Diagnostic {
    out.with_help("use `i64` / `u64` or split the value into two 64-bit halves")
        .with_note(format!(
            "`{ty}` has no 128-bit runtime representation on any tier (VM, JIT, or \
             compiled tier)"
        ))
}

fn mismatch_suggestion(expected: &str, found: &str) -> Option<String> {
    if expected.starts_with("Vec<") && found.starts_with("Iterator<") {
        return Some(
            "consume the iterator with `iter::collect(<expr>)` at this materialization boundary"
                .to_string(),
        );
    }
    if expected.starts_with("Iterator<") && found.starts_with("Vec<") {
        return Some(
            "start the lazy pipeline with `<expr>.iter()`, or pass the Vec to a lazy adapter"
                .to_string(),
        );
    }
    // Numeric width: i32 to i64, u32 to u64, and so on.
    let int_suffixes = [
        "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "isize", "usize",
    ];
    let float_suffixes = ["f32", "f64"];
    if int_suffixes.contains(&expected) && int_suffixes.contains(&found) {
        return Some(format!("cast explicitly with `<expr> as {expected}`"));
    }
    // Integer and float operands never widen into each other implicitly.
    if (int_suffixes.contains(&expected) && float_suffixes.contains(&found))
        || (float_suffixes.contains(&expected) && int_suffixes.contains(&found))
    {
        return Some(format!("cast explicitly with `<expr> as {expected}`"));
    }
    // T to Option<T>
    if let Some(inner) = expected
        .strip_prefix("Option<")
        .and_then(|s| s.strip_suffix('>'))
    {
        if inner == found {
            return Some(format!(
                "did you mean to wrap with `Some(<expr>)` to lift `{inner}` into `Option<{inner}>`?"
            ));
        }
    }
    // Option<T> to T
    if let Some(inner) = found
        .strip_prefix("Option<")
        .and_then(|s| s.strip_suffix('>'))
    {
        if inner == expected {
            return Some(
                "unwrap it with `<expr>.unwrap_or(<default>)`, or bind with \
                 `if let Some(value) = <expr>`"
                    .to_string(),
            );
        }
    }
    // Result<T, _> to T (handler returned a Result, caller wanted the inner value)
    if found.starts_with("Result<") && !expected.starts_with("Result<") {
        return Some(
            "did you mean to propagate with `?` (`<expr>?`) to unwrap the `Result`?".to_string(),
        );
    }
    // &T vs T
    if let Some(rest) = found.strip_prefix('&') {
        if rest == expected {
            return Some(format!(
                "did you mean to dereference with `*<expr>` to get `{expected}`?"
            ));
        }
    }
    if let Some(rest) = expected.strip_prefix('&') {
        if rest == found {
            return Some(format!(
                "did you mean to take a reference with `&<expr>` to get `&{found}`?"
            ));
        }
    }
    None
}

impl TypeDiagnostic {
    /// Renders this diagnostic as a structured
    /// [`gossamer_diagnostics::Diagnostic`] for the new error frame.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per diagnostic variant; splitting scatters the help text"
    )]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let location = Location::new(self.span.file, self.span);
        let title = format!("{}", self.error);
        let mut out = if self.is_advisory() {
            Diagnostic::warning(Code(self.error.code()), title.clone())
        } else {
            Diagnostic::error(Code(self.error.code()), title.clone())
        }
        .with_primary(location, title);
        match &self.error {
            // Both titles already carry the expected and found types, so a
            // note repeating them would double the diagnostic's length
            // without adding anything the reader can act on.
            TypeError::TypeMismatch { expected, found }
            | TypeError::ArgumentTypeMismatch {
                expected, found, ..
            } => {
                if let Some(suggestion) = mismatch_suggestion(expected, found) {
                    out = out.with_help(suggestion);
                }
            }
            TypeError::NumericOperandMismatch { cast, .. } => {
                out = out.with_help(format!("cast explicitly: `{cast}`"));
            }
            TypeError::OptionValueMismatch {
                actual,
                binding,
                default,
                ..
            } => {
                out = out.with_help(format!(
                    "unwrap it with `{actual}.unwrap_or({default})`, or bind with \
                     `if let Some({binding}) = {actual}`"
                ));
            }
            TypeError::SlotCollectionElement { owner, found } => {
                out = if matches!(found.as_str(), "u64" | "usize") {
                    out.with_help(format!(
                        "`{owner}` orders its elements as signed 64-bit values, which \
                         `{found}` outruns; hold `i64`, or keep the values in a \
                         `Deque` / `Queue` / `Stack` and order them yourself"
                    ))
                } else {
                    out.with_help(format!(
                        "`{owner}` orders every element it holds, and `{found}` has \
                         no order; hold it in a `Deque` / `Queue` / `Stack`, or key \
                         the heap by something the language orders"
                    ))
                    .with_note(
                        "a heap orders scalars and `String` by value, and tuples, \
                         structs, sequences, `Option` / `Result`, and enums by their \
                         parts; a `Map` and a `Set` have no element order",
                    )
                };
            }
            TypeError::UnresolvedMethod {
                ty,
                name,
                available,
                field_of_same_name,
                free_fn_of_same_name,
            } => {
                out = if (ty.starts_with("Set<") || ty.starts_with("BTreeSet<"))
                    && crate::is_iterator_method(name)
                {
                    out.with_help(format!(
                        "`{name}` is an iterator operation: write `<set>.iter().{name}(..)`"
                    ))
                    .with_note(
                        "a set answers membership, cardinality, and set algebra; \
                         traversal goes through `iter()`",
                    )
                } else if name == "set" && ty.starts_with("Map") {
                    out.with_help(format!("`{ty}` writes with `insert(key, value)`"))
                        .with_note(
                            "`set` is the `json::Value` field-update helper, not a map method",
                        )
                } else if is_callable_ty_spelling(ty) {
                    out.with_help(format!(
                        "call it first and reach `{name}` on what it answers: `<call>(..).{name}(..)`"
                    ))
                    .with_note("a callable is a code address, not data, and declares no methods")
                } else if *free_fn_of_same_name {
                    out.with_help(format!(
                        "`{name}` is a function, so it is written as the free call \
                         `{name}(value)`"
                    ))
                    .with_note("a receiver reaches only the methods its own type declares")
                } else if *field_of_same_name {
                    out.with_help(format!(
                        "`{name}` is a field of `{ty}`, so it is read without parentheses: \
                         `value.{name}` - and `|v| v.{name}` where a callback is expected"
                    ))
                } else {
                    unresolved_method_diagnostic(out, ty, name, available)
                };
            }
            TypeError::PrivateMethod { ty, name, module } => {
                out = out.with_help(format!(
                    "only `{module}` and the modules under it can call `{name}` as it is \
                     declared; write `pub` on the method for callers anywhere, \
                     `pub (package)` for the rest of this package, or reach it through a \
                     `pub` method of `{ty}`"
                ));
            }
            TypeError::UnresolvedOp { op, lhs, rhs } => {
                out = out.with_note(format!(
                    "operator `{op}` requires matching operand types; got `{lhs}` and `{rhs}`"
                ));
            }
            TypeError::UnresolvedOpImpl {
                op,
                trait_name,
                method,
                ty,
            } => {
                let note = if trait_name == "Neg" {
                    format!("user types support unary `{op}` through an `impl {trait_name}`")
                } else {
                    format!(
                        "user types support `{op}` through an `impl {trait_name}`, dispatched on the left operand"
                    )
                };
                out = out.with_note(note).with_help(format!(
                    "implement `impl {trait_name} for {ty} {{ fn {method}(..) -> .. }}`"
                ));
            }
            TypeError::NonExhaustiveMatch { missing } => {
                out = out
                    .with_help(format!("add an arm for: {missing}"))
                    .with_note("match expressions must cover every possible value");
            }
            TypeError::CannotAssignToLiteral => {
                out = out
                    .with_help(
                        "assign the value to a name instead, for example `let value = 8`",
                    )
                    .with_note(
                        "literals are values, not variables; use `if let`, `match`, or `let ... else` to compare with a literal pattern",
                    );
            }
            TypeError::LetPatternMayNotMatch => {
                out = out
                    .with_help(
                        "use a name or `_` to bind every value, or use `if let`, `match`, or `let ... else` to test the pattern",
                    )
                    .with_note("a plain `let` must accept every possible initializer value");
            }
            TypeError::ReferencePatternRequiresReference { pattern } => {
                out = out
                    .with_help(format!(
                        "write `let name = {pattern} value` to borrow the value, or remove `{pattern}` to bind it directly"
                    ))
                    .with_note(format!(
                        "`let {pattern} name = value` can only destructure an existing {pattern} reference"
                    ));
            }
            TypeError::ReferenceParameterPatternPosition {
                pattern,
                binding,
                reference_ty,
                ..
            } => {
                out = out
                    .with_help(format!(
                        "write `{binding}: {reference_ty}` to take a reference, or remove `{pattern}` to take the value"
                    ))
                    .with_note(format!(
                        "a parameter's reference is declared in its type; `{pattern}` in the pattern destructures a reference the type already names"
                    ));
            }
            TypeError::UnsizedSliceValue { .. } => {
                out = out
                    .with_help(
                        "borrow a sequence as `&[T]` or `&mut [T]`, or use `Vec<T>` for an owned growable sequence",
                    )
                    .with_note(
                        "`[T]` is an unsized view; `[T; N]` is an owned fixed-size array and `Vec<T>` is an owned growable collection",
                    );
            }
            TypeError::SequenceResizeRequiresVec { ty, method } => {
                out = out
                    .with_help(format!(
                        "use `Vec<T>` when `{method}` must change the sequence length or capacity"
                    ))
                    .with_note(format!(
                        "`{ty}` has fixed storage; arrays and slices only expose non-resizing sequence methods"
                    ));
            }
            TypeError::ArrayLengthNotConstant => {
                out = out
                    .with_help("use a constant length for `[value; N]`, or build a `Vec<T>` explicitly")
                    .with_note("array lengths are part of the fixed array's type and must be known during compilation");
            }
            TypeError::StructPatternNameRequired { ty } => {
                out = out
                    .with_help(format!("write `{ty} {{ field, .. }}` with its declared fields"))
                    .with_note(
                        "parenthesized patterns destructure tuples only; structs are nominal and use named-field patterns",
                    );
            }
            TypeError::StructConstructorBracesRequired { name } => {
                out = out
                    .with_help(format!("write `{name} {{ field: value, ... }}`"))
                    .with_note(
                        "named structs use braced construction; tuple structs use parentheses",
                    );
            }
            TypeError::TupleStructConstructorParenthesesRequired { name } => {
                out = out
                    .with_help(format!("write `{name}(...)` in declared field order"))
                    .with_note(
                        "tuple structs use parenthesized construction; named structs use braces",
                    );
            }
            TypeError::NamedStructFieldsRequired { name } => {
                out = out
                    .with_help(format!("write `{name} {{ field: value, ... }}`"))
                    .with_note("named structs do not accept positional initializers");
            }
            TypeError::MissingStructField { name, field } => {
                out = out.with_help(format!("add `{field}: ...` to the `{name}` initializer"));
            }
            TypeError::DuplicateStructField { name, field } => {
                out = out.with_help(format!(
                    "remove the duplicate `{field}` initializer for `{name}`"
                ));
            }
            TypeError::TooManyStructFields {
                name,
                expected,
                found,
            } => {
                out = out
                    .with_help(format!(
                        "`{name}` declares {expected} fields, but this initializer supplies {found}"
                    ))
                    .with_note(
                        "positional entries in a named-struct literal fill the next unfilled declared field",
                    );
            }
            TypeError::InvalidCast { from, to } => {
                out = out
                    .with_help(
                        "`as` is restricted to numeric-to-numeric, `bool` / `char` to integer, `u8` to `char`, and no-op same-type casts",
                    )
                    .with_note(format!("cannot cast `{from}` to `{to}`"));
            }
            TypeError::NoConversion {
                from,
                to,
                borrowed_sequence,
            } => {
                if *borrowed_sequence {
                    out = out.with_help(
                        "copy the sequence with `.to_vec()`; `.into()` takes the array itself"
                            .to_string(),
                    );
                } else if from == to {
                    out = out.with_help(format!(
                        "`.into()` here converts `{to}` to itself and nothing backs it; remove it - the value is already `{to}`"
                    ));
                } else {
                    out = out
                        .with_help(format!("write `impl From<{from}> for {to}`"))
                        .with_note(
                            "an opaque alias converts to and from its own representation with `.into()`; every other pair needs a `From` impl"
                                .to_string(),
                        );
                }
            }
            TypeError::ConversionTargetUnknown { method } => {
                out = out
                    .with_help(format!(
                        "give the call a target: annotate the binding, pass it to a typed parameter, or return it - `let x: Target = value.{method}()`"
                    ))
                    .with_note(
                        "the target of a conversion comes from the use site, never from the receiver"
                            .to_string(),
                    );
            }
            TypeError::UnknownField {
                ty,
                field,
                opaque,
                declared,
                field_span,
                method_of_same_name,
            } => {
                out = unknown_field_diagnostic(
                    out,
                    self.span.file,
                    UnknownFieldParts {
                        ty,
                        field,
                        opaque: *opaque,
                        declared,
                        field_span: *field_span,
                        method_of_same_name: *method_of_same_name,
                    },
                );
            }
            TypeError::TooManyVariants { .. } => {
                out = out.with_help(
                    "the heap enum discriminant is one byte; split the enum or group variants into nested enums",
                );
            }
            TypeError::DiscardedResult => out = discarded_result_diagnostic(out),
            TypeError::PrivateField { name, .. } => {
                out = out.with_help(format!(
                    "write `pub(package)` on field `{name}` to reach it from anywhere in \
                     this package, or `pub` to expose it beyond the package"
                ));
            }
            TypeError::DiscardedMustUse { .. } => {
                out = out
                    .with_note("`#[must_use]` marks a value whose whole point is the value")
                    .with_help("bind it with `let`, consume it, or discard it explicitly with `let _ = <expr>`");
            }
            TypeError::IterableWrapper { name, taken } => {
                out = out
                    .with_note(format!(
                        "`{name}` holds at most one value and carries no element type, \
                         so the loop binding is unconstrained and the body never runs"
                    ))
                    .with_help(format!(
                        "take the value first with {taken}, then iterate it"
                    ));
            }
            TypeError::TraitBoundNotSatisfied { ty, bound } => {
                out = out.with_help(format!(
                    "add `impl {bound} for {ty} {{ ... }}`, or pass a type that already implements `{bound}`"
                ));
            }
            TypeError::MethodNotOnBound {
                param,
                method,
                bounds,
            } => {
                out = out.with_help(match bounds.as_slice() {
                    [] => format!(
                        "`{param}` has no trait bound, so it has no methods; write \
                         `<{param}: SomeTrait>` where `SomeTrait` declares `fn {method}`"
                    ),
                    [only] => format!(
                        "`{param}` can only do what `{only}` declares; add `fn {method}` to \
                         `{only}`, or bound `{param}` by a trait that declares it"
                    ),
                    _ => format!(
                        "none of `{}` declares `fn {method}`; bound `{param}` by a trait \
                         that does",
                        bounds.join("`, `")
                    ),
                });
            }
            TypeError::OperatorNotOnBound {
                param,
                op,
                trait_name,
                method,
                bounds,
            } => {
                out = out
                    .with_help(format!(
                        "bound the parameter as `{param}: {trait_name}` so every instantiation \
                         supplies `{op}`"
                    ))
                    .with_note(match bounds.as_slice() {
                        [] => format!("`{param}` has no trait bound, so it has no `fn {method}`"),
                        _ => format!("none of `{}` declares `fn {method}`", bounds.join("`, `")),
                    });
            }
            TypeError::MissingTraitImplMethods {
                trait_name,
                ty,
                missing,
            } => {
                out = out
                    .with_help(format!(
                        "add {} to `impl {trait_name} for {ty}`, or give {} a default body in \
                         `trait {trait_name}`",
                        missing
                            .iter()
                            .map(|m| format!("`fn {m}`"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        if missing.len() == 1 { "it" } else { "them" },
                    ))
                    .with_note(
                        "a call through the trait lowers to a direct call to each declared method",
                    );
            }
            TypeError::MissingTraitImplAssocItems {
                trait_name,
                ty,
                missing,
            } => {
                out = out
                    .with_help(format!(
                        "add {} to `impl {trait_name} for {ty}`, or give {} a default in \
                         `trait {trait_name}`",
                        missing
                            .iter()
                            .map(|m| format!("`{m} = ...`"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        if missing.len() == 1 { "it" } else { "them" },
                    ))
                    .with_note(
                        "a projection through the trait has to land on a concrete item in this impl",
                    );
            }
            TypeError::ImplItemNotInTrait {
                trait_name,
                ty,
                item,
                declared,
            } => {
                out = out
                    .with_help(format!(
                        "move it to an inherent `impl {ty} {{ fn {item} .. }}` block, or \
                         declare `fn {item}` in `trait {trait_name}`"
                    ))
                    .with_note(match declared.as_slice() {
                        [] => format!("`{trait_name}` declares no items of its own"),
                        _ => format!("`{trait_name}` declares: `{}`", declared.join("`, `")),
                    });
            }
            TypeError::ConflictingTraitImpl {
                trait_name,
                ty,
                derived,
            } => {
                out = out.with_help(if *derived {
                    format!(
                        "`{ty}` already gets `{trait_name}` from its `#[derive({trait_name})]`; \
                         drop the derive to keep the written `impl`, or drop the `impl`"
                    )
                } else {
                    format!(
                        "keep one `impl {trait_name} for {ty}` block and merge the other into it"
                    )
                });
            }
            TypeError::RangeBorrow { base, range, .. } => {
                out = out
                    .with_note(
                        "a sequence's elements live in its own buffer; a borrow of part of \
                         one would have to alias that buffer, which no value shape carries yet",
                    )
                    .with_help(format!(
                        "read a copy with `{base}[{range}]` or `{base}.slice(..)`, or edit in \
                         place with `copy_within` / `copy_from_slice` / an indexed write"
                    ));
            }
            TypeError::UndeclaredReturnValue { name, found } => {
                out = out
                    .with_help(format!(
                        "return it: `fn {name}(..) -> {found}`, or say the discard is \
                         deliberate: `fn {name}(..) -> ()`"
                    ))
                    .with_note("lint: undeclared_return_value");
            }
            TypeError::UnknownAssocItem {
                base,
                name,
                declared,
            } => {
                out = out.with_help(match declared.as_slice() {
                    [] => format!("nothing in scope declares an associated `{name}` for `{base}`"),
                    _ => format!("available here: `{}`", declared.join("`, `")),
                });
            }
            TypeError::AmbiguousAssocItem {
                base,
                trait_name,
                name,
                kind,
            } => {
                let help = if *kind == "type" {
                    format!(
                        "write `{base}: {trait_name}<{name} = ...>` to pin it, or name the \
                         concrete type instead of `{base}`"
                    )
                } else {
                    format!(
                        "name the concrete type instead of `{base}`, or give `{trait_name}` a \
                         default `const {name}`"
                    )
                };
                out = out.with_help(help).with_note(format!(
                    "several impls of `{trait_name}` supply `{name}` and it has no default"
                ));
            }
            TypeError::BuiltinIteratorNotGeneric { ty } => {
                out = out.with_help(format!(
                    "name the iterator on the parameter instead, as in `fn f(it: {ty})`,                      which every tier lowers"
                ));
            }
            TypeError::RecursionLimit { .. } => {
                out = out.with_help("split the expression into smaller helpers");
            }
            TypeError::CyclicTypeAlias { name } => {
                out = out
                    .with_help(format!(
                        "`{name}` must eventually expand to a concrete type, not back to itself"
                    ))
                    .with_note("a cyclic alias has no underlying type, so every use is ill-typed");
            }
            TypeError::UnsupportedDerive { hint, .. } => {
                out = out.with_help(hint.clone());
            }
            TypeError::IntLiteralOverflow { literal, ty } => {
                out = out.with_help(format!("`{literal}` exceeds the range of `{ty}`"));
            }
            TypeError::InvalidEscape { escape, reason } => {
                out = out.with_help(format!("`{escape}` is not a valid escape: {reason}"));
            }
            TypeError::UnknownTraitBound { param, name } => {
                out = out
                    .with_help(format!(
                        "trait `{name}` is not declared anywhere; bound on `{param}` cannot be enforced",
                    ))
                    .with_note("check for a typo or import the trait into scope");
            }
            TypeError::TraitInTypePosition { name } => {
                out = out
                    .with_help(format!(
                        "a parameter takes a type; bound a generic by the trait instead, \
                         as in `fn f<T: {name}>(value: T)`"
                    ))
                    .with_note("there is no `dyn`, so a trait has no value shape of its own");
            }
            TypeError::SpawnOutsideCohort { .. } => {
                out = out
                    .with_help(
                        "open a `cohort { }` around the spawns and the code that collects \
                         from them, so the block owns every child it starts",
                    )
                    .with_note(
                        "a cohort joins every child on every exit path; without one the \
                         child attaches to whatever cohort the caller is in, and to the \
                         root cohort - whose extent is the process - when there is none",
                    );
            }
            TypeError::ContainerIgnoresUserOrder { owner, elem } => {
                out = out
                    .with_help(format!(
                        "sort a `Vec<{elem}>`, which reads the type's `cmp`, or key the \
                         container on a value that carries the order - `{owner}<i64>` beside \
                         a `Map<i64, {elem}>`"
                    ))
                    .with_note(
                        "the container orders its elements as it stores them, with no \
                         comparator to call",
                    );
            }
            TypeError::AutomaticTraitImpl {
                name,
                instead,
                reason,
                ..
            } => {
                out = out
                    .with_help(instead.clone())
                    .with_note(format!("`{name}`: {reason}"));
            }
            TypeError::UnknownImplTrait { name, ty } => {
                out = out
                    .with_help(format!(
                        "nothing declares `{name}`, so the block's methods are inherent \
                         methods on `{ty}` under a header that promises a contract"
                    ))
                    .with_note(
                        "declare the trait, fix the spelling, or write `impl <type>` \
                         with no trait name",
                    );
            }
            // The title names the combinator and spells the annotation to
            // write, so a help line here would only restate it.
            TypeError::ClosureParamUninferred { .. } => {}
            TypeError::GenericReturnTypeUninferred { callable, param } => {
                out = out
                    .with_help(format!(
                        "write `{callable}::<{param}>(...)` or give the expression an expected `Result<{param}, errors::Error>` type"
                    ))
                    .with_note("the payload type does not appear in the call arguments");
            }
            TypeError::QuestionMarkUnsupported { reason, .. } => {
                out = out
                    .with_help(
                        "use `?` on a `Result<T, E>` inside a function returning `Result<_, _>`, or on an `Option<T>` inside a function returning `Option<_>`"
                            .to_string(),
                    )
                    .with_note(reason.clone());
            }
            TypeError::Int128Unsupported { ty } => out = int128_diagnostic(out, ty),
            TypeError::StdFnValueUnsupported { path } => {
                out = std_fn_value_diagnostic(out, path);
            }
            TypeError::IteratorStateFormatted => {
                out = out.with_help(
                    "consume the iterator with `iter::collect(...)` before formatting it"
                        .to_string(),
                );
            }
            TypeError::ValueNotDisplayable { ty, class } => {
                out = value_not_displayable_diagnostic(out, ty, *class);
            }
            TypeError::IteratorStateConsumed { name, operation } => {
                out = out
                    .with_note(format!("`{operation}` takes ownership of `{name}`"))
                    .with_help("create a new iterator pipeline for another traversal".to_string());
            }
            TypeError::JsonNotSerializable { op, ty } => {
                out = json_not_serializable_diagnostic(out, op, ty);
            }
            TypeError::CallArityMismatch {
                callee, expected, ..
            } => out = arity_mismatch_diagnostic(out, callee, *expected),
            TypeError::UnknownVariant {
                enum_name,
                variant,
                declared,
            } => {
                out = unknown_variant_diagnostic(out, enum_name, variant, declared);
            }
            TypeError::SupertraitMethodThroughBound {
                method,
                bound,
                supertrait,
                ..
            } => out = supertrait_method_diagnostic(out, method, bound, supertrait),
            TypeError::NotIndexable { .. }
            | TypeError::NotCallable { .. }
            | TypeError::NoTupleField { .. } => out = structural_use_diagnostic(out, &self.error),
            TypeError::JsonValuePatternUnsupported { .. } => {
                out = out
                    .with_help(
                        "read a `json::Value` with the dynamic accessors instead: \
                         `json::as_i64` / `json::as_f64` / `json::as_str` / `json::as_bool`, \
                         `json::is_null`, `json::get(v, key)`, `json::at(v, i)`, \
                         `json::keys(v)`, `json::len(v)`",
                    )
                    .with_note(
                        "`json::Value` is an opaque dynamic-document handle with no matchable \
                         discriminant, so a `json::Value::Variant(..)` pattern falls through \
                         on the VM and faults on the compiled tiers",
                    );
            }
            TypeError::CombinatorDataArgMismatch { combinator, shape } => {
                out = out
                    .with_help(format!(
                        "write `{combinator}(f, value)` or pipe the value in: \
                         `value |> {combinator}(f)`"
                    ))
                    .with_note(format!(
                        "the data slot is the last argument; a non-`{shape}` value there \
                         makes the runtime return the empty fallback instead of applying `f`"
                    ));
            }
            TypeError::WeakDowngradeNonRc { .. } => {
                out = out
                    .with_help(
                        "call `.downgrade()` on a reference-counted aggregate (a struct, \
                         payload-bearing enum, or other heap value) - the shape that \
                         participates in cycles a `Weak<T>` breaks",
                    )
                    .with_note(
                        "a scalar / `Option` / `Result` is a by-value word with no RC header, \
                         so `Weak` of it would read a header off the value's bits and fault \
                         on the compiled tiers",
                    );
            }
            TypeError::AssignToImmutable { name } => {
                out = out
                    .with_help(format!(
                        "declare it mutable: `let mut {name} = ...` (or `mut {name}` in the \
                         parameter list)"
                    ))
                    .with_note(
                        "bindings are immutable by default; only a `mut` place can be assigned",
                    );
            }
            TypeError::EnumReprTooNarrow { bits, count, .. } => {
                let needed = gossamer_ast::EnumRepr::natural_bits(true, *count);
                out = out
                    .with_help(format!(
                        "{count} variants need {needed} bits; write `: u{needed}` or wider, \
                         or drop the width and let the compiler pick"
                    ))
                    .with_note(
                        "a declared width is exact - it is what the discriminant is stored \
                         in, so it cannot be widened silently",
                    );
                let _ = bits;
            }
            TypeError::ReferenceArgumentNeedsDeref { argument } => {
                out = out
                    .with_help(format!("write `*{argument}`"))
                    .with_note(
                        "a parameter that is not `&mut` takes the value, and a reference \
                         names one rather than being one: the deref says which",
                    )
                    .with_suggestion(gossamer_diagnostics::Suggestion::replacement(
                        location,
                        format!("write `*{argument}`"),
                        format!("*{argument}"),
                    ));
            }
            TypeError::WrappingMethodRetired {
                operator,
                replacement,
                ..
            } => {
                out = out
                    .with_help(match replacement {
                        Some(rewrite) => format!("write `{rewrite}`"),
                        None => format!("write the `{operator}` operator"),
                    })
                    .with_note(
                        "wrapping arithmetic has one spelling, the operator, which wraps at \
                         the operands' declared width on every tier",
                    );
                if let Some(rewrite) = replacement {
                    out = out.with_suggestion(gossamer_diagnostics::Suggestion::replacement(
                        location,
                        format!("write `{rewrite}`"),
                        rewrite.clone(),
                    ));
                }
            }
            TypeError::OrderedRangeArgument { .. } => {
                out = out.with_note(
                    "the bounds of a `BTreeMap` range are keys, so the range is written in the \
                     call: `m.range(lo..hi)`, `m.range(lo..=hi)`, `m.range(lo..)`, `m.range(..hi)`",
                );
            }
            TypeError::SimdShape { .. } => {
                out = out.with_note(
                    "`Simd<T, N>` takes `f32`, `f64`, `i32`, `i64`, `u8`, or `u32` lanes, \
                     `N` of 2, 4, 8, or 16 (16 for `u8`, `i32`, and `u32`); `Mask<N>` is the \
                     `bool`-laned form a lane comparison answers",
                );
            }
            TypeError::ConstGenericNotInferred {
                callee,
                literal_ty: None,
                assoc_owner: Some(owner),
            } => {
                out = out
                    .with_help(format!(
                        "name the value on the type: `{owner}::<4>::{callee}(..)`"
                    ))
                    .with_note(
                        "an associated function of a const generic `impl` takes the block's \
                         const parameters from the type it is called through",
                    );
            }
            TypeError::ConstGenericNotInferred {
                callee,
                literal_ty: None,
                assoc_owner: None,
            } => {
                out = out
                    .with_help(format!(
                        "name the value with a turbofish: `{callee}::<4>(..)`"
                    ))
                    .with_note(
                        "a const generic parameter takes its value from the call: an explicit \
                         `::<N>` argument, or the length of an array argument whose type names it",
                    );
            }
            TypeError::ConstGenericNotInferred {
                literal_ty: Some(ty),
                ..
            } => {
                out = out
                    .with_help(format!(
                        "name the value in the type it is bound to: `let value: {ty}<4> = ..`"
                    ))
                    .with_note(
                        "a const generic parameter of a literal or a variant takes its value from \
                         the length of an array field or payload that names it, or from the type \
                         the context expects",
                    );
            }
            TypeError::StringParseRetired { suggestion } => {
                out = out
                    .with_help(format!("write `{suggestion}()`"))
                    .with_note(
                        "`to_i64` / `to_f64` / `to_bool` parse the whole string and answer \
                         an `Option<T>`; add `.ok_or(..)` where a `Result` is wanted",
                    )
                    .with_suggestion(gossamer_diagnostics::Suggestion::replacement(
                        location,
                        format!("write `{suggestion}()`"),
                        format!("{suggestion}()"),
                    ));
            }
            TypeError::TransparentWrapper { wrapper, inner } => {
                out = out
                    .with_help(format!("write `{inner}`"))
                    .with_note(
                        "every value is heap-shared and reference-counted already, so a \
                         pointer wrapper names no choice the writer has to make",
                    )
                    .with_suggestion(gossamer_diagnostics::Suggestion::replacement(
                        location,
                        format!("write `{inner}`"),
                        inner.clone(),
                    ));
                let _ = wrapper;
            }
            TypeError::InvalidAssignTarget { target } => {
                out = out
                    .with_help(format!(
                        "bind the value instead: `let value = {target}`, or assign to a \
                         binding, field, index, or dereference"
                    ))
                    .with_note(
                        "an assignment writes through a place; a literal, a call result, and \
                         any other temporary name none",
                    );
            }
            TypeError::DerefWriteToNonReference { name } => {
                out = out
                    .with_help(format!(
                        "write `{name}` directly, or declare it `&mut T` and pass `&mut` at \
                         the call site"
                    ))
                    .with_note(
                        "`*` reaches the place a reference names; over a value there is no \
                         place to write, so the assignment would be discarded",
                    );
            }
            TypeError::AssignThroughSharedReference { name } => {
                out = out
                    .with_help(format!(
                        "create `{name}` with `&mut` from a mutable place to write through it"
                    ))
                    .with_note("a shared `&T` reference permits reads but not writes");
            }
            TypeError::MutableReferenceToImmutable { name } => {
                out = out
                    .with_help(format!(
                        "declare the source mutable before borrowing it: `let mut {name} = ...`"
                    ))
                    .with_note("`&mut` requires a mutable place");
            }
            TypeError::MutableReferenceConflict { root, borrower } => {
                out = out
                    .with_help(format!(
                        "end or narrow `{borrower}` before borrowing `{root}` mutably again"
                    ))
                    .with_note("named mutable references are exclusive for their lexical scope");
            }
            TypeError::ReferenceEscapeUnsupported { .. } => {
                out = out
                    .with_help(
                        "keep the reference call-scoped or bind it directly to a named local place",
                    )
                    .with_note(
                        "references are non-owning views; storing or returning one could outlive its referent",
                    );
            }
            TypeError::BorrowedPlaceConflict { borrower, .. } => {
                out = out
                    .with_help(format!(
                        "access the value through `{borrower}`, or narrow that reference's lexical scope"
                    ))
                    .with_note(
                        "a mutable reference is exclusive; any live reference prevents mutation of its source",
                    );
            }
            TypeError::ReferencePatternAggregateUnsupported { .. } => {
                out = out
                    .with_help("bind the reference itself, then access its elements or fields")
                    .with_note(
                        "reference-pattern destructuring is limited to scalar referents in Gossamer",
                    );
            }
            TypeError::ConcurrentAggregateUnsupported { .. } => {
                out = out
                    .with_help(
                        "publish the Vec separately, or pass scalar fields and reconstruct the aggregate in the receiving goroutine",
                    )
                    .with_note(
                        "nested growable storage does not yet have one ownership descriptor shared by every concurrency ABI",
                    );
            }
            TypeError::ConcurrentCaptureUnsupported { name, .. } => {
                out = out
                    .with_help(format!(
                        "open `{name}`'s value inside the goroutine, hand it over through a channel, or guard it with `sync::Shared` and reach it through `with`"
                    ))
                    .with_note(
                        "a goroutine reaches a captured binding through the same storage the spawning one holds, and nested growable storage has no representation both sides can own",
                    );
            }
            TypeError::SharedPayloadUnsupported { .. } => {
                out = out
                    .with_help(
                        "guard a scalar or a `String`; publish a collection through a channel, or keep one `Shared` per field",
                    )
                    .with_note(
                        "the guarded slot is one word on every tier, and only a scalar or a `String` has the same reading in the bytecode VM and in compiled code",
                    );
            }
            TypeError::MutableArgumentRequiresReference { argument } => {
                out = out
                    .with_help(format!(
                        "change this argument to `&mut {argument}`; its binding must be declared `mut`"
                    ))
                    .with_note(
                        "calls never create mutable references implicitly; `&mut` makes possible mutation visible",
                    );
            }
        }
        out
    }
}

/// Attaches the GT0021 / GT0022 / GT0023 help + note. Split out of
/// `to_diagnostic` to keep that match within the line-count lint budget.
fn structural_use_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    error: &TypeError,
) -> gossamer_diagnostics::Diagnostic {
    match error {
        TypeError::NotIndexable { ty } => out.with_help(format!(
            "`{ty}` is not a `[T]`, `[T; N]`, `Vec<T>`, or `String`; index a user type through `impl Index for {ty}`"
        )),
        TypeError::NotCallable { ty } => out.with_help(format!(
            "`{ty}` is not a function; only `fn` items, `fn(..)` pointers, and `Fn(..)` values can be called"
        )),
        TypeError::NoTupleField { ty, index } => out.with_help(format!(
            "`{ty}` has no field `.{index}`; positional access works only on tuples within their arity"
        )),
        _ => out,
    }
}

/// Attaches the GT0007 help + note. Split out of `to_diagnostic` to keep
/// that match within the line-count lint budget.
fn discarded_result_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
) -> gossamer_diagnostics::Diagnostic {
    out.with_help(
        "propagate the error with `?`, handle it with `match` / `if let`, \
         or explicitly discard with `let _ = <expr>`",
    )
    .with_note("every `Result` value must be handled")
}

/// How many method names a GT0002 diagnostic lists before truncating.
const AVAILABLE_NAME_LIMIT: usize = 3;

/// Edit-distance budget for a "did you mean" candidate.
const SUGGEST_MAX_DISTANCE: usize = 3;

/// Renders `names` as a comma-separated list of backticked spellings,
/// keeping at most `limit` of them and marking a truncated tail.
fn name_list(names: &[String], limit: usize) -> String {
    let shown = names
        .iter()
        .take(limit)
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > limit {
        format!("{shown}, ...")
    } else {
        shown
    }
}

/// The GT0006 rendering inputs, grouped so the helper takes one argument
/// per concern rather than a flat parameter list.
struct UnknownFieldParts<'a> {
    ty: &'a str,
    field: &'a str,
    opaque: bool,
    declared: &'a [String],
    field_span: Option<Span>,
    method_of_same_name: bool,
}

/// Attaches the GT0006 help and rename suggestion. Split out of
/// `to_diagnostic` to keep that match within the line-count lint budget.
fn unknown_field_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    file: gossamer_lex::FileId,
    parts: UnknownFieldParts<'_>,
) -> gossamer_diagnostics::Diagnostic {
    let UnknownFieldParts {
        ty,
        field,
        opaque,
        declared,
        field_span,
        method_of_same_name,
    } = parts;
    if method_of_same_name {
        return out.with_help(format!(
            "`{field}` is a method of `{ty}`, not a field: call it as `value.{field}()`"
        ));
    }
    if opaque {
        return out.with_help(format!(
            "`{ty}` has no named struct fields exposed to the language; \
             read it through the type's methods, for example `value.get(\"{field}\")` \
             on a `json::Value`"
        ));
    }
    let mut out = out;
    if let Some(candidate) = gossamer_diagnostics::suggest(
        field,
        declared.iter().map(String::as_str),
        SUGGEST_MAX_DISTANCE,
    ) {
        out = out.with_help(format!("did you mean `{candidate}`?"));
        if let Some(span) = field_span {
            out = out.with_suggestion(gossamer_diagnostics::Suggestion::replacement(
                gossamer_diagnostics::Location::new(file, span),
                format!("replace `{field}` with `{candidate}`"),
                candidate,
            ));
        }
    }
    if declared.is_empty() {
        out.with_help(format!("`{ty}` declares no fields"))
    } else {
        out.with_help(format!(
            "declared fields are {}",
            name_list(declared, declared.len())
        ))
    }
}

/// Attaches the GT0002 did-you-mean and the receiver's method surface.
/// Split out of `to_diagnostic` to keep that match within the line-count
/// lint budget.
/// Whether a rendered type is a callable: `fn(..) -> ..`, `Fn(..) -> ..`, or a
/// closure. These share one property the method paths care about - no method
/// surface at all - so the diagnostic speaks to the shape rather than the name.
fn is_callable_ty_spelling(ty: &str) -> bool {
    ty == "fn"
        || ty == "closure"
        || ty.starts_with("fn(")
        || ty.starts_with("fn ")
        || ty.starts_with("Fn(")
}

/// The free call that sorts a sequence the way the named method sorts a
/// buffer it owns. `sort_by_key_desc` has no free twin: a descending sort is
/// the ascending one over a reversed key.
fn sort_free_call_form(name: &str) -> Option<&'static str> {
    match name {
        "sort_by" => Some("iter::sort_by(<sequence>, cmp)"),
        "sort_by_key" => Some("iter::sort_by_key(<sequence>, key)"),
        "sort_by_key_desc" => Some("iter::sort_by_key(<sequence>, |v| Reverse(key(v)))"),
        _ => None,
    }
}

fn unresolved_method_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    ty: &str,
    name: &str,
    available: &[String],
) -> gossamer_diagnostics::Diagnostic {
    let mut out = out;
    // Sorting is a receiver form on the types that hold an ordered buffer, so
    // a receiver without one is told what it lacks rather than that the
    // method does not exist anywhere.
    if let Some(free_call) = sort_free_call_form(name) {
        return out
            .with_help(format!("write it as the free call `{free_call}`"))
            .with_note(
                "this receiver holds no ordered buffer to sort; `Vec`, an array, \
                 and a slice sort in place",
            );
    }
    // A traversal that only has a data-last free call is not a near-miss for
    // some other method: naming the spelling that exists beats sending the
    // reader to a different operation with a similar name.
    if crate::is_free_call_only_traversal(name) {
        return out
            .with_help(format!(
                "`{name}` is written as a free call taking the sequence last: \
                 `iter::{name}(.., <sequence>)`, or `<sequence> |> |v| iter::{name}(.., v)`"
            ))
            .with_note("this traversal has no receiver form on any tier");
    }
    if let Some(candidate) = gossamer_diagnostics::suggest(
        name,
        available.iter().map(String::as_str),
        SUGGEST_MAX_DISTANCE,
    ) {
        out = out.with_help(format!("did you mean `{candidate}`?"));
    }
    if available.is_empty() {
        out
    } else {
        out.with_help(format!(
            "`{ty}` has {}",
            name_list(available, AVAILABLE_NAME_LIMIT)
        ))
    }
}

/// Attaches the GT0019 did-you-mean and the enum's declared variants.
/// Split out of `to_diagnostic` to keep that match within the line-count
/// lint budget.
fn unknown_variant_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    enum_name: &str,
    variant: &str,
    declared: &[String],
) -> gossamer_diagnostics::Diagnostic {
    let mut out = out;
    if let Some(candidate) = gossamer_diagnostics::suggest(
        variant,
        declared.iter().map(String::as_str),
        SUGGEST_MAX_DISTANCE,
    ) {
        out = out.with_help(format!("did you mean `{enum_name}::{candidate}`?"));
    }
    if declared.is_empty() {
        out
    } else {
        out.with_help(format!(
            "declared variants are {}",
            name_list(declared, declared.len())
        ))
    }
}

/// Attaches the GT0018 help + note. Split out of `to_diagnostic` to keep
/// that match within the line-count lint budget.
fn arity_mismatch_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    callee: &str,
    expected: usize,
) -> gossamer_diagnostics::Diagnostic {
    out.with_help(format!(
        "`{callee}` is declared with {expected} parameter(s); pass exactly that many"
    ))
}

/// Attaches the GT0020 help + note. Split out of `to_diagnostic` to keep
/// that match within the line-count lint budget.
fn supertrait_method_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    method: &str,
    bound: &str,
    supertrait: &str,
) -> gossamer_diagnostics::Diagnostic {
    out.with_help(format!(
        "add `{method}` to bound `{bound}`, or bound the parameter on `{supertrait}` \
         directly (`<T: {supertrait}>`)"
    ))
    .with_note("a generic bound exposes only the named trait's own methods")
}

/// Attaches the GT0016 note + help. Split out of `to_diagnostic` to keep
/// that match within the line-count lint budget.
fn json_not_serializable_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    op: &str,
    ty: &str,
) -> gossamer_diagnostics::Diagnostic {
    out.with_note(format!(
        "`{ty}` is an enum, which has no JSON representation"
    ))
    .with_help(format!(
        "unwrap the value before encoding - e.g. `let v = <expr>?` then \
             `json::{op}(&v)` - or build a `json::Value`"
    ))
}

/// Attaches the GT0015 help + note. Split out of `to_diagnostic` to
/// keep that match within the line-count lint budget.
fn std_fn_value_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    path: &str,
) -> gossamer_diagnostics::Diagnostic {
    out.with_help(format!(
        "write the closure yourself: `|x| {path}(x)` works on every tier"
    ))
    .with_note(
        "a std function named in value position is rewritten into the closure that \
         calls it, which every tier lowers; this one takes a variable number of \
         arguments, so there is no single closure to write for it",
    )
}

fn value_not_displayable_diagnostic(
    out: gossamer_diagnostics::Diagnostic,
    ty: &str,
    class: NotDisplayableClass,
) -> gossamer_diagnostics::Diagnostic {
    match class {
        NotDisplayableClass::Handle => out
            .with_help(format!(
                "print an accessor of the `{ty}` instead - the value itself is a \
                 runtime-owned handle"
            ))
            .with_note(
                "a handle carries no fields and no text form: its address differs on \
                 every run, so formatting it could never be reproducible",
            ),
        NotDisplayableClass::Callable => out
            .with_help(format!(
                "call the {ty} and format its result, or format a name you choose for it"
            ))
            .with_note("a callable is a code address, not data"),
        NotDisplayableClass::Concurrency => out
            .with_help(format!(
                "format the values that pass through the `{ty}`, not the endpoint itself"
            ))
            .with_note("a channel endpoint or join handle is runtime state, not data"),
        NotDisplayableClass::GenericWithoutDebug => out
            .with_help(format!(
                "add `#[derive(Debug)]` to `{ty}` so every instantiation gets a `fmt`"
            ))
            .with_note(
                "whether a generic type's fields render depends on the arguments each \
                 instantiation supplies, so the declaration is where the choice is made",
            ),
    }
}

#[cfg(test)]
mod tests {
    use super::mismatch_suggestion;

    #[test]
    fn iterator_vec_mismatches_offer_migration_hints() {
        assert!(
            mismatch_suggestion("Vec<i64>", "Iterator<i64>")
                .is_some_and(|help| help.contains("iter::collect"))
        );
        assert!(
            mismatch_suggestion("Iterator<i64>", "Vec<i64>")
                .is_some_and(|help| help.contains("`<expr>.iter()`"))
        );
    }
}
