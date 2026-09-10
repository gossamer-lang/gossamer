//! Mid-level IR (MIR) data types.
//! MIR is the **single source of truth** for all language semantics.
//! The interpreter executes MIR directly; the compiler lowers MIR to
//! machine code. No semantic logic lives outside this IR - if a
//! behaviour is not expressible as a [`StatementKind`], [`Terminator`],
//! [`Rvalue`], or [`ConstValue`], it does not exist at this layer.
//! Mirrors rustc's MIR in spirit: a per-function control-flow graph of
//! [`BasicBlock`]s, each ending in a [`Terminator`]. Local variables
//! live in a flat `Vec` indexed by [`Local`]. The IR is SSA-lite:
//! locals may be assigned multiple times, but the lowerer gives every
//! temporary a fresh local so most intermediates do obey single
//! assignment in practice.

#![forbid(unsafe_code)]

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::Ty;

/// Local variable index within a [`Body`]. `Local(0)` is the return
/// slot; subsequent indices are parameters followed by temporaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Local(pub u32);

impl Local {
    /// Index `0` - reserved for the function's return value.
    pub const RETURN: Self = Self(0);

    /// Raw numeric index.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Basic-block identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

impl BlockId {
    /// Entry block assigned at body construction time.
    pub const ENTRY: Self = Self(0);

    /// Raw numeric index.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Per-function CFG plus locals table.
#[derive(Debug, Clone)]
pub struct Body {
    /// Source-level function name, useful in diagnostics.
    pub name: String,
    /// [`DefId`] assigned to this function by the resolver. Needed
    /// by the native backend to link `Operand::FnRef(def)` sites to
    /// their definitions without going through the function name.
    /// `None` for functions without a resolver-assigned id (e.g.
    /// synthesised closures before resolver integration lands).
    pub def: Option<DefId>,
    /// Number of parameters; parameters live at locals `1..=arity`.
    pub arity: u32,
    /// Type of each local, indexed by [`Local`].
    pub locals: Vec<LocalDecl>,
    /// CFG blocks indexed by [`BlockId`].
    pub blocks: Vec<BasicBlock>,
    /// Source span of the source-level function declaration.
    pub span: Span,
}

impl Body {
    /// Borrows a block by id.
    ///
    /// # Panics
    ///
    /// Panics if `id` is out of range.
    #[must_use]
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.0 as usize]
    }

    /// Mutably borrows a block by id.
    ///
    /// # Panics
    ///
    /// Panics if `id` is out of range.
    pub fn block_mut(&mut self, id: BlockId) -> &mut BasicBlock {
        &mut self.blocks[id.0 as usize]
    }

    /// Returns the type of `local`.
    ///
    /// # Panics
    ///
    /// Panics if `local` is out of range.
    #[must_use]
    pub fn local_ty(&self, local: Local) -> Ty {
        self.locals[local.0 as usize].ty
    }
}

/// Metadata attached to every [`Local`].
#[derive(Debug, Clone)]
pub struct LocalDecl {
    /// Type assigned to the local.
    pub ty: Ty,
    /// Optional source-level identifier that introduced this local.
    pub debug_name: Option<Ident>,
    /// `true` when the local is declared mutable at the source level.
    pub mutable: bool,
    /// `true` when the local was created inside an arena region
    /// (`runtime::arena_push` .. `arena_pop`). Its RC value is freed
    /// wholesale at region pop, so the drop pass must NOT emit a
    /// retain/release for it - doing so would touch freed memory after the
    /// pop (use-after-free).
    pub region: bool,
}

/// A basic block: a straight-line sequence of statements terminated by
/// a single [`Terminator`].
#[derive(Debug, Clone)]
pub struct BasicBlock {
    /// Stable id (matches this block's position in [`Body::blocks`]).
    pub id: BlockId,
    /// Straight-line body.
    pub stmts: Vec<Statement>,
    /// Control-flow terminator.
    pub terminator: Terminator,
    /// Source span covering the original construct.
    pub span: Span,
}

/// One statement inside a [`BasicBlock`].
#[derive(Debug, Clone)]
pub struct Statement {
    /// Statement kind.
    pub kind: StatementKind,
    /// Source span.
    pub span: Span,
}

