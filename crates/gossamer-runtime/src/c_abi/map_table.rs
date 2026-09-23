//! The entry table under one `GosMap` storage shape: a hash table for a
//! `Map`, the ordered B+ tree for a `BTreeMap`. Both answer the same calls,
//! so each storage shape's shims serve both kinds of map.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::hash::Hash;

use rustc_hash::FxHashMap;

use super::map::{ByteKey, ByteKeyRef, KeyOrder, skey_order_cmp};
use super::slot_key::UserCmp;
use crate::ordered::{Cursor, Iter as TreeIter, OrderedTree};

/// How an ordered table compares its keys.
#[derive(Clone, Debug)]
pub(crate) enum TableOrder {
    /// A stored word under a signed, unsigned, or float order.
    Word(KeyOrder),
    /// Raw bytes, lexicographically: strings and canonical enum encodings.
    Bytes,
    /// An aggregate key's flat encoding, field by field under its slot
    /// descriptor.
    Skey(Box<[u8]>),
    /// The order the key type writes for itself, reached through its own
    /// `cmp`. The key's stored bytes are the slots that comparator reads.
    User(UserCmp),
}

/// A key an ordered table can compare under a [`TableOrder`].
pub(crate) trait TableKey {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering;
}

impl TableKey for i64 {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering {
        match order {
            TableOrder::Word(o) => o.rank(*self).cmp(&o.rank(*other)),
            // SAFETY: the comparator is the one the program compiled for this
            // key type, whose value is the word the map stores.
            TableOrder::User(cmp) => unsafe { cmp.order_word(*self, *other) },
            _ => self.cmp(other),
        }
    }
}

impl TableKey for ByteKeyRef {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering {
        match order {
            TableOrder::Skey(desc) => skey_order_cmp(self.as_slice(), other.as_slice(), desc),
            // SAFETY: a user-ordered map stores the slots its key type's own
            // `cmp` reads, so the comparator is called on what it expects.
            TableOrder::User(cmp) => unsafe { cmp.order(self.as_slice(), other.as_slice()) },
            _ => self.as_slice().cmp(other.as_slice()),
        }
    }
}

impl TableKey for [u8] {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering {
        ByteKeyRef::new(self).table_cmp(ByteKeyRef::new(other), order)
    }
}

impl TableKey for Box<[u8]> {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering {
        (**self).table_cmp(&**other, order)
    }
}

impl TableKey for str {
    fn table_cmp(&self, other: &Self, _order: &TableOrder) -> Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl TableKey for String {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering {
        self.as_str().table_cmp(other.as_str(), order)
    }
}

impl TableKey for ByteKey {
    fn table_cmp(&self, other: &Self, order: &TableOrder) -> Ordering {
        let (a, b): (&ByteKeyRef, &ByteKeyRef) = (self.borrow(), other.borrow());
        a.table_cmp(b, order)
    }
}

/// A map's entries, hashed or ordered.
///
/// The ordered arm is boxed so a hashed map, the common case, is no larger
/// than the hash table it holds.
#[derive(Clone)]
pub(crate) enum Table<K, V> {
    Hash(FxHashMap<K, V>),
    Tree(Box<OrderedEntries<K, V>>),
}

/// An ordered table's tree and the order its keys are compared under.
#[derive(Clone)]
pub(crate) struct OrderedEntries<K, V>(pub(crate) OrderedTree<K, V>, pub(crate) TableOrder);

impl<K, V> Default for Table<K, V> {
    fn default() -> Self {
        Self::Hash(FxHashMap::default())
    }
}

impl<K: Hash + Eq + Clone + TableKey, V> Table<K, V> {
    /// A hash table with room for `cap` entries.
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self::Hash(FxHashMap::with_capacity_and_hasher(
            cap,
            rustc_hash::FxBuildHasher,
        ))
    }

