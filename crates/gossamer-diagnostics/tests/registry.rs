//! CI-grade invariants on the centralized error-code registry.

use std::collections::HashSet;

use gossamer_diagnostics::{REGISTRY, codes, explain};

/// Codes the compiler currently emits. Keep in sync with the matches
/// in `gossamer-parse/src/diagnostic.rs`, `gossamer-resolve/src/diagnostic.rs`,
/// `gossamer-types/src/error.rs`, `gossamer-types/src/exhaustiveness.rs`,
/// `gossamer-interp/src/value.rs`, and `gossamer-lint/src/lib.rs::lint_code`.
const EMITTED_CODES: &[&str] = &[
    // Parser (gossamer-parse/src/diagnostic.rs).
    "GP0001", "GP0002", "GP0003", "GP0004", "GP0005", "GP0006", "GP0007", "GP0008", "GP0009",
    "GP0010", "GP0011", "GP0012", "GP0013", "GP0014", "GP0015", "GP0016", "GP0017", "GP0018",
    "GP0019", "GP0020", "GP0021", "GP0022", "GP0023", "GP0024", "GP0026", "GP0027", "GP0029",
    "GP0030", "GP0031", "GP0032", "GP0033", "GP0035", "GP0036", "GP0037", "GP0038", "GP0039",
    "GP0040", "GP0041", "GP0042", "GP0043", "GP0044", "GP0045", "GP0046", "GP0047", "GP0048",
    "GP0049", "GP0050", "GP0051", "GP0052", "GP0053", "GP0054", "GP0055",
    "GP0056", // Resolver (gossamer-resolve/src/diagnostic.rs).
    "GR0001", "GR0002", "GR0003", "GR0004", "GR0005", "GR0006", "GR0007", "GR0008", "GR0009",
    "GR0010", "GR0011", "GR0012", "GR0013", "GR0017", "GR0019", "GR0020", "GR0021",
    "GR0014", // Type checker (gossamer-types/src/error.rs).
    "GT0001", "GT0002", "GT0003", "GT0004", "GT0005", "GT0006", "GT0007", "GT0008", "GT0009",
    "GT0010", "GT0011", "GT0012", "GT0013", "GT0014", "GT0015", "GT0016", "GT0017", "GT0018",
    "GT0019", "GT0020", "GT0021", "GT0022", "GT0023", "GT0024", "GT0025", "GT0027", "GT0028",
    "GT0029", "GT0030", "GT0031", "GT0032", "GT0033", "GT0034", "GT0035", "GT0036", "GT0037",
    "GT0041", "GT0042", "GT0043", "GT0044", "GT0045", "GT0046", "GT0047", "GT0048", "GT0049",
    "GT0050", "GT0051", "GT0052", "GT0053", "GT0054", "GT0056", "GT0057", "GT0058", "GT0059",
    "GT0060", "GT0061", "GT0062", "GT0063", "GT0064", "GT0065", "GT0066", "GT0067", "GT0068",
    "GT0069", "GT0070", "GT0071", "GT0072", "GT0073", "GT0075", "GT0078", "GT0079", "GT0080",
    "GT0081", "GT0082", "GT0083", "GT0084", "GT0085", "GT0086",
    "GT0055", // Match exhaustiveness (gossamer-types/src/exhaustiveness.rs).
    "GM0001", "GM0002", // Arena-escape safety (gossamer-types/src/arena_escape.rs).
    "GM0003", // Runtime (gossamer-interp/src/value.rs).
    "GX0001", "GX0002", "GX0003", "GX0004", "GX0005", "GX0006", "GX0007", "GX0008", "GX0009",
    "GX0010", "GX0011", "GX0012",
    // Lint registry (gossamer-lint/src/lib.rs::lint_code).
    "GL0001", "GL0002", "GL0003", "GL0004", "GL0005", "GL0006", "GL0007", "GL0008", "GL0009",
    "GL0010", "GL0011", "GL0012", "GL0013", "GL0014", "GL0015", "GL0016", "GL0017", "GL0018",
    "GL0019", "GL0021", "GL0022", "GL0023", "GL0024", "GL0025", "GL0026", "GL0027", "GL0028",
    "GL0029", "GL0030", "GL0031", "GL0032", "GL0033", "GL0034", "GL0035", "GL0036", "GL0037",
    "GL0038", "GL0039", "GL0040", "GL0041", "GL0042", "GL0043", "GL0044", "GL0045", "GL0046",
    "GL0047", "GL0048", "GL0049", "GL0050", "GL0051", "GL0052", "GL0055", "GL0056",
];

#[test]
fn every_emitted_code_has_a_registry_entry() {
    let known: HashSet<&'static str> = codes().collect();
    let mut missing: Vec<&'static str> = EMITTED_CODES
        .iter()
        .copied()
        .filter(|c| !known.contains(c))
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "REGISTRY is missing emitted codes: {missing:?}"
    );
}

#[test]
fn registry_has_no_duplicate_codes() {
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut duplicates: Vec<&'static str> = Vec::new();
    for code in codes() {
        if !seen.insert(code) {
            duplicates.push(code);
        }
    }
    assert!(
        duplicates.is_empty(),
        "duplicate codes in REGISTRY: {duplicates:?}"
    );
}

#[test]
fn registry_is_sorted_alphabetically() {
    let codes: Vec<&'static str> = codes().collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    assert_eq!(
        codes, sorted,
        "REGISTRY must be sorted alphabetically by code"
    );
}

#[test]
fn registry_explanations_are_non_empty() {
    let mut empties: Vec<&'static str> = Vec::new();
    for (code, text) in REGISTRY {
        if text.trim().is_empty() {
            empties.push(*code);
        }
    }
    assert!(
        empties.is_empty(),
        "REGISTRY entries with empty explanation: {empties:?}"
    );
}

#[test]
fn explain_lookup_round_trips_every_entry() {
    for (code, text) in REGISTRY {
        let looked_up = explain(code).expect("registered code must look up");
        assert_eq!(
            looked_up, *text,
            "explain({code}) returned a different body than REGISTRY"
        );
    }
}

#[test]
fn explain_returns_none_for_unknown_code() {
    assert!(explain("GZ9999").is_none());
    assert!(explain("").is_none());
}
