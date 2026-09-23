//! The ABI registry is the single declaration of every `gos_rt_*`
//! symbol's C signature: the LLVM backend builds its `declare`
//! statements from it and the Cranelift backend builds its call
//! signatures from it. This test pins that declaration to the Rust
//! `extern "C"` definitions it describes.
//!
//! A registry entry that disagrees with its definition is a wrong-ABI
//! call on both compiled tiers with no diagnostic: arguments land in
//! the wrong registers, or a two-word carrier return is read as one
//! word, and the program simply computes the wrong answer.

#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use gossamer_abi::{AbiType, REGISTRY};

/// Every `.rs` file under the runtime crate's `src/`, concatenated.
fn runtime_source() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../gossamer-runtime/src")
        .canonicalize()
        .expect("locate gossamer-runtime/src");
    let mut out = String::new();
    let mut stack = vec![root];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read runtime source directory") {
            let path = entry.expect("read runtime source entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    for path in files {
        out.push_str(&read(&path));
        out.push('\n');
    }
    out
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The ABI type a Rust parameter or return type crosses the C boundary
/// as, or `None` for a spelling this scan does not model.
fn abi_of_rust(ty: &str) -> Option<AbiType> {
    let ty = ty.split_whitespace().collect::<Vec<_>>().join(" ");
    let ty = ty.trim().trim_end_matches(',').trim();
    if ty.is_empty() || ty == "()" {
        return Some(AbiType::Void);
    }
    if ty.starts_with('*') {
        return Some(AbiType::Ptr);
    }
    Some(match ty {
        "i128" | "u128" => AbiType::I128,
        "i64" | "isize" | "usize" => AbiType::I64,
        "u64" => AbiType::U64,
        "i32" | "u32" | "c_int" | "char" => AbiType::I32,
        "i8" | "u8" | "bool" => AbiType::I8,
        "f32" | "f64" => AbiType::F64,
        _ => return None,
    })
}

/// `I64` and `U64` are one wire type and differ only in the Rust-level
/// signedness contract, so either spelling satisfies the other.
fn compatible(a: AbiType, b: AbiType) -> bool {
    a == b
        || matches!(
            (a, b),
            (AbiType::I64, AbiType::U64) | (AbiType::U64, AbiType::I64)
        )
}

