//! HIR data types.
//! The HIR is a structurally simplified form of the AST. Control-flow
//! sugar such as `for`, `?`, and the forward-pipe `|>` has been
//! desugared; every node carries a [`HirId`] and an optional [`Ty`]
//! annotation propagated.

#![forbid(unsafe_code)]

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::Ty;

use crate::ids::HirId;

/// Whole program - the collection of items lowered from a source file.
#[derive(Debug, Clone, Default)]
pub struct HirProgram {
    /// Items in source order.
    pub items: Vec<HirItem>,
}

/// One top-level item.
#[derive(Debug, Clone)]
pub struct HirItem {
    /// Stable id for this item.
    pub id: HirId,
    /// Source range.
    pub span: Span,
    /// `DefId` associated with the item, when resolver assigned one.
    pub def: Option<DefId>,
    /// Module path the item was originally declared inside, in
    /// outer-to-inner order. Empty when the item is at the source
    /// file's top level. Items lowered out of `mod foo { mod bar { ... } }`
    /// arrive here as `["foo", "bar"]`. Loaders use the path to
    /// register the item under both its bare name (`item`) and the
    /// fully-qualified key (`foo::bar::item`) so cross-module
    /// callers like `bar::item()` can resolve at runtime.
    pub module_path: Vec<String>,
    /// Item variant.
    pub kind: HirItemKind,
}

/// HIR item kinds mirror [`gossamer_ast::ItemKind`] but drop forms that
/// don't produce lowered bodies (attributes, external modules).
#[derive(Debug, Clone)]
pub enum HirItemKind {
    /// Function or method declaration.
    Fn(HirFn),
    /// `const NAME: T = EXPR;`.
    Const(HirConst),
    /// `static [mut] NAME: T = EXPR;`.
    Static(HirStatic),
    /// Aggregate declaration whose body is its field/variant list.
    Adt(HirAdt),
    /// `impl` block. Items inside are flattened into `Fn` items after
    /// lowering.
    Impl(HirImpl),
    /// `trait` declaration with its methods.
    Trait(HirTrait),
}

/// Lowered function declaration.
#[derive(Debug, Clone)]
pub struct HirFn {
    /// Function name.
    pub name: Ident,
    /// Parameter types paired with their binding patterns.
    pub params: Vec<HirParam>,
    /// Declared return type.
    pub ret: Option<Ty>,
    /// Function body, if present (trait method signatures have no
    /// body).
    pub body: Option<HirBody>,
    /// `true` when the declaration is `unsafe`.
    pub is_unsafe: bool,
    /// `true` when the declaration is `comptime`. Calls to such a
    /// function are evaluated at compile time by the comptime fold pass.
    pub is_comptime: bool,
    /// `true` when the first parameter is a `self` receiver.
    pub has_self: bool,
    /// Where the declaration came from.
    pub origin: FnOrigin,
}

/// Whether a function was written as a `fn` item or synthesized from a
/// closure by the lift pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnOrigin {
    /// Declared as a `fn` item in source.
    Declared,
    /// Synthesized by the closure lift. Such a body runs once per element
    /// when a sequence combinator drives it, so it is a candidate for an
    /// invocation-scoped automatic region.
    LiftedClosure,
}

/// Lowered parameter.
#[derive(Debug, Clone)]
pub struct HirParam {
    /// Binding pattern.
    pub pattern: HirPat,
    /// Resolved type of the parameter.
    pub ty: Ty,
    /// `true` when the parameter was declared `comptime`; the matching
    /// call argument is folded at compile time.
    pub is_comptime: bool,
}

/// Lowered constant item.
#[derive(Debug, Clone)]
pub struct HirConst {
    /// Item name.
    pub name: Ident,
    /// Resolved type of the constant.
    pub ty: Ty,
    /// Initializer expression.
    pub value: HirExpr,
}

/// Lowered static item.
#[derive(Debug, Clone)]
pub struct HirStatic {
    /// Item name.
    pub name: Ident,
    /// Resolved type of the static.
    pub ty: Ty,
    /// Mutability as declared.
    pub mutable: bool,
    /// Initializer expression.
    pub value: HirExpr,
}

