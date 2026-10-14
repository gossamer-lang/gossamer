//! Serde writers for hashed collections that write their contents in order.
//!
//! A `HashMap` or `HashSet` iterates in an order its hasher's seed decides,
//! which differs between processes and between two maps of one process. A
//! field serialized through these writers lists its entries in key order
//! instead, so two equal tables encode to the same bytes. The wire format is
//! the one serde gives the collection itself, so reading is unchanged.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

use serde::{Serialize, Serializer};

/// Writes `map` with its entries in key order.
///
/// # Errors
///
/// Whatever `serializer` reports.
pub fn map<K, V, H, S>(map: &HashMap<K, V, H>, serializer: S) -> Result<S::Ok, S::Error>
where
    K: Serialize + Ord,
    V: Serialize,
    H: BuildHasher,
    S: Serializer,
{
    let mut entries: Vec<(&K, &V)> = map.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    serializer.collect_map(entries)
}

/// Writes `map`, whose keys have no order, with its entries in the order of
/// their values, which are distinct.
///
/// # Errors
///
/// Whatever `serializer` reports.
pub fn map_by_value<K, V, H, S>(map: &HashMap<K, V, H>, serializer: S) -> Result<S::Ok, S::Error>
where
    K: Serialize,
    V: Serialize + Ord,
    H: BuildHasher,
    S: Serializer,
{
    let mut entries: Vec<(&K, &V)> = map.iter().collect();
    entries.sort_unstable_by(|a, b| a.1.cmp(b.1));
    serializer.collect_map(entries)
}

/// Writes `map`, whose values are maps, with every level in key order.
///
/// # Errors
///
/// Whatever `serializer` reports.
pub fn map_of_maps<K, K2, V, H, H2, S>(
    map: &HashMap<K, HashMap<K2, V, H2>, H>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    K: Serialize + Ord,
    K2: Serialize + Ord,
    V: Serialize,
    H: BuildHasher,
    H2: BuildHasher,
    S: Serializer,
{
    let mut entries: Vec<(&K, Ordered<'_, K2, V, H2>)> =
        map.iter().map(|(k, v)| (k, Ordered(v))).collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    serializer.collect_map(entries)
}

/// Writes `set` with its elements in order.
///
/// # Errors
///
/// Whatever `serializer` reports.
pub fn set<T, H, S>(set: &HashSet<T, H>, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize + Ord,
    H: BuildHasher,
    S: Serializer,
{
    let mut elements: Vec<&T> = set.iter().collect();
    elements.sort_unstable();
    serializer.collect_seq(elements)
}

/// A borrowed map written in key order.
struct Ordered<'a, K, V, H>(&'a HashMap<K, V, H>);

impl<K: Serialize + Ord, V: Serialize, H: BuildHasher> Serialize for Ordered<'_, K, V, H> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        map(self.0, serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Holder {
        #[serde(serialize_with = "map")]
        m: HashMap<u32, String>,
        #[serde(serialize_with = "set")]
        s: HashSet<u32>,
    }

    fn holder(order: &[u32]) -> Holder {
        Holder {
            m: order.iter().map(|&k| (k, k.to_string())).collect(),
            s: order.iter().copied().collect(),
        }
    }

    #[test]
    fn equal_tables_built_in_any_order_encode_alike() {
        let a = postcard::to_allocvec(&holder(&[3, 1, 2, 9, 7])).unwrap();
        for _ in 0..8 {
            let b = postcard::to_allocvec(&holder(&[7, 9, 2, 1, 3])).unwrap();
            assert_eq!(a, b);
        }
    }

    #[test]
    fn an_ordered_map_reads_back_as_the_map_it_was() {
        let written = holder(&[5, 4, 6]);
        let bytes = postcard::to_allocvec(&written).unwrap();
        let (m, s): (HashMap<u32, String>, HashSet<u32>) = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(m, written.m);
        assert_eq!(s, written.s);
    }
}
