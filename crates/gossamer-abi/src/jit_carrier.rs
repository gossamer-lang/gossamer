//! Shape of a two-word `Option` / `Result` carrier crossing the JIT boundary.
//!
//! A carrier is `[disc, payload]`: disc 0 is `Some` / `Ok`, disc 1 is `None` /
//! `Err`. The payload word of an arm is the scalar itself, a string body, an
//! `errors::Error` node, or - for a payload that is itself a carrier - the
//! address of the 16-byte counted box holding that carrier's two words.

/// One node of a [`CarrierShape`], in preorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CarrierNode {
    /// A unit payload; the word carries nothing.
    Unit,
    /// An integer payload, the word itself.
    I64,
    /// A double payload, stored as its bit pattern.
    F64,
    /// A boolean payload in the low bit of the word.
    Bool,
    /// A Unicode scalar payload, stored as its code point.
    Char,
    /// A `String` payload: the word is a string body the carrier holds a share of.
    Str,
    /// An `errors::Error` payload: the word is a runtime error node.
    Error,
    /// An `Option`; the node for its `Some` payload follows.
    Option,
    /// A `Result`; the node for its `Ok` payload follows, then its `Err` payload.
    Result,
}

impl CarrierNode {
    const ALL: [Self; 9] = [
        Self::Unit,
        Self::I64,
        Self::F64,
        Self::Bool,
        Self::Char,
        Self::Str,
        Self::Error,
        Self::Option,
        Self::Result,
    ];

    /// Whether this node is itself a two-word carrier.
    #[must_use]
    pub const fn is_carrier(self) -> bool {
        matches!(self, Self::Option | Self::Result)
    }

    const fn code(self) -> u64 {
        match self {
            Self::Unit => 1,
            Self::I64 => 2,
            Self::F64 => 3,
            Self::Bool => 4,
            Self::Char => 5,
            Self::Str => 6,
            Self::Error => 7,
            Self::Option => 8,
            Self::Result => 9,
        }
    }
}

/// The counted-box meta of each node of a [`CarrierShape`], indexed by node:
/// `Some` for a carrier kept in a box as another carrier's payload.
pub type CarrierBoxMetas = Box<[Option<&'static [i64]>]>;

const BITS_PER_NODE: u32 = 4;

/// A carrier's payload tree packed into one word, so a JIT slot kind stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CarrierShape {
    bits: u64,
    len: u8,
}

impl CarrierShape {
    /// Most nodes one shape holds.
    pub const MAX_NODES: usize = (u64::BITS / BITS_PER_NODE) as usize;

    /// Packs a preorder node list whose root is a carrier, or `None` when the
    /// list is not exactly one well-formed tree or holds too many nodes.
    #[must_use]
    pub fn from_nodes(nodes: &[CarrierNode]) -> Option<Self> {
        if nodes.len() > Self::MAX_NODES || !nodes.first().is_some_and(|n| n.is_carrier()) {
            return None;
        }
        let mut shape = Self { bits: 0, len: 0 };
        for (i, node) in nodes.iter().enumerate() {
            shape.bits |= node.code() << (i as u32 * BITS_PER_NODE);
        }
        shape.len = u8::try_from(nodes.len()).ok()?;
        (shape.subtree_end(0)? == nodes.len()).then_some(shape)
    }

    /// Number of nodes in the tree.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Always false: a shape's root is a carrier node.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The node at preorder index `at`.
    ///
    /// # Panics
    /// When `at` is not below [`Self::len`].
    #[must_use]
    pub fn node(&self, at: usize) -> CarrierNode {
        assert!(at < self.len(), "carrier shape index {at} out of range");
        let code = (self.bits >> (at as u32 * BITS_PER_NODE)) & 0xf;
        // Every stored code was written by `from_nodes` from a real node.
        CarrierNode::ALL[(code - 1) as usize]
    }

    /// Index of the payload node for arm `disc` of the carrier node at `at`:
    /// disc 0 is `Some` / `Ok`, disc 1 is `Err`. `None` for an `Option`'s
    /// `None` arm, which has no payload, and for a node that is not a carrier.
    #[must_use]
    pub fn arm(&self, at: usize, disc: i64) -> Option<usize> {
        match (self.node(at), disc) {
            (CarrierNode::Option | CarrierNode::Result, 0) => Some(at + 1),
            (CarrierNode::Result, 1) => self.subtree_end(at + 1),
            _ => None,
        }
    }

    /// Index one past the last node of the subtree rooted at `at`.
    fn subtree_end(&self, at: usize) -> Option<usize> {
        if at >= self.len() {
            return None;
        }
        match self.node(at) {
            CarrierNode::Option => self.subtree_end(at + 1),
            CarrierNode::Result => self.subtree_end(self.subtree_end(at + 1)?),
            _ => Some(at + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CarrierNode as N, CarrierShape};

    #[test]
    fn nested_result_arms_address_each_payload_subtree() {
        let shape = CarrierShape::from_nodes(&[N::Result, N::Option, N::Str, N::Str]).unwrap();
        assert_eq!(shape.len(), 4);
        assert_eq!(shape.arm(0, 0), Some(1));
        assert_eq!(shape.arm(0, 1), Some(3));
        assert_eq!(shape.node(shape.arm(1, 0).unwrap()), N::Str);
        assert_eq!(shape.arm(1, 1), None);
    }

    #[test]
    fn from_nodes_rejects_malformed_trees() {
        assert!(CarrierShape::from_nodes(&[N::Str]).is_none());
        assert!(CarrierShape::from_nodes(&[N::Result, N::Str]).is_none());
        assert!(CarrierShape::from_nodes(&[N::Option, N::Str, N::Str]).is_none());
        assert!(CarrierShape::from_nodes(&[N::Option; 17]).is_none());
    }
}
