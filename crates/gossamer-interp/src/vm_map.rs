//! A VM map's entries: an insertion-dense hash table for a `Map`, the
//! runtime's ordered B+ tree for a `BTreeMap`, so the VM and the compiled
//! tiers run one ordered algorithm.

use std::cmp::Ordering;

use gossamer_runtime::ordered::{Iter as TreeIter, OrderedTree};

use crate::value::{DenseMap, MapKey, Value, dense_map, dense_map_with_capacity};

/// How a `BTreeMap`'s keys compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrder {
    /// The language's order over [`MapKey`].
    Natural,
    /// As [`KeyOrder::Natural`], except an integer key orders by the unsigned
    /// value its bits spell: the keys were declared `u64` / `usize`.
    Unsigned,
    /// The order the key type's own `cmp` answers, reached through the named
    /// comparator the program compiled for that type.
    User(&'static str),
}

impl KeyOrder {
    /// Orders two keys.
    #[must_use]
    pub fn cmp(self, a: &MapKey, b: &MapKey) -> Ordering {
        match (self, a, b) {
            (Self::Unsigned, MapKey::Int(x), MapKey::Int(y)) => (*x as u64).cmp(&(*y as u64)),
            (Self::User(comparator), _, _) => {
                crate::vm::user_order::order(comparator, a, b).unwrap_or_else(|| a.cmp(b))
            }
            _ => a.cmp(b),
        }
    }
}

/// A VM map's entries.
#[derive(Debug, Clone)]
pub enum VmMap {
    /// A `Map`.
    Hash(DenseMap<MapKey, Value>),
    /// A `BTreeMap`.
    Tree(OrderedTree<MapKey, Value>, KeyOrder),
}

impl Default for VmMap {
    fn default() -> Self {
        Self::Hash(dense_map())
    }
}

