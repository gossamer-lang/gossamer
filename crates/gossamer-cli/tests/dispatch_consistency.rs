//! Cross-table consistency between the ABI registry and the runtime.
//!
//! The registry names every `gos_rt_*` symbol the compiled tiers may call,
//! and the runtime's generated symbol table names every one it defines; the
//! two must agree in both directions. The signature checks below parse the
//! runtime source, since a registry row whose parameter count or ABI class
//! drifts from the definition is miscompiled rather than rejected.
//!
//! Runtime signatures are checked at the C-ABI class level: raw pointers and
//! callback function pointers are `Ptr`, Rust `bool` is `I8`, 32-bit C scalar
//! types are `I32`, and `usize`/`u64` are compared as the existing 64-bit ABI
//! class. This catches width/return drift without treating signedness aliases
//! as separate machine ABIs.

#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use gossamer_abi::AbiType;

/// Yields every Rust source file under
/// `gossamer-runtime/src/{c_abi,c_abi/*,*}.rs` that may contain
/// `gos_rt_*` exports. The `c_abi.rs` split into a directory of
/// per-domain submodules means a flat list is no longer enough.
fn runtime_source_files() -> Vec<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let runtime_src = PathBuf::from(&manifest_dir)
        .join("..")
        .join("gossamer-runtime")
        .join("src");
    let candidate_files = [
        "c_abi.rs",
        "gc.rs",
        "preempt.rs",
        "lib.rs",
        "safe_env.rs",
        "race.rs",
    ];
    let mut out: Vec<PathBuf> = candidate_files
        .iter()
        .map(|f| runtime_src.join(f))
        .filter(|p| p.is_file())
        .collect();
    let c_abi_dir = runtime_src.join("c_abi");
    if c_abi_dir.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&c_abi_dir)
            .expect("read c_abi dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
            .collect();
        entries.sort();
        out.extend(entries);
    }
    out
}

/// Every registry entry must be a symbol the runtime exports on this target,
/// and every exported symbol must be in the registry. A name in one and not
/// the other is the shape that lowers to a call nothing resolves.
#[test]
fn registry_and_generated_symbol_table_agree() {
    let registry: BTreeSet<&str> = gossamer_abi::REGISTRY.iter().map(|e| e.name).collect();
    let table: BTreeSet<&str> = gossamer_runtime::symbols::names().collect();
    let missing_from_table: Vec<&&str> = registry.difference(&table).collect();
    let missing_from_registry: Vec<&&str> = table.difference(&registry).collect();
    assert!(
        missing_from_table.is_empty(),
        "in the ABI registry but not exported by the runtime: {missing_from_table:?}"
    );
    assert!(
        missing_from_registry.is_empty(),
        "exported by the runtime but absent from the ABI registry: {missing_from_registry:?}"
    );
}

/// The ABI registry must have no duplicate entries and must be
/// sorted (enforced by the gossamer-abi unit tests, but also
/// validated here for belt-and-suspenders).
#[test]
fn registry_sorted_and_unique() {
    let names: Vec<&str> = gossamer_abi::REGISTRY.iter().map(|e| e.name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "REGISTRY is not sorted alphabetically");

    let mut deduped = names.clone();
    deduped.dedup();
    assert_eq!(
        names.len(),
        deduped.len(),
        "REGISTRY contains duplicate entries"
    );
}

/// Every `declare` produced by the registry round-trips correctly
/// through LLVM IR syntax: starts with `declare ` and includes
/// the symbol name.
#[test]
fn registry_llvm_declares_are_well_formed() {
    for entry in gossamer_abi::REGISTRY {
        let decl = entry.llvm_declare();
        assert!(
            decl.starts_with("declare "),
            "bad declare for {}: {decl}",
            entry.name
        );
        assert!(
            decl.contains(&format!("@{}", entry.name)),
            "declare missing symbol name for {}: {decl}",
            entry.name
        );
    }
}