/// Lowered struct/enum declaration.
#[derive(Debug, Clone)]
pub struct HirAdt {
    /// Declared name.
    pub name: Ident,
    /// Kind of aggregate.
    pub kind: HirAdtKind,
    /// Resolved self type for convenience.
    pub self_ty: Ty,
    /// How an enum's discriminant is stored. A struct leaves it at its
    /// default, which describes no packing.
    pub repr: gossamer_ast::EnumRepr,
}

/// Kind of aggregate at the HIR level.
#[derive(Debug, Clone)]
pub enum HirAdtKind {
    /// `struct` with named, positional, or unit body. The embedded
    /// list carries the field names in declaration order - empty for
    /// a unit struct or tuple struct with positional-only fields.
    Struct(Vec<Ident>),
    /// `enum` with the given variants. Each variant carries its
    /// name plus an optional ordered field list - `None` for unit
    /// (`Line`) and tuple variants (`Circle(f64)`); `Some(names)`
    /// for struct-payload variants (`Rect { w, h }`). The MIR
    /// lowerer reads the field names so `__struct("Rect", ...)`
    /// calls can reorder operands into declaration order even
    /// when the matching enum variant rather than a real struct
    /// is the target.
    Enum(Vec<HirEnumVariant>),
}

impl HirAdtKind {
    /// The variants an enum declares, or an empty slice for a struct.
    #[must_use]
    pub fn variants(&self) -> &[HirEnumVariant] {
        match self {
            Self::Enum(variants) => variants,
            Self::Struct(_) => &[],
        }
    }
}

/// A single enum variant's HIR representation.
#[derive(Debug, Clone)]
pub struct HirEnumVariant {
    /// Variant name (e.g. `Rect` in `enum Shape { Rect { w, h } }`).
    pub name: Ident,
    /// Ordered field names for struct-payload variants. `None`
    /// for unit and tuple-struct variants.
    pub struct_fields: Option<Vec<Ident>>,
    /// Ordered field types matching `struct_fields`, parallel
    /// vector. Filled by [`crate::lower`] from the AST so the MIR
    /// lowerer can emit typed loads for `Shape::Rect { w, h }`
    /// match arms (without this the loaded i64 was printed verbatim
    /// for f64 fields). Length matches `struct_fields`'s when
    /// present.
    pub struct_field_tys: Option<Vec<crate::tree::Ty>>,
}

/// Lowered `impl` block.
#[derive(Debug, Clone)]
pub struct HirImpl {
    /// Self type the impl attaches to.
    pub self_ty: Ty,
    /// Syntactic name of the self type (last path segment of the
    /// impl header's type expression), if the impl targets a nominal
    /// type. `impl Counter { ... }` yields `Some("Counter")`;
    /// reference and tuple self types yield `None`. Used by the
    /// tree-walker to key method lookups by type name so two impls
    /// with the same method name on different types do not collide.
    pub self_name: Option<Ident>,
    /// Trait being implemented, if any, represented by its name.
    pub trait_name: Option<Ident>,
    /// Method items in source order.
    pub methods: Vec<HirFn>,
}

/// Lowered `trait` declaration.
#[derive(Debug, Clone)]
pub struct HirTrait {
    /// Trait name.
    pub name: Ident,
    /// Method items in declaration order.
    pub methods: Vec<HirFn>,
}

/// Body of a function/closure/const initializer.
#[derive(Debug, Clone)]
pub struct HirBody {
    /// Root block evaluated by the body.
    pub block: HirBlock,
}

/// Block expression: statements followed by an optional tail.
#[derive(Debug, Clone)]
pub struct HirBlock {
    /// Stable id for this block.
    pub id: HirId,
    /// Source range covering the block.
    pub span: Span,
    /// Statements executed in order.
    pub stmts: Vec<HirStmt>,
    /// Optional tail expression whose value is the block's result.
    pub tail: Option<Box<HirExpr>>,
    /// Type of the block (unit when no tail).
    pub ty: Ty,
    /// `true` when this block was spelled `comptime { ... }`. The
    /// comptime fold pass evaluates it on the bytecode VM during
    /// compilation and replaces it in source with its result literal.
    pub is_comptime: bool,
}

