//! Every runtime helper the compiled tiers may call resolves by name.
//!
//! The Cranelift JIT registers helpers from the table the runtime generates
//! from its own definitions, and the native backend imports every ABI
//! registry entry. A registry name the table cannot resolve would lower to a
//! call the JIT cannot finalize, and a table name outside the registry is a
//! helper neither backend can declare with its signature.

use std::collections::BTreeSet;

#[test]
fn every_registry_entry_resolves_through_the_generated_table() {
    let unresolved: Vec<&str> = gossamer_abi::REGISTRY
        .iter()
        .map(|entry| entry.name)
        .filter(|name| gossamer_runtime::symbols::address(name).is_none())
        .collect();
    assert!(unresolved.is_empty(), "no address for: {unresolved:?}");
}

#[test]
fn every_generated_symbol_has_a_registry_signature() {
    let registry: BTreeSet<&str> = gossamer_abi::REGISTRY.iter().map(|e| e.name).collect();
    let undeclared: Vec<&str> = gossamer_runtime::symbols::names()
        .filter(|name| !registry.contains(name))
        .collect();
    assert!(
        undeclared.is_empty(),
        "runtime exports with no ABI registry signature: {undeclared:?}"
    );
}
