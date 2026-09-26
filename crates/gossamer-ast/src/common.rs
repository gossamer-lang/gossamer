//! Shared enums referenced by expression, pattern, and item nodes.

#![forbid(unsafe_code)]

/// Binding mutability declared at a `let`, pattern, or function parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Mutability {
    /// Immutable binding (`let x`).
    Immutable,
    /// Mutable binding (`let mut x`).
    Mutable,
}

impl Mutability {
    /// Returns `true` when this binding is `mut`.
    #[must_use]
    pub const fn is_mutable(self) -> bool {
        matches!(self, Self::Mutable)
    }
}

/// Item visibility as parsed from the source (`pub` vs absent).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Visibility {
    /// Not annotated - private to the declaring module and its descendants.
    #[default]
    Inherited,
    /// Annotated with `pub(package)` - reachable from every module of the
    /// declaring package, and from nowhere outside it.
    Package,
    /// Annotated with `pub` - part of the package's public API.
    Public,
}

impl Visibility {
    /// Returns `true` when this visibility is `pub`.
    #[must_use]
    pub const fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }

    /// Returns `true` when a reference from another module of the same
    /// package may reach the item. Both `pub` and `pub(package)` qualify.
    #[must_use]
    pub const fn is_package_visible(self) -> bool {
        matches!(self, Self::Public | Self::Package)
    }

    /// The annotation as written, for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherited => "private",
            Self::Package => "pub(package)",
            Self::Public => "pub",
        }
    }
}

/// Every infix operator at precedence levels 5 through 15 of SPEC §4.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BinaryOp {
    /// Multiplication `*` (level 5).
    Mul,
    /// Division `/` (level 5).
    Div,
    /// Remainder `%` (level 5).
    Rem,
    /// Wrapping multiplication `*%`, wrapping at the operands' declared width
    /// (level 5).
    WrappingMul,
    /// Addition `+` (level 6).
    Add,
    /// Subtraction `-` (level 6).
    Sub,
    /// Wrapping addition `+%`, wrapping at the operands' declared width
    /// (level 6).
    WrappingAdd,
    /// Wrapping subtraction `-%`, wrapping at the operands' declared width
    /// (level 6).
    WrappingSub,
    /// Left shift `<<` (level 7).
    Shl,
    /// Right shift `>>` (level 7).
    Shr,
    /// Bitwise AND `&` (level 8).
    BitAnd,
    /// Bitwise XOR `^` (level 9).
    BitXor,
    /// Bitwise OR `|` (level 10).
    BitOr,
    /// Equality `==` (level 11).
    Eq,
    /// Disequality `!=` (level 11).
    Ne,
    /// Less-than `<` (level 11).
    Lt,
    /// Less-than-or-equal `<=` (level 11).
    Le,
    /// Greater-than `>` (level 11).
    Gt,
    /// Greater-than-or-equal `>=` (level 11).
    Ge,
    /// Logical AND `&&` (level 12).
    And,
    /// Logical OR `||` (level 13).
    Or,
    /// Forward pipe `|>` (level 15).
    PipeGt,
}

impl BinaryOp {
    /// Returns the canonical source spelling of this operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
            Self::WrappingMul => "*%",
            Self::Add => "+",
            Self::Sub => "-",
            Self::WrappingAdd => "+%",
            Self::WrappingSub => "-%",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::BitAnd => "&",
            Self::BitXor => "^",
            Self::BitOr => "|",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "&&",
            Self::Or => "||",
            Self::PipeGt => "|>",
        }
    }

    /// Returns the precedence level from SPEC §4.7 (lower number binds tighter).
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Mul | Self::Div | Self::Rem | Self::WrappingMul => 5,
            Self::Add | Self::Sub | Self::WrappingAdd | Self::WrappingSub => 6,
            Self::Shl | Self::Shr => 7,
            Self::BitAnd => 8,
            Self::BitXor => 9,
            Self::BitOr => 10,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge => 11,
            Self::And => 12,
            Self::Or => 13,
            Self::PipeGt => 15,
        }
    }
}

/// Prefix unary operator applied to an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum UnaryOp {
    /// Arithmetic negation `-expr`.
    Neg,
    /// Logical negation `!expr`.
    Not,
    /// Shared reference `&expr`.
    RefShared,
    /// Mutable reference `&mut expr`.
    RefMut,
    /// Raw-pointer dereference `*expr` (only valid inside `unsafe`).
    Deref,
}

impl UnaryOp {
    /// Returns the canonical source spelling of this prefix operator.
    ///
    /// For `RefMut` the returned spelling includes the trailing space that
    /// separates `&mut` from its operand.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neg => "-",
            Self::Not => "!",
            Self::RefShared => "&",
            Self::RefMut => "&mut ",
            Self::Deref => "*",
        }
    }
}

