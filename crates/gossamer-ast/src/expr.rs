//! Expression nodes covering every production in SPEC §15.

#![forbid(unsafe_code)]

use gossamer_lex::Span;

use crate::common::{AssignOp, BinaryOp, Ident, RangeKind, UnaryOp};
use crate::node_id::NodeId;
use crate::pattern::Pattern;
use crate::stmt::Stmt;
use crate::ty::{GenericArg, Type};

/// A syntactic expression, carrying a stable id and source span.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Expr {
    /// Unique id within the enclosing source file.
    pub id: NodeId,
    /// Source range covered by this expression.
    pub span: Span,
    /// The kind of expression being represented.
    pub kind: ExprKind,
}

impl Expr {
    /// Constructs a new expression node with the given id, span, and kind.
    #[must_use]
    pub fn new(id: NodeId, span: Span, kind: ExprKind) -> Self {
        Self { id, span, kind }
    }

    /// Whether this expression is the `_` wildcard, which names no binding
    /// and is written for a destructuring-assignment element to discard.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        matches!(&self.kind, ExprKind::Path(path)
            if path.segments.len() == 1
                && path.segments[0].generics.is_empty()
                && path.segments[0].name.name == "_")
    }

    /// Whether this expression names a place an assignment can write
    /// through: a binding or item path, a field, an index, or a
    /// dereference. Every other expression answers a temporary.
    #[must_use]
    pub fn is_place(&self) -> bool {
        matches!(
            &self.kind,
            ExprKind::Path(_)
                | ExprKind::FieldAccess { .. }
                | ExprKind::Index { .. }
                | ExprKind::Unary {
                    op: UnaryOp::Deref,
                    ..
                }
        )
    }
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

/// Every expression production in the grammar.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExprKind {
    /// Literal value.
    Literal(Literal),
    /// Name path `a::b::c` or `a::b::<T>::c`.
    Path(PathExpr),
    /// Function call `callee(arg1, arg2, ...)`.
    Call {
        /// Callee expression.
        callee: Box<Expr>,
        /// Call arguments.
        args: Vec<Expr>,
    },
    /// Method call `receiver.name::<T1, T2>(args)`.
    MethodCall {
        /// Receiver expression.
        receiver: Box<Expr>,
        /// Method name.
        name: Ident,
        /// Source range of the method name, so a diagnostic about the method
        /// points at it rather than at the receiver it hangs off.
        name_span: Span,
        /// The method the source actually wrote, when a parse-time desugar
        /// synthesized this call under a different name. A diagnostic names
        /// this rather than `name`, so it never reports a spelling that
        /// appears nowhere in the file.
        desugared_from: Option<Ident>,
        /// Turbofish generic arguments.
        generics: Vec<GenericArg>,
        /// Call arguments.
        args: Vec<Expr>,
    },
    /// Field access `receiver.name` or tuple index `receiver.0`.
    FieldAccess {
        /// Receiver expression.
        receiver: Box<Expr>,
        /// Field selector.
        field: FieldSelector,
    },
    /// Index expression `base[index]`.
    Index {
        /// Base expression.
        base: Box<Expr>,
        /// Index expression.
        index: Box<Expr>,
    },
    /// Prefix unary expression `-x`, `!x`, `&x`, `&mut x`, `*x`.
    Unary {
        /// Operator applied.
        op: UnaryOp,
        /// Operand expression.
        operand: Box<Expr>,
    },
    /// Infix binary expression `lhs op rhs`.
    Binary {
        /// Operator applied.
        op: BinaryOp,
        /// Left-hand operand.
        lhs: Box<Expr>,
        /// Right-hand operand.
        rhs: Box<Expr>,
    },
    /// Assignment expression `place op rhs`.
    Assign {
        /// Assignment operator (`=`, `+=`, ...).
        op: AssignOp,
        /// Place being assigned to.
        place: Box<Expr>,
        /// Right-hand value.
        value: Box<Expr>,
    },
    /// Cast expression `expr as Type` or `expr as% Type`.
    Cast {
        /// Expression being cast.
        value: Box<Expr>,
        /// Target type.
        ty: Box<Type>,
        /// `true` for `expr as% Type`, which keeps an integer's low bits and
        /// saturates a float rather than checking that the value fits.
        #[serde(default)]
        wrapping: bool,
    },
    /// `if` / `else if` / `else` chain.
    If {
        /// Condition expression.
        condition: Box<Expr>,
        /// Then-branch block expression.
        then_branch: Box<Expr>,
        /// Optional else branch (another `If` or a block).
        else_branch: Option<Box<Expr>>,
    },
    /// `match scrutinee { arms }`.
    Match {
        /// Scrutinee expression.
        scrutinee: Box<Expr>,
        /// Arms in source order.
        arms: Vec<MatchArm>,
    },
    /// `loop { body }` with an optional label.
    Loop {
        /// Optional label `'ident:`.
        label: Option<Label>,
        /// Block body.
        body: Box<Expr>,
    },
    /// `while cond { body }` with an optional label.
    While {
        /// Optional label `'ident:`.
        label: Option<Label>,
        /// Loop condition.
        condition: Box<Expr>,
        /// Block body.
        body: Box<Expr>,
    },
    /// `for pat in iter { body }` with an optional label.
    For {
        /// Optional label `'ident:`.
        label: Option<Label>,
        /// Binding pattern for the iteration value.
        pattern: Pattern,
        /// Iterator expression.
        iter: Box<Expr>,
        /// Block body.
        body: Box<Expr>,
    },
    /// Block expression `{ stmts; tail? }`.
    Block(Block),
    /// Closure expression `|params| body` or `|params| -> Ret { body }`.
    ///
    /// Gossamer has no ownership transfer, so there is no `move`
    /// qualifier: closures always capture by GC reference for heap
    /// types and by copy for `Copy` types.
    Closure {
        /// Parameter patterns with optional type annotations.
        params: Vec<ClosureParam>,
        /// Optional explicit return type.
        ret: Option<Type>,
        /// Closure body expression.
        body: Box<Expr>,
    },
    /// `return expr?`.
    Return(Option<Box<Expr>>),
    /// `break 'label? expr?`.
    Break {
        /// Optional label to break to.
        label: Option<Label>,
        /// Optional value returned from a `loop`.
        value: Option<Box<Expr>>,
    },
    /// `continue 'label?`.
    Continue {
        /// Optional label to continue to.
        label: Option<Label>,
    },
    /// Tuple expression `(a, b, c)` with two or more elements. The empty tuple
    /// `()` is represented by `Literal(Literal::Unit)`.
    Tuple(Vec<Expr>),
    /// Hash map literal `{key: value, ...}`.
    MapLiteral(Vec<Expr>),
    /// Hash set literal `#{a, b, c}`.
    SetLiteral(Vec<Expr>),
    /// Struct construction represented with named fields internally.
    Struct {
        /// Path naming the struct.
        path: PathExpr,
        /// Field initializers.
        fields: Vec<StructExprField>,
        /// Optional `..base` functional update.
        base: Option<Box<Expr>>,
        /// Source form that produced this construction.
        syntax: StructExprSyntax,
    },
    /// Vec literal `[a, b, c]` or `[value; count]`.
    Array(ArrayExpr),
    /// Fixed array literal `#[a, b, c]` or `#[value; count]`.
    FixedArray(ArrayExpr),
    /// Range expression `lo..hi` / `lo..=hi`; bounds may be omitted.
    Range {
        /// Lower bound, if present.
        start: Option<Box<Expr>>,
        /// Upper bound, if present.
        end: Option<Box<Expr>>,
        /// Whether the upper bound is inclusive.
        kind: RangeKind,
    },
    /// `unsafe { ... }` block.
    Unsafe(Block),
    /// `expr?` - the `?` operator.
    Try(Box<Expr>),
    /// `select { arms }` expression.
    Select(Vec<SelectArm>),
    /// Synthetic error placeholder inserted during error recovery. Downstream
    /// passes return a fresh type variable or unit when they encounter this
    /// variant, suppressing cascading diagnostics for the same malformed
    /// sub-expression.
    Error,
}