/// Single statement inside a block.
#[derive(Debug, Clone)]
pub struct HirStmt {
    /// Stable id.
    pub id: HirId,
    /// Source range.
    pub span: Span,
    /// Statement kind.
    pub kind: HirStmtKind,
}

/// Statement kinds at the HIR level.
#[derive(Debug, Clone)]
pub enum HirStmtKind {
    /// `let pat = expr`.
    Let {
        /// Binding pattern.
        pattern: HirPat,
        /// Declared type of the binding.
        ty: Ty,
        /// Optional initializer.
        init: Option<HirExpr>,
    },
    /// Expression used as a statement (with or without trailing `;`).
    Expr {
        /// Expression evaluated for effect.
        expr: HirExpr,
        /// `true` when the expression was followed by `;`.
        has_semi: bool,
    },
    /// `defer { ... }`.
    Defer(HirExpr),
    /// A nested item declaration.
    Item(Box<HirItem>),
}

/// Expression node.
#[derive(Debug, Clone)]
pub struct HirExpr {
    /// Stable id.
    pub id: HirId,
    /// Source range.
    pub span: Span,
    /// Resolved type for this expression.
    pub ty: Ty,
    /// Variant.
    pub kind: HirExprKind,
}

/// One arm of a `select { … }` expression after HIR lowering.
#[derive(Debug, Clone)]
pub struct HirSelectArm {
    /// Operation kind - recv on a channel, send on a channel, or the
    /// default fallback arm.
    pub op: HirSelectOp,
    /// Body evaluated when this arm is chosen.
    pub body: HirExpr,
}

/// Operation performed by a `select` arm.
#[derive(Debug, Clone)]
pub enum HirSelectOp {
    /// `pat = chan.recv()` - receive from `channel`, binding the
    /// received value to `pattern`.
    Recv {
        /// Pattern that receives the value.
        pattern: HirPat,
        /// Channel expression.
        channel: HirExpr,
    },
    /// `chan.send(value)` - send `value` on `channel`.
    Send {
        /// Channel expression.
        channel: HirExpr,
        /// Value being sent.
        value: HirExpr,
    },
    /// `default`.
    Default,
}

