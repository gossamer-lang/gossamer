//! A statement whose only work is a method that answers a new value.

use gossamer_ast::{Expr, ExprKind};

use super::{BTREE_SET_DEF_LOCAL, HASH_SET_DEF_LOCAL, TypeChecker};
use crate::{Ty, TyKind, TypeError};

/// `String` methods that answer a value and leave the receiver as it was.
const STRING_VALUE_METHODS: &[&str] = &[
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
    "to_string",
    "clone",
];

/// Sequence methods (`Vec`, arrays, slices) that answer a value and leave
/// the receiver as it was. `sort`, `reverse`, `push`, `for_each`, and the
/// other writers and effects are absent.
const SEQUENCE_VALUE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "slice",
    "first",
    "last",
    "get",
    "contains",
    "index_of",
    "count_of",
    "windows",
    "chunks",
    "join",
    "to_vec",
    "iter",
    "capacity",
    "binary_search",
    "dedup",
    "clone",
    "to_string",
];

/// `Map` / `BTreeMap` methods that answer a value and leave the map as it was.
const MAP_VALUE_METHODS: &[&str] = &[
    "get",
    "get_or",
    "contains_key",
    "contains",
    "len",
    "is_empty",
    "keys",
    "values",
    "iter",
    "clone",
    "to_string",
    "first_key_value",
    "last_key_value",
    "range",
];

/// `Set` / `BTreeSet` methods that answer a value and leave the set as it was.
const SET_VALUE_METHODS: &[&str] = &[
    "contains",
    "len",
    "is_empty",
    "iter",
    "to_vec",
    "union",
    "intersection",
    "difference",
    "symmetric_difference",
    "is_subset",
    "is_superset",
    "is_disjoint",
    "first",
    "last",
    "range",
    "clone",
    "to_string",
];

/// Traversals a collection answers with a new value. Each one that takes a
/// callback is a value only when the callback computes without effect.
const TRAVERSAL_VALUE_METHODS: &[&str] = &[
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
    "par_map",
    "par_filter",
    "par_sum",
    "par_min",
    "par_max",
    "par_reduce",
];

/// Integer and float methods. A number is a value, so none of its methods
/// changes it; a program cannot add a method of one of these names to a
/// number, since the number's own surface answers first.
const NUMBER_VALUE_METHODS: &[&str] = &[
    "abs",
    "abs_diff",
    "pow",
    "powi",
    "powf",
    "min",
    "max",
    "clamp",
    "signum",
    "isqrt",
    "sqrt",
    "cbrt",
    "count_ones",
    "count_zeros",
    "leading_zeros",
    "trailing_zeros",
    "rem_euclid",
    "div_euclid",
    "checked_add",
    "checked_sub",
    "checked_mul",
    "checked_div",
    "checked_rem",
    "checked_pow",
    "saturating_add",
    "saturating_sub",
    "saturating_mul",
    "wrapping_add",
    "wrapping_sub",
    "wrapping_mul",
    "overflowing_add",
    "overflowing_sub",
    "overflowing_mul",
    "floor",
    "ceil",
    "round",
    "trunc",
    "fract",
    "exp",
    "ln",
    "log",
    "log2",
    "log10",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "sinh",
    "cosh",
    "tanh",
    "hypot",
    "mul_add",
    "total_cmp",
    "is_nan",
    "is_infinite",
    "is_finite",
    "is_positive",
    "is_negative",
    "to_degrees",
    "to_radians",
    "to_string",
    "clone",
];

/// `char` methods.
const CHAR_VALUE_METHODS: &[&str] = &[
    "is_alphabetic",
    "is_alphanumeric",
    "is_numeric",
    "is_whitespace",
    "is_uppercase",
    "is_lowercase",
    "is_ascii",
    "is_ascii_digit",
    "is_ascii_alphabetic",
    "is_ascii_alphanumeric",
    "is_ascii_uppercase",
    "is_ascii_lowercase",
    "is_ascii_whitespace",
    "is_ascii_punctuation",
    "is_ascii_hexdigit",
    "to_digit",
    "to_uppercase",
    "to_lowercase",
    "to_ascii_uppercase",
    "to_ascii_lowercase",
    "to_string",
    "clone",
];

/// `Option` / `Result` methods with no callback.
const WRAPPER_VALUE_METHODS: &[&str] = &[
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "ok",
    "err",
    "unwrap_or",
    "or",
    "ok_or",
    "flatten",
    "clone",
];

