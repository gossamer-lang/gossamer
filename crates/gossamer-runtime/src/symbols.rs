//! Name-to-address lookup for every `gos_rt_*` runtime export.
//!
//! `build.rs` generates the table from the definitions themselves, so a
//! helper the runtime defines is reachable by name without a second list to
//! keep in step, and a helper a target does not build is absent from that
//! target's table.

/// A runtime helper's address, shareable across threads because it names
/// code, never mutable data.
#[derive(Clone, Copy)]
struct SymbolAddr(*const ());

// SAFETY: the pointer is a function's address, which is immutable for the
// life of the process.
unsafe impl Sync for SymbolAddr {}

struct SymbolEntry {
    name: &'static str,
    addr: SymbolAddr,
}

include!(concat!(env!("OUT_DIR"), "/symbol_table.rs"));

/// Address of the runtime helper named `name`, or `None` when the runtime
/// defines no such symbol on this target.
#[must_use]
pub fn address(name: &str) -> Option<*const ()> {
    GENERATED
        .binary_search_by(|entry| entry.name.cmp(name))
        .ok()
        .map(|index| GENERATED[index].addr.0)
}

/// Every runtime helper name available on this target, sorted.
#[must_use]
pub fn names() -> impl ExactSizeIterator<Item = &'static str> {
    GENERATED.iter().map(|entry| entry.name)
}

/// Every runtime helper available on this target with its address, sorted by
/// name.
#[must_use]
pub fn entries() -> impl ExactSizeIterator<Item = (&'static str, *const ())> {
    GENERATED.iter().map(|entry| (entry.name, entry.addr.0))
}