/// Splits a parameter list on commas that are not inside a nested
/// generic, tuple, or function-pointer type.
fn split_params(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in list.chars() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// One parsed `extern "C"` definition: parameter ABI types and return
/// ABI type. `None` for a definition this scan cannot model, which is
/// reported rather than silently skipped.
type ParsedSig = Option<(Vec<AbiType>, AbiType)>;

fn parse_definitions(src: &str) -> BTreeMap<String, ParsedSig> {
    let mut out = BTreeMap::new();
    for marker in ["extern \"C\" fn ", "extern \"C-unwind\" fn "] {
        let mut cursor = 0;
        while let Some(idx) = src[cursor..].find(marker) {
            let start = cursor + idx + marker.len();
            cursor = start;
            let decl = &src[start..];
            let Some(open) = decl.find('(') else { break };
            let name = decl[..open].trim();
            if !name.starts_with("gos_rt_")
                || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            // Balanced scan to the closing paren of the parameter list,
            // then to the `{` that opens the body: a return type may
            // itself carry parentheses.
            let bytes = decl.as_bytes();
            let mut depth = 0i32;
            let mut i = open;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let params = &decl[open + 1..i];
            let tail = &decl[i + 1..];
            let Some(brace_at) = tail.find('{') else {
                break;
            };
            let ret = tail[..brace_at].trim().trim_start_matches("->").trim();
            out.insert(name.to_string(), parse_sig(params, ret));
        }
    }
    out
}

fn parse_sig(params: &str, ret: &str) -> ParsedSig {
    let mut tys = Vec::new();
    for param in split_params(params) {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        let (_, ty) = param.split_once(':')?;
        tys.push(abi_of_rust(ty)?);
    }
    Some((tys, abi_of_rust(ret)?))
}

/// Definitions this scan cannot model, each with the reason. A
/// diverging (`-> !`) function has no return type to compare, and the
/// callback registrar takes a function pointer, which is a `Ptr` the
/// spelling does not say.
const UNMODELLED: &[&str] = &[
    "gos_rt_panic_oob",
    "gos_rt_panic_vec_index",
    "gos_rt_exit",
    "gos_rt_process_abort",
    "gos_rt_callback_register",
];

#[test]
fn every_registry_entry_matches_its_rust_definition() {
    let defs = parse_definitions(&runtime_source());
    assert!(
        defs.len() > 1500,
        "only {} `extern \"C\"` definitions found - the scan is not reading the runtime",
        defs.len(),
    );

    let mut problems = Vec::new();
    for entry in REGISTRY {
        let Some(parsed) = defs.get(entry.name) else {
            // A symbol the registry declares but the runtime crate does
            // not define lives in another crate (the HTTP/3 and binding
            // crates each carry some), and is out of this scan's reach.
            continue;
        };
        let Some((params, ret)) = parsed else {
            assert!(
                UNMODELLED.contains(&entry.name),
                "{}: its Rust definition uses a spelling this scan does not model; \
                 extend `abi_of_rust` or add it to `UNMODELLED` with the reason",
                entry.name,
            );
            continue;
        };
        if params.len() != entry.sig.params.len() {
            problems.push(format!(
                "{}: registry declares {} parameter(s), the Rust definition takes {}",
                entry.name,
                entry.sig.params.len(),
                params.len(),
            ));
            continue;
        }
        for (i, (rust, declared)) in params.iter().zip(entry.sig.params).enumerate() {
            if !compatible(*rust, *declared) {
                problems.push(format!(
                    "{}: parameter {i} is {rust:?} in Rust, {declared:?} in the registry",
                    entry.name,
                ));
            }
        }
        if !compatible(*ret, entry.sig.ret) {
            problems.push(format!(
                "{}: returns {ret:?} in Rust, {:?} in the registry",
                entry.name, entry.sig.ret,
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "{} registry entr(ies) disagree with their Rust definition. Every compiled \
         call to one of these is a wrong-ABI call with no diagnostic:\n  {}",
        problems.len(),
        problems.join("\n  "),
    );
}

#[test]
fn unmodelled_entries_are_all_still_present() {
    let defs = parse_definitions(&runtime_source());
    for name in UNMODELLED {
        assert!(
            defs.contains_key(*name),
            "{name} is listed as unmodelled but the runtime no longer defines it - \
             drop the row",
        );
    }
}

/// Registry symbols whose `c_char` pointer answer the caller does not
/// take a fresh reference to, each with the reason.
const UNOWNED_STRING_RETURNS: &[(&str, &str)] = &[
    (
        "gos_rt_flag_cell_load_str",
        "reads the String a flag cell holds",
    ),
    (
        "gos_rt_os_program_name",
        "answers the process-lifetime argv[0] copy",
    ),
    (
        "gos_rt_str_append_bytes",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_push_substring",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_append_f64",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_append_bool",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_append_i64",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_append_u64",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_clear",
        "answers its receiver, emptied in place when held alone",
    ),
    (
        "gos_rt_str_clone",
        "answers its argument with the retain already taken",
    ),
    (
        "gos_rt_str_concat_drop_a",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_push_byte",
        "answers its accumulator, grown in place",
    ),
    (
        "gos_rt_str_truncate",
        "answers its receiver, shortened in place when held alone",
    ),
    (
        "gos_rt_str_push_char",
        "answers its accumulator, grown in place",
    ),
];

/// The Rust return type spelled by each `gos_rt_*` definition.
fn return_spellings(src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for marker in ["extern \"C\" fn ", "extern \"C-unwind\" fn "] {
        let mut cursor = 0;
        while let Some(idx) = src[cursor..].find(marker) {
            let start = cursor + idx + marker.len();
            cursor = start;
            let decl = &src[start..];
            let Some(open) = decl.find('(') else { break };
            let name = decl[..open].trim();
            if !name.starts_with("gos_rt_") {
                continue;
            }
            let mut depth = 0i32;
            let Some(close) = decl[open..].char_indices().find_map(|(i, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(open + i)
            }) else {
                break;
            };
            let tail = &decl[close + 1..];
            let Some(brace_at) = tail.find('{') else {
                break;
            };
            let ret = tail[..brace_at].trim().trim_start_matches("->").trim();
            out.insert(
                name.to_string(),
                ret.split_whitespace().collect::<Vec<_>>().join(" "),
            );
        }
    }
    out
}

/// A shim answering a single `c_char` pointer answers a `String`, and the
/// drop pass releases a call's result only when the registry says the
/// caller owns it. An undeclared minting shim leaks one String per call
/// on the compiled tiers; a borrowed answer declared owned is freed
/// under its holder.
#[test]
fn every_string_return_declares_its_ownership() {
    let rets = return_spellings(&runtime_source());
    let mut problems = Vec::new();
    for entry in REGISTRY {
        let Some(ret) = rets.get(entry.name) else {
            continue;
        };
        let answers_string = ret.ends_with("c_char") && ret.matches('*').count() == 1;
        let unowned = UNOWNED_STRING_RETURNS
            .iter()
            .any(|(name, _)| *name == entry.name);
        if answers_string && !unowned && !entry.mints_string {
            problems.push(format!(
                "{}: answers `{ret}` but is declared with `rt!`; declare it `rt_str!` if the \
                 caller owns the String, or list it in UNOWNED_STRING_RETURNS with the reason",
                entry.name,
            ));
        }
        if unowned && entry.mints_string {
            problems.push(format!(
                "{}: listed in UNOWNED_STRING_RETURNS but declared `rt_str!`",
                entry.name,
            ));
        }
        if entry.mints_string && !answers_string {
            problems.push(format!(
                "{}: declared `rt_str!` but answers `{ret}`, not a String",
                entry.name,
            ));
        }
    }
    for (name, _) in UNOWNED_STRING_RETURNS {
        if !rets.contains_key(*name) {
            problems.push(format!(
                "{name}: listed in UNOWNED_STRING_RETURNS but not defined"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