/// Extracts the number of parameters from a Rust function signature
/// by counting top-level commas in the argument list. Handles nested
/// angle brackets, parentheses (function pointer params), and trailing
/// commas (idiomatic in multi-line Rust signatures).
fn count_params_in_sig(params_text: &str) -> usize {
    let trimmed = params_text.trim().trim_end_matches(',').trim();
    if trimmed.is_empty() {
        return 0;
    }
    let mut depth = 0i32;
    let mut commas = 0usize;
    for c in trimmed.chars() {
        match c {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
    }
    commas + 1
}

/// Parses `gos_rt_*` function param counts from the given Rust source
/// file. Returns a map of `function_name → param_count`. Only counts
/// the declared parameters (ignores the return type).
fn parse_param_counts(source: &str) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    let mut chars = source.char_indices().peekable();

    while let Some((i, _)) = chars.next() {
        // Find `fn gos_rt_` prefix anywhere in the source.
        let rest = &source[i..];
        if !rest.starts_with("fn gos_rt_") {
            continue;
        }
        // Scan forward to find the function name (up to `(`).
        let after_fn = &rest["fn ".len()..];
        let Some(paren) = after_fn.find('(') else {
            continue;
        };
        let name = after_fn[..paren].trim().to_string();
        if !name.starts_with("gos_rt_") {
            continue;
        }
        // Scan for the matching close paren.
        let params_start = "fn ".len() + paren + 1;
        if params_start >= rest.len() {
            continue;
        }
        let mut depth = 1i32;
        let mut params_end = params_start;
        for (j, c) in rest[params_start..].char_indices() {
            match c {
                '(' | '<' | '[' => depth += 1,
                ')' | '>' | ']' => {
                    depth -= 1;
                    if depth == 0 {
                        params_end = params_start + j;
                        break;
                    }
                }
                _ => {}
            }
        }
        let params_text = &rest[params_start..params_end];
        // Filter out `self` and `&self` - not real extern params.
        let params_text = params_text
            .replace("&mut self,", "")
            .replace("&self,", "")
            .replace("mut self,", "")
            .replace("self,", "")
            .replace("&mut self", "")
            .replace("&self", "")
            .replace("mut self", "")
            .replace("self", "");
        let count = count_params_in_sig(&params_text);
        out.insert(name, count);
        // Skip past this function to avoid re-matching.
        for _ in 0..(params_start + params_end) {
            chars.next();
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRuntimeSig {
    params: Vec<AbiType>,
    ret: AbiType,
}

fn split_top_level_params(params_text: &str) -> Vec<&str> {
    let trimmed = params_text.trim().trim_end_matches(',').trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut start = 0usize;
    let mut paren_depth = 0i32;
    let mut angle_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut out = Vec::new();
    for (idx, c) in trimmed.char_indices() {
        match c {
            '(' => paren_depth += 1,
            ')' => paren_depth -= 1,
            '<' => angle_depth += 1,
            '>' if angle_depth > 0 => angle_depth -= 1,
            '[' => bracket_depth += 1,
            ']' => bracket_depth -= 1,
            ',' if paren_depth == 0 && angle_depth == 0 && bracket_depth == 0 => {
                let part = trimmed[start..idx].trim();
                if !part.is_empty() {
                    out.push(part);
                }
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    let tail = trimmed[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn param_type_text(param: &str) -> Option<&str> {
    let (_, ty) = param.split_once(':')?;
    Some(ty.trim())
}

fn abi_class_for_rust_type(ty: &str, is_return: bool) -> Option<AbiType> {
    let ty = ty.trim();
    if ty.is_empty() || ty == "()" {
        return Some(AbiType::Void);
    }
    if is_return && ty == "!" {
        return Some(AbiType::Void);
    }
    if ty.starts_with('*') || ty.starts_with('&') || ty.starts_with("extern \"C\" fn") {
        return Some(AbiType::Ptr);
    }
    match ty {
        "bool" | "u8" | "i8" | "c_char" | "std::os::raw::c_char" => Some(AbiType::I8),
        "i32" | "u32" => Some(AbiType::I32),
        "i64" | "u64" | "usize" | "isize" => Some(AbiType::I64),
        "i128" => Some(AbiType::I128),
        "f64" => Some(AbiType::F64),
        _ => None,
    }
}

fn parse_return_type(rest_after_params: &str) -> Option<AbiType> {
    let trimmed = rest_after_params.trim_start();
    if !trimmed.starts_with("->") {
        return Some(AbiType::Void);
    }
    let ret = trimmed["->".len()..]
        .trim_start()
        .chars()
        .take_while(|&c| c != '{' && c != ';' && c != '\n')
        .collect::<String>();
    abi_class_for_rust_type(ret.trim(), true)
}

/// Parses `gos_rt_*` function ABI classes from visible Rust extern function
/// declarations. Macro-generated exports are intentionally absent here and are
/// still covered by the export-existence test.
fn parse_runtime_sigs(source: &str) -> std::collections::HashMap<String, ParsedRuntimeSig> {
    let mut out = std::collections::HashMap::new();
    let mut cursor = 0usize;
    while let Some(rel) = source[cursor..].find("fn gos_rt_") {
        let i = cursor + rel;
        let rest = &source[i..];
        let after_fn = &rest["fn ".len()..];
        let Some(paren) = after_fn.find('(') else {
            cursor = i + "fn gos_rt_".len();
            continue;
        };
        let name = after_fn[..paren].trim().to_string();
        let params_start = "fn ".len() + paren + 1;
        let mut paren_depth = 1i32;
        let mut angle_depth = 0i32;
        let mut bracket_depth = 0i32;
        let mut params_end = params_start;
        for (j, c) in rest[params_start..].char_indices() {
            match c {
                '(' => paren_depth += 1,
                ')' => {
                    paren_depth -= 1;
                    if paren_depth == 0 && angle_depth == 0 && bracket_depth == 0 {
                        params_end = params_start + j;
                        break;
                    }
                }
                '<' => angle_depth += 1,
                '>' if angle_depth > 0 => angle_depth -= 1,
                '[' => bracket_depth += 1,
                ']' => bracket_depth -= 1,
                _ => {}
            }
        }
        if paren_depth != 0 || angle_depth != 0 || bracket_depth != 0 {
            cursor = i + "fn gos_rt_".len();
            continue;
        }
        let params_text = &rest[params_start..params_end];
        let mut params = Vec::new();
        let mut supported = true;
        for param in split_top_level_params(params_text) {
            let Some(ty) = param_type_text(param) else {
                supported = false;
                break;
            };
            let Some(abi) = abi_class_for_rust_type(ty, false) else {
                supported = false;
                break;
            };
            params.push(abi);
        }
        let after_params = &rest[params_end + 1..];
        let return_abi = if let Some(parsed_return) = parse_return_type(after_params) {
            parsed_return
        } else {
            supported = false;
            AbiType::Void
        };
        if supported {
            out.insert(
                name,
                ParsedRuntimeSig {
                    params,
                    ret: return_abi,
                },
            );
        }
        cursor = i + params_end + 1;
    }
    out
}

/// Every REGISTRY entry's `params.len()` must match the number of
/// parameters declared in the corresponding `pub extern "C" fn` in
/// the runtime source. Catches param-count mismatches that would
/// silently produce wrong-code or segfaults at runtime.
#[test]
fn registry_param_counts_match_runtime_exports() {
    let mut all_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for path in runtime_source_files() {
        if let Ok(source) = std::fs::read_to_string(&path) {
            all_counts.extend(parse_param_counts(&source));
        }
    }
    assert!(
        all_counts.len() > 50,
        "param-count parser found only {} functions - likely broken",
        all_counts.len()
    );

    let mut mismatches: Vec<String> = Vec::new();
    for entry in gossamer_abi::REGISTRY {
        let Some(&actual) = all_counts.get(entry.name) else {
            // Export-existence is checked by all_registry_entries_exported_by_runtime.
            continue;
        };
        let expected = entry.sig.params.len();
        if actual != expected {
            mismatches.push(format!(
                "{}: REGISTRY has {} param(s), c_abi.rs has {}",
                entry.name, expected, actual
            ));
        }
    }
    mismatches.sort();
    assert!(
        mismatches.is_empty(),
        "{} param-count mismatch{}:\n  {}",
        mismatches.len(),
        if mismatches.len() == 1 { "" } else { "es" },
        mismatches.join("\n  ")
    );
}

/// Every visible runtime `pub extern "C" fn gos_rt_*` entry in the ABI
/// registry must match the Rust implementation's C-ABI class signature.
/// Macro-generated exports are validated for existence but do not have a
/// visible expanded signature in the scanned source.
#[test]
fn registry_abi_classes_match_runtime_exports() {
    let mut all_sigs: std::collections::HashMap<String, ParsedRuntimeSig> =
        std::collections::HashMap::new();
    for path in runtime_source_files() {
        if let Ok(source) = std::fs::read_to_string(&path) {
            all_sigs.extend(parse_runtime_sigs(&source));
        }
    }
    assert!(
        all_sigs.len() > 50,
        "signature parser found only {} functions - likely broken",
        all_sigs.len()
    );

    let mut mismatches: Vec<String> = Vec::new();
    let mut covered = 0usize;
    for entry in gossamer_abi::REGISTRY {
        let Some(actual) = all_sigs.get(entry.name) else {
            continue;
        };
        covered += 1;
        if actual.params != entry.sig.params || actual.ret != entry.sig.ret {
            mismatches.push(format!(
                "{}: REGISTRY has ({:?}) -> {:?}, runtime has ({:?}) -> {:?}",
                entry.name, entry.sig.params, entry.sig.ret, actual.params, actual.ret
            ));
        }
    }
    assert!(
        covered > 200,
        "signature audit covered only {covered} registry entries; parser likely missed too much"
    );
    mismatches.sort();
    assert!(
        mismatches.is_empty(),
        "{} ABI-class mismatch{}:\n  {}",
        mismatches.len(),
        if mismatches.len() == 1 { "" } else { "es" },
        mismatches.join("\n  ")
    );
}

/// Sanity-check the tier field counts: at least 30 Both-tier, 100
/// Cranelift-tier, and 1 Llvm-tier entry. Catches trivial mistakes
/// (e.g. all entries set to the same tier after a bulk edit).
#[test]
fn tier_field_counts_are_plausible() {
    use gossamer_abi::Tier;
    let both = gossamer_abi::REGISTRY
        .iter()
        .filter(|e| e.tier == Tier::Both)
        .count();
    let cl = gossamer_abi::REGISTRY
        .iter()
        .filter(|e| e.tier == Tier::Cranelift)
        .count();
    let ll = gossamer_abi::REGISTRY
        .iter()
        .filter(|e| e.tier == Tier::Llvm)
        .count();
    assert!(
        both >= 30,
        "expected >=30 Both-tier entries, got {both}; tier classifications may be wrong"
    );
    assert!(
        cl >= 100,
        "expected >=100 Cranelift-tier entries, got {cl}; tier classifications may be wrong"
    );
    assert!(
        ll >= 1,
        "expected >=1 Llvm-tier entries, got {ll}; gos_rt_race_access should be Llvm"
    );
}
