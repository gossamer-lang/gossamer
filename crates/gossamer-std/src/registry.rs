//! Registry of stdlib modules and the items each module exports.
//! Until `gossamer-std/std/*.gos` source files can be compiled by the
//! Gossamer toolchain (which depends on the bytecode VM gaining ADT
//! support, ), the stdlib lives here as a manifest backed by
//! Rust-side runtime helpers. The interpreter and bytecode VM consult
//! this table to install built-in functions; the type checker can use
//! it to validate that imported names exist.

#![forbid(unsafe_code)]

/// Top-level stdlib module description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdModule {
    /// Path Gossamer source code uses (e.g. `"std::fmt"`, `"fmt"`).
    pub path: &'static str,
    /// Brief one-line summary used in `gos doc` output.
    pub summary: &'static str,
    /// Items exported from this module.
    pub items: &'static [StdItem],
}

/// Single item (function, type, constant) exported from a stdlib
/// module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdItem {
    /// Item name as imported.
    pub name: &'static str,
    /// Kind tag used by the type checker / `gos doc`.
    pub kind: StdItemKind,
    /// One-line documentation.
    pub doc: &'static str,
}

impl StdItem {
    /// The item spelled the way source calls it: a macro carries its `!`,
    /// so a listing names the form a reader can type.
    #[must_use]
    pub fn call_name(&self) -> String {
        call_name(self.name, self.kind)
    }
}

/// The call form of `name` for an item of `kind`.
#[must_use]
pub fn call_name(name: &str, _kind: StdItemKind) -> String {
    name.to_string()
}

/// Fully-qualified item metadata derived from the stdlib manifest.
///
/// This is the item-level contract used by docs/audit tooling. The
/// manifest remains the authoring source (`StdModule { items: ... }`);
/// this record flattens it into a stable per-item view so drift tests do
/// not need to reimplement path/status joining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdItemRecord {
    /// Canonical item path, e.g. `std::encoding::json::parse`.
    pub path: String,
    /// Module path, e.g. `std::encoding::json`.
    pub module_path: &'static str,
    /// Item name inside the module.
    pub name: &'static str,
    /// Kind tag used by docs and resolver audits.
    pub kind: StdItemKind,
    /// Lifecycle status from this exact canonical item path. Module lifecycle
    /// entries never implicitly promote their exports.
    pub status: crate::manifest::feature_status::Status,
    /// Item-level status, tier, target, documentation, test, and limitation
    /// evidence. This is deliberately materialized per exported item rather
    /// than inferred by consumers from a module roll-up.
    pub evidence: crate::manifest::feature_status::ItemEvidence,
    /// Module summary for roll-up reports.
    pub module_summary: &'static str,
    /// One-line item documentation.
    pub doc: &'static str,
}

/// Classification for a stdlib item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdItemKind {
    /// Plain function.
    Function,
    /// User-facing type (struct or enum).
    Type,
    /// Trait declaration.
    Trait,
    /// Compiler-known call the parser expands where it is written.
    Builtin,
    /// Module-level constant.
    Const,
}

/// Returns every registered stdlib module.
#[must_use]
pub fn modules() -> &'static [StdModule] {
    crate::manifest::ALL_MODULES
}

/// Looks up a module by canonical path.
#[must_use]
pub fn module(path: &str) -> Option<&'static StdModule> {
    modules().iter().find(|m| m.path == path)
}

/// Resolves an item by its `module::name` canonical spelling.
#[must_use]
pub fn item(qualified: &str) -> Option<(&'static StdModule, &'static StdItem)> {
    let mut parts: Vec<&str> = qualified.split("::").collect();
    let last = parts.pop()?;
    let path = parts.join("::");
    let module = module(&path)?;
    module
        .items
        .iter()
        .find(|i| i.name == last)
        .map(|i| (module, i))
}

/// Returns one flattened metadata record for every manifest item.
///
/// The vector is sorted by canonical item path and contains no aliases.
/// Deprecated convenience aliases live in resolver/interpreter tables and
/// must map back to one of these canonical records in drift tests.
#[must_use]
pub fn item_records() -> Vec<StdItemRecord> {
    let mut out = Vec::new();
    for module in modules() {
        for item in module.items {
            let path = format!("{}::{}", module.path, item.name);
            let status = crate::manifest::feature_status::item_status(&path);
            out.push(StdItemRecord {
                evidence: crate::manifest::feature_status::item_evidence(&path, status),
                path,
                module_path: module.path,
                name: item.name,
                kind: item.kind,
                status,
                module_summary: module.summary,
                doc: item.doc,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_records_are_sorted_unique_and_complete() {
        let records = item_records();
        let manifest_count: usize = modules().iter().map(|module| module.items.len()).sum();
        assert_eq!(
            records.len(),
            manifest_count,
            "flattened item metadata lost or duplicated manifest entries"
        );
        assert!(
            records
                .windows(2)
                .all(|window| window[0].path < window[1].path),
            "item records must be sorted by canonical path"
        );
    }

    #[test]
    fn item_records_do_not_inherit_module_promotion() {
        let records = item_records();
        let process_run = records
            .iter()
            .find(|record| record.path == "std::process::run")
            .expect("std::process::run is manifest-listed");
        assert_eq!(
            process_run.status,
            crate::manifest::feature_status::Status::Experimental
        );
        assert_eq!(process_run.evidence.status, process_run.status);
        assert_eq!(
            process_run.evidence.doc_path.as_deref(),
            Some("stdlib/process_run.md")
        );
    }
}