/// HIR expression kinds.
#[derive(Debug, Clone)]
pub enum HirExprKind {
    /// Primitive literal preserved in source form.
    Literal(HirLiteral),
    /// Named path reference resolved to a concrete target.
    Path {
        /// Path segments.
        segments: Vec<Ident>,
        /// Resolved definition id, when the resolver produced one.
        def: Option<DefId>,
    },
    /// Direct function call.
    Call {
        /// Callee expression.
        callee: Box<HirExpr>,
        /// Call arguments.
        args: Vec<HirExpr>,
    },
    /// Method call.
    MethodCall {
        /// Receiver expression.
        receiver: Box<HirExpr>,
        /// Method name.
        name: Ident,
        /// Call arguments.
        args: Vec<HirExpr>,
        /// The `impl` block this call resolves to, named by the type the block
        /// was written for, when the checker knew the receiver's type.
        ///
        /// Below this point a container and a structural type both reach a
        /// method as an untyped handle, so the receiver no longer says which
        /// block to call. Carrying the answer is what lets two types implement
        /// one trait and each call reach its own body.
        owner: Option<Ident>,
    },
    /// Field access `receiver.name`.
    Field {
        /// Receiver expression.
        receiver: Box<HirExpr>,
        /// Field name.
        name: Ident,
    },
    /// Tuple index `receiver.0`.
    TupleIndex {
        /// Receiver expression.
        receiver: Box<HirExpr>,
        /// Tuple index.
        index: u32,
    },
    /// Indexing `base[index]`.
    Index {
        /// Base expression.
        base: Box<HirExpr>,
        /// Index expression.
        index: Box<HirExpr>,
    },
    /// Unary operator.
    Unary {
        /// Operator.
        op: HirUnaryOp,
        /// Operand.
        operand: Box<HirExpr>,
    },
    /// Binary operator.
    Binary {
        /// Operator.
        op: HirBinaryOp,
        /// Left operand.
        lhs: Box<HirExpr>,
        /// Right operand.
        rhs: Box<HirExpr>,
    },
    /// Assignment.
    Assign {
        /// Place being assigned to.
        place: Box<HirExpr>,
        /// Value being stored.
        value: Box<HirExpr>,
    },
    /// `if` / `else` chain.
    If {
        /// Condition expression.
        condition: Box<HirExpr>,
        /// Then branch.
        then_branch: Box<HirExpr>,
        /// Optional else branch.
        else_branch: Option<Box<HirExpr>>,
    },
    /// `match` expression.
    Match {
        /// Scrutinee expression.
        scrutinee: Box<HirExpr>,
        /// Arms in source order.
        arms: Vec<HirMatchArm>,
    },
    /// `loop { body }`, optionally carrying a loop label `'name`.
    Loop {
        /// Body expression.
        body: Box<HirExpr>,
        /// Optional loop label (without the leading apostrophe).
        label: Option<String>,
    },
    /// `while cond { body }`, optionally carrying a loop label `'name`.
    While {
        /// Condition.
        condition: Box<HirExpr>,
        /// Body.
        body: Box<HirExpr>,
        /// Optional loop label (without the leading apostrophe).
        label: Option<String>,
    },
    /// Block expression.
    Block(HirBlock),
    /// Closure expression.
    Closure {
        /// Parameters.
        params: Vec<HirParam>,
        /// Optional return type.
        ret: Option<Ty>,
        /// Body expression.
        body: Box<HirExpr>,
    },
    /// Post-lifting reference to a closure whose body has been moved
    /// to a synthetic top-level function. `captures` holds the
    /// expressions that produce each captured value in declaration
    /// order; the MIR lowerer stores them on the heap and tracks
    /// which local holds the resulting env pointer so subsequent
    /// direct calls can be dispatched to `name` natively.
    LiftedClosure {
        /// Synthetic top-level function name (`__closure_N`).
        name: Ident,
        /// Captured-value expressions in the same order the lifted
        /// function's `gos_load`s expect them.
        captures: Vec<HirExpr>,
    },
    /// `select { … }` expression. Preserves the channel/default arm
    /// structure so the evaluator can poll each channel's readiness
    /// at runtime and pick the first ready arm, falling back to the
    /// `default` arm when none are ready.
    Select {
        /// Arms in source order.
        arms: Vec<HirSelectArm>,
    },
    /// `return expr?`.
    Return(Option<Box<HirExpr>>),
    /// `break ['label] [value]`.
    Break {
        /// Optional value returned from a `loop`.
        value: Option<Box<HirExpr>>,
        /// Optional target loop label (without the leading apostrophe).
        label: Option<String>,
    },
    /// `continue ['label]`.
    Continue {
        /// Optional target loop label (without the leading apostrophe).
        label: Option<String>,
    },
    /// Tuple literal.
    Tuple(Vec<HirExpr>),
    /// Array literal (explicit or repeat form).
    Array(HirArrayExpr),
    /// Cast `expr as T`.
    Cast {
        /// Value being cast.
        value: Box<HirExpr>,
        /// Target type after lowering.
        ty: Ty,
    },
    /// Range expression `a..b` / `a..=b`.
    Range {
        /// Lower bound.
        start: Option<Box<HirExpr>>,
        /// Upper bound.
        end: Option<Box<HirExpr>>,
        /// `true` when the upper bound is inclusive.
        inclusive: bool,
    },
    /// Unresolved placeholder for forms the lowerer does not yet
    /// rewrite (e.g. macro invocations, select expressions).
    Placeholder,
}

