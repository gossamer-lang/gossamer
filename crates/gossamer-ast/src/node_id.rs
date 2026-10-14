//! Node identifier and monotonic generator used across AST structures.

#![forbid(unsafe_code)]

/// Monotonic identifier assigned to every AST node within a single source file.
///
/// `NodeId` is opaque and cheap to copy. It is not meaningful across files:
/// two nodes in different source files may share the same numeric value. Use
/// the enclosing `FileId` to disambiguate when necessary.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(u32);

impl NodeId {
    /// Sentinel identifier reserved for nodes that have not yet been assigned
    /// an id. Tests and round-trip comparisons treat every `NodeId` as equal,
    /// so this value works as a harmless default.
    pub const DUMMY: Self = Self(u32::MAX);

    /// Returns the raw numeric index of this identifier.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Constructs a `NodeId` from a raw numeric index.
    ///
    /// Prefer `NodeIdGenerator::next` over this constructor in production
    /// code; raw construction exists primarily for hand-built test fixtures.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

/// Hands out fresh `NodeId` values in monotonically increasing order.
///
/// A generator is owned by whatever builds the AST for a single source file,
/// typically the parser. The first identifier produced has value `0`.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeIdGenerator {
    /// Next identifier the generator will hand out.
    next: u32,
}

impl NodeIdGenerator {
    /// Returns a fresh generator whose next id is `0`.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Returns a generator whose next id is `first`, continuing the
    /// numbering of a parse that issued `first` ids.
    #[must_use]
    pub const fn starting_at(first: u32) -> Self {
        Self { next: first }
    }

    /// Produces the next identifier and advances the counter.
    ///
    /// The method is named `next` to read naturally at call sites; it is not a
    /// `std::iter::Iterator::next` implementation because the generator has
    /// no exhaustion condition.
    #[allow(
        clippy::should_implement_trait,
        reason = "infallible generator; Iterator::next would force Option<NodeId>"
    )]
    pub fn next(&mut self) -> NodeId {
        let id = NodeId(self.next);
        self.next = self.next.saturating_add(1);
        id
    }

    /// Returns the number of identifiers this generator has produced so far.
    #[must_use]
    pub const fn issued(&self) -> u32 {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeId, NodeIdGenerator};

    #[test]
    fn generator_starts_at_zero_and_monotonically_increases() {
        let mut generator = NodeIdGenerator::new();
        assert_eq!(generator.next(), NodeId(0));
        assert_eq!(generator.next(), NodeId(1));
        assert_eq!(generator.next(), NodeId(2));
        assert_eq!(generator.issued(), 3);
    }

    #[test]
    fn dummy_identifier_is_distinguishable_from_generated_ids() {
        let mut generator = NodeIdGenerator::new();
        assert_ne!(generator.next(), NodeId::DUMMY);
    }

    #[test]
    fn from_raw_round_trips_through_as_u32() {
        assert_eq!(NodeId::from_raw(42).as_u32(), 42);
    }
}