impl TypeChecker<'_> {
    /// GT0140 when the statement `expr` calls a built-in method that answers
    /// a new value and writes nothing, on a receiver whose evaluation has no
    /// effect either, so the statement does nothing.
    pub(super) fn report_discarded_pure_value(&mut self, expr: &Expr) {
        let ExprKind::MethodCall { receiver, name, .. } = &expr.kind else {
            return;
        };
        if !self.is_value_only_call(expr) {
            return;
        }
        let receiver_name = match &receiver.kind {
            ExprKind::Path(path) if path.segments.len() == 1 => path.segments[0].name.name.clone(),
            _ => String::new(),
        };
        self.emit(
            TypeError::DiscardedPureValue {
                method: name.name.clone(),
                receiver: receiver_name,
            },
            expr.span,
        );
    }

    /// Whether `expr` is a call of a built-in value-only method whose
    /// receiver and arguments evaluate without effect, so dropping what it
    /// answers leaves the program as if the statement were absent.
    fn is_value_only_call(&mut self, expr: &Expr) -> bool {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return false;
        };
        let method = name.name.as_str();
        let Some(receiver_ty) = self.table.get(receiver.id) else {
            return false;
        };
        let receiver_ty = self.peel_refs(receiver_ty);
        let Some(names) = self.value_methods_of(receiver_ty) else {
            return false;
        };
        let traversal = TRAVERSAL_VALUE_METHODS.contains(&method)
            && !matches!(
                self.tcx.kind(receiver_ty),
                Some(TyKind::String | TyKind::Int(_) | TyKind::Float(_) | TyKind::Char)
            );
        (names.contains(&method) || traversal)
            && (computes_without_effect(receiver) || self.is_value_only_call(receiver))
            && args.iter().all(|arg| self.arg_computes_without_effect(arg))
    }

    /// The value-only methods of a built-in receiver type, or `None` for a
    /// type whose methods a program writes.
    fn value_methods_of(&self, ty: Ty) -> Option<&'static [&'static str]> {
        Some(match self.tcx.kind(ty)? {
            TyKind::String => STRING_VALUE_METHODS,
            TyKind::Vec(_) | TyKind::Array { .. } | TyKind::Slice(_) => SEQUENCE_VALUE_METHODS,
            TyKind::HashMap { .. } => MAP_VALUE_METHODS,
            TyKind::Int(_) | TyKind::Float(_) => NUMBER_VALUE_METHODS,
            TyKind::Char => CHAR_VALUE_METHODS,
            TyKind::Adt { def, .. }
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
            {
                SET_VALUE_METHODS
            }
            TyKind::Adt { def, .. }
                if matches!(self.tcx.def_name(*def), Some("Option" | "Result")) =>
            {
                WRAPPER_VALUE_METHODS
            }
            _ => return None,
        })
    }

    /// Whether evaluating `arg` as a method argument has no effect beyond its
    /// value: anything but a callable, or a closure literal whose body only
    /// computes.
    fn arg_computes_without_effect(&mut self, arg: &Expr) -> bool {
        if let ExprKind::Closure { body, .. } = &arg.kind {
            return computes_without_effect(body);
        }
        let Some(ty) = self.table.get(arg.id) else {
            return false;
        };
        let ty = self.peel_refs(ty);
        !matches!(
            self.tcx.kind(ty),
            Some(
                TyKind::FnDef { .. }
                    | TyKind::FnPtr(_)
                    | TyKind::FnTrait(_)
                    | TyKind::Closure { .. }
            ) | None
        )
    }
}

/// Whether `expr` only computes: no call, assignment, or control flow.
fn computes_without_effect(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Path(_) | ExprKind::Literal(_) => true,
        ExprKind::Unary { operand, .. } => computes_without_effect(operand),
        ExprKind::Binary { lhs, rhs, .. } => {
            computes_without_effect(lhs) && computes_without_effect(rhs)
        }
        ExprKind::FieldAccess { receiver, .. } => computes_without_effect(receiver),
        ExprKind::Index { base, index } => {
            computes_without_effect(base) && computes_without_effect(index)
        }
        ExprKind::Cast { value, .. } => computes_without_effect(value),
        ExprKind::Tuple(items) => items.iter().all(computes_without_effect),
        ExprKind::Block(block) => {
            block.stmts.is_empty() && block.tail.as_deref().is_none_or(computes_without_effect)
        }
        _ => false,
    }
}