/// Literal values at the HIR level.
#[derive(Debug, Clone)]
pub enum HirLiteral {
    /// Integer literal preserved verbatim.
    Int(String),
    /// Float literal preserved verbatim.
    Float(String),
    /// String literal with lexer-decoded contents.
    String(String),
    /// Char literal.
    Char(char),
    /// Byte literal.
    Byte(u8),
    /// Byte-string literal.
    ByteString(Vec<u8>),
    /// Boolean literal.
    Bool(bool),
    /// Unit literal `()`.
    Unit,
}

/// Unary operators at the HIR level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HirUnaryOp {
    /// `-x`.
    Neg,
    /// `!x`.
    Not,
    /// `&x`.
    RefShared,
    /// `&mut x`.
    RefMut,
    /// `*x` (raw deref inside unsafe).
    Deref,
}

/// Binary operators at the HIR level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HirBinaryOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
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
    /// `&&`.
    And,
    /// `||`.
    Or,
}

/// One arm of a `match` expression.
#[derive(Debug, Clone)]
pub struct HirMatchArm {
    /// Pattern matched by this arm.
    pub pattern: HirPat,
    /// Optional `if`-guard.
    pub guard: Option<HirExpr>,
    /// Right-hand side expression.
    pub body: HirExpr,
}

/// Array expression forms at the HIR level.
#[derive(Debug, Clone)]
pub enum HirArrayExpr {
    /// Explicit element list.
    List(Vec<HirExpr>),
    /// Repeat form `[value; count]`.
    Repeat {
        /// Value to repeat.
        value: Box<HirExpr>,
        /// Count expression.
        count: Box<HirExpr>,
    },
}

/// HIR pattern form. Deliberately simpler than the AST pattern so
/// downstream passes only need a handful of variants.
#[derive(Debug, Clone)]
pub struct HirPat {
    /// Stable id.
    pub id: HirId,
    /// Source range.
    pub span: Span,
    /// Type of the matched value.
    pub ty: Ty,
    /// Pattern variant.
    pub kind: HirPatKind,
}

/// HIR pattern kinds.
#[derive(Debug, Clone)]
pub enum HirPatKind {
    /// `_` matching anything.
    Wildcard,
    /// Identifier binding (`mut? name`).
    Binding {
        /// Binding name.
        name: Ident,
        /// `true` when declared mutable.
        mutable: bool,
    },
    /// Literal pattern.
    Literal(HirLiteral),
    /// Tuple pattern.
    Tuple(Vec<HirPat>),
    /// Slice pattern `[p0, .., pN]`. `prefix` binds the leading
    /// elements, `suffix` the trailing elements (indexed from the end),
    /// and `rest` is `Some` when a `..` was written - bound to a
    /// sub-slice when it is a binding pattern, `None` for a
    /// fixed-length slice pattern.
    Slice {
        /// Element patterns before the `..` rest.
        prefix: Vec<HirPat>,
        /// The `..` rest binding, or `None` when no `..` is present.
        rest: Option<Box<HirPat>>,
        /// Element patterns after the `..` rest.
        suffix: Vec<HirPat>,
    },
    /// Enum variant or tuple-struct pattern.
    Variant {
        /// Variant name (last path segment).
        name: Ident,
        /// Sub-patterns, in declaration order.
        fields: Vec<HirPat>,
    },
    /// Struct pattern `Path { f: p, .. }`.
    Struct {
        /// Path naming the struct.
        name: Ident,
        /// Field patterns.
        fields: Vec<HirFieldPat>,
        /// `true` when `..` was written to ignore remaining fields.
        rest: bool,
    },
    /// `&pat` or `&mut pat`.
    Ref {
        /// Inner pattern.
        inner: Box<HirPat>,
        /// `true` when the reference was declared `&mut`.
        mutable: bool,
    },
    /// Or-pattern `a | b | c`.
    Or(Vec<HirPat>),
    /// `..` rest pattern.
    Rest,
    /// Range pattern `lo..hi` (exclusive) or `lo..=hi` (inclusive).
    /// Carries both literal bounds plus the inclusivity flag so
    /// MIR can lower it into a `(scrut >= lo) && (scrut < hi)` /
    /// `(scrut >= lo) && (scrut <= hi)` predicate.
    Range {
        /// Lower bound literal (always present in source).
        lo: HirLiteral,
        /// Upper bound literal (always present in source).
        hi: HirLiteral,
        /// `true` for `..=` (inclusive of `hi`); `false` for `..`.
        inclusive: bool,
    },
    /// `name @ subpattern` - binds the scrutinee to `name` *and*
    /// requires the scrutinee to match `sub`. Pattern compilers
    /// that ignore the `sub` filter (or the binding) drop one
    /// half of the semantics; the lowering must recurse into
    /// `sub` for the test, then bind `name` to the matched value
    /// in the arm body.
    At {
        /// Binding name introduced by the `@`.
        name: Ident,
        /// `true` when declared `mut`.
        mutable: bool,
        /// Required subpattern.
        sub: Box<HirPat>,
    },
}