    /// An empty table of the kind `order` names: ordered under it, or hashed
    /// when there is none.
    pub(crate) fn for_order(order: Option<TableOrder>) -> Self {
        match order {
            Some(order) => Self::Tree(Box::new(OrderedEntries(OrderedTree::new(), order))),
            None => Self::default(),
        }
    }

    /// An empty table of the same kind and order.
    pub(crate) fn empty_like(&self) -> Self {
        match self {
            Self::Hash(_) => Self::default(),
            Self::Tree(o) => Self::Tree(Box::new(OrderedEntries(OrderedTree::new(), o.1.clone()))),
        }
    }

    /// Whether the keys are the slots a user comparator reads, which the
    /// table owns a share of each counted word of.
    pub(crate) fn keys_own_slots(&self) -> bool {
        matches!(self, Self::Tree(o) if matches!(o.1, TableOrder::User(_)))
    }

    /// Whether traversal already yields key order.
    pub(crate) fn is_ordered(&self) -> bool {
        matches!(self, Self::Tree(..))
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Hash(h) => h.len(),
            Self::Tree(o) => o.0.len(),
        }
    }

    // The hashed arm of each lookup is inlined into its caller; the ordered
    // arm lives out of line, so a tree search does not make every hashed
    // lookup too large to inline.
    #[inline]
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(h) => h.get(key),
            Self::Tree(o) => tree_get(&o.0, &o.1, key),
        }
    }

    #[inline]
    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(h) => h.get_mut(key),
            Self::Tree(o) => {
                let OrderedEntries(t, order) = &mut **o;
                tree_get_mut(t, order, key)
            }
        }
    }

    #[inline]
    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(h) => h.contains_key(key),
            Self::Tree(o) => tree_get(&o.0, &o.1, key).is_some(),
        }
    }

    /// Stores `val` under `key`, answering the value it replaced; an equal
    /// key already stored is kept.
    #[inline]
    pub(crate) fn insert(&mut self, key: K, val: V) -> Option<V> {
        match self {
            Self::Hash(h) => h.insert(key, val),
            Self::Tree(o) => {
                let OrderedEntries(t, order) = &mut **o;
                tree_insert(t, order, key, val)
            }
        }
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        self.remove_entry(key).map(|(_, v)| v)
    }

    #[inline]
    pub(crate) fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + TableKey + ?Sized,
    {
        match self {
            Self::Hash(h) => h.remove_entry(key),
            Self::Tree(o) => {
                let OrderedEntries(t, order) = &mut **o;
                tree_remove(t, order, key)
            }
        }
    }

    /// The value under `key`, inserting `make()` first when there is none.
    pub(crate) fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        match self {
            Self::Hash(h) => h.entry(key).or_insert_with(make),
            Self::Tree(o) => {
                let OrderedEntries(t, order) = &mut **o;
                if !t.contains_key(|k| k.table_cmp(&key, order)) {
                    t.insert(key.clone(), make(), |a, b| a.table_cmp(b, order));
                }
                // Invariant: the key was present or has just been inserted.
                t.get_mut(|k| k.table_cmp(&key, order))
                    .unwrap_or_else(|| unreachable!("inserted key is missing"))
            }
        }
    }

    /// Number of entries ordering before `key`, or at or before it when
    /// `inclusive`. A hashed table has no order, so it answers zero.
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

    /// The keys ranked `lo..hi` in an ordered table, in key order.
    pub(crate) fn keys_between(&self, lo: usize, hi: usize) -> Vec<K> {
        let Self::Tree(o) = self else {
            return Vec::new();
        };
        let OrderedEntries(t, order) = &**o;
        let mut cursor = Cursor::new(t, lo, hi);
        std::iter::from_fn(|| {
            cursor
                .next(t, |a, b| a.table_cmp(b, order))
                .map(|(k, _)| k.clone())
        })
        .collect()
    }

    /// A table of the same order holding the entries ranked `lo..hi`: moved
    /// out of this one when `take`, copied with `copy` otherwise.
    pub(crate) fn window(
        &mut self,
        lo: usize,
        hi: usize,
        take: bool,
        copy: impl Fn(&V) -> V,
    ) -> Self {
        let mut out = self.empty_like();
        let Self::Tree(src) = self else {
            return out;
        };
        let OrderedEntries(t, order) = &mut **src;
        let hi = hi.min(t.len());
        let Self::Tree(dst) = &mut out else {
            return out;
        };
        let dst = &mut dst.0;
        if take {
            for _ in lo..hi.max(lo) {
                if let Some((k, v)) = t.remove_at(lo) {
                    dst.insert(k, v, |a, b| a.table_cmp(b, order));
                }
            }
        } else {
            let mut cursor = Cursor::new(t, lo, hi);
            while let Some((k, v)) = cursor.next(t, |a, b| a.table_cmp(b, order)) {
                dst.insert(k.clone(), copy(v), |a, b| a.table_cmp(b, order));
            }
        }
        out
    }

    /// Every entry: in key order for an ordered table, in hash order
    /// otherwise.
    pub(crate) fn iter(&self) -> TableIter<'_, K, V> {
        match self {
            Self::Hash(h) => TableIter::Hash(h.iter()),
            Self::Tree(o) => TableIter::Tree(o.0.iter()),
        }
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// Every entry with its value writable, in no particular order.
    pub(crate) fn iter_mut(&mut self) -> Box<dyn Iterator<Item = (&K, &mut V)> + '_> {
        match self {
            Self::Hash(h) => Box::new(h.iter_mut()),
            Self::Tree(o) => Box::new(o.0.iter_mut()),
        }
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.iter_mut().map(|(_, v)| v)
    }
}

