//! Limits the XML decoders on every tier share.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Default cap on element nesting, as the JSON and YAML decoders take.
pub const DEFAULT_MAX_DEPTH: usize = 128;

/// No cap on input bytes by default, as `quick-xml` has none.
pub const DEFAULT_MAX_SIZE: usize = usize::MAX;

/// Process-wide parser caps, read by every tier.
static MAX_DEPTH: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_DEPTH);
static MAX_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_SIZE);

/// Overrides the process-wide cap on parser nesting depth.
pub fn set_max_depth(n: usize) {
    MAX_DEPTH.store(n, Ordering::Relaxed);
}

/// Overrides the process-wide cap on parser input bytes.
pub fn set_max_size(n: usize) {
    MAX_SIZE.store(n, Ordering::Relaxed);
}

/// Current cap on parser nesting depth.
#[must_use]
pub fn max_depth() -> usize {
    MAX_DEPTH.load(Ordering::Relaxed)
}

/// Current cap on parser input bytes.
#[must_use]
pub fn max_size() -> usize {
    MAX_SIZE.load(Ordering::Relaxed)
}

/// How many times its own length a document's entity references may expand
/// to, as libxml2 bounds amplification, so a few nested entities cannot grow
/// a small document without bound.
pub const ENTITY_AMPLIFICATION: usize = 8;

/// Expansion every document may use however small it is.
pub const ENTITY_EXPANSION_FLOOR: usize = 1 << 20;

/// The bytes the entity references of a document of `len` bytes may expand
/// to, within the size cap `cap`.
#[must_use]
pub fn entity_expansion_budget(len: usize, cap: usize) -> usize {
    len.saturating_mul(ENTITY_AMPLIFICATION)
        .max(ENTITY_EXPANSION_FLOOR)
        .min(cap)
}

/// The failure an expansion past [`entity_expansion_budget`] reports.
pub const EXPANSION_EXCEEDED: &str = "xml: entity references expand past the document's budget";

/// The failure `xml::encode` reports for a value that is not the tree
/// `xml::parse` answers.
pub const NOT_A_TREE: &str =
    "xml::encode: the value is not an element tree of the shape `xml::parse` answers";
