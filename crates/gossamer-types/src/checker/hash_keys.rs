//! Keys of hashed containers whose type compares by a hand-written `eq`.

use std::collections::HashSet;

use gossamer_lex::Span;

use super::{BTREE_SET_DEF_LOCAL, DefId, HASH_SET_DEF_LOCAL, TypeChecker};
use crate::{Ty, TyKind, TypeError};

impl TypeChecker<'_> {
    /// Notes an expression whose type is a `Map` or `Set` of any order, so
    /// its key type is checked once inference settles.
    pub(super) fn note_hashed_container(&mut self, ty: Ty, span: Span) {
        let resolved = self.infer.resolve(self.tcx, ty);
        let keyed = match self.tcx.kind(resolved) {
            Some(TyKind::HashMap { .. }) => true,
            Some(TyKind::Adt { def, .. }) => {
                matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
            }
            _ => false,
        };
        if keyed && self.noted_hashed_containers.insert(resolved) {
            self.deferred_hash_keys.push((resolved, span));
        }
    }

    /// GT0138 for a `Map` key or `Set` element type that reaches a type
    /// with a hand-written `PartialEq`: a hashed container hashes and
    /// compares structurally, and an ordered one seats keys by `cmp`, which
    /// is structural unless the type writes its own. Either would keep two
    /// keys that `eq` calls equal.
    pub(super) fn check_deferred_hash_keys(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_hash_keys);
        if self.custom_eq_defs.is_empty() {
            return;
        }
        let custom_eq = self.custom_eq_defs.clone();
        // An ordered container seats keys by `cmp`, so a type whose own `cmp`
        // is written beside its `eq` keys one consistently.
        let eq_without_cmp: HashSet<DefId> = custom_eq
            .difference(&self.custom_cmp_defs)
            .copied()
            .collect();
        let mut reported = HashSet::new();
        for (ty, span) in deferred {
            let ty = self.deep_resolve(ty);
            let (key, ordered) = match self.tcx.kind(ty).cloned() {
                Some(TyKind::HashMap { key, ordered, .. }) => (key, ordered),
                Some(TyKind::Adt { def, substs }) => match substs.types().first() {
                    Some(elem) => (*elem, def.local == BTREE_SET_DEF_LOCAL),
                    None => continue,
                },
                _ => continue,
            };
            let offending = if ordered { &eq_without_cmp } else { &custom_eq };
            let mut visited = HashSet::new();
            let Some(owner) = self.custom_eq_type_in(key, offending, &mut visited) else {
                continue;
            };
            if reported.insert((owner.clone(), ordered)) {
                let key = self.render_public_ty(key);
                self.emit(
                    TypeError::HashKeyCustomEq {
                        key,
                        owner,
                        ordered,
                    },
                    span,
                );
            }
        }
    }

    /// The name of a type with a hand-written `eq` that `ty` is or holds.
    fn custom_eq_type_in(
        &mut self,
        ty: Ty,
        custom_eq: &HashSet<DefId>,
        visited: &mut HashSet<Ty>,
    ) -> Option<String> {
        let ty = self.infer.resolve(self.tcx, ty);
        if !visited.insert(ty) {
            return None;
        }
        let parts: Vec<Ty> = match self.tcx.kind(ty).cloned()? {
            TyKind::Adt { def, substs } => {
                let name = self.tcx.def_name(def).map(str::to_string);
                if custom_eq.contains(&def) {
                    return Some(name.unwrap_or_default());
                }
                let mut parts = substs.types().clone();
                if let Some(fields) = self.struct_fields.get(&def) {
                    parts.extend(fields.iter().map(|(_, field)| *field));
                }
                if let Some(name) = &name {
                    parts.extend(
                        self.enum_variant_payloads
                            .iter()
                            .filter(|((owner, _), _)| owner == name)
                            .flat_map(|(_, payload)| payload.iter().copied()),
                    );
                    parts.extend(
                        self.enum_variant_named_payloads
                            .iter()
                            .filter(|((owner, _), _)| owner == name)
                            .flat_map(|(_, fields)| fields.iter().map(|(_, field)| *field)),
                    );
                }
                parts
            }
            TyKind::Tuple(elems) => elems,
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => vec![elem],
            _ => return None,
        };
        parts
            .into_iter()
            .find_map(|part| self.custom_eq_type_in(part, custom_eq, visited))
    }
}
