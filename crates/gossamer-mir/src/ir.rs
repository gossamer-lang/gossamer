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
    /// Source span of the expression the terminator was lowered from, when
    /// the builder knew it. A debug frame names this line for a raise inside
    /// the terminator, where `span` names the construct the block opened with.
    pub terminator_span: Option<Span>,
    /// The inlined calls the terminator's code came through.
    pub terminator_inlined: InlineChain,
}

/// One call whose callee body an inliner placed in the caller: the callee
/// and where the call is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFrame {
    /// The inlined callee.
    pub function: String,
    /// Where the call is written, in the function it was inlined into.
    pub call: Span,
}

/// The inlined calls a piece of code came through, outermost first, or
/// `None` for a body's own code.
pub type InlineChain = Option<std::sync::Arc<[InlineFrame]>>;

/// One statement inside a [`BasicBlock`].
#[derive(Debug, Clone)]
pub struct Statement {
    /// Statement kind.
    pub kind: StatementKind,
    /// Source span.
    pub span: Span,
    /// The inlined calls this statement's code came through.
    pub inlined: InlineChain,
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
    /// `index < len` failed for an indexing operation. The panic names the
    /// index and the sequence's length, which the failing path reads from the
    /// sequence itself, so a passing check keeps no length alive for it.
    BoundsCheck {
        /// The index the access used, an integer.
        index: Operand,
        /// The `Vec` the access indexed.
        seq: Operand,
    },
    /// Arithmetic overflow in debug mode.
    Overflow,
    /// Integer divide/modulo by zero.
    DivideByZero,
}

impl AssertMessage {
    /// The operands the message reads when the assertion fails, beside the
    /// assertion's own condition.
    pub fn operands(&self) -> impl Iterator<Item = &Operand> {
        let pair = match self {
            Self::BoundsCheck { index, seq } => Some([index, seq]),
            Self::Overflow | Self::DivideByZero => None,
        };
        pair.into_iter().flatten()
    }

    /// [`Self::operands`], mutably.
    pub fn operands_mut(&mut self) -> impl Iterator<Item = &mut Operand> {
        let pair = match self {
            Self::BoundsCheck { index, seq } => Some([index, seq]),
            Self::Overflow | Self::DivideByZero => None,
        };
        pair.into_iter().flatten()
    }
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
    /// Runtime helper with an ABI registry entry.
    Runtime,
    /// Floating point LLVM intrinsic facade.
    F64Math(F64MathIntrinsic),
    /// Internal marker used to keep bytecode-only user iterators out of native
    /// promotion.
    JitUnsupportedUserIterator,
    /// A call to a function declared in an `unsafe extern "C"` block; the
    /// name carries the call's [`ForeignCall`] parts.
    Foreign,
    /// The address of a C-ABI entry for a callback adapter
    /// ([`ForeignCallback`]); its one operand names the adapter.
    ForeignCallback,
    /// The address of a C global declared in an extern block
    /// ([`ForeignStatic`]); it takes no operands.
    ForeignStatic,
}

/// How one parameter of a foreign call crosses, as the signature spells it
/// (see `gossamer_hir::foreign_signature`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignParam {
    /// A scalar of this C class.
    Scalar(char),
    /// A slice of scalars of class `elem`, passed as a pointer to its first
    /// element; `writable` when the callee's writes come back.
    Slice {
        /// The element class.
        elem: char,
        /// Whether the callee's writes come back.
        writable: bool,
    },
    /// A pointer to a C-layout struct copy, already packed when the call is
    /// lowered; `writable` when the callee's writes come back.
    Struct {
        /// Whether the callee's writes come back.
        writable: bool,
    },
    /// A struct passed by value, already packed into a C-layout buffer when
    /// the call is lowered; the backend moves it per the target's calling
    /// convention.
    ByValue(gossamer_abi::c_aggregate::CLayout),
}