/// Non-terminator statement kinds.
#[derive(Debug, Clone)]
pub enum StatementKind {
    /// `place = rvalue`. Copies (or moves) the value produced by
    /// `rvalue` into `place`. For aggregates the copy is a shallow
    /// bitwise copy of the flat layout; heap objects reachable through
    /// the value are handled by the GC write barrier.
    Assign {
        /// Destination place.
        place: Place,
        /// Right-hand value.
        rvalue: Rvalue,
    },
    /// Marks `local` as live. Emitted at block entry for temporaries.
    StorageLive(Local),
    /// Marks `local` as dead. Emitted when a temporary goes out of
    /// scope.
    StorageDead(Local),
    /// Sets the active discriminant of an enum place to `variant`.
    SetDiscriminant {
        /// Place whose tag is being written.
        place: Place,
        /// Variant index within the enum's declaration order.
        variant: u32,
    },
    /// Stores `value` into a `static mut` global. Lowers to a `store`
    /// into the backing module global in the native backends.
    StaticStore {
        /// The static being written.
        target: StaticRef,
        /// Value to store.
        value: Operand,
    },
    /// Creates a typed iterator state from a concrete source. Iterator states
    /// are linear: an adapter may take ownership of a state once, while
    /// [`StatementKind::IterNext`] advances it in place.
    IterSource {
        /// Destination local that owns the newly-created state.
        dst: Place,
        /// Concrete source representation.
        source_kind: IteratorSourceKind,
        /// Range bounds or the source collection.
        source: Operand,
        /// Type yielded by each successful next operation.
        item_ty: Ty,
        /// Whether the state borrows or owns its source allocation.
        ownership: IteratorOwnership,
    },
    /// Creates an adapter state that owns its upstream iterator state.
    IterAdapter {
        /// Destination local that owns the adapter state.
        dst: Place,
        /// Concrete adapter representation.
        adapter_kind: IteratorAdapterKind,
        /// Upstream iterator state. This is consumed by the adapter.
        upstream: Place,
        /// Closure or scalar adapter argument, where the adapter needs one.
        closure_or_arg: Option<Operand>,
        /// Type yielded by each successful next operation.
        item_ty: Ty,
    },
    /// Advances a mutable iterator state and writes an `Option<Item>` result.
    /// Repeated next operations are valid, including after exhaustion.
    IterNext {
        /// Destination receiving the typed `Option<Item>` result.
        dst_option: Place,
        /// Mutable iterator state to advance.
        iter_place: Place,
        /// Type yielded by `Some`.
        item_ty: Ty,
    },
    /// No-op preserved for alignment with rustc-style MIR dumps.
    Nop,
}

/// Concrete sources supported by the first typed iterator representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorSourceKind {
    /// Integer range state.
    Range,
    /// Borrowed slice or Vec state.
    Slice,
    /// Owning Vec state that moves elements out once.
    VecInto,
}

/// Ownership mode of an iterator source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorOwnership {
    /// The state observes a source allocation without taking it over.
    Borrowed,
    /// The state owns the source allocation and its remaining elements.
    Owning,
}

/// Concrete adapters supported by the first typed iterator representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorAdapterKind {
    /// Transform one source item through a closure.
    Map,
    /// Yield only items a predicate accepts.
    Filter,
    /// Bound the number of yielded items.
    Take,
    /// Discard a prefix before yielding.
    Skip,
    /// Pair items with their source index.
    Enumerate,
    /// Yield the first source then the second source.
    Chain,
    /// Pair items until either input is exhausted.
    Zip,
}

