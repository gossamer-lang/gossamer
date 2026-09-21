//! Rewrites a std free function named in value position into the
//! closure that calls it.
//!
//! `xs.map(math::abs)` and `xs.map(|x| math::abs(x))` mean the same
//! thing, but only the second has a shape every tier lowers: the VM
//! models a std function as a callable builtin value, while the
//! compiled tiers need a concrete symbol to take the address of. This
//! pass writes the second form for the author, after resolution and
//! before type checking, so the checker, HIR, and every tier's codegen
//! only ever see a closure - the same normalisation
//! [`gossamer_resolve::resolve_named_arguments`] performs for labelled
//! and defaulted arguments.
//!
//! The arity comes from the stdlib signature catalogue, so a function
//! the catalogue does not describe is left alone. A path the resolver
//! bound to an item or a local of this compilation unit is left alone
//! too: a user module is free to declare a `sort::by_key` of its own,
//! and its arity is the declaration's, not the catalogue's.

#![forbid(unsafe_code)]

use gossamer_ast::visitor::{VisitorMut, walk_expr_mut};
use std::collections::HashMap;

use gossamer_ast::{
    BinaryOp, ClosureParam, Expr, ExprKind, Ident, Item, ItemKind, ModBody, Mutability,
    NodeIdGenerator, PathExpr, Pattern, PatternKind, SourceFile, StructBody,
};
use gossamer_resolve::{Resolution, Resolutions};

/// Prefix of the parameter names this pass introduces. Not writable as
/// a source identifier the author could collide with.
const ETA_PARAM_PREFIX: &str = "__gos_eta";

/// Expands every std free function used as a value into the closure
/// that calls it. Returns the number of paths rewritten.
pub fn expand_std_fn_values(sf: &mut SourceFile, resolutions: &Resolutions) -> usize {
    let mut ids = NodeIdGenerator::new();
    while ids.issued() < sf.next_node_id {
        let _ = ids.next();
    }
    let mut tuple_variants = HashMap::new();
    collect_tuple_variants(&sf.items, &mut tuple_variants);
    let mut pass = Expand {
        ids: &mut ids,
        resolutions,
        tuple_variants,
        rewritten: 0,
    };
    pass.visit_source_file(sf);
    let rewritten = pass.rewritten;
    sf.next_node_id = ids.issued();
    rewritten
}

struct Expand<'a> {
    ids: &'a mut NodeIdGenerator,
    resolutions: &'a Resolutions,
    /// Payload arity of every tuple variant this unit declares, keyed by
    /// `(enum, variant)`.
    tuple_variants: HashMap<(String, String), usize>,
    rewritten: usize,
}

/// Records the payload arity of each tuple variant `items` declare, at any
/// module depth.
fn collect_tuple_variants(items: &[Item], out: &mut HashMap<(String, String), usize>) {
    for item in items {
        match &item.kind {
            ItemKind::Enum(decl) => {
                for variant in &decl.variants {
                    if let StructBody::Tuple(fields) = &variant.body
                        && !fields.is_empty()
                    {
                        out.insert(
                            (decl.name.name.clone(), variant.name.name.clone()),
                            fields.len(),
                        );
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    collect_tuple_variants(inner, out);
                }
            }
            _ => {}
        }
    }
}

impl Expand<'_> {
    /// Arity of the std function `path` names, or `None` when the path
    /// is not a catalogued std free function.
    fn std_fn_arity(path: &PathExpr) -> Option<usize> {
        let segments: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        // The prelude's one-payload constructors are functions of their
        // payload wherever a callable is taken: `r.and_then(Ok)`.
        if matches!(
            segments.as_slice(),
            ["Some" | "Ok" | "Err"] | ["Option", "Some"] | ["Result", "Ok" | "Err"]
        ) {
            return Some(1);
        }
        let stripped: &[&str] = match segments.as_slice() {
            ["std", rest @ ..] => rest,
            other => other,
        };
        // A module-qualified, all-lowercase path is the shape a std free
        // function has; a leading capital is an associated function on a
        // type, whose receiver the catalogue does not describe.
        if stripped.len() < 2
            || !stripped
                .iter()
                .all(|seg| seg.chars().next().is_some_and(char::is_lowercase))
        {
            return None;
        }
        let (name, modules) = stripped.split_last()?;
        let shape = crate::stdlib_signatures::function_shape_for_path(modules, name)?;
        Some(shape.params.len())
    }

    /// Payload arity of the tuple variant `path` names as `Enum::Variant`,
    /// which is a function of its payload wherever a callable is taken.
    fn tuple_variant_arity(&self, path: &PathExpr) -> Option<usize> {
        let [.., enum_name, variant] = path.segments.as_slice() else {
            return None;
        };
        self.tuple_variants
            .get(&(enum_name.name.name.clone(), variant.name.name.clone()))
            .copied()
    }

    /// Whether `expr` names something this compilation unit declares,
    /// which shadows any same-spelled entry in the std catalogue.
    fn resolves_locally(&self, expr: &Expr) -> bool {
        matches!(
            self.resolutions.get(expr.id),
            Some(Resolution::Local(_) | Resolution::Def { .. })
        )
    }

    /// Builds `|p0, .., pn| path(p0, .., pn)` around `path`.
    fn eta_expand(&mut self, path: &PathExpr, expr: &Expr, arity: usize) -> ExprKind {
        let names: Vec<String> = (0..arity)
            .map(|i| format!("{ETA_PARAM_PREFIX}{i}"))
            .collect();
        let params = names
            .iter()
            .map(|name| ClosureParam {
                pattern: Pattern::new(
                    self.ids.next(),
                    expr.span,
                    PatternKind::Ident {
                        mutability: Mutability::Immutable,
                        name: Ident { name: name.clone() },
                        subpattern: None,
                    },
                ),
                ty: None,
            })
            .collect();
        let args = names
            .iter()
            .map(|name| {
                Expr::new(
                    self.ids.next(),
                    expr.span,
                    ExprKind::Path(PathExpr::single(name.clone())),
                )
            })
            .collect();
        let callee = Expr::new(self.ids.next(), expr.span, ExprKind::Path(path.clone()));
        let body = Expr::new(
            self.ids.next(),
            expr.span,
            ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        );
        ExprKind::Closure {
            params,
            ret: None,
            body: Box::new(body),
        }
    }
}

