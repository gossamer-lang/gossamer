//! A B+ tree whose order is supplied per operation.
//!
//! Values live only in the leaves, leaves are linked for traversal, and every
//! internal node records how many entries each child subtree holds, so the
//! entry at a rank and the rank of a key are both found in O(log n). The
//! comparator is an argument rather than an `Ord` bound: the interpreter can
//! pass one that calls a user `cmp`, native code one that calls a compiled
//! thunk, and both run this one algorithm.

use std::cmp::Ordering;

/// Most entries a leaf holds, and most children an internal node holds.
///
/// A node's keys are searched by binary search and its entries shift on
/// insert; 64 keeps a node of 8-byte key words within eight cache lines, so
/// the shift is a short `memmove` while the tree stays shallow (a million
/// entries sit four levels deep).
pub const FANOUT: usize = 64;

/// Fewest entries or children a node other than the root keeps.
const MIN_FILL: usize = FANOUT / 2;

const NIL: u32 = u32::MAX;

#[derive(Debug, Clone)]
struct Leaf<K, V> {
    keys: Vec<K>,
    vals: Vec<V>,
    prev: u32,
    next: u32,
}

/// Every key under `children[i]` orders before `seps[i]`, and every key under
/// `children[i + 1]` orders at or after it.
#[derive(Debug, Clone)]
struct Internal<K> {
    seps: Vec<K>,
    children: Vec<u32>,
    counts: Vec<usize>,
}

#[derive(Debug, Clone)]
enum Node<K, V> {
    Leaf(Leaf<K, V>),
    Internal(Internal<K>),
    Free,
}

/// Which entry a removal targets.
enum Target<'a, K> {
    Key(&'a dyn Fn(&K) -> Ordering),
    Rank(usize),
}

/// A B+ tree keyed by `K` under an order the caller passes to each operation.
///
/// Every operation on one tree must pass the same order.
#[derive(Debug, Clone)]
pub struct OrderedTree<K, V> {
    nodes: Vec<Node<K, V>>,
    free: Vec<u32>,
    root: u32,
    len: usize,
    version: u64,
}

