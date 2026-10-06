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

/// The name of the type, or of the trait bounding a type parameter, that
/// each receiver in `calls` has, for the method calls whose labelled or
/// defaulted arguments wait on it. Diagnostics are left to the real check.
#[must_use]
pub(crate) fn receiver_owners(
    source: &SourceFile,
    resolutions: &Resolutions,
    calls: impl IntoIterator<Item = NodeId>,
) -> HashMap<NodeId, String> {
    let mut tcx = TyCtxt::new();
    let mut checker = TypeChecker::new(&mut tcx, resolutions);
    checker.receiver_owner_watch = calls.into_iter().collect();
    checker.collect_receiver_owners(source)
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
    fn collect_receiver_owners(mut self, source: &SourceFile) -> HashMap<NodeId, String> {
        let active = gossamer_resolve::without_inactive_items(source);
        let source = active.as_ref().unwrap_or(source);
        self.collect_import_targets(&source.uses);
        self.assoc = gossamer_ast::AssocIndex::build(source);
        self.collect_signatures(&source.items);
        for item in &source.items {
            self.check_item(item);
        }
        self.receiver_owners
    }

    fn run(mut self, source: &SourceFile) -> (TypeTable, Vec<TypeDiagnostic>) {
        // The items the resolver resolved, and no others.
        let active = gossamer_resolve::without_inactive_items(source);
        let source = active.as_ref().unwrap_or(source);
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
        self.check_deferred_try_conversions();
        self.check_deferred_literal_type_mismatches();
        self.check_deferred_scalar_method_rejections();
        self.record_deferred_method_owners();
        self.check_deferred_binding_callbacks();
        self.check_deferred_wrapping_operands();
        self.check_deferred_mutating_receivers();
        self.check_deferred_private_fields();
        self.check_deferred_field_receivers();
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

/// An associated-type equality constraint on a generic parameter: the
/// parameter's slot, the associated type's name, and the type it equals.
type AssocConstraint = (usize, String, Ty);

/// Hard limit on type-checker recursion depth. Mirrors the parser's
/// guard and keeps adversarial input that survives parsing from
/// blowing the C stack inside [`TypeChecker::check_expr`].
const RECURSION_LIMIT: u32 = 256;
/// The parts of a method call that identify the call itself, as opposed to
/// the receiver and arguments it is applied to.
struct MethodCallSite<'a> {
    call_id: NodeId,
    /// Source range of the whole call, receiver through closing parenthesis.
    call_span: Span,
    method: &'a str,
    /// Source range of the method name, so a diagnostic about the method
    /// points at it rather than at the receiver.
    name_span: Span,
    generics: &'a [AstGenericArg],
}

/// Signature of a trait function as seen through a type parameter bounded by
/// the trait. `Self` is the placeholder `Param` at index zero, which a use
/// site substitutes with the bounded parameter.
#[derive(Clone)]
struct TraitFnSelfSig {
    /// The receiver the function declares, if any.
    receiver: Option<gossamer_ast::Receiver>,
    /// Non-receiver parameters and return.
    sig: FnSig,
}

/// Signature of a method on a concrete user type whose own type parameters
/// reach its return. Parameter and return types carry those parameters as
/// rigid `Param` slots, indexed from zero.
#[derive(Clone)]
struct OwnGenericMethodSig {
    /// Number of type parameters the method declares.
    generics: usize,
    /// Non-receiver parameter types.
    params: Vec<Ty>,
    /// Declared return type.
    ret: Ty,
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

/// Sentinel-offset band of the `std::sync` handles (`sync::RwLock` predates
/// it and sits in the pure band) and the shared `I64Vec` word buffer. Each is
/// a runtime pointer with no text form and a closed method table, typed by
/// [`TypeChecker::sync_handle_method_ret`] and [`TypeChecker::heap_buffer_method_ret`].
const SYNC_HANDLE_LO_OFFSET: u32 = 50;
const SYNC_HANDLE_HI_OFFSET: u32 = 58;

/// Sentinel offsets of the `trace` span handles a `Tracer` hands out.
const TRACE_SPAN_OFFSET: u32 = 59;
const TRACE_ENDED_SPAN_OFFSET: u32 = 60;

/// Sentinel offset of the `U8Vec` byte buffer, which predates the bands.
const U8_VEC_OFFSET: u32 = 20;

/// Widest sentinel offset any stdlib handle occupies, the handle bands and
/// the pre-band handles alike. A receiver inside this span whose display name
/// is module-qualified answers a closed method table, which is what lets an
/// unknown name on one be named at the call site.
pub(crate) const HANDLE_SENTINEL_SPAN: u32 = TRACE_ENDED_SPAN_OFFSET;

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
        &[(&["rand", "Rng"], "new"), (&["math", "rand", "Rng"], "new")],
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
    // The `std::sync` band. Their constructors are typed, arguments and
    // all, by `sync_call_ret_ty`; these rows name the types an annotation
    // resolves to.
    (
        50,
        "sync::Mutex",
        &[(&["sync", "Mutex"], "new"), (&["Mutex"], "new")],
    ),
    (
        51,
        "sync::Once",
        &[(&["sync", "Once"], "new"), (&["Once"], "new")],
    ),
    (
        52,
        "sync::WaitGroup",
        &[(&["sync", "WaitGroup"], "new"), (&["WaitGroup"], "new")],
    ),
    (
        53,
        "sync::Barrier",
        &[(&["sync", "Barrier"], "new"), (&["Barrier"], "new")],
    ),
    (
        54,
        "sync::AtomicI64",
        &[(&["sync", "AtomicI64"], "new"), (&["AtomicI64"], "new")],
    ),
    (
        55,
        "sync::AtomicI32",
        &[(&["sync", "AtomicI32"], "new"), (&["AtomicI32"], "new")],
    ),
    (
        56,
        "sync::AtomicU64",
        &[(&["sync", "AtomicU64"], "new"), (&["AtomicU64"], "new")],
    ),
    (
        57,
        "sync::AtomicBool",
        &[(&["sync", "AtomicBool"], "new"), (&["AtomicBool"], "new")],
    ),
    // A shared buffer of i64 words; its constructor is typed by
    // `heap_buffer_call_ret_ty`.
    (SYNC_HANDLE_HI_OFFSET, "I64Vec", &[(&["I64Vec"], "new")]),
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

/// The `time::Duration` method surface: its unit accessors.
const DURATION_METHODS: &[&str] = &[
    "as_micros",
    "as_millis",
    "as_nanos",
    "as_secs",
    "as_secs_f64",
];

/// The `time::Instant` method surface.
const INSTANT_METHODS: &[&str] = &["duration_since", "elapsed", "elapsed_ms"];

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
    /// Outer names each `let`-bound closure mentions, keyed by the binding.
    /// A call that hands such a closure beside a `&mut` argument aliases the
    /// referent when the closure captures its root.
    closure_captures: Vec<HashMap<Box<str>, HashSet<String>>>,
    /// Functions declared in an `unsafe extern "C"` block, with their names.
    foreign_fns: HashMap<DefId, String>,
    /// The C symbol of each `#[export]` function checked so far, with the
    /// function that claimed it.
    export_symbols: HashMap<String, String>,
    /// Structs declared `#[repr(C)]`, which may cross the C boundary.
    repr_c_structs: HashSet<DefId>,
    /// Foreign types declared `type Name` in an extern block, which have no
    /// Gossamer value and are reached only through `ffi::Ptr`.
    opaque_types: HashSet<DefId>,
    /// `ffi` memory operations whose pointee is checked once the enclosing
    /// body's inference has settled: `(operation, report name, span, argument
    /// types, result type)`.
    pending_ffi_types: Vec<(&'static str, &'static str, gossamer_lex::Span, Vec<Ty>, Ty)>,
    /// C globals declared `static NAME: T` in an extern block, with their
    /// names.
    foreign_statics: HashMap<DefId, String>,
    /// The argument expressions of `ffi::addr_of` calls, the one place a
    /// foreign static may be named.
    addr_of_args: HashSet<NodeId>,
    /// Layout queries and union operations checked once the enclosing body's
    /// inference has settled.
    pending_ffi_layouts: Vec<foreign::PendingLayout>,
    /// How many `unsafe { }` blocks enclose the expression being checked.
    unsafe_depth: u32,
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
    /// Signatures of methods on concrete user types whose own type
    /// parameters reach the return, keyed like [`Self::method_ret_types`].
    /// Each call site instantiates those parameters afresh.
    own_generic_method_sigs: HashMap<(String, String, usize), OwnGenericMethodSig>,
    /// Signatures of the functions a non-generic trait declares, keyed by
    /// `(trait, function)`, for calls through a bounded type parameter.
    trait_fn_self_sigs: HashMap<(String, String), TraitFnSelfSig>,
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
    /// Field reads from written source whose receiver was an inference
    /// variable when checked; one still unresolved once inference settles
    /// names a type nothing decided.
    deferred_field_receivers: Vec<(Ty, String, Span)>,
    /// The type variable each unannotated closure parameter starts as.
    unannotated_closure_params: Vec<Ty>,
    /// Set while a stdlib signature is read as a template, where each
    /// catalogue type parameter stands for a `Param` a call binds.
    catalog_params_as_params: bool,
    /// Assignment mismatches whose outer shapes are already incompatible but
    /// whose literal elements need integer/float defaulting before their
    /// rendered types are useful to the user.
    deferred_type_mismatches: Vec<(Ty, Ty, Span)>,
    /// `.into()` call sites, recorded as (receiver, result, span). The
    /// target is a fresh variable when the call is checked, so whether a
    /// conversion exists can only be decided once unification has pinned
    /// it.
    deferred_into_conversions: Vec<(Ty, Ty, Span)>,
    /// `(operand error, enclosing error, span)` for each `?` on a `Result`,
    /// audited once unification has settled both types.
    deferred_try_conversions: Vec<(Ty, Ty, Span)>,
    deferred_literal_type_mismatches: Vec<(Ty, &'static str, Span)>,
    /// `x.name()` on an unsuffixed numeric literal, recorded as
    /// (receiver, method, span). The literal's width is pinned by
    /// defaulting after the last item is checked, so whether the receiver
    /// is a scalar - and which scalar to name - is only known then.
    deferred_scalar_method_rejections: Vec<(Ty, String, Span)>,
    /// Method calls on an unsuffixed numeric literal, recorded as (call,
    /// receiver, method). Which `impl` block the call reaches depends on the
    /// width defaulting gives the literal, so the owner is recorded then.
    deferred_method_owners: Vec<(NodeId, Ty, String)>,
    /// Method calls whose receiver's type the named-argument rewrite waits
    /// on, and the type (or bounding trait) each receiver turned out to have.
    receiver_owner_watch: HashSet<NodeId>,
    receiver_owners: HashMap<NodeId, String>,
    /// Closures handed to a `[rust-bindings]` callback parameter, as (type,
    /// callee, span). A binding's signature does not name the closure's
    /// types, so they are checked once inference has settled them.
    deferred_binding_callbacks: Vec<(Ty, String, Span)>,
    /// Wrapping arithmetic operands still being inferred when checked, with
    /// the other operand's type, the operator, and its span. Literal defaulting
    /// settles them, so the integer requirement is checked afterwards.
    deferred_wrapping_operands: Vec<(Ty, Ty, &'static str, Span)>,
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
    /// A generic function's associated-type equality constraints
    /// (`S: Source<Out = T>`): the constrained parameter's position, the
    /// associated type's name, and the type it equals, in the function's
    /// own generic scope.
    fn_assoc_constraints: HashMap<gossamer_resolve::DefId, Vec<AssocConstraint>>,
    /// The same constraints for a method, keyed by its owner and name, with
    /// slots numbered as the method's instantiation numbers them: the
    /// `impl` block's parameters first, then the method's own.
    method_assoc_constraints: HashMap<(String, String), Vec<AssocConstraint>>,
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
    /// Const generic parameters of generic functions, types, and impl methods.
    const_generics: ConstGenerics,
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
    current_const_generic_scope: HashMap<String, (crate::ParamIdx, Ty)>,
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
    /// Types declared at the unit root, whose identity is their bare name.
    root_type_names: std::collections::HashSet<String>,
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
    /// Every method defined on a type, keyed by the type's dispatch name and
    /// the method name, with where it sits and which block holds it.
    claimed_methods: HashMap<(String, String), (Span, String)>,
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
/// The const positions of a struct literal's type and what its fields have
/// said about each: a value, or the enclosing body's own parameter.
struct LiteralConsts {
    mask: Vec<bool>,
    values: Vec<Option<i128>>,
    forwarded: Vec<Option<crate::ParamIdx>>,
}

impl LiteralConsts {
    fn new(mask: Vec<bool>) -> Self {
        let n = mask.len();
        Self {
            mask,
            values: vec![None; n],
            forwarded: vec![None; n],
        }
    }

    fn has_const_positions(&self) -> bool {
        self.mask.iter().any(|is_const| *is_const)
    }

    /// Whether `field_ty` is an array whose length names a const position.
    fn is_const_array_field(&self, tcx: &TyCtxt, field_ty: Ty) -> bool {
        let mut ty = field_ty;
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        matches!(
            tcx.kind_of(ty),
            TyKind::Array { len: crate::ArrayLen::Param(idx), .. }
                if self.mask.get(idx.0 as usize).copied().unwrap_or(false)
        )
    }

    fn infer_from_field(&mut self, checker: &TypeChecker<'_>, field_ty: Ty, value_ty: Ty) {
        let Some((idx, len)) = checker.infer_array_const_len(field_ty, value_ty) else {
            return;
        };
        match len {
            crate::ArrayLen::Concrete(value) => {
                if let Some(slot) = self.values.get_mut(idx) {
                    slot.get_or_insert(value as i128);
                }
            }
            crate::ArrayLen::Param(param) => {
                if let Some(slot) = self.forwarded.get_mut(idx) {
                    slot.get_or_insert(param);
                }
            }
        }
    }

    /// `field_ty` with the const positions known so far substituted.
    fn apply(&self, checker: &mut TypeChecker<'_>, field_ty: Ty, types: &[Ty]) -> Ty {
        let substituted = checker.subst_generics_in_ty(field_ty, types, &self.values);
        checker.forward_const_params(substituted, &self.forwarded)
    }
}

/// What the checker knows about const generic parameters, gathered while
/// signatures are registered and read back at every use site.
#[derive(Default)]
struct ConstGenerics {
    /// Declared type of each const generic parameter of each generic function
    /// or type, by parameter position (`None` at a type position). Lets a use
    /// site record a `GenericArg::Const` rather than a type argument at each
    /// const position.
    param_tys: HashMap<gossamer_resolve::DefId, Vec<Option<Ty>>>,
    /// For a method of a generic `impl` block, keyed by owner and method, the
    /// impl's const parameters in declaration order: each one's position in
    /// the impl's parameter list, which is the position the self type's
    /// arguments carry it at, and its declared type.
    impl_method_params: HashMap<(String, String), Vec<(usize, Ty)>>,
}

struct GenericScope {
    types: HashMap<String, (crate::ParamIdx, Box<str>)>,
    consts: HashMap<String, (crate::ParamIdx, Ty)>,
    bounds: Vec<Vec<String>>,
    assoc_bindings: HashMap<(String, String), gossamer_ast::Type>,
}

impl<'a> TypeChecker<'a> {
    // One initializer per field and no control flow: the length follows the
    // number of tables the checker carries, which is not what the lint
    // measures.
    #[allow(clippy::too_many_lines)]
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
            closure_captures: vec![HashMap::new()],
            pending_ffi_types: Vec::new(),
            foreign_statics: HashMap::new(),
            addr_of_args: HashSet::new(),
            pending_ffi_layouts: Vec::new(),
            opaque_types: HashSet::new(),
            foreign_fns: HashMap::new(),
            export_symbols: HashMap::new(),
            repr_c_structs: HashSet::new(),
            unsafe_depth: 0,
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
            own_generic_method_sigs: HashMap::new(),
            trait_fn_self_sigs: HashMap::new(),
            method_arities: HashMap::new(),
            inherent_method_requires_mut: HashMap::new(),
            trait_impl_method_requires_mut: HashMap::new(),
            deferred_structural: Vec::new(),
            deferred_mutating_receivers: Vec::new(),
            deferred_private_fields: Vec::new(),
            deferred_field_receivers: Vec::new(),
            unannotated_closure_params: Vec::new(),
            catalog_params_as_params: false,
            deferred_type_mismatches: Vec::new(),
            deferred_into_conversions: Vec::new(),
            deferred_try_conversions: Vec::new(),
            deferred_literal_type_mismatches: Vec::new(),
            deferred_scalar_method_rejections: Vec::new(),
            deferred_method_owners: Vec::new(),
            receiver_owner_watch: HashSet::new(),
            receiver_owners: HashMap::new(),
            deferred_binding_callbacks: Vec::new(),
            deferred_wrapping_operands: Vec::new(),
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
            fn_assoc_constraints: HashMap::new(),
            method_assoc_constraints: HashMap::new(),
            adt_param_bounds: HashMap::new(),
            user_type_defs: HashMap::new(),
            deferred_adt_bounds: Vec::new(),
            const_generics: ConstGenerics::default(),
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
            root_type_names: std::collections::HashSet::new(),
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
            claimed_methods: HashMap::new(),
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
                gossamer_ast::GenericParam::Const { name, ty, .. } => {
                    let const_ty = self.type_from_ast(ty);
                    self.record(ty.id, const_ty);
                    self.current_const_generic_scope
                        .insert(name.name.clone(), (crate::ParamIdx(i as u32), const_ty));
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
                gossamer_ast::GenericParam::Const { name, ty, .. } => {
                    let const_ty = self.type_from_ast(ty);
                    self.record(ty.id, const_ty);
                    self.current_const_generic_scope
                        .insert(name.name.clone(), (idx, const_ty));
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

    /// Enters a function's generic scope the way its body is checked: inside
    /// an `impl` with generics of its own, the block's parameters come first,
    /// so a signature naming the block's `N` or `T` reads the slot the body
    /// and the struct's fields use.
    fn enter_fn_generic_scope(&mut self, generics: &gossamer_ast::Generics) -> GenericScope {
        match self.current_impl_generics.clone() {
            Some(impl_generics) if !impl_generics.params.is_empty() => {
                self.enter_generic_scope_combined(&impl_generics, generics)
            }
            _ => self.enter_generic_scope(generics),
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

    /// The `Name = Type` constraints `bindings` write, lowered in the generic
    /// scope that is current: each is the slot of the parameter it constrains,
    /// the associated type's name, and the type that parameter's impl must
    /// supply for it.
    fn assoc_constraints_in_scope(
        &mut self,
        bindings: &HashMap<(String, String), gossamer_ast::Type>,
    ) -> Vec<AssocConstraint> {
        let mut constraints: Vec<AssocConstraint> = Vec::new();
        for ((param, assoc), ty) in bindings {
            let Some((idx, _)) = self.current_generic_scope.get(param).cloned() else {
                continue;
            };
            let target = self.type_from_ast(ty);
            constraints.push((idx.0 as usize, assoc.clone(), target));
        }
        constraints.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        constraints
    }

    /// The type `ty` supplies for its associated type `assoc`: a built-in
    /// iterator's element for `Item`, or what the impl of a declared type
    /// writes, read against the type's own arguments.
    fn assoc_type_supplied(&mut self, ty: Ty, assoc: &str) -> Option<Ty> {
        let resolved = self.infer.resolve(self.tcx, ty);
        let (def, substs) = match self.tcx.kind(resolved)? {
            TyKind::Iterator(item) if assoc == "Item" => return Some(*item),
            TyKind::Adt { def, substs } => (*def, substs.clone()),
            _ => return None,
        };
        let name = self.tcx.def_name(def)?.to_string();
        let written = self.assoc.assoc_type_for_self(&name, assoc).cloned()?;
        let params = self
            .assoc
            .assoc_type_self_params(&name, assoc)
            .map(<[Option<String>]>::to_vec)
            .unwrap_or_default();
        if params.is_empty() {
            return Some(self.type_from_ast(&written));
        }
        let prior = std::mem::take(&mut self.current_generic_scope);
        for (position, param) in params.iter().enumerate() {
            if let Some(param) = param {
                self.current_generic_scope.insert(
                    param.clone(),
                    (
                        crate::ParamIdx(position as u32),
                        param.clone().into_boxed_str(),
                    ),
                );
            }
        }
        let template = self.type_from_ast(&written);
        self.current_generic_scope = prior;
        let (subst_tys, subst_consts) = self.adt_subst_vectors(&substs);
        Some(self.subst_generics_in_ty(template, &subst_tys, &subst_consts))
    }

    /// Unifies each constrained parameter's associated type with what the
    /// constraint names, under one call's instantiation `vars`, so
    /// `S: Source<Out = T>` decides `T` from the `S` an argument supplies.
    fn apply_assoc_constraints(
        &mut self,
        constraints: &[(usize, String, Ty)],
        vars: &[Ty],
        span: Span,
    ) {
        for (position, assoc, target) in constraints {
            let Some(&var) = vars.get(*position) else {
                continue;
            };
            let Some(supplied) = self.assoc_type_supplied(var, assoc) else {
                continue;
            };
            let expected = self.subst_generics_in_ty(*target, vars, &[]);
            self.unify(expected, supplied, span);
        }
    }

    /// Verifies each instantiated type parameter of a generic call
    /// satisfies its declared trait bounds: the concrete type must carry
    /// an `impl Bound for Type` (or `Bound` is a recognised built-in
    /// trait). A still-unresolved parameter or a non-named type is left
    /// for argument unification to report; only a concrete type with a
    /// definitely-missing impl is flagged.
    fn check_trait_bounds(&mut self, def: gossamer_resolve::DefId, vars: &[Ty], span: Span) {
        // `S: Source<Out = T>` decides `T` from the argument `S` names: the
        // concrete type's impl says what its `Out` is.
        let constraints = self
            .fn_assoc_constraints
            .get(&def)
            .cloned()
            .unwrap_or_default();
        self.apply_assoc_constraints(&constraints, vars, span);
        let Some(bounds) = self.fn_param_bounds.get(&def).cloned() else {
            return;
        };
        for (i, var) in vars.iter().enumerate() {
            let resolved = self.infer.resolve(self.tcx, *var);
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
            let (name, kind) = match item {
                ImplItem::Fn(fn_decl) => (fn_decl.name.name.clone(), "fn"),
                ImplItem::Type { name, .. } => (name.name.clone(), "type"),
                ImplItem::Const { name, .. } => (name.name.clone(), "const"),
            };
            if declared.contains(&name) {
                continue;
            }
            // An operator trait's `Output` restates what its method answers;
            // one that agrees was folded away before checking, so what reaches
            // here disagrees with the method's return.
            if kind == "type"
                && name == "Output"
                && let ImplItem::Type { ty: output, .. } = item
                && crate::builtin_traits::builtin_trait(&trait_name)
                    .is_some_and(|t| t.kind == crate::builtin_traits::BuiltinTraitKind::Operator)
            {
                let method = crate::builtin_traits::builtin_trait(&trait_name)
                    .and_then(|t| t.impl_items.first().copied())
                    .unwrap_or_default();
                let returns = decl.items.iter().find_map(|i| match i {
                    ImplItem::Fn(f) if f.name.name == method => f.ret.as_ref(),
                    _ => None,
                });
                let expected = returns.map_or_else(|| "()".to_string(), render_ast_type);
                self.emit(
                    TypeError::TypeMismatch {
                        expected,
                        found: render_ast_type(output),
                    },
                    output.span,
                );
                continue;
            }
            self.emit(
                TypeError::ImplItemNotInTrait {
                    trait_name: trait_name.clone(),
                    ty: self_ty.clone(),
                    item: name,
                    kind,
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
        let mut names: Vec<String> = builtin_trait_impl_items(trait_name)?
            .iter()
            .map(ToString::to_string)
            .collect();
        // An iterator may state the element type it yields.
        if trait_name == "Iterator" {
            names.push("Item".to_string());
        }
        Some(names)
    }

    /// Reports a method whose name another `impl` block already defines on
    /// the same type. `Display` and `Debug` each answer `fmt` for their own
    /// rendering channel, which a call reaches by format spec, not by name.
    fn check_method_uniqueness(&mut self, decl: &ImplDecl, module_path: &[String]) {
        let trait_name = decl
            .trait_ref
            .as_ref()
            .and_then(|trait_ref| trait_ref.path.segments.last())
            .map(|segment| segment.name.name.clone());
        if matches!(trait_name.as_deref(), Some("Display" | "Debug")) {
            return;
        }
        // The definition the self type resolves to separates two modules'
        // `Point`s however either block spells the path.
        let ty = match self.resolutions.get(decl.self_ty.id) {
            Some(Resolution::Def { def, .. }) => format!("{def:?}"),
            _ => qualified_type_name(module_path, &impl_self_ty_name(decl)),
        };
        let block = match &trait_name {
            Some(name) => format!("`impl {name} for {}`", written_type_name(&decl.self_ty)),
            None => "an inherent impl".to_string(),
        };
        for item in &decl.items {
            let ImplItem::Fn(fn_decl) = item else {
                continue;
            };
            let key = (ty.clone(), fn_decl.name.name.clone());
            match self.claimed_methods.get(&key) {
                Some((claimed, _)) if *claimed == fn_decl.span => {}
                Some((_, first)) => {
                    let first = first.clone();
                    self.emit(
                        TypeError::DuplicateMethod {
                            ty: written_type_name(&decl.self_ty),
                            method: fn_decl.name.name.clone(),
                            first,
                        },
                        fn_decl.span,
                    );
                }
                None => {
                    self.claimed_methods
                        .insert(key, (fn_decl.span, block.clone()));
                }
            }
        }
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
        if let Some(param) = blanket_impl_param(decl) {
            self.emit(TypeError::BlanketImpl { trait_name, param }, span);
            return;
        }
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

    /// Reports each direct supertrait of the implemented trait that the
    /// implementing type does not implement too.
    pub(super) fn check_supertrait_impls(
        &mut self,
        decl: &ImplDecl,
        module_path: &[String],
        span: Span,
    ) {
        let Some(trait_name) = decl.trait_ref.as_ref().and_then(|b| b.trait_name()) else {
            return;
        };
        let Some(supertraits) = self.trait_supertraits.get(trait_name).cloned() else {
            return;
        };
        let own = impl_self_ty_name(decl);
        let qualified = qualified_type_name(module_path, &own);
        for supertrait in supertraits {
            if self.bound_is_satisfied(&supertrait, &own)
                || self.bound_is_satisfied(&supertrait, &qualified)
            {
                continue;
            }
            self.emit(
                TypeError::MissingSupertraitImpl {
                    trait_name: trait_name.to_string(),
                    supertrait,
                    ty: written_type_name(&decl.self_ty),
                },
                span,
            );
        }
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
    fn infer_array_const_len(&self, param_ty: Ty, arg_ty: Ty) -> Option<(usize, crate::ArrayLen)> {
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
        let TyKind::Array { len, .. } = self.tcx.kind_of(a) else {
            return None;
        };
        Some((idx, *len))
    }

    /// Every const parameter position `param_ty` names that `arg_ty` supplies a
    /// value for, walking the two types together: an array length, or a const
    /// argument of a generic type (`Grid<N>` against `Grid<2>`), through
    /// references, sequences, tuples, and type arguments.
    fn infer_const_args(&self, param_ty: Ty, arg_ty: Ty, out: &mut Vec<(usize, crate::ArrayLen)>) {
        let arg_ty = self.infer.resolve(self.tcx, arg_ty);
        match (self.tcx.kind_of(param_ty), self.tcx.kind_of(arg_ty)) {
            (TyKind::Ref { inner: p, .. }, TyKind::Ref { inner: a, .. })
            | (TyKind::Vec(p), TyKind::Vec(a))
            | (TyKind::Slice(p), TyKind::Slice(a)) => self.infer_const_args(*p, *a, out),
            (TyKind::Ref { inner: p, .. }, _) => self.infer_const_args(*p, arg_ty, out),
            (TyKind::Array { elem: pe, len: pl }, TyKind::Array { elem: ae, len: al })
            | (
                TyKind::Simd {
                    elem: pe,
                    lanes: pl,
                },
                TyKind::Simd {
                    elem: ae,
                    lanes: al,
                },
            ) => {
                if let crate::ArrayLen::Param(idx) = pl {
                    out.push((idx.0 as usize, *al));
                }
                self.infer_const_args(*pe, *ae, out);
            }
            (TyKind::Tuple(ps), TyKind::Tuple(ars)) => {
                for (p, a) in ps.iter().zip(ars.iter()) {
                    self.infer_const_args(*p, *a, out);
                }
            }
            (
                TyKind::Adt {
                    def: pd,
                    substs: ps,
                },
                TyKind::Adt {
                    def: ad,
                    substs: ars,
                },
            ) if pd == ad => {
                for (p, a) in ps.as_slice().iter().zip(ars.as_slice().iter()) {
                    match (p, a) {
                        (crate::GenericArg::ConstParam(idx), crate::GenericArg::Const(value)) => {
                            if let Ok(value) = usize::try_from(*value) {
                                out.push((idx.0 as usize, crate::ArrayLen::Concrete(value)));
                            }
                        }
                        (
                            crate::GenericArg::ConstParam(idx),
                            crate::GenericArg::ConstParam(from),
                        ) => out.push((idx.0 as usize, crate::ArrayLen::Param(*from))),
                        (crate::GenericArg::Type(p), crate::GenericArg::Type(a)) => {
                            self.infer_const_args(*p, *a, out);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// `ty` with each array length still naming a callee const parameter that
    /// the call forwards from the caller's own parameter renamed to the
    /// caller's position.
    fn forward_const_params(&mut self, ty: Ty, forwarded: &[Option<crate::ParamIdx>]) -> Ty {
        if forwarded.iter().all(Option::is_none) {
            return ty;
        }
        match self.tcx.kind_of(ty).clone() {
            TyKind::Array { elem, len } => {
                let elem = self.forward_const_params(elem, forwarded);
                let len = match len {
                    crate::ArrayLen::Param(idx) => forwarded
                        .get(idx.0 as usize)
                        .copied()
                        .flatten()
                        .map_or(len, crate::ArrayLen::Param),
                    concrete @ crate::ArrayLen::Concrete(_) => concrete,
                };
                self.tcx.intern(TyKind::Array { elem, len })
            }
            TyKind::Simd { elem, lanes } => {
                let elem = self.forward_const_params(elem, forwarded);
                let lanes = match lanes {
                    crate::ArrayLen::Param(idx) => forwarded
                        .get(idx.0 as usize)
                        .copied()
                        .flatten()
                        .map_or(lanes, crate::ArrayLen::Param),
                    concrete @ crate::ArrayLen::Concrete(_) => concrete,
                };
                self.tcx.intern(TyKind::Simd { elem, lanes })
            }
            TyKind::Ref { mutability, inner } => {
                let inner = self.forward_const_params(inner, forwarded);
                self.tcx.intern(TyKind::Ref { mutability, inner })
            }
            TyKind::Vec(elem) => {
                let elem = self.forward_const_params(elem, forwarded);
                self.tcx.intern(TyKind::Vec(elem))
            }
            TyKind::Slice(elem) => {
                let elem = self.forward_const_params(elem, forwarded);
                self.tcx.intern(TyKind::Slice(elem))
            }
            TyKind::Tuple(elems) => {
                let elems = elems
                    .iter()
                    .map(|elem| self.forward_const_params(*elem, forwarded))
                    .collect();
                self.tcx.intern(TyKind::Tuple(elems))
            }
            TyKind::Adt { def, substs } => {
                let args = substs
                    .as_slice()
                    .iter()
                    .map(|arg| match arg {
                        crate::GenericArg::Type(t) => {
                            crate::GenericArg::Type(self.forward_const_params(*t, forwarded))
                        }
                        crate::GenericArg::ConstParam(idx) => {
                            forwarded.get(idx.0 as usize).copied().flatten().map_or(
                                crate::GenericArg::ConstParam(*idx),
                                crate::GenericArg::ConstParam,
                            )
                        }
                        other @ crate::GenericArg::Const(_) => other.clone(),
                    })
                    .collect();
                self.tcx.intern(TyKind::Adt {
                    def,
                    substs: crate::Substs::from_args(args),
                })
            }
            _ => ty,
        }
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
            TyKind::Simd { elem, lanes } => {
                let new_elem = self.subst_generics_in_ty(elem, substs, const_substs);
                let new_lanes = subst_array_len(lanes, const_substs);
                if new_elem == elem && new_lanes == lanes {
                    ty
                } else {
                    self.tcx.intern(TyKind::Simd {
                        elem: new_elem,
                        lanes: new_lanes,
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
                crate::GenericArg::ConstParam(idx) => {
                    match const_substs.get(idx.0 as usize).copied().flatten() {
                        Some(value) => crate::GenericArg::Const(value),
                        None => crate::GenericArg::ConstParam(*idx),
                    }
                }
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
                other @ (crate::GenericArg::Const(_) | crate::GenericArg::ConstParam(_)) => {
                    other.clone()
                }
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
                self.deep_resolve_wrap(resolved, elem, |elem| TyKind::Array { elem, len })
            }
            TyKind::Simd { elem, lanes } => {
                self.deep_resolve_wrap(resolved, elem, |elem| TyKind::Simd { elem, lanes })
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
    fn deep_resolve_wrap(&mut self, resolved: Ty, elem: Ty, wrap: impl FnOnce(Ty) -> TyKind) -> Ty {
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
        self.closure_captures.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.mut_scopes.pop();
        self.consumed_iterators.pop();
        self.mutable_borrows.pop();
        self.shared_borrows.pop();
        self.reference_origins.pop();
        self.closure_captures.pop();
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

    fn record_closure_captures(&mut self, pattern: &Pattern, init: &Expr) {
        let PatternKind::Ident { name, .. } = &pattern.kind else {
            return;
        };
        let Some(scope) = self.closure_captures.last_mut() else {
            return;
        };
        match &init.kind {
            ExprKind::Closure { params, body, .. } => {
                let names = closure_outer_names(params, body);
                scope.insert(name.name.clone().into_boxed_str(), names);
            }
            _ => {
                scope.remove(name.name.as_str());
            }
        }
    }

    /// Outer names the closure bound to local `name` mentions, when `name`
    /// was bound from a closure literal.
    fn closure_binding_captures(&self, name: &str) -> Option<&HashSet<String>> {
        self.closure_captures
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
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
            // A function item nested in the value - `Some(dbl)` where an
            // `Option<Fn(f64) -> f64>` is expected - coerces at its own
            // position the way a bare one does.
            _ => self
                .coerce_nested_fn_items(lhs_resolved, rhs_resolved)
                .map(|coerced| self.infer.unify(self.tcx, lhs_resolved, coerced)),
        };
        let result = callable_result
            .unwrap_or_else(|| self.infer.unify(self.tcx, lhs_resolved, rhs_resolved));
        match result {
            Ok(()) => {}
            Err(err) => self.report_unify(err, lhs, rhs, span),
        }
    }

    /// Records the callable-shaped type on each expression that produces a
    /// value holding a function item where `expected` names a callable,
    /// descending through block tails, `if` branches, and `match` arms, so the
    /// expression that builds the value is lowered with the slot's shape.
    fn record_fn_item_coercion(&mut self, expr: &Expr, expected: Ty) {
        match &expr.kind {
            ExprKind::Block(block) => {
                if let Some(tail) = &block.tail {
                    self.record_fn_item_coercion(tail, expected);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.record_fn_item_coercion(then_branch, expected);
                if let Some(else_branch) = else_branch {
                    self.record_fn_item_coercion(else_branch, expected);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.record_fn_item_coercion(&arm.body, expected);
                }
            }
            _ => {}
        }
        if let Some(found) = self.table.get(expr.id)
            && let Some(coerced) = self.coerce_nested_fn_items(expected, found)
        {
            self.record(expr.id, coerced);
        }
    }

    /// `found` with every function item that stands where `expected` names a
    /// callable replaced by the item's instantiated signature, looking through
    /// generic arguments, tuples, `Vec`s, and arrays. `None` when no nested
    /// position needed the coercion.
    fn coerce_nested_fn_items(&mut self, expected: Ty, found: Ty) -> Option<Ty> {
        let expected = self.infer.resolve(self.tcx, expected);
        let found = self.infer.resolve(self.tcx, found);
        let expected_kind = self.tcx.kind(expected).cloned()?;
        let found_kind = self.tcx.kind(found).cloned()?;
        match (expected_kind, found_kind) {
            (TyKind::FnPtr(_) | TyKind::FnTrait(_), TyKind::FnDef { def, substs }) => {
                let sig = self.instantiated_fn_item_sig(def, &substs)?;
                Some(self.tcx.intern(TyKind::FnPtr(sig)))
            }
            (
                TyKind::Adt {
                    def: expected_def,
                    substs: expected_substs,
                },
                TyKind::Adt {
                    def: found_def,
                    substs: found_substs,
                },
            ) if expected_def == found_def
                && expected_substs.as_slice().len() == found_substs.as_slice().len() =>
            {
                let mut changed = false;
                let mut args = Vec::with_capacity(found_substs.as_slice().len());
                for (e, f) in expected_substs
                    .as_slice()
                    .iter()
                    .zip(found_substs.as_slice())
                {
                    match (e, f) {
                        (crate::GenericArg::Type(e), crate::GenericArg::Type(f)) => {
                            match self.coerce_nested_fn_items(*e, *f) {
                                Some(coerced) => {
                                    changed = true;
                                    args.push(crate::GenericArg::Type(coerced));
                                }
                                None => args.push(crate::GenericArg::Type(*f)),
                            }
                        }
                        (_, other) => args.push(other.clone()),
                    }
                }
                changed.then(|| {
                    self.tcx.intern(TyKind::Adt {
                        def: found_def,
                        substs: crate::Substs::from_args(args),
                    })
                })
            }
            (TyKind::Tuple(expected_elems), TyKind::Tuple(found_elems))
                if expected_elems.len() == found_elems.len() =>
            {
                let mut changed = false;
                let mut elems = Vec::with_capacity(found_elems.len());
                for (e, f) in expected_elems.iter().zip(&found_elems) {
                    match self.coerce_nested_fn_items(*e, *f) {
                        Some(coerced) => {
                            changed = true;
                            elems.push(coerced);
                        }
                        None => elems.push(*f),
                    }
                }
                changed.then(|| self.tcx.intern(TyKind::Tuple(elems)))
            }
            (TyKind::Vec(e), TyKind::Vec(f)) => {
                let coerced = self.coerce_nested_fn_items(e, f)?;
                Some(self.tcx.intern(TyKind::Vec(coerced)))
            }
            (TyKind::Array { elem: e, .. }, TyKind::Array { elem: f, len }) => {
                let coerced = self.coerce_nested_fn_items(e, f)?;
                Some(self.tcx.intern(TyKind::Array { elem: coerced, len }))
            }
            _ => None,
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
        let const_mask = self.fn_generic_const_mask_of(def);
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

/// `(receiver op arg)` for a wrapping arithmetic method call, or `None` when
/// an operand has no short spelling. The parentheses keep the rewrite meaning
/// what the call meant wherever it stood, whatever operator surrounds it.
fn wrapping_operator_rewrite(receiver: &Expr, arg: &Expr, operator: &str) -> Option<String> {
    let receiver = wrapping_operand_display(receiver)?;
    let arg = wrapping_operand_display(arg)?;
    Some(format!("({receiver} {operator} {arg})"))
}

/// Spelling of a wrapping rewrite's operand with any wrapping method call at
/// its head already written as the operator, so the rewrite of the outermost
/// call in a chain is the whole fix.
fn wrapping_operand_display(expr: &Expr) -> Option<String> {
    if let ExprKind::MethodCall {
        receiver,
        name,
        args,
        ..
    } = &expr.kind
        && let [arg] = args.as_slice()
        && let Some(operator) = wrapping_method_operator(&name.name)
    {
        return wrapping_operator_rewrite(receiver, arg, operator);
    }
    // A prefix operator binds tighter than the wrapping operator, and the
    // enclosing rewrite parenthesizes the pair, so the operand keeps its
    // reach where the rewrite lands.
    if let ExprKind::Unary { op, operand } = &expr.kind
        && matches!(op, UnaryOp::Neg | UnaryOp::Not | UnaryOp::Deref)
    {
        return Some(format!(
            "{}{}",
            op.as_str(),
            wrapping_operand_display(operand)?
        ));
    }
    expr_display(expr)
}

/// The operator a retired wrapping arithmetic method is spelled as.
fn wrapping_method_operator(method: &str) -> Option<&'static str> {
    match method {
        "wrapping_add" => Some("+%"),
        "wrapping_sub" => Some("-%"),
        "wrapping_mul" => Some("*%"),
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

/// One slot per letter a catalogue type parameter can be spelled with.
const CATALOG_TYPE_PARAM_SLOTS: usize = 26;

fn is_catalog_type_param(src: &str) -> bool {
    let mut chars = src.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ch.is_ascii_uppercase())
}

/// Pre-registers field types for stdlib structs that user source can
/// name (`http::Response`, `http::ResponseStream`). The MIR-side dispatch pins free-call
/// destinations to sentinel `DefId`s (`u32::MAX - N`) for these
/// structs; without their field types registered here, `entry.path`
/// / `r.status` projections leave the result `Var(_)` and downstream
/// `.len()` / println fall back to the wrong dispatch.
fn register_stdlib_struct_fields(tcx: &mut TyCtxt) {
    let str_ty = tcx.string_ty();
    let i64_ty = tcx.int_ty(IntTy::I64);
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

/// The stdlib struct field tables the checker starts with, registered into
/// `tcx` and returned as the checker's own copy, so a field lookup on one of
/// those structs finds the layout `tcx` knows.
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
        AssignOp::Assign
        | AssignOp::WrappingAddAssign
        | AssignOp::WrappingSubAssign
        | AssignOp::WrappingMulAssign => None,
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

/// The source spelling of an array length or lane count: its number, or the
/// const parameter it names.
fn render_array_len(len: crate::ArrayLen) -> String {
    match len {
        crate::ArrayLen::Concrete(n) => n.to_string(),
        crate::ArrayLen::Param(idx) => format!("N{}", idx.as_u32()),
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

/// Single-segment names a closure body mentions that its parameters do not
/// bind: the outer bindings it may capture.
fn closure_outer_names(params: &[ClosureParam], body: &Expr) -> HashSet<String> {
    struct Collector<'a> {
        bound: &'a HashSet<String>,
        names: HashSet<String>,
    }

    impl gossamer_ast::visitor::Visitor for Collector<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let ExprKind::Path(path) = &expr.kind
                && let [segment] = path.segments.as_slice()
                && !self.bound.contains(&segment.name.name)
            {
                self.names.insert(segment.name.name.clone());
            }
            gossamer_ast::visitor::walk_expr(self, expr);
        }
    }

    let bound = closure_bound_names(params);
    let mut collector = Collector {
        bound: &bound,
        names: HashSet::new(),
    };
    gossamer_ast::visitor::Visitor::visit_expr(&mut collector, body);
    collector.names
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
        | TyKind::Simd { elem, .. }
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
            crate::GenericArg::ConstParam(_) => false,
        }),
        TyKind::Dyn(trait_ref) => trait_ref.substs.as_slice().iter().all(|arg| match arg {
            crate::GenericArg::Type(ty) => checker.is_concrete(*ty),
            crate::GenericArg::Const(_) => true,
            crate::GenericArg::ConstParam(_) => false,
        }),
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

/// `errors::Error`, and the `Error` a stdlib module's fallible calls answer
/// (`http::Error`, `io::Error`), which is that same type.
fn path_matches_dyn_error(path: &TypePath) -> bool {
    let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    match names.as_slice() {
        [module, "Error"] | ["std", module, "Error"] => {
            *module == "error"
                || gossamer_resolve::STDLIB_MODULES
                    .binary_search(module)
                    .is_ok()
        }
        _ => false,
    }
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

/// Where a branch's value is written: the tail of a block body (through
/// nested blocks), or the expression itself.
fn branch_value_span(expr: &Expr) -> Span {
    match &expr.kind {
        ExprKind::Block(block) => block.tail.as_deref().map_or(expr.span, branch_value_span),
        _ => expr.span,
    }
}

/// The type parameter an `impl<T: ..> Trait for T` block names as its self
/// type: a blanket impl, which would attach the trait to every type.
fn blanket_impl_param(decl: &ImplDecl) -> Option<String> {
    let AstTypeKind::Path(path) = &decl.self_ty.kind else {
        return None;
    };
    let [segment] = path.segments.as_slice() else {
        return None;
    };
    let name = segment.name.name.as_str();
    decl.generics
        .params
        .iter()
        .any(|param| {
            matches!(param, gossamer_ast::GenericParam::Type { name: param, .. } if param.name == name)
        })
        .then(|| name.to_string())
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

/// An AST type rendered as source, for a diagnostic that quotes it.
fn render_ast_type(ty: &gossamer_ast::Type) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type(ty);
    printer.finish()
}

// The checker's methods, by responsibility.
mod calls;
mod deferred;
mod foreign;
mod items;
mod methods;
mod operators;
mod patterns;
mod stdlib;
mod types;

// Tables and pure helpers the methods consult.
mod handles;
mod literals;
mod method_catalog;

use handles::{
    HANDLE_METHODS, Shape, atomic_method, fs_file_ctor, is_opaque_handle_def, net_socket_ctor,
    stdlib_fs_handle, stdlib_handle_by_path, stdlib_handle_ctor, stdlib_handle_def_offset,
    stdlib_net_handle, table_handle_owner,
};
use literals::{
    FLOAT_SUFFIXES, INT_SUFFIXES, evaluate_const_int_from_expr, int_assoc_const, int_literal_fits,
    int_ty_from_width, parse_int_magnitude,
};
use method_catalog::{
    BTREE_MAP_ONLY_METHODS, BTREE_SET_ONLY_METHODS, COLLECTION_TRAVERSAL_METHODS,
    builtin_type_constructors, core_type_own_method_names, is_string_method,
};
pub use method_catalog::{
    core_type_accepts_method, core_type_declares_method, is_array_sequence_method,
    is_btree_map_method, is_btree_set_method, is_collection_traversal_method,
    is_free_call_only_traversal, is_iterator_method, is_map_method, is_set_method,
    is_slice_sequence_method, is_tuple_method, is_tuple_rejected_method,
    is_vec_only_sequence_method, iterator_adapter_is_lazy, iterator_receiver_accepts_method,
};

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