/// The `s(layout)` spelling at the start of `text`: the layout and the
/// rest of `text`.
fn by_value_prefix(text: &str) -> Option<(gossamer_abi::c_aggregate::CLayout, &str)> {
    let inner = text.strip_prefix("s(")?;
    let close = inner.find(')')?;
    let layout = gossamer_abi::c_aggregate::CLayout::parse(&inner[..close])?;
    Some((layout, &inner[close + 1..]))
}

/// The parameters a foreign signature's parameter part spells, or `None`
/// when it is malformed.
#[must_use]
pub fn foreign_params(spelling: &str) -> Option<Vec<ForeignParam>> {
    let mut params = Vec::new();
    let mut rest = spelling;
    while !rest.is_empty() {
        if let Some((layout, after)) = by_value_prefix(rest) {
            params.push(ForeignParam::ByValue(layout));
            rest = after;
            continue;
        }
        let mut chars = rest.chars();
        let c = chars.next()?;
        params.push(match c {
            'p' | 'P' => {
                let elem = chars.next()?;
                gossamer_types::c_class_width(elem)?;
                ForeignParam::Slice {
                    elem,
                    writable: c == 'P',
                }
            }
            'r' | 'R' => ForeignParam::Struct { writable: c == 'R' },
            class => {
                gossamer_types::c_class_width(class)?;
                ForeignParam::Scalar(class)
            }
        });
        rest = chars.as_str();
    }
    Some(params)
}

impl ForeignParam {
    /// The signature spelling of this parameter.
    #[must_use]
    pub fn spelling(&self) -> String {
        match self {
            Self::Scalar(class) => class.to_string(),
            Self::Slice { elem, writable } => {
                format!("{}{elem}", if *writable { 'P' } else { 'p' })
            }
            Self::Struct { writable } => (if *writable { "R" } else { "r" }).to_string(),
            Self::ByValue(layout) => format!("s({})", layout.render()),
        }
    }
}

/// The parts of a foreign-call intrinsic name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignCall<'a> {
    /// The C symbol called.
    pub symbol: &'a str,
    /// The parameter part of the signature, as `gossamer_hir::foreign_signature`
    /// spells it; [`ForeignCall::param_list`] reads it.
    pub params: &'a str,
    /// The result's class character, `s` for a struct.
    pub ret: char,
    /// A struct result's layout spelling, inside `s(..)`.
    pub ret_layout: Option<&'a str>,
    /// The library `#[link(name = ..)]` names, or `""`.
    pub library: &'a str,
}

const FOREIGN_PREFIX: &str = "gos_ffi_call:";

/// The symbol `gossamer_hir`'s foreign-boundary pass gives a call through an
/// address.
const fn gossamer_hir_indirect_symbol() -> &'static str {
    "@"
}

/// Interned foreign-call intrinsic names: MIR stores an intrinsic's name as
/// `&'static str`, and a program has one per foreign function it calls.
static FOREIGN_NAMES: std::sync::Mutex<Option<std::collections::HashSet<&'static str>>> =
    std::sync::Mutex::new(None);