impl<K: Clone, V> Default for OrderedTree<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone, V> OrderedTree<K, V> {
    /// An empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::Leaf(Leaf {
                keys: Vec::new(),
                vals: Vec::new(),
                prev: NIL,
                next: NIL,
            })],
            free: Vec::new(),
            root: 0,
            len: 0,
            version: 0,
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the tree holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Changes whenever the tree's shape changes, so a [`Cursor`] knows when
    /// its cached leaf position is stale.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        let version = self.version.wrapping_add(1);
        *self = Self::new();
        self.version = version;
    }

    fn leaf(&self, id: u32) -> &Leaf<K, V> {
        match &self.nodes[id as usize] {
            Node::Leaf(leaf) => leaf,
            _ => unreachable!("node {id} is not a leaf"),
        }
    }

    /// Two nodes at once, which a borrow from a sibling and a merge each
    /// write in one step. The ids name different nodes.
    fn pair_mut(&mut self, a: u32, b: u32) -> (&mut Node<K, V>, &mut Node<K, V>) {
        debug_assert_ne!(a, b, "a node cannot be its own sibling");
        let (a, b) = (a as usize, b as usize);
        if a < b {
            let (head, tail) = self.nodes.split_at_mut(b);
            (&mut head[a], &mut tail[0])
        } else {
            let (head, tail) = self.nodes.split_at_mut(a);
            (&mut tail[0], &mut head[b])
        }
    }

    fn alloc(&mut self, node: Node<K, V>) -> u32 {
        if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = node;
            id
        } else {
            self.nodes.push(node);
            (self.nodes.len() - 1) as u32
        }
    }

    fn release(&mut self, id: u32) {
        self.nodes[id as usize] = Node::Free;
        self.free.push(id);
    }

    fn subtree_len(&self, id: u32) -> usize {
        match &self.nodes[id as usize] {
            Node::Leaf(leaf) => leaf.keys.len(),
            Node::Internal(node) => node.counts.iter().sum(),
            Node::Free => 0,
        }
    }

    fn fill(&self, id: u32) -> usize {
        match &self.nodes[id as usize] {
            Node::Leaf(leaf) => leaf.keys.len(),
            Node::Internal(node) => node.children.len(),
            Node::Free => 0,
        }
    }

    /// The leaf holding the probed key's position, and the index the key has
    /// or would take there. `probe(k)` orders a stored `k` against the key
    /// sought.
    fn locate(&self, probe: &impl Fn(&K) -> Ordering) -> (u32, Result<usize, usize>) {
        let mut id = self.root;
        loop {
            match &self.nodes[id as usize] {
                Node::Internal(node) => {
                    let i = node.seps.partition_point(|s| probe(s) != Ordering::Greater);
                    id = node.children[i];
                }
                Node::Leaf(leaf) => return (id, leaf.keys.binary_search_by(probe)),
                Node::Free => unreachable!("reached a freed node"),
            }
        }
    }

    /// The value stored under the probed key. `probe(k)` orders a stored `k`
    /// against the key sought, as for every lookup below.
    pub fn get(&self, probe: impl Fn(&K) -> Ordering) -> Option<&V> {
        self.get_key_value(probe).map(|(_, v)| v)
    }

    /// The stored key the probe matches, and its value.
    pub fn get_key_value(&self, probe: impl Fn(&K) -> Ordering) -> Option<(&K, &V)> {
        let (id, at) = self.locate(&probe);
        let i = at.ok()?;
        let leaf = self.leaf(id);
        Some((&leaf.keys[i], &leaf.vals[i]))
    }

    /// The value stored under the probed key, writable in place.
    pub fn get_mut(&mut self, probe: impl Fn(&K) -> Ordering) -> Option<&mut V> {
        let (id, at) = self.locate(&probe);
        let i = at.ok()?;
        match &mut self.nodes[id as usize] {
            Node::Leaf(leaf) => Some(&mut leaf.vals[i]),
            _ => None,
        }
    }

    /// Whether an entry is stored under the probed key.
    pub fn contains_key(&self, probe: impl Fn(&K) -> Ordering) -> bool {
        self.locate(&probe).1.is_ok()
    }

    /// Number of entries ordering before the probed key, or at or before it
    /// when `inclusive`.
    pub fn rank(&self, inclusive: bool, probe: impl Fn(&K) -> Ordering) -> usize {
        let mut id = self.root;
        let mut before = 0;
        loop {
            match &self.nodes[id as usize] {
                Node::Internal(node) => {
                    let i = node.seps.partition_point(|s| probe(s) != Ordering::Greater);
                    before += node.counts[..i].iter().sum::<usize>();
                    id = node.children[i];
                }
                Node::Leaf(leaf) => {
                    return before
                        + leaf.keys.partition_point(|k| match probe(k) {
                            Ordering::Less => true,
                            Ordering::Equal => inclusive,
                            Ordering::Greater => false,
                        });
                }
                Node::Free => unreachable!("reached a freed node"),
            }
        }
    }

    /// The leaf and index of the entry at `rank`, which is below `len`.
    fn seek(&self, mut rank: usize) -> (u32, usize) {
        let mut id = self.root;
        loop {
            match &self.nodes[id as usize] {
                Node::Internal(node) => {
                    let mut i = 0;
                    while rank >= node.counts[i] {
                        rank -= node.counts[i];
                        i += 1;
                    }
                    id = node.children[i];
                }
                Node::Leaf(_) => return (id, rank),
                Node::Free => unreachable!("reached a freed node"),
            }
        }
    }

    /// The entry at `rank` in key order.
    #[must_use]
    pub fn entry_at(&self, rank: usize) -> Option<(&K, &V)> {
        if rank >= self.len {
            return None;
        }
        let (id, i) = self.seek(rank);
        let leaf = self.leaf(id);
        Some((&leaf.keys[i], &leaf.vals[i]))
    }

    /// The first entry in key order.
    #[must_use]
    pub fn first(&self) -> Option<(&K, &V)> {
        self.entry_at(0)
    }

    /// The last entry in key order.
    #[must_use]
    pub fn last(&self) -> Option<(&K, &V)> {
        self.len.checked_sub(1).and_then(|r| self.entry_at(r))
    }

    /// Every entry in key order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K, V> {
        let (leaf, idx) = if self.len == 0 {
            (NIL, 0)
        } else {
            self.seek(0)
        };
        Iter {
            tree: self,
            leaf,
            idx,
            left: self.len,
        }
    }

    /// Every entry, in no particular order, with the value writable.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.nodes.iter_mut().flat_map(|node| {
            match node {
                Node::Leaf(leaf) => Some(leaf.keys.iter().zip(leaf.vals.iter_mut())),
                _ => None,
            }
            .into_iter()
            .flatten()
        })
    }

    /// Stores `val` under `key`, answering the value it replaced. An equal key
    /// already stored stays; only its value changes.
    pub fn insert(&mut self, key: K, val: V, cmp: impl Fn(&K, &K) -> Ordering) -> Option<V> {
        match self.insert_at(self.root, key, val, &cmp) {
            Inserted::Replaced(old) => Some(old),
            Inserted::Added(split) => {
                self.len += 1;
                self.version = self.version.wrapping_add(1);
                if let Some((sep, right)) = split {
                    let left = self.root;
                    let counts = vec![self.subtree_len(left), self.subtree_len(right)];
                    self.root = self.alloc(Node::Internal(Internal {
                        seps: vec![sep],
                        children: vec![left, right],
                        counts,
                    }));
                }
                None
            }
        }
    }

    fn insert_at(
        &mut self,
        id: u32,
        key: K,
        val: V,
        cmp: &impl Fn(&K, &K) -> Ordering,
    ) -> Inserted<K, V> {
        // The descent reads the routing keys where they sit, so a node is
        // never moved out of the arena and back to be written.
        let descent = match &self.nodes[id as usize] {
            Node::Internal(node) => {
                let i = node
                    .seps
                    .partition_point(|s| cmp(s, &key) != Ordering::Greater);
                Some((i, node.children[i]))
            }
            Node::Leaf(_) => None,
            Node::Free => unreachable!("reached a freed node"),
        };
        let Some((i, child)) = descent else {
            return self.insert_into_leaf(id, key, val, cmp);
        };
        let result = self.insert_at(child, key, val, cmp);
        let (sep, right) = match result {
            Inserted::Replaced(old) => return Inserted::Replaced(old),
            Inserted::Added(None) => {
                if let Node::Internal(node) = &mut self.nodes[id as usize] {
                    node.counts[i] += 1;
                }
                return Inserted::Added(None);
            }
            Inserted::Added(Some(split)) => split,
        };
        let (left_len, right_len) = (self.subtree_len(child), self.subtree_len(right));
        let split = {
            let Node::Internal(node) = &mut self.nodes[id as usize] else {
                unreachable!("node {id} changed kind during insert");
            };
            node.counts[i] = left_len;
            node.seps.insert(i, sep);
            node.children.insert(i + 1, right);
            node.counts.insert(i + 1, right_len);
            if node.children.len() <= FANOUT {
                None
            } else {
                let mid = node.seps.len() / 2;
                let mut seps = node.seps.split_off(mid);
                let promoted = seps.remove(0);
                Some((
                    promoted,
                    Internal {
                        seps,
                        children: node.children.split_off(mid + 1),
                        counts: node.counts.split_off(mid + 1),
                    },
                ))
            }
        };
        match split {
            None => Inserted::Added(None),
            Some((promoted, sibling)) => {
                let sibling_id = self.alloc(Node::Internal(sibling));
                Inserted::Added(Some((promoted, sibling_id)))
            }
        }
    }

    /// Stores the entry in the leaf `id`, splitting it when it overflows.
    fn insert_into_leaf(
        &mut self,
        id: u32,
        key: K,
        val: V,
        cmp: &impl Fn(&K, &K) -> Ordering,
    ) -> Inserted<K, V> {
        let overflow = {
            let Node::Leaf(leaf) = &mut self.nodes[id as usize] else {
                unreachable!("node {id} is not a leaf");
            };
            match leaf.keys.binary_search_by(|k| cmp(k, &key)) {
                Ok(i) => return Inserted::Replaced(std::mem::replace(&mut leaf.vals[i], val)),
                Err(i) => {
                    leaf.keys.insert(i, key);
                    leaf.vals.insert(i, val);
                    if leaf.keys.len() <= FANOUT {
                        return Inserted::Added(None);
                    }
                    let mid = leaf.keys.len() / 2;
                    Leaf {
                        keys: leaf.keys.split_off(mid),
                        vals: leaf.vals.split_off(mid),
                        prev: id,
                        next: leaf.next,
                    }
                }
            }
        };
        let sep = overflow.keys[0].clone();
        let after = overflow.next;
        let right_id = self.alloc(Node::Leaf(overflow));
        if after != NIL
            && let Node::Leaf(next) = &mut self.nodes[after as usize]
        {
            next.prev = right_id;
        }
        if let Node::Leaf(leaf) = &mut self.nodes[id as usize] {
            leaf.next = right_id;
        }
        Inserted::Added(Some((sep, right_id)))
    }

    /// Removes the entry stored under the probed key.
    pub fn remove(&mut self, probe: impl Fn(&K) -> Ordering) -> Option<(K, V)> {
        self.remove_target(Target::Key(&probe))
    }

    /// Removes the entry at `rank` in key order.
    pub fn remove_at(&mut self, rank: usize) -> Option<(K, V)> {
        if rank >= self.len {
            return None;
        }
        self.remove_target(Target::Rank(rank))
    }

    /// Removes the first entry in key order.
    pub fn pop_first(&mut self) -> Option<(K, V)> {
        self.remove_at(0)
    }

    /// Removes the last entry in key order.
    pub fn pop_last(&mut self) -> Option<(K, V)> {
        self.len.checked_sub(1).and_then(|r| self.remove_at(r))
    }

    fn remove_target(&mut self, target: Target<'_, K>) -> Option<(K, V)> {
        let removed = self.remove_in(self.root, target)?;
        self.len -= 1;
        self.version = self.version.wrapping_add(1);
        if let Node::Internal(node) = &self.nodes[self.root as usize]
            && node.children.len() == 1
        {
            let only = node.children[0];
            self.release(self.root);
            self.root = only;
        }
        Some(removed)
    }

    fn remove_in(&mut self, id: u32, target: Target<'_, K>) -> Option<(K, V)> {
        match &mut self.nodes[id as usize] {
            Node::Leaf(leaf) => {
                let i = match target {
                    Target::Key(probe) => leaf.keys.binary_search_by(probe).ok()?,
                    Target::Rank(rank) => rank,
                };
                Some((leaf.keys.remove(i), leaf.vals.remove(i)))
            }
            Node::Internal(node) => {
                let (i, next) = match target {
                    Target::Key(probe) => (
                        node.seps.partition_point(|s| probe(s) != Ordering::Greater),
                        Target::Key(probe),
                    ),
                    Target::Rank(mut rank) => {
                        let mut i = 0;
                        while rank >= node.counts[i] {
                            rank -= node.counts[i];
                            i += 1;
                        }
                        (i, Target::Rank(rank))
                    }
                };
                let child = node.children[i];
                let removed = self.remove_in(child, next)?;
                if let Node::Internal(node) = &mut self.nodes[id as usize] {
                    node.counts[i] -= 1;
                }
                if self.fill(child) < MIN_FILL {
                    self.rebalance(id, i);
                }
                Some(removed)
            }
            Node::Free => unreachable!("reached a freed node"),
        }
    }

    /// Restores the fill of `parent`'s child `i` by borrowing from a sibling
    /// with entries to spare, or merging with one that has none.
    fn rebalance(&mut self, parent: u32, i: usize) {
        let (children, siblings) = {
            let Node::Internal(p) = &self.nodes[parent as usize] else {
                unreachable!("rebalance under a leaf");
            };
            (
                p.children.len(),
                (
                    (i > 0).then(|| p.children[i - 1]),
                    (i + 1 < p.children.len()).then(|| p.children[i + 1]),
                ),
            )
        };
        if children < 2 {
            return;
        }
        let left_spare = siblings.0.is_some_and(|left| self.fill(left) > MIN_FILL);
        let right_spare = siblings.1.is_some_and(|right| self.fill(right) > MIN_FILL);
        if left_spare {
            self.borrow_from_left(parent, i);
        } else if right_spare {
            self.borrow_from_right(parent, i);
        } else if i > 0 {
            self.merge(parent, i - 1);
        } else {
            self.merge(parent, i);
        }
    }

    /// Moves the left sibling's last entry into child `i`, which the parent's
    /// separator then names.
    fn borrow_from_left(&mut self, parent: u32, i: usize) {
        let (left_id, id) = {
            let Node::Internal(p) = &self.nodes[parent as usize] else {
                unreachable!("rebalance under a leaf");
            };
            (p.children[i - 1], p.children[i])
        };
        // The separator the parent takes, and the entries the move carries.
        let (sep, moved) = {
            let (left, node) = self.pair_mut(left_id, id);
            match (left, node) {
                (Node::Leaf(left), Node::Leaf(leaf)) => {
                    // Invariant: the left sibling has more than MIN_FILL entries.
                    let (Some(k), Some(v)) = (left.keys.pop(), left.vals.pop()) else {
                        unreachable!("a spare sibling is empty");
                    };
                    leaf.keys.insert(0, k);
                    leaf.vals.insert(0, v);
                    (leaf.keys[0].clone(), 1)
                }
                (Node::Internal(left), Node::Internal(node)) => {
                    let (Some(sep), Some(child), Some(count)) =
                        (left.seps.pop(), left.children.pop(), left.counts.pop())
                    else {
                        unreachable!("a spare sibling is empty");
                    };
                    node.children.insert(0, child);
                    node.counts.insert(0, count);
                    (sep, count)
                }
                _ => unreachable!("siblings at different depths"),
            }
        };
        let down = {
            let Node::Internal(p) = &mut self.nodes[parent as usize] else {
                unreachable!("rebalance under a leaf");
            };
            p.counts[i - 1] -= moved;
            p.counts[i] += moved;
            std::mem::replace(&mut p.seps[i - 1], sep)
        };
        // An internal child takes the separator the parent held as its own
        // first one; a leaf routes by its keys and needs none.
        if let Node::Internal(node) = &mut self.nodes[id as usize] {
            node.seps.insert(0, down);
        }
    }

    /// Moves the right sibling's first entry into child `i`, which the
    /// parent's separator then names.
    fn borrow_from_right(&mut self, parent: u32, i: usize) {
        let (id, right_id) = {
            let Node::Internal(p) = &self.nodes[parent as usize] else {
                unreachable!("rebalance under a leaf");
            };
            (p.children[i], p.children[i + 1])
        };
        let (sep, moved, down_needed) = {
            let (node, right) = self.pair_mut(id, right_id);
            match (node, right) {
                (Node::Leaf(leaf), Node::Leaf(right)) => {
                    leaf.keys.push(right.keys.remove(0));
                    leaf.vals.push(right.vals.remove(0));
                    (right.keys[0].clone(), 1, false)
                }
                (Node::Internal(node), Node::Internal(right)) => {
                    let up = right.seps.remove(0);
                    let count = right.counts.remove(0);
                    node.children.push(right.children.remove(0));
                    node.counts.push(count);
                    (up, count, true)
                }
                _ => unreachable!("siblings at different depths"),
            }
        };
        let down = {
            let Node::Internal(p) = &mut self.nodes[parent as usize] else {
                unreachable!("rebalance under a leaf");
            };
            p.counts[i] += moved;
            p.counts[i + 1] -= moved;
            std::mem::replace(&mut p.seps[i], sep)
        };
        if down_needed && let Node::Internal(node) = &mut self.nodes[id as usize] {
            node.seps.push(down);
        }
    }

    /// Folds `parent`'s child `i + 1` into child `i`.
    fn merge(&mut self, parent: u32, i: usize) {
        let (left_id, right_id, sep) = {
            let Node::Internal(p) = &mut self.nodes[parent as usize] else {
                unreachable!("rebalance under a leaf");
            };
            let ids = (p.children[i], p.children[i + 1]);
            let sep = p.seps.remove(i);
            p.children.remove(i + 1);
            let moved = p.counts.remove(i + 1);
            p.counts[i] += moved;
            (ids.0, ids.1, sep)
        };
        let after = {
            let (left, right) = self.pair_mut(left_id, right_id);
            match (left, right) {
                (Node::Leaf(left), Node::Leaf(right)) => {
                    left.keys.append(&mut right.keys);
                    left.vals.append(&mut right.vals);
                    left.next = right.next;
                    right.next
                }
                (Node::Internal(left), Node::Internal(right)) => {
                    left.seps.push(sep);
                    left.seps.append(&mut right.seps);
                    left.children.append(&mut right.children);
                    left.counts.append(&mut right.counts);
                    NIL
                }
                _ => unreachable!("siblings at different depths"),
            }
        };
        if after != NIL
            && let Node::Leaf(next) = &mut self.nodes[after as usize]
        {
            next.prev = left_id;
        }
        self.release(right_id);
    }
}

