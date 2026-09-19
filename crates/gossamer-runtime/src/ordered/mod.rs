//! The ordered container behind `BTreeMap` and `BTreeSet` on every tier, and
//! the order-preserving key encoding its built-in order compares.

pub mod key;
pub mod tree;

pub use key::KeyEncoder;
pub use tree::{Cursor, Iter, OrderedTree};
