//! Which content-keyed entry points take their key by address is one list both
//! backends read. A name there that no runtime symbol carries, or a
//! content-keyed call the MIR emits that the list does not account for, makes
//! a backend pass the key's own value where the runtime reads the key's slots,
//! and the runtime then dereferences a length or an element as a pointer.

#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use gossamer_abi::{SKEY_BY_ADDRESS, lookup};

/// Content-keyed entry points that take no key, so none crosses by address: a
/// key snapshot, and set operations over whole sets.
const SKEY_WITHOUT_KEY: &[&str] = &[
    "gos_rt_map_keys_skey",
    "gos_rt_set_intersection_skey",
    "gos_rt_set_intersection_to_vec_skey",
    "gos_rt_set_to_vec_skey",
];

#[test]
fn every_by_address_skey_symbol_is_a_runtime_entry_point() {
    for name in SKEY_BY_ADDRESS {
        assert!(
            lookup(name).is_some(),
            "{name} is listed as taking its key by address but the registry has no such symbol"
        );
    }
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read MIR source directory") {
            let path = entry.expect("read MIR source entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn every_content_keyed_call_the_mir_emits_says_how_its_key_crosses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gossamer-mir/src");
    let mut emitted = BTreeSet::new();
    for path in rust_sources(&root) {
        let text = fs::read_to_string(&path).expect("read MIR source file");
        for piece in text.split('"') {
            if piece.starts_with("gos_rt_")
                && piece.contains("skey")
                && piece.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                emitted.insert(piece.to_string());
            }
        }
    }
    assert!(
        !emitted.is_empty(),
        "the scan found no content-keyed calls in the MIR source"
    );
    for name in &emitted {
        assert!(
            SKEY_BY_ADDRESS.contains(&name.as_str()) || SKEY_WITHOUT_KEY.contains(&name.as_str()),
            "MIR emits {name}, which neither SKEY_BY_ADDRESS nor SKEY_WITHOUT_KEY names"
        );
    }
    for name in SKEY_WITHOUT_KEY {
        assert!(
            lookup(name).is_some(),
            "{name} is listed as taking no key but the registry has no such symbol"
        );
    }
}
