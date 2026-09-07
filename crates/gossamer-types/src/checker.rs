//! Type checker and inference driver.
//! Walks a parsed and name-resolved [`SourceFile`], assigns a [`Ty`]
//! handle to every expression and pattern, and records obvious
//! type-equality mismatches as diagnostics.
//! The implementation is deliberately lenient where later phases will
//! add strength: unresolved methods, operators on non-primitive types,
//! and external stdlib references fall back to fresh inference
//! variables instead of emitting diagnostics. Only conflicts between
//! two known-concrete types are reported. This keeps the checker
//! quiet on programs that reach heavily into the stdlib before the
//! trait solver arrives.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

/// A type's identity below the resolver: the name it is declared under,
/// prefixed by the modules that contain it. Two modules may declare the same
/// name, so the bare spelling is not unique - everything keyed by "the type's
/// name" (the type tables, `{:?}` dispatch, the native constructor registry)
/// keys on this instead.
fn qualified_type_name(module_path: &[String], name: &str) -> String {
    if module_path.is_empty() {
        return name.to_string();
    }
    format!("{}::{name}", module_path.join("::"))
}

use crate::builtin_traits::ITERATOR_BOUND_METHODS as ITERATOR_METHODS;
use gossamer_ast::{
    ArrayExpr, BinaryOp, Block, ClosureParam, Expr, ExprKind, FieldPattern, FnDecl, FnParam,
    GenericArg as AstGenericArg, ImplDecl, ImplItem, Item, ItemKind, Literal, MatchArm, NodeId,
    Pattern, PatternKind, SourceFile, Stmt, StmtKind, StructBody, TraitItem, Type as AstType,
    TypeKind as AstTypeKind, TypePath, UnaryOp, Visibility,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, FloatWidth, IntWidth, PrimitiveTy, Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::error::{TypeDiagnostic, TypeError};
use crate::infer::{InferCtxt, UnifyError};
use crate::printer::render_ty;
use crate::table::TypeTable;
use crate::ty::{FloatTy, FnSig, IntTy, Mutbl, Ty, TyKind};

/// Runs type inference on `source` using the name-resolution output in
/// `resolutions` and the shared type interner `tcx`.
#[must_use]
pub fn typecheck_source_file(
    source: &SourceFile,
    resolutions: &Resolutions,
    tcx: &mut TyCtxt,
) -> (TypeTable, Vec<TypeDiagnostic>) {
    let checker = TypeChecker::new(tcx, resolutions);
    checker.run(source)
}

/// Typechecks generated REPL inspection programs.
///
/// Normal user code rejects reads through an owner while a named `&mut`
/// borrower is live. `%bindings` and `%explain` synthesize tiny programs that
/// read a binding solely to display its current value and type, so this entry
/// point suppresses that read diagnostic without changing assignment or borrow
/// creation checks.
#[must_use]
pub fn typecheck_source_file_for_repl_inspection(
    source: &SourceFile,
    resolutions: &Resolutions,
    tcx: &mut TyCtxt,
) -> (TypeTable, Vec<TypeDiagnostic>) {
    let mut checker = TypeChecker::new(tcx, resolutions);
    checker.suppressed.borrow_read_conflict = true;
    checker.suppressed.consumed_iterator_read = true;
    checker.run(source)
}

impl TypeChecker<'_> {
    fn run(mut self, source: &SourceFile) -> (TypeTable, Vec<TypeDiagnostic>) {
        self.collect_import_targets(&source.uses);
        self.assoc = gossamer_ast::AssocIndex::build(source);
        self.collect_signatures(&source.items);
        // A file-level `#![allow(unused_result)]` covers every item in it.
        self.unused_result_allowed = source.attrs.allows("unused_result");
        for item in &source.items {
            self.check_item(item);
        }
        self.infer.default_unresolved_int_vars(self.tcx);
        self.infer.default_unresolved_float_vars(self.tcx);
        self.check_deferred_reference_storage();
        self.check_deferred_adt_bounds();
        self.check_deferred_type_mismatches();
        self.check_deferred_conversion_targets();
        self.check_deferred_into_conversions();
        self.check_deferred_literal_type_mismatches();
        self.check_deferred_scalar_method_rejections();
        self.check_deferred_mutating_receivers();
        self.check_deferred_private_fields();
        self.check_deferred_structural();
        self.check_deferred_shared_payloads();
        self.resolve_table();
        let diagnostics = Self::dedupe_diagnostics(self.diagnostics);
        (self.table, diagnostics)
    }

    /// Drops repeats of a diagnostic already reported with the same error
    /// at the same span, keeping the first occurrence's position in the
    /// list.
    ///
    /// A signature's types are converted once while collecting signatures
    /// and again while checking the item, so a diagnostic raised during
    /// that conversion is reached more than once for one piece of source.
    /// The repeats carry no information the first one did not.
    fn dedupe_diagnostics(diagnostics: Vec<TypeDiagnostic>) -> Vec<TypeDiagnostic> {
        let mut seen: HashSet<(&'static str, Span, String)> = HashSet::new();
        diagnostics
            .into_iter()
            .filter(|diagnostic| {
                seen.insert((
                    diagnostic.error.code(),
                    diagnostic.span,
                    diagnostic.error.to_string(),
                ))
            })
            .collect()
    }
}

/// Hard limit on type-checker recursion depth. Mirrors the parser's
/// guard and keeps adversarial input that survives parsing from
/// blowing the C stack inside [`TypeChecker::check_expr`].
const RECURSION_LIMIT: u32 = 256;
/// The parts of a method call that identify the call itself, as opposed to
/// the receiver and arguments it is applied to.
struct MethodCallSite<'a> {
    call_id: NodeId,
    method: &'a str,
    /// Source range of the method name, so a diagnostic about the method
    /// points at it rather than at the receiver.
    name_span: Span,
    generics: &'a [AstGenericArg],
}

const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
const VALIDATE_ERRORS_DEF_LOCAL: u32 = u32::MAX - 9;
const VALIDATE_FIELD_ERROR_DEF_LOCAL: u32 = u32::MAX - 10;
const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
const VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
const REVERSE_DEF_LOCAL: u32 = u32::MAX - 29;
const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;
const RESULT_DEF_LOCAL: u32 = u32::MAX;
const OPTION_DEF_LOCAL: u32 = u32::MAX - 1;

/// Sentinel-offset band reserved for opaque runtime handles that carry
/// nothing but their i64 slot: no field layout, no text form. The band is
/// contiguous so both the checker's display gate and the MIR's
/// representation rule recognise the whole family by range.
const PURE_HANDLE_LO_OFFSET: u32 = 34;
const PURE_HANDLE_HI_OFFSET: u32 = 49;

/// Widest sentinel offset any stdlib handle occupies, pure band and the
/// pre-band handles alike. A receiver inside this span whose display name is
/// module-qualified answers a closed method table, which is what lets an
/// unknown name on one be named at the call site.
const HANDLE_SENTINEL_SPAN: u32 = PURE_HANDLE_HI_OFFSET;

/// One constructor of a runtime handle: the module path it is written
/// under, and the associated function's name.
type HandleCtor = (&'static [&'static str], &'static str);

/// One runtime handle: its sentinel offset, the name diagnostics print,
/// and the constructors that produce it.
type HandleRow = (u32, &'static str, &'static [HandleCtor]);

/// `(sentinel offset, display name)` for each opaque runtime handle, and
/// the constructor paths that produce one. A handle's identity lives in
/// the type so a format macro can name it and refuse it; its method
/// surface is still resolved by the tier lowerings, not here.
const PURE_HANDLES: &[HandleRow] = &[
    (34, "sync::Map", &[(&["sync", "Map"], "new")]),
    (35, "sync::RwLock", &[(&["sync", "RwLock"], "new")]),
    (36, "metrics::Counter", &[(&["metrics", "Counter"], "new")]),
    (37, "metrics::Gauge", &[(&["metrics", "Gauge"], "new")]),
    (
        38,
        "metrics::Histogram",
        &[(&["metrics", "Histogram"], "new")],
    ),
    (
        39,
        "metrics::Registry",
        &[(&["metrics", "Registry"], "new")],
    ),
    (40, "trace::Tracer", &[(&["trace", "Tracer"], "new")]),
    (
        41,
        "http::Router",
        &[
            (&["router", "Router"], "new"),
            (&["http", "router", "Router"], "new"),
        ],
    ),
    (
        42,
        "rand::Rng",
        &[
            (&["rand", "Rng"], "new"),
            (&["math", "rand", "Rng"], "new"),
            (&["rand", "Rng"], "seeded"),
            (&["math", "rand", "Rng"], "seeded"),
        ],
    ),
    (43, "bufio::Scanner", &[(&["bufio", "Scanner"], "new")]),
    // `File::open` / `File::create` answer their handle through a
    // `Result`, so they are typed by `fs_file_ctor` rather than as bare
    // handle constructors here.
    (44, "fs::File", &[]),
    (45, "fs::OpenOptions", &[(&["fs", "OpenOptions"], "new")]),
    (46, "sync::Shared", &[(&["sync", "Shared"], "new")]),
    (
        46,
        "http::FileServer",
        &[
            (&["static_files", "FileServer"], "new"),
            (&["http", "static_files", "FileServer"], "new"),
        ],
    ),
    (
        47,
        "http::Proxy",
        &[
            (&["proxy", "Proxy"], "new"),
            (&["http", "proxy", "Proxy"], "new"),
        ],
    ),
    (
        4,
        "http::ResponseStream",
        &[
            (&["http", "ResponseStream"], "new"),
            (&["ResponseStream"], "new"),
        ],
    ),
    (
        48,
        "http::Server",
        &[(&["http", "Server"], "new"), (&["Server"], "new")],
    ),
    // The composed-middleware handler closes the band; it is produced by
    // the `middleware::*` wrappers rather than by a named constructor.
    (PURE_HANDLE_HI_OFFSET, "http::Handler", &[]),
];

/// One constructor of a pre-band handle: its module path and name,
/// followed by the sentinel offset and display name it lands on.
type LegacyHandleCtor = (&'static [&'static str], &'static str, u32, &'static str);

/// Constructors for the handle sentinels that predate the pure-handle
/// band. Their offsets are fixed by the annotation paths that already
/// resolve to them, so a constructor lands on the same type a written
/// `fn f() -> http::Response` does.
const LEGACY_HANDLE_CTORS: &[LegacyHandleCtor] = &[
    (
        &["context", "Context"],
        "background",
        11,
        "context::Context",
    ),
    (
        &["context", "Context"],
        "with_cancel",
        11,
        "context::Context",
    ),
    (
        &["context", "Context"],
        "with_timeout",
        11,
        "context::Context",
    ),
    (&["validate", "Errors"], "new", 9, "validate::Errors"),
    (
        &["validate", "FieldError"],
        "new",
        10,
        "validate::FieldError",
    ),
    (&["flag", "Set"], "new", 21, "flag::Set"),
    (&["http", "Client"], "new", 22, "http::Client"),
    (&["http", "Client"], "builder", 23, "http::ClientBuilder"),
    (&["http", "Response"], "text", 5, "http::Response"),
    (&["http", "Response"], "json", 5, "http::Response"),
    (&["http", "Response"], "stream", 5, "http::Response"),
];

/// Eager sequence combinators callable in method form on a `Vec`.
const SEQUENCE_COMBINATOR_METHODS: &[&str] = &[
    "map",
    "filter",
    "for_each",
    "fold",
    "any",
    "all",
    "find",
    "position",
    "min_by_key",
    "max_by_key",
    "take",
    "take_while",
    "skip",
    "skip_while",
    "step_by",
    "chain",
    "zip",
    "enumerate",
    "rev",
    "dedup",
    "flatten",
    "pairwise",
    "sum",
    "min",
    "max",
    "count",
    "filter_map",
    "find_map",
    "flat_map",
    "chunk_by",
    "count_by",
    "max_by",
    "min_by",
    "partition",
    "product_by",
    "reduce",
    "sum_by",
    "unzip",
    "scan",
];

/// The `Set` and `BTreeSet` method surface: membership, cardinality, and set
/// algebra - the operations a set defines. A set has no element order, so the
/// sequence operations belong to the iterator `iter()` answers, and are
/// written `s.iter().take(3)` the way they are on any other iterator.
const SET_METHODS: &[&str] = &[
    "insert",
    "remove",
    "contains",
    "len",
    "is_empty",
    "clear",
    "iter",
    "to_vec",
    "union",
    "intersection",
    "difference",
    "symmetric_difference",
    "is_subset",
    "is_superset",
    "is_disjoint",
];

/// The `Deque` method surface.
const DEQUE_METHODS: &[&str] = &[
    "push_back",
    "push_front",
    "pop_back",
    "pop_front",
    "peek_back",
    "peek_front",
    "len",
    "is_empty",
    "clear",
];

/// The `Queue`, `Stack`, `MaxHeap`, and `MinHeap` method surface.
const PUSH_POP_METHODS: &[&str] = &["push", "pop", "peek", "len", "is_empty", "clear"];

/// Method names synthesized for every user type, which say nothing about
/// the type the reader declared and so list after its own methods.
const AUTOMATIC_METHODS: &[&str] = &["eq", "ne", "cmp", "partial_cmp", "fmt", "hash", "clone"];

/// The `Result<T, E>` method surface.
const RESULT_METHODS: &[&str] = &[
    "is_ok",
    "is_err",
    "ok",
    "err",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
    "expect",
    "map",
    "map_err",
    "and_then",
    "or_else",
];

/// The `Option<T>` method surface.
const OPTION_METHODS: &[&str] = &[
    "is_some",
    "is_none",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
    "expect",
    "map",
    "and_then",
    "filter",
    "or",
    "or_else",
    "zip",
    "ok_or",
    "ok_or_else",
    "flatten",
    "iter",
];

/// Where a combinator's data argument sits in the call as written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DataPosition {
    /// The method receiver, written before the other arguments.
    Receiver,
    /// The trailing argument of a data-last free or piped call.
    Last,
}

/// Expected type pushed down into an expression while it is checked -
/// the "checking mode" of bidirectional typechecking. The expectation
/// decides structural questions unification cannot settle after the
/// fact (an array literal lowering as a fixed `[T; N]` versus a heap
/// `Vec<T>`), and it propagates through value-producing positions:
/// block tails, `if` / `match` branches, `&`-borrows, call and
/// constructor arguments.
#[derive(Clone, Copy, Debug)]
enum Expectation {
    /// No expectation: the expression synthesizes its type bottom-up.
    None,
    /// The expression must produce this type. Literal containers adopt
    /// the expected shape and their children are unified against the
    /// expected child types, so mismatches surface at the leaf span.
    HasType(Ty),
    /// Shape-only hint: literal containers adopt a matching expected
    /// shape but nothing is unified. Used where the expectation source
    /// is unreliable - name-global method signatures and variant
    /// constructors whose declared payload may belong to another type.
    Coerce(Ty),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuiltinPatternFamily {
    Option,
    Result,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TryFamily {
    Option,
    Result,
}

impl Expectation {
    /// The expected type, if any.
    fn ty(self) -> Option<Ty> {
        match self {
            Expectation::None => None,
            Expectation::HasType(ty) | Expectation::Coerce(ty) => Some(ty),
        }
    }

    /// Re-wraps a child type at the same expectation strength, so a
    /// `Vec<T>` expectation hands its elements a `T` expectation of
    /// equal force.
    fn rewrap(self, ty: Ty) -> Expectation {
        match self {
            Expectation::None => Expectation::None,
            Expectation::HasType(_) => Expectation::HasType(ty),
            Expectation::Coerce(_) => Expectation::Coerce(ty),
        }
    }

    /// Whether the expectation participates in unification.
    fn unifies(self) -> bool {
        matches!(self, Expectation::HasType(_))
    }
}

/// A structural use (`value[i]` / `value(args)` / `value.N`) whose
/// operand type was still an unresolved inference variable when first
/// checked. Unsuffixed integer/float literals (`let x = 5`) only
/// default to a concrete scalar after the whole file is checked, so the
/// soundness check is deferred and re-run once defaulting has happened.
#[derive(Clone, Copy)]
enum DeferredStructuralKind {
    Index,
    Call,
    TupleField(u64),
    /// `value.downgrade()` whose receiver was an unresolved inference
    /// variable; re-validated as a valid RC-backed receiver after
    /// defaulting so `let x = 5; x.downgrade()` is caught.
    Downgrade,
}

struct DeferredStructural {
    ty: Ty,
    span: Span,
    kind: DeferredStructuralKind,
    /// The variable standing in for the use's result while the operand's
    /// type was unknown. Once the operand resolves, the element or field
    /// type it names is unified with this variable, so the node the caller
    /// already recorded ends up carrying a grounded type.
    result: Option<Ty>,
}

struct DeferredMutatingReceiver {
    ty: Ty,
    method: String,
    place: PlaceMut,
    name: String,
    span: Span,
}

/// Read diagnostics a surrounding operation has already accounted for.
///
/// Both describe a read the checker performs on the program's behalf rather
/// than one the user wrote, so the rule the read would violate does not apply
/// to it.
#[derive(Debug, Clone, Copy, Default)]
struct SuppressedReadChecks {
    /// Ordinary path-read checks, paused while validating the place of a
    /// borrow or assignment. Those operations issue their more precise
    /// conflict diagnostic after the place has been typed.
    borrow_read_conflict: bool,
    /// The consumed-iterator read diagnostic. Observing a binding to report
    /// its type and value is not a traversal, so a REPL inspection program
    /// reads an already-consumed iterator without violating the source-level
    /// linearity the check enforces for user code.
    consumed_iterator_read: bool,
}

struct TypeChecker<'a> {
    tcx: &'a mut TyCtxt,
    infer: InferCtxt,
    table: TypeTable,
    diagnostics: Vec<TypeDiagnostic>,
    resolutions: &'a Resolutions,
    scopes: Vec<HashMap<Box<str>, Ty>>,
    /// Declared mutability of each in-scope value binding, kept in
    /// lockstep with `scopes`. A place rooted at an immutable binding
    /// cannot be assigned to (GT0030).
    mut_scopes: Vec<HashMap<Box<str>, bool>>,
    binding_types: HashMap<NodeId, Ty>,
    /// Set while checking a body under `#[allow(unused_result)]`, the
    /// suppression SPEC §9 names for the discarded-value reports.
    unused_result_allowed: bool,
    /// Functions declared `#[must_use]`, so a discarded call to one is
    /// reported even though its return type carries no marker.
    must_use_fns: HashMap<DefId, String>,
    /// Structs and enums declared `#[must_use]`.
    must_use_types: HashMap<DefId, String>,
    /// The `Self` type of the `impl` block currently being checked,
    /// so a method's `self` receiver binds to the concrete type
    /// instead of a free inference var. Without this, `self.field`
    /// reads inside a method leave the field type unresolved and a
    /// `for x in self.items` loop binds `x` at the i64 default -
    /// printing a `[String]` field's element pointers as integers.
    current_self_ty: Option<Ty>,
    /// Generics of the `impl` block currently being checked, if any. An
    /// `impl<T> Wrapper<T>` brings `T` into scope for every method, so a
    /// method signature `(&self) -> T` records a rigid `Param(T)` (matching
    /// the struct's generic) rather than a fresh inference variable that
    /// never binds. Merged ahead of each method's own generics in
    /// [`Self::check_fn`].
    current_impl_generics: Option<gossamer_ast::Generics>,
    /// `where` clause of the `impl` block currently being checked. Its
    /// predicates bound the same parameters `current_impl_generics`
    /// introduces, so both fold into one per-parameter bound table.
    current_impl_where: gossamer_ast::WhereClause,
    /// Source name of the `impl` block's self type, used to resolve a
    /// `Self::Item` projection against the impl that supplies `Item`.
    current_self_ty_name: Option<String>,
    /// Name of the `trait` declaration currently being checked. A
    /// `Self::Item` written in a trait method signature resolves through
    /// this trait rather than through a concrete self type.
    current_trait_name: Option<String>,
    /// Associated-type equality constraints in scope, keyed by
    /// `(type parameter name, associated type name)`. `fn f<T: Holder<Item
    /// = i64>>` records `("T", "Item") -> i64` and pins `T::Item` for the
    /// whole signature and body.
    current_assoc_bindings: HashMap<(String, String), gossamer_ast::Type>,
    /// Projections currently being expanded, so a self-referential
    /// associated type terminates instead of recursing.
    assoc_expanding: std::collections::HashSet<(String, String)>,
    /// Program-wide view of which traits declare which associated items
    /// and which impls supply them.
    assoc: gossamer_ast::AssocIndex,
    /// `DefId` of every user struct and enum by source name. Types written
    /// where the resolver does not walk - an associated-type binding inside
    /// a trait bound - resolve nominally through this table.
    adt_def_by_name: HashMap<String, gossamer_resolve::DefId>,
    /// Running depth of recursive entries into expression / block /
    /// pattern type checks. Reaching [`RECURSION_LIMIT`] short-circuits
    /// the offending subtree to `tcx.error_ty()` after emitting one
    /// diagnostic.
    recursion_depth: u32,
    /// `true` once the recursion-limit diagnostic has been emitted in
    /// the current source file. Prevents flooding the diagnostic
    /// stream with duplicates.
    recursion_limit_reported: bool,
    /// Iterator locals consumed by a lazy adapter or terminal, keyed by the
    /// `NodeId` of the binding occurrence so that a later `let` of the same
    /// name is a distinct, unconsumed binding. This is a conservative
    /// source-level linearity check for simple named locals.
    consumed_iterators: Vec<HashMap<NodeId, String>>,
    /// The method spelling the source wrote for the call being checked. A
    /// parse-time desugar can rename a call - `xs.sort_by_key(f)` is checked
    /// as the `sort_by` it builds - and a diagnostic that named the rewritten
    /// method sent the reader looking for a word their file does not contain.
    written_method: String,
    /// Lexically active named mutable borrows, keyed by referent root. This
    /// is deliberately conservative: it prevents a second named `&mut`
    /// binding while the first remains in scope.
    mutable_borrows: Vec<HashMap<Box<str>, Box<str>>>,
    /// Lexically active named shared borrows, keyed by referent root.
    shared_borrows: Vec<HashMap<Box<str>, Box<str>>>,
    /// Provenance root for each local reference binding. This lets a cursor
    /// advance through a reference yielded by pattern matching while still
    /// rejecting a rebind to storage declared in a shorter-lived scope.
    reference_origins: Vec<HashMap<Box<str>, Box<str>>>,
    /// Read checks paused for the current context.
    suppressed: SuppressedReadChecks,
    /// Owned local types that may only reveal a nested reference after
    /// inference has unified a channel, closure, tuple, or container generic.
    deferred_reference_storage: Vec<(Ty, Span, &'static str)>,
    /// Ordered field name + type for every named struct, keyed by
    /// the struct's `DefId`. Built during `collect_signatures` so
    /// field-access and struct-literal expressions can resolve leaf
    /// types without having to look up the original AST.
    struct_fields: HashMap<gossamer_resolve::DefId, Vec<(String, Ty)>>,
    /// Declaring module and declared visibility of each struct field,
    /// keyed by the owning struct and the field name.
    field_homes: HashMap<(DefId, String), (Vec<String>, Visibility)>,
    /// Nesting depth inside autoderive-spliced items, which reach a
    /// type's private surface by construction.
    synthesized_depth: u32,
    /// Cached function signatures keyed by `DefId`. Built during
    /// `collect_signatures` so a cross-function call site can pull
    /// the input/return types instead of returning a fresh var.
    fn_sigs: HashMap<gossamer_resolve::DefId, FnSig>,
    /// Non-receiver parameter types of user `impl` / trait methods,
    /// keyed by method name + arity. Every distinct signature is
    /// kept (method dispatch is name-global, so several types may
    /// share a name + arity); a literal argument is re-typed only
    /// when exactly one candidate expects a container shape at that
    /// position. Mirrors the free-fn re-typing against `fn_sigs`.
    method_arg_sigs: HashMap<(String, usize), Vec<Vec<Ty>>>,
    /// Declared return type of the function body currently being
    /// checked; drives literal re-typing at explicit `return`
    /// statements (the block-tail path is handled in `check_fn`).
    current_fn_ret: Option<Ty>,
    /// Per-enclosing-`loop` break-value type var plus whether any value
    /// break fired. A `loop` pushes `(fresh_var, false)`; each `break value`
    /// inside unifies its value with the var and sets the flag, so `let x =
    /// loop { break v }` infers `x` as `v`'s type. The flag distinguishes a
    /// value break whose type is still an unresolved (e.g. integer-literal)
    /// var from a loop with no value break at all - the former yields the
    /// var (it defaults later), the latter stays divergent (`never`).
    loop_break_tys: Vec<(Ty, bool)>,
    /// Declared return types of non-generic `impl` methods, keyed by
    /// `(self type name, method name, arity)`. When a method-call
    /// receiver resolves to that Adt, the call types as the declared
    /// return instead of a fresh inference var - without this,
    /// `sel.params()` reaches MIR untyped and the compiled tier
    /// guesses the element layout.
    method_ret_types: HashMap<(String, String, usize), Ty>,
    /// Declared non-receiver parameter types for methods on concrete user
    /// types, keyed by `(self type, method)`.
    method_param_types: HashMap<(String, String), Vec<Ty>>,
    /// Declared return types of generic-`impl` methods (`impl<T> Add for
    /// Wrap<T>`), keyed like [`Self::method_ret_types`]. The stored type
    /// carries rigid `Param` slots; a use site substitutes the receiver
    /// instantiation's `substs` before returning it.
    generic_method_ret_types: HashMap<(String, String, usize), Ty>,
    /// Generic-impl counterpart of [`Self::method_param_types`]. Stored
    /// parameter types carry rigid `Param` slots substituted from the
    /// receiver at each call site.
    generic_method_param_types: HashMap<(String, String), Vec<Ty>>,
    /// Declared argument arity (excluding `self`) of each user method,
    /// keyed by `(type_name, method_name)`. Drives the
    /// method-call arity check (GT0018): a call with the wrong count
    /// aborts on the VM and zero-fills/drops on the compiled tier, so it
    /// is rejected statically the same way free calls are.
    method_arities: HashMap<(String, String), usize>,
    /// Whether an inherent method requires an `&mut self` receiver, keyed by
    /// `(self type, method)`. Inherent methods take precedence over trait
    /// methods with the same name.
    inherent_method_requires_mut: HashMap<(String, String), bool>,
    /// Whether a trait-impl method requires an `&mut self` receiver, keyed by
    /// `(self type, method)`.
    trait_impl_method_requires_mut: HashMap<(String, String), bool>,
    /// Structural uses whose operand was an unresolved inference var at
    /// first check; re-validated after integer/float defaulting.
    deferred_structural: Vec<DeferredStructural>,
    /// Method receivers whose concrete type is established only by numeric
    /// defaulting. Their place capability is stable and can be checked once
    /// the receiver type selects the actual method.
    deferred_mutating_receivers: Vec<DeferredMutatingReceiver>,
    /// Field accesses whose receiver was still an inference variable when
    /// the access was checked, re-examined once inference has settled.
    deferred_private_fields: Vec<(Ty, String, Span, Vec<String>)>,
    /// Assignment mismatches whose outer shapes are already incompatible but
    /// whose literal elements need integer/float defaulting before their
    /// rendered types are useful to the user.
    deferred_type_mismatches: Vec<(Ty, Ty, Span)>,
    /// `.into()` call sites, recorded as (receiver, result, span). The
    /// target is a fresh variable when the call is checked, so whether a
    /// conversion exists can only be decided once unification has pinned
    /// it.
    deferred_into_conversions: Vec<(Ty, Ty, Span)>,
    deferred_literal_type_mismatches: Vec<(Ty, &'static str, Span)>,
    /// `x.name()` on an unsuffixed numeric literal, recorded as
    /// (receiver, method, span). The literal's width is pinned by
    /// defaulting after the last item is checked, so whether the receiver
    /// is a scalar - and which scalar to name - is only known then.
    deferred_scalar_method_rejections: Vec<(Ty, String, Span)>,
    /// `.into()` / `.try_into()` call sites, recorded as (result, method,
    /// span). The target comes from the use site, so whether one was given
    /// at all is only known once unification has run.
    deferred_conversion_targets: Vec<(Ty, &'static str, Span)>,
    /// Tuple-variant payload types keyed by `(enum_name,
    /// variant_name)`. Drives literal re-typing at variant
    /// constructor sites so `Value::Blob([1, 2, 3])` records a heap
    /// `[u8]`, not a fixed `[i64; 3]` whose first slot would pose as
    /// the payload word on the compiled tier.
    enum_variant_payloads: HashMap<(String, String), Vec<Ty>>,
    /// Per-call-site instantiation of a generic enum's parameters, keyed by
    /// the constructor path's node. The argument expectations, the argument
    /// checks, and the call's result type all read the same variables.
    variant_ctor_substs: HashMap<NodeId, (DefId, Vec<Ty>)>,
    /// Struct-variant field types keyed by `(enum_name, variant_name)`,
    /// each entry a declared field name paired with its type. A
    /// `Shape::Rect { w, h }` pattern binds through these exactly as a
    /// tuple variant binds through [`Self::enum_variant_payloads`].
    enum_variant_named_payloads: HashMap<(String, String), Vec<(String, Ty)>>,
    /// `Adt` type of every non-generic user enum, keyed by name. A
    /// tuple-variant constructor call (`E::B(1)`) resolves its result
    /// to this so the value carries the concrete enum type instead of a
    /// fresh inference variable - without it `E::B(1) < E::B(2)` leaves
    /// both operands unresolved and the comparison can't dispatch.
    enum_tys: HashMap<String, Ty>,
    /// Declared types for `const NAME: T = ...` items, keyed by
    /// `DefId`. Without this, a path expression that resolves to a
    /// const falls back to a fresh inference variable, leaving the
    /// use site unconstrained and the codegen reading the slot at
    /// the wrong layout.
    const_tys: HashMap<gossamer_resolve::DefId, Ty>,
    /// Integer value of every `const` item whose initializer is an integer
    /// literal, keyed by `DefId`. An array length is part of the array's type,
    /// so it is read here rather than at run time: a `const` is a
    /// compile-time constant, and naming one as a length resolves to its value.
    const_int_values: HashMap<gossamer_resolve::DefId, u128>,
    /// Declared mutability of `static` items, keyed by `DefId`, so place
    /// mutability checks treat `static` and `static mut` like their local
    /// binding counterparts.
    static_mutability: HashMap<gossamer_resolve::DefId, bool>,
    /// Type-parameter names and right-hand-side AST of every `type X<..> =
    /// T` alias, keyed by the alias's `DefId`. Built up front so a use of
    /// `X` expands to `T` lazily during type lowering (transparent
    /// aliases) - with the params substituted by the use-site arguments
    /// for a generic alias - rather than surfacing `X` as an opaque
    /// `adt#N`.
    alias_targets: HashMap<gossamer_resolve::DefId, (Vec<String>, gossamer_ast::Type)>,
    /// Alias `DefId`s currently being expanded, guarding against cyclic
    /// aliases (`type A = B; type B = A`).
    alias_expanding: std::collections::HashSet<gossamer_resolve::DefId>,
    /// Alias `DefId`s declared with the opaque form `type X = new T`.
    /// A use of one surfaces as [`TyKind::Nominal`] over the expansion of
    /// `T` rather than as `T` itself.
    nominal_aliases: std::collections::HashSet<gossamer_resolve::DefId>,
    /// Generic-parameter arity for every named struct, keyed by
    /// the struct's `DefId`. Built during `register_struct`. Used
    /// at struct-literal sites to allocate one fresh inference
    /// variable per parameter and substitute them into the
    /// declared field types' `TyKind::Param` slots. Without this,
    /// a `Pair<A, B> { fst: 10, snd: "hi" }` literal would fail
    /// to unify the `A`/`B` `Param` slots against `i64` /
    /// `String` and surface a confusing `type mismatch` against
    /// the rigid `Param`.
    struct_generic_arity: HashMap<gossamer_resolve::DefId, usize>,
    /// Generic type-parameter count for every named function, keyed by
    /// `DefId`. Drives per-call-site instantiation: each call to a
    /// generic function gets one fresh inference variable per parameter,
    /// substituted into the signature before unifying with the arguments,
    /// so distinct call sites bind the parameters independently.
    fn_generic_arity: HashMap<gossamer_resolve::DefId, usize>,
    /// Declared trait bounds on each generic function's type parameters,
    /// keyed by `DefId`, outer index = parameter index, inner = bound
    /// trait names. After a call's parameters resolve to concrete types,
    /// each bound is checked against [`Self::trait_impl_types`].
    fn_param_bounds: HashMap<gossamer_resolve::DefId, Vec<Vec<String>>>,
    /// Declared trait bounds on each user struct / enum's generic
    /// parameters, keyed by `DefId` and indexed by parameter position.
    /// Bounds written on an `impl` block whose self type instantiates the
    /// declaration with its own parameters in order fold in here too, so
    /// every construction site of the type is checked against them.
    adt_param_bounds: HashMap<gossamer_resolve::DefId, Vec<Vec<String>>>,
    /// `DefId` of every user struct / enum, keyed by declared name, so an
    /// `impl` block can attach its parameter bounds to the type it targets.
    user_type_defs: HashMap<String, gossamer_resolve::DefId>,
    /// Construction sites of a generic user type, held until inference
    /// finishes so each argument is checked against the declaration's
    /// bounds at its final resolved type.
    deferred_adt_bounds: Vec<(gossamer_resolve::DefId, Vec<Ty>, Span)>,
    /// Per-position flags marking which of each generic function's
    /// parameters are const parameters, keyed by `DefId`. Lets a call
    /// site record a `GenericArg::Const` (rather than a type argument)
    /// at each const position when inferring the substitution.
    fn_generic_const_mask: HashMap<gossamer_resolve::DefId, Vec<bool>>,
    /// Set of concrete type names implementing each trait, keyed by trait
    /// name. Built from every `impl Trait for Type` block so a generic
    /// call can verify a `T: Trait` bound is satisfied by the argument.
    trait_impl_types: HashMap<String, std::collections::HashSet<String>>,
    /// Declared return type of each trait method, keyed by
    /// `(trait name, method name)`. Lets a `s.method()` call on a bound
    /// type parameter (`s: &T`, `T: Shape`) resolve to the method's real
    /// return type instead of the i64 default - so a `String`-returning
    /// trait method renders as text on the compiled tiers.
    trait_method_ret: HashMap<(String, String), Ty>,
    /// Declared non-receiver parameter types of trait methods.
    trait_method_params: HashMap<(String, String), Vec<Ty>>,
    /// Associated type a trait method's return projects (`-> Self::Item`
    /// records `"Item"`), keyed by trait and method. The concrete type
    /// depends on the receiver, so a call resolves it against the
    /// receiver's own impl or bound rather than once at declaration.
    trait_method_ret_assoc: HashMap<(String, String), String>,
    /// Whether a trait method declares `&mut self`, keyed by trait and method.
    trait_method_requires_mut: HashMap<(String, String), bool>,
    /// Per-parameter trait bounds of the function currently being checked,
    /// indexed by parameter position. Set on entry to a generic function
    /// body so a method call on a `Param` receiver can find its bounds.
    current_param_bounds: Vec<Vec<String>>,
    /// Currently-active generic-parameter name → `ParamIdx`
    /// mapping. Populated while walking a struct / enum / fn /
    /// impl declaration so `type_from_ast_path` can render a
    /// type-parameter reference (`A`, `B`) as the right
    /// `TyKind::Param` index. Keyed by name because the AST's
    /// generic param reference and its definition site use names,
    /// not indices.
    ///
    /// `GenericParam::Type` carries an `Ident` without a
    /// resolver-assigned `NodeId`.
    current_generic_scope: HashMap<String, (crate::ParamIdx, Box<str>)>,
    /// Currently-active const-generic-parameter name → `ParamIdx`
    /// mapping, populated alongside [`Self::current_generic_scope`].
    /// Lets a `[T; N]` array-length expression naming a const
    /// parameter type as a symbolic [`crate::ArrayLen::Param`] rather
    /// than collapsing to a concrete `0`.
    current_const_generic_scope: HashMap<String, crate::ParamIdx>,
    /// Trait names declared in this source file. Populated upfront
    /// by `collect_signatures` from every `ItemKind::Trait`. Used
    /// by `register_fn_sig` to validate that each `<T: Bound>`
    /// names a trait that actually exists - typos surface as a
    /// `GT0011 unknown-trait-bound` diagnostic at declaration time
    /// instead of as a runtime "no method" error later.
    declared_trait_names: std::collections::HashSet<String>,
    /// Local `let`-binding pattern nodes whose value flows into a
    /// stdlib `archive::{tar,zip}::write` call. A pre-scan of each
    /// function body fills this so the binding's literal initializer
    /// is re-typed to the `[(String, [u8])]` parameter - backward
    /// inference the single-pass checker can't otherwise reach, which
    /// the compiled tier needs so the nested byte arrays become heap
    /// Vecs instead of fixed inline arrays.
    write_arg_bindings: HashMap<NodeId, Ty>,
    /// Path-expression nodes sitting in a callee position (a call's
    /// callee, or the rhs of `|>`). A bare std-module path there is a
    /// normal stdlib call shape; everywhere else it is a std fn used
    /// as a VALUE, which is only legal for the
    /// [`crate::std_fn_values`] tabled set (GT0015 otherwise).
    callee_path_nodes: std::collections::HashSet<NodeId>,
    /// Bound import name by use declaration id to its full module path. Used
    /// so `use std::iter::skip_while` lets a bare `skip_while(...)` type-check
    /// as the qualified `iter::skip_while(...)` call.
    import_targets: HashMap<(NodeId, String), Vec<String>>,
    /// Names of every user-declared struct and enum in this source file.
    /// Distinguishes a genuine user Adt receiver (eligible for the
    /// name-global method-dispatch soundness check) from the sentinel
    /// Adts the checker synthesizes (`Result`, `Option`, `http::Response`,
    /// `VecDeque`).
    user_type_decls: std::collections::HashSet<String>,
    /// For every method name defined in a user `impl` block (inherent or
    /// trait impl) or declared on a trait, the set of user type names
    /// that own it. Lets [`Self::maybe_reject_unknown_adt_method`] reject
    /// `b.label()` when `label` belongs to a different type than `b`.
    user_method_owners: HashMap<String, std::collections::HashSet<String>>,
    /// Every free function this file declares, by name. A scalar has no
    /// method surface of its own, so `x.f()` on one is the free call
    /// `f(x)`; the name has to belong to something for that to mean
    /// anything.
    user_fn_names: std::collections::HashSet<String>,
    /// For each `(owner identity, method)`, the module the `impl` was
    /// written in and whether the method is `pub`. A method without
    /// `pub` is nameable only from that module, matching the resolver's
    /// rule for a free function.
    method_homes: HashMap<(String, String), (Vec<String>, Visibility)>,
    /// `sync::Shared` payloads awaiting the end of inference, with the
    /// span of the value each was built from.
    deferred_shared_payloads: Vec<(Ty, Span)>,
    /// Module path of the item currently being checked, so a call site
    /// can be tested against a method's declaring module.
    current_module: Vec<String>,
    /// Method names declared directly on each trait, keyed by trait name.
    /// Used with [`Self::trait_supertraits`] to detect a method reached
    /// only through a bound's supertrait (P0-5).
    trait_own_methods: HashMap<String, std::collections::HashSet<String>>,
    /// Method names each trait declares without a default body, in
    /// declaration order. Every `impl Trait for Type` has to supply them;
    /// monomorphisation emits a direct call to each one.
    trait_required_methods: HashMap<String, Vec<String>>,
    /// Types whose source writes its own `cmp`, so their order is the one that
    /// `impl` answers rather than the field-by-field one. A container that
    /// orders its elements internally cannot reach it.
    user_ordered_types: std::collections::HashSet<String>,
    /// Every method name a trait declares, defaults included, in declaration
    /// order. An `impl` of the trait may define these and nothing else.
    trait_declared_methods: HashMap<String, Vec<String>>,
    /// `(trait, type)` pairs an `impl` block already claimed, with the span
    /// of that block, so a second one for the same pair is reported.
    claimed_trait_impls: HashMap<(String, String), Span>,
    /// Trait names each struct / enum derives, keyed by declared type name.
    derived_traits: HashMap<String, std::collections::HashSet<String>>,
    /// Supertrait names of each trait, keyed by trait name, from the
    /// `trait Pet: Animal` clause.
    trait_supertraits: HashMap<String, Vec<String>>,
    /// Callee nodes of a call sitting on the right of `|>`. A step with no
    /// arguments takes the piped value as its only one, supplied during HIR
    /// lowering, so the call writes one fewer argument than the callee's
    /// arity; the arity check accounts for the piped argument.
    pipe_stage_callees: std::collections::HashSet<NodeId>,
    /// Type of the value piped into a method call on the right of `|>`,
    /// keyed by the method-call node. The value lands in the method's
    /// last argument slot, so the built-in receiver surface counts and
    /// types it alongside the explicit arguments.
    pipe_stage_arg_tys: HashMap<NodeId, Ty>,
    /// Declared variant names of every user enum, keyed by enum name.
    /// Lets a `Enum::Variant` path reject an undeclared variant
    /// (`Shape::Triangle`) at check, instead of faulting at runtime.
    enum_variants: HashMap<String, std::collections::HashSet<String>>,
}

/// Saved generic-parameter scopes restored by
/// [`TypeChecker::leave_generic_scope`].
struct GenericScope {
    types: HashMap<String, (crate::ParamIdx, Box<str>)>,
    consts: HashMap<String, crate::ParamIdx>,
    bounds: Vec<Vec<String>>,
    assoc_bindings: HashMap<(String, String), gossamer_ast::Type>,
}

impl<'a> TypeChecker<'a> {
    fn new(tcx: &'a mut TyCtxt, resolutions: &'a Resolutions) -> Self {
        let checker_struct_fields = stdlib_struct_fields(tcx);
        Self {
            tcx,
            infer: InferCtxt::new(),
            table: TypeTable::new(),
            diagnostics: Vec::new(),
            resolutions,
            scopes: vec![HashMap::new()],
            mut_scopes: vec![HashMap::new()],
            binding_types: HashMap::new(),
            unused_result_allowed: false,
            must_use_fns: HashMap::new(),
            must_use_types: HashMap::new(),
            current_self_ty: None,
            current_impl_generics: None,
            current_impl_where: gossamer_ast::WhereClause::default(),
            current_self_ty_name: None,
            current_trait_name: None,
            current_assoc_bindings: HashMap::new(),
            assoc_expanding: std::collections::HashSet::new(),
            assoc: gossamer_ast::AssocIndex::default(),
            adt_def_by_name: HashMap::new(),
            recursion_depth: 0,
            recursion_limit_reported: false,
            consumed_iterators: vec![HashMap::new()],
            written_method: String::new(),
            mutable_borrows: vec![HashMap::new()],
            shared_borrows: vec![HashMap::new()],
            reference_origins: vec![HashMap::new()],
            suppressed: SuppressedReadChecks::default(),
            deferred_reference_storage: Vec::new(),
            struct_fields: checker_struct_fields,
            field_homes: HashMap::new(),
            synthesized_depth: 0,
            fn_sigs: HashMap::new(),
            method_arg_sigs: HashMap::new(),
            current_fn_ret: None,
            loop_break_tys: Vec::new(),
            method_ret_types: HashMap::new(),
            method_param_types: HashMap::new(),
            generic_method_ret_types: HashMap::new(),
            generic_method_param_types: HashMap::new(),
            method_arities: HashMap::new(),
            inherent_method_requires_mut: HashMap::new(),
            trait_impl_method_requires_mut: HashMap::new(),
            deferred_structural: Vec::new(),
            deferred_mutating_receivers: Vec::new(),
            deferred_private_fields: Vec::new(),
            deferred_type_mismatches: Vec::new(),
            deferred_into_conversions: Vec::new(),
            deferred_literal_type_mismatches: Vec::new(),
            deferred_scalar_method_rejections: Vec::new(),
            deferred_conversion_targets: Vec::new(),
            enum_variant_payloads: HashMap::new(),
            variant_ctor_substs: HashMap::new(),
            enum_variant_named_payloads: HashMap::new(),
            enum_tys: HashMap::new(),
            const_tys: HashMap::new(),
            const_int_values: HashMap::new(),
            static_mutability: HashMap::new(),
            alias_targets: HashMap::new(),
            alias_expanding: std::collections::HashSet::new(),
            nominal_aliases: std::collections::HashSet::new(),
            struct_generic_arity: HashMap::new(),
            fn_generic_arity: HashMap::new(),
            fn_param_bounds: HashMap::new(),
            adt_param_bounds: HashMap::new(),
            user_type_defs: HashMap::new(),
            deferred_adt_bounds: Vec::new(),
            fn_generic_const_mask: HashMap::new(),
            trait_impl_types: HashMap::new(),
            trait_method_ret: HashMap::new(),
            trait_method_params: HashMap::new(),
            trait_method_ret_assoc: HashMap::new(),
            trait_method_requires_mut: HashMap::new(),
            current_param_bounds: Vec::new(),
            current_generic_scope: HashMap::new(),
            current_const_generic_scope: HashMap::new(),
            declared_trait_names: std::collections::HashSet::new(),
            write_arg_bindings: HashMap::new(),
            callee_path_nodes: std::collections::HashSet::new(),
            import_targets: HashMap::new(),
            user_type_decls: std::collections::HashSet::new(),
            user_method_owners: HashMap::new(),
            user_fn_names: std::collections::HashSet::new(),
            deferred_shared_payloads: Vec::new(),
            method_homes: HashMap::new(),
            current_module: Vec::new(),
            trait_own_methods: HashMap::new(),
            trait_required_methods: builtin_trait_required_methods(),
            user_ordered_types: std::collections::HashSet::new(),
            trait_declared_methods: HashMap::new(),
            claimed_trait_impls: HashMap::new(),
            derived_traits: HashMap::new(),
            trait_supertraits: HashMap::new(),
            pipe_stage_callees: std::collections::HashSet::new(),
            pipe_stage_arg_tys: HashMap::new(),
            enum_variants: HashMap::new(),
        }
    }

    fn collect_import_targets(&mut self, uses: &[gossamer_ast::UseDecl]) {
        for use_decl in uses {
            let gossamer_ast::UseTarget::Module(path) = &use_decl.target else {
                continue;
            };
            let base: Vec<String> = path.segments.iter().map(|seg| seg.name.clone()).collect();
            if let Some(list) = &use_decl.list {
                for entry in list {
                    let bound = entry.alias.as_ref().unwrap_or(&entry.name).name.clone();
                    let mut full = base.clone();
                    full.extend(entry.prefix.iter().map(|ident| ident.name.clone()));
                    full.push(entry.name.name.clone());
                    self.import_targets.insert((use_decl.id, bound), full);
                }
            } else if let Some(bound) = use_decl
                .alias
                .as_ref()
                .map_or_else(|| base.last().cloned(), |alias| Some(alias.name.clone()))
            {
                self.import_targets.insert((use_decl.id, bound), base);
            }
        }
    }

    fn resolved_value_path_names(
        &self,
        node: NodeId,
        path: &gossamer_ast::PathExpr,
    ) -> Vec<String> {
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|segment| segment.name.name.clone())
            .collect();
        if segments.len() != 1 {
            return segments;
        }
        let name = &segments[0];
        let Some(Resolution::Import { use_id }) = self.resolutions.get(node) else {
            return segments;
        };
        self.import_targets
            .get(&(use_id, name.clone()))
            .cloned()
            .unwrap_or(segments)
    }

    /// Returns `Err(())` once the recursion counter hits
    /// [`RECURSION_LIMIT`], emitting a one-shot diagnostic at `span`.
    /// Callers should respond by returning `tcx.error_ty()` so the
    /// caller's caller stops walking into the doomed subtree.
    fn enter_recursion(&mut self, span: Span) -> Result<(), ()> {
        if self.recursion_depth >= RECURSION_LIMIT {
            if !self.recursion_limit_reported {
                self.recursion_limit_reported = true;
                self.emit(
                    TypeError::RecursionLimit {
                        limit: RECURSION_LIMIT,
                    },
                    span,
                );
            }
            return Err(());
        }
        self.recursion_depth += 1;
        Ok(())
    }

    /// Pairs with [`Self::enter_recursion`]. Decrements the counter.
    fn leave_recursion(&mut self) {
        self.recursion_depth = self.recursion_depth.saturating_sub(1);
    }

    /// Pushes a generic-parameter scope while walking a declaration
    /// (struct, enum, fn, trait, impl). Each parameter name maps to
    /// its position so `type_from_ast_path` renders references as
    /// the right `TyKind::Param`. Returns the prior scope so the
    /// caller can restore it.
    /// Per-parameter list of bound trait names for a generics clause,
    /// indexed by full parameter position so const and lifetime slots
    /// keep their place (`<'a, T: A + B, const N: usize>` ->
    /// `[[], [A, B], []]`).
    fn type_param_bounds(generics: &gossamer_ast::Generics) -> Vec<Vec<String>> {
        generics
            .params
            .iter()
            .map(|p| match p {
                gossamer_ast::GenericParam::Type { bounds, .. } => bound_names(bounds),
                _ => Vec::new(),
            })
            .collect()
    }

    /// Per-parameter bound trait names for a declaration's generics with its
    /// `where` predicates folded in, so `where T: Shape` constrains `T`
    /// exactly as `<T: Shape>` does. Indexed by full parameter position.
    fn declared_param_bounds(
        generics: &gossamer_ast::Generics,
        where_clause: &gossamer_ast::WhereClause,
    ) -> Vec<Vec<String>> {
        let mut bounds = Self::type_param_bounds(generics);
        merge_where_predicates(&generics.params, 0, where_clause, &mut bounds);
        bounds
    }

    /// Per-parameter bound trait names for an `impl` block's generics
    /// followed by a method's own generics, in the index order
    /// [`Self::enter_generic_scope_combined`] assigns. Both `where` clauses
    /// fold into the same table, so an impl-level and a method-level bound
    /// on distinct parameters never displace one another.
    fn combined_param_bounds(
        outer: &gossamer_ast::Generics,
        outer_where: &gossamer_ast::WhereClause,
        inner: &gossamer_ast::Generics,
        inner_where: &gossamer_ast::WhereClause,
    ) -> Vec<Vec<String>> {
        let mut bounds = Vec::with_capacity(outer.params.len() + inner.params.len());
        for param in outer.params.iter().chain(inner.params.iter()) {
            bounds.push(match param {
                gossamer_ast::GenericParam::Type { bounds, .. } => bound_names(bounds),
                _ => Vec::new(),
            });
        }
        merge_where_predicates(&outer.params, 0, outer_where, &mut bounds);
        merge_where_predicates(&inner.params, outer.params.len(), inner_where, &mut bounds);
        bounds
    }

    /// Associated-type equality constraints written on `generics` and its
    /// `where` predicates, keyed by `(parameter name, associated type
    /// name)`. Parameter names are unique inside one scope, so an impl's
    /// and a method's constraints merge into a single table.
    fn assoc_bindings_of(
        generics: &gossamer_ast::Generics,
        where_clause: &gossamer_ast::WhereClause,
        out: &mut HashMap<(String, String), gossamer_ast::Type>,
    ) {
        for param in &generics.params {
            let gossamer_ast::GenericParam::Type { name, bounds, .. } = param else {
                continue;
            };
            collect_assoc_bindings(&name.name, bounds, out);
        }
        for predicate in &where_clause.predicates {
            let Some(name) = bare_path_type_name(&predicate.bounded) else {
                continue;
            };
            collect_assoc_bindings(name, &predicate.bounds, out);
        }
    }

    /// Per-parameter flags marking which generic positions are const
    /// parameters, indexed by full parameter position.
    fn const_param_mask(generics: &gossamer_ast::Generics) -> Vec<bool> {
        generics
            .params
            .iter()
            .map(|p| matches!(p, gossamer_ast::GenericParam::Const { .. }))
            .collect()
    }

    fn enter_generic_scope(&mut self, generics: &gossamer_ast::Generics) -> GenericScope {
        let prior_types = std::mem::take(&mut self.current_generic_scope);
        let prior_consts = std::mem::take(&mut self.current_const_generic_scope);
        let prior_bounds = std::mem::replace(
            &mut self.current_param_bounds,
            Self::type_param_bounds(generics),
        );
        let mut bindings = HashMap::new();
        Self::assoc_bindings_of(
            generics,
            &gossamer_ast::WhereClause::default(),
            &mut bindings,
        );
        let prior_bindings = std::mem::replace(&mut self.current_assoc_bindings, bindings);
        for (i, param) in generics.params.iter().enumerate() {
            match param {
                gossamer_ast::GenericParam::Type { name, .. } => {
                    let owned: Box<str> = name.name.clone().into_boxed_str();
                    self.current_generic_scope
                        .insert(name.name.clone(), (crate::ParamIdx(i as u32), owned));
                }
                gossamer_ast::GenericParam::Const { name, .. } => {
                    self.current_const_generic_scope
                        .insert(name.name.clone(), crate::ParamIdx(i as u32));
                }
                gossamer_ast::GenericParam::Lifetime { .. } => {}
            }
        }
        GenericScope {
            types: prior_types,
            consts: prior_consts,
            bounds: prior_bounds,
            assoc_bindings: prior_bindings,
        }
    }

    /// Enters a generic scope combining an `impl` block's generics (first,
    /// so they keep the `Param` indices the struct's fields use) with a
    /// method's own generics (offset after them). Used when checking a
    /// method of `impl<T> Wrapper<T>`, so `-> T` records `Param(0)`
    /// matching `Wrapper`'s first generic, while the method's own `<U>`
    /// gets the next index.
    fn enter_generic_scope_combined(
        &mut self,
        outer: &gossamer_ast::Generics,
        inner: &gossamer_ast::Generics,
    ) -> GenericScope {
        let prior_types = std::mem::take(&mut self.current_generic_scope);
        let prior_consts = std::mem::take(&mut self.current_const_generic_scope);
        let prior_bounds = std::mem::replace(
            &mut self.current_param_bounds,
            Self::combined_param_bounds(
                outer,
                &gossamer_ast::WhereClause::default(),
                inner,
                &gossamer_ast::WhereClause::default(),
            ),
        );
        let mut bindings = HashMap::new();
        Self::assoc_bindings_of(outer, &gossamer_ast::WhereClause::default(), &mut bindings);
        Self::assoc_bindings_of(inner, &gossamer_ast::WhereClause::default(), &mut bindings);
        let prior_bindings = std::mem::replace(&mut self.current_assoc_bindings, bindings);
        // Every parameter position advances the index, matching the full
        // positional numbering `type_param_bounds` and `const_param_mask`
        // use, so a bounds or const-mask lookup indexes the same slot.
        for (idx, param) in outer.params.iter().chain(inner.params.iter()).enumerate() {
            let idx = crate::ParamIdx(idx as u32);
            match param {
                gossamer_ast::GenericParam::Type { name, .. } => {
                    let owned: Box<str> = name.name.clone().into_boxed_str();
                    self.current_generic_scope
                        .insert(name.name.clone(), (idx, owned));
                }
                gossamer_ast::GenericParam::Const { name, .. } => {
                    self.current_const_generic_scope
                        .insert(name.name.clone(), idx);
                }
                gossamer_ast::GenericParam::Lifetime { .. } => {}
            }
        }
        GenericScope {
            types: prior_types,
            consts: prior_consts,
            bounds: prior_bounds,
            assoc_bindings: prior_bindings,
        }
    }

    /// Restores a generic-parameter scope saved by
    /// [`Self::enter_generic_scope`].
    fn leave_generic_scope(&mut self, prior: GenericScope) {
        self.current_generic_scope = prior.types;
        self.current_const_generic_scope = prior.consts;
        self.current_param_bounds = prior.bounds;
        self.current_assoc_bindings = prior.assoc_bindings;
    }

    /// Verifies each instantiated type parameter of a generic call
    /// satisfies its declared trait bounds: the concrete type must carry
    /// an `impl Bound for Type` (or `Bound` is a recognised built-in
    /// trait). A still-unresolved parameter or a non-named type is left
    /// for argument unification to report; only a concrete type with a
    /// definitely-missing impl is flagged.
    fn check_trait_bounds(&mut self, def: gossamer_resolve::DefId, vars: &[Ty], span: Span) {
        let Some(bounds) = self.fn_param_bounds.get(&def).cloned() else {
            return;
        };
        for (i, var) in vars.iter().enumerate() {
            let resolved = self.infer.resolve(self.tcx, *var);
            self.reject_builtin_iterator_instantiation(
                resolved,
                bounds.get(i).map(Vec::as_slice).unwrap_or_default(),
                span,
            );
            let Some(ty_name) = self.concrete_type_name(resolved) else {
                continue;
            };
            for bound in bounds.get(i).into_iter().flatten() {
                if self.bound_is_satisfied(bound, &ty_name) {
                    continue;
                }
                self.emit(
                    TypeError::TraitBoundNotSatisfied {
                        ty: ty_name.clone(),
                        bound: bound.clone(),
                    },
                    span,
                );
            }
        }
    }

    /// Records the declared bounds on a struct / enum's generic parameters,
    /// merging with anything an earlier `impl` block already attached.
    fn record_adt_param_bounds(
        &mut self,
        def: gossamer_resolve::DefId,
        generics: &gossamer_ast::Generics,
        where_clause: &gossamer_ast::WhereClause,
    ) {
        let bounds = Self::declared_param_bounds(generics, where_clause);
        if bounds.iter().all(Vec::is_empty) {
            return;
        }
        merge_bound_table(self.adt_param_bounds.entry(def).or_default(), &bounds);
    }

    /// Attaches an `impl` block's parameter bounds to the type it targets
    /// when the self type instantiates that type with the impl's own
    /// parameters in declaration order (`impl<T: Shape> Wrapper<T>`). Any
    /// other self-type shape names no declaration position to bound.
    fn record_impl_param_bounds(&mut self, decl: &ImplDecl) {
        let bounds = Self::declared_param_bounds(&decl.generics, &decl.where_clause);
        if bounds.iter().all(Vec::is_empty) {
            return;
        }
        let gossamer_ast::ty::TypeKind::Path(path) = &decl.self_ty.kind else {
            return;
        };
        let Some(segment) = path.segments.last() else {
            return;
        };
        let Some(def) = self.user_type_defs.get(&segment.name.name).copied() else {
            return;
        };
        let param_names: Vec<&str> = decl
            .generics
            .params
            .iter()
            .map(|param| match param {
                gossamer_ast::GenericParam::Type { name, .. }
                | gossamer_ast::GenericParam::Const { name, .. } => name.name.as_str(),
                gossamer_ast::GenericParam::Lifetime { name } => name.as_str(),
            })
            .collect();
        let applied: Vec<Option<&str>> = segment
            .generics
            .iter()
            .map(|arg| match arg {
                AstGenericArg::Type(ty) => bare_path_type_name(ty),
                AstGenericArg::Const(_) => None,
            })
            .collect();
        if applied.len() != param_names.len()
            || !applied
                .iter()
                .zip(&param_names)
                .all(|(applied, declared)| *applied == Some(*declared))
        {
            return;
        }
        merge_bound_table(self.adt_param_bounds.entry(def).or_default(), &bounds);
    }

    /// Reports every method a trait declares without a default body that
    /// this `impl` block leaves out. Monomorphisation emits a direct call
    /// to each such method, so the impl has to supply a body for it.
    fn check_trait_impl_completeness(&mut self, decl: &ImplDecl, span: Span) {
        let Some(trait_name) = decl
            .trait_ref
            .as_ref()
            .and_then(|trait_ref| trait_ref.path.segments.last())
            .map(|segment| segment.name.name.clone())
        else {
            return;
        };
        let self_ty = impl_self_ty_name(decl);
        // A header naming a trait nothing declares promises a contract that
        // cannot be checked, so the block's methods would silently become
        // inherent ones - which is how a misspelled name compiles clean.
        if !self.trait_own_methods.contains_key(&trait_name) && !known_builtin_trait(&trait_name) {
            self.emit(
                TypeError::UnknownImplTrait {
                    name: trait_name,
                    ty: self_ty,
                },
                span,
            );
            return;
        }
        // A trait the language supplies itself is decided once, for every
        // type: a block naming one would sit there with nothing dispatching
        // through it, and the behaviour it means to change would not change.
        if !self.trait_own_methods.contains_key(&trait_name)
            && let Some(entry) = crate::builtin_traits::builtin_trait(&trait_name)
            && entry.kind == crate::builtin_traits::BuiltinTraitKind::Automatic
        {
            self.emit(
                TypeError::AutomaticTraitImpl {
                    name: trait_name,
                    ty: self_ty,
                    reason: entry.doc.to_string(),
                    instead: entry.instead.to_string(),
                },
                span,
            );
            return;
        }
        let Some(required) = self.trait_required_methods.get(&trait_name).cloned() else {
            return;
        };
        let supplied: std::collections::HashSet<&str> = decl
            .items
            .iter()
            .filter_map(|item| match item {
                ImplItem::Fn(fn_decl) => Some(fn_decl.name.name.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = required
            .into_iter()
            .filter(|method| !supplied.contains(method.as_str()))
            .collect();
        if missing.is_empty() {
            return;
        }
        self.emit(
            TypeError::MissingTraitImplMethods {
                trait_name,
                ty: self_ty,
                missing,
            },
            span,
        );
    }

    /// Reports every item an `impl Trait for Type` block defines that the
    /// trait does not declare. The header promises exactly the trait's
    /// contract, so anything outside it would become an inherent method
    /// under a misleading heading and never dispatch through the trait.
    fn check_trait_impl_membership(&mut self, decl: &ImplDecl, span: Span) {
        let Some(trait_name) = decl
            .trait_ref
            .as_ref()
            .and_then(|trait_ref| trait_ref.path.segments.last())
            .map(|segment| segment.name.name.clone())
        else {
            return;
        };
        let Some(declared) = self.trait_declared_item_names(&trait_name) else {
            return;
        };
        let self_ty = impl_self_ty_name(decl);
        for item in &decl.items {
            let name = match item {
                ImplItem::Fn(fn_decl) => fn_decl.name.name.clone(),
                ImplItem::Type { name, .. } | ImplItem::Const { name, .. } => name.name.clone(),
            };
            if declared.contains(&name) {
                continue;
            }
            self.emit(
                TypeError::ImplItemNotInTrait {
                    trait_name: trait_name.clone(),
                    ty: self_ty.clone(),
                    item: name,
                    declared: declared.clone(),
                },
                span,
            );
        }
    }

    /// Every item name an `impl` of `trait_name` may define, in declaration
    /// order. `None` means the trait's surface is not known here, so nothing
    /// the block defines can be ruled out.
    fn trait_declared_item_names(&self, trait_name: &str) -> Option<Vec<String>> {
        if let Some(methods) = self.trait_declared_methods.get(trait_name) {
            let mut names = methods.clone();
            names.extend(
                self.assoc
                    .declared_assoc_names(trait_name)
                    .into_iter()
                    .map(ToString::to_string),
            );
            return Some(names);
        }
        builtin_trait_impl_items(trait_name)
            .map(|items| items.iter().map(ToString::to_string).collect())
    }

    /// Reports a second `impl Trait for Type` for a pair one block already
    /// claimed, and a written `impl` that duplicates what a `#[derive(..)]`
    /// on the type already supplies. Either way a call through the trait
    /// has two bodies to reach and no rule picks one.
    fn check_trait_impl_uniqueness(&mut self, decl: &ImplDecl, module_path: &[String], span: Span) {
        let Some(trait_name) = decl
            .trait_ref
            .as_ref()
            .and_then(|trait_ref| trait_ref.path.segments.last())
            .map(|segment| segment.name.name.clone())
        else {
            return;
        };
        // The key is the name a dispatch identifies the receiver by, so two
        // blocks it cannot separate collide here; the message names what the
        // block was written for, which is what the reader is looking at.
        let self_ty = written_type_name(&decl.self_ty);
        let key = (
            trait_name.clone(),
            qualified_type_name(module_path, &impl_self_ty_name(decl)),
        );
        // The collection pass is idempotent, so the same block may be visited
        // more than once; only a block at a different span is a second impl.
        match self.claimed_trait_impls.get(&key) {
            Some(claimed) if *claimed == span => return,
            Some(_) => {
                self.emit(
                    TypeError::ConflictingTraitImpl {
                        trait_name,
                        ty: self_ty,
                        derived: false,
                    },
                    span,
                );
                return;
            }
            None => {}
        }
        if self
            .derived_traits
            .get(&key.1)
            .is_some_and(|derives| derives.contains(&trait_name))
        {
            self.emit(
                TypeError::ConflictingTraitImpl {
                    trait_name,
                    ty: self_ty,
                    derived: true,
                },
                span,
            );
        }
        self.claimed_trait_impls.insert(key, span);
    }

    /// Reports every associated type and constant a trait declares without
    /// a default that this `impl` block leaves out. A projection through
    /// the trait has to land on a concrete item in the impl.
    fn check_trait_impl_assoc_items(&mut self, decl: &ImplDecl, span: Span) {
        let Some(trait_name) = decl.trait_ref.as_ref().and_then(|b| b.trait_name()) else {
            return;
        };
        let trait_name = trait_name.to_string();
        let required = self.assoc.required_assoc_items(&trait_name);
        if required.is_empty() {
            return;
        }
        let missing: Vec<String> = required
            .iter()
            .filter(|item| {
                !decl.items.iter().any(|supplied| match supplied {
                    ImplItem::Type { name, .. } => item.kind == "type" && name.name == item.name,
                    ImplItem::Const { name, .. } => item.kind == "const" && name.name == item.name,
                    ImplItem::Fn(_) => false,
                })
            })
            .map(|item| format!("{} {}", item.kind, item.name))
            .collect();
        if missing.is_empty() {
            return;
        }
        let ty = gossamer_ast::assoc::type_head_name(&decl.self_ty)
            .map_or_else(|| "this type".to_string(), ToString::to_string);
        self.emit(
            TypeError::MissingTraitImplAssocItems {
                trait_name,
                ty,
                missing,
            },
            span,
        );
    }

    /// Records a construction of `def` with `substs` for the end-of-run
    /// bound check. Sites with no declared bounds are dropped immediately.
    fn defer_adt_bounds(&mut self, def: gossamer_resolve::DefId, substs: &[Ty], span: Span) {
        if substs.is_empty() || !self.adt_param_bounds.contains_key(&def) {
            return;
        }
        self.deferred_adt_bounds.push((def, substs.to_vec(), span));
    }

    /// Verifies every recorded construction of a bounded generic type
    /// against its declared bounds, once inference has pinned the
    /// arguments.
    fn check_deferred_adt_bounds(&mut self) {
        for (def, substs, span) in std::mem::take(&mut self.deferred_adt_bounds) {
            let Some(bounds) = self.adt_param_bounds.get(&def).cloned() else {
                continue;
            };
            for (i, arg) in substs.iter().enumerate() {
                let resolved = self.infer.resolve(self.tcx, *arg);
                let Some(ty_name) = self.concrete_type_name(resolved) else {
                    continue;
                };
                for bound in bounds.get(i).into_iter().flatten() {
                    if self.bound_is_satisfied(bound, &ty_name) {
                        continue;
                    }
                    self.emit(
                        TypeError::TraitBoundNotSatisfied {
                            ty: ty_name.clone(),
                            bound: bound.clone(),
                        },
                        span,
                    );
                }
            }
        }
    }

    /// Whether the concrete type named `ty_name` carries `bound`.
    ///
    /// A trait declared in this unit is always checked against its impls,
    /// even when its name matches a built-in one. A built-in name with no
    /// declaration behind it is checked only when the language expects an
    /// explicit `impl` block to supply it: the operator traits. Every other
    /// built-in name (`Clone`, `Debug`, `Ord`, ...) names behaviour the
    /// language derives automatically and so is always satisfied.
    fn bound_is_satisfied(&self, bound: &str, ty_name: &str) -> bool {
        if known_builtin_trait(bound)
            && !self.declared_trait_names.contains(bound)
            && !builtin_trait_needs_impl(bound)
        {
            return true;
        }
        self.trait_impl_types
            .get(bound)
            .is_some_and(|types| types.contains(ty_name))
    }

    /// Name of a concrete named type (`Dog`, `Cat`), peeling `&` / `&mut`.
    /// `None` for inference variables, primitives, and structural types so
    /// a bound check only fires on a definitely-named type.
    fn concrete_type_name(&self, ty: Ty) -> Option<String> {
        let mut t = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = *inner;
        }
        match self.tcx.kind(t) {
            Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def).map(str::to_string),
            _ => None,
        }
    }

    fn fresh(&mut self) -> Ty {
        self.infer.fresh_var(self.tcx)
    }

    /// True when `ty` names a generic parameter anywhere inside it. A
    /// declared return that does is only meaningful under the call site's
    /// substitution, so it is not recorded as a concrete method return.
    fn ty_mentions_generic_param(&self, ty: Ty) -> bool {
        let mut seen = Vec::new();
        self.ty_mentions_generic_param_at(ty, 0, &mut seen)
    }

    fn ty_mentions_generic_param_at(&self, ty: Ty, depth: u32, seen: &mut Vec<Ty>) -> bool {
        if depth > 12 || seen.contains(&ty) {
            return false;
        }
        seen.push(ty);
        match self.tcx.kind(ty) {
            Some(TyKind::Param { .. }) => true,
            Some(
                TyKind::Vec(inner)
                | TyKind::Slice(inner)
                | TyKind::Array { elem: inner, .. }
                | TyKind::Ref { inner, .. },
            ) => self.ty_mentions_generic_param_at(*inner, depth + 1, seen),
            Some(TyKind::Tuple(parts)) => {
                for part in parts {
                    if self.ty_mentions_generic_param_at(*part, depth + 1, seen) {
                        return true;
                    }
                }
                false
            }
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.ty_mentions_generic_param_at(key, depth + 1, seen)
                    || self.ty_mentions_generic_param_at(value, depth + 1, seen)
            }
            Some(TyKind::Adt { substs, .. }) => {
                for arg in substs.types() {
                    if self.ty_mentions_generic_param_at(arg, depth + 1, seen) {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Walks `ty` and substitutes each `TyKind::Param { idx }`
    /// reference with `substs[idx]`. Used at struct-literal and
    /// generic-call sites where the declared field/parameter
    /// types carry rigid `Param` slots that must be replaced by
    /// fresh inference vars (or by explicit generic arguments)
    /// before unification.
    ///
    /// Out-of-range `idx` falls back to the original `ty` so a
    /// malformed declaration produces a deferred unification
    /// error rather than a panic.
    fn subst_params_in_ty(&mut self, ty: Ty, substs: &[Ty]) -> Ty {
        self.subst_generics_in_ty(ty, substs, &[])
    }

    /// Infers a const generic array length from a call argument: a
    /// parameter typed `[T; N]` (peeling references) matched against an
    /// argument of concrete length yields `(N's param index, length)`.
    fn infer_array_const_len(&self, param_ty: Ty, arg_ty: Ty) -> Option<(usize, i128)> {
        let mut p = param_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(p) {
            p = *inner;
        }
        let TyKind::Array {
            len: crate::ArrayLen::Param(idx),
            ..
        } = self.tcx.kind_of(p)
        else {
            return None;
        };
        let idx = idx.0 as usize;
        // An argument that is a binding rather than a literal reaches here as
        // the inference variable its `let` bound, so the length is read after
        // resolving it: the array's own length is what names the parameter.
        let mut a = self.infer.resolve(self.tcx, arg_ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(a) {
            a = *inner;
        }
        let TyKind::Array {
            len: crate::ArrayLen::Concrete(n),
            ..
        } = self.tcx.kind_of(a)
        else {
            return None;
        };
        Some((idx, *n as i128))
    }

    /// Like [`Self::subst_params_in_ty`] but also rewrites a const
    /// generic array length (`[T; N]` where `N` is the `idx`-th
    /// parameter) to a concrete `ArrayLen` when `const_substs[idx]`
    /// supplies a value. Used at generic call sites where the const
    /// argument is inferred from the array argument's length.
    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "type-constructor dispatch - arms map 1:1 to TyKind variants; splitting hides the type walk"
    )]
    fn subst_generics_in_ty(&mut self, ty: Ty, substs: &[Ty], const_substs: &[Option<i128>]) -> Ty {
        if substs.is_empty() && const_substs.iter().all(Option::is_none) {
            return ty;
        }
        let kind = self.tcx.kind_of(ty).clone();
        match kind {
            TyKind::Param { idx, .. } => substs.get(idx.0 as usize).copied().unwrap_or(ty),
            TyKind::Ref { inner, mutability } => {
                let new_inner = self.subst_generics_in_ty(inner, substs, const_substs);
                if new_inner == inner {
                    ty
                } else {
                    self.tcx.intern(TyKind::Ref {
                        inner: new_inner,
                        mutability,
                    })
                }
            }
            TyKind::Tuple(elems) => {
                let new_elems: Vec<Ty> = elems
                    .iter()
                    .map(|e| self.subst_generics_in_ty(*e, substs, const_substs))
                    .collect();
                if new_elems == elems {
                    ty
                } else {
                    self.tcx.intern(TyKind::Tuple(new_elems))
                }
            }
            TyKind::Array { elem, len } => {
                let new_elem = self.subst_generics_in_ty(elem, substs, const_substs);
                let new_len = subst_array_len(len, const_substs);
                if new_elem == elem && new_len == len {
                    ty
                } else {
                    self.tcx.intern(TyKind::Array {
                        elem: new_elem,
                        len: new_len,
                    })
                }
            }
            TyKind::Slice(elem) => {
                let new = self.subst_generics_in_ty(elem, substs, const_substs);
                if new == elem {
                    ty
                } else {
                    self.tcx.intern(TyKind::Slice(new))
                }
            }
            TyKind::Vec(elem) => {
                let new = self.subst_generics_in_ty(elem, substs, const_substs);
                if new == elem {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(new))
                }
            }
            TyKind::HashMap {
                key,
                value,
                ordered,
            } => {
                let new_k = self.subst_generics_in_ty(key, substs, const_substs);
                let new_v = self.subst_generics_in_ty(value, substs, const_substs);
                if new_k == key && new_v == value {
                    ty
                } else {
                    self.tcx.intern(TyKind::HashMap {
                        key: new_k,
                        value: new_v,
                        ordered,
                    })
                }
            }
            TyKind::Sender(inner) => {
                let new = self.subst_generics_in_ty(inner, substs, const_substs);
                if new == inner {
                    ty
                } else {
                    self.tcx.intern(TyKind::Sender(new))
                }
            }
            TyKind::Receiver(inner) => {
                let new = self.subst_generics_in_ty(inner, substs, const_substs);
                if new == inner {
                    ty
                } else {
                    self.tcx.intern(TyKind::Receiver(new))
                }
            }
            TyKind::JoinHandle(inner) => {
                let new = self.subst_generics_in_ty(inner, substs, const_substs);
                if new == inner {
                    ty
                } else {
                    self.tcx.intern(TyKind::JoinHandle(new))
                }
            }
            // A callable parameter's own signature holds the function's
            // rigid `Param`s (`f: Fn() -> T`), so substitute into it too:
            // left rigid, the instantiated signature would ask a call site
            // for `Fn() -> T` and reject the concrete callable it passed.
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                let new_sig = FnSig {
                    inputs: sig
                        .inputs
                        .iter()
                        .map(|t| self.subst_generics_in_ty(*t, substs, const_substs))
                        .collect(),
                    output: self.subst_generics_in_ty(sig.output, substs, const_substs),
                };
                if new_sig == sig {
                    ty
                } else if matches!(self.tcx.kind_of(ty), TyKind::FnTrait(_)) {
                    self.tcx.intern(TyKind::FnTrait(new_sig))
                } else {
                    self.tcx.intern(TyKind::FnPtr(new_sig))
                }
            }
            // Adt / Alias carry their own generic-argument lists; a
            // function signature naming a generic struct (`w: Wrapper<T>`)
            // holds the function's rigid `Param` inside those args, so
            // substitute into them too. Without this, instantiating the
            // signature leaves `Wrapper<Param>` and unifying it against a
            // concrete `Wrapper<i64>` argument fails (rigid `Param` vs
            // `i64`).
            TyKind::Adt {
                def,
                substs: adt_substs,
            } => {
                let new_substs = self.subst_generics_in_substs(&adt_substs, substs, const_substs);
                if new_substs == adt_substs {
                    ty
                } else {
                    self.tcx.intern(TyKind::Adt {
                        def,
                        substs: new_substs,
                    })
                }
            }
            TyKind::Alias {
                def,
                substs: alias_substs,
            } => {
                let new_substs = self.subst_generics_in_substs(&alias_substs, substs, const_substs);
                if new_substs == alias_substs {
                    ty
                } else {
                    self.tcx.intern(TyKind::Alias {
                        def,
                        substs: new_substs,
                    })
                }
            }
            _ => ty,
        }
    }

    /// Substitutes the generic-parameter slots inside a `Substs`' type
    /// arguments, leaving const arguments untouched. Used by
    /// [`Self::subst_generics_in_ty`] to instantiate a generic struct named
    /// in a function signature (`Wrapper<T>`).
    fn subst_generics_in_substs(
        &mut self,
        args: &crate::Substs,
        substs: &[Ty],
        const_substs: &[Option<i128>],
    ) -> crate::Substs {
        let new_args: Vec<crate::GenericArg> = args
            .as_slice()
            .iter()
            .map(|a| match a {
                crate::GenericArg::Type(t) => {
                    crate::GenericArg::Type(self.subst_generics_in_ty(*t, substs, const_substs))
                }
                crate::GenericArg::Const(c) => crate::GenericArg::Const(*c),
            })
            .collect();
        crate::Substs::from_args(new_args)
    }

    fn emit(&mut self, error: TypeError, span: Span) {
        // A type that failed to check renders as a placeholder. Reporting it
        // again names something absent from the source, so the diagnostic
        // that produced the placeholder stands as the only report.
        if error.mentions_error_type() {
            return;
        }
        self.diagnostics.push(TypeDiagnostic::new(error, span));
    }

    fn record(&mut self, node: NodeId, ty: Ty) -> Ty {
        self.table.insert(node, ty);
        ty
    }

    fn resolve_table(&mut self) {
        let pairs: Vec<(NodeId, Ty)> = self.table.sorted_entries();
        for (node, ty) in pairs {
            let resolved = self.deep_resolve(ty);
            if resolved != ty {
                self.table.insert(node, resolved);
            }
        }
    }

    /// Resolves a type deeply - after shallow-resolving top-level `Var`
    /// nodes, recurses into `FnPtr` / `FnTrait` sigs so that compound
    /// types like `FnPtr(FnSig { output: Var(1) })` are fully grounded
    /// when the inference var was unified with a concrete type.
    /// Deep-resolves every type argument inside a `Substs`.
    fn deep_resolve_substs(&mut self, substs: &crate::Substs) -> crate::Substs {
        let new_args: Vec<crate::GenericArg> = substs
            .as_slice()
            .iter()
            .map(|arg| match arg {
                crate::GenericArg::Type(t) => crate::GenericArg::Type(self.deep_resolve(*t)),
                crate::GenericArg::Const(c) => crate::GenericArg::Const(*c),
            })
            .collect();
        crate::Substs::from_args(new_args)
    }

    /// Deep-resolves a map's key and value, keeping which map it is.
    fn deep_resolve_map(&mut self, resolved: Ty, key: Ty, value: Ty, ordered: bool) -> Ty {
        let k = self.deep_resolve(key);
        let v = self.deep_resolve(value);
        if k == key && v == value {
            return resolved;
        }
        self.tcx.intern(TyKind::HashMap {
            key: k,
            value: v,
            ordered,
        })
    }

    fn deep_resolve(&mut self, ty: Ty) -> Ty {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind_of(resolved).clone() {
            TyKind::FnPtr(sig) => {
                let out = self.deep_resolve(sig.output);
                let inputs: Vec<Ty> = sig.inputs.iter().map(|&t| self.deep_resolve(t)).collect();
                if out != sig.output || inputs != sig.inputs {
                    self.tcx.intern(TyKind::FnPtr(FnSig {
                        inputs,
                        output: out,
                    }))
                } else {
                    resolved
                }
            }
            TyKind::FnTrait(sig) => {
                let out = self.deep_resolve(sig.output);
                let inputs: Vec<Ty> = sig.inputs.iter().map(|&t| self.deep_resolve(t)).collect();
                if out != sig.output || inputs != sig.inputs {
                    self.tcx.intern(TyKind::FnTrait(FnSig {
                        inputs,
                        output: out,
                    }))
                } else {
                    resolved
                }
            }
            // Recurse into generic arguments / element types so a
            // `Triple<?4, ?5, ?6>` whose inference vars unified to
            // `<i64, String, f64>` is recorded as the concrete
            // `Triple<i64, String, f64>`. Without this, a generic
            // struct's field access (`r.third`) reads the field's
            // `Param(n)` against unresolved-`Var` substs and the field
            // local defaults to i64/ptr - printing an `f64`'s bit
            // pattern or strlen'ing a non-pointer.
            TyKind::Adt { def, substs } => {
                let new_substs = self.deep_resolve_substs(&substs);
                if new_substs == substs {
                    resolved
                } else {
                    self.tcx.intern(TyKind::Adt {
                        def,
                        substs: new_substs,
                    })
                }
            }
            // A generic call's callee carries the per-call-site type
            // arguments as inference variables; resolve them so the MIR
            // monomorphiser sees the concrete instantiation (`fn<Dog>`)
            // rather than an unresolved variable.
            TyKind::FnDef { def, substs } => {
                let new_substs = self.deep_resolve_substs(&substs);
                if new_substs == substs {
                    resolved
                } else {
                    self.tcx.intern(TyKind::FnDef {
                        def,
                        substs: new_substs,
                    })
                }
            }
            // Recurse into element / payload types so a composite whose
            // inner inference var only gained a concrete type via the
            // late integer/float defaulting (`let v = if c { [1, 2] }
            // else { [3, 4] }` -> `[i64; 2]`) is recorded fully grounded.
            // Without this the recorded node keeps `[?v; 2]` and the
            // format/codegen dispatch can't classify the element.
            TyKind::Array { elem, len } => {
                let new_elem = self.deep_resolve(elem);
                if new_elem == elem {
                    resolved
                } else {
                    self.tcx.intern(TyKind::Array {
                        elem: new_elem,
                        len,
                    })
                }
            }
            TyKind::Slice(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::Slice),
            TyKind::Vec(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::Vec),
            TyKind::Iterator(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::Iterator),
            TyKind::Range(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::Range),
            TyKind::Sender(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::Sender),
            TyKind::Receiver(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::Receiver),
            TyKind::JoinHandle(elem) => self.deep_resolve_wrap(resolved, elem, TyKind::JoinHandle),
            TyKind::Ref { mutability, inner } => {
                let new_inner = self.deep_resolve(inner);
                if new_inner == inner {
                    resolved
                } else {
                    self.tcx.intern(TyKind::Ref {
                        mutability,
                        inner: new_inner,
                    })
                }
            }
            TyKind::Tuple(elems) => {
                let new: Vec<Ty> = elems.iter().map(|&t| self.deep_resolve(t)).collect();
                if new == elems {
                    resolved
                } else {
                    self.tcx.intern(TyKind::Tuple(new))
                }
            }
            TyKind::HashMap {
                key,
                value,
                ordered,
            } => self.deep_resolve_map(resolved, key, value, ordered),
            _ => resolved,
        }
    }

    /// Deep-resolves a single-payload composite (`Vec`/`Slice`/channel
    /// endpoints) and re-interns it through `wrap` only when the payload
    /// actually changed.
    fn deep_resolve_wrap(&mut self, resolved: Ty, elem: Ty, wrap: fn(Ty) -> TyKind) -> Ty {
        let new_elem = self.deep_resolve(elem);
        if new_elem == elem {
            resolved
        } else {
            self.tcx.intern(wrap(new_elem))
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.mut_scopes.push(HashMap::new());
        self.consumed_iterators.push(HashMap::new());
        self.mutable_borrows.push(HashMap::new());
        self.shared_borrows.push(HashMap::new());
        self.reference_origins.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.mut_scopes.pop();
        self.consumed_iterators.pop();
        self.mutable_borrows.pop();
        self.shared_borrows.pop();
        self.reference_origins.pop();
    }

    fn bind_local(&mut self, name: &str, ty: Ty) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(Box::from(name), ty);
        }
    }

    /// Records the declared mutability of a value binding in the current
    /// scope, so an assignment can reject a write to an immutable place.
    fn bind_local_mutability(&mut self, name: &str, mutable: bool) {
        if let Some(scope) = self.mut_scopes.last_mut() {
            scope.insert(Box::from(name), mutable);
        }
    }

    fn lookup_local(&self, name: &str) -> Option<Ty> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(*ty);
            }
        }
        None
    }

    /// Declared mutability of the nearest enclosing binding of `name`.
    /// `None` when the name is not a tracked local (a `const`, `static`,
    /// module item, or unresolved name), where mutability is not checked.
    fn lookup_local_mutability(&self, name: &str) -> Option<bool> {
        for scope in self.mut_scopes.iter().rev() {
            if let Some(mutable) = scope.get(name) {
                return Some(*mutable);
            }
        }
        None
    }

    fn active_mutable_borrower(&self, root: &str) -> Option<&str> {
        self.mutable_borrows
            .iter()
            .rev()
            .find_map(|scope| scope.get(root).map(Box::as_ref))
    }

    fn active_shared_borrower(&self, root: &str) -> Option<&str> {
        self.shared_borrows
            .iter()
            .rev()
            .find_map(|scope| scope.get(root).map(Box::as_ref))
    }

    fn register_named_mutable_borrow(&mut self, pattern: &Pattern, init: &Expr) {
        let PatternKind::Ident { name, .. } = &pattern.kind else {
            return;
        };
        if let ExprKind::Path(path) = &init.kind
            && let [source] = path.segments.as_slice()
            && let Some(source_ty) = self.lookup_local(&source.name.name)
            && matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, source_ty)),
                Some(TyKind::Ref {
                    mutability: Mutbl::Not,
                    ..
                })
            )
            && let Some(origin) = self.reference_origin(&source.name.name).map(str::to_string)
        {
            if let Some(scope) = self.reference_origins.last_mut() {
                scope.insert(
                    name.name.clone().into_boxed_str(),
                    origin.clone().into_boxed_str(),
                );
            }
            if self.active_mutable_borrower(&origin).is_none()
                && let Some(scope) = self.shared_borrows.last_mut()
            {
                scope.insert(origin.into_boxed_str(), name.name.clone().into_boxed_str());
            }
            return;
        }
        let ExprKind::Unary { op, operand } = &init.kind else {
            return;
        };
        if !matches!(op, UnaryOp::RefShared | UnaryOp::RefMut) {
            return;
        }
        let Some(root) = Self::place_root_name(operand) else {
            return;
        };
        let borrower = name.name.clone().into_boxed_str();
        if let Some(scope) = self.reference_origins.last_mut() {
            scope.insert(borrower.clone(), root.clone().into_boxed_str());
        }
        if matches!(op, UnaryOp::RefMut) {
            if self.active_mutable_borrower(&root).is_none()
                && self.active_shared_borrower(&root).is_none()
                && let Some(scope) = self.mutable_borrows.last_mut()
            {
                scope.insert(Box::from(root), borrower);
            }
        } else if self.active_mutable_borrower(&root).is_none()
            && let Some(scope) = self.shared_borrows.last_mut()
        {
            scope.insert(Box::from(root), borrower);
        }
    }

    fn reference_origin(&self, binding: &str) -> Option<&str> {
        self.reference_origins
            .iter()
            .rev()
            .find_map(|scope| scope.get(binding).map(Box::as_ref))
    }

    fn binding_scope(&self, binding: &str) -> Option<usize> {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, scope)| scope.contains_key(binding).then_some(index))
    }

    fn register_pattern_reference_origins(&mut self, pattern: &Pattern, origin: &str) {
        let mut names = Vec::new();
        pattern_binding_names(pattern, &mut names);
        for name in names {
            let Some(ty) = self.lookup_local(&name) else {
                continue;
            };
            let resolved = self.infer.resolve(self.tcx, ty);
            if matches!(self.tcx.kind(resolved), Some(TyKind::Ref { .. }))
                && let Some(scope) = self.reference_origins.last_mut()
            {
                scope.insert(name.into_boxed_str(), Box::from(origin));
            }
        }
    }

    fn register_reference_parameter_origins(&mut self, pattern: &Pattern) {
        let mut names = Vec::new();
        pattern_binding_names(pattern, &mut names);
        for name in names {
            let Some(ty) = self.lookup_local(&name) else {
                continue;
            };
            let resolved = self.infer.resolve(self.tcx, ty);
            if matches!(self.tcx.kind(resolved), Some(TyKind::Ref { .. }))
                && let Some(scope) = self.reference_origins.last_mut()
            {
                scope.insert(name.clone().into_boxed_str(), name.into_boxed_str());
            }
        }
    }

    fn is_stable_shared_reference_alias(&mut self, expr: &Expr) -> bool {
        let ExprKind::Path(path) = &expr.kind else {
            return false;
        };
        let [source] = path.segments.as_slice() else {
            return false;
        };
        let Some(source_ty) = self.lookup_local(&source.name.name) else {
            return false;
        };
        let resolved = self.infer.resolve(self.tcx, source_ty);
        matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Ref {
                mutability: Mutbl::Not,
                ..
            })
        ) && self.reference_origin(&source.name.name).is_some()
    }

    fn rebind_named_borrow(&mut self, place: &Expr, value: &Expr) -> bool {
        let ExprKind::Path(path) = &place.kind else {
            return false;
        };
        let [binding] = path.segments.as_slice() else {
            return false;
        };
        let binding = binding.name.name.as_str();
        let owner_scope = self
            .mutable_borrows
            .iter()
            .position(|scope| scope.values().any(|borrower| borrower.as_ref() == binding))
            .or_else(|| {
                self.shared_borrows
                    .iter()
                    .position(|scope| scope.values().any(|borrower| borrower.as_ref() == binding))
            });
        let Some(owner_scope) = owner_scope else {
            return false;
        };

        if let ExprKind::Path(path) = &value.kind
            && let [source] = path.segments.as_slice()
            && let Some(root) = self.reference_origin(&source.name.name).map(str::to_string)
            && self
                .binding_scope(&root)
                .is_none_or(|scope| scope <= owner_scope)
        {
            if let Some(scope) = self.reference_origins.get_mut(owner_scope) {
                scope.insert(Box::from(binding), root.into_boxed_str());
            }
            return true;
        }

        let ExprKind::Unary { op, operand } = &value.kind else {
            return false;
        };
        if !matches!(op, UnaryOp::RefShared | UnaryOp::RefMut) || !is_stable_borrow_place(operand) {
            return false;
        }
        let Some(root) = Self::place_root_name(operand) else {
            return false;
        };
        if self
            .binding_scope(&root)
            .is_some_and(|scope| scope > owner_scope)
        {
            return false;
        }
        let conflicting = match op {
            UnaryOp::RefMut => self
                .active_mutable_borrower(&root)
                .or_else(|| self.active_shared_borrower(&root)),
            UnaryOp::RefShared => self.active_mutable_borrower(&root),
            _ => None,
        };
        if let Some(borrower) = conflicting
            && borrower != binding
        {
            self.emit(
                TypeError::MutableReferenceConflict {
                    root,
                    borrower: borrower.to_string(),
                },
                operand.span,
            );
            return true;
        }
        self.mutable_borrows[owner_scope].retain(|_, borrower| borrower.as_ref() != binding);
        self.shared_borrows[owner_scope].retain(|_, borrower| borrower.as_ref() != binding);
        let target = if matches!(op, UnaryOp::RefMut) {
            &mut self.mutable_borrows[owner_scope]
        } else {
            &mut self.shared_borrows[owner_scope]
        };
        target.insert(root.clone().into_boxed_str(), Box::from(binding));
        if let Some(scope) = self.reference_origins.get_mut(owner_scope) {
            scope.insert(Box::from(binding), root.into_boxed_str());
        }
        true
    }

    fn unify(&mut self, lhs: Ty, rhs: Ty, span: Span) {
        let lhs_resolved = self.infer.resolve(self.tcx, lhs);
        let rhs_resolved = self.infer.resolve(self.tcx, rhs);
        let lhs_kind = self.tcx.kind(lhs_resolved).cloned();
        let rhs_kind = self.tcx.kind(rhs_resolved).cloned();

        // Directional slice-reference unsizing. Keep the expression's concrete
        // array or Vec type in the type table while accepting it where a slice
        // reference is expected. A mutable reference may also reborrow as a
        // shared slice, but a shared reference cannot become mutable.
        if let (
            Some(TyKind::Ref {
                mutability: expected_mut,
                inner: expected_inner,
            }),
            Some(TyKind::Ref {
                mutability: found_mut,
                inner: found_inner,
            }),
        ) = (&lhs_kind, &rhs_kind)
            && (*expected_mut == Mutbl::Not || *found_mut == Mutbl::Mut)
            && let expected_inner = self.infer.resolve(self.tcx, *expected_inner)
            && let found_inner = self.infer.resolve(self.tcx, *found_inner)
            && let Some(TyKind::Slice(expected_elem)) = self.tcx.kind(expected_inner).cloned()
            && let Some(
                TyKind::Slice(found_elem)
                | TyKind::Vec(found_elem)
                | TyKind::Array {
                    elem: found_elem, ..
                },
            ) = self.tcx.kind(found_inner).cloned()
        {
            let result = self.infer.unify(self.tcx, expected_elem, found_elem);
            if let Err(err) = result {
                self.report_unify(err, expected_elem, found_elem, span);
            }
            return;
        }

        // A sequence reaches a `[T]` view parameter as itself: the view is
        // what the parameter names, and no sigil at the call site says
        // anything the parameter's own type does not.
        if let Some(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: expected_inner,
        }) = &lhs_kind
            && let expected_inner = self.infer.resolve(self.tcx, *expected_inner)
            && let Some(TyKind::Slice(expected_elem)) = self.tcx.kind(expected_inner).cloned()
            && let Some(
                TyKind::Slice(found_elem)
                | TyKind::Vec(found_elem)
                | TyKind::Array {
                    elem: found_elem, ..
                },
            ) = &rhs_kind
        {
            let found_elem = *found_elem;
            let result = self.infer.unify(self.tcx, expected_elem, found_elem);
            if let Err(err) = result {
                self.report_unify(err, expected_elem, found_elem, span);
            }
            return;
        }

        // Function items carry only a DefId in their TyKind; their signature
        // lives in `fn_sigs`. Materialize that signature before unification so
        // a named function can coerce to a compatible `fn`/`Fn` parameter but
        // never to an incompatible one.
        let callable_result = match (&lhs_kind, &rhs_kind) {
            (Some(TyKind::FnPtr(_) | TyKind::FnTrait(_)), Some(TyKind::FnDef { def, substs })) => {
                self.instantiated_fn_item_sig(*def, substs).map(|sig| {
                    let actual = self.tcx.intern(TyKind::FnPtr(sig));
                    self.infer.unify(self.tcx, lhs_resolved, actual)
                })
            }
            (Some(TyKind::FnDef { def, substs }), Some(TyKind::FnPtr(_) | TyKind::FnTrait(_))) => {
                self.instantiated_fn_item_sig(*def, substs).map(|sig| {
                    let actual = self.tcx.intern(TyKind::FnPtr(sig));
                    self.infer.unify(self.tcx, actual, rhs_resolved)
                })
            }
            _ => None,
        };
        let result = callable_result
            .unwrap_or_else(|| self.infer.unify(self.tcx, lhs_resolved, rhs_resolved));
        match result {
            Ok(()) => {}
            Err(err) => self.report_unify(err, lhs, rhs, span),
        }
    }

    fn instantiated_fn_item_sig(
        &mut self,
        def: gossamer_resolve::DefId,
        explicit: &crate::Substs,
    ) -> Option<FnSig> {
        let sig = self.fn_sigs.get(&def)?.clone();
        let n = self.fn_generic_arity.get(&def).copied().unwrap_or(0);
        if n == 0 {
            return Some(sig);
        }
        let const_mask = self
            .fn_generic_const_mask
            .get(&def)
            .cloned()
            .unwrap_or_default();
        let vars: Vec<Ty> = (0..n)
            .map(|i| match explicit.as_slice().get(i) {
                Some(crate::GenericArg::Type(ty))
                    if !const_mask.get(i).copied().unwrap_or(false) =>
                {
                    *ty
                }
                _ => self.fresh(),
            })
            .collect();
        let consts: Vec<Option<i128>> = (0..n)
            .map(|i| match explicit.as_slice().get(i) {
                Some(crate::GenericArg::Const(value)) => Some(*value),
                _ => None,
            })
            .collect();
        Some(FnSig {
            inputs: sig
                .inputs
                .into_iter()
                .map(|ty| self.subst_generics_in_ty(ty, &vars, &consts))
                .collect(),
            output: self.subst_generics_in_ty(sig.output, &vars, &consts),
        })
    }

    fn report_unify(&mut self, err: UnifyError, lhs: Ty, rhs: Ty, span: Span) {
        match err {
            UnifyError::Mismatch => {
                let lhs = self.infer.resolve(self.tcx, lhs);
                let rhs = self.infer.resolve(self.tcx, rhs);
                if !self.is_concrete(lhs) || !self.is_concrete(rhs) {
                    // A structural mismatch cannot become compatible when its
                    // remaining leaf variables resolve. Hold it only so
                    // numeric literals render as i64/f64 instead of `?N`.
                    self.deferred_type_mismatches.push((lhs, rhs, span));
                    return;
                }
                let expected = self.render_public_ty(lhs);
                let found = self.render_public_ty(rhs);
                self.emit(TypeError::TypeMismatch { expected, found }, span);
            }
            UnifyError::IntegerConstraint => {
                let lhs = self.infer.resolve(self.tcx, lhs);
                let rhs = self.infer.resolve(self.tcx, rhs);
                if matches!(self.tcx.kind(lhs), Some(TyKind::Var(_))) {
                    // The expected type was established by an integer
                    // literal, as in `let v = [1]; v.push('a')`.
                    // Preserve lhs/rhs orientation and let defaulting render
                    // the expected literal type as i64.
                    self.deferred_type_mismatches.push((lhs, rhs, span));
                } else {
                    // The supplied expression is the integer literal.
                    self.deferred_literal_type_mismatches
                        .push((lhs, "i64", span));
                }
            }
            UnifyError::FloatConstraint => {
                let lhs = self.infer.resolve(self.tcx, lhs);
                let rhs = self.infer.resolve(self.tcx, rhs);
                if matches!(self.tcx.kind(lhs), Some(TyKind::Var(_))) {
                    self.deferred_type_mismatches.push((lhs, rhs, span));
                } else {
                    self.deferred_literal_type_mismatches
                        .push((lhs, "f64", span));
                }
            }
            UnifyError::Occurs { .. } => {
                // Recursive inference equations such as
                // `HashMap<K, V> = V` are real type mismatches. Deferring lets
                // unresolved key/value literals default before rendering the
                // diagnostic, while still preventing the binding from being
                // silently retyped.
                let lhs = self.infer.resolve(self.tcx, lhs);
                let rhs = self.infer.resolve(self.tcx, rhs);
                self.deferred_type_mismatches.push((lhs, rhs, span));
            }
        }
    }

    fn is_concrete(&self, ty: Ty) -> bool {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(kind) => kind_is_concrete(self, kind),
            None => false,
        }
    }

    fn collect_signatures(&mut self, items: &[Item]) {
        self.collect_signatures_in(items, &mut Vec::new());
    }

    /// Registers every signature in `items`, tracking the module path so a
    /// type's identity is the name it can be reached by rather than the bare
    /// name two modules may share.
    fn collect_signatures_in(&mut self, items: &[Item], module_path: &mut Vec<String>) {
        // First pass: index every trait name + its methods + supertraits,
        // and every user struct / enum name, so subsequent passes can
        // validate `<T: Bound>` bounds, reject name-global method
        // mis-dispatch, and detect supertrait-through-bound calls
        // regardless of declaration order relative to impl blocks.
        self.collect_trait_names_in(items, module_path);
        // Register alias targets before any type lowering so a struct
        // field / let / param naming `X` (where `type X = T`) expands to
        // `T` regardless of declaration order.
        self.collect_type_aliases(items);
        for item in items {
            self.register_must_use(item);
            match &item.kind {
                ItemKind::Fn(decl) => self.register_fn_sig(item.id, decl, item.span),
                ItemKind::Impl(decl) => {
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.collect_impl_signatures(decl, module_path);
                    // A `cmp` the source wrote is the type's order. The
                    // synthesized field-by-field one carries the marker, and
                    // is the order every ordering primitive already reads.
                    if !item.attrs.has_word("gos_synthesized")
                        && decl
                            .items
                            .iter()
                            .any(|it| matches!(it, ImplItem::Fn(f) if f.name.name == "cmp"))
                    {
                        self.user_ordered_types.insert(impl_self_ty_name(decl));
                    }
                }
                ItemKind::Trait(decl) => {
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.collect_trait_signatures(decl);
                }
                ItemKind::Struct(decl) => {
                    self.validate_derives(&item.attrs, item.span);
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.register_struct(item.id, decl, module_path);
                }
                ItemKind::Enum(decl) => {
                    self.validate_derives(&item.attrs, item.span);
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.register_enum(item.id, decl, item.span, module_path);
                }
                ItemKind::Const(decl) => self.register_const(item.id, &decl.ty, &decl.value),
                ItemKind::Static(decl) => {
                    if let Some(def) = self.resolutions.definition_of(item.id) {
                        self.static_mutability.insert(
                            def,
                            matches!(decl.mutability, gossamer_ast::Mutability::Mutable),
                        );
                    }
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        module_path.push(decl.name.name.clone());
                        self.collect_signatures_in(inner, module_path);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
        // Every user type is registered by now, so an `impl` block can be
        // matched to the declaration its self type names regardless of the
        // order the two appear in.
        self.collect_impl_obligations_in(items, module_path);
    }

    /// Indexes one struct / enum under both the identity it is reached by
    /// (`a::Point`) and its declared name. The bare key is first-declaration
    /// wins, so a second module declaring the name never displaces the first -
    /// a reference that means the second one is written or imported through
    /// its module and resolves on the qualified key.
    fn register_adt_name(&mut self, item_id: NodeId, name: &str, module_path: &[String]) {
        self.user_type_decls.insert(name.to_string());
        let identity = qualified_type_name(module_path, name);
        self.user_type_decls.insert(identity.clone());
        if let Some(def) = self.resolutions.definition_of(item_id) {
            self.adt_def_by_name.insert(identity, def);
            self.adt_def_by_name.entry(name.to_string()).or_insert(def);
        }
    }

    /// Attaches each `impl` block's generic bounds to the type it targets
    /// and verifies every trait impl supplies exactly the items its trait
    /// declares. Tracks the module path so two modules each declaring a
    /// `Point` claim distinct `(trait, type)` pairs.
    fn collect_impl_obligations_in(&mut self, items: &[Item], module_path: &mut Vec<String>) {
        for item in items {
            match &item.kind {
                ItemKind::Impl(decl) => {
                    self.record_impl_param_bounds(decl);
                    self.check_trait_impl_completeness(decl, item.span);
                    self.check_trait_impl_membership(decl, item.span);
                    self.check_trait_impl_uniqueness(decl, module_path, item.span);
                    self.check_trait_impl_assoc_items(decl, item.span);
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        module_path.push(decl.name.name.clone());
                        self.collect_impl_obligations_in(inner, module_path);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    /// Walks `items` recursively into inline modules and records, for
    /// later passes: every trait name (for `<T: Bound>` validation),
    /// each trait's own method names and supertrait list (for the
    /// supertrait-through-bound check), and every user struct / enum
    /// name (to tell a real user Adt receiver from a synthesized
    /// sentinel one). Idempotent - re-calling adds to the existing sets.
    /// Tracks the module path so a type registers under the identity it is
    /// reached by.
    fn collect_trait_names_in(&mut self, items: &[Item], module_path: &mut Vec<String>) {
        for item in items {
            match &item.kind {
                ItemKind::Trait(decl) => {
                    self.declared_trait_names.insert(decl.name.name.clone());
                    let methods: std::collections::HashSet<String> = decl
                        .items
                        .iter()
                        .filter_map(|it| match it {
                            TraitItem::Fn(fn_decl) => Some(fn_decl.name.name.clone()),
                            _ => None,
                        })
                        .collect();
                    self.trait_own_methods
                        .entry(decl.name.name.clone())
                        .or_default()
                        .extend(methods);
                    let required = self
                        .trait_required_methods
                        .entry(decl.name.name.clone())
                        .or_default();
                    for it in &decl.items {
                        if let TraitItem::Fn(fn_decl) = it
                            && fn_decl.body.is_none()
                            && !required.contains(&fn_decl.name.name)
                        {
                            required.push(fn_decl.name.name.clone());
                        }
                    }
                    let declared = self
                        .trait_declared_methods
                        .entry(decl.name.name.clone())
                        .or_default();
                    for it in &decl.items {
                        if let TraitItem::Fn(fn_decl) = it
                            && !declared.contains(&fn_decl.name.name)
                        {
                            declared.push(fn_decl.name.name.clone());
                        }
                    }
                    for item in &decl.items {
                        if let TraitItem::Fn(fn_decl) = item {
                            let requires_mut = fn_decl.params.iter().any(|param| {
                                matches!(param, FnParam::Receiver(gossamer_ast::Receiver::RefMut))
                            });
                            self.trait_method_requires_mut.insert(
                                (decl.name.name.clone(), fn_decl.name.name.clone()),
                                requires_mut,
                            );
                        }
                    }
                    let supers: Vec<String> = decl
                        .supertraits
                        .iter()
                        .filter_map(|b| b.path.segments.last().map(|s| s.name.name.clone()))
                        .collect();
                    if !supers.is_empty() {
                        self.trait_supertraits
                            .insert(decl.name.name.clone(), supers);
                    }
                }
                ItemKind::Struct(decl) => {
                    self.register_adt_name(item.id, &decl.name.name, module_path);
                    self.record_derived_traits(
                        &qualified_type_name(module_path, &decl.name.name),
                        &item.attrs,
                    );
                }
                ItemKind::Enum(decl) => {
                    self.register_adt_name(item.id, &decl.name.name, module_path);
                    self.record_derived_traits(
                        &qualified_type_name(module_path, &decl.name.name),
                        &item.attrs,
                    );
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        module_path.push(decl.name.name.clone());
                        self.collect_trait_names_in(inner, module_path);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    /// Records the trait names a declaration's `#[derive(..)]` attributes
    /// supply, keyed by the identity the type is reached by, so a written
    /// `impl` of one of them is reported rather than silently competing with
    /// the synthesized body.
    fn record_derived_traits(&mut self, ty_name: &str, attrs: &gossamer_ast::Attrs) {
        for attr in &attrs.outer {
            if attr.path.segments.len() != 1 || attr.path.segments[0].name.name != "derive" {
                continue;
            }
            let Some(tokens) = &attr.tokens else {
                continue;
            };
            let entry = self.derived_traits.entry(ty_name.to_string()).or_default();
            for name in tokens.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                entry.insert(name.to_string());
            }
        }
    }

    fn register_const(&mut self, item_id: NodeId, ty: &gossamer_ast::Type, value: &Expr) {
        let Some(def) = self.resolutions.definition_of(item_id) else {
            return;
        };
        let resolved = self.type_from_ast(ty);
        self.const_tys.insert(def, resolved);
        if let Some(literal) = evaluate_const_int_from_expr(value) {
            self.const_int_values.insert(def, literal);
        }
    }

    /// The integer a length expression names when it is a path to a `const`
    /// item holding one, or `None` for any other expression.
    fn const_int_of_path(&mut self, expr: &Expr) -> Option<u128> {
        let ExprKind::Path(_) = &expr.kind else {
            return None;
        };
        let Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Const,
        } = self.resolutions.get(expr.id)?
        else {
            return None;
        };
        self.const_int_values.get(&def).copied()
    }

    /// Records each `type X<..> = T` alias's type-parameter names and
    /// right-hand side by `DefId`, recursing into inline modules. A use of
    /// the alias expands to `T` (with the params substituted by the
    /// use-site arguments for a generic alias) during type lowering.
    fn collect_type_aliases(&mut self, items: &[Item]) {
        for item in items {
            match &item.kind {
                ItemKind::TypeAlias(decl) => {
                    if let Some(def) = self.resolutions.definition_of(item.id) {
                        let params: Vec<String> = decl
                            .generics
                            .params
                            .iter()
                            .filter_map(|p| match p {
                                gossamer_ast::GenericParam::Type { name, .. } => {
                                    Some(name.name.clone())
                                }
                                _ => None,
                            })
                            .collect();
                        if decl.nominal {
                            self.nominal_aliases.insert(def);
                            // The type prints under its own name, so a
                            // mismatch against the representation names
                            // both sides distinctly.
                            self.tcx.register_def_name(def, decl.name.name.clone());
                        }
                        self.alias_targets.insert(def, (params, decl.ty.clone()));
                    }
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        self.collect_type_aliases(inner);
                    }
                }
                _ => {}
            }
        }
    }

    /// Registers an enum's `DefId -> name` so `render_ty` / `adt_dispatch_name`
    /// recover "Shape" instead of the "adt#N" placeholder - needed for `==` /
    /// `{:?}` dispatch on enum values whose type resolves to the Adt.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass per fact an enum declaration records"
    )]
    fn register_enum(
        &mut self,
        item_id: NodeId,
        decl: &gossamer_ast::EnumDecl,
        span: Span,
        module_path: &[String],
    ) {
        let identity = qualified_type_name(module_path, &decl.name.name);
        if let Some(def) = self.resolutions.definition_of(item_id) {
            self.tcx.register_def_name(def, identity.as_str());
            self.user_type_defs.insert(identity.clone(), def);
            self.user_type_defs
                .entry(decl.name.name.clone())
                .or_insert(def);
            self.record_adt_param_bounds(def, &decl.generics, &decl.where_clause);
            // A generic enum instantiates its parameters per constructor call
            // and per match arm, exactly as a generic struct does. The arity
            // says how many fresh variables each of those needs.
            let arity = decl
                .generics
                .params
                .iter()
                .filter(|p| matches!(p, gossamer_ast::GenericParam::Type { .. }))
                .count();
            if arity > 0 {
                self.struct_generic_arity.insert(def, arity);
            }
            self.tcx.register_enum_variant_names(
                def,
                decl.variants
                    .iter()
                    .map(|variant| variant.name.name.clone())
                    .collect(),
            );
            // Payload-bearing enums are reference-counted heap
            // values; register the def eagerly so the MIR drop pass
            // sees enum-typed locals as RC-managed in every body,
            // not just bodies lowered after the enum's first
            // constructor. All-unit enums lower as bare `i64`
            // discriminants and are excluded.
            let has_payload = decl.variants.iter().any(|v| match &v.body {
                StructBody::Tuple(fields) => !fields.is_empty(),
                StructBody::Named(fields) => !fields.is_empty(),
                StructBody::Unit => false,
            });
            if has_payload {
                self.tcx.register_rc_managed_enum_def(def.local);
            }
            // Cache the enum's `Adt` type so a tuple-variant constructor call
            // resolves to it. Generic enums need per-call-site substs, so they
            // keep the fresh-variable path.
            if decl.generics.params.is_empty() {
                let adt = self.tcx.intern(TyKind::Adt {
                    def,
                    substs: crate::Substs::from_types(std::iter::empty()),
                });
                // The identity is authoritative; the bare alias is
                // first-declaration wins so a second module declaring the
                // name never displaces the first.
                self.enum_tys.insert(identity.clone(), adt);
                self.enum_tys.entry(decl.name.name.clone()).or_insert(adt);
                self.tcx.register_enum_ty_by_name(identity.as_str(), adt);
            }
        }
        for key in [identity.clone(), decl.name.name.clone()] {
            let variant_names = self.enum_variants.entry(key).or_default();
            for variant in &decl.variants {
                variant_names.insert(variant.name.name.clone());
            }
        }
        // A payload type naming a parameter (`Leaf(T)`) records a rigid
        // `TyKind::Param` slot so each constructor call and each match arm
        // substitutes it independently. Lowered outside the scope it would be
        // one shared inference variable, and the first instantiation to pin
        // it would fix the payload type for every other.
        let payload_scope = self.enter_generic_scope(&decl.generics);
        for variant in &decl.variants {
            match &variant.body {
                StructBody::Tuple(fields) => {
                    let tys: Vec<Ty> = fields.iter().map(|f| self.type_from_ast(&f.ty)).collect();
                    self.enum_variant_payloads
                        .insert((identity.clone(), variant.name.name.clone()), tys.clone());
                    self.enum_variant_payloads
                        .entry((decl.name.name.clone(), variant.name.name.clone()))
                        .or_insert(tys);
                }
                StructBody::Named(fields) => {
                    let tys: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| (f.name.name.clone(), self.type_from_ast(&f.ty)))
                        .collect();
                    self.enum_variant_named_payloads
                        .insert((identity.clone(), variant.name.name.clone()), tys.clone());
                    self.enum_variant_named_payloads
                        .entry((decl.name.name.clone(), variant.name.name.clone()))
                        .or_insert(tys);
                }
                StructBody::Unit => {}
            }
        }
        // Per-variant field types in declaration (discriminant) order, for the
        // MIR structural-equality descriptor of heap enums.
        if let Some(def) = self.resolutions.definition_of(item_id) {
            let mut variant_tys: Vec<Vec<Ty>> = Vec::with_capacity(decl.variants.len());
            for variant in &decl.variants {
                let tys: Vec<Ty> = match &variant.body {
                    StructBody::Tuple(fields) => {
                        fields.iter().map(|f| self.type_from_ast(&f.ty)).collect()
                    }
                    StructBody::Named(fields) => {
                        fields.iter().map(|f| self.type_from_ast(&f.ty)).collect()
                    }
                    StructBody::Unit => Vec::new(),
                };
                variant_tys.push(tys);
            }
            self.tcx.register_enum_variant_tys(def, variant_tys);
        }
        self.leave_generic_scope(payload_scope);
        // The width the declaration settles on, so layout reads one answer
        // rather than re-deriving it per back-end.
        if let Some(def) = self.resolutions.definition_of(item_id) {
            let bits = decl.repr.bits(decl.variants.len());
            if bits != gossamer_ast::EnumRepr::natural_bits(false, decl.variants.len()) {
                self.tcx.register_enum_repr_bits(def, bits);
            }
        }
        // A declared width has to hold every variant, and the natural width
        // caps the rest: the heap representation stores the discriminant in a
        // one-byte header field.
        if let Some(bits) = decl.repr.declared_bits {
            if !gossamer_ast::EnumRepr::fits(bits, decl.variants.len()) {
                self.emit(
                    TypeError::EnumReprTooNarrow {
                        name: decl.name.name.clone(),
                        bits,
                        count: decl.variants.len(),
                    },
                    span,
                );
            }
        } else if decl.variants.len() > 256 {
            self.emit(
                TypeError::TooManyVariants {
                    name: decl.name.name.clone(),
                    count: decl.variants.len(),
                },
                span,
            );
        }
    }

    /// Rejects `#[derive(...)]` names that synthesize nothing. Gossamer's
    /// value-type structs / enums compare, order, hash, and copy by value
    /// automatically, so the meaningful derives are exactly `Debug`, `Default`,
    /// `PartialEq`, `Eq`, `PartialOrd`, and `Ord` - each of which still does
    /// work for a generic type, whose rendering and comparison depend on the
    /// arguments an instantiation supplies. Every other name is either
    /// automatic (`Clone` - `let b = a` copies, `Hash`, `Copy`, `Display`,
    /// serde) or implemented with `impl Trait for T` (`From`, operators).
    /// Records `#[must_use]` on a function, struct, or enum declaration so
    /// a discarded value of it is reported (GT0064).
    fn register_must_use(&mut self, item: &Item) {
        if !item.attrs.has_word("must_use") {
            return;
        }
        let Some(def) = self.resolutions.definition_of(item.id) else {
            return;
        };
        match &item.kind {
            ItemKind::Fn(decl) => {
                self.must_use_fns.insert(def, decl.name.name.clone());
            }
            ItemKind::Struct(decl) => {
                self.must_use_types.insert(def, decl.name.name.clone());
            }
            ItemKind::Enum(decl) => {
                self.must_use_types.insert(def, decl.name.name.clone());
            }
            _ => {}
        }
    }

    fn validate_derives(&mut self, attrs: &gossamer_ast::Attrs, span: Span) {
        for attr in &attrs.outer {
            let is_derive =
                attr.path.segments.len() == 1 && attr.path.segments[0].name.name == "derive";
            if !is_derive {
                continue;
            }
            let Some(tokens) = &attr.tokens else {
                continue;
            };
            for tok in tokens.split(',') {
                let name = tok.trim();
                if name.is_empty()
                    || matches!(
                        name,
                        "Debug" | "Default" | "PartialEq" | "Eq" | "PartialOrd" | "Ord"
                    )
                {
                    continue;
                }
                self.emit(
                    TypeError::UnsupportedDerive {
                        name: name.to_string(),
                        hint: derive_rejection_hint(name),
                    },
                    span,
                );
            }
        }
    }

    fn register_struct(
        &mut self,
        item_id: NodeId,
        decl: &gossamer_ast::StructDecl,
        module_path: &[String],
    ) {
        let Some(def) = self.resolutions.definition_of(item_id) else {
            return;
        };
        let identity = qualified_type_name(module_path, &decl.name.name);
        let name = identity.as_str();
        self.tcx.register_def_name(def, name);
        self.user_type_defs.insert(identity.clone(), def);
        self.user_type_defs
            .entry(decl.name.name.clone())
            .or_insert(def);
        self.record_adt_param_bounds(def, &decl.generics, &decl.where_clause);
        // Build the generic-parameter scope so `Pair<A, B> { fst:
        // A, snd: B }` field-type references resolve to the right
        // `TyKind::Param` indices.
        let prior_scope = self.enter_generic_scope(&decl.generics);
        // Record the struct's generic-parameter arity in source
        // order so struct-literal substitution at use sites knows
        // how many fresh inference variables to allocate.
        let arity = decl
            .generics
            .params
            .iter()
            .filter(|p| matches!(p, gossamer_ast::GenericParam::Type { .. }))
            .count();
        if arity > 0 {
            self.struct_generic_arity.insert(def, arity);
        }
        // Tuple-struct fields are modelled as named fields "0".."N-1", so a
        // `Pt(a, b)` constructor (rewritten to a `Pt { 0: a, 1: b }` literal)
        // and positional access `p.0` reuse the named-field machinery.
        let list: Vec<(String, Ty)> = match &decl.body {
            StructBody::Named(fields) => fields
                .iter()
                .map(|f| (f.name.name.clone(), self.type_from_ast(&f.ty)))
                .collect(),
            StructBody::Tuple(fields) => fields
                .iter()
                .enumerate()
                .map(|(i, f)| (i.to_string(), self.type_from_ast(&f.ty)))
                .collect(),
            StructBody::Unit => Vec::new(),
        };
        // A field's visibility is declared on the field, so a `pub` struct
        // may keep private ones. Record each field's home module alongside
        // it so a reference from outside is checked like a method call.
        let visibilities: Vec<(String, Visibility)> = match &decl.body {
            StructBody::Named(fields) => fields
                .iter()
                .map(|f| (f.name.name.clone(), f.visibility))
                .collect(),
            StructBody::Tuple(fields) => fields
                .iter()
                .enumerate()
                .map(|(i, f)| (i.to_string(), f.visibility))
                .collect(),
            StructBody::Unit => Vec::new(),
        };
        for (name, visibility) in visibilities {
            self.field_homes
                .insert((def, name), (module_path.to_vec(), visibility));
        }
        // A unit struct is the zero-field shape `Unit {}` also spells, so it
        // carries the same registered (empty) layout: the tiers read its
        // fields, its slots, and its key content through one description.
        let tys: Vec<Ty> = list.iter().map(|(_, t)| *t).collect();
        self.tcx.register_struct_fields(def, tys);
        self.struct_fields.insert(def, list);
        if matches!(decl.body, StructBody::Tuple(_)) {
            self.tcx.register_tuple_struct(def.local);
        }
        self.leave_generic_scope(prior_scope);
    }

    /// Resolves `receiver_ty.field_name` to the leaf field type.
    /// Auto-dereferences through `&T`/`&mut T` wrappers. Returns
    /// `None` when the receiver does not name a known struct or the
    /// field is not declared on it.
    /// Resolves field access to a type, distinguishing failure
    /// modes worth surfacing to the user:
    ///
    /// - `Err(UnknownField { opaque: true })` - the receiver is an
    ///   `Adt` whose field map isn't registered (typical of opaque
    ///   stdlib types like `json::Value`).
    /// - `Err(UnknownField { opaque: false })` - the receiver is a
    ///   known struct but the field name doesn't match any of its
    ///   fields.
    ///
    /// Non-Adt receivers (primitives, tuples, unresolved inference
    /// vars, generic params) get `Ok(fresh_var)` so the rest of the
    /// expression keeps type-checking. Catching those would either
    /// fight the trait-method machinery or block legitimate
    /// inference.
    fn lookup_field_ty_diagnosed(
        &mut self,
        receiver_ty: Ty,
        field_name: &str,
    ) -> Result<Ty, TypeError> {
        let resolved = self.infer.resolve(self.tcx, receiver_ty);
        let mut cur = resolved;
        loop {
            match self.tcx.kind_of(cur).clone() {
                TyKind::Ref { inner, .. } => cur = inner,
                TyKind::Adt { def, substs } => {
                    let ty_name = self.render_public_ty(resolved);
                    let Some(fields) = self.struct_fields.get(&def).cloned() else {
                        return Err(TypeError::UnknownField {
                            ty: ty_name,
                            field: field_name.to_string(),
                            opaque: true,
                            declared: Vec::new(),
                            field_span: None,
                            method_of_same_name: false,
                        });
                    };
                    for (name, ty) in &fields {
                        if name == field_name {
                            // Substitute `TyKind::Param { idx }`
                            // slots in the declared field type
                            // with the matching generic argument
                            // from the receiver's `substs`. This
                            // is the dual of the substitution at
                            // struct-literal sites: literals
                            // allocate fresh vars for each
                            // parameter; field reads need to
                            // resolve `Param` back to the
                            // receiver's per-instance argument.
                            let substs_vec = substs.types();
                            return Ok(self.subst_params_in_ty(*ty, &substs_vec));
                        }
                    }
                    return Err(TypeError::UnknownField {
                        ty: ty_name,
                        field: field_name.to_string(),
                        opaque: false,
                        declared: fields.iter().map(|(name, _)| name.clone()).collect(),
                        field_span: None,
                        method_of_same_name: self.has_method_named(cur, field_name),
                    });
                }
                // A scalar, a text value, and a sequence carry no named
                // fields at all, so the read has no type to answer with
                // and every tier faults on it at run time.
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Array { .. } => {
                    return Err(TypeError::UnknownField {
                        ty: self.render_public_ty(cur),
                        field: field_name.to_string(),
                        opaque: false,
                        declared: Vec::new(),
                        field_span: None,
                        method_of_same_name: self.has_method_named(cur, field_name),
                    });
                }
                // A variable constrained to a numeric family can only
                // ever be a scalar, whatever width it settles on.
                TyKind::Var(_)
                    if self.infer.is_float_literal_var(self.tcx, cur)
                        || self.infer.is_integer_constrained_var(self.tcx, cur) =>
                {
                    return Err(TypeError::UnknownField {
                        ty: self.render_public_ty(cur),
                        field: field_name.to_string(),
                        opaque: false,
                        declared: Vec::new(),
                        field_span: None,
                        method_of_same_name: self.has_method_named(cur, field_name),
                    });
                }
                _ => return Ok(self.fresh()),
            }
        }
    }

    /// Whether `resolved` answers a method spelled `name` - the
    /// difference between a misspelled field and a call missing its
    /// parentheses.
    fn has_method_named(&mut self, resolved: Ty, name: &str) -> bool {
        if self.known_method_names(resolved).iter().any(|m| m == name) {
            return true;
        }
        let numeric = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Int(_) | TyKind::Float(_))
        ) || self.infer.is_float_literal_var(self.tcx, resolved)
            || self.infer.is_integer_constrained_var(self.tcx, resolved);
        numeric && crate::stdlib_signatures::function_shape_for_path(&["math"], name).is_some()
    }

    /// The names an `impl` block's methods register under: the identity
    /// its self type is reached by (`lib::Point`) first, then the bare
    /// spelling. A resolved receiver carries the identity
    /// [`Self::register_struct`] recorded, so an impl inside `mod lib`
    /// that keyed only the bare name would be invisible to every
    /// receiver-typed lookup; the bare key stays for the sites that key
    /// on a written `Type::method` path instead.
    fn impl_owner_keys(
        &mut self,
        self_ty: &gossamer_ast::Type,
        module_path: &[String],
    ) -> Option<Vec<String>> {
        let gossamer_ast::ty::TypeKind::Path(tp) = &self_ty.kind else {
            // A structural type has no path to name it by; it keys on the
            // spelling every layer shares for one.
            let lowered = self.type_from_ast(self_ty);
            let settled = self.deep_resolve(lowered);
            return crate::printer::structural_impl_owner(self.tcx, settled).map(|name| vec![name]);
        };
        let segments: Vec<&str> = tp.segments.iter().map(|s| s.name.name.as_str()).collect();
        let bare = (*segments.last()?).to_string();
        // A path written with its module (`impl lib::Point`) already
        // spells the identity; a bare one names a type the enclosing
        // module declares, or an imported one that keeps its own name.
        let mut candidates: Vec<String> = Vec::new();
        if segments.len() > 1 {
            candidates.push(segments.join("::"));
        }
        // The type may be declared in the module this `impl` sits in, or in
        // any module enclosing it: a package splits one type's methods across
        // sibling files, and consumed as a dependency every one of those sits
        // one module deeper than the type they implement.
        for level in (0..=module_path.len()).rev() {
            candidates.push(qualified_type_name(&module_path[..level], &bare));
        }
        let identity = candidates
            .into_iter()
            .find(|candidate| self.user_type_decls.contains(candidate))
            .unwrap_or_else(|| bare.clone());
        if identity == bare {
            return Some(vec![bare]);
        }
        Some(vec![identity, bare])
    }

    /// Identities the owner path of an associated call could name, most
    /// specific first: the path anchored under each enclosing module, then
    /// the path as written, then its bare tail.
    ///
    /// A path is written relative to the module it appears in, while a type
    /// registers under its full module-qualified identity, so `model::Point`
    /// written inside `engine` names `pkg::model::Point` when both sit under
    /// `pkg` - which is what a package consumed as a dependency looks like.
    fn owner_identity_candidates(&self, owner: &[&str]) -> Vec<String> {
        let written = owner.join("::");
        let mut out = Vec::new();
        for level in (1..=self.current_module.len()).rev() {
            out.push(format!(
                "{}::{written}",
                self.current_module[..level].join("::")
            ));
        }
        out.push(written);
        if let Some(bare) = owner.last() {
            out.push((*bare).to_string());
        }
        out
    }

    /// Records which module each of an impl's methods is declared in, and
    /// at what visibility, so a call from elsewhere is checked against the
    /// declaration rather than against the call site's module.
    fn record_impl_method_homes(
        &mut self,
        decl: &ImplDecl,
        owners: &[String],
        module_path: &[String],
    ) {
        for owner in owners {
            self.collect_impl_method_owners_and_mutability(decl, owner);
        }
        // A trait impl's methods are reachable wherever the trait is: the
        // trait declares the surface, the impl only supplies it.
        let via_trait = decl.trait_ref.is_some();
        for item in &decl.items {
            if let ImplItem::Fn(fn_decl) = item {
                let visibility = if via_trait {
                    Visibility::Public
                } else {
                    fn_decl.visibility
                };
                for owner in owners {
                    self.method_homes.insert(
                        (owner.clone(), fn_decl.name.name.clone()),
                        (module_path.to_vec(), visibility),
                    );
                }
            }
        }
    }

    fn collect_impl_signatures(&mut self, decl: &ImplDecl, module_path: &[String]) {
        // Self-type names for receiver-keyed method return types.
        // Generic impls are skipped: their returns may mention
        // `Param` slots that a bare lookup cannot substitute.
        let owner_keys = self.impl_owner_keys(&decl.self_ty, module_path);
        let self_names = if decl.generics.params.is_empty() {
            owner_keys.clone()
        } else {
            None
        };
        // The owner type names for method-ownership tracking are recorded
        // even for generic impls (`impl<T> Stack<T>`), so a method call
        // on a generic user type is not falsely flagged as belonging to
        // a different type.
        let owner_names = owner_keys;
        if let Some(owners) = &owner_names {
            self.record_impl_method_homes(decl, owners, module_path);
        }
        // Record `impl Trait for Type` so a `T: Trait` bound can be verified
        // against the concrete argument type at a generic call site.
        if let Some(trait_ref) = &decl.trait_ref
            && let Some(trait_seg) = trait_ref.path.segments.last()
            && let Some(owners) = &owner_names
        {
            self.trait_impl_types
                .entry(trait_seg.name.name.clone())
                .or_default()
                .extend(owners.iter().cloned());
        }
        // `Self` in a signature names the type being implemented, and
        // signatures are collected before any impl body is checked, so the
        // binding has to be in place here too - otherwise a `-> Self`
        // constructor records an unconstrained return type and every call
        // on its result goes unchecked.
        let self_scope = self.enter_generic_scope(&decl.generics);
        let impl_self_ty = self.type_from_ast(&decl.self_ty);
        self.leave_generic_scope(self_scope);
        let prev_self_ty = self.current_self_ty.replace(impl_self_ty);
        let prev_self_name = std::mem::replace(
            &mut self.current_self_ty_name,
            gossamer_ast::assoc::type_head_name(&decl.self_ty).map(ToString::to_string),
        );
        for item in &decl.items {
            if let ImplItem::Fn(fn_decl) = item {
                let id = NodeId::DUMMY;
                let _ = id;
                self.register_fn_sig_anonymous(fn_decl);
                self.register_method_arg_sig(fn_decl);
                // A method with its own type parameters is registered too,
                // as long as its RETURN names none of them: `fn arg<T:
                // Arg>(self, v: T) -> Cmd` answers a `Cmd` at every call
                // site, so recording it is what keeps a field read through
                // the result checked. Without this the call typed as a fresh
                // variable and `c.no_such_field` passed `gos check`.
                let method_ret_is_concrete = fn_decl.generics.params.is_empty()
                    || fn_decl.ret.as_ref().is_some_and(|ty| {
                        let scope = self.enter_generic_scope(&fn_decl.generics);
                        let resolved = self.type_from_ast(ty);
                        self.leave_generic_scope(scope);
                        !self.ty_mentions_generic_param(resolved)
                    });
                if let Some(names) = &self_names
                    && method_ret_is_concrete
                {
                    // A method with its own type parameters contributes its
                    // RETURN only: its parameter types carry rigid `Param`
                    // slots that each call site instantiates for itself, so
                    // recording them would check the second `arg("two")`
                    // against the first `arg(1)`'s instantiation.
                    let own_generics = !fn_decl.generics.params.is_empty();
                    let scope = self.enter_generic_scope(&fn_decl.generics);
                    let params: Vec<Ty> = fn_decl
                        .params
                        .iter()
                        .filter(|p| matches!(p, FnParam::Typed { .. }))
                        .map(|p| self.param_ty(p))
                        .collect();
                    let ret = match fn_decl.ret.as_ref() {
                        Some(ty) => self.type_from_ast(ty),
                        None => self.tcx.unit(),
                    };
                    self.leave_generic_scope(scope);
                    let arity = params.len();
                    for name in names {
                        if !own_generics {
                            self.method_param_types
                                .insert((name.clone(), fn_decl.name.name.clone()), params.clone());
                        }
                        self.method_ret_types
                            .insert((name.clone(), fn_decl.name.name.clone(), arity), ret);
                        self.method_arities
                            .insert((name.clone(), fn_decl.name.name.clone()), arity);
                    }
                } else if !decl.generics.params.is_empty()
                    && fn_decl.generics.params.is_empty()
                    && let Some(names) = &owner_names
                {
                    // Generic-impl methods (`impl<T> Add for Wrap<T>`):
                    // record the return with rigid `Param` slots, resolved
                    // inside the impl's generic scope. A receiver-typed use
                    // site substitutes its instantiation's `substs`.
                    let scope = self.enter_generic_scope(&decl.generics);
                    let params: Vec<Ty> = fn_decl
                        .params
                        .iter()
                        .filter(|p| matches!(p, FnParam::Typed { .. }))
                        .map(|p| self.param_ty(p))
                        .collect();
                    let arity = params.len();
                    let ret = match fn_decl.ret.as_ref() {
                        Some(ty) => self.type_from_ast(ty),
                        None => self.tcx.unit(),
                    };
                    self.leave_generic_scope(scope);
                    for name in names {
                        self.generic_method_param_types
                            .insert((name.clone(), fn_decl.name.name.clone()), params.clone());
                        self.generic_method_ret_types
                            .insert((name.clone(), fn_decl.name.name.clone(), arity), ret);
                        self.method_arities
                            .insert((name.clone(), fn_decl.name.name.clone()), arity);
                    }
                }
            }
        }
        self.current_self_ty_name = prev_self_name;
        self.current_self_ty = prev_self_ty;
    }

    fn collect_impl_method_owners_and_mutability(&mut self, decl: &ImplDecl, owner: &str) {
        let is_trait_impl = decl.trait_ref.is_some();
        for item in &decl.items {
            if let ImplItem::Fn(fn_decl) = item {
                self.user_method_owners
                    .entry(fn_decl.name.name.clone())
                    .or_default()
                    .insert(owner.to_string());
                let requires_mut = fn_decl.params.iter().any(|param| {
                    matches!(param, FnParam::Receiver(gossamer_ast::Receiver::RefMut))
                });
                let receiver_map = if is_trait_impl {
                    &mut self.trait_impl_method_requires_mut
                } else {
                    &mut self.inherent_method_requires_mut
                };
                receiver_map
                    .entry((owner.to_string(), fn_decl.name.name.clone()))
                    .and_modify(|current| *current |= requires_mut)
                    .or_insert(requires_mut);
            }
        }
        // Trait defaults are callable even when the impl does not restate
        // them, so propagate their ownership and receiver capabilities.
        let Some(trait_name) = decl
            .trait_ref
            .as_ref()
            .and_then(|trait_ref| trait_ref.path.segments.last())
            .map(|segment| segment.name.name.as_str())
        else {
            return;
        };
        if let Some(methods) = self.trait_own_methods.get(trait_name).cloned() {
            for method in methods {
                self.user_method_owners
                    .entry(method)
                    .or_default()
                    .insert(owner.to_string());
            }
        }
        for ((declaring_trait, method), requires_mut) in &self.trait_method_requires_mut {
            if declaring_trait == trait_name {
                self.trait_impl_method_requires_mut
                    .entry((owner.to_string(), method.clone()))
                    .and_modify(|current| *current |= *requires_mut)
                    .or_insert(*requires_mut);
            }
        }
    }

    fn collect_trait_signatures(&mut self, decl: &gossamer_ast::TraitDecl) {
        let trait_name = decl.name.name.clone();
        // `Self::Item` in a trait method signature resolves through the
        // declaring trait, since no concrete self type is known yet.
        let prev_trait = self.current_trait_name.replace(trait_name.clone());
        for item in &decl.items {
            if let TraitItem::Fn(fn_decl) = item {
                self.register_fn_sig_anonymous(fn_decl);
                self.register_method_arg_sig(fn_decl);
                let ret = match fn_decl.ret.as_ref() {
                    Some(ty) => self.type_from_ast(ty),
                    None => self.tcx.unit(),
                };
                let params = fn_decl
                    .params
                    .iter()
                    .filter(|p| matches!(p, FnParam::Typed { .. }))
                    .map(|p| self.param_ty(p))
                    .collect();
                self.trait_method_params
                    .insert((trait_name.clone(), fn_decl.name.name.clone()), params);
                self.trait_method_ret
                    .insert((trait_name.clone(), fn_decl.name.name.clone()), ret);
                if let Some(assoc) = fn_decl.ret.as_ref().and_then(self_assoc_projection) {
                    self.trait_method_ret_assoc
                        .insert((trait_name.clone(), fn_decl.name.name.clone()), assoc);
                }
                let requires_mut = fn_decl.params.iter().any(|param| {
                    matches!(param, FnParam::Receiver(gossamer_ast::Receiver::RefMut))
                });
                self.trait_method_requires_mut.insert(
                    (trait_name.clone(), fn_decl.name.name.clone()),
                    requires_mut,
                );
            }
        }
        self.current_trait_name = prev_trait;
    }

    /// Records a method's non-receiver parameter types under its bare
    /// name + arity so [`Self::check_method_call`] can re-type
    /// literal arguments. Every structurally distinct signature for
    /// a key is recorded; coercion later applies only where the
    /// candidates agree (or exactly one is container-shaped), so a
    /// literal is never shaped by the wrong same-named method.
    fn register_method_arg_sig(&mut self, decl: &FnDecl) {
        let inputs: Vec<Ty> = decl
            .params
            .iter()
            .filter(|p| matches!(p, FnParam::Typed { .. }))
            .map(|p| self.param_ty(p))
            .collect();
        let key = (decl.name.name.clone(), inputs.len());
        let entry = self.method_arg_sigs.entry(key).or_default();
        let duplicate = entry.iter().any(|existing| {
            existing.len() == inputs.len()
                && existing
                    .iter()
                    .zip(&inputs)
                    .all(|(x, y)| render_ty(self.tcx, *x) == render_ty(self.tcx, *y))
        });
        if !duplicate {
            entry.push(inputs);
        }
    }

    fn register_fn_sig(&mut self, node: NodeId, decl: &FnDecl, span: Span) {
        self.user_fn_names.insert(decl.name.name.clone());
        let sig = self.fn_sig_of(decl);
        if let Some(def) = self.resolutions.definition_of(node) {
            self.fn_sigs.insert(def, sig);
            // Record the generic arity and per-parameter bounds so each
            // call site can instantiate the parameters independently and
            // verify the argument types satisfy the declared bounds. The
            // arity counts every parameter position (so a const or type
            // parameter's `ParamIdx` indexes the substitution vector
            // directly); the const mask records which positions take a
            // `GenericArg::Const`.
            let has_type_or_const = decl.generics.params.iter().any(|p| {
                matches!(
                    p,
                    gossamer_ast::GenericParam::Type { .. }
                        | gossamer_ast::GenericParam::Const { .. }
                )
            });
            if has_type_or_const {
                self.fn_generic_arity
                    .insert(def, decl.generics.params.len());
                self.fn_param_bounds.insert(
                    def,
                    Self::declared_param_bounds(&decl.generics, &decl.where_clause),
                );
                self.fn_generic_const_mask
                    .insert(def, Self::const_param_mask(&decl.generics));
            }
        }
        self.validate_declared_bounds(&decl.generics, &decl.where_clause, span);
    }

    /// Validates that every trait bound written on a declaration's generic
    /// parameters - in the angle brackets or in its `where` clause - names a
    /// trait this unit (or a recognised built-in) declares. Catches typos
    /// (`Hashabel` for `Hashable`) at the declaration site rather than as a
    /// "no method" error at the use site.
    fn validate_declared_bounds(
        &mut self,
        generics: &gossamer_ast::Generics,
        where_clause: &gossamer_ast::WhereClause,
        span: Span,
    ) {
        let mut declared: Vec<(String, Vec<String>)> = generics
            .params
            .iter()
            .filter_map(|param| match param {
                gossamer_ast::GenericParam::Type { name, bounds, .. } => {
                    Some((name.name.clone(), bound_names(bounds)))
                }
                _ => None,
            })
            .collect();
        for predicate in &where_clause.predicates {
            let Some(name) = bare_path_type_name(&predicate.bounded) else {
                continue;
            };
            declared.push((name.to_string(), bound_names(&predicate.bounds)));
        }
        for (param, bounds) in declared {
            for bound in bounds {
                if bound.is_empty()
                    || self.declared_trait_names.contains(&bound)
                    || known_builtin_trait(&bound)
                {
                    continue;
                }
                self.emit(
                    TypeError::UnknownTraitBound {
                        param: param.clone(),
                        name: bound,
                    },
                    span,
                );
            }
        }
    }

    fn register_fn_sig_anonymous(&mut self, decl: &FnDecl) {
        self.fn_sig_of(decl);
    }

    fn fn_sig_of(&mut self, decl: &FnDecl) -> FnSig {
        // Enter the function's generic scope so a parameter / return type
        // that names a type parameter (`&T`) records a rigid `TyKind::Param`
        // slot rather than a fresh inference variable. The `Param` slots are
        // what per-call-site instantiation substitutes with fresh variables.
        let prior = self.enter_generic_scope(&decl.generics);
        // A `where` predicate constrains the same parameters the angle
        // brackets introduce, so an associated-type projection written in
        // the signature resolves through either spelling.
        self.current_param_bounds = Self::declared_param_bounds(&decl.generics, &decl.where_clause);
        Self::assoc_bindings_of(
            &decl.generics,
            &decl.where_clause,
            &mut self.current_assoc_bindings,
        );
        let inputs: Vec<Ty> = decl
            .params
            .iter()
            .map(|param| self.param_ty(param))
            .collect();
        let output = match decl.ret.as_ref() {
            Some(ty) => self.type_from_ast(ty),
            None => self.tcx.unit(),
        };
        self.leave_generic_scope(prior);
        FnSig { inputs, output }
    }

    fn param_ty(&mut self, param: &FnParam) -> Ty {
        match param {
            FnParam::Typed { ty, .. } => self.type_from_ast(ty),
            FnParam::Receiver(_) => self.fresh(),
        }
    }

    /// Checks every item of an `impl` block inside a scope where `Self`
    /// names the type being implemented, so a `-> Self` return and a
    /// `Self::Item` projection both land on it.
    fn check_impl(&mut self, decl: &ImplDecl) {
        // The self type is lowered inside the impl's own generic scope, so
        // `impl<T: Shape> Wrapper<T>` records `Wrapper` at `Param(0)`. Field
        // reads off `self` then carry that rigid parameter, which is what
        // bound-method resolution keys on.
        let self_scope = self.enter_generic_scope(&decl.generics);
        let self_ty = self.type_from_ast(&decl.self_ty);
        self.leave_generic_scope(self_scope);
        let prev_self = self.current_self_ty.replace(self_ty);
        let prev_self_name = std::mem::replace(
            &mut self.current_self_ty_name,
            gossamer_ast::assoc::type_head_name(&decl.self_ty).map(ToString::to_string),
        );
        let prev_impl_generics = self.current_impl_generics.replace(decl.generics.clone());
        let prev_impl_where =
            std::mem::replace(&mut self.current_impl_where, decl.where_clause.clone());
        for impl_item in &decl.items {
            match impl_item {
                ImplItem::Fn(fn_decl) => self.check_fn(fn_decl),
                ImplItem::Const { ty, value, .. } => {
                    let annotated = self.type_from_ast(ty);
                    let init = self.check_expr_expecting(value, Expectation::HasType(annotated));
                    self.unify(annotated, init, value.span);
                }
                ImplItem::Type { ty, .. } => {
                    self.type_from_ast(ty);
                }
            }
        }
        self.current_impl_generics = prev_impl_generics;
        self.current_impl_where = prev_impl_where;
        self.current_self_ty_name = prev_self_name;
        self.current_self_ty = prev_self;
    }

    /// Checks a trait's default bodies and the types of its associated
    /// declarations. `Self` stands for every implementor here, so a
    /// projection resolves through the trait rather than a concrete type.
    fn check_trait(&mut self, decl: &gossamer_ast::TraitDecl) {
        let prev_trait = self.current_trait_name.replace(decl.name.name.clone());
        for trait_item in &decl.items {
            match trait_item {
                TraitItem::Fn(fn_decl) => self.check_fn(fn_decl),
                TraitItem::Const { ty, default, .. } => {
                    let annotated = self.type_from_ast(ty);
                    if let Some(value) = default {
                        let init =
                            self.check_expr_expecting(value, Expectation::HasType(annotated));
                        self.unify(annotated, init, value.span);
                    }
                }
                TraitItem::Type { default, .. } => {
                    if let Some(ty) = default {
                        self.type_from_ast(ty);
                    }
                }
            }
        }
        self.current_trait_name = prev_trait;
    }

    fn check_item(&mut self, item: &Item) {
        // SPEC §9: `#[allow(unused_result)]` on an item covers its body.
        let prior_allowed = self.unused_result_allowed;
        self.unused_result_allowed |= item.attrs.allows("unused_result");
        // An autoderive-spliced body belongs to the type it completes, and
        // reads every field of it regardless of where the splice landed.
        // The serde and reflection helpers carry the same meaning in their
        // `__`-prefixed names.
        let synthesized = item.attrs.has_word("gos_synthesized")
            || match &item.kind {
                ItemKind::Fn(decl) => is_compiler_generated(&decl.name.name),
                ItemKind::Struct(decl) => is_compiler_generated(&decl.name.name),
                ItemKind::Enum(decl) => is_compiler_generated(&decl.name.name),
                _ => false,
            };
        if synthesized {
            self.synthesized_depth += 1;
        }
        self.check_item_inner(item);
        if synthesized {
            self.synthesized_depth -= 1;
        }
        self.unused_result_allowed = prior_allowed;
    }

    fn check_item_inner(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => self.check_fn(decl),
            ItemKind::Impl(decl) => self.check_impl(decl),
            ItemKind::Trait(decl) => self.check_trait(decl),
            ItemKind::Const(decl) => {
                let annotated = self.type_from_ast(&decl.ty);
                let static_string_reference = matches!(
                    self.tcx.kind_of(annotated),
                    TyKind::Ref {
                        inner,
                        mutability: Mutbl::Not,
                    } if matches!(self.tcx.kind_of(*inner), TyKind::String)
                ) && expr_is_static_string_value(&decl.value);
                if !static_string_reference {
                    self.reject_stored_reference_type(
                        annotated,
                        decl.ty.span,
                        "be stored in a constant",
                    );
                }
                let init = self.check_expr_expecting(&decl.value, Expectation::HasType(annotated));
                self.unify(annotated, init, decl.value.span);
            }
            ItemKind::Static(decl) => {
                let annotated = self.type_from_ast(&decl.ty);
                let static_string_reference = matches!(
                    self.tcx.kind_of(annotated),
                    TyKind::Ref {
                        inner,
                        mutability: Mutbl::Not,
                    } if matches!(self.tcx.kind_of(*inner), TyKind::String)
                ) && expr_is_static_string_value(&decl.value);
                if !static_string_reference {
                    self.reject_stored_reference_type(
                        annotated,
                        decl.ty.span,
                        "be stored in a static",
                    );
                }
                let init = self.check_expr_expecting(&decl.value, Expectation::HasType(annotated));
                self.unify(annotated, init, decl.value.span);
            }
            ItemKind::Struct(decl) => self.check_struct_body(&decl.body),
            ItemKind::Enum(decl) => {
                for variant in &decl.variants {
                    self.check_struct_body(&variant.body);
                }
            }
            ItemKind::TypeAlias(decl) => {
                let _ = self.type_from_ast(&decl.ty);
            }
            ItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    self.current_module.push(decl.name.name.clone());
                    for nested in inner {
                        self.check_item(nested);
                    }
                    self.current_module.pop();
                }
            }
            ItemKind::AttrItem(_) => {}
        }
    }

    fn check_struct_body(&mut self, body: &StructBody) {
        match body {
            StructBody::Named(fields) => {
                for field in fields {
                    let ty = self.type_from_ast(&field.ty);
                    self.reject_stored_reference_type(
                        ty,
                        field.ty.span,
                        "be stored in a struct field",
                    );
                }
            }
            StructBody::Tuple(fields) => {
                for field in fields {
                    let ty = self.type_from_ast(&field.ty);
                    self.reject_stored_reference_type(
                        ty,
                        field.ty.span,
                        "be stored in a tuple-struct field",
                    );
                }
            }
            StructBody::Unit => {}
        }
    }

    fn check_fn(&mut self, decl: &FnDecl) {
        // Enter the function's generic scope so a parameter / return / body
        // type that names a type parameter (`&T`) records a rigid
        // `TyKind::Param`. Monomorphisation substitutes those `Param` slots,
        // and trait-method dispatch on a `T` receiver keys off them.
        // The bound table is built from the same parameter sequence as the
        // scope, so index `i` of one names index `i` of the other.
        let mut assoc_bindings = HashMap::new();
        let (prior_scope, bounds) = match self.current_impl_generics.clone() {
            Some(impl_g) if !impl_g.params.is_empty() => {
                let impl_where = self.current_impl_where.clone();
                let bounds = Self::combined_param_bounds(
                    &impl_g,
                    &impl_where,
                    &decl.generics,
                    &decl.where_clause,
                );
                Self::assoc_bindings_of(&impl_g, &impl_where, &mut assoc_bindings);
                (
                    self.enter_generic_scope_combined(&impl_g, &decl.generics),
                    bounds,
                )
            }
            _ => (
                self.enter_generic_scope(&decl.generics),
                Self::declared_param_bounds(&decl.generics, &decl.where_clause),
            ),
        };
        Self::assoc_bindings_of(&decl.generics, &decl.where_clause, &mut assoc_bindings);
        let prior_assoc_bindings =
            std::mem::replace(&mut self.current_assoc_bindings, assoc_bindings);
        let prior_bounds = std::mem::replace(&mut self.current_param_bounds, bounds);
        self.push_scope();
        for param in &decl.params {
            self.bind_fn_param(param);
            match param {
                FnParam::Typed { pattern, .. } => {
                    self.register_reference_parameter_origins(pattern);
                }
                FnParam::Receiver(
                    gossamer_ast::Receiver::RefShared | gossamer_ast::Receiver::RefMut,
                ) => {
                    if let Some(scope) = self.reference_origins.last_mut() {
                        scope.insert(Box::from("self"), Box::from("self"));
                    }
                }
                FnParam::Receiver(gossamer_ast::Receiver::Owned) => {}
            }
            if let FnParam::Typed { ty, .. } = param {
                let param_ty = self.type_from_ast(ty);
                if !matches!(
                    self.tcx.kind_of(param_ty),
                    TyKind::Ref { .. } | TyKind::FnPtr(_) | TyKind::FnTrait(_)
                ) {
                    self.reject_stored_reference_type(
                        param_ty,
                        ty.span,
                        "be nested inside an owned function parameter",
                    );
                }
            }
        }
        let declared_ret = decl.ret.as_ref().map(|ty| self.type_from_ast(ty));
        if let Some(ret) = declared_ret {
            let static_string_reference = matches!(
                self.tcx.kind_of(ret),
                TyKind::Ref {
                    inner,
                    mutability: Mutbl::Not,
                } if matches!(self.tcx.kind_of(*inner), TyKind::String)
            ) && decl
                .body
                .as_ref()
                .is_some_and(|body| expr_is_static_string_value(body));
            if !static_string_reference {
                self.reject_stored_reference_type(
                    ret,
                    decl.ret.as_ref().expect("declared return").span,
                    "escape through a function return",
                );
            }
        }
        if let Some(body) = &decl.body {
            self.check_fn_body(decl, body, declared_ret);
        }
        self.pop_scope();
        self.current_param_bounds = prior_bounds;
        self.current_assoc_bindings = prior_assoc_bindings;
        self.leave_generic_scope(prior_scope);
    }

    /// Checks one function body against the return type its signature
    /// declares, or against the unit a missing one answers.
    fn check_fn_body(&mut self, decl: &FnDecl, body: &Expr, declared_ret: Option<Ty>) {
        let ret = declared_ret.unwrap_or_else(|| self.tcx.unit());
        self.reject_unscoped_spawns(decl, body);
        self.collect_write_arg_bindings(body);
        let prev_ret = self.current_fn_ret.replace(ret);
        // The declared return type flows into the body as its expectation
        // to constrain literals and conversions. Container identity does
        // not change: an array literal remains `[T; N]` even when `Vec<T>`
        // is expected, and unification reports the mismatch.
        let body_ty = if let Some(ret) = declared_ret {
            self.check_expr_expecting(body, Expectation::HasType(ret))
        } else {
            let body_ty = self.check_expr(body);
            if !self.unused_result_allowed && self.is_result_ty(body_ty) {
                self.emit(TypeError::DiscardedResult, body_value_span(body));
            } else {
                self.report_discarded_result(body, None);
            }
            self.check_undeclared_return(decl, body, body_ty);
            body_ty
        };
        self.current_fn_ret = prev_ret;
        // A declared `-> ()` says the discard is deliberate, so a body whose
        // tail computes a value is accepted and the value dropped - the same
        // shape a signature with no return type has, written out. Every
        // other declared return unifies with the body.
        let discards_tail = declared_ret.is_some_and(|declared| {
            matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, declared)),
                Some(TyKind::Unit)
            )
        });
        if declared_ret.is_some() && !discards_tail {
            self.unify(ret, body_ty, body.span);
        }
    }

    /// Reports a `spawn` that no `cohort { }` in the same body encloses.
    ///
    /// `spawn` attaches its child to the cohort the goroutine is inside at
    /// that moment, and a function is not a cohort. A function that spawns
    /// without opening one hands its children to whatever cohort its caller
    /// happens to be in - and, when the program has none, to the root cohort,
    /// whose extent is the process. Neither side of the call says so: the
    /// callee's signature does not mention spawning and the caller cannot see
    /// what it took on. Requiring the block lexically is what makes the block
    /// that joins a child the block a reader can point at.
    ///
    /// `main` is the exemption, and the only one. The root cohort's extent IS
    /// main's extent, so a child started there does not outlive the scope that
    /// owns it - the one place ambient attachment tells the truth. An entry
    /// file's top-level statements are a synthesized `fn main` and ride the
    /// same exemption; a method named `main` in an `impl` block does not.
    ///
    /// Containment is the whole test, so a closure does not break it: a
    /// closure written inside a cohort block runs where it is written.
    fn reject_unscoped_spawns(&mut self, decl: &FnDecl, body: &Expr) {
        let name = decl.name.name.as_str();
        if (name == "main" && self.current_self_ty_name.is_none()) || is_compiler_generated(name) {
            return;
        }
        let mut scan = SpawnScopeScan::default();
        gossamer_ast::visitor::Visitor::visit_expr(&mut scan, body);
        if scan.spawns.is_empty() {
            return;
        }
        for spawn in scan.spawns {
            let scoped = scan.cohorts.iter().any(|cohort| {
                cohort.file == spawn.file && cohort.start <= spawn.start && spawn.end <= cohort.end
            });
            if !scoped {
                self.emit(
                    TypeError::SpawnOutsideCohort {
                        function: name.to_string(),
                    },
                    spawn,
                );
            }
        }
    }

    /// Reports a body whose tail answers a value through a signature that
    /// declares no return type. A missing return type is a unit, so the
    /// value the tail computed is discarded; the report says how to return
    /// it and how to mark the discard deliberate.
    fn check_undeclared_return(&mut self, decl: &FnDecl, body: &Expr, body_ty: Ty) {
        // A wrapper the front end synthesized around an expression - the
        // REPL's per-input entry point, the binding-type probe - answers that
        // expression by construction, and its caller reads the value back
        // rather than the signature.
        if is_compiler_generated(&decl.name.name) {
            return;
        }
        // A body with no tail expression answers a unit whatever its
        // statements compute, so only a tail can hand a value back.
        if !matches!(&body.kind, ExprKind::Block(block) if block.tail.is_some()) {
            return;
        }
        let resolved = self.infer.resolve(self.tcx, body_ty);
        if !self.ty_is_returnable_value(resolved) {
            return;
        }
        let found = self.render_public_ty(resolved);
        self.emit(
            TypeError::UndeclaredReturnValue {
                name: decl.name.name.clone(),
                found,
            },
            body_value_span(body),
        );
    }

    /// Whether a body's answer is a value a caller could read back, as
    /// opposed to a unit, a diverging path, or a type inference never
    /// settled (which already has its own diagnostic).
    fn ty_is_returnable_value(&self, ty: Ty) -> bool {
        !matches!(
            self.tcx.kind(ty),
            None | Some(TyKind::Unit | TyKind::Never | TyKind::Error | TyKind::Var(_))
        )
    }

    fn ty_contains_reference(&self, ty: Ty) -> bool {
        match self.tcx.kind_of(ty) {
            TyKind::Ref { .. } => true,
            TyKind::Array { elem, .. }
            | TyKind::Slice(elem)
            | TyKind::Vec(elem)
            | TyKind::Sender(elem)
            | TyKind::Receiver(elem)
            | TyKind::JoinHandle(elem) => self.ty_contains_reference(*elem),
            TyKind::Tuple(items) => items.iter().any(|item| self.ty_contains_reference(*item)),
            TyKind::HashMap { key, value, .. } => {
                self.ty_contains_reference(*key) || self.ty_contains_reference(*value)
            }
            TyKind::Adt { substs, .. } | TyKind::FnDef { substs, .. } => substs
                .types()
                .iter()
                .any(|item| self.ty_contains_reference(*item)),
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                self.ty_contains_reference(sig.output)
                    || sig
                        .inputs
                        .iter()
                        .any(|item| self.ty_contains_reference(*item))
            }
            _ => false,
        }
    }

    fn ty_contains_nested_vec(&self, ty: Ty) -> bool {
        fn walk(checker: &TypeChecker<'_>, ty: Ty, seen: &mut HashSet<Ty>) -> bool {
            let ty = checker.infer.resolve(checker.tcx, ty);
            if !seen.insert(ty) {
                return false;
            }
            match checker.tcx.kind_of(ty) {
                TyKind::Vec(_) => true,
                TyKind::Array { elem, .. } | TyKind::Slice(elem) => walk(checker, *elem, seen),
                TyKind::Tuple(items) => items.iter().any(|item| walk(checker, *item, seen)),
                TyKind::Adt { def, substs } => checker
                    .tcx
                    .adt_field_tys(*def, substs)
                    .is_some_and(|fields| fields.iter().any(|field| walk(checker, *field, seen))),
                _ => false,
            }
        }

        walk(self, ty, &mut HashSet::new())
    }

    fn reject_stored_reference_type(&mut self, ty: Ty, span: Span, context: &str) {
        if self.ty_contains_reference(ty) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: context.to_string(),
                },
                span,
            );
        }
    }

    fn check_deferred_reference_storage(&mut self) {
        let pending = std::mem::take(&mut self.deferred_reference_storage);
        for (ty, span, context) in pending {
            let ty = self.infer.resolve(self.tcx, ty);
            if !matches!(self.tcx.kind_of(ty), TyKind::Ref { .. }) && self.ty_contains_reference(ty)
            {
                self.emit(
                    TypeError::ReferenceEscapeUnsupported {
                        context: context.to_string(),
                    },
                    span,
                );
            }
        }
    }

    /// Return type of a method called on a bound type-parameter receiver
    /// (`s.method()` where `s: &T` and `T: Trait`): look the method up in
    /// each of the parameter's bound traits. `None` when the receiver is
    /// not a type parameter or no bound declares the method.
    /// Resolves `ty` and strips any chain of `&` / `&mut` wrappers,
    /// returning the underlying type. References are layout-transparent
    /// in Gossamer (the runtime owns memory), so this is used wherever a
    /// value-vs-reference distinction must not produce a diagnostic.
    fn peel_refs(&mut self, ty: Ty) -> Ty {
        let mut cur = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(cur) {
            cur = self.infer.resolve(self.tcx, *inner);
        }
        cur
    }

    fn param_method_sig(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        span: Span,
    ) -> Option<(Ty, Vec<Ty>)> {
        let mut t = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let TyKind::Param { idx, name } = self.tcx.kind(t)? else {
            return None;
        };
        let param_name = name.to_string();
        let bounds = self.current_param_bounds.get(idx.0 as usize)?.clone();
        for bound in bounds {
            let key = (bound, method.to_string());
            if let Some(ret) = self.trait_method_ret.get(&key).copied() {
                let params = self
                    .trait_method_params
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                // A `-> Self::Item` return is concrete only once the
                // receiver is known, so resolve it against this
                // parameter's own bound rather than the trait's
                // declaration-time placeholder.
                let ret = match self.trait_method_ret_assoc.get(&key).cloned() {
                    Some(assoc) => self.resolve_assoc_type_projection_inner(
                        &param_name,
                        &assoc,
                        true,
                        false,
                        span,
                        false,
                    ),
                    None => ret,
                };
                return Some((ret, params));
            }
        }
        None
    }

    /// Rejects a binary operator whose left or right operand is a generic
    /// parameter with no bound licensing it. Returns `true` when a
    /// diagnostic was emitted.
    fn reject_operands_off_bound(
        &mut self,
        lhs_ty: Ty,
        rhs_ty: Ty,
        op: &str,
        method: &str,
        span: Span,
    ) -> bool {
        self.reject_operator_off_bound(lhs_ty, op, method, span)
            || self.reject_operator_off_bound(rhs_ty, op, method, span)
    }

    /// Rejects an operator applied to a generic-parameter operand whose
    /// bounds do not license it.
    ///
    /// A parameter stands for every type a caller may supply, so only a
    /// bound declaring the operator's method guarantees each instantiation
    /// can perform the operation. Returns `true` when a diagnostic was
    /// emitted.
    fn reject_operator_off_bound(
        &mut self,
        operand_ty: Ty,
        op: &str,
        method: &str,
        span: Span,
    ) -> bool {
        let peeled = self.peel_refs(operand_ty);
        let Some(TyKind::Param { idx, name }) = self.tcx.kind(peeled) else {
            return false;
        };
        let param = name.to_string();
        let bounds = self
            .current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default();
        let trait_name = op_trait_name(method);
        // A bound licenses the operator when it is the operator's own trait,
        // when it (or one of its supertraits) declares the operator method,
        // or when its method surface is unknown and so cannot rule the
        // operation out.
        let licensed = bounds.iter().any(|bound| {
            bound == trait_name
                || self
                    .trait_own_methods
                    .get(bound)
                    .is_some_and(|methods| methods.contains(method))
                || self.supertrait_owning_method(bound, method).is_some()
                || (!self.declared_trait_names.contains(bound)
                    && builtin_trait_methods(bound).is_none())
        });
        if licensed {
            return false;
        }
        self.emit(
            TypeError::OperatorNotOnBound {
                param,
                op: op.to_string(),
                trait_name: trait_name.to_string(),
                method: method.to_string(),
                bounds,
            },
            span,
        );
        true
    }

    /// Rejects a method on a bound type-parameter receiver that resolves
    /// only through a *supertrait* of one of the parameter's bounds
    /// (P0-5: `fn describe<T: Pet>(p: &T)` calling `p.name()` where
    /// `name` is declared on `Animal` and `trait Pet: Animal`). The
    /// compiled tiers cannot lower supertrait-through-bound dispatch
    /// (SPEC §3.8); it runs right on the VM but miscompiles native, so it
    /// is rejected uniformly. Returns `true` when a diagnostic was
    /// emitted.
    fn reject_supertrait_method_through_bound(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        let mut t = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Param { idx, .. }) = self.tcx.kind(t) else {
            return false;
        };
        let idx = *idx;
        let Some(bounds) = self.current_param_bounds.get(idx.0 as usize).cloned() else {
            return false;
        };
        // If the method is declared directly on any bound, it is a normal
        // generic-bound call (handled elsewhere), not a supertrait leak.
        for bound in &bounds {
            if self
                .trait_own_methods
                .get(bound)
                .is_some_and(|m| m.contains(method))
            {
                return false;
            }
        }
        for bound in &bounds {
            if let Some(supertrait) = self.supertrait_owning_method(bound, method) {
                let param = self
                    .current_generic_scope
                    .iter()
                    .find(|(_, (pidx, _))| *pidx == idx)
                    .map_or_else(|| "T".to_string(), |(name, _)| name.clone());
                self.emit(
                    TypeError::SupertraitMethodThroughBound {
                        param,
                        method: method.to_string(),
                        bound: bound.clone(),
                        supertrait,
                    },
                    span,
                );
                return true;
            }
        }
        false
    }

    /// Walks the supertrait graph of `trait_name` (transitively) and
    /// returns the first supertrait that declares `method`, or `None`.
    fn supertrait_owning_method(&self, trait_name: &str, method: &str) -> Option<String> {
        let mut stack: Vec<String> = self
            .trait_supertraits
            .get(trait_name)
            .cloned()
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if self
                .trait_own_methods
                .get(&name)
                .is_some_and(|m| m.contains(method))
            {
                return Some(name);
            }
            if let Some(supers) = self.trait_supertraits.get(&name) {
                stack.extend(supers.iter().cloned());
            }
        }
        None
    }

    /// Pre-scans a function body for `archive::{tar,zip}::write(arg)`
    /// calls whose single argument is a path to a local binding, and
    /// records that binding's node so its literal initializer is later
    /// re-typed to the `[(String, [u8])]` parameter.
    fn collect_write_arg_bindings(&mut self, body: &Expr) {
        let mut collector = WriteArgPathCollector {
            arg_paths: Vec::new(),
        };
        gossamer_ast::visitor::Visitor::visit_expr(&mut collector, body);
        if collector.arg_paths.is_empty() {
            return;
        }
        let vec_pair = self.archive_entry_vec_ty();
        for path_node in collector.arg_paths {
            if let Some(Resolution::Local(binding)) = self.resolutions.get(path_node) {
                self.write_arg_bindings.insert(binding, vec_pair);
            }
        }
    }

    fn bind_fn_param(&mut self, param: &FnParam) {
        match param {
            FnParam::Typed { pattern, ty, .. } => {
                let param_ty = self.type_from_ast(ty);
                self.check_param_reference_pattern(pattern, param_ty);
                self.bind_pattern(pattern, param_ty);
            }
            FnParam::Receiver(recv) => {
                // Bind `self` to the enclosing `impl`'s `Self` type so
                // `self.field` accesses resolve; fall back to a fresh var
                // only outside an impl context (defensive). `self` and
                // `&self` name the same value - a receiver is reached
                // without copying either way - so only `&mut self`, which
                // writes back, carries a reference in the type.
                let ty = match self.current_self_ty {
                    Some(self_ty) => match recv {
                        gossamer_ast::Receiver::Owned | gossamer_ast::Receiver::RefShared => {
                            self_ty
                        }
                        gossamer_ast::Receiver::RefMut => self.tcx.intern(TyKind::Ref {
                            mutability: Mutbl::Mut,
                            inner: self_ty,
                        }),
                    },
                    None => self.fresh(),
                };
                self.bind_local("self", ty);
                // Receiver syntax controls referent capability, not whether
                // the local `self` slot may be rebound.
                self.bind_local_mutability("self", false);
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr) -> Ty {
        self.check_expr_expecting(expr, Expectation::None)
    }

    fn check_expr_expecting(&mut self, expr: &Expr, expected: Expectation) -> Ty {
        if self.enter_recursion(expr.span).is_err() {
            let err = self.tcx.error_ty();
            return self.record(expr.id, err);
        }
        let ty = self.check_expr_kind(expr, expected);
        self.check_expected_integer_literal_range(expr, expected, ty);
        self.leave_recursion();
        self.record(expr.id, ty)
    }

    fn check_expected_integer_literal_range(
        &mut self,
        expr: &Expr,
        expected: Expectation,
        actual: Ty,
    ) {
        let (text, has_suffix) = match &expr.kind {
            ExprKind::Literal(Literal::Int(text)) => (
                text.clone(),
                INT_SUFFIXES
                    .iter()
                    .any(|(suffix, _)| text.ends_with(suffix)),
            ),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => match &operand.kind {
                ExprKind::Literal(Literal::Int(text)) => (
                    format!("-{text}"),
                    INT_SUFFIXES
                        .iter()
                        .any(|(suffix, _)| text.ends_with(suffix)),
                ),
                _ => return,
            },
            _ => return,
        };
        if matches!(self.tcx.kind(actual), Some(TyKind::Error)) || has_suffix {
            return;
        }
        let Some(expected) = self.expectation_target(expected) else {
            return;
        };
        let Some(TyKind::Int(int_ty)) = self.tcx.kind(expected).cloned() else {
            return;
        };
        if !int_literal_fits(&text, int_ty) {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: text,
                    ty: int_ty.as_str().to_string(),
                },
                expr.span,
            );
        }
    }

    /// Resolves the expectation to the structural type it imposes,
    /// peeling one `Ref` - a `&[T]` parameter shapes a bare `[..]`
    /// literal exactly like `[T]` (the borrow is transparent at the
    /// layout level).
    fn expectation_target(&mut self, expected: Expectation) -> Option<Ty> {
        let ty = expected.ty()?;
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Ref { inner, .. }) => Some(self.infer.resolve(self.tcx, *inner)),
            Some(_) => Some(resolved),
            None => None,
        }
    }

    /// Type of a `loop` expression: the unified type of its value-carrying
    /// breaks (`let x = loop { break v }` => `x: typeof(v)`); a value-less
    /// loop keeps the divergent `never` type.
    fn check_loop(&mut self, body: &Expr) -> Ty {
        let break_ty = self.fresh();
        self.loop_break_tys.push((break_ty, false));
        self.check_expr(body);
        self.report_discarded_result(body, None);
        let (break_ty, used) = self.loop_break_tys.pop().expect("loop stack");
        if used {
            self.infer.resolve(self.tcx, break_ty)
        } else {
            self.tcx.never()
        }
    }

    /// Type-checks a `return value` / `break value`; both diverge (`never`)
    /// but thread their value into the enclosing function return type or the
    /// loop break-type var respectively.
    fn check_return_or_break(&mut self, expr: &Expr, value: Option<&Expr>) -> Ty {
        if let Some(value) = value {
            // `return [..]` carries the declared return shape the same way the
            // block-tail path does, so an explicit `return []` in a `-> [T]` fn
            // is shaped as a Vec rather than a fixed `[T; 0]`.
            let value_expected = match (&expr.kind, self.current_fn_ret) {
                (ExprKind::Return(_), Some(ret)) => Expectation::HasType(ret),
                _ => Expectation::None,
            };
            let got = self.check_expr_expecting(value, value_expected);
            // The expectation only shapes literal containers; unify the checked
            // value against the declared return type so a non-literal mismatch
            // is reported the same way a block tail is.
            if let (ExprKind::Return(_), Some(ret)) = (&expr.kind, self.current_fn_ret) {
                self.unify(ret, got, value.span);
            }
            // `break value` unifies its value with the enclosing loop's
            // break-type var and marks the loop as value-yielding.
            if matches!(expr.kind, ExprKind::Break { .. }) {
                let break_ty = self.loop_break_tys.last_mut().map(|last| {
                    last.1 = true;
                    last.0
                });
                if let Some(break_ty) = break_ty {
                    self.unify(break_ty, got, value.span);
                }
            }
        } else if matches!(expr.kind, ExprKind::Return(_))
            && let Some(ret) = self.current_fn_ret
        {
            let unit = self.tcx.unit();
            self.unify(ret, unit, expr.span);
        }
        self.tcx.never()
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "expression dispatch - arms map 1:1 to ExprKind variants; splitting hides the dispatch table"
    )]
    fn check_expr_kind(&mut self, expr: &Expr, expected: Expectation) -> Ty {
        match &expr.kind {
            ExprKind::Literal(lit) => self.type_of_literal(lit, expr.span),
            ExprKind::Path(path) => self.check_path_expr(expr.id, path, expr.span),
            ExprKind::Call { callee, args } => {
                let ty = self.check_call(callee, args, expected);
                if let ExprKind::Path(path) = &callee.kind
                    && let Some(last) = path.segments.last()
                {
                    // The prelude `spawn`, not a module's own (`exec::spawn`).
                    if last.name.name == "spawn"
                        && path.segments.len() == 1
                        && let Some(arg) = args.first()
                    {
                        // An aggregate written inline at the spawned call has
                        // no owner on either side of the boundary. A reference
                        // the closure body creates and consumes never crosses
                        // it, so only a captured one is rejected, which
                        // `reject_unshareable_goroutine_captures` covers.
                        self.reject_spawn_inline_aggregate_args(arg);
                        self.reject_unshareable_goroutine_captures(arg);
                    }
                    // The guarded slot is one word that every tier reads
                    // back as an integer. A payload without that agreement
                    // is refused here rather than compiling on one tier and
                    // failing to lower on another.
                    if last.name.name == "new"
                        && path
                            .segments
                            .iter()
                            .any(|segment| segment.name.name == "Shared")
                        && let Some(arg) = args.first()
                        && let Some(arg_ty) = self.table.get(arg.id)
                    {
                        // Judged once inference has settled: a numeric
                        // literal is still an open variable here, and
                        // whether it lands on an integer decides the answer.
                        self.deferred_shared_payloads.push((arg_ty, arg.span));
                    }
                }
                // A non-generic tuple-variant constructor call is its
                // enum: unify so bindings (`let p = Sign::Pos(7)`) carry
                // the nominal type operator dispatch resolves against.
                // Generic enums are absent from `enum_tys` and keep the
                // fresh-var path.
                if let Some(e) = self.variant_ctor_enum_ty(expr) {
                    let r = self.infer.resolve(self.tcx, ty);
                    if matches!(self.tcx.kind(r), Some(TyKind::Var(_))) {
                        self.unify(ty, e, expr.span);
                    }
                }
                ty
            }
            ExprKind::MethodCall {
                receiver,
                name,
                name_span,
                desugared_from,
                generics,
                args,
            } => {
                let written = desugared_from
                    .as_ref()
                    .map_or(name.name.as_str(), |i| &i.name);
                // Scoped to this call: a method call nested in the receiver or
                // an argument sets its own spelling and restores this one.
                let outer = std::mem::replace(&mut self.written_method, written.to_string());
                let ty = self.check_method_call(
                    MethodCallSite {
                        call_id: expr.id,
                        method: &name.name,
                        name_span: *name_span,
                        generics,
                    },
                    receiver,
                    args,
                    expected,
                );
                self.written_method = outer;
                ty
            }
            ExprKind::FieldAccess { receiver, field } => {
                let receiver_ty = self.check_expr(receiver);
                match field {
                    gossamer_ast::FieldSelector::Named(name) => {
                        self.reject_private_field(receiver_ty, &name.name, expr.span);
                        match self.lookup_field_ty_diagnosed(receiver_ty, &name.name) {
                            Ok(ty) => ty,
                            Err(err) => {
                                self.emit(
                                    with_field_span(err, field_name_span(expr, &name.name)),
                                    expr.span,
                                );
                                self.fresh()
                            }
                        }
                    }
                    gossamer_ast::FieldSelector::Index(idx) => {
                        self.check_tuple_field(receiver_ty, *idx, expr.span)
                    }
                }
            }
            ExprKind::Unary { op, operand } => self.check_unary(*op, operand, expr.span, expected),
            ExprKind::Index { base, index } => self.check_index_expr(base, index, expr.span),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, expr.span),
            ExprKind::Assign { place, value, op } => self.check_assign(place, value, *op),
            ExprKind::Cast { value, ty } => {
                let from = self.check_expr(value);
                let to = self.type_from_ast(ty);
                self.check_cast(from, to, expr.span);
                to
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.check_if(condition, then_branch, else_branch.as_deref(), expected),
            ExprKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms, expected),
            ExprKind::Loop { body, .. } => self.check_loop(body),
            ExprKind::While {
                condition, body, ..
            } => {
                let bool_ty = self.tcx.bool_ty();
                let cond_ty = self.check_expr(condition);
                self.unify(bool_ty, cond_ty, condition.span);
                self.check_expr(body);
                self.report_discarded_result(body, None);
                self.tcx.unit()
            }
            ExprKind::For {
                pattern,
                iter,
                body,
                ..
            } => self.check_for(pattern, iter, body),
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.check_block(block, expected),
            ExprKind::Closure { params, ret, body } => {
                self.check_closure(params, ret.as_ref(), body, expected)
            }
            ExprKind::Return(value) | ExprKind::Break { value, .. } => {
                self.check_return_or_break(expr, value.as_deref())
            }
            ExprKind::Continue { .. } => self.tcx.never(),
            ExprKind::Tuple(elems) => {
                let want: Option<Vec<Ty>> = match self.expectation_target(expected) {
                    Some(target) => match self.tcx.kind(target) {
                        Some(TyKind::Tuple(tys)) if tys.len() == elems.len() => Some(tys.clone()),
                        _ => None,
                    },
                    None => None,
                };
                if let Some(want) = want {
                    for (elem, want_ty) in elems.iter().zip(&want) {
                        let got = self.check_expr_expecting(elem, expected.rewrap(*want_ty));
                        if expected.unifies() {
                            self.unify(*want_ty, got, elem.span);
                        }
                    }
                    return self.tcx.intern(TyKind::Tuple(want));
                }
                let tys: Vec<Ty> = elems.iter().map(|e| self.check_expr(e)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            ExprKind::MapLiteral(entries) => self.check_map_literal(entries, expected),
            ExprKind::SetLiteral(entries) => self.check_set_literal(entries, expected),
            ExprKind::Struct {
                path,
                fields,
                base,
                syntax,
            } => {
                // Resolve the header path to an Adt type. Unifying
                // named field values with the declared field
                // types lets downstream field-access nodes see
                // concrete leaf types.
                //
                // For a generic struct (`Pair<A, B>`), the
                // declared field types carry `TyKind::Param`
                // slots. We allocate one fresh inference variable
                // per generic parameter and substitute those into
                // each field type before unifying with the
                // literal's value type - that lets the inferencer
                // pin `A` and `B` from the field values.
                let head_node = expr.id;
                // A `use`d type keeps its opaque `Import` resolution; the
                // definition it names is what the literal is built from.
                let head_res = self.resolutions.get(head_node).map(|res| match res {
                    Resolution::Import { .. } => self
                        .resolutions
                        .import_def(head_node)
                        .and_then(|def| {
                            self.resolutions
                                .kind_of(def)
                                .map(|kind| Resolution::Def { def, kind })
                        })
                        .unwrap_or(res),
                    other => other,
                });
                let (struct_ty, substs_table) = if let Some(res) = head_res {
                    match res {
                        Resolution::Def {
                            def,
                            kind:
                                gossamer_resolve::DefKind::Struct | gossamer_resolve::DefKind::Enum,
                        } => {
                            let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
                            let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
                            let substs_obj = crate::Substs::from_types(substs.iter().copied());
                            self.defer_adt_bounds(def, &substs, expr.span);
                            (
                                self.tcx.intern(TyKind::Adt {
                                    def,
                                    substs: substs_obj,
                                }),
                                substs,
                            )
                        }
                        _ => (self.fresh(), Vec::new()),
                    }
                } else {
                    (self.fresh(), Vec::new())
                };
                // `http::Response { … }` - no resolver entry (stdlib
                // opaque type). Pin the literal to the sentinel
                // Response Adt and check the known field shapes so a
                // wrong-typed field reports a clean type mismatch
                // instead of slipping through as an inference
                // variable. `body` stays unchecked: the runtime
                // accepts both String and `[u8]` bodies.
                let resolved_probe = self.infer.resolve(self.tcx, struct_ty);
                let path_tail = path.segments.last().map(|s| s.name.name.as_str());
                let (struct_ty, http_response_fields) =
                    if matches!(self.tcx.kind_of(resolved_probe), TyKind::Var(_))
                        && path_tail == Some("Response")
                    {
                        let def = gossamer_resolve::DefId::local(u32::MAX - 5);
                        let response_ty = self.tcx.intern(TyKind::Adt {
                            def,
                            substs: crate::Substs::new(),
                        });
                        let s = self.tcx.string_ty();
                        let pair = self.tcx.intern(TyKind::Tuple(vec![s, s]));
                        let headers_ty = self.tcx.intern(TyKind::Vec(pair));
                        let fields: Vec<(String, Ty)> = vec![
                            ("status".to_string(), self.tcx.int_ty(IntTy::I64)),
                            ("body".to_string(), self.fresh()),
                            ("content_type".to_string(), s),
                            ("headers".to_string(), headers_ty),
                        ];
                        (response_ty, Some(fields))
                    } else {
                        (struct_ty, None)
                    };
                let resolved = self.infer.resolve(self.tcx, struct_ty);
                let http_response_literal = http_response_fields.is_some();
                let declared: Option<Vec<(String, Ty)>> = match self.tcx.kind_of(resolved) {
                    // The literal-specific Response list takes priority
                    // over the stdlib layout: the layout declares
                    // `body: String`, but literal bodies may also be
                    // `[u8]` byte arrays (interp parity).
                    TyKind::Adt { def, .. } => {
                        http_response_fields.or_else(|| self.struct_fields.get(def).cloned())
                    }
                    _ => None,
                };
                let tuple_struct_literal = if let TyKind::Adt { def, .. } =
                    self.tcx.kind_of(resolved).clone()
                {
                    let is_tuple = self.tcx.is_tuple_struct(def.local);
                    if matches!(syntax, gossamer_ast::expr::StructExprSyntax::Braced) && is_tuple {
                        let name = self
                            .tcx
                            .def_name(def)
                            .map_or_else(|| "<struct>".to_string(), ToString::to_string);
                        self.emit(
                            TypeError::TupleStructConstructorParenthesesRequired { name },
                            expr.span,
                        );
                    }
                    is_tuple
                } else {
                    false
                };
                let require_all_fields = !http_response_literal;
                if !tuple_struct_literal
                    && fields
                        .iter()
                        .any(|field| struct_literal_positional_index(&field.name.name).is_some())
                {
                    let name = path.segments.last().map_or_else(
                        || "<struct>".to_string(),
                        |segment| segment.name.name.clone(),
                    );
                    self.emit(TypeError::NamedStructFieldsRequired { name }, expr.span);
                }
                let resolved_literal_fields =
                    if !tuple_struct_literal && let Some(declared_fields) = declared.as_ref() {
                        Some(self.resolve_struct_literal_fields(
                            path,
                            fields,
                            base.is_some(),
                            declared_fields,
                            require_all_fields,
                            expr.span,
                        ))
                    } else {
                        None
                    };
                // Naming a field in a literal is a reference to it, so the
                // same visibility rule applies: a struct with a private
                // field cannot be built from outside its declaring module.
                // A `..base` spread carries every field the literal does not
                // name, so it references all of them.
                if let TyKind::Adt { def, .. } = self.tcx.kind_of(resolved).clone()
                    && let (Some(literal_fields), Some(declared_fields)) =
                        (resolved_literal_fields.as_ref(), declared.as_ref())
                {
                    let referenced: Vec<String> = if base.is_some() {
                        declared_fields
                            .iter()
                            .map(|(field_name, _)| field_name.clone())
                            .collect()
                    } else {
                        literal_fields
                            .values()
                            .filter_map(|&idx| declared_fields.get(idx))
                            .map(|(field_name, _)| field_name.clone())
                            .collect()
                    };
                    for field_name in referenced {
                        self.reject_private_field_of(def, &field_name, expr.span);
                    }
                }
                for (field_idx, field) in fields.iter().enumerate() {
                    if let Some(value) = &field.value {
                        // Substitute `Param { idx }` slots with the
                        // fresh inference vars allocated above so
                        // unification can drive `A`, `B`, ... from
                        // each literal's value type. Checking the
                        // value against the declared field type lets
                        // `S { xs: ["a", "b"] }` lay a heap Vec, not
                        // a fixed `[T; N]`, into a Vec-typed field.
                        let dty_sub = declared.as_ref().and_then(|declared_fields| {
                            resolved_literal_fields
                                .as_ref()
                                .and_then(|resolved| resolved.get(&field_idx).copied())
                                .and_then(|decl_idx| declared_fields.get(decl_idx))
                                .or_else(|| {
                                    declared_fields.iter().find(|(n, _)| n == &field.name.name)
                                })
                                .map(|(_, dty)| *dty)
                        });
                        let dty_sub =
                            dty_sub.map(|dty| self.subst_params_in_ty(dty, &substs_table));
                        let field_expected = match dty_sub {
                            Some(dty) => Expectation::HasType(dty),
                            None => Expectation::None,
                        };
                        let val_ty = self.check_expr_expecting(value, field_expected);
                        if let Some(dty) = dty_sub {
                            self.unify(dty, val_ty, value.span);
                        }
                    }
                }
                if let Some(base) = base {
                    self.check_expr(base);
                }
                struct_ty
            }
            ExprKind::Array(arr) => {
                let target = self.expectation_target(expected);
                let wants_growable = target.is_some_and(|target| {
                    matches!(
                        self.tcx.kind(target),
                        Some(TyKind::Vec(_) | TyKind::Slice(_))
                    )
                });
                let wants_array = target.is_some_and(|target| {
                    matches!(self.tcx.kind(target), Some(TyKind::Array { .. }))
                });
                let _ = wants_growable;
                if wants_array {
                    self.check_array(arr, expected)
                } else {
                    self.check_vec_literal(arr, expected)
                }
            }
            ExprKind::FixedArray(arr) => self.check_array(arr, expected),
            ExprKind::Range { start, end, .. } => {
                // Rust-style ranges are lazy values. Index and for-loop
                // positions still consume their bounds syntactically.
                let start_ty = start.as_ref().map(|bound| self.check_expr(bound));
                let end_ty = end.as_ref().map(|bound| self.check_expr(bound));
                let elem = start_ty
                    .filter(|ty| self.is_integer(*ty))
                    .or_else(|| end_ty.filter(|ty| self.is_integer(*ty)))
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                if let (Some(bound), Some(ty)) = (start, start_ty) {
                    self.unify(elem, ty, bound.span);
                }
                if let (Some(bound), Some(ty)) = (end, end_ty) {
                    self.unify(elem, ty, bound.span);
                }
                self.tcx.range_ty(elem)
            }
            ExprKind::Try(inner) => {
                let inner_expectation = match self.expectation_target(expected) {
                    Some(ok) => {
                        let err = self.tcx.dyn_error_ty();
                        Expectation::HasType(self.result_adt_ty(ok, err))
                    }
                    None => Expectation::None,
                };
                let inner_ty = self.check_expr_expecting(inner, inner_expectation);
                self.check_question_mark(inner_ty, inner.span)
            }
            ExprKind::Select(arms) => self.check_select(arms),
            ExprKind::Error => self.fresh(),
        }
    }

    /// Returns the type of field `idx` when `ty` is a tuple struct (an
    /// `Adt` whose `idx`-th field is the positional name "idx"), else
    /// `None`. Tuple-struct fields are modelled as named fields "0".."N-1",
    /// so `p.0` positional access reads field "0".
    fn tuple_struct_field_ty(&self, ty: Ty, idx: u32) -> Option<Ty> {
        let TyKind::Adt { def, substs } = self.tcx.kind_of(ty).clone() else {
            return None;
        };
        let is_positional = self
            .struct_fields
            .get(&def)
            .and_then(|list| list.get(idx as usize))
            .is_some_and(|(name, _)| *name == idx.to_string());
        if !is_positional {
            return None;
        }
        self.tcx
            .adt_field_tys(def, &substs)
            .and_then(|tys| tys.get(idx as usize).copied())
    }

    fn resolve_struct_literal_fields(
        &mut self,
        path: &gossamer_ast::PathExpr,
        fields: &[gossamer_ast::StructExprField],
        has_base: bool,
        declared_fields: &[(String, Ty)],
        require_all_fields: bool,
        span: Span,
    ) -> HashMap<usize, usize> {
        let name = path.segments.last().map_or_else(
            || "<struct>".to_string(),
            |segment| segment.name.name.clone(),
        );
        let declared_by_name: HashMap<&str, usize> = declared_fields
            .iter()
            .enumerate()
            .map(|(idx, (field_name, _))| (field_name.as_str(), idx))
            .collect();
        let mut resolved = HashMap::new();
        let mut filled = HashSet::new();
        let mut keyed_seen = HashSet::new();
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_some() {
                continue;
            }
            let Some(&decl_idx) = declared_by_name.get(field.name.name.as_str()) else {
                self.emit(
                    TypeError::UnknownField {
                        ty: name.clone(),
                        field: field.name.name.clone(),
                        opaque: false,
                        declared: declared_fields
                            .iter()
                            .map(|(field_name, _)| field_name.clone())
                            .collect(),
                        field_span: None,
                        method_of_same_name: false,
                    },
                    span,
                );
                continue;
            };
            if !keyed_seen.insert(field.name.name.as_str()) {
                self.emit(
                    TypeError::DuplicateStructField {
                        name: name.clone(),
                        field: field.name.name.clone(),
                    },
                    span,
                );
            }
            filled.insert(decl_idx);
            resolved.insert(field_idx, decl_idx);
        }

        let mut next_pos = 0usize;
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_none() {
                continue;
            }
            while next_pos < declared_fields.len() && filled.contains(&next_pos) {
                next_pos += 1;
            }
            if next_pos >= declared_fields.len() {
                self.emit(
                    TypeError::TooManyStructFields {
                        name: name.clone(),
                        expected: declared_fields.len(),
                        found: fields.len(),
                    },
                    span,
                );
                continue;
            }
            filled.insert(next_pos);
            resolved.insert(field_idx, next_pos);
        }

        if require_all_fields && !has_base {
            for (idx, (field_name, _)) in declared_fields.iter().enumerate() {
                if !filled.contains(&idx) {
                    self.emit(
                        TypeError::MissingStructField {
                            name: name.clone(),
                            field: field_name.clone(),
                        },
                        span,
                    );
                }
            }
        }
        resolved
    }

    /// Type of `value.N` positional access. Rejects access on a concrete
    /// non-tuple receiver and out-of-range indices (GT0023); a still-
    /// unresolved receiver is deferred for re-check after defaulting.
    fn check_tuple_field(&mut self, receiver_ty: Ty, idx: u32, span: Span) -> Ty {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(resolved).clone() {
            resolved = self.infer.resolve(self.tcx, inner);
        }
        if let Some(fty) = self.tuple_struct_field_ty(resolved, idx) {
            return fty;
        }
        match self.tcx.kind_of(resolved).clone() {
            TyKind::Tuple(elems) => elems.get(idx as usize).copied().unwrap_or_else(|| {
                let ty = self.render_public_ty(resolved);
                self.emit(
                    TypeError::NoTupleField {
                        ty,
                        index: u64::from(idx),
                    },
                    span,
                );
                self.fresh()
            }),
            TyKind::Var(_) => {
                let result = self.fresh();
                self.deferred_structural.push(DeferredStructural {
                    ty: resolved,
                    span,
                    kind: DeferredStructuralKind::TupleField(u64::from(idx)),
                    result: Some(result),
                });
                result
            }
            other => {
                if !is_soft_for_structural_use(&other) {
                    let ty = self.render_public_ty(resolved);
                    self.emit(
                        TypeError::NoTupleField {
                            ty,
                            index: u64::from(idx),
                        },
                        span,
                    );
                }
                self.fresh()
            }
        }
    }

    /// Element type of `base[index]`. Rejects indexing a concrete
    /// non-indexable receiver (GT0021); a still-unresolved receiver is
    /// deferred for re-check after defaulting.
    fn check_index_expr(&mut self, base: &Expr, index: &Expr, span: Span) -> Ty {
        let base_ty = self.check_expr(base);
        self.check_expr(index);
        let range_index = matches!(index.kind, ExprKind::Range { .. });
        let mut cur = self.infer.resolve(self.tcx, base_ty);
        loop {
            match self.tcx.kind_of(cur).clone() {
                TyKind::Ref { inner, .. } => cur = inner,
                TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => {
                    if range_index {
                        return self.tcx.intern(TyKind::Vec(elem));
                    }
                    return elem;
                }
                TyKind::String => {
                    if range_index {
                        return self.tcx.string_ty();
                    }
                    return self.tcx.char_ty();
                }
                TyKind::Var(_) => {
                    let result = self.fresh();
                    if std::env::var_os("GOS_DBG_IDX").is_some() {
                        eprintln!(
                            "[idx] deferred base={:?} result={:?}",
                            self.tcx.kind_of(cur),
                            self.tcx.kind_of(result)
                        );
                    }
                    self.deferred_structural.push(DeferredStructural {
                        ty: cur,
                        span,
                        kind: DeferredStructuralKind::Index,
                        result: Some(result),
                    });
                    return result;
                }
                other => {
                    // `a[i]` on a user struct / enum routes to its `index` impl
                    // method (one argument); the element type is that method's
                    // return type. The base node is anchored to its resolved
                    // nominal type so tier lowering dispatches the call.
                    if matches!(other, TyKind::Adt { .. }) && self.adt_name_of(cur).is_some() {
                        self.record(base.id, cur);
                        if let Some(ret) = self.adt_op_method_ret(cur, "index", 1) {
                            return ret;
                        }
                    }
                    if !is_soft_for_structural_use(&other) {
                        let ty = self.render_public_ty(cur);
                        self.emit(TypeError::NotIndexable { ty }, span);
                    }
                    return self.fresh();
                }
            }
        }
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], expected: Expectation) -> Ty {
        self.check_overlapping_mutable_call_args(args);
        if matches!(callee.kind, ExprKind::Path(_)) {
            self.callee_path_nodes.insert(callee.id);
        }
        let callee_ty = self.check_expr(callee);
        let arg_expectations = self.call_arg_expectations(callee, callee_ty, args.len(), expected);
        let arg_tys: Vec<Ty> = match self.data_last_combinator_arg_tys(callee, args) {
            Some(tys) => tys,
            None => args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let exp = arg_expectations
                        .as_ref()
                        .and_then(|exps| exps.get(i).copied())
                        .unwrap_or(Expectation::None);
                    self.check_expr_expecting(a, exp)
                })
                .collect(),
        };
        self.check_mutating_qualified_call(callee, args);
        self.check_call_inner(callee, args, callee_ty, &arg_tys, expected)
    }

    /// Argument types for a data-last `iter::` combinator, checked with the
    /// sequence argument first. The element type it yields binds the leading
    /// closure's parameter, so a projection out of that parameter resolves
    /// while the closure body is checked. Returns `None` for every other
    /// call, which keeps source-order checking.
    fn data_last_combinator_arg_tys(&mut self, callee: &Expr, args: &[Expr]) -> Option<Vec<Ty>> {
        if args.len() < 2 {
            return None;
        }
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names = self.resolved_value_path_names(callee.id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let name = last.first().copied()?;
        if combinator_module_name(module)? != "iter"
            || Self::std_combinator_arity("iter", name)? != args.len()
        {
            return None;
        }
        let data_index = args.len() - 1;
        let data_ty = self.check_expr_expecting(&args[data_index], Expectation::None);
        let elem = match self.tcx.kind(self.infer.resolve(self.tcx, data_ty)) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. }
                | TyKind::Iterator(elem),
            ) => *elem,
            _ => return None,
        };
        let mut arg_tys = vec![data_ty; args.len()];
        for (i, arg) in args.iter().enumerate().take(data_index) {
            let expectation = match &arg.kind {
                ExprKind::Closure { params, .. } if params.len() == 1 => {
                    let output = self.fresh();
                    let sig = FnSig {
                        inputs: vec![elem],
                        output,
                    };
                    Expectation::HasType(self.tcx.intern(TyKind::FnPtr(sig)))
                }
                _ => Expectation::None,
            };
            arg_tys[i] = self.check_expr_expecting(arg, expectation);
        }
        Some(arg_tys)
    }

    /// Per-argument expectations for a call, derived (in priority
    /// order) from the callee's known signature, a variant
    /// constructor's declared payload types (`Value::Blob([1, 2, 3])`
    /// shapes its payload as a heap `[u8]`, not a fixed `[i64; 3]`),
    /// the stdlib archive-write parameter, or - for the bare `Some` /
    /// `Ok` / `Err` constructors - the call's own expected type.
    fn call_arg_expectations(
        &mut self,
        callee: &Expr,
        callee_ty: Ty,
        n_args: usize,
        expected: Expectation,
    ) -> Option<Vec<Expectation>> {
        let resolved = self.infer.resolve(self.tcx, callee_ty);
        // A generic function's parameter types carry rigid `Param` slots;
        // shaping the arguments against them would bind the shared `Param`
        // at the first call and reject every later call with a different
        // concrete type. Leave such arguments unshaped - `check_call_inner`
        // instantiates the signature with fresh variables per call site.
        if let Some(TyKind::FnDef { def, .. }) = self.tcx.kind(resolved)
            && self.fn_generic_arity.contains_key(def)
        {
            return None;
        }
        let sig: Option<FnSig> = match self.tcx.kind(resolved).cloned() {
            Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) => Some(sig),
            Some(TyKind::FnDef { def, .. }) => self.fn_sigs.get(&def).cloned(),
            _ => None,
        };
        if let Some(sig) = sig {
            if sig.inputs.len() == n_args {
                return Some(
                    sig.inputs
                        .iter()
                        .map(|t| Expectation::HasType(*t))
                        .collect(),
                );
            }
            return None;
        }
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let n = path.segments.len();
        if n >= 2 {
            let key = (
                path.segments[n - 2].name.name.clone(),
                path.segments[n - 1].name.name.clone(),
            );
            if let Some(payloads) = self.enum_variant_payloads.get(&key).cloned()
                && payloads.len() == n_args
            {
                // A generic enum's declared payloads carry `Param` slots;
                // this call site's own instantiation is what they shape
                // against.
                let payloads = match self.variant_ctor_instantiation(callee.id, &key.0) {
                    Some((_, substs)) => payloads
                        .iter()
                        .map(|t| self.subst_params_in_ty(*t, &substs))
                        .collect(),
                    None => payloads,
                };
                // Coerce-only: variant payload registration is keyed
                // by (enum, variant) name and a same-named pair from
                // another scope must not unify into this call.
                return Some(payloads.iter().map(|t| Expectation::Coerce(*t)).collect());
            }
        }
        let last = path.segments[n - 1].name.name.as_str();
        if last == "write"
            && n >= 2
            && matches!(path.segments[n - 2].name.name.as_str(), "tar" | "zip")
            && n_args == 1
        {
            let entries = self.archive_entry_vec_ty();
            return Some(vec![Expectation::Coerce(entries)]);
        }
        if n == 1 && n_args == 1 {
            if last == "Reverse"
                && let Some(target) = self.expectation_target(expected)
                && let Some(TyKind::Adt { def, substs }) = self.tcx.kind(target)
                && def.local == REVERSE_DEF_LOCAL
                && let Some(payload) = substs.types().first().copied()
            {
                return Some(vec![expected.rewrap(payload)]);
            }
            // `Some(x)` / `Ok(x)` / `Err(e)`: thread the expected
            // `Option<T>` / `Result<T, E>` payload slot into the
            // argument so `Some([1, 2])` against `Option<Vec<i64>>`
            // lays a heap Vec into the payload.
            let payload_slot = match last {
                "Some" | "Ok" => Some(0),
                "Err" => Some(1),
                _ => None,
            };
            if let Some(slot) = payload_slot
                && let Some(target) = self.expectation_target(expected)
                && let Some(TyKind::Adt { def, substs }) = self.tcx.kind(target)
            {
                let name_ok = match last {
                    "Some" => self.tcx.def_name(*def) == Some("Option"),
                    _ => self.tcx.def_name(*def) == Some("Result"),
                };
                if name_ok && let Some(payload) = substs.types().get(slot).copied() {
                    return Some(vec![expected.rewrap(payload)]);
                }
            }
        }
        self.stdlib_signature_arg_expectations(callee.id, path, n_args)
    }

    /// Rejects `json::render` / `json::encode` of an enum value
    /// (`Result` / `Option` / user enum). The encoder is polymorphic
    /// over `json::Value`, scalars, arrays, and structs, but an enum
    /// has no JSON form and is almost always a `json::parse(..)` whose
    /// `?` was forgotten. The VM tolerated the misuse (emitting the
    /// `Ok` payload) while a native build silently emitted `""`, so the
    /// checker rejects it uniformly with a `?`-pointing diagnostic.
    fn reject_json_enum_arg(&mut self, op: &str, callee: &Expr, args: &[Expr], arg_tys: &[Ty]) {
        let Some(&first_ty) = arg_tys.first() else {
            return;
        };
        let mut peeled = self.infer.resolve(self.tcx, first_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled).cloned() {
            peeled = self.infer.resolve(self.tcx, inner);
        }
        let adt_def = match self.tcx.kind(peeled) {
            Some(TyKind::Adt { def, .. }) => Some(*def),
            _ => None,
        };
        if let Some(def) = adt_def
            && self.tcx.struct_field_tys(def).is_none()
        {
            let span = args.first().map_or(callee.span, |a| a.span);
            let ty = self.render_public_ty(peeled);
            self.emit(
                TypeError::JsonNotSerializable {
                    op: op.to_string(),
                    ty,
                },
                span,
            );
        }
    }

    /// Substitutes a generic function's signature for one call site:
    /// type parameters become the fresh inference variables in `vars`,
    /// and const parameters are inferred from the array arguments whose
    /// lengths name them. Re-records the callee as a `FnDef` carrying the
    /// resolved substitution so MIR monomorphisation reads the concrete
    /// instantiation.
    fn instantiate_generic_sig(
        &mut self,
        callee: &Expr,
        def: gossamer_resolve::DefId,
        vars: &[Ty],
        explicit_substs: &crate::Substs,
        sig: FnSig,
        arg_tys: &[Ty],
    ) -> FnSig {
        let n = vars.len();
        let const_mask = self
            .fn_generic_const_mask
            .get(&def)
            .cloned()
            .unwrap_or_default();
        // Infer each const generic from the array argument whose length
        // names it (`sum_arr([1, 2, 3])` => N = 3), so the substituted
        // `[T; N]` carries the concrete count.
        let mut const_substs: Vec<Option<i128>> = (0..n)
            .map(|i| match explicit_substs.as_slice().get(i) {
                Some(crate::GenericArg::Const(value)) => Some(*value),
                _ => None,
            })
            .collect();
        // An explicit `f::<N>(..)` argument is authoritative: inference fills
        // only the positions the call site left open, so an argument of a
        // different length reports a mismatch against the written `N`.
        for (param, arg_ty) in sig.inputs.iter().zip(arg_tys.iter()) {
            if let Some((idx, value)) = self.infer_array_const_len(*param, *arg_ty)
                && idx < n
                && !matches!(
                    explicit_substs.as_slice().get(idx),
                    Some(crate::GenericArg::Const(_))
                )
            {
                const_substs[idx] = Some(value);
            }
        }
        // A const-generic array return (`-> [T; N]`) is carried as a runtime
        // GosVec - the same representation as the by-value `[T; N]` parameter
        // it is derived from. The call-site result type is therefore `Vec<T>`,
        // not the substituted fixed-length array: binding it as `[T; k]` would
        // make the caller read the heap Vec inline and treat the buffer pointer
        // as element 0.
        let output = match self.tcx.kind_of(sig.output) {
            TyKind::Array {
                elem,
                len: crate::ArrayLen::Param(_),
            } => {
                let elem = *elem;
                let elem = self.subst_generics_in_ty(elem, vars, &const_substs);
                self.tcx.intern(TyKind::Vec(elem))
            }
            _ => self.subst_generics_in_ty(sig.output, vars, &const_substs),
        };
        let new_sig = FnSig {
            inputs: sig
                .inputs
                .iter()
                .map(|t| self.subst_generics_in_ty(*t, vars, &const_substs))
                .collect(),
            output,
        };
        // Const positions carry the inferred value; every other position
        // carries its fresh type variable (pinned by argument unification).
        let subst_args: Vec<crate::GenericArg> = (0..n)
            .map(|i| {
                if const_mask.get(i).copied().unwrap_or(false) {
                    crate::GenericArg::Const(const_substs[i].unwrap_or(0))
                } else {
                    crate::GenericArg::Type(vars[i])
                }
            })
            .collect();
        let fndef = self.tcx.intern(TyKind::FnDef {
            def,
            substs: crate::Substs::from_args(subst_args),
        });
        self.record(callee.id, fndef);
        new_sig
    }

    #[allow(
        clippy::cognitive_complexity,
        clippy::too_many_lines,
        reason = "sequential callee-shape dispatch: signature, variant constructor, then stdlib fallbacks"
    )]
    /// Return type for a qualified stdlib path call (`Vec::from`,
    /// `strings::parse`, `String::slice`, …). These have no `FnSig` to unify
    /// against, so each family validates its own argument slots and reports
    /// the type it produces. `None` leaves the call to the generic fallback.
    fn check_qualified_path_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
        resolved: Ty,
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names = self.resolved_value_path_names(callee.id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let Some(last) = last.first().copied() else {
            return Some(self.fresh());
        };
        if matches!(module, ["Vec"] | ["std", "Vec"])
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.check_qualified_vec_call(last, args, arg_tys, callee.span)
        {
            return Some(ret);
        }
        if matches!(module, ["String"] | ["std", "String"])
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.check_qualified_string_call(last, args, arg_tys, callee.span)
        {
            return Some(ret);
        }
        if !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) =
                self.check_qualified_bytes_handle_call(module, last, args, arg_tys, callee.span)
        {
            return Some(ret);
        }
        let is_strings_call = matches!(module, ["strings"] | ["std", "strings"]);
        let has_specialized_combinator_sig = combinator_module_name(module)
            .is_some_and(|m| Self::std_combinator_arity(m, last).is_some());
        if !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. })) {
            self.check_stdlib_signature_arity(
                module,
                last,
                args.len(),
                usize::from(self.pipe_stage_callees.contains(&callee.id)),
                callee.span,
            );
            // `strings::*` has a dedicated validator below. Running the
            // generic signature catalogue too reports the same bad slot
            // twice, once without its parameter name.
            if !is_strings_call && !has_specialized_combinator_sig {
                self.check_stdlib_signature_args(module, last, args, arg_tys);
            }
        }
        // `strings::` free functions have no `FnSig` to unify
        // against, so validate their string-typed argument slots
        // here. Skipped when the callee resolves to a user `FnDef`
        // (a user module named `strings` keeps its own typing) or
        // when the value is piped in (`|>` appends the data argument
        // during lowering, shifting the positions this table keys
        // on).
        if is_strings_call
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && !self.pipe_stage_callees.contains(&callee.id)
        {
            self.check_strings_free_call_args(last, args, arg_tys, callee.span);
        }
        if is_strings_call
            && last == "parse"
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
        {
            // The free form is the same operation as the method, so it takes
            // the same one parse surface.
            let suggestion = self.string_parse_replacement(expected);
            self.emit(TypeError::StringParseRetired { suggestion }, callee.span);
            let generics = path
                .segments
                .last()
                .map_or(&[][..], |segment| segment.generics.as_slice());
            return Some(self.string_parse_ret("strings::parse", generics, expected, callee.span));
        }
        if matches!(module, ["String"] | ["std", "String"])
            && last == "slice"
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
        {
            let s = self.tcx.string_ty();
            let err = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(s, err));
        }
        if matches!(module, ["String"] | ["std", "String"])
            && matches!(last, "from" | "new" | "with_capacity")
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
        {
            return Some(self.tcx.string_ty());
        }
        if module.is_empty()
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.raw_stdlib_helper_ret(last)
        {
            return Some(ret);
        }
        // Data-last std combinators (`result::map_err(f, r)`,
        // `iter::map(f, xs)`, ...): the signature table pins
        // closure params to the data payload type. Gated on the
        // callee not being a resolved user `FnDef` so a user
        // module that happens to be named `iter` / `result` /
        // `option` keeps its own typing.
        if !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.check_std_combinator_free_call(
                callee,
                args,
                arg_tys,
                combinator_module_name(module),
                last,
            )
        {
            return Some(ret);
        }
        // `archive::tar::write` / `archive::zip::write` take
        // `[(String, [u8])]` and return `Result<[u8], Error>`.
        // These are stdlib (no `fn_sig`), so re-type the literal
        // argument against the synthesized parameter type so a
        // `[("a", [1, 2, 3])]` literal builds heap Vecs at every
        // level on the compiled tier.
        if last == "write"
            && matches!(module.last().copied(), Some("tar" | "zip"))
            && args.len() == 1
        {
            // The `[(String, [u8])]` argument shape flowed in via
            // `call_arg_expectations`; only the return type is
            // synthesized here.
            let u8_ty = self.tcx.int_ty(IntTy::U8);
            let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
            let e = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(vec_u8, e));
        }
        if let Some(ty) =
            self.check_stdlib_module_ret_ty(module, last, callee, args, arg_tys, expected)
        {
            return Some(ty);
        }
        if let Some(ty) = self.stdlib_signature_return_ty(module, last) {
            return Some(ty);
        }
        // The IEEE-754 reinterpretations are associated functions on a
        // primitive rather than module members, so they carry their
        // contract here: the bit pattern is the unsigned integer of the
        // float's own width, in both directions.
        if let Some(ty) = self.float_bits_assoc_ret(module, last) {
            return Some(ty);
        }
        // `String::from_utf8` is an associated function on a primitive
        // rather than a module member, so it has no catalogue row; pin
        // its `Result` here or `?` sees an unresolved variable.
        if module == ["String"] && last == "from_utf8" {
            let string_ty = self.tcx.string_ty();
            let err = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(string_ty, err));
        }
        // Built-in intrinsics emitted by the parser's macro
        // expansion (`format!` only - `println!` / `print!` /
        // `eprintln!` / `eprint!` etc. expand to a call to the
        // outer name with the format-built string as the single
        // argument, and pinning `println` to Unit broke
        // generic-monomorph paths that route through user-named
        // functions called `println`). Pinning `__concat` and
        // `__fmt_prec` to `String` is safe: they're synthetic
        // names the parser injects and no user code can
        // shadow them.
        if module.is_empty()
            && let Some(ty) = self.check_bare_intrinsic_call(last, arg_tys, callee.span)
        {
            return Some(ty);
        }
        None
    }

    /// Checks one call against the callee's known signature: instantiates a
    /// generic function's rigid parameter slots per call site, unifies each
    /// argument, and reports an arity mismatch. Returns the call's type when
    /// the signature determines it.
    fn check_call_against_sig(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        mut sig: FnSig,
        callee_item: Option<(gossamer_resolve::DefId, crate::Substs)>,
    ) -> Option<Ty> {
        // Per-call-site instantiation of a generic function: replace
        // the signature's rigid `Param` slots with one fresh inference
        // variable each, so independent call sites bind the parameters
        // independently (without this, the second call with a different
        // concrete type fails to unify against the first's binding).
        let inst: Option<(gossamer_resolve::DefId, Vec<Ty>, crate::Substs)> =
            callee_item.and_then(|(def, explicit)| {
                let n = self.fn_generic_arity.get(&def).copied()?;
                if n == 0 {
                    return None;
                }
                if !explicit.is_empty() && explicit.len() != n {
                    self.emit(
                        TypeError::CallArityMismatch {
                            callee: format!("{} generic arguments", callee_display_name(callee)),
                            expected: n,
                            found: explicit.len(),
                        },
                        callee.span,
                    );
                }
                let const_mask = self
                    .fn_generic_const_mask
                    .get(&def)
                    .cloned()
                    .unwrap_or_default();
                let vars = (0..n)
                    .map(|i| {
                        if const_mask.get(i).copied().unwrap_or(false) {
                            self.fresh()
                        } else {
                            match explicit.as_slice().get(i) {
                                Some(crate::GenericArg::Type(ty)) => *ty,
                                _ => self.fresh(),
                            }
                        }
                    })
                    .collect();
                Some((def, vars, explicit))
            });
        if let Some((def, vars, explicit)) = &inst {
            sig = self.instantiate_generic_sig(callee, *def, vars, explicit, sig, arg_tys);
        }
        let pipe_extra = usize::from(self.pipe_stage_callees.contains(&callee.id));
        let effective = arg_tys.len() + pipe_extra;
        if effective == sig.inputs.len() {
            for (param, (arg_ty, arg_expr)) in sig.inputs.iter().zip(arg_tys.iter().zip(args)) {
                self.check_sig_param_arg(*param, *arg_ty, arg_expr);
            }
            if let Some((def, vars, _)) = &inst {
                self.check_trait_bounds(*def, vars, callee.span);
            }
            return Some(sig.output);
        }
        // A known callee signature whose declared arity does not
        // match the call: the VM aborts (`CallArityMismatch` in the
        // MIR verifier) and the native backend silently drops or
        // zero-fills the surplus/missing arguments. Reject it
        // statically so `check` is never looser than the tiers. A
        // call on the right of `|>` receives the piped value as an
        // implicit trailing argument, so count it toward the arity.
        if effective != sig.inputs.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(callee),
                    expected: sig.inputs.len(),
                    found: effective,
                },
                callee.span,
            );
        }
        // Fall through to the existing stdlib / fresh handling so a
        // pipe-stage call keeps its current return typing.
        None
    }

    /// Return type for the call shapes that name a user item rather than a
    /// function value: an `impl`'s associated function, a reverse
    /// constructor, a tuple or named struct literal, and an enum variant.
    /// The first shape that recognises the callee decides the type.
    fn check_constructor_like_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        self.check_user_assoc_fn_call(callee, args)
            .or_else(|| self.check_reverse_ctor_call(callee, args, arg_tys))
            .or_else(|| self.check_tuple_struct_ctor_call(callee, args, arg_tys))
            .or_else(|| self.check_named_struct_ctor_call(callee, args))
            .or_else(|| self.check_enum_variant_ctor_call(callee, args, arg_tys))
    }

    /// Return type of `Type::assoc(..)` for a user `impl`'s associated
    /// function. Without it the call's result is a fresh variable, so a
    /// `-> Self` constructor produces an untyped value and every method
    /// call on it - including its argument types - goes unchecked.
    fn check_user_assoc_fn_call(&mut self, callee: &Expr, args: &[Expr]) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let segments: Vec<&str> = path
            .segments
            .iter()
            .map(|seg| seg.name.name.as_str())
            .collect();
        let [owner @ .., fn_name] = segments.as_slice() else {
            return None;
        };
        if owner.is_empty() {
            return None;
        }
        // A type reached through its module (`lib::Point::new`) is keyed
        // by the identity it registers under, so try the written path
        // before the bare name two modules could share.
        let type_name = self
            .owner_identity_candidates(owner)
            .into_iter()
            .find(|candidate| self.user_type_decls.contains(candidate))?;
        // An associated function carries its own visibility, the same as a
        // method: `Type::helper()` reached from outside the module the
        // `impl` was written in is private unless it says `pub`.
        self.reject_private_method(&type_name, fn_name, callee.span);
        self.method_ret_types
            .get(&(type_name, (*fn_name).to_string(), args.len()))
            .copied()
    }

    fn check_call_inner(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        callee_ty: Ty,
        arg_tys: &[Ty],
        expected: Expectation,
    ) -> Ty {
        let resolved = self.infer.resolve(self.tcx, callee_ty);
        let kind = self.tcx.kind(resolved).cloned();
        // Recognised callee shapes: `FnPtr` (anonymous or first-class
        // closure pointer) and `FnDef { def, .. }` (named function
        // resolved to a definition). Looking the def up in
        // `fn_sigs` lets cross-function call sites pin both args and
        // return type to the callee's signature instead of returning
        // a fresh inference variable that never gets bound.
        let callee_item = match self.tcx.kind(resolved) {
            Some(TyKind::FnDef { def, substs }) => Some((*def, substs.clone())),
            _ => None,
        };
        let sig_lookup: Option<FnSig> = match kind {
            Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) => Some(sig),
            Some(TyKind::FnDef { def, .. }) => self.fn_sigs.get(&def).cloned(),
            _ => None,
        };
        if let Some(sig) = sig_lookup
            && let Some(ty) = self.check_call_against_sig(callee, args, arg_tys, sig, callee_item)
        {
            return ty;
        }
        if let Some(ty) = self.check_constructor_like_call(callee, args, arg_tys) {
            return ty;
        }
        // Fallback: known stdlib free functions whose signatures are
        // not present in `fn_sigs` (because they live outside user
        // source). Returning a real type instead of a fresh variable
        // lets the type checker catch mismatches such as returning
        // `Result<json::Value, String>` from a function declared
        // `Result<ComicResponse, String>`.
        if let Some(ty) = self.check_qualified_path_call(callee, args, arg_tys, expected, resolved)
        {
            return ty;
        }
        self.reject_noncallable_callee(callee, callee_ty);
        self.fresh()
    }

    /// Validates Rust-style associated String mutators such as
    /// `String::push(&mut s, ch)`. These calls do not pass through method-call
    /// checking and are not stdlib module functions, so without this table a
    /// malformed argument can reach a permissive runtime builtin and become a
    /// silent no-op.
    fn check_qualified_string_call(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let string = self.tcx.string_ty();
        let receiver = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: string,
        });
        let params = match method {
            "clear" => vec![receiver],
            "push" | "push_char" => {
                vec![receiver, self.tcx.intern(TyKind::Char)]
            }
            "push_str" => vec![receiver, string],
            "push_byte" | "truncate" => {
                vec![receiver, self.tcx.int_ty(IntTy::I64)]
            }
            "push_utf8" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                let bytes = self.tcx.intern(TyKind::Vec(u8_ty));
                let idx = self.tcx.int_ty(IntTy::I64);
                vec![receiver, bytes, idx, idx]
            }
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("String::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(self.tcx.unit())
    }

    /// Qualified Vec counterpart of method-call checking. Rust-style UFCS
    /// calls bypass `vec_method_ret`, so validate the complete receiver and
    /// argument contract here instead of allowing runtime builtins to accept
    /// mixed element types.
    fn check_qualified_vec_call(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let actual_receiver = arg_tys.first().copied();
        let elem = actual_receiver
            .map(|ty| self.infer.resolve(self.tcx, ty))
            .map(|ty| self.peel_refs(ty))
            .and_then(|ty| match self.tcx.kind(ty) {
                Some(TyKind::Vec(elem)) => Some(*elem),
                _ => None,
            })
            .unwrap_or_else(|| self.fresh());
        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
        let shared = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: vec_ty,
        });
        let mutable = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: vec_ty,
        });
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let error_ty = self.tcx.dyn_error_ty();
        let unit_ty = self.tcx.unit();
        let vec_result_ty = self.tcx.intern(TyKind::Vec(elem));
        let (params, ret) = match method {
            "push" => (vec![mutable, elem], unit_ty),
            "insert" => (
                vec![mutable, i64_ty, elem],
                self.result_adt_ty(unit_ty, error_ty),
            ),
            "remove" => (vec![mutable, i64_ty], self.result_adt_ty(elem, error_ty)),
            "sort" | "reverse" => (vec![mutable], self.tcx.unit()),
            "fill" => (vec![mutable, elem], self.tcx.unit()),
            "swap" => (vec![mutable, i64_ty, i64_ty], self.tcx.unit()),
            "slice" => (
                vec![shared, i64_ty, i64_ty],
                self.result_adt_ty(vec_result_ty, error_ty),
            ),
            "first" | "last" => (vec![shared], self.option_adt_ty(elem)),
            "rev" => (vec![shared], self.tcx.intern(TyKind::Vec(elem))),
            "index_of" => (vec![shared, elem], self.option_adt_ty(i64_ty)),
            "count_of" => (vec![shared, elem], i64_ty),
            "contains" => (vec![shared, elem], self.tcx.bool_ty()),
            "len" => (vec![shared], i64_ty),
            // Closure typing is handled by the combinator path for method
            // syntax. Still enforce the qualified form's receiver and arity.
            "sort_by" => (vec![mutable, self.fresh()], self.tcx.unit()),
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("Vec::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    fn check_qualified_bytes_handle_call(
        &mut self,
        module: &[&str],
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let owner = match module {
            ["Buffer"] | ["bytes", "Buffer"] | ["std", "bytes", "Buffer"] => "bytes::Buffer",
            ["Builder"] | ["bytes", "Builder"] | ["std", "bytes", "Builder"] => "bytes::Builder",
            _ => return None,
        };
        let handle = self.bytes_handle_ty(owner);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let string = self.tcx.string_ty();
        let mutable = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: handle,
        });
        let shared = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: handle,
        });
        let (params, ret) = match (owner, method) {
            (_, "new") => (vec![], handle),
            (_, "with_capacity") => (vec![i64_ty], handle),
            ("bytes::Buffer", "push") => (vec![mutable, u8_ty], self.tcx.unit()),
            ("bytes::Buffer", "write_str") => (vec![mutable, string], self.tcx.unit()),
            ("bytes::Buffer", "clear") => (vec![mutable], self.tcx.unit()),
            ("bytes::Buffer", "len") => (vec![shared], i64_ty),
            ("bytes::Buffer", "is_empty") => (vec![shared], self.tcx.bool_ty()),
            ("bytes::Buffer", "to_string") => (vec![shared], string),
            ("bytes::Builder", "write") => (vec![mutable, string], self.tcx.unit()),
            ("bytes::Builder", "write_char") => (
                vec![mutable, self.tcx.intern(TyKind::Char)],
                self.tcx.unit(),
            ),
            ("bytes::Builder", "len") => (vec![shared], i64_ty),
            ("bytes::Builder", "build" | "as_str") => (vec![shared], string),
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    fn check_tuple_struct_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let Some(Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Struct,
        }) = self.resolutions.get(callee.id)
        else {
            return None;
        };
        if !self.tcx.is_tuple_struct(def.local) {
            return None;
        }
        let called_name = path.segments.last()?.name.name.as_str();
        // A type's registered name carries the modules containing it, so
        // compare the written leaf against the identity's leaf.
        let identity = self.tcx.def_name(def)?;
        if identity.rsplit("::").next() != Some(called_name) {
            return None;
        }
        let fields = self.struct_fields.get(&def)?.clone();
        let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
        let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
        self.defer_adt_bounds(def, &substs, callee.span);
        if fields.len() == arg_tys.len() {
            for ((_, field_ty), (arg_ty, arg_expr)) in fields.iter().zip(arg_tys.iter().zip(args)) {
                let field_ty = self.subst_params_in_ty(*field_ty, &substs);
                self.check_sig_param_arg(field_ty, *arg_ty, arg_expr);
            }
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(callee),
                    expected: fields.len(),
                    found: arg_tys.len(),
                },
                callee.span,
            );
        }
        Some(self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::from_types(substs.iter().copied()),
        }))
    }

    fn check_named_struct_ctor_call(&mut self, callee: &Expr, args: &[Expr]) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let Some(Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Struct,
        }) = self.resolutions.get(callee.id)
        else {
            return None;
        };
        if self.tcx.is_tuple_struct(def.local) {
            return None;
        }
        let called_name = path.segments.last()?.name.name.as_str();
        // A type's registered name carries the modules containing it, so
        // compare the written leaf against the identity's leaf.
        if self.tcx.def_name(def)?.rsplit("::").next() != Some(called_name) {
            return None;
        }
        let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
        let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
        let name = self
            .tcx
            .def_name(def)
            .map_or_else(|| called_name.to_string(), ToString::to_string);
        self.emit(
            TypeError::StructConstructorBracesRequired { name },
            callee.span,
        );
        for arg in args {
            self.check_expr(arg);
        }
        Some(self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::from_types(substs.iter().copied()),
        }))
    }

    fn check_reverse_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        if path.segments.len() != 1 || path.segments[0].name.name != "Reverse" {
            return None;
        }
        if args.len() != 1 {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: "Reverse".to_string(),
                    expected: 1,
                    found: args.len(),
                },
                callee.span,
            );
            return Some(self.tcx.error_ty());
        }
        let elem = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
        Some(self.reverse_ty(elem))
    }

    /// Validates a user enum's tuple-variant constructor. Payload expectations
    /// shape collection literals before this point, but shaping alone is not a
    /// type check. Every supplied payload must still unify with its declared
    /// slot, and the constructor's result remains the nominal enum type.
    fn check_enum_variant_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let n = path.segments.len();
        if n < 2 {
            return None;
        }
        let enum_name = path.segments[n - 2].name.name.clone();
        let variant_name = path.segments[n - 1].name.name.clone();
        let payloads = self
            .enum_variant_payloads
            .get(&(enum_name.clone(), variant_name))?
            .clone();
        // A generic enum's parameters are instantiated once per call site;
        // the payload checks below and the type this call produces read the
        // same variables, so an argument pins the enum's own arguments.
        let instantiation = self.variant_ctor_instantiation(callee.id, &enum_name);
        let payloads: Vec<Ty> = match &instantiation {
            Some((_, substs)) => payloads
                .iter()
                .map(|t| self.subst_params_in_ty(*t, substs))
                .collect(),
            None => payloads,
        };
        if payloads.len() == arg_tys.len() {
            for (param, (arg_ty, arg)) in payloads.iter().zip(arg_tys.iter().zip(args)) {
                self.check_sig_param_arg(*param, *arg_ty, arg);
            }
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(callee),
                    expected: payloads.len(),
                    found: arg_tys.len(),
                },
                callee.span,
            );
        }
        if let Some((def, substs)) = instantiation {
            return Some(self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::from_types(substs.iter().copied()),
            }));
        }
        Some(
            self.enum_tys
                .get(&enum_name)
                .copied()
                .unwrap_or_else(|| self.fresh()),
        )
    }

    /// After a call failed every resolution path: if the callee is a
    /// concrete, fully-known value that can never be a function or an ADT
    /// constructor, reject it (`GT0022`) - the compiled tier would emit a
    /// call through a non-function symbol. A qualified path callee
    /// (`String::new`) types loosely as the receiving type and is never
    /// flagged; an inference-var callee (e.g. an unsuffixed `let x = 5`)
    /// is deferred for re-check after defaulting.
    fn reject_noncallable_callee(&mut self, callee: &Expr, callee_ty: Ty) {
        let resolved_callee = self.infer.resolve(self.tcx, callee_ty);
        let callee_kind = self.tcx.kind_of(resolved_callee).clone();
        let qualified_path_callee =
            matches!(&callee.kind, ExprKind::Path(p) if p.segments.len() >= 2);
        if matches!(callee_kind, TyKind::Var(_)) {
            self.deferred_structural.push(DeferredStructural {
                ty: resolved_callee,
                span: callee.span,
                kind: DeferredStructuralKind::Call,
                result: None,
            });
        } else if is_definitely_not_callable_value(&callee_kind) && !qualified_path_callee {
            let ty = self.render_public_ty(resolved_callee);
            self.emit(TypeError::NotCallable { ty }, callee.span);
        }
    }

    /// Concrete return type of a stdlib `json` / `errors` / `fs` / `os`
    /// free call whose signature lives outside user source. Returning a
    /// real type (rather than a fresh inference var) lets the checker
    /// catch mismatches - e.g. matching `fs::file_size(p)`'s bare `i64`
    /// against a `Result` pattern. The json accessors return `Option<T>`
    /// at runtime (the interp emits `Some`/`None`), so they are typed as
    /// such; `json::get(v, k).unwrap()` and the autoderive `Some`/`None`
    /// matches both rely on it.
    /// Return type of a qualified `HashMap::pop(m, k)` / `HashMap::get(m, k)`
    /// free-fn call. The method form (`m.pop(k)`) is typed by
    /// `check_method_call` from the receiver's static type; the qualified form
    /// must recover the value type from the first argument's map type so the
    /// `Option<V>` payload binding is the concrete value rather than an
    /// unresolved var - a struct field read on an unresolved payload lowers to
    /// the dynamic json accessor and faults at runtime.
    fn check_qualified_map_accessor_ret(
        &mut self,
        module: &[&str],
        last: &str,
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        if !matches!(
            module,
            ["Map"] | ["collections", "Map"] | ["std", "collections", "Map"]
        ) || !matches!(last, "pop" | "get" | "insert" | "remove")
        {
            return None;
        }
        let value = arg_tys.first().and_then(|t| {
            let resolved = self.infer.resolve(self.tcx, *t);
            let peeled = match self.tcx.kind(resolved) {
                Some(TyKind::Ref { inner, .. }) => self.infer.resolve(self.tcx, *inner),
                _ => resolved,
            };
            match self.tcx.kind(peeled) {
                Some(TyKind::HashMap { value, .. }) => Some(*value),
                _ => None,
            }
        });
        value.map(|v| self.option_adt_ty(v))
    }

    /// Rejects a non-string argument in a string-typed parameter slot
    /// of a `strings::` free-function call. The stdlib free path has no
    /// `FnSig` to unify against, so without this the checker accepts an
    /// integer where a `String` is expected and the compiled string
    /// shims dereference it as a pointer (a SIGSEGV the VM masks).
    fn check_strings_free_call_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        _callee_span: Span,
    ) {
        let Some(shapes) = strings_fn_str_params(name) else {
            return;
        };
        for &(idx, shape) in shapes {
            let (Some(arg), Some(&arg_ty)) = (args.get(idx), arg_tys.get(idx)) else {
                continue;
            };
            let meta = strings_fn_param_metadata(name, idx, shape);
            self.check_str_param_arg(
                shape,
                arg,
                arg_ty,
                arg.span,
                &format!("strings::{name}"),
                meta,
            );
        }
        self.check_strings_int_args(name, args, arg_tys, 0);
        self.check_strings_char_args(name, args, arg_tys, 0);
    }

    /// Unifies one call argument against its declared parameter type.
    /// Shared references retain the language's read-only convenience
    /// coercion, but `&mut T` is never created implicitly: mutation must
    /// be visible as `&mut place` or arrive through an existing `&mut T`.
    fn check_sig_param_arg(&mut self, param: Ty, arg_ty: Ty, arg: &Expr) {
        let param = self.infer.resolve(self.tcx, param);
        let arg_ty = self.infer.resolve(self.tcx, arg_ty);
        let param_ref = match self.tcx.kind(param) {
            Some(TyKind::Ref { inner, mutability }) => Some((*inner, *mutability)),
            _ => None,
        };
        let arg_ref = match self.tcx.kind(arg_ty) {
            Some(TyKind::Ref { inner, mutability }) => Some((*inner, *mutability)),
            _ => None,
        };
        let (lhs, rhs) = match (param_ref, arg_ref) {
            (Some((_p, Mutbl::Mut)), Some((_a, Mutbl::Mut))) => (param, arg_ty),
            (Some((_p, Mutbl::Mut)), Some((a, Mutbl::Not))) => (
                param,
                self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Not,
                    inner: a,
                }),
            ),
            (Some((p, Mutbl::Mut)), None) => {
                // An argument whose own type already failed carries no
                // evidence about how it was passed; a second report here
                // would describe the wrong thing.
                if !matches!(self.tcx.kind(arg_ty), Some(TyKind::Error)) {
                    self.emit(
                        TypeError::MutableArgumentRequiresReference {
                            argument: Self::place_display(arg),
                        },
                        arg.span,
                    );
                }
                (p, arg_ty)
            }
            // A `[T]` view parameter keeps its reference here: the sequence
            // reaching it is an array, a `Vec`, or another view, and the
            // unsizing that accepts all three is decided against the view.
            (Some((p, Mutbl::Not)), None) => {
                if matches!(self.tcx.kind(p), Some(TyKind::Slice(_))) {
                    (param, arg_ty)
                } else {
                    (p, arg_ty)
                }
            }
            // A reference reaching a value parameter is the value it names,
            // but nothing at the call says so: the compiled tiers hand the
            // callee the address while the bytecode VM hands it the value.
            // The deref is written.
            (None, Some((a, _))) => {
                if !matches!(
                    self.tcx.kind(param),
                    Some(TyKind::Error | TyKind::Var(_) | TyKind::Param { .. })
                ) && !matches!(self.tcx.kind(arg_ty), Some(TyKind::Error))
                    && !matches!(
                        arg.kind,
                        ExprKind::Unary {
                            op: UnaryOp::RefMut,
                            ..
                        }
                    )
                {
                    self.emit(
                        TypeError::ReferenceArgumentNeedsDeref {
                            argument: Self::place_display(arg),
                        },
                        arg.span,
                    );
                }
                (param, a)
            }
            _ => (param, arg_ty),
        };
        // Render an unsuffixed float literal as its default source type in a
        // String slot. The unifier also rejects the constraint, but this
        // parameter-specific path identifies the bad argument.
        if matches!(
            self.tcx.kind(self.infer.resolve(self.tcx, lhs)),
            Some(TyKind::String)
        ) && self.infer.is_float_literal_var(self.tcx, rhs)
        {
            self.emit_str_slot_mismatch("f64", arg.span);
        } else {
            self.unify(lhs, rhs, arg.span);
        }
    }

    /// Validates the string-typed arguments of a `String` method call
    /// (`s.contains(x)`). The method dispatches to the same `strings::`
    /// shim as the free function with the receiver as the implicit first
    /// argument, so the explicit args occupy parameter positions 1..
    fn check_strings_method_call_args(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        receiver_span: Span,
    ) {
        self.check_strings_arity(method, args.len(), 1, receiver_span);
        let Some(shapes) = strings_fn_str_params(method) else {
            return;
        };
        for &(pos, shape) in shapes {
            // Position 0 is the receiver, already known to be a `String`.
            let Some(idx) = pos.checked_sub(1) else {
                continue;
            };
            let (Some(arg), Some(&arg_ty)) = (args.get(idx), arg_tys.get(idx)) else {
                continue;
            };
            let meta = strings_fn_param_metadata(method, pos, shape);
            self.check_str_param_arg(
                shape,
                arg,
                arg_ty,
                arg.span,
                &format!("String::{method}"),
                meta,
            );
        }
        self.check_strings_int_args(method, args, arg_tys, 1);
        self.check_strings_char_args(method, args, arg_tys, 1);
    }

    /// Validates the complete fixed arity of a known string operation. The
    /// runtime shims historically treated omitted arguments as zero/empty,
    /// which turned a source error such as `s.slice(1)` into a misleading
    /// range operation. Keep this beside the string-slot checks so free and
    /// method syntax share exactly one contract.
    fn check_strings_arity(
        &mut self,
        name: &str,
        supplied: usize,
        implicit_receiver: usize,
        span: Span,
    ) {
        let Some(total) = strings_fn_arity(name) else {
            return;
        };
        let expected = total.saturating_sub(implicit_receiver);
        if supplied != expected {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("strings::{name}"),
                    expected,
                    found: supplied,
                },
                span,
            );
        }
    }

    /// Validates integer slots in the same string operation catalogue. Unlike
    /// string slots, these are all specified as `i64`; this catches ranges,
    /// strings, and floats rather than letting runtime helpers coerce them to
    /// zero.
    fn check_strings_int_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        implicit_receiver: usize,
    ) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        for &position in strings_fn_int_params(name) {
            let Some(index) = position.checked_sub(implicit_receiver) else {
                continue;
            };
            let (Some(arg), Some(&arg_ty)) = (args.get(index), arg_tys.get(index)) else {
                continue;
            };
            self.check_sig_param_arg(i64_ty, arg_ty, arg);
        }
    }

    /// Validates char-only slots such as `pad_left(text, width, fill)`.
    fn check_strings_char_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        implicit_receiver: usize,
    ) {
        let char_ty = self.tcx.char_ty();
        for &position in strings_fn_char_params(name) {
            let Some(index) = position.checked_sub(implicit_receiver) else {
                continue;
            };
            let (Some(arg), Some(&arg_ty)) = (args.get(index), arg_tys.get(index)) else {
                continue;
            };
            self.check_sig_param_arg(char_ty, arg_ty, arg);
        }
    }

    /// Validates one argument against a string-shaped parameter slot.
    fn check_str_param_arg(
        &mut self,
        shape: StrArgShape,
        arg: &Expr,
        arg_ty: Ty,
        span: Span,
        callee: &str,
        param: StringParamMeta,
    ) {
        // `&"hi"` (a `Ref<String>`) is layout-transparent to its inner
        // `String` at every call boundary; validate the referent.
        let resolved = self.infer.resolve(self.tcx, arg_ty);
        let inner = match self.tcx.kind(resolved) {
            Some(TyKind::Ref { inner, .. }) => *inner,
            _ => resolved,
        };
        // Catch unsuffixed numeric literals up front in either slot shape so
        // a `5` / `1.5` in a string position is rejected with the same
        // `i64` / `f64` spelling used by every other type diagnostic.
        if self.infer.is_integer_constrained_var(self.tcx, inner) {
            self.emit_named_str_slot_mismatch(callee, param.name, param.expected, "i64", arg, span);
            return;
        }
        if self.infer.is_float_literal_var(self.tcx, inner) {
            self.emit_named_str_slot_mismatch(callee, param.name, param.expected, "f64", arg, span);
            return;
        }
        match shape {
            StrArgShape::Str => {
                // A `String` slot admits only a real string. Keep an
                // unresolved inference variable unifiable for valid generic
                // expressions, but report every concrete wrong shape with
                // the parameter name instead of a context-free mismatch.
                let r = self.infer.resolve(self.tcx, inner);
                if matches!(self.tcx.kind(r), Some(TyKind::String)) {
                    return;
                }
                if matches!(self.tcx.kind(r), Some(TyKind::Var(_))) {
                    let s = self.tcx.string_ty();
                    self.unify(s, inner, span);
                } else if self.tcx.kind(r).is_some() {
                    self.emit_named_str_slot_mismatch(
                        callee,
                        param.name,
                        param.expected,
                        &string_argument_found_type(arg, self.tcx, r),
                        arg,
                        span,
                    );
                } else {
                    let s = self.tcx.string_ty();
                    self.unify(s, inner, span);
                }
            }
            StrArgShape::StrOrChar => {
                // A pattern slot also admits a `char`, so the
                // unifier (single expected type) is too strict. Report every
                // non-string / non-char shape through the named argument path.
                let r = self.infer.resolve(self.tcx, inner);
                if matches!(self.tcx.kind(r), Some(TyKind::Var(_))) {
                    return;
                }
                if !matches!(self.tcx.kind(r), Some(TyKind::String | TyKind::Char)) {
                    self.emit_named_str_slot_mismatch(
                        callee,
                        param.name,
                        param.expected,
                        &string_argument_found_type(arg, self.tcx, r),
                        arg,
                        span,
                    );
                }
            }
        }
    }

    fn emit_str_slot_mismatch(&mut self, found: &str, span: Span) {
        self.emit(
            TypeError::TypeMismatch {
                expected: "String".to_string(),
                found: found.to_string(),
            },
            span,
        );
    }

    fn emit_named_str_slot_mismatch(
        &mut self,
        callee: &str,
        parameter: &str,
        expected: &str,
        found: &str,
        arg: &Expr,
        span: Span,
    ) {
        self.emit(
            TypeError::ArgumentTypeMismatch {
                callee: callee.to_string(),
                parameter: parameter.to_string(),
                expected: expected.to_string(),
                found: found.to_string(),
                actual: argument_value_display(arg),
            },
            span,
        );
    }

    /// Result type of an `fs::` / `os::` free call, or `None` for the
    /// unlisted surface. Typed reads keep the `?`-unwrapped payload
    /// concrete (`fs::read_to_string(p)?.to_lowercase()` stays `String`
    /// into codegen), and directory walks yield the `fs::DirInfo`
    /// sentinel whose field layout is pre-registered so `e.path` /
    /// `e.size` on the entries stay concretely typed.
    fn fs_call_ret_ty(&mut self, last: &str) -> Option<Ty> {
        match last {
            "file_size" => Some(self.tcx.int_ty(IntTy::I64)),
            "exists" | "is_file" | "is_dir" | "is_symlink" => Some(self.tcx.bool_ty()),
            "read_to_string" | "read_file_to_string" => {
                let s = self.tcx.string_ty();
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(s, e))
            }
            "read" | "read_file" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                let v = self.tcx.intern(TyKind::Vec(u8_ty));
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(v, e))
            }
            // `walk_dir` is the visiting form and is typed from its declared
            // signature, so only the listing call is pinned here.
            "read_dir" => {
                let def = gossamer_resolve::DefId::local(u32::MAX - 2);
                self.tcx.register_def_name(def, "DirInfo");
                let entry = self.tcx.intern(TyKind::Adt {
                    def,
                    substs: crate::Substs::new(),
                });
                let v = self.tcx.intern(TyKind::Vec(entry));
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(v, e))
            }
            _ => None,
        }
    }

    /// Result type of a `Collection::new()` constructor call, or `None`
    /// for a non-collection path. An unannotated `let m = HashMap::new()`
    /// grounds to a real `TyKind::HashMap` (generics pinned by the first
    /// `insert` / `get`, see `map_method_ret`) so method dispatch reaches
    /// the properly keyed runtime symbol on every tier; `VecDeque` /
    /// `HashSet` ground to the same sentinel Adts their annotations
    /// resolve to.
    fn collection_ctor_ty(&mut self, module: &[&str]) -> Option<Ty> {
        let tail = match module {
            [t] | ["collections", t] | ["std", "collections", t] => *t,
            _ => return None,
        };
        match tail {
            "Vec" => {
                let elem = self.fresh();
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            "Deque" => {
                let elem = self.fresh();
                Some(self.vecdeque_ty(elem))
            }
            "Queue" => {
                let elem = self.fresh();
                Some(self.vecqueue_ty(elem))
            }
            "Stack" => {
                let elem = self.fresh();
                Some(self.vecstack_ty(elem))
            }
            "MaxHeap" => {
                let elem = self.fresh();
                Some(self.binary_heap_ty(elem))
            }
            "MinHeap" => {
                let elem = self.fresh();
                Some(self.min_heap_ty(elem))
            }
            "Map" => {
                let key = self.fresh();
                let value = self.fresh();
                Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: false,
                }))
            }
            "BTreeMap" => {
                let key = self.fresh();
                let value = self.fresh();
                Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: true,
                }))
            }
            "Set" | "BTreeSet" => {
                let elem = self.fresh();
                Some(self.set_ty(tail, elem))
            }
            _ => None,
        }
    }

    fn collection_from_ty(
        &mut self,
        module: &[&str],
        source: Ty,
        expected: Expectation,
        span: Span,
    ) -> Option<Ty> {
        let owner = match module {
            [owner] | ["collections", owner] | ["std", "collections", owner] => *owner,
            _ => return None,
        };
        let source = self.infer.resolve(self.tcx, source);
        let array_source_elem = |checker: &mut Self| {
            if let Some(TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem)) =
                checker.tcx.kind(source)
            {
                *elem
            } else {
                let found = checker.render_public_ty(source);
                checker.emit(
                    TypeError::TypeMismatch {
                        expected: "array, slice, or Vec".to_string(),
                        found,
                    },
                    span,
                );
                checker.fresh()
            }
        };
        match owner {
            "Vec" => {
                let elem = array_source_elem(self);
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            "Set" | "BTreeSet" => {
                let elem = array_source_elem(self);
                if owner == "BTreeSet" {
                    self.require_container_reads_the_types_order(elem, owner, span);
                }
                Some(self.set_ty(owner, elem))
            }
            "Deque" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.vecdeque_ty(elem))
            }
            "Queue" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.vecqueue_ty(elem))
            }
            "Stack" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.vecstack_ty(elem))
            }
            "MaxHeap" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.binary_heap_ty(elem))
            }
            "MinHeap" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.min_heap_ty(elem))
            }
            "Map" | "BTreeMap" => {
                let (key, value) = if let Some(
                    TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem),
                ) = self.tcx.kind(source)
                {
                    match self.tcx.kind(*elem) {
                        Some(TyKind::Tuple(parts)) if parts.len() == 2 => (parts[0], parts[1]),
                        Some(TyKind::Var(_)) => {
                            let target = self.expectation_target(expected)?;
                            match self.tcx.kind(target) {
                                Some(TyKind::HashMap { key, value, .. }) => (*key, *value),
                                _ => return None,
                            }
                        }
                        _ => return None,
                    }
                } else {
                    let found = self.render_public_ty(source);
                    self.emit(
                        TypeError::TypeMismatch {
                            expected: "fixed array of key-value tuples".to_string(),
                            found,
                        },
                        span,
                    );
                    return Some(self.fresh());
                };
                Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: owner == "BTreeMap",
                }))
            }
            _ => None,
        }
    }

    fn collection_call_ret_ty(
        &mut self,
        module: &[&str],
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
    ) -> Option<Ty> {
        match method {
            "new" | "with_capacity" => self.collection_ctor_ty(module),
            "from" => {
                self.collection_from_ty(module, *arg_tys.first()?, expected, args.first()?.span)
            }
            _ => None,
        }
    }

    fn flag_set_ty(&mut self) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - 21);
        self.tcx.register_def_name(def, "flag::Set");
        self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::new(),
        })
    }

    fn flag_set_method_ret(&mut self, method: &str, resolved: Ty) -> Option<Ty> {
        let is_flag_set = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => {
                def.local == u32::MAX - 21 || self.tcx.def_name(*def) == Some("flag::Set")
            }
            _ => false,
        };
        if !is_flag_set || method != "parse" {
            return None;
        }
        let s = self.tcx.string_ty();
        let vec = self.tcx.intern(TyKind::Vec(s));
        let err = self.tcx.dyn_error_ty();
        Some(self.result_adt_ty(vec, err))
    }

    fn validate_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, resolved);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let public_owner = self.render_public_ty(resolved);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self
            .tcx
            .def_name(*def)
            .map_or_else(|| public_owner.clone(), ToString::to_string);
        let string = self.tcx.string_ty();
        let field_error = self.stdlib_handle_ty(10, "validate::FieldError");
        let owner_key = match public_owner.as_str() {
            "validate::Errors" | "validate::FieldError" => public_owner.as_str(),
            _ => owner.as_str(),
        };
        let (params, ret) = match (owner_key, method) {
            ("Errors" | "validate::Errors", "add") => (vec![string, field_error], self.tcx.unit()),
            ("Errors" | "validate::Errors", "is_empty") => (Vec::new(), self.tcx.bool_ty()),
            ("Errors" | "validate::Errors", "len") => (Vec::new(), self.tcx.int_ty(IntTy::I64)),
            ("Errors" | "validate::Errors", "count") => (vec![string], self.tcx.int_ty(IntTy::I64)),
            ("Errors" | "validate::Errors", "get") => (vec![string], string),
            ("Errors" | "validate::Errors", "collect") => (Vec::new(), string),
            ("FieldError" | "validate::FieldError", "path" | "message" | "code") => {
                (Vec::new(), string)
            }
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    fn stdlib_handle_ty(&mut self, offset: u32, name: &str) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - offset);
        self.tcx.register_def_name(def, name);
        self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::new(),
        })
    }

    /// Names the value class when `ty` has no textual form, so a format
    /// macro over it is refused here rather than rendering a pointer whose
    /// bits differ on every run.
    /// Types `x.to_string()` as the rendering `{}` gives the same value, for
    /// any receiver that has one. A `String` is already its own text, and a
    /// handle, a callable, or a concurrency type has no rendering, so both
    /// keep whatever surface declares the name for them.
    fn check_display_to_string(&mut self, method: &str, resolved: Ty, args: &[Expr]) -> Option<Ty> {
        (method == "to_string"
            && args.is_empty()
            && !matches!(self.tcx.kind(resolved), Some(TyKind::String))
            && self.is_displayable_value(resolved))
        .then(|| self.tcx.string_ty())
    }

    /// Whether `ty` is a value with a rendering: what `{}` accepts, minus the
    /// lazy cursors, which stand for a sequence rather than holding one.
    fn is_displayable_value(&mut self, ty: Ty) -> bool {
        let peeled = self.peel_refs(ty);
        !matches!(
            self.tcx.kind(peeled),
            Some(TyKind::Iterator(_) | TyKind::Range(_))
        ) && self.not_displayable(ty).is_none()
    }

    fn not_displayable(&mut self, ty: Ty) -> Option<(String, crate::NotDisplayableClass)> {
        use crate::NotDisplayableClass as Class;
        let peeled = self.peel_refs(ty);
        match self.tcx.kind(peeled)? {
            TyKind::Adt { def, .. } if is_opaque_handle_def(def.local) => Some((
                self.tcx
                    .def_name(*def)
                    .map_or_else(|| crate::render_ty(self.tcx, peeled), ToString::to_string),
                Class::Handle,
            )),
            TyKind::FnDef { .. } | TyKind::FnPtr(_) => {
                Some(("function".to_string(), Class::Callable))
            }
            TyKind::FnTrait(_) | TyKind::Closure { .. } => {
                Some(("closure".to_string(), Class::Callable))
            }
            TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_) => {
                Some((crate::render_ty(self.tcx, peeled), Class::Concurrency))
            }
            _ => None,
        }
    }

    /// Handle type a stdlib constructor or middleware wrapper yields.
    ///
    /// Every wrapping `middleware::<name>(inner, ..)` composes the same
    /// handler handle, whatever shape the inner handler has: the catalogue
    /// spells that slot `T` (any handler) or `http::Handler`, while the
    /// sibling helpers returning `bool` / `String` keep their catalogue
    /// types.
    fn handle_call_ret_ty(&mut self, module: &[&str], last: &str) -> Option<Ty> {
        if let Some((offset, handle)) = stdlib_handle_ctor(module, last) {
            return Some(self.stdlib_handle_ty(offset, handle));
        }

        // A socket constructor answers its handle through a `Result`, so
        // `TcpStream::connect(addr)?` propagates like any fallible call.
        if let Some((offset, handle)) = net_socket_ctor(module, last) {
            let socket = self.stdlib_handle_ty(offset, handle);
            return Some(self.fallible(socket));
        }
        // Same shape for the streaming filesystem handle: opening a file
        // can fail, so `fs::File::open(p)?` propagates.
        if fs_file_ctor(module, last) {
            let file = self.stdlib_handle_ty(44, "fs::File");
            return Some(self.fallible(file));
        }
        // And for a compiled pattern, which a bad expression can refuse.
        if matches!(
            (module.strip_prefix(&["std"]).unwrap_or(module), last),
            (["regex"], "compile")
        ) {
            let pattern = self.stdlib_handle_ty(26, "regex::Pattern");
            return Some(self.fallible(pattern));
        }
        let is_middleware = matches!(
            module,
            ["middleware"] | ["http", "middleware"] | ["std", "http", "middleware"]
        );
        if is_middleware
            && crate::stdlib_signatures::function_shape_for_path(module, last)
                .is_some_and(|shape| matches!(shape.return_ty.trim(), "T" | "http::Handler"))
        {
            return Some(self.http_handler_ty());
        }
        None
    }

    /// The handle every `middleware::*` wrapper composes and returns.
    fn http_handler_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(PURE_HANDLE_HI_OFFSET, "http::Handler")
    }

    fn http_response_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(5, "http::Response")
    }

    fn http_client_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(22, "http::Client")
    }

    fn http_client_builder_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(23, "http::ClientBuilder")
    }

    fn http_request_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(24, "http::Request")
    }

    fn io_stream_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(25, "io::Stream")
    }

    fn bytes_handle_ty(&mut self, name: &str) -> Ty {
        let offset = if name == "bytes::Buffer" { 26 } else { 27 };
        self.stdlib_handle_ty(offset, name)
    }

    /// Return type of a method on one of the opaque `std::net` socket
    /// handles. Without a row here the call answers a fresh variable, so
    /// `?` on `sock.read(n)` reports the operand is not a `Result`.
    ///
    /// The table is the handle's complete surface, so a name absent from
    /// it is reported here. Letting it through typed the call as a fresh
    /// variable and reached the compiled tier as an undefined `@method`
    /// symbol - a link failure with no source location, after `gos check`
    /// had said the program was fine.
    fn net_handle_method_ret(
        &mut self,
        method: &str,
        resolved: Ty,
        arg_count: usize,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        if !matches!(
            owner.as_str(),
            "net::TcpStream"
                | "net::UnixStream"
                | "net::TcpListener"
                | "net::UnixListener"
                | "net::UdpSocket"
        ) {
            return None;
        }
        let unit = self.tcx.unit();
        match (owner.as_str(), method) {
            (
                "net::TcpStream" | "net::UnixStream" | "net::TcpListener" | "net::UnixListener"
                | "net::UdpSocket",
                "close",
            )
            | (
                "net::TcpStream",
                "set_read_timeout_ms"
                | "set_write_timeout_ms"
                | "set_nodelay"
                | "clear_read_timeout"
                | "clear_write_timeout",
            ) => Some(unit),
            ("net::TcpStream" | "net::UnixStream", "read") => {
                let bytes = self.byte_vec_ty();
                Some(self.fallible(bytes))
            }
            // Answers the byte count it wrote into the caller's buffer.
            ("net::TcpStream", "read_into") => {
                let count = self.tcx.int_ty(IntTy::I64);
                Some(self.fallible(count))
            }
            ("net::TcpStream" | "net::UnixStream", "read_to_string")
            | ("net::TcpListener" | "net::UdpSocket", "local_addr") => {
                let string = self.tcx.string_ty();
                Some(self.fallible(string))
            }
            ("net::TcpStream" | "net::UnixStream", "write" | "write_all")
            | ("net::UdpSocket", "send_to") => Some(self.fallible(unit)),
            ("net::TcpStream", "start_tls" | "start_tls_ca" | "start_tls_insecure") => {
                let handle = self.stdlib_handle_ty(12, "net::TcpStream");
                Some(self.fallible(handle))
            }
            // The peer's certificate is bytes, not a handle: what a caller
            // does with it - hash it for a channel binding, read its
            // fields - is its own business.
            ("net::TcpStream", "peer_certificate") => {
                let byte = self.tcx.int_ty(IntTy::U8);
                Some(self.tcx.intern(TyKind::Vec(byte)))
            }
            ("net::TcpListener", "accept") => {
                let stream = self.stdlib_handle_ty(12, "net::TcpStream");
                let string = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![stream, string]));
                Some(self.fallible(pair))
            }
            ("net::UnixListener", "accept") => {
                let stream = self.stdlib_handle_ty(15, "net::UnixStream");
                let string = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![stream, string]));
                Some(self.fallible(pair))
            }
            ("net::UdpSocket", "recv_from") => {
                let bytes = self.byte_vec_ty();
                let string = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![bytes, string]));
                Some(self.fallible(pair))
            }
            // `clone` is the language's own copy, answered for every value.
            (_, "clone") => Some(resolved),
            _ => {
                let error = self.unresolved_method_call(owner.clone(), method, resolved, arg_count);
                self.emit(error, span);
                Some(self.tcx.error_ty())
            }
        }
    }

    /// `Vec<u8>`, the shape every socket read answers.
    fn byte_vec_ty(&mut self) -> Ty {
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        self.tcx.intern(TyKind::Vec(u8_ty))
    }

    /// `Result<ok, errors::Error>` - the stdlib's fallible answer shape.
    fn fallible(&mut self, ok: Ty) -> Ty {
        let err = self.tcx.dyn_error_ty();
        self.result_adt_ty(ok, err)
    }

    /// Return type of a method on the streaming filesystem handles, with
    /// the parameter list each one declares.
    ///
    /// Without a contract here every `f.write(..)` typed as a fresh
    /// variable: a `Vec<u8>` passed to the text `write` reached the
    /// runtime as an argument-shape error rather than a type error, and
    /// `?` on a fallible call saw no `Result` at all.
    fn fs_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        if !matches!(owner.as_str(), "fs::File" | "fs::OpenOptions") {
            return None;
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let string = self.tcx.string_ty();
        let unit = self.tcx.unit();
        let bytes = self.byte_vec_ty();
        let (params, ret) = match (owner.as_str(), method) {
            ("fs::File", "read") => (vec![i64_ty], self.fallible(bytes)),
            ("fs::File", "read_at") => (vec![i64_ty, i64_ty], self.fallible(bytes)),
            ("fs::File", "read_to_string") => (vec![], self.fallible(string)),
            ("fs::File", "write" | "write_all") => (vec![string], self.fallible(i64_ty)),
            ("fs::File", "write_bytes") => (vec![bytes], self.fallible(i64_ty)),
            ("fs::File", "write_at") => (vec![bytes, i64_ty], self.fallible(i64_ty)),
            ("fs::File", "seek") => (vec![i64_ty, i64_ty], self.fallible(i64_ty)),
            ("fs::File", "set_len") => (vec![i64_ty], self.fallible(unit)),
            ("fs::File", "len") => (vec![], self.fallible(i64_ty)),
            ("fs::File", "flush" | "sync_all" | "sync_data") => (vec![], self.fallible(unit)),
            ("fs::File", "try_lock_range") => {
                (vec![i64_ty, i64_ty, bool_ty], self.fallible(bool_ty))
            }
            ("fs::File", "unlock_range") => (vec![i64_ty, i64_ty], self.fallible(unit)),
            ("fs::File", "try_lock_shared" | "try_lock_exclusive") => {
                (vec![], self.fallible(bool_ty))
            }
            ("fs::File", "unlock") => (vec![], self.fallible(unit)),
            ("fs::File", "close") => (vec![], unit),
            (
                "fs::OpenOptions",
                "read" | "write" | "append" | "truncate" | "create" | "create_new",
            ) => {
                let opts = self.stdlib_handle_ty(45, "fs::OpenOptions");
                (vec![bool_ty], opts)
            }
            ("fs::OpenOptions", "open") => {
                let file = self.stdlib_handle_ty(44, "fs::File");
                (vec![string], self.fallible(file))
            }
            _ => {
                let error = self.unresolved_method_call(owner, method, resolved, args.len());
                self.emit(error, span);
                return Some(self.tcx.error_ty());
            }
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    /// Types a method on a runtime handle, whichever family owns it.
    ///
    /// Each family answers `None` for a receiver it does not own, so the
    /// order here is immaterial and adding a handle family costs one line
    /// rather than another branch in the method-call checker.
    fn handle_family_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        receiver: &Expr,
    ) -> Option<Ty> {
        let span = receiver.span;
        self.bytes_handle_method_ret(method, args, arg_tys, resolved, span)
            .or_else(|| self.regex_handle_method_ret(method, args, arg_tys, resolved, span))
            .or_else(|| self.fs_handle_method_ret(method, args, arg_tys, resolved, span))
    }

    /// Types a `regex::Pattern` method.
    ///
    /// Every signature here mirrors the free form the stdlib manifest
    /// declares, with the pattern as the receiver instead of the first
    /// argument. Without it the receiver stays an inference variable, so
    /// `println!("{}", pattern)` passes `gos check` and then renders a
    /// struct on the VM and a raw pointer natively.
    fn regex_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        // A handle is conventionally written `&regex::Pattern` where it is
        // a parameter, and the methods take it either way.
        let resolved = self.peel_refs(resolved);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        if owner != "regex::Pattern" {
            return None;
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let string = self.tcx.string_ty();
        let strings = self.tcx.intern(TyKind::Vec(string));
        // `find` answers the match's start, end, and text.
        let span_ty = self.tcx.intern(TyKind::Tuple(vec![i64_ty, i64_ty, string]));
        let spans = self.tcx.intern(TyKind::Vec(span_ty));
        // A capture group that did not participate has no text.
        let maybe_string = self.option_adt_ty(string);
        let groups = self.tcx.intern(TyKind::Vec(maybe_string));
        let all_groups = self.tcx.intern(TyKind::Vec(groups));
        let (params, ret) = match method {
            "is_match" => (vec![string], bool_ty),
            "find" => (vec![string], self.option_adt_ty(span_ty)),
            "find_all" => (vec![string], spans),
            "captures" => (vec![string], self.option_adt_ty(groups)),
            "captures_all" => (vec![string], all_groups),
            "replace" | "replace_all" => (vec![string, string], string),
            "split" => (vec![string], strings),
            _ => {
                let error =
                    self.unresolved_method_call(owner.clone(), method, resolved, args.len());
                self.emit(error, span);
                return Some(self.tcx.error_ty());
            }
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    fn bytes_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let string = self.tcx.string_ty();
        let (params, ret) = match (owner.as_str(), method) {
            ("bytes::Buffer", "push") => (vec![self.tcx.int_ty(IntTy::U8)], self.tcx.unit()),
            ("bytes::Buffer", "write_str") => (vec![string], self.tcx.unit()),
            ("bytes::Buffer", "clear") => (vec![], self.tcx.unit()),
            ("bytes::Buffer", "len") => (vec![], i64_ty),
            ("bytes::Buffer", "is_empty") => (vec![], self.tcx.bool_ty()),
            ("bytes::Buffer", "to_string") => (vec![], string),
            ("bytes::Builder", "write") => (vec![string], self.tcx.unit()),
            ("bytes::Builder", "write_char") => {
                (vec![self.tcx.intern(TyKind::Char)], self.tcx.unit())
            }
            ("bytes::Builder", "len") => (vec![], i64_ty),
            ("bytes::Builder", "build" | "as_str") => (vec![], string),
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    fn result_response_error_ty(&mut self) -> Ty {
        let resp = self.http_response_ty();
        let err = self.tcx.dyn_error_ty();
        self.result_adt_ty(resp, err)
    }

    fn http_client_method_ret(&mut self, method: &str, resolved: Ty) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        match self.tcx.def_name(*def) {
            Some("http::Client") => match method {
                "get" | "post" | "put" | "options" | "delete" | "head" => {
                    Some(self.http_request_ty())
                }
                "request" | "request_bytes" => Some(self.result_response_error_ty()),
                _ => None,
            },
            Some("http::ClientBuilder") => match method {
                "max_redirects" | "timeout_ms" | "cookie_jar" | "proxy" => {
                    Some(self.http_client_builder_ty())
                }
                "build" => Some(self.http_client_ty()),
                _ => None,
            },
            Some("http::Request") => match method {
                "header" | "body" | "set_value" => Some(self.http_request_ty()),
                "send" => Some(self.result_response_error_ty()),
                "path" | "path_value" | "method" | "value" | "form_value" => {
                    Some(self.tcx.string_ty())
                }
                "path_int" => {
                    let i = self.tcx.int_ty(IntTy::I64);
                    Some(self.option_adt_ty(i))
                }
                "path_float" => {
                    let f = self.tcx.float_ty(FloatTy::F64);
                    Some(self.option_adt_ty(f))
                }
                "basic_auth" => {
                    let s = self.tcx.string_ty();
                    let pair = self.tcx.intern(TyKind::Tuple(vec![s, s]));
                    Some(self.option_adt_ty(pair))
                }
                _ => None,
            },
            Some("http::ResponseStream") => match method {
                "write" | "write_bytes" => Some(self.tcx.int_ty(IntTy::I64)),
                "close" => Some(self.tcx.unit()),
                "is_open" => Some(self.tcx.bool_ty()),
                _ => None,
            },
            // Every setter answers the server itself, which is what lets a
            // configuration be written as one `|>` chain.
            Some("http::Server") => match method {
                "read_header_timeout_ms"
                | "read_body_timeout_ms"
                | "write_timeout_ms"
                | "idle_timeout_ms"
                | "max_header_bytes"
                | "max_body_bytes"
                | "max_connections"
                | "request_timeout_ms"
                | "server_name" => Some(self.http_server_ty()),
                "listen" => {
                    let unit = self.tcx.unit();
                    Some(self.fallible(unit))
                }
                "serve" => {
                    let unit = self.tcx.unit();
                    Some(self.fallible(unit))
                }
                "addr" => Some(self.tcx.string_ty()),
                "shutdown" => Some(self.tcx.bool_ty()),
                _ => None,
            },
            // A verb method answers the router itself, which is what lets a
            // routing table be built as one `|>` chain. Without the row the
            // chain's later steps carry an open receiver type.
            Some("http::Router") => match method {
                "get" | "post" | "put" | "delete" | "patch" | "head" | "options" => {
                    Some(self.http_router_ty())
                }
                "serve" => Some(self.result_response_error_ty()),
                _ => None,
            },
            Some("http::Response") => match method {
                "with_header" => Some(self.http_response_ty()),
                "bytes" => {
                    let byte = self.tcx.int_ty(IntTy::U8);
                    Some(self.tcx.intern(TyKind::Vec(byte)))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Return type of a method on a `sync::Shared`.
    ///
    /// The guarded slot is one word that every tier reads back as an
    /// integer, so that is what a read answers and what an update stores.
    fn shared_method_ret(&mut self, method: &str, resolved: Ty, args: &[Expr]) -> Option<Ty> {
        let TyKind::Adt { def, .. } = self.tcx.kind_of(resolved) else {
            return None;
        };
        if self.tcx.def_name(*def) != Some("sync::Shared") {
            return None;
        }
        let elem = self.tcx.int_ty(IntTy::I64);
        match method {
            "get" => Some(elem),
            "set" => {
                if let Some(arg) = args.first() {
                    let value = self.check_expr(arg);
                    self.unify(elem, value, arg.span);
                }
                Some(self.tcx.unit())
            }
            // `with` answers whatever the callback answers; `update` stores
            // what it answers, so that has to be the guarded type.
            "with" | "update" => {
                let output = if method == "update" {
                    elem
                } else {
                    self.fresh()
                };
                let sig = FnSig {
                    inputs: vec![elem],
                    output,
                };
                let want = self.tcx.intern(TyKind::FnTrait(sig));
                if let Some(arg) = args.first() {
                    let got = self.check_expr_expecting(arg, Expectation::HasType(want));
                    self.unify(want, got, arg.span);
                }
                Some(output)
            }
            _ => None,
        }
    }

    fn http_router_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(41, "http::Router")
    }

    fn http_server_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(48, "http::Server")
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "flat stdlib module dispatch table keeps call typing local"
    )]
    /// `sort::sort_stable(xs) -> Vec<T>`, answering the element type its
    /// argument holds.
    ///
    /// The catalogue row cannot say so on its own: its `Vec<T>` names a
    /// parameter the row has no argument to pin, and an unpinned return
    /// leaves the sequence spelled as a slice wherever it is shown.
    fn sort_stable_ret_ty(&mut self, module: &[&str], last: &str, arg_tys: &[Ty]) -> Option<Ty> {
        if !matches!(module, ["sort"] | ["std", "sort"]) || last != "sort_stable" {
            return None;
        }
        let resolved = self.infer.resolve(self.tcx, *arg_tys.first()?);
        let (TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) =
            self.tcx.kind(resolved)?
        else {
            return None;
        };
        let elem = *elem;
        Some(self.tcx.intern(TyKind::Vec(elem)))
    }

    fn check_stdlib_module_ret_ty(
        &mut self,
        module: &[&str],
        last: &str,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
    ) -> Option<Ty> {
        if let Some(ty) = self.check_qualified_map_accessor_ret(module, last, arg_tys) {
            return Some(ty);
        }
        if is_channel_constructor_path(module, last) {
            return Some(self.channel_tuple_ty());
        }
        if let Some(ty) = self.sort_stable_ret_ty(module, last, arg_tys) {
            return Some(ty);
        }
        // `env::var(name) -> Option<String>`. Typing it concretely lets the
        // match checker reject matching its result with `Result` patterns
        // (`Ok`/`Err`), which otherwise silently fell through on the VM and
        // matched by discriminant on the compiled tier.
        if matches!(module, ["env"] | ["std", "env"]) && last == "var" {
            let s = self.tcx.string_ty();
            return Some(self.option_adt_ty(s));
        }
        // `process::spawn_piped(prog, args) -> Result<Child, errors::Error>`.
        // The Ok payload is the named `Child` sentinel Adt so the
        // extracted binder carries the `process::Child` runtime kind
        // and its method calls dispatch to the child shims on every
        // tier.
        if matches!(
            module,
            ["process" | "exec"] | ["os", "exec"] | ["std", "process"] | ["std", "os", "exec"]
        ) && last == "spawn_piped"
        {
            let child_def = gossamer_resolve::DefId::local(u32::MAX - 8);
            self.tcx.register_def_name(child_def, "Child");
            let child_ty = self.tcx.intern(TyKind::Adt {
                def: child_def,
                substs: crate::Substs::new(),
            });
            let err = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(child_ty, err));
        }
        // `signal::on(sig) -> signal::Notifier`. The runtime value is
        // the same opaque i64 handle; the sentinel type keeps
        // method-form dispatch (`n.wait()`, `n.try_wait()`) uniform
        // with free-form dispatch across tiers.
        if matches!(
            module,
            ["signal"] | ["os", "signal"] | ["std", "os", "signal"]
        ) && last == "on"
        {
            let notifier_def = gossamer_resolve::DefId::local(u32::MAX - 17);
            self.tcx.register_def_name(notifier_def, "Notifier");
            return Some(self.tcx.intern(TyKind::Adt {
                def: notifier_def,
                substs: crate::Substs::new(),
            }));
        }
        if matches!(
            module,
            ["signal"] | ["os", "signal"] | ["std", "os", "signal"]
        ) && matches!(last, "wait" | "try_wait")
        {
            return Some(if last == "wait" {
                self.tcx.unit()
            } else {
                self.tcx.bool_ty()
            });
        }
        // `json::Value::*` constructor calls produce the opaque dynamic
        // JSON value. Without this the call is a fresh var, so a
        // chained method (`.set`, `.get`) loses the JsonValue receiver
        // tag and the compiled tiers cannot route it to the json
        // runtime helpers.
        if matches!(
            module,
            ["json", "Value"]
                | ["encoding", "json", "Value"]
                | ["std", "encoding", "json", "Value"]
        ) {
            return Some(self.tcx.json_value_ty());
        }
        // `DynValue::<ctor>(..)` builds the open dynamic value. Every
        // constructor answers one, whatever it was built from.
        if matches!(module, ["DynValue"]) {
            return self.dyn_value_ctor_ret(last);
        }
        if matches!(
            module,
            ["json"] | ["encoding", "json"] | ["std", "encoding", "json"]
        ) {
            return self.json_module_ret_ty(last, callee, args, arg_tys);
        }
        if matches!(module, ["errors"] | ["std", "errors"]) {
            return match last {
                "new" | "wrap" => Some(self.tcx.dyn_error_ty()),
                _ => None,
            };
        }
        if let Some(ty) = self.handle_call_ret_ty(module, last) {
            return Some(ty);
        }
        if let Some(ty) = self.collection_call_ret_ty(module, last, args, arg_tys, expected) {
            return Some(ty);
        }
        if matches!(module, ["fs" | "os"] | ["std", "fs" | "os"]) {
            return self.fs_call_ret_ty(last);
        }
        // `time::Duration` constructors yield the transparent Duration
        // newtype so the method-form accessors (`d.as_millis()`) can
        // dispatch on the receiver's static type; the accessors
        // themselves return a bare `i64`.
        if matches!(module, ["time", "Duration"] | ["std", "time", "Duration"]) {
            return match last {
                "from_millis" | "from_secs" | "from_micros" => Some(self.tcx.duration_ty()),
                "as_millis" | "as_secs" | "as_micros" => Some(self.tcx.int_ty(IntTy::I64)),
                _ => None,
            };
        }
        // `time::Instant::now()` yields the transparent Instant newtype so
        // the method-form accessor (`inst.elapsed_ms()`) can dispatch on
        // the receiver's static type; the accessor itself returns `i64`.
        if matches!(module, ["time", "Instant"] | ["std", "time", "Instant"]) {
            return match last {
                "now" => Some(self.tcx.instant_ty()),
                "elapsed_ms" => Some(self.tcx.int_ty(IntTy::I64)),
                _ => None,
            };
        }
        None
    }

    /// Return type of a `json::` / `encoding::json::` free-function call
    /// (`parse`, `get`, `as_i64`, ...); `None` for an unrecognised name.
    fn json_module_ret_ty(
        &mut self,
        last: &str,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        match last {
            "parse" | "decode" => {
                let j = self.tcx.json_value_ty();
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(j, e))
            }
            "render" | "encode" => {
                self.reject_json_enum_arg(last, callee, args, arg_tys);
                Some(self.tcx.string_ty())
            }
            "at" | "identity" | "set" => Some(self.tcx.json_value_ty()),
            "get" => {
                let j = self.tcx.json_value_ty();
                Some(self.option_adt_ty(j))
            }
            "len" => Some(self.tcx.int_ty(IntTy::I64)),
            "is_null" => Some(self.tcx.bool_ty()),
            "as_i64" => {
                let i = self.tcx.int_ty(IntTy::I64);
                Some(self.option_adt_ty(i))
            }
            "as_f64" => {
                let f = self.tcx.float_ty(FloatTy::F64);
                Some(self.option_adt_ty(f))
            }
            "as_str" => {
                let s = self.tcx.string_ty();
                Some(self.option_adt_ty(s))
            }
            "as_bool" => {
                let b = self.tcx.bool_ty();
                Some(self.option_adt_ty(b))
            }
            "as_array" => {
                let j = self.tcx.json_value_ty();
                let arr = self.tcx.intern(TyKind::Vec(j));
                Some(self.option_adt_ty(arr))
            }
            _ => None,
        }
    }

    /// Return type of a `DynValue::<name>(..)` constructor, or `None` when
    /// the name is not one.
    fn dyn_value_ctor_ret(&mut self, last: &str) -> Option<Ty> {
        matches!(
            last,
            "nil"
                | "bool"
                | "int"
                | "float"
                | "char"
                | "string"
                | "bytes"
                | "list"
                | "map"
                | "tagged"
        )
        .then(|| self.tcx.dyn_value_ty())
    }

    /// Return type of a method call on one of the runtime's opaque dynamic
    /// receivers - a `json::Value` or a `DynValue`. Each answers the same
    /// surface in method form that its own table declares; falling through to
    /// a fresh variable would strip the receiver's tag and leave a chained
    /// call untagged for the compiled tiers.
    fn opaque_value_method_ret(
        &mut self,
        resolved: Ty,
        method: &str,
        arg_count: usize,
    ) -> Option<Ty> {
        match self.tcx.kind(resolved) {
            Some(TyKind::JsonValue) => self.json_value_method_ret(method, arg_count),
            Some(TyKind::DynValue) => self.dyn_value_method_ret(method, arg_count),
            _ => None,
        }
    }

    /// Return type of a method call on a `DynValue` receiver. `None` leaves
    /// the call to the later dispatch arms, which report it as unknown.
    fn dyn_value_method_ret(&mut self, method: &str, arg_count: usize) -> Option<Ty> {
        let ty = match (method, arg_count) {
            ("kind" | "name", 0) => self.tcx.string_ty(),
            ("len", 0) => self.tcx.int_ty(IntTy::I64),
            ("at" | "key_at", 1) | ("clone", 0) => self.tcx.dyn_value_ty(),
            ("as_i64", 0) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            ("as_f64", 0) => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.option_adt_ty(f)
            }
            ("as_bool", 0) => {
                let b = self.tcx.bool_ty();
                self.option_adt_ty(b)
            }
            ("as_char", 0) => {
                let c = self.tcx.char_ty();
                self.option_adt_ty(c)
            }
            ("as_str", 0) => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            ("as_bytes", 0) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Vec(i))
            }
            ("to_string", 0) => self.tcx.string_ty(),
            _ => return None,
        };
        Some(ty)
    }

    /// Return type of a method call on a `json::Value` receiver, which is
    /// the free function of the same name with the receiver as its first
    /// argument. `None` leaves the call to the later dispatch arms.
    fn json_value_method_ret(&mut self, method: &str, arg_count: usize) -> Option<Ty> {
        let ty = match (method, arg_count) {
            ("at", 1) | ("set", 2) => self.tcx.json_value_ty(),
            ("get", 1) => {
                let j = self.tcx.json_value_ty();
                self.option_adt_ty(j)
            }
            ("keys", 0) => {
                let name = self.tcx.string_ty();
                let names = self.tcx.intern(TyKind::Vec(name));
                self.option_adt_ty(names)
            }
            ("len", 0) => self.tcx.int_ty(IntTy::I64),
            ("is_null", 0) => self.tcx.bool_ty(),
            ("as_i64", 0) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            ("as_f64", 0) => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.option_adt_ty(f)
            }
            ("as_str", 0) => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            ("as_bool", 0) => {
                let b = self.tcx.bool_ty();
                self.option_adt_ty(b)
            }
            ("as_array", 0) => {
                let j = self.tcx.json_value_ty();
                let arr = self.tcx.intern(TyKind::Vec(j));
                self.option_adt_ty(arr)
            }
            _ => return None,
        };
        Some(ty)
    }

    /// Types the parser-injected format intrinsics and the bare
    /// variant constructors. The resolver doesn't hand `Some` / `Ok` /
    /// `Err` / `None` a `DefId`, so the call expression typechecks as
    /// a fresh `Var` and the binding `let first = Some(10)` collapses
    /// to `Int(I64)` - losing the Adt wrapper. Match dispatch later
    /// treats the 8-byte `*mut GosResult` pointer as a raw i64 and
    /// reads garbage from the slot. Recognise the four standard
    /// variants here and synthesise the right Adt: `Some(t)` →
    /// `Option<t>`, `Ok(t)` → `Result<t, ?>`, `Err(e)` →
    /// `Result<?, e>`, `None` → `Option<?>`. Pinning `__concat` /
    /// `__fmt_prec` to `String` is safe: they're synthetic names the
    /// parser injects and no user code can shadow them.
    /// `spawn(f, reason: "..")`: the label is text a report prints, so
    /// anything else would reach the runtime as a word it would read as a
    /// pointer.
    fn check_spawn_reason(&mut self, reason: Option<Ty>, span: Span) {
        let Some(reason) = reason else {
            return;
        };
        if matches!(
            self.tcx.kind_of(reason),
            TyKind::String | TyKind::Var(_) | TyKind::Error
        ) {
            return;
        }
        self.emit(
            TypeError::ArgumentTypeMismatch {
                callee: "spawn".to_string(),
                parameter: "reason".to_string(),
                expected: "String".to_string(),
                found: crate::render_ty(self.tcx, reason),
                actual: "the `reason:` label".to_string(),
            },
            span,
        );
    }

    fn check_bare_intrinsic_call(&mut self, name: &str, arg_tys: &[Ty], span: Span) -> Option<Ty> {
        let constructor_arity = match name {
            "Some" | "Ok" | "Err" => Some(1),
            "None" => Some(0),
            _ => None,
        };
        if let Some(expected) = constructor_arity
            && arg_tys.len() != expected
        {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: name.to_string(),
                    expected,
                    found: arg_tys.len(),
                },
                span,
            );
        }
        let ty = match name {
            "__concat"
            | "__debug"
            | "__fmt_prec"
            | "__fmt_pad"
            | "__fmt_radix"
            | "__fmt_upper"
            | "__gos_strconv_quote" => {
                for ty in arg_tys {
                    let resolved = self.infer.resolve(self.tcx, *ty);
                    if matches!(
                        self.tcx.kind(resolved),
                        Some(TyKind::Iterator(_) | TyKind::Range(_))
                    ) {
                        self.emit(TypeError::IteratorStateFormatted, span);
                        return Some(self.tcx.error_ty());
                    }
                    if let Some((ty, class)) = self.not_displayable(resolved) {
                        self.emit(TypeError::ValueNotDisplayable { ty, class }, span);
                        return Some(self.tcx.error_ty());
                    }
                    if let Some(ty) = self.generic_without_fmt(resolved) {
                        self.emit(
                            TypeError::ValueNotDisplayable {
                                ty,
                                class: crate::error::NotDisplayableClass::GenericWithoutDebug,
                            },
                            span,
                        );
                        return Some(self.tcx.error_ty());
                    }
                }
                self.tcx.string_ty()
            }
            "__repl_discard" => self.tcx.unit(),
            // `channel()` / `channel(n)` / `channel::unbounded()` ->
            // `(Sender<?T>, Receiver<?T>)` sharing one element var, so
            // `tx.send(v)` unifies the element through the shared `?T` and
            // `rx.recv()` yields `Option<?T>` with the real payload type even
            // for an inferred local channel. The optional constructor argument
            // is capacity only; it never changes the element type.
            "channel"
            | "channel::new"
            | "channel::unbounded"
            | "sync::channel"
            | "sync::channel_unbounded"
            | "std::sync::channel"
            | "std::sync::channel_unbounded" => self.channel_tuple_ty(),
            // `spawn(f) -> JoinHandle<T>`, T being the callable's return
            // type. Typing the call itself is what lets `spawn(f).join()`
            // resolve without binding the handle first: the method arm
            // that lowers `join` keys on the receiver's static type.
            "spawn" if matches!(arg_tys.len(), 1 | 2) => {
                self.check_spawn_reason(arg_tys.get(1).copied(), span);
                let elem = match self.tcx.kind_of(arg_tys[0]).clone() {
                    TyKind::FnTrait(sig) | TyKind::FnPtr(sig) => sig.output,
                    TyKind::Var(_) => self.fresh(),
                    _ => return None,
                };
                self.tcx.intern(TyKind::JoinHandle(elem))
            }
            "Some" => {
                let payload = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
                self.option_adt_ty(payload)
            }
            "None" => {
                let payload = self.fresh();
                self.option_adt_ty(payload)
            }
            "Ok" => {
                let ok_ty = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
                let err_ty = self.fresh();
                self.result_adt_ty(ok_ty, err_ty)
            }
            "Err" => {
                let ok_ty = self.fresh();
                let err_ty = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
                self.result_adt_ty(ok_ty, err_ty)
            }
            _ => return None,
        };
        Some(ty)
    }

    fn channel_tuple_ty(&mut self) -> Ty {
        let elem = self.fresh();
        let sender = self.tcx.intern(TyKind::Sender(elem));
        let receiver = self.tcx.intern(TyKind::Receiver(elem));
        self.tcx.intern(TyKind::Tuple(vec![sender, receiver]))
    }

    /// The `[(String, [u8])]` entry-list parameter type of the stdlib
    /// `archive::{tar,zip}::write` calls.
    fn archive_entry_vec_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        let pair = self.tcx.intern(TyKind::Tuple(vec![s, vec_u8]));
        self.tcx.intern(TyKind::Vec(pair))
    }

    /// Shapes stdlib call arguments from the checker-owned source signature
    /// catalogue when the parameter type is concrete enough to enforce safely.
    /// Generic, callable, and JSON-value slots are left unshaped so existing
    /// inference-sensitive paths keep their current semantics.
    fn stdlib_signature_arg_expectations(
        &mut self,
        callee_id: NodeId,
        path: &gossamer_ast::PathExpr,
        n_args: usize,
    ) -> Option<Vec<Expectation>> {
        let names = self.resolved_value_path_names(callee_id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let name = last.first().copied()?;
        // String functions have String|char pattern slots that the generic
        // signature parser cannot model precisely. Their dedicated validator
        // both enforces the complete contract and emits one named diagnostic.
        if matches!(module, ["strings"] | ["std", "strings"]) {
            return None;
        }
        // Arguments reach the checker in the order every pass below the
        // front end uses, which the rotation in `normalize` produced.
        let shape = crate::stdlib_signatures::internal_shape_for_path(module, name)?;
        if shape.params.len() != n_args {
            return None;
        }
        Some(
            shape
                .params
                .iter()
                .map(|param| {
                    self.stdlib_signature_arg_ty(param.ty)
                        .map_or(Expectation::None, Expectation::Coerce)
                })
                .collect(),
        )
    }

    /// Emits the arity diagnostic for stdlib free functions from the same
    /// signature row `%help` displays.
    fn check_stdlib_signature_arity(
        &mut self,
        module: &[&str],
        name: &str,
        supplied: usize,
        pipe_extra: usize,
        span: Span,
    ) {
        if matches!(module, ["slog"] | ["std", "slog"]) {
            return;
        }
        let found = supplied + pipe_extra;
        if is_channel_constructor_path(module, name) && matches!(found, 0 | 1) {
            return;
        }
        let Some(shape) = crate::stdlib_signatures::function_shape_for_path(module, name) else {
            return;
        };
        let expected = shape.params.len();
        if found != expected {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: if module.is_empty() {
                        name.to_string()
                    } else {
                        format!("{}::{name}", module.join("::"))
                    },
                    expected,
                    found,
                },
                span,
            );
        }
    }

    /// Validates concrete stdlib parameter slots after argument synthesis.
    /// The expectation path shapes literals before checking; this pass catches
    /// non-literal mismatches and scalar/string literals whose checker path
    /// does not unify against expectations directly.
    fn check_stdlib_signature_args(
        &mut self,
        module: &[&str],
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        if matches!(module, ["slog"] | ["std", "slog"]) {
            return;
        }
        let Some(shape) = crate::stdlib_signatures::internal_shape_for_path(module, name) else {
            return;
        };
        if shape.params.len() != arg_tys.len() {
            return;
        }
        for (param, (arg, &arg_ty)) in shape.params.iter().zip(args.iter().zip(arg_tys)) {
            if let Some(param_ty) = self.stdlib_signature_arg_ty(param.ty) {
                self.check_sig_param_arg(param_ty, arg_ty, arg);
            }
        }
    }

    /// Return type for a stdlib free function from the checker-owned signature
    /// catalogue. Rows with generics or opaque nominal stdlib handles that the
    /// checker cannot represent yet fall back to the existing specialised paths
    /// or a fresh variable.
    fn stdlib_signature_return_ty(&mut self, module: &[&str], name: &str) -> Option<Ty> {
        let shape = crate::stdlib_signatures::function_shape_for_path(module, name)?;
        self.stdlib_signature_ty(shape.return_ty)
    }

    fn stdlib_signature_arg_ty(&mut self, src: &str) -> Option<Ty> {
        // `json::encode` / `json::render` accept scalars and structs in
        // addition to `json::Value`; pinning those slots to the opaque JSON
        // handle would reject valid calls. Return typing can still use it.
        if src.trim() == "json::Value" {
            return None;
        }
        self.stdlib_signature_ty(src)
    }

    /// Callable type for a `Fn(A, B) -> R` slot in a stdlib signature.
    ///
    /// Returns `None` when the slot is not a callable, or when any part of
    /// it names a type the catalogue cannot represent, so an unmodelled
    /// callback keeps its previous inference-variable behaviour.
    fn stdlib_signature_fn_ty(&mut self, src: &str) -> Option<Ty> {
        let rest = src.trim().strip_prefix("Fn(")?;
        // The parameter list ends at the paren matching `Fn(`, not at the
        // last one in the row: a return type may carry its own parentheses.
        let mut depth = 1usize;
        let mut close = None;
        for (i, ch) in rest.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let (params_src, tail) = rest.split_at(close);
        let mut inputs = Vec::new();
        for part in crate::stdlib_signatures::split_top_level(params_src, ',') {
            if part.trim().is_empty() {
                continue;
            }
            inputs.push(self.stdlib_signature_ty(part)?);
        }
        // An unmodelled return type still leaves the parameters pinned, which
        // is what a closure body needs to type its field accesses.
        let output = match tail[1..].trim().strip_prefix("->") {
            Some(return_src) => self
                .stdlib_signature_ty(return_src)
                .unwrap_or_else(|| self.fresh()),
            None => self.tcx.unit(),
        };
        Some(self.tcx.intern(TyKind::FnTrait(FnSig { inputs, output })))
    }

    fn stdlib_signature_ty(&mut self, src: &str) -> Option<Ty> {
        let src = src.trim();
        // A callback slot resolves to the callable shape it declares, so a
        // closure literal passed there types its parameters from the
        // signature instead of leaving them inference variables that field
        // access then reads dynamically.
        if let Some(sig) = self.stdlib_signature_fn_ty(src) {
            return Some(sig);
        }
        if src.is_empty()
            || src.contains('|')
            || src.starts_with("Fn(")
            || is_catalog_type_param(src)
        {
            return None;
        }
        let src = src.strip_prefix('&').unwrap_or(src).trim();
        match src {
            "String" => return Some(self.tcx.string_ty()),
            "bool" => return Some(self.tcx.bool_ty()),
            "char" => return Some(self.tcx.char_ty()),
            "i8" => return Some(self.tcx.int_ty(IntTy::I8)),
            "i16" => return Some(self.tcx.int_ty(IntTy::I16)),
            "i32" => return Some(self.tcx.int_ty(IntTy::I32)),
            "i64" => return Some(self.tcx.int_ty(IntTy::I64)),
            "i128" => return Some(self.tcx.int_ty(IntTy::I128)),
            "isize" => return Some(self.tcx.int_ty(IntTy::Isize)),
            "u8" => return Some(self.tcx.int_ty(IntTy::U8)),
            "u16" => return Some(self.tcx.int_ty(IntTy::U16)),
            "u32" => return Some(self.tcx.int_ty(IntTy::U32)),
            "u64" => return Some(self.tcx.int_ty(IntTy::U64)),
            "u128" => return Some(self.tcx.int_ty(IntTy::U128)),
            "usize" => return Some(self.tcx.int_ty(IntTy::Usize)),
            "f32" => return Some(self.tcx.float_ty(FloatTy::F32)),
            "f64" => return Some(self.tcx.float_ty(FloatTy::F64)),
            "()" => return Some(self.tcx.unit()),
            "!" => return Some(self.tcx.intern(TyKind::Never)),
            "json::Value" => return Some(self.tcx.json_value_ty()),
            "time::Instant" => return Some(self.tcx.instant_ty()),
            "time::Duration" => return Some(self.tcx.duration_ty()),
            "io::Reader" | "io::Writer" => return Some(self.io_stream_ty()),
            "errors::Error" | "io::Error" => return Some(self.tcx.dyn_error_ty()),
            _ if src.ends_with("::Error") || src.ends_with("ParseError") => {
                return Some(self.tcx.dyn_error_ty());
            }
            _ => {}
        }
        if let Some(inner) = strip_catalog_wrapper(src, "Vec") {
            let elem = self.stdlib_signature_ty(inner)?;
            return Some(self.tcx.intern(TyKind::Vec(elem)));
        }
        if let Some(inner) = strip_catalog_wrapper(src, "Option") {
            let elem = self.stdlib_signature_ty(inner)?;
            return Some(self.option_adt_ty(elem));
        }
        for (spelling, ordered) in [("Map", false), ("BTreeMap", true)] {
            if let Some(inner) = strip_catalog_wrapper(src, spelling) {
                let parts = crate::stdlib_signatures::split_top_level(inner, ',');
                let [key_src, value_src] = parts.as_slice() else {
                    return None;
                };
                let key = self.stdlib_signature_ty(key_src)?;
                let value = self.stdlib_signature_ty(value_src)?;
                return Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered,
                }));
            }
        }
        if let Some(inner) = strip_catalog_wrapper(src, "Result") {
            let parts = crate::stdlib_signatures::split_top_level(inner, ',');
            let [ok_src, err_src] = parts.as_slice() else {
                return None;
            };
            let ok = self.stdlib_signature_ty(ok_src)?;
            let err = self.stdlib_signature_ty(err_src)?;
            return Some(self.result_adt_ty(ok, err));
        }
        if let Some(inner) = src.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            if inner.trim().is_empty() {
                return Some(self.tcx.unit());
            }
            let elems = crate::stdlib_signatures::split_top_level(inner, ',')
                .into_iter()
                .map(|part| self.stdlib_signature_ty(part))
                .collect::<Option<Vec<_>>>()?;
            return Some(self.tcx.intern(TyKind::Tuple(elems)));
        }
        // A nominal stdlib handle resolves to the same sentinel Adt a written
        // annotation gets, so a signature slot naming one carries its fields
        // rather than an inference variable.
        let tail = src.rsplit("::").next().unwrap_or(src);
        if let Some(offset) = stdlib_handle_def_offset(tail) {
            let def = gossamer_resolve::DefId::local(u32::MAX - offset);
            // A socket or filesystem handle keeps the qualified name the
            // annotation path registers, so one `DefId` never carries two
            // spellings.
            let name = stdlib_net_handle(tail)
                .or_else(|| stdlib_fs_handle(tail))
                .map_or_else(|| tail.to_string(), |(_, n)| n.to_string());
            self.tcx.register_def_name(def, &name);
            return Some(self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            }));
        }
        None
    }

    /// Re-records literal nodes to a type discovered by *joining*
    /// sibling branches - `if c { [1, 2] } else { [3] }` joins to
    /// `Vec<i64>` only after both arms are checked, so the arm
    /// literals (and the wrapper nodes codegen sizes result slots
    /// from) are re-shaped afterwards. This is the synthesis-side
    /// complement of [`Expectation`], which handles every site where
    /// the expected type is known *before* checking.
    /// Bare nominal name of a struct/enum type, seeing through `&`/`&mut`.
    /// Returns `None` for non-ADT types. Used to look up operator-overload
    /// impl methods (`V2::add`).
    /// Nominal-type name of an operator operand: a user ADT, or an opaque
    /// alias.
    ///
    /// An opaque alias inherits nothing from its representation, so
    /// arithmetic on one routes to the alias's own operator impl and is
    /// rejected when it has none - the same contract a struct or enum
    /// operand gets. Comparison, hashing and formatting are unaffected;
    /// those describe the value, which the alias and its representation
    /// share.
    fn operand_nominal_name_of(&mut self, ty: Ty) -> Option<String> {
        let mut r = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Nominal { def, .. }) = self.tcx.kind(r) {
            return self.tcx.def_name(*def).map(str::to_string);
        }
        self.adt_name_of(ty)
    }

    fn adt_name_of(&mut self, ty: Ty) -> Option<String> {
        let mut r = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(r) {
            self.tcx.def_name(*def).map(str::to_string)
        } else {
            None
        }
    }

    /// Return type of the operator-overload impl method `method` (with
    /// `arity` non-receiver parameters) on an ADT operand, seeing through
    /// `&` / `&mut`. Covers non-generic impls directly and generic impls
    /// (`impl<T> Add for Wrap<T>`) by substituting the operand
    /// instantiation's generic arguments into the stored return type.
    /// `None` when the operand is not an ADT or carries no such impl.
    fn adt_op_method_ret(&mut self, ty: Ty, method: &str, arity: usize) -> Option<Ty> {
        let mut r = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        // An opaque alias carries operator impls under its own name, and
        // takes no generic arguments of its own.
        let (def, substs) = match self.tcx.kind(r) {
            Some(TyKind::Adt { def, substs }) => (*def, substs.clone()),
            Some(TyKind::Nominal { def, .. }) => (*def, crate::Substs::new()),
            _ => return None,
        };
        let name = self.tcx.def_name(def)?.to_string();
        if let Some(&ret) = self
            .method_ret_types
            .get(&(name.clone(), method.to_string(), arity))
        {
            return Some(ret);
        }
        let &ret = self
            .generic_method_ret_types
            .get(&(name, method.to_string(), arity))?;
        let subst_tys = substs.types();
        Some(self.subst_params_in_ty(ret, &subst_tys))
    }

    /// `time::Duration` / `time::Instant` accessors in method form
    /// (`d.as_millis()`, `inst.elapsed_ms()`) mirror the qualified free
    /// calls; all yield a bare `i64`. `None` for every other receiver.
    fn time_accessor_method_ret(
        &mut self,
        resolved: Ty,
        method: &str,
        args: &[Expr],
    ) -> Option<Ty> {
        if !args.is_empty() {
            return None;
        }
        let duration = matches!(self.tcx.kind(resolved), Some(TyKind::Duration))
            && matches!(method, "as_millis" | "as_secs" | "as_micros");
        let instant =
            matches!(self.tcx.kind(resolved), Some(TyKind::Instant)) && method == "elapsed_ms";
        (duration || instant).then(|| self.tcx.int_ty(IntTy::I64))
    }

    /// Return type of `method` (with `arity` non-receiver arguments) called
    /// on a generic-instantiation receiver, from the generic impl's declared
    /// return with the instantiation's arguments substituted. `None` for
    /// non-generic receivers or unknown methods.
    fn generic_recv_method_ret(&mut self, resolved: Ty, method: &str, arity: usize) -> Option<Ty> {
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved) else {
            return None;
        };
        let substs = substs.clone();
        if substs.types().is_empty() {
            return None;
        }
        let name = self.tcx.def_name(*def)?.to_string();
        let &ret = self
            .generic_method_ret_types
            .get(&(name, method.to_string(), arity))?;
        let subst_tys = substs.types();
        Some(self.subst_params_in_ty(ret, &subst_tys))
    }

    /// Coerces a byte literal compared against an integer operand to that
    /// operand's integer type, so `s[i] == b'>'` type-checks without a cast.
    /// Returns true when it applied (caller then skips the same-type unify).
    fn coerce_byte_literal_cmp(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr, rhs_ty: Ty) -> bool {
        let is_byte_lit = |e: &Expr| matches!(&e.kind, ExprKind::Literal(Literal::Byte(_)));
        let lr = self.infer.resolve(self.tcx, lhs_ty);
        let rr = self.infer.resolve(self.tcx, rhs_ty);
        if is_byte_lit(lhs) && self.is_integer(rr) {
            self.record(lhs.id, rr);
            true
        } else if is_byte_lit(rhs) && self.is_integer(lr) {
            self.record(rhs.id, lr);
            true
        } else {
            false
        }
    }

    fn adjust_literal_to_join(&mut self, expr: &Expr, expected: Ty) {
        let expected = self.infer.resolve(self.tcx, expected);
        let expected = match self.tcx.kind(expected) {
            Some(TyKind::Ref { inner, .. }) => *inner,
            _ => expected,
        };
        match &expr.kind {
            // `&[..]` / `&mut [..]`: the borrow is transparent at the
            // layout level - re-type the borrowed literal itself
            // (expected already had its `Ref` stripped above).
            ExprKind::Unary {
                op: UnaryOp::RefShared | UnaryOp::RefMut,
                operand,
            } => self.adjust_literal_to_join(operand, expected),
            ExprKind::Array(ArrayExpr::List(elems)) => {
                let _ = elems;
            }
            ExprKind::Array(ArrayExpr::Repeat { value, .. }) => {
                let _ = value;
            }
            ExprKind::Tuple(elems) => {
                if let Some(TyKind::Tuple(tys)) = self.tcx.kind(expected).cloned() {
                    if tys.len() == elems.len() {
                        self.record(expr.id, expected);
                        for (el, t) in elems.iter().zip(tys) {
                            self.adjust_literal_to_join(el, t);
                        }
                    }
                }
            }
            // Push the expected type through value-producing positions so
            // a literal in a block tail / branch / arm is re-recorded too:
            // `fn f() -> Vec<T> { [..] }`,
            // `let v: Vec<T> = if c { [..] } else { [..] }`. The wrapping
            // node is re-recorded as well - codegen sizes the block/if/
            // match result slot from its node type, so leaving it `[T; N]`
            // while the branches build a heap Vec would desync the slot.
            ExprKind::Block(block) | ExprKind::Unsafe(block) => {
                if let Some(tail) = &block.tail {
                    self.record(expr.id, expected);
                    self.adjust_literal_to_join(tail, expected);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.record(expr.id, expected);
                self.adjust_literal_to_join(then_branch, expected);
                if let Some(else_branch) = else_branch {
                    self.adjust_literal_to_join(else_branch, expected);
                }
            }
            ExprKind::Match { arms, .. } => {
                self.record(expr.id, expected);
                for arm in arms {
                    self.adjust_literal_to_join(&arm.body, expected);
                }
            }
            _ => {}
        }
    }

    fn option_adt_ty(&mut self, payload: Ty) -> Ty {
        let substs = crate::Substs::from_types([payload]);
        let def = gossamer_resolve::DefId::local(u32::MAX - 1);
        self.tcx.register_def_name(def, "Option");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn hashset_ty(&mut self, elem: Ty) -> Ty {
        self.set_ty("Set", elem)
    }

    fn btreeset_ty(&mut self, elem: Ty) -> Ty {
        self.set_ty("BTreeSet", elem)
    }

    fn vecdeque_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(VEC_DEQUE_DEF_LOCAL);
        self.tcx.register_def_name(def, "Deque");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn vecqueue_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(VEC_QUEUE_DEF_LOCAL);
        self.tcx.register_def_name(def, "Queue");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn vecstack_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(VEC_STACK_DEF_LOCAL);
        self.tcx.register_def_name(def, "Stack");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn binary_heap_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(BINARY_HEAP_DEF_LOCAL);
        self.tcx.register_def_name(def, "MaxHeap");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn min_heap_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(MIN_HEAP_DEF_LOCAL);
        self.tcx.register_def_name(def, "MinHeap");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    /// The element type a slot-backed container's methods read and write. An
    /// element the constructor never pinned - `Queue::new()` with no
    /// annotation and no `push` yet - settles as `i64`, the width a slot
    /// holds.
    /// The element type a slot-backed container's methods read and write,
    /// without re-checking it. The declaration that pinned the element is
    /// where an element the container cannot hold is reported; a call on the
    /// receiver reads whatever was written there.
    fn slot_collection_elem_as_written(&mut self, elem: Option<Ty>) -> Ty {
        elem.unwrap_or_else(|| self.tcx.int_ty(IntTy::I64))
    }

    /// Checks the element type of a slot-backed container (`Deque`, `Queue`,
    /// `Stack`, `MaxHeap`, `MinHeap`).
    ///
    /// A `Deque` / `Queue` / `Stack` stores and hands back; it holds an
    /// element of any type, in the same element store a `Vec<T>` uses. A heap
    /// also orders its elements, so its element must be one the language
    /// orders: every scalar, a `String`, a tuple, a struct, an array, a
    /// sequence, an `Option` / `Result`, and any nesting of those. A `Map` or
    /// a `Set` has no ordering, and a `u64` / `usize` runs past the signed
    /// range the heap compares by, so both are declined there. An unresolved
    /// element is left to the push that pins it.
    fn require_slot_collection_elem(&mut self, elem: Ty, owner: &str, span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, elem);
        if !matches!(owner, "MaxHeap" | "MinHeap" | "BinaryHeap") {
            return elem;
        }
        self.require_container_reads_the_types_order(resolved, owner, span);
        if self.is_orderable_elem(resolved) {
            return elem;
        }
        let found = self.render_public_ty(resolved);
        self.emit(
            TypeError::SlotCollectionElement {
                owner: owner.to_string(),
                found,
            },
            span,
        );
        // Recovery keeps the element the annotation named, so the pushes and
        // pops that follow are checked against it rather than reported a
        // second time against a substituted `i64`.
        elem
    }

    /// Reports a container that orders its elements as it stores them when the
    /// element writes its own `cmp`.
    ///
    /// A sequence orders on demand, so its ordering calls route through the
    /// type's comparator. A heap and a sorted set or map keep their elements
    /// in the order they were stored in, reached with no comparator to call,
    /// so the order the type declares would silently not be the one read back.
    fn require_container_reads_the_types_order(&mut self, elem: Ty, owner: &str, span: Span) {
        let resolved = self.infer.resolve(self.tcx, elem);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def) else {
            return;
        };
        let bare = name.rsplit("::").next().unwrap_or(name).to_string();
        if !self.user_ordered_types.contains(&bare) {
            return;
        }
        let elem = self.render_public_ty(resolved);
        self.emit(
            TypeError::ContainerIgnoresUserOrder {
                owner: owner.to_string(),
                elem,
            },
            span,
        );
    }

    /// Whether values of `ty` have an ordering: the scalars, `String`, and
    /// every aggregate whose parts are themselves ordered. A `u64` / `usize`
    /// spans past the signed comparison a heap slot orders by, and a `Map` or
    /// `Set` has no element order at all.
    fn is_orderable_elem(&mut self, ty: Ty) -> bool {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Int(IntTy::U64 | IntTy::Usize)) => false,
            Some(
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Var(_),
            ) => true,
            Some(TyKind::Ref { inner, .. }) => {
                let inner = *inner;
                self.is_orderable_elem(inner)
            }
            Some(TyKind::Vec(inner) | TyKind::Slice(inner) | TyKind::Array { elem: inner, .. }) => {
                let inner = *inner;
                self.is_orderable_elem(inner)
            }
            Some(TyKind::Tuple(elems)) => {
                let elems = elems.clone();
                elems.into_iter().all(|e| self.is_orderable_elem(e))
            }
            Some(TyKind::Adt { def, substs }) => {
                let (def, substs) = (*def, substs.clone());
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) {
                    return false;
                }
                // `Option` / `Result` order by arm, then by payload; a user
                // struct or enum orders by its fields in declaration order.
                if def.local == u32::MAX || def.local == u32::MAX - 1 {
                    return substs.types().iter().all(|t| self.is_orderable_elem(*t));
                }
                if let Some(fields) = self.tcx.struct_field_tys(def) {
                    let fields = fields.to_vec();
                    return fields.into_iter().all(|f| self.is_orderable_elem(f));
                }
                if let Some(variants) = self.tcx.enum_variant_tys(def) {
                    let variants: Vec<Vec<Ty>> = variants.to_vec();
                    return variants
                        .into_iter()
                        .all(|fields| fields.into_iter().all(|f| self.is_orderable_elem(f)));
                }
                false
            }
            _ => false,
        }
    }

    fn reverse_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(REVERSE_DEF_LOCAL);
        self.tcx.register_def_name(def, "Reverse");
        self.tcx.register_tuple_struct(def.local);
        self.tcx.register_struct_fields(def, vec![elem]);
        self.tcx
            .register_struct_fields_inst(def, substs.clone(), vec![elem]);
        self.struct_fields
            .insert(def, vec![("0".to_string(), elem)]);
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn set_ty(&mut self, owner: &str, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let (local, name) = match owner {
            "BTreeSet" => (BTREE_SET_DEF_LOCAL, "BTreeSet"),
            _ => (HASH_SET_DEF_LOCAL, "Set"),
        };
        let def = gossamer_resolve::DefId::local(local);
        self.tcx.register_def_name(def, name);
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn set_elem_ty(&self, ty: Ty) -> Option<(String, Ty)> {
        match self.tcx.kind(ty) {
            Some(TyKind::Adt { def, substs })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
            {
                let owner = if def.local == BTREE_SET_DEF_LOCAL {
                    "BTreeSet"
                } else {
                    "Set"
                };
                substs
                    .types()
                    .first()
                    .copied()
                    .map(|elem| (owner.to_string(), elem))
            }
            _ => None,
        }
    }

    /// Returns true when `.downgrade()` on this (already ref-peeled)
    /// receiver type has no runtime RC header: a by-value scalar, `Unit` /
    /// `Never`, a transparent time newtype, `Option` / `Result`, or an
    /// inline (2-word by-value) enum. Such a value carries no pointer for
    /// `gos_rt_rc_downgrade` to read, so a `Weak` of it faults on the
    /// compiled tiers.
    fn downgrade_receiver_is_non_rc(&self, ty: Ty) -> bool {
        match self.tcx.kind(ty) {
            Some(
                TyKind::Bool
                | TyKind::Char
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Unit
                | TyKind::Never
                | TyKind::Duration
                | TyKind::Instant,
            ) => true,
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                true
            }
            Some(TyKind::Adt { .. }) => self.tcx.is_inline_enum_ty(ty),
            _ => false,
        }
    }

    fn weak_adt_ty(&mut self, payload: Ty) -> Ty {
        let substs = crate::Substs::from_types([payload]);
        let def = gossamer_resolve::DefId::local(u32::MAX - 6);
        self.tcx.register_def_name(def, "Weak");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    /// `value.downgrade()` produces a `Weak<T>` for any RC-managed aggregate;
    /// `weak.upgrade()` produces `Option<T>` from a `Weak<T>`. Name-global
    /// dispatch resolved these while the receiver type was an unresolved
    /// variable; a concretely-typed receiver (e.g. an enum bound from a
    /// variant constructor) needs the explicit rule. Returns `None` for any
    /// other method / receiver so normal dispatch continues.
    fn check_weak_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        receiver_span: Span,
    ) -> Option<Ty> {
        if !args.is_empty() {
            return None;
        }
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if method == "downgrade" {
            // `downgrade` needs a runtime RC pointer to bump the weak count.
            // A by-value word (scalar / `Option` / `Result` / other packed
            // value) has no header, so `gos_rt_rc_downgrade` reads a bogus
            // header off the value's bits and faults on the compiled tiers
            // (the VM hands back a nonsense handle). Reject it here rather
            // than let name-global dispatch type it to `Weak<T>`. An
            // unresolved receiver (`Var`) carries no decision - leave it for
            // normal dispatch so a later-inferred aggregate still works.
            if self.downgrade_receiver_is_non_rc(resolved) {
                let ty = self.render_public_ty(resolved);
                self.emit(TypeError::WeakDowngradeNonRc { ty }, receiver_span);
                return Some(self.tcx.error_ty());
            }
            if matches!(self.tcx.kind(resolved), Some(TyKind::Adt { .. })) {
                return Some(self.weak_adt_ty(resolved));
            }
            // An unresolved receiver (an unsuffixed literal that defaults
            // later, e.g. `let x = 5; x.downgrade()`) defers to the
            // post-defaulting pass, which rejects it if it lands on a
            // by-value scalar.
            if matches!(self.tcx.kind(resolved), Some(TyKind::Var(_))) {
                self.deferred_structural.push(DeferredStructural {
                    ty: resolved,
                    span: receiver_span,
                    kind: DeferredStructuralKind::Downgrade,
                    result: None,
                });
            }
            return None;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs })
                if def.local == u32::MAX - 6 && method == "upgrade" =>
            {
                let payload = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                Some(self.option_adt_ty(payload))
            }
            _ => None,
        }
    }

    /// `recv` / `try_recv` on a `Receiver<T>` yields `Option<T>`, and `send`
    /// / `try_send` on a `Sender<T>` consumes a `T`. Pinning the element type
    /// here sizes a `while let Some(p) = rx.recv()` binding by `T`'s real slot
    /// count, so a struct sent over a channel materialises as its inline
    /// fields rather than a single pointer word. Returns `None` for any
    /// non-channel receiver so the caller continues normal dispatch.
    fn check_channel_method(&mut self, method: &str, receiver_ty: Ty, args: &[Expr]) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Receiver(elem)) if matches!(method, "recv" | "try_recv" | "recv_ctx") => {
                let elem = *elem;
                for arg in args {
                    self.check_expr(arg);
                }
                Some(self.option_adt_ty(elem))
            }
            // `join` on a `JoinHandle<T>` yields `Result<T, String>`: the
            // goroutine's value, or the message it panicked with. Pinning
            // it here is what lets `handle.join()?` propagate, and what
            // gives the binding `T`'s real shape rather than a bare word.
            Some(TyKind::JoinHandle(elem)) if method == "join" && args.is_empty() => {
                let elem = *elem;
                let message = self.tcx.string_ty();
                Some(self.result_adt_ty(elem, message))
            }
            Some(TyKind::Sender(elem)) if matches!(method, "send" | "try_send") => {
                let elem = *elem;
                for arg in args {
                    let v = self.check_expr(arg);
                    self.unify(elem, v, arg.span);
                    let v = self.infer.resolve(self.tcx, v);
                    if self.ty_contains_reference(v) {
                        self.emit(
                            TypeError::ReferenceEscapeUnsupported {
                                context: "be stored in a channel".to_string(),
                            },
                            arg.span,
                        );
                    }
                }
                Some(self.tcx.unit())
            }
            _ => None,
        }
    }

    /// Phase 1 `VecDeque` support is backed by the native `i64` deque ABI.
    /// Keep method typing aligned with that runtime until the handle is made
    /// fully generic.
    fn check_deque_method(&mut self, method: &str, receiver_ty: Ty, args: &[Expr]) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved)
            && def.local == VEC_DEQUE_DEF_LOCAL
        {
            let elem = self.slot_collection_elem_as_written(substs.types().first().copied());
            return match method {
                "push_back" | "push_front" if args.len() == 1 => {
                    let v = self.check_expr_expecting(&args[0], Expectation::HasType(elem));
                    self.unify(elem, v, args[0].span);
                    Some(self.tcx.unit())
                }
                "pop_back" | "pop_front" | "peek_back" | "peek_front" if args.is_empty() => {
                    Some(self.option_adt_ty(elem))
                }
                "len" if args.is_empty() => Some(self.tcx.int_ty(IntTy::I64)),
                "is_empty" if args.is_empty() => Some(self.tcx.bool_ty()),
                "clear" if args.is_empty() => Some(self.tcx.unit()),
                _ => None,
            };
        }
        None
    }

    fn check_queue_or_stack_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved)
            && matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL)
        {
            let elem = self.slot_collection_elem_as_written(substs.types().first().copied());
            return match method {
                "push" if args.len() == 1 => {
                    let v = self.check_expr_expecting(&args[0], Expectation::HasType(elem));
                    self.unify(elem, v, args[0].span);
                    Some(self.tcx.unit())
                }
                "pop" | "peek" if args.is_empty() => Some(self.option_adt_ty(elem)),
                "len" if args.is_empty() => Some(self.tcx.int_ty(IntTy::I64)),
                "is_empty" if args.is_empty() => Some(self.tcx.bool_ty()),
                "clear" if args.is_empty() => Some(self.tcx.unit()),
                _ => None,
            };
        }
        None
    }

    fn check_binary_heap_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        span: gossamer_lex::Span,
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let elem_ty = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                substs.types().first().copied()
            }
            _ => return None,
        };
        let elem_ty = self.slot_collection_elem_as_written(elem_ty);
        match method {
            "push" if args.len() == 1 => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(elem_ty));
                self.unify(elem_ty, got, args[0].span);
                Some(self.tcx.unit())
            }
            "pop" | "peek" if args.is_empty() => Some(self.option_adt_ty(elem_ty)),
            "len" if args.is_empty() => Some(self.tcx.int_ty(IntTy::I64)),
            "is_empty" if args.is_empty() => Some(self.tcx.bool_ty()),
            "clear" if args.is_empty() => Some(self.tcx.unit()),
            "push" | "pop" | "peek" | "len" | "is_empty" | "clear" => {
                let expected = usize::from(method == "push");
                let owner = self.render_public_ty(resolved);
                self.emit(
                    TypeError::CallArityMismatch {
                        callee: format!("{owner}::{method}"),
                        expected,
                        found: args.len(),
                    },
                    span,
                );
                Some(self.tcx.error_ty())
            }
            _ => None,
        }
    }

    /// Expected closure parameter types for a `Vec`/slice/array
    /// closure-combinator method (`xs.sort_by(cmp)`, `xs.map(f)`), or
    /// `None` when the method is not such a combinator or the receiver is
    /// not a sequence. Comparator-shaped methods take two element
    /// parameters; the rest take one.
    fn vec_combinator_closure_inputs(&mut self, method: &str, receiver_ty: Ty) -> Option<Vec<Ty>> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        // An `Option` / `Result` receiver hands its payload to the closure the
        // same way a sequence hands over its element. Without this the closure
        // body is checked against an unconstrained parameter, so a projection
        // out of the payload never resolves and the mapped payload stays a
        // free variable.
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved) {
            let payload_family = match def.local {
                d if d == u32::MAX - 1 => Some(0),
                d if d == u32::MAX => Some(0),
                _ => None,
            };
            if let Some(index) = payload_family {
                let payloads = substs.types();
                let ok = payloads.get(index).copied();
                let err = payloads.get(1).copied();
                if let Some(ok) = ok {
                    return match method {
                        "map" | "and_then" | "filter" | "is_some_and" | "inspect" => Some(vec![ok]),
                        "map_err" | "or_else" => err.map(|e| vec![e]),
                        _ => None,
                    };
                }
            }
        }
        let elem = match self.tcx.kind(resolved) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. }
                | TyKind::Iterator(elem),
            ) => *elem,
            // A map hands the closure its key/value pair, a set its value.
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.tcx.intern(TyKind::Tuple(vec![key, value]))
            }
            _ => self.set_elem_ty(resolved).map(|(_owner, elem)| elem)?,
        };
        match method {
            "sort_by" | "min_by" | "max_by" => Some(vec![elem, elem]),
            "sort_by_key" | "min_by_key" | "max_by_key" | "map" | "filter" | "filter_map"
            | "flat_map" | "for_each" | "any" | "all" | "find" | "position" | "find_map"
            | "take_while" | "skip_while" | "partition" | "chunk_by" | "count_by" | "sum_by"
            | "product_by" => Some(vec![elem]),
            _ => None,
        }
    }

    /// The call's argument types with the piped value appended.
    /// `x |> recv.m(a)` desugars to `recv.m(a, x)`, so the built-in
    /// receiver surface sees the piped value as the trailing argument:
    /// it counts toward the method's arity and its type is checked
    /// against the slot it lands in.
    fn arg_tys_with_piped(&self, call_id: NodeId, arg_tys: &[Ty]) -> Vec<Ty> {
        let mut tys = arg_tys.to_vec();
        if let Some(piped) = self.pipe_stage_arg_tys.get(&call_id).copied() {
            tys.push(piped);
        }
        tys
    }

    #[allow(
        clippy::too_many_lines,
        reason = "receiver dispatch is intentionally kept in source order"
    )]
    fn check_method_call(
        &mut self,
        site: MethodCallSite<'_>,
        receiver: &Expr,
        args: &[Expr],
        expected: Expectation,
    ) -> Ty {
        let MethodCallSite {
            call_id,
            method,
            name_span,
            generics,
        } = site;
        self.check_overlapping_mutable_call_args(args);
        let receiver_expected = self.method_receiver_expectation(method, receiver, expected);
        let receiver_ty = self.check_expr_expecting(receiver, receiver_expected);
        if self.reject_invalid_builtin_receiver_call(receiver_ty, method, args, call_id, name_span)
        {
            return self.tcx.error_ty();
        }
        self.check_mutating_method_receiver(receiver, receiver_ty, method);
        self.reject_private_method_call(receiver_ty, method, receiver.span);
        if let Some(ty) = self.check_param_receiver_method(receiver_ty, method, args, receiver.span)
        {
            return ty;
        }
        if let Some(ty) = self.reject_method_on_receiver(receiver_ty, method, args, receiver.span) {
            return ty;
        }
        // Which `impl` block this call reaches is decided by the receiver's
        // type, which is known here and nowhere later: a container and a
        // structural type both reach a method as an untyped handle below.
        self.record_method_owner(call_id, receiver_ty, method);
        // A method an `impl` block declared for a built-in type. Its own
        // surface is dispatched below and answers first, so an impl adds
        // names to a type rather than replacing any it already had.
        if let Some(ty) = self.user_impl_method_on_builtin(receiver_ty, method, args) {
            return ty;
        }
        // `wg.wait_ctx(ctx)` answers whether the group completed. A sync
        // handle's receiver stays an inference variable by design, so the
        // name carries the return type; no other receiver declares it.
        if method == "wait_ctx" && args.len() == 1 {
            for arg in args {
                self.check_expr(arg);
            }
            return self.tcx.bool_ty();
        }
        if let Some(ty) = self.check_channel_method(method, receiver_ty, args) {
            return ty;
        }
        if let Some(ty) = self.check_weak_method(method, receiver_ty, args, receiver.span) {
            return ty;
        }
        // `x.into()` converts to an inferred target `B` via `B::from`, and
        // `x.try_into()` to `Result<B, E>` via `B::try_from`. The target is
        // fixed by the use site (a `let B` / `let Result<B, E>`, a parameter,
        // a return), so type it as a fresh variable here and let unification
        // bind it; lowering reads the resolved type and routes accordingly.
        if matches!(method, "into" | "try_into") && args.is_empty() {
            return self.check_conversion_method(method, receiver_ty, receiver.span);
        }
        if let Some(ty) = self.check_deque_method(method, receiver_ty, args) {
            return ty;
        }
        if let Some(ty) = self.check_queue_or_stack_method(method, receiver_ty, args) {
            return ty;
        }
        if let Some(ty) = self.check_binary_heap_method(method, receiver_ty, args, receiver.span) {
            return ty;
        }
        // Result/Option combinator methods (`r.map_err(f)`,
        // `o.map(f)`) have known signatures: type them through the
        // std combinator table so closure params pin to the payload
        // type instead of falling through unresolved.
        if let Some(ty) =
            self.check_payload_combinator_method(method, receiver_ty, receiver.span, args)
        {
            return ty;
        }
        let arg_tys = self.check_method_call_arg_tys(method, receiver_ty, args);
        let all_arg_tys = self.arg_tys_with_piped(call_id, &arg_tys);
        let arg_count = all_arg_tys.len();
        // When the receiver resolves to a non-generic Adt with a
        // recorded method return type, use it: a fresh var here
        // leaves chained results (`sel.params()`) untyped all the
        // way into codegen.
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if method == "clone" && args.is_empty() {
            return resolved;
        }
        if let Some(ty) = self.check_display_to_string(method, resolved, args) {
            return ty;
        }
        if matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Int(_) | TyKind::Var(_))
        ) && matches!(method, "wrapping_add" | "wrapping_mul")
        {
            if arg_count != 1 {
                let owner = self.render_public_ty(resolved);
                self.emit(
                    TypeError::CallArityMismatch {
                        callee: format!("{owner}::{method}"),
                        expected: 1,
                        found: arg_count,
                    },
                    receiver.span,
                );
                return self.tcx.error_ty();
            }
            let arg_ty = self.peel_refs(all_arg_tys[0]);
            let arg_span = args.first().map_or(receiver.span, |arg| arg.span);
            self.unify(resolved, arg_ty, arg_span);
            return resolved;
        }
        if self.reject_collection_method_arity(resolved, method, arg_count, receiver.span) {
            return self.tcx.error_ty();
        }
        if self.reject_unknown_deque_method(resolved, method, arg_count, receiver.span) {
            return self.tcx.error_ty();
        }
        if let Some(ty) =
            self.validate_handle_method_ret(method, args, &arg_tys, resolved, receiver.span)
        {
            return ty;
        }
        self.check_user_method_args(resolved, method, args, &arg_tys);
        if method == "where_eq"
            && args.len() == 2
            && let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved)
            && self.tcx.def_name(*def) == Some("__gos_sql_Select")
            && let Some(value_ty) = self.tcx.enum_ty_by_name("__gos_sql_Value")
        {
            self.unify(value_ty, arg_tys[1], args[1].span);
        }
        if let Some(ty) = self.time_accessor_method_ret(resolved, method, args) {
            return ty;
        }
        if let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved)
            && substs.types().is_empty()
            && let Some(name) = self.tcx.def_name(*def)
            && let Some(&ret) =
                self.method_ret_types
                    .get(&(name.to_string(), method.to_string(), arg_count))
        {
            return ret;
        }
        // A generic-instantiation receiver (`Wrap<f64>`) types the call
        // from the generic impl's return with the instantiation's
        // arguments substituted, so chained uses resolve concretely.
        if let Some(ret) = self.generic_recv_method_ret(resolved, method, arg_count) {
            return ret;
        }
        if let Some(ty) = self.vec_method_ret(method, &all_arg_tys, resolved, receiver.span) {
            return ty;
        }
        if let Some(ty) = self.seq_combinator_method_ret(method, &all_arg_tys, resolved, name_span)
        {
            self.mark_consumed_iterator_expr(method, receiver, resolved);
            return ty;
        }
        if let Some(ty) = self.set_method_ret(method, &all_arg_tys, resolved, receiver.span) {
            return ty;
        }
        if self.reject_unknown_set_method(resolved, method, arg_count, receiver.span) {
            return self.tcx.error_ty();
        }
        if let Some(ty) = self.map_method_ret(method, &all_arg_tys, resolved, receiver.span) {
            return ty;
        }
        if let Some(ty) = self.flag_set_method_ret(method, resolved) {
            return ty;
        }
        if let Some(ty) = self.shared_method_ret(method, resolved, args) {
            return ty;
        }
        if let Some(ty) = self.http_client_method_ret(method, resolved) {
            return ty;
        }
        if let Some(ty) = self.handle_family_method_ret(method, args, &arg_tys, resolved, receiver)
        {
            return ty;
        }
        if let Some(ty) = self.net_handle_method_ret(method, resolved, arg_count, receiver.span) {
            return ty;
        }
        // A `json::Value` answers the same surface in method form that
        // `json::` does as free functions, so it is typed from the same
        // table. Falling through to a fresh variable would strip the
        // JsonValue tag - leaving a chained `.set(..).set(..)` receiver
        // untagged for the compiled tiers - and would let a document read
        // bind to any annotation the caller wrote.
        if let Some(ty) = self.opaque_value_method_ret(resolved, method, arg_count) {
            return ty;
        }
        if method != "clone"
            && self.reject_unknown_sequence_method(resolved, method, arg_count, receiver.span)
        {
            return self.tcx.error_ty();
        }
        if let Some(ty) = self.check_string_receiver_method(
            call_id,
            method,
            generics,
            expected,
            resolved,
            receiver.span,
            args,
            &all_arg_tys,
        ) {
            return ty;
        }
        self.check_unsurfaced_method(call_id, method, resolved, args, arg_count, receiver.span)
    }

    /// Types a call the surfaced-receiver arms did not claim: the `math`
    /// surface reached on a scalar, then the reports for a name no
    /// receiver of this type answers. A fresh variable is the historical
    /// fallback for a receiver the checker has no surface for at all.
    fn check_unsurfaced_method(
        &mut self,
        call_id: NodeId,
        method: &str,
        resolved: Ty,
        args: &[Expr],
        arg_count: usize,
        span: Span,
    ) -> Ty {
        if let Some(ty) = self.float_bits_method_ret(method, resolved, arg_count) {
            return ty;
        }
        if let Some(ty) = self.check_numeric_receiver_method(method, resolved, arg_count) {
            return ty;
        }
        if let Some(name) = self.payload_adt_method_owner(resolved)
            && !matches!(method, "clone")
        {
            let error = self.unresolved_method(name.to_string(), method, resolved);
            self.emit(error, span);
            return self.tcx.error_ty();
        }
        self.check_method_arity(call_id, resolved, method, args, span);
        self.maybe_reject_unknown_adt_method(resolved, method, span);
        if self.reject_unknown_scalar_method(resolved, method, span) {
            return self.tcx.error_ty();
        }
        if self.reject_unknown_stdlib_handle_method(resolved, method, span) {
            return self.tcx.error_ty();
        }
        self.fresh()
    }

    /// Rejects `req.nowhere()` on a handle whose method surface the checker
    /// owns in full. Most runtime handles resolve their methods in the tier
    /// lowerings, so an unknown name on one has to stay open; the handles
    /// listed here answer a closed table in [`Self::http_client_method_ret`],
    /// and a name that table does not claim has no binding on any tier. The
    /// VM refuses such a call and the native build ends with an undefined
    /// `@name` symbol once the whole program has compiled, so naming it at
    /// the call site is what shows the reader the receiver's own surface.
    fn reject_unknown_stdlib_handle_method(
        &mut self,
        resolved: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        const CLOSED_HANDLE_SURFACES: &[&str] = &[
            "http::Client",
            "http::ClientBuilder",
            "http::Request",
            "http::ResponseStream",
        ];
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return false;
        };
        let def = *def;
        if def.local < u32::MAX - HANDLE_SENTINEL_SPAN {
            return false;
        }
        let Some(name) = self.tcx.def_name(def).map(str::to_string) else {
            return false;
        };
        if !CLOSED_HANDLE_SURFACES.contains(&name.as_str()) {
            return false;
        }
        // The conversions every value answers, and any name a user impl or
        // trait declares for this receiver, keep their own resolution.
        if matches!(method, "clone" | "into" | "try_into" | "to_string")
            || self.user_method_owners.contains_key(method)
        {
            return false;
        }
        let error = self.unresolved_method(name, method, resolved);
        self.emit(error, span);
        true
    }

    /// Rejects `x.nowhere()` on a scalar receiver. A scalar declares no
    /// methods of its own: the surface it answers is the `math` row set,
    /// the conversions, and a `use` that binds the name as a free value.
    /// Anything else has no binding on any tier and can only fail at run
    /// time, so it is named here instead.
    fn reject_unknown_scalar_method(&mut self, resolved: Ty, method: &str, span: Span) -> bool {
        // An unsuffixed literal is still an inference variable here: its
        // width is pinned by defaulting once every item is checked, so the
        // report waits until the scalar it names is known.
        let literal = self.infer.is_integer_constrained_var(self.tcx, resolved)
            || self.infer.is_float_literal_var(self.tcx, resolved);
        if !literal
            && !matches!(
                self.tcx.kind(resolved),
                Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
            )
        {
            return false;
        }
        let declared = self.user_method_owners.contains_key(method)
            || gossamer_resolve::is_prelude_value(method)
            || self.import_binds_free_name(method)
            // The conversions every value answers, which no signature row
            // describes. `cmp`, `eq`, `fmt`, and `hash` are absent on
            // purpose: a scalar orders with `<`, compares with `==`, renders
            // through `{}`, and keys a map by value, so no tier binds a
            // method spelling for them - the same surface `Vec`, `Map`,
            // `Set`, and a tuple present.
            || matches!(
                method,
                "clone"
                    | "into"
                    | "to_string"
                    | "try_into"
                    | "wrapping_add"
                    | "wrapping_mul"
            );
        if declared {
            return false;
        }
        if literal {
            self.deferred_scalar_method_rejections
                .push((resolved, method.to_string(), span));
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method(ty, method, resolved);
        self.emit(error, span);
        true
    }

    /// Reports `x.name()` on a numeric literal, once defaulting has given
    /// the literal the width its diagnostic names.
    fn check_deferred_scalar_method_rejections(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_scalar_method_rejections);
        for (resolved, method, span) in deferred {
            let resolved = self.deep_resolve(resolved);
            if !matches!(
                self.tcx.kind(resolved),
                Some(TyKind::Int(_) | TyKind::Float(_))
            ) {
                continue;
            }
            let ty = self.render_public_ty(resolved);
            let error = self.unresolved_method(ty, &method, resolved);
            self.emit(error, span);
        }
    }

    /// Whether a `use` in this file binds `name` as an unqualified value,
    /// which is what `x.name()` on a scalar needs: the call is the free call
    /// `name(x)`, and a std function is reachable only through its module
    /// path or an item import of its own.
    fn import_binds_free_name(&self, name: &str) -> bool {
        self.import_targets
            .iter()
            .filter(|((_, bound), _)| bound == name)
            .any(|(_, full)| {
                full.first().map(String::as_str) != Some("std")
                    || gossamer_resolve::is_stdlib_item_path(&full.join("::"))
            })
    }

    /// Types `x.sqrt()`, `(-2).abs()`, `a.pow(b)` and the rest of the
    /// `math` surface reached in method position on a numeric receiver.
    ///
    /// The receiver is the function's first argument, so the arity and
    /// the answer both come from the `math` signature row. `abs`, `min`,
    /// `max`, and `clamp` answer in the receiver's own type; every other
    /// row computes in floating point whatever it was handed.
    /// Return type of `f64::to_bits` / `f64::from_bits` and their `f32`
    /// siblings, written as associated functions on the primitive.
    fn float_bits_assoc_ret(&mut self, module: &[&str], last: &str) -> Option<Ty> {
        match (module, last) {
            (["f64"], "to_bits") => Some(self.tcx.int_ty(IntTy::U64)),
            (["f64"], "from_bits") => Some(self.tcx.float_ty(FloatTy::F64)),
            (["f32"], "to_bits") => Some(self.tcx.int_ty(IntTy::U32)),
            (["f32"], "from_bits") => Some(self.tcx.float_ty(FloatTy::F32)),
            _ => None,
        }
    }

    /// `x.to_bits()` on a float receiver: the method spelling of
    /// [`Self::float_bits_assoc_ret`], answering the unsigned integer of
    /// the receiver's own width.
    fn float_bits_method_ret(
        &mut self,
        method: &str,
        resolved: Ty,
        arg_count: usize,
    ) -> Option<Ty> {
        if method != "to_bits" || arg_count != 0 {
            return None;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Float(FloatTy::F32)) => Some(self.tcx.int_ty(IntTy::U32)),
            Some(TyKind::Float(FloatTy::F64)) => Some(self.tcx.int_ty(IntTy::U64)),
            Some(TyKind::Var(_)) if self.infer.is_float_literal_var(self.tcx, resolved) => {
                Some(self.tcx.int_ty(IntTy::U64))
            }
            _ => None,
        }
    }

    fn check_numeric_receiver_method(
        &mut self,
        method: &str,
        resolved: Ty,
        arg_count: usize,
    ) -> Option<Ty> {
        // An unsuffixed literal's width is pinned at the end of
        // inference, so a numeric receiver reached from one is still a
        // variable here - constrained to a family, which is all this
        // needs to answer in.
        let receiver_is_float = match self.tcx.kind(resolved) {
            Some(TyKind::Float(_)) => true,
            Some(TyKind::Int(_)) => false,
            Some(TyKind::Var(_)) => {
                if self.infer.is_float_literal_var(self.tcx, resolved) {
                    true
                } else if self.infer.is_integer_constrained_var(self.tcx, resolved) {
                    false
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        let shape = crate::stdlib_signatures::function_shape_for_path(&["math"], method)?;
        if shape.params.len() != arg_count + 1 {
            return None;
        }
        if shape.return_ty == "bool" {
            return Some(self.tcx.bool_ty());
        }
        if receiver_is_float || matches!(method, "abs" | "min" | "max" | "clamp") {
            return Some(resolved);
        }
        Some(self.tcx.float_ty(FloatTy::F64))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the helper preserves the method-call checking context"
    )]
    fn check_string_receiver_method(
        &mut self,
        call_id: NodeId,
        method: &str,
        generics: &[AstGenericArg],
        expected: Expectation,
        resolved: Ty,
        span: Span,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        if !matches!(self.tcx.kind(resolved), Some(TyKind::String)) {
            return None;
        }
        let expected_arity = match method {
            // The intrinsic String surface: the rest of the catalogue is
            // shared with the `strings::` free functions and gets its arity
            // from `check_strings_arity`.
            "clear" | "len" | "is_empty" | "as_bytes" => Some(0),
            "truncate" | "push" | "push_str" | "push_char" | "push_byte" => Some(1),
            "push_utf8" => Some(3),
            _ => None,
        };
        if let Some(expected_arity) = expected_arity
            && arg_tys.len() != expected_arity
        {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("String::{method}"),
                    expected: expected_arity,
                    found: arg_tys.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        if let Some(arg_ty) = arg_tys.first().copied() {
            let want = match method {
                "push" | "push_char" => Some(self.tcx.intern(TyKind::Char)),
                "push_str" => Some(self.tcx.string_ty()),
                "push_byte" | "truncate" => Some(self.tcx.int_ty(IntTy::I64)),
                "push_utf8" => {
                    let u8_ty = self.tcx.int_ty(IntTy::U8);
                    Some(self.tcx.intern(TyKind::Vec(u8_ty)))
                }
                _ => None,
            };
            if let Some(want) = want {
                let found = self.peel_refs(arg_ty);
                let arg_span = args.first().map_or(span, |arg| arg.span);
                self.unify(want, found, arg_span);
            }
        }
        self.check_string_method_args_if_needed(call_id, method, span, args, arg_tys);
        Some(self.string_method_ret(method, generics, expected, span))
    }

    /// Method names the checker resolves on `resolved`, in the order a
    /// diagnostic lists them. Empty for a receiver with no tabled surface,
    /// which leaves the diagnostic without a did-you-mean.
    /// The owner key an `impl` block for `resolved` registers its methods
    /// under, when the type has a spelling an impl header can name.
    ///
    /// An impl's owner is the last segment of the path it is written for, so
    /// a type reachable only as a structural spelling - `[T]`, `[T; N]`,
    /// `(A, B)` - has no key here and registers its methods by name alone.
    fn builtin_impl_owner(&self, resolved: Ty) -> Option<&'static str> {
        match self.tcx.kind(resolved)? {
            TyKind::String => Some("String"),
            TyKind::Vec(_) => Some("Vec"),
            TyKind::HashMap { ordered, .. } => Some(if *ordered { "BTreeMap" } else { "Map" }),
            TyKind::Bool => Some("bool"),
            TyKind::Char => Some("char"),
            TyKind::Int(int) => Some(int.as_str()),
            TyKind::Float(float) => Some(float.as_str()),
            _ => None,
        }
    }

    /// Records the `impl` block `method` reaches on `receiver_ty`, so the
    /// lowering below calls that block's body rather than re-deriving the
    /// owner from a type that no longer names it.
    fn record_method_owner(&mut self, call_id: NodeId, receiver_ty: Ty, method: &str) {
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        // A receiver whose type is a parameter has one type per instantiation,
        // and the bytecode VM runs one body for all of them, so there is no
        // single block to name. Such a call is dispatched on the value in hand.
        if self.ty_mentions_generic_param(resolved) {
            return;
        }
        let Some(owner) = self.impl_owner_of(resolved) else {
            return;
        };
        if !self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&owner))
        {
            return;
        }
        // A type's own surface answers first: an `impl` block adds names to a
        // type rather than replacing any it already had, so a method the
        // receiver already carries is not redirected to a block that happens
        // to spell the same name. Checked last, so it costs nothing for the
        // calls that reach no user block at all.
        if self
            .tabled_method_names(resolved)
            .iter()
            .any(|name| name == method)
        {
            return;
        }
        self.table.insert_method_owner(call_id, owner);
    }

    /// Types a call to a method an `impl` block declared for a built-in
    /// receiver, when the receiver's own surface does not carry that name.
    ///
    /// Method resolution reads a tabled surface per built-in type, so without
    /// this a program's `impl Trait for String` is rejected at every call
    /// site even though the compiled tiers resolve and run it.
    fn user_impl_method_on_builtin(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
    ) -> Option<Ty> {
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        let owner = self.impl_owner_of(resolved)?;
        if self.user_type_decls.contains(&owner) {
            // A type the program declares resolves its methods through its own
            // identity, which is checked against what that type owns.
            return None;
        }
        if !self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&owner))
        {
            return None;
        }
        if self
            .tabled_method_names(resolved)
            .iter()
            .any(|name| name == method)
        {
            return None;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let key = (owner, method.to_string(), args.len());
        Some(
            self.method_ret_types
                .get(&key)
                .copied()
                .unwrap_or_else(|| self.fresh()),
        )
    }

    /// The name an `impl` block for `resolved` registers its methods under.
    ///
    /// A structural spelling - `[T]`, `[T; N]`, `(A, B)` - is not a path an
    /// impl header can name, so it has no owner here and its methods register
    /// by name alone.
    fn impl_owner_of(&mut self, resolved: Ty) -> Option<String> {
        if let Some(builtin) = self.builtin_impl_owner(resolved) {
            return Some(builtin.to_string());
        }
        match self.tcx.kind(resolved)? {
            TyKind::Adt { def, .. } => {
                let def = *def;
                self.tcx.def_name(def).map(str::to_string)
            }
            // A tuple has no path to name it by, so it registers under the
            // spelling every layer shares for one.
            TyKind::Tuple(_) => {
                // The parts are what tell one structural type from another, so
                // each has to be resolved before it is named: a tuple whose
                // elements are still inference variables names them, and no
                // impl registered under that spelling.
                let settled = self.deep_resolve(resolved);
                crate::printer::structural_impl_owner(self.tcx, settled)
            }
            _ => None,
        }
    }

    /// Whether an `impl` block declared `method` for `resolved`'s type.
    fn user_impl_declares(&mut self, resolved: Ty, method: &str) -> bool {
        self.impl_owner_of(resolved).is_some_and(|owner| {
            self.user_method_owners
                .get(method)
                .is_some_and(|owners| owners.contains(&owner))
        })
    }

    /// Method names user `impl` blocks declared for the type named `owner`.
    fn user_methods_for_owner(&self, owner: &str) -> Vec<String> {
        self.user_method_owners
            .iter()
            .filter(|(_, owners)| owners.contains(owner))
            .map(|(method, _)| method.clone())
            .collect()
    }

    fn known_method_names(&self, resolved: Ty) -> Vec<String> {
        // A type's own surface is what it always carried; an impl block adds
        // to it, so a diagnostic lists both. Only the nominal owners are read
        // here: rendering a structural one needs the interner mutably, and a
        // diagnostic is a listing rather than a decision.
        let mut out = self.tabled_method_names(resolved);
        let owner = self
            .builtin_impl_owner(resolved)
            .map(str::to_string)
            .or_else(|| match self.tcx.kind(resolved) {
                Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def).map(str::to_string),
                _ => None,
            });
        if let Some(owner) = owner {
            for method in self.user_methods_for_owner(&owner) {
                if !out.contains(&method) {
                    out.push(method);
                }
            }
        }
        out
    }

    /// The method surface a receiver carries on its own, before any `impl`
    /// block a program writes for it.
    fn tabled_method_names(&self, resolved: Ty) -> Vec<String> {
        let owner = match self.tcx.kind(resolved) {
            Some(TyKind::String) => "String",
            Some(TyKind::Vec(_)) => "Vec",
            Some(TyKind::Slice(_)) => "Slice",
            Some(TyKind::Array { .. }) => "Array",
            Some(TyKind::HashMap { .. }) => "Map",
            Some(TyKind::Iterator(_) | TyKind::Range(_)) => "Iterator",
            Some(TyKind::Tuple(_)) => "Tuple",
            Some(TyKind::Adt { def, .. }) => return self.adt_method_names(*def),
            _ => return Vec::new(),
        };
        let mut names = core_type_own_method_names(owner).unwrap_or_default();
        // A fixed array answers a copy and a conversion to the `Vec` of its
        // element; neither is part of the sequence surface it shares with a
        // view over one.
        if owner == "Array" {
            names.extend(["clone", "into"]);
        }
        let mut seen = HashSet::new();
        names
            .into_iter()
            .filter(|name| seen.insert(*name))
            .map(str::to_string)
            .collect()
    }

    /// Method names of an `Adt` receiver: the tabled surface for the
    /// checker's sentinel collections, and the user impl and trait methods
    /// for a declared type.
    fn adt_method_names(&self, def: gossamer_resolve::DefId) -> Vec<String> {
        let tabled = match def.local {
            HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL => Some(SET_METHODS),
            VEC_DEQUE_DEF_LOCAL => Some(DEQUE_METHODS),
            VEC_QUEUE_DEF_LOCAL
            | VEC_STACK_DEF_LOCAL
            | BINARY_HEAP_DEF_LOCAL
            | MIN_HEAP_DEF_LOCAL => Some(PUSH_POP_METHODS),
            RESULT_DEF_LOCAL => Some(RESULT_METHODS),
            OPTION_DEF_LOCAL => Some(OPTION_METHODS),
            _ => None,
        };
        if let Some(names) = tabled {
            return names.iter().map(|name| (*name).to_string()).collect();
        }
        let Some(owner) = self.tcx.def_name(def) else {
            return Vec::new();
        };
        let mut methods: Vec<String> = self
            .user_method_owners
            .iter()
            .filter(|(_, owners)| owners.contains(owner))
            .map(|(method, _)| method.clone())
            .collect();
        // The methods written for this type lead the listing; the surface
        // every type carries says nothing about what the reader declared.
        methods
            .sort_by_key(|method| (AUTOMATIC_METHODS.contains(&method.as_str()), method.clone()));
        methods
    }

    /// Builds the GT0002 diagnostic for `method` on `resolved`, carrying
    /// the receiver's method surface so the reader gets a did-you-mean.
    /// The spelling to name in a diagnostic about `method`: what the source
    /// wrote, which differs only where a parse-time desugar renamed the call.
    fn written_method_name(&self, method: &str) -> String {
        if self.written_method.is_empty() {
            return method.to_string();
        }
        self.written_method.clone()
    }

    fn unresolved_method(&self, ty: String, method: &str, resolved: Ty) -> TypeError {
        TypeError::UnresolvedMethod {
            ty,
            name: self.written_method_name(method),
            available: self.known_method_names(resolved),
            field_of_same_name: self.adt_declares_field(resolved, method),
            free_fn_of_same_name: self.user_fn_names.contains(method),
        }
    }

    /// Whether the struct `resolved` names declares a field called
    /// `name`, which a call spelling would have missed.
    fn adt_declares_field(&self, resolved: Ty, name: &str) -> bool {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return false;
        };
        self.struct_fields
            .get(def)
            .is_some_and(|fields| fields.iter().any(|(field, _)| field == name))
    }

    /// The diagnostic for a method call that did not resolve, given how many
    /// arguments it was written with. A name the receiver does declare failed
    /// on its argument count rather than its spelling, so it reports the count
    /// the method takes instead of claiming the method does not exist.
    fn unresolved_method_call(
        &self,
        ty: String,
        method: &str,
        resolved: Ty,
        found_args: usize,
    ) -> TypeError {
        let available = self.known_method_names(resolved);
        // A declared method's parameters come from its signature; a built-in
        // sequence or iterator combinator has no `FnDecl`, so its count comes
        // from the same table the combinator's own typing reads (total arity,
        // receiver included).
        if available.iter().any(|name| name == method)
            && let Some(expected) = (0..=8)
                .find(|arity| {
                    self.method_arg_sigs
                        .contains_key(&(method.to_string(), *arity))
                })
                .or_else(|| {
                    Self::std_combinator_arity("iter", method).and_then(|a| a.checked_sub(1))
                })
                .filter(|expected| *expected != found_args)
        {
            return TypeError::CallArityMismatch {
                callee: method.to_string(),
                expected,
                found: found_args,
            };
        }
        TypeError::UnresolvedMethod {
            ty,
            name: self.written_method_name(method),
            available,
            field_of_same_name: self.adt_declares_field(resolved, method),
            free_fn_of_same_name: self.user_fn_names.contains(method),
        }
    }

    fn reject_unknown_sequence_method(
        &mut self,
        resolved: Ty,
        method: &str,
        found_args: usize,
        span: Span,
    ) -> bool {
        if !matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. })
        ) {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, found_args);
        self.emit(error, span);
        true
    }

    fn reject_unknown_set_method(
        &mut self,
        resolved: Ty,
        method: &str,
        found_args: usize,
        span: Span,
    ) -> bool {
        let is_hash_set = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Adt { def, .. }) if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
        );
        if !is_hash_set {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, found_args);
        self.emit(error, span);
        true
    }

    fn reject_unknown_deque_method(
        &mut self,
        resolved: Ty,
        method: &str,
        found_args: usize,
        span: Span,
    ) -> bool {
        let is_vec_deque = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Adt { def, .. })
                if matches!(
                    def.local,
                    VEC_DEQUE_DEF_LOCAL | VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL
                )
        );
        if !is_vec_deque || method == "clone" {
            return false;
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, found_args);
        self.emit(error, span);
        true
    }

    /// Rejects a call on a built-in handle receiver whose argument count
    /// is not the one that method takes. These receivers dispatch by name
    /// to a runtime shim that reads a fixed number of slots, so an extra
    /// argument is dropped and a missing one is read as zero.
    fn reject_handle_method_arity(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        pipe_extra: usize,
        span: Span,
    ) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let found = args.len() + pipe_extra;
        let (owner, expected) = match self.tcx.kind(resolved) {
            Some(TyKind::Sender(_)) => (
                "Sender",
                match method {
                    "send" | "try_send" => Some(1),
                    "close" => Some(0),
                    _ => None,
                },
            ),
            Some(TyKind::Receiver(_)) => (
                "Receiver",
                match method {
                    "recv_ctx" => Some(1),
                    "recv" | "try_recv" | "close" => Some(0),
                    _ => None,
                },
            ),
            Some(TyKind::JoinHandle(_)) => ("JoinHandle", (method == "join").then_some(0)),
            Some(TyKind::Instant) => ("time::Instant", (method == "elapsed_ms").then_some(0)),
            Some(TyKind::Duration) => (
                "time::Duration",
                matches!(method, "as_millis" | "as_secs" | "as_micros").then_some(0),
            ),
            Some(TyKind::DynError) => (
                "errors::Error",
                match method {
                    "with_field" => Some(2),
                    "is" | "field" => Some(1),
                    "message" | "cause" | "chain" | "fields" => Some(0),
                    _ => None,
                },
            ),
            Some(TyKind::JsonValue) => (
                "json::Value",
                match method {
                    "set" => Some(2),
                    "get" | "at" => Some(1),
                    "keys" | "len" | "is_null" | "as_str" | "as_i64" | "as_f64" | "as_bool"
                    | "as_array" => Some(0),
                    _ => None,
                },
            ),
            _ => return false,
        };
        let Some(expected) = expected.filter(|expected| *expected != found) else {
            return false;
        };
        for arg in args {
            self.check_expr(arg);
        }
        self.emit(
            TypeError::CallArityMismatch {
                callee: format!("{owner}::{method}"),
                expected,
                found,
            },
            span,
        );
        true
    }

    fn reject_collection_method_arity(
        &mut self,
        resolved: Ty,
        method: &str,
        found: usize,
        span: Span,
    ) -> bool {
        let expected = match self.tcx.kind(resolved) {
            Some(TyKind::HashMap { .. }) => match method {
                "insert" | "get_or" | "or_insert" => Some(2),
                "get" | "remove" | "pop" | "contains" | "contains_key" => Some(1),
                "clear" | "len" | "is_empty" | "keys" | "values" | "iter" => Some(0),
                _ => None,
            },
            // A tuple's surface is whole-value operations plus positional
            // access; nothing else reaches a tuple receiver, so every name
            // here has one arity.
            Some(TyKind::Tuple(_)) => match method {
                "get" => Some(1),
                "len" | "is_empty" | "clone" | "to_string" | "into" | "try_into" => Some(0),
                _ => None,
            },
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
            {
                match method {
                    "insert" | "remove" | "contains" => Some(1),
                    "clear" | "len" | "is_empty" | "to_vec" | "iter" => Some(0),
                    "union"
                    | "intersection"
                    | "difference"
                    | "symmetric_difference"
                    | "is_subset"
                    | "is_superset"
                    | "is_disjoint" => Some(1),
                    _ => None,
                }
            }
            Some(TyKind::Adt { def, .. }) if def.local == VEC_DEQUE_DEF_LOCAL => match method {
                "push_back" | "push_front" => Some(1),
                "pop_back" | "pop_front" | "peek_back" | "peek_front" | "len" | "is_empty"
                | "clear" => Some(0),
                _ => None,
            },
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL) =>
            {
                match method {
                    "push" => Some(1),
                    "pop" | "peek" | "len" | "is_empty" | "clear" => Some(0),
                    _ => None,
                }
            }
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL) =>
            {
                match method {
                    "push" => Some(1),
                    "pop" | "peek" | "len" | "is_empty" | "clear" => Some(0),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(expected) = expected.filter(|expected| *expected != found) else {
            return false;
        };
        let owner = self.render_public_ty(resolved);
        self.emit(
            TypeError::CallArityMismatch {
                callee: format!("{owner}::{method}"),
                expected,
                found,
            },
            span,
        );
        true
    }

    /// Rejects passing a built-in iterator to a parameter bound by an
    /// iteration trait.
    ///
    /// A built-in iterator carries no impl block, so the specialised body
    /// keeps an unresolved `next` that no backend can lower. Naming the
    /// iterator type on the parameter directly is the form every tier
    /// lowers, and it is what the help points at.
    fn reject_builtin_iterator_instantiation(
        &mut self,
        concrete: Ty,
        bounds: &[String],
        span: Span,
    ) {
        if !matches!(
            self.tcx.kind(concrete),
            Some(TyKind::Iterator(_) | TyKind::Range(_))
        ) {
            return;
        }
        let iterating = bounds.iter().any(|bound| {
            matches!(bound.as_str(), "Iterator" | "IntoIterator")
                || self
                    .trait_method_ret
                    .contains_key(&(bound.clone(), "next".to_string()))
        });
        if !iterating {
            return;
        }
        let rendered = self.render_public_ty(concrete);
        self.emit(TypeError::BuiltinIteratorNotGeneric { ty: rendered }, span);
    }

    /// Rejects `for x in t` where `t` is a generic parameter no bound makes
    /// iterable.
    ///
    /// A parameter iterates through `.next()`, so a bound has to guarantee
    /// that method exists. Without one the loop lowers against whatever
    /// shape each instantiation happens to have, which the compiled tier
    /// cannot resolve.
    fn reject_unbounded_generic_iteration(&mut self, iter_ty: Ty, span: Span) {
        let mut t = self.infer.resolve(self.tcx, iter_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Param { idx, name }) = self.tcx.kind(t) else {
            return;
        };
        let param = name.to_string();
        let bounds = self
            .current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default();
        let iterable = bounds.iter().any(|bound| {
            matches!(bound.as_str(), "Iterator" | "IntoIterator")
                || self
                    .trait_method_ret
                    .contains_key(&(bound.clone(), "next".to_string()))
        });
        if iterable {
            return;
        }
        self.emit(
            TypeError::MethodNotOnBound {
                param,
                method: "next".to_string(),
                bounds,
            },
            span,
        );
    }

    /// Rejects a method on a generic-parameter receiver that none of the
    /// parameter's bounds declares.
    ///
    /// A parameter stands for every type a caller may supply, so its bounds
    /// are the whole of what it can do. Without this the call fell through
    /// to a name-global lookup and bound an unrelated type's body, reading
    /// the receiver at that type's field layout.
    fn reject_method_off_bound(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> bool {
        // Every value answers these regardless of its bounds.
        if AUTOMATIC_METHODS.contains(&method) {
            return false;
        }
        let mut t = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Param { idx, name }) = self.tcx.kind(t) else {
            return false;
        };
        let param = name.to_string();
        let bounds = self
            .current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default();
        // A bound whose method surface is unknown cannot say whether this
        // call is valid, so it is left alone rather than guessed at.
        if bounds.iter().any(|bound| {
            !self.declared_trait_names.contains(bound) && builtin_trait_methods(bound).is_none()
        }) {
            return false;
        }
        // A built-in bound answers for the methods it licenses.
        if bounds
            .iter()
            .filter_map(|bound| builtin_trait_methods(bound))
            .any(|surface| surface.contains(&method))
        {
            return false;
        }
        for arg in args {
            self.check_expr(arg);
        }
        self.emit(
            TypeError::MethodNotOnBound {
                param,
                method: method.to_string(),
                bounds,
            },
            span,
        );
        true
    }

    fn check_param_receiver_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        // A method on a bound type-parameter receiver (`s.area()` where
        // `s: &T`, `T: Shape`) resolves to the trait method's declared
        // return type, so a `String`-returning trait method is not left to
        // default to i64 and render its pointer bits on the compiled tiers.
        let (ret, params) = self.param_method_sig(receiver_ty, method, span)?;
        let arg_tys: Vec<Ty> = args.iter().map(|arg| self.check_expr(arg)).collect();
        if params.len() == args.len() {
            for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
                self.check_sig_param_arg(*param, *arg_ty, arg);
            }
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: method.to_string(),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
        }
        Some(ret)
    }

    fn check_string_method_args_if_needed(
        &mut self,
        call_id: NodeId,
        method: &str,
        span: Span,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        // `s.contains(x)` dispatches to the same `strings::` shim as
        // the free function with the receiver as the implicit first
        // argument; validate the explicit args so an integer in a
        // string slot is rejected here too. Skipped under `|>`,
        // which appends the piped value as a trailing argument.
        if !self.pipe_stage_callees.contains(&call_id) {
            self.check_strings_method_call_args(method, args, arg_tys, span);
        }
    }

    fn method_receiver_expectation(
        &mut self,
        method: &str,
        receiver: &Expr,
        expected: Expectation,
    ) -> Expectation {
        if !Self::is_string_parse_expr(receiver) {
            return Expectation::None;
        }
        match method {
            "map" | "map_err" | "and_then" | "or_else" => {
                if let Some((ok, _)) = self.result_payload_expectation(expected) {
                    let err = self.fresh();
                    return Expectation::HasType(self.result_adt_ty(ok, err));
                }
            }
            "ok" => {
                if let Some(payload) = self.option_payload_expectation(expected) {
                    let err = self.fresh();
                    return Expectation::HasType(self.result_adt_ty(payload, err));
                }
            }
            "unwrap_or" | "unwrap_or_else" => {
                if let Some(payload) = self.non_result_expectation_target(expected) {
                    let err = self.fresh();
                    return Expectation::HasType(self.result_adt_ty(payload, err));
                }
            }
            _ => {}
        }
        Expectation::None
    }

    fn is_string_parse_expr(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::MethodCall { name, .. } => name.name == "parse",
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Path(path) => path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.name.name == "parse"),
                _ => false,
            },
            _ => false,
        }
    }

    /// Enforces writable receivers for user `&mut self` methods and built-in
    /// methods whose execution path writes a replacement value back into the
    /// receiver place.
    fn check_mutating_method_receiver(&mut self, receiver: &Expr, receiver_ty: Ty, method: &str) {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if matches!(self.tcx.kind(resolved), Some(TyKind::Var(_))) {
            self.deferred_mutating_receivers
                .push(DeferredMutatingReceiver {
                    ty: receiver_ty,
                    method: method.to_string(),
                    place: self.auto_deref_place_mutability(receiver),
                    name: Self::place_root_name(receiver).unwrap_or_else(|| "value".to_string()),
                    span: receiver.span,
                });
            return;
        }
        if !self.method_requires_mut_receiver(receiver_ty, method) {
            return;
        }
        self.check_mutating_receiver_place(receiver);
    }

    fn reject_non_vec_resizing_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if !matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Array { .. } | TyKind::Slice(_))
        ) || !is_vec_only_sequence_method(method)
        {
            return false;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let ty = self.render_public_ty(resolved);
        self.emit(
            TypeError::SequenceResizeRequiresVec {
                ty,
                method: method.to_string(),
            },
            span,
        );
        true
    }

    /// Rejects a call a built-in receiver cannot take: a method its
    /// surface does not carry, or one it carries at a different arity.
    fn reject_invalid_builtin_receiver_call(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        call_id: NodeId,
        span: Span,
    ) -> bool {
        let pipe_extra = usize::from(self.pipe_stage_callees.contains(&call_id));
        self.reject_non_vec_resizing_method(receiver_ty, method, args, span)
            || self.reject_unavailable_non_vec_sequence_method(receiver_ty, method, args, span)
            || self.reject_handle_method_arity(receiver_ty, method, args, pipe_extra, span)
    }

    fn reject_unavailable_non_vec_sequence_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        // A resizing method on a fixed-size sequence gets the more specific
        // `SequenceResizeRequiresVec` diagnostic from the sibling check, so
        // it is left alone here. An iterator has no buffer to resize, so
        // its surface is decided by the combinator list alone.
        // Every value `{}` renders answers `to_string`, whatever other surface
        // its receiver declares. A lazy cursor is not a value, so it keeps the
        // rejection its own surface gives it.
        if method == "to_string" && args.is_empty() && self.is_displayable_value(resolved) {
            return false;
        }
        // A method an `impl` block declared for this receiver is part of its
        // surface, so the tabled list is not the whole answer.
        if self.user_impl_declares(resolved, method) {
            return false;
        }
        let (available, resize_reported_separately) = match self.tcx.kind(resolved) {
            Some(TyKind::Array { .. }) => (is_array_sequence_method(method), true),
            Some(TyKind::Slice(_)) => (is_slice_sequence_method(method), true),
            // A tuple is not iterable: its elements may differ in type, so
            // there is no element type to hand a loop or a combinator.
            // Positional access (`t.0`, `t.get(i)`) stays available.
            Some(TyKind::Tuple(_)) => (!is_tuple_rejected_method(method), false),
            // Iterator state addresses elements through the combinator
            // surface; a buffer method has no length or storage to act on
            // and would read as a silent no-op.
            Some(TyKind::Iterator(_) | TyKind::Range(_)) => {
                (iterator_receiver_accepts_method(method), false)
            }
            // A map is keyed, not ordered by position: the sequence surface
            // has nothing to index, reorder, or slice on it.
            Some(TyKind::HashMap { .. }) => (is_map_method(method), false),
            _ => return false,
        };
        if available || (resize_reported_separately && is_vec_only_sequence_method(method)) {
            return false;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let ty = self.render_public_ty(resolved);
        let error = self.unresolved_method_call(ty, method, resolved, args.len());
        self.emit(error, span);
        true
    }

    fn render_public_ty(&mut self, ty: Ty) -> String {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Bool) => "bool".to_string(),
            Some(TyKind::Char) => "char".to_string(),
            Some(TyKind::String) => "String".to_string(),
            Some(TyKind::Int(int)) => int.as_str().to_string(),
            Some(TyKind::Float(float)) => float.as_str().to_string(),
            Some(TyKind::Unit) => "()".to_string(),
            Some(TyKind::Never) => "!".to_string(),
            Some(TyKind::DynValue) => "DynValue".to_string(),
            Some(TyKind::Array { elem, len }) => {
                format!(
                    "[{}; {}]",
                    self.render_public_ty(elem),
                    match len {
                        crate::ArrayLen::Concrete(n) => n.to_string(),
                        crate::ArrayLen::Param(idx) => format!("N{}", idx.as_u32()),
                    }
                )
            }
            Some(TyKind::Slice(elem)) => {
                format!("[{}]", self.render_public_ty(elem))
            }
            Some(TyKind::Vec(elem)) => {
                format!("Vec<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Iterator(elem)) => {
                format!("Iterator<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Range(elem)) => {
                format!("Range<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::HashMap {
                key,
                value,
                ordered,
            }) => {
                format!(
                    "{}<{}, {}>",
                    if ordered { "BTreeMap" } else { "Map" },
                    self.render_public_ty(key),
                    self.render_public_ty(value)
                )
            }
            Some(TyKind::Sender(elem)) => {
                format!("Sender<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Receiver(elem)) => {
                format!("Receiver<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::JoinHandle(elem)) => {
                format!("JoinHandle<{}>", self.render_public_ty(elem))
            }
            Some(TyKind::Tuple(parts)) => {
                let rendered = parts
                    .iter()
                    .map(|part| self.render_public_ty(*part))
                    .collect::<Vec<_>>();
                if rendered.len() == 1 {
                    format!("({},)", rendered[0])
                } else {
                    format!("({})", rendered.join(", "))
                }
            }
            Some(TyKind::Ref { mutability, inner }) => {
                format!("{}{}", mutability.prefix(), self.render_public_ty(inner))
            }
            Some(TyKind::FnPtr(sig)) => self.render_public_fn_sig("fn", &sig),
            Some(TyKind::FnTrait(sig)) => self.render_public_fn_sig("Fn", &sig),
            Some(TyKind::FnDef { def, substs }) => {
                self.render_public_def("fn", def.local, substs.as_slice())
            }
            Some(TyKind::Closure { def, .. }) => format!("<closure #{}>", def.local),
            Some(TyKind::Adt { def, substs }) => {
                self.render_public_def("adt", def.local, substs.as_slice())
            }
            Some(TyKind::Alias { def, substs }) => {
                self.render_public_def("alias", def.local, substs.as_slice())
            }
            Some(TyKind::Nominal { def, .. }) => self.render_public_def("alias", def.local, &[]),
            Some(TyKind::Dyn(trait_ref)) => {
                self.render_public_def("trait", trait_ref.def.local, trait_ref.substs.as_slice())
            }
            Some(TyKind::Duration) => "time::Duration".to_string(),
            Some(TyKind::Instant) => "time::Instant".to_string(),
            Some(TyKind::JsonValue) => "json::Value".to_string(),
            Some(TyKind::DynError) => "errors::Error".to_string(),
            Some(TyKind::Var(vid)) if self.infer.is_unresolved_integer_var(vid) => {
                "i64".to_string()
            }
            Some(TyKind::Var(vid)) if self.infer.is_unresolved_float_var(vid) => "f64".to_string(),
            Some(TyKind::Var(_)) => "_".to_string(),
            Some(TyKind::Param { name, .. }) => name.to_string(),
            Some(TyKind::Error) => "<error>".to_string(),
            None => format!("<ty:{}>", resolved.as_u32()),
        }
    }

    fn render_public_fn_sig(&mut self, prefix: &str, sig: &FnSig) -> String {
        let inputs = sig
            .inputs
            .iter()
            .map(|ty| self.render_public_ty(*ty))
            .collect::<Vec<_>>()
            .join(", ");
        let output = self.infer.resolve(self.tcx, sig.output);
        if matches!(self.tcx.kind(output), Some(TyKind::Unit)) {
            format!("{prefix}({inputs})")
        } else {
            format!("{prefix}({inputs}) -> {}", self.render_public_ty(output))
        }
    }

    fn render_public_def(
        &mut self,
        fallback: &str,
        local: u32,
        substs: &[crate::GenericArg],
    ) -> String {
        let mut out = self
            .tcx
            .def_name(gossamer_resolve::DefId::local(local))
            .map_or_else(|| format!("{fallback}#{local}"), ToString::to_string);
        if !substs.is_empty() {
            let args = substs
                .iter()
                .map(|arg| match arg {
                    crate::GenericArg::Type(ty) => self.render_public_ty(*ty),
                    crate::GenericArg::Const(value) => value.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push('<');
            out.push_str(&args);
            out.push('>');
        }
        out
    }

    /// Qualified user-method calls (`Type::method(receiver, ...)`) and the
    /// qualified map/set mutation surface do not pass through
    /// `check_method_call`, so enforce the same receiver capability here.
    fn check_mutating_qualified_call(&mut self, callee: &Expr, args: &[Expr]) {
        let ExprKind::Path(path) = &callee.kind else {
            return;
        };
        let segments = &path.segments;
        if segments.len() < 2 {
            return;
        }
        let owner = segments[segments.len() - 2].name.name.as_str();
        let method = segments[segments.len() - 1].name.name.as_str();
        let key = (owner.to_string(), method.to_string());
        let user_requirement = self
            .inherent_method_requires_mut
            .get(&key)
            .or_else(|| self.trait_impl_method_requires_mut.get(&key))
            .copied();
        let requires_mut = user_requirement.unwrap_or_else(|| {
            if self.user_type_decls.contains(owner)
                || matches!(
                    self.resolutions.get(callee.id),
                    Some(Resolution::Def { .. })
                )
            {
                return false;
            }
            matches!(owner, "Map" | "Set" | "BTreeSet") && crate::is_mutating_method_name(method)
        });
        if requires_mut && let Some(receiver) = args.first() {
            if user_requirement == Some(true) {
                match self.expr_ref_mutbl(receiver) {
                    Some(Mutbl::Mut) => {}
                    Some(Mutbl::Not) => self.check_mutating_receiver_place(receiver),
                    None => self.emit(
                        TypeError::MutableArgumentRequiresReference {
                            argument: Self::place_display(receiver),
                        },
                        receiver.span,
                    ),
                }
            } else {
                self.check_mutating_receiver_place(receiver);
            }
        }
    }

    fn check_mutating_receiver_place(&mut self, receiver: &Expr) {
        let name = Self::place_root_name(receiver).unwrap_or_else(|| "value".to_string());
        self.emit_mutating_place_error(
            self.auto_deref_place_mutability(receiver),
            name,
            receiver.span,
        );
    }

    fn emit_mutating_place_error(&mut self, place: PlaceMut, name: String, span: Span) {
        match place {
            PlaceMut::ImmutableBinding => {
                self.emit(TypeError::AssignToImmutable { name }, span);
            }
            PlaceMut::SharedReference => {
                self.emit(TypeError::AssignThroughSharedReference { name }, span);
            }
            PlaceMut::NotAReference => {
                self.emit(TypeError::DerefWriteToNonReference { name }, span);
            }
            PlaceMut::Writable | PlaceMut::Unknown => {}
        }
    }

    fn receiver_method_owner_name(&self, resolved: Ty) -> Option<String> {
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def).map(str::to_string),
            Some(
                TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Vec(_)
                | TyKind::Iterator(_)
                | TyKind::Range(_)
                | TyKind::HashMap { .. }
                | TyKind::Sender(_)
                | TyKind::Receiver(_)
                | TyKind::JoinHandle(_)
                | TyKind::Duration
                | TyKind::Instant
                | TyKind::JsonValue
                | TyKind::DynValue
                | TyKind::DynError,
            ) => {
                let rendered = render_ty(self.tcx, resolved);
                let bare = rendered.split('<').next().unwrap_or(&rendered);
                bare.rsplit("::").next().map(str::to_string)
            }
            _ => None,
        }
    }

    fn method_requires_mut_receiver(&mut self, receiver_ty: Ty, method: &str) -> bool {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Param { idx, .. }) = self.tcx.kind(resolved) {
            return self
                .current_param_bounds
                .get(idx.0 as usize)
                .is_some_and(|bounds| {
                    bounds.iter().any(|bound| {
                        self.trait_method_requires_mut
                            .get(&(bound.clone(), method.to_string()))
                            .copied()
                            .unwrap_or(false)
                    })
                });
        }
        if let Some(owner) = self.receiver_method_owner_name(resolved) {
            let key = (owner.clone(), method.to_string());
            // A user method named `push` or `remove` must follow its declared
            // receiver, not inherit the built-in writeback policy by name.
            if self.user_type_decls.contains(&owner) {
                return self
                    .inherent_method_requires_mut
                    .get(&key)
                    .or_else(|| self.trait_impl_method_requires_mut.get(&key))
                    .copied()
                    .unwrap_or(false);
            }
            // Counter-like `inc` methods use interior mutability. The
            // write-back variants are specific to HashMap receivers.
            if matches!(method, "inc" | "inc_at" | "inc_batch") {
                return matches!(owner.as_str(), "Map");
            }
            if let Some(requires_mut) = self
                .inherent_method_requires_mut
                .get(&key)
                .or_else(|| self.trait_impl_method_requires_mut.get(&key))
            {
                return *requires_mut;
            }
        }
        if matches!(method, "inc" | "inc_at" | "inc_batch") {
            return false;
        }
        crate::is_mutating_method_name(method)
    }

    fn user_method_params_for(&mut self, receiver_ty: Ty, method: &str) -> Option<Vec<Ty>> {
        let TyKind::Adt { def, substs } = self.tcx.kind(receiver_ty)?.clone() else {
            return None;
        };
        let name = self.tcx.def_name(def)?.to_string();
        let key = (name, method.to_string());
        if let Some(params) = self.method_param_types.get(&key) {
            return Some(params.clone());
        }
        let params = self.generic_method_param_types.get(&key)?.clone();
        let subst_tys = substs.types();
        Some(
            params
                .into_iter()
                .map(|param| self.subst_params_in_ty(param, &subst_tys))
                .collect(),
        )
    }

    fn check_user_method_args(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        let Some(params) = self.user_method_params_for(receiver_ty, method) else {
            return;
        };
        // Arity has its own receiver-aware diagnostic below. Validate every
        // explicit leading argument here; a pipeline supplies the final slot
        // later in `pipe_result_ty`.
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
    }

    /// Types a method call's explicit arguments, shaping each by the
    /// method's declared parameter. A closure argument to a Vec/slice
    /// combinator (`xs.sort_by`, `xs.map`) is pinned to the element type
    /// so a field access in its body resolves to the struct projection
    /// rather than the dynamic JSON path; a container-literal argument is
    /// coerced (never unified) toward the sole unambiguous candidate.
    fn check_method_call_arg_tys(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
    ) -> Vec<Ty> {
        let candidates = self
            .method_arg_sigs
            .get(&(method.to_string(), args.len()))
            .cloned()
            .unwrap_or_default();
        let closure_combinator_inputs = self.vec_combinator_closure_inputs(method, receiver_ty);
        let mut arg_tys: Vec<Ty> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            let exp = match (&closure_combinator_inputs, &arg.kind) {
                (Some(inputs), ExprKind::Closure { params, .. })
                    if params.len() == inputs.len() =>
                {
                    let output = self.fresh();
                    let sig = FnSig {
                        inputs: inputs.clone(),
                        output,
                    };
                    Expectation::HasType(self.tcx.intern(TyKind::FnPtr(sig)))
                }
                _ => match self.unique_container_expectation(&candidates, i) {
                    // A fixed-array literal never stands in for a `Vec`
                    // parameter: the spellings name different containers, so
                    // the mismatch is reported here rather than coerced into
                    // an argument the callee then treats as a Vec.
                    Some(want)
                        if matches!(&arg.kind, ExprKind::FixedArray(_))
                            && matches!(
                                self.tcx.kind(self.infer.resolve(self.tcx, want)),
                                Some(TyKind::Vec(_))
                            ) =>
                    {
                        Expectation::HasType(want)
                    }
                    Some(want) => Expectation::Coerce(want),
                    None => Expectation::None,
                },
            };
            arg_tys.push(self.check_expr_expecting(arg, exp));
        }
        arg_tys
    }

    /// Rejects a method call whose argument count does not match the
    /// receiver method's declared arity. A call on the right of `|>`
    /// receives the piped value as an implicit trailing argument, so it
    /// is counted toward the supplied arity. Mirrors the free-call
    /// GT0018 check, which method calls never reached.
    fn check_method_arity(
        &mut self,
        call_id: NodeId,
        resolved: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def).map(str::to_string) else {
            return;
        };
        let Some(&expected) = self.method_arities.get(&(name.clone(), method.to_string())) else {
            return;
        };
        let pipe_extra = usize::from(self.pipe_stage_callees.contains(&call_id));
        let effective = args.len() + pipe_extra;
        if effective != expected {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{name}::{method}"),
                    expected,
                    found: effective,
                },
                span,
            );
        }
    }

    /// Return type of a method on a `HashSet` receiver (sentinel `Adt`,
    /// def `u32::MAX - 7`). Without this the set-algebra methods are left
    /// a fresh `Var`, so iterating their result (`for e in a.union(&b)`)
    /// could not recover the set kind and read the handle as a vec.
    fn set_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let (_owner, elem) = self.set_elem_ty(resolved)?;
        // The value a set is asked about is one of its elements, so a set
        // built empty (`Set::new()`) learns its element type from the first
        // such call. Left unpinned, the element stays a variable that no later
        // traversal can dispatch a field read against. Only an unpinned
        // element is filled in here, and the queried value is read through any
        // borrow: `s.contains(&k)` asks about `k`.
        if matches!(method, "insert" | "remove" | "contains")
            && let [value] = arg_tys
            && matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, elem)),
                Some(TyKind::Var(_))
            )
        {
            let value = self.peel_refs(*value);
            self.unify(elem, value, span);
        }
        match method {
            // New sets - same element type as the receiver.
            "union" | "intersection" | "difference" | "symmetric_difference" => Some(resolved),
            // `to_vec` snapshots into a Vec; `iter` starts a pipeline, and
            // answers with an iterator the way every other sequence does.
            "to_vec" => Some(self.tcx.intern(TyKind::Vec(elem))),
            "iter" => Some(self.tcx.intern(TyKind::Iterator(elem))),
            "insert" | "remove" | "contains" | "is_empty" | "is_subset" | "is_superset"
            | "is_disjoint" => Some(self.tcx.bool_ty()),
            "len" => Some(self.tcx.int_ty(IntTy::I64)),
            "clear" => Some(self.tcx.unit()),
            _ => None,
        }
    }

    /// Type of `xs.count(pred)` - the accepted-element count - pinning the
    /// predicate to one that takes an element and answers a bool. Every
    /// receiver that traverses reaches this, so the predicate's parameter is
    /// the element type wherever the call is written; a parameter left
    /// unresolved reaches codegen with no type to project a field against.
    fn pred_count_ty(&mut self, pred_ty: Ty, elem: Ty, span: Span) -> Ty {
        let out = self.callable_output(pred_ty, &[elem], span);
        let bool_ty = self.tcx.bool_ty();
        self.unify(bool_ty, out, span);
        self.tcx.int_ty(IntTy::I64)
    }

    /// Return type of an `iter::` combinator called in method form on a
    /// sequence receiver (`xs.map(f)`, `xs.filter(f)`, `xs.sum()`, …):
    /// the same typing as the data-last free form, with the receiver as
    /// the data argument. `None` for non-sequence receivers and
    /// non-combinator names, so `Result::map` / `Option::map` / the
    /// String surface keep their own dispatch.
    fn seq_combinator_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        // A map holds its pairs, so a traversal on one answers eagerly with a
        // sequence, the way one on a Vec does. A free-call-only traversal is
        // declined here for the same reason `Vec` declines it: no receiver
        // form exists to reach.
        if COLLECTION_TRAVERSAL_METHODS.contains(&method) && !is_free_call_only_traversal(method) {
            // A map's element is its key/value pair. A set has no order for a
            // traversal to read its elements in, so a set answers these
            // through the iterator `iter()` gives, not on the collection.
            let elem = match self.tcx.kind(resolved) {
                Some(TyKind::HashMap { key, value, .. }) => {
                    let (key, value) = (*key, *value);
                    Some(self.tcx.intern(TyKind::Tuple(vec![key, value])))
                }
                _ => None,
            };
            if let Some(elem) = elem {
                // The predicate form of `count` has no data-last free
                // spelling for `std_combinator_ty` to answer from, so it is
                // typed here the way a sequence receiver types it.
                if method == "count" && arg_tys.len() == 1 {
                    return Some(self.pred_count_ty(arg_tys[0], elem, span));
                }
                let seq = self.tcx.intern(TyKind::Vec(elem));
                return self.std_combinator_ty_at(
                    "iter",
                    method,
                    arg_tys,
                    seq,
                    DataPosition::Receiver,
                    span,
                );
            }
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Iterator(_) | TyKind::Range(_)) => {
                // The receiver is the combinator's data argument, so the
                // declared arity leaves one slot for the explicit
                // arguments. A count the surface does not declare is
                // reported here: dispatch past this point has no iterator
                // entry to reach, so it would answer an unconstrained
                // variable and the call would run as a silent no-op.
                let accepted = Self::iterator_method_arities(method)?;
                if !accepted.contains(&arg_tys.len()) {
                    self.emit(
                        TypeError::CallArityMismatch {
                            callee: method.to_string(),
                            expected: accepted[0],
                            found: arg_tys.len(),
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                }
                if method == "next" {
                    let elem = self.sequence_elem_ty(resolved, span)?;
                    return Some(self.option_adt_ty(elem));
                }
                // The predicate form of `count` has no data-last free
                // spelling, so it is typed here: the predicate answers a
                // bool for an element and the count is an integer.
                if method == "count" && arg_tys.len() == 1 {
                    let elem = self.sequence_elem_ty(resolved, span)?;
                    return Some(self.pred_count_ty(arg_tys[0], elem, span));
                }
                return self.std_combinator_ty_at(
                    "iter",
                    method,
                    arg_tys,
                    resolved,
                    DataPosition::Receiver,
                    span,
                );
            }
            // A collection already holds its values, so traversing it answers
            // eagerly with a materialised result. `iter()` is how a caller
            // asks for the lazy walk that never holds the whole sequence.
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }) => {}
            _ => return None,
        }
        match (method, arg_tys.len()) {
            ("sum", 0) => self.sequence_elem_ty(resolved, span),
            ("min" | "max", 0) => {
                let elem = self.sequence_elem_ty(resolved, span)?;
                Some(self.option_adt_ty(elem))
            }
            ("count", 0) => Some(self.tcx.int_ty(IntTy::I64)),
            // `xs.count(f)`: the accepted-element count - the predicate
            // takes an element and yields bool.
            ("count", 1) => {
                let elem = self.sequence_elem_ty(resolved, span)?;
                Some(self.pred_count_ty(arg_tys[0], elem, span))
            }
            (
                m @ ("map" | "filter" | "for_each" | "any" | "all" | "find" | "position"
                | "max_by_key" | "min_by_key" | "take_while" | "skip_while" | "skip" | "chain"
                | "zip" | "windows" | "chunks" | "filter_map" | "find_map" | "flat_map"
                | "chunk_by" | "count_by" | "partition" | "product_by" | "sum_by" | "min_by"
                | "max_by" | "reduce"),
                1,
            )
            | (m @ ("enumerate" | "rev" | "dedup" | "flatten" | "pairwise" | "unzip"), 0)
            | (m @ ("fold" | "scan"), 2) => self.std_combinator_ty_at(
                "iter",
                m,
                arg_tys,
                resolved,
                DataPosition::Receiver,
                span,
            ),
            _ => None,
        }
    }

    /// Return type of a method on a `HashMap` / `BTreeMap` receiver whose
    /// result depends on the key/value types. Without this `m.iter()` is a
    /// fresh `Var`, so the for-vec lowering can't see the `(K, V)` element
    /// type and mis-sizes the element (especially when a destructure slot
    /// is `_`). Key/value-shaped arguments unify against the map's generics
    /// so an unannotated `HashMap::new()` is grounded by its first
    /// `insert` / `get` and native dispatch picks the right keyed symbol.
    /// Returns `None` for a non-map receiver so dispatch continues.
    fn map_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let (key, value) = match self.tcx.kind(resolved) {
            Some(TyKind::HashMap { key, value, .. }) => (*key, *value),
            _ => return None,
        };
        // `set` is json's field-update helper, not a map method; the
        // bare-name dispatch would route it there and the write would
        // vanish (VM) or the symbol would not link (native), so reject
        // it uniformly here.
        if method == "set" {
            let ty = self.render_public_ty(resolved);
            let error = self.unresolved_method(ty, "set", resolved);
            self.emit(error, span);
            return Some(self.tcx.error_ty());
        }
        let (key_arg, value_arg) = match (method, arg_tys.len()) {
            ("insert" | "get_or" | "or_insert", 2) => {
                (arg_tys.first().copied(), arg_tys.get(1).copied())
            }
            ("get" | "remove" | "pop" | "contains" | "contains_key", 1) => {
                (arg_tys.first().copied(), None)
            }
            // `inc` is the integer-counter idiom: it pins the value to
            // i64 so an unannotated `HashMap::new()` grounded only by
            // `inc` still classifies for the counter lowering.
            ("inc", 1 | 2) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.unify(value, i, span);
                (arg_tys.first().copied(), None)
            }
            _ => (None, None),
        };
        if let Some(arg_ty) = key_arg {
            let key_peeled = self.peel_refs(key);
            let arg_peeled = self.peel_refs(arg_ty);
            self.unify(key_peeled, arg_peeled, span);
        }
        if let Some(arg_ty) = value_arg {
            let value_peeled = self.peel_refs(value);
            let arg_peeled = self.peel_refs(arg_ty);
            self.unify(value_peeled, arg_peeled, span);
        }
        match method {
            // `m.iter()` yields `(K, V)` pairs, lazily like any other `iter`;
            // `collect` on that walk is how they are materialised.
            "iter" => {
                let pair = self.tcx.intern(TyKind::Tuple(vec![key, value]));
                Some(self.tcx.intern(TyKind::Iterator(pair)))
            }
            "keys" => {
                let key = self.peel_refs(key);
                Some(self.tcx.intern(TyKind::Vec(key)))
            }
            "values" => Some(self.tcx.intern(TyKind::Vec(value))),
            "get" | "pop" | "insert" | "remove" => Some(self.option_adt_ty(value)),
            "get_or" | "or_insert" => Some(value),
            "contains" | "contains_key" | "is_empty" => Some(self.tcx.bool_ty()),
            "len" => Some(self.tcx.int_ty(IntTy::I64)),
            "clear" => Some(self.tcx.unit()),
            _ => None,
        }
    }

    /// Whether the `Vec` surface accepts `name` at `arity` arguments. Used to
    /// name the count a method takes when a call supplies a different one.
    fn vec_method_arity_exists(name: &str, arity: usize) -> bool {
        match name {
            "len" | "is_empty" | "first" | "last" | "to_vec" | "iter" | "sort" | "reverse"
            | "enumerate" | "rev" | "dedup" | "flatten" | "pairwise" | "sum" | "product"
            | "min" | "max" | "unzip" | "pop" | "clear" | "capacity" | "shrink_to_fit" => {
                arity == 0
            }
            "join" | "take" | "skip" | "step_by" | "chunks" | "windows" | "map" | "filter"
            | "filter_map" | "find_map" | "flat_map" | "take_while" | "skip_while" | "for_each"
            | "any" | "all" | "find" | "position" | "max_by_key" | "min_by_key" | "chunk_by"
            | "count_by" | "max_by" | "min_by" | "partition" | "product_by" | "reduce"
            | "sum_by" | "get" | "contains" | "index_of" | "count_of" | "insert" | "remove" => {
                arity == 1
            }
            "count" => arity <= 1,
            "slice" | "swap" | "fold" | "scan" => arity == 2,
            _ => false,
        }
    }

    /// Reports the argument count `name` takes when the receiver declares it
    /// but no arity accepted `found`. `None` leaves the call to the caller's
    /// ordinary unresolved-method path.
    fn sequence_arity_mismatch(&mut self, name: &str, found: usize, span: Span) -> Option<Ty> {
        if !is_slice_sequence_method(name) && !is_vec_only_sequence_method(name) {
            return None;
        }
        // A count the surface accepts is typed by the combinator path.
        if Self::vec_method_arity_exists(name, found) {
            return None;
        }
        let expected =
            (0..=8).find(|arity| *arity != found && Self::vec_method_arity_exists(name, *arity))?;
        self.emit(
            TypeError::CallArityMismatch {
                callee: name.to_string(),
                expected,
                found,
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Return type of a method on a `Vec` / slice / fixed-array receiver
    /// whose result is a function of the element type. Without this the
    /// checker falls through to a fresh `Var`, so a chained `.first()` /
    /// `.index_of(..).map(..)` reaches codegen with an untyped payload and
    /// the native tier mis-represents it. Also checks the `push` / `insert`
    /// argument against the element type (a `[i64]` accepting a `String`
    /// pointer word is a silent memory hazard on the native backend).
    /// Returns `None` for a non-sequence receiver so dispatch continues.
    #[allow(
        clippy::too_many_lines,
        reason = "method dispatch table stays readable as one row set"
    )]
    fn vec_method_ret(
        &mut self,
        method: &str,
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let elem = match self.tcx.kind(resolved) {
            Some(TyKind::Vec(e) | TyKind::Slice(e)) => *e,
            Some(TyKind::Array { elem, .. }) => *elem,
            _ => return None,
        };
        let expected_arity = match method {
            "push" | "remove" | "truncate" | "extend" | "extend_from_slice" | "reserve"
            | "reserve_exact" | "get" | "fill" | "copy_from_slice" | "binary_search" => Some(1),
            "insert" | "swap" | "resize" => Some(2),
            "pop" | "clear" | "sort" | "reverse" | "capacity" | "iter" => Some(0),
            "sort_by" | "sort_by_key" => Some(1),
            "copy_within" => Some(3),
            _ => None,
        };
        if let Some(expected) = expected_arity
            && arg_tys.len() != expected
        {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("Vec::{method}"),
                    expected,
                    found: arg_tys.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        // References are layout-transparent (the runtime owns memory), so
        // peel them before comparing the pushed element to the slot type.
        let push_arg = match (method, arg_tys.len()) {
            ("push" | "fill", 1) => arg_tys.first().copied(),
            ("insert" | "resize", 2) => arg_tys.get(1).copied(),
            ("binary_search", 1) => arg_tys.first().copied(),
            _ => None,
        };
        if let Some(arg_ty) = push_arg {
            let elem_peeled = self.peel_refs(elem);
            let arg_peeled = self.peel_refs(arg_ty);
            self.unify(elem_peeled, arg_peeled, span);
        }
        // `xs.extend(ys)` appends a sequence of the receiver's own element
        // type. Unifying it pins a literal argument to that element type, so
        // `Vec<u8>.extend(#[4, 5])` appends bytes rather than leaving the
        // literal at the default integer width.
        if matches!(method, "extend" | "extend_from_slice" | "copy_from_slice")
            && let Some(arg_ty) = arg_tys.first().copied()
        {
            let sequence = self.tcx.intern(TyKind::Vec(elem));
            let arg_peeled = self.peel_refs(arg_ty);
            if matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, arg_peeled)),
                Some(TyKind::Vec(_) | TyKind::Var(_))
            ) {
                self.unify(sequence, arg_peeled, span);
            }
        }
        match (method, arg_tys.len()) {
            (
                "push" | "clear" | "truncate" | "extend" | "extend_from_slice" | "reserve"
                | "reserve_exact" | "sort" | "sort_by" | "sort_by_key" | "reverse" | "fill"
                | "swap" | "resize" | "copy_within" | "copy_from_slice",
                _,
            ) => Some(self.tcx.unit()),
            // `Ok(i)` is the found index and `Err(i)` the position an
            // insert would keep sorted, so both arms carry an index.
            ("binary_search", 1) => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                Some(self.result_adt_ty(i64_ty, i64_ty))
            }
            ("insert", 2) => {
                let error_ty = self.tcx.dyn_error_ty();
                let unit_ty = self.tcx.unit();
                Some(self.result_adt_ty(unit_ty, error_ty))
            }
            ("remove", 1) => {
                let error_ty = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(elem, error_ty))
            }
            ("capacity" | "len", 0) => Some(self.tcx.int_ty(IntTy::I64)),
            ("iter", 0) => {
                // Gossamer iteration yields managed values, not references.
                // Scalar elements copy and RC-backed elements gain their own
                // managed share. This keeps an iterator independent of a raw
                // element address while its runtime state retains the source.
                Some(self.tcx.intern(TyKind::Iterator(elem)))
            }
            ("is_empty", 0) => Some(self.tcx.bool_ty()),
            ("pop", 0) => Some(self.option_adt_ty(elem)),
            ("first" | "last", 0) => Some(self.option_adt_ty(elem)),
            ("get", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let i = self.tcx.int_ty(IntTy::I64);
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                Some(self.option_adt_ty(elem))
            }
            // `dedup` describes the collection: it removes adjacent repeats
            // in place. `collect` and `rev` are traversals and belong to the
            // iterator, so they fall through to the collection-traversal
            // rejection.
            ("dedup", 0) => Some(self.tcx.intern(TyKind::Vec(elem))),
            // `to_vec` copies a borrowed or fixed-length sequence into an
            // owned one. A `Vec` is already that, so it does not carry the
            // conversion to itself.
            ("to_vec", 0) if !matches!(self.tcx.kind(resolved), Some(TyKind::Vec(_))) => {
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            ("index_of", 1) => {
                let i = self.tcx.int_ty(IntTy::I64);
                Some(self.option_adt_ty(i))
            }
            ("count_of", 1) => Some(self.tcx.int_ty(IntTy::I64)),
            ("contains", 1) => Some(self.tcx.bool_ty()),
            ("slice", _) => {
                if arg_tys.len() != 2 {
                    self.emit(
                        TypeError::CallArityMismatch {
                            callee: "Vec::slice".to_string(),
                            expected: 2,
                            found: arg_tys.len(),
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                }
                let i = self.tcx.int_ty(IntTy::I64);
                for arg_ty in arg_tys {
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                let vec = self.tcx.intern(TyKind::Vec(elem));
                let err = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(vec, err))
            }
            ("windows" | "chunks", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let i = self.tcx.int_ty(IntTy::I64);
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                let window = self.tcx.intern(TyKind::Vec(elem));
                Some(self.tcx.intern(TyKind::Vec(window)))
            }
            ("pairwise", 0) => {
                let pair = self.tcx.intern(TyKind::Tuple(vec![elem, elem]));
                Some(self.tcx.intern(TyKind::Vec(pair)))
            }
            ("flatten", 0) => {
                let inner = self
                    .sequence_elem_ty(elem, span)
                    .unwrap_or_else(|| self.fresh());
                Some(self.tcx.intern(TyKind::Vec(inner)))
            }
            // `xs.join(sep)`: Display-renders scalar / String elements,
            // separator unifies with String. An aggregate element has no
            // joinable rendering and is rejected here so it can never
            // reach a shim that would join pointer words.
            ("join", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let s = self.tcx.string_ty();
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(s, arg_peeled, span);
                }
                let elem_resolved = self.infer.resolve(self.tcx, elem);
                let elem_peeled = self.peel_refs(elem_resolved);
                if let Some((ty, class)) = self.not_displayable(elem_peeled) {
                    self.emit(TypeError::ValueNotDisplayable { ty, class }, span);
                    return Some(self.tcx.error_ty());
                }
                Some(self.tcx.string_ty())
            }
            // `xs.take(n)` / `xs.step_by(s)`: fresh Vec of the same
            // element type; the count/stride argument is an integer.
            ("take" | "step_by", 1) => {
                if let Some(arg_ty) = arg_tys.first() {
                    let i = self.tcx.int_ty(IntTy::I64);
                    let arg_peeled = self.peel_refs(*arg_ty);
                    self.unify(i, arg_peeled, span);
                }
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            // A name this receiver does declare, written with an argument
            // count no arm accepts, failed on its arity rather than its
            // spelling.
            (name, found) => self.sequence_arity_mismatch(name, found, span),
        }
    }

    /// Return type of a method on a `String` receiver, with precise types
    /// for the commonly-chained methods (so `s.rfind(&"/").map(|i| i as
    /// i64)` types the `Option<i64>` payload rather than leaving it an
    /// untyped var the native tier mis-represents - the P0-4 shape).
    /// A method outside the `String` surface is the name-global dispatch
    /// leak (a `unicode::*` char predicate like `"abc".is_letter()`, or a
    /// typo like `"abc".bogus()`): it runs the wrong global body on the
    /// VM and fails to lower native, so it is rejected (P0-6).
    fn string_method_ret(
        &mut self,
        method: &str,
        generics: &[AstGenericArg],
        expected: Expectation,
        span: Span,
    ) -> Ty {
        match method {
            "split" | "splitn" | "split_whitespace" | "lines" => {
                let s = self.tcx.string_ty();
                self.tcx.intern(TyKind::Vec(s))
            }
            // A cursor over the encoded text: walking a String holds the
            // text and a position, not a slot per scalar. `collect`
            // materialises, and `as_bytes` is the owned byte sequence.
            "chars" => {
                let c = self.tcx.intern(TyKind::Char);
                self.tcx.intern(TyKind::Iterator(c))
            }
            "bytes" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Vec(u8_ty))
            }
            // `Option<i64>` Unicode scalar offsets.
            "find" | "rfind" | "find_any" | "rfind_any" | "index_rune" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            // Strict full-string parses: `"42".to_i64() -> Option<i64>`.
            "to_i64" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            "to_f64" => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.option_adt_ty(f)
            }
            "to_bool" => {
                let b = self.tcx.bool_ty();
                self.option_adt_ty(b)
            }
            "contains" | "contains_any" | "contains_rune" | "starts_with" | "ends_with"
            | "equal_fold" | "is_empty" => self.tcx.bool_ty(),
            "len" | "count" | "byte_at" | "byte_len" => self.tcx.int_ty(IntTy::I64),
            "clone" => self.tcx.string_ty(),
            "clear" | "truncate" | "push" | "push_str" | "push_char" | "push_byte" => {
                self.tcx.unit()
            }
            // Answers whether the window was valid UTF-8 and therefore appended.
            "push_utf8" => self.tcx.bool_ty(),
            // Methods that return a fresh `String` (runtime `*mut c_char`):
            // pinning the result type so chained calls (`s.trim().len()`) and
            // typed bindings lower from a known type instead of an inference
            // var carrying an untyped heap payload into MIR.
            "trim" | "trim_start" | "trim_end" | "trim_matches" | "trim_start_matches"
            | "trim_end_matches" | "to_uppercase" | "to_lowercase" | "to_title" | "replace"
            | "replacen" | "repeat" | "pad_left" | "pad_right" | "center" | "substring" => {
                self.tcx.string_ty()
            }
            // `as_bytes` -> `[u8]` (runtime `*mut GosVec` of bytes).
            "as_bytes" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Vec(u8_ty))
            }
            // `split_once` / `rsplit_once` -> `Option<(String, String)>`.
            "split_once" | "rsplit_once" => {
                let s = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![s, s]));
                self.option_adt_ty(pair)
            }
            // `strip_prefix` / `strip_suffix` -> `Option<String>`.
            "strip_prefix" | "strip_suffix" => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            // `slice(a, b) -> Result<String, errors::Error>` (out-of-range Err).
            "slice" => {
                let s = self.tcx.string_ty();
                let e = self.tcx.dyn_error_ty();
                self.result_adt_ty(s, e)
            }
            // One string reaches one parse: `to_i64` / `to_f64` / `to_bool`
            // are the strict, full-string forms, and each answers an
            // `Option<T>`. `parse` was a second surface with a second carrier
            // type for the same operation, so it reports with the rewrite and
            // still types as it did, leaving the rest of the body diagnosed on
            // its own terms.
            "parse" => {
                let suggestion = self.string_parse_replacement(expected);
                self.emit(TypeError::StringParseRetired { suggestion }, span);
                self.string_parse_ret("String::parse", generics, expected, span)
            }
            _ if is_string_method(method) => self.fresh(),
            _ => {
                let string_ty = self.tcx.string_ty();
                let error = self.unresolved_method("String".to_string(), method, string_ty);
                self.emit(error, span);
                self.tcx.error_ty()
            }
        }
    }

    fn first_type_generic_arg(&mut self, generics: &[AstGenericArg]) -> Option<Ty> {
        generics.iter().find_map(|arg| match arg {
            AstGenericArg::Type(ty) => Some(self.type_from_ast(ty)),
            AstGenericArg::Const(_) => None,
        })
    }

    /// The `to_T()` spelling that replaces a `parse()` at this site, chosen
    /// from the type the surrounding code expects.
    fn string_parse_replacement(&mut self, expected: Expectation) -> String {
        let payload = self.expectation_target(expected).and_then(|target| {
            let resolved = self.infer.resolve(self.tcx, target);
            match self.tcx.kind(resolved) {
                Some(TyKind::Adt { def, substs }) if def.local == RESULT_DEF_LOCAL => {
                    substs.types().first().copied()
                }
                _ => Some(resolved),
            }
        });
        match payload.map(|ty| self.tcx.kind_of(ty)) {
            Some(TyKind::Float(_)) => "to_f64",
            Some(TyKind::Bool) => "to_bool",
            _ => "to_i64",
        }
        .to_string()
    }

    fn string_parse_ret(
        &mut self,
        callable: &str,
        generics: &[AstGenericArg],
        expected: Expectation,
        span: Span,
    ) -> Ty {
        let payload = if let Some(ty) = self.first_type_generic_arg(generics) {
            Some(ty)
        } else if let Some(ty) = self.result_ok_expectation(expected) {
            Some(ty)
        } else if let Some(target) = self.non_result_expectation_target(expected) {
            Some(target)
        } else {
            self.emit(
                TypeError::GenericReturnTypeUninferred {
                    callable: callable.to_string(),
                    param: "T".to_string(),
                },
                span,
            );
            None
        };
        let Some(payload) = payload else {
            return self.tcx.error_ty();
        };
        let e = self.tcx.dyn_error_ty();
        self.result_adt_ty(payload, e)
    }

    /// The `Ok` payload of a resolved `Result<T, E>`, or `None` for any
    /// other type.
    fn result_ok_payload(&self, ty: Ty) -> Option<Ty> {
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if def.local != RESULT_DEF_LOCAL && self.tcx.def_name(*def) != Some("Result") {
            return None;
        }
        match substs.as_slice().first()? {
            crate::GenericArg::Type(ok) => Some(*ok),
            crate::GenericArg::Const(_) => None,
        }
    }

    fn result_ok_expectation(&mut self, expected: Expectation) -> Option<Ty> {
        self.result_payload_expectation(expected).map(|(ok, _)| ok)
    }

    fn result_payload_expectation(&mut self, expected: Expectation) -> Option<(Ty, Ty)> {
        let ty = self.expectation_target(expected)?;
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if def.local != u32::MAX && self.tcx.def_name(*def) != Some("Result") {
            return None;
        }
        let args = substs.as_slice();
        match (args.first()?, args.get(1)?) {
            (crate::GenericArg::Type(ok), crate::GenericArg::Type(err)) => Some((*ok, *err)),
            _ => None,
        }
    }

    fn option_payload_expectation(&mut self, expected: Expectation) -> Option<Ty> {
        let ty = self.expectation_target(expected)?;
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if def.local != u32::MAX - 1 && self.tcx.def_name(*def) != Some("Option") {
            return None;
        }
        match substs.as_slice().first()? {
            crate::GenericArg::Type(payload) => Some(*payload),
            crate::GenericArg::Const(_) => None,
        }
    }

    fn non_result_expectation_target(&mut self, expected: Expectation) -> Option<Ty> {
        let target = self.expectation_target(expected)?;
        match self.tcx.kind(target)? {
            TyKind::Adt { def, .. }
                if def.local == u32::MAX || self.tcx.def_name(*def) == Some("Result") =>
            {
                None
            }
            TyKind::Var(_) | TyKind::Error => None,
            _ => Some(target),
        }
    }

    /// Rejects a method call on a concrete user struct / enum receiver
    /// when the method demonstrably belongs to a *different* user type -
    /// the name-global dispatch soundness hole (P0-6: `b.label()` runs
    /// `A`'s body against `B`'s memory on the VM and fails to lower on
    /// the native tier). Conservative on purpose: it fires only when the
    /// method name is owned by some user type but not this one, so an
    /// unknown method (a genuine typo with no owner anywhere) or a
    /// builtin / derived method (`clone`) still falls through.
    /// Re-validates deferred structural uses (`value[i]` / `value(args)`
    /// / `value.N`) after integer/float defaulting has given unsuffixed
    /// literals their concrete type. An operand that resolved to a
    /// concrete non-indexable / non-callable / non-tuple type is
    /// rejected here, so `let x = 5; x[0]` - whose `x` was an inference
    /// var at first check - is caught instead of faulting on the
    /// compiled tier.
    fn check_deferred_structural(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_structural);
        for d in deferred {
            let mut resolved = self.infer.resolve(self.tcx, d.ty);
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(resolved).clone() {
                resolved = self.infer.resolve(self.tcx, inner);
            }
            let kind = self.tcx.kind_of(resolved).clone();
            match d.kind {
                DeferredStructuralKind::Index => {
                    let indexable = matches!(
                        kind,
                        TyKind::Array { .. } | TyKind::Slice(_) | TyKind::Vec(_) | TyKind::String
                    );
                    if !indexable && !is_soft_for_structural_use(&kind) {
                        let ty = self.render_public_ty(resolved);
                        self.emit(TypeError::NotIndexable { ty }, d.span);
                    }
                    if let Some(result) = d.result {
                        let element = match &kind {
                            TyKind::Array { elem, .. }
                            | TyKind::Slice(elem)
                            | TyKind::Vec(elem) => Some(*elem),
                            TyKind::String => Some(self.tcx.char_ty()),
                            _ => None,
                        };
                        if std::env::var_os("GOS_DBG_IDX").is_some() {
                            eprintln!("[idx] resolve kind={kind:?} element={element:?}");
                        }
                        if let Some(element) = element {
                            self.unify(result, element, d.span);
                        }
                    }
                }
                DeferredStructuralKind::Call => {
                    if is_definitely_not_callable_value(&kind) {
                        let ty = self.render_public_ty(resolved);
                        self.emit(TypeError::NotCallable { ty }, d.span);
                    }
                }
                DeferredStructuralKind::TupleField(idx) => match &kind {
                    TyKind::Tuple(elems) => {
                        if idx as usize >= elems.len() {
                            let ty = self.render_public_ty(resolved);
                            self.emit(TypeError::NoTupleField { ty, index: idx }, d.span);
                        } else if let Some(result) = d.result {
                            let field = elems[idx as usize];
                            self.unify(result, field, d.span);
                        }
                    }
                    other => {
                        let is_tuple_struct = u32::try_from(idx)
                            .ok()
                            .is_some_and(|i| self.tuple_struct_field_ty(resolved, i).is_some());
                        if !is_tuple_struct && !is_soft_for_structural_use(other) {
                            let ty = self.render_public_ty(resolved);
                            self.emit(TypeError::NoTupleField { ty, index: idx }, d.span);
                        }
                    }
                },
                DeferredStructuralKind::Downgrade => {
                    if self.downgrade_receiver_is_non_rc(resolved) {
                        let ty = self.render_public_ty(resolved);
                        self.emit(TypeError::WeakDowngradeNonRc { ty }, d.span);
                    }
                }
            }
        }
    }

    fn check_deferred_mutating_receivers(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_mutating_receivers);
        for receiver in deferred {
            if !self.method_requires_mut_receiver(receiver.ty, &receiver.method) {
                continue;
            }
            // The place verdict was taken while the receiver's type was still
            // an inference variable, so a `&mut` binding read as an
            // undeclared-`mut` local. Now that the type is known, a receiver
            // that crosses a mutable reference is a writable place.
            let mut resolved = self.infer.resolve(self.tcx, receiver.ty);
            let mut crossed_mutable_reference = false;
            while let Some(TyKind::Ref { mutability, inner }) = self.tcx.kind(resolved) {
                if *mutability == Mutbl::Mut {
                    crossed_mutable_reference = true;
                }
                resolved = self.infer.resolve(self.tcx, *inner);
            }
            if crossed_mutable_reference {
                continue;
            }
            self.emit_mutating_place_error(receiver.place, receiver.name, receiver.span);
        }
    }

    /// Emits mismatches held until numeric literal defaulting has made their
    /// type names stable. This keeps a rejected `r = [2, 3]` diagnostic
    /// readable as `&[i64; 2]` versus `[i64; 2]`, not inference variables.
    /// Runs the receiver-shape rejections that share one outcome: the
    /// method does not exist on this receiver, so the arguments are
    /// checked for their own errors and the call types as `error`.
    ///
    /// Returns the error type when one of them reported.
    fn reject_method_on_receiver(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        if self.reject_method_off_bound(receiver_ty, method, args, span) {
            return Some(self.tcx.error_ty());
        }
        if self.reject_supertrait_method_through_bound(receiver_ty, method, span) {
            for arg in args {
                self.check_expr(arg);
            }
            return Some(self.tcx.error_ty());
        }
        // A callable carries no method surface, so a method reached on one is
        // rejected before the conversion and representation paths below: those
        // would otherwise type `into` / `to_string` against a code address.
        if let Some(ty) = self.reject_method_on_callable(receiver_ty, method, args, span) {
            return Some(ty);
        }
        // `into` / `try_into` are conversions rather than surface the
        // receiver has to declare, and are typed further down. An opaque
        // alias formats as its representation does, so `to_string` is its own
        // Display surface rather than a representation method.
        if matches!(method, "into" | "try_into" | "to_string") && args.is_empty() {
            return None;
        }
        self.reject_nominal_repr_method(receiver_ty, method, args, span)
    }

    /// Rejects any method reached on a function, closure, or `Fn(..)` value.
    ///
    /// A callable is a code address: it declares no methods of its own and
    /// inherits none, so every such call is unresolved. Naming it here keeps
    /// the receiver from reaching the generic method paths, which would treat
    /// an unconstrained callable as text and answer the function's own name.
    /// Returns the error type when a diagnostic was emitted.
    fn reject_method_on_callable(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let mut r = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        if !matches!(
            self.tcx.kind(r),
            Some(
                TyKind::FnDef { .. }
                    | TyKind::FnPtr(_)
                    | TyKind::FnTrait(_)
                    | TyKind::Closure { .. }
            )
        ) {
            return None;
        }
        for arg in args {
            self.check_expr(arg);
        }
        // `FnDef` and `Closure` print with an internal def index, which says
        // nothing to a reader; name the callable by what it is instead.
        let ty = match self.tcx.kind(r) {
            Some(TyKind::FnDef { def, .. }) => {
                let def = *def;
                match self.tcx.def_name(def) {
                    Some(name) => format!("fn {name}"),
                    None => "fn".to_string(),
                }
            }
            Some(TyKind::Closure { .. }) => "closure".to_string(),
            _ => self.render_public_ty(r),
        };
        self.emit(
            TypeError::UnresolvedMethod {
                ty,
                name: self.written_method_name(method),
                available: Vec::new(),
                field_of_same_name: false,
                free_fn_of_same_name: self.user_fn_names.contains(method),
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Rejects a method reached on an opaque alias that only its
    /// representation declares.
    ///
    /// The alias exists to hide what it is made of, so the
    /// representation's surface is not part of it: `type Name = new
    /// String` gets no `len()` unless its own `impl` provides one.
    /// Converting to the representation is how that surface is reached.
    /// Returns the error type when a diagnostic was emitted.
    fn reject_nominal_repr_method(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let mut r = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Nominal { def, .. }) = self.tcx.kind(r) else {
            return None;
        };
        let name = self.tcx.def_name(*def).map(str::to_string)?;
        if self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&name))
        {
            return None;
        }
        for arg in args {
            self.check_expr(arg);
        }
        let mut available: Vec<String> = self
            .user_method_owners
            .iter()
            .filter(|(_, owners)| owners.contains(&name))
            .map(|(m, _)| m.clone())
            .collect();
        available.sort();
        self.emit(
            TypeError::UnresolvedMethod {
                ty: name,
                name: self.written_method_name(method),
                available,
                field_of_same_name: false,
                free_fn_of_same_name: self.user_fn_names.contains(method),
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Types `x.into()` / `x.try_into()`, whose target is fixed by the use
    /// site rather than by the call, and records `into` for the conversion
    /// audit once unification has pinned that target.
    fn check_conversion_method(&mut self, method: &str, receiver_ty: Ty, span: Span) -> Ty {
        let result = self.fresh();
        if method == "into" {
            self.deferred_into_conversions
                .push((receiver_ty, result, span));
            self.deferred_conversion_targets
                .push((result, "into", span));
        } else if method == "try_into" {
            self.deferred_conversion_targets
                .push((result, "try_into", span));
        }
        result
    }

    /// Reports `.into()` / `.try_into()` written where no use site fixes
    /// the target.
    ///
    /// The target of a conversion never comes from the receiver, so a call
    /// nothing constrains leaves its result an inference variable. Such a
    /// call has no `From` impl to reach and lowers to a bare `into`, which
    /// no tier binds; naming it here keeps the failure at `check`.
    fn check_deferred_conversion_targets(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_conversion_targets);
        for (result, method, span) in deferred {
            let result = self.deep_resolve(result);
            let open = match self.tcx.kind(result) {
                Some(TyKind::Var(_)) => true,
                // `try_into` answers `Result<B, E>`; the target is `B`.
                _ => self
                    .result_ok_payload(result)
                    .map(|ok| self.deep_resolve(ok))
                    .is_some_and(|ok| matches!(self.tcx.kind(ok), Some(TyKind::Var(_)))),
            };
            if !open {
                continue;
            }
            self.emit(
                TypeError::ConversionTargetUnknown {
                    method: method.to_string(),
                },
                span,
            );
        }
    }

    /// Reports `.into()` across an opaque alias boundary with nothing
    /// behind it.
    ///
    /// An alias and its representation convert for free in both
    /// directions - one runtime value, so the conversion is the identity.
    /// Every other pair, including two aliases that happen to erase to the
    /// same representation, needs a `From` impl, and saying so here keeps
    /// the failure at `check` instead of at run time.
    fn check_deferred_into_conversions(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_into_conversions);
        for (recv, result, span) in deferred {
            // The built-in array conversion answers for the value itself, so
            // it reads the receiver as written; a `From` impl is reached
            // through a reference just as it is through the value.
            let written = self.deep_resolve(recv);
            let mut recv = written;
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
                recv = self.deep_resolve(*inner);
            }
            let result = self.deep_resolve(result);
            // An unresolved side has nothing to audit yet; an ambiguous
            // `.into()` with no target is caught where its type stays open.
            if matches!(self.tcx.kind(recv), Some(TyKind::Var(_)))
                || matches!(self.tcx.kind(result), Some(TyKind::Var(_)))
            {
                continue;
            }
            // A repr pair converts in both directions.
            if self.is_nominal_repr_pair(recv, result) {
                continue;
            }
            // `From<[T; N]> for Vec<T>` is built in, and lowers to the same
            // buffer copy `Vec::from(array)` does on every tier.
            if self.is_array_to_vec(written, result) {
                continue;
            }
            // An opaque alias converts to itself for free - one runtime
            // value. A primitive `.into()` to its own type has no `From`
            // behind it and lowers to an unbound `into`, so identity is a
            // conversion only for a nominal type.
            let recv_nominal = matches!(self.tcx.kind(recv), Some(TyKind::Nominal { .. }));
            if recv == result && recv_nominal {
                continue;
            }
            // A user `From` impl on the target answers for the pair.
            let target = self.render_public_ty(result);
            if self
                .user_method_owners
                .get("from")
                .is_some_and(|owners| owners.contains(&target))
            {
                continue;
            }
            let borrowed_sequence = self.is_array_to_vec(recv, result);
            let from = self.render_public_ty(if borrowed_sequence { written } else { recv });
            self.emit(
                TypeError::NoConversion {
                    from,
                    to: target,
                    borrowed_sequence,
                },
                span,
            );
        }
    }

    /// Whether `from` is a fixed array and `to` the `Vec` of its element.
    fn is_array_to_vec(&self, from: Ty, to: Ty) -> bool {
        let (Some(TyKind::Array { elem, .. }), Some(TyKind::Vec(target))) =
            (self.tcx.kind(from), self.tcx.kind(to))
        else {
            return false;
        };
        elem == target
    }

    /// Whether one of `a` / `b` is an opaque alias whose representation is
    /// the other.
    fn is_nominal_repr_pair(&self, a: Ty, b: Ty) -> bool {
        let over = |outer: Ty, inner: Ty| matches!(self.tcx.kind(outer), Some(TyKind::Nominal { repr, .. }) if *repr == inner);
        over(a, b) || over(b, a)
    }

    fn check_deferred_type_mismatches(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_type_mismatches);
        for (expected, found, span) in deferred {
            let expected = self.deep_resolve(expected);
            let found = self.deep_resolve(found);
            let expected = self.render_public_ty(expected);
            let found = self.render_public_ty(found);
            self.emit(TypeError::TypeMismatch { expected, found }, span);
        }
    }

    /// Emits literal-constraint mismatches after defaulting has resolved every
    /// nested component of the expected type. This prevents diagnostics such
    /// as `&mut ?0` when the referent is known to be `i64`.
    fn check_deferred_literal_type_mismatches(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_literal_type_mismatches);
        for (expected, found, span) in deferred {
            let expected = self.deep_resolve(expected);
            let expected = self.render_public_ty(expected);
            self.emit(
                TypeError::TypeMismatch {
                    expected,
                    found: found.to_string(),
                },
                span,
            );
        }
    }

    fn maybe_reject_unknown_adt_method(&mut self, resolved: Ty, method: &str, span: Span) {
        if matches!(method, "clone") {
            return;
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def).map(str::to_string) else {
            return;
        };
        // Only genuine user-declared struct / enum receivers: sentinel
        // Adts (Result / Option / http::Response / VecDeque) are not in
        // `user_type_decls`.
        if !self.user_type_decls.contains(&name) {
            return;
        }
        // `user_method_owners` records every impl and trait method, so a
        // name this receiver does not own is a typo or a method of another
        // type; both would reach the compiled tier as an undefined
        // `@Type::method` symbol.
        let owned_here = self
            .user_method_owners
            .get(method)
            .is_some_and(|owners| owners.contains(&name));
        if !owned_here {
            let error = self.unresolved_method(name, method, resolved);
            self.emit(error, span);
        }
    }

    /// As [`Self::reject_private_method`], resolving the receiver's own
    /// identity first. Runs for every method call, including the ones a
    /// later pass resolves a return type for.
    fn reject_private_method_call(&mut self, receiver_ty: Ty, method: &str, span: Span) {
        let mut peeled = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
            peeled = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def).map(str::to_string) else {
            return;
        };
        self.reject_private_method(&name, method, span);
    }

    /// Rejects a reference to a field declared without `pub` from outside
    /// the module its struct was declared in. A `pub` struct may keep
    /// private fields: the type is API, its representation need not be.
    fn reject_private_field(&mut self, receiver_ty: Ty, field: &str, span: Span) {
        let mut peeled = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
            peeled = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
            // The receiver's type is not known yet; a struct it later
            // resolves to still has to satisfy the rule.
            self.deferred_private_fields.push((
                receiver_ty,
                field.to_string(),
                span,
                self.current_module.clone(),
            ));
            return;
        };
        let def = *def;
        self.reject_private_field_of(def, field, span);
    }

    /// Re-runs the field-visibility rule for accesses whose receiver type
    /// only became known after inference finished.
    fn check_deferred_private_fields(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_private_fields);
        for (receiver_ty, field, span, module) in deferred {
            let mut peeled = self.infer.resolve(self.tcx, receiver_ty);
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                peeled = self.infer.resolve(self.tcx, *inner);
            }
            let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
                continue;
            };
            let def = *def;
            let prior = std::mem::replace(&mut self.current_module, module);
            self.reject_private_field_of(def, &field, span);
            self.current_module = prior;
        }
    }

    /// As [`Self::reject_private_field`], with the owning struct already
    /// resolved.
    fn reject_private_field_of(&mut self, def: DefId, field: &str, span: Span) {
        if self.synthesized_depth > 0 {
            return;
        }
        let Some((home, visibility)) = self.field_homes.get(&(def, field.to_string())) else {
            return;
        };
        let reachable = self.current_module.starts_with(home.as_slice())
            || match visibility {
                Visibility::Public => true,
                Visibility::Package => self.resolutions.same_package(home, &self.current_module),
                Visibility::Inherited => false,
            };
        if reachable {
            return;
        }
        let module = home.join("::");
        let ty = self
            .tcx
            .def_name(def)
            .map_or_else(|| "?".to_string(), str::to_string);
        self.emit(
            TypeError::PrivateField {
                ty,
                name: field.to_string(),
                module,
            },
            span,
        );
    }

    /// Rejects a call to a method declared without `pub` from outside the
    /// module its `impl` was written in. This is the rule the resolver
    /// applies to a free function: the declaring module and its
    /// descendants keep access, so a `pub` wrapper always reaches the
    /// private helpers declared beside it.
    fn reject_private_method(&mut self, ty: &str, method: &str, span: Span) {
        let Some((home, visibility)) = self.method_homes.get(&(ty.to_string(), method.to_string()))
        else {
            return;
        };
        // The same rule a field and a free function get: the declaring module
        // and its descendants always reach it, `pub` reaches everywhere, and
        // `pub(package)` reaches the rest of its own package.
        let reachable = self.current_module.starts_with(home.as_slice())
            || match visibility {
                Visibility::Public => true,
                Visibility::Package => self.resolutions.same_package(home, &self.current_module),
                Visibility::Inherited => false,
            };
        if reachable {
            return;
        }
        let error = TypeError::PrivateMethod {
            ty: ty.to_string(),
            name: method.to_string(),
            module: home.join("::"),
        };
        self.emit(error, span);
    }

    /// The single container-shaped (`Vec` / `Slice` / `Tuple`, ref
    /// transparent) parameter type at position `i` across the
    /// candidate signatures, or `None` when absent or ambiguous.
    fn unique_container_expectation(&mut self, candidates: &[Vec<Ty>], i: usize) -> Option<Ty> {
        let mut found: Option<(Ty, String)> = None;
        for sig in candidates {
            let Some(&ty) = sig.get(i) else { continue };
            let mut peeled = self.infer.resolve(self.tcx, ty);
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled) {
                peeled = self.infer.resolve(self.tcx, *inner);
            }
            if !matches!(
                self.tcx.kind(peeled),
                Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Tuple(_))
            ) {
                continue;
            }
            let rendered = render_ty(self.tcx, ty);
            match &found {
                Some((_, existing)) if *existing == rendered => {}
                Some(_) => return None,
                None => found = Some((ty, rendered)),
            }
        }
        found.map(|(ty, _)| ty)
    }

    fn result_adt_ty(&mut self, ok: Ty, err: Ty) -> Ty {
        let substs = crate::Substs::from_types([ok, err]);
        let def = gossamer_resolve::DefId::local(u32::MAX);
        self.tcx.register_def_name(def, "Result");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    fn raw_stdlib_helper_ret(&mut self, name: &str) -> Option<Ty> {
        let ok = match name {
            "__gos_pem_decode_raw" => self.tuple_str_bytes_ty(),
            "__gos_pem_decode_all_raw" => {
                let entry = self.tuple_str_bytes_ty();
                self.tcx.intern(TyKind::Vec(entry))
            }
            "__gos_fs_metadata_raw" => self.tuple_fs_metadata_ty(),
            "__gos_x509_parse_pem_raw" => self.tuple_cert_info_ty(),
            "__gos_tar_read_raw" | "__gos_zip_read_raw" => {
                let entry = self.tuple_archive_entry_ty();
                self.tcx.intern(TyKind::Vec(entry))
            }
            "__gos_time_location_raw" | "__gos_time_fixed_location_raw" => self.tcx.string_ty(),
            "__gos_time_civil_raw" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Tuple(vec![i; 9]))
            }
            "__gos_time_resolve_raw" => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Tuple(vec![i; 3]))
            }
            "__gos_time_format_in_raw" => self.tcx.string_ty(),
            "__gos_time_add_date_raw" => self.tcx.int_ty(IntTy::I64),
            _ => return None,
        };
        let err = self.tcx.dyn_error_ty();
        Some(self.result_adt_ty(ok, err))
    }

    fn tuple_cert_info_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let i = self.tcx.int_ty(IntTy::I64);
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        let vec_str = self.tcx.intern(TyKind::Vec(s));
        self.tcx
            .intern(TyKind::Tuple(vec![s, s, vec_u8, i, i, vec_str, vec_u8]))
    }

    fn tuple_fs_metadata_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(IntTy::I64);
        let b = self.tcx.bool_ty();
        self.tcx.intern(TyKind::Tuple(vec![i, b, b, b, b, i]))
    }

    fn tuple_archive_entry_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        let b = self.tcx.bool_ty();
        self.tcx.intern(TyKind::Tuple(vec![s, vec_u8, b]))
    }

    fn tuple_str_bytes_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        self.tcx.intern(TyKind::Tuple(vec![s, vec_u8]))
    }

    fn payload_adt_method_owner(&mut self, ty: Ty) -> Option<&'static str> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX => Some("Result"),
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 1 => Some("Option"),
            _ => None,
        }
    }

    /// Explicit argument counts a method on a built-in iterator receiver
    /// accepts: the combinator's declared arity less the data slot the
    /// receiver fills. `None` for a name the iterator surface does not
    /// declare, which the unresolved-method path reports instead.
    fn iterator_method_arities(name: &str) -> Option<&'static [usize]> {
        match name {
            "next" => Some(&[0]),
            // `count` answers the length, or the accepted-element count
            // when handed a predicate.
            "count" => Some(&[0, 1]),
            _ => match Self::std_combinator_arity("iter", name)?.checked_sub(1)? {
                0 => Some(&[0]),
                1 => Some(&[1]),
                2 => Some(&[2]),
                _ => None,
            },
        }
    }

    /// Full argument arity (closure/seed args plus the trailing data
    /// arg) of a std data-last combinator the checker can type, or
    /// `None` for names it has no signature row for.
    fn std_combinator_arity(module: &str, name: &str) -> Option<usize> {
        let arity = match (module, name) {
            (
                "result",
                "map" | "map_err" | "and_then" | "or_else" | "unwrap_or" | "unwrap_or_else",
            ) => 2,
            ("result", "expect") => 2,
            ("result", "ok" | "err" | "is_ok" | "is_err" | "unwrap") => 1,
            (
                "option",
                "map" | "and_then" | "filter" | "or" | "or_else" | "unwrap_or" | "unwrap_or_else"
                | "zip" | "ok_or" | "ok_or_else",
            ) => 2,
            ("option", "expect") => 2,
            ("option", "flatten" | "is_some" | "is_none" | "iter" | "unwrap") => 1,
            (
                "iter",
                "collect" | "count" | "sum" | "product" | "min" | "max" | "once" | "range"
                | "range_inclusive" | "repeat",
            ) => {
                if matches!(name, "range" | "range_inclusive" | "repeat") {
                    2
                } else {
                    1
                }
            }
            ("iter", "fold" | "scan") => 3,
            ("iter", "take" | "skip" | "step_by" | "chain" | "zip" | "windows" | "chunks") => 2,
            ("iter", "enumerate" | "rev" | "dedup" | "flatten" | "pairwise" | "unzip") => 1,
            ("iter", "empty") => 0,
            (
                "iter",
                "for_each" | "map" | "filter" | "filter_map" | "flat_map" | "reduce" | "sum_by"
                | "product_by" | "any" | "all" | "find" | "position" | "find_map" | "take_while"
                | "skip_while" | "partition" | "sort_by" | "sort_by_key" | "min_by" | "max_by"
                | "min_by_key" | "max_by_key" | "chunk_by" | "count_by",
            ) => 2,
            _ => return None,
        };
        Some(arity)
    }

    fn iter_adapter_result_ty(&mut self, item: Ty, lazy_result: bool) -> Ty {
        if lazy_result {
            self.tcx.iterator_ty(item)
        } else {
            self.tcx.intern(TyKind::Vec(item))
        }
    }

    fn check_question_mark(&mut self, ty: Ty, span: Span) -> Ty {
        let Some((inner_family, payload)) = self.try_family_and_payload(ty) else {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "the operand is not a `Result` or `Option`".to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        let Some(ret) = self.current_fn_ret else {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "`?` is only valid inside a function with a compatible return type"
                        .to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        let Some((ret_family, _)) = self.try_family_and_payload(ret) else {
            let ty = self.render_public_ty(ret);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "the enclosing function does not return `Result` or `Option`"
                        .to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        if inner_family != ret_family {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::QuestionMarkUnsupported {
                    ty,
                    reason: "the operand and enclosing function use different propagation types"
                        .to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        payload
    }

    fn try_family_and_payload(&mut self, ty: Ty) -> Option<(TryFamily, Ty)> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let TyKind::Adt { def, substs } = self.tcx.kind(resolved)? else {
            return None;
        };
        let payload = substs.types().first().copied()?;
        match def.local {
            u32::MAX => Some((TryFamily::Result, payload)),
            n if n == u32::MAX - 1 => Some((TryFamily::Option, payload)),
            _ => match self.tcx.def_name(*def)? {
                "Result" => Some((TryFamily::Result, payload)),
                "Option" => Some((TryFamily::Option, payload)),
                _ => None,
            },
        }
    }

    /// `(ok, err)` payload types of a Result-shaped `ty`. A still-free
    /// inference var is unified with a fresh `Result<?, ?>` so the
    /// payload slots exist for the combinator row to pin against.
    fn result_payload_tys(&mut self, ty: Ty, span: Span) -> Option<(Ty, Ty)> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX => {
                let tys = substs.types();
                Some((tys.first().copied()?, tys.get(1).copied()?))
            }
            Some(TyKind::Var(_)) => {
                let ok = self.fresh();
                let err = self.fresh();
                let shaped = self.result_adt_ty(ok, err);
                self.unify(resolved, shaped, span);
                Some((ok, err))
            }
            _ => None,
        }
    }

    /// Payload type of an Option-shaped `ty`, unifying a free var
    /// with `Option<?>` the same way as [`Self::result_payload_tys`].
    fn option_payload_ty(&mut self, ty: Ty, span: Span) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX - 1 => {
                substs.types().first().copied()
            }
            Some(TyKind::Var(_)) => {
                let payload = self.fresh();
                let shaped = self.option_adt_ty(payload);
                self.unify(resolved, shaped, span);
                Some(payload)
            }
            _ => None,
        }
    }

    /// Element type of a sequence-shaped `ty` (`Vec`, `Iterator`, slice, or
    /// fixed array, ref-transparent), unifying a free var with `Vec<?>`.
    fn sequence_elem_ty(&mut self, ty: Ty, span: Span) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Iterator(elem)
                | TyKind::Range(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. },
            ) => Some(*elem),
            Some(TyKind::Var(_)) => {
                let elem = self.fresh();
                let shaped = self.tcx.intern(TyKind::Vec(elem));
                self.unify(resolved, shaped, span);
                Some(elem)
            }
            _ => None,
        }
    }

    /// Pins a callable argument's parameter types to `inputs` and
    /// returns its output type. This is the load-bearing step for
    /// lifted closures: binding the param inference vars here is what
    /// keeps the HIR lift pass from pinning an unresolved String/Error
    /// param to i64 (which renders the payload as a raw pointer on the
    /// compiled tiers).
    fn callable_output(&mut self, callable_ty: Ty, inputs: &[Ty], span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, callable_ty);
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) => {
                self.check_callable_arity(&sig, inputs, resolved, span);
                sig.output
            }
            Some(TyKind::FnDef { def, .. }) => match self.fn_sigs.get(&def).cloned() {
                Some(sig) => {
                    self.check_callable_arity(&sig, inputs, resolved, span);
                    sig.output
                }
                None => self.fresh(),
            },
            Some(TyKind::Var(_)) => {
                let output = self.fresh();
                let shaped = self.tcx.intern(TyKind::FnPtr(FnSig {
                    inputs: inputs.to_vec(),
                    output,
                }));
                // `unify` reads its first argument as the expected type: the
                // callable shape this slot declares, not what was passed.
                self.unify(shaped, resolved, span);
                output
            }
            // A callback slot given something that plainly cannot be called.
            // Left unreported, the call reached a runtime that read the
            // argument as a name and failed with an unrelated message.
            Some(kind) if is_plainly_not_callable(&kind) => {
                let output = self.fresh();
                let shaped = self.tcx.intern(TyKind::FnPtr(FnSig {
                    inputs: inputs.to_vec(),
                    output,
                }));
                let expected = self.render_public_ty(shaped);
                let found = self.render_public_ty(resolved);
                self.emit(TypeError::TypeMismatch { expected, found }, span);
                output
            }
            _ => self.fresh(),
        }
    }

    /// Unifies a callable's declared parameters with the slot's, and
    /// reports a callable whose parameter count does not match.
    ///
    /// A silent skip let a callback of the wrong arity reach a runtime
    /// that raised an argument-count error with no source position.
    fn check_callable_arity(&mut self, sig: &FnSig, inputs: &[Ty], actual: Ty, span: Span) {
        if sig.inputs.len() == inputs.len() {
            for (have, want) in sig.inputs.iter().zip(inputs) {
                self.unify(*have, *want, span);
            }
            return;
        }
        let output = self.fresh();
        let shaped = self.tcx.intern(TyKind::FnPtr(FnSig {
            inputs: inputs.to_vec(),
            output,
        }));
        let expected = self.render_public_ty(shaped);
        let found = self.render_public_ty(actual);
        self.emit(TypeError::TypeMismatch { expected, found }, span);
    }

    /// Unifies `var` with `fallback` when it is still an unresolved
    /// inference variable. Used to default a combinator's unpinned
    /// payload slot to the receiver's payload type: an unresolved
    /// slot blocks `{:?}` lowering on the compiled tiers.
    fn default_free_var_to(&mut self, var: Ty, fallback: Ty, span: Span) {
        let resolved = self.infer.resolve(self.tcx, var);
        if matches!(self.tcx.kind(resolved), Some(TyKind::Var(_))) {
            self.unify(resolved, fallback, span);
        }
    }

    /// Resolves `ty` to a Result if possible: an already-Result type
    /// is returned as-is, a free var is unified with
    /// `Result<ok, err>`, anything else degrades to a fresh var.
    fn shape_result_like(&mut self, ty: Ty, ok: Ty, err: Ty, span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX => {
                // Pin the payload slots: a combinator closure's `Ok(v)`
                // body leaves the Err slot a free var, and an
                // unresolved payload blocks `{:?}` lowering on the
                // compiled tiers.
                let tys = substs.types();
                if let (Some(&have_ok), Some(&have_err)) = (tys.first(), tys.get(1)) {
                    self.unify(have_ok, ok, span);
                    self.unify(have_err, err, span);
                }
                resolved
            }
            Some(TyKind::Var(_)) => {
                let shaped = self.result_adt_ty(ok, err);
                self.unify(resolved, shaped, span);
                shaped
            }
            _ => self.fresh(),
        }
    }

    /// Option-shaped counterpart of [`Self::shape_result_like`].
    fn shape_option_like(&mut self, ty: Ty, payload: Ty, span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) if def.local == u32::MAX - 1 => {
                let tys = substs.types();
                if let Some(&have) = tys.first() {
                    self.unify(have, payload, span);
                }
                resolved
            }
            Some(TyKind::Var(_)) => {
                let shaped = self.option_adt_ty(payload);
                self.unify(resolved, shaped, span);
                shaped
            }
            _ => self.fresh(),
        }
    }

    /// Return type of a known std data-last combinator call
    /// (`result::*` / `option::*` / closure-taking `iter::*`, free,
    /// piped, or method form). `lead_tys` are the leading closure /
    /// seed argument types; `data_ty` is the trailing data argument
    /// (the method receiver, or the piped value). Unifies closure
    /// parameter vars with the data payload types so lifted closure
    /// bodies keep String/Error params instead of the i64 pin.
    #[allow(
        clippy::too_many_lines,
        reason = "one row per std combinator; splitting the table would obscure the signature catalog"
    )]
    fn std_combinator_ty(
        &mut self,
        module: &str,
        name: &str,
        lead_tys: &[Ty],
        data_ty: Ty,
        span: Span,
    ) -> Option<Ty> {
        self.std_combinator_ty_at(module, name, lead_tys, data_ty, DataPosition::Last, span)
    }

    /// [`Self::std_combinator_ty`] with the data argument's position in the
    /// call as written. Only a combinator that pairs its two inputs - `zip` -
    /// reads differently between the two spellings.
    #[allow(
        clippy::too_many_lines,
        reason = "one row per std combinator; splitting the table would obscure the signature catalog"
    )]
    fn std_combinator_ty_at(
        &mut self,
        module: &str,
        name: &str,
        lead_tys: &[Ty],
        data_ty: Ty,
        data_position: DataPosition,
        span: Span,
    ) -> Option<Ty> {
        if Self::std_combinator_arity(module, name)? != lead_tys.len() + 1 {
            return None;
        }
        match module {
            "result" => {
                let (ok, err) = self.result_payload_tys(data_ty, span)?;
                let ty = match name {
                    "map" => {
                        let mapped = self.callable_output(lead_tys[0], &[ok], span);
                        self.result_adt_ty(mapped, err)
                    }
                    "map_err" => {
                        let mapped = self.callable_output(lead_tys[0], &[err], span);
                        self.result_adt_ty(ok, mapped)
                    }
                    "and_then" => {
                        let out = self.callable_output(lead_tys[0], &[ok], span);
                        let next_ok = self.fresh();
                        let shaped = self.shape_result_like(out, next_ok, err, span);
                        // An `Err`-only handler leaves the next Ok type
                        // free; default it to the receiver's so the
                        // result is fully resolved for the compiled
                        // tiers' `{:?}` lowering.
                        self.default_free_var_to(next_ok, ok, span);
                        shaped
                    }
                    "or_else" => {
                        let out = self.callable_output(lead_tys[0], &[err], span);
                        let next_err = self.fresh();
                        let shaped = self.shape_result_like(out, ok, next_err, span);
                        self.default_free_var_to(next_err, err, span);
                        shaped
                    }
                    "unwrap_or" => {
                        self.unify(ok, lead_tys[0], span);
                        ok
                    }
                    "unwrap" => ok,
                    "expect" => {
                        let s = self.tcx.string_ty();
                        let message = self.peel_refs(lead_tys[0]);
                        self.unify(s, message, span);
                        ok
                    }
                    // `Ok(v)` yields `v` and `Err(e)` yields `f(e)`, so
                    // both arms answer the Ok payload and the handler
                    // is pinned to produce one. A handler that diverges
                    // contributes no value and is left alone.
                    "unwrap_or_else" => {
                        let out = self.callable_output(lead_tys[0], &[err], span);
                        let resolved_out = self.infer.resolve(self.tcx, out);
                        if !matches!(self.tcx.kind(resolved_out), Some(TyKind::Never)) {
                            self.unify(ok, out, span);
                        }
                        ok
                    }
                    "ok" => self.option_adt_ty(ok),
                    "err" => self.option_adt_ty(err),
                    "is_ok" | "is_err" => self.tcx.bool_ty(),
                    _ => return None,
                };
                Some(ty)
            }
            "option" => {
                let payload = self.option_payload_ty(data_ty, span)?;
                let ty = match name {
                    "map" => {
                        let mapped = self.callable_output(lead_tys[0], &[payload], span);
                        self.option_adt_ty(mapped)
                    }
                    "and_then" => {
                        let out = self.callable_output(lead_tys[0], &[payload], span);
                        let next = self.fresh();
                        let shaped = self.shape_option_like(out, next, span);
                        self.default_free_var_to(next, payload, span);
                        shaped
                    }
                    "filter" => {
                        let out = self.callable_output(lead_tys[0], &[payload], span);
                        let bool_ty = self.tcx.bool_ty();
                        self.unify(bool_ty, out, span);
                        self.option_adt_ty(payload)
                    }
                    "or" => {
                        let shaped = self.option_adt_ty(payload);
                        self.unify(shaped, lead_tys[0], span);
                        shaped
                    }
                    "or_else" => {
                        let out = self.callable_output(lead_tys[0], &[], span);
                        self.shape_option_like(out, payload, span)
                    }
                    "ok_or" => {
                        let err = self.peel_refs(lead_tys[0]);
                        self.result_adt_ty(payload, err)
                    }
                    "ok_or_else" => {
                        let err = self.callable_output(lead_tys[0], &[], span);
                        self.result_adt_ty(payload, err)
                    }
                    "unwrap_or" => {
                        self.unify(payload, lead_tys[0], span);
                        payload
                    }
                    "unwrap" => payload,
                    "expect" => {
                        let s = self.tcx.string_ty();
                        let message = self.peel_refs(lead_tys[0]);
                        self.unify(s, message, span);
                        payload
                    }
                    // Same mixed-type rationale as the Result row: both arms
                    // answer the payload, so the fallback is pinned to
                    // produce one. A fallback that diverges contributes no
                    // value and is left alone.
                    "unwrap_or_else" => {
                        let out = self.callable_output(lead_tys[0], &[], span);
                        let resolved_out = self.infer.resolve(self.tcx, out);
                        if !matches!(self.tcx.kind(resolved_out), Some(TyKind::Never)) {
                            self.unify(payload, out, span);
                        }
                        payload
                    }
                    "zip" => {
                        let other = match self.option_payload_ty(lead_tys[0], span) {
                            Some(other) => other,
                            None => self.fresh(),
                        };
                        let pair = match data_position {
                            DataPosition::Receiver => {
                                self.tcx.intern(TyKind::Tuple(vec![payload, other]))
                            }
                            DataPosition::Last => {
                                self.tcx.intern(TyKind::Tuple(vec![other, payload]))
                            }
                        };
                        self.option_adt_ty(pair)
                    }
                    "flatten" => {
                        let inner = self.fresh();
                        self.shape_option_like(payload, inner, span)
                    }
                    "is_some" | "is_none" => self.tcx.bool_ty(),
                    "iter" => self.tcx.intern(TyKind::Vec(payload)),
                    _ => return None,
                };
                Some(ty)
            }
            "iter" => {
                // A constructor or adapter answers an iterator only when it
                // was handed one; a collection traverses eagerly.
                let edition_lazy_result = false;
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                if matches!(name, "range" | "range_inclusive") {
                    self.unify(i64_ty, lead_tys[0], span);
                    self.unify(i64_ty, data_ty, span);
                    return Some(self.iter_adapter_result_ty(i64_ty, edition_lazy_result));
                }
                if name == "once" {
                    return Some(self.iter_adapter_result_ty(data_ty, edition_lazy_result));
                }
                if name == "repeat" {
                    self.unify(i64_ty, data_ty, span);
                    return Some(self.iter_adapter_result_ty(lead_tys[0], edition_lazy_result));
                }
                let all_tier_iterator_input = is_iterator_method(name);
                let data_is_iterator = matches!(
                    self.tcx.kind_of(self.infer.resolve(self.tcx, data_ty)),
                    TyKind::Iterator(_) | TyKind::Range(_)
                );
                // `enumerate` pairs an index with each element as it is
                // asked for, so it answers an iterator whatever it is
                // handed, in every edition.
                let lazy_result =
                    edition_lazy_result || data_is_iterator || matches!(name, "enumerate");
                if data_is_iterator && !all_tier_iterator_input {
                    let found = self.render_public_ty(data_ty);
                    self.emit(
                        TypeError::TypeMismatch {
                            expected: "Vec<T>".to_string(),
                            found,
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                }
                let elem = self.sequence_elem_ty(data_ty, span)?;
                let bool_ty = self.tcx.bool_ty();
                let ty = match name {
                    "collect" => self.tcx.intern(TyKind::Vec(elem)),
                    "count" => i64_ty,
                    // The sum of a sequence has its element's type.
                    "sum" | "product" => {
                        let elem = self.infer.resolve(self.tcx, elem);
                        match self.tcx.kind(elem) {
                            Some(TyKind::Float(float)) => self.tcx.float_ty(*float),
                            Some(TyKind::Int(int)) => self.tcx.int_ty(*int),
                            // An element the receiver has not pinned yet
                            // settles together with the sum rather than
                            // fixing the result to `i64` here.
                            Some(TyKind::Var(_)) => elem,
                            _ => i64_ty,
                        }
                    }
                    "min" | "max" => self.option_adt_ty(elem),
                    "take" | "skip" | "step_by" => {
                        self.unify(i64_ty, lead_tys[0], span);
                        self.iter_adapter_result_ty(elem, lazy_result)
                    }
                    "enumerate" => {
                        let pair = self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem]));
                        self.iter_adapter_result_ty(pair, lazy_result)
                    }
                    "rev" => self.iter_adapter_result_ty(elem, lazy_result),
                    "dedup" => self.tcx.intern(TyKind::Vec(elem)),
                    "chain" => {
                        let other = self.sequence_elem_ty(lead_tys[0], span).unwrap_or(elem);
                        self.unify(elem, other, span);
                        self.iter_adapter_result_ty(elem, lazy_result)
                    }
                    "zip" => {
                        let other = self
                            .sequence_elem_ty(lead_tys[0], span)
                            .unwrap_or_else(|| self.fresh());
                        // The pair carries the two sequences in the order the
                        // call writes them, which a receiver leads and a
                        // data-last free or piped call trails.
                        let pair = match data_position {
                            DataPosition::Receiver => {
                                self.tcx.intern(TyKind::Tuple(vec![elem, other]))
                            }
                            DataPosition::Last => self.tcx.intern(TyKind::Tuple(vec![other, elem])),
                        };
                        self.iter_adapter_result_ty(pair, lazy_result)
                    }
                    "flatten" => {
                        // Flattening needs an element that is itself a
                        // sequence. A scalar element has no inner type, and
                        // inventing a fresh one let the call through to a
                        // runtime that reads the scalar as a sequence header.
                        let Some(inner) = self.sequence_elem_ty(elem, span) else {
                            let found = self.render_public_ty(data_ty);
                            self.emit(
                                TypeError::TypeMismatch {
                                    expected: "a sequence of sequences".to_string(),
                                    found,
                                },
                                span,
                            );
                            return Some(self.tcx.error_ty());
                        };
                        self.tcx.intern(TyKind::Vec(inner))
                    }
                    "pairwise" => {
                        let pair = self.tcx.intern(TyKind::Tuple(vec![elem, elem]));
                        self.tcx.intern(TyKind::Vec(pair))
                    }
                    "unzip" => {
                        // Splitting pairs needs pairs. Without an element type
                        // to take apart the call went through untyped and each
                        // tier read the missing second slot differently.
                        let resolved_elem = self.infer.resolve(self.tcx, elem);
                        let Some([left, right]) =
                            self.tcx.kind(resolved_elem).and_then(|kind| match kind {
                                TyKind::Tuple(parts) if parts.len() == 2 => {
                                    Some([parts[0], parts[1]])
                                }
                                _ => None,
                            })
                        else {
                            let found = self.render_public_ty(data_ty);
                            self.emit(
                                TypeError::TypeMismatch {
                                    expected: "a sequence of two-element tuples".to_string(),
                                    found,
                                },
                                span,
                            );
                            return Some(self.tcx.error_ty());
                        };
                        let lefts = self.tcx.intern(TyKind::Vec(left));
                        let rights = self.tcx.intern(TyKind::Vec(right));
                        self.tcx.intern(TyKind::Tuple(vec![lefts, rights]))
                    }
                    "windows" | "chunks" => {
                        self.unify(i64_ty, lead_tys[0], span);
                        let window = self.tcx.intern(TyKind::Vec(elem));
                        self.tcx.intern(TyKind::Vec(window))
                    }
                    "for_each" => {
                        let _ = self.callable_output(lead_tys[0], &[elem], span);
                        self.tcx.unit()
                    }
                    "map" => {
                        let mapped = self.callable_output(lead_tys[0], &[elem], span);
                        self.iter_adapter_result_ty(mapped, lazy_result)
                    }
                    "filter" | "take_while" | "skip_while" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        self.iter_adapter_result_ty(elem, lazy_result)
                    }
                    "filter_map" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        let mapped = match self.option_payload_ty(out, span) {
                            Some(payload) => payload,
                            None => self.fresh(),
                        };
                        self.iter_adapter_result_ty(mapped, lazy_result)
                    }
                    "flat_map" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        let mapped = self.sequence_elem_ty(out, span).unwrap_or_else(|| {
                            // Non-sequence closure output is a real
                            // bug, but the runtime flattens anything;
                            // degrade to fresh instead of erroring.
                            self.fresh()
                        });
                        self.iter_adapter_result_ty(mapped, lazy_result)
                    }
                    "fold" | "scan" => {
                        let acc = lead_tys[0];
                        let out = self.callable_output(lead_tys[1], &[acc, elem], span);
                        self.unify(acc, out, span);
                        if name == "fold" {
                            acc
                        } else {
                            self.iter_adapter_result_ty(acc, lazy_result)
                        }
                    }
                    "reduce" => {
                        let out = self.callable_output(lead_tys[0], &[elem, elem], span);
                        self.unify(elem, out, span);
                        self.option_adt_ty(elem)
                    }
                    "sum_by" | "product_by" => self.callable_output(lead_tys[0], &[elem], span),
                    "any" | "all" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        bool_ty
                    }
                    "find" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        self.option_adt_ty(elem)
                    }
                    "position" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        let i64_ty = self.tcx.int_ty(IntTy::I64);
                        self.option_adt_ty(i64_ty)
                    }
                    "find_map" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        let mapped = match self.option_payload_ty(out, span) {
                            Some(payload) => payload,
                            None => self.fresh(),
                        };
                        self.option_adt_ty(mapped)
                    }
                    "partition" => {
                        let out = self.callable_output(lead_tys[0], &[elem], span);
                        self.unify(bool_ty, out, span);
                        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
                        self.tcx.intern(TyKind::Tuple(vec![vec_ty, vec_ty]))
                    }
                    // Comparator output is an ordering integer of any
                    // width; the row pins only the element params.
                    "sort_by" => {
                        let _ = self.callable_output(lead_tys[0], &[elem, elem], span);
                        self.tcx.intern(TyKind::Vec(elem))
                    }
                    "sort_by_key" => {
                        let _ = self.callable_output(lead_tys[0], &[elem], span);
                        self.tcx.intern(TyKind::Vec(elem))
                    }
                    "min_by" | "max_by" => {
                        let _ = self.callable_output(lead_tys[0], &[elem, elem], span);
                        self.option_adt_ty(elem)
                    }
                    "min_by_key" | "max_by_key" => {
                        let _ = self.callable_output(lead_tys[0], &[elem], span);
                        self.option_adt_ty(elem)
                    }
                    "chunk_by" => {
                        let key = self.callable_output(lead_tys[0], &[elem], span);
                        let value = self.tcx.intern(TyKind::Vec(elem));
                        self.tcx.intern(TyKind::HashMap {
                            key,
                            value,
                            ordered: false,
                        })
                    }
                    "count_by" => {
                        let key = self.callable_output(lead_tys[0], &[elem], span);
                        let value = self.tcx.int_ty(IntTy::I64);
                        self.tcx.intern(TyKind::HashMap {
                            key,
                            value,
                            ordered: false,
                        })
                    }
                    _ => return None,
                };
                Some(ty)
            }
            _ => None,
        }
    }

    /// Types a full-arity std combinator free call (`result::*` /
    /// `option::*` / `iter::*` data-last forms), or emits the loud
    /// uninferrable-closure error when the name has no signature row
    /// but a closure argument is present. `None` falls back to the
    /// existing stdlib heuristics (including partial applications,
    /// which the pipe site completes).
    fn check_std_combinator_free_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        module: Option<&'static str>,
        name: &str,
    ) -> Option<Ty> {
        let module = module?;
        match Self::std_combinator_arity(module, name) {
            // A source with no data argument stands outside the data-last
            // shape the rest of this table describes: there is no sequence to
            // split off, only the element type the call site expects.
            Some(0) if args.is_empty() => {
                let elem = self.fresh();
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            Some(arity) if arity >= 1 && args.len() == arity => {
                let (lead, data) = arg_tys.split_at(arity - 1);
                let lead = lead.to_vec();
                let span = args.last().map_or(callee.span, |arg| arg.span);
                let ty = self.std_combinator_ty(module, name, &lead, data[0], span);
                // An iterator is consumed by the adapter that takes it in
                // every edition: reading it again yields nothing, so the
                // second read is reported rather than silently empty.
                if module == "iter" && ty.is_some() {
                    self.mark_consumed_iterator_args(name, args, arg_tys);
                }
                // A rowed option/result combinator at full arity whose
                // data argument is concretely non-payload-shaped (the
                // classic mistake is the swapped order, data first and
                // closure last) would run the closure slot as the data
                // value and yield the empty fallback; reject it.
                if ty.is_none() {
                    let shape = match module {
                        "option" => Some("Option"),
                        "result" => Some("Result"),
                        _ => None,
                    };
                    if let Some(shape) = shape {
                        self.emit(
                            TypeError::CombinatorDataArgMismatch {
                                combinator: format!("{module}::{name}"),
                                shape: shape.to_string(),
                            },
                            callee.span,
                        );
                        return Some(self.tcx.error_ty());
                    }
                }
                ty
            }
            // A step naming its slot (`xs |> iter::map(f, $)`) is typed at
            // the pipe site, where the data argument's type is known.
            Some(_) => None,
            // A std combinator the checker has no signature row for
            // cannot type its closure argument; the compiled tiers
            // would pin the param to i64 and print String payloads as
            // pointers. Reject loudly instead.
            None => {
                if args
                    .iter()
                    .any(|arg| matches!(arg.kind, ExprKind::Closure { .. }))
                {
                    self.emit(
                        TypeError::ClosureParamUninferred {
                            combinator: format!("{module}::{name}"),
                        },
                        callee.span,
                    );
                    return Some(self.tcx.error_ty());
                }
                None
            }
        }
    }

    fn mark_consumed_iterator_args(&mut self, name: &str, args: &[Expr], arg_tys: &[Ty]) {
        for (arg, ty) in args.iter().zip(arg_tys.iter()) {
            self.mark_consumed_iterator_expr(name, arg, *ty);
        }
    }

    /// Records the left operand of a `|>` step as a spent iterator, named
    /// after the call the step makes so the diagnostic points at it.
    fn mark_piped_iterator_consumed(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr) {
        let operation = pipe_step_operation_name(rhs).unwrap_or_else(|| "|>".to_string());
        self.mark_consumed_iterator_labelled(&operation, lhs, lhs_ty);
    }

    fn mark_consumed_iterator_expr(&mut self, name: &str, expr: &Expr, ty: Ty) {
        self.mark_consumed_iterator_labelled(&format!("iter::{name}"), expr, ty);
    }

    /// [`Self::mark_consumed_iterator_expr`] with the operation spelled out.
    fn mark_consumed_iterator_labelled(&mut self, operation: &str, expr: &Expr, ty: Ty) {
        let resolved = self.infer.resolve(self.tcx, ty);
        if !matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Iterator(_) | TyKind::Range(_))
        ) {
            return;
        }
        let ExprKind::Path(path) = &expr.kind else {
            return;
        };
        if path.segments.len() != 1 {
            return;
        }
        let Some(Resolution::Local(binding)) = self.resolutions.get(expr.id) else {
            return;
        };
        if let Some(scope) = self.consumed_iterators.last_mut() {
            scope.insert(binding, operation.to_string());
        }
    }

    /// Types Result/Option combinator *method* calls
    /// (`r.map_err(f)`, `o.map(f)`) through the same signature table
    /// as the free `result::*` / `option::*` functions, with the
    /// receiver as the data argument. Returns `None` (leaving the
    /// generic method path untouched) when the receiver is not a
    /// resolved Result/Option or the name has no row.
    fn check_payload_combinator_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        receiver_span: Span,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let module = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX => "result",
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 1 => "option",
            _ => return None,
        };
        if Self::std_combinator_arity(module, method)? != args.len() + 1 {
            return None;
        }
        let span = args.first().map_or(receiver_span, |arg| arg.span);
        // Check a closure argument against the payload it will receive. Its
        // body is checked once, so an unconstrained parameter leaves any
        // projection out of the payload as a free variable that later unifies
        // with whatever context demands - and the mapped payload never gets
        // the closure's real return type.
        let closure_inputs = self.vec_combinator_closure_inputs(method, resolved);
        let lead_tys: Vec<Ty> = args
            .iter()
            .map(|arg| match (&closure_inputs, &arg.kind) {
                (Some(inputs), ExprKind::Closure { params, .. })
                    if params.len() == inputs.len() =>
                {
                    let output = self.fresh();
                    let sig = FnSig {
                        inputs: inputs.clone(),
                        output,
                    };
                    let want = self.tcx.intern(TyKind::FnPtr(sig));
                    self.check_expr_expecting(arg, Expectation::HasType(want))
                }
                _ => self.check_expr(arg),
            })
            .collect();
        self.std_combinator_ty_at(
            module,
            method,
            &lead_tys,
            resolved,
            DataPosition::Receiver,
            span,
        )
    }

    fn check_unary(
        &mut self,
        op: UnaryOp,
        operand: &Expr,
        span: Span,
        _expected: Expectation,
    ) -> Ty {
        // A borrow preserves the operand's concrete owned type. The resulting
        // reference may then unsize from an array or Vec reference to a slice.
        let operand_expected = match op {
            UnaryOp::RefShared | UnaryOp::RefMut => Expectation::None,
            _ => Expectation::None,
        };
        let previous_suppression = self.suppressed.borrow_read_conflict;
        if matches!(op, UnaryOp::RefShared | UnaryOp::RefMut) {
            self.suppressed.borrow_read_conflict = true;
        }
        let operand_ty = self.check_expr_expecting(operand, operand_expected);
        self.suppressed.borrow_read_conflict = previous_suppression;
        let resolved = self.infer.resolve(self.tcx, operand_ty);
        match op {
            UnaryOp::Not => {
                if matches!(self.tcx.kind(resolved), Some(TyKind::Bool)) {
                    self.tcx.bool_ty()
                } else if self.reject_operator_off_bound(resolved, "!", "not", span) {
                    self.tcx.error_ty()
                } else if self.adt_name_of(resolved).is_some() {
                    // `!x` on a user struct / enum routes to its `not` impl
                    // (a zero-arg method on the receiver), the same way `-x`
                    // routes to `neg`. The operand node is anchored to its
                    // resolved nominal type so tier lowering dispatches the
                    // call.
                    self.record(operand.id, resolved);
                    if let Some(ret) = self.adt_op_method_ret(resolved, "not", 0) {
                        ret
                    } else {
                        let ty = self.render_public_ty(resolved);
                        self.emit(
                            TypeError::UnresolvedOpImpl {
                                op: "!".to_string(),
                                trait_name: "Not".to_string(),
                                method: "not".to_string(),
                                ty,
                            },
                            span,
                        );
                        self.tcx.error_ty()
                    }
                } else if self.is_concrete(resolved) && !self.is_integer(resolved) {
                    let lhs = self.render_public_ty(resolved);
                    self.emit(
                        TypeError::UnresolvedOp {
                            op: "!".to_string(),
                            lhs,
                            rhs: String::new(),
                        },
                        span,
                    );
                    self.tcx.error_ty()
                } else {
                    operand_ty
                }
            }
            UnaryOp::Neg => {
                // `-x` on a user struct / enum routes to its `neg` impl
                // (a zero-arg method on the receiver); the result is that
                // method's return type, and the operand node is anchored
                // to its resolved nominal type so tier lowering dispatches
                // the call. An ADT with no `impl Neg` is rejected here
                // rather than faulting at runtime. Scalars keep the
                // operand type.
                if self.reject_operator_off_bound(resolved, "-", "neg", span) {
                    self.tcx.error_ty()
                } else if self.adt_name_of(resolved).is_some() {
                    self.record(operand.id, resolved);
                    if let Some(ret) = self.adt_op_method_ret(resolved, "neg", 0) {
                        ret
                    } else {
                        let ty = self.render_public_ty(resolved);
                        self.emit(
                            TypeError::UnresolvedOpImpl {
                                op: "-".to_string(),
                                trait_name: "Neg".to_string(),
                                method: "neg".to_string(),
                                ty,
                            },
                            span,
                        );
                        self.tcx.error_ty()
                    }
                } else {
                    operand_ty
                }
            }
            UnaryOp::RefShared | UnaryOp::RefMut => {
                self.check_reference_unary(op, operand, operand_ty)
            }
            UnaryOp::Deref => {
                // `*x` strips a single `&T` / `&mut T` wrapper.
                // For any other concrete operand shape the deref is
                // an identity (matches the interp's behaviour on
                // for-loop bound elements where the iterator hands
                // back values rather than references). Without
                // either pinning, downstream `println!("{}", *x)`
                // dispatches via `TyKind::Var → StrPtr` and tries
                // to dereference the value as a pointer - segv.
                let resolved = self.infer.resolve(self.tcx, operand_ty);
                match self.tcx.kind(resolved) {
                    Some(TyKind::Ref { inner, .. }) => *inner,
                    _ => operand_ty,
                }
            }
        }
    }

    fn check_reference_unary(&mut self, op: UnaryOp, operand: &Expr, operand_ty: Ty) -> Ty {
        let root = Self::place_root_name(operand).unwrap_or_else(|| "value".to_string());
        if self.reject_range_borrow(op, operand) {
            return self.tcx.error_ty();
        }
        let mutability = if op == UnaryOp::RefMut {
            let conflict = self
                .active_mutable_borrower(&root)
                .or_else(|| self.active_shared_borrower(&root))
                .map(str::to_string);
            if let Some(borrower) = conflict {
                self.emit(
                    TypeError::MutableReferenceConflict {
                        root: root.clone(),
                        borrower,
                    },
                    operand.span,
                );
            }
            match self.place_mutability(operand) {
                PlaceMut::ImmutableBinding => self.emit(
                    TypeError::MutableReferenceToImmutable { name: root.clone() },
                    operand.span,
                ),
                PlaceMut::SharedReference => self.emit(
                    TypeError::AssignThroughSharedReference { name: root.clone() },
                    operand.span,
                ),
                PlaceMut::NotAReference => self.emit(
                    TypeError::DerefWriteToNonReference { name: root.clone() },
                    operand.span,
                ),
                PlaceMut::Writable | PlaceMut::Unknown => {}
            }
            Mutbl::Mut
        } else {
            if let Some(borrower) = self.active_mutable_borrower(&root).map(str::to_string) {
                self.emit(
                    TypeError::BorrowedPlaceConflict {
                        root,
                        borrower,
                        action: "read through a new shared reference to",
                    },
                    operand.span,
                );
            }
            Mutbl::Not
        };
        self.tcx.intern(TyKind::Ref {
            mutability,
            inner: operand_ty,
        })
    }

    /// If `expr` is a tuple-variant constructor call (`E::B(1)`), the `Adt`
    /// type of its enum. Used to anchor comparison operands - the constructor's
    /// result is otherwise a fresh variable unless used at a typed site, which
    /// leaves an inline `E::B(1) < E::B(2)` undispatchable. Scoped to the
    /// comparison arm so it does not retype constructors feeding a `let`
    /// destructure (whose compiled-tier payload extraction wants the bare form).
    /// Instantiation of a generic enum's parameters for the constructor
    /// call at `node`, allocating fresh variables on first use and returning
    /// the same ones afterwards.
    ///
    /// Returns `None` for a non-generic enum, whose payload types carry no
    /// parameters and whose `Adt` is cached whole in `enum_tys`.
    fn variant_ctor_instantiation(
        &mut self,
        node: NodeId,
        enum_name: &str,
    ) -> Option<(DefId, Vec<Ty>)> {
        if let Some(found) = self.variant_ctor_substs.get(&node) {
            return Some(found.clone());
        }
        let def = *self.user_type_defs.get(enum_name)?;
        let arity = self.struct_generic_arity.get(&def).copied()?;
        let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
        self.variant_ctor_substs.insert(node, (def, substs.clone()));
        Some((def, substs))
    }

    fn variant_ctor_enum_ty(&self, expr: &Expr) -> Option<Ty> {
        let ExprKind::Call { callee, .. } = &expr.kind else {
            return None;
        };
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let n = path.segments.len();
        if n < 2 {
            return None;
        }
        let enum_name = path.segments[n - 2].name.name.as_str();
        let var_name = path.segments[n - 1].name.name.as_str();
        if self
            .enum_variants
            .get(enum_name)
            .is_some_and(|vs| vs.contains(var_name))
        {
            self.enum_tys.get(enum_name).copied()
        } else {
            None
        }
    }

    fn both_integer_types(&mut self, lhs: Ty, rhs: Ty) -> bool {
        let lhs = self.infer.resolve(self.tcx, lhs);
        let rhs = self.infer.resolve(self.tcx, rhs);
        matches!(
            (self.tcx.kind(lhs), self.tcx.kind(rhs)),
            (Some(TyKind::Int(_)), Some(TyKind::Int(_)))
        )
    }

    /// Records what the right of `|>` is before its stage is checked: a
    /// bare path there is a callee, and a call or method call receives
    /// the piped value as its trailing argument during lowering, so the
    /// arity checks account for one argument the source does not spell.
    fn record_pipe_stage(&mut self, op: BinaryOp, rhs: &Expr) {
        if op != BinaryOp::PipeGt {
            return;
        }
        match &rhs.kind {
            ExprKind::Path(_) => {
                self.callee_path_nodes.insert(rhs.id);
            }
            ExprKind::Call { callee, .. } => {
                self.pipe_stage_callees.insert(callee.id);
            }
            ExprKind::MethodCall { .. } => {
                self.pipe_stage_callees.insert(rhs.id);
            }
            _ => {}
        }
    }

    fn check_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Ty {
        self.record_pipe_stage(op, rhs);
        let lhs_ty = self.check_expr(lhs);
        if op == BinaryOp::PipeGt && matches!(rhs.kind, ExprKind::MethodCall { .. }) {
            self.pipe_stage_arg_tys.insert(rhs.id, lhs_ty);
        }
        let rhs_ty = self.check_pipe_rhs(op, lhs_ty, rhs);
        match op {
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => {
                // Anchor a variant-constructor operand to its enum so an inline
                // same-variant comparison (`E::B(1) < E::B(2)`) can dispatch -
                // both sides are otherwise fresh variables.
                let lhs_ty = if let Some(e) = self.variant_ctor_enum_ty(lhs) {
                    self.record(lhs.id, e);
                    e
                } else {
                    lhs_ty
                };
                let rhs_ty = if let Some(e) = self.variant_ctor_enum_ty(rhs) {
                    self.record(rhs.id, e);
                    e
                } else {
                    rhs_ty
                };
                // A byte literal compares against any integer operand without
                // an explicit `as i64`: `s[i] == b'>'`. A byte literal is an
                // `Int` value on every tier, so re-typing its node to the
                // integer operand's type lets the comparison flow unchanged.
                if !self.both_integer_types(lhs_ty, rhs_ty)
                    && !self.coerce_byte_literal_cmp(lhs, lhs_ty, rhs, rhs_ty)
                {
                    self.unify_operands(op, lhs, lhs_ty, rhs, rhs_ty, span);
                }
                self.tcx.bool_ty()
            }
            BinaryOp::And | BinaryOp::Or => {
                let bool_ty = self.tcx.bool_ty();
                self.unify(bool_ty, lhs_ty, lhs.span);
                self.unify(bool_ty, rhs_ty, rhs.span);
                bool_ty
            }
            BinaryOp::PipeGt => self.pipe_result_ty(lhs, lhs_ty, rhs, rhs_ty),
            _ => {
                // String concatenation accepts a borrowed RHS:
                // `"hello, " + &name` (the documented spelling). Peel
                // the reference before unifying so the expression
                // stays `String` instead of failing `String != &T`.
                if op == BinaryOp::Add {
                    let l = self.infer.resolve(self.tcx, lhs_ty);
                    if matches!(self.tcx.kind_of(l), TyKind::String) {
                        let r = self.infer.resolve(self.tcx, rhs_ty);
                        if let TyKind::Ref { inner, .. } = self.tcx.kind_of(r) {
                            let inner = *inner;
                            self.unify(lhs_ty, inner, span);
                            return lhs_ty;
                        }
                    }
                }
                // Arithmetic / bitwise on a user struct/enum routes to its
                // operator impl (`+` -> `add`, `|` -> `bitor`, ...); the
                // result is that method's return type. Dispatch is
                // receiver-first (the left operand is `self`), so an ADT
                // operand with no such impl - or an ADT appearing only on
                // the right of a non-ADT left operand - is rejected here
                // rather than miscompiling to a runtime fault.
                if let Some(method) = arith_op_method(op) {
                    if self.reject_operands_off_bound(lhs_ty, rhs_ty, op.as_str(), method, span) {
                        return self.tcx.error_ty();
                    }
                    let lhs_res = self.infer.resolve(self.tcx, lhs_ty);
                    let rhs_res = self.infer.resolve(self.tcx, rhs_ty);
                    let lhs_adt = self.operand_nominal_name_of(lhs_res);
                    let rhs_adt = self.operand_nominal_name_of(rhs_res);
                    if lhs_adt.is_some() || rhs_adt.is_some() {
                        // Anchor ADT operand nodes to their resolved
                        // nominal type: tier lowering dispatches the
                        // impl-method call off the operand node's type,
                        // which otherwise may stay an inference var
                        // (enum locals in particular).
                        if lhs_adt.is_some() {
                            self.record(lhs.id, lhs_res);
                        }
                        if rhs_adt.is_some() {
                            self.record(rhs.id, rhs_res);
                        }
                        if lhs_adt.is_some()
                            && let Some(ret) = self.adt_op_method_ret(lhs_res, method, 1)
                        {
                            return ret;
                        }
                        let ty = if lhs_adt.is_some() { lhs_res } else { rhs_res };
                        let ty = self.render_public_ty(ty);
                        self.emit(
                            TypeError::UnresolvedOpImpl {
                                op: op.as_str().to_string(),
                                trait_name: op_trait_name(method).to_string(),
                                method: method.to_string(),
                                ty,
                            },
                            span,
                        );
                        return self.tcx.error_ty();
                    }
                }
                // A byte literal joins integer arithmetic without an explicit
                // `as i64` - `s[i] - b'0'` - through the same node re-typing
                // the comparison arms apply; the result takes the integer
                // operand's type.
                let lhs_is_byte = matches!(&lhs.kind, ExprKind::Literal(Literal::Byte(_)));
                if self.coerce_byte_literal_cmp(lhs, lhs_ty, rhs, rhs_ty) {
                    return if lhs_is_byte { rhs_ty } else { lhs_ty };
                }
                self.unify_operands(op, lhs, lhs_ty, rhs, rhs_ty, span);
                lhs_ty
            }
        }
    }

    /// Unifies two operand types, reporting an integer paired with a float
    /// as the cast the reader has to write instead of a bare mismatch.
    fn unify_operands(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
        rhs_ty: Ty,
        span: Span,
    ) {
        if let Some(error) = self.numeric_operand_mismatch(op, lhs, lhs_ty, rhs, rhs_ty) {
            self.emit(error, span);
            return;
        }
        self.unify(lhs_ty, rhs_ty, span);
    }

    /// The GT0001 diagnostic for an integer operand paired with a float
    /// one, spelling the whole expression with the cast in place. `None`
    /// for every other operand pairing.
    fn numeric_operand_mismatch(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
        rhs_ty: Ty,
    ) -> Option<TypeError> {
        let left = self.infer.resolve(self.tcx, lhs_ty);
        let right = self.infer.resolve(self.tcx, rhs_ty);
        let (left_kind, right_kind) = (self.tcx.kind(left)?.clone(), self.tcx.kind(right)?.clone());
        let expected = self.render_public_ty(left);
        let found = self.render_public_ty(right);
        let op = op.as_str();
        let (left_text, right_text) = (operand_display(lhs), operand_display(rhs));
        let cast = match (left_kind, right_kind) {
            (TyKind::Int(_), TyKind::Float(_)) => {
                format!("{left_text} as {found} {op} {right_text}")
            }
            (TyKind::Float(_), TyKind::Int(_)) => {
                format!("{left_text} {op} {right_text} as {expected}")
            }
            _ => return None,
        };
        Some(TypeError::NumericOperandMismatch {
            expected,
            found,
            cast,
        })
    }

    /// Type-checks a pipe RHS closure with its parameter shaped by the value
    /// flowing in. Other expressions retain ordinary expression checking.
    fn check_pipe_rhs(&mut self, op: BinaryOp, lhs_ty: Ty, rhs: &Expr) -> Ty {
        // A pipe into a closure determines the closure's sole parameter before
        // checking its body. Delaying this until `pipe_result_ty` left method
        // calls in the body with an unresolved receiver, so a malformed
        // `s |> |s| s.slice(s, 1, 3)` skipped String's arity check and reached
        // the runtime shim with an ignored extra argument.
        if op == BinaryOp::PipeGt
            && let ExprKind::Closure { params, .. } = &rhs.kind
            && params.len() == 1
        {
            let output = self.fresh();
            let expected = self.tcx.intern(TyKind::FnPtr(FnSig {
                inputs: vec![lhs_ty],
                output,
            }));
            self.check_expr_expecting(rhs, Expectation::HasType(expected))
        } else {
            self.check_expr(rhs)
        }
    }

    fn check_direct_pipe_sig(&mut self, sig: &FnSig, lhs: &Expr, lhs_ty: Ty, rhs: &Expr) {
        if sig.inputs.len() == 1 {
            self.check_sig_param_arg(sig.inputs[0], lhs_ty, lhs);
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(rhs),
                    expected: sig.inputs.len(),
                    found: 1,
                },
                rhs.span,
            );
        }
    }

    fn check_piped_user_method_arg(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr) {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &rhs.kind
        else {
            return;
        };
        let Some(receiver_ty) = self.table.get(receiver.id) else {
            return;
        };
        let receiver_ty = self.peel_refs(receiver_ty);
        if let Some(params) = self.user_method_params_for(receiver_ty, &name.name)
            && args.len() + 1 == params.len()
            && let Some(last) = params.last().copied()
        {
            self.check_sig_param_arg(last, lhs_ty, lhs);
        }
    }

    /// Returns the result type of a `lhs |> rhs` pipe expression.
    ///
    /// `|>` desugars to `rhs(lhs)` (or `rhs(partial_args…, lhs)` for
    /// partial-application RHS). The expression type is the callee's
    /// return type, not the callee's function type. Unifies `lhs_ty`
    /// with the callee's last parameter so that un-annotated closure
    /// params (`|x| x + 1`) are pinned from the piped value's type.
    fn pipe_result_ty(&mut self, lhs: &Expr, lhs_ty: Ty, rhs: &Expr, rhs_ty: Ty) -> Ty {
        // Try to extract the callee's return type from rhs_ty first.
        let resolved = self.infer.resolve(self.tcx, rhs_ty);
        match self.tcx.kind_of(resolved).clone() {
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                if matches!(rhs.kind, ExprKind::Path(_) | ExprKind::Closure { .. }) {
                    self.check_direct_pipe_sig(&sig, lhs, lhs_ty, rhs);
                }
                // A step takes the piped value by value, so a lazy iterator
                // handed to one is spent: whatever the step does with it, the
                // next read of the binding sees a drained cursor.
                self.mark_piped_iterator_consumed(lhs, lhs_ty, rhs);
                return self.infer.resolve(self.tcx, sig.output);
            }
            TyKind::FnDef { def, .. } => {
                if let Some(sig) = self.fn_sigs.get(&def).cloned() {
                    if matches!(rhs.kind, ExprKind::Path(_)) {
                        self.check_direct_pipe_sig(&sig, lhs, lhs_ty, rhs);
                    }
                    return sig.output;
                }
            }
            _ => {}
        }
        self.check_piped_user_method_arg(lhs, lhs_ty, rhs);
        // Data-last std combinators, partially applied through the
        // pipe (`xs |> iter::map(f, $)`, `r |> result::map_err(f, $)`,
        // `r |> result::ok`): the piped value is the data argument,
        // so its payload types pin the closure params here.
        let combinator: Option<(&gossamer_ast::PathExpr, &[Expr])> = match &rhs.kind {
            ExprKind::Call { callee, args } => match &callee.kind {
                // A callee that resolved to a user `FnDef` keeps its
                // own typing even under a std-module-shaped name.
                ExprKind::Path(path)
                    if !matches!(
                        self.table
                            .get(callee.id)
                            .map(|t| self.tcx.kind_of(t).clone()),
                        Some(TyKind::FnDef { .. })
                    ) =>
                {
                    Some((path, args.as_slice()))
                }
                _ => None,
            },
            ExprKind::Path(path) => Some((path, &[])),
            _ => None,
        };
        if let Some((path, lead_args)) = combinator {
            let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
            let (module, last) = names.split_at(names.len().saturating_sub(1));
            let comb = combinator_module_name(module);
            if let (Some(comb), Some(&last)) = (comb, last.first()) {
                if Self::std_combinator_arity(comb, last) == Some(lead_args.len() + 1) {
                    let lead_tys: Vec<Ty> = lead_args
                        .iter()
                        .map(|arg| {
                            self.table
                                .get(arg.id)
                                .unwrap_or_else(|| self.tcx.error_ty())
                        })
                        .collect();
                    if let Some(ret) =
                        self.std_combinator_ty(comb, last, &lead_tys, lhs_ty, lhs.span)
                    {
                        if comb == "iter" {
                            self.mark_consumed_iterator_args(last, lead_args, &lead_tys);
                            self.mark_consumed_iterator_expr(last, lhs, lhs_ty);
                        }
                        return ret;
                    }
                }
            }
        }
        // rhs_ty might be an unresolved Var when `rhs` is a call whose arity
        // guard fired in check_call. Recover by inspecting the call's inner
        // callee type directly.
        if let ExprKind::Call {
            callee: inner_callee,
            args,
        } = &rhs.kind
        {
            let inner_ty = self.table.get(inner_callee.id).unwrap_or(rhs_ty);
            let resolved_inner = self.infer.resolve(self.tcx, inner_ty);
            match self.tcx.kind_of(resolved_inner).clone() {
                TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                    if args.len() + 1 == sig.inputs.len()
                        && let Some(last) = sig.inputs.last().copied()
                    {
                        self.check_sig_param_arg(last, lhs_ty, lhs);
                    }
                    return self.infer.resolve(self.tcx, sig.output);
                }
                TyKind::FnDef { def, .. } => {
                    if let Some(sig) = self.fn_sigs.get(&def).cloned() {
                        if args.len() + 1 == sig.inputs.len()
                            && let Some(last) = sig.inputs.last().copied()
                        {
                            self.check_sig_param_arg(last, lhs_ty, lhs);
                        }
                        return sig.output;
                    }
                }
                _ => {}
            }
        }
        rhs_ty
    }

    /// Resolved reference mutability of `expr`, or `None` when its type is
    /// not a reference. Lets a write through a `&mut T` succeed regardless
    /// of the reference binding's own declared mutability.
    fn expr_ref_mutbl(&self, expr: &Expr) -> Option<Mutbl> {
        let ty = self.table.get(expr.id)?;
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Ref { mutability, .. }) => Some(*mutability),
            _ => None,
        }
    }

    fn check_overlapping_mutable_call_args(&mut self, args: &[Expr]) {
        let mut roots = HashSet::new();
        for arg in args {
            let ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } = &arg.kind
            else {
                continue;
            };
            let Some(root) = Self::place_root_name(operand) else {
                continue;
            };
            if !roots.insert(root.clone()) {
                self.emit(
                    TypeError::MutableReferenceConflict {
                        root,
                        borrower: "an earlier call argument".to_string(),
                    },
                    arg.span,
                );
            }
        }
    }

    /// Whether an assignment place is writable: writable when rooted at a
    /// `mut` binding or reached through a `&mut` reference; immutable when
    /// rooted at a non-`mut` binding or reached through a `&T`; unknown
    /// otherwise (module item, deref of a non-reference, complex base).
    /// Only a definitely-immutable place is rejected.
    fn place_mutability(&self, place: &Expr) -> PlaceMut {
        match &place.kind {
            ExprKind::Path(path) => {
                if path.segments.len() == 1 && path.segments[0].generics.is_empty() {
                    if let Some(mutable) = self.lookup_local_mutability(&path.segments[0].name.name)
                    {
                        return if mutable {
                            PlaceMut::Writable
                        } else {
                            PlaceMut::ImmutableBinding
                        };
                    }
                    match self.resolutions.get(place.id) {
                        Some(Resolution::Def {
                            def,
                            kind: gossamer_resolve::DefKind::Static,
                        }) => match self.static_mutability.get(&def) {
                            Some(true) => PlaceMut::Writable,
                            Some(false) => PlaceMut::ImmutableBinding,
                            None => PlaceMut::Unknown,
                        },
                        Some(Resolution::Def {
                            kind: gossamer_resolve::DefKind::Const,
                            ..
                        }) => PlaceMut::ImmutableBinding,
                        _ => PlaceMut::Unknown,
                    }
                } else {
                    PlaceMut::Unknown
                }
            }
            ExprKind::FieldAccess { receiver, .. } => self.base_place_mutability(receiver),
            ExprKind::Index { base, .. } => self.base_place_mutability(base),
            ExprKind::Unary {
                op: UnaryOp::Deref,
                operand,
            } => match self.expr_ref_mutbl(operand) {
                Some(Mutbl::Mut) => PlaceMut::Writable,
                Some(Mutbl::Not) => PlaceMut::SharedReference,
                None if self.operand_is_concrete_non_reference(operand) => PlaceMut::NotAReference,
                None => PlaceMut::Unknown,
            },
            _ => PlaceMut::Unknown,
        }
    }

    /// Mutability of an auto-dereferenced projection or method receiver.
    /// Every crossed reference layer must be mutable. An outer `&mut` cannot
    /// tunnel through an inner shared reference in a `&mut &T` chain.
    fn auto_deref_place_mutability(&self, base: &Expr) -> PlaceMut {
        let Some(ty) = self.table.get(base.id) else {
            return self.place_mutability(base);
        };
        let mut resolved = self.infer.resolve(self.tcx, ty);
        let mut crossed_mutable_reference = false;
        loop {
            match self.tcx.kind(resolved) {
                Some(TyKind::Ref {
                    mutability: Mutbl::Not,
                    ..
                }) => return PlaceMut::SharedReference,
                Some(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner,
                }) => {
                    crossed_mutable_reference = true;
                    resolved = self.infer.resolve(self.tcx, *inner);
                }
                _ => break,
            }
        }
        if crossed_mutable_reference {
            PlaceMut::Writable
        } else {
            self.place_mutability(base)
        }
    }

    fn base_place_mutability(&self, base: &Expr) -> PlaceMut {
        self.auto_deref_place_mutability(base)
    }

    /// Leftmost path-segment name of a place, naming the root binding in
    /// the immutability diagnostic.
    /// Rejects `&xs[a..b]` / `&mut xs[a..b]`. The index answers a fresh copy
    /// of the range, so borrowing it hands out a reference to a temporary
    /// nothing owns - which the tiers disagree about at run time rather than
    /// diagnosing. A window into part of a sequence has no value shape yet.
    fn reject_range_borrow(&mut self, op: UnaryOp, operand: &Expr) -> bool {
        let ExprKind::Index { base, index } = &operand.kind else {
            return false;
        };
        if !matches!(index.kind, ExprKind::Range { .. }) {
            return false;
        }
        let Some(recorded) = self.table.get(base.id) else {
            return false;
        };
        let base_ty = self.infer.resolve(self.tcx, recorded);
        let base_ty = self.peel_refs(base_ty);
        if !matches!(
            self.tcx.kind(base_ty),
            Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. })
        ) {
            return false;
        }
        let base_text = Self::place_root_name(base).unwrap_or_else(|| "xs".to_string());
        self.emit(
            TypeError::RangeBorrow {
                mutability: if op == UnaryOp::RefMut { "mut " } else { "" },
                base: base_text,
                range: "start..end".to_string(),
            },
            operand.span,
        );
        true
    }

    fn place_root_name(place: &Expr) -> Option<String> {
        match &place.kind {
            ExprKind::Path(path) => path.segments.first().map(|s| s.name.name.clone()),
            ExprKind::FieldAccess { receiver, .. } => Self::place_root_name(receiver),
            ExprKind::Index { base, .. } => Self::place_root_name(base),
            ExprKind::Unary { operand, .. } => Self::place_root_name(operand),
            _ => None,
        }
    }

    /// Names an invalid assignment target in its diagnostic: the literal as
    /// written where the expression is one, and the expression's category
    /// otherwise.
    fn assign_target_display(target: &Expr) -> String {
        match &target.kind {
            ExprKind::Literal(
                gossamer_ast::Literal::Int(text) | gossamer_ast::Literal::Float(text),
            ) => text.clone(),
            ExprKind::Literal(gossamer_ast::Literal::Bool(value)) => value.to_string(),
            ExprKind::Literal(gossamer_ast::Literal::String(text)) => format!("{text:?}"),
            ExprKind::Literal(gossamer_ast::Literal::Char(value)) => format!("'{value}'"),
            ExprKind::Call { .. } => "a call result".to_string(),
            ExprKind::MethodCall { .. } => "a method result".to_string(),
            ExprKind::Binary { .. } | ExprKind::Unary { .. } => "an operator result".to_string(),
            ExprKind::Array(_) | ExprKind::FixedArray(_) => "a sequence literal".to_string(),
            ExprKind::MapLiteral(_) => "a map literal".to_string(),
            ExprKind::SetLiteral(_) => "a set literal".to_string(),
            ExprKind::Struct { .. } => "a struct literal".to_string(),
            ExprKind::Range { .. } => "a range".to_string(),
            _ => "this expression".to_string(),
        }
    }

    fn place_display(place: &Expr) -> String {
        match &place.kind {
            ExprKind::Path(path) => path
                .segments
                .iter()
                .map(|segment| segment.name.name.as_str())
                .collect::<Vec<_>>()
                .join("::"),
            ExprKind::FieldAccess { receiver, field } => match field {
                gossamer_ast::FieldSelector::Named(name) => {
                    format!("{}.{}", Self::place_display(receiver), name.name)
                }
                gossamer_ast::FieldSelector::Index(index) => {
                    format!("{}.{}", Self::place_display(receiver), index)
                }
            },
            ExprKind::Index { base, .. } => format!("{}[...]", Self::place_display(base)),
            _ => "value".to_string(),
        }
    }

    fn correct_map_lookup_assignment_result(&mut self, value: &Expr, mut value_ty: Ty) -> Ty {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &value.kind
        else {
            return value_ty;
        };
        if !matches!(name.name.as_str(), "get_or" | "or_insert") {
            return value_ty;
        }
        if let Some(default) = args.get(1)
            && let Some(default_ty) = self.table.get(default.id)
        {
            value_ty = self.infer.resolve(self.tcx, default_ty);
            self.record(value.id, value_ty);
        }
        let receiver_ty = self
            .table
            .get(receiver.id)
            .unwrap_or_else(|| self.check_expr(receiver));
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::HashMap {
            value: map_value, ..
        }) = self.tcx.kind(resolved)
        {
            let map_value = self.infer.resolve(self.tcx, *map_value);
            if matches!(self.tcx.kind(value_ty), Some(TyKind::Var(_))) {
                value_ty = map_value;
                self.record(value.id, value_ty);
            }
        }
        value_ty
    }

    /// Reports a place that cannot be written: one borrowed elsewhere, one
    /// rooted at an immutable binding, or one reached through a shared `&T`.
    fn check_place_writable(&mut self, place: &Expr) {
        let name = Self::place_root_name(place).unwrap_or_else(|| "value".to_string());
        if let Some(borrower) = self
            .active_mutable_borrower(&name)
            .or_else(|| self.active_shared_borrower(&name))
            .map(str::to_string)
        {
            self.emit(
                TypeError::BorrowedPlaceConflict {
                    root: name.clone(),
                    borrower,
                    action: "mutate",
                },
                place.span,
            );
        }
        match self.place_mutability(place) {
            PlaceMut::ImmutableBinding => {
                self.emit(TypeError::AssignToImmutable { name }, place.span);
            }
            PlaceMut::SharedReference => {
                self.emit(TypeError::AssignThroughSharedReference { name }, place.span);
            }
            PlaceMut::NotAReference => {
                self.emit(TypeError::DerefWriteToNonReference { name }, place.span);
            }
            PlaceMut::Writable | PlaceMut::Unknown => {}
        }
    }

    /// True when `operand`'s type is known and is not a reference, so a
    /// `*operand` write names no place at all.
    fn operand_is_concrete_non_reference(&self, operand: &Expr) -> bool {
        let Some(ty) = self.table.get(operand.id) else {
            return false;
        };
        let resolved = self.infer.resolve(self.tcx, ty);
        matches!(
            self.tcx.kind(resolved),
            Some(
                TyKind::Int(_)
                    | TyKind::Float(_)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::String
                    | TyKind::Unit
            )
        )
    }

    /// Types one destructuring-assignment target. A tuple recurses
    /// element-wise, `_` discards its element and answers a fresh variable,
    /// and every other element must name a writable place.
    fn check_assign_target(&mut self, target: &Expr) -> Ty {
        if let ExprKind::Tuple(elems) = &target.kind {
            let tys: Vec<Ty> = elems
                .iter()
                .map(|elem| self.check_assign_target(elem))
                .collect();
            let ty = self.tcx.intern(TyKind::Tuple(tys));
            self.record(target.id, ty);
            return ty;
        }
        if target.is_wildcard() {
            let ty = self.fresh();
            self.record(target.id, ty);
            return ty;
        }
        if !target.is_place() {
            self.emit(
                TypeError::InvalidAssignTarget {
                    target: Self::assign_target_display(target),
                },
                target.span,
            );
            return self.tcx.error_ty();
        }
        let previous_suppression = self.suppressed.borrow_read_conflict;
        self.suppressed.borrow_read_conflict = true;
        let ty = self.check_expr(target);
        self.suppressed.borrow_read_conflict = previous_suppression;
        self.check_place_writable(target);
        ty
    }

    /// Type-checks `a, b.c, xs[i] = rhs`, whose targets are the elements of
    /// the left-hand list. The tuple of element types is the expectation the
    /// right-hand side is checked against, so a literal there is shaped by
    /// the destination exactly as in a scalar assignment. A compound operator
    /// pairs with the same element types, since each place is combined with
    /// its own element.
    fn check_destructuring_assign(&mut self, place: &Expr, value: &Expr) -> Ty {
        let place_ty = self.check_assign_target(place);
        let value_ty = self.check_expr_expecting(value, Expectation::HasType(place_ty));
        self.unify(place_ty, value_ty, value.span);
        self.tcx.unit()
    }

    fn check_assign(&mut self, place: &Expr, value: &Expr, op: gossamer_ast::AssignOp) -> Ty {
        if matches!(place.kind, ExprKind::Tuple(_)) {
            return self.check_destructuring_assign(place, value);
        }
        if !place.is_place() {
            self.emit(
                TypeError::InvalidAssignTarget {
                    target: Self::assign_target_display(place),
                },
                place.span,
            );
            let _ = self.check_expr(value);
            return self.tcx.unit();
        }
        let previous_suppression = self.suppressed.borrow_read_conflict;
        self.suppressed.borrow_read_conflict = true;
        let place_ty = self.check_expr(place);
        self.suppressed.borrow_read_conflict = previous_suppression;
        self.check_place_writable(place);
        let place_resolved = self.infer.resolve(self.tcx, place_ty);
        let place_is_reference = matches!(self.tcx.kind(place_resolved), Some(TyKind::Ref { .. }));
        // A reference binding must be rebound with another reference. Do not
        // pass its referent through as a literal-shaping expectation: doing so
        // would allow `let mut r = &value; r = value` to discard the `&`.
        // Other assignment destinations retain the expected-type flow that
        // shapes `[2, 3]` as a Vec for a `Vec<i64>` slot.
        let mut value_ty = if place_is_reference {
            self.check_expr(value)
        } else {
            self.check_expr_expecting(value, Expectation::HasType(place_ty))
        };
        // A map lookup-like method returns V, independently of the assignment
        // destination. When V is still inferred, feeding the destination
        // HashMap<K, V> expectation into `h = h.or_insert(k, default)` could
        // recursively bind V to the map itself and silently retype `h`.
        // Re-read the authoritative value type from the receiver after its
        // key/default arguments have grounded the map generics.
        value_ty = self.correct_map_lookup_assignment_result(value, value_ty);
        if place_is_reference {
            if !self.rebind_named_borrow(place, value) {
                self.emit(
                    TypeError::ReferenceEscapeUnsupported {
                        context: "be rebound through an alias or from a temporary".to_string(),
                    },
                    value.span,
                );
            }
        }
        if place_is_reference {
            let value_resolved = self.infer.resolve(self.tcx, value_ty);
            let value_is_reference_or_unresolved = matches!(
                self.tcx.kind(value_resolved),
                Some(TyKind::Ref { .. } | TyKind::Var(_) | TyKind::Error | TyKind::Never)
            );
            if !value_is_reference_or_unresolved {
                self.deferred_type_mismatches
                    .push((place_resolved, value_resolved, value.span));
                return self.tcx.unit();
            }
        }
        // `s += &t` / `s += &str`: String append accepts a borrowed operand
        // on the right, mirroring the `+` concatenation operator. Only the
        // compound `+=` form relaxes; plain `=` still requires an owned String.
        if matches!(op, gossamer_ast::AssignOp::AddAssign) {
            let pr = self.infer.resolve(self.tcx, place_ty);
            if matches!(self.tcx.kind(pr), Some(TyKind::String)) {
                let mut vr = self.infer.resolve(self.tcx, value_ty);
                while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(vr) {
                    vr = self.infer.resolve(self.tcx, *inner);
                }
                if matches!(self.tcx.kind(vr), Some(TyKind::String)) {
                    return self.tcx.unit();
                }
            }
        }
        // Compound assignment on a user struct / enum desugars through the
        // binary operator, so it routes to the same operator impl
        // (`+=` -> `add`). The impl's return re-binds the place, so it
        // must be the place's own type. A place with no impl is rejected
        // here rather than faulting at runtime; the value operand keeps
        // the impl's declared parameter shape (a scalar for `v *= 2.0`),
        // so the place/value unification below is skipped on this path.
        if let Some(method) = assign_op_method(op) {
            let pr = self.infer.resolve(self.tcx, place_ty);
            if self.adt_name_of(pr).is_some() {
                self.record(place.id, pr);
                let vr = self.infer.resolve(self.tcx, value_ty);
                if self.adt_name_of(vr).is_some() {
                    self.record(value.id, vr);
                }
                if let Some(ret) = self.adt_op_method_ret(pr, method, 1) {
                    self.unify(place_ty, ret, place.span);
                } else {
                    let ty = self.render_public_ty(pr);
                    self.emit(
                        TypeError::UnresolvedOpImpl {
                            op: op.as_str().to_string(),
                            trait_name: op_trait_name(method).to_string(),
                            method: method.to_string(),
                            ty,
                        },
                        place.span,
                    );
                }
                return self.tcx.unit();
            }
        }
        self.unify(place_ty, value_ty, value.span);
        self.tcx.unit()
    }

    /// Validates an `as` cast against the whitelist of permitted
    /// conversions: numeric ↔ numeric, `bool`/`char` → integer,
    /// `u8` → `char`, and same-type no-ops. Matches Rust's RFC 401.
    /// Fails soft when either side is still an inference variable -
    /// the unification pass will resolve it, and a later run can
    /// recheck; inventing an error on a not-yet-known type would
    /// cascade into noise.
    fn check_cast(&mut self, from: Ty, to: Ty, span: Span) {
        let resolved_from = self.infer.resolve(self.tcx, from);
        let resolved_to = self.infer.resolve(self.tcx, to);
        let Some(from_kind) = self.tcx.kind(resolved_from).cloned() else {
            return;
        };
        let Some(to_kind) = self.tcx.kind(resolved_to).cloned() else {
            return;
        };
        if matches!(from_kind, TyKind::Var(_) | TyKind::Error)
            || matches!(to_kind, TyKind::Var(_) | TyKind::Error)
        {
            return;
        }
        if cast_allowed(&from_kind, &to_kind) {
            return;
        }
        let from = self.render_public_ty(resolved_from);
        let to = self.render_public_ty(resolved_to);
        self.diagnostics.push(TypeDiagnostic::new(
            TypeError::InvalidCast { from, to },
            span,
        ));
    }

    fn check_if(
        &mut self,
        condition: &Expr,
        then_branch: &Expr,
        else_branch: Option<&Expr>,
        expected: Expectation,
    ) -> Ty {
        let cond_ty = self.check_expr(condition);
        let bool_ty = self.tcx.bool_ty();
        self.unify(bool_ty, cond_ty, condition.span);
        let then_ty = self.check_expr_expecting(then_branch, expected);
        if let Some(else_branch) = else_branch {
            let else_ty = self.check_expr_expecting(else_branch, expected);
            let joined = self.join_branch_tys(then_ty, else_ty, else_branch.span);
            // When the branches joined to a Vec/slice, re-record each
            // array-literal branch to that shape so an unannotated
            // `let v = if c { [1, 2] } else { [3, 4, 5] }` lowers both
            // arms as a heap Vec, matching the joined result slot.
            self.adjust_literal_to_join(then_branch, joined);
            self.adjust_literal_to_join(else_branch, joined);
            joined
        } else {
            self.tcx.unit()
        }
    }

    /// Joins branch types without silently converting arrays or slices to Vec.
    /// As in Rust, both value-producing branches must have one compatible type.
    fn join_branch_tys(&mut self, a: Ty, b: Ty, span: Span) -> Ty {
        self.unify(a, b, span);
        a
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], expected: Expectation) -> Ty {
        let scrut_expectation = self.match_scrutinee_expectation(arms);
        let scrut_ty = self.check_expr_expecting(scrutinee, scrut_expectation);
        self.reject_constructor_scrutinee_mismatch(scrut_ty, arms);
        self.reject_json_value_variant_patterns(arms);
        let mut result_ty = self.fresh();
        for arm in arms {
            self.push_scope();
            let pat_ty = self.type_of_pattern(&arm.pattern);
            // String literal patterns compare by value through any leading `&`
            // on the scrutinee, so `match ref_str { "foo" => ... }` is valid.
            let effective_scrut_ty = if matches!(
                &arm.pattern.kind,
                PatternKind::Literal(Literal::String(_) | Literal::RawString { .. })
            ) {
                let resolved = self.infer.resolve(self.tcx, scrut_ty);
                match self.tcx.kind(resolved) {
                    Some(TyKind::Ref { inner, .. }) => *inner,
                    _ => scrut_ty,
                }
            } else {
                scrut_ty
            };
            // Unify BEFORE binding: the pattern's synthesized type carries
            // fresh payload vars, and binding resolves each binder through
            // the inference table - unifying first lets a variant pattern's
            // binders see the scrutinee's concrete payload types instead of
            // unresolved vars.
            self.unify(effective_scrut_ty, pat_ty, arm.pattern.span);
            self.bind_pattern(&arm.pattern, pat_ty);
            let resolved_scrut_ty = self.infer.resolve(self.tcx, scrut_ty);
            if matches!(self.tcx.kind(resolved_scrut_ty), Some(TyKind::Ref { .. }))
                && let Some(scrutinee_binding) = Self::place_root_name(scrutinee)
            {
                let origin = self
                    .reference_origin(&scrutinee_binding)
                    .unwrap_or(&scrutinee_binding)
                    .to_string();
                self.register_pattern_reference_origins(&arm.pattern, &origin);
            }
            if let Some(guard) = &arm.guard {
                let guard_ty = self.check_expr(guard);
                let bool_ty = self.tcx.bool_ty();
                self.unify(bool_ty, guard_ty, guard.span);
            }
            let body_ty = self.check_expr_expecting(&arm.body, expected);
            result_ty = self.join_branch_tys(result_ty, body_ty, arm.body.span);
            self.pop_scope();
        }
        // Second pass: if the arms joined to a Vec/slice, re-record every
        // array-literal arm body to that shape so an unannotated
        // `let v = match n { 0 => ["a"], _ => ["b", "c"] }` lowers each
        // arm as a heap Vec, matching the joined result slot.
        for arm in arms {
            self.adjust_literal_to_join(&arm.body, result_ty);
        }
        result_ty
    }

    fn match_scrutinee_expectation(&mut self, arms: &[MatchArm]) -> Expectation {
        let mut wants_result = false;
        let mut wants_option = false;
        for arm in arms {
            match Self::builtin_enum_pattern_family(&arm.pattern) {
                Some(BuiltinPatternFamily::Result) => wants_result = true,
                Some(BuiltinPatternFamily::Option) => wants_option = true,
                None => {}
            }
        }
        match (wants_result, wants_option) {
            (true, false) => {
                let ok_ty = self.fresh();
                let err_ty = self.fresh();
                Expectation::HasType(self.result_adt_ty(ok_ty, err_ty))
            }
            (false, true) => {
                let payload = self.fresh();
                Expectation::HasType(self.option_adt_ty(payload))
            }
            _ => Expectation::None,
        }
    }

    fn builtin_enum_pattern_family(pattern: &Pattern) -> Option<BuiltinPatternFamily> {
        match &pattern.kind {
            PatternKind::TupleStruct { path, .. } | PatternKind::Path(path) => {
                match Self::bare_result_option_ctor(path)? {
                    "Ok" | "Err" => Some(BuiltinPatternFamily::Result),
                    "Some" | "None" => Some(BuiltinPatternFamily::Option),
                    _ => None,
                }
            }
            PatternKind::Or(alts) => {
                let mut family = None;
                for alt in alts {
                    let Some(next) = Self::builtin_enum_pattern_family(alt) else {
                        continue;
                    };
                    if let Some(prev) = family
                        && prev != next
                    {
                        return None;
                    }
                    family = Some(next);
                }
                family
            }
            PatternKind::Ref { inner, .. } => Self::builtin_enum_pattern_family(inner),
            _ => None,
        }
    }

    /// Rejects `Ok` / `Err` / `Some` / `None` arms whose scrutinee's
    /// resolved head is not the matching `Result` / `Option`. The
    /// `unify` mismatch is suppressed for these arms because the
    /// synthesized pattern type carries unresolved payload vars, so the
    /// hole is closed with a direct GT0001 here. Skips a scrutinee whose
    /// head is still an inference variable so a not-yet-resolved shape is
    /// never flagged.
    fn reject_constructor_scrutinee_mismatch(&mut self, scrut_ty: Ty, arms: &[MatchArm]) {
        let resolved = self.infer.resolve(self.tcx, scrut_ty);
        let resolved = match self.tcx.kind(resolved) {
            Some(TyKind::Ref { inner, .. }) => self.infer.resolve(self.tcx, *inner),
            _ => resolved,
        };
        // Judge the container by the resolved head only: an unresolved
        // scrutinee (`Var`) or an error type carries no decision; a
        // partially-inferred `Result<_, Var>` still has a known `Result`
        // head, so a `Some` arm against it is correctly rejected.
        match self.tcx.kind(resolved) {
            Some(TyKind::Var(_) | TyKind::Error) | None => return,
            _ => {}
        }
        for arm in arms {
            let ctor = match &arm.pattern.kind {
                PatternKind::TupleStruct { path, .. } | PatternKind::Path(path) => {
                    Self::bare_result_option_ctor(path)
                }
                _ => None,
            };
            let Some(ctor) = ctor else { continue };
            // `Ok` / `Err` need the `Result` sentinel (`u32::MAX`);
            // `Some` / `None` need the `Option` sentinel (`u32::MAX - 1`).
            let want_def = match ctor {
                "Ok" | "Err" => u32::MAX,
                _ => u32::MAX - 1,
            };
            let matches_container = matches!(
                self.tcx.kind(resolved),
                Some(TyKind::Adt { def, .. }) if def.local == want_def
            );
            if matches_container {
                continue;
            }
            let expected = if want_def == u32::MAX {
                let fresh_ok = self.fresh();
                let fresh_err = self.fresh();
                self.result_adt_ty(fresh_ok, fresh_err)
            } else {
                let fresh = self.fresh();
                self.option_adt_ty(fresh)
            };
            let expected = self.render_public_ty(expected);
            let found = self.render_public_ty(resolved);
            self.emit(
                TypeError::TypeMismatch { expected, found },
                arm.pattern.span,
            );
        }
    }

    /// Rejects `json::Value::Object(..)` / `::Int(..)` / `::Null` (etc.)
    /// constructor patterns in `match` / `if let` / `while let` arms. The
    /// `json::Value` handle carries no matchable discriminant across the
    /// tiers, so such a pattern silently falls through on the VM and faults
    /// on the compiled tiers; the dynamic accessor API (`json::as_*` /
    /// `json::get` / `json::keys`) is the supported way to read a document.
    /// The pattern is judged structurally (not on the scrutinee type) so a
    /// scrutinee left unresolved by inference is still caught - a
    /// `json::Value::X` path is never a valid matchable pattern.
    fn reject_json_value_variant_patterns(&mut self, arms: &[MatchArm]) {
        for arm in arms {
            self.reject_json_value_variant_pattern(&arm.pattern);
        }
    }

    /// Emits GT0027 for a single `json::Value::X` constructor pattern,
    /// recursing through or-patterns so every alternative is flagged.
    fn reject_json_value_variant_pattern(&mut self, pattern: &Pattern) {
        let path = match &pattern.kind {
            PatternKind::TupleStruct { path, .. }
            | PatternKind::Path(path)
            | PatternKind::Struct { path, .. } => Some(path),
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.reject_json_value_variant_pattern(alt);
                }
                None
            }
            _ => None,
        };
        if let Some(path) = path {
            if let Some(variant) = json_value_variant_of(path) {
                self.emit(
                    TypeError::JsonValuePatternUnsupported {
                        variant: variant.to_string(),
                    },
                    pattern.span,
                );
            }
        }
    }

    /// Type-checks a `select { … }` expression. Each arm is checked in its own
    /// scope: a recv arm binds its pattern to the channel's element type, a
    /// send arm unifies the sent value against the channel's element type, and
    /// every arm body unifies into the shared result type.
    fn check_select(&mut self, arms: &[gossamer_ast::SelectArm]) -> Ty {
        use gossamer_ast::SelectOp;
        let result_ty = self.fresh();
        for arm in arms {
            self.push_scope();
            match &arm.op {
                SelectOp::Recv { pattern, channel } => {
                    let chan_ty = self.check_expr(channel);
                    let resolved = self.infer.resolve(self.tcx, chan_ty);
                    let elem = match self.tcx.kind_of(resolved).clone() {
                        TyKind::Receiver(inner) | TyKind::Sender(inner) => inner,
                        _ => self.fresh(),
                    };
                    let pat_ty = self.type_of_pattern(pattern);
                    self.unify(pat_ty, elem, pattern.span);
                    self.bind_pattern(pattern, elem);
                }
                SelectOp::Send { channel, value } => {
                    let chan_ty = self.check_expr(channel);
                    let resolved = self.infer.resolve(self.tcx, chan_ty);
                    let elem = match self.tcx.kind_of(resolved).clone() {
                        TyKind::Sender(inner) | TyKind::Receiver(inner) => inner,
                        _ => self.fresh(),
                    };
                    let val_ty = self.check_expr(value);
                    self.unify(elem, val_ty, value.span);
                }
                SelectOp::Default => {}
            }
            let body_ty = self.check_expr(&arm.body);
            self.unify(result_ty, body_ty, arm.body.span);
            self.pop_scope();
        }
        result_ty
    }

    fn check_for(&mut self, pattern: &Pattern, iter: &Expr, body: &Expr) -> Ty {
        let iter_ty = self.check_expr(iter);
        self.reject_unbounded_generic_iteration(iter_ty, iter.span);
        self.reject_wrapper_iteration(iter_ty, iter.span);
        self.mark_consumed_iterator_expr("into_iter", iter, iter_ty);
        self.push_scope();
        // Derive the pattern's type from the iterator: arrays/slices
        // yield their element type, ranges over integers yield the
        // integer type. When the iterator is itself a method call
        // (`xs.iter()`, `xs.into_iter()`) whose return type is an
        // unresolved inference variable, fall back to looking at
        // the method's receiver - `.iter()` and friends always
        // produce the receiver's element type, regardless of which
        // wrapper they technically return.
        let derived = {
            let starting = self.infer.resolve(self.tcx, iter_ty);
            let is_var = matches!(self.tcx.kind(starting), Some(TyKind::Var(_)));
            let starting = if is_var {
                // Only `.iter()` / `.into_iter()` produce the receiver's
                // element type. Other methods (`m.get_or(k, d)`,
                // `m.values()`) return a different shape, so falling back to
                // the receiver there would derive the wrong element type.
                if let ExprKind::MethodCall { receiver, name, .. } = &iter.kind
                    && matches!(name.name.as_str(), "iter" | "into_iter")
                {
                    let recv_ty = self.check_expr(receiver);
                    self.infer.resolve(self.tcx, recv_ty)
                } else {
                    starting
                }
            } else {
                starting
            };
            let mut cur = starting;
            loop {
                match self.tcx.kind_of(cur).clone() {
                    TyKind::Ref { inner, mutability } => {
                        // Resolve the referent first: a `&mut` to a container
                        // whose element type is still being inferred would
                        // otherwise read as an opaque variable and leave the
                        // loop binding untyped.
                        let inner = self.infer.resolve(self.tcx, inner);
                        match self.tcx.kind_of(inner).clone() {
                            TyKind::Array { elem, .. }
                            | TyKind::Slice(elem)
                            | TyKind::Vec(elem) => {
                                // A shared borrow yields the same element binding
                                // the owned sequence does; only `&mut` keeps the
                                // reference, which is what carries a write through
                                // to the source.
                                if mutability == crate::Mutbl::Not {
                                    break Some(elem);
                                }
                                break Some(self.tcx.intern(TyKind::Ref {
                                    mutability,
                                    inner: elem,
                                }));
                            }
                            TyKind::Tuple(elems) => {
                                let Some(elem) = elems.first().copied() else {
                                    break None;
                                };
                                break Some(self.tcx.intern(TyKind::Ref {
                                    mutability,
                                    inner: elem,
                                }));
                            }
                            _ => cur = inner,
                        }
                    }
                    TyKind::Array { elem, .. }
                    | TyKind::Slice(elem)
                    | TyKind::Vec(elem)
                    | TyKind::Iterator(elem)
                    | TyKind::Range(elem) => {
                        break Some(elem);
                    }
                    TyKind::String => break Some(self.tcx.char_ty()),
                    // A map walked directly yields the same `(key, value)`
                    // pair its `iter()` cursor does, so a bare `for (k, v) in
                    // m` binds the map's own key and value types.
                    TyKind::HashMap { key, value, .. } => {
                        let key = self.infer.resolve(self.tcx, key);
                        let value = self.infer.resolve(self.tcx, value);
                        break Some(self.tcx.intern(TyKind::Tuple(vec![key, value])));
                    }
                    TyKind::Tuple(elems) => {
                        let Some(elem) = elems.first().copied() else {
                            break None;
                        };
                        break Some(elem);
                    }
                    _ => break None,
                }
            }
        };
        let pat_ty = match derived {
            Some(t) => {
                let p = self.type_of_pattern(pattern);
                let pattern_target = match (&pattern.kind, self.tcx.kind_of(t).clone()) {
                    (PatternKind::Tuple(_), TyKind::Ref { inner, .. }) => {
                        self.infer.resolve(self.tcx, inner)
                    }
                    _ => t,
                };
                self.unify(p, pattern_target, pattern.span);
                t
            }
            None => self.type_of_pattern(pattern),
        };
        self.bind_pattern(pattern, pat_ty);
        self.check_expr(body);
        self.report_discarded_result(body, None);
        self.pop_scope();
        self.tcx.unit()
    }

    /// Reports GT0064 when `expr` discards a value of a `#[must_use]` type
    /// or the result of a call to a `#[must_use]` function. Returns whether
    /// a report was made.
    fn report_discarded_must_use(&mut self, expr: &Expr, ty: Option<Ty>) -> bool {
        if let Some(ty) = ty {
            let resolved = self.infer.resolve(self.tcx, ty);
            if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved)
                && let Some(name) = self.must_use_types.get(def).cloned()
            {
                self.emit(
                    TypeError::DiscardedMustUse {
                        what: "value",
                        name,
                    },
                    expr.span,
                );
                return true;
            }
        }
        let ExprKind::Call { callee, .. } = &expr.kind else {
            return false;
        };
        let Some(Resolution::Def { def, .. }) = self.resolutions.get(callee.id) else {
            return false;
        };
        let Some(name) = self.must_use_fns.get(&def).cloned() else {
            return false;
        };
        self.emit(
            TypeError::DiscardedMustUse {
                what: "return value",
                name,
            },
            expr.span,
        );
        true
    }

    /// Name of a user generic type with no `fmt`, when `ty` is one.
    ///
    /// A generic declaration only gets a synthesized `fmt` from an explicit
    /// `#[derive(Debug)]`, because whether its fields render depends on the
    /// arguments each instantiation supplies. Formatting one without that
    /// would run on the interpreter and fail the native build, so it is
    /// rejected here, where both tiers see it.
    fn generic_without_fmt(&mut self, ty: Ty) -> Option<String> {
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        if substs.is_empty() {
            return None;
        }
        let name = self.tcx.def_name(*def)?.to_string();
        // Built-in generic types render through the runtime, not a `fmt`.
        if !self.user_type_defs.values().any(|d| d == def) {
            return None;
        }
        let has_fmt = self
            .method_homes
            .contains_key(&(name.clone(), "fmt".to_string()));
        (!has_fmt).then_some(name)
    }

    /// True when `ty` resolves to a `Result<T, E>`.
    /// Reports a `for` whose subject is a `Result` or an `Option`.
    ///
    /// Neither is a sequence. Iterating one binds nothing and runs the body
    /// zero times, and because the binding is then unconstrained, whatever
    /// the body reads off it type-checks - so the loop compiles, runs, and
    /// silently does nothing. The value inside has to be taken first.
    fn reject_wrapper_iteration(&mut self, ty: Ty, span: Span) {
        let resolved = self.infer.resolve(self.tcx, ty);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def) else {
            return;
        };
        let taken = match name {
            "Result" => "`?`, a `match`, or `unwrap_or(..)`",
            "Option" => "`if let Some(v) = ..`, `?`, or `unwrap_or(..)`",
            _ => return,
        };
        let name = name.to_string();
        self.emit(TypeError::IterableWrapper { name, taken }, span);
    }

    fn is_result_ty(&mut self, ty: Ty) -> bool {
        let resolved = self.infer.resolve(self.tcx, ty);
        matches!(self.tcx.kind(resolved), Some(TyKind::Adt { def, .. })
            if self.tcx.def_name(*def) == Some("Result"))
    }

    /// Reports GT0007 for `expr` sitting in a position whose value is
    /// discarded.
    ///
    /// A construct that passes its operand's value through - a block, an
    /// `if`, a `match` - discards that operand exactly when its own value is
    /// discarded, so the report lands on the expression that produced the
    /// `Result` rather than on the construct wrapping it. An else-less `if`
    /// is typed `()` while its `then` branch keeps the branch's own type, so
    /// recursion is what reaches it at all.
    ///
    /// `ty` is the type already computed for `expr` by the caller; the
    /// recursive steps read the side table instead.
    fn report_discarded_result(&mut self, expr: &Expr, ty: Option<Ty>) {
        if self.unused_result_allowed {
            return;
        }
        let ty = ty.or_else(|| self.table.get(expr.id));
        if let Some(ty) = ty
            && self.is_result_ty(ty)
        {
            self.emit(TypeError::DiscardedResult, expr.span);
            return;
        }
        if self.report_discarded_must_use(expr, ty) {
            return;
        }
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Unsafe(block) => {
                if let Some(tail) = &block.tail {
                    self.report_discarded_result(tail, None);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.report_discarded_result(then_branch, None);
                if let Some(else_branch) = else_branch {
                    self.report_discarded_result(else_branch, None);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.report_discarded_result(&arm.body, None);
                }
            }
            _ => {}
        }
    }

    fn check_block(&mut self, block: &Block, expected: Expectation) -> Ty {
        self.push_scope();
        let mut diverged = false;
        for stmt in &block.stmts {
            self.check_stmt(stmt);
            if !diverged && stmt_diverges(stmt) {
                diverged = true;
            }
        }
        let ty = if let Some(tail) = &block.tail {
            self.check_expr_expecting(tail, expected)
        } else if diverged {
            // A block whose statements unconditionally diverge
            // (`return`, `break`, `continue`, `panic!`) and whose
            // tail is missing has type `!`. Without this, a match
            // arm body like `{ eprintln!(...); return Err(msg) }`
            // would be typed as `unit` and force the match's
            // result type away from the other arms' real type.
            self.tcx.never()
        } else {
            self.tcx.unit()
        };
        self.pop_scope();
        ty
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, ty, init } => {
                self.check_let_stmt(pattern, ty.as_ref(), init.as_deref());
            }
            StmtKind::Expr { expr, .. } => {
                let expr_ty = self.check_expr(expr);
                // SPEC §9: a `Result<T, E>` value used as a statement (value
                // discarded) is a compile error. The explicit discard form
                // `let _ = expr` goes through `StmtKind::Let` and is not
                // subject to this check.
                self.report_discarded_result(expr, Some(expr_ty));
            }
            StmtKind::Item(item) => {
                // Block-local items are not part of the source file's
                // top-level signature prepass. Register their definitions
                // before checking the body so nested structs expose fields
                // and nested functions/types use the same DefId-keyed
                // metadata as module-level items.
                self.assoc.extend(std::slice::from_ref(item));
                self.collect_signatures(std::slice::from_ref(item));
                self.check_item(item);
            }
            StmtKind::Defer(inner) => {
                self.check_expr(inner);
            }
        }
    }

    fn check_let_stmt(&mut self, pattern: &Pattern, ty: Option<&AstType>, init: Option<&Expr>) {
        if let Some(problem) = plain_let_pattern_problem(pattern) {
            let error = match problem {
                PlainLetPatternProblem::Literal => TypeError::CannotAssignToLiteral,
                PlainLetPatternProblem::MayNotMatch => TypeError::LetPatternMayNotMatch,
            };
            self.emit(error, pattern.span);
        }
        let forced = self.write_arg_bindings.get(&pattern.id).copied();
        let binding_ty = if let Some(authored) = ty {
            self.type_from_ast(authored)
        } else {
            forced.unwrap_or_else(|| self.fresh())
        };
        if let Some(init) = init {
            let expected = if ty.is_some() || forced.is_some() {
                Expectation::HasType(binding_ty)
            } else {
                Expectation::None
            };
            let init_ty = self.check_expr_expecting(init, expected);
            if let Some(error) = self.option_value_mismatch(pattern, binding_ty, init, init_ty) {
                self.emit(error, init.span);
            } else {
                self.unify(binding_ty, init_ty, init.span);
            }
            // A target list says how many elements it expects, so its arity is
            // checked against a tuple value: `let a, b, c = pair` would
            // otherwise bind past the end of the pair. A sequence value keeps
            // its own element-wise binding, which names no arity to check.
            if matches!(pattern.kind, PatternKind::Tuple(_)) {
                let resolved = self.infer.resolve(self.tcx, binding_ty);
                if matches!(self.tcx.kind(resolved), Some(TyKind::Tuple(_))) {
                    let pattern_ty = self.type_of_pattern(pattern);
                    self.unify(pattern_ty, resolved, pattern.span);
                }
            }
            self.check_local_reference_storage(pattern, binding_ty, init);
            self.check_reference_pattern(pattern, init_ty);
        }
        if ty.is_none() && forced.is_none() {
            self.infer.default_numeric_vars_in_ty(self.tcx, binding_ty);
        }
        self.bind_pattern(pattern, binding_ty);
        if let Some(init) = init {
            self.register_named_mutable_borrow(pattern, init);
        }
    }

    /// The GT0001 diagnostic for binding an `Option<T>` where the
    /// annotation asks for `T`, spelling both fixes with the initializer's
    /// own text. `None` when the shapes do not match that case or the
    /// initializer has no short spelling, leaving ordinary unification to
    /// report the mismatch.
    fn option_value_mismatch(
        &mut self,
        pattern: &Pattern,
        binding_ty: Ty,
        init: &Expr,
        init_ty: Ty,
    ) -> Option<TypeError> {
        let want = self.infer.resolve(self.tcx, binding_ty);
        let found = self.infer.resolve(self.tcx, init_ty);
        if matches!(
            self.tcx.kind(want),
            Some(TyKind::Var(_) | TyKind::Error) | None
        ) {
            return None;
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(found) else {
            return None;
        };
        if def.local != OPTION_DEF_LOCAL {
            return None;
        }
        let payload = substs.types().first().copied()?;
        let payload = self.infer.resolve(self.tcx, payload);
        let expected = self.render_public_ty(want);
        if expected != self.render_public_ty(payload) {
            return None;
        }
        let actual = expr_display(init)?;
        let binding = match &pattern.kind {
            PatternKind::Ident { name, .. } => name.name.clone(),
            _ => "value".to_string(),
        };
        let default = default_value_spelling(self.tcx.kind(payload));
        Some(TypeError::OptionValueMismatch {
            expected,
            found: self.render_public_ty(found),
            actual,
            binding,
            default,
        })
    }

    fn check_local_reference_storage(&mut self, pattern: &Pattern, binding_ty: Ty, init: &Expr) {
        if matches!(pattern.kind, PatternKind::Ref { .. }) {
            return;
        }
        let resolved = self.infer.resolve(self.tcx, binding_ty);
        if matches!(self.tcx.kind(resolved), Some(TyKind::Ref { .. })) {
            let stable = matches!(
                &init.kind,
                ExprKind::Unary {
                    op: UnaryOp::RefShared | UnaryOp::RefMut,
                    operand,
                } if is_stable_borrow_place(operand)
            ) || self.is_stable_shared_reference_alias(init);
            if !stable {
                self.emit(
                    TypeError::ReferenceEscapeUnsupported {
                        context: "be copied into a local or borrow a temporary".to_string(),
                    },
                    init.span,
                );
            }
        } else {
            // A function value whose signature accepts a reference does not
            // store a reference. Its reference exists only for the duration
            // of a future call. Captured references are rejected separately
            // by `check_closure`.
            if !matches!(
                self.tcx.kind(resolved),
                Some(TyKind::FnPtr(_) | TyKind::FnTrait(_))
            ) {
                self.deferred_reference_storage.push((
                    binding_ty,
                    pattern.span,
                    "be nested inside an owned local value",
                ));
            }
        }
    }

    fn check_reference_pattern(&mut self, pattern: &Pattern, init_ty: Ty) {
        let PatternKind::Ref { mutability, .. } = &pattern.kind else {
            return;
        };
        self.infer.default_numeric_vars_in_ty(self.tcx, init_ty);
        let resolved = self.infer.resolve(self.tcx, init_ty);
        let expected = if mutability.is_mutable() {
            Mutbl::Mut
        } else {
            Mutbl::Not
        };
        let valid = match self.tcx.kind(resolved) {
            Some(TyKind::Ref {
                mutability: actual, ..
            }) => *actual == expected,
            Some(TyKind::Var(_) | TyKind::Error) => true,
            _ => false,
        };
        if !valid {
            self.emit(
                TypeError::ReferencePatternRequiresReference {
                    pattern: if mutability.is_mutable() { "&mut" } else { "&" },
                },
                pattern.span,
            );
            return;
        }
        let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let inner = *inner;
        self.check_reference_pattern_referent(pattern, inner);
    }

    /// A reference pattern copies its referent out of the reference, which
    /// only a scalar representation supports.
    fn check_reference_pattern_referent(&mut self, pattern: &Pattern, referent: Ty) {
        let referent = self.infer.resolve(self.tcx, referent);
        if !matches!(
            self.tcx.kind_of(referent),
            TyKind::Bool
                | TyKind::Char
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Unit
                | TyKind::Never
                | TyKind::Var(_)
                | TyKind::Error
        ) {
            let ty = self.render_public_ty(referent);
            self.emit(
                TypeError::ReferencePatternAggregateUnsupported { ty },
                pattern.span,
            );
        }
    }

    /// A parameter's reference is declared in its type. A `&` pattern over a
    /// declared type that is not a matching reference has no referent to
    /// bind, so name the `name: &Ty` spelling the parameter meant.
    fn check_param_reference_pattern(&mut self, pattern: &Pattern, param_ty: Ty) {
        let PatternKind::Ref { mutability, inner } = &pattern.kind else {
            return;
        };
        let expected = if mutability.is_mutable() {
            Mutbl::Mut
        } else {
            Mutbl::Not
        };
        let resolved = self.infer.resolve(self.tcx, param_ty);
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Ref {
                mutability: actual,
                inner: referent,
            }) if actual == expected => {
                self.check_reference_pattern_referent(pattern, referent);
            }
            Some(TyKind::Var(_) | TyKind::Error) => {}
            _ => {
                let ty = self.render_public_ty(resolved);
                let (spelling, reference_ty) = if mutability.is_mutable() {
                    ("&mut", format!("&mut {ty}"))
                } else {
                    ("&", format!("&{ty}"))
                };
                let mut names = Vec::new();
                pattern_binding_names(inner, &mut names);
                let binding = names
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "value".to_string());
                self.emit(
                    TypeError::ReferenceParameterPatternPosition {
                        pattern: spelling,
                        binding,
                        reference_ty,
                        ty,
                    },
                    pattern.span,
                );
            }
        }
    }

    /// Rejects a goroutine body that reads an outer binding whose type has
    /// no representation crossing the boundary.
    ///
    /// The spawning goroutine keeps its own handle on the value, so both
    /// sides would reach one piece of nested growable storage with nothing
    /// serialising them - the shape whose compiled ABI has no ownership
    /// descriptor, and which faults rather than racing. Checked here so the
    /// answer is the same on every tier, rather than at run time on one.
    /// Reports a `sync::Shared` payload the guarded slot cannot carry.
    ///
    /// Run after inference so a numeric literal has landed on its type:
    /// an integer is read back identically by every tier, and nothing else
    /// is, so nothing else may be guarded.
    fn check_deferred_shared_payloads(&mut self) {
        let pending = std::mem::take(&mut self.deferred_shared_payloads);
        for (ty, span) in pending {
            let elem = self.infer.resolve(self.tcx, ty);
            if matches!(self.tcx.kind_of(elem), TyKind::Int(_) | TyKind::Error) {
                continue;
            }
            let rendered = self.render_public_ty(elem);
            self.emit(TypeError::SharedPayloadUnsupported { ty: rendered }, span);
        }
    }

    fn reject_unshareable_goroutine_captures(&mut self, expr: &Expr) {
        let unshareable: Vec<(String, Ty)> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.iter())
            .filter_map(|(name, ty)| {
                let resolved = self.infer.resolve(self.tcx, *ty);
                self.ty_is_unshareable_across_goroutines(resolved)
                    .then(|| (name.to_string(), resolved))
            })
            .collect();
        if unshareable.is_empty() {
            return;
        }
        for body in goroutine_bodies(expr) {
            let bound = closure_bound_names(body.params);
            for (name, ty) in &unshareable {
                if bound.contains(name) {
                    continue;
                }
                let mut one = HashSet::new();
                one.insert(name.clone());
                if expr_mentions_any_name(body.body, &one) {
                    let rendered = self.render_public_ty(*ty);
                    self.emit(
                        TypeError::ConcurrentCaptureUnsupported {
                            name: name.clone(),
                            ty: rendered,
                        },
                        body.body.span,
                    );
                }
            }
        }
    }

    /// Whether a value of `ty` reaches a goroutine only as shared nested
    /// storage. A bare sequence or map is published as one owned container,
    /// and a scalar, a `String`, or a runtime handle carries its own
    /// representation; an aggregate *holding* growable storage does not.
    fn ty_is_unshareable_across_goroutines(&self, ty: Ty) -> bool {
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        if matches!(
            self.tcx.kind_of(peeled),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. }
        ) {
            return false;
        }
        if self.ty_is_shareable_handle(peeled) {
            return false;
        }
        self.ty_contains_nested_vec(peeled)
    }

    /// A stdlib handle built to be reached from several goroutines: the
    /// synchronisation types, the channel ends, and `sync::Shared`.
    fn ty_is_shareable_handle(&self, ty: Ty) -> bool {
        if matches!(
            self.tcx.kind_of(ty),
            TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_)
        ) {
            return true;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind_of(ty) else {
            return false;
        };
        self.tcx.def_name(*def).is_some_and(|name| {
            let bare = name.rsplit("::").next().unwrap_or(name);
            matches!(
                bare,
                "Shared"
                    | "Mutex"
                    | "RwLock"
                    | "Once"
                    | "WaitGroup"
                    | "Barrier"
                    | "AtomicI64"
                    | "AtomicI32"
                    | "AtomicU64"
                    | "AtomicBool"
            )
        })
    }

    /// Rejects an aggregate written inline at a spawned call.
    ///
    /// The argument is a closure, so the call it wraps is one level in: a
    /// `spawn(|| f(Pair { .. }))` puts the aggregate where the boundary is.
    fn reject_spawn_inline_aggregate_args(&mut self, arg: &Expr) {
        let ExprKind::Closure { body, .. } = &arg.kind else {
            return self.reject_go_inline_aggregate_args(arg);
        };
        self.reject_go_inline_aggregate_args(body);
    }

    fn reject_go_inline_aggregate_args(&mut self, expr: &Expr) {
        let ExprKind::Call { args, .. } = &expr.kind else {
            return;
        };
        for arg in args {
            let Some(ty) = self.table.get(arg.id) else {
                continue;
            };
            let resolved = self.infer.resolve(self.tcx, ty);
            let inline = match self.tcx.kind_of(resolved) {
                TyKind::Tuple(_) | TyKind::Array { .. } => true,
                TyKind::Adt { def, .. } => {
                    def.local < u32::MAX - 32 && self.tcx.struct_field_tys(*def).is_some()
                }
                _ => false,
            };
            if inline {
                let ty = self.render_public_ty(resolved);
                self.emit(
                    TypeError::ConcurrentAggregateUnsupported {
                        ty,
                        boundary: "cross a goroutine boundary",
                    },
                    arg.span,
                );
            }
        }
    }

    fn check_closure(
        &mut self,
        params: &[ClosureParam],
        ret: Option<&AstType>,
        body: &Expr,
        expected: Expectation,
    ) -> Ty {
        let mut outer_references: HashSet<String> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.iter())
            .filter_map(|(name, ty)| {
                let ty = self.infer.resolve(self.tcx, *ty);
                matches!(self.tcx.kind_of(ty), TyKind::Ref { .. }).then(|| name.to_string())
            })
            .collect();
        for param in params {
            let mut names = Vec::new();
            pattern_binding_names(&param.pattern, &mut names);
            for name in names {
                outer_references.remove(&name);
            }
        }
        self.push_scope();
        // When the call site expects a function of a known shape (e.g. a
        // `Vec<T>` comparator pins `Fn(T, T) -> _`), unify each unannotated
        // parameter with the expected input type before the body is checked.
        // Without this a field access inside the body (`a.size`) sees the
        // parameter as an unresolved inference var and falls back to the
        // dynamic JSON-field path rather than the struct projection.
        let expected_inputs: Option<Vec<Ty>> = self
            .expectation_target(expected)
            .map(|t| self.infer.resolve(self.tcx, t))
            .and_then(|t| match self.tcx.kind(t) {
                Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig))
                    if sig.inputs.len() == params.len() =>
                {
                    Some(sig.inputs.clone())
                }
                _ => None,
            });
        let inputs: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(i, param)| {
                let ty = match param.ty.as_ref() {
                    Some(ty) => self.type_from_ast(ty),
                    None => self.fresh(),
                };
                if let Some(want) = expected_inputs.as_ref().map(|inputs| inputs[i]) {
                    self.unify(ty, want, body.span);
                }
                self.check_param_reference_pattern(&param.pattern, ty);
                self.bind_pattern(&param.pattern, ty);
                self.register_reference_parameter_origins(&param.pattern);
                ty
            })
            .collect();
        let output = match ret {
            Some(ty) => self.type_from_ast(ty),
            None => self.fresh(),
        };
        let body_expected = if ret.is_some() {
            Expectation::HasType(output)
        } else {
            Expectation::None
        };
        // `return` inside the body leaves the CLOSURE, not the function
        // the closure was written in, so the body is checked against the
        // closure's own output type.
        let prev_ret = self.current_fn_ret.replace(output);
        let body_ty = self.check_expr_expecting(body, body_expected);
        self.current_fn_ret = prev_ret;
        self.unify(output, body_ty, body.span);
        if expr_mentions_any_name(body, &outer_references) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: "be captured by a closure".to_string(),
                },
                body.span,
            );
        }
        let resolved_output = self.infer.resolve(self.tcx, output);
        if self.ty_contains_reference(resolved_output) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: "escape through a closure return".to_string(),
                },
                body.span,
            );
        }
        self.pop_scope();
        self.tcx.intern(TyKind::FnPtr(FnSig { inputs, output }))
    }

    /// True when a value of `ty` can be hashed and compared by value, the
    /// requirement every `Map` / `Set` key must satisfy. Mirrors the
    /// hashable-key list in the language reference: scalars, `String`,
    /// tuples, fixed arrays, structs, and enums are hashable when every
    /// piece they carry is; `Vec`, `Map`, `Set`, closures, references, and
    /// runtime handles are not - the language has no `Hash` impl for a
    /// heap container or a value with no stable identity to hash.
    /// Unresolved (`Var`/`Error`/alias) types are treated as hashable so
    /// this never rejects a type the checker hasn't pinned down yet - except
    /// an unsuffixed float literal (`{1.5: 2}`), which is still an
    /// unresolved `Var` at this point (numeric-literal defaulting runs
    /// later) but can only ever default to `f64`.
    fn is_hashable_ty(&mut self, ty: Ty) -> bool {
        if self.infer.is_float_literal_var(self.tcx, ty) {
            return false;
        }
        let resolved = self.infer.resolve(self.tcx, ty);
        let mut seen = std::collections::HashSet::new();
        self.is_hashable_ty_rec(resolved, &mut seen)
    }

    fn is_hashable_ty_rec(
        &self,
        ty: Ty,
        seen: &mut std::collections::HashSet<gossamer_resolve::DefId>,
    ) -> bool {
        match self.tcx.kind(ty) {
            Some(
                TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Int(_)
                | TyKind::Unit
                | TyKind::Never,
            ) => true,
            // A dynamic value's shape is not known until it exists, so there
            // is no key layout to fold it into.
            Some(TyKind::DynValue) => false,
            Some(TyKind::Tuple(elems)) => elems
                .iter()
                .all(|elem| self.is_hashable_ty_rec(*elem, seen)),
            Some(TyKind::Array { elem, .. }) => self.is_hashable_ty_rec(*elem, seen),
            // A nominal alias hashes and compares exactly as the value it
            // erases to, so it is a key wherever its representation is.
            Some(TyKind::Nominal { repr, .. }) => self.is_hashable_ty_rec(*repr, seen),
            Some(TyKind::Adt { def, substs }) => {
                match def.local {
                    RESULT_DEF_LOCAL | OPTION_DEF_LOCAL => {
                        return substs
                            .types()
                            .iter()
                            .all(|t| self.is_hashable_ty_rec(*t, seen));
                    }
                    HASH_SET_DEF_LOCAL
                    | BTREE_SET_DEF_LOCAL
                    | VEC_DEQUE_DEF_LOCAL
                    | BINARY_HEAP_DEF_LOCAL
                    | REVERSE_DEF_LOCAL
                    | MIN_HEAP_DEF_LOCAL
                    | VEC_QUEUE_DEF_LOCAL
                    | VEC_STACK_DEF_LOCAL => return false,
                    _ => {}
                }
                if is_opaque_handle_def(def.local) {
                    return false;
                }
                // A recursive struct/enum (`List { next: Box<List> }`) would
                // otherwise recurse forever; a repeat visit can't be the
                // reason a type ISN'T hashable, since every concrete value
                // is still finite, so let it pass and let the other fields
                // decide.
                if !seen.insert(*def) {
                    return true;
                }
                if let Some(fields) = self.tcx.adt_field_tys(*def, substs) {
                    return fields
                        .iter()
                        .all(|field| self.is_hashable_ty_rec(*field, seen));
                }
                if let Some(variants) = self.tcx.enum_variant_tys(*def) {
                    return variants.iter().all(|fields| {
                        fields
                            .iter()
                            .all(|field| self.is_hashable_ty_rec(*field, seen))
                    });
                }
                // Unregistered def (a generic template body, or a shape the
                // checker doesn't track field-wise): permissive, matching
                // the Var/Error fallback below.
                true
            }
            Some(
                TyKind::Float(_)
                | TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Iterator(_)
                | TyKind::Range(_)
                | TyKind::HashMap { .. }
                | TyKind::Sender(_)
                | TyKind::Receiver(_)
                | TyKind::JoinHandle(_)
                | TyKind::JsonValue
                | TyKind::DynError
                | TyKind::Ref { .. }
                | TyKind::FnDef { .. }
                | TyKind::FnPtr(_)
                | TyKind::FnTrait(_)
                | TyKind::Closure { .. }
                | TyKind::Dyn(_),
            ) => false,
            // `Duration` / `Instant` are transparent `i64` newtypes.
            Some(TyKind::Duration | TyKind::Instant) => true,
            // Unresolved / erased: don't reject what the checker can't see.
            Some(TyKind::Alias { .. } | TyKind::Var(_) | TyKind::Param { .. } | TyKind::Error)
            | None => true,
        }
    }

    fn check_map_literal(&mut self, entries: &[Expr], expected: Expectation) -> Ty {
        // A brace literal builds whichever map the call site expects, the way
        // `#{..}` builds a `Set` or the `BTreeSet` an expectation names.
        let expected_map =
            self.expectation_target(expected)
                .and_then(|target| match self.tcx.kind(target) {
                    Some(TyKind::HashMap {
                        key,
                        value,
                        ordered,
                    }) => Some((*key, *value, *ordered)),
                    _ => None,
                });
        let ordered = expected_map.is_some_and(|(_, _, ordered)| ordered);
        let (mut key_ty, mut value_ty) =
            expected_map.map_or_else(|| (self.fresh(), self.fresh()), |(k, v, _)| (k, v));

        for entry in entries {
            let ExprKind::Tuple(parts) = &entry.kind else {
                let entry_ty = self.check_expr(entry);
                let found = self.render_public_ty(entry_ty);
                self.emit(
                    TypeError::TypeMismatch {
                        expected: "(K, V)".to_string(),
                        found,
                    },
                    entry.span,
                );
                continue;
            };
            let [key, value] = parts.as_slice() else {
                let entry_ty = self.check_expr(entry);
                let found = self.render_public_ty(entry_ty);
                self.emit(
                    TypeError::TypeMismatch {
                        expected: "(K, V)".to_string(),
                        found,
                    },
                    entry.span,
                );
                continue;
            };
            let got_key = self.check_expr_expecting(key, expected.rewrap(key_ty));
            let got_value = self.check_expr_expecting(value, expected.rewrap(value_ty));
            if expected_map.is_some() && expected.unifies() {
                self.unify(key_ty, got_key, key.span);
                self.unify(value_ty, got_value, value.span);
            } else {
                key_ty = self.join_branch_tys(key_ty, got_key, key.span);
                value_ty = self.join_branch_tys(value_ty, got_value, value.span);
            }
            let pair = self.tcx.intern(TyKind::Tuple(vec![key_ty, value_ty]));
            self.record(entry.id, pair);
        }

        if !self.is_hashable_ty(key_ty)
            && let Some(ExprKind::Tuple(parts)) = entries.first().map(|e| &e.kind)
            && let Some(key) = parts.first()
        {
            let ty = self.render_public_ty(key_ty);
            self.emit(
                TypeError::TraitBoundNotSatisfied {
                    ty,
                    bound: "Hash".to_string(),
                },
                key.span,
            );
        }
        self.tcx.intern(TyKind::HashMap {
            key: key_ty,
            value: value_ty,
            ordered,
        })
    }

    fn check_set_literal(&mut self, entries: &[Expr], expected: Expectation) -> Ty {
        let expected_elem =
            self.expectation_target(expected)
                .and_then(|target| match self.tcx.kind(target) {
                    Some(TyKind::Adt { def, substs })
                        if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
                    {
                        substs
                            .types()
                            .first()
                            .copied()
                            .map(|elem| (def.local, elem))
                    }
                    _ => None,
                });

        if let Some((want_owner, want_elem)) = expected_elem {
            for entry in entries {
                let got = self.check_expr_expecting(entry, expected.rewrap(want_elem));
                if expected.unifies() {
                    self.unify(want_elem, got, entry.span);
                }
            }
            self.check_hashable_elem_ty(want_elem, entries);
            return if want_owner == BTREE_SET_DEF_LOCAL {
                self.btreeset_ty(want_elem)
            } else {
                self.hashset_ty(want_elem)
            };
        }

        let mut elem_ty = if let Some(first) = entries.first() {
            self.check_expr(first)
        } else {
            self.fresh()
        };
        for entry in entries.iter().skip(1) {
            let ty = self.check_expr(entry);
            elem_ty = self.join_branch_tys(elem_ty, ty, entry.span);
        }
        self.check_hashable_elem_ty(elem_ty, entries);
        self.hashset_ty(elem_ty)
    }

    /// Emits `GT0017` at the first set element's span when `elem_ty` fails
    /// [`Self::is_hashable_ty`]. Shared by both `check_set_literal` return
    /// paths so an explicitly-annotated `Set<Vec<i64>>` and an
    /// inference-only `#{v1, v2}` are rejected the same way.
    fn check_hashable_elem_ty(&mut self, elem_ty: Ty, entries: &[Expr]) {
        if !self.is_hashable_ty(elem_ty)
            && let Some(entry) = entries.first()
        {
            let ty = self.render_public_ty(elem_ty);
            self.emit(
                TypeError::TraitBoundNotSatisfied {
                    ty,
                    bound: "Hash".to_string(),
                },
                entry.span,
            );
        }
    }

    fn check_vec_literal(&mut self, arr: &ArrayExpr, expected: Expectation) -> Ty {
        let expected_elem =
            self.expectation_target(expected)
                .and_then(|target| match self.tcx.kind(target) {
                    Some(TyKind::Vec(elem) | TyKind::Slice(elem)) => Some(*elem),
                    _ => None,
                });
        match arr {
            ArrayExpr::List(elems) => {
                if let Some(want_elem) = expected_elem {
                    for elem in elems {
                        let got = self.check_expr_expecting(elem, expected.rewrap(want_elem));
                        if expected.unifies() {
                            self.unify(want_elem, got, elem.span);
                        }
                    }
                    return self.tcx.intern(TyKind::Vec(want_elem));
                }
                let mut elem_ty = if let Some(first) = elems.first() {
                    self.check_expr(first)
                } else {
                    self.fresh()
                };
                for elem in elems.iter().skip(1) {
                    let ty = self.check_expr(elem);
                    elem_ty = self.join_branch_tys(elem_ty, ty, elem.span);
                }
                self.tcx.intern(TyKind::Vec(elem_ty))
            }
            ArrayExpr::Repeat { value, count } => {
                let elem_ty = match expected_elem {
                    Some(want_elem) => {
                        let got = self.check_expr_expecting(value, expected.rewrap(want_elem));
                        if expected.unifies() {
                            self.unify(want_elem, got, value.span);
                        }
                        want_elem
                    }
                    None => self.check_expr(value),
                };
                self.check_expr(count);
                self.tcx.intern(TyKind::Vec(elem_ty))
            }
        }
    }

    fn check_array(&mut self, arr: &ArrayExpr, expected: Expectation) -> Ty {
        // This handles explicit fixed-array literals (`#[...]`) and
        // expectation-shaped `[T; N]` arrays. Plain `[...]` literals are checked
        // through `check_vec` unless an array expectation selected this path.
        match arr {
            ArrayExpr::List(elems) => {
                let expected_elem = self
                    .expectation_target(expected)
                    .and_then(|target| match self.tcx.kind(target) {
                        Some(TyKind::Array { elem, len })
                            if *len == crate::ArrayLen::Concrete(elems.len()) =>
                        {
                            Some(*elem)
                        }
                        _ => None,
                    });
                if let Some(want_elem) = expected_elem {
                    for elem in elems {
                        let got = self.check_expr_expecting(elem, expected.rewrap(want_elem));
                        if expected.unifies() {
                            self.unify(want_elem, got, elem.span);
                        }
                    }
                    return self.tcx.intern(TyKind::Array {
                        elem: want_elem,
                        len: crate::ArrayLen::Concrete(elems.len()),
                    });
                }
                let mut elem_ty = if let Some(first) = elems.first() {
                    self.check_expr(first)
                } else {
                    self.fresh()
                };
                for elem in elems.iter().skip(1) {
                    let ty = self.check_expr(elem);
                    elem_ty = self.join_branch_tys(elem_ty, ty, elem.span);
                }
                self.tcx.intern(TyKind::Array {
                    elem: elem_ty,
                    len: crate::ArrayLen::Concrete(elems.len()),
                })
            }
            ArrayExpr::Repeat { value, count } => {
                let elem_ty = self.check_expr(value);
                self.check_expr(count);
                if let Some(len) = self.evaluate_array_len(count) {
                    let elem_ty = match self
                        .expectation_target(expected)
                        .and_then(|t| self.tcx.kind(t))
                    {
                        Some(TyKind::Array { elem, .. }) => self.infer.resolve(self.tcx, *elem),
                        _ => self.infer.resolve(self.tcx, elem_ty),
                    };
                    self.tcx.intern(TyKind::Array {
                        elem: elem_ty,
                        len: crate::ArrayLen::Concrete(len),
                    })
                } else {
                    self.emit(TypeError::ArrayLengthNotConstant, count.span);
                    self.tcx.intern(TyKind::Array {
                        elem: elem_ty,
                        len: crate::ArrayLen::Concrete(0),
                    })
                }
            }
        }
    }

    fn check_path_expr(&mut self, node: NodeId, path: &gossamer_ast::PathExpr, span: Span) -> Ty {
        self.check_path_read_conflict(path, span);
        // `Enum::Variant` naming a variant the enum does not declare: the
        // resolver resolves the path to the enum head and leaves the bad
        // tail to fault at runtime (GX0002 `Shape::Triangle`). Reject it
        // where the enum is known and the tail is neither a declared
        // variant nor an associated function on the enum.
        if self.reject_unknown_variant_path(path, span) {
            return self.tcx.error_ty();
        }
        if let Some(ty) = self.check_assoc_const_path(path, span) {
            return self.record(node, ty);
        }
        let Some(resolution) = self.resolutions.get(node) else {
            return self.check_std_path_value(node, path, span);
        };
        match resolution {
            Resolution::Local(binding_id) => {
                if !self.suppressed.consumed_iterator_read
                    && path.segments.len() == 1
                    && let Some(name) = path.segments.first().map(|seg| seg.name.name.as_str())
                    && let Some(operation) = self
                        .consumed_iterators
                        .iter()
                        .rev()
                        .find_map(|scope| scope.get(&binding_id).cloned())
                {
                    self.emit(
                        TypeError::IteratorStateConsumed {
                            name: name.to_string(),
                            operation,
                        },
                        span,
                    );
                }
                if let Some(ty) = self.binding_types.get(&binding_id).copied() {
                    return ty;
                }
                if let Some(first) = path.segments.first() {
                    if let Some(ty) = self.lookup_local(&first.name.name) {
                        return ty;
                    }
                }
                self.fresh()
            }
            Resolution::Primitive(prim) => self.type_from_primitive(prim),
            Resolution::Def { def, kind } => match kind {
                gossamer_resolve::DefKind::Struct => {
                    if self.struct_fields.get(&def).is_some_and(|fields| {
                        !fields.is_empty() || self.tcx.is_tuple_struct(def.local)
                    }) && !self.callee_path_nodes.contains(&node)
                    {
                        let name = self
                            .tcx
                            .def_name(def)
                            .map_or_else(|| "<struct>".to_string(), ToString::to_string);
                        let error = if self.tcx.is_tuple_struct(def.local) {
                            TypeError::TupleStructConstructorParenthesesRequired { name }
                        } else {
                            TypeError::StructConstructorBracesRequired { name }
                        };
                        self.emit(error, span);
                    }
                    self.tcx.intern(TyKind::Adt {
                        def,
                        substs: crate::Substs::new(),
                    })
                }
                gossamer_resolve::DefKind::Enum => {
                    // A generic enum named without a payload to infer from -
                    // `L::Nil` - still has to carry parameters, or it will
                    // not unify with the `L<i64>` it is being bound to.
                    let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
                    let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
                    self.tcx.intern(TyKind::Adt {
                        def,
                        substs: crate::Substs::from_types(substs),
                    })
                }
                gossamer_resolve::DefKind::Fn => {
                    // Pull turbofish args (`ident::<i64, bool>`) off
                    // the last path segment, resolve each to a
                    // concrete [`Ty`], and stamp the callee's type as
                    // `TyKind::FnDef { def, substs }` so that the MIR
                    // lowerer reads the real substitution instead of
                    // deriving one heuristically from argument types.
                    let substs = self.substs_from_path(path);
                    self.tcx.intern(TyKind::FnDef { def, substs })
                }
                gossamer_resolve::DefKind::Const => self
                    .const_tys
                    .get(&def)
                    .copied()
                    .unwrap_or_else(|| self.fresh()),
                _ => self.fresh(),
            },
            Resolution::Import { .. } | Resolution::Err => {
                // A `use` of this unit's own item keeps its opaque
                // `Import` resolution so lowering still qualifies the
                // name, but the item's type is known here: type the
                // reference by its definition rather than by a fresh
                // variable, which would leave every use of the value
                // unchecked.
                if let Some(def) = self.resolutions.import_def(node)
                    && let Some(ty) = self.ty_of_imported_def(def, path)
                {
                    return self.record(node, ty);
                }
                self.check_std_path_value(node, path, span)
            }
        }
    }

    fn check_path_read_conflict(&mut self, path: &gossamer_ast::PathExpr, span: Span) {
        if self.suppressed.borrow_read_conflict {
            return;
        }
        let [segment] = path.segments.as_slice() else {
            return;
        };
        let Some(borrower) = self
            .active_mutable_borrower(&segment.name.name)
            .map(str::to_string)
        else {
            return;
        };
        self.emit(
            TypeError::BorrowedPlaceConflict {
                root: segment.name.name.clone(),
                borrower,
                action: "read",
            },
            span,
        );
    }

    fn reject_unknown_variant_path(&mut self, path: &gossamer_ast::PathExpr, span: Span) -> bool {
        let n = path.segments.len();
        if n < 2 {
            return false;
        }
        let enum_name = path.segments[n - 2].name.name.as_str();
        let variant = path.segments[n - 1].name.name.as_str();
        let unknown = self.enum_variants.get(enum_name).is_some_and(|variants| {
            !variants.contains(variant)
                && !self
                    .user_method_owners
                    .get(variant)
                    .is_some_and(|owners| owners.contains(enum_name))
        });
        if unknown {
            let mut declared: Vec<String> = self
                .enum_variants
                .get(enum_name)
                .map(|variants| variants.iter().cloned().collect())
                .unwrap_or_default();
            declared.sort();
            self.emit(
                TypeError::UnknownVariant {
                    enum_name: enum_name.to_string(),
                    variant: variant.to_string(),
                    declared,
                },
                span,
            );
        }
        unknown
    }

    /// Types an unresolved path expression, handling std free
    /// functions used as first-class values. Tabled names type as a
    /// concrete `FnPtr` so combinator rows can pin against the
    /// signature; untabled std-fn-shaped paths in a value position
    /// are rejected uniformly (GT0015) because the compiled tiers
    /// have no symbol to take the address of. Everything else keeps
    /// the historical fresh-var fallback.
    /// Type of a reference to `def`, reached through a `use` of this
    /// unit's own item. Mirrors the `Resolution::Def` arm of
    /// [`Self::check_path_expr`] for the kinds a value path can name.
    fn ty_of_imported_def(&mut self, def: DefId, path: &gossamer_ast::PathExpr) -> Option<Ty> {
        match self.resolutions.kind_of(def)? {
            gossamer_resolve::DefKind::Fn => {
                let substs = self.substs_from_path(path);
                Some(self.tcx.intern(TyKind::FnDef { def, substs }))
            }
            gossamer_resolve::DefKind::Struct | gossamer_resolve::DefKind::Enum => {
                Some(self.tcx.intern(TyKind::Adt {
                    def,
                    substs: crate::Substs::new(),
                }))
            }
            gossamer_resolve::DefKind::Const => self.const_tys.get(&def).copied(),
            _ => None,
        }
    }

    fn check_std_path_value(
        &mut self,
        node: NodeId,
        path: &gossamer_ast::PathExpr,
        span: Span,
    ) -> Ty {
        let resolved_segments = self.resolved_value_path_names(node, path);
        let segments: Vec<&str> = resolved_segments.iter().map(String::as_str).collect();
        let joined = segments.join("::");
        if let Some((int_ty, _value)) = int_assoc_const(&segments) {
            return self.tcx.int_ty(int_ty);
        }
        // `fs::SEEK_SET` / `SEEK_CUR` / `SEEK_END` name the `whence`
        // selector `File::seek` takes.
        if matches!(
            segments.as_slice(),
            ["fs", "SEEK_SET" | "SEEK_CUR" | "SEEK_END"]
        ) {
            return self.tcx.int_ty(IntTy::I64);
        }
        if let Some(entry) = crate::std_fn_values::std_fn_value(
            joined.strip_prefix("std::").unwrap_or(joined.as_str()),
        ) {
            let inputs: Vec<Ty> = entry.params.iter().map(|p| self.std_val_ty(*p)).collect();
            let output = self.std_val_ty(entry.ret);
            return self.tcx.intern(TyKind::FnPtr(FnSig { inputs, output }));
        }
        if !self.callee_path_nodes.contains(&node)
            && crate::std_fn_values::is_std_free_fn_path(&segments)
        {
            // A macro path is not a function at all; the resolver already
            // named it and said how to write it, so adding a second report
            // about parameter lists describes the wrong thing.
            let relative = joined.strip_prefix("std::").unwrap_or(joined.as_str());
            if gossamer_resolve::stdlib_macro_named(relative).is_none() {
                self.emit(TypeError::StdFnValueUnsupported { path: joined }, span);
            }
            return self.tcx.error_ty();
        }
        self.fresh()
    }

    /// Concrete [`Ty`] for one [`crate::std_fn_values::StdValTy`] slot.
    fn std_val_ty(&mut self, shape: crate::std_fn_values::StdValTy) -> Ty {
        use crate::std_fn_values::StdValTy;
        match shape {
            StdValTy::Str => self.tcx.string_ty(),
            StdValTy::I64 => self.tcx.int_ty(IntTy::I64),
            StdValTy::Error => self.tcx.dyn_error_ty(),
            StdValTy::ResultI64 => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let err = self.tcx.dyn_error_ty();
                self.result_adt_ty(i64_ty, err)
            }
        }
    }

    fn substs_from_path(&mut self, path: &gossamer_ast::PathExpr) -> crate::Substs {
        let generics = match path.segments.last() {
            Some(seg) => &seg.generics,
            None => return crate::Substs::new(),
        };
        let args: Vec<crate::GenericArg> = generics
            .iter()
            .map(|arg| match arg {
                gossamer_ast::GenericArg::Type(t) => crate::GenericArg::Type(self.type_from_ast(t)),
                gossamer_ast::GenericArg::Const(expr) => {
                    crate::GenericArg::Const(self.evaluate_generic_const_arg(expr))
                }
            })
            .collect();
        crate::Substs::from_args(args)
    }

    fn type_of_literal(&mut self, lit: &Literal, span: Span) -> Ty {
        match lit {
            Literal::Int(text) => self.type_of_int_literal(text, span),
            Literal::Float(text) => self.type_of_float_literal(text),
            Literal::String(_) | Literal::RawString { .. } => self.tcx.string_ty(),
            Literal::Char(_) => self.tcx.char_ty(),
            Literal::Byte(_) => self.tcx.int_ty(IntTy::U8),
            Literal::ByteString(_) | Literal::RawByteString { .. } => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Slice(u8_ty))
            }
            Literal::Bool(_) => self.tcx.bool_ty(),
            Literal::Unit => self.tcx.unit(),
        }
    }

    fn type_of_int_literal(&mut self, text: &str, span: Span) -> Ty {
        for (suffix, int_ty) in INT_SUFFIXES {
            if text.ends_with(suffix) {
                if matches!(int_ty, IntTy::I128 | IntTy::U128) {
                    self.emit(
                        TypeError::Int128Unsupported {
                            ty: (*suffix).to_string(),
                        },
                        span,
                    );
                    return self.tcx.error_ty();
                }
                if !int_literal_fits(text, *int_ty) {
                    self.emit(
                        TypeError::IntLiteralOverflow {
                            literal: text.to_string(),
                            ty: (*suffix).to_string(),
                        },
                        span,
                    );
                    return self.tcx.error_ty();
                }
                return self.tcx.int_ty(*int_ty);
            }
        }
        for (suffix, float_ty) in FLOAT_SUFFIXES {
            if text.ends_with(suffix) {
                return self.tcx.float_ty(*float_ty);
            }
        }
        // Unsuffixed integer literal - Go-style untyped constant.
        // The fresh var is integer-constrained so it can only
        // unify with concrete integer types; if no use-site
        // constraints arise it defaults to `i64` at the end of
        // typechecking. Validate magnitude against the widest
        // integer bucket the language exposes (`u128`/`i128`),
        // not against `i64` alone - `let x: u64 = u64::MAX` is
        // a legitimate program; the use-site unification will
        // either succeed (assign to u64) or fail with a normal
        // type-mismatch diagnostic. Only literals whose
        // magnitude is genuinely impossible to represent in any
        // Gossamer integer type get the GT0009 here.
        let literal_too_wide =
            parse_int_magnitude(text).is_none_or(|magnitude| magnitude > u128::from(u64::MAX));
        if literal_too_wide {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: text.to_string(),
                    ty: "any integer type".to_string(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        self.infer.fresh_int_var(self.tcx)
    }

    fn type_of_float_literal(&mut self, text: &str) -> Ty {
        for (suffix, float_ty) in FLOAT_SUFFIXES {
            if text.ends_with(suffix) {
                return self.tcx.float_ty(*float_ty);
            }
        }
        // Unsuffixed float literal: a float-defaulting inference var.
        // Takes its use-site float width when constrained, falls back
        // to `f64` otherwise (see `default_unresolved_float_vars`).
        self.infer.fresh_float_var(self.tcx)
    }

    fn type_from_primitive(&mut self, prim: PrimitiveTy) -> Ty {
        match prim {
            PrimitiveTy::Bool => self.tcx.bool_ty(),
            PrimitiveTy::Char => self.tcx.char_ty(),
            PrimitiveTy::String => self.tcx.string_ty(),
            PrimitiveTy::Int(width) => self.tcx.int_ty(int_ty_from_width(width, true)),
            PrimitiveTy::UInt(width) => self.tcx.int_ty(int_ty_from_width(width, false)),
            PrimitiveTy::Float(FloatWidth::W32) => self.tcx.float_ty(FloatTy::F32),
            PrimitiveTy::Float(FloatWidth::W64) => self.tcx.float_ty(FloatTy::F64),
            PrimitiveTy::Never => self.tcx.never(),
            PrimitiveTy::Unit => self.tcx.unit(),
        }
    }

    fn type_from_ast(&mut self, ast_ty: &AstType) -> Ty {
        let ty = match &ast_ty.kind {
            AstTypeKind::Unit => self.tcx.unit(),
            AstTypeKind::Never => self.tcx.never(),
            AstTypeKind::Infer => self.fresh(),
            AstTypeKind::Path(path) => self.type_from_ast_path(ast_ty.id, ast_ty.span, path),
            AstTypeKind::Tuple(elems) => {
                let tys: Vec<Ty> = elems.iter().map(|e| self.type_from_ast(e)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            AstTypeKind::Array { elem, len } => {
                let elem_ty = self.type_from_ast(elem);
                let count = self.array_len_from_ast(len);
                self.tcx.intern(TyKind::Array {
                    elem: elem_ty,
                    len: count,
                })
            }
            AstTypeKind::Slice(inner) => {
                let inner_ty = self.type_from_ast(inner);
                let element = self.render_public_ty(inner_ty);
                self.emit(TypeError::UnsizedSliceValue { element }, ast_ty.span);
                self.tcx.error_ty()
            }
            AstTypeKind::Ref { mutability, inner } => {
                let inner_ty = match &inner.kind {
                    AstTypeKind::Slice(element) => {
                        let element = self.type_from_ast(element);
                        self.tcx.intern(TyKind::Slice(element))
                    }
                    _ => self.type_from_ast(inner),
                };
                let mutability = match mutability {
                    gossamer_ast::Mutability::Immutable => Mutbl::Not,
                    gossamer_ast::Mutability::Mutable => Mutbl::Mut,
                };
                self.tcx.intern(TyKind::Ref {
                    mutability,
                    inner: inner_ty,
                })
            }
            AstTypeKind::Fn { kind, params, ret } => {
                let inputs: Vec<Ty> = params.iter().map(|p| self.type_from_ast(p)).collect();
                let output = match ret.as_ref() {
                    Some(ty) => self.type_from_ast(ty),
                    None => self.tcx.unit(),
                };
                let sig = FnSig { inputs, output };
                match kind {
                    gossamer_ast::FnTypeKind::Fn => self.tcx.intern(TyKind::FnPtr(sig)),
                    // `Fn` / `FnMut` / `FnOnce` all map to the single
                    // `FnTrait` callable shape. The MIR / codegen
                    // machinery uses one fat-pointer ABI for all
                    // three; the borrow-style distinctions Rust
                    // makes are unnecessary in a fully GC'd world.
                    gossamer_ast::FnTypeKind::ClosureFn
                    | gossamer_ast::FnTypeKind::ClosureFnMut
                    | gossamer_ast::FnTypeKind::ClosureFnOnce => {
                        self.tcx.intern(TyKind::FnTrait(sig))
                    }
                }
            }
        };
        // i128 / u128 have no runtime representation on any tier
        // (GT0014); reject at the spelling site so every execution
        // mode fails identically instead of the VM running 128-bit
        // arithmetic at silent 64-bit width.
        if let Some(TyKind::Int(it @ (IntTy::I128 | IntTy::U128))) = self.tcx.kind(ty) {
            let name = if matches!(it, IntTy::I128) {
                "i128"
            } else {
                "u128"
            };
            self.emit(
                TypeError::Int128Unsupported {
                    ty: name.to_string(),
                },
                ast_ty.span,
            );
            let err = self.tcx.error_ty();
            return self.record(ast_ty.id, err);
        }
        self.record(ast_ty.id, ty)
    }

    /// Expands a `type X<..> = T` alias `def` to its underlying type `T`,
    /// substituting the alias's type parameters with the use-site arguments
    /// in `path` for a generic alias. Returns `None` when `def` is not a
    /// registered alias or the argument count does not match the alias's
    /// parameters (the caller then falls back to the nominal form). Emits
    /// GT0024 and yields the error type on a cyclic alias.
    fn expand_type_alias(
        &mut self,
        def: gossamer_resolve::DefId,
        name: &str,
        span: Span,
        path: &TypePath,
    ) -> Option<Ty> {
        let (params, rhs) = self.alias_targets.get(&def).cloned()?;
        if !self.alias_expanding.insert(def) {
            self.emit(
                TypeError::CyclicTypeAlias {
                    name: name.to_string(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        let body = if params.is_empty() {
            Some(rhs)
        } else {
            let args = alias_type_args(path);
            (args.len() == params.len()).then(|| subst_alias_params(&rhs, &params, &args))
        };
        let expanded = body.map(|b| self.type_from_ast(&b));
        self.alias_expanding.remove(&def);
        // An opaque alias keeps the expansion only as its representation:
        // the type it hands back is distinct from that representation and
        // from every other alias over it.
        if self.nominal_aliases.contains(&def) {
            return expanded.map(|repr| self.tcx.nominal_ty(def, repr));
        }
        expanded
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per built-in type constructor; splitting hides the dispatch table"
    )]
    fn type_from_ast_path(&mut self, node: NodeId, span: Span, path: &TypePath) -> Ty {
        let head_name = path
            .segments
            .first()
            .map_or("", |seg| seg.name.name.as_str());
        if let Some(prim) = primitive_from_name(head_name) {
            return prim_to_ty(self.tcx, prim);
        }
        // Inside an `impl`, `Self` names the type being implemented. Leaving
        // it as a fresh variable made every `-> Self` constructor's result
        // unconstrained, so the calls on that value went unchecked.
        if head_name == "Self"
            && path.segments.len() == 1
            && let Some(self_ty) = self.current_self_ty
        {
            return self_ty;
        }
        if path.segments.len() >= 2
            && let Some(projected) = self.resolve_assoc_type_projection(path, span)
        {
            return projected;
        }
        // Recognise the stdlib's opaque dynamic JSON value by
        // surface name. The resolver doesn't allocate a `DefId`
        // for it (it comes in via `use std::encoding::json` as a
        // bare import), so we'd otherwise fall through to a fresh
        // inference variable and lose the receiver-shape signal
        // that downstream MIR needs to route field access through
        // the json runtime helpers.
        if path_matches_json_value(path) {
            return self.tcx.json_value_ty();
        }
        if path_matches_dyn_error(path) {
            return self.tcx.dyn_error_ty();
        }
        // The open dynamic value is a prelude type, so its bare name is the
        // whole path.
        if path.segments.len() == 1 && path.segments[0].name.name.as_str() == "DynValue" {
            return self.tcx.dyn_value_ty();
        }
        // A trait names behaviour, not a value's type. Gossamer has no `dyn`,
        // so a bare trait in type position has no runtime shape to stand for
        // and would otherwise settle as an unconstrained variable that
        // accepts anything. A name that also names a type in scope is that
        // type: a program is free to declare its own `Reader`, and the
        // resolver has already said which one this path reached.
        if let Some(last) = path.segments.last()
            && !matches!(
                self.resolutions.get(node),
                Some(
                    Resolution::Def {
                        kind: gossamer_resolve::DefKind::Struct
                            | gossamer_resolve::DefKind::Enum
                            | gossamer_resolve::DefKind::TypeAlias
                            | gossamer_resolve::DefKind::TypeParam,
                        ..
                    } | Resolution::Primitive(_)
                )
            )
            && (STDLIB_TRAIT_NAMES.contains(&last.name.name.as_str())
                || self.trait_own_methods.contains_key(&last.name.name))
        {
            self.emit(
                TypeError::TraitInTypePosition {
                    name: last.name.name.clone(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        if let Some(resolution) = self.resolutions.get(node) {
            match resolution {
                Resolution::Primitive(prim) => return self.type_from_primitive(prim),
                Resolution::Def { def, kind } => {
                    // A path resolving to a generic type parameter (`fn
                    // f<T>(x: T)` or `struct Pair<A, B> { fst: A }`)
                    // must surface as `TyKind::Param`, not as an `Adt`
                    // whose `def` happens to point at the parameter's
                    // binding. Without this branch, the `A` in `Pair<A>`
                    // unifies as an opaque ADT and any concrete struct
                    // literal hits a `type mismatch: expected adt#N`
                    // error rather than driving inference of A from
                    // the field-value type.
                    //
                    // Use the resolver's per-resolution kind rather
                    // than `resolutions.kind_of(def)` because the
                    // resolver records the DefKind on the
                    // resolution itself but not always on the
                    // separate def→kind map (`bind_generics` only
                    // inserts into the scope, not into the global
                    // map).
                    if kind == gossamer_resolve::DefKind::TypeParam {
                        if let Some((idx, name)) =
                            self.current_generic_scope.get(head_name).cloned()
                        {
                            return self.tcx.intern(TyKind::Param { idx, name });
                        }
                        return self.fresh();
                    }
                    // A non-generic `type X = T` alias is transparent: a use
                    // of `X` lowers to `T`, not to an opaque `adt#N`.
                    if kind == gossamer_resolve::DefKind::TypeAlias
                        && let Some(ty) = self.expand_type_alias(def, head_name, span, path)
                    {
                        return ty;
                    }
                    let substs = self.substs_from_ast(path);
                    return self.tcx.intern(TyKind::Adt { def, substs });
                }
                Resolution::Import { .. } | Resolution::Err | Resolution::Local(_) => {}
            }
        }
        // A built-in type constructor may be written bare (`Deque<i64>`)
        // or under the module that exports it
        // (`std::collections::Deque<i64>`). Both spellings name the same
        // type, so the qualified one reduces to its last segment before
        // the table below keys on it. A user type reached by a qualified
        // path resolved above, so nothing here can capture one.
        let head_name = builtin_type_head(path).unwrap_or(head_name);
        // Fallback for built-in generic enums the resolver doesn't
        // hand out a DefId for (`Result<T, E>`, `Option<T>`). Without
        // this, an annotation like `let r: Result<i64, String> = ...`
        // falls through to a fresh inference variable, losing the
        // substs the variant-binding fixup later needs to re-pin
        // `x` in `Ok(x) => …` to the actual payload type. The
        // sentinel `DefId`s use `u32::MAX` / `u32::MAX-1` so they
        // never collide with anything the resolver emits.
        match head_name {
            "Result" => {
                let mut substs = self.substs_from_ast(path);
                // `Result<T>` with a single arg is shorthand for
                // `Result<T, errors::Error>`, matching Rust's
                // `anyhow::Result<T>` convention.
                if substs.types().len() == 1 {
                    let e = self.tcx.dyn_error_ty();
                    substs = crate::Substs::from_types([substs.types()[0], e]);
                }
                let def = gossamer_resolve::DefId::local(u32::MAX);
                self.tcx.register_def_name(def, "Result");
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "Option" => {
                let substs = self.substs_from_ast(path);
                let def = gossamer_resolve::DefId::local(u32::MAX - 1);
                self.tcx.register_def_name(def, "Option");
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "Vec" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::Vec(elem));
            }
            // `Range<T>` is what a range expression produces and converts to
            // `Iterator<T>`, so either spelling accepts a range while only
            // `Range` reports back as one.
            "Iterator" | "Range" => {
                let substs = self.substs_from_ast(path);
                let item = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return if head_name == "Range" {
                    self.tcx.range_ty(item)
                } else {
                    self.tcx.intern(TyKind::Iterator(item))
                };
            }
            // `Sender<T>` / `Receiver<T>` / `JoinHandle<T>` carry their
            // element type in a dedicated `TyKind`. Resolving the
            // annotation to that kind (rather than a fresh inference var)
            // lets `rx.recv()` recover `Option<T>` and a `Sender`-typed
            // param pin the channel element - without it a struct sent
            // over a channel infers as the default `i64` and materialises
            // a single pointer word instead of its inline fields.
            "Sender" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::Sender(elem));
            }
            "Receiver" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::Receiver(elem));
            }
            "JoinHandle" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::JoinHandle(elem));
            }
            "Map" => {
                let substs = self.substs_from_ast(path);
                let tys = substs.types();
                let key = tys.first().copied().unwrap_or_else(|| self.fresh());
                let value = tys.get(1).copied().unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: false,
                });
            }
            // `HashSet<T>` / `BTreeSet<T>` are opaque i64 handles at
            // runtime with no dedicated `TyKind`. Resolving the annotation to
            // a named sentinel Adt (rather than a fresh inference var) lets
            // method dispatch recover the receiver kind from its *type* when a
            // set/map flows across a function boundary and the construction
            // tag is gone.
            "Set" | "BTreeSet" => {
                let substs = self.substs_from_ast(path);
                // An annotation that names no element still names a set, so
                // the element is inferred rather than absent - the same
                // fallback the map arms below make. A set whose substs were
                // empty answered no element type, and every method lookup
                // that reads the receiver's element off its type then failed
                // to see a set at all: `impl Display for Set` could not call
                // `self.len()`.
                let substs = if substs.types().is_empty() {
                    let elem = self.fresh();
                    crate::Substs::from_types([elem])
                } else {
                    substs
                };
                let (local, name) = if path
                    .segments
                    .last()
                    .is_some_and(|seg| seg.name.name == "BTreeSet")
                {
                    (BTREE_SET_DEF_LOCAL, "BTreeSet")
                } else {
                    (HASH_SET_DEF_LOCAL, "Set")
                };
                if name == "BTreeSet"
                    && let Some(elem) = substs.types().first().copied()
                {
                    self.require_container_reads_the_types_order(elem, name, span);
                }
                let def = gossamer_resolve::DefId::local(local);
                self.tcx.register_def_name(def, name);
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "BTreeMap" => {
                let substs = self.substs_from_ast(path);
                let tys = substs.types();
                let key = tys.first().copied().unwrap_or_else(|| self.fresh());
                let value = tys.get(1).copied().unwrap_or_else(|| self.fresh());
                self.require_container_reads_the_types_order(key, "BTreeMap", span);
                return self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: true,
                });
            }
            // Phase 1 `VecDeque` is an opaque i64 ring-buffer handle. Resolve
            // the annotation to the named sentinel Adt so method dispatch can
            // recover the receiver kind after construction tags are gone.
            "Deque" | "Queue" | "Stack" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                let elem = self.require_slot_collection_elem(elem, head_name, span);
                let (local, name) = match head_name {
                    "Queue" => (VEC_QUEUE_DEF_LOCAL, "Queue"),
                    "Stack" => (VEC_STACK_DEF_LOCAL, "Stack"),
                    _ => (VEC_DEQUE_DEF_LOCAL, "Deque"),
                };
                let def = gossamer_resolve::DefId::local(local);
                self.tcx.register_def_name(def, name);
                let substs = crate::Substs::from_types([elem]);
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "MaxHeap" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                let elem = self.require_slot_collection_elem(elem, head_name, span);
                return self.binary_heap_ty(elem);
            }
            "MinHeap" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                let elem = self.require_slot_collection_elem(elem, head_name, span);
                return self.min_heap_ty(elem);
            }
            "Reverse" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.reverse_ty(elem);
            }
            // `Box<T>` / `Arc<T>` / `Rc<T>` name no distinction the language
            // draws: every value is already heap-shared and reference-counted,
            // so the wrapper reads as a choice a writer has to make and there
            // is none. The spelling reports with the rewrite that strips it,
            // and still checks as the inner type so the rest of the program
            // is diagnosed on its own terms.
            "Box" | "Arc" | "Rc" => {
                let substs = self.substs_from_ast(path);
                let tys = substs.types();
                let inner = tys.first().copied();
                let inner_text = inner.map_or_else(
                    || "T".to_string(),
                    |ty| {
                        let rendered = self.render_public_ty(ty);
                        if rendered.is_empty() {
                            "T".to_string()
                        } else {
                            rendered
                        }
                    },
                );
                self.emit(
                    TypeError::TransparentWrapper {
                        wrapper: head_name.to_string(),
                        inner: inner_text,
                    },
                    span,
                );
                if let Some(inner) = inner {
                    return inner;
                }
                return self.fresh();
            }
            // `Weak<T>` - a non-owning reference into an RC allocation.
            // Unlike `Box`/`Arc`/`Rc` it is NOT transparent: it carries
            // its own sentinel ADT so the drop pass releases it via the
            // weak helpers and `upgrade()` can produce an `Option<T>`.
            "Weak" => {
                let substs = self.substs_from_ast(path);
                let payload = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.weak_adt_ty(payload);
            }
            _ => {}
        }
        // `time::Duration` / `time::Instant` are transparent i64 newtypes
        // with no resolver `DefId`. An explicit annotation (`d:
        // time::Duration`) must resolve to the dedicated `TyKind` so the
        // method form (`d.as_millis()`) dispatches on the receiver's
        // static type the same way the inference form does - otherwise
        // it falls to name-global dispatch and fails to lower on the
        // compiled tiers. Match the full module path (not a bare tail)
        // so a user type or `flag::Cell::Duration` named `Duration` is
        // left untouched.
        let segs: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        if matches!(
            segs.as_slice(),
            ["time", "Duration"] | ["std", "time", "Duration"]
        ) {
            return self.tcx.duration_ty();
        }
        if matches!(
            segs.as_slice(),
            ["time", "Instant"] | ["std", "time", "Instant"]
        ) {
            return self.tcx.instant_ty();
        }
        if matches!(segs.as_slice(), ["flag", "Set"] | ["std", "flag", "Set"]) {
            return self.flag_set_ty();
        }
        match segs.as_slice() {
            ["http", "Client"] | ["std", "http", "Client"] => return self.http_client_ty(),
            ["http", "ClientBuilder"] | ["std", "http", "ClientBuilder"] => {
                return self.http_client_builder_ty();
            }
            ["http", "Request"] | ["std", "http", "Request"] => return self.http_request_ty(),
            ["http", "Response"] | ["std", "http", "Response"] => return self.http_response_ty(),
            _ => {}
        }
        // Recognise stdlib struct types by their last path segment
        // so parameter annotations like `entry: &fs::DirInfo` resolve
        // to the sentinel Adt rather than a fresh inference variable.
        // Without this, the MIR can't recover "DirInfo" from the
        // parameter's type and field access (`entry.is_symlink`) falls
        // through to gos_rt_json_get instead of a Field(idx) projection.
        let tail = path.segments.last().map_or("", |s| s.name.name.as_str());
        let stdlib_def_offset: Option<u32> = match tail {
            // A written `regex::Pattern` annotation is
            // the same sentinel handle the constructor answers. A parameter
            // carries no construction site, so without this the receiver
            // stays an inference variable and its methods dispatch by bare
            // name - a spelling the bytecode VM registers and the compiled
            // tiers have no symbol for.
            "Pattern" => Some(26),
            "DirInfo" => Some(2),
            "Output" => Some(3),
            "ResponseStream" => Some(4),
            "Response" => Some(5),
            // `context::Context` - an opaque i64 handle with no
            // dedicated `TyKind`. Resolving the annotation to a named
            // sentinel Adt (rather than a fresh inference var) lets
            // method dispatch recover the receiver kind from its *type*
            // when a context flows in as a parameter (the canonical
            // request-propagation shape) and the construction tag is
            // gone - the `is_cancelled` / `cancel` / `done` / `done_chan`
            // calls then route to the `gos_rt_ctx_*` shims.
            "Context" => Some(11),
            // `U8Vec`: a byte-buffer handle. A concrete sentinel here lets
            // the JIT marshal a `buf: U8Vec` parameter across the trampoline
            // (`ty_to_kind` keys on `u32::MAX - 20`) instead of leaving it a
            // fresh inference var the JIT can't classify. It is NOT
            // reference-counted (a handle, like the sockets), which
            // `is_rc_managed` already reports for unregistered sentinels.
            //
            // The sibling sync handles (`Mutex` / `WaitGroup` / `Atomic` /
            // `I64Vec`) are deliberately NOT registered: their methods
            // dispatch by name on every tier (`gos_rt_wg_done`, etc.), and
            // forcing a concrete receiver type reroutes that dispatch and
            // breaks the compiled lowering. A fresh inference var keeps them
            // on the working name-global path.
            "U8Vec" => Some(20),
            "Notifier" => Some(17),
            _ => None,
        };
        if let Some(off) = stdlib_def_offset {
            let def = gossamer_resolve::DefId::local(u32::MAX - off);
            match tail {
                "Context" => self.tcx.register_def_name(def, "context::Context"),
                "U8Vec" => self.tcx.register_def_name(def, tail),
                "Notifier" => self.tcx.register_def_name(def, tail),
                // The qualified name the constructor path registers, so a
                // written annotation and a constructed value name one type
                // and the method tables recognise both.
                "Pattern" => self.tcx.register_def_name(def, "regex::Pattern"),
                _ => {}
            }
            return self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            });
        }
        // `validate::Errors` / `validate::FieldError` are opaque i64
        // handles with no dedicated `TyKind`. Resolving the annotation to
        // a named sentinel Adt (instead of a fresh inference var) lets
        // method dispatch recover the handle kind from its *type* when an
        // `Errors` / `FieldError` flows across a function boundary and the
        // construction-site tag is gone - the same recovery `HashSet`
        // and `BTreeMap` rely on.
        let validate_handle: Option<(u32, &str)> = match tail {
            "Errors" => Some((VALIDATE_ERRORS_DEF_LOCAL, "Errors")),
            "FieldError" => Some((VALIDATE_FIELD_ERROR_DEF_LOCAL, "FieldError")),
            _ => None,
        };
        if let Some((local, name)) = validate_handle {
            let def = gossamer_resolve::DefId::local(local);
            self.tcx.register_def_name(def, name);
            return self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            });
        }
        // `net::TcpStream` / `TcpListener` / `UdpSocket` / `UnixStream` /
        // `UnixListener` are opaque i64 socket handles with no dedicated
        // `TyKind`. Resolving the annotation to a named sentinel Adt
        // (instead of a fresh inference var) lets method dispatch recover
        // the handle kind from its *type* when a socket flows through a
        // struct field or a parameter and the construction-site tag is
        // gone - without this `conn.sock.read(..)` lowers to an undefined
        // name-global symbol on the compiled tiers.
        if let Some((off, name)) = stdlib_net_handle(tail) {
            let def = gossamer_resolve::DefId::local(u32::MAX - off);
            self.tcx.register_def_name(def, name);
            return self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            });
        }
        // A type written inside an associated-type binding (`T: Holder<Item
        // = Point>`) sits outside every path the resolver walks, so its
        // node carries no resolution. A user type named there still has to
        // land on its nominal Adt rather than a fresh variable.
        if let Some(def) = self.adt_def_by_name.get(tail).copied() {
            let substs = self.substs_from_ast(path);
            return self.tcx.intern(TyKind::Adt { def, substs });
        }
        // Every other opaque runtime handle, named where no constructor
        // types the slot: a parameter, a field, a return type.
        let written: Vec<&str> = path
            .segments
            .iter()
            .map(|seg| seg.name.name.as_str())
            .collect();
        if let Some((offset, name)) = stdlib_handle_by_path(&written) {
            return self.stdlib_handle_ty(offset, name);
        }
        self.fresh()
    }

    /// `traits` followed by every supertrait reachable from them, in
    /// breadth-first order. A projection resolves at check time rather
    /// than through a vtable, so an associated item a supertrait declares
    /// is reachable through the subtrait that inherits it.
    fn with_supertraits(&self, traits: Vec<String>) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = traits.iter().cloned().collect();
        let mut out = traits;
        let mut next = 0;
        while next < out.len() {
            let current = out[next].clone();
            next += 1;
            for supertrait in self.trait_supertraits.get(&current).into_iter().flatten() {
                if seen.insert(supertrait.clone()) {
                    out.push(supertrait.clone());
                }
            }
        }
        out
    }

    /// Trait names bounding the generic parameter `name` in the scope
    /// currently being checked, from its inline bounds and `where`
    /// predicates alike.
    fn bounds_of_param(&self, name: &str) -> Vec<String> {
        let Some((idx, _)) = self.current_generic_scope.get(name) else {
            return Vec::new();
        };
        self.current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default()
    }

    /// Resolves a two-segment type path that projects an associated type
    /// (`T::Item`, `Self::Item`, `Point::Item`). Returns `None` when the
    /// path is not a projection at all, so ordinary qualified type paths
    /// keep their existing handling.
    ///
    /// A projection resolves, in order, through an equality constraint on
    /// the base's bound, through the impl that supplies it for a concrete
    /// base, and through the bounding trait's default or its single
    /// implementor. Everything reachable here is concrete, so the recorded
    /// type never carries a projection into HIR.
    fn resolve_assoc_type_projection(&mut self, path: &TypePath, span: Span) -> Option<Ty> {
        // The last two segments carry the projection, so a module-qualified
        // `inner::Holder::Item` reads the same as a bare `Holder::Item`.
        let count = path.segments.len();
        let base = path.segments.get(count.checked_sub(2)?)?.name.name.clone();
        let name = path.segments.last()?.name.name.clone();
        let is_param = self.current_generic_scope.contains_key(&base);
        let is_self = base == "Self";
        if !is_param
            && !is_self
            && self.assoc.assoc_type_for_self(&base, &name).is_none()
            && self.assoc.assoc_const_ty_for_self(&base, &name).is_none()
        {
            return None;
        }
        if !self.assoc_expanding.insert((base.clone(), name.clone())) {
            self.emit(
                TypeError::CyclicTypeAlias {
                    name: format!("{base}::{name}"),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        let resolved =
            self.resolve_assoc_type_projection_inner(&base, &name, is_param, is_self, span, true);
        self.assoc_expanding.remove(&(base, name));
        Some(resolved)
    }

    /// Body of [`Self::resolve_assoc_type_projection`], also reached from a
    /// method call whose receiver pins the projection. `report` is false
    /// when the same projection was already diagnosed at its declaration.
    fn resolve_assoc_type_projection_inner(
        &mut self,
        base: &str,
        name: &str,
        is_param: bool,
        is_self: bool,
        span: Span,
        report: bool,
    ) -> Ty {
        if let Some(bound_ty) = self
            .current_assoc_bindings
            .get(&(base.to_string(), name.to_string()))
            .cloned()
        {
            return self.type_from_ast(&bound_ty);
        }
        let concrete_base = if is_self {
            self.current_self_ty_name.clone()
        } else if is_param {
            None
        } else {
            Some(base.to_string())
        };
        if let Some(concrete) = concrete_base.as_deref() {
            if let Some(ast_ty) = self.assoc.assoc_type_for_self(concrete, name).cloned() {
                return self.type_from_ast(&ast_ty);
            }
            if self.assoc.self_ty_trait_declares(concrete, name) {
                // The trait declares the item and this impl leaves it out.
                // GT0059 names the impl; a second report here would only
                // repeat it at every projection site.
                return self.tcx.error_ty();
            }
        }
        let traits: Vec<String> = if is_self {
            self.with_supertraits(self.current_trait_name.iter().cloned().collect())
        } else if is_param {
            self.with_supertraits(self.bounds_of_param(base))
        } else {
            Vec::new()
        };
        // Inside the trait declaration itself `Self` stands for every
        // implementor, so a projection that no single impl pins is not an
        // error: each specialisation resolves it against its own impl.
        let self_is_abstract = is_self && self.current_self_ty_name.is_none();
        let mut ambiguous_in = None;
        for trait_name in &traits {
            match self.assoc.assoc_type_for_trait(trait_name, name) {
                gossamer_ast::AssocResolution::Found(ast_ty) => {
                    let ast_ty = ast_ty.clone();
                    return self.type_from_ast(&ast_ty);
                }
                gossamer_ast::AssocResolution::Ambiguous => {
                    ambiguous_in = Some(trait_name.clone());
                }
                gossamer_ast::AssocResolution::Unknown => {}
            }
        }
        if let Some(trait_name) = ambiguous_in {
            if self_is_abstract || !report {
                return self.tcx.error_ty();
            }
            self.emit(
                TypeError::AmbiguousAssocItem {
                    base: base.to_string(),
                    trait_name,
                    name: name.to_string(),
                    kind: "type",
                },
                span,
            );
            return self.tcx.error_ty();
        }
        if self_is_abstract
            && traits
                .iter()
                .any(|t| self.assoc.trait_declares_type(t, name))
        {
            return self.tcx.error_ty();
        }
        if !report {
            return self.tcx.error_ty();
        }
        let declared = traits
            .iter()
            .flat_map(|t| self.assoc.declared_assoc_names(t))
            .map(ToString::to_string)
            .collect();
        self.emit(
            TypeError::UnknownAssocItem {
                base: base.to_string(),
                name: name.to_string(),
                declared,
            },
            span,
        );
        self.tcx.error_ty()
    }

    /// Types a two-segment path expression that reads an associated
    /// constant (`Point::MAX`, `T::MAX`, `Self::MAX`). Returns `None` when
    /// the path names no associated constant, leaving ordinary path
    /// resolution in charge.
    fn check_assoc_const_path(&mut self, path: &gossamer_ast::PathExpr, span: Span) -> Option<Ty> {
        let count = path.segments.len();
        let base = path.segments.get(count.checked_sub(2)?)?.name.name.clone();
        let name = path.segments.last()?.name.name.clone();
        let is_param = self.current_generic_scope.contains_key(&base);
        let is_self = base == "Self";
        let concrete_base = if is_self {
            self.current_self_ty_name.clone()
        } else if is_param {
            None
        } else {
            Some(base.clone())
        };
        if let Some(concrete) = concrete_base.as_deref()
            && let Some(ast_ty) = self.assoc.assoc_const_ty_for_self(concrete, &name).cloned()
        {
            return Some(self.type_from_ast(&ast_ty));
        }
        let traits: Vec<String> = if is_self {
            self.with_supertraits(self.current_trait_name.iter().cloned().collect())
        } else if is_param {
            self.with_supertraits(self.bounds_of_param(&base))
        } else {
            return None;
        };
        let declaring: Vec<&String> = traits
            .iter()
            .filter(|t| self.assoc.trait_declares_const(t, &name))
            .collect();
        let trait_name = declaring.first().map(|t| (*t).clone())?;
        match self.assoc.assoc_const_owner_for_trait(&trait_name, &name) {
            gossamer_ast::AssocResolution::Found(owner) => {
                let ast_ty = self.assoc.assoc_const_ty_for_self(&owner, &name).cloned();
                match ast_ty {
                    Some(ast_ty) => Some(self.type_from_ast(&ast_ty)),
                    None => Some(self.tcx.error_ty()),
                }
            }
            gossamer_ast::AssocResolution::Ambiguous => {
                self.emit(
                    TypeError::AmbiguousAssocItem {
                        base,
                        trait_name,
                        name,
                        kind: "const",
                    },
                    span,
                );
                Some(self.tcx.error_ty())
            }
            gossamer_ast::AssocResolution::Unknown => None,
        }
    }

    fn substs_from_ast(&mut self, path: &TypePath) -> crate::Substs {
        let mut args = Vec::new();
        for segment in &path.segments {
            for arg in &segment.generics {
                match arg {
                    AstGenericArg::Type(ast_ty) => {
                        args.push(crate::GenericArg::Type(self.type_from_ast(ast_ty)));
                    }
                    AstGenericArg::Const(expr) => {
                        let value = self.evaluate_generic_const_arg(expr);
                        args.push(crate::GenericArg::Const(value));
                    }
                }
            }
        }
        crate::Substs::from_args(args)
    }

    fn is_integer(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), Some(TyKind::Int(_)))
    }

    /// Types an array-length expression. A literal yields a concrete
    /// count; a bare path naming a const generic parameter in scope
    /// yields a symbolic `Param` length linked to that parameter's
    /// position; anything else is rejected and uses `0` only as an error
    /// recovery placeholder.
    fn array_len_from_ast(&mut self, expr: &Expr) -> crate::ArrayLen {
        if let Some(len) = self.evaluate_array_len(expr) {
            return crate::ArrayLen::Concrete(len);
        }
        if let ExprKind::Path(path) = &expr.kind
            && path.segments.len() == 1
            && let Some(seg) = path.segments.first()
            && let Some(idx) = self
                .current_const_generic_scope
                .get(&seg.name.name)
                .copied()
        {
            return crate::ArrayLen::Param(idx);
        }
        self.emit(TypeError::ArrayLengthNotConstant, expr.span);
        crate::ArrayLen::Concrete(0)
    }

    /// Evaluates an array-length expression to a `usize`, emitting a
    /// diagnostic when the literal magnitude exceeds `usize::MAX`.
    /// Returns `None` for non-literal forms.
    fn evaluate_array_len(&mut self, expr: &Expr) -> Option<usize> {
        let raw = evaluate_const_int_from_expr(expr).or_else(|| self.const_int_of_path(expr))?;
        if raw > usize::MAX as u128 {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: format!("{raw}"),
                    ty: "usize".to_string(),
                },
                expr.span,
            );
            return None;
        }
        Some(raw as usize)
    }

    /// Evaluates a `const` generic argument to an `i128`, emitting a
    /// diagnostic when the literal magnitude does not fit. Returns
    /// `0` on overflow so the surrounding `Substs` stays well-formed.
    fn evaluate_generic_const_arg(&mut self, expr: &Expr) -> i128 {
        let Some(raw) = evaluate_const_int_from_expr(expr) else {
            return 0;
        };
        if let Ok(value) = i128::try_from(raw) {
            value
        } else {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: format!("{raw}"),
                    ty: "i128".to_string(),
                },
                expr.span,
            );
            0
        }
    }

    /// The built-in `Result` / `Option` constructor named by an
    /// unqualified pattern path (`Ok` / `Err` / `Some` / `None`), or
    /// `None` for a qualified path or any other name. Qualified
    /// variants (`MyEnum::Ok`) keep their user typing - only the bare
    /// spelling is the reserved built-in.
    fn bare_result_option_ctor(path: &gossamer_ast::Path) -> Option<&'static str> {
        if path.segments.len() != 1 {
            return None;
        }
        match path.segments.last()?.name.name.as_str() {
            "Ok" => Some("Ok"),
            "Err" => Some("Err"),
            "Some" => Some("Some"),
            "None" => Some("None"),
            _ => None,
        }
    }

    fn type_of_pattern(&mut self, pattern: &Pattern) -> Ty {
        if self.enter_recursion(pattern.span).is_err() {
            return self.tcx.error_ty();
        }
        let ty = self.type_of_pattern_kind(pattern);
        self.leave_recursion();
        ty
    }

    fn type_of_pattern_kind(&mut self, pattern: &Pattern) -> Ty {
        // A constructor pattern's type stays a fresh inference variable
        // here; the scrutinee unification binds it. Synthesizing the
        // `Result` / `Option` Adt at this site desugars `if let Some(n)
        // = m.get(k)` differently and miscompiles the payload binding,
        // so the bare `Ok` / `Err` / `Some` / `None` mismatch is caught
        // separately in `reject_constructor_scrutinee_mismatch`.
        match &pattern.kind {
            PatternKind::Wildcard
            | PatternKind::Ident { .. }
            | PatternKind::Path(_)
            | PatternKind::Struct { .. }
            | PatternKind::TupleStruct { .. }
            | PatternKind::Slice { .. }
            | PatternKind::Rest => self.fresh(),
            PatternKind::Error => self.tcx.error_ty(),
            PatternKind::Literal(lit) => self.type_of_literal(lit, pattern.span),
            // A `..` rest leaves the width open, so the tuple's type comes
            // from the scrutinee rather than from the pattern's arity.
            PatternKind::Tuple(parts)
                if parts
                    .iter()
                    .any(|part| matches!(part.kind, PatternKind::Rest)) =>
            {
                self.fresh()
            }
            PatternKind::Tuple(parts) => {
                let tys: Vec<Ty> = parts.iter().map(|p| self.type_of_pattern(p)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            PatternKind::Range { lo, hi, .. } => match lo.as_ref().or(hi.as_ref()) {
                Some(bound) => self.type_of_literal(bound, pattern.span),
                None => self.fresh(),
            },
            PatternKind::Or(alts) => match alts.first() {
                Some(first) => self.type_of_pattern(first),
                None => self.fresh(),
            },
            PatternKind::Ref { inner, mutability } => {
                let inner_ty = self.type_of_pattern(inner);
                let mutability = match mutability {
                    gossamer_ast::Mutability::Immutable => Mutbl::Not,
                    gossamer_ast::Mutability::Mutable => Mutbl::Mut,
                };
                self.tcx.intern(TyKind::Ref {
                    mutability,
                    inner: inner_ty,
                })
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, ty: Ty) {
        self.binding_types.insert(pattern.id, ty);
        self.table.insert(pattern.id, ty);
        match &pattern.kind {
            PatternKind::Ident {
                name,
                subpattern,
                mutability,
            } => {
                self.bind_local(&name.name, ty);
                self.bind_local_mutability(&name.name, mutability.is_mutable());
                if let Some(subpattern) = subpattern {
                    self.bind_pattern(subpattern, ty);
                }
            }
            PatternKind::Tuple(parts) => {
                self.bind_tuple_pattern(pattern, parts, ty);
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let resolved = self.infer.resolve(self.tcx, ty);
                let elem_ty = match self.tcx.kind(resolved).cloned() {
                    Some(TyKind::Vec(e) | TyKind::Slice(e) | TyKind::Array { elem: e, .. }) => e,
                    _ => self.fresh(),
                };
                for part in prefix {
                    self.bind_pattern(part, elem_ty);
                }
                if let Some(rest) = rest {
                    let rest_ty = self.tcx.intern(TyKind::Vec(elem_ty));
                    self.bind_pattern(rest, rest_ty);
                }
                for part in suffix {
                    self.bind_pattern(part, elem_ty);
                }
            }
            PatternKind::Struct { path, fields, .. } => {
                self.bind_struct_pattern(path, fields, ty);
            }
            PatternKind::TupleStruct { path, elems } => {
                // For built-in generic enums (`Option<T>`,
                // `Result<T, E>`) pull the payload type out of the
                // scrutinee's substs and bind it to the matching
                // pattern element. Without this, `Some(x)` would
                // bind `x` to a fresh inference variable that no
                // later use forces back to `T`, leaving downstream
                // code looking at an unresolved `Var`. Falls back
                // to a fresh var for any other variant constructor.
                let payload_tys = self.payload_types_for_variant(path, ty);
                for (i, elem) in elems.iter().enumerate() {
                    let elem_ty = payload_tys
                        .as_ref()
                        .and_then(|tys| tys.get(i).copied())
                        .unwrap_or_else(|| self.fresh());
                    self.bind_pattern(elem, elem_ty);
                }
            }
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.bind_pattern(alt, ty);
                }
                // Rust requires every alternative to bind a name with the
                // same mode. Until that receives its own diagnostic, use the
                // strict capability intersection: one immutable occurrence
                // keeps the resulting binding immutable regardless of order.
                let mut mutability = HashMap::new();
                for alt in alts {
                    Self::collect_pattern_binding_mutability(alt, &mut mutability);
                }
                for (name, mutable) in mutability {
                    self.bind_local_mutability(&name, mutable);
                }
            }
            PatternKind::Ref { inner, mutability } => {
                let resolved = self.infer.resolve(self.tcx, ty);
                let inner_ty = match self.tcx.kind(resolved).cloned() {
                    Some(TyKind::Ref { inner, .. }) => self.infer.resolve(self.tcx, inner),
                    // The pattern is what says the value is a reference, so
                    // an as-yet-unsolved type takes that shape from it.
                    Some(TyKind::Var(_)) => {
                        let referent = self.fresh();
                        let ref_ty = self.tcx.intern(TyKind::Ref {
                            mutability: if mutability.is_mutable() {
                                Mutbl::Mut
                            } else {
                                Mutbl::Not
                            },
                            inner: referent,
                        });
                        self.unify(resolved, ref_ty, pattern.span);
                        referent
                    }
                    _ => self.tcx.error_ty(),
                };
                self.bind_pattern(inner, inner_ty);
            }
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Path(_)
            | PatternKind::Range { .. }
            | PatternKind::Rest
            | PatternKind::Error => {}
        }
    }

    fn bind_tuple_pattern(&mut self, pattern: &Pattern, parts: &[Pattern], ty: Ty) {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        let mut ref_mutability = None;
        if let Some(TyKind::Ref { inner, mutability }) = self.tcx.kind(resolved).cloned() {
            resolved = self.infer.resolve(self.tcx, inner);
            ref_mutability = Some(mutability);
        }
        if matches!(self.tcx.kind(resolved), Some(TyKind::Adt { .. })) {
            let ty = self.render_public_ty(resolved);
            self.emit(TypeError::StructPatternNameRequired { ty }, pattern.span);
        }
        let element_tys = self.tuple_pattern_element_tys(resolved, ref_mutability, parts.len());
        // A `..` rest spans the elements the written prefix and suffix leave
        // between them, so the suffix binds from the end of the tuple.
        let rest_at = parts
            .iter()
            .position(|part| matches!(part.kind, PatternKind::Rest));
        for (i, part) in parts.iter().enumerate() {
            let position = match rest_at {
                Some(at) if i > at => element_tys.len() + i - parts.len(),
                _ => i,
            };
            let elem_ty = element_tys
                .get(position)
                .copied()
                .unwrap_or_else(|| self.fresh());
            self.bind_pattern(part, elem_ty);
        }
    }

    fn tuple_pattern_element_tys(
        &mut self,
        resolved: Ty,
        ref_mutability: Option<Mutbl>,
        count: usize,
    ) -> Vec<Ty> {
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Tuple(elems)) => match ref_mutability {
                Some(mutability) => elems
                    .into_iter()
                    .map(|inner| self.tcx.intern(TyKind::Ref { mutability, inner }))
                    .collect(),
                None => elems,
            },
            // A sequence taken apart positionally binds each part to the
            // element type, exactly as a slice pattern does. Every part
            // otherwise takes a fresh variable that no later use resolves, so
            // the binding reaches codegen with no type to dispatch a method
            // against.
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                let elem = match ref_mutability {
                    Some(mutability) => self.tcx.intern(TyKind::Ref {
                        mutability,
                        inner: elem,
                    }),
                    None => elem,
                };
                vec![elem; count]
            }
            _ => (0..count).map(|_| self.fresh()).collect(),
        }
    }

    fn bind_struct_pattern(&mut self, path: &gossamer_ast::Path, fields: &[FieldPattern], ty: Ty) {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        let mut ref_mutability = None;
        while let Some(TyKind::Ref { inner, mutability }) = self.tcx.kind(resolved).cloned() {
            resolved = self.infer.resolve(self.tcx, inner);
            ref_mutability = Some(mutability);
        }
        // A struct-variant pattern reads the enum's declared variant fields;
        // any other struct pattern reads the struct's own fields.
        let variant_fields = self.struct_variant_field_tys(path, resolved);
        let declared = variant_fields.unwrap_or_else(|| match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Adt { def, substs }) => {
                let names = self.struct_fields.get(&def).cloned().unwrap_or_default();
                let tys = self.tcx.adt_field_tys(def, &substs).unwrap_or_default();
                names
                    .into_iter()
                    .zip(tys.iter().copied())
                    .map(|((name, _), ty)| (name, ty))
                    .collect::<HashMap<_, _>>()
            }
            _ => HashMap::new(),
        });
        for field in fields {
            let mut field_ty = declared
                .get(&field.name.name)
                .copied()
                .unwrap_or_else(|| self.fresh());
            if let Some(mutability) = ref_mutability {
                field_ty = self.reference_binding_ty(field_ty, mutability);
            }
            self.bind_field_pattern(field, field_ty);
        }
    }

    /// Declared field types of the enum struct-variant `path` names on the
    /// `resolved` scrutinee enum, or `None` when the pattern is a plain
    /// struct pattern. Generic enums are excluded: their stored field types
    /// mention un-substituted parameters.
    fn struct_variant_field_tys(
        &mut self,
        path: &gossamer_ast::Path,
        resolved: Ty,
    ) -> Option<HashMap<String, Ty>> {
        let TyKind::Adt { def, substs } = self.tcx.kind(resolved)? else {
            return None;
        };
        if !substs.types().is_empty() {
            return None;
        }
        let enum_name = self.tcx.def_name(*def)?.to_string();
        let variant = path.segments.last()?.name.name.clone();
        let fields = self
            .enum_variant_named_payloads
            .get(&(enum_name, variant))?
            .clone();
        Some(fields.into_iter().collect())
    }

    /// Type a payload field binds at when it is reached through a reference
    /// scrutinee. A scalar copies through the borrow and binds by value; a
    /// heap-shaped payload binds as a borrow so the referent stays live.
    fn reference_binding_ty(&mut self, inner: Ty, mutability: Mutbl) -> Ty {
        let scalar = matches!(
            self.tcx.kind(inner),
            Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
        );
        if scalar {
            inner
        } else {
            self.tcx.intern(TyKind::Ref { inner, mutability })
        }
    }

    fn bind_field_pattern(&mut self, field: &FieldPattern, ty: Ty) {
        if let Some(pattern) = &field.pattern {
            self.bind_pattern(pattern, ty);
        } else {
            self.bind_local(&field.name.name, ty);
            self.bind_local_mutability(&field.name.name, false);
        }
    }

    fn collect_pattern_binding_mutability(pattern: &Pattern, out: &mut HashMap<String, bool>) {
        match &pattern.kind {
            PatternKind::Ident {
                name,
                subpattern,
                mutability,
            } => {
                out.entry(name.name.clone())
                    .and_modify(|current| *current &= mutability.is_mutable())
                    .or_insert_with(|| mutability.is_mutable());
                if let Some(subpattern) = subpattern {
                    Self::collect_pattern_binding_mutability(subpattern, out);
                }
            }
            PatternKind::Tuple(parts) | PatternKind::Or(parts) => {
                for part in parts {
                    Self::collect_pattern_binding_mutability(part, out);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for part in prefix {
                    Self::collect_pattern_binding_mutability(part, out);
                }
                if let Some(rest) = rest {
                    Self::collect_pattern_binding_mutability(rest, out);
                }
                for part in suffix {
                    Self::collect_pattern_binding_mutability(part, out);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for field in fields {
                    if let Some(pattern) = &field.pattern {
                        Self::collect_pattern_binding_mutability(pattern, out);
                    } else {
                        out.entry(field.name.name.clone())
                            .and_modify(|current| *current = false)
                            .or_insert(false);
                    }
                }
            }
            PatternKind::TupleStruct { elems, .. } => {
                for elem in elems {
                    Self::collect_pattern_binding_mutability(elem, out);
                }
            }
            PatternKind::Ref { inner, .. } => {
                Self::collect_pattern_binding_mutability(inner, out);
            }
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Path(_)
            | PatternKind::Range { .. }
            | PatternKind::Rest
            | PatternKind::Error => {}
        }
    }

    /// Returns the payload tuple element types for a tuple-struct
    /// pattern when the scrutinee is `Option<T>` or `Result<T, E>`.
    /// Returns `None` for any other shape (user enums, unknown
    /// substs); callers fall back to fresh inference variables.
    fn payload_types_for_variant(
        &mut self,
        path: &gossamer_ast::Path,
        scrutinee_ty: Ty,
    ) -> Option<Vec<Ty>> {
        let mut resolved = self.infer.resolve(self.tcx, scrutinee_ty);
        let mut ref_mutability = None;
        while let Some(TyKind::Ref { inner, mutability }) = self.tcx.kind(resolved) {
            ref_mutability = Some(*mutability);
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let TyKind::Adt { def, substs } = self.tcx.kind(resolved)? else {
            return None;
        };
        let last = path.segments.last()?.name.name.as_str();
        let args: Vec<Ty> = substs.types();
        match (last, args.as_slice()) {
            ("Some", [t]) => Some(vec![*t]),
            ("Ok", [t, _]) => Some(vec![*t]),
            ("Err", [_, e]) => Some(vec![*e]),
            _ => {
                // User enums: bind the declared tuple-variant payload
                // types, keyed by the scrutinee's own resolved enum name
                // so a same-named variant from another enum cannot unify
                // into this match. A generic enum's declared payloads carry
                // `Param` slots; the scrutinee's own arguments are what they
                // stand for here, so `Tree<i64>`'s `Leaf(v)` binds `v: i64`.
                let enum_name = self.tcx.def_name(*def)?;
                let tys = self
                    .enum_variant_payloads
                    .get(&(enum_name.to_string(), last.to_string()))
                    .cloned()?;
                let tys: Vec<Ty> = if args.is_empty() {
                    tys
                } else {
                    tys.iter()
                        .map(|t| self.subst_params_in_ty(*t, &args))
                        .collect()
                };
                // Match ergonomics: through a reference scrutinee a
                // heap-shaped payload binds as a borrow (a cursor walk's
                // `cursor = rest` with `cursor: &List` stays typed), while
                // a scalar payload copies through the borrow and binds by
                // value (`Tree::Node(v, ..) => v + 1`), matching how the
                // lowering loads payload words.
                Some(match ref_mutability {
                    Some(mutability) => tys
                        .into_iter()
                        .map(|inner| {
                            let scalar = matches!(
                                self.tcx.kind(inner),
                                Some(
                                    TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                                )
                            );
                            if scalar {
                                inner
                            } else {
                                self.tcx.intern(TyKind::Ref { inner, mutability })
                            }
                        })
                        .collect(),
                    None => tys,
                })
            }
        }
    }
}

/// True when a type is too unresolved or opaque to soundly reject a
/// structural use (`value[i]`, `value.N`) against it. Hard errors are
/// emitted only for concrete, fully-known types; an inference variable,
/// already-errored type, generic parameter, unresolved alias, or trait
/// object fails soft so inference, generics, and name-resolved stdlib
/// paths are never falsely rejected.
fn is_soft_for_structural_use(kind: &TyKind) -> bool {
    matches!(
        kind,
        TyKind::Var(_)
            | TyKind::Error
            | TyKind::Param { .. }
            | TyKind::Alias { .. }
            | TyKind::Dyn(_)
    )
}

/// True when a type is a concrete runtime value that is provably not a
/// function and not an ADT constructor, so `value(args)` on it can never
/// resolve to a callable on any tier. ADTs are deliberately excluded:
/// `Some(x)` / `Ok(x)` / `MyEnum::Variant(x)` type their callee as an
/// `Option` / `Result` / enum ADT, and rejecting those would break
/// constructor calls.
fn is_definitely_not_callable_value(kind: &TyKind) -> bool {
    matches!(
        kind,
        TyKind::Bool
            | TyKind::Char
            | TyKind::String
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Unit
            | TyKind::Tuple(_)
            | TyKind::Array { .. }
            | TyKind::Slice(_)
            | TyKind::Vec(_)
            | TyKind::HashMap { .. }
            | TyKind::Sender(_)
            | TyKind::Receiver(_)
            | TyKind::JoinHandle(_)
            | TyKind::Duration
            | TyKind::Instant
            | TyKind::JsonValue
            | TyKind::DynError
    )
}

fn is_channel_constructor_path(module: &[&str], last: &str) -> bool {
    matches!(
        (module, last),
        (["channel"], "new" | "unbounded")
            | (["sync"] | ["std", "sync"], "channel" | "channel_unbounded")
            | (["sync", "Channel"] | ["std", "sync", "Channel"], "new")
    )
}

/// Canonical std combinator module name for a call path's module
/// segments, or `None` when the path is not `result` / `option` /
/// `iter` (bare or `std::`-qualified).
/// Required shape of one `strings::` free-function parameter slot,
/// used to make `check` reject a non-string argument that the
/// compiled string shims would otherwise dereference as a string
/// pointer.
#[derive(Clone, Copy)]
enum StrArgShape {
    /// Must be a `String` (a `&String` borrow peels to the same).
    Str,
    /// A pattern or pad slot: a `String` or a `char` (a single-char
    /// pattern coerces to its one-byte string on every tier).
    StrOrChar,
}

#[derive(Clone, Copy)]
struct StringParamMeta {
    name: &'static str,
    expected: &'static str,
}

/// String-typed parameter positions of the canonical `strings::` free
/// functions, keyed by function name. Only positions that must hold a
/// string-shaped value are listed; integer width / count positions are
/// left out so the several integer widths the runtime accepts there are
/// not rejected. Argument order mirrors the interpreter's free-function
/// table (`stdlib_builtins/strings.rs`), so e.g. `splitn(text, n, sep)`
/// has its separator at index 2. Returns `None` for an unlisted name.
fn strings_fn_str_params(name: &str) -> Option<&'static [(usize, StrArgShape)]> {
    use StrArgShape::{Str, StrOrChar};
    Some(match name {
        "split" | "contains" | "find" | "rfind" | "split_once" | "rsplit_once" | "count"
        | "starts_with" | "ends_with" | "strip_prefix" | "strip_suffix" | "contains_any"
        | "find_any" | "rfind_any" | "trim_matches" | "trim_start_matches" | "trim_end_matches" => {
            &[(0, Str), (1, StrOrChar)]
        }
        "splitn" => &[(0, Str), (2, StrOrChar)],
        "replace" | "replacen" => &[(0, Str), (1, StrOrChar), (2, StrOrChar)],
        "equal_fold" => &[(0, Str), (1, StrOrChar)],
        "center" | "pad_left" | "pad_right" => &[(0, Str)],
        "split_whitespace" | "trim" | "trim_start" | "trim_end" | "to_lowercase"
        | "to_uppercase" | "to_title" | "lines" | "repeat" | "slice" | "substring" | "byte_at"
        | "byte_len" => &[(0, Str)],
        _ => return None,
    })
}

/// Source parameter metadata for string diagnostics. The stdlib signature
/// catalogue is the source of truth used by `%help`, so diagnostics must use
/// the same parameter names and expected type text.
fn strings_fn_param_metadata(name: &str, position: usize, shape: StrArgShape) -> StringParamMeta {
    if let Some(signature) = crate::stdlib_signatures::function("std::strings", name)
        && let Some(shape) = crate::stdlib_signatures::parse_signature(signature.signature)
        && let Some(param) = shape.params.get(position)
    {
        return StringParamMeta {
            name: param.name,
            expected: param.ty,
        };
    }
    let (name, expected) = match (name, position, shape) {
        (_, 0, _) => ("text", "String"),
        ("replace" | "replacen", 1, _) => ("from", "String | char"),
        ("replace" | "replacen", 2, _) => ("to", "String | char"),
        (_, _, StrArgShape::Str) => ("value", "String"),
        (_, _, StrArgShape::StrOrChar) => ("needle", "String | char"),
    };
    StringParamMeta { name, expected }
}

/// Concise source-like text for an invalid argument. Literals retain their
/// exact value, while paths remain useful without requiring the source map.
/// Source-shaped spelling of an expression, for a diagnostic that shows
/// the rewrite in the reader's own terms. `None` for a shape with no short
/// spelling, which leaves the diagnostic on its generic `<expr>` wording.
fn expr_display(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Path(_) => match argument_value_display(expr).as_str() {
            "<expression>" => None,
            text => Some(text.to_string()),
        },
        ExprKind::Call { callee, args } => {
            let callee = expr_display(callee)?;
            let args = expr_display_list(args)?;
            Some(format!("{callee}({args})"))
        }
        ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } => {
            let receiver = expr_display(receiver)?;
            let args = expr_display_list(args)?;
            Some(format!("{receiver}.{}({args})", name.name))
        }
        ExprKind::FieldAccess {
            receiver,
            field: gossamer_ast::FieldSelector::Named(name),
        } => Some(format!("{}.{}", expr_display(receiver)?, name.name)),
        _ => None,
    }
}

/// Comma-joined spellings of an argument list, or `None` when any argument
/// has no short spelling.
fn expr_display_list(args: &[Expr]) -> Option<String> {
    let rendered = args.iter().map(expr_display).collect::<Option<Vec<_>>>()?;
    Some(rendered.join(", "))
}

/// Spelling of an operand for a cast suggestion, falling back to the
/// `<expr>` placeholder the other help lines use.
fn operand_display(expr: &Expr) -> String {
    expr_display(expr).unwrap_or_else(|| "<expr>".to_string())
}

/// Spelling of a `T` value to seed `unwrap_or` with for the scalar types
/// that have an obvious zero, or the `<default>` placeholder otherwise.
fn default_value_spelling(kind: Option<&TyKind>) -> String {
    match kind {
        Some(TyKind::Int(_)) => "0".to_string(),
        Some(TyKind::Float(_)) => "0.0".to_string(),
        Some(TyKind::Bool) => "false".to_string(),
        Some(TyKind::String) => "\"\"".to_string(),
        _ => "<default>".to_string(),
    }
}

/// Span of the expression a function body evaluates to. A block yields
/// its tail expression, so a diagnostic about the produced value points at
/// that expression rather than at the enclosing braces.
fn body_value_span(body: &Expr) -> Span {
    match &body.kind {
        ExprKind::Block(block) => block
            .tail
            .as_ref()
            .map_or(body.span, |tail| body_value_span(tail)),
        _ => body.span,
    }
}

/// Collects, from one function body, the span of every `cohort { }` block and
/// the span of every prelude `spawn(...)` call.
///
/// A nested `fn` item carries its own body and its own rule, so the walk stops
/// at one. A closure is not a boundary: it runs where it is written, so a
/// spawn inside a closure inside a cohort block is inside that block.
#[derive(Default)]
struct SpawnScopeScan {
    cohorts: Vec<Span>,
    spawns: Vec<Span>,
}

impl gossamer_ast::visitor::Visitor for SpawnScopeScan {
    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Block(block) if block.kind == gossamer_ast::BlockKind::Cohort => {
                self.cohorts.push(expr.span);
            }
            // The prelude `spawn`, not a module's own (`exec::spawn`).
            ExprKind::Call { callee, .. } => {
                if let ExprKind::Path(path) = &callee.kind
                    && let [seg] = path.segments.as_slice()
                    && seg.name.name == "spawn"
                {
                    self.spawns.push(expr.span);
                }
            }
            _ => {}
        }
        gossamer_ast::visitor::walk_expr(self, expr);
    }

    fn visit_item(&mut self, _item: &gossamer_ast::Item) {}
}

/// True for a name the compiler synthesized rather than the user wrote.
/// The autoderive pass prefixes every helper it splices with `__`.
fn is_compiler_generated(name: &str) -> bool {
    name.starts_with("__")
}

/// Span of the field name inside a named field access. A named access
/// ends at its field name, so the trailing bytes of the access span cover
/// the name exactly.
fn field_name_span(access: &Expr, field: &str) -> Option<Span> {
    let len = u32::try_from(field.len()).ok()?;
    if access.span.len() <= len {
        return None;
    }
    Some(Span::new(
        access.span.file,
        access.span.end - len,
        access.span.end,
    ))
}

/// Records where a field name sits so the GT0006 rename suggestion can
/// replace that name alone.
fn with_field_span(error: TypeError, span: Option<Span>) -> TypeError {
    match error {
        TypeError::UnknownField {
            ty,
            field,
            opaque,
            declared,
            method_of_same_name,
            ..
        } => TypeError::UnknownField {
            ty,
            field,
            opaque,
            declared,
            field_span: span,
            method_of_same_name,
        },
        other => other,
    }
}

fn argument_value_display(arg: &Expr) -> String {
    match &arg.kind {
        ExprKind::Array(ArrayExpr::List(values)) => array_value_display("#[", "]", values),
        ExprKind::Array(ArrayExpr::Repeat { value, count }) => {
            format!(
                "#[{}; {}]",
                argument_value_display(value),
                argument_value_display(count)
            )
        }
        ExprKind::FixedArray(ArrayExpr::List(values)) => array_value_display("[", "]", values),
        ExprKind::FixedArray(ArrayExpr::Repeat { value, count }) => {
            format!(
                "[{}; {}]",
                argument_value_display(value),
                argument_value_display(count)
            )
        }
        ExprKind::Literal(Literal::Int(value) | Literal::Float(value)) => value.clone(),
        ExprKind::Literal(Literal::String(value)) => format!("{value:?}"),
        ExprKind::Literal(Literal::Char(value)) => format!("{value:?}"),
        ExprKind::Literal(Literal::Bool(value)) => value.to_string(),
        ExprKind::Literal(Literal::Unit) => "()".to_string(),
        ExprKind::Path(path) => path
            .segments
            .iter()
            .map(|segment| segment.name.name.as_str())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "<expression>".to_string(),
    }
}

fn array_value_display(prefix: &str, suffix: &str, values: &[Expr]) -> String {
    format!(
        "{prefix}{}{suffix}",
        values
            .iter()
            .map(argument_value_display)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Renders container-like invalid arguments without leaking unresolved
/// inference variables such as `[?1; 3]` into diagnostics. The expression
/// shape is more useful here than a partially inferred element type.
fn string_argument_found_type(arg: &Expr, tcx: &TyCtxt, ty: Ty) -> String {
    match &arg.kind {
        ExprKind::Array(_) => "Vec".to_string(),
        ExprKind::FixedArray(_) => "array".to_string(),
        ExprKind::MapLiteral(_) => "map literal".to_string(),
        ExprKind::SetLiteral(_) => "set literal".to_string(),
        ExprKind::Range { .. } => "range".to_string(),
        ExprKind::Tuple(_) => "tuple".to_string(),
        _ => match tcx.kind(ty) {
            Some(TyKind::Array { .. }) => "array".to_string(),
            Some(TyKind::Vec(_)) => "Vec".to_string(),
            Some(TyKind::Tuple(_)) => "tuple".to_string(),
            _ => render_ty(tcx, ty),
        },
    }
}

/// Full fixed arity of each string operation.  These are source-level arities
/// (including the string receiver / first free-function argument), so callers
/// subtract one for method syntax.  Keeping the table complete prevents an
/// omitted argument from silently becoming an empty pattern or zero index in
/// the VM implementation.
fn strings_fn_arity(name: &str) -> Option<usize> {
    Some(match name {
        "split" | "contains" | "find" | "rfind" | "split_once" | "rsplit_once" | "count"
        | "trim_start_matches" | "trim_end_matches" | "starts_with" | "ends_with"
        | "strip_prefix" | "strip_suffix" | "contains_any" | "find_any" | "rfind_any"
        | "equal_fold" | "trim_matches" => 2,
        "splitn" | "center" | "replace" | "pad_left" | "pad_right" | "slice" | "substring" => 3,
        "replacen" => 4,
        "split_whitespace" | "trim" | "trim_start" | "trim_end" | "to_lowercase"
        | "to_uppercase" | "to_title" | "to_i64" | "to_f64" | "to_bool" | "lines" | "chars"
        | "bytes" | "byte_len" => 1,
        "repeat" | "byte_at" | "index_rune" | "contains_rune" => 2,
        // `join(parts, sep)` is installed on `Vec` rather than `String`, but
        // it remains a `strings` free function and therefore belongs in the
        // same public arity catalogue.
        "join" => 2,
        _ => return None,
    })
}

/// `i64` parameter positions for the string-operation catalogue.  String and
/// pattern slots live in [`strings_fn_str_params`]; keeping numeric slots
/// separate preserves the accepted `String | char` pattern behaviour.
fn strings_fn_int_params(name: &str) -> &'static [usize] {
    match name {
        "splitn" | "center" | "pad_left" | "pad_right" | "repeat" | "byte_at" => &[1],
        "slice" | "substring" => &[1, 2],
        "replacen" => &[3],
        _ => &[],
    }
}

/// `char` parameter positions for the string-operation catalogue.
fn strings_fn_char_params(name: &str) -> &'static [usize] {
    match name {
        "center" | "pad_left" | "pad_right" => &[2],
        "index_rune" | "contains_rune" => &[1],
        _ => &[],
    }
}

fn combinator_module_name(module: &[&str]) -> Option<&'static str> {
    match module {
        ["result"] | ["std", "result"] => Some("result"),
        ["option"] | ["std", "option"] => Some("option"),
        ["iter"] | ["std", "iter"] => Some("iter"),
        _ => None,
    }
}

fn strip_catalog_wrapper<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    src.strip_prefix(name)?
        .strip_prefix('<')?
        .strip_suffix('>')
        .map(str::trim)
}

fn is_catalog_type_param(src: &str) -> bool {
    let mut chars = src.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ch.is_ascii_uppercase())
}

/// Pre-registers field types for stdlib structs that user source can
/// name (e.g. `fs::DirInfo`, `os::Output`, `http::Response`,
/// `http::ResponseStream`). The MIR-side dispatch pins free-call
/// destinations to sentinel `DefId`s (`u32::MAX - N`) for these
/// structs; without their field types registered here, `entry.path`
/// / `r.status` projections leave the result `Var(_)` and downstream
/// `.len()` / println fall back to the wrong dispatch.
fn register_stdlib_struct_fields(tcx: &mut TyCtxt) {
    let str_ty = tcx.string_ty();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let bool_ty = tcx.bool_ty();
    // DirInfo: [name, path, is_file, is_dir, is_symlink, size, modified_ms]
    tcx.register_struct_fields(
        gossamer_resolve::DefId::local(u32::MAX - 2),
        vec![str_ty, str_ty, bool_ty, bool_ty, bool_ty, i64_ty, i64_ty],
    );
    // Output: [stdout, stderr, code]
    tcx.register_struct_fields(
        gossamer_resolve::DefId::local(u32::MAX - 3),
        vec![str_ty, str_ty, i64_ty],
    );
    // ResponseStream: [__handle, status, content_type]
    tcx.register_struct_fields(
        gossamer_resolve::DefId::local(u32::MAX - 4),
        vec![i64_ty, i64_ty, str_ty],
    );
    // Response: [status, body, raw_bytes, content_type, location,
    // headers]. raw_bytes is Vec<u8>, headers is [(String, String)];
    // the per-name `gos_rt_http_response_*` helpers handle the actual
    // dispatch, so the field-list ordering matters only for
    // source-name lookup.
    let u8_ty = tcx.int_ty(IntTy::U8);
    let vec_u8 = tcx.intern(TyKind::Vec(u8_ty));
    let str_pair = tcx.intern(TyKind::Tuple(vec![str_ty, str_ty]));
    let vec_str_pair = tcx.intern(TyKind::Vec(str_pair));
    tcx.register_struct_fields(
        gossamer_resolve::DefId::local(u32::MAX - 5),
        vec![i64_ty, str_ty, vec_u8, str_ty, str_ty, vec_str_pair],
    );
}

/// Seeds the checker's own `struct_fields` map with the same stdlib
/// struct entries that `register_stdlib_struct_fields` puts in `tcx`.
/// Without this, `lookup_field_ty_diagnosed` returns `UnknownField {
/// opaque: true }` for `entry: &fs::DirInfo` even though `tcx` knows
/// the field layout.
/// The stdlib struct field tables the checker starts with, registered into
/// `tcx` and returned as the checker's own copy.
fn stdlib_struct_fields(tcx: &mut TyCtxt) -> HashMap<gossamer_resolve::DefId, Vec<(String, Ty)>> {
    register_stdlib_struct_fields(tcx);
    let mut fields = HashMap::new();
    seed_checker_stdlib_struct_fields(tcx, &mut fields);
    fields
}

fn seed_checker_stdlib_struct_fields(
    tcx: &mut TyCtxt,
    map: &mut HashMap<gossamer_resolve::DefId, Vec<(String, Ty)>>,
) {
    let str_ty = tcx.string_ty();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let u8_ty = tcx.int_ty(IntTy::U8);
    let vec_u8 = tcx.intern(TyKind::Vec(u8_ty));
    let str_pair = tcx.intern(TyKind::Tuple(vec![str_ty, str_ty]));
    let vec_str_pair = tcx.intern(TyKind::Vec(str_pair));
    let bool_ty = tcx.bool_ty();
    let context_def = gossamer_resolve::DefId::local(u32::MAX - 11);
    tcx.register_def_name(context_def, "context::Context");
    let context_ty = tcx.intern(TyKind::Adt {
        def: context_def,
        substs: crate::Substs::new(),
    });
    let entries: &[(u32, &[(&str, Ty)])] = &[
        (
            2,
            &[
                ("name", str_ty),
                ("path", str_ty),
                ("is_file", bool_ty),
                ("is_dir", bool_ty),
                ("is_symlink", bool_ty),
                ("size", i64_ty),
                ("modified_ms", i64_ty),
            ],
        ),
        (
            3,
            &[("stdout", str_ty), ("stderr", str_ty), ("code", i64_ty)],
        ),
        (
            4,
            &[
                ("__handle", i64_ty),
                ("status", i64_ty),
                ("content_type", str_ty),
            ],
        ),
        (
            5,
            &[
                ("status", i64_ty),
                ("body", str_ty),
                ("raw_bytes", vec_u8),
                ("content_type", str_ty),
                ("location", str_ty),
                ("headers", vec_str_pair),
            ],
        ),
        (
            24,
            &[
                ("method", str_ty),
                ("path", str_ty),
                ("query", str_ty),
                ("query_pairs", vec_str_pair),
                ("headers", vec_str_pair),
                ("body", str_ty),
                ("raw_body", vec_u8),
                ("peer_addr", str_ty),
                ("context", context_ty),
            ],
        ),
    ];
    for (offset, fields) in entries {
        let def = gossamer_resolve::DefId::local(u32::MAX - offset);
        let list: Vec<(String, Ty)> = fields.iter().map(|(n, t)| ((*n).to_string(), *t)).collect();
        map.insert(def, list);
    }
}

/// Resolves a symbolic array length against a const-substitution list:
/// a `Param(idx)` length becomes `Concrete` when `const_substs[idx]`
/// holds a non-negative value; every other length is unchanged.
/// Operator-overload impl-method name for an arithmetic binary operator,
/// or `None` for operators that are not overloadable on user types.
/// Why a rejected `#[derive(name)]` is unsupported, and what to do instead.
fn derive_rejection_hint(name: &str) -> String {
    match name {
        "Clone" => "values copy by value - `let b = a` is the copy, and `a.clone()` \
                    already works without a derive"
            .to_string(),
        "Hash" | "Hashable" => {
            "structs and enums hash by value automatically; remove it".to_string()
        }
        "Copy" => {
            "values are managed automatically; there is no Copy / move distinction".to_string()
        }
        "Display" | "Debug" => format!(
            "the rendering is synthesized; write `impl {name} for T` with \
             `fn {method}(&self) -> String` to override it",
            method = if name == "Display" {
                "to_string"
            } else {
                "fmt"
            }
        ),
        "Serialize" | "Deserialize" => {
            "serialization is automatic - call `to_json::<T>` / `from_json::<T>`".to_string()
        }
        "From" | "Into" | "TryFrom" | "TryInto" | "Add" | "Sub" | "Mul" | "Div" | "Rem" | "Neg"
        | "Not" | "BitAnd" | "BitOr" | "BitXor" | "Shl" | "Shr" | "Index" | "IndexMut" => {
            format!("implement it with `impl {name} for T`, not `#[derive]`")
        }
        _ => "Gossamer derives only Debug, Default, PartialEq, Eq, PartialOrd, Ord".to_string(),
    }
}

fn arith_op_method(op: BinaryOp) -> Option<&'static str> {
    match op {
        BinaryOp::Add => Some("add"),
        BinaryOp::Sub => Some("sub"),
        BinaryOp::Mul => Some("mul"),
        BinaryOp::Div => Some("div"),
        BinaryOp::Rem => Some("rem"),
        BinaryOp::BitAnd => Some("bitand"),
        BinaryOp::BitOr => Some("bitor"),
        BinaryOp::BitXor => Some("bitxor"),
        BinaryOp::Shl => Some("shl"),
        BinaryOp::Shr => Some("shr"),
        _ => None,
    }
}

/// Writability of an assignment place, computed from its root binding's
/// declared mutability and any reference it is reached through.
#[derive(Clone, Copy)]
enum PlaceMut {
    /// Rooted at a `mut` binding or reached through a `&mut` reference.
    Writable,
    /// Rooted at a non-`mut` binding.
    ImmutableBinding,
    /// Reached through a shared `&T` reference.
    SharedReference,
    /// Dereferenced from a binding whose type is not a reference, so the
    /// place the write would name does not exist.
    NotAReference,
    /// Not statically determinable; not checked.
    Unknown,
}

/// Operator-overload impl-method name for a compound assignment
/// (`+=` -> `add`, matching the binary desugar), or `None` for plain `=`.
fn assign_op_method(op: gossamer_ast::AssignOp) -> Option<&'static str> {
    use gossamer_ast::AssignOp;
    match op {
        AssignOp::Assign => None,
        AssignOp::AddAssign => Some("add"),
        AssignOp::SubAssign => Some("sub"),
        AssignOp::MulAssign => Some("mul"),
        AssignOp::DivAssign => Some("div"),
        AssignOp::RemAssign => Some("rem"),
        AssignOp::BitAndAssign => Some("bitand"),
        AssignOp::BitOrAssign => Some("bitor"),
        AssignOp::BitXorAssign => Some("bitxor"),
        AssignOp::ShlAssign => Some("shl"),
        AssignOp::ShrAssign => Some("shr"),
    }
}

/// Operator trait that declares the overload method `method`
/// (`add` -> `Add`), for diagnostics that suggest the missing impl.
fn op_trait_name(method: &str) -> &'static str {
    match method {
        "add" => "Add",
        "sub" => "Sub",
        "mul" => "Mul",
        "div" => "Div",
        "rem" => "Rem",
        "bitand" => "BitAnd",
        "bitor" => "BitOr",
        "bitxor" => "BitXor",
        "shl" => "Shl",
        "shr" => "Shr",
        "neg" => "Neg",
        "not" => "Not",
        "index" => "Index",
        _ => "Add",
    }
}

fn subst_array_len(len: crate::ArrayLen, const_substs: &[Option<i128>]) -> crate::ArrayLen {
    let crate::ArrayLen::Param(idx) = len else {
        return len;
    };
    match const_substs.get(idx.0 as usize).copied().flatten() {
        Some(v) if v >= 0 => crate::ArrayLen::Concrete(v as usize),
        _ => len,
    }
}

fn evaluate_const_int_from_expr(expr: &Expr) -> Option<u128> {
    if let ExprKind::Literal(Literal::Int(text)) = &expr.kind {
        let cleaned = strip_int_suffix(text).replace('_', "");
        return parse_int(&cleaned);
    }
    None
}

/// Parses the magnitude of an integer literal as a `u128`. The leading
/// `-` is dropped before parsing; callers that care about signedness
/// must apply it externally. Returns `None` for non-parseable text.
fn parse_int_magnitude(text: &str) -> Option<u128> {
    let cleaned = strip_int_suffix(text).replace('_', "");
    let trimmed = cleaned.strip_prefix('-').unwrap_or(&cleaned);
    parse_int(trimmed)
}

/// Returns `true` when the unsigned magnitude of `text` fits in the
/// declared integer type. Treats a leading `-` as the literal's
/// negation, applying the signed-range bound from the negative side.
fn int_literal_fits(text: &str, ty: IntTy) -> bool {
    let Some(magnitude) = parse_int_magnitude(text) else {
        return false;
    };
    let negative = text.trim_start().starts_with('-');
    let (signed_max, signed_min_abs, unsigned_max) = int_bounds(ty);
    if let Some(unsigned_max) = unsigned_max {
        if negative {
            return false;
        }
        return magnitude <= unsigned_max;
    }
    let limit = if negative { signed_min_abs } else { signed_max };
    magnitude <= limit
}

/// Returns `(signed_max, signed_min_abs, unsigned_max)` for `ty`. The
/// unsigned slot is `None` for signed widths. The signed-minimum is
/// stored as a positive magnitude (`-i8::MIN` is reported as `128`).
fn int_bounds(ty: IntTy) -> (u128, u128, Option<u128>) {
    match ty {
        IntTy::I8 => (i8::MAX as u128, 1u128 << 7, None),
        IntTy::I16 => (i16::MAX as u128, 1u128 << 15, None),
        IntTy::I32 => (i32::MAX as u128, 1u128 << 31, None),
        IntTy::I64 => (i64::MAX as u128, 1u128 << 63, None),
        IntTy::I128 => (i128::MAX as u128, 1u128 << 127, None),
        IntTy::Isize => (i64::MAX as u128, 1u128 << 63, None),
        IntTy::U8 => (0, 0, Some(u128::from(u8::MAX))),
        IntTy::U16 => (0, 0, Some(u128::from(u16::MAX))),
        IntTy::U32 => (0, 0, Some(u128::from(u32::MAX))),
        IntTy::U64 => (0, 0, Some(u128::from(u64::MAX))),
        IntTy::U128 => (0, 0, Some(u128::MAX)),
        IntTy::Usize => (0, 0, Some(u128::from(u64::MAX))),
    }
}

fn parse_int(text: &str) -> Option<u128> {
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u128::from_str_radix(rest, 16).ok();
    }
    if let Some(rest) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        return u128::from_str_radix(rest, 2).ok();
    }
    if let Some(rest) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
        return u128::from_str_radix(rest, 8).ok();
    }
    text.parse::<u128>().ok()
}

fn int_assoc_const(segments: &[&str]) -> Option<(IntTy, i128)> {
    if segments.len() != 2 {
        return None;
    }
    let ty = match segments[0] {
        "i8" => IntTy::I8,
        "i16" => IntTy::I16,
        "i32" => IntTy::I32,
        "i64" => IntTy::I64,
        "isize" => IntTy::Isize,
        "u8" => IntTy::U8,
        "u16" => IntTy::U16,
        "u32" => IntTy::U32,
        "u64" => IntTy::U64,
        "usize" => IntTy::Usize,
        _ => return None,
    };
    let value = match (ty, segments[1]) {
        (IntTy::I8, "MIN") => i128::from(i8::MIN),
        (IntTy::I8, "MAX") => i128::from(i8::MAX),
        (IntTy::I16, "MIN") => i128::from(i16::MIN),
        (IntTy::I16, "MAX") => i128::from(i16::MAX),
        (IntTy::I32, "MIN") => i128::from(i32::MIN),
        (IntTy::I32, "MAX") => i128::from(i32::MAX),
        (IntTy::I64 | IntTy::Isize, "MIN") => i128::from(i64::MIN),
        (IntTy::I64 | IntTy::Isize, "MAX") => i128::from(i64::MAX),
        (IntTy::U8, "MIN") => 0,
        (IntTy::U8, "MAX") => i128::from(u8::MAX),
        (IntTy::U16, "MIN") => 0,
        (IntTy::U16, "MAX") => i128::from(u16::MAX),
        (IntTy::U32, "MIN") => 0,
        (IntTy::U32, "MAX") => i128::from(u32::MAX),
        (IntTy::U64 | IntTy::Usize, "MIN") => 0,
        (IntTy::U64 | IntTy::Usize, "MAX") => i128::from(u64::MAX),
        _ => return None,
    };
    Some((ty, value))
}

fn strip_int_suffix(text: &str) -> String {
    for (suffix, _) in INT_SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    for (suffix, _) in FLOAT_SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    text.to_string()
}

fn is_stable_borrow_place(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Path(_) => true,
        ExprKind::FieldAccess { receiver, .. } => is_stable_borrow_place(receiver),
        ExprKind::Index { base, .. } => is_stable_borrow_place(base),
        ExprKind::Unary {
            op: UnaryOp::Deref,
            operand,
        } => is_stable_borrow_place(operand),
        _ => false,
    }
}

fn expr_is_static_string_value(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(Literal::String(_) | Literal::RawString { .. }) => true,
        ExprKind::Block(block) | ExprKind::Unsafe(block) => block
            .tail
            .as_deref()
            .is_some_and(expr_is_static_string_value),
        ExprKind::If {
            then_branch,
            else_branch: Some(else_branch),
            ..
        } => expr_is_static_string_value(then_branch) && expr_is_static_string_value(else_branch),
        ExprKind::Match { arms, .. } => {
            !arms.is_empty()
                && arms
                    .iter()
                    .all(|arm| expr_is_static_string_value(&arm.body))
        }
        _ => false,
    }
}

fn pattern_binding_names(pattern: &Pattern, out: &mut Vec<String>) {
    match &pattern.kind {
        PatternKind::Ident {
            name, subpattern, ..
        } => {
            out.push(name.name.clone());
            if let Some(subpattern) = subpattern {
                pattern_binding_names(subpattern, out);
            }
        }
        PatternKind::Tuple(items) | PatternKind::Or(items) => {
            for item in items {
                pattern_binding_names(item, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for item in prefix {
                pattern_binding_names(item, out);
            }
            if let Some(rest) = rest {
                pattern_binding_names(rest, out);
            }
            for item in suffix {
                pattern_binding_names(item, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for field in fields {
                if let Some(pattern) = &field.pattern {
                    pattern_binding_names(pattern, out);
                } else {
                    out.push(field.name.name.clone());
                }
            }
        }
        PatternKind::TupleStruct { elems, .. } => {
            for item in elems {
                pattern_binding_names(item, out);
            }
        }
        PatternKind::Ref { inner, .. } => pattern_binding_names(inner, out),
        PatternKind::Wildcard
        | PatternKind::Literal(_)
        | PatternKind::Path(_)
        | PatternKind::Range { .. }
        | PatternKind::Rest
        | PatternKind::Error => {}
    }
}

/// A closure that a goroutine will run: its parameters and its body.
struct GoroutineBody<'a> {
    params: &'a [ClosureParam],
    body: &'a Expr,
}

/// Every closure whose body a `spawn` / `go` expression will run.
///
/// `spawn(|| work())` names the closure directly;
/// `go f(closure)` hands one to a call, whose own body runs in the spawning
/// goroutine, so only the closure arguments are collected.
fn goroutine_bodies(expr: &Expr) -> Vec<GoroutineBody<'_>> {
    match &expr.kind {
        ExprKind::Closure { params, body, .. } => vec![GoroutineBody { params, body }],
        ExprKind::Call { callee, args } => {
            let mut out = goroutine_bodies(callee);
            for arg in args {
                if matches!(arg.kind, ExprKind::Closure { .. }) {
                    out.extend(goroutine_bodies(arg));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The names a closure's own parameters bind, which shadow anything outside.
fn closure_bound_names(params: &[ClosureParam]) -> HashSet<String> {
    let mut out = HashSet::new();
    for param in params {
        let mut names = Vec::new();
        pattern_binding_names(&param.pattern, &mut names);
        out.extend(names);
    }
    out
}

fn expr_mentions_any_name(expr: &Expr, names: &HashSet<String>) -> bool {
    struct Finder<'a> {
        names: &'a HashSet<String>,
        found: bool,
    }

    impl gossamer_ast::visitor::Visitor for Finder<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if self.found {
                return;
            }
            if let ExprKind::Path(path) = &expr.kind
                && let [segment] = path.segments.as_slice()
                && self.names.contains(&segment.name.name)
            {
                self.found = true;
                return;
            }
            gossamer_ast::visitor::walk_expr(self, expr);
        }
    }

    let mut finder = Finder {
        names,
        found: false,
    };
    gossamer_ast::visitor::Visitor::visit_expr(&mut finder, expr);
    finder.found
}

fn kind_is_concrete(checker: &TypeChecker<'_>, kind: &TyKind) -> bool {
    match kind {
        TyKind::Var(_) | TyKind::Error => false,
        TyKind::Bool
        | TyKind::Char
        | TyKind::String
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Unit
        | TyKind::Never
        | TyKind::Duration
        | TyKind::Instant
        | TyKind::JsonValue
        | TyKind::DynValue
        | TyKind::DynError
        | TyKind::Param { .. } => true,
        TyKind::Tuple(parts) => parts.iter().all(|t| checker.is_concrete(*t)),
        TyKind::Array { elem, .. }
        | TyKind::Slice(elem)
        | TyKind::Vec(elem)
        | TyKind::Iterator(elem)
        | TyKind::Range(elem)
        | TyKind::Sender(elem)
        | TyKind::Receiver(elem)
        | TyKind::JoinHandle(elem)
        | TyKind::Nominal { repr: elem, .. }
        | TyKind::Ref { inner: elem, .. } => checker.is_concrete(*elem),
        TyKind::HashMap { key, value, .. } => {
            checker.is_concrete(*key) && checker.is_concrete(*value)
        }
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
            sig.inputs.iter().all(|t| checker.is_concrete(*t)) && checker.is_concrete(sig.output)
        }
        TyKind::FnDef { substs, .. }
        | TyKind::Adt { substs, .. }
        | TyKind::Alias { substs, .. }
        | TyKind::Closure { substs, .. } => substs.as_slice().iter().all(|arg| match arg {
            crate::GenericArg::Type(ty) => checker.is_concrete(*ty),
            crate::GenericArg::Const(_) => true,
        }),
        TyKind::Dyn(trait_ref) => trait_ref.substs.as_slice().iter().all(|arg| match arg {
            crate::GenericArg::Type(ty) => checker.is_concrete(*ty),
            crate::GenericArg::Const(_) => true,
        }),
    }
}

const INT_SUFFIXES: &[(&str, IntTy)] = &[
    ("i128", IntTy::I128),
    ("u128", IntTy::U128),
    ("isize", IntTy::Isize),
    ("usize", IntTy::Usize),
    ("i64", IntTy::I64),
    ("u64", IntTy::U64),
    ("i32", IntTy::I32),
    ("u32", IntTy::U32),
    ("i16", IntTy::I16),
    ("u16", IntTy::U16),
    ("i8", IntTy::I8),
    ("u8", IntTy::U8),
];

const FLOAT_SUFFIXES: &[(&str, FloatTy)] = &[("f32", FloatTy::F32), ("f64", FloatTy::F64)];

fn int_ty_from_width(width: IntWidth, signed: bool) -> IntTy {
    match (signed, width) {
        (true, IntWidth::W8) => IntTy::I8,
        (true, IntWidth::W16) => IntTy::I16,
        (true, IntWidth::W32) => IntTy::I32,
        (true, IntWidth::W64) => IntTy::I64,
        (true, IntWidth::W128) => IntTy::I128,
        (true, IntWidth::Size) => IntTy::Isize,
        (false, IntWidth::W8) => IntTy::U8,
        (false, IntWidth::W16) => IntTy::U16,
        (false, IntWidth::W32) => IntTy::U32,
        (false, IntWidth::W64) => IntTy::U64,
        (false, IntWidth::W128) => IntTy::U128,
        (false, IntWidth::Size) => IntTy::Usize,
    }
}

/// Returns true when `path` names the stdlib `json::Value` type, in
/// any of the accepted spellings (`json::Value`,
/// `encoding::json::Value`, `std::encoding::json::Value`). The
/// resolver treats every prefix as a bare import binding so the
/// type checker has to recognise the surface syntax directly.
fn path_matches_json_value(path: &TypePath) -> bool {
    let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    matches!(
        names.as_slice(),
        ["json", "Value"] | ["encoding", "json", "Value"] | ["std", "encoding", "json", "Value"]
    )
}

/// Returns the json variant name when `path` names a `json::Value::X`
/// constructor in a pattern (`json::Value::Object`,
/// `encoding::json::Value::Int`, ...). Used to reject such constructors
/// in pattern position - `json::Value` is an opaque handle with no
/// matchable discriminant.
fn json_value_variant_of(path: &TypePath) -> Option<&'static str> {
    let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    let n = names.len();
    if n < 2 || names[n - 2] != "Value" || !names[..n - 1].contains(&"json") {
        return None;
    }
    match names[n - 1] {
        "Null" => Some("Null"),
        "Bool" => Some("Bool"),
        "Int" => Some("Int"),
        "Float" => Some("Float"),
        "Number" => Some("Number"),
        "String" => Some("String"),
        "Array" => Some("Array"),
        "Object" => Some("Object"),
        _ => None,
    }
}

fn path_matches_dyn_error(path: &TypePath) -> bool {
    let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    matches!(
        names.as_slice(),
        ["errors" | "error", "Error"] | ["std", "errors" | "error", "Error"]
    )
}

/// Returns the use-site type arguments of `path` (`Foo<i64, String>` ->
/// `[i64, String]`), used to instantiate a generic alias.
fn alias_type_args(path: &TypePath) -> Vec<AstType> {
    path.segments
        .last()
        .map(|seg| {
            seg.generics
                .iter()
                .filter_map(|g| match g {
                    AstGenericArg::Type(t) => Some(t.clone()),
                    AstGenericArg::Const(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Substitutes each alias type parameter in `rhs` with the matching
/// argument: `subst_alias_params((A, A), [A], [i64])` yields `(i64, i64)`.
fn subst_alias_params(rhs: &AstType, params: &[String], args: &[AstType]) -> AstType {
    use gossamer_ast::VisitorMut;
    let mut out = rhs.clone();
    AliasParamSubst { params, args }.visit_type(&mut out);
    out
}

struct AliasParamSubst<'a> {
    params: &'a [String],
    args: &'a [AstType],
}

impl gossamer_ast::VisitorMut for AliasParamSubst<'_> {
    fn visit_type(&mut self, ty: &mut AstType) {
        if let AstTypeKind::Path(p) = &ty.kind
            && p.segments.len() == 1
            && p.segments[0].generics.is_empty()
            && let Some(i) = self
                .params
                .iter()
                .position(|n| n.as_str() == p.segments[0].name.name.as_str())
        {
            *ty = self.args[i].clone();
            return;
        }
        gossamer_ast::visitor::walk_type_mut(self, ty);
    }
}

fn primitive_from_name(name: &str) -> Option<PrimitiveTy> {
    Some(match name {
        "bool" => PrimitiveTy::Bool,
        "char" => PrimitiveTy::Char,
        // `str` is the borrowed spelling of the runtime's string value.
        // The enclosing `Ref` preserves the source-level distinction while
        // the pointee shares `String`'s representation and operations.
        "str" => PrimitiveTy::String,
        "String" => PrimitiveTy::String,
        "i8" => PrimitiveTy::Int(IntWidth::W8),
        "i16" => PrimitiveTy::Int(IntWidth::W16),
        "i32" => PrimitiveTy::Int(IntWidth::W32),
        "i64" => PrimitiveTy::Int(IntWidth::W64),
        "i128" => PrimitiveTy::Int(IntWidth::W128),
        "isize" => PrimitiveTy::Int(IntWidth::Size),
        "u8" => PrimitiveTy::UInt(IntWidth::W8),
        "u16" => PrimitiveTy::UInt(IntWidth::W16),
        "u32" => PrimitiveTy::UInt(IntWidth::W32),
        "u64" => PrimitiveTy::UInt(IntWidth::W64),
        "u128" => PrimitiveTy::UInt(IntWidth::W128),
        "usize" => PrimitiveTy::UInt(IntWidth::Size),
        "f32" => PrimitiveTy::Float(FloatWidth::W32),
        "f64" => PrimitiveTy::Float(FloatWidth::W64),
        _ => return None,
    })
}

fn prim_to_ty(tcx: &mut TyCtxt, prim: PrimitiveTy) -> Ty {
    match prim {
        PrimitiveTy::Bool => tcx.bool_ty(),
        PrimitiveTy::Char => tcx.char_ty(),
        PrimitiveTy::String => tcx.string_ty(),
        PrimitiveTy::Int(width) => tcx.int_ty(int_ty_from_width(width, true)),
        PrimitiveTy::UInt(width) => tcx.int_ty(int_ty_from_width(width, false)),
        PrimitiveTy::Float(FloatWidth::W32) => tcx.float_ty(FloatTy::F32),
        PrimitiveTy::Float(FloatWidth::W64) => tcx.float_ty(FloatTy::F64),
        PrimitiveTy::Never => tcx.never(),
        PrimitiveTy::Unit => tcx.unit(),
    }
}

/// Returns `true` when `stmt` is a statement that always
/// diverges (transfers control out of the enclosing block via
/// `return`, `break`, `continue`, or a panicking call). Used by
/// `check_block` to give a tail-less, divergent block the type
/// `!` instead of `()`.
fn stmt_diverges(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Expr { expr, .. } => expr_diverges(expr),
        StmtKind::Let {
            init: Some(init), ..
        } => expr_diverges(init),
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum PlainLetPatternProblem {
    Literal,
    MayNotMatch,
}

fn plain_let_pattern_problem(pattern: &Pattern) -> Option<PlainLetPatternProblem> {
    match &pattern.kind {
        PatternKind::Literal(_) => Some(PlainLetPatternProblem::Literal),
        PatternKind::Range { .. } => Some(PlainLetPatternProblem::MayNotMatch),
        PatternKind::Ident { subpattern, .. } => {
            subpattern.as_deref().and_then(plain_let_pattern_problem)
        }
        PatternKind::Tuple(parts) => parts.iter().find_map(plain_let_pattern_problem),
        PatternKind::Or(parts) => {
            let problems: Vec<_> = parts.iter().map(plain_let_pattern_problem).collect();
            if problems.iter().all(Option::is_some) {
                problems
                    .into_iter()
                    .flatten()
                    .find(|problem| matches!(problem, PlainLetPatternProblem::Literal))
                    .or(Some(PlainLetPatternProblem::MayNotMatch))
            } else {
                None
            }
        }
        PatternKind::Struct { fields, .. } => fields
            .iter()
            .filter_map(|field| field.pattern.as_ref())
            .find_map(plain_let_pattern_problem),
        PatternKind::TupleStruct { elems, .. } => elems.iter().find_map(plain_let_pattern_problem),
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => prefix
            .iter()
            .chain(suffix)
            .find_map(plain_let_pattern_problem)
            .or_else(|| {
                (!prefix.is_empty() || rest.is_none() || !suffix.is_empty())
                    .then_some(PlainLetPatternProblem::MayNotMatch)
            }),
        PatternKind::Ref { inner, .. } => plain_let_pattern_problem(inner),
        PatternKind::Wildcard | PatternKind::Path(_) | PatternKind::Rest | PatternKind::Error => {
            None
        }
    }
}

fn expr_diverges(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue { .. },
    )
}

/// Returns `true` when `from as to` is in the permitted cast set.
///
/// Mirrors Rust RFC 401:
/// - numeric ↔ numeric (every pair of `Int(_)` / `Float(_)`)
/// - `bool` → integer
/// - `char` → integer
/// - `u8` → `char`
/// - same-type cast (no-op, always allowed)
fn cast_allowed(from: &TyKind, to: &TyKind) -> bool {
    if from == to {
        return true;
    }
    let from_is_num = matches!(from, TyKind::Int(_) | TyKind::Float(_));
    let to_is_num = matches!(to, TyKind::Int(_) | TyKind::Float(_));
    if from_is_num && to_is_num {
        return true;
    }
    // Any int → char reads the low byte on every tier (the same
    // masking `u8 as char` always applied), so `s[i] as char` works
    // without the `(s[i] as u8)` intermediate.
    matches!(
        (from, to),
        (TyKind::Bool | TyKind::Char, TyKind::Int(_)) | (TyKind::Int(_), TyKind::Char),
    )
}

/// Returns `true` when `name` is a trait the language ships
/// built-in (used for the `<T: Bound>` validation in
/// `register_fn_sig`). The list mirrors stdlib traits that
/// have no source-level declaration in the current file but
/// are part of the surface - keeps a `fn f<T: Iterator>(...)`
/// declaration in a file that itself does not declare
/// `trait Iterator` from raising a false unknown-trait
/// diagnostic.
/// The canonical `String` method surface. Mirrors the compiled-tier
/// dispatch in `gossamer-mir`'s `method_call.rs` (the `TyKind::String`
/// arms) plus the universal `push` / `push_str` building surface, so a
/// String receiver accepts exactly what every tier can lower. A method
/// outside this set on a `String` receiver is the name-global dispatch
/// leak (a `unicode::*` char predicate or a typo) and is rejected. The
/// commonly-typed methods (`find`, `len`, `contains`, ...) are handled
/// with precise return types before this fallback and need not recur
/// here, but listing them keeps the set self-describing.
fn is_string_method(name: &str) -> bool {
    STRING_METHODS.contains(&name)
}

/// The `String` method surface, in the order diagnostics list it.
const STRING_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "as_bytes",
    "bytes",
    "chars",
    "split",
    "splitn",
    "split_whitespace",
    "split_once",
    "rsplit_once",
    "lines",
    "find",
    "rfind",
    "find_any",
    "rfind_any",
    "index_rune",
    "contains",
    "contains_any",
    "contains_rune",
    "starts_with",
    "ends_with",
    "equal_fold",
    "count",
    "byte_at",
    "byte_len",
    "trim",
    "trim_start",
    "trim_end",
    "trim_matches",
    "trim_start_matches",
    "trim_end_matches",
    "replace",
    "replacen",
    "to_lowercase",
    "to_uppercase",
    "to_title",
    "to_i64",
    "to_f64",
    "to_bool",
    "repeat",
    "strip_prefix",
    "strip_suffix",
    "pad_left",
    "pad_right",
    "center",
    "slice",
    "substring",
    "clear",
    "truncate",
    "push",
    "push_str",
    "push_char",
    "push_byte",
    "push_utf8",
    "parse",
];

/// Returns whether a `Vec` method changes capacity or length and therefore
/// cannot be called on a fixed-size array receiver.
#[must_use]
pub fn is_vec_only_sequence_method(name: &str) -> bool {
    VEC_ONLY_SEQUENCE_METHODS.contains(&name)
}

/// The length- and capacity-changing sequence methods, which only a
/// `Vec<T>` receiver carries.
const VEC_ONLY_SEQUENCE_METHODS: &[&str] = &[
    "binary_search",
    "copy_from_slice",
    "copy_within",
    "push",
    "pop",
    "insert",
    "remove",
    "clear",
    "extend",
    "extend_from_slice",
    "truncate",
    "reserve",
    "reserve_exact",
    "capacity",
    "append",
    "resize",
    "resize_with",
    "split_off",
    "drain",
    "retain",
    "shrink_to_fit",
    "dedup",
];

/// Returns whether a method belongs to Gossamer's slice surface. This is the
/// canonical list used by method checking and REPL documentation. Eager
/// iterator combinators remain Vec operations; arrays and slices use `iter()`
/// before applying iterator methods, matching Rust's separation of collection
/// and iterator APIs.
#[must_use]
pub fn is_slice_sequence_method(name: &str) -> bool {
    SLICE_SEQUENCE_METHODS.contains(&name) || COLLECTION_TRAVERSAL_METHODS.contains(&name)
}

/// The slice method surface shared by slices, arrays, and `Vec`.
const SLICE_SEQUENCE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "slice",
    "first",
    "last",
    "get",
    "contains",
    "index_of",
    "count_of",
    "sort",
    "sort_by",
    "sort_by_key",
    "reverse",
    "swap",
    "fill",
    "windows",
    "chunks",
    "join",
    "to_vec",
    "iter",
];

/// Returns whether a method is rejected for a tuple receiver. A tuple's
/// elements may differ in type, so nothing that walks it as a sequence of
/// one element type applies: iteration has no element type to yield, and
/// the combinators built on it inherit that. Positional access (`t.0`,
/// `t.get(i)`) and whole-value operations stay available.
#[must_use]
pub fn is_tuple_rejected_method(name: &str) -> bool {
    !is_tuple_method(name)
}

/// Returns whether a method is implemented for a tuple receiver. A tuple is
/// a fixed heterogeneous group, so its surface is whole-value operations
/// plus positional access - the sequence methods have no single element
/// type to act on and no buffer to reorder.
#[must_use]
pub fn is_tuple_method(name: &str) -> bool {
    TUPLE_METHODS.contains(&name)
}

/// The tuple method surface: whole-value operations plus positional access.
const TUPLE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "get",
    "clone",
    "to_string",
    "into",
    "try_into",
];

/// Returns whether a method is implemented for a `Map` receiver. Keeping
/// discovery, type checking, and the runtime on one list stops a sequence
/// method from reaching a map, where it has no ordered buffer to act on and
/// would read as a silent no-op.
#[must_use]
pub fn is_map_method(name: &str) -> bool {
    MAP_METHODS.contains(&name)
        || (COLLECTION_TRAVERSAL_METHODS.contains(&name)
            && !MAP_UNTRAVERSABLE_METHODS.contains(&name)
            && !is_free_call_only_traversal(name))
}

/// Traversals a map cannot answer: its element is a `(K, V)` pair, which
/// neither adds up, multiplies, nor flattens.
const MAP_UNTRAVERSABLE_METHODS: &[&str] = &["flatten", "product", "sum"];

/// Whether the runtime binds `name` only as a data-last free call
/// (`iter::filter_map(f, xs)`), with no receiver form on any tier.
#[must_use]
pub fn is_free_call_only_traversal(name: &str) -> bool {
    FREE_CALL_ONLY_TRAVERSALS.contains(&name)
}

/// Traversals with a data-last free call and no receiver form.
///
/// `sort_by` and `sort_by_key` are the only two, and only for the receivers
/// this list gates - `Iterator`, `Range`, and `Set` - which hold no ordered
/// buffer a sort could reorder. `Vec`, an array, and a slice reach them
/// through the slice surface instead, where they sort the receiver in place.
const FREE_CALL_ONLY_TRAVERSALS: &[&str] = &["sort_by", "sort_by_key"];

/// Whether a `Set` / `BTreeSet` receiver answers `name`.
#[must_use]
pub fn is_set_method(name: &str) -> bool {
    SET_METHODS.contains(&name) && !is_free_call_only_traversal(name)
}

/// Whether an `Iterator` / `Range` receiver answers `name` in method
/// position. The data-last free surface is wider - it takes an iterator
/// for every name [`is_iterator_method`] lists - so the two differ.
#[must_use]
pub fn iterator_receiver_accepts_method(name: &str) -> bool {
    is_iterator_method(name) && !is_free_call_only_traversal(name)
}

/// The `Map` method surface shared by discovery, type checking, and the
/// runtime.
const MAP_METHODS: &[&str] = &[
    "insert",
    "get",
    "get_or",
    "or_insert",
    "remove",
    "pop",
    "contains_key",
    "contains",
    "inc",
    "inc_at",
    "inc_batch",
    "len",
    "is_empty",
    "keys",
    "values",
    "iter",
    "clear",
];

/// Fixed arrays expose value-preserving `clone` in addition to methods made
/// available through Rust-like array-to-slice receiver coercion.
#[must_use]
pub fn is_array_sequence_method(name: &str) -> bool {
    matches!(name, "clone" | "into") || is_slice_sequence_method(name)
}

/// Returns whether a method is implemented for an `Iterator<T>` receiver on
/// every execution tier. Keep discovery and type checking on this single list
/// so `%info` and `%explain` never advertise eager Vec-only helpers as lazy
/// iterator operations.
#[must_use]
pub fn is_iterator_method(name: &str) -> bool {
    ITERATOR_METHODS.contains(&name)
}

/// Returns whether an iterator method answers with another iterator rather
/// than materialising a value. A name absent from this list is a terminal:
/// it ends the pipeline and produces a concrete result. Type checking and
/// `%info` share this list so a rendered signature cannot drift from the
/// type the call actually has.
#[must_use]
pub fn iterator_adapter_is_lazy(name: &str) -> bool {
    LAZY_ITERATOR_ADAPTERS.contains(&name)
}

/// Methods that traverse a sequence rather than describe or mutate it. A
/// collection does not answer these: `xs.iter()` starts the traversal and the
/// iterator answers them from there. Kept apart from the collection surface so
/// one operation has one spelling instead of an eager and a lazy one.
/// Whether `name` traverses a collection's elements.
#[must_use]
pub fn is_collection_traversal_method(name: &str) -> bool {
    COLLECTION_TRAVERSAL_METHODS.contains(&name)
}

const COLLECTION_TRAVERSAL_METHODS: &[&str] = &[
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "scan",
    "take",
    "take_while",
    "skip",
    "skip_while",
    "step_by",
    "enumerate",
    "zip",
    "chain",
    "rev",
    "flatten",
    "pairwise",
    "fold",
    "reduce",
    "for_each",
    "sum",
    "sum_by",
    "product",
    "product_by",
    "min",
    "max",
    "min_by",
    "max_by",
    "min_by_key",
    "max_by_key",
    "any",
    "all",
    "find",
    "find_map",
    "position",
    "count",
    "partition",
    "unzip",
    "chunk_by",
    "count_by",
];

/// Iterator adapters that answer with another iterator on every tier.
const LAZY_ITERATOR_ADAPTERS: &[&str] = &[
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
];

/// Best-effort human-readable name for a call's callee expression,
/// used in arity diagnostics. A path renders as its joined segments;
/// anything else falls back to a generic label.
fn callee_display_name(callee: &Expr) -> String {
    match &callee.kind {
        ExprKind::Path(path) => path
            .segments
            .iter()
            .map(|s| s.name.name.as_str())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "this function".to_string(),
    }
}

/// The call a `|>` step makes, for the diagnostic that names what spent an
/// iterator. A closure step is named by the call in its body.
fn pipe_step_operation_name(rhs: &Expr) -> Option<String> {
    match &rhs.kind {
        ExprKind::Path(path) => Some(
            path.segments
                .iter()
                .map(|s| s.name.name.as_str())
                .collect::<Vec<_>>()
                .join("::"),
        ),
        ExprKind::Call { callee, .. } => Some(callee_display_name(callee)),
        ExprKind::MethodCall { name, .. } => Some(name.name.clone()),
        ExprKind::Closure { body, .. } => pipe_step_operation_name(body),
        _ => None,
    }
}

/// Trailing segment name of each trait bound in a `: A + B` list.
fn bound_names(bounds: &[gossamer_ast::TraitBound]) -> Vec<String> {
    bounds
        .iter()
        .filter_map(|b| b.path.segments.last())
        .map(|s| s.name.name.clone())
        .collect()
}

/// Source name of a type written as a single unqualified path segment
/// (`T`, `Shape`). Structural, generic, and qualified types name no single
/// declaration position, so they return `None`.
/// Records each `Name = Type` constraint written on `bounds` under the
/// parameter `param` they constrain.
fn collect_assoc_bindings(
    param: &str,
    bounds: &[gossamer_ast::TraitBound],
    out: &mut HashMap<(String, String), gossamer_ast::Type>,
) {
    for bound in bounds {
        for binding in &bound.bindings {
            out.insert(
                (param.to_string(), binding.name.name.clone()),
                binding.ty.clone(),
            );
        }
    }
}

/// Associated type name a `-> Self::Item` return projects, or `None` for
/// any other return type.
fn self_assoc_projection(ty: &gossamer_ast::Type) -> Option<String> {
    let gossamer_ast::ty::TypeKind::Path(path) = &ty.kind else {
        return None;
    };
    if path.segments.len() != 2 || path.segments[0].name.name != "Self" {
        return None;
    }
    Some(path.segments[1].name.name.clone())
}

fn bare_path_type_name(ty: &gossamer_ast::Type) -> Option<&str> {
    let gossamer_ast::ty::TypeKind::Path(path) = &ty.kind else {
        return None;
    };
    if path.segments.len() != 1 {
        return None;
    }
    Some(path.segments.last()?.name.name.as_str())
}

/// Appends each `where` predicate's bounds onto the entry of the parameter
/// it names. `offset` is the index the first entry of `params` occupies in
/// `out`, so an impl's and a method's clauses can share one table.
fn merge_where_predicates(
    params: &[gossamer_ast::GenericParam],
    offset: usize,
    where_clause: &gossamer_ast::WhereClause,
    out: &mut [Vec<String>],
) {
    for predicate in &where_clause.predicates {
        let Some(name) = bare_path_type_name(&predicate.bounded) else {
            continue;
        };
        let Some(position) = params.iter().position(|param| {
            matches!(param, gossamer_ast::GenericParam::Type { name: param_name, .. }
                if param_name.name == name)
        }) else {
            continue;
        };
        let Some(entry) = out.get_mut(offset + position) else {
            continue;
        };
        for bound in bound_names(&predicate.bounds) {
            if !entry.contains(&bound) {
                entry.push(bound);
            }
        }
    }
}

/// Appends `extra`'s bound names onto `table`, growing it as needed and
/// keeping each parameter's list free of repeats.
fn merge_bound_table(table: &mut Vec<Vec<String>>, extra: &[Vec<String>]) {
    if table.len() < extra.len() {
        table.resize(extra.len(), Vec::new());
    }
    for (entry, names) in table.iter_mut().zip(extra) {
        for name in names {
            if !entry.contains(name) {
                entry.push(name.clone());
            }
        }
    }
}

/// Whether a built-in trait name is one the language expects an explicit
/// `impl` block to supply. The operator traits are written out by hand;
/// every other built-in name (`Clone`, `Debug`, `Ord`, ...) names behaviour
/// every value already has.
fn builtin_trait_needs_impl(name: &str) -> bool {
    crate::builtin_traits::builtin_trait(name)
        .is_some_and(|entry| entry.kind == crate::builtin_traits::BuiltinTraitKind::Operator)
}

/// Head name of the type an `impl` block attaches to, as written.
fn impl_self_ty_name(decl: &ImplDecl) -> String {
    // A structural type keys on the name every tier identifies a receiver of
    // it by, so two blocks a dispatch cannot separate are reported here rather
    // than reaching one another's bodies.
    match &decl.self_ty.kind {
        gossamer_ast::ty::TypeKind::Tuple(elems) => format!("tuple_{}", elems.len()),
        _ => written_type_name(&decl.self_ty),
    }
}

/// The spelling an `impl` header wrote for its self type.
///
/// A structural type - `[T; N]`, `(A, B)`, `[T]`, `&T` - has no path to name
/// it by, and answering one placeholder for every such type keys them all
/// together: a second impl on a different structural type then reads as a
/// second impl on the same one. The written spelling distinguishes them and
/// is what a diagnostic about the block should print.
fn written_type_name(ty: &gossamer_ast::Type) -> String {
    use gossamer_ast::ty::TypeKind as K;
    match &ty.kind {
        K::Unit => "()".to_string(),
        K::Never => "!".to_string(),
        K::Infer => "_".to_string(),
        K::Path(path) => path
            .segments
            .last()
            .map_or_else(|| "this type".to_string(), |s| s.name.name.clone()),
        K::Tuple(elems) => {
            let rendered: Vec<String> = elems.iter().map(written_type_name).collect();
            format!("({})", rendered.join(", "))
        }
        K::Array { elem, len } => {
            let count = evaluate_const_int_from_expr(len)
                .map_or_else(|| "_".to_string(), |n| n.to_string());
            format!("[{}; {}]", written_type_name(elem), count)
        }
        K::Slice(inner) => format!("[{}]", written_type_name(inner)),
        K::Ref { mutability, inner } => {
            let prefix = match mutability {
                gossamer_ast::Mutability::Mutable => "&mut ",
                gossamer_ast::Mutability::Immutable => "&",
            };
            format!("{prefix}{}", written_type_name(inner))
        }
        K::Fn { kind, params, ret } => {
            let rendered: Vec<String> = params.iter().map(written_type_name).collect();
            let head = format!("{}({})", kind.as_str(), rendered.join(", "));
            match ret {
                Some(r) => format!("{head} -> {}", written_type_name(r)),
                None => head,
            }
        }
    }
}

/// Every item an `impl` of a built-in trait may define. `None` means the
/// trait's surface is not known here - a stdlib trait whose declaration
/// lives outside the checked source - so nothing the block writes can be
/// ruled out.
fn builtin_trait_impl_items(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "Handler" => Some(&["serve"]),
        _ => crate::builtin_traits::builtin_trait(name).map(|entry| entry.impl_items),
    }
}

/// Methods a built-in trait requires an `impl` block to supply. `Display`
/// and `Debug` name the rendering a value shows through `{}` and `{:?}`; a
/// type that implements one overrides the synthesized form with that method.
fn builtin_trait_required_methods() -> HashMap<String, Vec<String>> {
    HashMap::from([
        ("Display".to_string(), vec!["to_string".to_string()]),
        ("Debug".to_string(), vec!["fmt".to_string()]),
    ])
}

/// Every trait name an `impl` header may legitimately name: the language's
/// own built-ins plus the traits the standard library declares.
fn known_builtin_trait(name: &str) -> bool {
    STDLIB_TRAIT_NAMES.contains(&name) || crate::builtin_traits::builtin_trait(name).is_some()
}

/// Traits the standard library declares, which user code implements the same
/// way it implements a trait of its own. Kept in step with the manifest by
/// `stdlib_export_drift`.
pub const STDLIB_TRAIT_NAMES: &[&str] = &[
    "Debug",
    "Deserialize",
    "Display",
    "Driver",
    "Handler",
    "Http2Handler",
    "Http2StreamingHandler",
    "Reader",
    "Serialize",
    "Validate",
    "Writer",
];

/// Collects the argument-path node of every `archive::{tar,zip}::write`
/// call in a function body so the checker can re-type a `let`-bound
/// literal that flows into one. Read-only walk; records node ids only.
struct WriteArgPathCollector {
    arg_paths: Vec<NodeId>,
}

impl gossamer_ast::visitor::Visitor for WriteArgPathCollector {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Call { callee, args } = &expr.kind {
            if args.len() == 1 {
                if let ExprKind::Path(p) = &callee.kind {
                    let n = p.segments.len();
                    if n >= 2
                        && p.segments[n - 1].name.name.as_str() == "write"
                        && matches!(p.segments[n - 2].name.name.as_str(), "tar" | "zip")
                    {
                        self.arg_paths.push(args[0].id);
                    }
                }
            }
        }
        gossamer_ast::visitor::walk_expr(self, expr);
    }
}

fn struct_literal_positional_index(name: &str) -> Option<usize> {
    let idx = name.parse::<usize>().ok()?;
    if idx.to_string() == name {
        Some(idx)
    } else {
        None
    }
}

/// Methods a built-in trait licenses on a bound type parameter.
///
/// `None` means the name has no known surface, so a bound naming it cannot
/// decide whether a call is valid. An empty surface means the trait licenses
/// no methods of its own, which is different from having none known.
fn builtin_trait_methods(name: &str) -> Option<&'static [&'static str]> {
    crate::builtin_traits::builtin_trait(name).map(|entry| entry.bound_methods)
}

/// Sentinel-def offset for a stdlib handle named by its last path segment.
///
/// Mirrors the annotation path so `fs::DirInfo` means the same type whether
/// it is written in source or read out of a signature row.
/// Last segment of a module-qualified type path (`collections::Deque`),
/// or `None` for a bare name or a path whose leading segments are not
/// plain module names. Module segments are lowercase and carry no
/// generic arguments; a type segment is the one that may.
fn builtin_type_head(path: &TypePath) -> Option<&str> {
    let (last, modules) = path.segments.split_last()?;
    if modules.is_empty()
        || !modules.iter().all(|seg| {
            seg.generics.is_empty() && seg.name.name.chars().next().is_some_and(char::is_lowercase)
        })
    {
        return None;
    }
    Some(last.name.name.as_str())
}

/// `(sentinel offset, name)` of the runtime handle the stdlib constructor
/// `module::last` produces, if it produces one.
fn stdlib_handle_ctor(module: &[&str], last: &str) -> Option<(u32, &'static str)> {
    let module = module.strip_prefix(&["std"]).unwrap_or(module);
    PURE_HANDLES
        .iter()
        .find(|(_, _, ctors)| {
            ctors
                .iter()
                .any(|(path, name)| *name == last && *path == module)
        })
        .map(|(offset, name, _)| (*offset, *name))
        .or_else(|| {
            LEGACY_HANDLE_CTORS
                .iter()
                .find(|(path, name, _, _)| *name == last && *path == module)
                .map(|(_, _, offset, name)| (*offset, *name))
        })
}

/// `(sentinel offset, name)` of the runtime handle a written type
/// annotation names. A parameter, a struct field, and a return type carry
/// no constructor to infer from, so the annotation itself has to land on
/// the same sentinel `Adt` the constructor answers - otherwise the slot
/// stays an inference variable and method dispatch falls back to the
/// name, reaching whatever runtime symbol shares it.
fn stdlib_handle_by_path(segments: &[&str]) -> Option<(u32, &'static str)> {
    let last = *segments.last()?;
    let written = segments.strip_prefix(&["std"]).unwrap_or(segments);
    PURE_HANDLES.iter().find_map(|(offset, name, ctors)| {
        let (module, tail) = name.split_once("::")?;
        if tail != last {
            return None;
        }
        // The display name is not always the path the type is written
        // under - `http::Router` lives in `http::router` - so the module
        // its own constructors are written under counts too. Without that
        // a `router::Router` parameter stays an inference variable and its
        // dispatch falls back to the bare method name.
        let written_as_ctor = ctors.iter().any(|(path, _)| *path == written);
        (written_as_ctor
            || written.len() == 1
            || written[written.len() - 2] == module
            || module.split("::").last() == Some(written[written.len() - 2]))
        .then_some((*offset, *name))
    })
}

/// Sentinel offsets of the stdlib types that are runtime-owned handles:
/// a pointer the runtime hands back, with no text form. The pure-handle
/// band is covered by range; these are the older sentinels that predate
/// it, including the field-bearing blobs (`fs::DirInfo`,
/// `process::Output`, `http::Response`) whose fields are read through
/// accessors rather than rendered.
const OPAQUE_HANDLE_OFFSETS: &[u32] = &[
    2, 3, 4, 5, 9, 10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 23, 24, 25, 26, 27,
];

/// True when `def` names a runtime handle rather than a value with a
/// representation of its own.
fn is_opaque_handle_def(local: u32) -> bool {
    let offset = u32::MAX - local;
    (PURE_HANDLE_LO_OFFSET..=PURE_HANDLE_HI_OFFSET).contains(&offset)
        || OPAQUE_HANDLE_OFFSETS.contains(&offset)
}

fn stdlib_handle_def_offset(tail: &str) -> Option<u32> {
    Some(match tail {
        "Pattern" => 26,
        "Policy" => 13,
        "DirInfo" => 2,
        "Output" => 3,
        "ResponseStream" => 4,
        "Response" => 5,
        _ => {
            return stdlib_net_handle(tail)
                .map(|(offset, _)| offset)
                .or_else(|| stdlib_fs_handle(tail).map(|(offset, _)| offset));
        }
    })
}

/// `(sentinel offset, canonical name)` for the streaming filesystem
/// handles. One `DefId` per handle carries one registered spelling, so a
/// written `let f: fs::File` and a signature slot naming the same type
/// land on the same `Adt`.
fn stdlib_fs_handle(tail: &str) -> Option<(u32, &'static str)> {
    Some(match tail {
        "File" => (44, "fs::File"),
        "OpenOptions" => (45, "fs::OpenOptions"),
        _ => return None,
    })
}

/// `(sentinel offset, name)` of the socket a `std::net` constructor
/// answers. The path may name the type alone (`TcpStream::connect`) or
/// carry its module (`net::TcpStream::connect`), so only the type segment
/// the method hangs off is matched.
/// Whether `module::last` is one of the constructors that answers an
/// `fs::File` through a `Result`: the two `File` associated functions and
/// the terminal `OpenOptions::open`.
fn fs_file_ctor(module: &[&str], last: &str) -> bool {
    let module = module.strip_prefix(&["std"]).unwrap_or(module);
    matches!(
        (module, last),
        (["fs", "File"] | ["File"], "open" | "create")
            | (["fs", "OpenOptions"] | ["OpenOptions"], "open")
    )
}

fn net_socket_ctor(module: &[&str], last: &str) -> Option<(u32, &'static str)> {
    let type_name = *module.last()?;
    let expected = match (type_name, last) {
        ("TcpStream" | "UnixStream", "connect")
        | ("TcpListener" | "UnixListener" | "UdpSocket", "bind") => type_name,
        _ => return None,
    };
    stdlib_net_handle(expected)
}

/// Sentinel-`Adt` offset and canonical name for the opaque `std::net`
/// socket handles. The written annotation (`let s: net::TcpStream`) and a
/// signature slot naming the same type must land on one `DefId` under one
/// registered name, so both sides read this table.
fn stdlib_net_handle(tail: &str) -> Option<(u32, &'static str)> {
    Some(match tail {
        "TcpStream" => (12, "net::TcpStream"),
        "TcpListener" => (13, "net::TcpListener"),
        "UdpSocket" => (14, "net::UdpSocket"),
        "UnixStream" => (15, "net::UnixStream"),
        "UnixListener" => (16, "net::UnixListener"),
        _ => return None,
    })
}

/// A type parameter or trait object may still carry an `Fn` bound this
/// pass does not model, so only the concrete non-callable shapes are
/// listed here.
fn is_plainly_not_callable(kind: &TyKind) -> bool {
    matches!(
        kind,
        TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::String
            | TyKind::Unit
            | TyKind::Vec(_)
            | TyKind::Slice(_)
            | TyKind::Array { .. }
            | TyKind::Tuple(_)
            | TyKind::HashMap { .. }
            | TyKind::Range(..)
    )
}

/// Whether `owner` is a core type that already declares `name` itself.
///
/// The inverse-safe form of [`core_type_accepts_method`]: an owner with no
/// table here answers `false`, so only a name a core type genuinely carries is
/// reported. A type's own surface answers a call before any `impl` block a
/// program writes for it, so a block declaring one of these names declares a
/// method no call on that type can reach.
#[must_use]
pub fn core_type_declares_method(owner: &str, name: &str) -> bool {
    core_type_own_method_names(owner).is_some_and(|names| names.contains(&name))
}

/// The method names a core type carries itself, keyed by the name an `impl`
/// block's owner is written under. `None` is a type with no such surface.
///
/// One table answers both readers, which have to agree: the checker asks
/// whether a call is already answered before it consults a user block, and
/// the bytecode VM asks the same of an `impl` block as it loads one. A name
/// every type derives - equality, ordering, hashing, formatting, copying - is
/// absent here on purpose, because a written `impl` of one of those overrides
/// the derived behaviour rather than being answered before it.
fn core_type_own_method_names(owner: &str) -> Option<Vec<&'static str>> {
    // A tuple `impl` registers under the arity its receiver carries; the
    // surface is the one every tuple shares.
    let owner = if owner.starts_with("tuple_") {
        "Tuple"
    } else {
        owner
    };
    let names: Vec<&'static str> = match owner {
        "String" => STRING_METHODS.to_vec(),
        // `to_vec` converts a borrowed or fixed sequence into an owned one, so
        // it is not among a Vec's own names.
        "Vec" => SLICE_SEQUENCE_METHODS
            .iter()
            .chain(VEC_ONLY_SEQUENCE_METHODS)
            .chain(SEQUENCE_COMBINATOR_METHODS)
            .filter(|name| **name != "to_vec")
            .copied()
            .collect(),
        "Slice" => SLICE_SEQUENCE_METHODS.to_vec(),
        "Array" => SLICE_SEQUENCE_METHODS.to_vec(),
        "Map" | "BTreeMap" => MAP_METHODS.to_vec(),
        "Set" | "BTreeSet" => SET_METHODS.to_vec(),
        "Iterator" | "Range" => ITERATOR_METHODS.to_vec(),
        "Tuple" => TUPLE_METHODS.to_vec(),
        _ => return None,
    };
    Some(names)
}

/// An owner this does not model answers `true`, so a surface it has no
/// table for keeps whatever the runtime registered.
#[must_use]
pub fn core_type_accepts_method(owner: &str, name: &str) -> bool {
    // Equality, ordering, hashing, formatting, and copying are derived for
    // every type, so no per-owner table lists them.
    if AUTOMATIC_METHODS.contains(&name) {
        return true;
    }
    match owner {
        "Iterator" | "Range" => iterator_receiver_accepts_method(name),
        "Map" | "BTreeMap" => is_map_method(name),
        "Set" | "BTreeSet" => is_set_method(name),
        "Vec" => {
            is_slice_sequence_method(name)
                || is_vec_only_sequence_method(name)
                || SEQUENCE_COMBINATOR_METHODS.contains(&name)
        }
        "Slice" => is_slice_sequence_method(name),
        "Array" => is_array_sequence_method(name),
        "String" => STRING_METHODS.contains(&name),
        "Tuple" => is_tuple_method(name),
        _ => true,
    }
}

#[cfg(test)]
mod string_method_tests {
    use super::is_string_method;

    #[test]
    fn receiver_shaped_strings_functions_are_string_methods() {
        for name in [
            "bytes",
            "center",
            "chars",
            "clear",
            "contains",
            "contains_any",
            "count",
            "ends_with",
            "equal_fold",
            "find",
            "find_any",
            "lines",
            "pad_left",
            "pad_right",
            "repeat",
            "replace",
            "replacen",
            "rfind",
            "rfind_any",
            "rsplit_once",
            "slice",
            "split",
            "split_once",
            "split_whitespace",
            "splitn",
            "starts_with",
            "strip_prefix",
            "strip_suffix",
            "to_bool",
            "to_f64",
            "to_i64",
            "to_lowercase",
            "to_title",
            "to_uppercase",
            "trim",
            "trim_end",
            "trim_end_matches",
            "trim_matches",
            "trim_start",
            "trim_start_matches",
            "truncate",
        ] {
            assert!(is_string_method(name), "{name} should be a String method");
        }
    }

    #[test]
    fn strings_join_stays_vec_only() {
        assert!(!is_string_method("join"));
    }
}
