//! Syntactic index of associated types and constants.
//! Traits declare associated items; `impl` blocks supply them. Both the
//! parser's associated-constant hoist and the type-checker's projection
//! resolution need the same view of who declares and who supplies what,
//! so the index lives here, beside the AST it reads.

use std::collections::HashMap;

use crate::expr::Expr;
use crate::items::{ImplItem, ItemKind, ModBody, TraitItem};
use crate::source_file::SourceFile;
use crate::ty::Type;
use crate::{Item, TypeKind};

/// Outcome of resolving an associated item that is reached through a
/// trait rather than through a concrete self type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssocResolution<T> {
    /// Exactly one candidate applies.
    Found(T),
    /// The trait declares the item, but no single impl supplies it and
    /// the trait carries no default.
    Ambiguous,
    /// No trait in scope declares an item under this name.
    Unknown,
}

impl<T> AssocResolution<T> {
    /// The resolved candidate, or `None` for an ambiguous or unknown item.
    pub fn found(self) -> Option<T> {
        match self {
            Self::Found(value) => Some(value),
            Self::Ambiguous | Self::Unknown => None,
        }
    }
}

/// An associated item a trait requires that an impl does not supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingAssocItem {
    /// `"type"` or `"const"`, for the diagnostic's wording.
    pub kind: &'static str,
    /// Name of the missing item.
    pub name: String,
}

/// Associated items a single trait declares.
#[derive(Debug, Default, Clone)]
struct TraitAssoc {
    /// Supertrait names from the `trait Ext: Base` clause. A projection
    /// resolves at check time, so an item a supertrait declares is
    /// reachable through the subtrait.
    supertraits: Vec<String>,
    /// Associated type names mapped to the trait's default, if written.
    types: HashMap<String, Option<Type>>,
    /// Associated constant names mapped to their declared type and the
    /// trait's default value, if written.
    consts: HashMap<String, (Type, Option<Expr>)>,
}

/// Associated items a single self type supplies across all its impls.
#[derive(Debug, Default, Clone)]
struct SelfAssoc {
    types: HashMap<String, Type>,
    /// For each supplied associated type, the parameter each of the impl's
    /// self-type arguments names (`impl<T> Iterator for Stack<T>` gives
    /// `[Some("T")]`), so a supplied type written against those parameters
    /// reads against a concrete instantiation's arguments.
    type_self_params: HashMap<String, Vec<Option<String>>>,
    consts: HashMap<String, Type>,
    traits: Vec<String>,
}

/// Program-wide view of associated types and constants.
#[derive(Debug, Default, Clone)]
pub struct AssocIndex {
    traits: HashMap<String, TraitAssoc>,
    selves: HashMap<String, SelfAssoc>,
    /// Self type names implementing each trait, in source order.
    implementors: HashMap<String, Vec<String>>,
}

impl AssocIndex {
    /// Walks every trait and impl in `source`, including inline modules,
    /// and records the associated items each declares or supplies.
    #[must_use]
    pub fn build(source: &SourceFile) -> Self {
        let mut index = Self::default();
        index.collect(&source.items);
        index
    }

    /// Adds the associated items declared by `items` to an existing index.
    /// Block-local traits and impls reach the index through this entry.
    pub fn extend(&mut self, items: &[Item]) {
        self.collect(items);
    }