/// Control-flow terminator closing a block.
#[derive(Debug, Clone)]
pub enum Terminator {
    /// Unconditional jump to `target`.
    Goto {
        /// Successor block.
        target: BlockId,
    },
    /// Multi-way branch on an integer discriminant. Evaluates
    /// `discriminant` to an integer and jumps to the block whose arm
    /// value equals it (integer equality). If no arm matches,
    /// control falls through to `default`. Used for `if`, `match`
    /// on integers/bools, and loop headers.
    SwitchInt {
        /// Scrutinee operand.
        discriminant: Operand,
        /// Match arms: each pair is `(value, target)`.
        arms: Vec<(i128, BlockId)>,
        /// Default arm taken when no explicit value matches.
        default: BlockId,
    },
    /// `return place_0` from the enclosing function.
    Return,
    /// Function call. Control transfers to `target` on normal return.
    Call {
        /// Callee operand (usually a constant function reference).
        callee: Operand,
        /// Call arguments in source order.
        args: Vec<Operand>,
        /// Destination place receiving the returned value.
        destination: Place,
        /// Continuation block. `None` encodes a diverging call.
        target: Option<BlockId>,
    },
    /// Runtime assertion (bounds / overflow). On failure jumps to a
    /// dedicated panic block.
    Assert {
        /// Assertion to evaluate.
        cond: Operand,
        /// `true` when the assertion fires when `cond` is truthy; the
        /// normal "assert cond is true" form uses `false`.
        expected: bool,
        /// Runtime message selector.
        msg: AssertMessage,
        /// Success continuation.
        target: BlockId,
    },
    /// Compiler knows this block is never reached at runtime.
    Unreachable,
    /// Unconditional panic: terminates the program with `message`.
    Panic {
        /// Human-readable reason.
        message: String,
    },
    /// Drops the value stored at `place` (invokes its `drop_fn` if
    /// any) and jumps to `target`.
    Drop {
        /// Place to drop.
        place: Place,
        /// Continuation after the drop completes.
        target: BlockId,
    },
}

/// Assertion message category - used by the runtime to produce
/// human-readable panic text without interpolating strings in emitted
/// code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertMessage {
    /// `index < len` failed for an indexing operation.
    BoundsCheck,
    /// Arithmetic overflow in debug mode.
    Overflow,
    /// Integer divide/modulo by zero.
    DivideByZero,
}

/// An lvalue - a place the IR can read from or write to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// Local the place is rooted in.
    pub local: Local,
    /// Projection chain applied to `local` from outermost to innermost.
    pub projection: Vec<Projection>,
}

impl Place {
    /// Returns a bare local with no projection.
    #[must_use]
    pub const fn local(local: Local) -> Self {
        Self {
            local,
            projection: Vec::new(),
        }
    }

    /// `true` when this place is a bare local with no projection.
    #[must_use]
    pub fn is_simple(&self) -> bool {
        self.projection.is_empty()
    }
}

/// One step in a place projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    /// `*place` - dereference.
    Deref,
    /// `place.field` with the field's numeric index.
    Field(u32),
    /// `place[index]` - runtime array indexing.
    Index(Local),
    /// `place as variant` - access an enum's payload through an
    /// already-discriminated variant.
    Downcast(u32),
    /// The discriminant word of an enum place (read-only projection).
    Discriminant,
}

/// Operand form used by rvalues and terminators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// Copy/move the value stored at `place`.
    Copy(Place),
    /// Compile-time constant.
    Const(ConstValue),
    /// Reference to a named function plus the generic arguments it
    /// was instantiated with at this call site. Non-empty `substs`
    /// signal that the monomorphiser should produce a specialised
    /// copy of the callee body with a mangled name derived from the
    /// argument list.
    FnRef {
        /// `DefId` of the referenced function.
        def: DefId,
        /// Generic instantiation. Empty for monomorphic callees.
        substs: gossamer_types::Substs,
    },
}

/// Constant values surfaced in the IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstValue {
    /// `()`.
    Unit,
    /// `bool`.
    Bool(bool),
    /// Signed 128-bit; narrower widths sit inside until codegen
    /// truncates.
    Int(i128),
    /// IEEE-754 binary64 as its bit pattern (so `PartialEq` holds).
    Float(u64),
    /// Unicode scalar value.
    Char(char),
    /// UTF-8 string constant.
    Str(String),
}