impl VmMap {
    /// An empty `Map` with room for `cap` entries.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self::Hash(dense_map_with_capacity(cap))
    }

    /// An empty `BTreeMap` ordered by `order`.
    #[must_use]
    pub fn ordered(order: KeyOrder) -> Self {
        Self::Tree(OrderedTree::new(), order)
    }

    /// An empty map of the same kind and order.
    #[must_use]
    pub fn empty_like(&self) -> Self {
        match self {
            Self::Hash(_) => Self::default(),
            Self::Tree(_, order) => Self::ordered(*order),
        }
    }

    /// The key order of a `BTreeMap`, `None` for a `Map`.
    #[must_use]
    pub fn key_order(&self) -> Option<KeyOrder> {
        match self {
            Self::Hash(_) => None,
            Self::Tree(_, order) => Some(*order),
        }
    }

    /// The ordered tree and its order, for a `BTreeMap`'s own operations.
    #[must_use]
    pub fn tree(&self) -> Option<(&OrderedTree<MapKey, Value>, KeyOrder)> {
        match self {
            Self::Tree(tree, order) => Some((tree, *order)),
            Self::Hash(_) => None,
        }
    }

    /// [`VmMap::tree`], writable.
    pub fn tree_mut(&mut self) -> Option<(&mut OrderedTree<MapKey, Value>, KeyOrder)> {
        match self {
            Self::Tree(tree, order) => Some((tree, *order)),
            Self::Hash(_) => None,
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Hash(h) => h.len(),
            Self::Tree(t, _) => t.len(),
        }
    }

    /// Whether the map holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The value stored under `key`.
    #[must_use]
    pub fn get(&self, key: &MapKey) -> Option<&Value> {
        match self {
            Self::Hash(h) => h.get(key),
            Self::Tree(t, order) => t.get(|k| order.cmp(k, key)),
        }
    }

    /// The value stored under `key`, writable in place.
    pub fn get_mut(&mut self, key: &MapKey) -> Option<&mut Value> {
        match self {
            Self::Hash(h) => h.get_mut(key),
            Self::Tree(t, order) => {
                let order = *order;
                t.get_mut(|k| order.cmp(k, key))
            }
        }
    }

    /// Whether an entry is stored under `key`.
    #[must_use]
    pub fn contains_key(&self, key: &MapKey) -> bool {
        match self {
            Self::Hash(h) => h.contains_key(key),
            Self::Tree(t, order) => t.contains_key(|k| order.cmp(k, key)),
        }
    }

    /// Settles a `BTreeMap`'s order as its first key arrives: a key type that
    /// declares its own `cmp` orders by that body, whichever constructor built
    /// the map. An empty tree holds nothing the new order would reseat.
    fn settle_order(&mut self, key: &MapKey) {
        if let Self::Tree(tree, order) = self
            && *order == KeyOrder::Natural
            && tree.is_empty()
            && let Some(comparator) = crate::vm::user_order::comparator_for(key)
        {
            *order = KeyOrder::User(comparator);
        }
    }

    /// Stores `val` under `key`, answering the value it replaced.
    pub fn insert(&mut self, key: MapKey, val: Value) -> Option<Value> {
        self.settle_order(&key);
        match self {
            Self::Hash(h) => h.insert(key, val),
            Self::Tree(t, order) => {
                let order = *order;
                t.insert(key, val, |a, b| order.cmp(a, b))
            }
        }
    }

    /// Removes the entry under `key`.
    pub fn remove(&mut self, key: &MapKey) -> Option<Value> {
        self.remove_entry(key).map(|(_, v)| v)
    }

    /// Removes the entry under `key`, keeping a `Map`'s other entries in the
    /// order they were added.
    pub fn shift_remove(&mut self, key: &MapKey) -> Option<Value> {
        match self {
            Self::Hash(h) => h.shift_remove(key),
            Self::Tree(..) => self.remove(key),
        }
    }

    /// [`VmMap::remove`], answering the stored key too.
    pub fn remove_entry(&mut self, key: &MapKey) -> Option<(MapKey, Value)> {
        match self {
            Self::Hash(h) => h.swap_remove_entry(key),
            Self::Tree(t, order) => {
                let order = *order;
                t.remove(|k| order.cmp(k, key))
            }
        }
    }

    /// The value under `key`, inserting `make()` first when there is none.
    pub fn get_or_insert_with(&mut self, key: MapKey, make: impl FnOnce() -> Value) -> &mut Value {
        self.settle_order(&key);
        match self {
            Self::Hash(h) => h.entry(key).or_insert_with(make),
            Self::Tree(t, order) => {
                let order = *order;
                if !t.contains_key(|k| order.cmp(k, &key)) {
                    t.insert(key.clone(), make(), |a, b| order.cmp(a, b));
                }
                // Invariant: the key was present or has just been inserted.
                t.get_mut(|k| order.cmp(k, &key))
                    .unwrap_or_else(|| unreachable!("inserted key is missing"))
            }
        }
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        match self {
            Self::Hash(h) => h.clear(),
            Self::Tree(t, _) => t.clear(),
        }
    }

    /// Every entry: key order for a `BTreeMap`, insertion order for a `Map`.
    #[must_use]
    pub fn iter(&self) -> VmMapIter<'_> {
        match self {
            Self::Hash(h) => VmMapIter::Hash(h.iter()),
            Self::Tree(t, _) => VmMapIter::Tree(t.iter()),
        }
    }

    /// Every key, in [`VmMap::iter`] order.
    pub fn keys(&self) -> impl Iterator<Item = &MapKey> {
        self.iter().map(|(k, _)| k)
    }

    /// Every value, in [`VmMap::iter`] order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.iter().map(|(_, v)| v)
    }

    /// Every entry with its value writable: insertion order for a `Map`, no
    /// particular order for a `BTreeMap`.
    pub fn iter_mut(&mut self) -> Box<dyn Iterator<Item = (&MapKey, &mut Value)> + '_> {
        match self {
            Self::Hash(h) => Box::new(h.iter_mut()),
            Self::Tree(t, _) => Box::new(t.iter_mut()),
        }
    }

    /// Every value, writable, in [`VmMap::iter_mut`] order.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.iter_mut().map(|(_, v)| v)
    }

    /// Every entry in the language's key order, whichever kind of map this
    /// is.
    #[must_use]
    pub fn sorted(&self) -> Vec<(&MapKey, &Value)> {
        let mut entries: Vec<(&MapKey, &Value)> = self.iter().collect();
        if let Self::Hash(_) = self {
            entries.sort_by(|a, b| a.0.cmp(b.0));
        }
        entries
    }

    /// Number of entries ordering before `key`, or at or before it when
    /// `inclusive`. A `Map` has no order, so it answers zero.
    #[must_use]
    pub fn rank(&self, key: &MapKey, inclusive: bool) -> usize {
        match self {
            Self::Hash(_) => 0,
            Self::Tree(t, order) => t.rank(inclusive, |k| order.cmp(k, key)),
        }
    }

    /// A `BTreeMap` of the same order holding the entries ranked `lo..hi`
    /// (clamped; a negative `lo` counts back from the end): moved out of this
    /// map when `take`, copied otherwise.
    #[must_use = "the window is a new map; a take has already removed its entries"]
    pub fn window(&mut self, lo: i64, hi: i64, take: bool) -> Self {
        let mut out = self.empty_like();
        let (Self::Tree(src, order), Self::Tree(dst, _)) = (&mut *self, &mut out) else {
            return out;
        };
        let order = *order;
        let len = src.len() as i64;
        let lo = if lo < 0 { lo.saturating_add(len) } else { lo }.clamp(0, len) as usize;
        let hi = (hi.clamp(0, len) as usize).max(lo);
        if take {
            for _ in lo..hi {
                if let Some((k, v)) = src.remove_at(lo) {
                    dst.insert(k, v, |a, b| order.cmp(a, b));
                }
            }
        } else {
            let mut cursor = gossamer_runtime::ordered::Cursor::new(src, lo, hi);
            while let Some((k, v)) = cursor.next(src, |a, b| order.cmp(a, b)) {
                dst.insert(k.clone(), v.clone(), |a, b| order.cmp(a, b));
            }
        }
        out
    }

    /// The values, consuming the map.
    #[must_use]
    pub fn into_values(self) -> Vec<Value> {
        match self {
            Self::Hash(h) => h.into_values().collect(),
            Self::Tree(mut t, _) => {
                let mut out = Vec::with_capacity(t.len());
                while let Some((_, v)) = t.pop_first() {
                    out.push(v);
                }
                out
            }
        }
    }

    /// Keeps the entries `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&MapKey, &mut Value) -> bool) {
        match self {
            Self::Hash(h) => h.retain(|k, v| keep(k, v)),
            Self::Tree(t, order) => {
                let order = *order;
                let mut kept = OrderedTree::new();
                while let Some((k, mut v)) = t.pop_first() {
                    if keep(&k, &mut v) {
                        kept.insert(k, v, |a, b| order.cmp(a, b));
                    }
                }
                *t = kept;
            }
        }
    }
}

