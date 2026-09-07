//! Side-table mapping AST nodes to the types assigned by the checker.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use gossamer_ast::NodeId;

use crate::ty::Ty;

/// Persistent `NodeId → Ty` map produced by the type checker.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypeTable {
    entries: HashMap<NodeId, Ty>,
    method_owners: HashMap<NodeId, String>,
}

impl TypeTable {
    /// Returns an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the type assigned to `node`.
    pub fn insert(&mut self, node: NodeId, ty: Ty) {
        self.entries.insert(node, ty);
    }

    /// Returns the type recorded for `node`, if any.
    #[must_use]
    pub fn get(&self, node: NodeId) -> Option<Ty> {
        self.entries.get(&node).copied()
    }

    /// Records the `impl` block a method call resolves to, named by the type
    /// the block was written for.
    ///
    /// The receiver's type decides which block a call reaches, and it is known
    /// here and nowhere later: a container and a structural type both reach a
    /// method as an untyped handle, which the lowering below types the way it
    /// types an integer. Resolving here is what lets two types implement one
    /// trait and each call reach its own body.
    pub fn insert_method_owner(&mut self, node: NodeId, owner: String) {
        self.method_owners.insert(node, owner);
    }

    /// The `impl` owner recorded for a method call, if one was.
    #[must_use]
    pub fn method_owner(&self, node: NodeId) -> Option<&str> {
        self.method_owners.get(&node).map(String::as_str)
    }

    /// Returns every `(NodeId, Ty)` pair in ascending node order.
    #[must_use]
    pub fn sorted_entries(&self) -> Vec<(NodeId, Ty)> {
        let mut pairs: Vec<(NodeId, Ty)> = self.entries.iter().map(|(k, v)| (*k, *v)).collect();
        pairs.sort_by_key(|(node, _)| node.as_u32());
        pairs
    }

    /// Returns the number of annotated nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no types have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