/// Assignment operator appearing in an assignment statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AssignOp {
    /// Plain assignment `=`.
    Assign,
    /// Compound `+=`.
    AddAssign,
    /// Compound `-=`.
    SubAssign,
    /// Compound `*=`.
    MulAssign,
    /// Compound `/=`.
    DivAssign,
    /// Compound `%=`.
    RemAssign,
    /// Compound `&=`.
    BitAndAssign,
    /// Compound `|=`.
    BitOrAssign,
    /// Compound `^=`.
    BitXorAssign,
    /// Compound `<<=`.
    ShlAssign,
    /// Compound `>>=`.
    ShrAssign,
    /// Compound wrapping addition `+%=`.
    WrappingAddAssign,
    /// Compound wrapping subtraction `-%=`.
    WrappingSubAssign,
    /// Compound wrapping multiplication `*%=`.
    WrappingMulAssign,
}

impl AssignOp {
    /// Returns the canonical source spelling of this assignment operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assign => "=",
            Self::AddAssign => "+=",
            Self::SubAssign => "-=",
            Self::MulAssign => "*=",
            Self::DivAssign => "/=",
            Self::RemAssign => "%=",
            Self::BitAndAssign => "&=",
            Self::BitOrAssign => "|=",
            Self::BitXorAssign => "^=",
            Self::ShlAssign => "<<=",
            Self::ShrAssign => ">>=",
            Self::WrappingAddAssign => "+%=",
            Self::WrappingSubAssign => "-%=",
            Self::WrappingMulAssign => "*%=",
        }
    }
}

/// Whether a range pattern or expression is exclusive (`..`) or inclusive (`..=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RangeKind {
    /// Half-open range `lo..hi`.
    Exclusive,
    /// Closed range `lo..=hi`.
    Inclusive,
}

impl RangeKind {
    /// Returns the canonical spelling of this range operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exclusive => "..",
            Self::Inclusive => "..=",
        }
    }
}

/// An identifier captured from the lexer, stored as an owned string.
///
/// The AST does not re-use slices into the source buffer because parsed trees
/// outlive the text they came from in many workflows (pretty-printing, macro
/// expansion, incremental reparses).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Ident {
    /// The identifier's textual spelling.
    pub name: String,
}

/// Spelling the parser substitutes for a name it could not read.
///
/// It is not a legal identifier, so it can never collide with user source.
/// Later passes recognise it through [`Ident::is_error`] and stay silent,
/// leaving the parse diagnostic that produced it as the only report.
pub const ERROR_IDENT: &str = "<error>";

impl Ident {
    /// Constructs an identifier from any `Into<String>`.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Placeholder standing in for a name the parser could not read.
    #[must_use]
    pub fn error() -> Self {
        Self::new(ERROR_IDENT)
    }

    /// Whether this name is a parse-error placeholder rather than a name
    /// the user wrote. An empty spelling counts: it is unreachable from
    /// valid source and only ever arises from failed recovery.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.name.is_empty() || self.name == ERROR_IDENT
    }
}

/// Prefix of the callee a call to a reflecting generic function takes when it
/// names no type: the function has no specialisation to reach, and the
/// resolver reports the missing turbofish at that name.
pub const REFLECTING_CALL_WITHOUT_TYPE_PREFIX: &str = "__gos_reflect_untyped_";

/// Whether a raw name spelling is a parse-error placeholder.
#[must_use]
pub fn is_error_name(name: &str) -> bool {
    name.is_empty() || name == ERROR_IDENT
}

#[cfg(test)]
mod tests {
    use super::{AssignOp, BinaryOp, Ident, Mutability, RangeKind, UnaryOp, Visibility};

    #[test]
    fn binary_op_precedence_matches_spec_table() {
        assert_eq!(BinaryOp::Mul.precedence(), 5);
        assert_eq!(BinaryOp::Add.precedence(), 6);
        assert_eq!(BinaryOp::BitAnd.precedence(), 8);
        assert_eq!(BinaryOp::Eq.precedence(), 11);
        assert_eq!(BinaryOp::And.precedence(), 12);
        assert_eq!(BinaryOp::Or.precedence(), 13);
        assert_eq!(BinaryOp::PipeGt.precedence(), 15);
    }

    #[test]
    fn unary_op_ref_mut_includes_trailing_space() {
        assert_eq!(UnaryOp::RefMut.as_str(), "&mut ");
        assert_eq!(UnaryOp::RefShared.as_str(), "&");
    }

    #[test]
    fn assign_op_spellings_cover_all_compounds() {
        assert_eq!(AssignOp::Assign.as_str(), "=");
        assert_eq!(AssignOp::ShlAssign.as_str(), "<<=");
    }

    #[test]
    fn visibility_and_mutability_query_helpers() {
        assert!(Visibility::Public.is_public());
        assert!(!Visibility::Inherited.is_public());
        assert!(Mutability::Mutable.is_mutable());
        assert!(!Mutability::Immutable.is_mutable());
    }

    #[test]
    fn range_kind_spelling() {
        assert_eq!(RangeKind::Exclusive.as_str(), "..");
        assert_eq!(RangeKind::Inclusive.as_str(), "..=");
    }

    #[test]
    fn ident_constructs_from_string_or_str() {
        assert_eq!(Ident::new("foo").name, "foo");
        assert_eq!(Ident::new(String::from("bar")).name, "bar");
    }
}