impl Extend<(MapKey, Value)> for VmMap {
    fn extend<I: IntoIterator<Item = (MapKey, Value)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl FromIterator<(MapKey, Value)> for VmMap {
    fn from_iter<I: IntoIterator<Item = (MapKey, Value)>>(iter: I) -> Self {
        let mut map = Self::default();
        map.extend(iter);
        map
    }
}

impl<'a> IntoIterator for &'a mut VmMap {
    type Item = (&'a MapKey, &'a mut Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a MapKey, &'a mut Value)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a> IntoIterator for &'a VmMap {
    type Item = (&'a MapKey, &'a Value);
    type IntoIter = VmMapIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl From<DenseMap<MapKey, Value>> for VmMap {
    fn from(map: DenseMap<MapKey, Value>) -> Self {
        Self::Hash(map)
    }
}

/// [`VmMap::iter`]'s iterator.
pub enum VmMapIter<'a> {
    /// A `Map`'s entries.
    Hash(indexmap::map::Iter<'a, MapKey, Value>),
    /// A `BTreeMap`'s entries.
    Tree(TreeIter<'a, MapKey, Value>),
}

impl<'a> Iterator for VmMapIter<'a> {
    type Item = (&'a MapKey, &'a Value);

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

impl ExactSizeIterator for VmMapIter<'_> {}