impl<'a> ForeignCall<'a> {
    /// The intrinsic name for a call to `symbol` with compact `signature`
    /// (`params>ret`) from `library`.
    #[must_use]
    pub fn intrinsic_name(symbol: &str, signature: &str, library: &str) -> &'static str {
        let name = format!("{FOREIGN_PREFIX}{signature}:{library}:{symbol}");
        let mut names = FOREIGN_NAMES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names = names.get_or_insert_with(std::collections::HashSet::new);
        if let Some(&interned) = names.get(name.as_str()) {
            return interned;
        }
        let interned: &'static str = Box::leak(name.into_boxed_str());
        names.insert(interned);
        interned
    }

    /// The parts of `name` when it is a foreign-call intrinsic.
    #[must_use]
    pub fn parse(name: &'a str) -> Option<Self> {
        let parts = name.strip_prefix(FOREIGN_PREFIX)?;
        let (signature, located) = parts.split_once(':')?;
        let (library, symbol) = located.split_once(':')?;
        let (params, ret) = signature.split_once('>')?;
        let (ret, ret_layout) = if let Some(inner) = ret.strip_prefix("s(") {
            let inner = inner.strip_suffix(')')?;
            gossamer_abi::c_aggregate::CLayout::parse(inner)?;
            ('s', Some(inner))
        } else {
            let mut ret_chars = ret.chars();
            let ret = ret_chars.next()?;
            if ret_chars.next().is_some()
                || (ret != 'v' && gossamer_types::c_class_width(ret).is_none())
            {
                return None;
            }
            (ret, None)
        };
        if symbol.is_empty() {
            return None;
        }
        foreign_params(params)?;
        Some(Self {
            symbol,
            params,
            ret,
            ret_layout,
            library,
        })
    }

    /// A struct result's layout.
    #[must_use]
    pub fn ret_struct(&self) -> Option<gossamer_abi::c_aggregate::CLayout> {
        gossamer_abi::c_aggregate::CLayout::parse(self.ret_layout?)
    }

    /// The signature's call plan on `abi`: how each argument and the
    /// result travel.
    #[must_use]
    pub fn plan(
        &self,
        abi: gossamer_abi::c_aggregate::CAbi,
    ) -> gossamer_abi::c_aggregate::CallPlan {
        use gossamer_abi::c_aggregate::{CArg, plan_call};
        let params: Vec<CArg> = self
            .param_list()
            .into_iter()
            .map(|param| match param {
                ForeignParam::Scalar(class) => CArg::Scalar(class),
                ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => CArg::Scalar('L'),
                ForeignParam::ByValue(layout) => CArg::Aggregate(layout),
            })
            .collect();
        let ret = match (self.ret, self.ret_struct()) {
            ('v', _) => None,
            (_, Some(layout)) => Some(CArg::Aggregate(layout)),
            (class, None) => Some(CArg::Scalar(class)),
        };
        plan_call(abi, &params, ret.as_ref())
    }

    /// Whether the call goes through an address, its first operand, rather
    /// than a named symbol.
    #[must_use]
    pub fn is_indirect(&self) -> bool {
        self.symbol == gossamer_hir_indirect_symbol()
    }

    /// The call's parameters.
    #[must_use]
    pub fn param_list(&self) -> Vec<ForeignParam> {
        // `parse` validated the spelling.
        foreign_params(self.params).unwrap_or_default()
    }
}

/// The parts of a callback intrinsic: the address of a C-ABI entry that runs
/// a generated adapter, which `gossamer_hir`'s foreign-boundary pass names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignCallback<'a> {
    /// The parameter part of the entry's C signature: a class per scalar
    /// parameter, `s(layout)` per struct one.
    pub params: &'a str,
    /// The entry's result class, `v`, or `s` for a struct.
    pub ret: char,
    /// A struct result's layout spelling, inside `s(..)`.
    pub ret_layout: Option<&'a str>,
    /// The adapter body the entry runs: `fn(i64) -> i64` over the argument
    /// words.
    pub adapter: &'a str,
    /// The function the program passed, for reports; for an export, the C
    /// symbol the entry is defined as.
    pub name: &'a str,
    /// Whether the entry is an `#[export]` function's, defined externally
    /// under `name` rather than as an internal entry whose address is taken.
    pub exported: bool,
}

const CALLBACK_PREFIX: &str = "gos_ffi_callback:";
const EXPORT_PREFIX: &str = "gos_ffi_export:";

/// The C symbols of the `#[export]` entries among `bodies`, in declaration
/// order: the program's interface when it is built as a library.
#[must_use]
pub fn export_symbols(bodies: &[Body]) -> Vec<String> {
    let Some(exports) = bodies
        .iter()
        .find(|body| body.name == gossamer_hir::FFI_EXPORTS_FN)
    else {
        return Vec::new();
    };
    exports
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } => ForeignCallback::parse(name)
                .filter(|callback| callback.exported)
                .map(|callback| callback.name.to_string()),
            _ => None,
        })
        .collect()
}