    fn collect(&mut self, items: &[Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Trait(decl) => {
                    let entry = self.traits.entry(decl.name.name.clone()).or_default();
                    for supertrait in &decl.supertraits {
                        if let Some(name) = supertrait.trait_name()
                            && !entry.supertraits.iter().any(|s| s == name)
                        {
                            entry.supertraits.push(name.to_string());
                        }
                    }
                    for trait_item in &decl.items {
                        match trait_item {
                            TraitItem::Type { name, default, .. } => {
                                entry.types.insert(name.name.clone(), default.clone());
                            }
                            TraitItem::Const {
                                name, ty, default, ..
                            } => {
                                entry
                                    .consts
                                    .insert(name.name.clone(), (ty.clone(), default.clone()));
                            }
                            TraitItem::Fn(_) => {}
                        }
                    }
                }
                ItemKind::Impl(decl) => {
                    let Some(self_name) = type_head_name(&decl.self_ty) else {
                        continue;
                    };
                    let trait_name = decl.trait_ref.as_ref().and_then(|b| b.trait_name());
                    let entry = self.selves.entry(self_name.to_string()).or_default();
                    if let Some(trait_name) = trait_name
                        && !entry.traits.iter().any(|t| t == trait_name)
                    {
                        entry.traits.push(trait_name.to_string());
                    }
                    for impl_item in &decl.items {
                        match impl_item {
                            ImplItem::Type { name, ty, .. } => {
                                entry.types.insert(name.name.clone(), ty.clone());
                                entry
                                    .type_self_params
                                    .insert(name.name.clone(), self_type_params(&decl.self_ty));
                            }
                            ImplItem::Const { name, ty, .. } => {
                                entry.consts.insert(name.name.clone(), ty.clone());
                            }
                            ImplItem::Fn(_) => {}
                        }
                    }
                    if let Some(trait_name) = trait_name {
                        let implementors =
                            self.implementors.entry(trait_name.to_string()).or_default();
                        if !implementors.iter().any(|t| t == self_name) {
                            implementors.push(self_name.to_string());
                        }
                    }
                }
                ItemKind::Mod(decl) => {
                    if let ModBody::Inline(inner) = &decl.body {
                        self.collect(inner);
                    }
                }
                _ => {}
            }
        }
    }

    /// `true` when nothing in the program declares or supplies an
    /// associated constant, letting callers skip the rewrite entirely.
    #[must_use]
    pub fn is_assoc_const_free(&self) -> bool {
        self.traits.values().all(|t| t.consts.is_empty())
            && self.selves.values().all(|s| s.consts.is_empty())
    }

    /// `traits` followed by every supertrait reachable from them, in
    /// breadth-first order.
    #[must_use]
    pub fn with_supertraits(&self, traits: Vec<String>) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = traits.iter().cloned().collect();
        let mut out = traits;
        let mut next = 0;
        while next < out.len() {
            let current = out[next].clone();
            next += 1;
            let Some(entry) = self.traits.get(&current) else {
                continue;
            };
            for supertrait in &entry.supertraits {
                if seen.insert(supertrait.clone()) {
                    out.push(supertrait.clone());
                }
            }
        }
        out
    }

    /// `true` when `trait_name` declares an associated type `name`.
    #[must_use]
    pub fn trait_declares_type(&self, trait_name: &str, name: &str) -> bool {
        self.traits
            .get(trait_name)
            .is_some_and(|t| t.types.contains_key(name))
    }

    /// `true` when `trait_name` declares an associated constant `name`.
    #[must_use]
    pub fn trait_declares_const(&self, trait_name: &str, name: &str) -> bool {
        self.traits
            .get(trait_name)
            .is_some_and(|t| t.consts.contains_key(name))
    }

    /// `true` when a trait `self_ty` implements declares an associated item
    /// `name`, whether or not the impl supplies it.
    #[must_use]
    pub fn self_ty_trait_declares(&self, self_ty: &str, name: &str) -> bool {
        self.selves.get(self_ty).is_some_and(|entry| {
            entry
                .traits
                .iter()
                .any(|t| self.trait_declares_type(t, name) || self.trait_declares_const(t, name))
        })
    }

    /// Every associated type and constant name `trait_name` declares, sorted
    /// for stable diagnostic text.
    #[must_use]
    pub fn declared_assoc_names(&self, trait_name: &str) -> Vec<&str> {
        let Some(entry) = self.traits.get(trait_name) else {
            return Vec::new();
        };
        let mut names: Vec<&str> = entry
            .types
            .keys()
            .chain(entry.consts.keys())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names
    }

    /// Concrete type a named self type supplies for associated type `name`,
    /// falling back to the default of a trait it implements.
    #[must_use]
    pub fn assoc_type_for_self(&self, self_ty: &str, name: &str) -> Option<&Type> {
        let entry = self.selves.get(self_ty)?;
        if let Some(ty) = entry.types.get(name) {
            return Some(ty);
        }
        entry
            .traits
            .iter()
            .find_map(|t| self.traits.get(t)?.types.get(name)?.as_ref())
    }

    /// The parameter names the self-type arguments of the impl supplying
    /// `name` for `self_ty` are written as, in argument order. `None` when
    /// the impl supplies no such type, or names no arguments.
    #[must_use]
    pub fn assoc_type_self_params(&self, self_ty: &str, name: &str) -> Option<&[Option<String>]> {
        self.selves
            .get(self_ty)?
            .type_self_params
            .get(name)
            .map(Vec::as_slice)
    }

    /// Concrete type reached through `trait_name` alone: the trait's default
    /// if it has one, otherwise the single implementor that supplies it.
    #[must_use]
    pub fn assoc_type_for_trait(&self, trait_name: &str, name: &str) -> AssocResolution<&Type> {
        let Some(decl) = self.traits.get(trait_name) else {
            return AssocResolution::Unknown;
        };
        let Some(default) = decl.types.get(name) else {
            return AssocResolution::Unknown;
        };
        if let Some(ty) = default {
            return AssocResolution::Found(ty);
        }
        match self.sole_supplier(trait_name, |entry| entry.types.contains_key(name)) {
            Some(self_ty) => match self.selves.get(self_ty).and_then(|e| e.types.get(name)) {
                Some(ty) => AssocResolution::Found(ty),
                None => AssocResolution::Ambiguous,
            },
            None => AssocResolution::Ambiguous,
        }
    }

    /// Owner key of the associated constant `name` as reached through a
    /// named self type: the self type itself when its impl supplies the
    /// constant, otherwise the trait whose default applies.
    #[must_use]
    pub fn assoc_const_owner_for_self(&self, self_ty: &str, name: &str) -> Option<String> {
        let entry = self.selves.get(self_ty)?;
        if entry.consts.contains_key(name) {
            return Some(self_ty.to_string());
        }
        entry.traits.iter().find_map(|t| {
            let decl = self.traits.get(t)?;
            decl.consts.get(name)?.1.as_ref().map(|_| t.clone())
        })
    }

    /// Owner key of the associated constant `name` reached through
    /// `trait_name` alone: the trait when it carries a default, otherwise
    /// the single implementor that supplies the constant.
    #[must_use]
    pub fn assoc_const_owner_for_trait(
        &self,
        trait_name: &str,
        name: &str,
    ) -> AssocResolution<String> {
        let Some(decl) = self.traits.get(trait_name) else {
            return AssocResolution::Unknown;
        };
        let Some((_, default)) = decl.consts.get(name) else {
            return AssocResolution::Unknown;
        };
        if default.is_some() {
            return AssocResolution::Found(trait_name.to_string());
        }
        match self.sole_supplier(trait_name, |entry| entry.consts.contains_key(name)) {
            Some(self_ty) => AssocResolution::Found(self_ty.to_string()),
            None => AssocResolution::Ambiguous,
        }
    }

    /// Declared type of an associated constant supplied by `self_ty` or
    /// defaulted by a trait it implements.
    #[must_use]
    pub fn assoc_const_ty_for_self(&self, self_ty: &str, name: &str) -> Option<&Type> {
        let entry = self.selves.get(self_ty)?;
        if let Some(ty) = entry.consts.get(name) {
            return Some(ty);
        }
        entry
            .traits
            .iter()
            .find_map(|t| self.traits.get(t)?.consts.get(name).map(|(ty, _)| ty))
    }

    /// The one implementor of `trait_name` for which `has` holds, or `None`
    /// when zero or several qualify.
    fn sole_supplier(&self, trait_name: &str, has: impl Fn(&SelfAssoc) -> bool) -> Option<&str> {
        let implementors = self.implementors.get(trait_name)?;
        let mut found = None;
        for self_ty in implementors {
            let Some(entry) = self.selves.get(self_ty) else {
                continue;
            };
            if !has(entry) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(self_ty.as_str());
        }
        found
    }

    /// Associated items `trait_name` declares without a default, sorted by
    /// name. Every impl of the trait has to supply each of them.
    #[must_use]
    pub fn required_assoc_items(&self, trait_name: &str) -> Vec<MissingAssocItem> {
        let Some(decl) = self.traits.get(trait_name) else {
            return Vec::new();
        };
        let mut required: Vec<MissingAssocItem> = decl
            .types
            .iter()
            .filter(|(_, default)| default.is_none())
            .map(|(name, _)| MissingAssocItem {
                kind: "type",
                name: name.clone(),
            })
            .chain(
                decl.consts
                    .iter()
                    .filter(|(_, (_, default))| default.is_none())
                    .map(|(name, _)| MissingAssocItem {
                        kind: "const",
                        name: name.clone(),
                    }),
            )
            .collect();
        required.sort_by(|a, b| a.name.cmp(&b.name));
        required
    }
}

/// Source name of the head segment of a type written as a path
/// (`Point`, `Wrapper<T>`). Structural types name no declaration.
#[must_use]
pub fn type_head_name(ty: &Type) -> Option<&str> {
    let TypeKind::Path(path) = &ty.kind else {
        return None;
    };
    path.segments.last().map(|s| s.name.name.as_str())
}

/// The parameter each generic argument of a self type names: `Stack<T>`
/// gives `[Some("T")]`, and an argument that is not a bare name gives `None`.
fn self_type_params(ty: &Type) -> Vec<Option<String>> {
    let TypeKind::Path(path) = &ty.kind else {
        return Vec::new();
    };
    let Some(last) = path.segments.last() else {
        return Vec::new();
    };
    last.generics
        .iter()
        .map(|arg| match arg {
            crate::GenericArg::Type(arg_ty) => match &arg_ty.kind {
                TypeKind::Path(arg_path)
                    if arg_path.segments.len() == 1 && arg_path.segments[0].generics.is_empty() =>
                {
                    Some(arg_path.segments[0].name.name.clone())
                }
                _ => None,
            },
            crate::GenericArg::Const(_) => None,
        })
        .collect()
}
