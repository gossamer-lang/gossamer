//! The `math::big` functions the operators on big integers lower to.

use gossamer_ast::BinaryOp;

/// Whether `op` compares its operands, answering a `bool`.
#[must_use]
pub fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

/// The `math::big` function a binary operator on two `Int`s lowers to. A
/// comparison other than equality compares `int_cmp`'s sign against zero.
#[must_use]
pub fn big_int_operator(op: BinaryOp) -> Option<&'static str> {
    Some(match op {
        BinaryOp::Add => "int_add",
        BinaryOp::Sub => "int_sub",
        BinaryOp::Mul => "int_mul",
        BinaryOp::Div => "int_div",
        BinaryOp::Rem => "int_rem",
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            "int_cmp"
        }
        _ => return None,
    })
}

/// The `math::big` function a binary operator on two `Uint`s lowers to.
/// Equality compares the canonical decimal text, which is one spelling per
/// value.
#[must_use]
pub fn big_uint_operator(op: BinaryOp) -> Option<&'static str> {
    Some(match op {
        BinaryOp::Add => "uint_add",
        BinaryOp::Mul => "uint_mul",
        BinaryOp::Eq | BinaryOp::Ne => "",
        _ => return None,
    })
}

/// The binary operator a compound assignment applies, or `None` for plain
/// `=`.
#[must_use]
pub fn compound_binary(op: gossamer_ast::AssignOp) -> Option<BinaryOp> {
    use gossamer_ast::AssignOp;
    Some(match op {
        AssignOp::Assign => return None,
        AssignOp::AddAssign => BinaryOp::Add,
        AssignOp::SubAssign => BinaryOp::Sub,
        AssignOp::MulAssign => BinaryOp::Mul,
        AssignOp::DivAssign => BinaryOp::Div,
        AssignOp::RemAssign => BinaryOp::Rem,
        AssignOp::BitAndAssign => BinaryOp::BitAnd,
        AssignOp::BitOrAssign => BinaryOp::BitOr,
        AssignOp::BitXorAssign => BinaryOp::BitXor,
        AssignOp::ShlAssign => BinaryOp::Shl,
        AssignOp::ShrAssign => BinaryOp::Shr,
        AssignOp::WrappingAddAssign => BinaryOp::WrappingAdd,
        AssignOp::WrappingSubAssign => BinaryOp::WrappingSub,
        AssignOp::WrappingMulAssign => BinaryOp::WrappingMul,
        AssignOp::WrappingShlAssign => BinaryOp::WrappingShl,
        AssignOp::WrappingShrAssign => BinaryOp::WrappingShr,
    })
}