/// Source spelling of a struct construction before lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StructExprSyntax {
    /// Tuple-struct constructor syntax: `Name(args...)`.
    Parenthesized,
    /// Braced struct-literal syntax: `Name { field: value }` or `Name { value }`.
    Braced,
}

/// Literal values appearing in expressions and patterns.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Literal {
    /// Integer literal preserved in source form (e.g. `0x2a`, `1_000`).
    Int(String),
    /// Floating-point literal preserved in source form.
    Float(String),
    /// Double-quoted string literal with lexer-decoded contents.
    ///
    /// Pretty-printing escapes the contents back into a valid string literal.
    String(String),
    /// Raw string literal `r"..."` with the given number of surrounding `#` marks.
    RawString {
        /// Number of `#` characters on each side.
        hashes: u8,
        /// Raw contents, unescaped.
        value: String,
    },
    /// Char literal `'a'` decoded to a single scalar value.
    Char(char),
    /// Byte literal `b'a'`.
    Byte(u8),
    /// Byte string literal `b"..."`.
    ByteString(Vec<u8>),
    /// Raw byte string literal `br"..."`.
    RawByteString {
        /// Number of `#` characters on each side.
        hashes: u8,
        /// Raw bytes, unescaped.
        value: Vec<u8>,
    },
    /// Boolean literal `true` or `false`.
    Bool(bool),
    /// The unit value `()`.
    Unit,
}

/// A label identifier like `'outer` attached to a loop or referenced by `break`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Label {
    /// Label name without the leading apostrophe.
    pub name: String,
}

impl Label {
    /// Constructs a label from its textual name (no apostrophe).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// A field selector on the right-hand side of `.`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FieldSelector {
    /// Named field `foo.bar`.
    Named(Ident),
    /// Tuple index `foo.0`.
    Index(u32),
}

/// A `.`-separated path used as an expression.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PathExpr {
    /// Path segments in order.
    pub segments: Vec<PathSegment>,
}