/// Right-hand side of an [`StatementKind::Assign`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rvalue {
    /// Plain operand read.
    Use(Operand),
    /// Binary operator applied to two operands.
    BinaryOp {
        /// Operator.
        op: BinOp,
        /// Left operand.
        lhs: Operand,
        /// Right operand.
        rhs: Operand,
    },
    /// Unary operator.
    UnaryOp {
        /// Operator.
        op: UnOp,
        /// Operand.
        operand: Operand,
    },
    /// `expr as T`. Converts the operand to the target type. Same-
    /// width integer casts are identity; narrowing, widening, and
    /// float conversions are representation changes that codegen must
    /// materialise.
    Cast {
        /// Operand being converted.
        operand: Operand,
        /// Target type after the cast.
        target: Ty,
    },
    /// Aggregate constructor. Builds a tuple, array, struct, or
    /// enum payload in a flat memory layout. Elements appear in
    /// declaration order; the codegen backend and the interpreter
    /// must agree on the same field offsets and discriminant word
    /// placement (see [`Projection::Field`] and
    /// [`StatementKind::SetDiscriminant`]).
    Aggregate {
        /// Aggregate kind.
        kind: AggregateKind,
        /// Element operands in declaration order.
        operands: Vec<Operand>,
    },
    /// `len(place)` - length of an array/vec/slice.
    Len(Place),
    /// `[value; count]` repeat constructor.
    Repeat {
        /// Repeated value.
        value: Operand,
        /// Compile-time count.
        count: u64,
    },
    /// `&place` or `&mut place`.
    Ref {
        /// `true` for `&mut`.
        mutable: bool,
        /// Referent place.
        place: Place,
    },
    /// Direct intrinsic call. Arguments are inline operands.
    CallIntrinsic {
        /// Intrinsic name.
        name: &'static str,
        /// Arguments.
        args: Vec<Operand>,
    },
    /// Loads the current value of a `static mut` global. Lowers to a
    /// `load` from the backing module global in the native backends.
    StaticLoad(StaticRef),
}

/// A typed view over the raw intrinsic names that may appear in
/// [`Rvalue::CallIntrinsic`]. The MIR still stores the source spelling for
/// compact dumps and compatibility with existing builders, but every verifier
/// and native backend should parse it through this enum before acting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIntrinsic {
    /// `gos_enum_load(ptr, offset)`.
    EnumLoad,
    /// `gos_enum_slot_ptr(ptr, offset)` - where a payload's words live, which
    /// each back end answers in the representation its own slot holds.
    EnumSlotPtr,
    /// `gos_enum_tag(ptr, disc)`.
    EnumTag,
    /// `gos_enum_disc_tag(ptr)`.
    EnumDiscTag,
    /// `gos_enum_untag(ptr)`.
    EnumUntag,
    /// `gos_enum_disc(payload_ptr)`.
    EnumDisc,
    /// `gos_enum_set_disc(payload_ptr, disc)`.
    EnumSetDisc,
    /// `gos_load(ptr, offset)`.
    Load,
    /// `gos_store(ptr, offset, value)`.
    Store,
    /// `gos_store_i128(ptr, offset, carrier)` - writes both words of a
    /// two-word `Option` / `Result` / inline-enum value into a slot sized
    /// for it. `gos_store` writes one word, which every other slot is.
    StoreI128,
    /// `gos_alloc(size?)`.
    Alloc,
    /// `gos_rc_alloc(size?, meta?)`.
    RcAlloc,
    /// `gos_rc_alloc_tagged(size?, meta?)`.
    RcAllocTagged,
    /// `gos_rc_alloc_reuse(token, size, meta)`.
    RcAllocReuse,
    /// `gos_rt_enum_struct_eq(a, b, desc)`.
    EnumStructEq,
    /// `gos_rt_map_*_ekey(map, key_node, desc, [word])` - an enum-keyed map
    /// operation whose third argument names a descriptor blob rather than
    /// being an ordinary value.
    MapEnumKey,
    /// `gos_fn_addr(name)`.
    FnAddr,
    /// `gos_rt_weak_opt_payload(option_carrier)`.
    WeakOptPayload,
    /// `gos_result_payload_owned(carrier)` - the carrier's payload is an
    /// aggregate copy the container allocated for this read, so the back ends
    /// reclaim it once its words are in the destination's own storage.
    OwnedAggregatePayload,
    /// Runtime helper with an ABI registry entry.
    Runtime,
    /// Floating point LLVM intrinsic facade.
    F64Math(F64MathIntrinsic),
    /// Internal marker used to keep bytecode-only user iterators out of native
    /// promotion.
    JitUnsupportedUserIterator,
}

/// Floating point math intrinsic lowered directly to LLVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum F64MathIntrinsic {
    /// `llvm.sqrt.f64`.
    Sqrt,
    /// `llvm.sin.f64`.
    Sin,
    /// `llvm.cos.f64`.
    Cos,
    /// `llvm.fabs.f64`.
    Abs,
    /// `llvm.floor.f64`.
    Floor,
    /// `llvm.ceil.f64`.
    Ceil,
    /// `llvm.exp.f64`.
    Exp,
    /// `llvm.log.f64`.
    Log,
}

