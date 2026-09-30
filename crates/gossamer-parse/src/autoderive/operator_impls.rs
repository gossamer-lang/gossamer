//! Operator impls with a right-hand type of their own.
//!
//! `impl Mul<f64> for V2` names what `v * 2.0` answers, beside the
//! `impl Mul for V2` that answers `v * w`. One type may implement an operator
//! trait once per right-hand type, so each impl with a right-hand type other
//! than the type itself becomes an inherent method whose name carries that
//! type; the checker picks among them by the right operand's type. `type
//! Output` restates the method's return and is dropped when it agrees.

use gossamer_ast::{
    ImplDecl, ImplItem, Item, ItemKind, ModBody, OPERATOR_METHOD_PREFIX, SourceFile, Type,
};

/// Operator traits taking a right-hand operand, with the method each names.
const BINARY_OPERATORS: &[(&str, &str)] = &[
    ("Add", "add"),
    ("Sub", "sub"),
    ("Mul", "mul"),
    ("Div", "div"),
    ("Rem", "rem"),
    ("BitAnd", "bitand"),
    ("BitOr", "bitor"),
    ("BitXor", "bitxor"),
    ("Shl", "shl"),
    ("Shr", "shr"),
];

/// Operator traits with one operand, which take no right-hand type.
const UNARY_OPERATORS: &[(&str, &str)] = &[("Neg", "neg"), ("Not", "not")];

/// Rewrites every operator impl in `sf` as described in the module docs.
pub(crate) fn rewrite_operator_impls(sf: &mut SourceFile) {
    rewrite_items(&mut sf.items);
}

fn rewrite_items(items: &mut [Item]) {
    for item in items {
        match &mut item.kind {
            ItemKind::Impl(decl) => rewrite_impl(decl),
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &mut decl.body {
                    rewrite_items(inner);
                }
            }
            _ => {}
        }
    }
}

fn rewrite_impl(decl: &mut ImplDecl) {
    let Some(bound) = &decl.trait_ref else {
        return;
    };
    let Some(trait_name) = bound.trait_name() else {
        return;
    };
    let binary = BINARY_OPERATORS
        .iter()
        .find(|(name, _)| *name == trait_name);
    let unary = UNARY_OPERATORS.iter().find(|(name, _)| *name == trait_name);
    let Some(&(_, method)) = binary.or(unary) else {
        return;
    };
    let self_text = render(&decl.self_ty);
    let returns = decl.items.iter().find_map(|item| match item {
        ImplItem::Fn(f) if f.name.name == method => {
            Some(f.ret.as_ref().map_or_else(|| "()".to_string(), render))
        }
        _ => None,
    });
    let agrees = |ty: &Type| {
        let text = render(ty);
        let text = if text == "Self" {
            self_text.clone()
        } else {
            text
        };
        returns
            .as_ref()
            .is_some_and(|ret| *ret == text || (ret == "Self" && text == self_text))
    };
    decl.items.retain(|item| {
        !matches!(item, ImplItem::Type { name, ty, .. } if name.name == "Output" && agrees(ty))
    });
    // An `Output` that disagrees with the method stays on the trait impl,
    // where the checker reports the two types.
    let disagrees = decl
        .items
        .iter()
        .any(|item| matches!(item, ImplItem::Type { name, .. } if name.name == "Output"));
    if binary.is_none() || disagrees {
        return;
    }
    let rhs = bound
        .path
        .segments
        .last()
        .and_then(|segment| match segment.generics.first() {
            Some(gossamer_ast::GenericArg::Type(ty)) => Some(render(ty)),
            _ => None,
        });
    let Some(rhs) = rhs else {
        return;
    };
    if rhs == "Self" || rhs == self_text {
        return;
    }
    let renamed = format!("{OPERATOR_METHOD_PREFIX}{method}_{}", sanitize(&rhs));
    for item in &mut decl.items {
        if let ImplItem::Fn(f) = item
            && f.name.name == method
        {
            f.name.name.clone_from(&renamed);
        }
    }
    decl.trait_ref = None;
}

fn render(ty: &Type) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type(ty);
    printer.finish()
}

/// `text` with every character an identifier cannot hold replaced by `_`.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_keeps_a_type_readable_in_a_method_name() {
        assert_eq!(sanitize("Vec<f64>"), "Vec_f64_");
        assert_eq!(sanitize("(i64, u8)"), "_i64__u8_");
    }
}