#[inline(never)]
fn tree_get<'a, K, V, Q>(t: &'a OrderedTree<K, V>, order: &TableOrder, key: &Q) -> Option<&'a V>
where
    K: Clone + Borrow<Q>,
    Q: TableKey + ?Sized,
{
    t.get(|k| k.borrow().table_cmp(key, order))
}

#[inline(never)]
fn tree_get_mut<'a, K, V, Q>(
    t: &'a mut OrderedTree<K, V>,
    order: &TableOrder,
    key: &Q,
) -> Option<&'a mut V>
where
    K: Clone + Borrow<Q>,
    Q: TableKey + ?Sized,
{
    t.get_mut(|k| k.borrow().table_cmp(key, order))
}

#[inline(never)]
fn tree_insert<K: Clone + TableKey, V>(
    t: &mut OrderedTree<K, V>,
    order: &TableOrder,
    key: K,
    val: V,
) -> Option<V> {
    t.insert(key, val, |a, b| a.table_cmp(b, order))
}

#[inline(never)]
fn tree_remove<K, V, Q>(t: &mut OrderedTree<K, V>, order: &TableOrder, key: &Q) -> Option<(K, V)>
where
    K: Clone + Borrow<Q>,
    Q: TableKey + ?Sized,
{
    t.remove(|k| k.borrow().table_cmp(key, order))
}

impl<K, V, Q> std::ops::Index<&Q> for Table<K, V>
where
    K: Hash + Eq + Clone + TableKey + Borrow<Q>,
    Q: Hash + Eq + TableKey + ?Sized,
{
    type Output = V;

    fn index(&self, key: &Q) -> &V {
        self.get(key).unwrap_or_else(|| panic!("map key missing"))
    }
}

impl<'a, K: Hash + Eq + Clone + TableKey, V> IntoIterator for &'a Table<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = TableIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// [`Table::iter`]'s iterator.
pub(crate) enum TableIter<'a, K, V> {
    Hash(std::collections::hash_map::Iter<'a, K, V>),
    Tree(TreeIter<'a, K, V>),
}

impl<'a, K: Clone, V> Iterator for TableIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Hash(it) => it.next(),
            Self::Tree(it) => it.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Hash(it) => it.size_hint(),
            Self::Tree(it) => it.size_hint(),
        }
    }
}

impl<K: Clone, V> ExactSizeIterator for TableIter<'_, K, V> {}