/// Arity contract for a raw intrinsic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIntrinsicArity {
    /// Exactly this many operands.
    Exact(usize),
    /// Inclusive operand count range.
    Range {
        /// Minimum accepted operand count.
        min: usize,
        /// Maximum accepted operand count.
        max: usize,
    },
}

impl RawIntrinsicArity {
    /// Returns true when `count` satisfies this arity.
    #[must_use]
    pub const fn accepts(self, count: usize) -> bool {
        match self {
            Self::Exact(n) => count == n,
            Self::Range { min, max } => count >= min && count <= max,
        }
    }
}

impl F64MathIntrinsic {
    /// LLVM intrinsic symbol.
    #[must_use]
    pub const fn llvm_name(self) -> &'static str {
        match self {
            Self::Sqrt => "llvm.sqrt.f64",
            Self::Sin => "llvm.sin.f64",
            Self::Cos => "llvm.cos.f64",
            Self::Abs => "llvm.fabs.f64",
            Self::Floor => "llvm.floor.f64",
            Self::Ceil => "llvm.ceil.f64",
            Self::Exp => "llvm.exp.f64",
            Self::Log => "llvm.log.f64",
        }
    }
}

impl RawIntrinsic {
    /// Parse a MIR intrinsic name into the typed catalogue.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let intrinsic = match name {
            "gos_enum_load" => Self::EnumLoad,
            "gos_enum_slot_ptr" => Self::EnumSlotPtr,
            "gos_enum_tag" => Self::EnumTag,
            "gos_enum_disc_tag" => Self::EnumDiscTag,
            "gos_enum_untag" => Self::EnumUntag,
            "gos_enum_disc" => Self::EnumDisc,
            "gos_enum_set_disc" => Self::EnumSetDisc,
            "gos_load" => Self::Load,
            "gos_store" => Self::Store,
            "gos_store_i128" => Self::StoreI128,
            "gos_alloc" => Self::Alloc,
            "gos_rc_alloc" => Self::RcAlloc,
            "gos_rc_alloc_tagged" => Self::RcAllocTagged,
            "gos_rc_alloc_reuse" => Self::RcAllocReuse,
            "gos_rt_enum_struct_eq" => Self::EnumStructEq,
            "gos_rt_map_insert_ekey_opt"
            | "gos_rt_map_get_ekey_opt"
            | "gos_rt_map_contains_ekey"
            | "gos_rt_map_pop_ekey"
            | "gos_rt_map_get_or_ekey"
            | "gos_rt_map_or_insert_ekey"
            | "gos_rt_map_inc_ekey" => Self::MapEnumKey,
            "gos_fn_addr" => Self::FnAddr,
            "gos_rt_weak_opt_payload" => Self::WeakOptPayload,
            "gos_result_payload_owned" => Self::OwnedAggregatePayload,
            "gos_jit_unsupported_user_iterator" => Self::JitUnsupportedUserIterator,
            "f64.sqrt" | "sqrt" => Self::F64Math(F64MathIntrinsic::Sqrt),
            "f64.sin" | "sin" => Self::F64Math(F64MathIntrinsic::Sin),
            "f64.cos" | "cos" => Self::F64Math(F64MathIntrinsic::Cos),
            "f64.abs" | "fabs" | "abs" => Self::F64Math(F64MathIntrinsic::Abs),
            "f64.floor" | "floor" => Self::F64Math(F64MathIntrinsic::Floor),
            "f64.ceil" | "ceil" => Self::F64Math(F64MathIntrinsic::Ceil),
            "f64.exp" | "exp" => Self::F64Math(F64MathIntrinsic::Exp),
            "f64.ln" | "ln" | "f64.log" | "log" => Self::F64Math(F64MathIntrinsic::Log),
            other if gossamer_abi::lookup(other).is_some() => Self::Runtime,
            _ => return None,
        };
        Some(intrinsic)
    }

    /// Expected argument count for this intrinsic.
    #[must_use]
    pub fn arity(self) -> RawIntrinsicArity {
        self.arity_for_name("")
    }

    /// Expected argument count for this intrinsic using the original MIR
    /// spelling for registry-backed runtime helpers.
    #[must_use]
    pub fn arity_for_name(self, name: &str) -> RawIntrinsicArity {
        match self {
            Self::EnumLoad | Self::EnumSlotPtr | Self::EnumTag | Self::EnumSetDisc | Self::Load => {
                RawIntrinsicArity::Exact(2)
            }
            Self::Store | Self::StoreI128 | Self::RcAllocReuse | Self::EnumStructEq => {
                RawIntrinsicArity::Exact(3)
            }
            Self::MapEnumKey => RawIntrinsicArity::Range { min: 3, max: 4 },
            Self::EnumDiscTag
            | Self::EnumUntag
            | Self::EnumDisc
            | Self::FnAddr
            | Self::WeakOptPayload
            | Self::OwnedAggregatePayload
            | Self::F64Math(_) => RawIntrinsicArity::Exact(1),
            Self::Alloc => RawIntrinsicArity::Range { min: 0, max: 1 },
            Self::RcAlloc | Self::RcAllocTagged => RawIntrinsicArity::Range { min: 0, max: 2 },
            Self::JitUnsupportedUserIterator => RawIntrinsicArity::Exact(0),
            Self::Runtime => gossamer_abi::lookup(name)
                .map_or(RawIntrinsicArity::Exact(usize::MAX), |entry| {
                    RawIntrinsicArity::Exact(entry.sig.params.len())
                }),
        }
    }
}