impl VisitorMut for Expand<'_> {
    fn visit_expr(&mut self, expr: &mut Expr) {
        // A path in callee position is an ordinary call, not a value.
        if let ExprKind::Call { callee, args } = &mut expr.kind {
            if matches!(callee.kind, ExprKind::Path(_)) {
                for arg in args.iter_mut() {
                    self.visit_expr(arg);
                }
                return;
            }
        }
        // `x |> path` calls `path`, so its right side is a callee too.
        if let ExprKind::Binary { op, lhs, rhs } = &mut expr.kind {
            if *op == BinaryOp::PipeGt && matches!(rhs.kind, ExprKind::Path(_)) {
                self.visit_expr(lhs);
                return;
            }
        }
        walk_expr_mut(self, expr);
        let variant_arity = match &expr.kind {
            ExprKind::Path(path) => self.tuple_variant_arity(path),
            _ => None,
        };
        if variant_arity.is_none() && self.resolves_locally(expr) {
            return;
        }
        let Some(arity) = variant_arity.or_else(|| match &expr.kind {
            ExprKind::Path(path) => Self::std_fn_arity(path),
            _ => None,
        }) else {
            return;
        };
        let ExprKind::Path(path) = expr.kind.clone() else {
            return;
        };
        expr.kind = self.eta_expand(&path, expr, arity);
        self.rewritten += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gossamer_lex::SourceMap;

    fn expand(source: &str) -> (SourceFile, usize) {
        let mut map = SourceMap::new();
        let file = map.add_file("eta.gos".to_string(), source.to_string());
        let (mut sf, diags) = gossamer_parse::parse_source_file(source, file);
        assert!(diags.is_empty(), "parse errors: {diags:?}");
        let (resolutions, _) = gossamer_resolve::resolve_source_file(&sf);
        let count = expand_std_fn_values(&mut sf, &resolutions);
        (sf, count)
    }

    #[test]
    fn a_std_fn_in_value_position_becomes_a_closure() {
        let (_sf, count) = expand("fn main() { let v = #[1.0].map(math::abs) }");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_prelude_constructor_in_value_position_becomes_a_closure() {
        let (_sf, count) = expand("fn main() { let v = Some(1).map(Some) }");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_tuple_variant_in_value_position_becomes_a_closure() {
        let (_sf, count) = expand(
            "enum Shape { Circle(i64) }\nfn main() { let v = #[1].map(Shape::Circle)\n let c = Shape::Circle(2) }",
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn a_std_fn_being_called_is_left_alone() {
        let (_sf, count) = expand("fn main() { let v = math::abs(-1.0) }");
        assert_eq!(count, 0);
    }

    #[test]
    fn a_path_outside_the_stdlib_is_left_alone() {
        let (_sf, count) = expand("fn main() { let v = #[1].map(mymod::helper) }");
        assert_eq!(count, 0);
    }

    #[test]
    fn a_std_module_outside_the_first_class_value_table_expands_too() {
        let (_sf, count) = expand("fn main() { let v = #[\"a\"].map(base64::encode) }");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_locally_declared_module_shadows_the_catalogue() {
        let (_sf, count) = expand(
            "mod sort { pub fn by_key(a: i64, b: i64, c: i64) -> i64 { a } }\nfn main() { let f = sort::by_key }",
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn a_std_fn_with_no_catalogued_signature_is_left_alone() {
        let (_sf, count) = expand("fn main() { let v = #[1].map(math::not_a_real_function) }");
        assert_eq!(count, 0);
    }

    #[test]
    fn the_expanded_closure_takes_one_parameter_per_declared_one() {
        let (sf, count) = expand("fn main() { let f = strings::repeat }");
        assert_eq!(count, 1);
        let rendered = format!("{sf:?}");
        assert!(
            rendered.contains("__gos_eta0") && rendered.contains("__gos_eta1"),
            "two-parameter std fn should expand to a two-parameter closure: {rendered}"
        );
    }
}
