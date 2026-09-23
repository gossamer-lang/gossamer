//! The table under one `GosSet` element family: insertion-ordered for a
//! `Set`, which every tier reproduces, or the ordered B+ tree for a
//! `BTreeSet`.

use std::borrow::Borrow;
use std::hash::Hash;

use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;

use super::map_table::{OrderedEntries, TableKey, TableOrder};
use crate::ordered::{Cursor, OrderedTree};

/// Entries keyed by `K`, in insertion order or in key order.
///
/// The ordered arm is boxed so an insertion-ordered set, the common case, is
/// no larger than the index map it holds.
#[derive(Clone)]
pub(crate) enum OrdTable<K, V> {
    Hash(IndexMap<K, V, FxBuildHasher>),
    Tree(Box<OrderedEntries<K, V>>),
}

impl<K, V> Default for OrdTable<K, V> {
    fn default() -> Self {
        Self::Hash(IndexMap::default())
    }
}

impl<K: Hash + Eq + Clone + TableKey, V> OrdTable<K, V> {
    /// An empty ordered table under `order`.
    pub(crate) fn ordered(order: TableOrder) -> Self {
        Self::Tree(Box::new(OrderedEntries(OrderedTree::new(), order)))
    }

    /// An empty table of the same kind and order.
    pub(crate) fn empty_like(&self) -> Self {
        match self {
            Self::Hash(_) => Self::default(),
            Self::Tree(o) => Self::ordered(o.1.clone()),
        }
    }

    /// Whether traversal already yields key order.
    pub(crate) fn is_ordered(&self) -> bool {
        matches!(self, Self::Tree(..))
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Hash(h) => h.len(),
            Self::Tree(o) => o.0.len(),
        }
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(h) => h.get(key),
            Self::Tree(o) => o.0.get(|k| k.borrow().table_cmp(key, &o.1)),
        }
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        self.get(key).is_some()
    }

    /// Stores `val` under `key`, answering the value it replaced.
    pub(crate) fn insert(&mut self, key: K, val: V) -> Option<V> {
        match self {
            Self::Hash(h) => h.insert(key, val),
            Self::Tree(o) => {
                let OrderedEntries(t, order) = &mut **o;
                t.insert(key, val, |a, b| a.table_cmp(b, order))
            }
        }
    }

    /// Removes the entry under `key`; a `Set` keeps the others in the order
    /// they were added.
    pub(crate) fn shift_remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(h) => h.shift_remove(key),
            Self::Tree(o) => {
                let OrderedEntries(t, order) = &mut **o;
                t.remove(|k| k.borrow().table_cmp(key, order))
                    .map(|(_, v)| v)
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = self.empty_like();
    }

    /// Every entry, in insertion order or key order.
    pub(crate) fn iter(&self) -> Box<dyn Iterator<Item = (&K, &V)> + '_> {
        match self {
            Self::Hash(h) => Box::new(h.iter()),
            Self::Tree(o) => Box::new(o.0.iter()),
        }
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// Number of entries ordering before `key`, or at or before it when
    /// `inclusive`; zero for an insertion-ordered table.
    pub(crate) fn rank<Q>(&self, key: &Q, inclusive: bool) -> usize
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(_) => 0,
            Self::Tree(o) => o.0.rank(inclusive, |k| k.borrow().table_cmp(key, &o.1)),
        }
    }

    /// A table of the same order holding the entries ranked `lo..hi`: moved
    /// out of this one when `take`, copied with `copy` otherwise.
    pub(crate) fn window(
        &mut self,
        lo: usize,
        hi: usize,
        take: bool,
        copy: impl Fn(&K, &V) -> V,
    ) -> Self {
        let mut out = self.empty_like();
        let (Self::Tree(src), Self::Tree(dst)) = (&mut *self, &mut out) else {
            return out;
        };
        let OrderedEntries(src, order) = &mut **src;
        let dst = &mut dst.0;
        let hi = hi.min(src.len());
        if take {
            for _ in lo..hi.max(lo) {
                if let Some((k, v)) = src.remove_at(lo) {
                    dst.insert(k, v, |a, b| a.table_cmp(b, order));
                }
            }
        } else {
            let mut cursor = Cursor::new(src, lo, hi);
            while let Some((k, v)) = cursor.next(src, |a, b| a.table_cmp(b, order)) {
                dst.insert(k.clone(), copy(k, v), |a, b| a.table_cmp(b, order));
            }
        }
        out
    }
}

impl<K: Hash + Eq + Clone + TableKey> OrdTable<K, ()> {
    /// Adds `key`, answering whether it was absent.
    pub(crate) fn add(&mut self, key: K) -> bool {
        if self.contains_key(&key) {
            return false;
        }
        self.insert(key, ());
        true
    }

    /// The elements of this set that `other` also holds, in this set's order.
    pub(crate) fn common_keys<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = &'a K> {
        self.keys().filter(|k| other.contains_key(*k))
    }
}

/// Set algebra. Each result is a table of this one's kind, so `a.union(b)`
/// reads the way `a` does.
impl<K: Hash + Eq + Clone + TableKey, V: Clone> OrdTable<K, V> {
    fn filtered(&self, keep: impl Fn(&K) -> bool) -> Self {
        let mut out = self.empty_like();
        for (k, v) in self.iter() {
            if keep(k) {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Entries in this table or in `other`; an entry in both keeps this one's.
    pub(crate) fn union(&self, other: &Self) -> Self {
        let mut out = self.filtered(|_| true);
        for (k, v) in other.iter() {
            if !out.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Entries of this table whose key `other` also holds.
    pub(crate) fn intersection(&self, other: &Self) -> Self {
        self.filtered(|k| other.contains_key(k))
    }

    /// Entries of this table whose key `other` does not hold.
    pub(crate) fn difference(&self, other: &Self) -> Self {
        self.filtered(|k| !other.contains_key(k))
    }

    /// Entries whose key exactly one of the two tables holds.
    pub(crate) fn symmetric_difference(&self, other: &Self) -> Self {
        let mut out = self.difference(other);
        for (k, v) in other.iter() {
            if !self.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Whether `other` holds every key of this table.
    pub(crate) fn is_subset(&self, other: &Self) -> bool {
        self.keys().all(|k| other.contains_key(k))
    }

    /// Whether this table holds every key of `other`.
    pub(crate) fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }

    /// Whether the two tables share no key.
    pub(crate) fn is_disjoint(&self, other: &Self) -> bool {
        !self.keys().any(|k| other.contains_key(k))
    }
}

impl<K: Hash + Eq + Clone + TableKey> PartialEq for OrdTable<K, ()> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.keys().all(|k| other.contains_key(k))
    }
}