/// Reference to a `static mut` global. Every access (load or store)
/// carries the static's mangled symbol, value type, and const
/// initializer so any backend can materialise the backing global
/// locally - the native linker coalesces duplicate `linkonce_odr`
/// definitions emitted across object files into one shared cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticRef {
    /// Mangled global symbol (`gos_static_<defid>`).
    pub symbol: String,
    /// Declared value type of the static.
    pub ty: Ty,
    /// Const-folded initializer value.
    pub init: ConstValue,
}

/// Aggregate constructors surfaced by the lowerer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateKind {
    /// Tuple with the given element types.
    Tuple,
    /// Struct-shaped aggregate.
    Adt {
        /// `DefId` of the struct/enum.
        def: DefId,
        /// Variant index for enums; `0` for structs.
        variant: u32,
    },
    /// Array literal with explicit elements.
    Array,
}

/// Binary operators supported at the MIR level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    /// `+`.
    Add,
    /// Explicit `wrapping_add`.
    WrappingAdd,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// Explicit `wrapping_mul`.
    WrappingMul,
    /// `/`.
    Div,
    /// `%`.
    Rem,
    /// `&`.
    BitAnd,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
    /// `<<`.
    Shl,
    /// `>>`.
    Shr,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// Unary operators supported at the MIR level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnOp {
    /// `-x`.
    Neg,
    /// `!x`.
    Not,
}

/// Returns `true` when every write to `local` is an `as u64` /
/// `as usize` cast result (or a copy of another such local) - the
/// static analog of the VM's `Value::Uint` display provenance, used
/// by the compiled backends to decide between the signed and the
/// unsigned integer printer.
#[must_use]
pub fn local_is_uint_cast(body: &Body, tcx: &gossamer_types::TyCtxt, local: Local) -> bool {
    fn is_uint_ty(tcx: &gossamer_types::TyCtxt, ty: Ty) -> bool {
        matches!(
            tcx.kind(ty),
            Some(gossamer_types::TyKind::Int(
                gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
            ))
        )
    }
    fn check(
        body: &Body,
        tcx: &gossamer_types::TyCtxt,
        local: Local,
        visited: &mut Vec<Local>,
    ) -> bool {
        if visited.contains(&local) {
            return true;
        }
        visited.push(local);
        // The return slot and parameters have writers this body
        // cannot see.
        if local.0 <= body.arity {
            return false;
        }
        let mut saw_cast = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if place.local != local || !place.projection.is_empty() {
                    continue;
                }
                match rvalue {
                    Rvalue::Cast { target, .. } if is_uint_ty(tcx, *target) => saw_cast = true,
                    Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => {
                        if !check(body, tcx, src.local, visited) {
                            return false;
                        }
                        saw_cast = true;
                    }
                    _ => return false,
                }
            }
            if let Terminator::Call { destination, .. } = &block.terminator
                && destination.local == local
            {
                return false;
            }
        }
        saw_cast
    }
    check(body, tcx, local, &mut Vec::new())
}