impl<'t, K: Clone, V> IntoIterator for &'t OrderedTree<K, V> {
    type Item = (&'t K, &'t V);
    type IntoIter = Iter<'t, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// The entries of an [`OrderedTree`] in key order.
#[derive(Debug, Clone)]
pub struct Iter<'t, K, V> {
    tree: &'t OrderedTree<K, V>,
    leaf: u32,
    idx: usize,
    left: usize,
}

impl<'t, K: Clone, V> Iterator for Iter<'t, K, V> {
    type Item = (&'t K, &'t V);

    fn next(&mut self) -> Option<Self::Item> {
        while self.leaf != NIL {
            let leaf = self.tree.leaf(self.leaf);
            if self.idx < leaf.keys.len() {
                self.idx += 1;
                self.left -= 1;
                return Some((&leaf.keys[self.idx - 1], &leaf.vals[self.idx - 1]));
            }
            self.leaf = leaf.next;
            self.idx = 0;
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<K: Clone, V> ExactSizeIterator for Iter<'_, K, V> {}

enum Inserted<K, V> {
    Replaced(V),
    Added(Option<(K, u32)>),
}

/// A position in a tree's key order that survives mutation of the tree.
///
/// It keeps the ranks still to visit, the leaf position of the next one, and
/// the last key taken from each end. While the tree's version is unchanged the
/// leaf position is followed directly; after a change both ends are re-found
/// from those keys, so a walk resumes just past what it last answered.
#[derive(Debug, Clone)]
pub struct Cursor<K> {
    front: usize,
    back: usize,
    leaf: u32,
    idx: usize,
    version: u64,
    last_front: Option<K>,
    last_back: Option<K>,
}

impl<K: Clone> Cursor<K> {
    /// A cursor over the entries of `tree` ranked `front..back`.
    #[must_use]
    pub fn new<V>(tree: &OrderedTree<K, V>, front: usize, back: usize) -> Self {
        let back = back.min(tree.len());
        Self {
            front: front.min(back),
            back,
            leaf: NIL,
            idx: 0,
            version: tree.version(),
            last_front: None,
            last_back: None,
        }
    }

    /// Entries left to visit, from either end, as of the last step.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.back - self.front
    }

    /// Re-finds both ends after `tree` changed shape.
    fn resync<V>(&mut self, tree: &OrderedTree<K, V>, cmp: &impl Fn(&K, &K) -> Ordering) {
        if self.version == tree.version() {
            return;
        }
        if let Some(key) = &self.last_front {
            self.front = tree.rank(true, |k| cmp(k, key));
        }
        self.back = match &self.last_back {
            Some(key) => tree.rank(false, |k| cmp(k, key)),
            None => self.back.min(tree.len()),
        };
        self.front = self.front.min(self.back);
        self.leaf = NIL;
        self.version = tree.version();
    }

    /// Skips up to `n` entries from the front.
    pub fn skip<V>(
        &mut self,
        tree: &OrderedTree<K, V>,
        n: usize,
        cmp: impl Fn(&K, &K) -> Ordering,
    ) {
        self.resync(tree, &cmp);
        self.front = self.front.saturating_add(n).min(self.back);
        self.leaf = NIL;
        self.last_front = None;
    }

    /// The next entry from the front.
    pub fn next<'t, V>(
        &mut self,
        tree: &'t OrderedTree<K, V>,
        cmp: impl Fn(&K, &K) -> Ordering,
    ) -> Option<(&'t K, &'t V)> {
        self.resync(tree, &cmp);
        if self.front >= self.back {
            return None;
        }
        if self.leaf == NIL {
            (self.leaf, self.idx) = tree.seek(self.front);
        }
        let mut leaf = tree.leaf(self.leaf);
        while self.idx >= leaf.keys.len() {
            self.leaf = leaf.next;
            self.idx = 0;
            leaf = tree.leaf(self.leaf);
        }
        let (key, val) = (&leaf.keys[self.idx], &leaf.vals[self.idx]);
        self.idx += 1;
        self.front += 1;
        self.last_front = Some(key.clone());
        Some((key, val))
    }

    /// The next entry from the back.
    pub fn next_back<'t, V>(
        &mut self,
        tree: &'t OrderedTree<K, V>,
        cmp: impl Fn(&K, &K) -> Ordering,
    ) -> Option<(&'t K, &'t V)> {
        self.resync(tree, &cmp);
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        let entry = tree.entry_at(self.back)?;
        self.last_back = Some(entry.0.clone());
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Cursor, OrderedTree};

    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "a tree comparator takes its two keys by reference"
    )]
    fn cmp(a: &i64, b: &i64) -> std::cmp::Ordering {
        a.cmp(b)
    }

    /// A deterministic pseudo-random stream, so a failure replays.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    fn assert_same(tree: &OrderedTree<i64, i64>, model: &BTreeMap<i64, i64>) {
        assert_eq!(tree.len(), model.len());
        let walked: Vec<_> = tree.iter().map(|(k, v)| (*k, *v)).collect();
        let want: Vec<_> = model.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(walked, want);
    }

    #[test]
    fn random_operations_match_std_btreemap() {
        for (seed, span) in [(1, 50), (2, 5_000), (3, 200_000)] {
            let mut rng = Lcg(seed);
            let mut tree = OrderedTree::new();
            let mut model = BTreeMap::new();
            for step in 0..60_000 {
                let key = rng.below(span) as i64 - (span / 2) as i64;
                match rng.below(10) {
                    0..=4 => assert_eq!(tree.insert(key, step, cmp), model.insert(key, step)),
                    5..=6 => assert_eq!(tree.remove(|k| k.cmp(&key)), model.remove_entry(&key)),
                    7 => assert_eq!(tree.pop_first(), model.pop_first()),
                    8 => assert_eq!(tree.pop_last(), model.pop_last()),
                    _ => {
                        assert_eq!(tree.get(|k| k.cmp(&key)), model.get(&key));
                        let below = model.range(..key).count();
                        assert_eq!(tree.rank(false, |k| k.cmp(&key)), below);
                        assert_eq!(
                            tree.rank(true, |k| k.cmp(&key)),
                            model.range(..=key).count()
                        );
                        let at = model.iter().nth(below).map(|(k, v)| (*k, *v));
                        assert_eq!(tree.entry_at(below).map(|(k, v)| (*k, *v)), at);
                    }
                }
            }
            assert_same(&tree, &model);
            while let Some(entry) = tree.pop_first() {
                assert_eq!(Some(entry), model.pop_first());
            }
            assert!(model.is_empty());
        }
    }

    #[test]
    fn a_cursor_walks_a_range_from_both_ends_and_survives_mutation() {
        let mut tree = OrderedTree::new();
        for k in 0..1000 {
            tree.insert(k * 2, k, cmp);
        }
        let lo = tree.rank(false, |k| k.cmp(&100));
        let hi = tree.rank(true, |k| k.cmp(&120));
        let mut cursor = Cursor::new(&tree, lo, hi);
        let front: Vec<_> =
            std::iter::from_fn(|| cursor.next(&tree, cmp).map(|(k, _)| *k)).collect();
        assert_eq!(front, (50..=60).map(|k| k * 2).collect::<Vec<_>>());
        let mut back = Cursor::new(&tree, lo, hi);
        assert_eq!(back.next_back(&tree, cmp).map(|(k, _)| *k), Some(120));
        assert_eq!(back.next(&tree, cmp).map(|(k, _)| *k), Some(100));

        let mut walk = Cursor::new(&tree, 0, tree.len());
        walk.skip(&tree, 10, cmp);
        assert_eq!(walk.next(&tree, cmp).map(|(k, _)| *k), Some(20));
        tree.remove(|k| k.cmp(&0));
        tree.insert(21, 0, cmp);
        assert_eq!(walk.next(&tree, cmp).map(|(k, _)| *k), Some(21));
        assert_eq!(walk.next(&tree, cmp).map(|(k, _)| *k), Some(22));
        let mut tail = Cursor::new(&tree, 0, tree.len());
        assert_eq!(tail.next_back(&tree, cmp).map(|(k, _)| *k), Some(1998));
        tree.remove(|k| k.cmp(&1998));
        tree.remove(|k| k.cmp(&1996));
        assert_eq!(tail.next_back(&tree, cmp).map(|(k, _)| *k), Some(1994));
    }
}