/// The parts of a foreign-static intrinsic: the address of a C global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignStatic<'a> {
    /// The C symbol.
    pub symbol: &'a str,
    /// The library `#[link(name = ..)]` names, or `""`.
    pub library: &'a str,
}

const STATIC_PREFIX: &str = "gos_ffi_static:";

impl<'a> ForeignStatic<'a> {
    /// The intrinsic name for the address of `symbol` from `library`.
    #[must_use]
    pub fn intrinsic_name(symbol: &str, library: &str) -> &'static str {
        let name = format!("{STATIC_PREFIX}{library}:{symbol}");
        let mut names = FOREIGN_NAMES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names = names.get_or_insert_with(std::collections::HashSet::new);
        if let Some(&interned) = names.get(name.as_str()) {
            return interned;
        }
        let interned: &'static str = Box::leak(name.into_boxed_str());
        names.insert(interned);
        interned
    }

    /// The parts of `name` when it is a foreign-static intrinsic.
    #[must_use]
    pub fn parse(name: &'a str) -> Option<Self> {
        let (library, symbol) = name.strip_prefix(STATIC_PREFIX)?.split_once(':')?;
        (!symbol.is_empty()).then_some(Self { symbol, library })
    }
}

impl<'a> ForeignCallback<'a> {
    /// The intrinsic name for an entry with C signature `signature`
    /// (`params>ret`) running `adapter`, made for the function `name`.
    #[must_use]
    pub fn intrinsic_name(signature: &str, adapter: &str, name: &str) -> &'static str {
        Self::intern(format!("{CALLBACK_PREFIX}{signature}:{adapter}:{name}"))
    }

    /// The intrinsic name for the external entry `symbol` with C signature
    /// `signature` running `adapter`.
    #[must_use]
    pub fn export_intrinsic_name(signature: &str, adapter: &str, symbol: &str) -> &'static str {
        Self::intern(format!("{EXPORT_PREFIX}{signature}:{adapter}:{symbol}"))
    }

    fn intern(name: String) -> &'static str {
        let mut names = FOREIGN_NAMES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names = names.get_or_insert_with(std::collections::HashSet::new);
        if let Some(&interned) = names.get(name.as_str()) {
            return interned;
        }
        let interned: &'static str = Box::leak(name.into_boxed_str());
        names.insert(interned);
        interned
    }

    /// The parts of `name` when it is a callback intrinsic.
    #[must_use]
    pub fn parse(name: &'a str) -> Option<Self> {
        let (encoded, exported) = match name.strip_prefix(CALLBACK_PREFIX) {
            Some(encoded) => (encoded, false),
            None => (name.strip_prefix(EXPORT_PREFIX)?, true),
        };
        let (signature, labels) = encoded.split_once(':')?;
        let (adapter, name) = labels.split_once(':')?;
        let (params, result) = signature.split_once('>')?;
        let (ret, ret_layout) = if let Some(inner) = result.strip_prefix("s(") {
            let inner = inner.strip_suffix(')')?;
            gossamer_abi::c_aggregate::CLayout::parse(inner)?;
            ('s', Some(inner))
        } else {
            let mut result_chars = result.chars();
            let ret = result_chars.next()?;
            if result_chars.next().is_some()
                || (ret != 'v' && gossamer_types::c_class_width(ret).is_none())
            {
                return None;
            }
            (ret, None)
        };
        let scalars_and_structs = foreign_params(params)?
            .iter()
            .all(|param| matches!(param, ForeignParam::Scalar(_) | ForeignParam::ByValue(_)));
        if adapter.is_empty() || !scalars_and_structs {
            return None;
        }
        Some(Self {
            params,
            ret,
            ret_layout,
            adapter,
            name,
            exported,
        })
    }

    /// The entry's parameters.
    #[must_use]
    pub fn param_list(&self) -> Vec<ForeignParam> {
        // `parse` validated the spelling.
        foreign_params(self.params).unwrap_or_default()
    }

    /// Whether the entry moves a struct by value or answers one.
    #[must_use]
    pub fn has_struct(&self) -> bool {
        self.ret_layout.is_some()
            || self
                .param_list()
                .iter()
                .any(|param| matches!(param, ForeignParam::ByValue(_)))
    }

    /// How the entry's arguments and result arrive and leave on `abi`.
    #[must_use]
    pub fn plan(
        &self,
        abi: gossamer_abi::c_aggregate::CAbi,
    ) -> gossamer_abi::c_aggregate::CallPlan {
        use gossamer_abi::c_aggregate::{CArg, CLayout, plan_call};
        let params: Vec<CArg> = self
            .param_list()
            .into_iter()
            .map(|param| match param {
                ForeignParam::ByValue(layout) => CArg::Aggregate(layout),
                ForeignParam::Scalar(class) => CArg::Scalar(class),
                ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => CArg::Scalar('L'),
            })
            .collect();
        let ret = match (self.ret, self.ret_layout.and_then(CLayout::parse)) {
            ('v', _) => None,
            (_, Some(layout)) => Some(CArg::Aggregate(layout)),
            (class, None) => Some(CArg::Scalar(class)),
        };
        plan_call(abi, &params, ret.as_ref())
    }
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
            | "gos_rt_map_inc_ekey"
            | "gos_rt_map_range_ekey"
            | "gos_rt_set_range_ekey" => Self::MapEnumKey,
            "gos_fn_addr" => Self::FnAddr,
            "gos_rt_weak_opt_payload" => Self::WeakOptPayload,
            "gos_jit_unsupported_user_iterator" => Self::JitUnsupportedUserIterator,
            "f64.sqrt" | "sqrt" => Self::F64Math(F64MathIntrinsic::Sqrt),
            "f64.sin" | "sin" => Self::F64Math(F64MathIntrinsic::Sin),
            "f64.cos" | "cos" => Self::F64Math(F64MathIntrinsic::Cos),
            "f64.abs" | "fabs" | "abs" => Self::F64Math(F64MathIntrinsic::Abs),
            "f64.floor" | "floor" => Self::F64Math(F64MathIntrinsic::Floor),
            "f64.ceil" | "ceil" => Self::F64Math(F64MathIntrinsic::Ceil),
            "f64.exp" | "exp" => Self::F64Math(F64MathIntrinsic::Exp),
            "f64.ln" | "ln" => Self::F64Math(F64MathIntrinsic::Log),
            other if gossamer_abi::lookup(other).is_some() => Self::Runtime,
            other if ForeignCall::parse(other).is_some() => Self::Foreign,
            other if ForeignCallback::parse(other).is_some() => Self::ForeignCallback,
            other if ForeignStatic::parse(other).is_some() => Self::ForeignStatic,
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
            Self::MapEnumKey => RawIntrinsicArity::Range { min: 3, max: 5 },
            Self::EnumDiscTag
            | Self::EnumUntag
            | Self::EnumDisc
            | Self::FnAddr
            | Self::WeakOptPayload
            | Self::F64Math(_) => RawIntrinsicArity::Exact(1),
            Self::Alloc => RawIntrinsicArity::Range { min: 0, max: 1 },
            Self::RcAlloc | Self::RcAllocTagged => RawIntrinsicArity::Range { min: 0, max: 2 },
            Self::JitUnsupportedUserIterator | Self::ForeignStatic => RawIntrinsicArity::Exact(0),
            Self::Foreign => {
                RawIntrinsicArity::Exact(ForeignCall::parse(name).map_or(usize::MAX, |call| {
                    call.param_list().len()
                        + usize::from(call.is_indirect())
                        + usize::from(call.ret_layout.is_some())
                }))
            }
            Self::ForeignCallback => RawIntrinsicArity::Exact(1),
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
    /// Explicit `wrapping_sub`.
    WrappingSub,
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