/// A single field pattern inside a struct pattern.
#[derive(Debug, Clone)]
pub struct HirFieldPat {
    /// Field name.
    pub name: Ident,
    /// Sub-pattern, or shorthand binding if absent.
    pub pattern: Option<HirPat>,
}

/// Applies `f` to every expression directly under `expr`, in source order.
///
/// One arm per variant, so a new expression kind cannot silently acquire an
/// unvisited child edge. A pass that only needs "is this name mentioned
/// anywhere below" recurses through this rather than repeating the match.
#[allow(clippy::too_many_lines)]
pub fn for_each_child_expr<'a>(expr: &'a HirExpr, f: &mut impl FnMut(&'a HirExpr)) {
    match &expr.kind {
        HirExprKind::Literal(_)
        | HirExprKind::Path { .. }
        | HirExprKind::Continue { .. }
        | HirExprKind::Return(None)
        | HirExprKind::Break { value: None, .. }
        | HirExprKind::Placeholder => {}
        HirExprKind::Call { callee, args } => {
            f(callee);
            for a in args {
                f(a);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            f(receiver);
            for a in args {
                f(a);
            }
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            f(receiver);
        }
        HirExprKind::Index { base, index } => {
            f(base);
            f(index);
        }
        HirExprKind::Unary { operand, .. } => f(operand),
        HirExprKind::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        HirExprKind::Assign { place, value } => {
            f(place);
            f(value);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            f(condition);
            f(then_branch);
            if let Some(e) = else_branch {
                f(e);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    f(g);
                }
                f(&arm.body);
            }
        }
        HirExprKind::Loop { body, .. } => f(body),
        HirExprKind::While {
            condition, body, ..
        } => {
            f(condition);
            f(body);
        }
        HirExprKind::Block(block) => for_each_child_expr_in_block(block, f),
        HirExprKind::Closure { body, .. } => f(body),
        HirExprKind::LiftedClosure { captures, .. } => {
            for c in captures {
                f(c);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &arm.op {
                    HirSelectOp::Recv { channel, .. } => f(channel),
                    HirSelectOp::Send { channel, value } => {
                        f(channel);
                        f(value);
                    }
                    HirSelectOp::Default => {}
                }
                f(&arm.body);
            }
        }
        HirExprKind::Return(Some(e)) | HirExprKind::Break { value: Some(e), .. } => f(e),
        HirExprKind::Tuple(items) => {
            for e in items {
                f(e);
            }
        }
        HirExprKind::Array(HirArrayExpr::List(items)) => {
            for e in items {
                f(e);
            }
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            f(value);
            f(count);
        }
        HirExprKind::Cast { value, .. } => f(value),
        HirExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                f(s);
            }
            if let Some(e) = end {
                f(e);
            }
        }
    }
}

/// Applies `f` to every expression directly under `block`, in source order.
pub fn for_each_child_expr_in_block<'a>(block: &'a HirBlock, f: &mut impl FnMut(&'a HirExpr)) {
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let { init, .. } => {
                if let Some(e) = init {
                    f(e);
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => f(expr),
            HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        f(tail);
    }
}
