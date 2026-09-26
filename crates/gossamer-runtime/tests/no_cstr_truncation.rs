//! Structural invariant: the C-ABI shims read Gossamer `String` arguments
//! through their length header, never with `CStr::from_ptr`.
//!
//! A Gossamer string carries an explicit byte length and may contain interior
//! NUL bytes. `CStr::from_ptr` scans for the first NUL, so a shim that uses it
//! on a language `String` silently truncates the value and makes the compiled
//! tiers disagree with the bytecode VM. The length-carrying readers live in
//! `c_abi::string` (`gos_str_arg_bytes` / `_text` / `_lossy` / `_string` /
//! `_len`).
//!
//! A handful of parameters really are host C strings with no length header.
//! Those sites carry a `HOST-CSTRING:` comment naming the owner and are listed
//! in [`ALLOWLIST`] below. `#[cfg(test)]` modules are exempt: their pointers
//! come from fixtures the test itself built.

#![allow(missing_docs)]

use std::path::{Path, PathBuf};

/// Sites that legitimately read a host-owned C string. Each entry is
/// `(file, enclosing item, reason)`.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "binding_callback.rs",
        "invoke_closure",
        "a native Rust binding passes a callback's string argument as a C string it owns",
    ),
    (
        "args.rs",
        "gos_rt_set_args",
        "libc owns argv; the entries are copied into tagged Gossamer strings here",
    ),
    (
        "args.rs",
        "set_program_name",
        "the interpreter passes a Rust CString holding the script path",
    ),
    (
        "string.rs",
        "c_str_len",
        "the strlen fallback that typed_str_len uses for header-less pointers",
    ),
    (
        "string.rs",
        "parse_argv_flag_values",
        "the flag parser walks libc's argv directly",
    ),
    (
        "vec.rs",
        "gos_rt_binding_variant_to_result",
        "a native Rust binding publishes its string payload as a plain C string",
    ),
    (
        "binding_wire.rs",
        "wire_fields_to_slots",
        "a native Rust binding publishes a struct's string field as a plain C string",
    ),
    (
        "binding_wire.rs",
        "dyn_from_wire_field",
        "a native Rust binding publishes a dynamic value's text as a plain C string",
    ),
    (
        "binding_wire.rs",
        "dyn_from_wire_variant",
        "a native Rust binding arena-allocates an arm name as a plain C string",
    ),
];

#[test]
fn c_abi_shims_read_gossamer_strings_through_their_length_header() {
    let c_abi = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/c_abi");
    let mut offenders: Vec<String> = Vec::new();
    let mut used_allowlist: Vec<(&str, &str)> = Vec::new();

    walk_rs(&c_abi, &mut |path, body| {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        for site in scan(body) {
            let allowed = ALLOWLIST
                .iter()
                .find(|(f, item, _)| *f == file && *item == site.item);
            match allowed {
                Some((f, item, _)) => used_allowlist.push((f, item)),
                None => offenders.push(format!(
                    "{}:{}: `CStr::from_ptr` in `{}`",
                    path.display(),
                    site.line,
                    site.item,
                )),
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "C-ABI shims must read Gossamer `String` arguments through \
         `c_abi::string::gos_str_arg_*`, which honours the length header so a \
         string containing an interior NUL is not truncated. If a parameter is \
         genuinely a host-owned C string, add a `HOST-CSTRING:` comment naming \
         the owner and an ALLOWLIST entry in this test. Offenders:\n{}",
        offenders.join("\n"),
    );

    let stale: Vec<String> = ALLOWLIST
        .iter()
        .filter(|(f, item, _)| !used_allowlist.contains(&(f, item)))
        .map(|(f, item, _)| format!("{f}: {item}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these ALLOWLIST entries no longer match any `CStr::from_ptr` site and \
         should be removed:\n{}",
        stale.join("\n"),
    );
}

#[test]
fn allowlisted_sites_document_their_host_owner() {
    let c_abi = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/c_abi");
    let mut missing: Vec<String> = Vec::new();
    for (file, item, _) in ALLOWLIST {
        let body = std::fs::read_to_string(c_abi.join(file))
            .unwrap_or_else(|e| panic!("reading {file}: {e}"));
        let documented = scan(&body)
            .iter()
            .filter(|site| site.item == *item)
            .all(|site| {
                // The marker documents the enclosing item, so search from the
                // call back through the item's signature and its doc comment
                // rather than a fixed number of lines.
                let preceding: Vec<&str> = body.lines().take(site.line).collect();
                let mut span_start = 0;
                for (index, line) in preceding.iter().enumerate().rev() {
                    let text = line.trim_start();
                    if text.starts_with("fn ")
                        || text.starts_with("pub fn ")
                        || text.starts_with("pub(crate) fn ")
                        || text.starts_with("unsafe fn ")
                        || text.starts_with("pub unsafe fn ")
                        || text.starts_with("pub(crate) unsafe fn ")
                    {
                        span_start = index;
                        break;
                    }
                }
                // Walk back over the item's contiguous doc comment.
                while span_start > 0 {
                    let text = preceding[span_start - 1].trim_start();
                    if text.starts_with("///") || text.starts_with("#[") {
                        span_start -= 1;
                    } else {
                        break;
                    }
                }
                preceding[span_start..]
                    .iter()
                    .any(|l| l.contains("HOST-CSTRING:"))
            });
        if !documented {
            missing.push(format!("{file}: {item}"));
        }
    }
    assert!(
        missing.is_empty(),
        "every allowlisted `CStr::from_ptr` site needs a nearby `HOST-CSTRING:` \
         comment naming the owner of the pointer:\n{}",
        missing.join("\n"),
    );
}

struct Site {
    line: usize,
    item: String,
}

/// Finds `CStr::from_ptr` uses outside `#[cfg(test)]` modules, tagged with the
/// name of the enclosing `fn`.
fn scan(body: &str) -> Vec<Site> {
    let mut sites = Vec::new();
    let mut item = String::new();
    let mut brace_depth: i32 = 0;
    let mut cfg_test_depth: Option<i32> = None;
    let mut pending_cfg_test = false;

    for (i, line) in body.lines().enumerate() {
        let trimmed = line.trim_start();
        let is_comment =
            trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*");

        if !is_comment && let Some(name) = enclosing_fn_name(line) {
            item = name;
        }

        if !is_comment
            && cfg_test_depth.is_none()
            && !trimmed.starts_with("//")
            && trimmed.contains("CStr::from_ptr")
        {
            sites.push(Site {
                line: i + 1,
                item: item.clone(),
            });
        }

        if trimmed.starts_with("#[cfg(test)]") {
            pending_cfg_test = true;
        }
        if pending_cfg_test && trimmed.starts_with("mod ") {
            cfg_test_depth = Some(brace_depth + 1);
            pending_cfg_test = false;
        }
        for ch in line.chars() {
            if ch == '{' {
                brace_depth += 1;
            } else if ch == '}' {
                brace_depth -= 1;
                if cfg_test_depth.is_some_and(|d| brace_depth < d) {
                    cfg_test_depth = None;
                }
            }
        }
    }
    sites
}

/// Name of the function declared on `line`, if any.
fn enclosing_fn_name(line: &str) -> Option<String> {
    let idx = line.find("fn ")?;
    let preceded_by_ident = line[..idx]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_');
    if preceded_by_ident {
        return None;
    }
    let rest = &line[idx + 3..];
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn walk_rs(root: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                visit(&path, &body);
            }
        }
    }
}