impl PathExpr {
    /// Constructs a single-segment path with no generic arguments.
    #[must_use]
    pub fn single(name: impl Into<String>) -> Self {
        Self {
            segments: vec![PathSegment::new(name)],
        }
    }

    /// Constructs a multi-segment path from an iterator of segment names.
    #[must_use]
    pub fn from_names<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            segments: segments.into_iter().map(PathSegment::new).collect(),
        }
    }
}

/// A single `::`-delimited segment in an expression path.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PathSegment {
    /// Segment name.
    pub name: Ident,
    /// Generic arguments applied at this segment (turbofish style).
    pub generics: Vec<GenericArg>,
}

impl PathSegment {
    /// Constructs a segment with no generic arguments.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: Ident::new(name),
            generics: Vec::new(),
        }
    }
}

/// One arm of a `match` expression.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MatchArm {
    /// Pattern to match.
    pub pattern: Pattern,
    /// Optional `if` guard.
    pub guard: Option<Expr>,
    /// Right-hand side of `=>`.
    pub body: Expr,
}

/// Block expression consisting of statements and an optional tail expression.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Block {
    /// Statements in source order, excluding the optional tail expression.
    pub stmts: Vec<Stmt>,
    /// Optional tail expression that becomes the block's value.
    pub tail: Option<Box<Expr>>,
    /// True for parser-synthesized blocks with no source spelling
    /// (the implicit empty else arm of an else-less `if let`).
    /// Lints must not attribute these to the user.
    #[serde(default)]
    pub synthetic: bool,
    /// Which construct this block came from. The three markers are
    /// mutually exclusive - a block is desugared from exactly one of
    /// them, or from none.
    #[serde(default)]
    pub kind: BlockKind,
}

/// The construct a block was desugared from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BlockKind {
    /// An ordinary `{ }` block.
    #[default]
    Plain,
    /// Desugared from an `arena { }` statement. The front-end runs the
    /// arena-escape check (GM0003) only on these, so the checked surface
    /// stays distinct from the raw `runtime::arena_push()` / `arena_pop()`
    /// primitive.
    Arena,
    /// Desugared from a `cohort { }`. The lint pass reads it to find a
    /// detached `go` inside a cohort, which keeps that check off
    /// hand-written `runtime::cohort_push()` calls.
    Cohort,
    /// Spelled `comptime { ... }`. The comptime fold pass evaluates these
    /// on the bytecode VM during compilation and splices the resulting
    /// literal back into the source, so every tier sees a constant rather
    /// than the original computation.
    Comptime,
}

impl Block {
    /// Constructs an empty parser-synthesized block (no statements,
    /// no tail, no source spelling).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            stmts: Vec::new(),
            tail: None,
            synthetic: true,
            kind: BlockKind::Plain,
        }
    }

    /// Whether this block came from an `arena { }` statement.
    #[must_use]
    pub fn is_arena(&self) -> bool {
        self.kind == BlockKind::Arena
    }

    /// Whether this block came from a `cohort { }`.
    #[must_use]
    pub fn is_cohort(&self) -> bool {
        self.kind == BlockKind::Cohort
    }

    /// Whether this block was spelled `comptime { ... }`.
    #[must_use]
    pub fn is_comptime(&self) -> bool {
        self.kind == BlockKind::Comptime
    }
}

/// A closure parameter: pattern, optional type annotation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClosureParam {
    /// Binding pattern for the parameter.
    pub pattern: Pattern,
    /// Optional type annotation.
    pub ty: Option<Type>,
}

/// A single initializer inside a struct literal.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StructExprField {
    /// Field name being initialized.
    pub name: Ident,
    /// Initializer expression. The parser gives a shorthand field (`Point {
    /// x }`) the path naming the binding, so it resolves and checks like
    /// `x: x`.
    pub value: Option<Expr>,
    /// Whether the source wrote the field as shorthand.
    #[serde(default)]
    pub shorthand: bool,
}

impl StructExprField {
    /// Constructs an explicit initializer `name: value`.
    #[must_use]
    pub fn explicit(name: impl Into<String>, value: Expr) -> Self {
        Self {
            name: Ident::new(name),
            value: Some(value),
            shorthand: false,
        }
    }
}

/// Array expression form.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ArrayExpr {
    /// Explicit element list `[a, b, c]`.
    List(Vec<Expr>),
    /// Repeat form `[value; count]`.
    Repeat {
        /// Value to repeat.
        value: Box<Expr>,
        /// Count expression.
        count: Box<Expr>,
    },
}

/// One arm of a `select` expression.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SelectArm {
    /// The communication or default operation.
    pub op: SelectOp,
    /// Right-hand side body after `=>`.
    pub body: Expr,
}

/// Operation performed by a `select` arm.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SelectOp {
    /// `pat = chan.recv()`.
    Recv {
        /// Pattern bound to the received value.
        pattern: Pattern,
        /// Channel expression.
        channel: Expr,
    },
    /// `chan.send(value)`.
    Send {
        /// Channel expression.
        channel: Expr,
        /// Value to send.
        value: Expr,
    },
    /// `default`.
    Default,
}
